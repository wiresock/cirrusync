//! Configuration loading, validation, and secret-token handling.
//!
//! Configuration never contains the Cloudflare token itself. Production
//! deployments should put the token in a separate file readable by the service
//! account. `CIRRUSYNC_TOKEN` is supported as a development override.

use std::{
    collections::HashSet,
    env, fmt,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::public_ip::AddressFamily;

/// Default location used by the command-line interface.
#[cfg(not(windows))]
pub const DEFAULT_CONFIG_PATH: &str = "/etc/cirrusync/config.toml";
/// Default location used by the command-line interface on Windows.
#[cfg(windows)]
pub const DEFAULT_CONFIG_PATH: &str = r"C:\ProgramData\cirrusync\config.toml";
/// Development-only token override.
pub const TOKEN_ENV_VAR: &str = "CIRRUSYNC_TOKEN";

pub const MIN_INTERVAL_SECONDS: u64 = 60;
pub const MAX_INTERVAL_SECONDS: u64 = 86_400;
pub const MIN_REQUEST_TIMEOUT_SECONDS: u64 = 1;
pub const MAX_REQUEST_TIMEOUT_SECONDS: u64 = 120;
pub const MAX_RECORDS: usize = 50;
pub const MAX_PROVIDERS_PER_FAMILY: usize = 8;
pub const MAX_CONFIG_BYTES: u64 = 256 * 1024;

const DEFAULT_INTERVAL_SECONDS: u64 = 300;
const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 15;
const DEFAULT_TTL: u32 = 120;
const MAX_TOKEN_BYTES: u64 = 16 * 1024;
#[cfg(not(windows))]
const DEFAULT_TOKEN_PATH: &str = "/etc/cirrusync/token";
#[cfg(windows)]
const DEFAULT_TOKEN_PATH: &str = r"C:\ProgramData\cirrusync\token";
#[cfg(target_os = "linux")]
const LINUX_O_NOFOLLOW: i32 = 0o400_000;
#[cfg(target_os = "linux")]
const LINUX_O_CLOEXEC: i32 = 0o2_000_000;
#[cfg(target_os = "linux")]
const LINUX_O_NONBLOCK: i32 = 0o4_000;
#[cfg(target_os = "linux")]
const LINUX_ELOOP: i32 = 40;

/// A safe example suitable for `print-config`.
pub const EXAMPLE_CONFIG: &str = r#"interval_seconds = 300
request_timeout_seconds = 15

[cloudflare]
api_token_file = "/etc/cirrusync/token"

[ipv4]
enabled = true
providers = [
    "https://api.cloudflare.com/cdn-cgi/trace",
    "https://api.ipify.org",
]

[ipv6]
enabled = false
providers = ["https://api6.ipify.org"]

[[records]]
zone = "example.com"
name = "home.example.com"
type = "A"
ttl = 120
proxied = false
create_if_missing = false
"#;

/// Complete daemon configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub interval_seconds: u64,
    pub request_timeout_seconds: u64,
    pub cloudflare: CloudflareConfig,
    pub ipv4: IpDiscoveryConfig,
    pub ipv6: IpDiscoveryConfig,
    pub records: Vec<RecordConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval_seconds: DEFAULT_INTERVAL_SECONDS,
            request_timeout_seconds: DEFAULT_REQUEST_TIMEOUT_SECONDS,
            cloudflare: CloudflareConfig::default(),
            ipv4: IpDiscoveryConfig {
                enabled: true,
                providers: vec![
                    "https://api.cloudflare.com/cdn-cgi/trace".to_owned(),
                    "https://api.ipify.org".to_owned(),
                ],
            },
            ipv6: IpDiscoveryConfig {
                enabled: false,
                providers: vec!["https://api6.ipify.org".to_owned()],
            },
            records: Vec::new(),
        }
    }
}

