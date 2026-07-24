//! Minimal, typed client for the Cloudflare API v4.
//!
//! The client deliberately exposes DNS-specific operations instead of a generic
//! HTTP wrapper. This keeps authorization headers inside this module and makes
//! it harder for callers to accidentally log secrets.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{StatusCode, Url};
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

use crate::USER_AGENT;

const API_BASE: &str = "https://api.cloudflare.com/client/v4/";
const ZONE_PAGE_SIZE: usize = 50;
const RECORD_PAGE_SIZE: usize = 100;
const MAX_PAGES: u32 = 100;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);
const MAX_ERROR_DETAILS: usize = 4;
const MAX_ERROR_MESSAGE_BYTES: usize = 256;
const MAX_ERROR_SUMMARY_BYTES: usize = 1024;

/// A DNS record returned by Cloudflare.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DnsRecord {
    /// Cloudflare's opaque record identifier.
    pub id: String,
    /// DNS record type, normally `A` or `AAAA`.
    #[serde(rename = "type")]
    pub record_type: String,
    /// Fully-qualified DNS record name.
    pub name: String,
    /// Current record content.
    pub content: String,
    /// Cloudflare TTL value.
    pub ttl: u32,
    /// Whether Cloudflare proxying is enabled.
    pub proxied: bool,
}

/// Values sent when a DNS record is created or replaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordPayload {
    /// DNS record type.
    #[serde(rename = "type")]
    pub record_type: String,
    /// Fully-qualified DNS record name.
    pub name: String,
    /// Public IP address in textual form.
    pub content: String,
    /// Desired Cloudflare TTL.
    pub ttl: u32,
    /// Desired Cloudflare proxy state.
    pub proxied: bool,
}

#[derive(Serialize)]
struct EditProbePayload<'a> {
    #[serde(rename = "type")]
    record_type: &'a str,
    name: &'a str,
    ttl: u32,
}

/// A structured error item returned by Cloudflare.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ApiErrorDetail {
    /// Cloudflare error code, when present.
    #[serde(default)]
    pub code: Option<i64>,
    /// Human-readable Cloudflare error message.
    #[serde(default)]
    pub message: String,
}

/// Errors produced by [`CloudflareClient`].
#[derive(Debug, Error)]
pub enum CloudflareError {
    /// The configured API endpoint was not a valid base URL.
    #[error("invalid Cloudflare API base URL")]
    InvalidBaseUrl,

    /// The API token could not be represented as an HTTP authorization value.
    #[error("Cloudflare API token contains invalid characters")]
    InvalidToken,

    /// A zero request timeout would make every operation fail immediately.
    #[error("Cloudflare request timeout must be nonzero")]
    InvalidTimeout,

    /// The HTTP client could not be constructed.
    #[error("could not construct Cloudflare HTTP client: {0}")]
    ClientBuild(#[source] reqwest::Error),

    /// A request failed before Cloudflare returned a response.
    #[error("Cloudflare request failed while {operation}: {source}")]
    Request {
        /// Short, secret-safe operation description.
        operation: &'static str,
        /// Underlying transport error.
        #[source]
        source: reqwest::Error,
    },

    /// Cloudflare rejected the API token globally.
    #[error("Cloudflare authentication failed ({status}): {message}")]
    Authentication {
        /// HTTP status returned by Cloudflare.
        status: StatusCode,
        /// Sanitized Cloudflare error summary.
        message: String,
        /// Structured Cloudflare errors.
        errors: Vec<ApiErrorDetail>,
    },

    /// The token is valid but cannot access a particular Cloudflare resource.
    #[error("Cloudflare permission denied ({status}): {message}")]
    PermissionDenied {
        /// HTTP status returned by Cloudflare.
        status: StatusCode,
        /// Sanitized Cloudflare error summary.
        message: String,
        /// Structured Cloudflare errors.
        errors: Vec<ApiErrorDetail>,
    },

    /// Cloudflare asked the client to reduce its request rate.
    #[error("Cloudflare rate limit exceeded{retry_suffix}: {message}")]
    RateLimited {
        /// Parsed `Retry-After` delay, if Cloudflare supplied one.
        retry_after: Option<Duration>,
        /// Preformatted suffix used by the error display.
        retry_suffix: String,
        /// Sanitized Cloudflare error summary.
        message: String,
        /// Structured Cloudflare errors.
        errors: Vec<ApiErrorDetail>,
    },

    /// Cloudflare returned an unsuccessful API envelope or HTTP status.
    #[error("Cloudflare API error while {operation} ({status}){retry_suffix}: {message}")]
    Api {
        /// Short operation description.
        operation: &'static str,
        /// HTTP status returned by Cloudflare.
        status: StatusCode,
        /// Parsed `Retry-After` delay, if Cloudflare supplied one.
        retry_after: Option<Duration>,
        /// Preformatted suffix used by the error display.
        retry_suffix: String,
        /// Sanitized Cloudflare error summary.
        message: String,
        /// Structured Cloudflare errors.
        errors: Vec<ApiErrorDetail>,
    },

    /// Cloudflare returned JSON that did not match its documented API envelope.
    #[error("malformed Cloudflare response while {operation}: {reason}")]
    MalformedResponse {
        /// Short operation description.
        operation: &'static str,
        /// Sanitized JSON decoding error.
        reason: String,
    },

    /// A response exceeded the defensive in-memory size limit.
    #[error("Cloudflare response exceeded the {limit}-byte safety limit while {operation}")]
    ResponseTooLarge {
        /// Short operation description.
        operation: &'static str,
        /// Maximum accepted response size.
        limit: usize,
    },

    /// A successful envelope omitted its required `result`.
    #[error("Cloudflare response omitted its result while {operation}")]
    MissingResult {
        /// Short operation description.
        operation: &'static str,
    },

    /// No exact zone match was returned.
    #[error("Cloudflare zone not found: {zone}")]
    ZoneNotFound {
        /// Requested zone name.
        zone: String,
    },

    /// More than one exact zone matched a name.
    #[error("multiple Cloudflare zones matched {zone}; configure an explicit zone_id")]
    AmbiguousZone {
        /// Requested zone name.
        zone: String,
    },

    /// An explicit zone ID resolved to a different DNS zone.
    #[error(
        "Cloudflare zone ID {zone_id} belongs to {actual_zone}, not configured zone {expected_zone}"
    )]
    ZoneNameMismatch {
        /// Configured zone name.
        expected_zone: String,
        /// Zone name returned by Cloudflare.
        actual_zone: String,
        /// Explicit zone identifier.
        zone_id: String,
    },

    /// An explicit-zone response returned an ID other than the requested ID.
    #[error(
        "Cloudflare explicit-zone response returned ID {actual_zone_id}, expected {expected_zone_id}"
    )]
    ZoneIdMismatch {
        /// Explicit zone identifier sent in the request.
        expected_zone_id: String,
        /// Zone identifier returned in the response.
        actual_zone_id: String,
    },

    /// A matching zone exists but is not in Cloudflare's active state.
    #[error("Cloudflare zone {zone} is not active (status: {status})")]
    ZoneNotActive {
        /// Requested zone name.
        zone: String,
        /// Status returned by Cloudflare.
        status: String,
    },

    /// An exact DNS record was absent and creation was not permitted.
    #[error("Cloudflare {record_type} record {name} does not exist and creation is disabled")]
    RecordNotFound {
        /// Requested record type.
        record_type: String,
        /// Requested record name.
        name: String,
    },

    /// More than one exact DNS record matched a type and name.
    #[error("multiple Cloudflare {record_type} records matched {name}")]
    AmbiguousRecord {
        /// Requested record type.
        record_type: String,
        /// Requested record name.
        name: String,
    },

    /// Token verification succeeded but the token is not active.
    #[error("Cloudflare API token is not active (status: {status})")]
    TokenInactive {
        /// Status returned by the verification endpoint.
        status: String,
    },

    /// Pagination exceeded a defensive upper bound.
    #[error("Cloudflare pagination exceeded the safety limit while {operation}")]
    PaginationLimit {
        /// Short operation description.
        operation: &'static str,
    },
}

