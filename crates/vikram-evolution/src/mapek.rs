//! MAPE-K Loop: Monitor-Analyze-Plan-Execute over shared Knowledge.
//!
//! This engine orchestrates the complete autonomous management loop following
//! the MAPE-K autonomic computing architecture.
//!
//! # Monitoring is measured, not simulated
//!
//! Every value [`MapekLoop::monitor`] returns is derived from the shared
//! [`EvolutionStore`] (via [`crate::metrics`]) or from a real host probe:
//!
//! - runtime health is folded from recorded experience outcomes, reflection
//!   job states, proposal states and live memory counts, and *is allowed to be*
//!   `Degraded`/`Unhealthy`/`Unknown` — there is no hardcoded `Healthy`;
//! - resource metrics are real host readings and say in `notes` which value
//!   came from where (and which is unavailable on this platform): `/proc` load
//!   and memory, the mounted filesystem's used share from `df -Pk`, and the
//!   count of failed systemd units from `systemctl --failed`. A probe that
//!   cannot answer is recorded as unknown, never as `0`: an unread filesystem
//!   is not an empty one, and a missing `systemctl` is not a healthy host.
//!   The evolution store's own footprint is reported as an informational note
//!   only — it is not the disk figure health is judged on;
//! - provider status is the failure rate and mean duration of recorded
//!   experiences (`sample_count` says how many), and reports
//!   `available: false` when nothing has been recorded to judge by;
//! - execution outcomes and failures are the actual stored experiences and
//!   failed reflection jobs inside the recency window;
//! - network state is three-valued [`NetworkRoute`]: a default route from the
//!   host's `/proc/net/route`, no default route, or `Unknown` when that file
//!   cannot be read. A change is reported only on the transition — as
//!   [`MonitoringData::network_transition`] and as one analysis issue — while
//!   the reading that was compared against is persisted, so the hourly
//!   self-check cannot replay the same transition. A route is never reported
//!   as a reached endpoint; reachability is its own three-valued
//!   [`EndpointReading`] from a timed connection to `<gateway>:53`, where a
//!   refused connection still proves the host answers and an unreadable table
//!   is `Unknown` (reported once as [`MonitoringData::endpoint_transition`]);
//! - disk fill and failed system services use that same shape: the `df` reading
//!   and the failed-unit count are `Unknown` when their probe cannot answer
//!   (which raises no issue and leaves the baseline untouched), and each
//!   condition is reported once per crossing — as
//!   [`MonitoringData::disk_transition`] / [`MonitoringData::services_transition`]
//!   and one analysis issue — with the baseline kept durably under
//!   `mapek.disk` / `mapek.services`. Known disk readings also accumulate in a
//!   bounded 24-sample history; a projection is only made from at least three
//!   samples spanning an hour, never from one reading.
//! - the machine itself is a [`HostProfile`]: online CPUs, total memory, the
//!   workspace filesystem's capacity and free space, the failed-service count
//!   and whether a default route exists, read by the same probes. It is
//!   captured on first request, kept under `mapek.host` and rendered by
//!   [`describe_host_profile`] as one prompt-sized line, so the model of the
//!   host survives a restart instead of moving with the moment.
//!
//! When the store holds no evidence the loop reports `Unknown`/unavailable with
//! an explanation instead of inventing numbers, and `Healthy` is only claimed
//! once at least [`metrics::MIN_SAMPLE`] experiences back it.
//!
//! # Execution is bounded by what the loop can actually do
//!
//! [`MapekLoop`] owns one handle: the shared [`EvolutionStore`]. It has no
//! broker, provider client, scheduler, agent registry or runtime-configuration
//! handle, so it cannot restart a provider, switch a model, edit a skill or
//! change a runtime setting. [`MapekLoop::execute_action`] therefore reports
//! three distinct outcomes ([`ActionOutcome`]) instead of one optimistic one:
//!
//! | outcome | what happened | `success` |
//! |---|---|---|
//! | [`ActionOutcome::Remediated`] | a change was written through the store and read back (re-queueing failed reflection jobs) | `true` |
//! | [`ActionOutcome::RecordedForReview`] | a pending `record_issue` proposal was persisted for an operator; nothing was remediated | `false` |
//! | [`ActionOutcome::NotExecuted`] | nothing was done and nothing recorded; `error` says why | `false` |
//!
//! [`ExecutionResult::overall_success`] is `true` only for a non-empty plan in
//! which every action was remediated, so a cycle never reports "executed" for
//! work that was only written down. See [`ExecutionResult::failure_reason`].
//!
//! # Reachability
//!
//! PARTIALLY LIVE: [`MapekLoop::monitor`] and [`MapekLoop::analyze`] are
//! store probes called by the runtime's hourly self-check and by
//! `vikram evolution mapek`. Neither touches the knowledge base; `monitor`
//! persists only its own host baselines (default route, endpoint reachability,
//! disk fill and history, failed services), and only when a readable reading
//! crosses.
//!
//! [`MapekLoop::plan`], [`MapekLoop::execute`] and [`MapekLoop::run_cycle`] are
//! still dormant — nothing schedules a mutating cycle — but the state they
//! mutate (`KnowledgeBase`, `cycle_count`) is now durable: it is written to the
//! store's `engine_records` table by [`MapekLoop::update_knowledge`] and
//! reloaded by [`MapekLoop::new`], so decisions, outcomes and cycle
//! numbering survive a restart. `analyze`/`plan` stay pure reads and write
//! nothing.

use crate::error::Result;
use crate::metrics;
use crate::store::{EvolutionStore, from_json_value, to_json_value};
use crate::types::{
    ExperienceOutcome, LearningEvent, LearningProposal, ProposalKind, ProposalStatus,
    ReflectionJobStatus, gen_id, now_ms,
};
use serde::{Deserialize, Serialize};

/// Recency window used for every "recent" judgement in this module.
const WINDOW_MS: u64 = metrics::DEFAULT_WINDOW_MS;
/// Failure share at or above which the experience pipeline is `Unhealthy`.
const UNHEALTHY_FAILURE_RATE: f32 = 0.5;
/// Failure share at or above which the experience pipeline is `Degraded`.
const DEGRADED_FAILURE_RATE: f32 = 0.25;
/// Error share at or above which the experience pipeline is `Degraded`.
const DEGRADED_ERROR_RATE: f32 = 0.5;
/// Pending `record_issue` proposals at or above which issue tracking is
/// `Unhealthy` (below it, any such proposal is `Degraded`).
const UNRESOLVED_ISSUES_UNHEALTHY: usize = 3;
/// Proposal backlog (pending) at or above which learning is `Degraded`.
const PROPOSAL_BACKLOG_DEGRADED: usize = 20;
/// Reflection backlog (pending + running) at or above which it is `Degraded`.
const REFLECTION_BACKLOG_DEGRADED: usize = 20;
/// Rejection share of decided proposals at or above which learning is
/// `Degraded` (only judged with at least [`metrics::MIN_SAMPLE`] proposals).
const REJECTION_RATE_DEGRADED: f32 = 0.5;
/// Most execution outcomes / failures reported per monitoring pass.
const MAX_OUTCOMES: usize = 100;
const MAX_FAILURES: usize = 50;
/// Share of the mounted filesystem used at/above which disk fill is reported
/// (Medium), and the higher band that reports it as High.
const DISK_FILL_MEDIUM_SHARE: f32 = 0.90;
const DISK_FILL_HIGH_SHARE: f32 = 0.95;
/// Failed systemd units at/above which the host has a real service problem.
const FAILED_SERVICES_THRESHOLD: usize = 1;
/// Action target naming the reflection subsystem — the one component the loop
/// has a real actuator for (see [`MapekLoop::execute_action`]).
const REFLECTION_TARGET: &str = "reflection";
/// How many times a reflection job may fail before the loop stops re-queuing
/// it and hands it to an operator instead.
const MAX_REFLECTION_ATTEMPTS: u32 = 3;
/// `agent_id` the loop stamps on the records it persists.
const MAPEK_AGENT: &str = "mapek";
/// `engine_records` namespace + id holding the persisted loop state
/// ([`MapekState`]).
const STATE_NAMESPACE: &str = "mapek.state";
const STATE_ID: &str = "state";
/// `engine_records` namespace + id holding the last route reading
/// ([`NetworkState`]), so a transition is reported once even when the probe
/// runs from a fresh [`MapekLoop`] (the CLI) or after a restart.
const NETWORK_NAMESPACE: &str = "mapek.network";
const NETWORK_ID: &str = "default_route";
/// `engine_records` namespace + id holding the last disk band ([`DiskState`]).
const DISK_NAMESPACE: &str = "mapek.disk";
const DISK_ID: &str = "fill";
/// `engine_records` namespace + id holding the last failed-service reading
/// ([`ServiceState`]).
const SERVICES_NAMESPACE: &str = "mapek.services";
const SERVICES_ID: &str = "failed";
/// `engine_records` namespace + id holding the last endpoint reading
/// ([`EndpointState`]).
const ENDPOINT_NAMESPACE: &str = "mapek.endpoint";
const ENDPOINT_ID: &str = "reachability";
/// `engine_records` namespace + id holding the captured host profile
/// ([`HostProfile`]).
const HOST_NAMESPACE: &str = "mapek.host";
const HOST_ID: &str = "profile";
/// Bytes per gibibyte, used when rendering [`HostProfile`] figures.
const GIB_BYTES: f64 = 1024.0 * 1024.0 * 1024.0;
/// `engine_records` namespace + id holding the bounded disk history
/// ([`DiskHistory`]).
const DISK_HISTORY_NAMESPACE: &str = "mapek.disk_history";
const DISK_HISTORY_ID: &str = "samples";
/// Port on the default gateway the endpoint probe contacts, and its budget.
const ENDPOINT_PORT: u16 = 53;
const ENDPOINT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
/// Newest samples kept in the disk history, oldest dropped first.
const DISK_HISTORY_LIMIT: usize = 24;
/// Samples and span required before a disk trend may be claimed.
const DISK_PROJECTION_MIN_SAMPLES: usize = 3;
const DISK_PROJECTION_MIN_SPAN_MS: u64 = 3_600_000;
/// Projection bands: only a fill predicted within this horizon is reported,
/// at Medium when it is under a day and Low otherwise.
const DISK_PROJECTION_HORIZON_HOURS: f32 = 72.0;
const DISK_PROJECTION_MEDIUM_HOURS: f32 = 24.0;
const DISK_PROJECTION_TARGET_SHARE: f32 = 0.95;

/// Monitoring data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitoringData {
    pub id: String,
    pub timestamp_ms: u64,
    pub runtime_health: RuntimeHealth,
    pub resource_metrics: ResourceMetrics,
    pub provider_status: ProviderStatus,
    pub execution_outcomes: Vec<ExecutionOutcome>,
    pub failures: Vec<FailureRecord>,
    /// Set only on the tick where the host's default route appeared or
    /// vanished; absent on every other tick.
    #[serde(default)]
    pub network_transition: Option<NetworkTransition>,
    /// Set only on the tick where the mounted filesystem crossed into (or out
    /// of) a fill band; absent on every other tick.
    #[serde(default)]
    pub disk_transition: Option<DiskTransition>,
    /// Set only on the tick where the failed-service condition appeared or
    /// cleared; absent on every other tick.
    #[serde(default)]
    pub services_transition: Option<ServiceTransition>,
    /// Set only on the tick where the probed endpoint was lost or restored;
    /// absent on every other tick.
    #[serde(default)]
    pub endpoint_transition: Option<EndpointTransition>,
    /// Predicted disk fill from the bounded sample history; absent until a
    /// trend is backed by enough samples over a long enough span.
    #[serde(default)]
    pub disk_projection: Option<DiskProjection>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeHealth {
    pub overall_status: HealthStatus,
    pub subsystems: Vec<SubsystemHealth>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthStatus {
    /// Not enough recorded activity to judge. Distinct from `Healthy`: an
    /// empty store is *unverified*, not well.
    Unknown,
    Healthy,
    Degraded,
    Unhealthy,
    Critical,
}

impl HealthStatus {
    /// Rank used to fold subsystem statuses into one overall status.
    /// `Unknown` ranks below `Healthy` so it only wins when *every* subsystem
    /// is unverified.
    pub fn severity_rank(&self) -> u8 {
        match self {
            HealthStatus::Unknown => 0,
            HealthStatus::Healthy => 1,
            HealthStatus::Degraded => 2,
            HealthStatus::Unhealthy => 3,
            HealthStatus::Critical => 4,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubsystemHealth {
    pub name: String,
    pub status: HealthStatus,
    pub details: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceMetrics {
    /// Host CPU pressure: 1-minute load average divided by online CPUs,
    /// clamped to `0.0..=1.0`. `0.0` when the platform cannot be read (see
    /// `notes`).
    pub cpu_usage: f32,
    /// Fraction of host memory in use (`MemTotal - MemAvailable`). `0.0` when
    /// the platform cannot be read (see `notes`).
    pub memory_usage: f32,
    /// Share of the mounted filesystem holding the working directory in use,
    /// from `df -Pk`, as `0.0..=1.0`.
    ///
    /// Only a reading when [`Self::disk_usage_known`] is `true`: an unreadable
    /// `df` leaves this `0.0` *and* `disk_usage_known` `false`, so an unknown
    /// filesystem can never be mistaken for an empty one. This replaces the
    /// earlier evolution-store footprint ratio.
    pub disk_usage: f32,
    /// Whether `df` produced a parseable reading this pass.
    #[serde(default)]
    pub disk_usage_known: bool,
    /// Whether network activity was observed. Always `false`: the evolution
    /// store records no traffic, and a default route (see [`Self::network_route`])
    /// is presence, not activity.
    pub network_active: bool,
    /// What the host's route table says about a default route. `Unknown` when
    /// the probe could not read the table — an unrun probe is not evidence of
    /// a broken network. A present route says nothing about whether any
    /// endpoint is reachable.
    #[serde(default)]
    pub network_route: NetworkRoute,
    /// What the endpoint probe answered about reachability. `Unknown` when the
    /// routing table could not be read and no connection was attempted.
    #[serde(default)]
    pub endpoint_reachability: EndpointReading,
    /// Failed systemd units, from `systemctl --failed`. `None` when the probe
    /// could not answer (no `systemctl`, a non-systemd host, a non-zero exit):
    /// unknown, never `Some(0)`.
    #[serde(default)]
    pub failed_services: Option<usize>,
    /// Provenance for the numbers above: which probe produced each value and
    /// which values are unavailable.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Three-valued answer to "does the host have a default route?".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkRoute {
    /// The route table could not be read; nothing is known. Distinct from
    /// [`Self::NoDefaultRoute`]: an unread probe is not a down network.
    #[default]
    Unknown,
    /// A default route (non-zero gateway) exists. Says nothing about whether
    /// any endpoint answered.
    DefaultRoute,
    /// The route table was read and lists no default route.
    NoDefaultRoute,
}

/// A change in the host's default-route state, reported on the tick it is
/// observed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkTransition {
    /// Stable per-direction identity (`network.default_route.lost` /
    /// `.restored`), so the same change cannot mint a new id on a later tick.
    pub id: String,
    /// The last known reading, or `Unknown` when the first reading was taken
    /// after this process started (the baseline itself is not reported).
    pub previous: NetworkRoute,
    pub current: NetworkRoute,
    pub timestamp_ms: u64,
}

/// Three-valued answer to "does an endpoint on the default gateway answer?".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointReading {
    /// No connection was attempted (unreadable routing table); nothing is
    /// known. Distinct from [`Self::Lost`]: an unrun probe is not a down link.
    #[default]
    Unknown,
    /// A connection completed or was refused: the host answered.
    Reachable,
    /// The connection timed out or reported the network unreachable.
    Lost,
}

/// A change in endpoint reachability, reported on the tick it is observed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EndpointTransition {
    /// Stable per-direction identity (`network.endpoint.lost` /
    /// `.restored`).
    pub id: String,
    pub previous: EndpointReading,
    pub current: EndpointReading,
    pub timestamp_ms: u64,
}

/// One `df` reading: the share of one mounted filesystem in use.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiskReading {
    /// Path handed to `df` whose filesystem was measured.
    pub path: String,
    /// Used share of that filesystem, `0.0..=1.0`.
    pub used_pct: f32,
}

/// A disk-fill band crossing, reported on the tick it is observed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiskTransition {
    /// Stable per-direction identity (`disk.fill` when the filesystem entered
    /// or moved up a fill band, `disk.fill.cleared` when it left them all).
    pub id: String,
    /// Filesystem the reading was taken from.
    pub path: String,
    /// Used share of that filesystem, `0.0..=1.0`.
    pub used_pct: f32,
    pub timestamp_ms: u64,
}

