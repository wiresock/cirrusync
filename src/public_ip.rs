//! Public IPv4 and IPv6 discovery with ordered provider fallback.

use std::{
    error::Error as StdError,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use reqwest::{Client, StatusCode, redirect::Policy};
use thiserror::Error;

use crate::USER_AGENT;

const MAX_RESPONSE_BYTES: usize = 16 * 1024;

/// The IP address family a provider response must contain.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AddressFamily {
    Ipv4,
    Ipv6,
}

impl AddressFamily {
    #[must_use]
    pub const fn matches(self, address: IpAddr) -> bool {
        matches!(
            (self, address),
            (Self::Ipv4, IpAddr::V4(_)) | (Self::Ipv6, IpAddr::V6(_))
        )
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ipv4 => "IPv4",
            Self::Ipv6 => "IPv6",
        }
    }
}

impl fmt::Display for AddressFamily {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// HTTP public-address discovery client.
#[derive(Clone, Debug)]
pub struct PublicIpClient {
    client: Client,
    timeout: Duration,
}

impl PublicIpClient {
    /// Create a client with an explicit total request timeout.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero timeout or if the underlying HTTP client
    /// cannot be constructed.
    pub fn new(timeout: Duration) -> Result<Self, PublicIpError> {
        Self::build(timeout, true)
    }

    fn build(timeout: Duration, https_only: bool) -> Result<Self, PublicIpError> {
        if timeout.is_zero() {
            return Err(PublicIpError::InvalidTimeout);
        }
        let client = Client::builder()
            .timeout(timeout)
            .https_only(https_only)
            .redirect(Policy::none())
            .no_proxy()
            .user_agent(USER_AGENT)
            .build()
            .map_err(|error| PublicIpError::ClientBuild(error.without_url()))?;
        Ok(Self { client, timeout })
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests(timeout: Duration) -> Result<Self, PublicIpError> {
        Self::build(timeout, false)
    }

    /// Discover a public address, trying providers in the supplied order.
    ///
    /// A malformed response, wrong-family response, non-public address, HTTP
    /// status, or transport failure only fails that provider. The full call
    /// fails after every configured provider has been attempted or the shared
    /// total timeout has elapsed.
    ///
    /// # Errors
    ///
    /// Returns an error if no providers are supplied, every provider fails, or
    /// the total deadline expires. [`PublicIpError::failures`] preserves each
    /// provider attempt that completed or exhausted its allocated time.
    pub async fn discover(
        &self,
        family: AddressFamily,
        providers: &[String],
    ) -> Result<IpAddr, PublicIpError> {
        if providers.is_empty() {
            return Err(PublicIpError::NoProviders { family });
        }

        let started = std::time::Instant::now();
        let mut failures = Vec::with_capacity(providers.len());
        for (index, provider) in providers.iter().enumerate() {
            let remaining = self.timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(PublicIpError::DeadlineExceeded { family, failures });
            }
            let providers_left = u32::try_from(providers.len() - index).unwrap_or(u32::MAX);
            let attempt_timeout = remaining / providers_left;
            if attempt_timeout.is_zero() {
                return Err(PublicIpError::DeadlineExceeded { family, failures });
            }

            match tokio::time::timeout(attempt_timeout, self.query_provider(provider, family)).await
            {
                Ok(Ok(address)) => return Ok(address),
                Ok(Err(error)) => failures.push(ProviderFailure {
                    provider: redact_provider_url(provider),
                    error,
                }),
                Err(_) => {
                    failures.push(ProviderFailure {
                        provider: redact_provider_url(provider),
                        error: ProviderError::TimedOut,
                    });
                }
            }
        }

        Err(PublicIpError::AllProvidersFailed { family, failures })
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    async fn query_provider(
        &self,
        provider: &str,
        family: AddressFamily,
    ) -> Result<IpAddr, ProviderError> {
        let mut response = self
            .client
            .get(provider)
            .send()
            .await
            .map_err(sanitized_request_error)?;

        let status = response.status();
        if !status.is_success() {
            return Err(ProviderError::HttpStatus { status });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ProviderError::ResponseTooLarge {
                limit: MAX_RESPONSE_BYTES,
            });
        }

        let capacity = response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(MAX_RESPONSE_BYTES);
        let mut bytes = Vec::with_capacity(capacity);
        while let Some(chunk) = response.chunk().await.map_err(sanitized_request_error)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ProviderError::ResponseTooLarge {
                    limit: MAX_RESPONSE_BYTES,
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = std::str::from_utf8(&bytes).map_err(ProviderError::NonUtf8Response)?;
        parse_provider_response(body, family)
    }
}

fn sanitized_request_error(error: reqwest::Error) -> ProviderError {
    ProviderError::Request(error.without_url())
}

fn redact_provider_url(provider: &str) -> String {
    let Ok(url) = reqwest::Url::parse(provider) else {
        return "<invalid provider URL>".to_owned();
    };
    let origin = url.origin().ascii_serialization();
    if origin == "null" {
        "<invalid provider URL>".to_owned()
    } else {
        format!("{origin}/<redacted>")
    }
}

/// Parse either a plain IP response or Cloudflare's `cdn-cgi/trace` format.
///
/// # Errors
///
/// Returns an error if the response is malformed, too large, from the wrong
/// address family, or contains a non-public address.
pub fn parse_provider_response(
    response: &str,
    family: AddressFamily,
) -> Result<IpAddr, ProviderError> {
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(ProviderError::ResponseTooLarge {
            limit: MAX_RESPONSE_BYTES,
        });
    }