impl Config {
    /// Read, parse, canonicalize, and validate a TOML configuration file.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, its TOML is malformed,
    /// or any configuration invariant is violated.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let file = open_config_file(path)?;
        let mut contents = String::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut contents)
            .map_err(|source| ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        if contents.len() as u64 > MAX_CONFIG_BYTES {
            return Err(ConfigError::ConfigTooLarge {
                path: Some(path.to_path_buf()),
            });
        }
        Self::parse(&contents, Some(path))
    }

    /// Parse, canonicalize, and validate TOML supplied by another source.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed TOML or invalid configuration values.
    pub fn from_toml(contents: &str) -> Result<Self, ConfigError> {
        Self::parse(contents, None)
    }

    fn parse(contents: &str, path: Option<&Path>) -> Result<Self, ConfigError> {
        if contents.len() as u64 > MAX_CONFIG_BYTES {
            return Err(ConfigError::ConfigTooLarge {
                path: path.map(Path::to_path_buf),
            });
        }
        let mut config: Self = toml::from_str(contents).map_err(|source| ConfigError::Parse {
            path: path.map(Path::to_path_buf),
            position: source.span().map_or_else(String::new, |span| {
                safe_parse_position(contents, span.start)
            }),
        })?;
        config.canonicalize();
        config.validate()?;
        Ok(config)
    }

    /// Return the request timeout as a `Duration`.
    #[must_use]
    pub const fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_seconds)
    }

    /// Return the polling interval as a `Duration`.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_seconds)
    }

    /// Return the built-in example without reading the active configuration.
    #[must_use]
    pub fn example_toml() -> std::borrow::Cow<'static, str> {
        #[cfg(windows)]
        {
            std::borrow::Cow::Owned(EXAMPLE_CONFIG.replace(
                r#"api_token_file = "/etc/cirrusync/token""#,
                r#"api_token_file = "C:\\ProgramData\\cirrusync\\token""#,
            ))
        }
        #[cfg(not(windows))]
        {
            std::borrow::Cow::Borrowed(EXAMPLE_CONFIG)
        }
    }

    /// Validate all cross-field and security-sensitive invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] for the first invalid field.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_range(
            "interval_seconds",
            self.interval_seconds,
            MIN_INTERVAL_SECONDS,
            MAX_INTERVAL_SECONDS,
        )?;
        validate_range(
            "request_timeout_seconds",
            self.request_timeout_seconds,
            MIN_REQUEST_TIMEOUT_SECONDS,
            MAX_REQUEST_TIMEOUT_SECONDS,
        )?;

        if !self.cloudflare.api_token_file.is_absolute() {
            return Err(validation(
                "cloudflare.api_token_file",
                "must be an absolute path",
            ));
        }
        if self.records.is_empty() {
            return Err(validation("records", "at least one record is required"));
        }
        if self.records.len() > MAX_RECORDS {
            return Err(validation(
                "records",
                format!("at most {MAX_RECORDS} records are allowed"),
            ));
        }

        validate_discovery("ipv4", &self.ipv4)?;
        validate_discovery("ipv6", &self.ipv6)?;

        let mut unique_records = HashSet::with_capacity(self.records.len());
        for (index, record) in self.records.iter().enumerate() {
            let field = format!("records[{index}]");
            validate_record(record, &field)?;

            let family = record.record_type.family();
            let enabled = match family {
                AddressFamily::Ipv4 => self.ipv4.enabled,
                AddressFamily::Ipv6 => self.ipv6.enabled,
            };
            if !enabled {
                return Err(validation(
                    format!("{field}.type"),
                    format!(
                        "{} records require the {} discovery section to be enabled",
                        record.record_type, family
                    ),
                ));
            }

            let key = (
                canonical_dns_name(&record.zone),
                canonical_dns_name(&record.name),
                record.record_type,
            );
            if !unique_records.insert(key) {
                return Err(validation(
                    field,
                    format!(
                        "duplicate {} record for {} in zone {}",
                        record.record_type, record.name, record.zone
                    ),
                ));
            }
        }

        Ok(())
    }

    /// Read the API token, preferring the development environment override.
    ///
    /// Neither this function nor its errors expose the token contents.
    ///
    /// # Errors
    ///
    /// Returns an error when the environment override is invalid, the token
    /// file cannot be read securely, or the resulting token is empty or
    /// malformed.
    pub fn load_api_token(&self) -> Result<SecretString, ConfigError> {
        if let Some(value) = env::var_os(TOKEN_ENV_VAR) {
            let value = value
                .into_string()
                .map_err(|_| ConfigError::NonUnicodeEnvironmentToken)?;
            return secret_from_text(&value, TokenSource::Environment);
        }

        load_token_file(&self.cloudflare.api_token_file)
    }

    fn canonicalize(&mut self) {
        for record in &mut self.records {
            record.zone = canonical_dns_name(&record.zone);
            record.name = canonical_dns_name(&record.name);
            record.zone_id = record
                .zone_id
                .take()
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty());
        }
        for provider in self
            .ipv4
            .providers
            .iter_mut()
            .chain(self.ipv6.providers.iter_mut())
        {
            *provider = provider.trim().to_owned();
        }
    }
}

/// Cloudflare credentials configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CloudflareConfig {
    pub api_token_file: PathBuf,
}

impl Default for CloudflareConfig {
    fn default() -> Self {
        Self {
            api_token_file: PathBuf::from(DEFAULT_TOKEN_PATH),
        }
    }
}

/// Public-address discovery configuration for one address family.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IpDiscoveryConfig {
    pub enabled: bool,
    pub providers: Vec<String>,
}

/// One Cloudflare DNS record managed by the daemon.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordConfig {
    pub zone: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
    pub name: String,
    #[serde(rename = "type")]
    pub record_type: RecordType,
    #[serde(default = "default_ttl")]
    pub ttl: u32,
    #[serde(default)]
    pub proxied: bool,
    /// Create the record when absent.
    ///
    /// Cloudflare has no conditional-create operation, so this should be
    /// enabled only when external writers will not create the same name/type.
    #[serde(default)]
    pub create_if_missing: bool,
}

/// Cloudflare record types supported by this daemon.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum RecordType {
    #[serde(rename = "A")]
    A,
    #[serde(rename = "AAAA")]
    Aaaa,
}

