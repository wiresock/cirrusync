//! Long-running synchronization loop and graceful shutdown handling.

use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::updater::{CycleControl, CycleReport, FailureStage, Updater};

const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
static JITTER_COUNTER: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

/// Fatal daemon-loop errors.
///
/// Synchronization failures are deliberately absent: they are reported and
/// retried without terminating the service.
#[derive(Debug, Error)]
pub enum DaemonError {
    /// Another process owns the runtime lock or it could not be acquired.
    #[error("could not acquire the exclusive runtime lock: {0}")]
    RuntimeLock(String),

    /// The operating system's signal handler could not be installed.
    #[error("could not wait for a shutdown signal: {0}")]
    Signal(#[source] io::Error),
}

/// Run synchronization until SIGINT or SIGTERM is received.
///
/// A failed cycle uses capped exponential backoff with jitter. Any completely
/// successful cycle resets that backoff and restores the configured interval.
///
/// # Errors
///
/// Returns [`DaemonError::RuntimeLock`] when another process owns the lock (or
/// the lock cannot be opened securely), and [`DaemonError::Signal`] when the
/// operating system's shutdown signal handler cannot be installed or polled.
pub async fn run(updater: &Updater) -> Result<(), DaemonError> {
    let _runtime_lock = updater
        .acquire_runtime_lock()
        .map_err(|error| DaemonError::RuntimeLock(error.to_string()))?;
    info!(
        interval_seconds = updater.config().interval_seconds,
        "daemon started"
    );

    let result = run_loop(updater, shutdown_signal()).await;
    if result.is_ok() {
        info!("graceful shutdown completed");
    }
    result
}

async fn run_loop<F>(updater: &Updater, shutdown: F) -> Result<(), DaemonError>
where
    F: Future<Output = io::Result<()>>,
{
    let interval = Duration::from_secs(updater.config().interval_seconds);
    let mut next_delay = Duration::ZERO;
    let mut consecutive_failures = 0_u32;
    tokio::pin!(shutdown);

    loop {
        if !next_delay.is_zero() {
            tokio::select! {
                signal_result = &mut shutdown => {
                    return signal_result.map_err(DaemonError::Signal);
                }
                () = sleep(next_delay) => {}
            }
        }

        let control = CycleControl::default();
        let cycle = updater.run_cycle_locked(&control);
        tokio::pin!(cycle);
        let report = tokio::select! {
            signal_result = &mut shutdown => {
                control.request_stop();
                if control.mutation_in_flight() {
                    info!("shutdown requested; waiting for the in-flight DNS mutation");
                    let report = cycle.as_mut().await;
                    log_discovery_failures(&report);
                    if !report.is_success() {
                        warn!(%report, "final synchronization cycle ended during shutdown");
                    }
                }
                return signal_result.map_err(DaemonError::Signal);
            }
            report = cycle.as_mut() => report,
        };

        log_discovery_failures(&report);
        if report.is_success() {
            consecutive_failures = 0;
            next_delay = interval;
            info!(%report, "synchronization cycle completed");
        } else {
            consecutive_failures = consecutive_failures.saturating_add(1);
            next_delay = retry_delay(&report, consecutive_failures, interval, jitter_percent());
            warn!(
                %report,
                consecutive_failures,
                retry_seconds = next_delay.as_secs(),
                "synchronization cycle incomplete; retry scheduled"
            );
        }
    }
}

fn log_discovery_failures(report: &CycleReport) {
    for failure in report
        .failures
        .iter()
        .filter(|failure| failure.stage == FailureStage::Discovery)
    {
        warn!(
            target_name = %failure.target,
            error = %failure.message,
            "public IP discovery failed"
        );
    }
}

fn retry_delay(
    report: &CycleReport,
    consecutive_failures: u32,
    interval: Duration,
    jitter_percent: u16,
) -> Duration {
    let policy_delay = if report.has_authentication_failure() {
        MAX_BACKOFF
    } else if report.has_retryable_failure() {
        backoff_delay(consecutive_failures, jitter_percent)
    } else {
        interval
    };
    report
        .retry_after()
        .map_or(policy_delay, |retry_after| policy_delay.max(retry_after))
}

/// Calculate a capped exponential retry delay.
///
/// `jitter_percent` is clamped to 50–100, producing "equal jitter" that never
/// exceeds the hard cap. Passing a value explicitly keeps this function fully
/// deterministic and easy to test.
#[must_use]
pub fn backoff_delay(consecutive_failures: u32, jitter_percent: u16) -> Duration {
    calculate_backoff(
        consecutive_failures,
        INITIAL_BACKOFF,
        MAX_BACKOFF,
        jitter_percent,
    )
}

fn calculate_backoff(
    consecutive_failures: u32,
    initial: Duration,
    maximum: Duration,
    jitter_percent: u16,
) -> Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(31);
    let multiplier = 1_u64 << exponent;
    let uncapped_seconds = initial.as_secs().saturating_mul(multiplier);
    let capped_seconds = uncapped_seconds.min(maximum.as_secs());
    let jitter = u64::from(jitter_percent.clamp(50, 100));
    let jittered_seconds = capped_seconds.saturating_mul(jitter) / 100;
    Duration::from_secs(jittered_seconds.max(1).min(maximum.as_secs()))
}