    let trimmed = response.trim();
    if trimmed.is_empty() {
        return Err(ProviderError::EmptyResponse);
    }

    let candidate = if let Ok(address) = trimmed.parse::<IpAddr>() {
        address
    } else {
        let mut addresses = response.lines().filter_map(|line| {
            line.trim()
                .strip_prefix("ip=")
                .map(str::trim)
                .filter(|value| !value.is_empty())
        });
        let value = addresses.next().ok_or(ProviderError::MalformedResponse)?;
        if addresses.next().is_some() {
            return Err(ProviderError::MalformedResponse);
        }
        value
            .parse::<IpAddr>()
            .map_err(|_| ProviderError::MalformedResponse)?
    };

    if !family.matches(candidate) {
        return Err(ProviderError::WrongAddressFamily {
            expected: family,
            actual: candidate,
        });
    }
    if !is_public_address(candidate) {
        return Err(ProviderError::NonPublicAddress { address: candidate });
    }

    Ok(candidate)
}

/// Return whether an address is suitable for publishing as an Internet endpoint.
///
/// This is intentionally more conservative than merely checking RFC 1918 and
/// loopback ranges. Documentation, benchmarking, CGNAT, transition, and other
/// special-purpose ranges are rejected as well.
#[must_use]
pub fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _d] = address.octets();

    if a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
    {
        return false;
    }

    // IETF protocol assignments, TEST-NET ranges, deprecated 6to4 relay
    // anycast, and inter-network benchmarking ranges.
    let ietf_or_test_net = a == 192 && b == 0 && matches!(c, 0 | 2);
    if ietf_or_test_net
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 31 && c == 196)
        || (a == 192 && b == 52 && c == 193)
        || (a == 192 && b == 175 && c == 48)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
    {
        return false;
    }

    true
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    let first = segments[0];
    let second = segments[1];

    // Globally routed unicast allocations currently live in 2000::/3.
    if first & 0xe000 != 0x2000 {
        return false;
    }

    // 2001:0000::/23 is reserved for protocol assignments rather than normal
    // endpoint addresses. Also reject documentation, 6to4, former 6bone, and
    // the second documentation prefix allocated by RFC 9637.
    let special_2001 = first == 0x2001 && (second <= 0x01ff || second == 0x0db8);
    if special_2001
        || first == 0x2002
        || (first == 0x2620 && second == 0x004f && segments[2] == 0x8000)
        || first == 0x3ffe
        || (first == 0x3fff && second & 0xf000 == 0)
    {
        return false;
    }

    true
}