impl RecordType {
    #[must_use]
    pub const fn family(self) -> AddressFamily {
        match self {
            Self::A => AddressFamily::Ipv4,
            Self::Aaaa => AddressFamily::Ipv6,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::Aaaa => "AAAA",
        }
    }
}

impl fmt::Display for RecordType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Typed configuration and token-loading failures.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read configuration file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(
        "failed to parse TOML configuration{location}{position}",
        location = path
            .as_ref()
            .map(|path| format!(" {}", path.display()))
            .unwrap_or_default()
    )]
    Parse {
        path: Option<PathBuf>,
        /// Safe line and byte-column information without source text.
        position: String,
    },

    #[error(
        "configuration{location} exceeds the {MAX_CONFIG_BYTES}-byte safety limit",
        location = path
            .as_ref()
            .map(|path| format!(" {}", path.display()))
            .unwrap_or_default()
    )]
    ConfigTooLarge { path: Option<PathBuf> },

    #[error("configuration path {path} is insecure: {reason}")]
    InsecureConfigFile { path: PathBuf, reason: String },

    #[error("invalid configuration field {field}: {message}")]
    Validation { field: String, message: String },

    #[error("failed to inspect API token file {path}: {source}")]
    TokenMetadata {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to read API token file {path}: {source}")]
    TokenRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("API token path {path} is insecure: {reason}")]
    InsecureTokenFile { path: PathBuf, reason: String },

    #[error("API token file {path} exceeds the {MAX_TOKEN_BYTES}-byte safety limit")]
    TokenTooLarge { path: PathBuf },

    #[error("API token from {origin} exceeds the {MAX_TOKEN_BYTES}-byte safety limit")]
    TokenValueTooLarge { origin: &'static str },

    #[error("{TOKEN_ENV_VAR} contains non-Unicode data")]
    NonUnicodeEnvironmentToken,

    #[error("the API token from {origin} is empty")]
    EmptyToken { origin: &'static str },

    #[error("the API token from {origin} contains whitespace or control characters")]
    InvalidToken { origin: &'static str },
}

fn default_ttl() -> u32 {
    DEFAULT_TTL
}

fn safe_parse_position(contents: &str, offset: usize) -> String {
    let offset = offset.min(contents.len());
    let prefix = &contents.as_bytes()[..offset];
    let line = prefix.split(|byte| *byte == b'\n').count();
    let column = prefix
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(offset + 1, |newline| offset - newline);
    format!(" at line {line}, byte column {column}")
}

fn validation(field: impl Into<String>, message: impl Into<String>) -> ConfigError {
    ConfigError::Validation {
        field: field.into(),
        message: message.into(),
    }
}

fn validate_range(field: &str, value: u64, min: u64, max: u64) -> Result<(), ConfigError> {
    if !(min..=max).contains(&value) {
        return Err(validation(
            field,
            format!("must be between {min} and {max}, inclusive"),
        ));
    }
    Ok(())
}

fn validate_discovery(field: &str, discovery: &IpDiscoveryConfig) -> Result<(), ConfigError> {
    if discovery.enabled && discovery.providers.is_empty() {
        return Err(validation(
            format!("{field}.providers"),
            "must contain at least one provider when discovery is enabled",
        ));
    }
    if discovery.providers.len() > MAX_PROVIDERS_PER_FAMILY {
        return Err(validation(
            format!("{field}.providers"),
            format!("at most {MAX_PROVIDERS_PER_FAMILY} providers are allowed"),
        ));
    }

    let mut unique = HashSet::with_capacity(discovery.providers.len());
    for (index, provider) in discovery.providers.iter().enumerate() {
        let provider_field = format!("{field}.providers[{index}]");
        if provider.len() > 2_048 {
            return Err(validation(provider_field, "URL is too long"));
        }
        let url = reqwest::Url::parse(provider)
            .map_err(|error| validation(&provider_field, format!("invalid URL: {error}")))?;
        if url.scheme() != "https" {
            return Err(validation(
                &provider_field,
                "only HTTPS provider URLs are allowed",
            ));
        }
        if url.host_str().is_none() {
            return Err(validation(&provider_field, "URL must include a host"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(validation(
                &provider_field,
                "URL must not contain embedded credentials",
            ));
        }
        if url.fragment().is_some() {
            return Err(validation(&provider_field, "URL fragments are not allowed"));
        }
        if !unique.insert(url.to_string()) {
            return Err(validation(provider_field, "duplicate provider URL"));
        }
    }

    Ok(())
}

fn validate_record(record: &RecordConfig, field: &str) -> Result<(), ConfigError> {
    let zone = canonical_dns_name(&record.zone);
    let name = canonical_dns_name(&record.name);

    if record.zone != zone {
        return Err(validation(
            format!("{field}.zone"),
            "must already be lowercase, trimmed, and without a trailing dot",
        ));
    }
    if record.name != name {
        return Err(validation(
            format!("{field}.name"),
            "must already be lowercase, trimmed, and without a trailing dot",
        ));
    }
    validate_dns_name(&zone, false)
        .map_err(|message| validation(format!("{field}.zone"), message))?;
    validate_dns_name(&name, true)
        .map_err(|message| validation(format!("{field}.name"), message))?;

    if name != zone && !name.ends_with(&format!(".{zone}")) {
        return Err(validation(
            format!("{field}.name"),
            format!("{} does not belong to zone {}", record.name, record.zone),
        ));
    }
    if let Some(zone_id) = &record.zone_id {
        if zone_id != &zone_id.trim().to_ascii_lowercase() {
            return Err(validation(
                format!("{field}.zone_id"),
                "must already be lowercase and trimmed",
            ));
        }
        if zone_id.len() != 32 || !zone_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(validation(
                format!("{field}.zone_id"),
                "must be exactly 32 hexadecimal characters",
            ));
        }
    }
    if record.ttl != 1 && !(60..=86_400).contains(&record.ttl) {
        return Err(validation(
            format!("{field}.ttl"),
            "must be 1 (automatic) or between 60 and 86400 seconds",
        ));
    }
    if record.proxied && record.ttl != 1 {
        return Err(validation(
            format!("{field}.ttl"),
            "must be 1 (automatic) when proxied is true",
        ));
    }

    Ok(())
}

fn validate_dns_name(name: &str, allow_wildcard: bool) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("must not be empty");
    }
    if name.len() > 253 {
        return Err("must not exceed 253 bytes");
    }

    for (index, label) in name.split('.').enumerate() {
        if label.is_empty() {
            return Err("must not contain empty labels");
        }
        if allow_wildcard && index == 0 && label == "*" {
            continue;
        }
        if label.len() > 63 {
            return Err("each label must not exceed 63 bytes");
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("labels must not start or end with a hyphen");
        }
        if !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err("must contain only ASCII letters, digits, hyphens, and dots");
        }
    }

    Ok(())
}