fn jitter_percent() -> u16 {
    // Mixing wall-clock nanoseconds, PID, and a process-local sequence prevents
    // hosts started together from settling into synchronized retry traffic.
    // This is operational jitter, not cryptographic randomness.
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u64, |duration| {
            duration.as_secs() ^ u64::from(duration.subsec_nanos())
        });
    let sequence = JITTER_COUNTER.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    let mut mixed = timestamp ^ sequence ^ u64::from(std::process::id());
    mixed ^= mixed >> 30;
    mixed = mixed.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed ^= mixed >> 27;
    mixed = mixed.wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^= mixed >> 31;
    u16::try_from(50 + (mixed % 51)).unwrap_or(75)
}

#[cfg(unix)]
async fn shutdown_signal() -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> io::Result<()> {
    tokio::signal::ctrl_c().await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{backoff_delay, calculate_backoff, retry_delay};
    use crate::updater::{CycleFailure, CycleReport, FailureKind, FailureStage};

    #[test]
    fn exponential_backoff_grows_and_is_capped() {
        let initial = Duration::from_secs(5);
        let maximum = Duration::from_secs(60);

        assert_eq!(
            calculate_backoff(1, initial, maximum, 100),
            Duration::from_secs(5)
        );
        assert_eq!(
            calculate_backoff(2, initial, maximum, 100),
            Duration::from_secs(10)
        );
        assert_eq!(
            calculate_backoff(4, initial, maximum, 100),
            Duration::from_secs(40)
        );
        assert_eq!(
            calculate_backoff(20, initial, maximum, 100),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn jitter_is_bounded_and_cap_is_preserved() {
        assert_eq!(backoff_delay(1, 0), Duration::from_secs(2));
        assert_eq!(backoff_delay(1, 500), Duration::from_secs(5));
        assert_eq!(backoff_delay(20, 100), Duration::from_secs(300));
        assert_eq!(backoff_delay(20, 50), Duration::from_secs(150));
    }

    #[test]
    fn first_failure_uses_initial_backoff_even_for_zero_input() {
        assert_eq!(backoff_delay(0, 100), Duration::from_secs(5));
        assert_eq!(backoff_delay(1, 100), Duration::from_secs(5));
    }

    #[test]
    fn server_retry_after_is_a_minimum_for_rate_limit_backoff() {
        let report = CycleReport {
            records: 1,
            failures: vec![CycleFailure {
                target: "home.example.com".to_owned(),
                stage: FailureStage::RecordLookup,
                kind: FailureKind::RateLimited,
                retry_after: Some(Duration::from_secs(47)),
                message: "rate limited".to_owned(),
            }],
            ..CycleReport::default()
        };

        assert_eq!(
            retry_delay(&report, 1, Duration::from_secs(300), 100),
            Duration::from_secs(47)
        );
    }

    #[test]
    fn authentication_failure_uses_the_maximum_backoff() {
        let report = CycleReport {
            records: 1,
            failures: vec![CycleFailure {
                target: "Cloudflare API token".to_owned(),
                stage: FailureStage::Authentication,
                kind: FailureKind::Authentication,
                retry_after: None,
                message: "authentication failed".to_owned(),
            }],
            ..CycleReport::default()
        };

        assert_eq!(
            retry_delay(&report, 1, Duration::from_secs(60), 100),
            Duration::from_secs(300)
        );
    }
}
