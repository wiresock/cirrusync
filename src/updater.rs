//! Synchronization and non-mutating validation workflows.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::time::{Instant, timeout_at};
use tracing::{info, warn};

use crate::cloudflare::{CloudflareClient, CloudflareError, DnsRecord, RecordPayload};
use crate::config::{Config, ConfigError, RecordConfig, RecordType};
use crate::public_ip::{AddressFamily, PublicIpClient};

const CHECK_TIMEOUT: Duration = Duration::from_secs(300);
const MUTATION_START_MARGIN: Duration = Duration::from_secs(1);

/// Retry and scope classification retained after formatting an underlying error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// A local coordination failure, such as another active updater process.
    Coordination,
    /// A token is inactive or invalid for every API operation.
    Authentication,
    /// Cloudflare applied a global request-rate limit.
    RateLimited,
    /// A transport or upstream failure that can reasonably recover.
    Transient,
    /// A record or configuration problem that requires external correction.
    Permanent,
}

/// The operation that failed for one record or check target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureStage {
    /// Exclusive runtime coordination.
    Coordination,
    /// Total synchronization-cycle deadline.
    CycleDeadline,
    /// Public address discovery.
    Discovery,
    /// Cloudflare token verification.
    Authentication,
    /// Zone-ID resolution.
    ZoneLookup,
    /// Exact DNS record lookup.
    RecordLookup,
    /// Updating an existing record.
    Update,
    /// Creating a missing record.
    Creation,
    /// Proving DNS edit permission for a configured zone.
    EditPermission,
}

impl fmt::Display for FailureStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Coordination => "runtime coordination",
            Self::CycleDeadline => "cycle deadline",
            Self::Discovery => "public IP discovery",
            Self::Authentication => "authentication",
            Self::ZoneLookup => "zone lookup",
            Self::RecordLookup => "record lookup",
            Self::Update => "record update",
            Self::Creation => "record creation",
            Self::EditPermission => "DNS edit permission",
        };
        formatter.write_str(name)
    }
}

/// A secret-safe description of an isolated failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleFailure {
    /// Record name or global check target.
    pub target: String,
    /// Operation that failed.
    pub stage: FailureStage,
    /// Retry and global-scope classification.
    pub kind: FailureKind,
    /// Server-requested minimum retry delay, when supplied.
    pub retry_after: Option<Duration>,
    /// Concise error description.
    pub message: String,
}

impl fmt::Display for CycleFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} ({}): {}",
            self.target, self.stage, self.message
        )
    }
}

/// Aggregate result of one synchronization cycle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CycleReport {
    /// Number of configured records considered.
    pub records: usize,
    /// Records whose address was already correct.
    pub unchanged: usize,
    /// Existing records replaced with the discovered address.
    pub updated: usize,
    /// Missing records created by this cycle.
    pub created: usize,
    /// Whether shutdown stopped the cycle after an in-flight mutation completed.
    pub interrupted: bool,
    /// Record- or family-level failures.
    pub failures: Vec<CycleFailure>,
}

impl CycleReport {
    /// Whether every configured record was handled successfully.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.failures.is_empty() && !self.interrupted
    }

    /// Number of reported record- or family-level failures.
    #[must_use]
    pub fn failed(&self) -> usize {
        self.failures.len()
    }

    /// Longest server-requested retry delay reported by the cycle.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        self.failures
            .iter()
            .filter_map(|failure| failure.retry_after)
            .max()
    }

    /// Whether the configured token cannot currently authorize API work.
    #[must_use]
    pub fn has_authentication_failure(&self) -> bool {
        self.failures
            .iter()
            .any(|failure| failure.kind == FailureKind::Authentication)
    }

    /// Whether any failure can reasonably recover without a configuration change.
    #[must_use]
    pub fn has_retryable_failure(&self) -> bool {
        self.failures.iter().any(|failure| {
            matches!(
                failure.kind,
                FailureKind::RateLimited | FailureKind::Transient
            )
        })
    }
}

impl fmt::Display for CycleReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} record(s): {} unchanged, {} updated, {} created, {} failed",
            self.records,
            self.unchanged,
            self.updated,
            self.created,
            self.failed()
        )?;
        if self.interrupted {
            formatter.write_str(", interrupted by shutdown")?;
        }
        Ok(())
    }
}

/// Aggregate result of the `check` workflow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Number of configured records considered.
    pub records: usize,
    /// Whether `/user/tokens/verify` accepted the token as active.
    pub authenticated: bool,
    /// Existing records visible to the API token.
    pub existing: usize,
    /// Missing records that may be created, but were left untouched.
    pub creatable: usize,
    /// Missing records created because `allow_create` was explicitly enabled.
    pub created: usize,
    /// Configured zones for which a DNS write was successfully exercised.
    pub edit_verified_zones: usize,
    /// Whether DNS edit permission was proved for every configured zone.
    pub edit_permission_verified: bool,
    /// Validation failures.
    pub failures: Vec<CycleFailure>,
}

impl CheckReport {
    /// Whether authentication, discovery, zones, and records all validated.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.authenticated && self.edit_permission_verified && self.failures.is_empty()
    }
}

impl fmt::Display for CheckReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} record(s): {} existing, {} creatable, {} created, edit permission verified for {} zone(s), {} failure(s)",
            self.records,
            self.existing,
            self.creatable,
            self.created,
            self.edit_verified_zones,
            self.failures.len()
        )
    }
}

/// Coordinates public-IP discovery with Cloudflare DNS operations.
pub struct Updater {
    config: Arc<Config>,
    cloudflare: CloudflareClient,
    public_ip: PublicIpClient,
    runtime_lock: RuntimeLockIdentity,
    record_cursor: AtomicUsize,
}

impl fmt::Debug for Updater {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Updater")
            .field("record_count", &self.config.records.len())
            .finish_non_exhaustive()
    }
}

impl Updater {
    /// Construct an updater after revalidating all configuration invariants.
    ///
    /// Both an owned [`Config`] and `Arc<Config>` work.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a library caller supplied a configuration that
    /// did not pass [`Config::validate`].
    pub fn new(
        config: impl Into<Arc<Config>>,
        cloudflare: CloudflareClient,
        public_ip: PublicIpClient,
    ) -> Result<Self, ConfigError> {
        let config = config.into();
        config.validate()?;
        Ok(Self {
            runtime_lock: RuntimeLockIdentity::for_config(&config),
            record_cursor: AtomicUsize::new(0),
            config,
            cloudflare,
            public_ip,
        })
    }

    #[cfg(test)]
    fn new_for_test(
        config: impl Into<Arc<Config>>,
        cloudflare: CloudflareClient,
        public_ip: PublicIpClient,
    ) -> Self {
        let config = config.into();
        Self {
            runtime_lock: RuntimeLockIdentity::disabled(),
            record_cursor: AtomicUsize::new(0),
            config,
            cloudflare,
            public_ip,
        }
    }

    /// Access the validated configuration backing this updater.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Discover each required address family once and synchronize every record.
    ///
    /// Failures are collected per record, so a failure in one zone or address
    /// family does not prevent attempts for unrelated records.
    pub async fn run_cycle(&self) -> CycleReport {
        let control = CycleControl::default();
        let mut report = CycleReport {
            records: self.config.records.len(),
            ..CycleReport::default()
        };
        let _runtime_lock = match self.acquire_runtime_lock() {
            Ok(lock) => lock,
            Err(error) => {
                report.failures.push(coordination_failure(&error));
                return report;
            }
        };
        self.run_cycle_locked(&control).await
    }

    pub(crate) async fn run_cycle_locked(&self, control: &CycleControl) -> CycleReport {
        let deadline = Instant::now() + self.config.interval();
        self.run_cycle_until(control, deadline).await
    }

    async fn run_cycle_until(&self, control: &CycleControl, deadline: Instant) -> CycleReport {
        let mut report = CycleReport {
            records: self.config.records.len(),
            ..CycleReport::default()
        };
        let Ok(addresses) = timeout_at(deadline, self.discover_required_addresses()).await else {
            report.failures.push(cycle_deadline_failure());
            return report;
        };
        addresses.append_discovery_failures(&mut report.failures);

        let record_count = self.config.records.len();
        let start_index = if record_count == 0 {
            0
        } else {
            self.record_cursor.fetch_add(1, Ordering::Relaxed) % record_count
        };
        for offset in 0..record_count {
            let record = &self.config.records[(start_index + offset) % record_count];
            if control.stop_requested() {
                report.interrupted = true;
                break;
            }

            let Ok(address) = addresses.for_record(record) else {
                // One family-level failure was already recorded above.
                continue;
            };

            let outcome = match timeout_at(deadline, self.plan_record(record, address)).await {
                Ok(Ok(plan)) => {
                    if Instant::now() >= deadline {
                        report.failures.push(cycle_deadline_failure());
                        break;
                    }
                    self.execute_record_plan(plan, control).await
                }
                Ok(Err(error)) => Err(error),
                Err(_) => {
                    report.failures.push(cycle_deadline_failure());
                    break;
                }
            };

            match outcome {
                Ok(RecordOutcome::Unchanged) => {
                    report.unchanged += 1;
                    info!(
                        record = %record.name,
                        record_type = %record.record_type,
                        "DNS record is already up to date"
                    );
                }
                Ok(RecordOutcome::Updated) => {
                    report.updated += 1;
                    info!(
                        record = %record.name,
                        record_type = %record.record_type,
                        "DNS record updated"
                    );
                }
                Ok(RecordOutcome::Created) => {
                    report.created += 1;
                    info!(
                        record = %record.name,
                        record_type = %record.record_type,
                        "DNS record created"
                    );
                }
                Err((stage, error)) => {
                    let abort_cycle = error.is_authentication_failure() || error.is_rate_limited();
                    warn!(
                        record = %record.name,
                        record_type = %record.record_type,
                        stage = %stage,
                        error = %error,
                        "DNS record synchronization failed"
                    );
                    report
                        .failures
                        .push(cloudflare_failure(record.name.clone(), stage, &error));
                    if abort_cycle {
                        break;
                    }
                }
            }
        }

        report
    }