fn canonical_dns_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

#[derive(Clone, Copy)]
enum TokenSource {
    Environment,
    File,
}

impl TokenSource {
    const fn description(self) -> &'static str {
        match self {
            Self::Environment => TOKEN_ENV_VAR,
            Self::File => "the configured token file",
        }
    }
}

fn secret_from_text(text: &str, source: TokenSource) -> Result<SecretString, ConfigError> {
    if text.len() as u64 > MAX_TOKEN_BYTES {
        return Err(ConfigError::TokenValueTooLarge {
            origin: source.description(),
        });
    }
    let token = text.trim();
    if token.is_empty() {
        return Err(ConfigError::EmptyToken {
            origin: source.description(),
        });
    }
    if token
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(ConfigError::InvalidToken {
            origin: source.description(),
        });
    }
    Ok(SecretString::new(token.to_owned()))
}

fn open_config_file(path: &Path) -> Result<File, ConfigError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(LINUX_O_NOFOLLOW | LINUX_O_CLOEXEC | LINUX_O_NONBLOCK);
    }
    let file = options.open(path).map_err(|source| {
        #[cfg(target_os = "linux")]
        if source.raw_os_error() == Some(LINUX_ELOOP) {
            return ConfigError::InsecureConfigFile {
                path: path.to_path_buf(),
                reason: "symbolic links are not accepted".to_owned(),
            };
        }
        ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let opened = file.metadata().map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if !opened.is_file() {
        return Err(ConfigError::InsecureConfigFile {
            path: path.to_path_buf(),
            reason: "path must refer to a regular file".to_owned(),
        });
    }

    let named = fs::symlink_metadata(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if named.file_type().is_symlink() {
        return Err(ConfigError::InsecureConfigFile {
            path: path.to_path_buf(),
            reason: "symbolic links are not accepted".to_owned(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if named.dev() != opened.dev() || named.ino() != opened.ino() {
            return Err(ConfigError::InsecureConfigFile {
                path: path.to_path_buf(),
                reason: "path changed while the configuration was being opened".to_owned(),
            });
        }
        let mode = opened.permissions().mode() & 0o7777;
        if mode & 0o022 != 0 {
            return Err(ConfigError::InsecureConfigFile {
                path: path.to_path_buf(),
                reason: format!(
                    "mode {mode:04o} permits group or other users to modify DNS configuration"
                ),
            });
        }
        let owner = opened.uid();
        let effective_user = rustix::process::geteuid().as_raw();
        if owner != 0 && owner != effective_user {
            return Err(ConfigError::InsecureConfigFile {
                path: path.to_path_buf(),
                reason: format!("owner UID {owner} is neither root nor the current effective UID"),
            });
        }
    }
    if opened.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::ConfigTooLarge {
            path: Some(path.to_path_buf()),
        });
    }

    Ok(file)
}

fn load_token_file(path: &Path) -> Result<SecretString, ConfigError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // Linux O_NOFOLLOW, O_CLOEXEC, and O_NONBLOCK. The last flag prevents
        // an attacker-controlled FIFO from blocking before descriptor metadata
        // proves that this is a regular file.
        options.custom_flags(LINUX_O_NOFOLLOW | LINUX_O_CLOEXEC | LINUX_O_NONBLOCK);
    }
    let file = options.open(path).map_err(|source| {
        #[cfg(target_os = "linux")]
        if source.raw_os_error() == Some(LINUX_ELOOP) {
            return ConfigError::InsecureTokenFile {
                path: path.to_path_buf(),
                reason: "symbolic links are not accepted".to_owned(),
            };
        }
        ConfigError::TokenRead {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let metadata = file
        .metadata()
        .map_err(|source| ConfigError::TokenMetadata {
            path: path.to_path_buf(),
            source,
        })?;
    validate_opened_token_path(path, &metadata)?;
    validate_token_metadata(path, &file, &metadata)?;
    if metadata.len() > MAX_TOKEN_BYTES {
        return Err(ConfigError::TokenTooLarge {
            path: path.to_path_buf(),
        });
    }

    let mut contents = String::new();
    file.take(MAX_TOKEN_BYTES + 1)
        .read_to_string(&mut contents)
        .map_err(|source| ConfigError::TokenRead {
            path: path.to_path_buf(),
            source,
        })?;
    if contents.len() as u64 > MAX_TOKEN_BYTES {
        return Err(ConfigError::TokenTooLarge {
            path: path.to_path_buf(),
        });
    }

    secret_from_text(&contents, TokenSource::File)
}

fn validate_opened_token_path(path: &Path, opened: &Metadata) -> Result<(), ConfigError> {
    #[cfg(not(unix))]
    let _ = opened;

    let named = fs::symlink_metadata(path).map_err(|source| ConfigError::TokenMetadata {
        path: path.to_path_buf(),
        source,
    })?;
    if named.file_type().is_symlink() {
        return Err(ConfigError::InsecureTokenFile {
            path: path.to_path_buf(),
            reason: "symbolic links are not accepted".to_owned(),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if named.dev() != opened.dev() || named.ino() != opened.ino() {
            return Err(ConfigError::InsecureTokenFile {
                path: path.to_path_buf(),
                reason: "path changed while the token file was being opened".to_owned(),
            });
        }
    }

    Ok(())
}

fn validate_token_metadata(
    path: &Path,
    file: &File,
    metadata: &Metadata,
) -> Result<(), ConfigError> {
    #[cfg(not(target_os = "linux"))]
    let _ = file;

    if !metadata.is_file() {
        return Err(ConfigError::InsecureTokenFile {
            path: path.to_path_buf(),
            reason: "path must refer to a regular file".to_owned(),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let mode = metadata.permissions().mode() & 0o7777;
        // This accepts root-owned 0400/0440/0600/0640 files. Root ownership is
        // required even for owner-only files so the unprivileged daemon cannot
        // replace its own credential. Development overrides should use the
        // CIRRUSYNC_TOKEN environment variable.
        if mode & !0o640 != 0 || mode & 0o400 == 0 {
            return Err(ConfigError::InsecureTokenFile {
                path: path.to_path_buf(),
                reason: format!("mode {mode:04o} is too permissive; use 0600 or root:service 0640"),
            });
        }
        if metadata.uid() != 0 {
            return Err(ConfigError::InsecureTokenFile {
                path: path.to_path_buf(),
                reason: format!("mode {mode:04o} token must be owned by root"),
            });
        }
        if mode & 0o040 != 0 {
            #[cfg(not(target_os = "linux"))]
            return Err(ConfigError::InsecureTokenFile {
                path: path.to_path_buf(),
                reason: "group-readable token files are supported only on Linux, where access ACLs can be verified"
                    .to_owned(),
            });

            #[cfg(target_os = "linux")]
            let effective_group = rustix::process::getegid().as_raw();
            #[cfg(target_os = "linux")]
            if metadata.gid() != effective_group {
                return Err(ConfigError::InsecureTokenFile {
                    path: path.to_path_buf(),
                    reason: format!(
                        "group-readable token GID {} does not match effective GID {effective_group}",
                        metadata.gid()
                    ),
                });
            }
        }
    }

    #[cfg(target_os = "linux")]
    reject_token_access_acl(path, file)?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn reject_token_access_acl(path: &Path, file: &File) -> Result<(), ConfigError> {
    use rustix::fs::fgetxattr;
    use rustix::io::Errno;

    let mut value = Vec::<u8>::with_capacity(1);
    match fgetxattr(file, "system.posix_acl_access", &mut value) {
        Err(Errno::NODATA | Errno::NOTSUP) => Ok(()),
        Ok(_) | Err(Errno::RANGE) => Err(ConfigError::InsecureTokenFile {
            path: path.to_path_buf(),
            reason: "extended POSIX access ACLs are not accepted".to_owned(),
        }),
        Err(source) => Err(ConfigError::TokenMetadata {
            path: path.to_path_buf(),
            source: io::Error::from_raw_os_error(source.raw_os_error()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use secrecy::ExposeSecret;
    use tempfile::tempdir;

    use super::*;

    const VALID_CONFIG: &str = r#"
        [[records]]
        zone = "Example.COM."
        name = "Home.Example.COM."
        type = "A"
    "#;

    fn parse_error(input: &str) -> ConfigError {
        Config::from_toml(input).expect_err("configuration should be rejected")
    }

    #[test]
    fn loads_defaults_and_canonicalizes_names() {
        let config = Config::from_toml(VALID_CONFIG).expect("configuration should be valid");

        assert_eq!(config.interval_seconds, 300);
        assert_eq!(config.request_timeout_seconds, 15);
        assert!(config.ipv4.enabled);
        assert!(!config.ipv6.enabled);
        assert_eq!(config.ipv6.providers, ["https://api6.ipify.org"]);
        assert_eq!(config.records[0].zone, "example.com");
        assert_eq!(config.records[0].name, "home.example.com");
        assert_eq!(config.records[0].ttl, 120);
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = parse_error(
            r#"
                mystery = true
                [[records]]
                zone = "example.com"
                name = "home.example.com"
                type = "A"
            "#,
        );
        assert!(matches!(error, ConfigError::Parse { .. }));

        let error = parse_error(
            r#"
                update_on_start = false
                [[records]]
                zone = "example.com"
                name = "home.example.com"
                type = "A"
            "#,
        );
        assert!(matches!(error, ConfigError::Parse { .. }));
    }

    #[test]
    fn parse_errors_do_not_expose_source_text() {
        const SENTINEL: &str = "cf-secret-sentinel-that-must-never-be-logged";
        let input = format!(
            r#"
                interval_seconds = "{SENTINEL}"
                [[records]]
                zone = "example.com"
                name = "home.example.com"
                type = "A"
            "#
        );

        let error = Config::from_toml(&input).expect_err("invalid type should be rejected");
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(!display.contains(SENTINEL), "{display}");
        assert!(!debug.contains(SENTINEL), "{debug}");
        assert!(display.contains("line"), "{display}");
        assert!(display.len() < 256, "{display}");
    }

    #[test]
    fn programmatic_configuration_must_already_be_canonical() {
        let mut config =
            Config::from_toml(VALID_CONFIG).expect("baseline configuration should be valid");
        config.records[0].name = " Home.Example.COM. ".to_owned();

        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation { ref field, .. }) if field == "records[0].name"
        ));

        config.records[0].name = "home.example.com".to_owned();
        config.records[0].zone = "Example.COM.".to_owned();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation { ref field, .. }) if field == "records[0].zone"
        ));
    }

    #[test]
    fn rejects_relative_token_paths() {
        let error = parse_error(
            r#"
                [cloudflare]
                api_token_file = "token"

                [[records]]
                zone = "example.com"
                name = "home.example.com"
                type = "A"
            "#,
        );
        assert!(matches!(
            error,
            ConfigError::Validation { ref field, .. }
                if field == "cloudflare.api_token_file"
        ));
    }

    #[test]
    fn rejects_bad_ranges_and_ttl() {
        let error = parse_error(
            r#"
                interval_seconds = 0
                [[records]]
                zone = "example.com"
                name = "home.example.com"
                type = "A"
            "#,
        );
        assert!(matches!(
            error,
            ConfigError::Validation { ref field, .. } if field == "interval_seconds"
        ));

        let error = parse_error(
            r#"
                [[records]]
                zone = "example.com"
                name = "home.example.com"
                type = "A"
                ttl = 59
            "#,
        );
        assert!(matches!(
            error,
            ConfigError::Validation { ref field, .. } if field == "records[0].ttl"
        ));
    }

    #[test]
    fn accepts_capacity_boundaries_and_rejects_values_above_them() {
        let mut config = Config {
            interval_seconds: MIN_INTERVAL_SECONDS,
            ipv4: IpDiscoveryConfig {
                enabled: true,
                providers: (0..MAX_PROVIDERS_PER_FAMILY)
                    .map(|index| format!("https://provider-{index}.example/address"))
                    .collect(),
            },
            records: (0..MAX_RECORDS)
                .map(|index| RecordConfig {
                    zone: "example.com".to_owned(),
                    zone_id: None,
                    name: format!("host-{index}.example.com"),
                    record_type: RecordType::A,
                    ttl: 120,
                    proxied: false,
                    create_if_missing: false,
                })
                .collect(),
            ..Config::default()
        };
        config
            .validate()
            .expect("documented capacity boundaries should be accepted");

        config.interval_seconds = MIN_INTERVAL_SECONDS - 1;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation { ref field, .. }) if field == "interval_seconds"
        ));
        config.interval_seconds = MIN_INTERVAL_SECONDS;

        config
            .ipv4
            .providers
            .push("https://one-provider-too-many.example/address".to_owned());
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation { ref field, .. }) if field == "ipv4.providers"
        ));
        config.ipv4.providers.pop();

        config.records.push(RecordConfig {
            zone: "example.com".to_owned(),
            zone_id: None,
            name: "one-record-too-many.example.com".to_owned(),
            record_type: RecordType::A,
            ttl: 120,
            proxied: false,
            create_if_missing: false,
        });
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation { ref field, .. }) if field == "records"
        ));
    }

    #[test]
    fn accepts_automatic_ttl() {
        let config = Config::from_toml(
            r#"
                [[records]]
                zone = "example.com"
                name = "example.com"
                type = "A"
                ttl = 1
            "#,
        )
        .expect("automatic TTL should be accepted");
        assert_eq!(config.records[0].ttl, 1);
    }

    #[test]
    fn proxied_records_require_automatic_ttl() {
        let error = parse_error(
            r#"
                [[records]]
                zone = "example.com"
                name = "example.com"
                type = "A"
                ttl = 120
                proxied = true
            "#,
        );
        assert!(matches!(
            error,
            ConfigError::Validation { ref field, ref message }
                if field == "records[0].ttl" && message.contains("proxied")
        ));

        let config = Config::from_toml(
            r#"
                [[records]]
                zone = "example.com"
                name = "example.com"
                type = "A"
                ttl = 1
                proxied = true
            "#,
        )
        .expect("proxied record with automatic TTL should be accepted");
        assert!(config.records[0].proxied);
        assert_eq!(config.records[0].ttl, 1);
    }

    #[test]
    fn rejects_duplicate_records_case_insensitively() {
        let error = parse_error(
            r#"
                [[records]]
                zone = "example.com"
                name = "HOME.example.com"
                type = "A"

                [[records]]
                zone = "EXAMPLE.COM."
                name = "home.EXAMPLE.com."
                type = "A"
            "#,
        );
        assert!(matches!(
            error,
            ConfigError::Validation { ref field, .. } if field == "records[1]"
        ));
    }

    #[test]
    fn same_name_with_different_record_families_is_not_a_duplicate() {
        let config = Config::from_toml(
            r#"
                [ipv6]
                enabled = true
                providers = ["https://api6.ipify.org"]

                [[records]]
                zone = "example.com"
                name = "home.example.com"
                type = "A"

                [[records]]
                zone = "example.com"
                name = "home.example.com"
                type = "AAAA"
            "#,
        )
        .expect("A and AAAA records may share a name");
        assert_eq!(config.records.len(), 2);
    }

    #[test]
    fn rejects_names_outside_zone_and_suffix_tricks() {
        for name in ["example.net", "notexample.com"] {
            let input = format!(
                r#"
                    [[records]]
                    zone = "example.com"
                    name = "{name}"
                    type = "A"
                "#
            );
            let error = parse_error(&input);
            assert!(matches!(
                error,
                ConfigError::Validation { ref field, .. } if field == "records[0].name"
            ));
        }
    }

    #[test]
    fn validates_and_canonicalizes_cloudflare_zone_ids() {
        let config = Config::from_toml(
            r#"
                [[records]]
                zone = "example.com"
                zone_id = "ABCDEF0123456789ABCDEF0123456789"
                name = "home.example.com"
                type = "A"
            "#,
        )
        .expect("32 hexadecimal characters should form a valid zone ID");
        assert_eq!(
            config.records[0].zone_id.as_deref(),
            Some("abcdef0123456789abcdef0123456789")
        );

        for zone_id in [
            "zone-id",
            "../0123456789abcdef0123456789abc",
            "g123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
        ] {
            let input = format!(
                r#"
                    [[records]]
                    zone = "example.com"
                    zone_id = "{zone_id}"
                    name = "home.example.com"
                    type = "A"
                "#
            );
            let error = parse_error(&input);
            assert!(matches!(
                error,
                ConfigError::Validation { ref field, .. } if field == "records[0].zone_id"
            ));
        }
    }

    #[test]
    fn rejects_record_for_disabled_family() {
        let error = parse_error(
            r#"
                [[records]]
                zone = "example.com"
                name = "v6.example.com"
                type = "AAAA"
            "#,
        );
        assert!(matches!(
            error,
            ConfigError::Validation { ref field, .. } if field == "records[0].type"
        ));
    }

    #[test]
    fn rejects_insecure_and_credentialed_provider_urls() {
        for provider in [
            "http://api.ipify.org",
            "file:///etc/passwd",
            "https://user:password@example.com/ip",
            "https://example.com/ip#fragment",
        ] {
            let input = format!(
                r#"
                    [ipv4]
                    enabled = true
                    providers = ["{provider}"]

                    [[records]]
                    zone = "example.com"
                    name = "home.example.com"
                    type = "A"
                "#
            );
            let error = parse_error(&input);
            assert!(matches!(
                error,
                ConfigError::Validation { ref field, .. } if field == "ipv4.providers[0]"
            ));
        }
    }

    #[test]
    fn token_text_is_trimmed_and_wrapped_as_a_secret() {
        let token = secret_from_text("secret-token\n", TokenSource::File)
            .expect("valid token text should load");

        assert_eq!(token.expose_secret(), "secret-token");
        assert!(!format!("{token:?}").contains("secret-token"));
    }

    #[test]
    fn rejects_oversized_environment_token_text() {
        let oversized =
            "x".repeat(usize::try_from(MAX_TOKEN_BYTES).expect("token limit fits usize") + 1);

        assert!(matches!(
            secret_from_text(&oversized, TokenSource::Environment),
            Err(ConfigError::TokenValueTooLarge {
                origin: TOKEN_ENV_VAR
            })
        ));
    }

    #[test]
    fn rejects_oversized_configuration_input_and_file() {
        let max_config_bytes =
            usize::try_from(MAX_CONFIG_BYTES).expect("configuration limit fits usize");
        let prefix = format!("{VALID_CONFIG}\n#");
        let at_limit = format!("{prefix}{}", "x".repeat(max_config_bytes - prefix.len()));
        assert_eq!(at_limit.len(), max_config_bytes);
        Config::from_toml(&at_limit).expect("configuration exactly at the limit should load");

        let oversized = " ".repeat(max_config_bytes + 1);
        assert!(matches!(
            Config::from_toml(&oversized),
            Err(ConfigError::ConfigTooLarge { path: None })
        ));

        let directory = tempdir().expect("temporary directory should be created");
        let path = directory.path().join("oversized.toml");
        fs::write(&path, oversized).expect("oversized configuration should be written");
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::ConfigTooLarge { path: Some(error_path) }) if error_path == path
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_regular_or_symbolic_link_configuration_paths() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempdir().expect("temporary directory should be created");
        let config_path = directory.path().join("config.toml");
        let link_path = directory.path().join("config-link.toml");
        fs::write(&config_path, VALID_CONFIG).expect("configuration should be written");
        symlink(&config_path, &link_path).expect("configuration symlink should be created");

        assert!(matches!(
            Config::load(&link_path),
            Err(ConfigError::InsecureConfigFile { .. })
        ));
        assert!(matches!(
            Config::load(directory.path()),
            Err(ConfigError::InsecureConfigFile { .. })
        ));

        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o660))
            .expect("configuration permissions should be changed");
        assert!(matches!(
            Config::load(&config_path),
            Err(ConfigError::InsecureConfigFile { .. })
        ));
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o640))
            .expect("configuration permissions should be changed");
        Config::load(&config_path).expect("non-writable group permissions should be accepted");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_configuration_owned_by_an_unrelated_user() {
        use std::os::unix::fs::PermissionsExt;

        if !rustix::process::geteuid().is_root() {
            return;
        }
        let directory = tempdir().expect("temporary directory should be created");
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, VALID_CONFIG).expect("configuration should be written");
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644))
            .expect("configuration permissions should be set");
        rustix::fs::chown(
            &config_path,
            Some(rustix::process::Uid::from_raw(65_534)),
            None,
        )
        .expect("root test should be able to change the owner");

        assert!(matches!(
            Config::load(&config_path),
            Err(ConfigError::InsecureConfigFile { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn enforces_owner_appropriate_token_permissions() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempdir().expect("temporary directory should be created");
        let path = directory.path().join("token");
        fs::write(&path, "secret-token").expect("token should be written");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("permissions should be set");
        let owned_by_root = fs::metadata(&path)
            .expect("token metadata should be available")
            .uid()
            == 0;
        if owned_by_root {
            load_token_file(&path).expect("root-owned service token should be accepted");
        } else {
            assert!(matches!(
                load_token_file(&path),
                Err(ConfigError::InsecureTokenFile { .. })
            ));
        }

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("permissions should be set");
        if owned_by_root {
            load_token_file(&path).expect("root-owned service token should be accepted");
        } else {
            assert!(matches!(
                load_token_file(&path),
                Err(ConfigError::InsecureTokenFile { .. })
            ));
        }

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .expect("permissions should be set");
        assert!(matches!(
            load_token_file(&path),
            Err(ConfigError::InsecureTokenFile { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_readable_token_for_a_different_effective_group() {
        use std::os::unix::fs::PermissionsExt;

        if !rustix::process::geteuid().is_root() {
            return;
        }
        let directory = tempdir().expect("temporary directory should be created");
        let path = directory.path().join("token");
        fs::write(&path, "secret-token").expect("token should be written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("permissions should be set");
        let effective_group = rustix::process::getegid().as_raw();
        let other_group = if effective_group == 65_534 {
            65_533
        } else {
            65_534
        };
        rustix::fs::chown(
            &path,
            Some(rustix::process::Uid::ROOT),
            Some(rustix::process::Gid::from_raw(other_group)),
        )
        .expect("root test should be able to change token ownership");

        assert!(matches!(
            load_token_file(&path),
            Err(ConfigError::InsecureTokenFile { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symbolic_link_token_path() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("temporary directory should be created");
        let target = directory.path().join("target");
        let link = directory.path().join("token");
        fs::write(&target, "secret-token").expect("token should be written");
        set_secure_permissions(&target);
        symlink(&target, &link).expect("token symlink should be created");

        #[cfg(target_os = "linux")]
        assert!(matches!(
            load_token_file(&link),
            Err(ConfigError::InsecureTokenFile { .. })
        ));
        #[cfg(not(target_os = "linux"))]
        assert!(load_token_file(&link).is_err());
    }

    #[cfg(unix)]
    fn set_secure_permissions(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .expect("permissions should be set");
    }
}