/// A failed-service condition crossing, reported on the tick it is observed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceTransition {
    /// Stable per-direction identity (`services.failed` /
    /// `services.failed.cleared`).
    pub id: String,
    /// Failed systemd units counted by the probe.
    pub failed: usize,
    pub timestamp_ms: u64,
}

/// A model of the machine the runtime runs on, limited to what the host
/// probes can prove.
///
/// Every field is a measured value or a zero/`None` that means "not read",
/// never a guess. No hostname, username, path or address is kept: this line is
/// rendered into a prompt.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HostProfile {
    /// Online CPUs from `/proc/cpuinfo`; `0` when unreadable.
    pub cpus: usize,
    /// `MemTotal` from `/proc/meminfo` in bytes; `0` when unreadable.
    pub memory_total_bytes: u64,
    /// Total bytes of the filesystem holding the workspace, from `df -Pk`;
    /// `0` when `df` cannot answer. Exactly one filesystem is measured.
    pub disk_total_bytes: u64,
    /// Available bytes on that same filesystem; `0` when unreadable.
    pub disk_free_bytes: u64,
    /// Failed systemd units from `systemctl --failed`; `None` when the probe
    /// cannot answer (no systemd, non-zero exit), never `Some(0)`.
    pub failed_services: Option<usize>,
    /// Whether `/proc/net/route` lists a default route. `false` covers both
    /// "read and absent" and "unreadable": the route probe is three-valued,
    /// this model is not, so `false` proves nothing and is never rendered as
    /// "network down".
    pub default_route: bool,
    /// When the probes above were read, in Unix milliseconds.
    pub captured_ms: u64,
}

/// Render a [`HostProfile`] as one prompt-sized line, or `None` when no
/// hardware fact was read at all.
///
/// Each clause is omitted when its field is unknown: a `None` never becomes a
/// zero, and `default_route: false` makes no claim either way. The disk clause
/// names one filesystem only.
pub fn describe_host_profile(profile: &HostProfile) -> Option<String> {
    if profile.cpus == 0 && profile.memory_total_bytes == 0 && profile.disk_total_bytes == 0 {
        return None;
    }
    let mut clauses = Vec::new();
    if profile.cpus > 0 {
        clauses.push(format!("{} CPUs", profile.cpus));
    }
    if profile.memory_total_bytes > 0 {
        clauses.push(format!(
            "{:.1} GiB RAM",
            profile.memory_total_bytes as f64 / GIB_BYTES
        ));
    }
    if profile.disk_total_bytes > 0 {
        clauses.push(format!(
            "{:.1} GiB free of {:.1} GiB",
            profile.disk_free_bytes as f64 / GIB_BYTES,
            profile.disk_total_bytes as f64 / GIB_BYTES
        ));
    }
    if let Some(failed) = profile.failed_services {
        clauses.push(format!("{failed} failed services"));
    }
    if profile.default_route {
        clauses.push("network up".to_string());
    }
    Some(format!("machine: {}", clauses.join(", ")))
}

/// One timestamped disk reading kept for trend projection.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct DiskSample {
    timestamp_ms: u64,
    used_pct: f32,
}

/// Bounded disk sample history, persisted so a trend survives a restart.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DiskHistory {
    #[serde(default)]
    samples: Vec<DiskSample>,
}