    /// Validate authentication, public-IP discovery, zone access, and records.
    ///
    /// DNS remains untouched unless `allow_create` or `allow_edit_probe` is true.
    /// Creation is limited to records with `create_if_missing = true`. An edit
    /// probe sends a narrow `PATCH` for one existing record per configured
    /// zone, proving permission without rewriting its address or proxy state.
    /// Because Cloudflare documents no conditional-write precondition for this
    /// endpoint, callers must avoid concurrent external name or TTL edits.
    /// Cloudflare likewise has no conditional create, so enabled creation
    /// requires exclusive external ownership even though the missing state is
    /// rechecked immediately before the request.
    pub async fn check(&self, allow_create: bool, allow_edit_probe: bool) -> CheckReport {
        let mut report = CheckReport {
            records: self.config.records.len(),
            ..CheckReport::default()
        };
        let _runtime_lock = if allow_create || allow_edit_probe {
            match self.acquire_runtime_lock() {
                Ok(lock) => lock,
                Err(error) => {
                    report.failures.push(coordination_failure(&error));
                    return report;
                }
            }
        } else {
            None
        };
        let deadline = Instant::now() + CHECK_TIMEOUT;
        self.check_until(allow_create, allow_edit_probe, deadline)
            .await
    }

    async fn check_until(
        &self,
        allow_create: bool,
        allow_edit_probe: bool,
        deadline: Instant,
    ) -> CheckReport {
        let mut report = CheckReport {
            records: self.config.records.len(),
            ..CheckReport::default()
        };
        let mut proofs = ZoneProofs::for_config(&self.config);

        match timeout_at(deadline, self.cloudflare.verify_token()).await {
            Err(_) => {
                report.failures.push(check_deadline_failure());
                finalize_check_report(&mut report, &proofs, allow_edit_probe);
                return report;
            }
            Ok(Ok(())) => report.authenticated = true,
            Ok(Err(error)) => {
                let abort_api_work = error.is_authentication_failure() || error.is_rate_limited();
                report.failures.push(cloudflare_failure(
                    "Cloudflare API token".to_owned(),
                    FailureStage::Authentication,
                    &error,
                ));
                if abort_api_work {
                    finalize_check_report(&mut report, &proofs, allow_edit_probe);
                    return report;
                }
            }
        }

        let Ok(addresses) = timeout_at(deadline, self.discover_required_addresses()).await else {
            report.failures.push(check_deadline_failure());
            finalize_check_report(&mut report, &proofs, allow_edit_probe);
            return report;
        };
        addresses.append_discovery_failures(&mut report.failures);
        let options = CheckOptions {
            allow_create,
            allow_edit_probe,
            deadline,
        };
        self.check_records(&addresses, options, &mut proofs, &mut report)
            .await;

        finalize_check_report(&mut report, &proofs, allow_edit_probe);
        report
    }

    async fn check_records(
        &self,
        addresses: &DiscoveredAddresses,
        options: CheckOptions,
        proofs: &mut ZoneProofs,
        report: &mut CheckReport,
    ) {
        let control = CycleControl::default();

        for record in &self.config.records {
            let (zone_id, existing) = match self
                .inspect_check_record(record, options.deadline, proofs)
                .await
            {
                Ok(result) => result,
                Err(CheckLookupError::Deadline) => {
                    report.failures.push(check_deadline_failure());
                    break;
                }
                Err(CheckLookupError::Cloudflare(stage, error)) => {
                    let abort = error.is_authentication_failure() || error.is_rate_limited();
                    report
                        .failures
                        .push(cloudflare_failure(record.name.clone(), stage, &error));
                    if abort {
                        break;
                    }
                    continue;
                }
            };

            if let Some(existing) = existing {
                let mut context = CheckRecordContext {
                    options,
                    control: &control,
                    proofs,
                    report,
                };
                let abort = self
                    .handle_existing_check_record(record, &zone_id, &existing, &mut context)
                    .await;
                if abort {
                    break;
                }
                continue;
            }

            let mut context = CheckRecordContext {
                options,
                control: &control,
                proofs,
                report,
            };
            let abort = self
                .handle_missing_check_record(record, &zone_id, addresses, &mut context)
                .await;
            if abort {
                break;
            }
        }
    }

    async fn handle_existing_check_record(
        &self,
        record: &RecordConfig,
        zone_id: &str,
        existing: &DnsRecord,
        context: &mut CheckRecordContext<'_>,
    ) -> bool {
        context.report.existing += 1;
        if !context.options.allow_edit_probe || !context.proofs.needs_proof(zone_id) {
            return false;
        }
        if !self.mutation_can_finish_before(context.options.deadline) {
            context.report.failures.push(check_deadline_failure());
            return true;
        }
        match self
            .probe_edit_permission(zone_id, existing, context.control)
            .await
        {
            Ok(()) => context.proofs.mark_proven(zone_id),
            Err(error) => {
                let abort = error.is_authentication_failure() || error.is_rate_limited();
                context.report.failures.push(cloudflare_failure(
                    record.name.clone(),
                    FailureStage::EditPermission,
                    &error,
                ));
                return abort;
            }
        }
        false
    }

    async fn handle_missing_check_record(
        &self,
        record: &RecordConfig,
        zone_id: &str,
        addresses: &DiscoveredAddresses,
        context: &mut CheckRecordContext<'_>,
    ) -> bool {
        if !record.create_if_missing {
            context.report.failures.push(permanent_failure(
                record.name.clone(),
                FailureStage::RecordLookup,
                "record does not exist and creation is disabled".to_owned(),
            ));
            return false;
        }
        if !context.options.allow_create {
            context.report.creatable += 1;
            return false;
        }

        // The family-level error was already added above.
        let Ok(address) = addresses.for_record(record) else {
            return false;
        };
        if !self.mutation_can_finish_before(context.options.deadline) {
            context.report.failures.push(check_deadline_failure());
            return true;
        }
        let payload = record_payload(record, address);
        let rechecked = match timeout_at(
            context.options.deadline,
            self.cloudflare
                .find_record(zone_id, &payload.record_type, &payload.name),
        )
        .await
        {
            Err(_) => {
                context.report.failures.push(check_deadline_failure());
                return true;
            }
            Ok(Err(error)) => {
                let abort = error.is_authentication_failure() || error.is_rate_limited();
                context.report.failures.push(cloudflare_failure(
                    record.name.clone(),
                    FailureStage::RecordLookup,
                    &error,
                ));
                return abort;
            }
            Ok(Ok(existing)) => existing,
        };
        if let Some(existing) = rechecked {
            return self
                .handle_existing_check_record(record, zone_id, &existing, context)
                .await;
        }
        if !self.mutation_can_finish_before(context.options.deadline) {
            context.report.failures.push(check_deadline_failure());
            return true;
        }

        let _mutation = context.control.begin_mutation();
        match self.cloudflare.create_record(zone_id, &payload).await {
            Ok(_) => {
                context.report.created += 1;
                context.proofs.mark_proven(zone_id);
                false
            }
            Err(error) => {
                let abort = error.is_authentication_failure() || error.is_rate_limited();
                context.report.failures.push(cloudflare_failure(
                    record.name.clone(),
                    FailureStage::Creation,
                    &error,
                ));
                abort
            }
        }
    }

    fn mutation_can_finish_before(&self, deadline: Instant) -> bool {
        let required = self
            .cloudflare
            .request_timeout()
            .saturating_add(MUTATION_START_MARGIN);
        deadline
            .checked_duration_since(Instant::now())
            .is_some_and(|remaining| remaining >= required)
    }

    async fn inspect_check_record(
        &self,
        record: &RecordConfig,
        deadline: Instant,
        proofs: &mut ZoneProofs,
    ) -> Result<(String, Option<DnsRecord>), CheckLookupError> {
        let zone_id = timeout_at(
            deadline,
            self.cloudflare
                .resolve_zone_id(&record.zone, record.zone_id.as_deref()),
        )
        .await
        .map_err(|_| CheckLookupError::Deadline)?
        .map_err(|error| CheckLookupError::Cloudflare(FailureStage::ZoneLookup, error))?;
        proofs.mark_resolved(record, &zone_id);

        let existing = timeout_at(
            deadline,
            self.cloudflare
                .find_record(&zone_id, &record.record_type.to_string(), &record.name),
        )
        .await
        .map_err(|_| CheckLookupError::Deadline)?
        .map_err(|error| CheckLookupError::Cloudflare(FailureStage::RecordLookup, error))?;
        Ok((zone_id, existing))
    }

    async fn probe_edit_permission(
        &self,
        zone_id: &str,
        existing: &DnsRecord,
        control: &CycleControl,
    ) -> Result<(), CloudflareError> {
        let _mutation = control.begin_mutation();
        self.cloudflare.probe_record_edit(zone_id, existing).await?;
        Ok(())
    }

    async fn discover_required_addresses(&self) -> DiscoveredAddresses {
        let needs_ipv4 = self
            .config
            .records
            .iter()
            .any(|record| matches!(record.record_type, RecordType::A));
        let needs_ipv6 = self
            .config
            .records
            .iter()
            .any(|record| matches!(record.record_type, RecordType::Aaaa));

        let ipv4_discovery = async {
            if needs_ipv4 {
                Some(
                    self.public_ip
                        .discover(AddressFamily::Ipv4, &self.config.ipv4.providers)
                        .await
                        .map_err(|error| error.to_string()),
                )
            } else {
                None
            }
        };
        let ipv6_discovery = async {
            if needs_ipv6 {
                Some(
                    self.public_ip
                        .discover(AddressFamily::Ipv6, &self.config.ipv6.providers)
                        .await
                        .map_err(|error| error.to_string()),
                )
            } else {
                None
            }
        };
        let (ipv4, ipv6) = tokio::join!(ipv4_discovery, ipv6_discovery);

        if let Some(Ok(address)) = ipv4.as_ref() {
            info!(address = %address, family = "IPv4", "public address detected");
        }
        if let Some(Ok(address)) = ipv6.as_ref() {
            info!(address = %address, family = "IPv6", "public address detected");
        }

        DiscoveredAddresses { ipv4, ipv6 }
    }