/// Why a particular provider could not supply a usable address.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("HTTP request failed: {0}")]
    Request(#[source] reqwest::Error),

    #[error("provider request exhausted its share of the total discovery timeout")]
    TimedOut,

    #[error("provider returned HTTP status {status}")]
    HttpStatus { status: StatusCode },

    #[error("provider response exceeds the {limit}-byte safety limit")]
    ResponseTooLarge { limit: usize },

    #[error("provider response is not valid UTF-8")]
    NonUtf8Response(#[source] std::str::Utf8Error),

    #[error("provider response is empty")]
    EmptyResponse,

    #[error("provider response contains neither one plain IP nor one trace ip= line")]
    MalformedResponse,

    #[error("provider returned {actual}, but {expected} was requested")]
    WrongAddressFamily {
        expected: AddressFamily,
        actual: IpAddr,
    },

    #[error("provider returned non-public address {address}")]
    NonPublicAddress { address: IpAddr },
}

/// One failed provider attempt.
#[derive(Debug)]
pub struct ProviderFailure {
    pub provider: String,
    pub error: ProviderError,
}

impl fmt::Display for ProviderFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.provider, self.error)
    }
}

/// Discovery failures, including every attempted provider.
#[derive(Debug)]
pub enum PublicIpError {
    InvalidTimeout,
    ClientBuild(reqwest::Error),
    NoProviders {
        family: AddressFamily,
    },
    AllProvidersFailed {
        family: AddressFamily,
        failures: Vec<ProviderFailure>,
    },
    DeadlineExceeded {
        family: AddressFamily,
        failures: Vec<ProviderFailure>,
    },
}

impl PublicIpError {
    /// Return individual provider failures when discovery exhausted fallback.
    #[must_use]
    pub fn failures(&self) -> &[ProviderFailure] {
        match self {
            Self::AllProvidersFailed { failures, .. } | Self::DeadlineExceeded { failures, .. } => {
                failures
            }
            _ => &[],
        }
    }
}

impl fmt::Display for PublicIpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimeout => {
                formatter.write_str("public IP request timeout must be nonzero")
            }
            Self::ClientBuild(error) => {
                write!(formatter, "failed to build public IP HTTP client: {error}")
            }
            Self::NoProviders { family } => {
                write!(formatter, "no {family} public IP providers are configured")
            }
            Self::AllProvidersFailed { family, failures } => {
                write!(
                    formatter,
                    "all {} {family} public IP providers failed",
                    failures.len()
                )?;
                for failure in failures {
                    write!(formatter, "; {failure}")?;
                }
                Ok(())
            }
            Self::DeadlineExceeded { family, failures } => {
                write!(
                    formatter,
                    "{family} public IP discovery exceeded its total timeout after {} provider \
                     failure(s)",
                    failures.len()
                )?;
                for failure in failures {
                    write!(formatter, "; {failure}")?;
                }
                Ok(())
            }
        }
    }
}