/// A projected disk fill derived from the sample history.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiskProjection {
    /// Hours until the used share reaches
    /// [`DISK_PROJECTION_TARGET_SHARE`] at the sampled rate.
    pub hours_to_threshold: f32,
    /// Used share of the newest sample, `0.0..=1.0`.
    pub current_pct: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderStatus {
    /// `false` when there is no sample to judge availability by, or when the
    /// recorded failure rate is at/above [`UNHEALTHY_FAILURE_RATE`].
    pub available: bool,
    /// Mean recorded experience duration (else mean reflection-job latency);
    /// `0` when no duration was ever recorded — see `notes`.
    pub latency_ms: u64,
    /// Share of the sampled experiences that failed.
    pub error_rate: f32,
    /// How many experiences the rate was computed from.
    #[serde(default)]
    pub sample_count: usize,
    /// Provenance for the numbers above.
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionOutcome {
    pub task_id: String,
    pub success: bool,
    pub duration_ms: u64,
    pub tokens_used: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailureRecord {
    pub id: String,
    pub component: String,
    pub error_type: String,
    pub message: String,
    pub timestamp_ms: u64,
}

/// Analysis result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisResult {
    pub id: String,
    pub issues: Vec<Issue>,
    pub opportunities: Vec<Opportunity>,
    pub anomalies: Vec<Anomaly>,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Issue {
    pub severity: IssueSeverity,
    pub component: String,
    pub description: String,
    pub impact: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueSeverity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Opportunity {
    pub area: String,
    pub description: String,
    pub expected_benefit: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Anomaly {
    pub metric: String,
    pub expected: serde_json::Value,
    pub actual: serde_json::Value,
    pub deviation: f32,
}

/// Action plan.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionPlan {
    pub id: String,
    pub actions: Vec<PlannedAction>,
    pub estimated_benefit: String,
    pub estimated_cost: String,
    pub risk_level: RiskLevel,
    pub success_criteria: Vec<String>,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedAction {
    pub id: String,
    pub action_type: ActionType,
    pub target: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ActionType {
    ConfigurationChange,
    Adaptation,
    Improvement,
    Development,
    Recovery,
}

impl ActionType {
    /// Stable `snake_case` name, used in persisted records and in the
    /// execution report.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ConfigurationChange => "configuration_change",
            Self::Adaptation => "adaptation",
            Self::Improvement => "improvement",
            Self::Development => "development",
            Self::Recovery => "recovery",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

/// Execution result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub plan_id: String,
    pub action_results: Vec<ActionResult>,
    /// `true` only when the plan held at least one action and *every* action
    /// ended as [`ActionOutcome::Remediated`]. An action that was merely
    /// recorded for operator review is not a remediation, so it keeps this
    /// `false`.
    pub overall_success: bool,
    pub timestamp_ms: u64,
    /// How many actions the plan held.
    #[serde(default)]
    pub planned_actions: usize,
    /// How many actions a real change was applied for.
    #[serde(default)]
    pub remediated_actions: usize,
    /// How many actions could only be recorded for operator review.
    #[serde(default)]
    pub recorded_for_review: usize,
}

impl ExecutionResult {
    /// One-line account of why this execution is not a success, or `None`
    /// when every planned action was remediated.
    ///
    /// A plan that could not be remediated never reports a silent failure:
    /// this string names how many actions were not remediated, why the first
    /// one failed, and how many were handed to an operator.
    pub fn failure_reason(&self) -> Option<String> {
        if self.planned_actions == 0 {
            return Some("plan held no actions: nothing was executed".to_string());
        }
        if self.overall_success {
            return None;
        }
        let not_remediated = self.planned_actions.saturating_sub(self.remediated_actions);
        let first_error = self
            .action_results
            .iter()
            .find(|r| !r.success)
            .map(|r| {
                r.error.clone().unwrap_or_else(|| {
                    format!("action {} failed without a recorded reason", r.action_id)
                })
            })
            .unwrap_or_else(|| "no action result was recorded".to_string());
        Some(format!(
            "{not_remediated} of {} action(s) were not remediated ({} recorded for operator review): {first_error}",
            self.planned_actions, self.recorded_for_review
        ))
    }
}

/// What an [`ActionResult`] actually achieved.
///
/// The distinction exists so a plan report cannot conflate "I fixed it" with
/// "I wrote down that it needs fixing": only [`ActionOutcome::Remediated`]
/// ever carries `success: true`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    /// A real change was written through the engine's store and verified by
    /// reading it back. The only outcome that reports `success: true`.
    Remediated,
    /// The loop could not perform the action; it persisted a pending
    /// `record_issue` proposal so an operator sees it. Nothing about the
    /// system's behaviour changed, so `success` is `false`.
    RecordedForReview,
    /// Nothing was done and nothing was recorded. `success` is `false` and
    /// `error` says why.
    #[default]
    NotExecuted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionResult {
    /// Id of the [`PlannedAction`] this result belongs to.
    pub action_id: String,
    /// `true` only for [`ActionOutcome::Remediated`].
    pub success: bool,
    /// What was actually done, in words an operator can check.
    pub output: String,
    /// Why the action was not remediated, when it was not.
    pub error: Option<String>,
    /// What the action actually achieved; see [`ActionOutcome`].
    ///
    /// Defaults to [`ActionOutcome::NotExecuted`] for records written before
    /// this field existed, so old data claims nothing.
    #[serde(default)]
    pub outcome: ActionOutcome,
    /// The pending `record_issue` proposal persisted for operator review, when
    /// `outcome == ActionOutcome::RecordedForReview`.
    #[serde(default)]
    pub review_proposal_id: Option<String>,
}

/// Knowledge base.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnowledgeBase {
    pub decisions: Vec<DecisionRecord>,
    pub outcomes: Vec<OutcomeRecord>,
    pub self_model: serde_json::Value,
    pub learning_history: Vec<LearningRecord>,
    pub adaptation_strategies: Vec<AdaptationStrategy>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub id: String,
    pub context: String,
    pub decision: String,
    pub rationale: String,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutcomeRecord {
    pub id: String,
    pub action_id: String,
    pub outcome: String,
    pub effectiveness: f32,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LearningRecord {
    pub id: String,
    pub lesson: String,
    pub context: String,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdaptationStrategy {
    pub name: String,
    pub description: String,
    pub effectiveness: f32,
}

/// Durable form of the loop's mutable state: the knowledge base plus the
/// cycle counter, stored as one `engine_records` payload so the two can
/// never diverge across a partial write.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct MapekState {
    knowledge: KnowledgeBase,
    cycle_count: u32,
}

impl MapekState {
    fn empty() -> Self {
        Self {
            knowledge: KnowledgeBase {
                decisions: Vec::new(),
                outcomes: Vec::new(),
                self_model: serde_json::json!({}),
                learning_history: Vec::new(),
                adaptation_strategies: Vec::new(),
            },
            cycle_count: 0,
        }
    }
}

/// Durable route reading the next probe is compared against. Kept in its own
/// `engine_records` row (rather than in [`MapekState`]) so monitoring never
/// rewrites the knowledge base.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct NetworkState {
    /// Last readable reading. `None` until a probe has succeeded once.
    #[serde(default)]
    last_known: Option<NetworkRoute>,
}

/// Durable endpoint reading the next probe is compared against.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct EndpointState {
    /// Last readable reading. `None` until a probe has succeeded once: the
    /// first readable reading is a baseline, not a transition.
    #[serde(default)]
    last_known: Option<EndpointReading>,
}

/// Which fill band a filesystem's used share falls into.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DiskBand {
    #[default]
    Clear,
    Medium,
    High,
}

impl DiskBand {
    fn of(used_pct: f32) -> Self {
        if used_pct >= DISK_FILL_HIGH_SHARE {
            Self::High
        } else if used_pct >= DISK_FILL_MEDIUM_SHARE {
            Self::Medium
        } else {
            Self::Clear
        }
    }
}

/// Durable disk band the next probe is compared against.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct DiskState {
    /// Last readable band. `None` until a probe has succeeded once: the first
    /// reading is a baseline, not a transition, exactly like the route.
    #[serde(default)]
    last_band: Option<DiskBand>,
}

/// Durable failed-service condition the next probe is compared against.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct ServiceState {
    /// Last readable condition. `None` until a probe has succeeded once.
    #[serde(default)]
    last_failing: Option<bool>,
}

pub struct MapekLoop {
    store: EvolutionStore,
    knowledge: KnowledgeBase,
    cycle_count: u32,
    network: NetworkState,
    disk: DiskState,
    disk_history: DiskHistory,
    services: ServiceState,
    endpoint: EndpointState,
    route_probe: Box<dyn Fn() -> NetworkRoute + Send + Sync>,
    disk_probe: Box<dyn Fn() -> Option<DiskReading> + Send + Sync>,
    services_probe: Box<dyn Fn() -> Option<usize> + Send + Sync>,
    endpoint_probe: Option<std::sync::Arc<dyn Fn() -> EndpointReading + Send + Sync>>,
    host_probe: Box<dyn Fn(u64) -> HostProfile + Send + Sync>,
}

impl MapekLoop {
    pub fn new(store: EvolutionStore) -> Result<Self> {
        // An absent record means a fresh database; anything present must
        // decode — a corrupt payload is an error, not silently-empty state.
        let state = store
            .list_engine_records(STATE_NAMESPACE)?
            .into_iter()
            .find(|(id, _)| id == STATE_ID)
            .map(|(_, payload)| from_json_value::<MapekState>(payload))
            .transpose()?
            .unwrap_or_else(MapekState::empty);
        let network = store
            .list_engine_records(NETWORK_NAMESPACE)?
            .into_iter()
            .find(|(id, _)| id == NETWORK_ID)
            .map(|(_, payload)| from_json_value::<NetworkState>(payload))
            .transpose()?
            .unwrap_or_default();
        let disk = store
            .list_engine_records(DISK_NAMESPACE)?
            .into_iter()
            .find(|(id, _)| id == DISK_ID)
            .map(|(_, payload)| from_json_value::<DiskState>(payload))
            .transpose()?
            .unwrap_or_default();
        let services = store
            .list_engine_records(SERVICES_NAMESPACE)?
            .into_iter()
            .find(|(id, _)| id == SERVICES_ID)
            .map(|(_, payload)| from_json_value::<ServiceState>(payload))
            .transpose()?
            .unwrap_or_default();
        let endpoint = store
            .list_engine_records(ENDPOINT_NAMESPACE)?
            .into_iter()
            .find(|(id, _)| id == ENDPOINT_ID)
            .map(|(_, payload)| from_json_value::<EndpointState>(payload))
            .transpose()?
            .unwrap_or_default();
        let disk_history = store
            .list_engine_records(DISK_HISTORY_NAMESPACE)?
            .into_iter()
            .find(|(id, _)| id == DISK_HISTORY_ID)
            .map(|(_, payload)| from_json_value::<DiskHistory>(payload))
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            store,
            knowledge: state.knowledge,
            cycle_count: state.cycle_count,
            network,
            disk,
            disk_history,
            services,
            endpoint,
            route_probe: Box::new(probe_default_route),
            disk_probe: Box::new(probe_disk_usage),
            services_probe: Box::new(probe_failed_services),
            endpoint_probe: Some(std::sync::Arc::new(probe_endpoint_reachability)),
            host_probe: Box::new(capture_host_profile),
        })
    }

    /// The persisted host profile; captured and saved only when no row exists.
    ///
    /// Later calls return the stored row untouched (its `captured_ms` says when
    /// it was taken) and never re-probe the host. A store or decode failure is
    /// returned as an error, never replaced by a fresh capture.
    pub fn host_profile(&self) -> Result<HostProfile> {
        if let Some(profile) = load_host_profile(&self.store)? {
            return Ok(profile);
        }
        let profile = (self.host_probe)(now_ms());
        save_host_profile(&self.store, &profile)?;
        Ok(profile)
    }

    /// The shared store every subsystem reads and writes through.
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    /// Write the current knowledge base and cycle count to the store.
    fn persist_state(&self) -> Result<()> {
        let state = MapekState {
            knowledge: self.knowledge.clone(),
            cycle_count: self.cycle_count,
        };
        self.store
            .save_engine_record(STATE_NAMESPACE, STATE_ID, &to_json_value(&state)?)
    }

    /// Monitor: Collect data about runtime health and status.
    pub fn monitor(&mut self) -> Result<MonitoringData> {
        let route = self.probe_network_route();
        let network_transition = self.record_network_transition(route)?;
        let endpoint = self.probe_endpoint();
        let endpoint_transition = self.record_endpoint_transition(endpoint)?;
        let disk = (self.disk_probe)();
        let disk_transition = self.record_disk_transition(disk.as_ref())?;
        if let Some(reading) = disk.as_ref() {
            self.record_disk_sample(reading)?;
        }
        let disk_projection = if disk.is_some() {
            project_disk_fill(&self.disk_history.samples)
        } else {
            None
        };
        let failed_services = (self.services_probe)();
        let services_transition = self.record_services_transition(failed_services)?;
        let data = MonitoringData {
            id: gen_id("monitor"),
            timestamp_ms: now_ms(),
            runtime_health: self.collect_runtime_health()?,
            resource_metrics: self.collect_resource_metrics(
                route,
                endpoint,
                disk.as_ref(),
                failed_services,
            )?,
            provider_status: self.collect_provider_status()?,
            execution_outcomes: self.collect_execution_outcomes()?,
            failures: self.collect_failures()?,
            network_transition,
            disk_transition,
            services_transition,
            endpoint_transition,
            disk_projection,
        };

        Ok(data)
    }

    /// One route probe per monitoring pass, injected so tests can script it.
    fn probe_network_route(&self) -> NetworkRoute {
        (self.route_probe)()
    }

    /// One endpoint probe per monitoring pass. A missing probe is `Unknown`,
    /// never a claimed outage.
    fn probe_endpoint(&self) -> EndpointReading {
        match &self.endpoint_probe {
            Some(probe) => probe(),
            None => EndpointReading::Unknown,
        }
    }

    /// Compare this pass's endpoint reading with the durable last one and
    /// report a change exactly once.
    ///
    /// The first readable reading is a baseline, not a transition. `Unknown`
    /// reports nothing and does not overwrite the baseline.
    fn record_endpoint_transition(
        &mut self,
        reading: EndpointReading,
    ) -> Result<Option<EndpointTransition>> {
        if reading == EndpointReading::Unknown {
            return Ok(None);
        }
        let Some(previous) = self.endpoint.last_known else {
            self.endpoint.last_known = Some(reading);
            self.save_endpoint_state()?;
            return Ok(None);
        };
        if previous == reading {
            return Ok(None);
        }
        let transition = EndpointTransition {
            id: format!(
                "network.endpoint.{}",
                if reading == EndpointReading::Lost {
                    "lost"
                } else {
                    "restored"
                }
            ),
            previous,
            current: reading,
            timestamp_ms: now_ms(),
        };
        self.endpoint.last_known = Some(reading);
        self.save_endpoint_state()?;
        Ok(Some(transition))
    }

    fn save_endpoint_state(&self) -> Result<()> {
        self.store.save_engine_record(
            ENDPOINT_NAMESPACE,
            ENDPOINT_ID,
            &to_json_value(&self.endpoint)?,
        )
    }

    /// Compare this pass's reading with the durable last one and report a
    /// change exactly once.
    ///
    /// The first readable reading is a baseline, not a transition. `Unknown`
    /// reports nothing and does not overwrite the baseline, so one unreadable
    /// probe cannot mask — or fake — a real change on the next one.
    fn record_network_transition(
        &mut self,
        route: NetworkRoute,
    ) -> Result<Option<NetworkTransition>> {
        if route == NetworkRoute::Unknown {
            return Ok(None);
        }
        let Some(previous) = self.network.last_known else {
            self.network.last_known = Some(route);
            self.save_network_state()?;
            return Ok(None);
        };
        if previous == route {
            return Ok(None);
        }
        let transition = NetworkTransition {
            id: format!(
                "network.default_route.{}",
                if route == NetworkRoute::NoDefaultRoute {
                    "lost"
                } else {
                    "restored"
                }
            ),
            previous,
            current: route,
            timestamp_ms: now_ms(),
        };
        self.network.last_known = Some(route);
        self.save_network_state()?;
        Ok(Some(transition))
    }

    fn save_network_state(&self) -> Result<()> {
        self.store.save_engine_record(
            NETWORK_NAMESPACE,
            NETWORK_ID,
            &to_json_value(&self.network)?,
        )
    }

    /// Compare this pass's `df` reading with the durable band and report a
    /// crossing exactly once.
    ///
    /// The first readable reading is a baseline, not a transition. `None`
    /// (unreadable `df`) reports nothing and leaves the baseline untouched, so
    /// one failed probe cannot mask — or fake — a real crossing on the next.
    fn record_disk_transition(
        &mut self,
        reading: Option<&DiskReading>,
    ) -> Result<Option<DiskTransition>> {
        let Some(reading) = reading else {
            return Ok(None);
        };
        let band = DiskBand::of(reading.used_pct);
        let Some(previous) = self.disk.last_band else {
            self.disk.last_band = Some(band);
            self.save_disk_state()?;
            return Ok(None);
        };
        if previous == band {
            return Ok(None);
        }
        let transition = DiskTransition {
            id: if band == DiskBand::Clear {
                "disk.fill.cleared"
            } else {
                "disk.fill"
            }
            .to_string(),
            path: reading.path.clone(),
            used_pct: reading.used_pct,
            timestamp_ms: now_ms(),
        };
        self.disk.last_band = Some(band);
        self.save_disk_state()?;
        Ok(Some(transition))
    }

    fn save_disk_state(&self) -> Result<()> {
        self.store
            .save_engine_record(DISK_NAMESPACE, DISK_ID, &to_json_value(&self.disk)?)
    }

    /// Append a known disk reading to the bounded history, dropping the oldest
    /// sample once the cap is reached.
    fn record_disk_sample(&mut self, reading: &DiskReading) -> Result<()> {
        self.disk_history.samples.push(DiskSample {
            timestamp_ms: now_ms(),
            used_pct: reading.used_pct,
        });
        let excess = self
            .disk_history
            .samples
            .len()
            .saturating_sub(DISK_HISTORY_LIMIT);
        if excess > 0 {
            self.disk_history.samples.drain(0..excess);
        }
        self.store.save_engine_record(
            DISK_HISTORY_NAMESPACE,
            DISK_HISTORY_ID,
            &to_json_value(&self.disk_history)?,
        )
    }

    /// Compare this pass's failed-unit count with the durable condition and
    /// report a crossing exactly once. The first readable reading is a
    /// baseline; `None` reports nothing and leaves the baseline untouched.
    fn record_services_transition(
        &mut self,
        failed: Option<usize>,
    ) -> Result<Option<ServiceTransition>> {
        let Some(failed) = failed else {
            return Ok(None);
        };
        let failing = failed >= FAILED_SERVICES_THRESHOLD;
        let Some(previous) = self.services.last_failing else {
            self.services.last_failing = Some(failing);
            self.save_services_state()?;
            return Ok(None);
        };
        if previous == failing {
            return Ok(None);
        }
        let transition = ServiceTransition {
            id: if failing {
                "services.failed"
            } else {
                "services.failed.cleared"
            }
            .to_string(),
            failed,
            timestamp_ms: now_ms(),
        };
        self.services.last_failing = Some(failing);
        self.save_services_state()?;
        Ok(Some(transition))
    }

    fn save_services_state(&self) -> Result<()> {
        self.store.save_engine_record(
            SERVICES_NAMESPACE,
            SERVICES_ID,
            &to_json_value(&self.services)?,
        )
    }

    /// Analyze: Understand what is happening and identify issues.
    pub fn analyze(&mut self, data: &MonitoringData) -> Result<AnalysisResult> {
        let mut issues = Vec::new();
        let opportunities = Vec::new();
        let anomalies = Vec::new();

        match data.runtime_health.overall_status {
            HealthStatus::Healthy => {}
            HealthStatus::Unknown => issues.push(Issue {
                severity: IssueSeverity::Low,
                component: "runtime".to_string(),
                description: "Runtime health unverified: too little recorded activity".to_string(),
                impact: "No conclusion can be drawn from the store yet".to_string(),
            }),
            HealthStatus::Degraded => issues.push(Issue {
                severity: IssueSeverity::Medium,
                component: "runtime".to_string(),
                description: "Runtime health degraded".to_string(),
                impact: "May affect all operations".to_string(),
            }),
            HealthStatus::Unhealthy | HealthStatus::Critical => issues.push(Issue {
                severity: IssueSeverity::High,
                component: "runtime".to_string(),
                description: "Runtime health degraded".to_string(),
                impact: "May affect all operations".to_string(),
            }),
        }

        if let Some(transition) = &data.network_transition {
            let lost = transition.current == NetworkRoute::NoDefaultRoute;
            issues.push(Issue {
                severity: if lost {
                    IssueSeverity::Medium
                } else {
                    IssueSeverity::Low
                },
                component: "network".to_string(),
                description: if lost {
                    "The host route table stopped listing a default route".to_string()
                } else {
                    "The host route table lists a default route again".to_string()
                },
                impact: "This is a route-table change, not a reachability test".to_string(),
            });
        }

        if let Some(transition) = &data.endpoint_transition {
            let lost = transition.current == EndpointReading::Lost;
            issues.push(Issue {
                severity: if lost {
                    IssueSeverity::Medium
                } else {
                    IssueSeverity::Low
                },
                component: "network".to_string(),
                description: if lost {
                    "The probed endpoint on the default gateway stopped answering".to_string()
                } else {
                    "The probed endpoint on the default gateway answers again".to_string()
                },
                impact: if lost {
                    "Outbound network services may be unreachable"
                } else {
                    "Connectivity was restored; the loss was already reported"
                }
                .to_string(),
            });
        }

        if let Some(transition) = &data.disk_transition
            && transition.used_pct >= DISK_FILL_MEDIUM_SHARE
        {
            issues.push(Issue {
                severity: if transition.used_pct >= DISK_FILL_HIGH_SHARE {
                    IssueSeverity::High
                } else {
                    IssueSeverity::Medium
                },
                component: "disk".to_string(),
                description: format!(
                    "Disk fill: {:.0}% of {} is in use",
                    transition.used_pct * 100.0,
                    transition.path
                ),
                impact: "Writes fail once the filesystem is full".to_string(),
            });
        }

        if let Some(projection) = &data.disk_projection {
            issues.push(Issue {
                severity: if projection.hours_to_threshold < DISK_PROJECTION_MEDIUM_HOURS {
                    IssueSeverity::Medium
                } else {
                    IssueSeverity::Low
                },
                component: "disk".to_string(),
                description: format!(
                    "Disk is projected to reach 95% in {:.1} hours at the sampled rate \
                     (currently {:.0}% used)",
                    projection.hours_to_threshold,
                    projection.current_pct * 100.0
                ),
                impact: "Writes fail once the filesystem is full".to_string(),
            });
        }

        if let Some(transition) = &data.services_transition
            && transition.failed >= FAILED_SERVICES_THRESHOLD
        {
            issues.push(Issue {
                severity: IssueSeverity::Medium,
                component: "services".to_string(),
                description: format!(
                    "{} systemd unit(s) failed according to systemctl --failed",
                    transition.failed
                ),
                impact: "The functions those units provide are down".to_string(),
            });
        }

        if data.provider_status.error_rate > 0.1 && data.provider_status.sample_count > 0 {
            issues.push(Issue {
                severity: IssueSeverity::Medium,
                component: "provider".to_string(),
                description: format!(
                    "Provider error rate elevated: {:.0}% of {} sampled experience(s) failed",
                    data.provider_status.error_rate * 100.0,
                    data.provider_status.sample_count
                ),
                impact: "Degraded reliability".to_string(),
            });
        }

        // Failed reflection jobs are reported as their own issue because they
        // are the one failure class the loop can actually act on (see
        // `execute_action`): the job row can be re-queued in the store.
        let reflection_failures = data
            .failures
            .iter()
            .filter(|f| f.component == REFLECTION_TARGET)
            .count();
        if reflection_failures > 0 {
            issues.push(Issue {
                severity: if reflection_failures >= UNRESOLVED_ISSUES_UNHEALTHY {
                    IssueSeverity::High
                } else {
                    IssueSeverity::Medium
                },
                component: REFLECTION_TARGET.to_string(),
                description: format!(
                    "{reflection_failures} reflection job(s) failed and can be retried"
                ),
                impact: "The experiences behind those jobs were never reflected on".to_string(),
            });
        }

        let pipeline_failures = data.failures.len().saturating_sub(reflection_failures);
        if pipeline_failures >= metrics::MIN_SAMPLE {
            issues.push(Issue {
                severity: IssueSeverity::Medium,
                component: "experience_pipeline".to_string(),
                description: format!(
                    "{pipeline_failures} failing experience(s) recorded in the last {}h",
                    WINDOW_MS / 3_600_000
                ),
                impact: "Repeated failures in recorded work".to_string(),
            });
        }

        let result = AnalysisResult {
            id: gen_id("analysis"),
            issues,
            opportunities,
            anomalies,
            timestamp_ms: now_ms(),
        };

        Ok(result)
    }

    /// Plan: Generate action plans to address issues.
    ///
    /// The action *type* is chosen from what the loop can actually do, not
    /// from severity alone: an issue on [`REFLECTION_TARGET`] becomes a
    /// [`ActionType::Recovery`] (the loop can re-queue the failed job in the
    /// store), every other component keeps the severity mapping. Types the
    /// loop has no actuator for are still planned — they are the ones
    /// [`MapekLoop::execute_action`] escalates to an operator instead of
    /// pretending to perform them.
    pub fn plan(&mut self, analysis: &AnalysisResult) -> Result<ActionPlan> {
        let mut actions = Vec::new();

        for issue in &analysis.issues {
            let (action_type, description) = if issue.component == REFLECTION_TARGET
                && matches!(
                    issue.severity,
                    IssueSeverity::Critical | IssueSeverity::High | IssueSeverity::Medium
                ) {
                (
                    ActionType::Recovery,
                    format!("Re-queue failed reflection job(s): {}", issue.description),
                )
            } else {
                match issue.severity {
                    IssueSeverity::Critical | IssueSeverity::High => (
                        ActionType::Recovery,
                        format!("Address critical issue: {}", issue.description),
                    ),
                    IssueSeverity::Medium => (
                        ActionType::Adaptation,
                        format!("Adapt to issue: {}", issue.description),
                    ),
                    IssueSeverity::Low => (
                        ActionType::Improvement,
                        format!("Improve: {}", issue.description),
                    ),
                }
            };
            actions.push(PlannedAction {
                id: gen_id("action"),
                action_type,
                target: issue.component.clone(),
                description,
                parameters: serde_json::json!({}),
            });
        }

        let plan = ActionPlan {
            id: gen_id("plan"),
            actions,
            estimated_benefit: "Improved system health".to_string(),
            estimated_cost: "Moderate".to_string(),
            risk_level: RiskLevel::Medium,
            success_criteria: vec!["All issues resolved".to_string()],
            timestamp_ms: now_ms(),
        };

        Ok(plan)
    }

    /// Execute: carry out an approved action plan.
    ///
    /// Every action goes through [`Self::execute_action`], which reports what
    /// it actually achieved. [`ExecutionResult::overall_success`] is `true`
    /// only for a non-empty plan in which *every* action ended as
    /// [`ActionOutcome::Remediated`]; an action that could only be recorded
    /// for operator review keeps it `false`, and
    /// [`ExecutionResult::failure_reason`] says so in words.
    pub fn execute(&mut self, plan: &ActionPlan) -> Result<ExecutionResult> {
        let mut action_results = Vec::with_capacity(plan.actions.len());

        for action in &plan.actions {
            action_results.push(self.execute_action(action)?);
        }

        let remediated_actions = action_results
            .iter()
            .filter(|r| r.outcome == ActionOutcome::Remediated)
            .count();
        let recorded_for_review = action_results
            .iter()
            .filter(|r| r.outcome == ActionOutcome::RecordedForReview)
            .count();
        let overall_success =
            !action_results.is_empty() && remediated_actions == action_results.len();

        Ok(ExecutionResult {
            plan_id: plan.id.clone(),
            planned_actions: action_results.len(),
            remediated_actions,
            recorded_for_review,
            action_results,
            overall_success,
            timestamp_ms: now_ms(),
        })
    }

    /// Update knowledge: Store decisions and the *remediation* outcome.
    ///
    /// `effectiveness` is `1.0` only when the plan was actually remediated
    /// ([`ExecutionResult::overall_success`]); a plan that was merely recorded
    /// for an operator is stored as a failure with `0.0`, so the knowledge
    /// base cannot later "remember" work that never happened.
    ///
    /// This is the loop's only state mutation, so it is also where the
    /// knowledge base and cycle count are persisted.
    pub fn update_knowledge(
        &mut self,
        plan: &ActionPlan,
        execution: &ExecutionResult,
    ) -> Result<()> {
        for action in &plan.actions {
            self.knowledge.decisions.push(DecisionRecord {
                id: gen_id("decision"),
                context: format!("{:?}", action.action_type),
                decision: action.description.clone(),
                rationale: plan.estimated_benefit.clone(),
                timestamp_ms: now_ms(),
            });
        }

        self.knowledge.outcomes.push(OutcomeRecord {
            id: gen_id("outcome"),
            action_id: plan.id.clone(),
            outcome: if execution.overall_success {
                "success".to_string()
            } else {
                "failure".to_string()
            },
            effectiveness: if execution.overall_success { 1.0 } else { 0.0 },
            timestamp_ms: now_ms(),
        });

        self.persist_state()
    }

    /// Run a complete MAPE-K cycle.
    ///
    /// The returned [`CycleResult`] carries the honest execution verdict:
    /// `execution_success` is `true` only when the cycle's plan was actually
    /// remediated, and `failure_reason` explains what was not (see
    /// [`ExecutionResult::failure_reason`]). An unremediated cycle also leaves
    /// a `mapek_execution_incomplete` event in the store, so the failure is
    /// still findable after the report has been dropped.
    pub fn run_cycle(&mut self) -> Result<CycleResult> {
        self.cycle_count += 1;

        let monitoring_data = self.monitor()?;
        let analysis = self.analyze(&monitoring_data)?;
        let plan = self.plan(&analysis)?;
        let execution = self.execute(&plan)?;
        self.update_knowledge(&plan, &execution)?;

        let failure_reason = execution.failure_reason();
        if let Some(reason) = &failure_reason {
            self.store.save_event(&LearningEvent {
                id: gen_id("evt"),
                timestamp_ms: now_ms(),
                agent_id: MAPEK_AGENT.to_string(),
                kind: "mapek_execution_incomplete".to_string(),
                summary: format!(
                    "MAPE-K cycle {} did not remediate its plan: {reason}",
                    self.cycle_count
                ),
                payload: serde_json::json!({
                    "cycle_number": self.cycle_count,
                    "plan_id": plan.id,
                    "planned_actions": execution.planned_actions,
                    "remediated_actions": execution.remediated_actions,
                    "recorded_for_review": execution.recorded_for_review,
                    "failure_reason": reason,
                }),
            })?;
        }

        Ok(CycleResult {
            cycle_number: self.cycle_count,
            monitoring_data_id: monitoring_data.id,
            analysis_id: analysis.id,
            plan_id: plan.id,
            execution_success: execution.overall_success,
            planned_actions: execution.planned_actions,
            remediated_actions: execution.remediated_actions,
            recorded_for_review: execution.recorded_for_review,
            failure_reason,
            timestamp_ms: now_ms(),
        })
    }

    /// Monitor: runtime health, folded from recorded outcomes, reflection job
    /// states, proposal states and live memory counts.
    ///
    /// A store with no (or too little) recorded activity yields
    /// [`HealthStatus::Unknown`], never a fabricated `Healthy`.
    fn collect_runtime_health(&self) -> Result<RuntimeHealth> {
        let experiences = self.store.experience_stats(WINDOW_MS)?;
        let proposals = self.store.proposal_stats()?;
        let reflection = self.store.reflection_stats()?;
        let live_memory = self.store.count_memory()?;

        let mut subsystems = Vec::with_capacity(4);

        // 1. Experience pipeline: recorded outcomes.
        let (status, details) = match experiences.authoritative_sample() {
            None => (
                HealthStatus::Unknown,
                format!(
                    "{} experience(s) recorded ({} in the last {}h): no sample of {} to rate",
                    experiences.all_time.total,
                    experiences.recent.total,
                    WINDOW_MS / 3_600_000,
                    metrics::MIN_SAMPLE
                ),
            ),
            Some(sample) => {
                let failure = sample.failure_rate().unwrap_or(0.0);
                let errors = sample.error_rate().unwrap_or(0.0);
                let status = if failure >= UNHEALTHY_FAILURE_RATE {
                    HealthStatus::Unhealthy
                } else if failure >= DEGRADED_FAILURE_RATE || errors >= DEGRADED_ERROR_RATE {
                    HealthStatus::Degraded
                } else {
                    HealthStatus::Healthy
                };
                (
                    status,
                    format!(
                        "{} experience(s) sampled: failure {:.0}%, errors {:.0}%, {} succeeded",
                        sample.total,
                        failure * 100.0,
                        errors * 100.0,
                        sample.success
                    ),
                )
            }
        };
        subsystems.push(SubsystemHealth {
            name: "experience_pipeline".to_string(),
            status,
            details,
        });

        // 2. Reflection: job outcomes and backlog.
        let (status, details) = if reflection.total == 0 {
            (
                HealthStatus::Unknown,
                "no reflection jobs recorded".to_string(),
            )
        } else if reflection.failed > 0
            && (reflection.failure_rate().unwrap_or(0.0) >= 0.5 || reflection.failed >= 3)
        {
            (
                HealthStatus::Unhealthy,
                format!(
                    "{} of {} finished reflection job(s) failed",
                    reflection.failed,
                    reflection.done + reflection.failed
                ),
            )
        } else if reflection.failed > 0 {
            (
                HealthStatus::Degraded,
                format!(
                    "{} reflection job(s) failed ({} done, {} queued)",
                    reflection.failed,
                    reflection.done,
                    reflection.backlog()
                ),
            )
        } else if reflection.backlog() >= REFLECTION_BACKLOG_DEGRADED {
            (
                HealthStatus::Degraded,
                format!(
                    "{} reflection job(s) queued for {} done",
                    reflection.backlog(),
                    reflection.done
                ),
            )
        } else {
            (
                HealthStatus::Healthy,
                format!(
                    "{} reflection job(s): {} done, {} queued",
                    reflection.total,
                    reflection.done,
                    reflection.backlog()
                ),
            )
        };
        subsystems.push(SubsystemHealth {
            name: "reflection".to_string(),
            status,
            details,
        });

        // 3. Learning proposals: unresolved recorded issues, operator backlog,
        //    and wholesale rejection.
        let (status, details) = if proposals.pending_record_issue > 0 {
            let status = if proposals.pending_record_issue >= UNRESOLVED_ISSUES_UNHEALTHY {
                HealthStatus::Unhealthy
            } else {
                HealthStatus::Degraded
            };
            (
                status,
                format!(
                    "{} pending record_issue proposal(s): recorded failures are unresolved",
                    proposals.pending_record_issue
                ),
            )
        } else if proposals.total == 0 {
            if experiences.all_time.total == 0 {
                (
                    HealthStatus::Unknown,
                    "no learning proposals recorded yet".to_string(),
                )
            } else if experiences.all_time.problems > 0 {
                (
                    HealthStatus::Degraded,
                    format!(
                        "{} failing experience(s) recorded but no issue proposal exists: reflection has not run",
                        experiences.all_time.problems
                    ),
                )
            } else if experiences.authoritative_sample().is_some() {
                (
                    HealthStatus::Healthy,
                    "no proposals recorded: no failures or corrections to learn from".to_string(),
                )
            } else {
                // Too few experiences to claim the learning loop is fine.
                (
                    HealthStatus::Unknown,
                    format!(
                        "no learning proposals recorded yet (only {} experience(s), {} needed)",
                        experiences.all_time.total,
                        metrics::MIN_SAMPLE
                    ),
                )
            }
        } else if proposals.pending >= PROPOSAL_BACKLOG_DEGRADED {
            (
                HealthStatus::Degraded,
                format!(
                    "{} pending proposal(s) awaiting a decision",
                    proposals.pending
                ),
            )
        } else if proposals.total >= metrics::MIN_SAMPLE
            && proposals
                .rejection_rate()
                .is_some_and(|rate| rate >= REJECTION_RATE_DEGRADED)
        {
            (
                HealthStatus::Degraded,
                format!(
                    "{} of {} decided proposal(s) rejected",
                    proposals.rejected,
                    proposals.approved + proposals.applied + proposals.rejected
                ),
            )
        } else {
            (
                HealthStatus::Healthy,
                format!(
                    "{} proposal(s): {} pending, {} applied, {} rejected",
                    proposals.total, proposals.pending, proposals.applied, proposals.rejected
                ),
            )
        };
        subsystems.push(SubsystemHealth {
            name: "learning_proposals".to_string(),
            status,
            details,
        });

        // 4. Memory: corrections recorded but nothing persisted is a real
        //    learning failure; an empty store is simply unverified.
        let (status, details) = if live_memory > 0 {
            (
                HealthStatus::Healthy,
                format!("{live_memory} live memory entr(y/ies)"),
            )
        } else if experiences.all_time.with_corrections > 0 {
            (
                HealthStatus::Degraded,
                format!(
                    "{} experience(s) carried user corrections but no live memory entry exists",
                    experiences.all_time.with_corrections
                ),
            )
        } else {
            (
                HealthStatus::Unknown,
                "no live memory entries yet (no corrections recorded to learn from)".to_string(),
            )
        };
        subsystems.push(SubsystemHealth {
            name: "memory".to_string(),
            status,
            details,
        });

        let worst = subsystems
            .iter()
            .filter(|s| s.status != HealthStatus::Unknown)
            .map(|s| s.status)
            .max_by_key(|s| s.severity_rank());

        let overall_status = match worst {
            // Nothing in the store is backed by evidence.
            None => HealthStatus::Unknown,
            // `Healthy` is only claimed once the experience sample is large
            // enough to support it; a handful of rows is still unverified.
            Some(HealthStatus::Healthy) if experiences.authoritative_sample().is_none() => {
                HealthStatus::Unknown
            }
            Some(status) => status,
        };

        Ok(RuntimeHealth {
            overall_status,
            subsystems,
        })
    }

    /// Monitor: real host and store measurements.
    ///
    /// `cpu_usage`/`memory_usage` are host probes (`/proc`), `disk_usage` is
    /// the mounted filesystem's own used share (`df -Pk`), `failed_services` is
    /// the `systemctl --failed` count, `network_route` is the pass's
    /// `/proc/net/route` reading, and `notes` records the provenance (or
    /// unavailability) of each.
    fn collect_resource_metrics(
        &self,
        route: NetworkRoute,
        endpoint: EndpointReading,
        disk: Option<&DiskReading>,
        failed_services: Option<usize>,
    ) -> Result<ResourceMetrics> {
        let footprint = self.store.store_footprint_bytes()?;
        let mut notes = Vec::new();

        let cpu_usage = host_cpu_pressure();
        match cpu_usage {
            Some(value) => notes.push(format!(
                "cpu_usage: 1-minute host load average / online CPUs = {value:.2}"
            )),
            None => {
                notes.push("cpu_usage: host load is not readable on this platform; 0.0".to_string())
            }
        }

        let memory_usage = host_memory_usage();
        match memory_usage {
            Some(value) => notes.push(format!(
                "memory_usage: host (MemTotal - MemAvailable) / MemTotal = {value:.2}"
            )),
            None => notes.push(
                "memory_usage: host memory is not readable on this platform; 0.0".to_string(),
            ),
        }

        let disk_usage = match disk {
            Some(reading) => {
                notes.push(format!(
                    "disk_usage: df -Pk {} reports {:.0}% of the mounted filesystem used",
                    reading.path,
                    reading.used_pct * 100.0
                ));
                reading.used_pct.clamp(0.0, 1.0)
            }
            None => {
                notes.push(
                    "disk_usage: df is unavailable or its output is unparseable on this host; \
                     filesystem usage unknown, not 0"
                        .to_string(),
                );
                0.0
            }
        };
        match failed_services {
            Some(count) => notes.push(format!(
                "failed_services: systemctl --failed lists {count} failed unit(s)"
            )),
            None => notes.push(
                "failed_services: systemctl is unavailable or exited non-zero on this host; \
                 failed services unknown, not 0"
                    .to_string(),
            ),
        }
        notes.push(format!(
            "store footprint: evolution store occupies {footprint} bytes (informational; \
             disk_usage is the mounted filesystem)"
        ));
        notes.push(
            "network_active: the store records no network traffic, so activity is not claimed; see network_route"
                .to_string(),
        );
        match route {
            NetworkRoute::DefaultRoute => notes.push(
                "network_route: /proc/net/route lists a default route; no endpoint was contacted"
                    .to_string(),
            ),
            NetworkRoute::NoDefaultRoute => notes.push(
                "network_route: /proc/net/route was read and lists no default route".to_string(),
            ),
            NetworkRoute::Unknown => notes.push(
                "network_route: /proc/net/route is not readable; network state unknown".to_string(),
            ),
        }
        match endpoint {
            EndpointReading::Reachable => notes.push(
                "endpoint_reachability: an endpoint answered (connection completed or was refused)"
                    .to_string(),
            ),
            EndpointReading::Lost => notes.push(
                "endpoint_reachability: the endpoint did not answer within the connect budget"
                    .to_string(),
            ),
            EndpointReading::Unknown => notes.push(
                "endpoint_reachability: no endpoint was probed (routing table unreadable)"
                    .to_string(),
            ),
        }

        Ok(ResourceMetrics {
            cpu_usage: cpu_usage.unwrap_or(0.0),
            memory_usage: memory_usage.unwrap_or(0.0),
            disk_usage,
            disk_usage_known: disk.is_some(),
            network_active: false,
            network_route: route,
            endpoint_reachability: endpoint,
            failed_services,
            notes,
        })
    }

    /// Monitor: provider health as measured from recorded experiences.
    ///
    /// The store does not attribute outcomes to a provider or model, so the
    /// rate is the failure share of the sampled experiences and `notes` says
    /// so. With no sample the status is explicitly unavailable instead of
    /// optimistic.
    fn collect_provider_status(&self) -> Result<ProviderStatus> {
        let experiences = self.store.experience_stats(WINDOW_MS)?;
        let reflection = self.store.reflection_stats()?;
        let mut notes = Vec::new();

        let (error_rate, available, sample_count) = match experiences.authoritative_sample() {
            Some(sample) => {
                let rate = sample.failure_rate().unwrap_or(0.0);
                notes.push(format!(
                    "error_rate: {:.0}% of {} sampled experience(s) failed (outcomes are not attributed to a provider)",
                    rate * 100.0,
                    sample.total
                ));
                (rate, rate < UNHEALTHY_FAILURE_RATE, sample.total)
            }
            None => {
                notes.push(format!(
                    "available: unverified, only {} experience(s) recorded (need {})",
                    experiences.all_time.total,
                    metrics::MIN_SAMPLE
                ));
                (0.0, false, experiences.all_time.total)
            }
        };

        let latency_ms = match experiences.avg_duration_ms() {
            Some(ms) => {
                notes.push(format!(
                    "latency_ms: mean recorded experience duration ({ms}ms)"
                ));
                ms
            }
            None => match reflection.avg_latency_ms {
                Some(ms) => {
                    notes.push(format!(
                        "latency_ms: no experience recorded a duration; using mean reflection-job latency ({ms}ms)"
                    ));
                    ms
                }
                None => {
                    notes.push(
                        "latency_ms: no duration recorded in experiences or reflection jobs; 0"
                            .to_string(),
                    );
                    0
                }
            },
        };

        Ok(ProviderStatus {
            available,
            latency_ms,
            error_rate,
            sample_count,
            notes,
        })
    }

    /// Monitor: the actual recorded experiences inside the recency window.
    ///
    /// `duration_ms`/`tokens_used` are `0` when the producer did not record
    /// them; see [`metrics::recorded_duration_ms`].
    fn collect_execution_outcomes(&self) -> Result<Vec<ExecutionOutcome>> {
        let since = now_ms().saturating_sub(WINDOW_MS);
        let experiences = self.store.list_experiences_since(since, MAX_OUTCOMES)?;
        Ok(experiences
            .iter()
            .map(|e| ExecutionOutcome {
                task_id: e.id.clone(),
                success: e.outcome == ExperienceOutcome::Success,
                duration_ms: metrics::recorded_duration_ms(e).unwrap_or(0),
                tokens_used: metrics::recorded_tokens_used(e).unwrap_or(0),
            })
            .collect())
    }

    /// Monitor: failing experiences and failed reflection jobs, newest first.
    fn collect_failures(&self) -> Result<Vec<FailureRecord>> {
        let since = now_ms().saturating_sub(WINDOW_MS);
        let mut failures: Vec<FailureRecord> = self
            .store
            .list_failed_experiences_since(since, MAX_FAILURES)?
            .into_iter()
            .map(|e| FailureRecord {
                id: e.id.clone(),
                component: e.task_category.clone(),
                error_type: e.outcome.as_str().to_string(),
                message: if e.errors.is_empty() {
                    format!(
                        "{} task reported outcome `{}` with no error detail",
                        e.task_category,
                        e.outcome.as_str()
                    )
                } else {
                    e.errors.join("; ")
                },
                timestamp_ms: e.timestamp_ms,
            })
            .collect();

        for job in self.store.list_failed_jobs(MAX_FAILURES)? {
            failures.push(FailureRecord {
                id: job.id.clone(),
                component: "reflection".to_string(),
                error_type: "reflection_job_failed".to_string(),
                message: job.error.unwrap_or_else(|| {
                    "reflection job failed without an error message".to_string()
                }),
                timestamp_ms: job.updated_ms,
            });
        }

        failures.sort_by_key(|f| std::cmp::Reverse(f.timestamp_ms));
        Ok(failures)
    }

    /// Execute one planned action.
    ///
    /// # What this loop can and cannot do
    ///
    /// A [`MapekLoop`] owns exactly one handle — the shared [`EvolutionStore`].
    /// It has no broker, provider client, scheduler, agent registry or
    /// runtime-configuration handle, so it cannot itself restart a provider,
    /// switch a model, edit a skill or change a runtime setting. Execution is
    /// therefore split in two, and neither half claims more than it did:
    ///
    /// - **`ActionType::Recovery` targeting [`REFLECTION_TARGET`]** — a real
    ///   remediation the store can perform: every failed reflection job below
    ///   [`MAX_REFLECTION_ATTEMPTS`] is put back to `pending` so that
    ///   [`crate::reflection::ReflectionEngine::process_pending`] retries it,
    ///   and the new state is read back to verify the write landed. Only this
    ///   path reports `success: true` ([`ActionOutcome::Remediated`]).
    /// - **Everything else** — not performed. The action is persisted as a
    ///   pending [`ProposalKind::RecordIssue`] proposal so it reaches an
    ///   operator instead of being dropped, and the result reports
    ///   `success: false`, [`ActionOutcome::RecordedForReview`], an `error`
    ///   naming the missing actuator, and the output `"recorded action <id>
    ///   for operator review; no automatic remediation for <type>"`.
    ///
    /// A recovery action with no failed job to re-queue does nothing at all
    /// and says so ([`ActionOutcome::NotExecuted`]); failed jobs that already
    /// exhausted their retries are escalated for review rather than retried
    /// forever. Store failures are surfaced in the action's `error`/
    /// `outcome` — never swallowed — and keep
    /// [`ExecutionResult::overall_success`] `false`.
    fn execute_action(&self, action: &PlannedAction) -> Result<ActionResult> {
        match &action.action_type {
            ActionType::Recovery if action.target == REFLECTION_TARGET => {
                self.recover_failed_reflection_jobs(action)
            }
            ActionType::Recovery => self.record_for_review(
                action,
                &format!(
                    "the loop's only recovery actuator re-queues failed reflection jobs; it cannot recover `{}`",
                    action.target
                ),
            ),
            other => {
                let reason = format!(
                    "this loop has no {} actuator: it can only re-queue failed reflection jobs through the store",
                    other.as_str()
                );
                self.record_for_review(action, &reason)
            }
        }
    }

    /// Re-queue failed reflection jobs so the reflection engine retries them.
    ///
    /// This is the loop's only real actuator, and it is store-backed and
    /// verifiable: each job row is rewritten to `pending` with a fresh
    /// `updated_ms` (attempts and last error are preserved so retry accounting
    /// stays honest) and the result is read back with
    /// [`EvolutionStore::list_failed_jobs`] before success is claimed.
    ///
    /// - No failed job at all: nothing is written, outcome
    ///   [`ActionOutcome::NotExecuted`].
    /// - Every failed job already at [`MAX_REFLECTION_ATTEMPTS`]: nothing is
    ///   rewritten, and the situation is escalated for operator review.
    /// - Otherwise: the jobs are re-queued; the returned output names them.
    fn recover_failed_reflection_jobs(&self, action: &PlannedAction) -> Result<ActionResult> {
        let failed = match self.store.list_failed_jobs(MAX_FAILURES) {
            Ok(jobs) => jobs,
            Err(e) => {
                return Ok(not_executed(
                    action,
                    format!("could not read failed reflection jobs: {e}"),
                ));
            }
        };
        if failed.is_empty() {
            return Ok(not_executed(
                action,
                "no failed reflection job exists to re-queue; nothing was changed".to_string(),
            ));
        }

        let mut requeued: Vec<String> = Vec::new();
        let mut exhausted: Vec<String> = Vec::new();
        let total_failed = failed.len();

        for mut job in failed {
            if job.attempts >= MAX_REFLECTION_ATTEMPTS {
                exhausted.push(job.id);
                continue;
            }
            job.status = ReflectionJobStatus::Pending;
            job.updated_ms = now_ms();
            if let Err(e) = self.store.save_job(&job) {
                return Ok(not_executed(
                    action,
                    format!(
                        "could not re-queue reflection job {}: {e} ({} of {total_failed} job(s) re-queued before the failure)",
                        job.id,
                        requeued.len()
                    ),
                ));
            }
            requeued.push(job.id);
        }

        if requeued.is_empty() {
            return self.record_for_review(
                action,
                &format!(
                    "all {total_failed} failed reflection job(s) reached the {MAX_REFLECTION_ATTEMPTS}-attempt retry cap"
                ),
            );
        }

        // Verify the side effect instead of trusting the write: the jobs must
        // no longer be listed as failed.
        let still_failed: Vec<String> = match self.store.list_failed_jobs(MAX_FAILURES) {
            Ok(jobs) => jobs
                .into_iter()
                .map(|j| j.id)
                .filter(|id| requeued.contains(id))
                .collect(),
            Err(e) => {
                return Ok(not_executed(
                    action,
                    format!("could not verify the re-queued reflection jobs: {e}"),
                ));
            }
        };
        if !still_failed.is_empty() {
            return Ok(not_executed(
                action,
                format!(
                    "re-queue did not persist for reflection job(s) {}: the store still lists them as failed",
                    still_failed.join(", ")
                ),
            ));
        }

        let mut output = format!(
            "re-queued {} failed reflection job(s) as pending for retry: {}",
            requeued.len(),
            requeued.join(", ")
        );
        if !exhausted.is_empty() {
            output.push_str(&format!(
                "; {} job(s) left failed at the {MAX_REFLECTION_ATTEMPTS}-attempt cap: {}",
                exhausted.len(),
                exhausted.join(", ")
            ));
        }

        Ok(ActionResult {
            action_id: action.id.clone(),
            success: true,
            output,
            error: None,
            outcome: ActionOutcome::Remediated,
            review_proposal_id: None,
        })
    }

    /// Persist an action the loop cannot perform as a pending
    /// [`ProposalKind::RecordIssue`] proposal for operator review.
    ///
    /// This records; it does not remediate, and the returned [`ActionResult`]
    /// says exactly that: `success: false`,
    /// [`ActionOutcome::RecordedForReview`], the persisted proposal id, an
    /// `error` naming the missing actuator, and an output that spells out
    /// "recorded … for operator review; no automatic remediation for …". The
    /// proposal is read back through the store by the caller's own queries, so
    /// the effect is verifiable.
    fn record_for_review(&self, action: &PlannedAction, reason: &str) -> Result<ActionResult> {
        let proposal = LearningProposal {
            id: gen_id("prop"),
            experience_id: None,
            agent_id: MAPEK_AGENT.to_string(),
            kind: ProposalKind::RecordIssue,
            summary: format!(
                "MAPE-K action {} ({}) on `{}` needs an operator",
                action.id,
                action.action_type.as_str(),
                action.target
            ),
            details: serde_json::json!({
                "source": MAPEK_AGENT,
                "action_id": action.id,
                "action_type": action.action_type.as_str(),
                "target": action.target,
                "description": action.description,
                "parameters": action.parameters,
                "no_automatic_remediation": reason,
            }),
            confidence: 1.0,
            requires_approval: true,
            created_ms: now_ms(),
            status: ProposalStatus::Pending,
        };

        if let Err(e) = self.store.save_proposal(&proposal) {
            return Ok(not_executed(
                action,
                format!(
                    "could not persist the operator-review record for action {}: {e}",
                    action.id
                ),
            ));
        }

        Ok(ActionResult {
            action_id: action.id.clone(),
            success: false,
            output: format!(
                "recorded action {} for operator review; no automatic remediation for {}",
                action.id,
                action.action_type.as_str()
            ),
            error: Some(format!(
                "no automatic remediation for {} on `{}`: {reason}",
                action.action_type.as_str(),
                action.target
            )),
            outcome: ActionOutcome::RecordedForReview,
            review_proposal_id: Some(proposal.id),
        })
    }
}