    async fn plan_record(
        &self,
        record: &RecordConfig,
        address: IpAddr,
    ) -> Result<RecordPlan, (FailureStage, CloudflareError)> {
        let zone_id = self
            .cloudflare
            .resolve_zone_id(&record.zone, record.zone_id.as_deref())
            .await
            .map_err(|error| (FailureStage::ZoneLookup, error))?;
        let existing = self
            .cloudflare
            .find_record(&zone_id, &record.record_type.to_string(), &record.name)
            .await
            .map_err(|error| (FailureStage::RecordLookup, error))?;

        if let Some(existing) = existing {
            return Ok(plan_existing_record(record, address, zone_id, existing));
        }

        if !record.create_if_missing {
            return Err((
                FailureStage::RecordLookup,
                CloudflareError::RecordNotFound {
                    record_type: record.record_type.to_string(),
                    name: record.name.clone(),
                },
            ));
        }

        let rechecked = self
            .cloudflare
            .find_record(&zone_id, &record.record_type.to_string(), &record.name)
            .await
            .map_err(|error| (FailureStage::RecordLookup, error))?;
        if let Some(existing) = rechecked {
            return Ok(plan_existing_record(record, address, zone_id, existing));
        }

        let payload = record_payload(record, address);
        Ok(RecordPlan::Create { zone_id, payload })
    }

    async fn execute_record_plan(
        &self,
        plan: RecordPlan,
        control: &CycleControl,
    ) -> Result<RecordOutcome, (FailureStage, CloudflareError)> {
        match plan {
            RecordPlan::Unchanged => Ok(RecordOutcome::Unchanged),
            RecordPlan::Update {
                zone_id,
                record_id,
                payload,
            } => {
                let _mutation = control.begin_mutation();
                self.cloudflare
                    .update_record(&zone_id, &record_id, &payload)
                    .await
                    .map_err(|error| (FailureStage::Update, error))?;
                Ok(RecordOutcome::Updated)
            }
            RecordPlan::Create { zone_id, payload } => {
                let _mutation = control.begin_mutation();
                self.cloudflare
                    .create_record(&zone_id, &payload)
                    .await
                    .map_err(|error| (FailureStage::Creation, error))?;
                Ok(RecordOutcome::Created)
            }
        }
    }

    pub(crate) fn acquire_runtime_lock(
        &self,
    ) -> Result<Option<RuntimeLockGuard>, RuntimeLockError> {
        self.runtime_lock.acquire()
    }
}

/// Coordinates daemon shutdown with request cancellation.
///
/// Read-only work may be dropped immediately. Once a mutation begins, the
/// daemon waits for that bounded HTTP request to finish and the updater stops
/// before starting another record.
#[derive(Debug, Default)]
pub(crate) struct CycleControl {
    stop_requested: AtomicBool,
    mutation_in_flight: AtomicBool,
}

impl CycleControl {
    pub(crate) fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
    }

    pub(crate) fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }

    pub(crate) fn mutation_in_flight(&self) -> bool {
        self.mutation_in_flight.load(Ordering::Acquire)
    }

    fn begin_mutation(&self) -> MutationGuard<'_> {
        self.mutation_in_flight.store(true, Ordering::Release);
        MutationGuard { control: self }
    }
}

struct MutationGuard<'a> {
    control: &'a CycleControl,
}

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        self.control
            .mutation_in_flight
            .store(false, Ordering::Release);
    }
}

#[derive(Debug)]
struct RuntimeLockIdentity {
    resource_hashes: Vec<u64>,
    lock_root: PathBuf,
    temporary_root: bool,
    disabled: bool,
}

impl RuntimeLockIdentity {
    fn for_config(config: &Config) -> Self {
        #[cfg(unix)]
        let installed = match std::fs::symlink_metadata("/var/lib/cirrusync") {
            Ok(_) => true,
            Err(error) => error.kind() != io::ErrorKind::NotFound,
        };
        #[cfg(not(unix))]
        let installed = false;
        let (lock_root, temporary_root) = if installed {
            (PathBuf::from("/var/lib/cirrusync"), false)
        } else {
            (temporary_lock_root(), true)
        };
        let mut resource_hashes = config
            .records
            .iter()
            .map(resource_lock_hash)
            .collect::<Vec<_>>();
        resource_hashes.sort_unstable();
        resource_hashes.dedup();
        Self {
            resource_hashes,
            lock_root,
            temporary_root,
            disabled: false,
        }
    }

    #[cfg(test)]
    fn disabled() -> Self {
        Self {
            resource_hashes: Vec::new(),
            lock_root: PathBuf::new(),
            temporary_root: false,
            disabled: true,
        }
    }

    fn acquire(&self) -> Result<Option<RuntimeLockGuard>, RuntimeLockError> {
        if self.disabled {
            return Ok(None);
        }
        if self.resource_hashes.is_empty() {
            return Err(RuntimeLockError::Insecure {
                path: PathBuf::new(),
                reason: "no managed DNS resources were available to lock".to_owned(),
            });
        }
        let prepared = prepare_lock_parent(&self.lock_root, self.temporary_root)?;
        let mut files = Vec::with_capacity(self.resource_hashes.len());
        for path in self.lock_paths_in(&prepared.path) {
            let file = open_runtime_lock(&path, prepared.expected_owner)?;
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => files.push(file),
                Err(source) if is_lock_contended(&source) => {
                    return Err(RuntimeLockError::AlreadyRunning { path });
                }
                Err(source) => {
                    return Err(RuntimeLockError::Lock { path, source });
                }
            }
        }
        Ok(Some(RuntimeLockGuard { files }))
    }

    fn lock_paths_in(&self, parent: &Path) -> Vec<PathBuf> {
        self.resource_hashes
            .iter()
            .map(|hash| parent.join(format!("record-{hash:016x}.lock")))
            .collect()
    }

    #[cfg(test)]
    fn resolved_lock_paths(&self) -> Result<Vec<PathBuf>, RuntimeLockError> {
        let prepared = prepare_lock_parent(&self.lock_root, self.temporary_root)?;
        Ok(self.lock_paths_in(&prepared.path))
    }
}

fn is_lock_contended(error: &io::Error) -> bool {
    let expected = fs2::lock_contended_error();
    error.kind() == io::ErrorKind::WouldBlock
        || (error.raw_os_error().is_some() && error.raw_os_error() == expected.raw_os_error())
}

fn resource_lock_hash(record: &RecordConfig) -> u64 {
    let identity = format!(
        "{}\0{}\0{}",
        canonical_lock_name(&record.zone),
        record.record_type,
        canonical_lock_name(&record.name)
    );
    fnv1a_hash(identity.as_bytes())
}

fn canonical_lock_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn temporary_lock_root() -> PathBuf {
    #[cfg(unix)]
    {
        let system_temporary = Path::new("/tmp");
        std::fs::canonicalize(system_temporary).unwrap_or_else(|_| system_temporary.to_path_buf())
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir()
    }
}

const fn fnv1a_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    hash
}

#[derive(Clone, Copy)]
struct ExpectedLockOwner {
    #[cfg(unix)]
    uid: u32,
    #[cfg(not(unix))]
    _private_parent: bool,
}

struct PreparedLockParent {
    path: PathBuf,
    expected_owner: ExpectedLockOwner,
}

fn prepare_lock_parent(
    root: &Path,
    temporary: bool,
) -> Result<PreparedLockParent, RuntimeLockError> {
    if temporary {
        prepare_temporary_lock_parent(root)
    } else {
        prepare_installed_lock_parent(root)
    }
}

fn prepare_temporary_lock_parent(root: &Path) -> Result<PreparedLockParent, RuntimeLockError> {
    #[cfg(unix)]
    let (parent, current_owner) = {
        validate_private_lock_container(root)?;
        let current_owner = current_file_owner(root)?;
        (
            root.join(format!("cirrusync-runtime-{current_owner}")),
            current_owner,
        )
    };
    #[cfg(not(unix))]
    let parent = root.join("cirrusync-runtime");
    create_private_lock_directory(&parent)?;
    #[cfg(unix)]
    let metadata = lock_directory_metadata(&parent)?;
    #[cfg(not(unix))]
    lock_directory_metadata(&parent)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(RuntimeLockError::Insecure {
                path: parent,
                reason: format!("private lock directory mode {mode:04o} is too permissive"),
            });
        }
        validate_owner_match(&parent, metadata.uid(), current_owner)?;
        Ok(PreparedLockParent {
            path: parent,
            expected_owner: ExpectedLockOwner { uid: current_owner },
        })
    }
    #[cfg(not(unix))]
    {
        Ok(PreparedLockParent {
            path: parent,
            expected_owner: ExpectedLockOwner {
                _private_parent: true,
            },
        })
    }
}

fn prepare_installed_lock_parent(root: &Path) -> Result<PreparedLockParent, RuntimeLockError> {
    #[cfg(unix)]
    let metadata = lock_directory_metadata(root)?;
    #[cfg(not(unix))]
    lock_directory_metadata(root)?;
    #[cfg(unix)]
    {
        let current_owner = current_file_owner(&temporary_lock_root())?;
        validate_installed_lock_parent(root, &metadata, current_owner)
    }
    #[cfg(not(unix))]
    {
        Ok(PreparedLockParent {
            path: root.to_path_buf(),
            expected_owner: ExpectedLockOwner {
                _private_parent: false,
            },
        })
    }
}

