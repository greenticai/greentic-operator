use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose};
use greentic_runner_desktop::RunStatus;
use greentic_runner_host::{
    RunnerWasiPolicy,
    component_api::node::{ExecCtx as ComponentExecCtx, TenantCtx as ComponentTenantCtx},
    config::{
        FlowRetryConfig, HostConfig, OperatorPolicy, RateLimits, SecretsPolicy, StateStorePolicy,
        WebhookPolicy,
    },
    pack::{ComponentResolution, PackRuntime},
    storage::{DynSessionStore, DynStateStore},
    trace::TraceConfig,
    validate::ValidationConfig,
};
use greentic_session::SessionStore as RunnerSessionStore;
use greentic_state::StateStore as RunnerStateStore;
use greentic_types::cbor::canonical;
use greentic_types::decode_pack_manifest;
use greentic_types::{
    ErrorCode as GreenticErrorCode, GreenticError, ReplyScope, SessionData as StoreSessionData,
    SessionKey as StoreSessionKey, TenantCtx as StoreTenantCtx, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tokio::runtime::Runtime as TokioRuntime;
use zip::ZipArchive;

/// Create a Tokio runtime for blocking async operations.
/// When called from within an existing runtime (e.g., HTTP ingress handler),
/// spawns a dedicated thread to avoid "Cannot start a runtime from within a
/// runtime" panics.
fn make_runtime_or_thread_scope<F, T>(f: F) -> T
where
    F: FnOnce(&TokioRuntime) -> T + Send,
    T: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            s.spawn(|| {
                let rt = TokioRuntime::new().expect("failed to create tokio runtime");
                f(&rt)
            })
            .join()
            .expect("provider invocation thread panicked")
        })
    } else {
        let rt = TokioRuntime::new().expect("failed to create tokio runtime");
        f(&rt)
    }
}

use crate::bundle_access::{BundleAccessConfig, BundleAccessHandle};
use crate::bundle_lifecycle::{
    BundleInventorySnapshot, BundleLifecycleRegistry, BundleLifecycleSnapshot,
};
use crate::runner_exec;
use crate::runner_integration;
use crate::runner_integration::RunFlowOptions;
use crate::runner_integration::RunnerFlavor;
use crate::runner_integration::run_flow_with_options;
use crate::runtime_core::{
    AdminAction, AdminAuthorizationHook, AuthorizationDecision, BundleFs, BundleResolver,
    BundleSource, RuntimeCore, RuntimeEvent, RuntimeHealth, RuntimeHealthStatus,
    RuntimeHookDescriptor, RuntimeSeams, ScopedStateKey, SessionKey as RuntimeSessionKey,
    SessionProvider, SessionRecord, StateProvider, default_provider_requirements,
};
use crate::runtime_state;

use crate::capabilities::{
    CAP_OAUTH_BROKER_V1, CAP_OAUTH_TOKEN_VALIDATION_V1, CAP_OP_HOOK_POST, CAP_OP_HOOK_PRE,
    CapabilityBinding, CapabilityInstallRecord, HookStage, OAUTH_OP_AWAIT_RESULT,
    OAUTH_OP_GET_ACCESS_TOKEN, OAUTH_OP_INITIATE_AUTH, OAUTH_OP_REQUEST_RESOURCE_TOKEN,
    ResolveScope, is_binding_ready, is_oauth_broker_operation, write_install_record,
};
use crate::cards::CardRenderer;
use crate::discovery;
use crate::domains::{self, Domain, ProviderPack};
use crate::offers::load_pack_offers_from_bytes;
use crate::operator_log;
use crate::secrets_gate::{self, DynSecretsManager, SecretsManagerHandle};
use crate::secrets_manager;
use crate::state_layout;

#[derive(Clone)]
pub struct OperatorContext {
    pub tenant: String,
    pub team: Option<String>,
    pub correlation_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunnerExecutionMode {
    Exec,
    Integration,
}

#[derive(Clone)]
pub struct FlowOutcome {
    pub success: bool,
    pub output: Option<JsonValue>,
    pub raw: Option<String>,
    pub error: Option<String>,
    pub mode: RunnerExecutionMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DependencyFailureMode {
    Unavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OperationStatus {
    Pending,
    Denied,
    Ok,
    Err,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OperationEnvelopeContext {
    tenant: String,
    team: Option<String>,
    correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_claims: Option<JsonValue>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OperationEnvelope {
    op_id: String,
    op_name: String,
    ctx: OperationEnvelopeContext,
    payload_cbor: Vec<u8>,
    meta_cbor: Option<Vec<u8>>,
    status: OperationStatus,
    result_cbor: Option<Vec<u8>>,
}

impl OperationEnvelope {
    fn new(op_name: &str, payload: &[u8], ctx: &OperatorContext) -> Self {
        Self {
            op_id: uuid::Uuid::new_v4().to_string(),
            op_name: op_name.to_string(),
            ctx: OperationEnvelopeContext {
                tenant: ctx.tenant.clone(),
                team: ctx.team.clone(),
                correlation_id: ctx.correlation_id.clone(),
                auth_claims: None,
            },
            payload_cbor: payload.to_vec(),
            meta_cbor: None,
            status: OperationStatus::Pending,
            result_cbor: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct HookEvalRequest {
    stage: String,
    op_name: String,
    envelope: OperationEnvelope,
}

#[derive(Debug, Deserialize)]
struct HookEvalResponse {
    decision: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    envelope: Option<OperationEnvelope>,
}

#[derive(Debug)]
enum HookChainOutcome {
    Continue,
    Denied(String),
}

#[derive(Clone, Debug)]
enum RunnerMode {
    Exec,
    Integration {
        binary: PathBuf,
        flavor: RunnerFlavor,
    },
}

#[derive(Clone)]
struct ActiveRuntimeCore {
    current: Arc<RwLock<Arc<RuntimeCore>>>,
}

impl ActiveRuntimeCore {
    fn new(core: RuntimeCore) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(core))),
        }
    }

    fn current(&self) -> Arc<RuntimeCore> {
        self.current
            .read()
            .expect("active runtime core lock poisoned")
            .clone()
    }

    fn replace(&self, core: RuntimeCore) {
        *self
            .current
            .write()
            .expect("active runtime core lock poisoned") = Arc::new(core);
    }
}

#[derive(Clone)]
struct ActiveSessionProvider {
    runtime_core: ActiveRuntimeCore,
}

impl ActiveSessionProvider {
    fn new(runtime_core: ActiveRuntimeCore) -> Self {
        Self { runtime_core }
    }

    fn provider(&self) -> anyhow::Result<Arc<dyn SessionProvider>> {
        self.runtime_core()
            .seams()
            .session_provider
            .clone()
            .ok_or_else(|| anyhow!("session provider not configured"))
    }

    fn runtime_core(&self) -> Arc<RuntimeCore> {
        self.runtime_core.current()
    }
}

#[async_trait]
impl SessionProvider for ActiveSessionProvider {
    async fn get(&self, key: &RuntimeSessionKey) -> anyhow::Result<Option<SessionRecord>> {
        self.provider()?.get(key).await
    }

    async fn put(&self, key: &RuntimeSessionKey, record: SessionRecord) -> anyhow::Result<()> {
        self.provider()?.put(key, record).await
    }

    async fn compare_and_set(
        &self,
        key: &RuntimeSessionKey,
        expected_revision: u64,
        record: SessionRecord,
    ) -> anyhow::Result<bool> {
        self.provider()?
            .compare_and_set(key, expected_revision, record)
            .await
    }

    async fn delete(&self, key: &RuntimeSessionKey) -> anyhow::Result<()> {
        self.provider()?.delete(key).await
    }

    async fn find_by_user(
        &self,
        tenant: &str,
        team: Option<&str>,
        user: &str,
    ) -> anyhow::Result<Option<(RuntimeSessionKey, SessionRecord)>> {
        self.provider()?.find_by_user(tenant, team, user).await
    }

    async fn find_wait_by_scope(
        &self,
        tenant: &str,
        team: Option<&str>,
        user: &str,
        scope: &ReplyScope,
    ) -> anyhow::Result<Option<(RuntimeSessionKey, SessionRecord)>> {
        self.provider()?
            .find_wait_by_scope(tenant, team, user, scope)
            .await
    }

    async fn health(&self) -> anyhow::Result<RuntimeHealth> {
        match self.runtime_core().seams().session_provider.clone() {
            Some(provider) => provider.health().await,
            None => Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Unavailable,
                reason: Some("session provider not configured".to_string()),
            }),
        }
    }
}

#[derive(Clone)]
struct ActiveStateProvider {
    runtime_core: ActiveRuntimeCore,
}

impl ActiveStateProvider {
    fn new(runtime_core: ActiveRuntimeCore) -> Self {
        Self { runtime_core }
    }

    fn provider(&self) -> anyhow::Result<Arc<dyn StateProvider>> {
        self.runtime_core()
            .seams()
            .state_provider
            .clone()
            .ok_or_else(|| anyhow!("state provider not configured"))
    }

    fn runtime_core(&self) -> Arc<RuntimeCore> {
        self.runtime_core.current()
    }
}

#[async_trait]
impl StateProvider for ActiveStateProvider {
    async fn get(&self, key: &ScopedStateKey) -> anyhow::Result<Option<JsonValue>> {
        self.provider()?.get(key).await
    }

    async fn put(&self, key: &ScopedStateKey, value: JsonValue) -> anyhow::Result<()> {
        self.provider()?.put(key, value).await
    }

    async fn compare_and_set(
        &self,
        key: &ScopedStateKey,
        expected: Option<JsonValue>,
        value: JsonValue,
    ) -> anyhow::Result<Option<bool>> {
        self.provider()?.compare_and_set(key, expected, value).await
    }

    async fn delete(&self, key: &ScopedStateKey) -> anyhow::Result<()> {
        self.provider()?.delete(key).await
    }

    async fn health(&self) -> anyhow::Result<RuntimeHealth> {
        match self.runtime_core().seams().state_provider.clone() {
            Some(provider) => provider.health().await,
            None => Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Unavailable,
                reason: Some("state provider not configured".to_string()),
            }),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ActiveRuntimeIdentity {
    current_bundle_id: Arc<RwLock<String>>,
}

impl ActiveRuntimeIdentity {
    pub(crate) fn new(bundle_id: String) -> Self {
        Self {
            current_bundle_id: Arc::new(RwLock::new(bundle_id)),
        }
    }

    pub(crate) fn bundle_id(&self) -> String {
        self.current_bundle_id
            .read()
            .expect("active runtime identity lock poisoned")
            .clone()
    }