/// An action result for work that was neither remediated nor recorded.
fn not_executed(action: &PlannedAction, reason: String) -> ActionResult {
    ActionResult {
        action_id: action.id.clone(),
        success: false,
        output: format!(
            "no effect for action {} ({}) on `{}`: {reason}",
            action.id,
            action.action_type.as_str(),
            action.target
        ),
        error: Some(reason),
        outcome: ActionOutcome::NotExecuted,
        review_proposal_id: None,
    }
}

/// Outcome of one complete MAPE-K cycle.
///
/// `execution_success` is `true` only when the plan produced by this cycle was
/// really remediated; `failure_reason` explains any shortfall instead of
/// hiding it.
#[derive(Clone, Debug)]
pub struct CycleResult {
    pub cycle_number: u32,
    pub monitoring_data_id: String,
    pub analysis_id: String,
    pub plan_id: String,
    /// `true` only when every planned action was
    /// [`ActionOutcome::Remediated`] and the plan was not empty.
    pub execution_success: bool,
    pub timestamp_ms: u64,
    /// How many actions this cycle's plan held.
    pub planned_actions: usize,
    /// How many of them a real change was applied for.
    pub remediated_actions: usize,
    /// How many of them were only recorded for operator review.
    pub recorded_for_review: usize,
    /// Why the cycle did not remediate its plan, when it did not.
    pub failure_reason: Option<String>,
}

