//! Errors that cross subsystem boundaries in the command-line application.

use std::io;

use thiserror::Error;

use crate::cloudflare::CloudflareError;
use crate::config::ConfigError;
use crate::daemon::DaemonError;
use crate::public_ip::PublicIpError;

/// Exit status used for configuration, discovery, or synchronization failures.
pub const FAILURE_EXIT_CODE: u8 = 1;

/// Fatal command-line application errors.
///
/// Record-level synchronization failures are aggregated by the updater and
/// converted to the summary variants here only at the CLI boundary.
#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Cloudflare(#[from] CloudflareError),

    #[error(transparent)]
    PublicIp(#[from] PublicIpError),

    #[error(transparent)]
    Daemon(#[from] DaemonError),

    #[error("failed to initialize logging: {0}")]
    Logging(String),

    #[error("failed to write output: {0}")]
    Output(#[source] io::Error),

    #[error("synchronization failed: {0}")]
    Synchronization(String),

    #[error("configuration check failed: {0}")]
    Check(String),
}