    pub(crate) fn replace_bundle_id(&self, bundle_id: String) {
        *self
            .current_bundle_id
            .write()
            .expect("active runtime identity lock poisoned") = bundle_id;
    }
}

#[derive(Clone)]
struct ActiveRuntimeTransitionReport {
    current: Arc<RwLock<RuntimeTransitionReport>>,
}

#[derive(Clone, Debug, Serialize)]
struct RuntimeControlState {
    node_id: String,
    start_time_unix_ms: u64,
    draining: bool,
    is_leader: bool,
    log_level: Option<String>,
    shutdown_requested: bool,
    last_shutdown_request_unix_ms: Option<u64>,
    last_deployment_request: Option<RuntimeControlRequestReport>,
    last_config_publish: Option<RuntimeControlRequestReport>,
    last_cache_invalidate: Option<RuntimeControlRequestReport>,
}

#[derive(Clone, Debug, Serialize)]
struct RuntimeControlRequestReport {
    action: String,
    requested_at_unix_ms: u64,
    payload: JsonValue,
}

#[derive(Clone, Debug, Serialize)]
struct RuntimeDependencyReport {
    name: &'static str,
    required: bool,
    configured: bool,
    status: &'static str,
    reason: Option<String>,
}

#[derive(Clone, Debug)]
struct RuntimeDependencyState {
    overall_status: &'static str,
    reports: Vec<RuntimeDependencyReport>,
}

#[derive(Clone, Debug, Serialize)]
struct RuntimeProviderHealthReport {
    provider_class: &'static str,
    required: bool,
    configured: bool,
    status: &'static str,
    reason: Option<String>,
    last_checked_at_unix_ms: u64,
    consecutive_failures: u64,
    recovery_state: &'static str,
}

#[derive(Clone, Debug)]
struct RuntimeProviderHealthState {
    overall_status: &'static str,
    safe_mode: bool,
    degraded_level: u8,
    direct_execution_allowed: bool,
    reports: Vec<RuntimeProviderHealthReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeRequestPolicyRefusal {
    pub code: &'static str,
    pub request_class: String,
    pub message: String,
    pub safe_mode: bool,
    pub degraded_level: u8,
    pub blocking_provider_classes: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct RuntimeProviderHealthRegistryState {
    providers: BTreeMap<&'static str, RuntimeProviderHealthMemory>,
    safe_mode: bool,
    degraded_level: u8,
    overall_status: &'static str,
}

#[derive(Clone, Debug)]
struct RuntimeProviderHealthMemory {
    status: &'static str,
    consecutive_failures: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
struct RuntimeEventDeliveryReport {
    telemetry_failures: u64,
    telemetry_timeouts: u64,
    observer_failures: u64,
    observer_timeouts: u64,
    dropped_events: u64,
    last_dropped_event_type: Option<String>,
    last_telemetry_error: Option<String>,
    last_observer_error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct RuntimeTransitionReport {
    last_promotion: Option<RuntimeStatePromotionReport>,
    last_cleanup: Option<RuntimeStateCleanupReport>,
    last_session_index_reset: Option<RuntimeSessionIndexResetReport>,
}

#[derive(Clone, Debug, Serialize)]
struct RuntimeStatePromotionReport {
    from_bundle_id: String,
    to_bundle_id: String,
    copied_files: usize,
    rewritten_sessions: usize,
}

#[derive(Clone, Debug, Serialize)]
struct RuntimeStateCleanupReport {
    bundle_id: String,
    removed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RuntimeSessionIndexResetReport {
    session_locations: usize,
    user_sessions: usize,
    user_wait_entries: usize,
    scope_entries: usize,
}

#[derive(Clone)]
struct ActiveRuntimeEventDeliveryReport {
    current: Arc<RwLock<RuntimeEventDeliveryReport>>,
}

#[derive(Clone)]
struct ActiveRuntimeControlState {
    current: Arc<RwLock<RuntimeControlState>>,
}

#[derive(Clone, Copy)]
enum EventDeliveryTarget {
    Telemetry,
    Observer,
}

enum EventDeliveryOutcome {
    Delivered,
    TimedOut,
    Failed(String),
}

struct DependencyEventContext<'a> {
    provider_id: &'a str,
    op_id: &'a str,
    ctx: &'a OperatorContext,
    dependency_state: &'a RuntimeDependencyState,
    reason: Option<String>,
}

pub(crate) struct PhaseEventSpec<'a> {
    pub event_type: &'a str,
    pub severity: &'a str,
    pub outcome: Option<&'a str>,
    pub ctx: &'a OperatorContext,
    pub pack_id: Option<&'a str>,
    pub flow_id: Option<&'a str>,
    pub payload: JsonValue,
}

impl ActiveRuntimeControlState {
    fn new(node_id: String, start_time_unix_ms: u64) -> Self {
        Self {
            current: Arc::new(RwLock::new(RuntimeControlState {
                node_id,
                start_time_unix_ms,
                draining: false,
                is_leader: true,
                log_level: crate::operator_log::current_level()
                    .map(|level| format!("{level:?}").to_ascii_lowercase()),
                shutdown_requested: false,
                last_shutdown_request_unix_ms: None,
                last_deployment_request: None,
                last_config_publish: None,
                last_cache_invalidate: None,
            })),
        }
    }

    fn snapshot(&self) -> RuntimeControlState {
        self.current
            .read()
            .expect("active runtime control state lock poisoned")
            .clone()
    }

    fn set_draining(&self, draining: bool) -> RuntimeControlState {
        let mut current = self
            .current
            .write()
            .expect("active runtime control state lock poisoned");
        current.draining = draining;
        current.clone()
    }

    fn request_shutdown(&self, requested_at_unix_ms: u64) -> RuntimeControlState {
        let mut current = self
            .current
            .write()
            .expect("active runtime control state lock poisoned");
        current.draining = true;
        current.shutdown_requested = true;
        current.last_shutdown_request_unix_ms = Some(requested_at_unix_ms);
        current.clone()
    }

    fn record_deployment_request(
        &self,
        report: RuntimeControlRequestReport,
    ) -> RuntimeControlState {
        let mut current = self
            .current
            .write()
            .expect("active runtime control state lock poisoned");
        current.last_deployment_request = Some(report);
        current.clone()
    }

    fn record_config_publish(&self, report: RuntimeControlRequestReport) -> RuntimeControlState {
        let mut current = self
            .current
            .write()
            .expect("active runtime control state lock poisoned");
        current.last_config_publish = Some(report);
        current.clone()
    }

    fn record_cache_invalidate(&self, report: RuntimeControlRequestReport) -> RuntimeControlState {
        let mut current = self
            .current
            .write()
            .expect("active runtime control state lock poisoned");
        current.last_cache_invalidate = Some(report);
        current.clone()
    }

    fn set_log_level(&self, log_level: String) -> RuntimeControlState {
        let mut current = self
            .current
            .write()
            .expect("active runtime control state lock poisoned");
        current.log_level = Some(log_level);
        current.clone()
    }

    #[cfg(test)]
    fn set_leader(&self, is_leader: bool) {
        self.current
            .write()
            .expect("active runtime control state lock poisoned")
            .is_leader = is_leader;
    }
}

impl ActiveRuntimeTransitionReport {
    fn new() -> Self {
        Self {
            current: Arc::new(RwLock::new(RuntimeTransitionReport::default())),
        }
    }

    fn snapshot(&self) -> RuntimeTransitionReport {
        self.current
            .read()
            .expect("active runtime transition report lock poisoned")
            .clone()
    }

    fn record_promotion(&self, report: RuntimeStatePromotionReport) {
        self.current
            .write()
            .expect("active runtime transition report lock poisoned")
            .last_promotion = Some(report);
    }

    fn record_cleanup(&self, report: RuntimeStateCleanupReport) {
        self.current
            .write()
            .expect("active runtime transition report lock poisoned")
            .last_cleanup = Some(report);
    }

    fn record_session_index_reset(&self, report: RuntimeSessionIndexResetReport) {
        self.current
            .write()
            .expect("active runtime transition report lock poisoned")
            .last_session_index_reset = Some(report);
    }
}

impl ActiveRuntimeEventDeliveryReport {
    fn new() -> Self {
        Self {
            current: Arc::new(RwLock::new(RuntimeEventDeliveryReport::default())),
        }
    }

    fn snapshot(&self) -> RuntimeEventDeliveryReport {
        self.current
            .read()
            .expect("active runtime event delivery report lock poisoned")
            .clone()
    }

    fn record(&self, target: EventDeliveryTarget, outcome: EventDeliveryOutcome) {
        let mut current = self
            .current
            .write()
            .expect("active runtime event delivery report lock poisoned");
        match (target, outcome) {
            (EventDeliveryTarget::Telemetry, EventDeliveryOutcome::Delivered) => {}
            (EventDeliveryTarget::Telemetry, EventDeliveryOutcome::TimedOut) => {
                current.telemetry_timeouts += 1;
                current.last_telemetry_error =
                    Some("telemetry event delivery timed out".to_string());
            }
            (EventDeliveryTarget::Telemetry, EventDeliveryOutcome::Failed(err)) => {
                current.telemetry_failures += 1;
                current.last_telemetry_error = Some(err);
            }
            (EventDeliveryTarget::Observer, EventDeliveryOutcome::Delivered) => {}
            (EventDeliveryTarget::Observer, EventDeliveryOutcome::TimedOut) => {
                current.observer_timeouts += 1;
                current.last_observer_error = Some("observer event delivery timed out".to_string());
            }
            (EventDeliveryTarget::Observer, EventDeliveryOutcome::Failed(err)) => {
                current.observer_failures += 1;
                current.last_observer_error = Some(err);
            }
        }
    }

    fn record_drop(&self, event_type: &str) {
        let mut current = self
            .current
            .write()
            .expect("active runtime event delivery report lock poisoned");
        current.dropped_events += 1;
        current.last_dropped_event_type = Some(event_type.to_string());
    }
}

#[derive(Clone)]
struct ActiveProviderHealthRegistry {
    current: Arc<RwLock<RuntimeProviderHealthRegistryState>>,
}

impl ActiveProviderHealthRegistry {
    fn new() -> Self {
        Self {
            current: Arc::new(RwLock::new(RuntimeProviderHealthRegistryState {
                overall_status: "available",
                ..RuntimeProviderHealthRegistryState::default()
            })),
        }
    }

    fn update(
        &self,
        checked_at_unix_ms: u64,
        reports: Vec<RuntimeProviderHealthReport>,
    ) -> ProviderHealthUpdateResult {
        let mut state = self
            .current
            .write()
            .expect("active provider health registry lock poisoned");
        let previous_safe_mode = state.safe_mode;
        let previous_degraded_level = state.degraded_level;
        let previous_overall_status = state.overall_status;
        let mut provider_transitions = Vec::new();
        let mut current_reports = Vec::with_capacity(reports.len());
        for report in reports {
            let previous = state.providers.get(report.provider_class).cloned();
            let (consecutive_failures, recovery_state) =
                provider_health_progress(previous.as_ref(), report.status);
            let updated = RuntimeProviderHealthReport {
                last_checked_at_unix_ms: checked_at_unix_ms,
                consecutive_failures,
                recovery_state,
                ..report
            };
            if let Some(event) = provider_transition_event(previous.as_ref(), &updated) {
                provider_transitions.push(event);
            }
            state.providers.insert(
                updated.provider_class,
                RuntimeProviderHealthMemory {
                    status: updated.status,
                    consecutive_failures: updated.consecutive_failures,
                },
            );
            current_reports.push(updated);
        }

        let degraded_level = compute_degraded_level(&current_reports);
        let safe_mode = degraded_level >= 2;
        let overall_status = degraded_level_status_name(degraded_level);
        state.safe_mode = safe_mode;
        state.degraded_level = degraded_level;
        state.overall_status = overall_status;

        ProviderHealthUpdateResult {
            snapshot: RuntimeProviderHealthState {
                overall_status,
                safe_mode,
                degraded_level,
                direct_execution_allowed: degraded_level < 3,
                reports: current_reports,
            },
            mode_transition: (previous_overall_status != overall_status).then_some(
                ModeTransition {
                    previous_overall_status,
                    overall_status,
                    previous_safe_mode,
                    safe_mode,
                    previous_degraded_level,
                    degraded_level,
                },
            ),
            provider_transitions,
        }
    }
}

#[derive(Clone, Debug)]
struct ProviderHealthUpdateResult {
    snapshot: RuntimeProviderHealthState,
    mode_transition: Option<ModeTransition>,
    provider_transitions: Vec<ProviderTransitionEvent>,
}

#[derive(Clone, Debug)]
struct ModeTransition {
    previous_overall_status: &'static str,
    overall_status: &'static str,
    previous_safe_mode: bool,
    safe_mode: bool,
    previous_degraded_level: u8,
    degraded_level: u8,
}

#[derive(Clone, Debug)]
struct ProviderTransitionEvent {
    event_type: &'static str,
    severity: &'static str,
    outcome: &'static str,
    provider_class: &'static str,
    previous_status: Option<&'static str>,
    current_status: &'static str,
    reason: Option<String>,
}

fn degraded_dependency_messages(state: &RuntimeDependencyState) -> Vec<String> {
    state
        .reports
        .iter()
        .filter(|report| report.required && report.status == "degraded")
        .map(|report| {
            let reason = report
                .reason
                .clone()
                .unwrap_or_else(|| "degraded".to_string());
            format!("{}: {}", report.name, reason)
        })
        .collect()
}

fn remove_contract_cache_dirs(root: &Path) -> anyhow::Result<Vec<String>> {
    let mut removed = Vec::new();
    if !root.exists() {
        return Ok(removed);
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if entry.file_name() == "_contracts" {
            std::fs::remove_dir_all(&path)?;
            removed.push(path.display().to_string());
            continue;
        }
        removed.extend(remove_contract_cache_dirs(&path)?);
    }
    Ok(removed)
}

#[derive(Clone)]
struct ActiveProviderInventory {
    current: Arc<RwLock<ProviderInventory>>,
}

#[derive(Clone, Default)]
struct ActiveBundleLifecycle {
    current: Arc<RwLock<BundleLifecycleRegistry>>,
}

impl ActiveBundleLifecycle {
    fn new(lifecycle: BundleLifecycleRegistry) -> Self {
        Self {
            current: Arc::new(RwLock::new(lifecycle)),
        }
    }

    fn snapshot(&self) -> BundleLifecycleSnapshot {
        self.current
            .read()
            .expect("active bundle lifecycle lock poisoned")
            .snapshot()
    }

    fn warm_bundle(
        &self,
        bundle_ref: &Path,
        requirements: &[crate::runtime_core::RuntimeProviderRequirement],
    ) -> anyhow::Result<String> {
        self.current
            .write()
            .expect("active bundle lifecycle lock poisoned")
            .warm_bundle(bundle_ref, requirements)
    }

    fn warm_bundle_id(
        &self,
        bundle_id: &str,
        requirements: &[crate::runtime_core::RuntimeProviderRequirement],
    ) -> anyhow::Result<String> {
        self.current
            .write()
            .expect("active bundle lifecycle lock poisoned")
            .warm_bundle_id(bundle_id, requirements)
    }

    fn stage_bundle(&self, bundle_ref: &Path) -> anyhow::Result<String> {
        self.current
            .write()
            .expect("active bundle lifecycle lock poisoned")
            .stage_bundle(bundle_ref)
    }

    fn warm_and_activate(
        &self,
        bundle_ref: &Path,
        requirements: &[crate::runtime_core::RuntimeProviderRequirement],
    ) -> anyhow::Result<(
        String,
        crate::runtime_core::RuntimeCapabilityRegistry,
        crate::runtime_core::RuntimeWiringPlan,
    )> {
        self.current
            .write()
            .expect("active bundle lifecycle lock poisoned")
            .warm_and_activate(bundle_ref, requirements)
    }

    fn activate(&self, bundle_id: &str) -> anyhow::Result<()> {
        self.current
            .write()
            .expect("active bundle lifecycle lock poisoned")
            .activate(bundle_id)
    }

    fn rollback(&self) -> anyhow::Result<()> {
        self.current
            .write()
            .expect("active bundle lifecycle lock poisoned")
            .rollback()
    }

    fn complete_drain(&self, bundle_id: &str) -> anyhow::Result<()> {
        self.current
            .write()
            .expect("active bundle lifecycle lock poisoned")
            .complete_drain(bundle_id)
    }

    fn runtime_artifacts(
        &self,
        bundle_id: &str,
    ) -> Option<(
        crate::runtime_core::RuntimeCapabilityRegistry,
        crate::runtime_core::RuntimeWiringPlan,
    )> {
        self.current
            .read()
            .expect("active bundle lifecycle lock poisoned")
            .runtime_artifacts(bundle_id)
    }

    fn active_access_handle(&self) -> Option<BundleAccessHandle> {
        self.current
            .read()
            .expect("active bundle lifecycle lock poisoned")
            .active_access_handle()
    }

    fn inventory(&self, bundle_id: &str) -> Option<BundleInventorySnapshot> {
        self.current
            .read()
            .expect("active bundle lifecycle lock poisoned")
            .inventory(bundle_id)
    }
}

#[derive(Clone, Default)]
struct ProviderInventory {
    catalog: BTreeMap<(Domain, String), ProviderPack>,
    packs_by_path: BTreeMap<PathBuf, ProviderPack>,
}

impl ActiveProviderInventory {
    fn new(inventory: ProviderInventory) -> Self {
        Self {
            current: Arc::new(RwLock::new(inventory)),
        }
    }

    fn snapshot(&self) -> ProviderInventory {
        self.current
            .read()
            .expect("active provider inventory lock poisoned")
            .clone()
    }

    fn replace(&self, inventory: ProviderInventory) {
        *self
            .current
            .write()
            .expect("active provider inventory lock poisoned") = inventory;
    }
}

#[derive(Clone)]
pub struct DemoRunnerHost {
    bundle_root: PathBuf,
    active_bundle_access: ActiveBundleAccess,
    active_runtime_identity: ActiveRuntimeIdentity,
    provider_health_registry: ActiveProviderHealthRegistry,
    transition_report: ActiveRuntimeTransitionReport,
    event_delivery_report: ActiveRuntimeEventDeliveryReport,
    event_delivery_gate: Arc<Mutex<()>>,
    control_state: ActiveRuntimeControlState,
    runner_mode: RunnerMode,
    provider_inventory: ActiveProviderInventory,
    secrets_handle: SecretsManagerHandle,
    card_renderer: CardRenderer,
    session_store: DynSessionStore,
    session_store_adapter: Option<Arc<SessionProviderStoreAdapter>>,
    state_store: DynStateStore,
    runtime_core: ActiveRuntimeCore,
    bundle_lifecycle: ActiveBundleLifecycle,
    debug_enabled: bool,
}

#[derive(Clone)]
pub(crate) struct StateProviderStoreAdapter {
    provider: Arc<dyn StateProvider>,
}

impl StateProviderStoreAdapter {
    const CAS_RETRY_LIMIT: usize = 3;

    pub(crate) fn new(provider: Arc<dyn StateProvider>) -> Self {
        Self { provider }
    }

    fn scoped_key(
        &self,
        tenant: &greentic_types::TenantCtx,
        prefix: &str,
        key: &greentic_types::StateKey,
    ) -> ScopedStateKey {
        let team = tenant
            .team_id
            .as_ref()
            .or(tenant.team.as_ref())
            .map(ToString::to_string);
        ScopedStateKey {
            tenant: tenant.tenant_id.to_string(),
            team,
            scope: prefix.to_string(),
            key: sanitize_state_key(key.as_str()),
        }
    }

    fn put_with_optional_compare_and_set(
        &self,
        scoped: &ScopedStateKey,
        path: Option<&greentic_state::key::StatePath>,
        value: &JsonValue,
    ) -> greentic_types::GResult<()> {
        for attempt in 0..Self::CAS_RETRY_LIMIT {
            let current =
                make_runtime_or_thread_scope(|runtime| runtime.block_on(self.provider.get(scoped)))
                    .map_err(map_runtime_state_store_error)?;
            let payload = match path {
                None => value.clone(),
                Some(path) if path == &greentic_state::key::StatePath::root() => value.clone(),
                Some(path) => {
                    let mut merged = current.clone().unwrap_or_else(|| json!({}));
                    upsert_json_pointer(&mut merged, &path.to_pointer(), value.clone())
                        .map_err(map_runtime_state_store_error)?;
                    merged
                }
            };
            let compare_result = make_runtime_or_thread_scope(|runtime| {
                runtime.block_on(self.provider.compare_and_set(
                    scoped,
                    current.clone(),
                    payload.clone(),
                ))
            })
            .map_err(map_runtime_state_store_error)?;
            match compare_result {
                Some(true) => return Ok(()),
                Some(false) => {
                    if attempt + 1 == Self::CAS_RETRY_LIMIT {
                        return Err(GreenticError::new(
                            GreenticErrorCode::Internal,
                            format!(
                                "state compare-and-set retry limit exceeded for {}/{}/{}",
                                scoped.tenant, scoped.scope, scoped.key
                            ),
                        ));
                    }
                }
                None => {
                    return make_runtime_or_thread_scope(|runtime| {
                        runtime.block_on(self.provider.put(scoped, payload))
                    })
                    .map_err(map_runtime_state_store_error);
                }
            }
        }
        Err(GreenticError::new(
            GreenticErrorCode::Internal,
            "state compare-and-set retry loop terminated unexpectedly",
        ))
    }
}

impl RunnerStateStore for StateProviderStoreAdapter {
    fn get_json(
        &self,
        tenant: &greentic_types::TenantCtx,
        prefix: &str,
        key: &greentic_types::StateKey,
        path: Option<&greentic_state::key::StatePath>,
    ) -> greentic_types::GResult<Option<JsonValue>> {
        let scoped = self.scoped_key(tenant, prefix, key);
        let value =
            make_runtime_or_thread_scope(|runtime| runtime.block_on(self.provider.get(&scoped)))
                .map_err(map_runtime_state_store_error)?;
        Ok(match (value, path) {
            (Some(value), Some(path)) if path != &greentic_state::key::StatePath::root() => {
                value.pointer(&path.to_pointer()).cloned()
            }
            (value, _) => value,
        })
    }

    fn set_json(
        &self,
        tenant: &greentic_types::TenantCtx,
        prefix: &str,
        key: &greentic_types::StateKey,
        path: Option<&greentic_state::key::StatePath>,
        value: &JsonValue,
        _ttl_secs: Option<u32>,
    ) -> greentic_types::GResult<()> {
        let scoped = self.scoped_key(tenant, prefix, key);
        self.put_with_optional_compare_and_set(&scoped, path, value)
    }

    fn del(
        &self,
        tenant: &greentic_types::TenantCtx,
        prefix: &str,
        key: &greentic_types::StateKey,
    ) -> greentic_types::GResult<bool> {
        let scoped = self.scoped_key(tenant, prefix, key);
        let existed = self.get_json(tenant, prefix, key, None)?.is_some();
        make_runtime_or_thread_scope(|runtime| runtime.block_on(self.provider.delete(&scoped)))
            .map_err(map_runtime_state_store_error)?;
        Ok(existed)
    }

    fn del_prefix(
        &self,
        _tenant: &greentic_types::TenantCtx,
        _prefix: &str,
    ) -> greentic_types::GResult<u64> {
        Ok(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionUserLookupKey {
    tenant: String,
    team: Option<String>,
    user: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionScopeLookupKey {
    tenant: String,
    team: Option<String>,
    user: String,
    conversation: String,
    thread: Option<String>,
    reply_to: Option<String>,
    correlation: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct PersistedRuntimeSession {
    data: StoreSessionData,
    user: Option<String>,
    wait_scope: Option<ReplyScope>,
}

pub(crate) struct SessionProviderStoreAdapter {
    provider: Arc<dyn SessionProvider>,
    session_locations: RwLock<HashMap<StoreSessionKey, RuntimeSessionKey>>,
    user_sessions: RwLock<HashMap<SessionUserLookupKey, StoreSessionKey>>,
    user_waits: RwLock<HashMap<SessionUserLookupKey, HashSet<StoreSessionKey>>>,
    scope_index: RwLock<HashMap<SessionScopeLookupKey, StoreSessionKey>>,
}

impl SessionProviderStoreAdapter {
    const CAS_RETRY_LIMIT: usize = 3;

    pub(crate) fn new(provider: Arc<dyn SessionProvider>) -> Self {
        Self {
            provider,
            session_locations: RwLock::new(HashMap::new()),
            user_sessions: RwLock::new(HashMap::new()),
            user_waits: RwLock::new(HashMap::new()),
            scope_index: RwLock::new(HashMap::new()),
        }
    }

    fn encode_store_session_key(runtime_key: &RuntimeSessionKey) -> StoreSessionKey {
        let team = runtime_key.team.as_deref().unwrap_or("_");
        StoreSessionKey::new(format!(
            "rt::{tenant}::{team}::{session}",
            tenant = runtime_key.tenant,
            team = team,
            session = runtime_key.session_id
        ))
    }

    fn decode_store_session_key(key: &StoreSessionKey) -> Option<RuntimeSessionKey> {
        let encoded = key.to_string();
        let mut parts = encoded.splitn(5, "::");
        let prefix = parts.next()?;
        let tenant = parts.next()?;
        let team = parts.next()?;
        let session_id = parts.next()?;
        if prefix != "rt" || parts.next().is_some() {
            return None;
        }
        Some(RuntimeSessionKey {
            tenant: tenant.to_string(),
            team: if team == "_" {
                None
            } else {
                Some(team.to_string())
            },
            session_id: session_id.to_string(),
        })
    }

    fn session_user_lookup(ctx: &StoreTenantCtx, user: &UserId) -> SessionUserLookupKey {
        SessionUserLookupKey {
            tenant: ctx.tenant_id.to_string(),
            team: ctx
                .team_id
                .as_ref()
                .or(ctx.team.as_ref())
                .map(ToString::to_string),
            user: user.to_string(),
        }
    }

    fn session_scope_lookup(
        ctx: &StoreTenantCtx,
        user: &UserId,
        scope: &ReplyScope,
    ) -> SessionScopeLookupKey {
        SessionScopeLookupKey {
            tenant: ctx.tenant_id.to_string(),
            team: ctx
                .team_id
                .as_ref()
                .or(ctx.team.as_ref())
                .map(ToString::to_string),
            user: user.to_string(),
            conversation: scope.conversation.clone(),
            thread: scope.thread.clone(),
            reply_to: scope.reply_to.clone(),
            correlation: scope.correlation.clone(),
        }
    }

    fn encode_record(
        &self,
        existing: Option<SessionRecord>,
        data: &StoreSessionData,
        user: Option<&UserId>,
        wait_scope: Option<&ReplyScope>,
        ttl: Option<std::time::Duration>,
    ) -> anyhow::Result<SessionRecord> {
        let next_revision = existing
            .as_ref()
            .map(|record| record.revision.saturating_add(1))
            .unwrap_or(1);
        let bundle_assignment = existing.and_then(|record| record.bundle_assignment);
        let persisted = PersistedRuntimeSession {
            data: data.clone(),
            user: user.map(ToString::to_string),
            wait_scope: wait_scope.cloned(),
        };
        Ok(SessionRecord {
            revision: next_revision,
            route: Some(data.flow_id.to_string()),
            bundle_assignment,
            context: serde_json::to_value(persisted)?,
            expires_at_unix_ms: ttl.and_then(duration_deadline_unix_ms),
        })
    }

    fn decode_persisted_record(
        record: SessionRecord,
    ) -> greentic_types::GResult<Option<PersistedRuntimeSession>> {
        match serde_json::from_value::<PersistedRuntimeSession>(record.context.clone()) {
            Ok(value) => Ok(Some(value)),
            Err(wrapper_err) => serde_json::from_value::<StoreSessionData>(record.context)
                .map(|data| {
                    Some(PersistedRuntimeSession {
                        data,
                        user: None,
                        wait_scope: None,
                    })
                })
                .map_err(|legacy_err| {
                    GreenticError::new(
                        GreenticErrorCode::Internal,
                        format!(
                            "decode session provider record: wrapper={wrapper_err}; legacy={legacy_err}"
                        ),
                    )
                }),
        }
    }

    fn decode_data(record: SessionRecord) -> greentic_types::GResult<Option<StoreSessionData>> {
        Ok(Self::decode_persisted_record(record)?.map(|persisted| persisted.data))
    }

    fn get_runtime_key(&self, key: &StoreSessionKey) -> greentic_types::GResult<RuntimeSessionKey> {
        if let Some(runtime_key) = self
            .session_locations
            .read()
            .expect("session locations lock poisoned")
            .get(key)
            .cloned()
        {
            return Ok(runtime_key);
        }
        Self::decode_store_session_key(key).ok_or_else(|| {
            GreenticError::new(
                GreenticErrorCode::NotFound,
                format!("session {} not found", key),
            )
        })
    }

    fn register_runtime_key(&self, store_key: &StoreSessionKey, runtime_key: &RuntimeSessionKey) {
        self.session_locations
            .write()
            .expect("session locations lock poisoned")
            .insert(store_key.clone(), runtime_key.clone());
    }

    fn recover_session_from_provider(
        &self,
        key: &StoreSessionKey,
    ) -> greentic_types::GResult<Option<(RuntimeSessionKey, PersistedRuntimeSession)>> {
        let runtime_key = match Self::decode_store_session_key(key) {
            Some(runtime_key) => runtime_key,
            None => return Ok(None),
        };
        let record = make_runtime_or_thread_scope(|runtime| {
            runtime.block_on(self.provider.get(&runtime_key))
        })
        .map_err(map_runtime_session_store_error)?;
        let Some(record) = record else {
            return Ok(None);
        };
        let persisted = Self::decode_persisted_record(record)?;
        let Some(persisted) = persisted else {
            return Ok(None);
        };
        self.register_runtime_key(key, &runtime_key);
        Ok(Some((runtime_key, persisted)))
    }

    fn register_user_session(&self, ctx: &StoreTenantCtx, key: &StoreSessionKey) {
        if let Some(user) = ctx.user_id.as_ref().or(ctx.user.as_ref()) {
            self.user_sessions
                .write()
                .expect("user sessions lock poisoned")
                .insert(Self::session_user_lookup(ctx, user), key.clone());
        }
    }

    fn update_record_with_retry(
        &self,
        runtime_key: &RuntimeSessionKey,
        data: &StoreSessionData,
        user: Option<&UserId>,
        wait_scope: Option<&ReplyScope>,
        ttl: Option<std::time::Duration>,
    ) -> greentic_session::SessionResult<()> {
        for attempt in 0..Self::CAS_RETRY_LIMIT {
            let existing = make_runtime_or_thread_scope(|runtime| {
                runtime.block_on(self.provider.get(runtime_key))
            })
            .map_err(map_runtime_session_store_error)?;
            let Some(existing) = existing else {
                return Err(GreenticError::new(
                    GreenticErrorCode::NotFound,
                    format!("session {} not found", runtime_key.session_id),
                ));
            };
            let expected_revision = existing.revision;
            let record = self
                .encode_record(existing.into(), data, user, wait_scope, ttl)
                .map_err(map_runtime_session_store_error)?;
            let updated = make_runtime_or_thread_scope(|runtime| {
                runtime.block_on(self.provider.compare_and_set(
                    runtime_key,
                    expected_revision,
                    record,
                ))
            })
            .map_err(map_runtime_session_store_error)?;
            if updated {
                return Ok(());
            }
            if attempt + 1 == Self::CAS_RETRY_LIMIT {
                return Err(GreenticError::new(
                    GreenticErrorCode::Internal,
                    format!(
                        "session compare-and-set retry limit exceeded for {}",
                        runtime_key.session_id
                    ),
                ));
            }
        }
        Err(GreenticError::new(
            GreenticErrorCode::Internal,
            "session compare-and-set retry loop terminated unexpectedly",
        ))
    }

    pub(crate) fn reset_runtime_indexes(&self) -> RuntimeSessionIndexResetReport {
        let session_locations = self
            .session_locations
            .read()
            .expect("session locations lock poisoned")
            .len();
        let user_sessions = self
            .user_sessions
            .read()
            .expect("user sessions lock poisoned")
            .len();
        let user_wait_entries = self
            .user_waits
            .read()
            .expect("user waits lock poisoned")
            .values()
            .map(HashSet::len)
            .sum();
        let scope_entries = self
            .scope_index
            .read()
            .expect("scope index lock poisoned")
            .len();
        self.session_locations
            .write()
            .expect("session locations lock poisoned")
            .clear();
        self.user_sessions
            .write()
            .expect("user sessions lock poisoned")
            .clear();
        self.user_waits
            .write()
            .expect("user waits lock poisoned")
            .clear();
        self.scope_index
            .write()
            .expect("scope index lock poisoned")
            .clear();
        RuntimeSessionIndexResetReport {
            session_locations,
            user_sessions,
            user_wait_entries,
            scope_entries,
        }
    }
}

impl RunnerSessionStore for SessionProviderStoreAdapter {
    fn create_session(
        &self,
        ctx: &StoreTenantCtx,
        data: StoreSessionData,
    ) -> greentic_session::SessionResult<StoreSessionKey> {
        let runtime_key = RuntimeSessionKey {
            tenant: ctx.tenant_id.to_string(),
            team: ctx
                .team_id
                .as_ref()
                .or(ctx.team.as_ref())
                .map(ToString::to_string),
            session_id: uuid::Uuid::new_v4().to_string(),
        };
        let key = Self::encode_store_session_key(&runtime_key);
        let record = self
            .encode_record(
                None,
                &data,
                ctx.user_id.as_ref().or(ctx.user.as_ref()),
                None,
                None,
            )
            .map_err(map_runtime_session_store_error)?;
        make_runtime_or_thread_scope(|runtime| {
            runtime.block_on(self.provider.put(&runtime_key, record))
        })
        .map_err(map_runtime_session_store_error)?;
        self.register_runtime_key(&key, &runtime_key);
        self.register_user_session(ctx, &key);
        Ok(key)
    }

    fn get_session(
        &self,
        key: &StoreSessionKey,
    ) -> greentic_session::SessionResult<Option<StoreSessionData>> {
        if let Some((_, persisted)) = self.recover_session_from_provider(key)? {
            return Ok(Some(persisted.data));
        }
        let runtime_key = match self.get_runtime_key(key) {
            Ok(runtime_key) => runtime_key,
            Err(err) if err.code == GreenticErrorCode::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let record = make_runtime_or_thread_scope(|runtime| {
            runtime.block_on(self.provider.get(&runtime_key))
        })
        .map_err(map_runtime_session_store_error)?;
        match record {
            Some(record) => Self::decode_data(record),
            None => Ok(None),
        }
    }

    fn update_session(
        &self,
        key: &StoreSessionKey,
        data: StoreSessionData,
    ) -> greentic_session::SessionResult<()> {
        let runtime_key = self.get_runtime_key(key)?;
        self.update_record_with_retry(
            &runtime_key,
            &data,
            data.tenant_ctx
                .user_id
                .as_ref()
                .or(data.tenant_ctx.user.as_ref()),
            None,
            None,
        )?;
        self.register_user_session(&data.tenant_ctx, key);
        Ok(())
    }

    fn remove_session(&self, key: &StoreSessionKey) -> greentic_session::SessionResult<()> {
        let runtime_key = self.get_runtime_key(key)?;
        let _ = make_runtime_or_thread_scope(|runtime| {
            runtime.block_on(self.provider.delete(&runtime_key))
        })
        .map_err(map_runtime_session_store_error);
        self.session_locations
            .write()
            .expect("session locations lock poisoned")
            .remove(key);
        self.user_sessions
            .write()
            .expect("user sessions lock poisoned")
            .retain(|_, current| current != key);
        self.user_waits
            .write()
            .expect("user waits lock poisoned")
            .retain(|_, values| {
                values.remove(key);
                !values.is_empty()
            });
        self.scope_index
            .write()
            .expect("scope index lock poisoned")
            .retain(|_, current| current != key);
        Ok(())
    }

    fn register_wait(
        &self,
        ctx: &StoreTenantCtx,
        user_id: &UserId,
        scope: &ReplyScope,
        session_key: &StoreSessionKey,
        data: StoreSessionData,
        ttl: Option<std::time::Duration>,
    ) -> greentic_session::SessionResult<()> {
        let runtime_key = self.get_runtime_key(session_key)?;
        self.update_record_with_retry(&runtime_key, &data, Some(user_id), Some(scope), ttl)?;
        let user_lookup = Self::session_user_lookup(ctx, user_id);
        self.user_sessions
            .write()
            .expect("user sessions lock poisoned")
            .insert(user_lookup.clone(), session_key.clone());
        self.user_waits
            .write()
            .expect("user waits lock poisoned")
            .entry(user_lookup)
            .or_default()
            .insert(session_key.clone());
        self.scope_index
            .write()
            .expect("scope index lock poisoned")
            .insert(
                Self::session_scope_lookup(ctx, user_id, scope),
                session_key.clone(),
            );
        Ok(())
    }

    fn find_wait_by_scope(
        &self,
        ctx: &StoreTenantCtx,
        user_id: &UserId,
        scope: &ReplyScope,
    ) -> greentic_session::SessionResult<Option<StoreSessionKey>> {
        if let Some(found) = self
            .scope_index
            .read()
            .expect("scope index lock poisoned")
            .get(&Self::session_scope_lookup(ctx, user_id, scope))
            .cloned()
        {
            return Ok(Some(found));
        }
        let found = make_runtime_or_thread_scope(|runtime| {
            runtime.block_on(
                self.provider.find_wait_by_scope(
                    ctx.tenant_id.as_ref(),
                    ctx.team_id
                        .as_ref()
                        .or(ctx.team.as_ref())
                        .map(|team| team.as_str()),
                    user_id.as_ref(),
                    scope,
                ),
            )
        })
        .map_err(map_runtime_session_store_error)?;
        let Some((runtime_key, _)) = found else {
            return Ok(None);
        };
        let store_key = Self::encode_store_session_key(&runtime_key);
        self.register_runtime_key(&store_key, &runtime_key);
        Ok(Some(store_key))
    }

    fn list_waits_for_user(
        &self,
        ctx: &StoreTenantCtx,
        user_id: &UserId,
    ) -> greentic_session::SessionResult<Vec<StoreSessionKey>> {
        Ok(self
            .user_waits
            .read()
            .expect("user waits lock poisoned")
            .get(&Self::session_user_lookup(ctx, user_id))
            .map(|values| values.iter().cloned().collect())
            .unwrap_or_default())
    }

    fn clear_wait(
        &self,
        ctx: &StoreTenantCtx,
        user_id: &UserId,
        scope: &ReplyScope,
    ) -> greentic_session::SessionResult<()> {
        let scope_lookup = Self::session_scope_lookup(ctx, user_id, scope);
        let removed = self
            .scope_index
            .write()
            .expect("scope index lock poisoned")
            .remove(&scope_lookup);
        if let Some(session_key) = removed {
            let user_lookup = Self::session_user_lookup(ctx, user_id);
            self.user_waits
                .write()
                .expect("user waits lock poisoned")
                .entry(user_lookup)
                .and_modify(|values| {
                    values.remove(&session_key);
                });
        }
        Ok(())
    }

    fn find_by_user(
        &self,
        ctx: &StoreTenantCtx,
        user: &UserId,
    ) -> greentic_session::SessionResult<Option<(StoreSessionKey, StoreSessionData)>> {
        let Some(session_key) = self
            .user_sessions
            .read()
            .expect("user sessions lock poisoned")
            .get(&Self::session_user_lookup(ctx, user))
            .cloned()
        else {
            let found = make_runtime_or_thread_scope(|runtime| {
                runtime.block_on(
                    self.provider.find_by_user(
                        ctx.tenant_id.as_ref(),
                        ctx.team_id
                            .as_ref()
                            .or(ctx.team.as_ref())
                            .map(|team| team.as_str()),
                        user.as_ref(),
                    ),
                )
            })
            .map_err(map_runtime_session_store_error)?;
            let Some((runtime_key, record)) = found else {
                return Ok(None);
            };
            let session_key = Self::encode_store_session_key(&runtime_key);
            self.register_runtime_key(&session_key, &runtime_key);
            return Ok(Self::decode_data(record)?.map(|data| (session_key, data)));
        };
        Ok(self
            .get_session(&session_key)?
            .map(|data| (session_key, data)))
    }
}

#[derive(Clone)]
pub(crate) struct LocalRuntimeStateProvider {
    bundle_root: PathBuf,
    runtime_identity: ActiveRuntimeIdentity,
}

impl LocalRuntimeStateProvider {
    pub(crate) fn new(bundle_root: PathBuf, runtime_identity: ActiveRuntimeIdentity) -> Self {
        Self {
            bundle_root,
            runtime_identity,
        }
    }

    fn state_path(&self, key: &ScopedStateKey) -> PathBuf {
        let team = key.team.as_deref().unwrap_or("default");
        state_layout::runtime_bundle_state_root(
            &self.bundle_root,
            &self.runtime_identity.bundle_id(),
        )
        .join(&key.tenant)
        .join(team)
        .join("provider-state")
        .join(&key.scope)
        .join(format!("{}.json", key.key))
    }

    fn legacy_state_path(&self, key: &ScopedStateKey) -> PathBuf {
        let team = key.team.as_deref().unwrap_or("default");
        self.bundle_root
            .join("state")
            .join("runtime")
            .join(&key.tenant)
            .join(team)
            .join("provider-state")
            .join(&key.scope)
            .join(format!("{}.json", key.key))
    }
}

#[async_trait]
impl StateProvider for LocalRuntimeStateProvider {
    async fn get(&self, key: &ScopedStateKey) -> anyhow::Result<Option<JsonValue>> {
        let current = runtime_state::read_json(&self.state_path(key))?;
        if current.is_some() {
            Ok(current)
        } else {
            runtime_state::read_json(&self.legacy_state_path(key))
        }
    }

    async fn put(&self, key: &ScopedStateKey, value: JsonValue) -> anyhow::Result<()> {
        runtime_state::write_json(&self.state_path(key), &value)
    }

    async fn compare_and_set(
        &self,
        key: &ScopedStateKey,
        expected: Option<JsonValue>,
        value: JsonValue,
    ) -> anyhow::Result<Option<bool>> {
        let current = self.get(key).await?;
        if current != expected {
            return Ok(Some(false));
        }
        self.put(key, value).await?;
        Ok(Some(true))
    }

    async fn delete(&self, key: &ScopedStateKey) -> anyhow::Result<()> {
        for path in [self.state_path(key), self.legacy_state_path(key)] {
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    async fn health(&self) -> anyhow::Result<RuntimeHealth> {
        Ok(RuntimeHealth {
            status: RuntimeHealthStatus::Available,
            reason: None,
        })
    }
}

#[derive(Clone)]
pub(crate) struct LocalRuntimeSessionProvider {
    bundle_root: PathBuf,
    runtime_identity: ActiveRuntimeIdentity,
}

impl LocalRuntimeSessionProvider {
    pub(crate) fn new(bundle_root: PathBuf, runtime_identity: ActiveRuntimeIdentity) -> Self {
        Self {
            bundle_root,
            runtime_identity,
        }
    }

    fn session_path(&self, key: &RuntimeSessionKey) -> PathBuf {
        let team = key.team.as_deref().unwrap_or("default");
        state_layout::runtime_bundle_state_root(
            &self.bundle_root,
            &self.runtime_identity.bundle_id(),
        )
        .join(&key.tenant)
        .join(team)
        .join("sessions")
        .join(format!("{}.json", key.session_id))
    }

    fn legacy_session_path(&self, key: &RuntimeSessionKey) -> PathBuf {
        let team = key.team.as_deref().unwrap_or("default");
        self.bundle_root
            .join("state")
            .join("runtime")
            .join(&key.tenant)
            .join(team)
            .join("sessions")
            .join(format!("{}.json", key.session_id))
    }

    fn session_roots_for_lookup(&self, tenant: &str, team: Option<&str>) -> [PathBuf; 2] {
        let team = team.unwrap_or("default");
        [
            state_layout::runtime_bundle_state_root(
                &self.bundle_root,
                &self.runtime_identity.bundle_id(),
            )
            .join(tenant)
            .join(team)
            .join("sessions"),
            self.bundle_root
                .join("state")
                .join("runtime")
                .join(tenant)
                .join(team)
                .join("sessions"),
        ]
    }

    fn find_record_by_predicate<F>(
        &self,
        tenant: &str,
        team: Option<&str>,
        mut predicate: F,
    ) -> anyhow::Result<Option<(RuntimeSessionKey, SessionRecord)>>
    where
        F: FnMut(&PersistedRuntimeSession) -> bool,
    {
        for root in self.session_roots_for_lookup(tenant, team) {
            if !root.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&root)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    continue;
                }
                let Some(record) = runtime_state::read_json::<SessionRecord>(&path)? else {
                    continue;
                };
                let Some(persisted) =
                    SessionProviderStoreAdapter::decode_persisted_record(record.clone())?
                else {
                    continue;
                };
                if !predicate(&persisted) {
                    continue;
                }
                let Some(session_id) = path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .map(ToString::to_string)
                else {
                    continue;
                };
                return Ok(Some((
                    RuntimeSessionKey {
                        tenant: tenant.to_string(),
                        team: team.map(ToString::to_string),
                        session_id,
                    },
                    record,
                )));
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl SessionProvider for LocalRuntimeSessionProvider {
    async fn get(&self, key: &RuntimeSessionKey) -> anyhow::Result<Option<SessionRecord>> {
        let current = runtime_state::read_json(&self.session_path(key))?;
        if current.is_some() {
            Ok(current)
        } else {
            runtime_state::read_json(&self.legacy_session_path(key))
        }
    }

    async fn put(&self, key: &RuntimeSessionKey, record: SessionRecord) -> anyhow::Result<()> {
        runtime_state::write_json(&self.session_path(key), &record)
    }

    async fn compare_and_set(
        &self,
        key: &RuntimeSessionKey,
        expected_revision: u64,
        record: SessionRecord,
    ) -> anyhow::Result<bool> {
        let current = self.get(key).await?;
        let Some(current) = current else {
            return Ok(false);
        };
        if current.revision != expected_revision {
            return Ok(false);
        }
        self.put(key, record).await?;
        Ok(true)
    }

    async fn delete(&self, key: &RuntimeSessionKey) -> anyhow::Result<()> {
        for path in [self.session_path(key), self.legacy_session_path(key)] {
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    async fn find_by_user(
        &self,
        tenant: &str,
        team: Option<&str>,
        user: &str,
    ) -> anyhow::Result<Option<(RuntimeSessionKey, SessionRecord)>> {
        self.find_record_by_predicate(tenant, team, |persisted| {
            persisted.user.as_deref() == Some(user)
        })
    }

    async fn find_wait_by_scope(
        &self,
        tenant: &str,
        team: Option<&str>,
        user: &str,
        scope: &ReplyScope,
    ) -> anyhow::Result<Option<(RuntimeSessionKey, SessionRecord)>> {
        self.find_record_by_predicate(tenant, team, |persisted| {
            persisted.user.as_deref() == Some(user) && persisted.wait_scope.as_ref() == Some(scope)
        })
    }

    async fn health(&self) -> anyhow::Result<RuntimeHealth> {
        Ok(RuntimeHealth {
            status: RuntimeHealthStatus::Available,
            reason: None,
        })
    }
}

#[derive(Clone, Default)]
struct LocalAdminAuthorizationHook;

#[async_trait]
impl AdminAuthorizationHook for LocalAdminAuthorizationHook {
    async fn authorize(&self, _action: &AdminAction) -> anyhow::Result<AuthorizationDecision> {
        Ok(AuthorizationDecision::Allow)
    }

    async fn health(&self) -> anyhow::Result<RuntimeHealth> {
        Ok(RuntimeHealth {
            status: RuntimeHealthStatus::Available,
            reason: None,
        })
    }
}

#[derive(Clone)]
struct ActiveBundleAccess {
    current: Arc<RwLock<BundleAccessHandle>>,
}

impl ActiveBundleAccess {
    fn new(access: BundleAccessHandle) -> Self {
        Self {
            current: Arc::new(RwLock::new(access)),
        }
    }

    fn current(&self) -> BundleAccessHandle {
        self.current
            .read()
            .expect("active bundle access lock poisoned")
            .clone()
    }

    fn replace(&self, access: BundleAccessHandle) {
        *self
            .current
            .write()
            .expect("active bundle access lock poisoned") = access;
    }
}

#[derive(Clone)]
struct LocalBundleSource {
    access: ActiveBundleAccess,
}

impl LocalBundleSource {
    fn new(access: ActiveBundleAccess) -> Self {
        Self { access }
    }
}

#[async_trait]
impl BundleSource for LocalBundleSource {
    async fn stage(&self, bundle_ref: &str) -> anyhow::Result<PathBuf> {
        let access = self.access.current();
        if bundle_ref.is_empty() || bundle_ref == "." {
            return Ok(access.active_root().to_path_buf());
        }
        let candidate = PathBuf::from(bundle_ref);
        if candidate.is_absolute() {
            Ok(candidate)
        } else {
            Ok(access.active_root().join(candidate))
        }
    }

    async fn health(&self) -> anyhow::Result<RuntimeHealth> {
        Ok(RuntimeHealth {
            status: RuntimeHealthStatus::Available,
            reason: None,
        })
    }
}

#[derive(Clone)]
struct LocalBundleResolver {
    access: ActiveBundleAccess,
}

impl LocalBundleResolver {
    fn new(access: ActiveBundleAccess) -> Self {
        Self { access }
    }
}

#[async_trait]
impl BundleResolver for LocalBundleResolver {
    async fn resolve(&self, bundle_ref: &str) -> anyhow::Result<String> {
        let access = self.access.current();
        if bundle_ref.is_empty() || bundle_ref == "." {
            return Ok(access.diagnostics().bundle_ref.display().to_string());
        }
        let candidate = PathBuf::from(bundle_ref);
        if candidate.is_absolute() {
            Ok(candidate.display().to_string())
        } else {
            Ok(access.active_root().join(candidate).display().to_string())
        }
    }

    async fn health(&self) -> anyhow::Result<RuntimeHealth> {
        Ok(RuntimeHealth {
            status: RuntimeHealthStatus::Available,
            reason: None,
        })
    }
}

#[derive(Clone)]
struct LocalBundleFs {
    access: ActiveBundleAccess,
}

impl LocalBundleFs {
    fn new(access: ActiveBundleAccess) -> Self {
        Self { access }
    }

    fn resolve_path(&self, path: &str) -> PathBuf {
        let access = self.access.current();
        let candidate = PathBuf::from(path);
        if candidate.is_absolute() {
            candidate
        } else {
            access.active_root().join(candidate)
        }
    }
}

#[async_trait]
impl BundleFs for LocalBundleFs {
    async fn read(&self, path: &str) -> anyhow::Result<Vec<u8>> {
        Ok(std::fs::read(self.resolve_path(path))?)
    }

    async fn exists(&self, path: &str) -> anyhow::Result<bool> {
        Ok(self.resolve_path(path).exists())
    }

    async fn list_dir(&self, path: &str) -> anyhow::Result<Vec<String>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(self.resolve_path(path))? {
            let entry = entry?;
            entries.push(entry.path().display().to_string());
        }
        entries.sort();
        Ok(entries)
    }

    async fn health(&self) -> anyhow::Result<RuntimeHealth> {
        Ok(RuntimeHealth {
            status: RuntimeHealthStatus::Available,
            reason: None,
        })
    }
}

impl DemoRunnerHost {
    const EVENT_DELIVERY_TIMEOUT: Duration = Duration::from_millis(250);

    fn now_unix_ms() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};

        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }

    pub fn bundle_root(&self) -> &Path {
        &self.bundle_root
    }

    pub fn healthz_snapshot(&self) -> JsonValue {
        let control = self.control_state.snapshot();
        json!({
            "ok": true,
            "node_id": control.node_id,
            "start_time_unix_ms": control.start_time_unix_ms,
        })
    }

    pub fn readyz_snapshot(&self) -> (bool, JsonValue) {
        let control = self.control_state.snapshot();
        let provider_health = self.provider_health_snapshot();
        let ready = !control.draining && provider_health.degraded_level < 3;
        (
            ready,
            json!({
                "ready": ready,
                "draining": control.draining,
                "mode": provider_health.overall_status,
                "safe_mode": provider_health.safe_mode,
                "degraded_level": provider_health.degraded_level,
                "node_id": control.node_id,
            }),
        )
    }

    pub fn control_plane_snapshot(&self) -> JsonValue {
        let control = self.control_state.snapshot();
        let runtime = self.runtime_status_snapshot();
        let ready = !control.draining
            && runtime
                .pointer("/mode/degraded_level")
                .and_then(JsonValue::as_u64)
                .unwrap_or(3)
                < 3;
        json!({
            "node_id": control.node_id,
            "start_time_unix_ms": control.start_time_unix_ms,
            "ready": ready,
            "draining": control.draining,
            "is_leader": control.is_leader,
            "log_level": control.log_level,
            "shutdown_requested": control.shutdown_requested,
            "last_shutdown_request_unix_ms": control.last_shutdown_request_unix_ms,
            "last_deployment_request": control.last_deployment_request,
            "last_config_publish": control.last_config_publish,
            "last_cache_invalidate": control.last_cache_invalidate,
            "active_bundle_id": runtime.pointer("/bundle/lifecycle/active_bundle_id").cloned().unwrap_or(JsonValue::Null),
            "active_bundle_access_mode": runtime.pointer("/bundle/access/mode").cloned().unwrap_or(JsonValue::Null),
            "staged_bundles": runtime.pointer("/bundle/lifecycle/bundles").cloned().unwrap_or(JsonValue::Null),
            "discovered_providers": runtime.pointer("/roles/selected").cloned().unwrap_or(JsonValue::Null),
            "provider_health_summary": runtime.pointer("/provider_health").cloned().unwrap_or(JsonValue::Null),
            "current_degraded_safe_mode": runtime.pointer("/mode/status").cloned().unwrap_or(JsonValue::Null),
            "runtime": runtime,
        })
    }

    pub fn set_runtime_draining(&self, draining: bool) -> JsonValue {
        let control = self.control_state.set_draining(draining);
        self.publish_transition_event(
            if draining {
                "runtime.drain"
            } else {
                "runtime.resume"
            },
            "info",
            Some(if draining { "draining" } else { "ready" }),
            vec![if draining {
                "runtime.drain".to_string()
            } else {
                "runtime.resume".to_string()
            }],
            json!({
                "draining": control.draining,
                "node_id": control.node_id,
            }),
        );
        json!({
            "ok": true,
            "draining": control.draining,
            "node_id": control.node_id,
        })
    }

    pub fn request_runtime_shutdown(&self) -> JsonValue {
        let requested_at_unix_ms = Self::now_unix_ms();
        let control = self.control_state.request_shutdown(requested_at_unix_ms);
        self.publish_transition_event(
            "runtime.shutdown_requested",
            "warn",
            Some("shutdown_requested"),
            vec![
                "runtime.shutdown_requested".to_string(),
                "runtime.drain".to_string(),
            ],
            json!({
                "draining": control.draining,
                "shutdown_requested": control.shutdown_requested,
                "requested_at_unix_ms": requested_at_unix_ms,
                "node_id": control.node_id,
            }),
        );
        json!({
            "ok": true,
            "draining": control.draining,
            "shutdown_requested": control.shutdown_requested,
            "requested_at_unix_ms": requested_at_unix_ms,
            "node_id": control.node_id,
        })
    }

    pub fn record_deployment_request(&self, action: &str, payload: JsonValue) -> JsonValue {
        let requested_at_unix_ms = Self::now_unix_ms();
        let report = RuntimeControlRequestReport {
            action: action.to_string(),
            requested_at_unix_ms,
            payload: payload.clone(),
        };
        let control = self.control_state.record_deployment_request(report);
        self.publish_transition_event(
            &format!("deployments.{action}_requested"),
            "info",
            Some("requested"),
            vec![
                format!("deployments.{action}_requested"),
                "control.requested".to_string(),
            ],
            json!({
                "action": action,
                "requested_at_unix_ms": requested_at_unix_ms,
                "payload": payload,
                "node_id": control.node_id,
            }),
        );
        json!({
            "ok": true,
            "requested": true,
            "applied": false,
            "action": action,
            "requested_at_unix_ms": requested_at_unix_ms,
            "message": "deployment request recorded; synchronous lifecycle mutation is not exposed through the shared HTTP host yet",
        })
    }

    pub fn record_config_publish(&self, payload: JsonValue) -> JsonValue {
        let requested_at_unix_ms = Self::now_unix_ms();
        let report = RuntimeControlRequestReport {
            action: "config.publish".to_string(),
            requested_at_unix_ms,
            payload: payload.clone(),
        };
        let control = self.control_state.record_config_publish(report);
        self.publish_transition_event(
            "config.publish_requested",
            "info",
            Some("requested"),
            vec![
                "config.publish_requested".to_string(),
                "control.requested".to_string(),
            ],
            json!({
                "requested_at_unix_ms": requested_at_unix_ms,
                "payload": payload,
                "node_id": control.node_id,
            }),
        );
        json!({
            "ok": true,
            "requested": true,
            "applied": false,
            "requested_at_unix_ms": requested_at_unix_ms,
        })
    }

    pub fn apply_config_publish(&self, payload: JsonValue) -> anyhow::Result<JsonValue> {
        let _ = self.record_config_publish(payload.clone());
        let requested_at_unix_ms = Self::now_unix_ms();
        let control_root = self
            .bundle_root
            .join("state")
            .join("runtime")
            .join("control_plane")
            .join("config_publish");
        let latest_path = control_root.join("latest.json");
        let history_path = control_root.join(format!("{requested_at_unix_ms}.json"));
        let envelope = json!({
            "published_at_unix_ms": requested_at_unix_ms,
            "node_id": self.control_state.snapshot().node_id,
            "payload": payload,
        });
        runtime_state::write_json(&latest_path, &envelope)?;
        runtime_state::write_json(&history_path, &envelope)?;
        self.publish_transition_event(
            "config.publish_applied",
            "info",
            Some("applied"),
            vec![
                "config.publish_applied".to_string(),
                "control.applied".to_string(),
            ],
            json!({
                "published_at_unix_ms": requested_at_unix_ms,
                "latest_path": latest_path.display().to_string(),
                "history_path": history_path.display().to_string(),
            }),
        );
        Ok(json!({
            "ok": true,
            "requested": true,
            "applied": true,
            "published_at_unix_ms": requested_at_unix_ms,
            "latest_path": latest_path.display().to_string(),
            "history_path": history_path.display().to_string(),
        }))
    }

    pub fn record_cache_invalidate(&self, payload: JsonValue) -> JsonValue {
        let requested_at_unix_ms = Self::now_unix_ms();
        let report = RuntimeControlRequestReport {
            action: "cache.invalidate".to_string(),
            requested_at_unix_ms,
            payload: payload.clone(),
        };
        let control = self.control_state.record_cache_invalidate(report);
        self.publish_transition_event(
            "cache.invalidate_requested",
            "info",
            Some("requested"),
            vec![
                "cache.invalidate_requested".to_string(),
                "control.requested".to_string(),
            ],
            json!({
                "requested_at_unix_ms": requested_at_unix_ms,
                "payload": payload,
                "node_id": control.node_id,
            }),
        );
        json!({
            "ok": true,
            "requested": true,
            "applied": false,
            "requested_at_unix_ms": requested_at_unix_ms,
        })
    }

    pub fn apply_cache_invalidate(&self, payload: JsonValue) -> anyhow::Result<JsonValue> {
        let _ = self.record_cache_invalidate(payload.clone());
        let scope = payload
            .get("scope")
            .and_then(JsonValue::as_str)
            .unwrap_or("all");

        let mut removed_paths = Vec::new();
        let reset_report = self
            .session_store_adapter
            .as_ref()
            .map(|adapter| adapter.reset_runtime_indexes());
        if let Some(report) = reset_report.clone() {
            self.transition_report.record_session_index_reset(report);
        }

        if matches!(scope, "all" | "provider_registry") {
            let path = self
                .bundle_root
                .join(".greentic")
                .join("cache")
                .join("provider-registry");
            if path.exists() {
                std::fs::remove_dir_all(&path)?;
                removed_paths.push(path.display().to_string());
            }
        }

        if matches!(scope, "all" | "contracts") {
            let providers_root = self.bundle_root.join("providers");
            removed_paths.extend(remove_contract_cache_dirs(&providers_root)?);
        }

        self.sync_provider_inventory_from_lifecycle();
        self.publish_transition_event(
            "cache.invalidate_applied",
            "info",
            Some("applied"),
            vec![
                "cache.invalidate_applied".to_string(),
                "control.applied".to_string(),
            ],
            json!({
                "scope": scope,
                "removed_paths": removed_paths,
                "session_index_reset": reset_report,
            }),
        );
        Ok(json!({
            "ok": true,
            "requested": true,
            "applied": true,
            "scope": scope,
            "removed_paths": removed_paths,
            "session_index_reset": reset_report,
        }))
    }

    pub fn apply_log_level(&self, level: &str) -> anyhow::Result<JsonValue> {
        let normalized = level.trim().to_ascii_lowercase();
        let target_level = match normalized.as_str() {
            "trace" => crate::operator_log::Level::Trace,
            "debug" => crate::operator_log::Level::Debug,
            "info" => crate::operator_log::Level::Info,
            "warn" | "warning" => crate::operator_log::Level::Warn,
            "error" => crate::operator_log::Level::Error,
            _ => anyhow::bail!("unsupported log level: {level}"),
        };
        let applied = crate::operator_log::set_level(target_level);
        let control = self.control_state.set_log_level(normalized.clone());
        self.publish_transition_event(
            "observability.log_level",
            "info",
            Some(if applied { "applied" } else { "recorded" }),
            vec![
                "observability.log_level".to_string(),
                format!("log_level.{normalized}"),
            ],
            json!({
                "log_level": normalized,
                "applied": applied,
                "node_id": control.node_id,
            }),
        );
        Ok(json!({
            "ok": true,
            "applied": applied,
            "log_level": normalized,
        }))
    }

    pub fn is_leader(&self) -> bool {
        self.control_state.snapshot().is_leader
    }

    #[cfg(test)]
    pub(crate) fn replace_runtime_core_for_test(&self, core: RuntimeCore) {
        self.runtime_core.replace(core);
    }

    #[cfg(test)]
    pub(crate) fn set_is_leader_for_test(&self, is_leader: bool) {
        self.control_state.set_leader(is_leader);
    }

    pub fn bundle_read_root(&self) -> PathBuf {
        self.active_bundle_access
            .current()
            .active_root()
            .to_path_buf()
    }

    pub fn secrets_manager(&self) -> DynSecretsManager {
        self.secrets_handle.manager()
    }

    pub fn secrets_handle(&self) -> &SecretsManagerHandle {
        &self.secrets_handle
    }

    fn operation_event_metadata(
        &self,
        event_type: &str,
        envelope: &OperationEnvelope,
    ) -> (String, Option<String>, Vec<String>) {
        let severity = match envelope.status {
            OperationStatus::Err => "error",
            OperationStatus::Denied => "warn",
            OperationStatus::Pending | OperationStatus::Ok => "info",
        }
        .to_string();
        let outcome = Some(
            match envelope.status {
                OperationStatus::Pending => "pending",
                OperationStatus::Denied => "denied",
                OperationStatus::Ok => "ok",
                OperationStatus::Err => "error",
            }
            .to_string(),
        );
        let mut reason_codes = vec![event_type.to_string()];
        if matches!(
            envelope.status,
            OperationStatus::Denied | OperationStatus::Err
        ) {
            reason_codes.push(format!("status.{:?}", envelope.status).to_lowercase());
        }
        (severity, outcome, reason_codes)
    }

    fn publish_transition_event(
        &self,
        event_type: &str,
        severity: &str,
        outcome: Option<&str>,
        reason_codes: Vec<String>,
        payload: JsonValue,
    ) {
        let event = RuntimeEvent {
            event_type: event_type.to_string(),
            ts_unix_ms: Self::now_unix_ms(),
            tenant: None,
            team: None,
            session_id: None,
            bundle_id: Some(self.bundle_runtime_id()),
            pack_id: None,
            flow_id: None,
            node_id: None,
            correlation_id: None,
            trace_id: None,
            severity: severity.to_string(),
            outcome: outcome.map(ToString::to_string),
            reason_codes,
            payload,
        };
        self.deliver_runtime_event(event_type, event);
    }

    pub(crate) fn publish_phase_event(&self, spec: PhaseEventSpec<'_>) {
        let mut reason_codes = vec![spec.event_type.to_string()];
        if let Some(outcome) = spec.outcome {
            reason_codes.push(format!("outcome.{outcome}"));
        }
        let event = RuntimeEvent {
            event_type: spec.event_type.to_string(),
            ts_unix_ms: Self::now_unix_ms(),
            tenant: Some(spec.ctx.tenant.clone()),
            team: spec.ctx.team.clone(),
            session_id: None,
            bundle_id: Some(self.bundle_runtime_id()),
            pack_id: spec.pack_id.map(ToString::to_string),
            flow_id: spec.flow_id.map(ToString::to_string),
            node_id: spec.flow_id.map(ToString::to_string),
            correlation_id: spec.ctx.correlation_id.clone(),
            trace_id: None,
            severity: spec.severity.to_string(),
            outcome: spec.outcome.map(ToString::to_string),
            reason_codes,
            payload: spec.payload,
        };
        self.deliver_runtime_event(spec.event_type, event);
    }

    fn provider_health_snapshot(&self) -> RuntimeProviderHealthState {
        let update = self
            .provider_health_registry
            .update(Self::now_unix_ms(), self.collect_provider_health_reports());
        for provider_event in &update.provider_transitions {
            self.publish_transition_event(
                provider_event.event_type,
                provider_event.severity,
                Some(provider_event.outcome),
                vec![
                    provider_event.event_type.to_string(),
                    format!("provider_class.{}", provider_event.provider_class),
                    format!("status.{}", provider_event.current_status),
                ],
                json!({
                    "provider_class": provider_event.provider_class,
                    "previous_status": provider_event.previous_status,
                    "current_status": provider_event.current_status,
                    "reason": provider_event.reason,
                }),
            );
        }
        if let Some(mode_transition) = &update.mode_transition {
            self.publish_transition_event(
                "runtime.mode_transition",
                if mode_transition.overall_status == "unavailable" {
                    "warn"
                } else {
                    "info"
                },
                Some(mode_transition.overall_status),
                vec![
                    "runtime.mode_transition".to_string(),
                    format!("mode.{}", mode_transition.overall_status),
                ],
                json!({
                    "previous_mode": mode_transition.previous_overall_status,
                    "current_mode": mode_transition.overall_status,
                    "safe_mode": update.snapshot.safe_mode,
                    "degraded_level": update.snapshot.degraded_level,
                    "provider_health": self.provider_health_snapshot_value(&update.snapshot),
                }),
            );
            if mode_transition.previous_safe_mode != mode_transition.safe_mode {
                self.publish_transition_event(
                    if mode_transition.safe_mode {
                        "runtime.safe_mode_enter"
                    } else {
                        "runtime.safe_mode_leave"
                    },
                    if mode_transition.safe_mode {
                        "warn"
                    } else {
                        "info"
                    },
                    Some(if mode_transition.safe_mode {
                        "safe_mode"
                    } else {
                        "normal"
                    }),
                    vec![
                        if mode_transition.safe_mode {
                            "runtime.safe_mode_enter".to_string()
                        } else {
                            "runtime.safe_mode_leave".to_string()
                        },
                        format!("safe_mode.{}", mode_transition.safe_mode),
                    ],
                    json!({
                        "previous_safe_mode": mode_transition.previous_safe_mode,
                        "safe_mode": mode_transition.safe_mode,
                        "degraded_level": mode_transition.degraded_level,
                    }),
                );
            }
            if mode_transition.previous_degraded_level != mode_transition.degraded_level {
                self.publish_transition_event(
                    "runtime.degraded_level_changed",
                    if mode_transition.degraded_level >= 3 {
                        "warn"
                    } else {
                        "info"
                    },
                    Some(mode_transition.overall_status),
                    vec![
                        "runtime.degraded_level_changed".to_string(),
                        format!("degraded_level.{}", mode_transition.degraded_level),
                    ],
                    json!({
                        "previous_degraded_level": mode_transition.previous_degraded_level,
                        "degraded_level": mode_transition.degraded_level,
                        "safe_mode": mode_transition.safe_mode,
                    }),
                );
            }
        }
        update.snapshot
    }

    fn publish_dependency_event(
        &self,
        event_type: &str,
        severity: &str,
        outcome: &str,
        details: DependencyEventContext<'_>,
    ) {
        let mut reason_codes = vec![
            event_type.to_string(),
            format!(
                "dependency_state.{}",
                details.dependency_state.overall_status
            ),
        ];
        if outcome == "unavailable" {
            reason_codes.push("dependency.unavailable".to_string());
        } else if outcome == "degraded" {
            reason_codes.push("dependency.degraded".to_string());
        }
        let event = RuntimeEvent {
            event_type: event_type.to_string(),
            ts_unix_ms: Self::now_unix_ms(),
            tenant: Some(details.ctx.tenant.clone()),
            team: details.ctx.team.clone(),
            session_id: None,
            bundle_id: Some(self.bundle_runtime_id()),
            pack_id: Some(details.provider_id.to_string()),
            flow_id: Some(details.op_id.to_string()),
            node_id: Some(details.op_id.to_string()),
            correlation_id: details.ctx.correlation_id.clone(),
            trace_id: None,
            severity: severity.to_string(),
            outcome: Some(outcome.to_string()),
            reason_codes,
            payload: json!({
                "provider_id": details.provider_id,
                "op_id": details.op_id,
                "dependencies": {
                    "overall_status": details.dependency_state.overall_status,
                    "required": details.dependency_state.reports,
                },
                "reason": details.reason,
            }),
        };
        self.deliver_runtime_event(event_type, event);
    }

    pub fn new(
        bundle_root: PathBuf,
        _discovery: &discovery::DiscoveryResult,
        runner_binary: Option<PathBuf>,
        secrets_handle: SecretsManagerHandle,
        debug_enabled: bool,
    ) -> anyhow::Result<Self> {
        let bundle_access = BundleAccessHandle::open(
            &bundle_root,
            &BundleAccessConfig::new(bundle_root.join("state").join("runtime").join("bundle_fs")),
        )?;
        let runner_binary = runner_binary.and_then(validate_runner_binary);
        let mode = if let Some(ref binary) = runner_binary {
            let flavor = runner_integration::detect_runner_flavor(binary);
            RunnerMode::Integration {
                binary: binary.clone(),
                flavor,
            }
        } else {
            RunnerMode::Exec
        };
        let bundle_lifecycle = ActiveBundleLifecycle::new(BundleLifecycleRegistry::default());
        let (active_bundle_id, runtime_registry, runtime_wiring_plan) =
            bundle_lifecycle.warm_and_activate(&bundle_root, &default_provider_requirements())?;
        let active_bundle_access = ActiveBundleAccess::new(
            bundle_lifecycle
                .active_access_handle()
                .unwrap_or_else(|| bundle_access.clone()),
        );
        let active_runtime_identity = ActiveRuntimeIdentity::new(active_bundle_id.clone());
        let transition_report = ActiveRuntimeTransitionReport::new();
        let event_delivery_report = ActiveRuntimeEventDeliveryReport::new();
        let event_delivery_gate = Arc::new(Mutex::new(()));
        let control_state =
            ActiveRuntimeControlState::new(uuid::Uuid::new_v4().to_string(), Self::now_unix_ms());
        let provider_health_registry = ActiveProviderHealthRegistry::new();
        let runtime_seams = RuntimeSeams {
            admin_authorization_hook: Some(Arc::new(LocalAdminAuthorizationHook)),
            bundle_source: Some(Arc::new(LocalBundleSource::new(
                active_bundle_access.clone(),
            ))),
            bundle_resolver: Some(Arc::new(LocalBundleResolver::new(
                active_bundle_access.clone(),
            ))),
            bundle_fs: Some(Arc::new(LocalBundleFs::new(active_bundle_access.clone()))),
            session_provider: Some(Arc::new(LocalRuntimeSessionProvider::new(
                bundle_root.clone(),
                active_runtime_identity.clone(),
            ))),
            state_provider: Some(Arc::new(LocalRuntimeStateProvider::new(
                bundle_root.clone(),
                active_runtime_identity.clone(),
            ))),
            ..RuntimeSeams::default()
        };
        let runtime_core = ActiveRuntimeCore::new(RuntimeCore::new(
            runtime_registry,
            runtime_seams.clone(),
            runtime_wiring_plan,
        ));
        let (catalog, packs_by_path) = bundle_lifecycle
            .inventory(&active_bundle_id)
            .unwrap_or_default();
        let provider_inventory = ActiveProviderInventory::new(ProviderInventory {
            catalog,
            packs_by_path,
        });
        let session_store_adapter = Arc::new(SessionProviderStoreAdapter::new(Arc::new(
            ActiveSessionProvider::new(runtime_core.clone()),
        )));
        Ok(Self {
            session_store: session_store_adapter.clone() as DynSessionStore,
            session_store_adapter: Some(session_store_adapter),
            state_store: Arc::new(StateProviderStoreAdapter::new(Arc::new(
                ActiveStateProvider::new(runtime_core.clone()),
            ))) as DynStateStore,
            bundle_root,
            active_bundle_access,
            active_runtime_identity,
            provider_health_registry,
            transition_report,
            event_delivery_report,
            event_delivery_gate,
            control_state,
            runner_mode: mode,
            provider_inventory,
            secrets_handle,
            card_renderer: CardRenderer::new(),
            runtime_core,
            bundle_lifecycle,
            debug_enabled,
        })
    }

    pub fn debug_enabled(&self) -> bool {
        self.debug_enabled
    }

    pub fn runtime_core(&self) -> Arc<RuntimeCore> {
        self.runtime_core.current()
    }

    fn active_bundle_access(&self) -> BundleAccessHandle {
        self.active_bundle_access.current()
    }

    fn sync_active_bundle_access_from_lifecycle(&self) {
        if let Some(access) = self.bundle_lifecycle.active_access_handle() {
            self.active_bundle_access.replace(access);
        }
    }

    fn sync_runtime_identity_from_lifecycle(&self) {
        if let Some(bundle_id) = self.bundle_lifecycle.snapshot().active_bundle_id {
            self.active_runtime_identity.replace_bundle_id(bundle_id);
        }
    }

    fn sync_runtime_core_from_lifecycle(&self) {
        let Some(active_bundle_id) = self.bundle_lifecycle.snapshot().active_bundle_id else {
            return;
        };
        let Some((registry, wiring_plan)) =
            self.bundle_lifecycle.runtime_artifacts(&active_bundle_id)
        else {
            return;
        };
        let seams = self.runtime_core().seams().clone();
        self.runtime_core
            .replace(RuntimeCore::new(registry, seams, wiring_plan));
    }

    fn sync_provider_inventory_from_lifecycle(&self) {
        let Some(active_bundle_id) = self.bundle_lifecycle.snapshot().active_bundle_id else {
            return;
        };
        let Some((catalog, packs_by_path)) = self.bundle_lifecycle.inventory(&active_bundle_id)
        else {
            return;
        };
        self.provider_inventory.replace(ProviderInventory {
            catalog,
            packs_by_path,
        });
    }

    fn reset_session_store_indexes(&self) {
        if let Some(adapter) = &self.session_store_adapter {
            let report = adapter.reset_runtime_indexes();
            self.transition_report.record_session_index_reset(report);
        }
    }

    fn promote_runtime_state_from_lifecycle(&self) -> anyhow::Result<()> {
        let snapshot = self.bundle_lifecycle.snapshot();
        let Some(active_bundle_id) = snapshot.active_bundle_id else {
            return Ok(());
        };
        let Some(previous_bundle_id) = snapshot.previous_bundle_id else {
            return Ok(());
        };
        if active_bundle_id == previous_bundle_id {
            return Ok(());
        }
        let source_root =
            state_layout::runtime_bundle_state_root(&self.bundle_root, &previous_bundle_id);
        if !source_root.exists() {
            return Ok(());
        }
        let dest_root =
            state_layout::runtime_bundle_state_root(&self.bundle_root, &active_bundle_id);
        let report = promote_runtime_state_tree(
            &source_root,
            &dest_root,
            &active_bundle_id,
            &previous_bundle_id,
        )?;
        self.transition_report.record_promotion(report);
        Ok(())
    }

    fn provider_inventory(&self) -> ProviderInventory {
        self.provider_inventory.snapshot()
    }

    fn provider_pack(&self, pack_path: &Path) -> Option<ProviderPack> {
        self.provider_inventory()
            .packs_by_path
            .get(pack_path)
            .cloned()
    }

    fn runtime_run_dir(
        &self,
        domain: Domain,
        pack_label: &str,
        flow_id: &str,
    ) -> anyhow::Result<PathBuf> {
        state_layout::runtime_run_dir(
            &self.bundle_root,
            &self.bundle_runtime_id(),
            domain,
            pack_label,
            flow_id,
        )
    }

    pub fn warm_bundle_ref(&self, bundle_ref: &Path) -> anyhow::Result<String> {
        let bundle_id = self
            .bundle_lifecycle
            .warm_bundle(bundle_ref, &default_provider_requirements())?;
        self.publish_transition_event(
            "bundle.lifecycle.warm_request",
            "info",
            Some("ready"),
            vec![
                "bundle.lifecycle.warm_request".to_string(),
                "bundle.warmed".to_string(),
            ],
            json!({
                "bundle_id": bundle_id,
                "bundle_ref": bundle_ref.display().to_string(),
                "lifecycle": self.bundle_lifecycle.snapshot(),
            }),
        );
        Ok(bundle_id)
    }

    pub fn warm_bundle_id(&self, bundle_id: &str) -> anyhow::Result<String> {
        let warmed_bundle_id = self
            .bundle_lifecycle
            .warm_bundle_id(bundle_id, &default_provider_requirements())?;
        self.publish_transition_event(
            "bundle.lifecycle.warm_request",
            "info",
            Some("ready"),
            vec![
                "bundle.lifecycle.warm_request".to_string(),
                "bundle.warmed".to_string(),
            ],
            json!({
                "bundle_id": warmed_bundle_id,
                "lifecycle": self.bundle_lifecycle.snapshot(),
            }),
        );
        Ok(warmed_bundle_id)
    }

    pub fn activate_bundle_id(&self, bundle_id: &str) -> anyhow::Result<String> {
        self.bundle_lifecycle.activate(bundle_id)?;
        self.promote_runtime_state_from_lifecycle()?;
        self.sync_active_bundle_access_from_lifecycle();
        self.sync_runtime_identity_from_lifecycle();
        self.sync_runtime_core_from_lifecycle();
        self.sync_provider_inventory_from_lifecycle();
        self.reset_session_store_indexes();
        self.publish_transition_event(
            "bundle.lifecycle.activate",
            "info",
            Some("active"),
            vec![
                "bundle.lifecycle.activate".to_string(),
                "bundle.active".to_string(),
            ],
            json!({
                "bundle_id": bundle_id,
                "lifecycle": self.bundle_lifecycle.snapshot(),
            }),
        );
        Ok(bundle_id.to_string())
    }

    pub fn stage_bundle_ref(&self, bundle_ref: &Path) -> anyhow::Result<String> {
        let bundle_id = self.bundle_lifecycle.stage_bundle(bundle_ref)?;
        self.publish_transition_event(
            "bundle.lifecycle.stage_request",
            "info",
            Some("staged"),
            vec![
                "bundle.lifecycle.stage_request".to_string(),
                "bundle.staged".to_string(),
            ],
            json!({
                "bundle_id": bundle_id,
                "bundle_ref": bundle_ref.display().to_string(),
                "lifecycle": self.bundle_lifecycle.snapshot(),
            }),
        );
        Ok(bundle_id)
    }

    pub fn activate_bundle_ref(&self, bundle_ref: &Path) -> anyhow::Result<String> {
        let bundle_id = self
            .bundle_lifecycle
            .warm_bundle(bundle_ref, &default_provider_requirements())?;
        self.activate_bundle_id(&bundle_id)
    }

    pub fn rollback_active_bundle(&self) -> anyhow::Result<()> {
        self.bundle_lifecycle.rollback()?;
        self.promote_runtime_state_from_lifecycle()?;
        self.sync_active_bundle_access_from_lifecycle();
        self.sync_runtime_identity_from_lifecycle();
        self.sync_runtime_core_from_lifecycle();
        self.sync_provider_inventory_from_lifecycle();
        self.reset_session_store_indexes();
        self.publish_transition_event(
            "bundle.lifecycle.rollback",
            "warn",
            Some("rollback"),
            vec![
                "bundle.lifecycle.rollback".to_string(),
                "bundle.rollback".to_string(),
            ],
            json!({
                "lifecycle": self.bundle_lifecycle.snapshot(),
            }),
        );
        Ok(())
    }

    pub fn complete_bundle_drain(&self, bundle_id: &str) -> anyhow::Result<()> {
        self.bundle_lifecycle.complete_drain(bundle_id)?;
        let runtime_state_root =
            state_layout::runtime_bundle_state_root(&self.bundle_root, bundle_id);
        let removed = runtime_state_root.exists();
        if runtime_state_root.exists() {
            std::fs::remove_dir_all(&runtime_state_root)?;
        }
        self.transition_report
            .record_cleanup(RuntimeStateCleanupReport {
                bundle_id: bundle_id.to_string(),
                removed,
            });
        self.publish_transition_event(
            "bundle.lifecycle.complete_drain",
            "info",
            Some("retired"),
            vec![
                "bundle.lifecycle.complete_drain".to_string(),
                "bundle.retired".to_string(),
            ],
            json!({
                "bundle_id": bundle_id,
                "removed_runtime_state": removed,
                "lifecycle": self.bundle_lifecycle.snapshot(),
            }),
        );
        Ok(())
    }

    /// Return the canonical `provider_type` stored inside a provider pack manifest
    /// (e.g. `"messaging.webex.bot"`).  Falls back to the lookup key when the pack
    /// is not found or the manifest cannot be read.
    pub fn canonical_provider_type(&self, domain: Domain, lookup_key: &str) -> String {
        if let Some(pack) = self
            .provider_inventory()
            .catalog
            .get(&(domain, lookup_key.to_string()))
            .cloned()
        {
            self.primary_provider_type_for_pack(&pack.path)
                .unwrap_or_else(|_| lookup_key.to_string())
        } else {
            lookup_key.to_string()
        }
    }

    pub fn primary_provider_type_for_pack(&self, pack_path: &Path) -> anyhow::Result<String> {
        let bytes = self.read_bundle_bytes(pack_path)?;
        primary_provider_type_from_pack_bytes(pack_path, &bytes)
    }

    pub fn resolve_capability(
        &self,
        cap_id: &str,
        min_version: Option<&str>,
        scope: ResolveScope,
    ) -> Option<CapabilityBinding> {
        self.runtime_core()
            .registry()
            .resolve_capability(cap_id, min_version, &scope, None)
    }

    pub fn resolve_hook_chain(&self, stage: HookStage, op_name: &str) -> Vec<CapabilityBinding> {
        let cap_id = match stage {
            HookStage::Pre => CAP_OP_HOOK_PRE,
            HookStage::Post => CAP_OP_HOOK_POST,
        };
        self.runtime_core().registry().resolve_capability_chain(
            cap_id,
            None,
            &ResolveScope {
                env: env::var("GREENTIC_ENV").ok(),
                tenant: None,
                team: None,
            },
            Some(op_name),
        )
    }

    pub fn has_provider_packs_for_domain(&self, domain: Domain) -> bool {
        self.provider_inventory()
            .catalog
            .keys()
            .any(|(entry_domain, _)| *entry_domain == domain)
    }

    pub fn capability_setup_plan(&self, ctx: &OperatorContext) -> Vec<CapabilityBinding> {
        let scope = ResolveScope {
            env: env::var("GREENTIC_ENV").ok(),
            tenant: Some(ctx.tenant.clone()),
            team: ctx.team.clone(),
        };
        let core = self.runtime_core();
        core.registry()
            .discovered_capabilities()
            .into_iter()
            .filter(|offer| offer.requires_setup)
            .filter(|offer| {
                core.registry()
                    .resolve_capability(
                        &offer.capability_id,
                        Some(&offer.contract_id),
                        &scope,
                        offer.applies_to_ops.first().map(String::as_str),
                    )
                    .is_some_and(|binding| binding.stable_id == offer.stable_id)
            })
            .map(|offer| CapabilityBinding {
                cap_id: offer.capability_id.clone(),
                stable_id: offer.stable_id.clone(),
                pack_id: offer.provider_pack.clone(),
                domain: offer.domain,
                pack_path: offer.pack_path.clone(),
                provider_component_ref: offer.component_ref.clone(),
                provider_op: offer.entrypoint.clone(),
                version: offer.contract_id.clone(),
                requires_setup: offer.requires_setup,
                setup_qa_ref: offer.setup_qa_ref.clone(),
            })
            .collect()
    }

    pub fn supports_subscription_pack(
        &self,
        provider_pack: &str,
        contract_id: Option<&str>,
    ) -> bool {
        self.runtime_core()
            .wiring_plan()
            .has_subscription_provider(provider_pack, contract_id)
    }

    pub fn resolve_runtime_hook_chain(
        &self,
        stage: &str,
        contract_id: &str,
    ) -> Vec<RuntimeHookDescriptor> {
        self.runtime_core()
            .wiring_plan()
            .hook_chain(stage, contract_id)
            .to_vec()
    }

    pub fn authorize_admin_action(
        &self,
        action: AdminAction,
    ) -> anyhow::Result<AuthorizationDecision> {
        let decision =
            if let Some(hook) = self.runtime_core().seams().admin_authorization_hook.clone() {
                make_runtime_or_thread_scope(|runtime| {
                    runtime.block_on(async { hook.authorize(&action).await })
                })?
            } else {
                AuthorizationDecision::Allow
            };
        self.publish_admin_audit_event(&action, &decision);
        Ok(decision)
    }

    pub fn mark_capability_ready(
        &self,
        ctx: &OperatorContext,
        binding: &CapabilityBinding,
    ) -> anyhow::Result<PathBuf> {
        let record =
            CapabilityInstallRecord::ready(&binding.cap_id, &binding.stable_id, &binding.pack_id);
        write_install_record(&self.bundle_root, &ctx.tenant, ctx.team.as_deref(), &record)
    }

    pub fn mark_capability_failed(
        &self,
        ctx: &OperatorContext,
        binding: &CapabilityBinding,
        failure_key: &str,
    ) -> anyhow::Result<PathBuf> {
        let record = CapabilityInstallRecord::failed(
            &binding.cap_id,
            &binding.stable_id,
            &binding.pack_id,
            failure_key,
        );
        write_install_record(&self.bundle_root, &ctx.tenant, ctx.team.as_deref(), &record)
    }

    pub fn invoke_capability(
        &self,
        cap_id: &str,
        op: &str,
        payload_bytes: &[u8],
        ctx: &OperatorContext,
    ) -> anyhow::Result<FlowOutcome> {
        let requested_op = op.trim();
        if cap_id == CAP_OAUTH_BROKER_V1 {
            if requested_op.is_empty() {
                return Ok(capability_route_error_outcome(
                    cap_id,
                    "<missing-op>",
                    format!(
                        "oauth broker capability requires an explicit op (supported: {}, {}, {}, {})",
                        OAUTH_OP_INITIATE_AUTH,
                        OAUTH_OP_AWAIT_RESULT,
                        OAUTH_OP_GET_ACCESS_TOKEN,
                        OAUTH_OP_REQUEST_RESOURCE_TOKEN
                    ),
                ));
            }
            if !is_oauth_broker_operation(requested_op) {
                return Ok(capability_route_error_outcome(
                    cap_id,
                    requested_op,
                    format!(
                        "unsupported oauth broker op `{requested_op}` (supported: {}, {}, {}, {})",
                        OAUTH_OP_INITIATE_AUTH,
                        OAUTH_OP_AWAIT_RESULT,
                        OAUTH_OP_GET_ACCESS_TOKEN,
                        OAUTH_OP_REQUEST_RESOURCE_TOKEN
                    ),
                ));
            }
        }
        let scope = ResolveScope {
            env: env::var("GREENTIC_ENV").ok(),
            tenant: Some(ctx.tenant.clone()),
            team: ctx.team.clone(),
        };
        let binding = if requested_op.is_empty() {
            self.resolve_capability(cap_id, None, scope)
        } else {
            self.runtime_core().registry().resolve_capability(
                cap_id,
                None,
                &scope,
                Some(requested_op),
            )
        };
        let Some(binding) = binding else {
            return Ok(missing_capability_outcome(cap_id, op, None));
        };
        if !is_binding_ready(
            &self.bundle_root,
            &ctx.tenant,
            ctx.team.as_deref(),
            &binding,
        )? {
            return Ok(capability_not_installed_outcome(
                cap_id,
                op,
                &binding.stable_id,
            ));
        }

        let Some(pack) = self.provider_pack(&binding.pack_path) else {
            return Ok(capability_route_error_outcome(
                cap_id,
                op,
                format!("resolved pack not found at {}", binding.pack_path.display()),
            ));
        };

        let target_op = if cap_id == CAP_OAUTH_BROKER_V1 || requested_op.is_empty() {
            // OAuth broker cap.invoke always routes through the selected provider op.
            binding.provider_op.as_str()
        } else {
            requested_op
        };

        // Capability invocations go through the same operator pipeline.
        let mut envelope =
            OperationEnvelope::new(&format!("cap.invoke:{cap_id}"), payload_bytes, ctx);
        let token_validation_outcome =
            self.evaluate_token_validation_pre_hook(&mut envelope, payload_bytes, ctx)?;
        if let HookChainOutcome::Denied(reason) = token_validation_outcome {
            envelope.status = OperationStatus::Denied;
            self.emit_post_sub(&envelope);
            return Ok(capability_route_error_outcome(
                cap_id,
                target_op,
                format!("operation denied by pre-hook: {reason}"),
            ));
        }
        let pre_chain = self.resolve_hook_chain(HookStage::Pre, &envelope.op_name);
        let pre_hook_outcome =
            self.evaluate_hook_chain(&pre_chain, HookStage::Pre, &mut envelope)?;
        self.emit_pre_sub(&envelope);
        if let HookChainOutcome::Denied(reason) = pre_hook_outcome {
            envelope.status = OperationStatus::Denied;
            self.emit_post_sub(&envelope);
            return Ok(capability_route_error_outcome(
                cap_id,
                target_op,
                format!("operation denied by pre-hook: {reason}"),
            ));
        }

        let outcome = self.invoke_provider_component_op(
            binding.domain,
            &pack,
            &binding.pack_id,
            target_op,
            payload_bytes,
            ctx,
        )?;

        envelope.status = if outcome.success {
            OperationStatus::Ok
        } else {
            OperationStatus::Err
        };
        envelope.result_cbor = outcome.output.as_ref().and_then(json_to_canonical_cbor);
        let post_chain = self.resolve_hook_chain(HookStage::Post, &envelope.op_name);
        let _ = self.evaluate_hook_chain(&post_chain, HookStage::Post, &mut envelope)?;
        self.emit_post_sub(&envelope);
        Ok(outcome)
    }

    pub fn supports_op(&self, domain: Domain, provider_type: &str, op_id: &str) -> bool {
        self.provider_inventory()
            .catalog
            .get(&(domain, provider_type.to_string()))
            .map(|pack| {
                pack.entry_flows.iter().any(|flow| flow == op_id)
                    || self
                        .pack_supports_provider_op_for_pack(&pack.path, op_id)
                        .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    pub fn pack_supports_provider_op_for_pack(
        &self,
        pack_path: &Path,
        op_id: &str,
    ) -> anyhow::Result<bool> {
        let bytes = self.read_bundle_bytes(pack_path)?;
        pack_supports_provider_op_from_pack_bytes(pack_path, &bytes, op_id)
    }

    pub fn resolve_bundle_pack_path(&self, target_pack: &str) -> anyhow::Result<PathBuf> {
        let packs_root = self.bundle_read_root().join("packs");
        let candidates = [
            PathBuf::from(target_pack),
            packs_root.join(target_pack),
            packs_root.join(format!("{target_pack}.gtpack")),
        ];
        for candidate in candidates {
            if self.bundle_path_exists(&candidate) {
                return Ok(candidate);
            }
        }
        for entry in self.list_bundle_dir(&packs_root)? {
            let path = PathBuf::from(entry);
            if path
                .file_stem()
                .and_then(|value| value.to_str())
                .map(|value| value == target_pack)
                .unwrap_or(false)
            {
                return Ok(path);
            }
            let bytes = self.read_bundle_bytes(&path)?;
            let parsed = load_pack_offers_from_bytes(&path, &bytes)?;
            if parsed.pack_id == target_pack {
                return Ok(path);
            }
        }
        anyhow::bail!(
            "dispatch target pack {} not found under {}",
            target_pack,
            packs_root.display()
        );
    }

    pub fn bundle_runtime_root(&self) -> PathBuf {
        if let Some(bundle_source) = &self.runtime_core().seams().bundle_source {
            return make_runtime_or_thread_scope(|runtime| {
                runtime.block_on(bundle_source.stage("."))
            })
            .unwrap_or_else(|_| self.bundle_root.clone());
        }
        self.bundle_root.clone()
    }

    pub fn bundle_runtime_id(&self) -> String {
        if let Some(bundle_resolver) = &self.runtime_core().seams().bundle_resolver {
            return make_runtime_or_thread_scope(|runtime| {
                runtime.block_on(bundle_resolver.resolve("."))
            })
            .unwrap_or_else(|_| self.bundle_root.display().to_string());
        }
        self.bundle_root.display().to_string()
    }

    pub fn runtime_status_snapshot(&self) -> JsonValue {
        let provider_health = self.provider_health_snapshot();
        let active_bundle_access = self.active_bundle_access();
        let core = self.runtime_core();
        json!({
            "bundle": {
                "access": active_bundle_access.diagnostics(),
                "runtime_id": self.bundle_runtime_id(),
                "runtime_root": self.bundle_runtime_root(),
                "lifecycle": self.bundle_lifecycle.snapshot(),
            },
            "runtime_state": self.transition_report.snapshot(),
            "events": self.event_delivery_report.snapshot(),
            "mode": self.runtime_mode_snapshot(&provider_health),
            "roles": runtime_role_payload(core.as_ref()),
            "provider_health": self.provider_health_snapshot_value(&provider_health),
            "dependencies": self.runtime_dependency_snapshot(&provider_health),
            "seam_health": {
                "session": optional_seam_health(core.seams().session_provider.as_ref()),
                "state": optional_seam_health(core.seams().state_provider.as_ref()),
                "telemetry": optional_seam_health(core.seams().telemetry_provider.as_ref()),
                "observer": optional_seam_health(core.seams().observer_sink.as_ref()),
                "admin_auth": optional_seam_health(core.seams().admin_authorization_hook.as_ref()),
                "bundle_source": optional_seam_health(core.seams().bundle_source.as_ref()),
                "bundle_resolver": optional_seam_health(core.seams().bundle_resolver.as_ref()),
                "bundle_fs": optional_seam_health(core.seams().bundle_fs.as_ref()),
            }
        })
    }

    fn runtime_mode_snapshot(&self, provider_health: &RuntimeProviderHealthState) -> JsonValue {
        json!({
            "status": provider_health.overall_status,
            "safe_mode": provider_health.safe_mode,
            "degraded_level": provider_health.degraded_level,
            "direct_execution_allowed": provider_health.direct_execution_allowed,
        })
    }

    fn runtime_dependency_snapshot(
        &self,
        provider_health: &RuntimeProviderHealthState,
    ) -> JsonValue {
        let state = self.runtime_dependency_state_from_provider_health(provider_health);
        json!({
            "overall_status": state.overall_status,
            "required": state.reports,
        })
    }

    fn provider_health_snapshot_value(
        &self,
        provider_health: &RuntimeProviderHealthState,
    ) -> JsonValue {
        json!({
            "overall_status": provider_health.overall_status,
            "safe_mode": provider_health.safe_mode,
            "degraded_level": provider_health.degraded_level,
            "direct_execution_allowed": provider_health.direct_execution_allowed,
            "providers": provider_health.reports,
        })
    }

    fn runtime_dependency_state(&self) -> RuntimeDependencyState {
        let provider_health = self.provider_health_snapshot();
        self.runtime_dependency_state_from_provider_health(&provider_health)
    }

    fn runtime_dependency_state_from_provider_health(
        &self,
        provider_health: &RuntimeProviderHealthState,
    ) -> RuntimeDependencyState {
        let reports = provider_health
            .reports
            .iter()
            .filter(|report| report.required)
            .map(|report| RuntimeDependencyReport {
                name: report.provider_class,
                required: report.required,
                configured: report.configured,
                status: report.status,
                reason: report.reason.clone(),
            })
            .collect::<Vec<_>>();
        RuntimeDependencyState {
            overall_status: provider_health.overall_status,
            reports,
        }
    }

    fn collect_provider_health_reports(&self) -> Vec<RuntimeProviderHealthReport> {
        let core = self.runtime_core();
        vec![
            provider_health_report("session", true, core.seams().session_provider.as_ref()),
            provider_health_report("state", true, core.seams().state_provider.as_ref()),
            provider_health_report("telemetry", false, core.seams().telemetry_provider.as_ref()),
            provider_health_report("observer", false, core.seams().observer_sink.as_ref()),
            provider_health_report("bundle_source", true, core.seams().bundle_source.as_ref()),
            provider_health_report(
                "bundle_resolver",
                true,
                core.seams().bundle_resolver.as_ref(),
            ),
            provider_health_report("bundle_fs", true, core.seams().bundle_fs.as_ref()),
        ]
    }

    pub fn enforce_request_policy(
        &self,
        request_class: &str,
        required_provider_classes: &[&'static str],
        max_degraded_level: u8,
    ) -> Result<(), RuntimeRequestPolicyRefusal> {
        let provider_health = self.provider_health_snapshot();
        let mut blocking_provider_classes = provider_health
            .reports
            .iter()
            .filter(|report| {
                required_provider_classes.contains(&report.provider_class)
                    && report.status == "unavailable"
            })
            .map(|report| report.provider_class.to_string())
            .collect::<Vec<_>>();
        let degraded_level_exceeded = provider_health.degraded_level > max_degraded_level;
        if degraded_level_exceeded && blocking_provider_classes.is_empty() {
            blocking_provider_classes = required_provider_classes
                .iter()
                .map(|provider_class| provider_class.to_string())
                .collect();
        }
        if blocking_provider_classes.is_empty() && !degraded_level_exceeded {
            return Ok(());
        }
        let message = if degraded_level_exceeded {
            format!(
                "request class `{request_class}` refused in safe mode: degraded_level={} safe_mode={}",
                provider_health.degraded_level, provider_health.safe_mode
            )
        } else {
            format!(
                "request class `{request_class}` refused due to unavailable providers: {}",
                blocking_provider_classes.join(", ")
            )
        };
        self.publish_transition_event(
            "runtime.load_shed",
            "warn",
            Some("refused"),
            vec![
                "runtime.load_shed".to_string(),
                format!("request_class.{request_class}"),
            ],
            json!({
                "request_class": request_class,
                "mode": "refused",
                "safe_mode": provider_health.safe_mode,
                "degraded_level": provider_health.degraded_level,
                "blocking_provider_classes": blocking_provider_classes.clone(),
                "provider_health": self.provider_health_snapshot_value(&provider_health),
                "reason": message,
            }),
        );
        Err(RuntimeRequestPolicyRefusal {
            code: "runtime_request_refused",
            request_class: request_class.to_string(),
            message,
            safe_mode: provider_health.safe_mode,
            degraded_level: provider_health.degraded_level,
            blocking_provider_classes,
        })
    }

    fn ensure_required_runtime_dependencies_available(&self) -> anyhow::Result<()> {
        let state = self.runtime_dependency_state();
        let unavailable = state
            .reports
            .into_iter()
            .filter(|report| report.required && report.status == "unavailable")
            .map(|report| {
                let reason = report.reason.unwrap_or_else(|| "unavailable".to_string());
                format!("{}: {}", report.name, reason)
            })
            .collect::<Vec<_>>();
        if unavailable.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "required runtime dependencies unavailable: {}",
                unavailable.join(", ")
            );
        }
    }

    fn bundle_path_exists(&self, path: &Path) -> bool {
        if let Some(bundle_fs) = &self.runtime_core().seams().bundle_fs {
            return make_runtime_or_thread_scope(|runtime| {
                runtime.block_on(bundle_fs.exists(&path.display().to_string()))
            })
            .unwrap_or(false);
        }
        self.resolve_bundle_read_path(path).exists()
    }

    fn list_bundle_dir(&self, path: &Path) -> anyhow::Result<Vec<String>> {
        if let Some(bundle_fs) = &self.runtime_core().seams().bundle_fs {
            return make_runtime_or_thread_scope(|runtime| {
                runtime.block_on(bundle_fs.list_dir(&path.display().to_string()))
            });
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(self.resolve_bundle_read_path(path))? {
            let entry = entry?;
            entries.push(entry.path().display().to_string());
        }
        entries.sort();
        Ok(entries)
    }

    fn read_bundle_bytes(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        if let Some(bundle_fs) = &self.runtime_core().seams().bundle_fs {
            return make_runtime_or_thread_scope(|runtime| {
                runtime.block_on(bundle_fs.read(&path.display().to_string()))
            });
        }
        Ok(std::fs::read(self.resolve_bundle_read_path(path))?)
    }

    fn resolve_bundle_read_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.bundle_read_root().join(path)
        }
    }

    pub fn invoke_provider_op(
        &self,
        domain: Domain,
        provider_type: &str,
        op_id: &str,
        payload_bytes: &[u8],
        ctx: &OperatorContext,
    ) -> anyhow::Result<FlowOutcome> {
        let mut envelope = OperationEnvelope::new(op_id, payload_bytes, ctx);
        let token_validation_outcome =
            self.evaluate_token_validation_pre_hook(&mut envelope, payload_bytes, ctx)?;
        if let HookChainOutcome::Denied(reason) = token_validation_outcome {
            envelope.status = OperationStatus::Denied;
            self.emit_pre_sub(&envelope);
            self.emit_post_sub(&envelope);
            return Ok(FlowOutcome {
                success: false,
                output: None,
                raw: None,
                error: Some(format!("operation denied by pre-hook: {reason}")),
                mode: RunnerExecutionMode::Exec,
            });
        }
        let pre_chain = self.resolve_hook_chain(HookStage::Pre, op_id);
        let pre_hook_outcome =
            self.evaluate_hook_chain(&pre_chain, HookStage::Pre, &mut envelope)?;
        self.emit_pre_sub(&envelope);
        if let HookChainOutcome::Denied(reason) = pre_hook_outcome {
            envelope.status = OperationStatus::Denied;
            self.emit_post_sub(&envelope);
            return Ok(FlowOutcome {
                success: false,
                output: Some(serde_json::to_value(&envelope).unwrap_or_else(|_| json!({}))),
                raw: None,
                error: Some(format!("operation denied by pre-hook: {reason}")),
                mode: RunnerExecutionMode::Exec,
            });
        }

        let outcome =
            self.invoke_provider_op_inner(domain, provider_type, op_id, payload_bytes, ctx)?;
        envelope.status = if outcome.success {
            OperationStatus::Ok
        } else {
            OperationStatus::Err
        };
        envelope.result_cbor = outcome.output.as_ref().and_then(json_to_canonical_cbor);

        let post_chain = self.resolve_hook_chain(HookStage::Post, op_id);
        let _ = self.evaluate_hook_chain(&post_chain, HookStage::Post, &mut envelope)?;
        self.emit_post_sub(&envelope);
        Ok(outcome)
    }

    fn invoke_provider_op_inner(
        &self,
        domain: Domain,
        provider_type: &str,
        op_id: &str,
        payload_bytes: &[u8],
        ctx: &OperatorContext,
    ) -> anyhow::Result<FlowOutcome> {
        let pack = self
            .provider_inventory()
            .catalog
            .get(&(domain, provider_type.to_string()))
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "provider {} not found for domain {}",
                    provider_type,
                    domains::domain_name(domain)
                )
            })?;

        if pack.entry_flows.iter().any(|flow| flow == op_id) {
            let flow_id = op_id;
            if self.debug_enabled {
                operator_log::debug(
                    module_path!(),
                    format!(
                        "[demo dev] invoking provider domain={} provider={} flow={} tenant={} team={} payload_len={} preview={}",
                        domains::domain_name(domain),
                        provider_type,
                        flow_id,
                        ctx.tenant,
                        ctx.team.as_deref().unwrap_or("default"),
                        payload_bytes.len(),
                        payload_preview(payload_bytes),
                    ),
                );
            }
            let run_dir = self.runtime_run_dir(domain, &pack.pack_id, flow_id)?;
            std::fs::create_dir_all(&run_dir)?;

            let render_outcome = self.card_renderer.render_if_needed(
                provider_type,
                payload_bytes,
                |cap_id, op, input| {
                    let outcome = self.invoke_capability(cap_id, op, input, ctx)?;
                    if !outcome.success {
                        let reason = outcome
                            .error
                            .clone()
                            .or(outcome.raw.clone())
                            .unwrap_or_else(|| "capability invocation failed".to_string());
                        return Err(anyhow!(
                            "card capability {}:{} failed: {}",
                            cap_id,
                            op,
                            reason
                        ));
                    }
                    outcome.output.ok_or_else(|| {
                        anyhow!(
                            "card capability {}:{} returned no structured output",
                            cap_id,
                            op
                        )
                    })
                },
            )?;
            let payload = serde_json::from_slice(&render_outcome.bytes).unwrap_or_else(|_| {
                json!({
                    "payload": general_purpose::STANDARD.encode(&render_outcome.bytes)
                })
            });

            let outcome = match &self.runner_mode {
                RunnerMode::Exec => {
                    self.execute_with_runner_exec(domain, &pack, flow_id, &payload, ctx, &run_dir)?
                }
                RunnerMode::Integration { binary, flavor } => self
                    .execute_with_runner_integration(
                        domain, &pack, flow_id, &payload, ctx, &run_dir, binary, *flavor,
                    )?,
            };

            if self.debug_enabled {
                operator_log::debug(
                    module_path!(),
                    format!(
                        "[demo dev] provider={} flow={} tenant={} team={} success={} mode={:?} error={:?} corr_id={}",
                        provider_type,
                        flow_id,
                        ctx.tenant,
                        ctx.team.as_deref().unwrap_or("default"),
                        outcome.success,
                        outcome.mode,
                        outcome.error,
                        ctx.correlation_id.as_deref().unwrap_or("none"),
                    ),
                );
            }
            operator_log::info(
                module_path!(),
                format!(
                    "invoke domain={} provider={} op={} mode={:?} corr={}",
                    domains::domain_name(domain),
                    provider_type,
                    flow_id,
                    outcome.mode,
                    ctx.correlation_id.as_deref().unwrap_or("none")
                ),
            );

            return Ok(outcome);
        }

        self.invoke_provider_component_op(domain, &pack, provider_type, op_id, payload_bytes, ctx)
    }

    fn evaluate_hook_chain(
        &self,
        chain: &[CapabilityBinding],
        stage: HookStage,
        envelope: &mut OperationEnvelope,
    ) -> anyhow::Result<HookChainOutcome> {
        for binding in chain {
            let Some(pack) = self.provider_pack(&binding.pack_path) else {
                operator_log::warn(
                    module_path!(),
                    format!(
                        "hook binding skipped; pack not found stable_id={} path={}",
                        binding.stable_id,
                        binding.pack_path.display()
                    ),
                );
                continue;
            };

            let payload = canonical::to_canonical_cbor(&HookEvalRequest {
                stage: match stage {
                    HookStage::Pre => "pre",
                    HookStage::Post => "post",
                }
                .to_string(),
                op_name: envelope.op_name.clone(),
                envelope: envelope.clone(),
            })
            .map_err(|err| anyhow!("failed to encode hook request as cbor: {err}"))?;
            let ctx = OperatorContext {
                tenant: envelope.ctx.tenant.clone(),
                team: envelope.ctx.team.clone(),
                correlation_id: envelope.ctx.correlation_id.clone(),
            };
            let outcome = self.invoke_provider_component_op(
                binding.domain,
                &pack,
                &binding.pack_id,
                &binding.provider_op,
                &payload,
                &ctx,
            )?;
            if !outcome.success {
                operator_log::warn(
                    module_path!(),
                    format!(
                        "hook invocation failed stage={:?} binding={} err={}",
                        stage,
                        binding.stable_id,
                        outcome.error.unwrap_or_else(|| "unknown error".to_string())
                    ),
                );
                continue;
            }
            let Some(output) = outcome.output else {
                continue;
            };
            let parsed: HookEvalResponse = match decode_hook_response(&output) {
                Ok(value) => value,
                Err(err) => {
                    operator_log::warn(
                        module_path!(),
                        format!(
                            "hook response decode failed stage={:?} binding={} err={} (expected cbor, with legacy json fallback)",
                            stage, binding.stable_id, err
                        ),
                    );
                    continue;
                }
            };
            if let Some(updated) = parsed.envelope {
                *envelope = updated;
            }
            if parsed.decision.eq_ignore_ascii_case("deny") && matches!(stage, HookStage::Pre) {
                let reason = parsed
                    .reason
                    .unwrap_or_else(|| "hook denied operation".to_string());
                return Ok(HookChainOutcome::Denied(reason));
            }
        }
        Ok(HookChainOutcome::Continue)
    }

    fn evaluate_token_validation_pre_hook(
        &self,
        envelope: &mut OperationEnvelope,
        payload_bytes: &[u8],
        ctx: &OperatorContext,
    ) -> anyhow::Result<HookChainOutcome> {
        if envelope
            .op_name
            .starts_with(&format!("cap.invoke:{CAP_OAUTH_TOKEN_VALIDATION_V1}"))
        {
            return Ok(HookChainOutcome::Continue);
        }
        let Some(validation_request) = extract_token_validation_request(payload_bytes) else {
            return Ok(HookChainOutcome::Continue);
        };
        let scope = ResolveScope {
            env: env::var("GREENTIC_ENV").ok(),
            tenant: Some(ctx.tenant.clone()),
            team: ctx.team.clone(),
        };
        let Some(binding) = self.resolve_capability(CAP_OAUTH_TOKEN_VALIDATION_V1, None, scope)
        else {
            return Ok(HookChainOutcome::Continue);
        };
        if !is_binding_ready(
            &self.bundle_root,
            &ctx.tenant,
            ctx.team.as_deref(),
            &binding,
        )? {
            return Ok(HookChainOutcome::Denied(format!(
                "token validation capability is not installed (stable_id={})",
                binding.stable_id
            )));
        }
        let Some(pack) = self.provider_pack(&binding.pack_path) else {
            return Ok(HookChainOutcome::Denied(format!(
                "token validation pack not found at {}",
                binding.pack_path.display()
            )));
        };
        let request_bytes = serde_json::to_vec(&validation_request)
            .map_err(|err| anyhow!("failed to encode token validation payload: {err}"))?;
        let outcome = self.invoke_provider_component_op(
            binding.domain,
            &pack,
            &binding.pack_id,
            &binding.provider_op,
            &request_bytes,
            ctx,
        )?;
        if !outcome.success {
            let reason = outcome
                .error
                .unwrap_or_else(|| "token validation capability invocation failed".to_string());
            return Ok(HookChainOutcome::Denied(reason));
        }
        let Some(output) = outcome.output else {
            return Ok(HookChainOutcome::Denied(
                "token validation returned no output".to_string(),
            ));
        };
        match evaluate_token_validation_output(&output) {
            TokenValidationDecision::Allow(claims) => {
                envelope.ctx.auth_claims = claims;
                Ok(HookChainOutcome::Continue)
            }
            TokenValidationDecision::Deny(reason) => Ok(HookChainOutcome::Denied(reason)),
        }
    }

    fn emit_pre_sub(&self, envelope: &OperationEnvelope) {
        self.publish_runtime_event("runtime.pre_op", envelope);
        operator_log::info(
            module_path!(),
            format!(
                "sub.pre op={} status={:?} tenant={} team={}",
                envelope.op_name,
                envelope.status,
                envelope.ctx.tenant,
                envelope.ctx.team.as_deref().unwrap_or("default")
            ),
        );
    }

    fn emit_post_sub(&self, envelope: &OperationEnvelope) {
        self.publish_runtime_event("runtime.post_op", envelope);
        operator_log::info(
            module_path!(),
            format!(
                "sub.post op={} status={:?} tenant={} team={}",
                envelope.op_name,
                envelope.status,
                envelope.ctx.tenant,
                envelope.ctx.team.as_deref().unwrap_or("default")
            ),
        );
    }

    fn publish_runtime_event(&self, event_type: &str, envelope: &OperationEnvelope) {
        let bundle_id = self.bundle_runtime_id();
        let (severity, outcome, reason_codes) = self.operation_event_metadata(event_type, envelope);
        let event = RuntimeEvent {
            event_type: event_type.to_string(),
            ts_unix_ms: Self::now_unix_ms(),
            tenant: Some(envelope.ctx.tenant.clone()),
            team: envelope.ctx.team.clone(),
            session_id: None,
            bundle_id: Some(bundle_id),
            pack_id: None,
            flow_id: Some(envelope.op_id.clone()),
            node_id: Some(envelope.op_name.clone()),
            correlation_id: envelope.ctx.correlation_id.clone(),
            trace_id: None,
            severity,
            outcome,
            reason_codes,
            payload: json!({
                "envelope": envelope,
                "runtime_roles": runtime_role_payload(self.runtime_core().as_ref()),
            }),
        };
        self.deliver_runtime_event(event_type, event);
        self.persist_runtime_event_state(event_type, envelope);
        self.persist_runtime_session(event_type, envelope);
    }

    fn publish_admin_audit_event(&self, action: &AdminAction, decision: &AuthorizationDecision) {
        let event = RuntimeEvent {
            event_type: "admin.action".to_string(),
            ts_unix_ms: Self::now_unix_ms(),
            tenant: None,
            team: None,
            session_id: None,
            bundle_id: Some(self.bundle_runtime_id()),
            pack_id: None,
            flow_id: None,
            node_id: None,
            correlation_id: None,
            trace_id: None,
            severity: if matches!(decision, AuthorizationDecision::Deny { .. }) {
                "warn".to_string()
            } else {
                "info".to_string()
            },
            outcome: Some(
                match decision {
                    AuthorizationDecision::Allow => "allow",
                    AuthorizationDecision::Deny { .. } => "deny",
                }
                .to_string(),
            ),
            reason_codes: if matches!(decision, AuthorizationDecision::Deny { .. }) {
                vec![
                    "admin.action".to_string(),
                    "authorization.denied".to_string(),
                ]
            } else {
                vec!["admin.action".to_string()]
            },
            payload: json!({
                "action": action.action,
                "actor": action.actor,
                "resource": action.resource,
            }),
        };
        self.deliver_runtime_event("admin.action", event);
    }

    fn deliver_runtime_event(&self, event_type: &str, event: RuntimeEvent) {
        let Ok(_guard) = self.event_delivery_gate.try_lock() else {
            operator_log::warn(
                module_path!(),
                format!(
                    "runtime event dropped by backpressure policy event_type={} policy=drop_when_busy",
                    event_type
                ),
            );
            self.event_delivery_report.record_drop(event_type);
            return;
        };
        let core = self.runtime_core();
        let telemetry_event = event.clone();
        let (telemetry_outcome, observer_outcome) = make_runtime_or_thread_scope(|runtime| {
            runtime.block_on(async {
                let telemetry_outcome = match tokio::time::timeout(
                    Self::EVENT_DELIVERY_TIMEOUT,
                    core.emit_telemetry_event(telemetry_event),
                )
                .await
                {
                    Ok(Ok(())) => EventDeliveryOutcome::Delivered,
                    Ok(Err(err)) => EventDeliveryOutcome::Failed(err.to_string()),
                    Err(_) => EventDeliveryOutcome::TimedOut,
                };
                let observer_outcome = match tokio::time::timeout(
                    Self::EVENT_DELIVERY_TIMEOUT,
                    core.publish_observer_event(event),
                )
                .await
                {
                    Ok(Ok(())) => EventDeliveryOutcome::Delivered,
                    Ok(Err(err)) => EventDeliveryOutcome::Failed(err.to_string()),
                    Err(_) => EventDeliveryOutcome::TimedOut,
                };
                (telemetry_outcome, observer_outcome)
            })
        });
        self.record_event_delivery_outcome(
            event_type,
            EventDeliveryTarget::Telemetry,
            telemetry_outcome,
        );
        self.record_event_delivery_outcome(
            event_type,
            EventDeliveryTarget::Observer,
            observer_outcome,
        );
    }

    fn record_event_delivery_outcome(
        &self,
        event_type: &str,
        target: EventDeliveryTarget,
        outcome: EventDeliveryOutcome,
    ) {
        let target_name = match target {
            EventDeliveryTarget::Telemetry => "telemetry",
            EventDeliveryTarget::Observer => "observer",
        };
        match &outcome {
            EventDeliveryOutcome::Delivered => {}
            EventDeliveryOutcome::TimedOut => operator_log::warn(
                module_path!(),
                format!(
                    "runtime event delivery timed out event_type={} target={} timeout_ms={}",
                    event_type,
                    target_name,
                    Self::EVENT_DELIVERY_TIMEOUT.as_millis()
                ),
            ),
            EventDeliveryOutcome::Failed(err) => operator_log::warn(
                module_path!(),
                format!(
                    "runtime event delivery failed event_type={} target={} err={}",
                    event_type, target_name, err
                ),
            ),
        }
        self.event_delivery_report.record(target, outcome);
    }

    fn persist_runtime_event_state(&self, event_type: &str, envelope: &OperationEnvelope) {
        let Some(state_provider) = self.runtime_core().seams().state_provider.clone() else {
            return;
        };
        let key = ScopedStateKey {
            tenant: envelope.ctx.tenant.clone(),
            team: envelope.ctx.team.clone(),
            scope: "runtime_events".to_string(),
            key: event_type.replace(['/', '\\', ':'], "_"),
        };
        let value = json!({
            "event_type": event_type,
            "runtime_roles": runtime_role_payload(self.runtime_core().as_ref()),
            "envelope": envelope,
        });
        if let Err(err) = make_runtime_or_thread_scope(|runtime| {
            runtime.block_on(async { state_provider.put(&key, value).await })
        }) {
            operator_log::warn(
                module_path!(),
                format!(
                    "runtime event state persist failed event_type={} err={}",
                    event_type, err
                ),
            );
        }
    }

    fn persist_runtime_session(&self, event_type: &str, envelope: &OperationEnvelope) {
        let Some(session_provider) = self.runtime_core().seams().session_provider.clone() else {
            return;
        };
        let key = RuntimeSessionKey {
            tenant: envelope.ctx.tenant.clone(),
            team: envelope.ctx.team.clone(),
            session_id: envelope.op_id.clone(),
        };
        let route = Some(envelope.op_name.clone());
        let bundle_assignment = Some(self.bundle_runtime_id());
        let context = json!({
            "event_type": event_type,
            "status": envelope.status,
            "runtime_roles": runtime_role_payload(self.runtime_core().as_ref()),
            "envelope": envelope,
        });
        if let Err(err) = make_runtime_or_thread_scope(|runtime| {
            runtime.block_on(async {
                let current = session_provider.get(&key).await?;
                let next_revision = current
                    .map(|record| record.revision.saturating_add(1))
                    .unwrap_or(1);
                session_provider
                    .put(
                        &key,
                        SessionRecord {
                            revision: next_revision,
                            route,
                            bundle_assignment,
                            context,
                            expires_at_unix_ms: None,
                        },
                    )
                    .await
            })
        }) {
            operator_log::warn(
                module_path!(),
                format!(
                    "runtime session persist failed op_id={} event_type={} err={}",
                    envelope.op_id, event_type, err
                ),
            );
        }
    }

    fn execute_with_runner_exec(
        &self,
        domain: Domain,
        pack: &ProviderPack,
        flow_id: &str,
        payload: &JsonValue,
        ctx: &OperatorContext,
        run_dir: &Path,
    ) -> anyhow::Result<FlowOutcome> {
        let request = runner_exec::RunRequest {
            root: self.bundle_root.clone(),
            run_dir: Some(run_dir.to_path_buf()),
            domain,
            pack_path: pack.path.clone(),
            pack_label: pack.pack_id.clone(),
            flow_id: flow_id.to_string(),
            tenant: ctx.tenant.clone(),
            team: ctx.team.clone(),
            input: payload.clone(),
            dist_offline: true,
        };
        let run_output = runner_exec::run_provider_pack_flow(request)?;
        let parsed = read_transcript_outputs(&run_output.run_dir)?;
        Ok(FlowOutcome {
            success: run_output.result.status == RunStatus::Success,
            output: parsed,
            raw: None,
            error: run_output.result.error.clone(),
            mode: RunnerExecutionMode::Exec,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_with_runner_integration(
        &self,
        _domain: Domain,
        pack: &ProviderPack,
        flow_id: &str,
        payload: &JsonValue,
        ctx: &OperatorContext,
        run_dir: &Path,
        runner_binary: &Path,
        flavor: RunnerFlavor,
    ) -> anyhow::Result<FlowOutcome> {
        let output = run_flow_with_options(
            runner_binary,
            &pack.path,
            flow_id,
            payload,
            RunFlowOptions {
                dist_offline: true,
                tenant: Some(&ctx.tenant),
                team: ctx.team.as_deref(),
                artifacts_dir: Some(run_dir),
                runner_flavor: flavor,
            },
        )?;
        let mut parsed = output.parsed.clone();
        if parsed.is_none() {
            parsed = read_transcript_outputs(run_dir)?;
        }
        let raw = if output.stdout.trim().is_empty() {
            None
        } else {
            Some(output.stdout.clone())
        };
        Ok(FlowOutcome {
            success: output.status.success(),
            output: parsed,
            raw,
            error: if output.status.success() {
                None
            } else {
                Some(output.stderr.clone())
            },
            mode: RunnerExecutionMode::Integration,
        })
    }

    pub fn invoke_provider_component_op_direct(
        &self,
        domain: Domain,
        pack: &ProviderPack,
        provider_id: &str,
        op_id: &str,
        payload_bytes: &[u8],
        ctx: &OperatorContext,
    ) -> anyhow::Result<FlowOutcome> {
        self.invoke_provider_component_op(domain, pack, provider_id, op_id, payload_bytes, ctx)
    }

    fn invoke_provider_component_op(
        &self,
        domain: Domain,
        pack: &ProviderPack,
        provider_id: &str,
        op_id: &str,
        payload_bytes: &[u8],
        ctx: &OperatorContext,
    ) -> anyhow::Result<FlowOutcome> {
        let dependency_state = self.runtime_dependency_state();
        if let Err(err) = self.ensure_required_runtime_dependencies_available() {
            let reason = err.to_string();
            self.publish_dependency_event(
                "runtime.dependency_unavailable",
                "warn",
                "unavailable",
                DependencyEventContext {
                    provider_id,
                    op_id,
                    ctx,
                    dependency_state: &dependency_state,
                    reason: Some(reason.clone()),
                },
            );
            self.publish_transition_event(
                "runtime.load_shed",
                "warn",
                Some("refused"),
                vec![
                    "runtime.load_shed".to_string(),
                    "reason.provider_unavailable".to_string(),
                ],
                json!({
                    "provider_id": provider_id,
                    "op_id": op_id,
                    "mode": "refused",
                    "dependencies": {
                        "overall_status": dependency_state.overall_status,
                        "required": dependency_state.reports,
                    },
                    "reason": reason,
                }),
            );
            return Ok(runtime_dependency_failure_outcome(
                reason,
                DependencyFailureMode::Unavailable,
                if matches!(self.runner_mode, RunnerMode::Integration { .. }) {
                    RunnerExecutionMode::Integration
                } else {
                    RunnerExecutionMode::Exec
                },
            ));
        }
        if dependency_state.overall_status == "degraded" {
            let degraded_messages = degraded_dependency_messages(&dependency_state);
            self.publish_dependency_event(
                "runtime.dependency_degraded_execution",
                "info",
                "degraded",
                DependencyEventContext {
                    provider_id,
                    op_id,
                    ctx,
                    dependency_state: &dependency_state,
                    reason: Some(degraded_messages.join(", ")),
                },
            );
            operator_log::info(
                module_path!(),
                format!(
                    "direct provider execution continuing in degraded mode: {}",
                    degraded_messages.join(", ")
                ),
            );
        }
        if let RunnerMode::Integration { binary, flavor } = &self.runner_mode {
            let payload_value: JsonValue =
                serde_json::from_slice(payload_bytes).unwrap_or_else(|_| json!({}));
            let run_dir = self.runtime_run_dir(domain, &pack.pack_id, op_id)?;
            std::fs::create_dir_all(&run_dir)?;
            return self.execute_with_runner_integration(
                domain,
                pack,
                op_id,
                &payload_value,
                ctx,
                &run_dir,
                binary,
                *flavor,
            );
        }

        let payload = payload_bytes.to_vec();
        let result = make_runtime_or_thread_scope(|runtime| {
            runtime.block_on(async {
            let host_config = Arc::new(build_demo_host_config(&ctx.tenant));
            // Re-open the dev store on each invocation so newly-written secrets
            // (e.g. from QA wizard submit) are visible without restarting the demo.
            let fresh_secrets = secrets_gate::resolve_secrets_manager(
                &self.bundle_root,
                &ctx.tenant,
                ctx.team.as_deref(),
            )
            .unwrap_or_else(|_| self.secrets_handle.clone());
            let dev_store_display = fresh_secrets
                .dev_store_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<default>".to_string());
            let runtime_roles = runtime_role_payload(self.runtime_core().as_ref());
            operator_log::info(
                module_path!(),
                format!(
                    "secrets backend for wasm: using_env_fallback={} dev_store={}",
                    fresh_secrets.using_env_fallback, dev_store_display,
                ),
            );
            operator_log::info(
                module_path!(),
                format!(
                    "exec runtime: roles={} dev_store={} env_fallback={}",
                    runtime_roles,
                    dev_store_display,
                    fresh_secrets.using_env_fallback,
                ),
            );
            let pack_runtime = PackRuntime::load(
                &pack.path,
                host_config.clone(),
                None,
                Some(&pack.path),
                Some(self.session_store.clone()),
                Some(self.state_store.clone()),
                Arc::new(RunnerWasiPolicy::default()),
                fresh_secrets.runtime_manager(Some(&pack.pack_id)),
                None,
                false,
                ComponentResolution::default(),
            )
            .await?;
            let provider_type = self
                .primary_provider_type_for_pack(&pack.path)
                .context("failed to determine provider type for direct invocation")?;
            let env_value = env::var("GREENTIC_ENV").unwrap_or_else(|_| "<unset>".to_string());
            let canonical_team = secrets_manager::canonical_team(ctx.team.as_deref()).into_owned();
            let runner_dev_store_desc = self
                .secrets_handle
                .dev_store_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<none>".to_string());
            eprintln!(
                "secrets runner ctx: env={} tenant={} canonical_team={} provider_id={} pack_id={} dev_store_path={} using_env_fallback={}",
                env_value,
                ctx.tenant,
                canonical_team,
                provider_type,
                pack.pack_id,
                runner_dev_store_desc,
                self.secrets_handle.using_env_fallback,
            );
            let binding = pack_runtime.resolve_provider(None, Some(&provider_type))?;
            let exec_ctx = ComponentExecCtx {
                tenant: ComponentTenantCtx {
                    tenant: ctx.tenant.clone(),
                    team: ctx.team.clone(),
                    i18n_id: None,
                    user: None,
                    trace_id: None,
                    correlation_id: ctx.correlation_id.clone(),
                    deadline_unix_ms: None,
                    attempt: 1,
                    idempotency_key: None,
                },
                i18n_id: None,
                flow_id: op_id.to_string(),
                node_id: Some(op_id.to_string()),
            };
            pack_runtime
                .invoke_provider(&binding, exec_ctx, op_id, payload)
                .await
        })
        });

        let degraded_messages = degraded_dependency_messages(&dependency_state);
        match result {
            Ok(value) => Ok(FlowOutcome {
                success: true,
                output: Some(attach_degraded_dependency_warning(
                    value,
                    dependency_state.overall_status == "degraded",
                    &degraded_messages,
                )),
                raw: None,
                error: None,
                mode: RunnerExecutionMode::Exec,
            }),
            Err(err) => {
                let err_message = err.to_string();
                let needs_context = needs_secret_context(&err_message);
                let enriched_err = if needs_context {
                    err.context(secret_error_context(ctx, provider_id, op_id, pack))
                } else {
                    err
                };
                let error_text = if needs_context {
                    enriched_err.to_string()
                } else {
                    err_message
                };
                Ok(FlowOutcome {
                    success: false,
                    output: None,
                    raw: None,
                    error: Some(error_text),
                    mode: RunnerExecutionMode::Exec,
                })
            }
        }
    }
}

pub fn primary_provider_type(pack_path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(pack_path)?;
    primary_provider_type_from_pack_bytes(pack_path, &bytes)
}

fn primary_provider_type_from_pack_bytes(
    pack_path: &Path,
    pack_bytes: &[u8],
) -> anyhow::Result<String> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(pack_bytes))?;
    let mut manifest_entry = archive.by_name("manifest.cbor").map_err(|err| {
        anyhow!(
            "failed to open manifest.cbor in {}: {err}",
            pack_path.display()
        )
    })?;
    let mut bytes = Vec::new();
    manifest_entry.read_to_end(&mut bytes)?;
    let manifest = decode_pack_manifest(&bytes)
        .context("failed to decode pack manifest for provider introspection")?;
    let inline = manifest.provider_extension_inline().ok_or_else(|| {
        anyhow!(
            "pack {} provider extension missing or not inline",
            pack_path.display()
        )
    })?;
    let provider = inline.providers.first().ok_or_else(|| {
        anyhow!(
            "pack {} provider extension contains no providers",
            pack_path.display()
        )
    })?;
    Ok(provider.provider_type.clone())
}

fn needs_secret_context(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("secret store error") || message.contains("SecretsError")
}

fn secret_error_context(
    ctx: &OperatorContext,
    provider_id: &str,
    op_id: &str,
    pack: &ProviderPack,
) -> String {
    let env = env::var("GREENTIC_ENV").unwrap_or_else(|_| "local".to_string());
    let team = secrets_manager::canonical_team(ctx.team.as_deref()).into_owned();
    format!(
        "secret lookup context env={} tenant={} team={} provider={} flow={} pack_id={} pack_path={}",
        env,
        ctx.tenant,
        team,
        provider_id,
        op_id,
        pack.pack_id,
        pack.path.display()
    )
}

fn json_to_canonical_cbor(value: &JsonValue) -> Option<Vec<u8>> {
    canonical::to_canonical_cbor_allow_floats(value).ok()
}

fn runtime_role_payload(core: &RuntimeCore) -> JsonValue {
    let selected = core
        .wiring_plan()
        .selected_providers
        .iter()
        .map(|(role_id, provider)| {
            (
                role_id.clone(),
                json!({
                    "capability_id": provider.capability_id,
                    "contract_id": provider.contract_id,
                    "pack_id": provider.provider_pack,
                    "stable_id": provider.stable_id,
                }),
            )
        })
        .collect::<serde_json::Map<String, JsonValue>>();
    json!({
        "selected": selected,
        "warnings": core.wiring_plan().warnings,
        "blocking_failures": core.wiring_plan().blocking_failures,
        "seams": {
            "session": core.seams().session_provider.is_some(),
            "state": core.seams().state_provider.is_some(),
            "telemetry": core.seams().telemetry_provider.is_some(),
            "observer": core.seams().observer_sink.is_some(),
            "admin_auth": core.seams().admin_authorization_hook.is_some(),
            "bundle_source": core.seams().bundle_source.is_some(),
            "bundle_resolver": core.seams().bundle_resolver.is_some(),
            "bundle_fs": core.seams().bundle_fs.is_some(),
        }
    })
}

fn optional_seam_health<T>(seam: Option<&Arc<T>>) -> JsonValue
where
    T: ?Sized + SeamHealth + Send + Sync,
{
    let Some(seam) = seam else {
        return json!({
            "configured": false,
            "status": "unconfigured",
            "reason": null,
        });
    };
    match make_runtime_or_thread_scope(|runtime| runtime.block_on(seam.health_dyn())) {
        Ok(health) => json!({
            "configured": true,
            "status": runtime_health_status_name(health.status),
            "reason": health.reason,
        }),
        Err(err) => json!({
            "configured": true,
            "status": "error",
            "reason": err.to_string(),
        }),
    }
}

fn provider_health_report<T>(
    provider_class: &'static str,
    required: bool,
    seam: Option<&Arc<T>>,
) -> RuntimeProviderHealthReport
where
    T: ?Sized + SeamHealth + Send + Sync,
{
    let Some(seam) = seam else {
        return RuntimeProviderHealthReport {
            provider_class,
            required,
            configured: false,
            status: if required { "unavailable" } else { "unknown" },
            reason: Some("unconfigured".to_string()),
            last_checked_at_unix_ms: 0,
            consecutive_failures: 0,
            recovery_state: "unknown",
        };
    };
    match make_runtime_or_thread_scope(|runtime| runtime.block_on(seam.health_dyn())) {
        Ok(health) => RuntimeProviderHealthReport {
            provider_class,
            required,
            configured: true,
            status: runtime_health_status_name(health.status),
            reason: health.reason,
            last_checked_at_unix_ms: 0,
            consecutive_failures: 0,
            recovery_state: "unknown",
        },
        Err(err) => RuntimeProviderHealthReport {
            provider_class,
            required,
            configured: true,
            status: "unavailable",
            reason: Some(err.to_string()),
            last_checked_at_unix_ms: 0,
            consecutive_failures: 0,
            recovery_state: "unknown",
        },
    }
}

trait SeamHealth {
    fn health_dyn<'a>(
        &'a self,
    ) -> core::pin::Pin<
        Box<dyn core::future::Future<Output = anyhow::Result<RuntimeHealth>> + Send + 'a>,
    >;
}

impl SeamHealth for dyn SessionProvider {
    fn health_dyn<'a>(
        &'a self,
    ) -> core::pin::Pin<
        Box<dyn core::future::Future<Output = anyhow::Result<RuntimeHealth>> + Send + 'a>,
    > {
        Box::pin(async move { self.health().await })
    }
}

impl SeamHealth for dyn StateProvider {
    fn health_dyn<'a>(
        &'a self,
    ) -> core::pin::Pin<
        Box<dyn core::future::Future<Output = anyhow::Result<RuntimeHealth>> + Send + 'a>,
    > {
        Box::pin(async move { self.health().await })
    }
}

impl SeamHealth for dyn crate::runtime_core::TelemetryProvider {
    fn health_dyn<'a>(
        &'a self,
    ) -> core::pin::Pin<
        Box<dyn core::future::Future<Output = anyhow::Result<RuntimeHealth>> + Send + 'a>,
    > {
        Box::pin(async move { self.health().await })
    }
}

impl SeamHealth for dyn crate::runtime_core::ObserverHookSink {
    fn health_dyn<'a>(
        &'a self,
    ) -> core::pin::Pin<
        Box<dyn core::future::Future<Output = anyhow::Result<RuntimeHealth>> + Send + 'a>,
    > {
        Box::pin(async move { self.health().await })
    }
}

impl SeamHealth for dyn AdminAuthorizationHook {
    fn health_dyn<'a>(
        &'a self,
    ) -> core::pin::Pin<
        Box<dyn core::future::Future<Output = anyhow::Result<RuntimeHealth>> + Send + 'a>,
    > {
        Box::pin(async move { self.health().await })
    }
}

impl SeamHealth for dyn BundleSource {
    fn health_dyn<'a>(
        &'a self,
    ) -> core::pin::Pin<
        Box<dyn core::future::Future<Output = anyhow::Result<RuntimeHealth>> + Send + 'a>,
    > {
        Box::pin(async move { self.health().await })
    }
}

impl SeamHealth for dyn BundleResolver {
    fn health_dyn<'a>(
        &'a self,
    ) -> core::pin::Pin<
        Box<dyn core::future::Future<Output = anyhow::Result<RuntimeHealth>> + Send + 'a>,
    > {
        Box::pin(async move { self.health().await })
    }
}

impl SeamHealth for dyn BundleFs {
    fn health_dyn<'a>(
        &'a self,
    ) -> core::pin::Pin<
        Box<dyn core::future::Future<Output = anyhow::Result<RuntimeHealth>> + Send + 'a>,
    > {
        Box::pin(async move { self.health().await })
    }
}

fn runtime_health_status_name(status: RuntimeHealthStatus) -> &'static str {
    match status {
        RuntimeHealthStatus::Unknown => "unknown",
        RuntimeHealthStatus::Available => "available",
        RuntimeHealthStatus::Degraded => "degraded",
        RuntimeHealthStatus::Unavailable => "unavailable",
    }
}

fn provider_health_progress(
    previous: Option<&RuntimeProviderHealthMemory>,
    current_status: &'static str,
) -> (u64, &'static str) {
    match current_status {
        "available" => (
            0,
            if previous.is_some_and(|prev| prev.status != "available") {
                "recovering"
            } else {
                "steady"
            },
        ),
        "degraded" | "unavailable" => (
            previous
                .filter(|prev| prev.status == current_status)
                .map(|prev| prev.consecutive_failures + 1)
                .unwrap_or(1),
            "failing",
        ),
        _ => (0, "unknown"),
    }
}

fn provider_transition_event(
    previous: Option<&RuntimeProviderHealthMemory>,
    current: &RuntimeProviderHealthReport,
) -> Option<ProviderTransitionEvent> {
    let previous_status = previous.map(|prev| prev.status);
    if previous_status == Some(current.status) {
        return None;
    }
    if current.status == "available" && previous_status.is_some_and(|status| status != "available")
    {
        return Some(ProviderTransitionEvent {
            event_type: "runtime.provider_recovery",
            severity: "info",
            outcome: "available",
            provider_class: current.provider_class,
            previous_status,
            current_status: current.status,
            reason: current.reason.clone(),
        });
    }
    if matches!(current.status, "degraded" | "unavailable") {
        return Some(ProviderTransitionEvent {
            event_type: "runtime.provider_outage",
            severity: if current.status == "unavailable" {
                "warn"
            } else {
                "info"
            },
            outcome: current.status,
            provider_class: current.provider_class,
            previous_status,
            current_status: current.status,
            reason: current.reason.clone(),
        });
    }
    None
}

fn compute_degraded_level(reports: &[RuntimeProviderHealthReport]) -> u8 {
    let has_required_unavailable = reports
        .iter()
        .any(|report| report.required && report.status == "unavailable");
    if has_required_unavailable {
        return 3;
    }
    let has_required_degraded = reports
        .iter()
        .any(|report| report.required && report.status == "degraded");
    if has_required_degraded {
        return 2;
    }
    let has_optional_issue = reports
        .iter()
        .any(|report| !report.required && matches!(report.status, "degraded" | "unavailable"));
    if has_optional_issue {
        return 1;
    }
    0
}

fn degraded_level_status_name(level: u8) -> &'static str {
    match level {
        0 => "available",
        1 | 2 => "degraded",
        _ => "unavailable",
    }
}

fn sanitize_state_key(value: &str) -> String {
    value.replace(['/', '\\', ':', '#'], "_")
}

fn map_runtime_state_store_error(err: anyhow::Error) -> GreenticError {
    GreenticError::new(GreenticErrorCode::Internal, err.to_string())
}

fn map_runtime_session_store_error(err: anyhow::Error) -> GreenticError {
    GreenticError::new(GreenticErrorCode::Internal, err.to_string())
}

fn duration_deadline_unix_ms(value: std::time::Duration) -> Option<u64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis();
    let ttl = value.as_millis();
    let deadline = now.checked_add(ttl)?;
    u64::try_from(deadline).ok()
}

fn upsert_json_pointer(
    value: &mut JsonValue,
    pointer: &str,
    replacement: JsonValue,
) -> anyhow::Result<()> {
    if pointer.is_empty() || pointer == "/" {
        *value = replacement;
        return Ok(());
    }
    let Some(target) = value.pointer_mut(pointer) else {
        anyhow::bail!("state path not found: {pointer}");
    };
    *target = replacement;
    Ok(())
}

fn decode_hook_response(value: &JsonValue) -> anyhow::Result<HookEvalResponse> {
    if let Some(cbor) = extract_cbor_blob(value)
        && let Ok(parsed) = serde_cbor::from_slice::<HookEvalResponse>(&cbor)
    {
        return Ok(parsed);
    }
    serde_json::from_value(value.clone())
        .map_err(|err| anyhow!("hook response is not valid cbor or legacy json: {err}"))
}

fn extract_cbor_blob(value: &JsonValue) -> Option<Vec<u8>> {
    match value {
        JsonValue::Array(items) => items
            .iter()
            .map(|item| item.as_u64().and_then(|n| u8::try_from(n).ok()))
            .collect::<Option<Vec<u8>>>(),
        JsonValue::String(s) => general_purpose::STANDARD.decode(s).ok(),
        JsonValue::Object(map) => {
            for key in ["hook_decision_cbor_b64", "cbor_b64", "hook_decision_cbor"] {
                let Some(raw) = map.get(key) else {
                    continue;
                };
                if let JsonValue::String(s) = raw
                    && let Ok(bytes) = general_purpose::STANDARD.decode(s)
                {
                    return Some(bytes);
                }
                if let Some(bytes) = extract_cbor_blob(raw) {
                    return Some(bytes);
                }
            }
            None
        }
        _ => None,
    }
}

fn missing_capability_outcome(
    cap_id: &str,
    op_name: &str,
    component_id: Option<&str>,
) -> FlowOutcome {
    FlowOutcome {
        success: false,
        output: Some(json!({
            "code": "missing_capability",
            "error": {
                "type": "MissingCapability",
                "cap_id": cap_id,
                "op_name": op_name,
                "component_id": component_id,
            }
        })),
        raw: None,
        error: Some(format!(
            "MissingCapability(cap_id={cap_id}, op_name={op_name}, component_id={})",
            component_id.unwrap_or("<unknown>")
        )),
        mode: RunnerExecutionMode::Exec,
    }
}

fn capability_not_installed_outcome(cap_id: &str, op_name: &str, stable_id: &str) -> FlowOutcome {
    FlowOutcome {
        success: false,
        output: Some(json!({
            "code": "capability_not_installed",
            "error": {
                "type": "CapabilityNotInstalled",
                "cap_id": cap_id,
                "op_name": op_name,
                "stable_id": stable_id,
            }
        })),
        raw: None,
        error: Some(format!(
            "CapabilityNotInstalled(cap_id={cap_id}, op_name={op_name}, stable_id={stable_id})"
        )),
        mode: RunnerExecutionMode::Exec,
    }
}

fn capability_route_error_outcome(cap_id: &str, op_name: &str, reason: String) -> FlowOutcome {
    FlowOutcome {
        success: false,
        output: Some(json!({
            "code": "capability_route_error",
            "error": {
                "type": "CapabilityRouteError",
                "cap_id": cap_id,
                "op_name": op_name,
                "reason": reason,
            }
        })),
        raw: None,
        error: Some(reason),
        mode: RunnerExecutionMode::Exec,
    }
}

fn runtime_dependency_failure_outcome(
    reason: String,
    dependency_mode: DependencyFailureMode,
    mode: RunnerExecutionMode,
) -> FlowOutcome {
    let (code, error_type, prefix) = match dependency_mode {
        DependencyFailureMode::Unavailable => (
            "runtime_dependency_unavailable",
            "RuntimeDependencyUnavailable",
            "runtime dependency unavailable",
        ),
    };
    FlowOutcome {
        success: false,
        output: Some(json!({
            "code": code,
            "error": {
                "type": error_type,
                "reason": reason,
            }
        })),
        raw: None,
        error: Some(format!("{prefix}: {reason}")),
        mode,
    }
}

fn attach_degraded_dependency_warning(
    mut value: JsonValue,
    degraded: bool,
    messages: &[String],
) -> JsonValue {
    if !degraded {
        return value;
    }
    let warning = json!({
        "status": "degraded",
        "reasons": messages,
    });
    match &mut value {
        JsonValue::Object(map) => {
            map.insert("runtime_dependency_warning".to_string(), warning);
            value
        }
        _ => json!({
            "result": value,
            "runtime_dependency_warning": warning,
        }),
    }
}

fn read_transcript_outputs(run_dir: &Path) -> anyhow::Result<Option<JsonValue>> {
    let path = run_dir.join("transcript.jsonl");
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path)?;
    let mut last = None;
    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<JsonValue>(line) else {
            continue;
        };
        let Some(outputs) = value.get("outputs") else {
            continue;
        };
        if !outputs.is_null() {
            last = Some(outputs.clone());
        }
    }
    Ok(last)
}