impl CloudflareError {
    /// Return Cloudflare's requested retry delay, when supplied.
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. } | Self::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Whether this failure invalidates every operation using the current token.
    #[must_use]
    pub const fn is_authentication_failure(&self) -> bool {
        matches!(
            self,
            Self::Authentication { .. } | Self::TokenInactive { .. }
        )
    }

    /// Whether this failure is a global rate-limit response.
    #[must_use]
    pub const fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited { .. })
    }

    /// Whether Cloudflare reported that a requested resource no longer exists.
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Api {
                status: StatusCode::NOT_FOUND,
                ..
            }
        )
    }

    /// Whether retrying later can reasonably succeed without a configuration change.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Request { .. }
            | Self::RateLimited { .. }
            | Self::MalformedResponse { .. }
            | Self::ResponseTooLarge { .. }
            | Self::MissingResult { .. } => true,
            Self::Api { status, .. } => {
                status.is_server_error()
                    || *status == StatusCode::REQUEST_TIMEOUT
                    || *status == StatusCode::CONFLICT
            }
            _ => false,
        }
    }
}

/// A cloneable Cloudflare DNS client.
///
/// Clones share both the HTTP connection pool and zone-ID cache.
#[derive(Clone)]
pub struct CloudflareClient {
    http: reqwest::Client,
    api_base: Url,
    api_token: Arc<SecretString>,
    request_timeout: Duration,
    zone_cache: Arc<RwLock<HashMap<String, String>>>,
}

impl std::fmt::Debug for CloudflareClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloudflareClient")
            .field("api_base", &self.api_base)
            .field("request_timeout", &self.request_timeout)
            .field("cached_zones", &"<shared cache>")
            .finish_non_exhaustive()
    }
}

impl CloudflareClient {
    /// Construct a client using the public Cloudflare API endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error if the token cannot be encoded safely or the HTTP
    /// client cannot be constructed.
    pub fn new(api_token: &SecretString, timeout: Duration) -> Result<Self, CloudflareError> {
        Self::with_api_base(api_token, timeout, API_BASE, true)
    }

    fn with_api_base(
        api_token: &SecretString,
        timeout: Duration,
        api_base: &str,
        enforce_https: bool,
    ) -> Result<Self, CloudflareError> {
        if timeout.is_zero() {
            return Err(CloudflareError::InvalidTimeout);
        }
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {}", api_token.expose_secret()))
                .map_err(|_| CloudflareError::InvalidToken)?;
        authorization.set_sensitive(true);

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);