impl StdError for PublicIpError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::ClientBuild(error) => Some(error),
            Self::AllProvidersFailed { failures, .. } | Self::DeadlineExceeded { failures, .. } => {
                failures
                    .first()
                    .map(|failure| &failure.error as &(dyn StdError + 'static))
            }
            Self::InvalidTimeout | Self::NoProviders { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    #[test]
    fn parses_plain_ipv4_and_ipv6() {
        assert_eq!(
            parse_provider_response(" 8.8.8.8\r\n", AddressFamily::Ipv4)
                .expect("public IPv4 should parse"),
            "8.8.8.8".parse::<IpAddr>().expect("test IP should parse")
        );
        assert_eq!(
            parse_provider_response("2606:4700:4700::1111\n", AddressFamily::Ipv6)
                .expect("public IPv6 should parse"),
            "2606:4700:4700::1111"
                .parse::<IpAddr>()
                .expect("test IP should parse")
        );
    }

    #[test]
    fn parses_cloudflare_trace() {
        let trace = "fl=123f456\nh=example.test\nip=1.1.1.1\nts=1234567890\n";
        assert_eq!(
            parse_provider_response(trace, AddressFamily::Ipv4)
                .expect("trace response should parse"),
            "1.1.1.1".parse::<IpAddr>().expect("test IP should parse")
        );
    }

    #[test]
    fn rejects_missing_or_ambiguous_trace_address() {
        assert!(matches!(
            parse_provider_response("fl=123\nh=example.test\n", AddressFamily::Ipv4),
            Err(ProviderError::MalformedResponse)
        ));
        assert!(matches!(
            parse_provider_response("ip=1.1.1.1\nip=8.8.8.8\n", AddressFamily::Ipv4),
            Err(ProviderError::MalformedResponse)
        ));
    }

    #[test]
    fn rejects_wrong_address_family() {
        assert!(matches!(
            parse_provider_response("1.1.1.1", AddressFamily::Ipv6),
            Err(ProviderError::WrongAddressFamily { .. })
        ));
        assert!(matches!(
            parse_provider_response("2606:4700:4700::1111", AddressFamily::Ipv4),
            Err(ProviderError::WrongAddressFamily { .. })
        ));
    }

    #[test]
    fn rejects_non_public_ipv4_ranges() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.0.0.1",
            "192.0.2.1",
            "192.31.196.1",
            "192.52.193.1",
            "192.175.48.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            let error = parse_provider_response(address, AddressFamily::Ipv4)
                .expect_err("special-purpose IPv4 should be rejected");
            assert!(
                matches!(error, ProviderError::NonPublicAddress { .. }),
                "{address}: {error}"
            );
        }
    }

    #[test]
    fn rejects_non_public_ipv6_ranges() {
        for address in [
            "::",
            "::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "100::1",
            "2001:db8::1",
            "2002:808:808::1",
            "2620:4f:8000::1",
            "3fff::1",
            "3fff:10::1",
            "3fff:fff::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
        ] {
            let error = parse_provider_response(address, AddressFamily::Ipv6)
                .expect_err("special-purpose IPv6 should be rejected");
            assert!(
                matches!(error, ProviderError::NonPublicAddress { .. }),
                "{address}: {error}"
            );
        }
    }

    #[test]
    fn accepts_representative_public_addresses() {
        for (address, family) in [
            ("1.1.1.1", AddressFamily::Ipv4),
            ("8.8.8.8", AddressFamily::Ipv4),
            ("192.31.195.255", AddressFamily::Ipv4),
            ("192.31.197.0", AddressFamily::Ipv4),
            ("2606:4700:4700::1111", AddressFamily::Ipv6),
            ("2620:4f:7fff:ffff::1", AddressFamily::Ipv6),
            ("2620:4f:8001::1", AddressFamily::Ipv6),
            ("2001:4860:4860::8888", AddressFamily::Ipv6),
            ("3fff:1000::1", AddressFamily::Ipv6),
        ] {
            parse_provider_response(address, family)
                .expect("representative public address should be accepted");
        }
    }

    #[tokio::test]
    async fn falls_back_after_invalid_address_and_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/private"))
            .respond_with(ResponseTemplate::new(200).set_body_string("10.0.0.1"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/failure"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/public"))
            .respond_with(ResponseTemplate::new(200).set_body_string("1.1.1.1"))
            .mount(&server)
            .await;

        let providers = vec![
            format!("{}/private", server.uri()),
            format!("{}/failure", server.uri()),
            format!("{}/public", server.uri()),
        ];
        let client = PublicIpClient::new_for_tests(Duration::from_secs(2))
            .expect("client should be created");
        let address = client
            .discover(AddressFamily::Ipv4, &providers)
            .await
            .expect("third provider should succeed");

        assert_eq!(
            address,
            "1.1.1.1".parse::<IpAddr>().expect("test IP should parse")
        );
    }

    #[tokio::test]
    async fn preserves_all_provider_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/private"))
            .respond_with(ResponseTemplate::new(200).set_body_string("10.0.0.1"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/failure"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let providers = vec![
            format!("{}/private", server.uri()),
            format!("{}/failure", server.uri()),
        ];
        let client = PublicIpClient::new_for_tests(Duration::from_secs(2))
            .expect("client should be created");
        let error = client
            .discover(AddressFamily::Ipv4, &providers)
            .await
            .expect_err("all providers should fail");

        assert_eq!(error.failures().len(), 2);
        assert!(matches!(
            &error.failures()[0].error,
            ProviderError::NonPublicAddress { .. }
        ));
        assert!(matches!(
            &error.failures()[1].error,
            ProviderError::HttpStatus {
                status: StatusCode::TOO_MANY_REQUESTS
            }
        ));
    }

    #[tokio::test]
    async fn production_client_rejects_plain_http_without_leaking_query() {
        let client = PublicIpClient::new(Duration::from_secs(1)).expect("client should be created");
        let credential_secret = "provider-api-key";
        let path_secret = "path-api-key";
        let providers = vec![format!(
            "http://user:{credential_secret}@127.0.0.1:1/{path_secret}/address?token=\
             {credential_secret}&account=test"
        )];

        let error = client
            .discover(AddressFamily::Ipv4, &providers)
            .await
            .expect_err("plain HTTP provider should be rejected");
        let displayed = error.to_string();
        assert!(!displayed.contains(credential_secret));
        assert!(!displayed.contains(path_secret));
        assert!(displayed.contains("<redacted>"));
        assert!(matches!(
            &error.failures()[0].error,
            ProviderError::Request(_)
        ));
    }

    #[tokio::test]
    async fn does_not_follow_provider_redirects() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/redirect"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/public", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/public"))
            .respond_with(ResponseTemplate::new(200).set_body_string("1.1.1.1"))
            .mount(&server)
            .await;

        let client = PublicIpClient::new_for_tests(Duration::from_secs(1))
            .expect("client should be created");
        let error = client
            .discover(AddressFamily::Ipv4, &[format!("{}/redirect", server.uri())])
            .await
            .expect_err("redirect response should not be followed");

        assert!(matches!(
            &error.failures()[0].error,
            ProviderError::HttpStatus {
                status: StatusCode::FOUND
            }
        ));
    }

    #[tokio::test]
    async fn discovery_timeout_is_shared_fairly_across_provider_fallbacks() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(200))
                    .set_body_string("1.1.1.1"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/would-succeed"))
            .respond_with(ResponseTemplate::new(200).set_body_string("8.8.8.8"))
            .mount(&server)
            .await;

        let client = PublicIpClient::new_for_tests(Duration::from_millis(200))
            .expect("client should be created");
        let address = client
            .discover(
                AddressFamily::Ipv4,
                &[
                    format!("{}/slow", server.uri()),
                    format!("{}/would-succeed", server.uri()),
                ],
            )
            .await
            .expect("the second provider should retain part of the total timeout");

        assert_eq!(
            address,
            "8.8.8.8".parse::<IpAddr>().expect("test IP should parse")
        );
    }

    #[tokio::test]
    async fn all_slow_providers_remain_within_one_total_timeout() {
        let server = MockServer::start().await;
        for path_name in ["/slow-one", "/slow-two", "/slow-three"] {
            Mock::given(method("GET"))
                .and(path(path_name))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_delay(Duration::from_millis(300))
                        .set_body_string("1.1.1.1"),
                )
                .mount(&server)
                .await;
        }

        let client = PublicIpClient::new_for_tests(Duration::from_millis(180))
            .expect("client should be created");
        let started = std::time::Instant::now();
        let error = client
            .discover(
                AddressFamily::Ipv4,
                &[
                    format!("{}/slow-one", server.uri()),
                    format!("{}/slow-two", server.uri()),
                    format!("{}/slow-three", server.uri()),
                ],
            )
            .await
            .expect_err("all slow providers should exhaust the bounded budget");

        assert!(
            started.elapsed() < Duration::from_millis(400),
            "discovery exceeded its total timeout by too much"
        );
        assert!(
            !error.failures().is_empty() && error.failures().len() <= 3,
            "the deadline should preserve every provider attempt that actually started"
        );
        assert!(
            error
                .failures()
                .iter()
                .all(|failure| matches!(&failure.error, ProviderError::TimedOut))
        );
    }

    #[test]
    fn rejects_zero_timeout_and_empty_provider_list() {
        assert!(matches!(
            PublicIpClient::new(Duration::ZERO),
            Err(PublicIpError::InvalidTimeout)
        ));

        let runtime = tokio::runtime::Runtime::new().expect("runtime should be created");
        let client = PublicIpClient::new_for_tests(Duration::from_secs(1))
            .expect("client should be created");
        let error = runtime
            .block_on(client.discover(AddressFamily::Ipv4, &[]))
            .expect_err("empty provider list should fail");
        assert!(matches!(error, PublicIpError::NoProviders { .. }));
    }
}