fn build_demo_host_config(tenant: &str) -> HostConfig {
    HostConfig {
        tenant: tenant.to_string(),
        bindings_path: PathBuf::from("<demo-provider>"),
        flow_type_bindings: HashMap::new(),
        rate_limits: RateLimits::default(),
        retry: FlowRetryConfig::default(),
        http_enabled: true,
        secrets_policy: SecretsPolicy::allow_all(),
        state_store_policy: StateStorePolicy::default(),
        webhook_policy: WebhookPolicy::default(),
        timers: Vec::new(),
        oauth: None,
        mocks: None,
        pack_bindings: Vec::new(),
        env_passthrough: Vec::new(),
        trace: TraceConfig::from_env(),
        validation: ValidationConfig::from_env(),
        operator_policy: OperatorPolicy::allow_all(),
    }
}

fn validate_runner_binary(path: PathBuf) -> Option<PathBuf> {
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() && runner_binary_is_executable(&metadata) => Some(path),
        Ok(metadata) => {
            let reason = if !metadata.is_file() {
                "not a regular file"
            } else {
                "not executable"
            };
            operator_log::warn(
                module_path!(),
                format!(
                    "runner binary '{}' is not usable ({})",
                    path.display(),
                    reason
                ),
            );
            None
        }
        Err(err) => {
            operator_log::warn(
                module_path!(),
                format!(
                    "runner binary '{}' cannot be accessed: {}",
                    path.display(),
                    err
                ),
            );
            None
        }
    }
}