// ---------- host probes ----------
//
// These read real numbers from the host. They are deliberately best-effort:
// a platform without `/proc` returns `None` (and the collector says so in
// `notes`) rather than a made-up value.

/// The `procfs` value for `key` in kB, e.g. `("MemTotal:", 16_000_000)`.
fn proc_meminfo_kb(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
    })
}

/// Fraction of host memory currently in use, from `/proc/meminfo`.
fn host_memory_usage() -> Option<f32> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let total = proc_meminfo_kb(&text, "MemTotal:")?;
    let available = proc_meminfo_kb(&text, "MemAvailable:")?;
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(available);
    Some((used as f32 / total as f32).clamp(0.0, 1.0))
}

/// Total host memory in bytes, from `/proc/meminfo`'s `MemTotal`.
fn host_memory_total_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    proc_meminfo_kb(&text, "MemTotal:")?.checked_mul(1024)
}

/// Online CPUs counted from `processor` lines of `/proc/cpuinfo`.
fn proc_cpu_count(text: &str) -> Option<usize> {
    let count = text
        .lines()
        .filter(|line| line.starts_with("processor"))
        .count();
    (count > 0).then_some(count)
}

/// Number of online CPUs, from `/proc/cpuinfo`; `1` when unreadable.
fn host_cpu_count() -> usize {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| proc_cpu_count(&text))
        .unwrap_or(1)
}

