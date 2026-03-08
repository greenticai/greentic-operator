use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use greentic_types::ReplyScope;
use serde_json::Value as JsonValue;

use crate::capabilities::{CapabilityBinding, ResolveScope};
use crate::capabilities::{CapabilityPackRecord, CapabilityRegistry};
use crate::offers::{OfferKind, OfferRegistry};

pub const CAP_SESSION_PROVIDER_V1: &str = "greentic.cap.session.provider.v1";
pub const CAP_STATE_PROVIDER_V1: &str = "greentic.cap.state.provider.v1";
pub const CAP_TELEMETRY_PROVIDER_V1: &str = "greentic.cap.telemetry.provider.v1";

pub const CONTRACT_SESSION_PROVIDER_V1: &str = "greentic.contract.session.v1";
pub const CONTRACT_STATE_PROVIDER_V1: &str = "greentic.contract.state.v1";
pub const CONTRACT_TELEMETRY_PROVIDER_V1: &str = "greentic.contract.telemetry.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeLifecycleState {
    Discovered,
    Wired,
    Active,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeHealthStatus {
    Unknown,
    Available,
    Degraded,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeHealth {
    pub status: RuntimeHealthStatus,
    pub reason: Option<String>,
}

impl Default for RuntimeHealth {
    fn default() -> Self {
        Self {
            status: RuntimeHealthStatus::Unknown,
            reason: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeScope {
    pub envs: Vec<String>,
    pub tenants: Vec<String>,
    pub teams: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeCapabilityDescriptor {
    pub capability_id: String,
    pub contract_id: String,
    pub provider_pack: String,
    pub domain: crate::domains::Domain,
    pub pack_path: PathBuf,
    pub entrypoint: String,
    pub component_ref: String,
    pub priority: i32,
    pub scope: RuntimeScope,
    pub applies_to_ops: Vec<String>,
    pub lifecycle_state: RuntimeLifecycleState,
    pub health: RuntimeHealth,
    pub requires_setup: bool,
    pub setup_qa_ref: Option<String>,
    pub stable_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeHookDescriptor {
    pub offer_key: String,
    pub provider_pack: String,
    pub pack_path: PathBuf,
    pub stage: String,
    pub contract_id: String,
    pub entrypoint: String,
    pub priority: i32,
    pub lifecycle_state: RuntimeLifecycleState,
    pub health: RuntimeHealth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSubscriptionDescriptor {
    pub offer_key: String,
    pub provider_pack: String,
    pub pack_path: PathBuf,
    pub contract_id: String,
    pub entrypoint: String,
    pub priority: i32,
    pub lifecycle_state: RuntimeLifecycleState,
    pub health: RuntimeHealth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeProviderRequirement {
    pub role_id: String,
    pub capability_id: String,
    pub contract_id: String,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSelectedProvider {
    pub role_id: String,
    pub capability_id: String,
    pub contract_id: String,
    pub provider_pack: String,
    pub pack_path: PathBuf,
    pub entrypoint: String,
    pub component_ref: String,
    pub stable_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeWiringPlan {
    pub selected_providers: BTreeMap<String, RuntimeSelectedProvider>,
    pub hook_chains: BTreeMap<String, Vec<RuntimeHookDescriptor>>,
    pub subscriptions_by_contract: BTreeMap<String, Vec<RuntimeSubscriptionDescriptor>>,
    pub warnings: Vec<String>,
    pub blocking_failures: Vec<String>,
}

impl RuntimeWiringPlan {
    pub fn selected_provider(&self, role_id: &str) -> Option<&RuntimeSelectedProvider> {
        self.selected_providers.get(role_id)
    }

    pub fn hook_chain(&self, stage: &str, contract_id: &str) -> &[RuntimeHookDescriptor] {
        self.hook_chains
            .get(&format!("{stage}:{contract_id}"))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn subscriptions(&self, contract_id: &str) -> &[RuntimeSubscriptionDescriptor] {
        self.subscriptions_by_contract
            .get(contract_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn has_subscription_provider(
        &self,
        provider_pack: &str,
        contract_id: Option<&str>,
    ) -> bool {
        match contract_id {
            Some(contract_id) => self
                .subscriptions(contract_id)
                .iter()
                .any(|descriptor| descriptor.provider_pack == provider_pack),
            None => self
                .subscriptions_by_contract
                .values()
                .flatten()
                .any(|descriptor| descriptor.provider_pack == provider_pack),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeCapabilityRegistry {
    capabilities_by_id: BTreeMap<String, Vec<RuntimeCapabilityDescriptor>>,
    hooks_by_stage_contract: BTreeMap<(String, String), Vec<RuntimeHookDescriptor>>,
    subscriptions_by_contract: BTreeMap<String, Vec<RuntimeSubscriptionDescriptor>>,
}

impl RuntimeCapabilityRegistry {
    pub fn discover(pack_index: &BTreeMap<PathBuf, CapabilityPackRecord>) -> anyhow::Result<Self> {
        let capability_registry = CapabilityRegistry::build_from_pack_index(pack_index)?;
        let pack_refs = pack_index.keys().cloned().collect::<Vec<_>>();
        let offer_registry = OfferRegistry::from_pack_refs(&pack_refs)?;

        let mut capabilities_by_id = BTreeMap::new();
        for offer in capability_registry.all_offers() {
            capabilities_by_id
                .entry(offer.cap_id.clone())
                .or_insert_with(Vec::new)
                .push(RuntimeCapabilityDescriptor {
                    capability_id: offer.cap_id.clone(),
                    contract_id: offer.version.clone(),
                    provider_pack: offer.pack_id.clone(),
                    domain: offer.domain,
                    pack_path: offer.pack_path.clone(),
                    entrypoint: offer.provider_op.clone(),
                    component_ref: offer.provider_component_ref.clone(),
                    priority: offer.priority,
                    scope: RuntimeScope {
                        envs: offer.envs().to_vec(),
                        tenants: offer.tenants().to_vec(),
                        teams: offer.teams().to_vec(),
                    },
                    applies_to_ops: offer.applies_to_ops.clone(),
                    lifecycle_state: RuntimeLifecycleState::Discovered,
                    health: RuntimeHealth::default(),
                    requires_setup: offer.requires_setup,
                    setup_qa_ref: offer.setup_qa_ref.clone(),
                    stable_id: offer.stable_id.clone(),
                });
        }

        let mut hooks_by_stage_contract = BTreeMap::new();
        let mut subscriptions_by_contract = BTreeMap::new();
        for offer in offer_registry.offers() {
            match offer.kind {
                OfferKind::Hook => {
                    let Some(stage) = offer.stage.clone() else {
                        continue;
                    };
                    let Some(contract_id) = offer.contract.clone() else {
                        continue;
                    };
                    hooks_by_stage_contract
                        .entry((stage.clone(), contract_id.clone()))
                        .or_insert_with(Vec::new)
                        .push(RuntimeHookDescriptor {
                            offer_key: offer.offer_key.clone(),
                            provider_pack: offer.pack_id.clone(),
                            pack_path: offer.pack_ref.clone(),
                            stage,
                            contract_id,
                            entrypoint: offer.provider_op.clone(),
                            priority: offer.priority,
                            lifecycle_state: RuntimeLifecycleState::Discovered,
                            health: RuntimeHealth::default(),
                        });
                }
                OfferKind::Subs => {
                    let contract_id = offer
                        .contract
                        .clone()
                        .unwrap_or_else(|| "<none>".to_string());
                    subscriptions_by_contract
                        .entry(contract_id.clone())
                        .or_insert_with(Vec::new)
                        .push(RuntimeSubscriptionDescriptor {
                            offer_key: offer.offer_key.clone(),
                            provider_pack: offer.pack_id.clone(),
                            pack_path: offer.pack_ref.clone(),
                            contract_id,
                            entrypoint: offer.provider_op.clone(),
                            priority: offer.priority,
                            lifecycle_state: RuntimeLifecycleState::Discovered,
                            health: RuntimeHealth::default(),
                        });
                }
                OfferKind::Capability => {}
            }
        }

        for descriptors in capabilities_by_id.values_mut() {
            descriptors.sort_by(|a, b| {
                a.priority
                    .cmp(&b.priority)
                    .then_with(|| a.stable_id.cmp(&b.stable_id))
            });
        }
        for hooks in hooks_by_stage_contract.values_mut() {
            hooks.sort_by(|a, b| {
                a.priority
                    .cmp(&b.priority)
                    .then_with(|| a.offer_key.cmp(&b.offer_key))
            });
        }
        for subscriptions in subscriptions_by_contract.values_mut() {
            subscriptions.sort_by(|a, b| {
                a.priority
                    .cmp(&b.priority)
                    .then_with(|| a.offer_key.cmp(&b.offer_key))
            });
        }

        Ok(Self {
            capabilities_by_id,
            hooks_by_stage_contract,
            subscriptions_by_contract,
        })
    }

    pub fn capability_offers(&self, capability_id: &str) -> &[RuntimeCapabilityDescriptor] {
        self.capabilities_by_id
            .get(capability_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn discovered_capabilities(&self) -> Vec<&RuntimeCapabilityDescriptor> {
        self.capabilities_by_id
            .values()
            .flat_map(|items| items.iter())
            .collect()
    }

    pub fn discovered_hook_count(&self) -> usize {
        self.hooks_by_stage_contract.values().map(Vec::len).sum()
    }

    pub fn discovered_subscription_count(&self) -> usize {
        self.subscriptions_by_contract.values().map(Vec::len).sum()
    }

    pub fn resolve_capability(
        &self,
        capability_id: &str,
        contract_id: Option<&str>,
        scope: &ResolveScope,
        requested_op: Option<&str>,
    ) -> Option<CapabilityBinding> {
        self.capability_offers(capability_id)
            .iter()
            .find(|descriptor| {
                contract_matches(&descriptor.contract_id, contract_id)
                    && scope_matches(&descriptor.scope, scope)
                    && op_matches(&descriptor.applies_to_ops, requested_op)
            })
            .map(|descriptor| CapabilityBinding {
                cap_id: descriptor.capability_id.clone(),
                stable_id: descriptor.stable_id.clone(),
                pack_id: descriptor.provider_pack.clone(),
                domain: descriptor.domain,
                pack_path: descriptor.pack_path.clone(),
                provider_component_ref: descriptor.component_ref.clone(),
                provider_op: descriptor.entrypoint.clone(),
                version: descriptor.contract_id.clone(),
                requires_setup: descriptor.requires_setup,
                setup_qa_ref: descriptor.setup_qa_ref.clone(),
            })
    }

    pub fn resolve_capability_chain(
        &self,
        capability_id: &str,
        contract_id: Option<&str>,
        scope: &ResolveScope,
        requested_op: Option<&str>,
    ) -> Vec<CapabilityBinding> {
        self.capability_offers(capability_id)
            .iter()
            .filter(|descriptor| {
                contract_matches(&descriptor.contract_id, contract_id)
                    && scope_matches(&descriptor.scope, scope)
                    && op_matches(&descriptor.applies_to_ops, requested_op)
            })
            .map(|descriptor| CapabilityBinding {
                cap_id: descriptor.capability_id.clone(),
                stable_id: descriptor.stable_id.clone(),
                pack_id: descriptor.provider_pack.clone(),
                domain: descriptor.domain,
                pack_path: descriptor.pack_path.clone(),
                provider_component_ref: descriptor.component_ref.clone(),
                provider_op: descriptor.entrypoint.clone(),
                version: descriptor.contract_id.clone(),
                requires_setup: descriptor.requires_setup,
                setup_qa_ref: descriptor.setup_qa_ref.clone(),
            })
            .collect()
    }

    pub fn hook_chain(&self, stage: &str, contract_id: &str) -> &[RuntimeHookDescriptor] {
        self.hooks_by_stage_contract
            .get(&(stage.to_string(), contract_id.to_string()))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn subscriptions(&self, contract_id: &str) -> &[RuntimeSubscriptionDescriptor] {
        self.subscriptions_by_contract
            .get(contract_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn build_wiring_plan(
        &self,
        requirements: &[RuntimeProviderRequirement],
    ) -> RuntimeWiringPlan {
        let mut selected_providers = BTreeMap::new();
        let mut warnings = Vec::new();
        let mut blocking_failures = Vec::new();

        for requirement in requirements {
            let Some(selected) = self
                .capability_offers(&requirement.capability_id)
                .iter()
                .min_by(|a, b| {
                    a.priority
                        .cmp(&b.priority)
                        .then_with(|| a.stable_id.cmp(&b.stable_id))
                })
            else {
                let message = format!(
                    "missing {} provider capability {}",
                    if requirement.required {
                        "required"
                    } else {
                        "optional"
                    },
                    requirement.capability_id
                );
                if requirement.required {
                    blocking_failures.push(message);
                } else {
                    warnings.push(message);
                }
                continue;
            };
            selected_providers.insert(
                requirement.role_id.clone(),
                RuntimeSelectedProvider {
                    role_id: requirement.role_id.clone(),
                    capability_id: selected.capability_id.clone(),
                    contract_id: requirement.contract_id.clone(),
                    provider_pack: selected.provider_pack.clone(),
                    pack_path: selected.pack_path.clone(),
                    entrypoint: selected.entrypoint.clone(),
                    component_ref: selected.component_ref.clone(),
                    stable_id: selected.stable_id.clone(),
                },
            );
        }

        let mut hook_chains = BTreeMap::new();
        for ((stage, contract), descriptors) in &self.hooks_by_stage_contract {
            hook_chains.insert(format!("{stage}:{contract}"), descriptors.clone());
        }

        RuntimeWiringPlan {
            selected_providers,
            hook_chains,
            subscriptions_by_contract: self.subscriptions_by_contract.clone(),
            warnings,
            blocking_failures,
        }
    }
}

#[derive(Clone, Default)]
pub struct RuntimeSeams {
    pub session_provider: Option<Arc<dyn SessionProvider>>,
    pub state_provider: Option<Arc<dyn StateProvider>>,
    pub telemetry_provider: Option<Arc<dyn TelemetryProvider>>,
    pub observer_sink: Option<Arc<dyn ObserverHookSink>>,
    pub admin_authorization_hook: Option<Arc<dyn AdminAuthorizationHook>>,
    pub bundle_source: Option<Arc<dyn BundleSource>>,
    pub bundle_resolver: Option<Arc<dyn BundleResolver>>,
    pub bundle_fs: Option<Arc<dyn BundleFs>>,
}

#[derive(Clone)]
pub struct RuntimeCore {
    registry: RuntimeCapabilityRegistry,
    seams: RuntimeSeams,
    wiring_plan: RuntimeWiringPlan,
}

impl RuntimeCore {
    pub fn new(
        registry: RuntimeCapabilityRegistry,
        seams: RuntimeSeams,
        wiring_plan: RuntimeWiringPlan,
    ) -> Self {
        Self {
            registry,
            seams,
            wiring_plan,
        }
    }

    pub fn registry(&self) -> &RuntimeCapabilityRegistry {
        &self.registry
    }

    pub fn seams(&self) -> &RuntimeSeams {
        &self.seams
    }

    pub fn wiring_plan(&self) -> &RuntimeWiringPlan {
        &self.wiring_plan
    }

    pub async fn emit_telemetry_event(&self, event: RuntimeEvent) -> anyhow::Result<()> {
        if let Some(provider) = &self.seams.telemetry_provider {
            provider.emit(event).await?;
        }
        Ok(())
    }

    pub async fn publish_observer_event(&self, event: RuntimeEvent) -> anyhow::Result<()> {
        if let Some(sink) = &self.seams.observer_sink {
            sink.publish(event).await?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionKey {
    pub tenant: String,
    pub team: Option<String>,
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionRecord {
    pub revision: u64,
    pub route: Option<String>,
    pub bundle_assignment: Option<String>,
    pub context: JsonValue,
    pub expires_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScopedStateKey {
    pub tenant: String,
    pub team: Option<String>,
    pub scope: String,
    pub key: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeEvent {
    pub event_type: String,
    pub ts_unix_ms: u64,
    pub tenant: Option<String>,
    pub team: Option<String>,
    pub session_id: Option<String>,
    pub bundle_id: Option<String>,
    pub pack_id: Option<String>,
    pub flow_id: Option<String>,
    pub node_id: Option<String>,
    pub correlation_id: Option<String>,
    pub trace_id: Option<String>,
    pub severity: String,
    pub outcome: Option<String>,
    pub reason_codes: Vec<String>,
    pub payload: JsonValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthorizationDecision {
    Allow,
    Deny { reason: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdminAction {
    pub action: String,
    pub actor: String,
    pub resource: Option<String>,
}

#[async_trait]
pub trait SessionProvider: Send + Sync {
    async fn get(&self, key: &SessionKey) -> anyhow::Result<Option<SessionRecord>>;
    async fn put(&self, key: &SessionKey, record: SessionRecord) -> anyhow::Result<()>;
    async fn compare_and_set(
        &self,
        key: &SessionKey,
        expected_revision: u64,
        record: SessionRecord,
    ) -> anyhow::Result<bool>;
    async fn delete(&self, key: &SessionKey) -> anyhow::Result<()>;
    async fn find_by_user(
        &self,
        _tenant: &str,
        _team: Option<&str>,
        _user: &str,
    ) -> anyhow::Result<Option<(SessionKey, SessionRecord)>> {
        Ok(None)
    }
    async fn find_wait_by_scope(
        &self,
        _tenant: &str,
        _team: Option<&str>,
        _user: &str,
        _scope: &ReplyScope,
    ) -> anyhow::Result<Option<(SessionKey, SessionRecord)>> {
        Ok(None)
    }
    async fn health(&self) -> anyhow::Result<RuntimeHealth>;
}

#[async_trait]
pub trait StateProvider: Send + Sync {
    async fn get(&self, key: &ScopedStateKey) -> anyhow::Result<Option<JsonValue>>;
    async fn put(&self, key: &ScopedStateKey, value: JsonValue) -> anyhow::Result<()>;
    async fn compare_and_set(
        &self,
        _key: &ScopedStateKey,
        _expected: Option<JsonValue>,
        _value: JsonValue,
    ) -> anyhow::Result<Option<bool>> {
        Ok(None)
    }
    async fn delete(&self, key: &ScopedStateKey) -> anyhow::Result<()>;
    async fn health(&self) -> anyhow::Result<RuntimeHealth>;
}

#[async_trait]
pub trait TelemetryProvider: Send + Sync {
    async fn emit(&self, event: RuntimeEvent) -> anyhow::Result<()>;
    async fn health(&self) -> anyhow::Result<RuntimeHealth>;
}

#[async_trait]
pub trait ObserverHookSink: Send + Sync {
    async fn publish(&self, event: RuntimeEvent) -> anyhow::Result<()>;
    async fn health(&self) -> anyhow::Result<RuntimeHealth>;
}

#[async_trait]
pub trait AdminAuthorizationHook: Send + Sync {
    async fn authorize(&self, action: &AdminAction) -> anyhow::Result<AuthorizationDecision>;
    async fn health(&self) -> anyhow::Result<RuntimeHealth>;
}

#[async_trait]
pub trait BundleSource: Send + Sync {
    async fn stage(&self, bundle_ref: &str) -> anyhow::Result<PathBuf>;
    async fn health(&self) -> anyhow::Result<RuntimeHealth>;
}

#[async_trait]
pub trait BundleResolver: Send + Sync {
    async fn resolve(&self, bundle_ref: &str) -> anyhow::Result<String>;
    async fn health(&self) -> anyhow::Result<RuntimeHealth>;
}

#[async_trait]
pub trait BundleFs: Send + Sync {
    async fn read(&self, path: &str) -> anyhow::Result<Vec<u8>>;
    async fn exists(&self, path: &str) -> anyhow::Result<bool>;
    async fn list_dir(&self, path: &str) -> anyhow::Result<Vec<String>>;
    async fn health(&self) -> anyhow::Result<RuntimeHealth>;
}

pub fn require_started(core: &RuntimeCore) -> anyhow::Result<()> {
    if core.wiring_plan.blocking_failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "runtime wiring has blocking failures: {}",
            core.wiring_plan.blocking_failures.join("; ")
        ))
    }
}

pub fn default_provider_requirements() -> Vec<RuntimeProviderRequirement> {
    vec![
        RuntimeProviderRequirement {
            role_id: "session".to_string(),
            capability_id: CAP_SESSION_PROVIDER_V1.to_string(),
            contract_id: CONTRACT_SESSION_PROVIDER_V1.to_string(),
            required: false,
        },
        RuntimeProviderRequirement {
            role_id: "state".to_string(),
            capability_id: CAP_STATE_PROVIDER_V1.to_string(),
            contract_id: CONTRACT_STATE_PROVIDER_V1.to_string(),
            required: false,
        },
        RuntimeProviderRequirement {
            role_id: "telemetry".to_string(),
            capability_id: CAP_TELEMETRY_PROVIDER_V1.to_string(),
            contract_id: CONTRACT_TELEMETRY_PROVIDER_V1.to_string(),
            required: false,
        },
    ]
}

fn contract_matches(contract_id: &str, requested: Option<&str>) -> bool {
    match requested {
        None => true,
        Some(requested) => contract_id == requested,
    }
}

fn scope_matches(scope: &RuntimeScope, requested: &ResolveScope) -> bool {
    value_matches(&scope.envs, requested.env.as_deref())
        && value_matches(&scope.tenants, requested.tenant.as_deref())
        && value_matches(&scope.teams, requested.team.as_deref())
}

fn value_matches(values: &[String], current: Option<&str>) -> bool {
    if values.is_empty() {
        return true;
    }
    let Some(current) = current else {
        return false;
    };
    values.iter().any(|value| value == current)
}

fn op_matches(applies_to_ops: &[String], requested_op: Option<&str>) -> bool {
    let Some(requested_op) = requested_op else {
        return true;
    };
    if applies_to_ops.is_empty() {
        return true;
    }
    applies_to_ops.iter().any(|entry| entry == requested_op)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use greentic_types::{
        ExtensionInline, ExtensionRef, PackId, PackKind, PackManifest, PackSignatures,
    };
    use semver::Version;
    use serde_json::json;
    use tempfile::tempdir;
    use zip::ZipWriter;
    use zip::write::FileOptions;

    use super::*;
    use crate::domains::Domain;

    #[test]
    fn registry_discovers_capabilities_hooks_and_subscriptions() {
        let tmp = tempdir().expect("tempdir");
        let session_pack = tmp.path().join("session.gtpack");
        let observer_pack = tmp.path().join("observer.gtpack");
        write_test_pack(
            &session_pack,
            "session.provider",
            Domain::Messaging,
            json!({
                "schema_version": 1,
                "offers": [
                    {
                        "offer_id": "session.default",
                        "cap_id": CAP_SESSION_PROVIDER_V1,
                        "version": "greentic.contract.session.v1",
                        "provider": {"component_ref": "session.component", "op": "session.dispatch"},
                        "priority": 10
                    },
                    {
                        "offer_id": "state.default",
                        "cap_id": CAP_STATE_PROVIDER_V1,
                        "version": "greentic.contract.state.v1",
                        "provider": {"component_ref": "session.component", "op": "state.dispatch"},
                        "priority": 20
                    }
                ]
            }),
            json!({
                "offers": [
                    {
                        "id": "post_ingress.audit",
                        "kind": "hook",
                        "stage": "post_ingress",
                        "contract": "greentic.hook.control.v1",
                        "priority": 5,
                        "provider": {"op": "observer.post_ingress"}
                    },
                    {
                        "id": "bundle.lifecycle",
                        "kind": "subs",
                        "contract": "greentic.event.bundle_lifecycle.v1",
                        "priority": 9,
                        "provider": {"op": "observer.bundle_lifecycle"}
                    }
                ]
            }),
        )
        .expect("write session pack");
        write_test_pack(
            &observer_pack,
            "observer.pack",
            Domain::Events,
            json!({
                "schema_version": 1,
                "offers": []
            }),
            json!({
                "offers": []
            }),
        )
        .expect("write observer pack");

        let pack_index = BTreeMap::from([
            (
                session_pack.clone(),
                CapabilityPackRecord {
                    pack_id: "session.provider".to_string(),
                    domain: Domain::Messaging,
                },
            ),
            (
                observer_pack,
                CapabilityPackRecord {
                    pack_id: "observer.pack".to_string(),
                    domain: Domain::Events,
                },
            ),
        ]);
        let registry = RuntimeCapabilityRegistry::discover(&pack_index).expect("registry");

        assert_eq!(registry.capability_offers(CAP_SESSION_PROVIDER_V1).len(), 1);
        assert_eq!(
            registry
                .hook_chain("post_ingress", "greentic.hook.control.v1")
                .len(),
            1
        );
        assert_eq!(
            registry
                .subscriptions("greentic.event.bundle_lifecycle.v1")
                .len(),
            1
        );
    }

    #[test]
    fn wiring_plan_reports_missing_required_providers() {
        let registry = RuntimeCapabilityRegistry::default();
        let plan = registry.build_wiring_plan(&[
            RuntimeProviderRequirement {
                role_id: "session".to_string(),
                capability_id: CAP_SESSION_PROVIDER_V1.to_string(),
                contract_id: "greentic.contract.session.v1".to_string(),
                required: true,
            },
            RuntimeProviderRequirement {
                role_id: "telemetry".to_string(),
                capability_id: CAP_TELEMETRY_PROVIDER_V1.to_string(),
                contract_id: CONTRACT_TELEMETRY_PROVIDER_V1.to_string(),
                required: false,
            },
        ]);

        assert_eq!(plan.selected_providers.len(), 0);
        assert_eq!(plan.blocking_failures.len(), 1);
        assert_eq!(plan.warnings.len(), 1);
    }

    #[test]
    fn wiring_plan_selects_highest_priority_provider_for_default_requirements() {
        let registry = RuntimeCapabilityRegistry {
            capabilities_by_id: BTreeMap::from([(
                CAP_SESSION_PROVIDER_V1.to_string(),
                vec![
                    RuntimeCapabilityDescriptor {
                        capability_id: CAP_SESSION_PROVIDER_V1.to_string(),
                        contract_id: CONTRACT_SESSION_PROVIDER_V1.to_string(),
                        provider_pack: "pack.b".to_string(),
                        domain: Domain::Messaging,
                        pack_path: PathBuf::from("/tmp/b.gtpack"),
                        entrypoint: "session.low".to_string(),
                        component_ref: "session.component".to_string(),
                        priority: 50,
                        scope: RuntimeScope::default(),
                        applies_to_ops: Vec::new(),
                        lifecycle_state: RuntimeLifecycleState::Discovered,
                        health: RuntimeHealth::default(),
                        requires_setup: false,
                        setup_qa_ref: None,
                        stable_id: "pack.b::session".to_string(),
                    },
                    RuntimeCapabilityDescriptor {
                        capability_id: CAP_SESSION_PROVIDER_V1.to_string(),
                        contract_id: CONTRACT_SESSION_PROVIDER_V1.to_string(),
                        provider_pack: "pack.a".to_string(),
                        domain: Domain::Messaging,
                        pack_path: PathBuf::from("/tmp/a.gtpack"),
                        entrypoint: "session.high".to_string(),
                        component_ref: "session.component".to_string(),
                        priority: 10,
                        scope: RuntimeScope::default(),
                        applies_to_ops: Vec::new(),
                        lifecycle_state: RuntimeLifecycleState::Discovered,
                        health: RuntimeHealth::default(),
                        requires_setup: false,
                        setup_qa_ref: None,
                        stable_id: "pack.a::session".to_string(),
                    },
                ],
            )]),
            hooks_by_stage_contract: BTreeMap::new(),
            subscriptions_by_contract: BTreeMap::new(),
        };

        let plan = registry.build_wiring_plan(&default_provider_requirements());
        let selected = plan.selected_provider("session").expect("selected session");
        assert_eq!(selected.provider_pack, "pack.a");
        assert_eq!(selected.entrypoint, "session.high");
    }

    #[test]
    fn runtime_core_accepts_fake_seams() {
        let registry = RuntimeCapabilityRegistry::default();
        let seams = RuntimeSeams {
            session_provider: Some(Arc::new(FakeSessionProvider)),
            state_provider: Some(Arc::new(FakeStateProvider)),
            telemetry_provider: Some(Arc::new(FakeTelemetryProvider::default())),
            observer_sink: Some(Arc::new(FakeObserverSink::default())),
            admin_authorization_hook: Some(Arc::new(FakeAdminAuthHook)),
            bundle_source: Some(Arc::new(FakeBundleSource)),
            bundle_resolver: Some(Arc::new(FakeBundleResolver)),
            bundle_fs: Some(Arc::new(FakeBundleFs)),
        };
        let core = RuntimeCore::new(registry, seams, RuntimeWiringPlan::default());

        assert!(require_started(&core).is_ok());
        assert!(core.seams().session_provider.is_some());
        assert!(core.seams().bundle_fs.is_some());
    }

    #[test]
    fn resolve_capability_uses_scope_and_requested_op() {
        let registry = RuntimeCapabilityRegistry {
            capabilities_by_id: BTreeMap::from([(
                "greentic.cap.test".to_string(),
                vec![
                    RuntimeCapabilityDescriptor {
                        capability_id: "greentic.cap.test".to_string(),
                        contract_id: "greentic.contract.test.v1".to_string(),
                        provider_pack: "pack.default".to_string(),
                        domain: Domain::Messaging,
                        pack_path: PathBuf::from("/tmp/default.gtpack"),
                        entrypoint: "default.dispatch".to_string(),
                        component_ref: "component".to_string(),
                        priority: 50,
                        scope: RuntimeScope::default(),
                        applies_to_ops: vec!["op.default".to_string()],
                        lifecycle_state: RuntimeLifecycleState::Discovered,
                        health: RuntimeHealth::default(),
                        requires_setup: false,
                        setup_qa_ref: None,
                        stable_id: "pack.default::offer".to_string(),
                    },
                    RuntimeCapabilityDescriptor {
                        capability_id: "greentic.cap.test".to_string(),
                        contract_id: "greentic.contract.test.v1".to_string(),
                        provider_pack: "pack.tenant".to_string(),
                        domain: Domain::Messaging,
                        pack_path: PathBuf::from("/tmp/tenant.gtpack"),
                        entrypoint: "tenant.dispatch".to_string(),
                        component_ref: "component".to_string(),
                        priority: 10,
                        scope: RuntimeScope {
                            envs: Vec::new(),
                            tenants: vec!["tenant-a".to_string()],
                            teams: Vec::new(),
                        },
                        applies_to_ops: vec!["op.special".to_string()],
                        lifecycle_state: RuntimeLifecycleState::Discovered,
                        health: RuntimeHealth::default(),
                        requires_setup: false,
                        setup_qa_ref: None,
                        stable_id: "pack.tenant::offer".to_string(),
                    },
                ],
            )]),
            hooks_by_stage_contract: BTreeMap::new(),
            subscriptions_by_contract: BTreeMap::new(),
        };

        let resolved = registry
            .resolve_capability(
                "greentic.cap.test",
                Some("greentic.contract.test.v1"),
                &ResolveScope {
                    env: None,
                    tenant: Some("tenant-a".to_string()),
                    team: None,
                },
                Some("op.special"),
            )
            .expect("resolved");
        assert_eq!(resolved.pack_id, "pack.tenant");
        assert_eq!(resolved.provider_op, "tenant.dispatch");
    }

    struct FakeSessionProvider;
    struct FakeStateProvider;
    #[derive(Default)]
    struct FakeTelemetryProvider {
        events: std::sync::Mutex<Vec<RuntimeEvent>>,
    }
    #[derive(Default)]
    struct FakeObserverSink {
        events: std::sync::Mutex<Vec<RuntimeEvent>>,
    }
    struct FakeAdminAuthHook;
    struct FakeBundleSource;
    struct FakeBundleResolver;
    struct FakeBundleFs;

    #[async_trait]
    impl SessionProvider for FakeSessionProvider {
        async fn get(&self, _key: &SessionKey) -> anyhow::Result<Option<SessionRecord>> {
            Ok(None)
        }

        async fn put(&self, _key: &SessionKey, _record: SessionRecord) -> anyhow::Result<()> {
            Ok(())
        }

        async fn compare_and_set(
            &self,
            _key: &SessionKey,
            _expected_revision: u64,
            _record: SessionRecord,
        ) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn delete(&self, _key: &SessionKey) -> anyhow::Result<()> {
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
    impl StateProvider for FakeStateProvider {
        async fn get(&self, _key: &ScopedStateKey) -> anyhow::Result<Option<JsonValue>> {
            Ok(None)
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
    impl TelemetryProvider for FakeTelemetryProvider {
        async fn emit(&self, event: RuntimeEvent) -> anyhow::Result<()> {
            self.events.lock().expect("telemetry events").push(event);
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
    impl ObserverHookSink for FakeObserverSink {
        async fn publish(&self, event: RuntimeEvent) -> anyhow::Result<()> {
            self.events.lock().expect("observer events").push(event);
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
    impl AdminAuthorizationHook for FakeAdminAuthHook {
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

    #[async_trait]
    impl BundleSource for FakeBundleSource {
        async fn stage(&self, bundle_ref: &str) -> anyhow::Result<PathBuf> {
            Ok(PathBuf::from(bundle_ref))
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    #[async_trait]
    impl BundleResolver for FakeBundleResolver {
        async fn resolve(&self, bundle_ref: &str) -> anyhow::Result<String> {
            Ok(bundle_ref.to_string())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    #[async_trait]
    impl BundleFs for FakeBundleFs {
        async fn read(&self, _path: &str) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }

        async fn exists(&self, _path: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn list_dir(&self, _path: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    fn write_test_pack(
        path: &std::path::Path,
        pack_id: &str,
        _domain: Domain,
        capabilities_inline: JsonValue,
        offers_inline: JsonValue,
    ) -> anyhow::Result<()> {
        let mut extensions = BTreeMap::new();
        extensions.insert(
            "greentic.ext.capabilities.v1".to_string(),
            ExtensionRef {
                kind: "greentic.ext.capabilities.v1".to_string(),
                version: "1.0.0".to_string(),
                digest: None,
                location: None,
                inline: Some(ExtensionInline::Other(capabilities_inline)),
            },
        );
        extensions.insert(
            "greentic.ext.offers.v1".to_string(),
            ExtensionRef {
                kind: "greentic.ext.offers.v1".to_string(),
                version: "1.0.0".to_string(),
                digest: None,
                location: None,
                inline: Some(ExtensionInline::Other(offers_inline)),
            },
        );

        let manifest = PackManifest {
            schema_version: "pack-v1".to_string(),
            pack_id: PackId::new(pack_id)?,
            name: None,
            version: Version::parse("0.1.0")?,
            kind: PackKind::Provider,
            publisher: "demo".to_string(),
            components: Vec::new(),
            flows: Vec::new(),
            dependencies: Vec::new(),
            capabilities: Vec::new(),
            secret_requirements: Vec::new(),
            signatures: PackSignatures::default(),
            bootstrap: None,
            extensions: Some(extensions),
        };

        let bytes = greentic_types::encode_pack_manifest(&manifest)?;
        let file = std::fs::File::create(path)?;
        let mut zip = ZipWriter::new(file);
        zip.start_file("manifest.cbor", FileOptions::<()>::default())?;
        zip.write_all(&bytes)?;
        zip.finish()?;
        Ok(())
    }

    #[test]
    fn runtime_core_emits_events_to_optional_seams() {
        let telemetry = Arc::new(FakeTelemetryProvider::default());
        let observer = Arc::new(FakeObserverSink::default());
        let seams = RuntimeSeams {
            telemetry_provider: Some(telemetry.clone()),
            observer_sink: Some(observer.clone()),
            ..RuntimeSeams::default()
        };
        let core = RuntimeCore::new(
            RuntimeCapabilityRegistry::default(),
            seams,
            RuntimeWiringPlan::default(),
        );
        let event = RuntimeEvent {
            event_type: "runtime.pre_op".to_string(),
            ts_unix_ms: 1,
            tenant: Some("tenant-a".to_string()),
            team: Some("team-a".to_string()),
            session_id: None,
            bundle_id: Some("bundle-a".to_string()),
            pack_id: Some("provider.pack".to_string()),
            flow_id: Some("flow-a".to_string()),
            node_id: Some("node-a".to_string()),
            correlation_id: Some("corr-a".to_string()),
            trace_id: None,
            severity: "info".to_string(),
            outcome: Some("pending".to_string()),
            reason_codes: vec!["runtime.pre_op".to_string()],
            payload: json!({"op": "send_message"}),
        };
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        runtime
            .block_on(core.emit_telemetry_event(event.clone()))
            .expect("emit telemetry");
        runtime
            .block_on(core.publish_observer_event(event.clone()))
            .expect("publish observer");

        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let observer_events = observer.events.lock().expect("observer events");
        assert_eq!(telemetry_events.as_slice(), &[event.clone()]);
        assert_eq!(observer_events.as_slice(), &[event]);
    }

    #[test]
    fn wiring_plan_reports_subscription_provider_presence() {
        let plan = RuntimeWiringPlan {
            hook_chains: BTreeMap::from([(
                "post_ingress:greentic.hook.control.v1".to_string(),
                vec![RuntimeHookDescriptor {
                    offer_key: "observer.pack::post_ingress.audit".to_string(),
                    provider_pack: "observer.pack".to_string(),
                    pack_path: PathBuf::from("/tmp/observer.gtpack"),
                    stage: "post_ingress".to_string(),
                    contract_id: "greentic.hook.control.v1".to_string(),
                    entrypoint: "observer.post_ingress".to_string(),
                    priority: 5,
                    lifecycle_state: RuntimeLifecycleState::Discovered,
                    health: RuntimeHealth::default(),
                }],
            )]),
            subscriptions_by_contract: BTreeMap::from([(
                "greentic.event.bundle_lifecycle.v1".to_string(),
                vec![RuntimeSubscriptionDescriptor {
                    offer_key: "observer.pack::bundle.lifecycle".to_string(),
                    provider_pack: "observer.pack".to_string(),
                    pack_path: PathBuf::from("/tmp/observer.gtpack"),
                    contract_id: "greentic.event.bundle_lifecycle.v1".to_string(),
                    entrypoint: "observer.bundle_lifecycle".to_string(),
                    priority: 10,
                    lifecycle_state: RuntimeLifecycleState::Discovered,
                    health: RuntimeHealth::default(),
                }],
            )]),
            ..RuntimeWiringPlan::default()
        };

        assert!(plan.has_subscription_provider("observer.pack", None));
        assert!(plan.has_subscription_provider(
            "observer.pack",
            Some("greentic.event.bundle_lifecycle.v1")
        ));
        assert_eq!(
            plan.hook_chain("post_ingress", "greentic.hook.control.v1")
                .len(),
            1
        );
        assert!(
            !plan.has_subscription_provider("observer.pack", Some("greentic.event.unknown.v1"))
        );
        assert!(!plan.has_subscription_provider("other.pack", None));
    }
}