fn pack_supports_provider_op_from_pack_bytes(
    pack_path: &Path,
    pack_bytes: &[u8],
    op_id: &str,
) -> anyhow::Result<bool> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(pack_bytes))?;
    let mut manifest_entry = archive.by_name("manifest.cbor").map_err(|err| {
        anyhow!(
            "failed to open manifest.cbor in {}: {err}",
            pack_path.display()
        )
    })?;
    let mut bytes = Vec::new();
    manifest_entry.read_to_end(&mut bytes)?;
    let manifest = decode_pack_manifest(&bytes)
        .context("failed to decode pack manifest for op support introspection")?;
    let Some(provider_ext) = manifest.provider_extension_inline() else {
        return Ok(false);
    };
    Ok(provider_ext
        .providers
        .iter()
        .any(|provider| provider.ops.iter().any(|op| op == op_id)))
}

#[cfg(unix)]
fn runner_binary_is_executable(metadata: &fs::Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn runner_binary_is_executable(_: &fs::Metadata) -> bool {
    true
}

fn payload_preview(bytes: &[u8]) -> String {
    const MAX_PREVIEW: usize = 256;
    if bytes.is_empty() {
        return "<empty>".to_string();
    }
    let preview_len = bytes.len().min(MAX_PREVIEW);
    if let Ok(text) = std::str::from_utf8(&bytes[..preview_len]) {
        if bytes.len() <= MAX_PREVIEW {
            text.to_string()
        } else {
            format!("{text}...")
        }
    } else {
        let encoded = general_purpose::STANDARD.encode(&bytes[..preview_len]);
        if bytes.len() <= MAX_PREVIEW {
            encoded
        } else {
            format!("{encoded}...")
        }
    }
}

fn extract_token_validation_request(payload_bytes: &[u8]) -> Option<JsonValue> {
    let payload: JsonValue = serde_json::from_slice(payload_bytes).ok()?;
    let token = extract_bearer_token(&payload)?;
    let mut request = serde_json::Map::new();
    request.insert("token".to_string(), JsonValue::String(token));
    if let Some(issuer) = first_string_at_paths(
        &payload,
        &["/token_validation/issuer", "/auth/issuer", "/issuer"],
    ) {
        request.insert("issuer".to_string(), JsonValue::String(issuer));
    }
    if let Some(audience) = first_value_at_paths(
        &payload,
        &["/token_validation/audience", "/auth/audience", "/audience"],
    ) {
        request.insert("audience".to_string(), normalize_string_or_array(audience));
    }
    if let Some(scopes) = first_value_at_paths(
        &payload,
        &[
            "/token_validation/scopes",
            "/token_validation/required_scopes",
            "/auth/scopes",
            "/auth/required_scopes",
            "/scopes",
        ],
    ) {
        request.insert("scopes".to_string(), normalize_string_or_array(scopes));
    }
    Some(JsonValue::Object(request))
}

fn extract_bearer_token(payload: &JsonValue) -> Option<String> {
    if let Some(value) = first_string_at_paths(
        payload,
        &[
            "/token_validation/token",
            "/auth/token",
            "/bearer_token",
            "/token",
            "/access_token",
            "/authorization",
        ],
    ) && let Some(token) = parse_bearer_value(&value)
    {
        return Some(token);
    }

    if let Some(headers) = payload.get("headers")
        && let Some(token) = extract_bearer_from_headers(headers)
    {
        return Some(token);
    }

    if let Some(value) = payload
        .pointer("/metadata/authorization")
        .and_then(JsonValue::as_str)
        && let Some(token) = parse_bearer_value(value)
    {
        return Some(token);
    }

    None
}

fn extract_bearer_from_headers(headers: &JsonValue) -> Option<String> {
    match headers {
        JsonValue::Object(map) => {
            for key in ["authorization", "Authorization"] {
                if let Some(value) = map.get(key).and_then(JsonValue::as_str)
                    && let Some(token) = parse_bearer_value(value)
                {
                    return Some(token);
                }
            }
            None
        }
        JsonValue::Array(values) => values.iter().find_map(|entry| {
            let name = entry
                .get("name")
                .or_else(|| entry.get("key"))
                .and_then(JsonValue::as_str)?;
            if !name.eq_ignore_ascii_case("authorization") {
                return None;
            }
            let value = entry.get("value").and_then(JsonValue::as_str)?;
            parse_bearer_value(value)
        }),
        _ => None,
    }
}

fn parse_bearer_value(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("Bearer ") {
        let token = rest.trim();
        if token.is_empty() {
            None
        } else {
            Some(token.to_string())
        }
    } else {
        Some(trimmed.to_string())
    }
}

fn first_string_at_paths(payload: &JsonValue, paths: &[&str]) -> Option<String> {
    paths
        .iter()
        .find_map(|path| payload.pointer(path).and_then(JsonValue::as_str))
        .map(str::to_string)
}

fn first_value_at_paths<'a>(payload: &'a JsonValue, paths: &[&str]) -> Option<&'a JsonValue> {
    paths.iter().find_map(|path| payload.pointer(path))
}