/// Online CPUs, or `None` when `/proc/cpuinfo` cannot answer — the host
/// profile records an unread count as unknown instead of the fallback `1`.
fn host_cpu_count_known() -> Option<usize> {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| proc_cpu_count(&text))
}

/// 1-minute host load average divided by the online CPU count, clamped to
/// `0.0..=1.0` — a load-pressure reading for CPU utilisation that needs no
/// two-sample delta.
fn host_cpu_pressure() -> Option<f32> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    let one_minute: f32 = text.split_whitespace().next()?.parse().ok()?;
    Some((one_minute / host_cpu_count() as f32).clamp(0.0, 1.0))
}

/// First non-zero gateway in `/proc/net/route`, decoded from the kernel's
/// little-endian hex field.
fn gateway_from_route_table(text: &str) -> Option<std::net::Ipv4Addr> {
    text.lines().skip(1).find_map(|line| {
        line.split_whitespace()
            .nth(2)
            .and_then(|gateway| u32::from_str_radix(gateway, 16).ok())
            .filter(|gateway| *gateway != 0)
            .map(|gateway| std::net::Ipv4Addr::from(gateway.to_le_bytes()))
    })
}

/// A route-table line whose gateway (third column, hex) is non-zero.
fn route_table_lists_a_default_route(text: &str) -> bool {
    gateway_from_route_table(text).is_some()
}

/// Does the host have a default route? Reads the kernel's own table — no
/// traffic, no timeout needed, and an unreadable file is `Unknown`, never
/// `NoDefaultRoute`.
fn probe_default_route() -> NetworkRoute {
    match std::fs::read_to_string("/proc/net/route") {
        Ok(text) if route_table_lists_a_default_route(&text) => NetworkRoute::DefaultRoute,
        Ok(_) => NetworkRoute::NoDefaultRoute,
        Err(_) => NetworkRoute::Unknown,
    }
}

/// Grade one connect attempt: a completed or refused connection proves the
/// host answered; the unreachable variants are a lost endpoint; anything else
/// is not evidence either way.
fn endpoint_reading_from_result(result: std::io::Result<std::net::TcpStream>) -> EndpointReading {
    match result {
        Ok(stream) => {
            drop(stream);
            EndpointReading::Reachable
        }
        Err(error) => match error.kind() {
            std::io::ErrorKind::ConnectionRefused => EndpointReading::Reachable,
            std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::HostUnreachable
            | std::io::ErrorKind::NetworkUnreachable => EndpointReading::Lost,
            _ => EndpointReading::Unknown,
        },
    }
}

/// Reachability of `<gateway>:53`, reading the gateway from `path`.
///
/// An unreadable routing table is `Unknown` and no connection is attempted; a
/// readable table without a default gateway is a positively known dead end.
fn endpoint_reachability_from_route_file(path: &std::path::Path) -> EndpointReading {
    let Ok(text) = std::fs::read_to_string(path) else {
        return EndpointReading::Unknown;
    };
    let Some(gateway) = gateway_from_route_table(&text) else {
        return EndpointReading::Lost;
    };
    endpoint_reading_from_result(std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from((gateway, ENDPOINT_PORT)),
        ENDPOINT_TIMEOUT,
    ))
}

/// Does an endpoint on the host's default gateway answer? A route proves a
/// network exists, not that anything answers; a refused port still proves the
/// host replied.
fn probe_endpoint_reachability() -> EndpointReading {
    endpoint_reachability_from_route_file(std::path::Path::new("/proc/net/route"))
}

/// Path whose mounted filesystem is measured: the process's working directory
/// (the workspace root in practice), or `/` when that cannot be resolved.
fn disk_probe_path() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
}

/// `Use%` from the last data line of POSIX `df -Pk` output, as a `0.0..=1.0`
/// share. Parsing is split from spawning so its shape can be tested without
/// depending on the host's filesystem.
fn df_used_pct(text: &str) -> Option<f32> {
    let line = text.lines().map(str::trim).rfind(|line| !line.is_empty())?;
    let percent = line.split_whitespace().nth(4)?.strip_suffix('%')?;
    let percent: f32 = percent.parse().ok()?;
    if !(0.0..=100.0).contains(&percent) {
        return None;
    }
    Some(percent / 100.0)
}

/// Project hours until the used share reaches the target share, from the first
/// and last samples.
///
/// Returns `None` unless at least [`DISK_PROJECTION_MIN_SAMPLES`] samples span
/// at least [`DISK_PROJECTION_MIN_SPAN_MS`], the share is still below the
/// target, the rate is positive, and the projection falls inside the
/// [`DISK_PROJECTION_HORIZON_HOURS`] horizon.
fn project_disk_fill(samples: &[DiskSample]) -> Option<DiskProjection> {
    if samples.len() < DISK_PROJECTION_MIN_SAMPLES {
        return None;
    }
    let first = samples.first()?;
    let last = samples.last()?;
    let span_ms = last.timestamp_ms.checked_sub(first.timestamp_ms)?;
    if span_ms < DISK_PROJECTION_MIN_SPAN_MS {
        return None;
    }
    if last.used_pct >= DISK_PROJECTION_TARGET_SHARE {
        return None;
    }
    let span_hours = span_ms as f32 / 3_600_000.0;
    let per_hour = (last.used_pct - first.used_pct) / span_hours;
    if per_hour <= 0.0 {
        return None;
    }
    let hours_to_threshold = (DISK_PROJECTION_TARGET_SHARE - last.used_pct) / per_hour;
    if !hours_to_threshold.is_finite() || hours_to_threshold >= DISK_PROJECTION_HORIZON_HOURS {
        return None;
    }
    Some(DiskProjection {
        hours_to_threshold,
        current_pct: last.used_pct,
    })
}

