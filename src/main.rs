//! Command-line entry point for Cirrusync.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cirrusync::cloudflare::CloudflareClient;
use cirrusync::config::{Config, DEFAULT_CONFIG_PATH};
use cirrusync::daemon;
use cirrusync::error::{AppError, FAILURE_EXIT_CODE};
use cirrusync::public_ip::PublicIpClient;
use cirrusync::updater::{CheckReport, CycleReport, Updater};
use clap::{ArgAction, Parser, Subcommand};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "cirrusync",
    version,
    about = "Keep Cloudflare DNS records synchronized with this host's public IP address"
)]
struct Cli {
    /// TOML configuration file.
    #[arg(
        long,
        global = true,
        env = "CIRRUSYNC_CONFIG",
        default_value = DEFAULT_CONFIG_PATH
    )]
    config: PathBuf,

    /// Increase log detail. Repeat for trace-level output.
    #[arg(short, long, global = true, action = ArgAction::Count, conflicts_with = "quiet")]
    verbose: u8,

    /// Show warnings and errors only.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run continuously until SIGINT or SIGTERM.
    Run,

    /// Perform one synchronization cycle and exit.
    Once,

    /// Validate configuration, discovery, authentication, zones, and records.
    Check {
        /// Create configured missing records while checking.
        #[arg(long)]
        allow_create: bool,

        /// Prove DNS Edit with a narrow PATCH; avoid concurrent external edits.
        #[arg(long)]
        allow_edit_probe: bool,
    },

    /// Print a safe example configuration without reading the active file.
    PrintConfig,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Err(error) = init_logging(cli.verbose, cli.quiet) {
        eprintln!("cirrusync: {error}");
        return ExitCode::from(FAILURE_EXIT_CODE);
    }

    match execute(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!(error = %error, "command failed");
            ExitCode::from(FAILURE_EXIT_CODE)
        }
    }
}

async fn execute(cli: Cli) -> Result<(), AppError> {
    if matches!(cli.command, Command::PrintConfig) {
        return write_example_config();
    }

    let updater = build_updater(&cli.config)?;
    match cli.command {
        Command::Run => daemon::run(&updater).await.map_err(AppError::from),
        Command::Once => {
            let report = updater.run_cycle().await;
            report_cycle(&report);
            if report.is_success() {
                Ok(())
            } else {
                Err(AppError::Synchronization(report.to_string()))
            }
        }
        Command::Check {
            allow_create,
            allow_edit_probe,
        } => {
            let report = updater.check(allow_create, allow_edit_probe).await;
            report_check(&report);
            if report.is_success() {
                Ok(())
            } else {
                Err(AppError::Check(report.to_string()))
            }
        }
        Command::PrintConfig => write_example_config(),
    }
}

fn build_updater(config_path: &Path) -> Result<Updater, AppError> {
    let config = Config::load(config_path)?;
    info!(
        path = %config_path.display(),
        records = config.records.len(),
        interval_seconds = config.interval_seconds,
        "configuration loaded"
    );

    let token = config.load_api_token()?;
    let cloudflare = match config.cloudflare.account_id.as_deref() {
        Some(account_id) => {
            CloudflareClient::new_with_account_id(&token, account_id, config.request_timeout())?
        }
        None => CloudflareClient::new(&token, config.request_timeout())?,
    };
    let public_ip = PublicIpClient::new(config.request_timeout())?;
    Updater::new(config, cloudflare, public_ip).map_err(AppError::from)
}

fn report_cycle(report: &CycleReport) {
    for failure in &report.failures {
        error!(
            target_name = %failure.target,
            stage = %failure.stage,
            error = %failure.message,
            "record synchronization failed"
        );
    }

    if report.is_success() {
        info!(%report, "synchronization cycle completed");
    } else {
        warn!(%report, "synchronization cycle completed with failures");
    }
}

fn report_check(report: &CheckReport) {
    for failure in &report.failures {
        error!(
            target_name = %failure.target,
            stage = %failure.stage,
            error = %failure.message,
            "configuration check failed"
        );
    }

    if report.is_success() {
        info!(
            %report,
            authenticated = report.authenticated,
            edit_permission_verified = report.edit_permission_verified,
            "configuration check completed"
        );
        if report.creatable > 0 {
            warn!(
                eligible_missing_records = report.creatable,
                "configured missing records are eligible for creation but were not created"
            );
        }
    }
}

fn write_example_config() -> Result<(), AppError> {
    let mut output = io::stdout().lock();
    output
        .write_all(Config::example_toml().as_bytes())
        .map_err(AppError::Output)
}

fn init_logging(verbose: u8, quiet: bool) -> Result<(), AppError> {
    let rust_log = std::env::var("RUST_LOG").ok();
    let filter = select_log_filter(verbose, quiet, rust_log.as_deref());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .try_init()
        .map_err(|error| AppError::Logging(error.to_string()))
}

fn select_log_filter(verbose: u8, quiet: bool, rust_log: Option<&str>) -> EnvFilter {
    if quiet {
        return EnvFilter::new("cirrusync=warn");
    }
    match verbose {
        0 => rust_log
            .filter(|directives| !directives.trim().is_empty())
            .and_then(|directives| EnvFilter::try_new(directives).ok())
            .unwrap_or_else(|| EnvFilter::new("cirrusync=info")),
        1 => EnvFilter::new("cirrusync=debug"),
        _ => EnvFilter::new("cirrusync=trace"),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command, select_log_filter};

    #[test]
    fn parses_config_before_subcommand() {
        let cli = Cli::try_parse_from([
            "cirrusync",
            "--config",
            "/tmp/test.toml",
            "check",
            "--allow-create",
            "--allow-edit-probe",
        ])
        .expect("CLI should parse");

        assert_eq!(cli.config.to_string_lossy(), "/tmp/test.toml");
        assert!(matches!(
            cli.command,
            Command::Check {
                allow_create: true,
                allow_edit_probe: true
            }
        ));
    }

    #[test]
    fn global_config_is_also_accepted_after_subcommand() {
        let cli = Cli::try_parse_from(["cirrusync", "once", "--config", "/tmp/test.toml"])
            .expect("global CLI option should parse after the subcommand");

        assert!(matches!(cli.command, Command::Once));
        assert_eq!(cli.config.to_string_lossy(), "/tmp/test.toml");
    }

    #[test]
    fn explicit_logging_flags_override_rust_log() {
        assert_eq!(
            select_log_filter(0, true, Some("cirrusync=trace")).to_string(),
            "cirrusync=warn"
        );
        assert_eq!(
            select_log_filter(1, false, Some("cirrusync=error")).to_string(),
            "cirrusync=debug"
        );
        assert_eq!(
            select_log_filter(2, false, Some("cirrusync=error")).to_string(),
            "cirrusync=trace"
        );
    }

    #[test]
    fn rust_log_is_used_without_explicit_logging_flags() {
        assert_eq!(
            select_log_filter(0, false, Some("cirrusync=debug")).to_string(),
            "cirrusync=debug"
        );
        assert_eq!(
            select_log_filter(0, false, Some("not a valid directive[")).to_string(),
            "cirrusync=info"
        );
        assert_eq!(
            select_log_filter(0, false, Some("  ")).to_string(),
            "cirrusync=info"
        );
        assert_eq!(
            select_log_filter(0, false, None).to_string(),
            "cirrusync=info"
        );
    }
}