fn normalize_string_or_array(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::String(raw) => {
            let values = raw
                .split_whitespace()
                .filter(|entry| !entry.trim().is_empty())
                .map(|entry| JsonValue::String(entry.to_string()))
                .collect::<Vec<_>>();
            JsonValue::Array(values)
        }
        JsonValue::Array(items) => JsonValue::Array(
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(|item| JsonValue::String(item.to_string()))
                .collect(),
        ),
        _ => JsonValue::Array(Vec::new()),
    }
}

enum TokenValidationDecision {
    Allow(Option<JsonValue>),
    Deny(String),
}

fn evaluate_token_validation_output(output: &JsonValue) -> TokenValidationDecision {
    let valid = output
        .get("valid")
        .and_then(JsonValue::as_bool)
        .or_else(|| output.get("ok").and_then(JsonValue::as_bool))
        .unwrap_or(false);
    if !valid {
        let reason = output
            .get("reason")
            .and_then(JsonValue::as_str)
            .or_else(|| output.get("error").and_then(JsonValue::as_str))
            .unwrap_or("invalid bearer token");
        return TokenValidationDecision::Deny(reason.to_string());
    }
    let claims = output
        .get("claims")
        .filter(|value| value.is_object())
        .cloned()
        .or_else(|| {
            output
                .as_object()
                .is_some_and(|map| map.contains_key("sub"))
                .then(|| output.clone())
        });
    TokenValidationDecision::Allow(claims)
}