        let mut client_builder = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(timeout)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT);
        if enforce_https {
            client_builder = client_builder.https_only(true);
        }
        let http = client_builder
            .build()
            .map_err(CloudflareError::ClientBuild)?;

        let mut api_base = Url::parse(api_base).map_err(|_| CloudflareError::InvalidBaseUrl)?;
        if !api_base.path().ends_with('/') {
            let path = format!("{}/", api_base.path());
            api_base.set_path(&path);
        }

        Ok(Self {
            http,
            api_base,
            api_token: Arc::new(SecretString::new(api_token.expose_secret().to_owned())),
            request_timeout: timeout,
            zone_cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Return the per-request deadline enforced by the HTTP client.
    #[must_use]
    pub const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// Construct a client pointed at a mock server.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        api_token: &SecretString,
        timeout: Duration,
        api_base: &str,
    ) -> Result<Self, CloudflareError> {
        Self::with_api_base(api_token, timeout, api_base, false)
    }

    /// Verify that the configured API token is active.
    ///
    /// # Errors
    ///
    /// Returns a typed transport, authentication, API, or response-format error.
    pub async fn verify_token(&self) -> Result<(), CloudflareError> {
        let url = self.endpoint(&["user", "tokens", "verify"]);
        let response: ApiResponse<TokenVerification> = self
            .send(self.http.get(url), "verifying the API token")
            .await?;

        if response.result.status.eq_ignore_ascii_case("active") {
            Ok(())
        } else {
            Err(CloudflareError::TokenInactive {
                status: self
                    .sanitize_remote_value(&response.result.status, MAX_ERROR_MESSAGE_BYTES),
            })
        }
    }

    /// Resolve a zone name, using an explicit ID when one was configured.
    ///
    /// Automatically resolved IDs are cached for the lifetime of the client.
    ///
    /// # Errors
    ///
    /// Returns a typed API error, or [`CloudflareError::ZoneNotFound`] when no
    /// exact zone match exists.
    pub async fn resolve_zone_id(
        &self,
        zone_name: &str,
        explicit_zone_id: Option<&str>,
    ) -> Result<String, CloudflareError> {
        if let Some(zone_id) = explicit_zone_id {
            self.resolve_explicit_zone_id(zone_name, zone_id).await
        } else {
            self.resolve_named_zone_id(zone_name).await
        }
    }

    async fn resolve_explicit_zone_id(
        &self,
        zone_name: &str,
        zone_id: &str,
    ) -> Result<String, CloudflareError> {
        let cache_key = format!(
            "id:{}:{}",
            zone_id.to_ascii_lowercase(),
            zone_name.to_ascii_lowercase()
        );
        if let Some(cached_zone_id) = self.zone_cache.read().await.get(&cache_key).cloned() {
            return Ok(cached_zone_id);
        }

        let url = self.endpoint(&["zones", zone_id]);
        let response: ApiResponse<Zone> = self
            .send(self.http.get(url), "verifying an explicit zone ID")
            .await?;
        if !response.result.name.eq_ignore_ascii_case(zone_name) {
            return Err(CloudflareError::ZoneNameMismatch {
                expected_zone: zone_name.to_owned(),
                actual_zone: self
                    .sanitize_remote_value(&response.result.name, MAX_ERROR_MESSAGE_BYTES),
                zone_id: zone_id.to_owned(),
            });
        }
        if !response.result.id.eq_ignore_ascii_case(zone_id) {
            return Err(CloudflareError::ZoneIdMismatch {
                expected_zone_id: zone_id.to_owned(),
                actual_zone_id: self
                    .sanitize_remote_value(&response.result.id, MAX_ERROR_MESSAGE_BYTES),
            });
        }
        if !response.result.status.eq_ignore_ascii_case("active") {
            return Err(CloudflareError::ZoneNotActive {
                zone: zone_name.to_owned(),
                status: self
                    .sanitize_remote_value(&response.result.status, MAX_ERROR_MESSAGE_BYTES),
            });
        }

        let verified_zone_id = zone_id.to_owned();
        self.zone_cache
            .write()
            .await
            .insert(cache_key, verified_zone_id.clone());
        Ok(verified_zone_id)
    }

    async fn resolve_named_zone_id(&self, zone_name: &str) -> Result<String, CloudflareError> {
        let cache_key = format!("name:{}", zone_name.to_ascii_lowercase());
        if let Some(zone_id) = self.zone_cache.read().await.get(&cache_key).cloned() {
            return Ok(zone_id);
        }

        let mut exact_match = None;
        let mut inactive_match = None;
        let mut page = 1_u32;
        loop {
            if page > MAX_PAGES {
                return Err(CloudflareError::PaginationLimit {
                    operation: "looking up a zone",
                });
            }

            let url = self.endpoint(&["zones"]);
            let response: ApiResponse<Vec<Zone>> = self
                .send(
                    self.http.get(url).query(&[
                        ("name", zone_name),
                        ("page", &page.to_string()),
                        ("per_page", &ZONE_PAGE_SIZE.to_string()),
                    ]),
                    "looking up a zone",
                )
                .await?;

            for zone in &response.result {
                if zone.name.eq_ignore_ascii_case(zone_name) {
                    if zone.status.eq_ignore_ascii_case("active") {
                        if exact_match.is_some() {
                            return Err(CloudflareError::AmbiguousZone {
                                zone: zone_name.to_owned(),
                            });
                        }
                        exact_match = Some(zone.id.clone());
                    } else if inactive_match.is_none() {
                        inactive_match =
                            Some(self.sanitize_remote_value(&zone.status, MAX_ERROR_MESSAGE_BYTES));
                    }
                }
            }

            if !has_next_page(
                page,
                response.result.len(),
                ZONE_PAGE_SIZE,
                response.result_info.as_ref(),
            ) {
                let Some(zone_id) = exact_match else {
                    if let Some(status) = inactive_match {
                        return Err(CloudflareError::ZoneNotActive {
                            zone: zone_name.to_owned(),
                            status,
                        });
                    }
                    return Err(CloudflareError::ZoneNotFound {
                        zone: zone_name.to_owned(),
                    });
                };
                self.zone_cache
                    .write()
                    .await
                    .insert(cache_key, zone_id.clone());
                return Ok(zone_id);
            }
            page = page.saturating_add(1);
        }
    }

    /// Find one DNS record by exact type and name.
    ///
    /// Cloudflare permits pagination even for filtered requests, so every
    /// reported page is inspected. Multiple exact matches are rejected because
    /// choosing one nondeterministically could update the wrong record.
    ///
    /// # Errors
    ///
    /// Returns a typed API error or [`CloudflareError::AmbiguousRecord`] if the
    /// exact filter unexpectedly produces more than one match.
    pub async fn find_record(
        &self,
        zone_id: &str,
        record_type: &str,
        name: &str,
    ) -> Result<Option<DnsRecord>, CloudflareError> {
        let mut exact_match = None;
        let mut page = 1_u32;

        loop {
            if page > MAX_PAGES {
                return Err(CloudflareError::PaginationLimit {
                    operation: "looking up a DNS record",
                });
            }

            let url = self.endpoint(&["zones", zone_id, "dns_records"]);
            let response: ApiResponse<Vec<DnsRecord>> = match self
                .send(
                    self.http.get(url).query(&[
                        ("name.exact", name),
                        ("type", record_type),
                        ("match", "all"),
                        ("page", &page.to_string()),
                        ("per_page", &RECORD_PAGE_SIZE.to_string()),
                    ]),
                    "looking up a DNS record",
                )
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.invalidate_zone_after_not_found(zone_id, &error).await;
                    return Err(error);
                }
            };

            for record in &response.result {
                if record.name.eq_ignore_ascii_case(name)
                    && record.record_type.eq_ignore_ascii_case(record_type)
                {
                    if exact_match.is_some() {
                        return Err(CloudflareError::AmbiguousRecord {
                            record_type: record_type.to_owned(),
                            name: name.to_owned(),
                        });
                    }
                    exact_match = Some(record.clone());
                }
            }

            if !has_next_page(
                page,
                response.result.len(),
                RECORD_PAGE_SIZE,
                response.result_info.as_ref(),
            ) {
                return Ok(exact_match);
            }
            page = page.saturating_add(1);
        }
    }

    /// Patch an existing DNS record with the desired managed values.
    ///
    /// PATCH avoids clearing Cloudflare metadata such as comments and tags
    /// which Cirrusync does not own.
    ///
    /// # Errors
    ///
    /// Returns a typed transport, authentication, API, or response-format error.
    pub async fn update_record(
        &self,
        zone_id: &str,
        record_id: &str,
        payload: &RecordPayload,
    ) -> Result<DnsRecord, CloudflareError> {
        let url = self.endpoint(&["zones", zone_id, "dns_records", record_id]);
        let response: ApiResponse<DnsRecord> = match self
            .send(self.http.patch(url).json(payload), "updating a DNS record")
            .await
        {
            Ok(response) => response,
            Err(error) => {
                self.invalidate_zone_after_not_found(zone_id, &error).await;
                return Err(error);
            }
        };
        Ok(response.result)
    }

    /// Prove DNS-write permission while omitting address and proxy fields.
    ///
    /// The patch repeats only the current record identity and TTL. This limits
    /// a concurrent external-edit race so the probe cannot restore a stale
    /// address or proxy setting.
    ///
    /// # Errors
    ///
    /// Returns a typed transport, authentication, API, or response-format error.
    pub async fn probe_record_edit(
        &self,
        zone_id: &str,
        record: &DnsRecord,
    ) -> Result<DnsRecord, CloudflareError> {
        let url = self.endpoint(&["zones", zone_id, "dns_records", &record.id]);
        let payload = EditProbePayload {
            record_type: &record.record_type,
            name: &record.name,
            ttl: record.ttl,
        };
        let response: ApiResponse<DnsRecord> = match self
            .send(
                self.http.patch(url).json(&payload),
                "probing DNS edit permission",
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                self.invalidate_zone_after_not_found(zone_id, &error).await;
                return Err(error);
            }
        };
        Ok(response.result)
    }

    /// Create a DNS record with the desired values.
    ///
    /// # Errors
    ///
    /// Returns a typed transport, authentication, API, or response-format error.
    pub async fn create_record(
        &self,
        zone_id: &str,
        payload: &RecordPayload,
    ) -> Result<DnsRecord, CloudflareError> {
        let url = self.endpoint(&["zones", zone_id, "dns_records"]);
        let response: ApiResponse<DnsRecord> = match self
            .send(self.http.post(url).json(payload), "creating a DNS record")
            .await
        {
            Ok(response) => response,
            Err(error) => {
                self.invalidate_zone_after_not_found(zone_id, &error).await;
                return Err(error);
            }
        };
        Ok(response.result)
    }

    async fn invalidate_zone_after_not_found(&self, zone_id: &str, error: &CloudflareError) {
        if error.is_not_found() {
            self.zone_cache
                .write()
                .await
                .retain(|_, cached_zone_id| !cached_zone_id.eq_ignore_ascii_case(zone_id));
        }
    }

    fn endpoint(&self, path_segments: &[&str]) -> Url {
        let mut url = self.api_base.clone();
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty();
            segments.extend(path_segments);
        }
        url
    }

    async fn send<T>(
        &self,
        request: reqwest::RequestBuilder,
        operation: &'static str,
    ) -> Result<ApiResponse<T>, CloudflareError>
    where
        T: DeserializeOwned,
    {
        let response = request
            .send()
            .await
            .map_err(|source| CloudflareError::Request {
                operation,
                source: source.without_url(),
            })?;
        let (status, retry_after, body) = read_response_body(response, operation).await?;
        let parsed = serde_json::from_slice::<ApiEnvelope<T>>(&body);

        if status == StatusCode::UNAUTHORIZED {
            let errors = parsed
                .as_ref()
                .map_or_else(|_| Vec::new(), |envelope| envelope.errors.clone());
            let (message, errors) = self.prepare_error_details(errors, status);
            return Err(CloudflareError::Authentication {
                status,
                message,
                errors,
            });
        }

        if status == StatusCode::FORBIDDEN {
            let errors = parsed
                .as_ref()
                .map_or_else(|_| Vec::new(), |envelope| envelope.errors.clone());
            let (message, errors) = self.prepare_error_details(errors, status);
            return Err(CloudflareError::PermissionDenied {
                status,
                message,
                errors,
            });
        }

        if status == StatusCode::TOO_MANY_REQUESTS {
            let errors = parsed
                .as_ref()
                .map_or_else(|_| Vec::new(), |envelope| envelope.errors.clone());
            let (message, errors) = self.prepare_error_details(errors, status);
            let retry_suffix = retry_suffix(retry_after);
            return Err(CloudflareError::RateLimited {
                retry_after,
                retry_suffix,
                message,
                errors,
            });
        }

        if !status.is_success() {
            let errors = parsed
                .as_ref()
                .map_or_else(|_| Vec::new(), |envelope| envelope.errors.clone());
            let (message, errors) = self.prepare_error_details(errors, status);
            let retry_suffix = retry_suffix(retry_after);
            return Err(CloudflareError::Api {
                operation,
                status,
                retry_after,
                retry_suffix,
                message,
                errors,
            });
        }

        let envelope = parsed.map_err(|source| CloudflareError::MalformedResponse {
            operation,
            reason: self.sanitize_remote_value(&source.to_string(), MAX_ERROR_SUMMARY_BYTES),
        })?;

        if !envelope.success {
            let (message, errors) = self.prepare_error_details(envelope.errors, status);
            let retry_suffix = retry_suffix(retry_after);
            return Err(CloudflareError::Api {
                operation,
                status,
                retry_after,
                retry_suffix,
                message,
                errors,
            });
        }

        let result = envelope
            .result
            .ok_or(CloudflareError::MissingResult { operation })?;
        Ok(ApiResponse {
            result,
            result_info: envelope.result_info,
        })
    }

    fn prepare_error_details(
        &self,
        errors: Vec<ApiErrorDetail>,
        status: StatusCode,
    ) -> (String, Vec<ApiErrorDetail>) {
        prepare_error_details(errors, status, self.api_token.expose_secret())
    }

    fn sanitize_remote_value(&self, value: &str, max_bytes: usize) -> String {
        sanitize_remote_value(value, self.api_token.expose_secret(), max_bytes)
    }
}