/// `df -Pk` output for `path`, or `None` when `df` is absent, exits non-zero
/// or prints bytes that are not UTF-8.
fn df_output(path: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("df")
        .arg("-Pk")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Used share of the filesystem holding [`disk_probe_path`], from `df -Pk`.
///
/// Any failure — `df` absent, a non-zero exit, unparseable output — is `None`,
/// never `Some(0.0)`: an unread filesystem is unknown, not empty.
fn probe_disk_usage() -> Option<DiskReading> {
    let path = disk_probe_path();
    let text = df_output(&path)?;
    Some(DiskReading {
        path: path.display().to_string(),
        used_pct: df_used_pct(&text)?,
    })
}

/// (total, available) bytes from the last data line of POSIX `df -Pk` output.
///
/// A filesystem path containing spaces shifts the columns, so that output is
/// treated as unparseable rather than misread.
fn df_capacity_bytes(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().map(str::trim).rfind(|line| !line.is_empty())?;
    let mut fields = line.split_whitespace();
    let _filesystem = fields.next()?;
    let total_kb = fields.next()?.parse::<u64>().ok()?;
    let _used_kb = fields.next()?;
    let free_kb = fields.next()?.parse::<u64>().ok()?;
    Some((total_kb.checked_mul(1024)?, free_kb.checked_mul(1024)?))
}

/// (total, available) bytes of the filesystem holding [`disk_probe_path`].
fn probe_disk_capacity() -> Option<(u64, u64)> {
    df_capacity_bytes(&df_output(&disk_probe_path())?)
}

/// Failed systemd units from `systemctl --failed --no-legend --plain`.
///
/// `None` when `systemctl` cannot be spawned, exits non-zero (no systemd, no
/// bus) or prints bytes that are not UTF-8: unknown, never `Some(0)`.
fn probe_failed_services() -> Option<usize> {
    let output = std::process::Command::new("systemctl")
        .args(["--failed", "--no-legend", "--plain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(count_failed_units(&text))
}

/// One `--no-legend --plain` line per failed unit.
fn count_failed_units(text: &str) -> usize {
    text.lines().filter(|line| !line.trim().is_empty()).count()
}

/// The readings a [`HostProfile`] is built from; `None` means the probe could
/// not answer.
#[derive(Clone, Debug, Default)]
struct HostReadings {
    cpus: Option<usize>,
    memory_total_bytes: Option<u64>,
    disk_bytes: Option<(u64, u64)>,
    failed_services: Option<usize>,
    route: NetworkRoute,
}

/// Build a profile from already-taken readings, leaving every unknown field at
/// `0`/`None`/`false` instead of guessing.
fn host_profile_from(readings: HostReadings, captured_ms: u64) -> HostProfile {
    let (disk_total_bytes, disk_free_bytes) = readings.disk_bytes.unwrap_or((0, 0));
    HostProfile {
        cpus: readings.cpus.unwrap_or(0),
        memory_total_bytes: readings.memory_total_bytes.unwrap_or(0),
        disk_total_bytes,
        disk_free_bytes,
        failed_services: readings.failed_services,
        default_route: readings.route == NetworkRoute::DefaultRoute,
        captured_ms,
    }
}

/// Read the host profile from the same probes [`MapekLoop::monitor`] uses.
pub fn capture_host_profile(now_ms: u64) -> HostProfile {
    host_profile_from(
        HostReadings {
            cpus: host_cpu_count_known(),
            memory_total_bytes: host_memory_total_bytes(),
            disk_bytes: probe_disk_capacity(),
            failed_services: probe_failed_services(),
            route: probe_default_route(),
        },
        now_ms,
    )
}

/// Persisted host profile, if one was captured. A corrupt payload is an error.
fn load_host_profile(store: &EvolutionStore) -> Result<Option<HostProfile>> {
    store
        .list_engine_records(HOST_NAMESPACE)?
        .into_iter()
        .find(|(id, _)| id == HOST_ID)
        .map(|(_, payload)| from_json_value::<HostProfile>(payload))
        .transpose()
}

/// Overwrite the single persisted host profile row.
fn save_host_profile(store: &EvolutionStore, profile: &HostProfile) -> Result<()> {
    store.save_engine_record(HOST_NAMESPACE, HOST_ID, &to_json_value(profile)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knowledge_and_cycle_count_survive_a_store_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evolution.db");

        {
            let store = EvolutionStore::open(&path).unwrap();
            let mut loop_ = MapekLoop::new(store).unwrap();
            quiet_host(&mut loop_);
            let result = loop_.run_cycle().unwrap();
            assert_eq!(result.cycle_number, 1);
            assert_eq!(loop_.knowledge.decisions.len(), 1);
            assert_eq!(loop_.knowledge.outcomes.len(), 1);
        }

        let store = EvolutionStore::open(&path).unwrap();
        let mut loop_ = MapekLoop::new(store).unwrap();
        quiet_host(&mut loop_);
        assert_eq!(loop_.cycle_count, 1);
        assert_eq!(loop_.knowledge.decisions.len(), 1);
        assert_eq!(loop_.knowledge.outcomes.len(), 1);

        // Cycle numbering continues from the persisted count, not zero.
        let result = loop_.run_cycle().unwrap();
        assert_eq!(result.cycle_number, 2);
        assert_eq!(loop_.knowledge.decisions.len(), 2);
    }

    #[test]
    fn a_fresh_store_starts_with_empty_knowledge() {
        let loop_ = MapekLoop::new(EvolutionStore::open_in_memory().unwrap()).unwrap();
        assert_eq!(loop_.cycle_count, 0);
        assert!(loop_.knowledge.decisions.is_empty());
        assert!(loop_.knowledge.outcomes.is_empty());
    }

    /// Pin every host probe so a test never reads the machine's real disks,
    /// routes, endpoints or systemd state.
    fn quiet_host(loop_: &mut MapekLoop) {
        loop_.route_probe = Box::new(|| NetworkRoute::DefaultRoute);
        loop_.disk_probe = Box::new(|| {
            Some(DiskReading {
                path: "/".to_string(),
                used_pct: 0.10,
            })
        });
        loop_.services_probe = Box::new(|| Some(0));
        loop_.endpoint_probe = Some(std::sync::Arc::new(|| EndpointReading::Reachable));
    }

    fn scripted_loop(readings: Vec<NetworkRoute>) -> MapekLoop {
        let mut loop_ = MapekLoop::new(EvolutionStore::open_in_memory().unwrap()).unwrap();
        quiet_host(&mut loop_);
        let script = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            readings,
        )));
        loop_.route_probe = Box::new(move || {
            script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(NetworkRoute::Unknown)
        });
        loop_
    }

    fn scripted_endpoint_loop(readings: Vec<EndpointReading>) -> MapekLoop {
        let mut loop_ = MapekLoop::new(EvolutionStore::open_in_memory().unwrap()).unwrap();
        quiet_host(&mut loop_);
        loop_.route_probe = Box::new(|| NetworkRoute::Unknown);
        let script = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            readings,
        )));
        loop_.endpoint_probe = Some(std::sync::Arc::new(move || {
            script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(EndpointReading::Unknown)
        }));
        loop_
    }

    fn network_issues(analysis: &AnalysisResult) -> usize {
        analysis
            .issues
            .iter()
            .filter(|issue| issue.component == "network")
            .count()
    }

    #[test]
    fn a_lost_default_route_is_reported_on_the_transition_not_every_tick() {
        let mut loop_ = scripted_loop(vec![
            NetworkRoute::DefaultRoute,
            NetworkRoute::NoDefaultRoute,
            NetworkRoute::NoDefaultRoute,
        ]);

        let baseline = loop_.monitor().unwrap();
        assert!(baseline.network_transition.is_none());
        assert_eq!(
            baseline.resource_metrics.network_route,
            NetworkRoute::DefaultRoute
        );
        assert!(!baseline.resource_metrics.network_active);

        let lost = loop_.monitor().unwrap();
        let transition = lost
            .network_transition
            .as_ref()
            .expect("the lost route must be reported on the tick it is observed");
        assert_eq!(transition.id, "network.default_route.lost");
        assert_eq!(transition.previous, NetworkRoute::DefaultRoute);
        assert_eq!(transition.current, NetworkRoute::NoDefaultRoute);
        assert_eq!(
            lost.resource_metrics.network_route,
            NetworkRoute::NoDefaultRoute
        );
        assert_eq!(network_issues(&loop_.analyze(&lost).unwrap()), 1);

        let quiet = loop_.monitor().unwrap();
        assert!(
            quiet.network_transition.is_none(),
            "the same loss must not be reported again: {quiet:?}"
        );
        assert_eq!(network_issues(&loop_.analyze(&quiet).unwrap()), 0);
    }

    #[test]
    fn an_unreadable_route_table_is_unknown_not_down() {
        let mut loop_ = scripted_loop(vec![NetworkRoute::Unknown, NetworkRoute::Unknown]);

        let data = loop_.monitor().unwrap();
        assert_eq!(data.resource_metrics.network_route, NetworkRoute::Unknown);
        assert!(!data.resource_metrics.network_active);
        assert!(data.network_transition.is_none());
        let notes = data.resource_metrics.notes.join("\n");
        assert!(notes.contains("not readable"), "{notes}");
        assert!(
            !notes.contains("lists no default route"),
            "an unread probe is not a missing route: {notes}"
        );
        assert!(!notes.to_lowercase().contains("network down"), "{notes}");
        assert_eq!(network_issues(&loop_.analyze(&data).unwrap()), 0);

        let again = loop_.monitor().unwrap();
        assert!(again.network_transition.is_none());
        assert_eq!(again.resource_metrics.network_route, NetworkRoute::Unknown);
    }

    #[test]
    fn a_route_that_never_changed_is_reported_once() {
        let mut loop_ = scripted_loop(vec![
            NetworkRoute::DefaultRoute,
            NetworkRoute::NoDefaultRoute,
            NetworkRoute::NoDefaultRoute,
            NetworkRoute::NoDefaultRoute,
        ]);

        assert!(loop_.monitor().unwrap().network_transition.is_none());
        let changed = loop_.monitor().unwrap();
        assert!(changed.network_transition.is_some());
        for _ in 0..2 {
            assert!(loop_.monitor().unwrap().network_transition.is_none());
        }

        let mut reopened = MapekLoop::new(loop_.store().clone()).unwrap();
        reopened.route_probe = Box::new(|| NetworkRoute::DefaultRoute);
        reopened.endpoint_probe = Some(std::sync::Arc::new(|| EndpointReading::Reachable));
        let restored = reopened.monitor().unwrap();
        let transition = restored
            .network_transition
            .as_ref()
            .expect("a fresh loop must compare against the durable reading, not start over");
        assert_eq!(transition.id, "network.default_route.restored");
        assert_eq!(transition.previous, NetworkRoute::NoDefaultRoute);
        assert!(reopened.monitor().unwrap().network_transition.is_none());
    }

    fn capacity_loop(disk: Vec<Option<f32>>, services: Vec<Option<usize>>) -> MapekLoop {
        let mut loop_ = MapekLoop::new(EvolutionStore::open_in_memory().unwrap()).unwrap();
        loop_.route_probe = Box::new(|| NetworkRoute::Unknown);
        loop_.endpoint_probe = Some(std::sync::Arc::new(|| EndpointReading::Reachable));
        let disk = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            disk.into_iter()
                .map(|used_pct| {
                    used_pct.map(|used_pct| DiskReading {
                        path: "/data".to_string(),
                        used_pct,
                    })
                })
                .collect::<Vec<_>>(),
        )));
        loop_.disk_probe = Box::new(move || disk.lock().unwrap().pop_front().flatten());
        let services = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::from(services),
        ));
        loop_.services_probe = Box::new(move || services.lock().unwrap().pop_front().flatten());
        loop_
    }

    fn issue_count(analysis: &AnalysisResult, component: &str) -> usize {
        analysis
            .issues
            .iter()
            .filter(|issue| issue.component == component)
            .count()
    }

    fn only_issue<'a>(analysis: &'a AnalysisResult, component: &str) -> &'a Issue {
        let matching: Vec<&Issue> = analysis
            .issues
            .iter()
            .filter(|issue| issue.component == component)
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "expected exactly one `{component}` issue: {analysis:?}"
        );
        matching[0]
    }

    #[test]
    fn a_disk_above_the_threshold_is_reported_once() {
        let mut loop_ = capacity_loop(
            vec![Some(0.85), Some(0.91), Some(0.91), Some(0.96), Some(0.96)],
            vec![None; 5],
        );

        let baseline = loop_.monitor().unwrap();
        assert!(baseline.resource_metrics.disk_usage_known);
        assert!((baseline.resource_metrics.disk_usage - 0.85).abs() < 1e-6);
        assert!(
            baseline.disk_transition.is_none(),
            "the first readable reading is a baseline: {baseline:?}"
        );

        let filled = loop_.monitor().unwrap();
        assert!(filled.resource_metrics.disk_usage_known);
        assert!((filled.resource_metrics.disk_usage - 0.91).abs() < 1e-6);
        let transition = filled
            .disk_transition
            .as_ref()
            .expect("a filling disk must be reported on the tick it is observed");
        assert_eq!(transition.id, "disk.fill");
        assert_eq!(transition.path, "/data");
        let analysis = loop_.analyze(&filled).unwrap();
        assert_eq!(
            only_issue(&analysis, "disk").severity,
            IssueSeverity::Medium
        );

        let quiet = loop_.monitor().unwrap();
        assert!(quiet.disk_transition.is_none(), "{quiet:?}");
        assert_eq!(issue_count(&loop_.analyze(&quiet).unwrap(), "disk"), 0);

        let escalated = loop_.monitor().unwrap();
        assert_eq!(escalated.disk_transition.as_ref().unwrap().id, "disk.fill");
        let analysis = loop_.analyze(&escalated).unwrap();
        assert_eq!(only_issue(&analysis, "disk").severity, IssueSeverity::High);

        let quiet = loop_.monitor().unwrap();
        assert!(quiet.disk_transition.is_none(), "{quiet:?}");

        let mut reopened = MapekLoop::new(loop_.store().clone()).unwrap();
        reopened.route_probe = Box::new(|| NetworkRoute::Unknown);
        reopened.disk_probe = Box::new(|| {
            Some(DiskReading {
                path: "/data".to_string(),
                used_pct: 0.96,
            })
        });
        reopened.services_probe = Box::new(|| Some(0));
        reopened.endpoint_probe = Some(std::sync::Arc::new(|| EndpointReading::Reachable));
        assert!(
            reopened.monitor().unwrap().disk_transition.is_none(),
            "the persisted band must survive a reopen"
        );
    }

    #[test]
    fn a_disk_below_the_threshold_is_silent() {
        let mut loop_ = capacity_loop(vec![Some(0.42), Some(0.89)], vec![None; 2]);

        for _ in 0..2 {
            let data = loop_.monitor().unwrap();
            assert!(data.resource_metrics.disk_usage_known);
            assert!(data.disk_transition.is_none(), "{data:?}");
            assert_eq!(issue_count(&loop_.analyze(&data).unwrap(), "disk"), 0);
        }
    }

    #[test]
    fn an_unreadable_disk_is_not_a_zero() {
        let mut loop_ = capacity_loop(
            vec![None, Some(0.50), Some(0.91), None, Some(0.91)],
            vec![None; 5],
        );

        let unknown = loop_.monitor().unwrap();
        assert!(!unknown.resource_metrics.disk_usage_known);
        assert!(unknown.disk_transition.is_none());
        let notes = unknown.resource_metrics.notes.join("\n");
        assert!(notes.contains("filesystem usage unknown"), "{notes}");
        assert_eq!(issue_count(&loop_.analyze(&unknown).unwrap(), "disk"), 0);

        let baseline = loop_.monitor().unwrap();
        assert!(
            baseline.disk_transition.is_none(),
            "the first readable reading after an unknown tick is a baseline: {baseline:?}"
        );

        let filled = loop_.monitor().unwrap();
        assert!(filled.disk_transition.is_some(), "{filled:?}");

        let unknown = loop_.monitor().unwrap();
        assert!(!unknown.resource_metrics.disk_usage_known);
        assert_eq!(issue_count(&loop_.analyze(&unknown).unwrap(), "disk"), 0);

        let quiet = loop_.monitor().unwrap();
        assert!(
            quiet.disk_transition.is_none(),
            "an unreadable tick must not overwrite the baseline: {quiet:?}"
        );
    }

    #[test]
    fn failed_services_are_counted_from_the_probe() {
        let mut loop_ = capacity_loop(
            vec![Some(0.10); 4],
            vec![Some(0), Some(2), Some(2), Some(0)],
        );

        let baseline = loop_.monitor().unwrap();
        assert_eq!(baseline.resource_metrics.failed_services, Some(0));
        assert!(
            baseline.services_transition.is_none(),
            "the first readable count is a baseline: {baseline:?}"
        );

        let failing = loop_.monitor().unwrap();
        assert_eq!(failing.resource_metrics.failed_services, Some(2));
        let transition = failing
            .services_transition
            .as_ref()
            .expect("failed services must be reported on the tick they are observed");
        assert_eq!(transition.id, "services.failed");
        assert_eq!(transition.failed, 2);
        let analysis = loop_.analyze(&failing).unwrap();
        let issue = only_issue(&analysis, "services");
        assert_eq!(issue.severity, IssueSeverity::Medium);
        assert!(issue.description.contains('2'), "{issue:?}");

        let quiet = loop_.monitor().unwrap();
        assert!(quiet.services_transition.is_none(), "{quiet:?}");
        assert_eq!(issue_count(&loop_.analyze(&quiet).unwrap(), "services"), 0);

        let recovered = loop_.monitor().unwrap();
        assert_eq!(
            recovered.services_transition.as_ref().unwrap().id,
            "services.failed.cleared"
        );
        assert_eq!(
            issue_count(&loop_.analyze(&recovered).unwrap(), "services"),
            0
        );
    }

    #[test]
    fn a_recovered_disk_clears_the_issue() {
        let mut loop_ = capacity_loop(
            vec![Some(0.60), Some(0.96), Some(0.40), Some(0.40)],
            vec![None; 4],
        );

        let baseline = loop_.monitor().unwrap();
        assert!(baseline.disk_transition.is_none(), "{baseline:?}");

        let filled = loop_.monitor().unwrap();
        let analysis = loop_.analyze(&filled).unwrap();
        assert_eq!(only_issue(&analysis, "disk").severity, IssueSeverity::High);

        let recovered = loop_.monitor().unwrap();
        let transition = recovered
            .disk_transition
            .as_ref()
            .expect("a filesystem that fell below the threshold is a crossing");
        assert_eq!(transition.id, "disk.fill.cleared");
        assert!((transition.used_pct - 0.40).abs() < 1e-6);
        assert_eq!(issue_count(&loop_.analyze(&recovered).unwrap(), "disk"), 0);

        let quiet = loop_.monitor().unwrap();
        assert!(
            quiet.disk_transition.is_none(),
            "the cleared state must not be re-reported: {quiet:?}"
        );
    }

    #[test]
    fn host_probe_output_is_parsed_not_guessed() {
        let df = "Filesystem     1024-blocks      Used  Available Capacity Mounted on\n\
                  /dev/nvme0n1p2   490000000 455000000   35000000      93% /\n";
        assert_eq!(df_used_pct(df), Some(0.93));
        assert_eq!(
            df_used_pct("Filesystem 1024-blocks Used Available Capacity Mounted on\n"),
            None,
        );
        assert_eq!(df_used_pct(""), None);
        assert_eq!(df_used_pct("/dev/sda1 100 100 0 140% /\n"), None);

        assert_eq!(count_failed_units(""), 0);
        assert_eq!(
            count_failed_units("nginx.service loaded failed failed A web server\n"),
            1,
        );

        let capacity = "Filesystem     1024-blocks      Used  Available Capacity Mounted on\n\
                        /dev/nvme0n1p2   490000000 455000000   35000000      93% /\n";
        assert_eq!(
            df_capacity_bytes(capacity),
            Some((490_000_000 * 1024, 35_000_000 * 1024))
        );
        assert_eq!(
            df_capacity_bytes("Filesystem 1024-blocks Used Available Capacity Mounted on\n"),
            None,
        );
    }

    #[test]
    fn an_endpoint_timeout_is_reported_once() {
        let mut loop_ = scripted_endpoint_loop(vec![
            EndpointReading::Reachable,
            EndpointReading::Lost,
            EndpointReading::Lost,
            EndpointReading::Reachable,
        ]);

        let baseline = loop_.monitor().unwrap();
        assert!(baseline.endpoint_transition.is_none());
        assert_eq!(
            baseline.resource_metrics.endpoint_reachability,
            EndpointReading::Reachable
        );
        assert!(!baseline.resource_metrics.network_active);
        assert_eq!(network_issues(&loop_.analyze(&baseline).unwrap()), 0);

        let lost = loop_.monitor().unwrap();
        let transition = lost
            .endpoint_transition
            .as_ref()
            .expect("a timed-out endpoint must be reported on the tick it is observed");
        assert_eq!(transition.id, "network.endpoint.lost");
        assert_eq!(transition.previous, EndpointReading::Reachable);
        assert_eq!(transition.current, EndpointReading::Lost);
        assert_eq!(
            only_issue(&loop_.analyze(&lost).unwrap(), "network").severity,
            IssueSeverity::Medium
        );

        let quiet = loop_.monitor().unwrap();
        assert!(quiet.endpoint_transition.is_none(), "{quiet:?}");
        assert_eq!(network_issues(&loop_.analyze(&quiet).unwrap()), 0);

        let restored = loop_.monitor().unwrap();
        assert_eq!(
            restored.endpoint_transition.as_ref().unwrap().id,
            "network.endpoint.restored"
        );
        assert_eq!(
            only_issue(&loop_.analyze(&restored).unwrap(), "network").severity,
            IssueSeverity::Low
        );
    }

    #[test]
    fn a_refused_connection_still_counts_as_reachable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let reading = endpoint_reading_from_result(std::net::TcpStream::connect_timeout(
            &address,
            std::time::Duration::from_millis(500),
        ));
        assert_eq!(reading, EndpointReading::Reachable);
    }

    #[test]
    fn an_unreadable_routing_table_is_not_a_lost_endpoint() {
        let missing = std::path::Path::new("/nonexistent/vikram/proc/net/route");
        assert_eq!(
            endpoint_reachability_from_route_file(missing),
            EndpointReading::Unknown
        );

        let mut loop_ = scripted_endpoint_loop(vec![
            EndpointReading::Reachable,
            EndpointReading::Unknown,
            EndpointReading::Lost,
        ]);
        assert!(loop_.monitor().unwrap().endpoint_transition.is_none());
        assert_eq!(
            loop_
                .store()
                .list_engine_records(ENDPOINT_NAMESPACE)
                .unwrap()
                .len(),
            1
        );

        let unknown = loop_.monitor().unwrap();
        assert_eq!(
            unknown.resource_metrics.endpoint_reachability,
            EndpointReading::Unknown
        );
        assert!(unknown.endpoint_transition.is_none());
        assert_eq!(network_issues(&loop_.analyze(&unknown).unwrap()), 0);
        assert_eq!(
            loop_
                .store()
                .list_engine_records(ENDPOINT_NAMESPACE)
                .unwrap()
                .len(),
            1,
            "an unreadable tick must not rewrite the durable baseline"
        );

        let lost = loop_.monitor().unwrap();
        let transition = lost
            .endpoint_transition
            .as_ref()
            .expect("an unreadable tick must not overwrite the stored baseline");
        assert_eq!(transition.previous, EndpointReading::Reachable);
        assert_eq!(transition.current, EndpointReading::Lost);
    }

    #[test]
    fn a_growing_disk_is_projected_before_it_fills() {
        let samples = vec![
            DiskSample {
                timestamp_ms: 0,
                used_pct: 0.70,
            },
            DiskSample {
                timestamp_ms: 3_600_000,
                used_pct: 0.76,
            },
            DiskSample {
                timestamp_ms: 7_200_000,
                used_pct: 0.82,
            },
        ];
        let projection = project_disk_fill(&samples).expect("3 samples over 2h growing 6%/h");
        assert!((projection.hours_to_threshold - 2.166_666_7).abs() < 0.01);
        assert!((projection.current_pct - 0.82).abs() < 1e-6);

        let mut loop_ = capacity_loop(vec![Some(0.10)], vec![None]);
        let mut data = loop_.monitor().unwrap();
        data.disk_projection = Some(projection);
        let analysis = loop_.analyze(&data).unwrap();
        let issue = only_issue(&analysis, "disk");
        assert_eq!(issue.severity, IssueSeverity::Medium);
        assert!(issue.description.contains("2.2"), "{issue:?}");
        assert!(issue.description.contains("82%"), "{issue:?}");
    }

    #[test]
    fn a_flat_disk_is_not_projected() {
        let samples = vec![
            DiskSample {
                timestamp_ms: 0,
                used_pct: 0.50,
            },
            DiskSample {
                timestamp_ms: 3_600_000,
                used_pct: 0.50,
            },
            DiskSample {
                timestamp_ms: 7_200_000,
                used_pct: 0.50,
            },
        ];
        assert_eq!(project_disk_fill(&samples), None);

        let mut loop_ = capacity_loop(vec![Some(0.50)], vec![None]);
        let data = loop_.monitor().unwrap();
        assert!(data.disk_projection.is_none());
        assert_eq!(issue_count(&loop_.analyze(&data).unwrap(), "disk"), 0);
    }

    #[test]
    fn a_single_reading_is_not_a_trend() {
        let one = [DiskSample {
            timestamp_ms: 0,
            used_pct: 0.70,
        }];
        assert_eq!(project_disk_fill(&one), None);

        let mut loop_ = capacity_loop(vec![Some(0.70), Some(0.80)], vec![None; 2]);
        let first = loop_.monitor().unwrap();
        assert_eq!(loop_.disk_history.samples.len(), 1);
        assert!(first.disk_projection.is_none());

        let second = loop_.monitor().unwrap();
        assert_eq!(loop_.disk_history.samples.len(), 2);
        assert!(second.disk_projection.is_none());
    }

    fn fixed_host_readings() -> HostReadings {
        HostReadings {
            cpus: Some(16),
            memory_total_bytes: Some(16_642_998_272),
            disk_bytes: Some((500_041_567_437, 129_385_889_792)),
            failed_services: Some(0),
            route: NetworkRoute::DefaultRoute,
        }
    }

    #[test]
    fn a_host_profile_names_what_the_probes_saw() {
        let profile = host_profile_from(fixed_host_readings(), 1_700_000_000_000);

        assert_eq!(
            describe_host_profile(&profile).as_deref(),
            Some(
                "machine: 16 CPUs, 15.5 GiB RAM, 120.5 GiB free of 465.7 GiB, \
                 0 failed services, network up"
            )
        );
        assert_eq!(profile.captured_ms, 1_700_000_000_000);
        assert_eq!(profile.disk_free_bytes, 129_385_889_792);
    }

    #[test]
    fn unknown_facts_are_left_out_of_the_line() {
        let partial = HostProfile {
            cpus: 8,
            failed_services: None,
            default_route: false,
            ..HostProfile::default()
        };
        let line = describe_host_profile(&partial).expect("cpus were read");
        assert_eq!(line, "machine: 8 CPUs");
        assert!(!line.contains("services"), "{line}");
        assert!(!line.contains("network"), "{line}");
        assert!(!line.contains("GiB"), "{line}");

        assert_eq!(
            describe_host_profile(&HostProfile {
                failed_services: Some(2),
                default_route: true,
                ..HostProfile::default()
            }),
            None,
            "no hardware fact was read, so services alone must not mint a line"
        );
        assert_eq!(describe_host_profile(&HostProfile::default()), None);
    }

    #[test]
    fn a_host_profile_survives_a_store_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evolution.db");

        let captured;
        {
            let store = EvolutionStore::open(&path).unwrap();
            let mut loop_ = MapekLoop::new(store).unwrap();
            loop_.host_probe =
                Box::new(|captured_ms| host_profile_from(fixed_host_readings(), captured_ms));
            captured = loop_.host_profile().unwrap();
            assert!(captured.captured_ms > 0);
            assert_eq!(captured.cpus, 16);
        }

        let store = EvolutionStore::open(&path).unwrap();
        let mut loop_ = MapekLoop::new(store).unwrap();
        loop_.host_probe =
            Box::new(|_| panic!("host_profile must not recapture while a persisted row exists"));
        let reopened = loop_.host_profile().unwrap();

        assert_eq!(reopened, captured);
        assert_eq!(reopened.captured_ms, captured.captured_ms);
        assert_eq!(reopened.memory_total_bytes, 16_642_998_272);
        assert_eq!(reopened.disk_total_bytes, 500_041_567_437);
        assert_eq!(reopened.disk_free_bytes, 129_385_889_792);
        assert_eq!(reopened.failed_services, Some(0));
        assert!(reopened.default_route);
    }
}