fn promote_runtime_state_tree(
    source_root: &Path,
    dest_root: &Path,
    active_bundle_id: &str,
    previous_bundle_id: &str,
) -> anyhow::Result<RuntimeStatePromotionReport> {
    if !source_root.exists() {
        return Ok(RuntimeStatePromotionReport {
            from_bundle_id: previous_bundle_id.to_string(),
            to_bundle_id: active_bundle_id.to_string(),
            copied_files: 0,
            rewritten_sessions: 0,
        });
    }
    let mut copied_files = 0usize;
    let mut rewritten_sessions = 0usize;
    for entry in std::fs::read_dir(source_root)? {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = dest_root.join(entry.file_name());
        if source_path.is_dir() {
            let nested = promote_runtime_state_tree(
                &source_path,
                &dest_path,
                active_bundle_id,
                previous_bundle_id,
            )?;
            copied_files += nested.copied_files;
            rewritten_sessions += nested.rewritten_sessions;
            continue;
        }
        if dest_path.exists() {
            continue;
        }
        if source_path
            .ancestors()
            .any(|ancestor| ancestor.file_name().and_then(|name| name.to_str()) == Some("sessions"))
        {
            let Some(mut record) = runtime_state::read_json::<SessionRecord>(&source_path)? else {
                continue;
            };
            record.bundle_assignment = Some(active_bundle_id.to_string());
            runtime_state::write_json(&dest_path, &record)?;
            copied_files += 1;
            rewritten_sessions += 1;
        } else {
            let bytes = std::fs::read(&source_path)?;
            runtime_state::atomic_write(&dest_path, &bytes)?;
            copied_files += 1;
        }
    }
    Ok(RuntimeStatePromotionReport {
        from_bundle_id: previous_bundle_id.to_string(),
        to_bundle_id: active_bundle_id.to_string(),
        copied_files,
        rewritten_sessions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_core::{RuntimeCapabilityRegistry, RuntimeSeams, RuntimeWiringPlan};
    use greentic_types::{
        ExtensionInline, ExtensionRef, PackId, PackKind, PackManifest, PackSignatures,
    };
    use semver::Version;
    use serde_json::json;
    use tempfile::tempdir;
    use zip::ZipWriter;
    use zip::write::FileOptions;

    fn write_test_provider_pack(path: &Path, pack_id: &str, capability_extension: JsonValue) {
        let mut extensions = BTreeMap::new();
        extensions.insert(
            "greentic.ext.capabilities.v1".to_string(),
            ExtensionRef {
                kind: "greentic.ext.capabilities.v1".to_string(),
                version: "1.0.0".to_string(),
                digest: None,
                location: None,
                inline: Some(ExtensionInline::Other(capability_extension)),
            },
        );
        extensions.insert(
            "greentic.ext.offers.v1".to_string(),
            ExtensionRef {
                kind: "greentic.ext.offers.v1".to_string(),
                version: "1.0.0".to_string(),
                digest: None,
                location: None,
                inline: Some(ExtensionInline::Other(json!({"offers": []}))),
            },
        );

        let manifest = PackManifest {
            schema_version: "pack-v1".into(),
            pack_id: PackId::new(pack_id).expect("pack id"),
            name: None,
            version: Version::parse("0.1.0").expect("version"),
            kind: PackKind::Provider,
            publisher: "test".into(),
            components: Vec::new(),
            flows: Vec::new(),
            dependencies: Vec::new(),
            capabilities: Vec::new(),
            secret_requirements: Vec::new(),
            signatures: PackSignatures::default(),
            bootstrap: None,
            extensions: Some(extensions),
        };
        let file = std::fs::File::create(path).expect("create gtpack");
        let mut zip = ZipWriter::new(file);
        zip.start_file("manifest.cbor", FileOptions::<()>::default())
            .expect("start manifest");
        let encoded = greentic_types::encode_pack_manifest(&manifest).expect("encode manifest");
        std::io::Write::write_all(&mut zip, &encoded).expect("write manifest");
        zip.finish().expect("finish zip");
    }

    #[test]
    fn token_validation_request_extracts_bearer_and_requirements() {
        let payload = json!({
            "headers": {
                "Authorization": "Bearer token-123"
            },
            "token_validation": {
                "issuer": "https://issuer.example",
                "audience": ["api://svc"],
                "required_scopes": "read write"
            }
        });
        let request =
            extract_token_validation_request(&serde_json::to_vec(&payload).expect("payload bytes"))
                .expect("request");
        assert_eq!(
            request.pointer("/token").and_then(JsonValue::as_str),
            Some("token-123")
        );
        assert_eq!(
            request.pointer("/issuer").and_then(JsonValue::as_str),
            Some("https://issuer.example")
        );
        assert_eq!(
            request.pointer("/audience/0").and_then(JsonValue::as_str),
            Some("api://svc")
        );
        assert_eq!(
            request.pointer("/scopes/0").and_then(JsonValue::as_str),
            Some("read")
        );
        assert_eq!(
            request.pointer("/scopes/1").and_then(JsonValue::as_str),
            Some("write")
        );
    }

    #[test]
    fn token_validation_output_deny_when_invalid() {
        let output = json!({
            "valid": false,
            "reason": "issuer mismatch"
        });
        match evaluate_token_validation_output(&output) {
            TokenValidationDecision::Deny(reason) => {
                assert_eq!(reason, "issuer mismatch");
            }
            TokenValidationDecision::Allow(_) => panic!("expected deny"),
        }
    }

    #[test]
    fn token_validation_output_allows_and_returns_claims() {
        let output = json!({
            "valid": true,
            "claims": {
                "sub": "user-1",
                "scope": "read write",
                "aud": ["api://svc"]
            }
        });
        match evaluate_token_validation_output(&output) {
            TokenValidationDecision::Allow(Some(claims)) => {
                assert_eq!(
                    claims.pointer("/sub").and_then(JsonValue::as_str),
                    Some("user-1")
                );
            }
            TokenValidationDecision::Allow(None) => panic!("expected claims"),
            TokenValidationDecision::Deny(reason) => panic!("unexpected deny: {reason}"),
        }
    }

    #[test]
    fn runtime_role_payload_includes_selected_roles_and_seam_flags() {
        let core = RuntimeCore::new(
            RuntimeCapabilityRegistry::default(),
            RuntimeSeams::default(),
            RuntimeWiringPlan {
                selected_providers: BTreeMap::from([(
                    "state".to_string(),
                    crate::runtime_core::RuntimeSelectedProvider {
                        role_id: "state".to_string(),
                        capability_id: "greentic.cap.state.provider.v1".to_string(),
                        contract_id: "greentic.contract.state.v1".to_string(),
                        provider_pack: "state.pack".to_string(),
                        pack_path: PathBuf::from("/tmp/state.gtpack"),
                        entrypoint: "state.dispatch".to_string(),
                        component_ref: "state.component".to_string(),
                        stable_id: "state.pack::default".to_string(),
                    },
                )]),
                warnings: vec!["missing optional telemetry provider".to_string()],
                ..RuntimeWiringPlan::default()
            },
        );

        let payload = runtime_role_payload(&core);
        assert_eq!(
            payload
                .pointer("/selected/state/pack_id")
                .and_then(JsonValue::as_str),
            Some("state.pack")
        );
        assert_eq!(
            payload
                .pointer("/selected/state/contract_id")
                .and_then(JsonValue::as_str),
            Some("greentic.contract.state.v1")
        );
        assert_eq!(
            payload.pointer("/seams/state").and_then(JsonValue::as_bool),
            Some(false)
        );
        assert_eq!(
            payload.pointer("/warnings/0").and_then(JsonValue::as_str),
            Some("missing optional telemetry provider")
        );
    }

    fn test_runtime_identity(bundle_id: &str) -> ActiveRuntimeIdentity {
        ActiveRuntimeIdentity::new(bundle_id.to_string())
    }

    #[test]
    fn local_runtime_state_provider_roundtrips_json() {
        let tmp = tempdir().expect("tempdir");
        let provider =
            LocalRuntimeStateProvider::new(tmp.path().to_path_buf(), test_runtime_identity("a"));
        let key = ScopedStateKey {
            tenant: "tenant-a".to_string(),
            team: Some("team-a".to_string()),
            scope: "runtime_events".to_string(),
            key: "runtime.pre_op".to_string(),
        };
        let value = json!({"ok": true});
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

        runtime
            .block_on(provider.put(&key, value.clone()))
            .expect("put state");
        let loaded = runtime
            .block_on(provider.get(&key))
            .expect("get state")
            .expect("stored value");
        assert_eq!(loaded, value);

        runtime
            .block_on(provider.delete(&key))
            .expect("delete state");
        assert!(
            runtime
                .block_on(provider.get(&key))
                .expect("get after delete")
                .is_none()
        );
    }

    #[test]
    fn state_provider_store_adapter_roundtrips_runner_state() {
        let tmp = tempdir().expect("tempdir");
        let store = StateProviderStoreAdapter::new(Arc::new(LocalRuntimeStateProvider::new(
            tmp.path().to_path_buf(),
            test_runtime_identity("a"),
        )));
        let tenant = greentic_types::TenantCtx::new(
            greentic_types::EnvId::try_from("dev").expect("env"),
            greentic_types::TenantId::try_from("tenant-a").expect("tenant"),
        )
        .with_team(Some(
            greentic_types::TeamId::try_from("team-a").expect("team"),
        ));
        let key = greentic_types::StateKey::new("flow/demo");
        let value = json!({"ok": true});

        store
            .set_json(&tenant, "runner", &key, None, &value, None)
            .expect("set runner state");
        let loaded = store
            .get_json(&tenant, "runner", &key, None)
            .expect("get runner state")
            .expect("stored state");
        assert_eq!(loaded, value);

        let deleted = store.del(&tenant, "runner", &key).expect("delete");
        assert!(deleted);
        assert!(
            store
                .get_json(&tenant, "runner", &key, None)
                .expect("get after delete")
                .is_none()
        );
    }

    #[test]
    fn state_provider_store_adapter_retries_compare_and_set_on_conflict() {
        let provider = Arc::new(FlakyCompareAndSetStateProvider::new());
        let store = StateProviderStoreAdapter::new(provider.clone());
        let tenant = greentic_types::TenantCtx::new(
            greentic_types::EnvId::try_from("dev").expect("env"),
            greentic_types::TenantId::try_from("tenant-a").expect("tenant"),
        )
        .with_team(Some(
            greentic_types::TeamId::try_from("team-a").expect("team"),
        ));
        let key = greentic_types::StateKey::new("flow/demo");

        store
            .set_json(
                &tenant,
                "runner",
                &key,
                None,
                &json!({"existing": true}),
                None,
            )
            .expect("seed state");
        *provider
            .compare_and_set_calls
            .lock()
            .expect("state compare-and-set call lock") = 0;
        store
            .set_json(
                &tenant,
                "runner",
                &key,
                None,
                &json!({"existing": true, "other": 2}),
                None,
            )
            .expect("retrying state update");

        let loaded = store
            .get_json(&tenant, "runner", &key, None)
            .expect("get merged state")
            .expect("merged state");
        assert_eq!(loaded, json!({"existing": true, "other": 2}));
        assert_eq!(
            *provider
                .compare_and_set_calls
                .lock()
                .expect("state compare-and-set call lock"),
            2
        );
    }

    #[test]
    fn state_provider_store_adapter_falls_back_when_compare_and_set_is_unsupported() {
        let provider = Arc::new(CompareAndSetUnsupportedStateProvider::new());
        let store = StateProviderStoreAdapter::new(provider.clone());
        let tenant = greentic_types::TenantCtx::new(
            greentic_types::EnvId::try_from("dev").expect("env"),
            greentic_types::TenantId::try_from("tenant-a").expect("tenant"),
        )
        .with_team(Some(
            greentic_types::TeamId::try_from("team-a").expect("team"),
        ));
        let key = greentic_types::StateKey::new("flow/demo");

        store
            .set_json(&tenant, "runner", &key, None, &json!({"ok": true}), None)
            .expect("set state with fallback put");

        let loaded = store
            .get_json(&tenant, "runner", &key, None)
            .expect("get stored state")
            .expect("stored state");
        assert_eq!(loaded, json!({"ok": true}));
        assert_eq!(
            *provider
                .compare_and_set_calls
                .lock()
                .expect("compare-and-set call lock"),
            1
        );
        assert_eq!(*provider.put_calls.lock().expect("put call lock"), 1);
    }

    #[test]
    #[allow(deprecated)]
    fn session_provider_store_adapter_supports_lookup_and_waits() {
        let tmp = tempdir().expect("tempdir");
        let store = SessionProviderStoreAdapter::new(Arc::new(LocalRuntimeSessionProvider::new(
            tmp.path().to_path_buf(),
            test_runtime_identity("a"),
        )));
        let tenant = greentic_types::TenantCtx::new(
            greentic_types::EnvId::try_from("dev").expect("env"),
            greentic_types::TenantId::try_from("tenant-a").expect("tenant"),
        )
        .with_team(Some(
            greentic_types::TeamId::try_from("team-a").expect("team"),
        ))
        .with_user(Some(
            greentic_types::UserId::try_from("user-a").expect("user"),
        ));
        let user = tenant.user_id.clone().expect("user set");
        let data = greentic_types::SessionData {
            tenant_ctx: tenant.clone(),
            flow_id: greentic_types::FlowId::try_from("flow-demo").expect("flow"),
            pack_id: Some(greentic_types::PackId::try_from("pack-demo").expect("pack")),
            cursor: greentic_types::SessionCursor::new("node-1"),
            context_json: "{\"ok\":true}".to_string(),
        };
        let scope = greentic_types::ReplyScope {
            conversation: "conv-1".to_string(),
            thread: Some("thread-1".to_string()),
            reply_to: None,
            correlation: None,
        };

        let session_key = store
            .create_session(&tenant, data.clone())
            .expect("create session");
        let loaded = store
            .get_session(&session_key)
            .expect("get session")
            .expect("stored session");
        assert_eq!(loaded.context_json, data.context_json);

        store
            .register_wait(&tenant, &user, &scope, &session_key, data.clone(), None)
            .expect("register wait");
        let found = store
            .find_wait_by_scope(&tenant, &user, &scope)
            .expect("find wait");
        assert_eq!(found, Some(session_key.clone()));

        let by_user = store.find_by_user(&tenant, &user).expect("find by user");
        assert_eq!(by_user.expect("session").0, session_key);
    }

    #[test]
    #[allow(deprecated)]
    fn session_provider_store_adapter_reset_clears_runtime_indexes() {
        let tmp = tempdir().expect("tempdir");
        let store = SessionProviderStoreAdapter::new(Arc::new(LocalRuntimeSessionProvider::new(
            tmp.path().to_path_buf(),
            test_runtime_identity("a"),
        )));
        let tenant = greentic_types::TenantCtx::new(
            greentic_types::EnvId::try_from("dev").expect("env"),
            greentic_types::TenantId::try_from("tenant-a").expect("tenant"),
        )
        .with_team(Some(
            greentic_types::TeamId::try_from("team-a").expect("team"),
        ))
        .with_user(Some(
            greentic_types::UserId::try_from("user-a").expect("user"),
        ));
        let user = tenant.user_id.clone().expect("user set");
        let data = greentic_types::SessionData {
            tenant_ctx: tenant.clone(),
            flow_id: greentic_types::FlowId::try_from("flow-demo").expect("flow"),
            pack_id: Some(greentic_types::PackId::try_from("pack-demo").expect("pack")),
            cursor: greentic_types::SessionCursor::new("node-1"),
            context_json: "{\"ok\":true}".to_string(),
        };
        let scope = greentic_types::ReplyScope {
            conversation: "conv-1".to_string(),
            thread: Some("thread-1".to_string()),
            reply_to: None,
            correlation: None,
        };

        let session_key = store
            .create_session(&tenant, data.clone())
            .expect("create session");
        store
            .register_wait(&tenant, &user, &scope, &session_key, data, None)
            .expect("register wait");
        assert!(
            store
                .find_wait_by_scope(&tenant, &user, &scope)
                .expect("find wait before reset")
                .is_some()
        );

        let report = store.reset_runtime_indexes();
        assert_eq!(report.session_locations, 1);
        assert_eq!(report.user_sessions, 1);
        assert_eq!(report.user_wait_entries, 1);
        assert_eq!(report.scope_entries, 1);

        assert_eq!(
            store
                .find_wait_by_scope(&tenant, &user, &scope)
                .expect("find wait after reset"),
            Some(session_key.clone())
        );
        let loaded = store
            .get_session(&session_key)
            .expect("get session after reset")
            .expect("session still recoverable from provider");
        assert_eq!(loaded.context_json, "{\"ok\":true}");
    }

    #[test]
    #[allow(deprecated)]
    fn session_provider_store_adapter_recovers_session_after_restart() {
        let tmp = tempdir().expect("tempdir");
        let provider = Arc::new(LocalRuntimeSessionProvider::new(
            tmp.path().to_path_buf(),
            test_runtime_identity("a"),
        ));
        let first = SessionProviderStoreAdapter::new(provider.clone());
        let tenant = greentic_types::TenantCtx::new(
            greentic_types::EnvId::try_from("dev").expect("env"),
            greentic_types::TenantId::try_from("tenant-a").expect("tenant"),
        )
        .with_team(Some(
            greentic_types::TeamId::try_from("team-a").expect("team"),
        ))
        .with_user(Some(
            greentic_types::UserId::try_from("user-a").expect("user"),
        ));
        let data = greentic_types::SessionData {
            tenant_ctx: tenant.clone(),
            flow_id: greentic_types::FlowId::try_from("flow-demo").expect("flow"),
            pack_id: Some(greentic_types::PackId::try_from("pack-demo").expect("pack")),
            cursor: greentic_types::SessionCursor::new("node-1"),
            context_json: "{\"ok\":true}".to_string(),
        };
        let user = tenant.user_id.clone().expect("user set");
        let scope = greentic_types::ReplyScope {
            conversation: "conv-1".to_string(),
            thread: Some("thread-1".to_string()),
            reply_to: None,
            correlation: None,
        };

        let session_key = first
            .create_session(&tenant, data.clone())
            .expect("create session");
        first
            .register_wait(&tenant, &user, &scope, &session_key, data.clone(), None)
            .expect("register wait");

        let restarted = SessionProviderStoreAdapter::new(provider);
        let loaded = restarted
            .get_session(&session_key)
            .expect("get session after restart")
            .expect("stored session");
        assert_eq!(loaded.context_json, data.context_json);
        assert_eq!(
            restarted
                .find_wait_by_scope(&tenant, &user, &scope)
                .expect("find wait after restart"),
            Some(session_key.clone())
        );
        assert_eq!(
            restarted
                .find_by_user(&tenant, &user)
                .expect("find by user after restart")
                .map(|(key, _)| key),
            Some(session_key.clone())
        );

        let updated = greentic_types::SessionData {
            context_json: "{\"ok\":\"updated\"}".to_string(),
            ..data
        };
        restarted
            .update_session(&session_key, updated.clone())
            .expect("update session after restart");
        let loaded = restarted
            .get_session(&session_key)
            .expect("get updated session")
            .expect("updated session");
        assert_eq!(loaded.context_json, updated.context_json);

        restarted
            .remove_session(&session_key)
            .expect("remove session after restart");
        assert!(
            restarted
                .get_session(&session_key)
                .expect("get removed session")
                .is_none()
        );
    }

    #[test]
    #[allow(deprecated)]
    fn session_provider_store_adapter_retries_compare_and_set_on_conflict() {
        let provider = Arc::new(FlakyCompareAndSetSessionProvider::new());
        let store = SessionProviderStoreAdapter::new(provider.clone());
        let tenant = greentic_types::TenantCtx::new(
            greentic_types::EnvId::try_from("dev").expect("env"),
            greentic_types::TenantId::try_from("tenant-a").expect("tenant"),
        )
        .with_team(Some(
            greentic_types::TeamId::try_from("team-a").expect("team"),
        ))
        .with_user(Some(
            greentic_types::UserId::try_from("user-a").expect("user"),
        ));
        let data = greentic_types::SessionData {
            tenant_ctx: tenant.clone(),
            flow_id: greentic_types::FlowId::try_from("flow-demo").expect("flow"),
            pack_id: Some(greentic_types::PackId::try_from("pack-demo").expect("pack")),
            cursor: greentic_types::SessionCursor::new("node-1"),
            context_json: "{\"ok\":true}".to_string(),
        };

        let session_key = store
            .create_session(&tenant, data.clone())
            .expect("create session");
        let updated = greentic_types::SessionData {
            context_json: "{\"ok\":\"updated\"}".to_string(),
            ..data
        };
        store
            .update_session(&session_key, updated.clone())
            .expect("update session with retry");

        let loaded = store
            .get_session(&session_key)
            .expect("get updated session")
            .expect("updated session");
        assert_eq!(loaded.context_json, updated.context_json);
        assert_eq!(
            *provider
                .compare_and_set_calls
                .lock()
                .expect("compare-and-set call lock"),
            2
        );
    }

    #[test]
    fn local_runtime_session_provider_roundtrips_records() {
        let tmp = tempdir().expect("tempdir");
        let provider =
            LocalRuntimeSessionProvider::new(tmp.path().to_path_buf(), test_runtime_identity("a"));
        let key = RuntimeSessionKey {
            tenant: "tenant-a".to_string(),
            team: Some("team-a".to_string()),
            session_id: "session-1".to_string(),
        };
        let record = SessionRecord {
            revision: 1,
            route: Some("cap.invoke:test".to_string()),
            bundle_assignment: Some("/tmp/bundle".to_string()),
            context: json!({"ok": true}),
            expires_at_unix_ms: None,
        };
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

        runtime
            .block_on(provider.put(&key, record.clone()))
            .expect("put session");
        let loaded = runtime
            .block_on(provider.get(&key))
            .expect("get session")
            .expect("stored record");
        assert_eq!(loaded, record);

        let updated = SessionRecord {
            revision: 2,
            route: Some("cap.invoke:test".to_string()),
            bundle_assignment: Some("/tmp/bundle".to_string()),
            context: json!({"ok": "updated"}),
            expires_at_unix_ms: None,
        };
        assert!(
            runtime
                .block_on(provider.compare_and_set(&key, 1, updated.clone()))
                .expect("cas")
        );
        let loaded = runtime
            .block_on(provider.get(&key))
            .expect("get updated")
            .expect("updated record");
        assert_eq!(loaded, updated);

        runtime
            .block_on(provider.delete(&key))
            .expect("delete session");
        assert!(
            runtime
                .block_on(provider.get(&key))
                .expect("get after delete")
                .is_none()
        );
    }

    #[test]
    fn local_runtime_providers_namespace_state_by_active_bundle() {
        let tmp = tempdir().expect("tempdir");
        let identity = test_runtime_identity("bundle-a");
        let state_provider =
            LocalRuntimeStateProvider::new(tmp.path().to_path_buf(), identity.clone());
        let session_provider =
            LocalRuntimeSessionProvider::new(tmp.path().to_path_buf(), identity.clone());
        let state_key = ScopedStateKey {
            tenant: "tenant-a".to_string(),
            team: Some("team-a".to_string()),
            scope: "runtime_events".to_string(),
            key: "runtime.pre_op".to_string(),
        };
        let session_key = RuntimeSessionKey {
            tenant: "tenant-a".to_string(),
            team: Some("team-a".to_string()),
            session_id: "session-1".to_string(),
        };
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

        runtime
            .block_on(state_provider.put(&state_key, json!({"bundle": "a"})))
            .expect("put state a");
        runtime
            .block_on(session_provider.put(
                &session_key,
                SessionRecord {
                    revision: 1,
                    route: Some("cap.invoke:test".to_string()),
                    bundle_assignment: Some("bundle-a".to_string()),
                    context: json!({"bundle": "a"}),
                    expires_at_unix_ms: None,
                },
            ))
            .expect("put session a");

        identity.replace_bundle_id("bundle-b".to_string());

        assert!(
            runtime
                .block_on(state_provider.get(&state_key))
                .expect("get state b")
                .is_none()
        );
        assert!(
            runtime
                .block_on(session_provider.get(&session_key))
                .expect("get session b")
                .is_none()
        );

        assert!(
            tmp.path()
                .join("state/runtime/bundles/bundle-a/tenant-a/team-a/provider-state/runtime_events/runtime.pre_op.json")
                .exists()
        );
        assert!(
            tmp.path()
                .join("state/runtime/bundles/bundle-a/tenant-a/team-a/sessions/session-1.json")
                .exists()
        );
    }

    #[test]
    fn complete_bundle_drain_removes_bundle_runtime_state_root() {
        let tmp = tempdir().expect("tempdir");
        let bundle_a = tmp.path().join("bundle-a");
        let bundle_b = tmp.path().join("bundle-b");
        let providers_a = bundle_a.join("providers").join("messaging");
        let providers_b = bundle_b.join("providers").join("messaging");
        std::fs::create_dir_all(&providers_a).expect("providers a");
        std::fs::create_dir_all(&providers_b).expect("providers b");
        write_test_provider_pack(
            &providers_a.join("session-a.gtpack"),
            "session.a",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.a"},
                    "priority": 10
                }]
            }),
        );
        write_test_provider_pack(
            &providers_b.join("session-b.gtpack"),
            "session.b",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.b"},
                    "priority": 10
                }]
            }),
        );

        let discovery = crate::discovery::discover(&bundle_a).expect("discover bundle a");
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(&bundle_a, "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(bundle_a.clone(), &discovery, None, secrets_handle, false)
            .expect("build host");

        let retired_bundle_id = host.bundle_runtime_id();
        let retired_state_root =
            state_layout::runtime_bundle_state_root(&bundle_a, &retired_bundle_id);
        std::fs::create_dir_all(retired_state_root.join("tenant-a/team-a/sessions"))
            .expect("create runtime state root");
        runtime_state::write_json(
            &retired_state_root.join("tenant-a/team-a/sessions/session-1.json"),
            &SessionRecord {
                revision: 1,
                route: Some("cap.invoke:test".to_string()),
                bundle_assignment: Some(retired_bundle_id.clone()),
                context: json!({"ok": true}),
                expires_at_unix_ms: None,
            },
        )
        .expect("write runtime state");

        let active_bundle_b = host
            .activate_bundle_ref(&bundle_b)
            .expect("activate bundle b");
        assert_ne!(active_bundle_b, retired_bundle_id);
        assert!(retired_state_root.exists());

        host.complete_bundle_drain(&retired_bundle_id)
            .expect("retire drained bundle");

        assert!(!retired_state_root.exists());
        let snapshot = host.runtime_status_snapshot();
        assert_eq!(
            snapshot
                .pointer("/runtime_state/last_cleanup/bundle_id")
                .and_then(JsonValue::as_str),
            Some(retired_bundle_id.as_str())
        );
        assert_eq!(
            snapshot
                .pointer("/runtime_state/last_cleanup/removed")
                .and_then(JsonValue::as_bool),
            Some(true)
        );
    }

    #[test]
    fn activating_bundle_ref_promotes_previous_runtime_state() {
        let tmp = tempdir().expect("tempdir");
        let bundle_a = tmp.path().join("bundle-a");
        let bundle_b = tmp.path().join("bundle-b");
        let providers_a = bundle_a.join("providers").join("messaging");
        let providers_b = bundle_b.join("providers").join("messaging");
        std::fs::create_dir_all(&providers_a).expect("providers a");
        std::fs::create_dir_all(&providers_b).expect("providers b");
        write_test_provider_pack(
            &providers_a.join("session-a.gtpack"),
            "session.a",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.a"},
                    "priority": 10
                }]
            }),
        );
        write_test_provider_pack(
            &providers_b.join("session-b.gtpack"),
            "session.b",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.b"},
                    "priority": 10
                }]
            }),
        );

        let discovery = crate::discovery::discover(&bundle_a).expect("discover bundle a");
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(&bundle_a, "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(bundle_a.clone(), &discovery, None, secrets_handle, false)
            .expect("build host");

        let previous_bundle_id = host.bundle_runtime_id();
        let previous_state_root =
            state_layout::runtime_bundle_state_root(&bundle_a, &previous_bundle_id);
        let session_path = previous_state_root.join("tenant-a/team-a/sessions/session-1.json");
        let event_path = previous_state_root
            .join("tenant-a/team-a/provider-state/runtime_events/runtime.pre_op.json");
        runtime_state::write_json(
            &session_path,
            &SessionRecord {
                revision: 1,
                route: Some("cap.invoke:test".to_string()),
                bundle_assignment: Some(previous_bundle_id.clone()),
                context: json!({"ok": true}),
                expires_at_unix_ms: None,
            },
        )
        .expect("write session");
        runtime_state::write_json(&event_path, &json!({"event_type": "runtime.pre_op"}))
            .expect("write event");

        let active_bundle_id = host
            .activate_bundle_ref(&bundle_b)
            .expect("activate bundle b");
        let promoted_root = state_layout::runtime_bundle_state_root(&bundle_a, &active_bundle_id);

        let promoted_session = runtime_state::read_json::<SessionRecord>(
            &promoted_root.join("tenant-a/team-a/sessions/session-1.json"),
        )
        .expect("read promoted session")
        .expect("promoted session");
        assert_eq!(
            promoted_session.bundle_assignment.as_deref(),
            Some(active_bundle_id.as_str())
        );
        assert!(
            promoted_root
                .join("tenant-a/team-a/provider-state/runtime_events/runtime.pre_op.json")
                .exists()
        );
        let snapshot = host.runtime_status_snapshot();
        assert_eq!(
            snapshot
                .pointer("/runtime_state/last_promotion/from_bundle_id")
                .and_then(JsonValue::as_str),
            Some(previous_bundle_id.as_str())
        );
        assert_eq!(
            snapshot
                .pointer("/runtime_state/last_promotion/to_bundle_id")
                .and_then(JsonValue::as_str),
            Some(active_bundle_id.as_str())
        );
        assert_eq!(
            snapshot
                .pointer("/runtime_state/last_promotion/rewritten_sessions")
                .and_then(JsonValue::as_u64),
            Some(1)
        );
        assert_eq!(
            snapshot
                .pointer("/runtime_state/last_session_index_reset/session_locations")
                .and_then(JsonValue::as_u64),
            Some(0)
        );
    }

    #[test]
    #[allow(deprecated)]
    fn activating_bundle_ref_reports_session_index_reset_counts() {
        let tmp = tempdir().expect("tempdir");
        let bundle_a = tmp.path().join("bundle-a");
        let bundle_b = tmp.path().join("bundle-b");
        let providers_a = bundle_a.join("providers").join("messaging");
        let providers_b = bundle_b.join("providers").join("messaging");
        std::fs::create_dir_all(&providers_a).expect("providers a");
        std::fs::create_dir_all(&providers_b).expect("providers b");
        write_test_provider_pack(
            &providers_a.join("session-a.gtpack"),
            "session.a",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.a"},
                    "priority": 10
                }]
            }),
        );
        write_test_provider_pack(
            &providers_b.join("session-b.gtpack"),
            "session.b",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.b"},
                    "priority": 10
                }]
            }),
        );

        let discovery = crate::discovery::discover(&bundle_a).expect("discover bundle a");
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(&bundle_a, "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(bundle_a.clone(), &discovery, None, secrets_handle, false)
            .expect("build host");

        let store = Arc::clone(
            host.session_store_adapter
                .as_ref()
                .expect("session store adapter"),
        );
        let tenant = greentic_types::TenantCtx::new(
            greentic_types::EnvId::try_from("dev").expect("env"),
            greentic_types::TenantId::try_from("tenant-a").expect("tenant"),
        )
        .with_team(Some(
            greentic_types::TeamId::try_from("team-a").expect("team"),
        ))
        .with_user(Some(
            greentic_types::UserId::try_from("user-a").expect("user"),
        ));
        let user = tenant.user_id.clone().expect("user set");
        let data = greentic_types::SessionData {
            tenant_ctx: tenant.clone(),
            flow_id: greentic_types::FlowId::try_from("flow-demo").expect("flow"),
            pack_id: Some(greentic_types::PackId::try_from("pack-demo").expect("pack")),
            cursor: greentic_types::SessionCursor::new("node-1"),
            context_json: "{\"ok\":true}".to_string(),
        };
        let scope = greentic_types::ReplyScope {
            conversation: "conv-1".to_string(),
            thread: Some("thread-1".to_string()),
            reply_to: None,
            correlation: None,
        };
        let session_key = store
            .create_session(&tenant, data.clone())
            .expect("create session");
        store
            .register_wait(&tenant, &user, &scope, &session_key, data, None)
            .expect("register wait");

        host.activate_bundle_ref(&bundle_b)
            .expect("activate bundle b");

        assert_eq!(
            store
                .find_wait_by_scope(&tenant, &user, &scope)
                .expect("find wait after activation"),
            Some(session_key.clone())
        );
        let snapshot = host.runtime_status_snapshot();
        assert_eq!(
            snapshot
                .pointer("/runtime_state/last_session_index_reset/session_locations")
                .and_then(JsonValue::as_u64),
            Some(1)
        );
        assert_eq!(
            snapshot
                .pointer("/runtime_state/last_session_index_reset/user_sessions")
                .and_then(JsonValue::as_u64),
            Some(1)
        );
        assert_eq!(
            snapshot
                .pointer("/runtime_state/last_session_index_reset/user_wait_entries")
                .and_then(JsonValue::as_u64),
            Some(1)
        );
        assert_eq!(
            snapshot
                .pointer("/runtime_state/last_session_index_reset/scope_entries")
                .and_then(JsonValue::as_u64),
            Some(1)
        );
    }

    #[test]
    fn local_admin_authorization_hook_allows_actions() {
        let hook = LocalAdminAuthorizationHook;
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let decision = runtime
            .block_on(hook.authorize(&AdminAction {
                action: "onboard.get./status".to_string(),
                actor: "onboard_api".to_string(),
                resource: Some("/api/onboard/status".to_string()),
            }))
            .expect("authorize");
        assert_eq!(decision, AuthorizationDecision::Allow);
    }

    #[test]
    fn authorize_admin_action_emits_audit_event() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let telemetry = Arc::new(RecordingTelemetryProvider::default());
        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.telemetry_provider = Some(telemetry.clone());
        seams.observer_sink = Some(observer.clone());
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let decision = host
            .authorize_admin_action(AdminAction {
                action: "onboard.get./status".to_string(),
                actor: "onboard_api".to_string(),
                resource: Some("/api/onboard/status".to_string()),
            })
            .expect("authorize action");

        assert_eq!(decision, AuthorizationDecision::Allow);
        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let observer_events = observer.events.lock().expect("observer events");
        let event = telemetry_events.last().expect("admin audit event");
        assert_eq!(observer_events.last(), Some(event));
        assert_eq!(event.event_type, "admin.action");
        assert_eq!(event.severity, "info");
        assert_eq!(event.outcome.as_deref(), Some("allow"));
        assert_eq!(event.reason_codes, vec!["admin.action".to_string()]);
        assert_eq!(
            event.payload.get("action").and_then(JsonValue::as_str),
            Some("onboard.get./status")
        );
        assert!(event.ts_unix_ms > 0);
    }

    #[test]
    fn activate_bundle_ref_emits_lifecycle_event() {
        let tmp = tempdir().expect("tempdir");
        let bundle_a = tmp.path().join("bundle-a");
        let bundle_b = tmp.path().join("bundle-b");
        let providers_a = bundle_a.join("providers").join("messaging");
        let providers_b = bundle_b.join("providers").join("messaging");
        std::fs::create_dir_all(&providers_a).expect("providers a");
        std::fs::create_dir_all(&providers_b).expect("providers b");
        write_test_provider_pack(
            &providers_a.join("session-a.gtpack"),
            "session.a",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.a"},
                    "priority": 10
                }]
            }),
        );
        write_test_provider_pack(
            &providers_b.join("session-b.gtpack"),
            "session.b",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.b"},
                    "priority": 10
                }]
            }),
        );

        let discovery = crate::discovery::discover(&bundle_a).expect("discover bundle a");
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(&bundle_a, "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(bundle_a.clone(), &discovery, None, secrets_handle, false)
            .expect("build host");

        let telemetry = Arc::new(RecordingTelemetryProvider::default());
        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.telemetry_provider = Some(telemetry.clone());
        seams.observer_sink = Some(observer.clone());
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let bundle_id = host
            .activate_bundle_ref(&bundle_b)
            .expect("activate bundle b");

        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let observer_events = observer.events.lock().expect("observer events");
        let event = telemetry_events.last().expect("lifecycle event");
        assert_eq!(observer_events.last(), Some(event));
        assert_eq!(event.event_type, "bundle.lifecycle.activate");
        assert_eq!(event.outcome.as_deref(), Some("active"));
        assert_eq!(event.severity, "info");
        assert_eq!(
            event.payload.get("bundle_id").and_then(JsonValue::as_str),
            Some(bundle_id.as_str())
        );
    }

    struct UnavailableSessionProvider;

    struct UnavailableStateProvider;

    struct DegradedSessionProvider;

    struct DegradedStateProvider;

    struct FlakyCompareAndSetSessionProvider {
        current: Mutex<Option<SessionRecord>>,
        compare_and_set_calls: Mutex<usize>,
    }

    impl FlakyCompareAndSetSessionProvider {
        fn new() -> Self {
            Self {
                current: Mutex::new(None),
                compare_and_set_calls: Mutex::new(0),
            }
        }
    }

    struct FlakyCompareAndSetStateProvider {
        current: Mutex<Option<JsonValue>>,
        compare_and_set_calls: Mutex<usize>,
    }

    impl FlakyCompareAndSetStateProvider {
        fn new() -> Self {
            Self {
                current: Mutex::new(None),
                compare_and_set_calls: Mutex::new(0),
            }
        }
    }

    struct CompareAndSetUnsupportedStateProvider {
        current: Mutex<Option<JsonValue>>,
        put_calls: Mutex<usize>,
        compare_and_set_calls: Mutex<usize>,
    }

    impl CompareAndSetUnsupportedStateProvider {
        fn new() -> Self {
            Self {
                current: Mutex::new(None),
                put_calls: Mutex::new(0),
                compare_and_set_calls: Mutex::new(0),
            }
        }
    }

    #[derive(Default)]
    struct RecordingTelemetryProvider {
        events: Mutex<Vec<RuntimeEvent>>,
    }

    #[async_trait]
    impl crate::runtime_core::TelemetryProvider for RecordingTelemetryProvider {
        async fn emit(&self, event: RuntimeEvent) -> anyhow::Result<()> {
            self.events
                .lock()
                .expect("recording telemetry provider lock poisoned")
                .push(event);
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    #[derive(Default)]
    struct RecordingObserverSink {
        events: Mutex<Vec<RuntimeEvent>>,
    }

    #[async_trait]
    impl crate::runtime_core::ObserverHookSink for RecordingObserverSink {
        async fn publish(&self, event: RuntimeEvent) -> anyhow::Result<()> {
            self.events
                .lock()
                .expect("recording observer sink lock poisoned")
                .push(event);
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    struct FailingTelemetryProvider;

    #[async_trait]
    impl crate::runtime_core::TelemetryProvider for FailingTelemetryProvider {
        async fn emit(&self, _event: RuntimeEvent) -> anyhow::Result<()> {
            anyhow::bail!("telemetry sink failed");
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    struct SlowTelemetryProvider;

    #[async_trait]
    impl crate::runtime_core::TelemetryProvider for SlowTelemetryProvider {
        async fn emit(&self, _event: RuntimeEvent) -> anyhow::Result<()> {
            tokio::time::sleep(DemoRunnerHost::EVENT_DELIVERY_TIMEOUT + Duration::from_millis(50))
                .await;
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    struct UnavailableTelemetryProvider;

    #[async_trait]
    impl crate::runtime_core::TelemetryProvider for UnavailableTelemetryProvider {
        async fn emit(&self, _event: RuntimeEvent) -> anyhow::Result<()> {
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Unavailable,
                reason: Some("telemetry sink offline".to_string()),
            })
        }
    }

    #[derive(Default)]
    struct RecordingSessionProvider {
        last_key: Mutex<Option<RuntimeSessionKey>>,
    }

    #[async_trait]
    impl SessionProvider for RecordingSessionProvider {
        async fn get(&self, key: &RuntimeSessionKey) -> anyhow::Result<Option<SessionRecord>> {
            *self
                .last_key
                .lock()
                .expect("recording session provider lock poisoned") = Some(key.clone());
            let tenant = greentic_types::TenantCtx::new(
                greentic_types::EnvId::try_from("dev").expect("env"),
                greentic_types::TenantId::try_from(key.tenant.as_str()).expect("tenant"),
            )
            .with_team(
                key.team
                    .as_deref()
                    .map(|team| greentic_types::TeamId::try_from(team).expect("team")),
            );
            Ok(Some(SessionRecord {
                revision: 7,
                route: None,
                context: serde_json::to_value(PersistedRuntimeSession {
                    data: StoreSessionData {
                        tenant_ctx: tenant,
                        flow_id: greentic_types::FlowId::try_from("flow-demo").expect("flow"),
                        pack_id: Some(greentic_types::PackId::try_from("pack-demo").expect("pack")),
                        cursor: greentic_types::SessionCursor::new("node-1"),
                        context_json: "{\"provider\":\"recording\"}".to_string(),
                    },
                    user: None,
                    wait_scope: None,
                })
                .expect("persisted session"),
                bundle_assignment: Some("replacement-bundle".to_string()),
                expires_at_unix_ms: None,
            }))
        }

        async fn put(
            &self,
            _key: &RuntimeSessionKey,
            _record: SessionRecord,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn compare_and_set(
            &self,
            _key: &RuntimeSessionKey,
            _expected_revision: u64,
            _record: SessionRecord,
        ) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn delete(&self, _key: &RuntimeSessionKey) -> anyhow::Result<()> {
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    #[derive(Default)]
    struct RecordingStateProvider {
        last_key: Mutex<Option<ScopedStateKey>>,
    }

    #[async_trait]
    impl StateProvider for RecordingStateProvider {
        async fn get(&self, key: &ScopedStateKey) -> anyhow::Result<Option<JsonValue>> {
            *self
                .last_key
                .lock()
                .expect("recording state provider lock poisoned") = Some(key.clone());
            Ok(Some(json!({"provider":"recording"})))
        }

        async fn put(&self, _key: &ScopedStateKey, _value: JsonValue) -> anyhow::Result<()> {
            Ok(())
        }

        async fn delete(&self, _key: &ScopedStateKey) -> anyhow::Result<()> {
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    #[async_trait]
    impl SessionProvider for UnavailableSessionProvider {
        async fn get(&self, _key: &RuntimeSessionKey) -> anyhow::Result<Option<SessionRecord>> {
            Ok(None)
        }

        async fn put(
            &self,
            _key: &RuntimeSessionKey,
            _record: SessionRecord,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn compare_and_set(
            &self,
            _key: &RuntimeSessionKey,
            _expected_revision: u64,
            _record: SessionRecord,
        ) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn delete(&self, _key: &RuntimeSessionKey) -> anyhow::Result<()> {
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Unavailable,
                reason: Some("session provider offline".to_string()),
            })
        }
    }

    #[async_trait]
    impl StateProvider for UnavailableStateProvider {
        async fn get(&self, _key: &ScopedStateKey) -> anyhow::Result<Option<JsonValue>> {
            Ok(None)
        }

        async fn put(&self, _key: &ScopedStateKey, _value: JsonValue) -> anyhow::Result<()> {
            Ok(())
        }

        async fn compare_and_set(
            &self,
            _key: &ScopedStateKey,
            _expected: Option<JsonValue>,
            _value: JsonValue,
        ) -> anyhow::Result<Option<bool>> {
            Ok(Some(false))
        }

        async fn delete(&self, _key: &ScopedStateKey) -> anyhow::Result<()> {
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Unavailable,
                reason: Some("state provider offline".to_string()),
            })
        }
    }

    #[async_trait]
    impl SessionProvider for FlakyCompareAndSetSessionProvider {
        async fn get(&self, _key: &RuntimeSessionKey) -> anyhow::Result<Option<SessionRecord>> {
            Ok(self
                .current
                .lock()
                .expect("flaky cas provider lock poisoned")
                .clone())
        }

        async fn put(&self, _key: &RuntimeSessionKey, record: SessionRecord) -> anyhow::Result<()> {
            *self
                .current
                .lock()
                .expect("flaky cas provider lock poisoned") = Some(record);
            Ok(())
        }

        async fn compare_and_set(
            &self,
            _key: &RuntimeSessionKey,
            expected_revision: u64,
            record: SessionRecord,
        ) -> anyhow::Result<bool> {
            let mut calls = self
                .compare_and_set_calls
                .lock()
                .expect("flaky cas call lock poisoned");
            *calls += 1;
            let mut current = self
                .current
                .lock()
                .expect("flaky cas provider lock poisoned");
            let Some(existing) = current.clone() else {
                return Ok(false);
            };
            if existing.revision != expected_revision {
                return Ok(false);
            }
            if *calls == 1 {
                let mut conflicted = existing;
                conflicted.revision = conflicted.revision.saturating_add(1);
                *current = Some(conflicted);
                return Ok(false);
            }
            *current = Some(record);
            Ok(true)
        }

        async fn delete(&self, _key: &RuntimeSessionKey) -> anyhow::Result<()> {
            *self
                .current
                .lock()
                .expect("flaky cas provider lock poisoned") = None;
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    #[async_trait]
    impl StateProvider for FlakyCompareAndSetStateProvider {
        async fn get(&self, _key: &ScopedStateKey) -> anyhow::Result<Option<JsonValue>> {
            Ok(self
                .current
                .lock()
                .expect("flaky state provider lock poisoned")
                .clone())
        }

        async fn put(&self, _key: &ScopedStateKey, value: JsonValue) -> anyhow::Result<()> {
            *self
                .current
                .lock()
                .expect("flaky state provider lock poisoned") = Some(value);
            Ok(())
        }

        async fn compare_and_set(
            &self,
            _key: &ScopedStateKey,
            expected: Option<JsonValue>,
            value: JsonValue,
        ) -> anyhow::Result<Option<bool>> {
            let mut calls = self
                .compare_and_set_calls
                .lock()
                .expect("flaky state call lock poisoned");
            *calls += 1;
            let mut current = self
                .current
                .lock()
                .expect("flaky state provider lock poisoned");
            if *current != expected {
                return Ok(Some(false));
            }
            if *calls == 1 {
                *current = Some(json!({"existing": true, "other": 1}));
                return Ok(Some(false));
            }
            *current = Some(value);
            Ok(Some(true))
        }

        async fn delete(&self, _key: &ScopedStateKey) -> anyhow::Result<()> {
            *self
                .current
                .lock()
                .expect("flaky state provider lock poisoned") = None;
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    #[async_trait]
    impl StateProvider for CompareAndSetUnsupportedStateProvider {
        async fn get(&self, _key: &ScopedStateKey) -> anyhow::Result<Option<JsonValue>> {
            Ok(self
                .current
                .lock()
                .expect("unsupported cas state provider lock poisoned")
                .clone())
        }

        async fn put(&self, _key: &ScopedStateKey, value: JsonValue) -> anyhow::Result<()> {
            *self
                .put_calls
                .lock()
                .expect("unsupported cas state provider put lock poisoned") += 1;
            *self
                .current
                .lock()
                .expect("unsupported cas state provider lock poisoned") = Some(value);
            Ok(())
        }

        async fn compare_and_set(
            &self,
            _key: &ScopedStateKey,
            _expected: Option<JsonValue>,
            _value: JsonValue,
        ) -> anyhow::Result<Option<bool>> {
            *self
                .compare_and_set_calls
                .lock()
                .expect("unsupported cas compare-and-set lock poisoned") += 1;
            Ok(None)
        }

        async fn delete(&self, _key: &ScopedStateKey) -> anyhow::Result<()> {
            *self
                .current
                .lock()
                .expect("unsupported cas state provider lock poisoned") = None;
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    #[async_trait]
    impl SessionProvider for DegradedSessionProvider {
        async fn get(&self, _key: &RuntimeSessionKey) -> anyhow::Result<Option<SessionRecord>> {
            Ok(None)
        }

        async fn put(
            &self,
            _key: &RuntimeSessionKey,
            _record: SessionRecord,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn compare_and_set(
            &self,
            _key: &RuntimeSessionKey,
            _expected_revision: u64,
            _record: SessionRecord,
        ) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn delete(&self, _key: &RuntimeSessionKey) -> anyhow::Result<()> {
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Degraded,
                reason: Some("session provider latency high".to_string()),
            })
        }
    }

    #[async_trait]
    impl StateProvider for DegradedStateProvider {
        async fn get(&self, _key: &ScopedStateKey) -> anyhow::Result<Option<JsonValue>> {
            Ok(None)
        }

        async fn put(&self, _key: &ScopedStateKey, _value: JsonValue) -> anyhow::Result<()> {
            Ok(())
        }

        async fn compare_and_set(
            &self,
            _key: &ScopedStateKey,
            _expected: Option<JsonValue>,
            _value: JsonValue,
        ) -> anyhow::Result<Option<bool>> {
            Ok(Some(true))
        }

        async fn delete(&self, _key: &ScopedStateKey) -> anyhow::Result<()> {
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Degraded,
                reason: Some("state provider latency high".to_string()),
            })
        }
    }

    #[test]
    fn runtime_dependency_snapshot_reports_unavailable_required_session_provider() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.session_provider = Some(Arc::new(UnavailableSessionProvider));
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let snapshot = host.runtime_status_snapshot();
        assert_eq!(
            snapshot
                .pointer("/dependencies/overall_status")
                .and_then(JsonValue::as_str),
            Some("unavailable")
        );
        assert_eq!(
            snapshot.pointer("/mode/status").and_then(JsonValue::as_str),
            Some("unavailable")
        );
        assert_eq!(
            snapshot
                .pointer("/mode/direct_execution_allowed")
                .and_then(JsonValue::as_bool),
            Some(false)
        );
        assert_eq!(
            snapshot
                .pointer("/dependencies/required/0/status")
                .and_then(JsonValue::as_str),
            Some("unavailable")
        );
        assert_eq!(
            snapshot
                .pointer("/dependencies/required/0/reason")
                .and_then(JsonValue::as_str),
            Some("session provider offline")
        );
        let err = host
            .ensure_required_runtime_dependencies_available()
            .expect_err("session dependency should fail");
        assert!(
            err.to_string()
                .contains("session: session provider offline")
        );
    }

    #[test]
    fn runtime_events_include_rich_envelope_metadata() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let telemetry = Arc::new(RecordingTelemetryProvider::default());
        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.telemetry_provider = Some(telemetry.clone());
        seams.observer_sink = Some(observer.clone());
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let envelope = OperationEnvelope::new(
            "demo.send",
            br#"{"ok":true}"#,
            &OperatorContext {
                tenant: "tenant-a".to_string(),
                team: Some("team-a".to_string()),
                correlation_id: Some("corr-1".to_string()),
            },
        );
        host.publish_runtime_event("runtime.pre_op", &envelope);

        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let observer_events = observer.events.lock().expect("observer events");
        let event = telemetry_events.last().expect("telemetry event");
        assert_eq!(observer_events.last(), Some(event));
        assert_eq!(event.event_type, "runtime.pre_op");
        assert_eq!(event.tenant.as_deref(), Some("tenant-a"));
        assert_eq!(event.team.as_deref(), Some("team-a"));
        assert_eq!(event.flow_id.as_deref(), Some(envelope.op_id.as_str()));
        assert_eq!(event.node_id.as_deref(), Some("demo.send"));
        assert_eq!(event.correlation_id.as_deref(), Some("corr-1"));
        assert_eq!(event.severity, "info");
        assert_eq!(event.outcome.as_deref(), Some("pending"));
        assert_eq!(event.reason_codes, vec!["runtime.pre_op".to_string()]);
        assert!(event.ts_unix_ms > 0);
    }

    #[test]
    fn publish_phase_event_emits_structured_ingress_event() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let telemetry = Arc::new(RecordingTelemetryProvider::default());
        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.telemetry_provider = Some(telemetry.clone());
        seams.observer_sink = Some(observer.clone());
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let ctx = OperatorContext {
            tenant: "tenant-a".to_string(),
            team: Some("team-a".to_string()),
            correlation_id: Some("corr-ingress".to_string()),
        };
        host.publish_phase_event(PhaseEventSpec {
            event_type: "ingress.received",
            severity: "info",
            outcome: Some("received"),
            ctx: &ctx,
            pack_id: Some("telegram"),
            flow_id: Some("ingest_http"),
            payload: json!({
                "domain": "messaging",
                "provider": "telegram",
                "method": "POST",
                "path": "/hooks/telegram",
            }),
        });

        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let observer_events = observer.events.lock().expect("observer events");
        let event = telemetry_events.last().expect("phase event");
        assert_eq!(observer_events.last(), Some(event));
        assert_eq!(event.event_type, "ingress.received");
        assert_eq!(event.outcome.as_deref(), Some("received"));
        assert_eq!(event.tenant.as_deref(), Some("tenant-a"));
        assert_eq!(event.team.as_deref(), Some("team-a"));
        assert_eq!(event.correlation_id.as_deref(), Some("corr-ingress"));
        assert_eq!(event.pack_id.as_deref(), Some("telegram"));
        assert_eq!(event.flow_id.as_deref(), Some("ingest_http"));
        assert_eq!(
            event.payload.get("path").and_then(JsonValue::as_str),
            Some("/hooks/telegram")
        );
    }

    #[test]
    fn runtime_event_delivery_isolates_telemetry_failure_from_observer_delivery() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.telemetry_provider = Some(Arc::new(FailingTelemetryProvider));
        seams.observer_sink = Some(observer.clone());
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let envelope = OperationEnvelope::new(
            "demo.send",
            br#"{"ok":true}"#,
            &OperatorContext {
                tenant: "tenant-a".to_string(),
                team: None,
                correlation_id: Some("corr-2".to_string()),
            },
        );
        host.publish_runtime_event("runtime.pre_op", &envelope);

        let observer_events = observer.events.lock().expect("observer events");
        assert_eq!(observer_events.len(), 1);
        assert_eq!(observer_events[0].event_type, "runtime.pre_op");

        let snapshot = host.runtime_status_snapshot();
        assert_eq!(
            snapshot
                .pointer("/events/telemetry_failures")
                .and_then(JsonValue::as_u64),
            Some(1)
        );
        assert_eq!(
            snapshot
                .pointer("/events/observer_failures")
                .and_then(JsonValue::as_u64),
            Some(0)
        );
        assert_eq!(
            snapshot
                .pointer("/events/last_telemetry_error")
                .and_then(JsonValue::as_str),
            Some("telemetry sink failed")
        );
    }

    #[test]
    fn runtime_event_delivery_records_timeout_without_blocking_other_sinks() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.telemetry_provider = Some(Arc::new(SlowTelemetryProvider));
        seams.observer_sink = Some(observer.clone());
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let envelope = OperationEnvelope::new(
            "demo.send",
            br#"{"ok":true}"#,
            &OperatorContext {
                tenant: "tenant-a".to_string(),
                team: None,
                correlation_id: Some("corr-3".to_string()),
            },
        );
        host.publish_runtime_event("runtime.pre_op", &envelope);

        let observer_events = observer.events.lock().expect("observer events");
        assert_eq!(observer_events.len(), 1);

        let snapshot = host.runtime_status_snapshot();
        assert_eq!(
            snapshot
                .pointer("/events/telemetry_timeouts")
                .and_then(JsonValue::as_u64),
            Some(1)
        );
        assert_eq!(
            snapshot
                .pointer("/events/observer_timeouts")
                .and_then(JsonValue::as_u64),
            Some(0)
        );
        assert_eq!(
            snapshot
                .pointer("/events/last_telemetry_error")
                .and_then(JsonValue::as_str),
            Some("telemetry event delivery timed out")
        );
    }

    #[test]
    fn runtime_event_delivery_drops_when_backpressure_gate_is_busy() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = Arc::new(
            DemoRunnerHost::new(
                tmp.path().to_path_buf(),
                &discovery,
                None,
                secrets_handle,
                false,
            )
            .expect("build host"),
        );

        let telemetry = Arc::new(SlowTelemetryProvider);
        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.telemetry_provider = Some(telemetry);
        seams.observer_sink = Some(observer);
        host.replace_runtime_core_for_test(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let first_host = host.clone();
        let first = std::thread::spawn(move || {
            let envelope = OperationEnvelope::new(
                "demo.send",
                br#"{"ok":true}"#,
                &OperatorContext {
                    tenant: "tenant-a".to_string(),
                    team: None,
                    correlation_id: Some("corr-4".to_string()),
                },
            );
            first_host.publish_runtime_event("runtime.pre_op", &envelope);
        });

        std::thread::sleep(Duration::from_millis(20));

        let second_host = host.clone();
        let second = std::thread::spawn(move || {
            let envelope = OperationEnvelope::new(
                "demo.send",
                br#"{"ok":true}"#,
                &OperatorContext {
                    tenant: "tenant-a".to_string(),
                    team: None,
                    correlation_id: Some("corr-5".to_string()),
                },
            );
            second_host.publish_runtime_event("runtime.post_op", &envelope);
        });

        first.join().expect("first publish thread");
        second.join().expect("second publish thread");

        let snapshot = host.runtime_status_snapshot();
        assert_eq!(
            snapshot
                .pointer("/events/dropped_events")
                .and_then(JsonValue::as_u64),
            Some(1)
        );
        assert!(matches!(
            snapshot
                .pointer("/events/last_dropped_event_type")
                .and_then(JsonValue::as_str),
            Some("runtime.pre_op" | "runtime.post_op")
        ));
    }

    #[test]
    fn runtime_status_snapshot_emits_mode_transition_event() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let telemetry = Arc::new(RecordingTelemetryProvider::default());
        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.telemetry_provider = Some(telemetry.clone());
        seams.observer_sink = Some(observer.clone());
        seams.state_provider = Some(Arc::new(DegradedStateProvider));
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let _ = host.runtime_status_snapshot();

        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let observer_events = observer.events.lock().expect("observer events");
        let event = telemetry_events
            .iter()
            .find(|event| event.event_type == "runtime.mode_transition")
            .expect("mode transition event");
        assert!(observer_events.iter().any(|candidate| candidate == event));
        assert_eq!(event.event_type, "runtime.mode_transition");
        assert_eq!(event.outcome.as_deref(), Some("degraded"));
        assert_eq!(event.severity, "info");
        assert_eq!(
            event
                .payload
                .get("current_mode")
                .and_then(JsonValue::as_str),
            Some("degraded")
        );
        assert_eq!(
            event.payload.get("safe_mode").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            event
                .payload
                .get("degraded_level")
                .and_then(JsonValue::as_u64),
            Some(2)
        );
        let safe_mode_event = telemetry_events
            .iter()
            .find(|event| event.event_type == "runtime.safe_mode_enter")
            .expect("safe mode enter event");
        assert_eq!(safe_mode_event.outcome.as_deref(), Some("safe_mode"));
        let degraded_level_event = telemetry_events
            .iter()
            .find(|event| event.event_type == "runtime.degraded_level_changed")
            .expect("degraded level changed event");
        assert_eq!(degraded_level_event.outcome.as_deref(), Some("degraded"));
    }

    #[test]
    fn runtime_status_snapshot_reports_optional_provider_outage_without_safe_mode() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.telemetry_provider = Some(Arc::new(UnavailableTelemetryProvider));
        seams.observer_sink = Some(observer.clone());
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let snapshot = host.runtime_status_snapshot();
        assert_eq!(
            snapshot.pointer("/mode/status").and_then(JsonValue::as_str),
            Some("degraded")
        );
        assert_eq!(
            snapshot
                .pointer("/mode/safe_mode")
                .and_then(JsonValue::as_bool),
            Some(false)
        );
        assert_eq!(
            snapshot
                .pointer("/mode/degraded_level")
                .and_then(JsonValue::as_u64),
            Some(1)
        );
        assert_eq!(
            snapshot
                .pointer("/provider_health/providers/2/status")
                .and_then(JsonValue::as_str),
            Some("unavailable")
        );

        let (ready, readyz) = host.readyz_snapshot();
        assert!(ready);
        assert_eq!(
            readyz
                .pointer("/degraded_level")
                .and_then(JsonValue::as_u64),
            Some(1)
        );

        let observer_events = observer.events.lock().expect("observer events");
        let event = observer_events
            .iter()
            .find(|event| event.event_type == "runtime.provider_outage")
            .expect("provider outage event");
        assert_eq!(event.outcome.as_deref(), Some("unavailable"));
        assert_eq!(
            event
                .payload
                .get("provider_class")
                .and_then(JsonValue::as_str),
            Some("telemetry")
        );
    }

    #[test]
    fn runtime_status_snapshot_emits_provider_recovery_event() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let telemetry = Arc::new(RecordingTelemetryProvider::default());
        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.telemetry_provider = Some(Arc::new(UnavailableTelemetryProvider));
        seams.observer_sink = Some(observer);
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));
        let _ = host.runtime_status_snapshot();

        let current = host.runtime_core();
        let mut recovered_seams = current.seams().clone();
        recovered_seams.telemetry_provider = Some(telemetry.clone());
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            recovered_seams,
            current.wiring_plan().clone(),
        ));
        let _ = host.runtime_status_snapshot();

        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let event = telemetry_events
            .iter()
            .find(|event| event.event_type == "runtime.provider_recovery")
            .expect("provider recovery event");
        assert_eq!(event.outcome.as_deref(), Some("available"));
        assert_eq!(
            event
                .payload
                .get("provider_class")
                .and_then(JsonValue::as_str),
            Some("telemetry")
        );
    }

    #[test]
    fn runtime_dependency_snapshot_reports_unavailable_required_state_provider() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.state_provider = Some(Arc::new(UnavailableStateProvider));
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let snapshot = host.runtime_status_snapshot();
        assert_eq!(
            snapshot
                .pointer("/dependencies/overall_status")
                .and_then(JsonValue::as_str),
            Some("unavailable")
        );
        assert_eq!(
            snapshot.pointer("/mode/status").and_then(JsonValue::as_str),
            Some("unavailable")
        );
        assert_eq!(
            snapshot
                .pointer("/mode/safe_mode")
                .and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            snapshot
                .pointer("/mode/degraded_level")
                .and_then(JsonValue::as_u64),
            Some(3)
        );
        assert_eq!(
            snapshot
                .pointer("/mode/direct_execution_allowed")
                .and_then(JsonValue::as_bool),
            Some(false)
        );
        assert_eq!(
            snapshot
                .pointer("/dependencies/required/1/status")
                .and_then(JsonValue::as_str),
            Some("unavailable")
        );
        assert_eq!(
            snapshot
                .pointer("/dependencies/required/1/reason")
                .and_then(JsonValue::as_str),
            Some("state provider offline")
        );
        let err = host
            .ensure_required_runtime_dependencies_available()
            .expect_err("state dependency should fail");
        assert!(err.to_string().contains("state: state provider offline"));
    }

    #[test]
    #[allow(deprecated)]
    fn host_store_adapters_follow_runtime_core_provider_replacement() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let session_provider = Arc::new(RecordingSessionProvider::default());
        let state_provider = Arc::new(RecordingStateProvider::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.session_provider = Some(session_provider.clone());
        seams.state_provider = Some(state_provider.clone());
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let tenant = greentic_types::TenantCtx::new(
            greentic_types::EnvId::try_from("dev").expect("env"),
            greentic_types::TenantId::try_from("tenant-a").expect("tenant"),
        )
        .with_team(Some(
            greentic_types::TeamId::try_from("team-a").expect("team"),
        ))
        .with_user(Some(
            greentic_types::UserId::try_from("user-a").expect("user"),
        ));
        let session_key = StoreSessionKey::new("rt::tenant-a::team-a::session-1");
        let session = host
            .session_store
            .get_session(&session_key)
            .expect("get session from active provider")
            .expect("session from replacement provider");
        assert_eq!(session.context_json, "{\"provider\":\"recording\"}");
        assert_eq!(
            session_provider
                .last_key
                .lock()
                .expect("session provider lock")
                .clone()
                .as_ref()
                .map(|key| key.tenant.as_str()),
            Some("tenant-a")
        );

        let state_key = greentic_types::StateKey::new("flow/demo");
        let state = host
            .state_store
            .get_json(&tenant, "runner", &state_key, None)
            .expect("get state from active provider")
            .expect("state from replacement provider");
        assert_eq!(state, json!({"provider":"recording"}));
        assert_eq!(
            state_provider
                .last_key
                .lock()
                .expect("state provider lock")
                .clone()
                .as_ref()
                .map(|key| (key.tenant.as_str(), key.scope.as_str(), key.key.as_str())),
            Some(("tenant-a", "runner", "flow_demo"))
        );
    }

    #[test]
    fn runtime_dependency_snapshot_reports_degraded_required_session_provider() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.session_provider = Some(Arc::new(DegradedSessionProvider));
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let snapshot = host.runtime_status_snapshot();
        assert_eq!(
            snapshot
                .pointer("/dependencies/overall_status")
                .and_then(JsonValue::as_str),
            Some("degraded")
        );
        assert_eq!(
            snapshot.pointer("/mode/status").and_then(JsonValue::as_str),
            Some("degraded")
        );
        assert_eq!(
            snapshot
                .pointer("/mode/safe_mode")
                .and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            snapshot
                .pointer("/mode/degraded_level")
                .and_then(JsonValue::as_u64),
            Some(2)
        );
        assert_eq!(
            snapshot
                .pointer("/mode/direct_execution_allowed")
                .and_then(JsonValue::as_bool),
            Some(true)
        );
        host.ensure_required_runtime_dependencies_available()
            .expect("degraded dependency should remain executable");
    }

    #[test]
    fn runtime_dependency_snapshot_reports_degraded_required_state_provider() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.state_provider = Some(Arc::new(DegradedStateProvider));
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let snapshot = host.runtime_status_snapshot();
        assert_eq!(
            snapshot
                .pointer("/dependencies/overall_status")
                .and_then(JsonValue::as_str),
            Some("degraded")
        );
        assert_eq!(
            snapshot.pointer("/mode/status").and_then(JsonValue::as_str),
            Some("degraded")
        );
        assert_eq!(
            snapshot
                .pointer("/mode/safe_mode")
                .and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            snapshot
                .pointer("/mode/degraded_level")
                .and_then(JsonValue::as_u64),
            Some(2)
        );
        assert_eq!(
            snapshot
                .pointer("/mode/direct_execution_allowed")
                .and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            snapshot
                .pointer("/dependencies/required/1/status")
                .and_then(JsonValue::as_str),
            Some("degraded")
        );
        assert_eq!(
            snapshot
                .pointer("/dependencies/required/1/reason")
                .and_then(JsonValue::as_str),
            Some("state provider latency high")
        );
        host.ensure_required_runtime_dependencies_available()
            .expect("degraded dependency should remain executable");
    }

    #[test]
    fn direct_provider_invocation_returns_classified_dependency_failure_outcome() {
        let tmp = tempdir().expect("tempdir");
        let providers = tmp.path().join("providers").join("messaging");
        std::fs::create_dir_all(&providers).expect("providers dir");
        let pack_path = providers.join("session.gtpack");
        write_test_provider_pack(
            &pack_path,
            "session.provider",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.dispatch"},
                    "priority": 10
                }]
            }),
        );

        let discovery = crate::discovery::discover(tmp.path()).expect("discover bundle");
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.session_provider = Some(Arc::new(UnavailableSessionProvider));
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let pack = ProviderPack {
            pack_id: "session.provider".to_string(),
            file_name: "session.gtpack".to_string(),
            path: pack_path,
            entry_flows: Vec::new(),
        };
        let ctx = OperatorContext {
            tenant: "default".to_string(),
            team: None,
            correlation_id: None,
        };

        let outcome = host
            .invoke_provider_component_op_direct(
                Domain::Messaging,
                &pack,
                "session.provider",
                "session.dispatch",
                br#"{}"#,
                &ctx,
            )
            .expect("dependency failure should return FlowOutcome");

        assert!(!outcome.success);
        assert_eq!(outcome.mode, RunnerExecutionMode::Exec);
        assert_eq!(
            outcome
                .output
                .as_ref()
                .and_then(|value| value.get("code"))
                .and_then(JsonValue::as_str),
            Some("runtime_dependency_unavailable")
        );
        assert!(
            outcome
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("session provider offline")
        );
    }

    #[test]
    fn direct_provider_invocation_emits_dependency_unavailable_event() {
        let tmp = tempdir().expect("tempdir");
        let providers = tmp.path().join("providers").join("messaging");
        std::fs::create_dir_all(&providers).expect("providers dir");
        let pack_path = providers.join("session.gtpack");
        write_test_provider_pack(
            &pack_path,
            "session.provider",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.dispatch"},
                    "priority": 10
                }]
            }),
        );

        let discovery = crate::discovery::discover(tmp.path()).expect("discover bundle");
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let telemetry = Arc::new(RecordingTelemetryProvider::default());
        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.session_provider = Some(Arc::new(UnavailableSessionProvider));
        seams.telemetry_provider = Some(telemetry.clone());
        seams.observer_sink = Some(observer.clone());
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let pack = ProviderPack {
            pack_id: "session.provider".to_string(),
            file_name: "session.gtpack".to_string(),
            path: pack_path,
            entry_flows: Vec::new(),
        };
        let ctx = OperatorContext {
            tenant: "default".to_string(),
            team: Some("ops".to_string()),
            correlation_id: Some("corr-dep-unavail".to_string()),
        };

        let _ = host
            .invoke_provider_component_op_direct(
                Domain::Messaging,
                &pack,
                "session.provider",
                "session.dispatch",
                br#"{}"#,
                &ctx,
            )
            .expect("dependency failure should return FlowOutcome");

        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let observer_events = observer.events.lock().expect("observer events");
        let event = telemetry_events
            .iter()
            .find(|event| event.event_type == "runtime.dependency_unavailable")
            .expect("dependency unavailable event");
        assert!(observer_events.iter().any(|candidate| candidate == event));
        assert_eq!(event.event_type, "runtime.dependency_unavailable");
        assert_eq!(event.outcome.as_deref(), Some("unavailable"));
        assert_eq!(event.severity, "warn");
        assert_eq!(event.tenant.as_deref(), Some("default"));
        assert_eq!(event.team.as_deref(), Some("ops"));
        assert_eq!(event.correlation_id.as_deref(), Some("corr-dep-unavail"));
        assert_eq!(event.pack_id.as_deref(), Some("session.provider"));
        assert_eq!(event.flow_id.as_deref(), Some("session.dispatch"));
        assert_eq!(
            event
                .payload
                .get("reason")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .contains("session provider offline"),
            true
        );
        let load_shed = telemetry_events
            .iter()
            .find(|event| event.event_type == "runtime.load_shed")
            .expect("load shed event");
        assert_eq!(load_shed.outcome.as_deref(), Some("refused"));
        assert_eq!(
            load_shed
                .payload
                .get("reason")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .contains("session provider offline"),
            true
        );
    }

    #[test]
    fn direct_provider_invocation_returns_classified_state_dependency_failure_outcome() {
        let tmp = tempdir().expect("tempdir");
        let providers = tmp.path().join("providers").join("messaging");
        std::fs::create_dir_all(&providers).expect("providers dir");
        let pack_path = providers.join("state.gtpack");
        write_test_provider_pack(
            &pack_path,
            "state.provider",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "state.default",
                    "cap_id": crate::runtime_core::CAP_STATE_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_STATE_PROVIDER_V1,
                    "provider": {"component_ref": "state.component", "op": "state.dispatch"},
                    "priority": 10
                }]
            }),
        );

        let discovery = crate::discovery::discover(tmp.path()).expect("discover bundle");
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.state_provider = Some(Arc::new(UnavailableStateProvider));
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let pack = ProviderPack {
            pack_id: "state.provider".to_string(),
            file_name: "state.gtpack".to_string(),
            path: pack_path,
            entry_flows: Vec::new(),
        };
        let ctx = OperatorContext {
            tenant: "default".to_string(),
            team: None,
            correlation_id: None,
        };

        let outcome = host
            .invoke_provider_component_op_direct(
                Domain::Messaging,
                &pack,
                "state.provider",
                "state.dispatch",
                br#"{}"#,
                &ctx,
            )
            .expect("dependency failure should return FlowOutcome");

        assert!(!outcome.success);
        assert_eq!(outcome.mode, RunnerExecutionMode::Exec);
        assert_eq!(
            outcome
                .output
                .as_ref()
                .and_then(|value| value.get("code"))
                .and_then(JsonValue::as_str),
            Some("runtime_dependency_unavailable")
        );
        assert!(
            outcome
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("state provider offline")
        );
    }

    #[test]
    fn direct_provider_invocation_emits_dependency_degraded_event() {
        let tmp = tempdir().expect("tempdir");
        let providers = tmp.path().join("providers").join("messaging");
        std::fs::create_dir_all(&providers).expect("providers dir");
        let pack_path = providers.join("state.gtpack");
        write_test_provider_pack(
            &pack_path,
            "state.provider",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "state.default",
                    "cap_id": crate::runtime_core::CAP_STATE_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_STATE_PROVIDER_V1,
                    "provider": {"component_ref": "state.component", "op": "state.dispatch"},
                    "priority": 10
                }]
            }),
        );

        let discovery = crate::discovery::discover(tmp.path()).expect("discover bundle");
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let telemetry = Arc::new(RecordingTelemetryProvider::default());
        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.state_provider = Some(Arc::new(DegradedStateProvider));
        seams.telemetry_provider = Some(telemetry.clone());
        seams.observer_sink = Some(observer.clone());
        host.runtime_core.replace(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let pack = ProviderPack {
            pack_id: "state.provider".to_string(),
            file_name: "state.gtpack".to_string(),
            path: pack_path,
            entry_flows: Vec::new(),
        };
        let ctx = OperatorContext {
            tenant: "default".to_string(),
            team: None,
            correlation_id: Some("corr-dep-degraded".to_string()),
        };

        let _ = host
            .invoke_provider_component_op_direct(
                Domain::Messaging,
                &pack,
                "state.provider",
                "state.dispatch",
                br#"{}"#,
                &ctx,
            )
            .expect("degraded invocation should return FlowOutcome");

        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let observer_events = observer.events.lock().expect("observer events");
        let event = telemetry_events
            .iter()
            .find(|event| event.event_type == "runtime.dependency_degraded_execution")
            .expect("dependency degraded event");
        assert!(observer_events.iter().any(|candidate| candidate == event));
        assert_eq!(event.outcome.as_deref(), Some("degraded"));
        assert_eq!(event.severity, "info");
        assert_eq!(event.pack_id.as_deref(), Some("state.provider"));
        assert_eq!(event.flow_id.as_deref(), Some("state.dispatch"));
        assert_eq!(
            event
                .payload
                .pointer("/dependencies/overall_status")
                .and_then(JsonValue::as_str),
            Some("degraded")
        );
    }

    #[test]
    fn degraded_dependency_warning_is_attached_to_success_output() {
        let wrapped = attach_degraded_dependency_warning(
            json!({"ok": true}),
            true,
            &[String::from("session: session provider latency high")],
        );
        assert_eq!(
            wrapped
                .pointer("/runtime_dependency_warning/status")
                .and_then(JsonValue::as_str),
            Some("degraded")
        );
        assert_eq!(
            wrapped
                .pointer("/runtime_dependency_warning/reasons/0")
                .and_then(JsonValue::as_str),
            Some("session: session provider latency high")
        );
        assert_eq!(
            wrapped.pointer("/ok").and_then(JsonValue::as_bool),
            Some(true)
        );
    }

    #[test]
    fn degraded_dependency_warning_wraps_non_object_output() {
        let wrapped = attach_degraded_dependency_warning(
            json!("ok"),
            true,
            &[String::from("state: state provider latency high")],
        );
        assert_eq!(
            wrapped.get("result").and_then(JsonValue::as_str),
            Some("ok")
        );
        assert_eq!(
            wrapped
                .pointer("/runtime_dependency_warning/status")
                .and_then(JsonValue::as_str),
            Some("degraded")
        );
        assert_eq!(
            wrapped
                .pointer("/runtime_dependency_warning/reasons/0")
                .and_then(JsonValue::as_str),
            Some("state: state provider latency high")
        );
    }

    #[test]
    fn degraded_dependency_warning_is_noop_when_not_degraded() {
        let original = json!({"ok": true});
        let wrapped = attach_degraded_dependency_warning(
            original.clone(),
            false,
            &[String::from("session: session provider latency high")],
        );
        assert_eq!(wrapped, original);
        assert!(wrapped.get("runtime_dependency_warning").is_none());
    }

    #[test]
    fn local_bundle_seams_resolve_and_read_paths() {
        let tmp = tempdir().expect("tempdir");
        let pack_path = tmp.path().join("packs").join("demo.gtpack");
        std::fs::create_dir_all(pack_path.parent().expect("parent")).expect("create packs dir");
        std::fs::write(&pack_path, b"bundle-bytes").expect("write pack");

        let access = BundleAccessHandle::open(
            tmp.path(),
            &BundleAccessConfig::new(tmp.path().join("state").join("runtime").join("bundle_fs")),
        )
        .expect("bundle access");
        let active = ActiveBundleAccess::new(access);
        let source = LocalBundleSource::new(active.clone());
        let resolver = LocalBundleResolver::new(active.clone());
        let fs = LocalBundleFs::new(active);
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

        let staged = runtime.block_on(source.stage(".")).expect("stage bundle");
        assert_eq!(staged, tmp.path());

        let resolved = runtime
            .block_on(resolver.resolve("packs/demo.gtpack"))
            .expect("resolve bundle path");
        assert_eq!(PathBuf::from(resolved), pack_path);

        let exists = runtime
            .block_on(fs.exists("packs/demo.gtpack"))
            .expect("exists");
        assert!(exists);

        let bytes = runtime
            .block_on(fs.read("packs/demo.gtpack"))
            .expect("read bytes");
        assert_eq!(bytes, b"bundle-bytes");
    }

    #[test]
    fn local_bundle_seams_follow_replaced_active_access() {
        let tmp = tempdir().expect("tempdir");
        let bundle_a = tmp.path().join("bundle-a");
        let bundle_b = tmp.path().join("bundle-b");
        std::fs::create_dir_all(bundle_a.join("packs")).expect("bundle a packs");
        std::fs::create_dir_all(bundle_b.join("packs")).expect("bundle b packs");
        std::fs::write(bundle_a.join("packs").join("demo.gtpack"), b"bundle-a")
            .expect("write bundle a");
        std::fs::write(bundle_b.join("packs").join("demo.gtpack"), b"bundle-b")
            .expect("write bundle b");

        let access_a = BundleAccessHandle::open(
            &bundle_a,
            &BundleAccessConfig::new(bundle_a.join("state").join("runtime").join("bundle_fs")),
        )
        .expect("bundle access a");
        let access_b = BundleAccessHandle::open(
            &bundle_b,
            &BundleAccessConfig::new(bundle_b.join("state").join("runtime").join("bundle_fs")),
        )
        .expect("bundle access b");
        let active = ActiveBundleAccess::new(access_a);
        let source = LocalBundleSource::new(active.clone());
        let resolver = LocalBundleResolver::new(active.clone());
        let fs = LocalBundleFs::new(active.clone());
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

        let first = runtime
            .block_on(fs.read("packs/demo.gtpack"))
            .expect("read bundle a");
        assert_eq!(first, b"bundle-a");

        active.replace(access_b);

        let staged = runtime.block_on(source.stage(".")).expect("stage bundle b");
        assert_eq!(staged, bundle_b);

        let resolved = runtime
            .block_on(resolver.resolve("packs/demo.gtpack"))
            .expect("resolve bundle b path");
        assert_eq!(
            PathBuf::from(resolved),
            bundle_b.join("packs").join("demo.gtpack")
        );

        let second = runtime
            .block_on(fs.read("packs/demo.gtpack"))
            .expect("read bundle b");
        assert_eq!(second, b"bundle-b");
    }

    #[test]
    fn runtime_status_snapshot_reports_bundle_and_seam_health() {
        let tmp = tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let snapshot = host.runtime_status_snapshot();
        assert_eq!(
            snapshot
                .pointer("/bundle/runtime_id")
                .and_then(JsonValue::as_str),
            Some(tmp.path().to_string_lossy().as_ref())
        );
        assert_eq!(
            snapshot
                .pointer("/bundle/access/mode")
                .and_then(JsonValue::as_str),
            Some("directory")
        );
        assert_eq!(
            snapshot
                .pointer("/bundle/lifecycle/active_bundle_id")
                .and_then(JsonValue::as_str),
            Some(tmp.path().to_string_lossy().as_ref())
        );
        assert_eq!(
            snapshot
                .pointer("/bundle/lifecycle/events/0/kind")
                .and_then(JsonValue::as_str),
            Some("warm")
        );
        assert_eq!(
            snapshot
                .pointer("/bundle/lifecycle/events/1/kind")
                .and_then(JsonValue::as_str),
            Some("activate")
        );
        assert_eq!(
            snapshot
                .pointer("/seam_health/state/status")
                .and_then(JsonValue::as_str),
            Some("available")
        );
        assert_eq!(
            snapshot.pointer("/mode/status").and_then(JsonValue::as_str),
            Some("available")
        );
        assert_eq!(
            snapshot
                .pointer("/mode/direct_execution_allowed")
                .and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            snapshot
                .pointer("/seam_health/bundle_fs/configured")
                .and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            snapshot
                .pointer("/roles/seams/admin_auth")
                .and_then(JsonValue::as_bool),
            Some(true)
        );
    }

    #[test]
    fn activating_bundle_ref_swaps_active_runtime_core_selection() {
        let tmp = tempdir().expect("tempdir");
        let bundle_a = tmp.path().join("bundle-a");
        let bundle_b = tmp.path().join("bundle-b");
        let providers_a = bundle_a.join("providers").join("messaging");
        let providers_b = bundle_b.join("providers").join("messaging");
        std::fs::create_dir_all(&providers_a).expect("providers a");
        std::fs::create_dir_all(&providers_b).expect("providers b");
        write_test_provider_pack(
            &providers_a.join("session-a.gtpack"),
            "session.a",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.a"},
                    "priority": 10
                }]
            }),
        );
        write_test_provider_pack(
            &providers_b.join("session-b.gtpack"),
            "session.b",
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                    "version": crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.b"},
                    "priority": 10
                }]
            }),
        );

        let discovery = crate::discovery::discover(&bundle_a).expect("discover bundle a");
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(&bundle_a, "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(bundle_a.clone(), &discovery, None, secrets_handle, false)
            .expect("build host");

        let before = host.runtime_status_snapshot();
        assert_eq!(
            before
                .pointer("/roles/selected/session/pack_id")
                .and_then(JsonValue::as_str),
            Some("session.a")
        );

        host.activate_bundle_ref(&bundle_b)
            .expect("activate bundle b");

        let after = host.runtime_status_snapshot();
        assert_eq!(
            after
                .pointer("/roles/selected/session/pack_id")
                .and_then(JsonValue::as_str),
            Some("session.b")
        );
        assert_eq!(
            host.resolve_capability(
                crate::runtime_core::CAP_SESSION_PROVIDER_V1,
                Some(crate::runtime_core::CONTRACT_SESSION_PROVIDER_V1),
                ResolveScope::default(),
            )
            .as_ref()
            .map(|binding| binding.pack_id.as_str()),
            Some("session.b")
        );
    }
}