async fn read_response_body(
    mut response: reqwest::Response,
    operation: &'static str,
) -> Result<(StatusCode, Option<Duration>, Vec<u8>), CloudflareError> {
    let status = response.status();
    let retry_after = parse_retry_after(response.headers());
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(CloudflareError::ResponseTooLarge {
            operation,
            limit: MAX_RESPONSE_BYTES,
        });
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|source| CloudflareError::Request {
            operation,
            source: source.without_url(),
        })?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(CloudflareError::ResponseTooLarge {
                operation,
                limit: MAX_RESPONSE_BYTES,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, retry_after, body))
}

#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiErrorDetail>,
    result: Option<T>,
    #[serde(default)]
    result_info: Option<ResultInfo>,
}

#[derive(Debug)]
struct ApiResponse<T> {
    result: T,
    result_info: Option<ResultInfo>,
}

#[derive(Debug, Deserialize)]
struct ResultInfo {
    #[serde(default)]
    total_pages: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct Zone {
    id: String,
    name: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct TokenVerification {
    status: String,
}

fn has_next_page(
    page: u32,
    result_count: usize,
    page_size: usize,
    result_info: Option<&ResultInfo>,
) -> bool {
    result_info
        .and_then(|info| info.total_pages)
        .map_or(result_count == page_size, |total_pages| page < total_pages)
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .map(|delay| delay.min(MAX_RETRY_AFTER))
}

fn retry_suffix(retry_after: Option<Duration>) -> String {
    retry_after.map_or_else(String::new, |delay| {
        format!(" (retry after {}s)", delay.as_secs())
    })
}

fn prepare_error_details(
    errors: Vec<ApiErrorDetail>,
    status: StatusCode,
    sensitive_value: &str,
) -> (String, Vec<ApiErrorDetail>) {
    let summary = error_summary(&errors, status, sensitive_value);
    let errors = errors
        .into_iter()
        .filter_map(|error| {
            let message =
                sanitize_remote_value(&error.message, sensitive_value, MAX_ERROR_MESSAGE_BYTES);
            (error.code.is_some() || !message.is_empty()).then_some(ApiErrorDetail {
                code: error.code,
                message,
            })
        })
        .take(MAX_ERROR_DETAILS)
        .collect();
    (summary, errors)
}

fn error_summary(errors: &[ApiErrorDetail], status: StatusCode, sensitive_value: &str) -> String {
    let mut omitted = false;
    let mut summaries = Vec::with_capacity(errors.len().min(MAX_ERROR_DETAILS));
    for error in errors {
        let message =
            sanitize_remote_value(&error.message, sensitive_value, MAX_ERROR_MESSAGE_BYTES);
        if message.is_empty() {
            continue;
        }
        if summaries.len() == MAX_ERROR_DETAILS {
            omitted = true;
            break;
        }
        summaries.push(match error.code {
            Some(code) => format!("{message} ({code})"),
            None => message,
        });
    }

    let mut summary = summaries.join("; ");
    if omitted {
        summary.push_str("; ...");
    }
    summary = sanitize_and_truncate(&summary, MAX_ERROR_SUMMARY_BYTES);

    if summary.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("unspecified Cloudflare error")
            .to_owned()
    } else {
        summary
    }
}

fn sanitize_remote_value(value: &str, sensitive_value: &str, max_bytes: usize) -> String {
    if sensitive_value.is_empty() || !value.contains(sensitive_value) {
        return sanitize_and_truncate(value, max_bytes);
    }
    sanitize_and_truncate(&value.replace(sensitive_value, "[redacted]"), max_bytes)
}

fn sanitize_and_truncate(value: &str, max_bytes: usize) -> String {
    const ELLIPSIS: &str = "...";

    let mut sanitized = String::with_capacity(value.len().min(max_bytes));
    let mut previous_space = false;
    let mut truncated = false;

    for character in value.trim().chars() {
        let character = if character.is_control() || character.is_whitespace() {
            ' '
        } else {
            character
        };
        if character == ' ' && (previous_space || sanitized.is_empty()) {
            continue;
        }
        if sanitized.len().saturating_add(character.len_utf8()) > max_bytes {
            truncated = true;
            break;
        }
        sanitized.push(character);
        previous_space = character == ' ';
    }

    while sanitized.ends_with(' ') {
        sanitized.pop();
    }
    if truncated {
        let suffix_length = ELLIPSIS.len().min(max_bytes);
        let content_limit = max_bytes.saturating_sub(suffix_length);
        while sanitized.len() > content_limit {
            sanitized.pop();
        }
        while sanitized.ends_with(' ') {
            sanitized.pop();
        }
        sanitized.push_str(&ELLIPSIS[..suffix_length]);
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use secrecy::SecretString;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        ApiErrorDetail, CloudflareClient, CloudflareError, MAX_ERROR_SUMMARY_BYTES, MAX_PAGES,
        MAX_RESPONSE_BYTES, RecordPayload, error_summary, parse_retry_after,
    };

    fn token() -> SecretString {
        SecretString::new("test-token".to_owned())
    }

    fn envelope(result: serde_json::Value) -> serde_json::Value {
        let mut value = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": null,
            "result_info": {
                "page": 1,
                "per_page": 100,
                "count": 1,
                "total_count": 1,
                "total_pages": 1
            }
        });
        value["result"] = result;
        value
    }

    fn client(server: &MockServer) -> CloudflareClient {
        CloudflareClient::new_for_test(
            &token(),
            Duration::from_secs(2),
            &format!("{}/client/v4/", server.uri()),
        )
        .expect("test client should be valid")
    }

    #[test]
    fn rejects_a_zero_request_timeout() {
        let error =
            CloudflareClient::new_for_test(&token(), Duration::ZERO, "http://127.0.0.1/client/v4/")
                .expect_err("a zero timeout must be rejected");

        assert!(matches!(error, CloudflareError::InvalidTimeout));
    }

    #[tokio::test]
    async fn verifies_active_token_without_exposing_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/user/tokens/verify"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "id": "token-id",
                "status": "active"
            }))))
            .expect(1)
            .mount(&server)
            .await;

        client(&server)
            .verify_token()
            .await
            .expect("active token should verify");
    }

    #[tokio::test]
    async fn zone_lookup_uses_pagination_and_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .and(query_param("name", "example.com"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "errors": [],
                "result": [],
                "result_info": {"total_pages": 2}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .and(query_param("name", "example.com"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([
                {"id": "zone-id", "name": "example.com", "status": "active"}
            ]))))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let first = client
            .resolve_zone_id("example.com", None)
            .await
            .expect("zone should be found");
        let second = client
            .resolve_zone_id("EXAMPLE.COM", None)
            .await
            .expect("cached zone should be found");

        assert_eq!(first, "zone-id");
        assert_eq!(second, "zone-id");
    }

    #[tokio::test]
    async fn stale_zone_not_found_evicts_automatic_lookup_cache() {
        let server = MockServer::start().await;
        let old_zone = Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .and(query_param("name", "example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([{
                "id": "old-zone",
                "name": "example.com",
                "status": "active"
            }]))))
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        let client = client(&server);

        assert_eq!(
            client
                .resolve_zone_id("example.com", None)
                .await
                .expect("initial zone lookup should succeed"),
            "old-zone"
        );
        drop(old_zone);

        Mock::given(method("GET"))
            .and(path("/client/v4/zones/old-zone/dns_records"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "success": false,
                "errors": [{"code": 7003, "message": "zone route not found"}],
                "result": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            client
                .find_record("old-zone", "A", "home.example.com")
                .await
                .expect_err("the stale zone should fail")
                .is_not_found()
        );

        Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .and(query_param("name", "example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([{
                "id": "new-zone",
                "name": "example.com",
                "status": "active"
            }]))))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            client
                .resolve_zone_id("example.com", None)
                .await
                .expect("zone lookup should be repeated after a stale-ID failure"),
            "new-zone"
        );
    }

    #[tokio::test]
    async fn explicit_zone_id_is_verified_and_cached() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones/zone-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "id": "zone-id",
                "name": "example.com",
                "status": "active"
            }))))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let first = client
            .resolve_zone_id("example.com", Some("zone-id"))
            .await
            .expect("matching explicit zone should verify");
        let second = client
            .resolve_zone_id("EXAMPLE.COM", Some("zone-id"))
            .await
            .expect("verified explicit zone should be cached");

        assert_eq!(first, "zone-id");
        assert_eq!(second, "zone-id");
    }

    #[tokio::test]
    async fn rejects_explicit_zone_id_for_a_different_zone() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones/zone-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "id": "zone-id",
                "name": "other.example",
                "status": "active"
            }))))
            .expect(1)
            .mount(&server)
            .await;

        assert!(matches!(
            client(&server)
                .resolve_zone_id("example.com", Some("zone-id"))
                .await,
            Err(CloudflareError::ZoneNameMismatch {
                expected_zone,
                actual_zone,
                ..
            }) if expected_zone == "example.com" && actual_zone == "other.example"
        ));
    }

    #[tokio::test]
    async fn rejects_explicit_zone_response_with_a_different_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones/requested-zone-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "id": "different-zone-id",
                "name": "example.com",
                "status": "active"
            }))))
            .expect(1)
            .mount(&server)
            .await;

        assert!(matches!(
            client(&server)
                .resolve_zone_id("example.com", Some("requested-zone-id"))
                .await,
            Err(CloudflareError::ZoneIdMismatch {
                expected_zone_id,
                actual_zone_id,
            }) if expected_zone_id == "requested-zone-id"
                && actual_zone_id == "different-zone-id"
        ));
    }

    #[tokio::test]
    async fn rejects_an_inactive_explicit_zone() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones/zone-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "id": "zone-id",
                "name": "example.com",
                "status": "pending"
            }))))
            .expect(1)
            .mount(&server)
            .await;

        assert!(matches!(
            client(&server)
                .resolve_zone_id("example.com", Some("zone-id"))
                .await,
            Err(CloudflareError::ZoneNotActive { status, .. }) if status == "pending"
        ));
    }

    #[tokio::test]
    async fn automatic_lookup_ignores_inactive_exact_matches() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([
                {"id": "pending-zone", "name": "example.com", "status": "pending"},
                {"id": "active-zone", "name": "example.com", "status": "active"}
            ]))))
            .expect(1)
            .mount(&server)
            .await;

        let zone_id = client(&server)
            .resolve_zone_id("example.com", None)
            .await
            .expect("the sole active exact match should be selected");
        assert_eq!(zone_id, "active-zone");
    }

    #[tokio::test]
    async fn automatic_lookup_rejects_when_only_inactive_zone_matches() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([
                {"id": "pending-zone", "name": "example.com", "status": "pending"}
            ]))))
            .expect(1)
            .mount(&server)
            .await;

        assert!(matches!(
            client(&server).resolve_zone_id("example.com", None).await,
            Err(CloudflareError::ZoneNotActive { status, .. }) if status == "pending"
        ));
    }

    #[tokio::test]
    async fn rejects_ambiguous_zone_matches_across_pages() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "errors": [],
                "result": [{"id": "first-zone", "name": "example.com", "status": "active"}],
                "result_info": {"total_pages": 2}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([
                {"id": "second-zone", "name": "EXAMPLE.COM", "status": "active"}
            ]))))
            .expect(1)
            .mount(&server)
            .await;

        assert!(matches!(
            client(&server).resolve_zone_id("example.com", None).await,
            Err(CloudflareError::AmbiguousZone { zone }) if zone == "example.com"
        ));
    }

    #[tokio::test]
    async fn record_lookup_uses_current_exact_name_filter() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones/zone-id/dns_records"))
            .and(query_param("name.exact", "home.example.com"))
            .and(query_param("type", "A"))
            .and(query_param("match", "all"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([
                {
                    "id": "record-id",
                    "type": "A",
                    "name": "home.example.com",
                    "content": "203.0.113.8",
                    "ttl": 120,
                    "proxied": false
                }
            ]))))
            .expect(1)
            .mount(&server)
            .await;

        let record = client(&server)
            .find_record("zone-id", "A", "home.example.com")
            .await
            .expect("lookup should succeed")
            .expect("record should exist");

        assert_eq!(record.id, "record-id");
    }

    #[tokio::test]
    async fn record_lookup_rejects_multiple_exact_matches() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones/zone-id/dns_records"))
            .and(query_param("name.exact", "home.example.com"))
            .and(query_param("type", "A"))
            .and(query_param("match", "all"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([
                {
                    "id": "first-record",
                    "type": "A",
                    "name": "home.example.com",
                    "content": "192.0.2.1",
                    "ttl": 120,
                    "proxied": false
                },
                {
                    "id": "second-record",
                    "type": "A",
                    "name": "HOME.EXAMPLE.COM",
                    "content": "192.0.2.2",
                    "ttl": 120,
                    "proxied": false
                }
            ]))))
            .expect(1)
            .mount(&server)
            .await;

        assert!(matches!(
            client(&server)
                .find_record("zone-id", "A", "home.example.com")
                .await,
            Err(CloudflareError::AmbiguousRecord { record_type, name })
                if record_type == "A" && name == "home.example.com"
        ));
    }

    #[tokio::test]
    async fn update_and_create_send_complete_desired_state() {
        let server = MockServer::start().await;
        let payload = RecordPayload {
            record_type: "A".to_owned(),
            name: "home.example.com".to_owned(),
            content: "198.51.100.9".to_owned(),
            ttl: 300,
            proxied: false,
        };
        let result = json!({
            "id": "record-id",
            "type": "A",
            "name": "home.example.com",
            "content": "198.51.100.9",
            "ttl": 300,
            "proxied": false
        });

        Mock::given(method("PATCH"))
            .and(path("/client/v4/zones/zone-id/dns_records/record-id"))
            .and(wiremock::matchers::body_json(&payload))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(result.clone())))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/client/v4/zones/zone-id/dns_records"))
            .and(wiremock::matchers::body_json(&payload))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(result)))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        client
            .update_record("zone-id", "record-id", &payload)
            .await
            .expect("update should succeed");
        client
            .create_record("zone-id", &payload)
            .await
            .expect("creation should succeed");
    }

    #[tokio::test]
    async fn classifies_authentication_and_rate_limit_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/user/tokens/verify"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "success": false,
                "errors": [{"code": 9109, "message": "Invalid access token"}],
                "result": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones/zone-id/dns_records"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "17")
                    .set_body_json(json!({
                        "success": false,
                        "errors": [{"code": 10000, "message": "rate limited"}],
                        "result": null
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let error = client
            .verify_token()
            .await
            .expect_err("401 should be an authentication failure");
        assert!(matches!(
            error,
            CloudflareError::Authentication {
                status: reqwest::StatusCode::UNAUTHORIZED,
                ..
            }
        ));
        assert!(error.is_authentication_failure());

        match client.find_record("zone-id", "A", "home.example.com").await {
            Err(CloudflareError::RateLimited {
                retry_after: Some(delay),
                ..
            }) => assert_eq!(delay, Duration::from_secs(17)),
            other => panic!("expected rate-limit error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forbidden_resource_is_not_a_global_authentication_failure() {
        let server = MockServer::start().await;
        let remote_errors = (0_i64..8)
            .map(|code| {
                json!({
                    "code": code,
                    "message": "forged\r\njournal\tentry\u{7} ".repeat(80)
                })
            })
            .collect::<Vec<_>>();
        Mock::given(method("GET"))
            .and(path("/client/v4/zones/forbidden-zone/dns_records"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "success": false,
                "errors": remote_errors,
                "result": null
            })))
            .expect(1)
            .mount(&server)
            .await;

        let error = client(&server)
            .find_record("forbidden-zone", "A", "home.example.com")
            .await
            .expect_err("403 should be a resource-scoped permission failure");

        assert!(matches!(
            error,
            CloudflareError::PermissionDenied {
                status: reqwest::StatusCode::FORBIDDEN,
                ..
            }
        ));
        assert!(!error.is_authentication_failure());
        assert!(!error.is_transient());
        let CloudflareError::PermissionDenied {
            message, errors, ..
        } = &error
        else {
            unreachable!("the variant was asserted above");
        };
        assert!(message.len() <= MAX_ERROR_SUMMARY_BYTES);
        assert!(errors.len() <= super::MAX_ERROR_DETAILS);
        assert!(errors.iter().all(|detail| {
            detail.message.len() <= super::MAX_ERROR_MESSAGE_BYTES
                && !detail.message.chars().any(char::is_control)
        }));
        assert!(format!("{error:?}").len() < 4096);
    }

    #[test]
    fn retry_after_is_clamped_to_a_sane_upper_bound() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("999999999"),
        );

        assert_eq!(
            parse_retry_after(&headers),
            Some(Duration::from_secs(60 * 60))
        );
    }

    #[tokio::test]
    async fn rejects_malformed_success_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .expect(1)
            .mount(&server)
            .await;

        assert!(matches!(
            client(&server).resolve_zone_id("example.com", None).await,
            Err(CloudflareError::MalformedResponse { .. })
        ));
    }

    #[tokio::test]
    async fn preserves_http_status_for_non_json_error_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "41")
                    .set_body_string("upstream unavailable"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let error = client(&server)
            .resolve_zone_id("example.com", None)
            .await
            .expect_err("503 should be retained as an API error");
        assert!(matches!(
            error,
            CloudflareError::Api {
                status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                retry_after: Some(delay),
                ..
            } if delay == Duration::from_secs(41)
        ));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(41)));
        assert!(error.is_transient());
    }

    #[test]
    fn request_timeout_and_conflict_are_transient_api_errors() {
        for status in [
            reqwest::StatusCode::REQUEST_TIMEOUT,
            reqwest::StatusCode::CONFLICT,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let error = CloudflareError::Api {
                operation: "testing retry classification",
                status,
                retry_after: None,
                retry_suffix: String::new(),
                message: "temporary failure".to_owned(),
                errors: Vec::new(),
            };
            assert!(error.is_transient(), "{status} should be transient");
        }

        let permanent = CloudflareError::Api {
            operation: "testing retry classification",
            status: reqwest::StatusCode::BAD_REQUEST,
            retry_after: None,
            retry_suffix: String::new(),
            message: "invalid request".to_owned(),
            errors: Vec::new(),
        };
        assert!(!permanent.is_transient());
    }

    #[test]
    fn remote_error_summary_is_sanitized_and_bounded() {
        let errors = (0_i64..8)
            .map(|code| ApiErrorDetail {
                code: Some(code),
                message: "forged\r\njournal\tentry\u{7} ".repeat(80),
            })
            .collect::<Vec<_>>();

        let summary = error_summary(&errors, reqwest::StatusCode::BAD_REQUEST, "");

        assert!(summary.len() <= MAX_ERROR_SUMMARY_BYTES);
        assert!(!summary.chars().any(char::is_control));
        assert!(summary.contains("forged journal entry"));
        assert!(summary.ends_with("..."));
    }

    #[tokio::test]
    async fn redacts_a_token_echoed_by_a_remote_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "success": false,
                "errors": [{
                    "code": 1000,
                    "message": "server echoed bearer test-token in an error"
                }],
                "result": null
            })))
            .expect(1)
            .mount(&server)
            .await;

        let error = client(&server)
            .resolve_zone_id("example.com", None)
            .await
            .expect_err("the API error should be preserved");
        let rendered = format!("{error} {error:?}");

        assert!(!rendered.contains("test-token"));
        assert!(rendered.contains("[redacted]"));
    }

    #[tokio::test]
    async fn rejects_oversized_response_before_parsing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_RESPONSE_BYTES + 1]),
            )
            .expect(1)
            .mount(&server)
            .await;

        assert!(matches!(
            client(&server).resolve_zone_id("example.com", None).await,
            Err(CloudflareError::ResponseTooLarge {
                limit: MAX_RESPONSE_BYTES,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn pagination_stops_at_the_defensive_request_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/zones"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "errors": [],
                "result": [],
                "result_info": {"total_pages": MAX_PAGES + 1}
            })))
            .expect(u64::from(MAX_PAGES))
            .mount(&server)
            .await;

        assert!(matches!(
            client(&server).resolve_zone_id("example.com", None).await,
            Err(CloudflareError::PaginationLimit {
                operation: "looking up a zone"
            })
        ));
    }
}