#[cfg(unix)]
fn validate_installed_lock_parent(
    root: &Path,
    metadata: &std::fs::Metadata,
    current_owner: u32,
) -> Result<PreparedLockParent, RuntimeLockError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(RuntimeLockError::Insecure {
            path: root.to_path_buf(),
            reason: format!(
                "installed lock directory mode {mode:04o} permits group or other writes"
            ),
        });
    }
    let parent_owner = metadata.uid();
    validate_owner_match(root, parent_owner, current_owner)?;
    Ok(PreparedLockParent {
        path: root.to_path_buf(),
        expected_owner: ExpectedLockOwner { uid: parent_owner },
    })
}

fn create_private_lock_directory(parent: &Path) -> Result<(), RuntimeLockError> {
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = DirBuilder::new();
    match builder.create(parent) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(RuntimeLockError::Open {
            path: parent.to_path_buf(),
            source,
        }),
    }
}

fn lock_directory_metadata(path: &Path) -> Result<std::fs::Metadata, RuntimeLockError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| RuntimeLockError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RuntimeLockError::Insecure {
            path: path.to_path_buf(),
            reason: "lock parent must be a real directory, not a symbolic link".to_owned(),
        });
    }
    Ok(metadata)
}

#[cfg(unix)]
fn validate_owner_match(
    path: &Path,
    parent_owner: u32,
    current_owner: u32,
) -> Result<(), RuntimeLockError> {
    if parent_owner != current_owner {
        return Err(RuntimeLockError::Insecure {
            path: path.to_path_buf(),
            reason: format!(
                "lock directory owner {parent_owner} does not match process owner {current_owner}"
            ),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_lock_container(probe_root: &Path) -> Result<(), RuntimeLockError> {
    use std::os::unix::fs::PermissionsExt;

    let root_metadata =
        std::fs::symlink_metadata(probe_root).map_err(|source| RuntimeLockError::Open {
            path: probe_root.to_path_buf(),
            source,
        })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(RuntimeLockError::Insecure {
            path: probe_root.to_path_buf(),
            reason: "ownership-probe parent must be a real directory".to_owned(),
        });
    }
    let root_mode = root_metadata.permissions().mode();
    if root_mode & 0o022 != 0 && root_mode & 0o1000 == 0 {
        return Err(RuntimeLockError::Insecure {
            path: probe_root.to_path_buf(),
            reason: "ownership-probe parent is writable by other users without the sticky bit"
                .to_owned(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn current_file_owner(probe_root: &Path) -> Result<u32, RuntimeLockError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::time::{SystemTime, UNIX_EPOCH};

    static OWNER_PROBE_COUNTER: AtomicUsize = AtomicUsize::new(0);

    validate_private_lock_container(probe_root)?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..16_u32 {
        let sequence = OWNER_PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let probe_path = probe_root.join(format!(
            ".cirrusync-owner-{}-{timestamp:032x}-{sequence:016x}-{attempt:02x}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true).mode(0o600);
        #[cfg(target_os = "linux")]
        options.custom_flags(0o2_000_000 | 0o4_000 | 0o400_000);
        match options.open(&probe_path) {
            Ok(file) => {
                let owner_result = file.metadata().map(|metadata| metadata.uid());
                let cleanup_result = std::fs::remove_file(&probe_path);
                let owner = owner_result.map_err(|source| RuntimeLockError::Open {
                    path: probe_path.clone(),
                    source,
                })?;
                cleanup_result.map_err(|source| RuntimeLockError::Open {
                    path: probe_path,
                    source,
                })?;
                return Ok(owner);
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(RuntimeLockError::Open {
                    path: probe_path,
                    source,
                });
            }
        }
    }
    Err(RuntimeLockError::Insecure {
        path: probe_root.to_path_buf(),
        reason: "could not create a collision-free ownership probe".to_owned(),
    })
}

fn open_runtime_lock(
    path: &Path,
    expected_owner: ExpectedLockOwner,
) -> Result<File, RuntimeLockError> {
    #[cfg(unix)]
    let expected_owner_uid = expected_owner.uid;
    #[cfg(not(unix))]
    let _ = expected_owner;

    #[cfg(unix)]
    validate_existing_lock_target(path, expected_owner_uid)?;

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;

        const O_NOFOLLOW: i32 = 0o400_000;
        const O_CLOEXEC: i32 = 0o2_000_000;
        const O_NONBLOCK: i32 = 0o4_000;
        options
            .mode(0o600)
            .custom_flags(O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK);
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|source| RuntimeLockError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    let opened = file.metadata().map_err(|source| RuntimeLockError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let named = std::fs::symlink_metadata(path).map_err(|source| RuntimeLockError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    if named.file_type().is_symlink() || !opened.is_file() {
        return Err(RuntimeLockError::Insecure {
            path: path.to_path_buf(),
            reason: "lock target must be a regular file, not a symbolic link".to_owned(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if named.dev() != opened.dev() || named.ino() != opened.ino() {
            return Err(RuntimeLockError::Insecure {
                path: path.to_path_buf(),
                reason: "lock path changed while it was being opened".to_owned(),
            });
        }
        if opened.nlink() != 1 {
            return Err(RuntimeLockError::Insecure {
                path: path.to_path_buf(),
                reason: "hard-linked lock files are not accepted".to_owned(),
            });
        }
        if opened.uid() != expected_owner_uid {
            return Err(RuntimeLockError::Insecure {
                path: path.to_path_buf(),
                reason: format!(
                    "lock file owner {} does not match lock directory owner {expected_owner_uid}",
                    opened.uid()
                ),
            });
        }
        let mode = opened.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(RuntimeLockError::Insecure {
                path: path.to_path_buf(),
                reason: format!("lock file mode {mode:04o} is too permissive"),
            });
        }
    }
    Ok(file)
}

#[cfg(unix)]
fn validate_existing_lock_target(path: &Path, expected_owner: u32) -> Result<(), RuntimeLockError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(RuntimeLockError::Open {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(RuntimeLockError::Insecure {
            path: path.to_path_buf(),
            reason: "existing lock target must be a regular, non-symbolic-link file".to_owned(),
        });
    }
    if metadata.uid() != expected_owner {
        return Err(RuntimeLockError::Insecure {
            path: path.to_path_buf(),
            reason: format!(
                "lock file owner {} does not match lock directory owner {expected_owner}",
                metadata.uid()
            ),
        });
    }
    if metadata.nlink() != 1 {
        return Err(RuntimeLockError::Insecure {
            path: path.to_path_buf(),
            reason: "hard-linked lock files are not accepted".to_owned(),
        });
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(RuntimeLockError::Insecure {
            path: path.to_path_buf(),
            reason: format!("lock file mode {mode:04o} is too permissive"),
        });
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct RuntimeLockGuard {
    files: Vec<File>,
}

impl Drop for RuntimeLockGuard {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = fs2::FileExt::unlock(file);
        }
    }
}

#[derive(Debug)]
pub(crate) enum RuntimeLockError {
    Open { path: PathBuf, source: io::Error },
    Insecure { path: PathBuf, reason: String },
    AlreadyRunning { path: PathBuf },
    Lock { path: PathBuf, source: io::Error },
}

impl fmt::Display for RuntimeLockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, source } => write!(
                formatter,
                "could not open runtime lock target {}: {source}",
                path.display()
            ),
            Self::AlreadyRunning { path } => write!(
                formatter,
                "another Cirrusync process already holds runtime lock {}",
                path.display()
            ),
            Self::Insecure { path, reason } => {
                write!(
                    formatter,
                    "unsafe runtime lock {}: {reason}",
                    path.display()
                )
            }
            Self::Lock { path, source } => write!(
                formatter,
                "could not lock runtime target {}: {source}",
                path.display()
            ),
        }
    }
}

#[derive(Debug)]
enum CheckLookupError {
    Deadline,
    Cloudflare(FailureStage, CloudflareError),
}

#[derive(Debug, Clone, Copy)]
struct CheckOptions {
    allow_create: bool,
    allow_edit_probe: bool,
    deadline: Instant,
}

struct CheckRecordContext<'a> {
    options: CheckOptions,
    control: &'a CycleControl,
    proofs: &'a mut ZoneProofs,
    report: &'a mut CheckReport,
}

#[derive(Debug)]
enum RecordPlan {
    Unchanged,
    Update {
        zone_id: String,
        record_id: String,
        payload: RecordPayload,
    },
    Create {
        zone_id: String,
        payload: RecordPayload,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordOutcome {
    Unchanged,
    Updated,
    Created,
}

#[derive(Debug)]
struct DiscoveredAddresses {
    ipv4: Option<Result<IpAddr, String>>,
    ipv6: Option<Result<IpAddr, String>>,
}

impl DiscoveredAddresses {
    fn for_record(&self, record: &RecordConfig) -> Result<IpAddr, String> {
        let discovered = match record.record_type {
            RecordType::A => self.ipv4.as_ref(),
            RecordType::Aaaa => self.ipv6.as_ref(),
        };

        match discovered {
            Some(Ok(address)) => Ok(*address),
            Some(Err(message)) => Err(message.clone()),
            None => Err(format!(
                "{} address discovery was not configured",
                record.record_type
            )),
        }
    }

    fn append_discovery_failures(&self, failures: &mut Vec<CycleFailure>) {
        if let Some(Err(message)) = self.ipv4.as_ref() {
            failures.push(CycleFailure {
                target: "public IPv4 address".to_owned(),
                stage: FailureStage::Discovery,
                kind: FailureKind::Transient,
                retry_after: None,
                message: message.clone(),
            });
        }
        if let Some(Err(message)) = self.ipv6.as_ref() {
            failures.push(CycleFailure {
                target: "public IPv6 address".to_owned(),
                stage: FailureStage::Discovery,
                kind: FailureKind::Transient,
                retry_after: None,
                message: message.clone(),
            });
        }
    }
}

fn record_payload(record: &RecordConfig, address: IpAddr) -> RecordPayload {
    RecordPayload {
        record_type: record.record_type.to_string(),
        name: record.name.clone(),
        content: address.to_string(),
        ttl: record.ttl,
        proxied: record.proxied,
    }
}

fn cloudflare_failure(
    target: String,
    stage: FailureStage,
    error: &CloudflareError,
) -> CycleFailure {
    let kind = if error.is_authentication_failure() {
        FailureKind::Authentication
    } else if error.is_rate_limited() {
        FailureKind::RateLimited
    } else if error.is_transient() {
        FailureKind::Transient
    } else {
        FailureKind::Permanent
    };
    CycleFailure {
        target,
        stage,
        kind,
        retry_after: error.retry_after(),
        message: error.to_string(),
    }
}

fn permanent_failure(target: String, stage: FailureStage, message: String) -> CycleFailure {
    CycleFailure {
        target,
        stage,
        kind: FailureKind::Permanent,
        retry_after: None,
        message,
    }
}

fn coordination_failure(error: &RuntimeLockError) -> CycleFailure {
    CycleFailure {
        target: "runtime lock".to_owned(),
        stage: FailureStage::Coordination,
        kind: FailureKind::Coordination,
        retry_after: None,
        message: error.to_string(),
    }
}

fn cycle_deadline_failure() -> CycleFailure {
    CycleFailure {
        target: "synchronization cycle".to_owned(),
        stage: FailureStage::CycleDeadline,
        kind: FailureKind::Transient,
        retry_after: None,
        message: "cycle deadline expired before all read-only work completed".to_owned(),
    }
}

fn check_deadline_failure() -> CycleFailure {
    CycleFailure {
        target: "configuration check".to_owned(),
        stage: FailureStage::CycleDeadline,
        kind: FailureKind::Transient,
        retry_after: None,
        message: "check deadline expired before all configured work completed".to_owned(),
    }
}

fn plan_existing_record(
    record: &RecordConfig,
    address: IpAddr,
    zone_id: String,
    existing: DnsRecord,
) -> RecordPlan {
    if existing.content.parse::<IpAddr>().ok() == Some(address)
        && existing.ttl == record.ttl
        && existing.proxied == record.proxied
    {
        return RecordPlan::Unchanged;
    }

    RecordPlan::Update {
        zone_id,
        record_id: existing.id,
        payload: record_payload(record, address),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ConfiguredZone {
    name: String,
    explicit_id: Option<String>,
}

impl ConfiguredZone {
    fn for_record(record: &RecordConfig) -> Self {
        Self {
            name: record.zone.to_ascii_lowercase(),
            explicit_id: record.zone_id.clone(),
        }
    }

    fn display_target(&self, resolved_id: Option<&str>) -> String {
        resolved_id.or(self.explicit_id.as_deref()).map_or_else(
            || self.name.clone(),
            |zone_id| format!("{} (zone ID {zone_id})", self.name),
        )
    }
}

#[derive(Debug)]
struct ZoneProofs {
    expected: HashSet<ConfiguredZone>,
    resolved: HashMap<ConfiguredZone, String>,
    proven_ids: HashSet<String>,
}

impl ZoneProofs {
    fn for_config(config: &Config) -> Self {
        Self {
            expected: config
                .records
                .iter()
                .map(ConfiguredZone::for_record)
                .collect(),
            resolved: HashMap::new(),
            proven_ids: HashSet::new(),
        }
    }

    fn mark_resolved(&mut self, record: &RecordConfig, zone_id: &str) {
        self.resolved
            .insert(ConfiguredZone::for_record(record), zone_id.to_owned());
    }

    fn needs_proof(&self, zone_id: &str) -> bool {
        !self.proven_ids.contains(zone_id)
    }

    fn mark_proven(&mut self, zone_id: &str) {
        self.proven_ids.insert(zone_id.to_owned());
    }

    fn verified_count(&self) -> usize {
        self.resolved
            .values()
            .filter(|zone_id| self.proven_ids.contains(zone_id.as_str()))
            .collect::<HashSet<_>>()
            .len()
    }

    fn is_complete(&self) -> bool {
        self.resolved.len() == self.expected.len()
            && self
                .resolved
                .values()
                .all(|zone_id| self.proven_ids.contains(zone_id))
    }

    fn unverified_targets(&self) -> Vec<String> {
        let mut targets = BTreeMap::new();
        for expected in &self.expected {
            match self.resolved.get(expected) {
                Some(zone_id) if self.proven_ids.contains(zone_id) => {}
                Some(zone_id) => {
                    targets
                        .entry(format!("resolved:{zone_id}"))
                        .or_insert_with(|| expected.display_target(Some(zone_id)));
                }
                None => {
                    let key = format!(
                        "unresolved:{}:{}",
                        expected.name,
                        expected.explicit_id.as_deref().unwrap_or_default()
                    );
                    targets
                        .entry(key)
                        .or_insert_with(|| expected.display_target(None));
                }
            }
        }
        targets.into_values().collect()
    }
}

fn finalize_check_report(
    report: &mut CheckReport,
    proofs: &ZoneProofs,
    edit_probe_requested: bool,
) {
    report.edit_verified_zones = proofs.verified_count();
    report.edit_permission_verified = proofs.is_complete();
    for target in proofs.unverified_targets() {
        let message = if edit_probe_requested {
            "DNS edit permission could not be proved for this zone".to_owned()
        } else {
            "DNS edit permission was not exercised; rerun check with --allow-edit-probe".to_owned()
        };
        report.failures.push(permanent_failure(
            target,
            FailureStage::EditPermission,
            message,
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use secrecy::SecretString;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::time::{Instant, sleep};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{CHECK_TIMEOUT, CycleControl, FailureKind, RuntimeLockError, Updater};
    use crate::cloudflare::CloudflareClient;
    use crate::config::{
        CloudflareConfig, Config, ConfigError, IpDiscoveryConfig, MAX_INTERVAL_SECONDS,
        RecordConfig, RecordType,
    };
    use crate::public_ip::PublicIpClient;

    fn envelope(result: serde_json::Value) -> serde_json::Value {
        let mut value = json!({
            "success": true,
            "errors": [],
            "messages": [],
            "result": null,
            "result_info": {"total_pages": 1}
        });
        value["result"] = result;
        value
    }

    fn record(name: &str, create_if_missing: bool) -> RecordConfig {
        RecordConfig {
            zone: "example.com".to_owned(),
            zone_id: Some("0123456789abcdef0123456789abcdef".to_owned()),
            name: name.to_owned(),
            record_type: RecordType::A,
            ttl: 120,
            proxied: false,
            create_if_missing,
        }
    }

    fn config(ip_provider: String, records: Vec<RecordConfig>) -> Config {
        Config {
            interval_seconds: 300,
            request_timeout_seconds: 2,
            cloudflare: CloudflareConfig {
                api_token_file: std::env::temp_dir().join("cirrusync-unused-token"),
            },
            ipv4: IpDiscoveryConfig {
                enabled: true,
                providers: vec![ip_provider],
            },
            ipv6: IpDiscoveryConfig {
                enabled: false,
                providers: vec![],
            },
            records,
        }
    }

    fn dns_record(id: &str, name: &str, content: &str) -> serde_json::Value {
        json!({
            "id": id,
            "type": "A",
            "name": name,
            "content": content,
            "ttl": 120,
            "proxied": false
        })
    }

    async fn updater(
        cloudflare_server: &MockServer,
        ip_server: &MockServer,
        records: Vec<RecordConfig>,
    ) -> Updater {
        Mock::given(method("GET"))
            .and(path("/client/v4/zones/0123456789abcdef0123456789abcdef"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "id": "0123456789abcdef0123456789abcdef",
                "name": "example.com",
                "status": "active"
            }))))
            .expect(1)
            .mount(cloudflare_server)
            .await;
        let token = SecretString::new("test-token".to_owned());
        let cloudflare = CloudflareClient::new_for_test(
            &token,
            Duration::from_secs(2),
            &format!("{}/client/v4/", cloudflare_server.uri()),
        )
        .expect("test Cloudflare client should be valid");
        let public_ip = PublicIpClient::new_for_tests(Duration::from_secs(2))
            .expect("test IP client should be valid");
        Updater::new_for_test(
            config(format!("{}/ip", ip_server.uri()), records),
            cloudflare,
            public_ip,
        )
    }

    async fn mock_public_ip(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/ip"))
            .respond_with(ResponseTemplate::new(200).set_body_string("1.1.1.1\n"))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn no_change_does_not_send_an_update() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .and(query_param("name.exact", "home.example.com"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(json!([dns_record(
                    "record-id",
                    "home.example.com",
                    "1.1.1.1"
                )]))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;

        let report = updater(&cloudflare, &ip, vec![record("home.example.com", false)])
            .await
            .run_cycle()
            .await;

        assert!(report.is_success());
        assert_eq!(report.unchanged, 1);
        let requests = cloudflare
            .received_requests()
            .await
            .expect("request recording should be enabled");
        assert!(
            requests
                .iter()
                .all(|request| request.method.as_str() != "PATCH")
        );
    }

    #[tokio::test]
    async fn changed_address_updates_the_record() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(json!([dns_record(
                    "record-id",
                    "home.example.com",
                    "8.8.8.8"
                )]))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;
        Mock::given(method("PATCH"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records/record-id",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(dns_record(
                    "record-id",
                    "home.example.com",
                    "1.1.1.1",
                ))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;

        let report = updater(&cloudflare, &ip, vec![record("home.example.com", false)])
            .await
            .run_cycle()
            .await;

        assert!(report.is_success());
        assert_eq!(report.updated, 1);
    }

    #[tokio::test]
    async fn metadata_drift_is_reconciled_even_when_address_matches() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([{
                "id": "record-id",
                "type": "A",
                "name": "home.example.com",
                "content": "1.1.1.1",
                "ttl": 300,
                "proxied": false
            }]))))
            .expect(1)
            .mount(&cloudflare)
            .await;
        Mock::given(method("PATCH"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records/record-id",
            ))
            .and(wiremock::matchers::body_json(json!({
                "type": "A",
                "name": "home.example.com",
                "content": "1.1.1.1",
                "ttl": 120,
                "proxied": false
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(dns_record(
                    "record-id",
                    "home.example.com",
                    "1.1.1.1",
                ))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;

        let report = updater(&cloudflare, &ip, vec![record("home.example.com", false)])
            .await
            .run_cycle()
            .await;

        assert!(report.is_success());
        assert_eq!(report.updated, 1);
        assert_eq!(report.unchanged, 0);
    }

    #[tokio::test]
    async fn missing_record_is_created_only_when_configured() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([]))))
            .expect(2)
            .mount(&cloudflare)
            .await;
        Mock::given(method("POST"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(dns_record(
                    "new-record-id",
                    "new.example.com",
                    "1.1.1.1",
                ))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;

        let report = updater(&cloudflare, &ip, vec![record("new.example.com", true)])
            .await
            .run_cycle()
            .await;

        assert!(report.is_success());
        assert_eq!(report.created, 1);
    }

    #[tokio::test]
    async fn concurrent_create_seen_by_recheck_does_not_create_a_duplicate() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        let lookup_count = Arc::new(AtomicUsize::new(0));
        let responder_count = Arc::clone(&lookup_count);
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .and(query_param("name.exact", "new.example.com"))
            .respond_with(move |_request: &wiremock::Request| {
                let result = if responder_count.fetch_add(1, Ordering::Relaxed) == 0 {
                    json!([])
                } else {
                    json!([dns_record(
                        "concurrent-record-id",
                        "new.example.com",
                        "1.1.1.1"
                    )])
                };
                ResponseTemplate::new(200).set_body_json(envelope(result))
            })
            .expect(2)
            .mount(&cloudflare)
            .await;
        Mock::given(method("POST"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&cloudflare)
            .await;

        let report = updater(&cloudflare, &ip, vec![record("new.example.com", true)])
            .await
            .run_cycle()
            .await;

        assert!(report.is_success());
        assert_eq!(report.unchanged, 1);
        assert_eq!(report.created, 0);
        assert_eq!(lookup_count.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn one_missing_record_does_not_prevent_other_records() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .and(query_param("name.exact", "missing.example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([]))))
            .expect(1)
            .mount(&cloudflare)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .and(query_param("name.exact", "healthy.example.com"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(json!([dns_record(
                    "healthy-id",
                    "healthy.example.com",
                    "1.1.1.1"
                )]))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;

        let report = updater(
            &cloudflare,
            &ip,
            vec![
                record("missing.example.com", false),
                record("healthy.example.com", false),
            ],
        )
        .await
        .run_cycle()
        .await;

        assert!(!report.is_success());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.unchanged, 1);
    }

    #[tokio::test]
    async fn check_reports_creatable_record_without_modifying_dns() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path("/client/v4/user/tokens/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "id": "token-id",
                "status": "active"
            }))))
            .expect(1)
            .mount(&cloudflare)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([]))))
            .expect(1)
            .mount(&cloudflare)
            .await;

        let report = updater(&cloudflare, &ip, vec![record("new.example.com", true)])
            .await
            .check(false, false)
            .await;

        assert!(!report.is_success());
        assert_eq!(report.creatable, 1);
        assert!(!report.edit_permission_verified);
        let requests = cloudflare
            .received_requests()
            .await
            .expect("request recording should be enabled");
        assert!(
            requests
                .iter()
                .all(|request| request.method.as_str() != "POST")
        );
    }

    #[tokio::test]
    async fn edit_probe_proves_permission_without_changing_record_state() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path("/client/v4/user/tokens/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "id": "token-id",
                "status": "active"
            }))))
            .expect(1)
            .mount(&cloudflare)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(json!([dns_record(
                    "record-id",
                    "home.example.com",
                    "1.1.1.1"
                )]))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;
        let current_state = json!({
            "type": "A",
            "name": "home.example.com",
            "ttl": 120
        });
        Mock::given(method("PATCH"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records/record-id",
            ))
            .and(wiremock::matchers::body_json(current_state))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(dns_record(
                    "record-id",
                    "home.example.com",
                    "1.1.1.1",
                ))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;

        let report = updater(&cloudflare, &ip, vec![record("home.example.com", false)])
            .await
            .check(false, true)
            .await;

        assert!(report.is_success());
        assert!(report.edit_permission_verified);
        assert_eq!(report.edit_verified_zones, 1);
        assert_eq!(report.existing, 1);
    }

    #[tokio::test]
    async fn check_does_not_start_a_mutation_without_a_full_request_budget() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path("/client/v4/user/tokens/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "id": "token-id",
                "status": "active"
            }))))
            .expect(1)
            .mount(&cloudflare)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(json!([dns_record(
                    "record-id",
                    "home.example.com",
                    "1.1.1.1",
                )]))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;
        let updater = updater(&cloudflare, &ip, vec![record("home.example.com", false)]).await;

        let report = updater
            .check_until(false, true, Instant::now() + Duration::from_secs(1))
            .await;

        assert!(
            report
                .failures
                .iter()
                .any(|failure| { failure.stage == super::FailureStage::CycleDeadline })
        );
        let requests = cloudflare
            .received_requests()
            .await
            .expect("request recording should be enabled");
        assert!(
            requests
                .iter()
                .all(|request| request.method.as_str() != "PATCH")
        );
    }

    #[tokio::test]
    async fn check_create_recheck_uses_a_concurrently_created_record() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        let lookup_count = Arc::new(AtomicUsize::new(0));
        let responder_count = Arc::clone(&lookup_count);
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path("/client/v4/user/tokens/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "id": "token-id",
                "status": "active"
            }))))
            .expect(1)
            .mount(&cloudflare)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .and(query_param("name.exact", "new.example.com"))
            .respond_with(move |_request: &wiremock::Request| {
                let result = if responder_count.fetch_add(1, Ordering::Relaxed) == 0 {
                    json!([])
                } else {
                    json!([dns_record(
                        "concurrent-record-id",
                        "new.example.com",
                        "1.1.1.1"
                    )])
                };
                ResponseTemplate::new(200).set_body_json(envelope(result))
            })
            .expect(2)
            .mount(&cloudflare)
            .await;
        Mock::given(method("PATCH"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records/concurrent-record-id",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(dns_record(
                    "concurrent-record-id",
                    "new.example.com",
                    "1.1.1.1",
                ))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;
        Mock::given(method("POST"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&cloudflare)
            .await;

        let report = updater(&cloudflare, &ip, vec![record("new.example.com", true)])
            .await
            .check(true, true)
            .await;

        assert!(report.is_success());
        assert_eq!(report.existing, 1);
        assert_eq!(report.created, 0);
        assert!(report.edit_permission_verified);
        assert_eq!(lookup_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn mutation_budget_uses_the_cloudflare_clients_actual_timeout() {
        let token = SecretString::new("test-token".to_owned());
        let cloudflare = CloudflareClient::new_for_test(
            &token,
            Duration::from_secs(30),
            "http://127.0.0.1:1/client/v4/",
        )
        .expect("test Cloudflare client should be valid");
        let public_ip = PublicIpClient::new_for_tests(Duration::from_secs(2))
            .expect("test IP client should be valid");
        let updater = Updater::new_for_test(
            config(
                "http://127.0.0.1:1/ip".to_owned(),
                vec![record("home.example.com", false)],
            ),
            cloudflare,
            public_ip,
        );

        assert!(!updater.mutation_can_finish_before(Instant::now() + Duration::from_secs(10)));
    }

    #[test]
    fn public_constructor_rejects_noncanonical_record_names() {
        let token = SecretString::new("test-token".to_owned());
        let cloudflare = CloudflareClient::new_for_test(
            &token,
            Duration::from_secs(2),
            "http://127.0.0.1:1/client/v4/",
        )
        .expect("test Cloudflare client should be valid");
        let public_ip = PublicIpClient::new_for_tests(Duration::from_secs(2))
            .expect("test IP client should be valid");
        let mut invalid = config(
            "https://127.0.0.1:1/ip".to_owned(),
            vec![record("home.example.com", false)],
        );
        invalid.records[0].name = " Home.Example.COM. ".to_owned();

        assert!(matches!(
            Updater::new(invalid, cloudflare, public_ip),
            Err(ConfigError::Validation { ref field, .. }) if field == "records[0].name"
        ));
    }

    #[tokio::test]
    async fn edit_probe_is_required_for_each_verified_zone_id() {
        const SECOND_ZONE_ID: &str = "fedcba9876543210fedcba9876543210";

        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path("/client/v4/user/tokens/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "id": "token-id",
                "status": "active"
            }))))
            .expect(1)
            .mount(&cloudflare)
            .await;
        for zone_id in ["0123456789abcdef0123456789abcdef", SECOND_ZONE_ID] {
            Mock::given(method("GET"))
                .and(path(format!("/client/v4/zones/{zone_id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                    "id": zone_id,
                    "name": "example.com",
                    "status": "active"
                }))))
                .expect(1)
                .mount(&cloudflare)
                .await;
        }
        for (zone_id, record_id, name) in [
            (
                "0123456789abcdef0123456789abcdef",
                "first-id",
                "first.example.com",
            ),
            (SECOND_ZONE_ID, "second-id", "second.example.com"),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/client/v4/zones/{zone_id}/dns_records")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(envelope(json!([dns_record(record_id, name, "1.1.1.1")]))),
                )
                .expect(1)
                .mount(&cloudflare)
                .await;
            Mock::given(method("PATCH"))
                .and(path(format!(
                    "/client/v4/zones/{zone_id}/dns_records/{record_id}"
                )))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(envelope(dns_record(record_id, name, "1.1.1.1"))),
                )
                .expect(1)
                .mount(&cloudflare)
                .await;
        }

        let first = record("first.example.com", false);
        let mut second = record("second.example.com", false);
        second.zone_id = Some(SECOND_ZONE_ID.to_owned());
        let token = SecretString::new("test-token".to_owned());
        let client = CloudflareClient::new_for_test(
            &token,
            Duration::from_secs(2),
            &format!("{}/client/v4/", cloudflare.uri()),
        )
        .expect("test Cloudflare client should be valid");
        let public_ip = PublicIpClient::new_for_tests(Duration::from_secs(2))
            .expect("test IP client should be valid");
        let updater = Updater::new_for_test(
            config(format!("{}/ip", ip.uri()), vec![first, second]),
            client,
            public_ip,
        );

        let report = updater.check(false, true).await;

        assert!(report.is_success());
        assert_eq!(report.edit_verified_zones, 2);
    }

    #[tokio::test]
    async fn rate_limit_metadata_survives_cycle_aggregation() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "47")
                    .set_body_json(json!({
                        "success": false,
                        "errors": [{"code": 10000, "message": "rate limited"}],
                        "result": null
                    })),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;

        let report = updater(&cloudflare, &ip, vec![record("home.example.com", false)])
            .await
            .run_cycle()
            .await;

        assert!(!report.is_success());
        assert_eq!(report.retry_after(), Some(Duration::from_secs(47)));
        assert_eq!(report.failures[0].kind, FailureKind::RateLimited);
    }

    #[tokio::test]
    async fn zone_scoped_forbidden_does_not_skip_unrelated_records() {
        const FIRST_ZONE_ID: &str = "0123456789abcdef0123456789abcdef";
        const SECOND_ZONE_ID: &str = "fedcba9876543210fedcba9876543210";

        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        mock_public_ip(&ip).await;
        for (zone_id, zone_name) in [
            (FIRST_ZONE_ID, "example.com"),
            (SECOND_ZONE_ID, "example.net"),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/client/v4/zones/{zone_id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                    "id": zone_id,
                    "name": zone_name,
                    "status": "active"
                }))))
                .expect(1)
                .mount(&cloudflare)
                .await;
        }
        Mock::given(method("GET"))
            .and(path(format!(
                "/client/v4/zones/{FIRST_ZONE_ID}/dns_records"
            )))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "success": false,
                "errors": [{"code": 10000, "message": "DNS Read denied"}],
                "result": null
            })))
            .expect(1)
            .mount(&cloudflare)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/client/v4/zones/{SECOND_ZONE_ID}/dns_records"
            )))
            .and(query_param("name.exact", "home.example.net"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(json!([dns_record(
                    "second-id",
                    "home.example.net",
                    "1.1.1.1"
                )]))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;

        let first = record("home.example.com", false);
        let mut second = record("home.example.net", false);
        second.zone = "example.net".to_owned();
        second.zone_id = Some(SECOND_ZONE_ID.to_owned());
        let token = SecretString::new("test-token".to_owned());
        let client = CloudflareClient::new_for_test(
            &token,
            Duration::from_secs(2),
            &format!("{}/client/v4/", cloudflare.uri()),
        )
        .expect("test Cloudflare client should be valid");
        let public_ip = PublicIpClient::new_for_tests(Duration::from_secs(2))
            .expect("test IP client should be valid");
        let updater = Updater::new_for_test(
            config(format!("{}/ip", ip.uri()), vec![first, second]),
            client,
            public_ip,
        );

        let report = updater.run_cycle().await;

        assert_eq!(report.unchanged, 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].kind, FailureKind::Permanent);
        assert_eq!(report.failures[0].stage, super::FailureStage::RecordLookup);
    }

    #[tokio::test]
    async fn shutdown_waits_for_active_mutation_then_stops_before_next_record() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        mock_public_ip(&ip).await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .and(query_param("name.exact", "first.example.com"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(json!([dns_record(
                    "first-id",
                    "first.example.com",
                    "8.8.8.8"
                )]))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;
        Mock::given(method("PATCH"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records/first-id",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(250))
                    .set_body_json(envelope(dns_record(
                        "first-id",
                        "first.example.com",
                        "1.1.1.1",
                    ))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .and(query_param("name.exact", "second.example.com"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(json!([dns_record(
                    "second-id",
                    "second.example.com",
                    "8.8.4.4"
                )]))),
            )
            .expect(0)
            .mount(&cloudflare)
            .await;

        let updater = Arc::new(
            updater(
                &cloudflare,
                &ip,
                vec![
                    record("first.example.com", false),
                    record("second.example.com", false),
                ],
            )
            .await,
        );
        let control = Arc::new(CycleControl::default());
        let task = {
            let updater = Arc::clone(&updater);
            let control = Arc::clone(&control);
            tokio::spawn(async move { updater.run_cycle_locked(&control).await })
        };

        for _ in 0..100 {
            if control.mutation_in_flight() {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert!(
            control.mutation_in_flight(),
            "test mutation should have started"
        );
        control.request_stop();

        let report = task.await.expect("cycle task should complete");
        assert!(report.interrupted);
        assert!(!report.is_success());
        assert_eq!(report.updated, 1);
    }

    #[tokio::test]
    async fn stable_runtime_lock_survives_atomic_token_replacement() {
        let cloudflare = MockServer::start().await;
        let directory = tempdir().expect("temporary directory should be created");
        let token_path = directory.path().join("token");
        fs::write(&token_path, "old-token").expect("initial token should be written");
        let mut test_config = config(
            "https://example.invalid/ip".to_owned(),
            vec![record("atomic-lock.example.com", false)],
        );
        test_config.cloudflare.api_token_file = token_path.clone();
        let token = SecretString::new("test-token".to_owned());
        let client = CloudflareClient::new_for_test(
            &token,
            Duration::from_secs(2),
            &format!("{}/client/v4/", cloudflare.uri()),
        )
        .expect("test Cloudflare client should be valid");
        let public_ip =
            PublicIpClient::new(Duration::from_secs(2)).expect("test IP client should be valid");
        let mut first = Updater::new(test_config.clone(), client.clone(), public_ip.clone())
            .expect("first updater should be valid");
        first.runtime_lock.lock_root = directory.path().to_path_buf();
        first.runtime_lock.temporary_root = true;
        let guard = first
            .acquire_runtime_lock()
            .expect("first updater should acquire the lock");
        let replacement = directory.path().join("replacement-token");
        fs::write(&replacement, "new-token").expect("replacement token should be written");
        #[cfg(windows)]
        fs::remove_file(&token_path).expect("old token should be removed before replacement");
        fs::rename(&replacement, &token_path).expect("token should be replaced");

        let mut second =
            Updater::new(test_config, client, public_ip).expect("second updater should be valid");
        second.runtime_lock.lock_root = directory.path().to_path_buf();
        second.runtime_lock.temporary_root = true;
        match second.acquire_runtime_lock() {
            Err(RuntimeLockError::AlreadyRunning { .. }) => {}
            other => panic!("expected a contended runtime lock, got {other:?}"),
        }
        let lock_paths = first
            .runtime_lock
            .resolved_lock_paths()
            .expect("test lock paths should resolve");
        assert!(
            lock_paths.iter().all(|lock_path| lock_path != &token_path),
            "runtime locks must not reuse the replaceable token inode"
        );
        drop(guard);
        let second_guard = second
            .acquire_runtime_lock()
            .expect("lock should be released when its guard is dropped");
        drop(second_guard);
        for lock_path in lock_paths {
            fs::remove_file(&lock_path).expect("test lock file should be removable");
        }
    }

    #[tokio::test]
    async fn runtime_locks_cover_only_overlapping_dns_resources() {
        let cloudflare = MockServer::start().await;
        let directory = tempdir().expect("temporary directory should be created");
        let first_token_path = directory.path().join("first-token");
        let copied_token_path = directory.path().join("copied-token");
        fs::write(&first_token_path, "first-token").expect("first token should be written");
        fs::write(&copied_token_path, "copied-token").expect("copied token should be written");

        let mut home_config = config(
            "https://example.invalid/ip".to_owned(),
            vec![record("overlap-lock.example.com", false)],
        );
        home_config.cloudflare.api_token_file = first_token_path.clone();
        let mut office_config = config(
            "https://example.invalid/ip".to_owned(),
            vec![record("disjoint-lock.example.com", false)],
        );
        office_config.cloudflare.api_token_file = first_token_path;
        let mut copied_home_config = home_config.clone();
        copied_home_config.cloudflare.api_token_file = copied_token_path;

        let token = SecretString::new("test-token".to_owned());
        let client = CloudflareClient::new_for_test(
            &token,
            Duration::from_secs(2),
            &format!("{}/client/v4/", cloudflare.uri()),
        )
        .expect("test Cloudflare client should be valid");
        let public_ip =
            PublicIpClient::new(Duration::from_secs(2)).expect("test IP client should be valid");
        let mut home = Updater::new(home_config, client.clone(), public_ip.clone())
            .expect("home updater should be valid");
        let mut office = Updater::new(office_config, client.clone(), public_ip.clone())
            .expect("office updater should be valid");
        let mut copied_home = Updater::new(copied_home_config, client, public_ip)
            .expect("copied-token updater should be valid");
        for updater in [&mut home, &mut office, &mut copied_home] {
            updater.runtime_lock.lock_root = directory.path().to_path_buf();
            updater.runtime_lock.temporary_root = true;
        }

        assert_ne!(
            home.runtime_lock.resource_hashes,
            office.runtime_lock.resource_hashes
        );
        assert_eq!(
            home.runtime_lock.resource_hashes,
            copied_home.runtime_lock.resource_hashes
        );
        let home_guard = home
            .acquire_runtime_lock()
            .expect("home resource should be lockable");
        let office_guard = office
            .acquire_runtime_lock()
            .expect("a disjoint resource should remain lockable");
        match copied_home.acquire_runtime_lock() {
            Err(RuntimeLockError::AlreadyRunning { .. }) => {}
            other => panic!("expected copied-token overlap to contend, got {other:?}"),
        }

        drop(office_guard);
        drop(home_guard);
        let mut lock_paths = home
            .runtime_lock
            .resolved_lock_paths()
            .expect("home lock paths should resolve");
        lock_paths.extend(
            office
                .runtime_lock
                .resolved_lock_paths()
                .expect("office lock paths should resolve"),
        );
        lock_paths.sort();
        lock_paths.dedup();
        for lock_path in lock_paths {
            fs::remove_file(lock_path).expect("test resource lock should be removable");
        }
    }

    #[cfg(unix)]
    #[test]
    fn installed_lock_parent_rejects_owner_mismatch_before_lock_creation() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempdir().expect("temporary directory should be created");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o750))
            .expect("installed-style directory mode should be set");
        let metadata =
            fs::metadata(directory.path()).expect("installed-style directory should be readable");
        let actual_owner = metadata.uid();
        let different_owner = actual_owner.wrapping_add(1);
        let would_be_lock = directory.path().join("record-poison.lock");

        assert!(matches!(
            super::validate_installed_lock_parent(directory.path(), &metadata, different_owner),
            Err(RuntimeLockError::Insecure { .. })
        ));
        assert!(
            !would_be_lock.exists(),
            "an unauthorized process must be rejected before a persistent lock is created"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fallback_lock_parent_is_namespaced_by_verified_process_owner() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempdir().expect("temporary directory should be created");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("temporary root mode should be private");
        let prepared = super::prepare_temporary_lock_parent(directory.path())
            .expect("temporary lock parent should be prepared");
        let owner = fs::metadata(&prepared.path)
            .expect("temporary lock parent should exist")
            .uid();

        assert_eq!(
            prepared
                .path
                .file_name()
                .expect("temporary lock parent should have a name")
                .to_string_lossy(),
            format!("cirrusync-runtime-{owner}")
        );
        assert_eq!(prepared.expected_owner.uid, owner);
    }

    #[cfg(unix)]
    #[test]
    fn existing_lock_file_must_match_the_authorized_parent_owner() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempdir().expect("temporary directory should be created");
        let lock_path = directory.path().join("record-existing.lock");
        fs::write(&lock_path, "").expect("existing lock should be created");
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600))
            .expect("existing lock mode should be private");
        let actual_owner = fs::metadata(&lock_path)
            .expect("existing lock metadata should be readable")
            .uid();

        assert!(matches!(
            super::validate_existing_lock_target(&lock_path, actual_owner.wrapping_add(1)),
            Err(RuntimeLockError::Insecure { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_lock_rejects_precreated_symbolic_and_hard_links() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempdir().expect("temporary directory should be created");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("temporary directory mode should be private");
        let victim = directory.path().join("victim");
        fs::write(&victim, "not a lock").expect("victim should be created");
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600))
            .expect("victim mode should be private");
        let resource_hash = super::fnv1a_hash(b"link-test-resource");
        let lock_path = directory
            .path()
            .join(format!("record-{resource_hash:016x}.lock"));
        let identity = super::RuntimeLockIdentity {
            resource_hashes: vec![resource_hash],
            lock_root: directory.path().to_path_buf(),
            temporary_root: false,
            disabled: false,
        };

        symlink(&victim, &lock_path).expect("symbolic link should be created");
        assert!(
            matches!(
                identity.acquire(),
                Err(RuntimeLockError::Open { .. } | RuntimeLockError::Insecure { .. })
            ),
            "symbolic lock targets must not be followed"
        );
        fs::remove_file(&lock_path).expect("symbolic link should be removable");

        fs::hard_link(&victim, &lock_path).expect("hard link should be created");
        assert!(
            matches!(identity.acquire(), Err(RuntimeLockError::Insecure { .. })),
            "precreated hard-linked lock targets must be rejected"
        );
    }

    #[tokio::test]
    async fn dual_stack_discovery_runs_address_families_concurrently() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ipv4"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(200))
                    .set_body_string("1.1.1.1\n"),
            )
            .expect(1)
            .mount(&ip)
            .await;
        Mock::given(method("GET"))
            .and(path("/ipv6"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(200))
                    .set_body_string("2606:4700:4700::1111\n"),
            )
            .expect(1)
            .mount(&ip)
            .await;

        let mut aaaa_record = record("v6.example.com", false);
        aaaa_record.record_type = RecordType::Aaaa;
        let mut test_config = config(
            format!("{}/ipv4", ip.uri()),
            vec![record("v4.example.com", false), aaaa_record],
        );
        test_config.ipv6.enabled = true;
        test_config.ipv6.providers = vec![format!("{}/ipv6", ip.uri())];
        let token = SecretString::new("test-token".to_owned());
        let client = CloudflareClient::new_for_test(
            &token,
            Duration::from_secs(2),
            &format!("{}/client/v4/", cloudflare.uri()),
        )
        .expect("test Cloudflare client should be valid");
        let public_ip = PublicIpClient::new_for_tests(Duration::from_secs(2))
            .expect("test IP client should be valid");
        let updater = Updater::new_for_test(test_config, client, public_ip);

        let started = Instant::now();
        let addresses = updater.discover_required_addresses().await;

        assert!(
            started.elapsed() < Duration::from_millis(350),
            "family discovery should overlap rather than take two full delays"
        );
        assert_eq!(
            addresses
                .for_record(&record("v4.example.com", false))
                .expect("IPv4 should be discovered")
                .to_string(),
            "1.1.1.1"
        );
    }

    #[tokio::test]
    async fn deadline_limited_cycles_rotate_the_first_record() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ip"))
            .respond_with(ResponseTemplate::new(200).set_body_string("1.1.1.1\n"))
            .expect(2)
            .mount(&ip)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .and(query_param("name.exact", "slow.example.com"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(300))
                    .set_body_json(envelope(json!([dns_record(
                        "slow-id",
                        "slow.example.com",
                        "1.1.1.1"
                    )]))),
            )
            .expect(2)
            .mount(&cloudflare)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/client/v4/zones/0123456789abcdef0123456789abcdef/dns_records",
            ))
            .and(query_param("name.exact", "fast.example.com"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(json!([dns_record(
                    "fast-id",
                    "fast.example.com",
                    "1.1.1.1"
                )]))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;
        let updater = updater(
            &cloudflare,
            &ip,
            vec![
                record("slow.example.com", false),
                record("fast.example.com", false),
            ],
        )
        .await;
        let control = CycleControl::default();

        let first = updater
            .run_cycle_until(&control, Instant::now() + Duration::from_millis(80))
            .await;
        let second = updater
            .run_cycle_until(&control, Instant::now() + Duration::from_millis(80))
            .await;

        assert_eq!(first.unchanged, 0);
        assert!(
            first
                .failures
                .iter()
                .any(|failure| { failure.stage == super::FailureStage::CycleDeadline })
        );
        assert_eq!(
            second.unchanged, 1,
            "the second cycle should give the previously-starved record first access"
        );
        assert!(
            second
                .failures
                .iter()
                .any(|failure| { failure.stage == super::FailureStage::CycleDeadline })
        );
    }

    #[tokio::test]
    async fn check_has_a_total_deadline() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/client/v4/user/tokens/verify"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(500))
                    .set_body_json(envelope(json!({
                        "id": "token-id",
                        "status": "active"
                    }))),
            )
            .expect(1)
            .mount(&cloudflare)
            .await;
        let token = SecretString::new("test-token".to_owned());
        let client = CloudflareClient::new_for_test(
            &token,
            Duration::from_secs(2),
            &format!("{}/client/v4/", cloudflare.uri()),
        )
        .expect("test Cloudflare client should be valid");
        let public_ip = PublicIpClient::new_for_tests(Duration::from_secs(2))
            .expect("test IP client should be valid");
        let updater = Updater::new_for_test(
            config(
                format!("{}/ip", ip.uri()),
                vec![record("home.example.com", false)],
            ),
            client,
            public_ip,
        );

        let started = Instant::now();
        let report = updater
            .check_until(false, false, Instant::now() + Duration::from_millis(50))
            .await;

        assert!(
            started.elapsed() < Duration::from_millis(300),
            "check should stop at its own deadline, not the HTTP timeout"
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| { failure.stage == super::FailureStage::CycleDeadline })
        );
    }

    #[test]
    fn check_timeout_is_independent_of_the_polling_interval() {
        let mut config = config(
            "https://ip.example.test".to_owned(),
            vec![record("home.example.com", false)],
        );
        config.interval_seconds = MAX_INTERVAL_SECONDS;

        assert_eq!(CHECK_TIMEOUT, Duration::from_secs(300));
        assert!(CHECK_TIMEOUT < config.interval());
    }

    #[tokio::test]
    async fn expired_cycle_deadline_stops_before_api_work() {
        let cloudflare = MockServer::start().await;
        let ip = MockServer::start().await;
        let token = SecretString::new("test-token".to_owned());
        let client = CloudflareClient::new_for_test(
            &token,
            Duration::from_secs(2),
            &format!("{}/client/v4/", cloudflare.uri()),
        )
        .expect("test Cloudflare client should be valid");
        let public_ip = PublicIpClient::new_for_tests(Duration::from_secs(2))
            .expect("test IP client should be valid");
        let updater = Updater::new_for_test(
            config(
                format!("{}/ip", ip.uri()),
                vec![record("home.example.com", false)],
            ),
            client,
            public_ip,
        );

        let report = updater
            .run_cycle_until(
                &CycleControl::default(),
                Instant::now() - Duration::from_millis(1),
            )
            .await;

        assert!(!report.is_success());
        assert_eq!(report.failures[0].stage, super::FailureStage::CycleDeadline);
        assert_eq!(report.failures[0].kind, FailureKind::Transient);
        let requests = cloudflare
            .received_requests()
            .await
            .expect("request recording should be enabled");
        assert!(requests.is_empty());
    }
}
