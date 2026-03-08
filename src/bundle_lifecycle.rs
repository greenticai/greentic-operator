use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::bundle_access::{
    BundleAccessDiagnostics, BundleAccessHandle, operator_bundle_access_config,
};
use crate::capabilities::CapabilityPackRecord;
use crate::discovery;
use crate::domains::{self, Domain, ProviderPack};
use crate::runtime_core::{
    RuntimeCapabilityRegistry, RuntimeProviderRequirement, RuntimeWiringPlan,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleLifecycleState {
    Staged,
    Warming,
    Ready,
    Active,
    Draining,
    Retired,
}

#[derive(Clone, Debug, Serialize)]
pub struct BundleWarmReport {
    pub bundle_id: String,
    pub access: BundleAccessDiagnostics,
    pub provider_pack_count: usize,
    pub capability_count: usize,
    pub hook_count: usize,
    pub subscription_count: usize,
    pub selected_provider_roles: Vec<String>,
    pub hook_chain_keys: Vec<String>,
    pub subscription_contracts: Vec<String>,
    pub duplicate_pack_conflicts: Vec<String>,
    pub warnings: Vec<String>,
    pub blocking_failures: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BundleRecordSnapshot {
    pub bundle_id: String,
    pub bundle_ref: PathBuf,
    pub state: BundleLifecycleState,
    pub access: BundleAccessDiagnostics,
    pub warm_report: BundleWarmReport,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct BundleLifecycleSnapshot {
    pub active_bundle_id: Option<String>,
    pub previous_bundle_id: Option<String>,
    pub bundles: Vec<BundleRecordSnapshot>,
    pub events: Vec<BundleLifecycleEvent>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BundleLifecycleEvent {
    pub kind: String,
    pub bundle_id: String,
    pub from_state: Option<BundleLifecycleState>,
    pub to_state: BundleLifecycleState,
    pub previous_active_bundle_id: Option<String>,
}

#[derive(Clone)]
struct BundleInventoryArtifacts {
    catalog: BTreeMap<(Domain, String), ProviderPack>,
    packs_by_path: BTreeMap<PathBuf, ProviderPack>,
}

pub type BundleInventorySnapshot = (
    BTreeMap<(Domain, String), ProviderPack>,
    BTreeMap<PathBuf, ProviderPack>,
);

#[derive(Clone)]
struct BundleRuntimeArtifacts {
    #[allow(dead_code)]
    registry: RuntimeCapabilityRegistry,
    #[allow(dead_code)]
    wiring_plan: RuntimeWiringPlan,
}

#[derive(Clone)]
struct BundleRecord {
    snapshot: BundleRecordSnapshot,
    access_handle: BundleAccessHandle,
    inventory: BundleInventoryArtifacts,
    #[allow(dead_code)]
    runtime: Option<BundleRuntimeArtifacts>,
}

#[derive(Clone, Default)]
pub struct BundleLifecycleRegistry {
    bundles: BTreeMap<String, BundleRecord>,
    active_bundle_id: Option<String>,
    previous_bundle_id: Option<String>,
    events: Vec<BundleLifecycleEvent>,
}

impl BundleLifecycleRegistry {
    pub fn stage_bundle(&mut self, bundle_ref: &Path) -> anyhow::Result<String> {
        let staged = stage_bundle(bundle_ref)?;
        let bundle_id = staged.snapshot.bundle_id.clone();
        let prior_state = self
            .bundles
            .get(&bundle_id)
            .map(|record| record.snapshot.state);
        self.events.push(BundleLifecycleEvent {
            kind: "stage".to_string(),
            bundle_id: bundle_id.clone(),
            from_state: prior_state,
            to_state: BundleLifecycleState::Staged,
            previous_active_bundle_id: self.active_bundle_id.clone(),
        });
        self.bundles.insert(bundle_id.clone(), staged);
        Ok(bundle_id)
    }

    pub fn warm_and_activate(
        &mut self,
        bundle_ref: &Path,
        requirements: &[RuntimeProviderRequirement],
    ) -> anyhow::Result<(String, RuntimeCapabilityRegistry, RuntimeWiringPlan)> {
        let bundle_id = self.warm_bundle(bundle_ref, requirements)?;
        self.activate(&bundle_id)?;
        let (registry, wiring_plan) = self
            .runtime_artifacts(&bundle_id)
            .ok_or_else(|| anyhow::anyhow!("bundle {bundle_id} missing runtime artifacts"))?;
        Ok((bundle_id, registry, wiring_plan))
    }

    pub fn warm_bundle(
        &mut self,
        bundle_ref: &Path,
        requirements: &[RuntimeProviderRequirement],
    ) -> anyhow::Result<String> {
        let warmed = warm_bundle(bundle_ref, requirements)?;
        let bundle_id = warmed.snapshot.bundle_id.clone();
        self.events.push(BundleLifecycleEvent {
            kind: "warm".to_string(),
            bundle_id: bundle_id.clone(),
            from_state: Some(BundleLifecycleState::Warming),
            to_state: warmed.snapshot.state,
            previous_active_bundle_id: self.active_bundle_id.clone(),
        });
        self.bundles.insert(bundle_id.clone(), warmed);
        Ok(bundle_id)
    }

    pub fn warm_bundle_id(
        &mut self,
        bundle_id: &str,
        requirements: &[RuntimeProviderRequirement],
    ) -> anyhow::Result<String> {
        let bundle_ref = self
            .bundles
            .get(bundle_id)
            .map(|record| record.snapshot.bundle_ref.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown bundle id {bundle_id}"))?;
        self.warm_bundle(&bundle_ref, requirements)
    }

    pub fn runtime_artifacts(
        &self,
        bundle_id: &str,
    ) -> Option<(RuntimeCapabilityRegistry, RuntimeWiringPlan)> {
        self.bundles.get(bundle_id).and_then(|record| {
            record
                .runtime
                .as_ref()
                .map(|runtime| (runtime.registry.clone(), runtime.wiring_plan.clone()))
        })
    }

    pub fn access_handle(&self, bundle_id: &str) -> Option<BundleAccessHandle> {
        self.bundles
            .get(bundle_id)
            .map(|record| record.access_handle.clone())
    }

    pub fn active_access_handle(&self) -> Option<BundleAccessHandle> {
        self.active_bundle_id
            .as_deref()
            .and_then(|bundle_id| self.access_handle(bundle_id))
    }

    pub fn inventory(&self, bundle_id: &str) -> Option<BundleInventorySnapshot> {
        self.bundles.get(bundle_id).map(|record| {
            (
                record.inventory.catalog.clone(),
                record.inventory.packs_by_path.clone(),
            )
        })
    }

    pub fn register_active_bundle(
        &mut self,
        access_handle: BundleAccessHandle,
        bundle_ref: PathBuf,
        access: BundleAccessDiagnostics,
        registry: RuntimeCapabilityRegistry,
        wiring_plan: RuntimeWiringPlan,
        provider_packs: Vec<ProviderPack>,
    ) -> String {
        let bundle_id = bundle_id_for(&access, &bundle_ref);
        let snapshot = BundleRecordSnapshot {
            bundle_id: bundle_id.clone(),
            bundle_ref,
            state: BundleLifecycleState::Active,
            warm_report: build_warm_report(
                &bundle_id,
                &access,
                &provider_packs,
                Some(&registry),
                Some(&wiring_plan),
                Vec::new(),
            ),
            access,
        };
        self.previous_bundle_id = self.active_bundle_id.clone();
        self.active_bundle_id = Some(bundle_id.clone());
        self.events.push(BundleLifecycleEvent {
            kind: "register_active".to_string(),
            bundle_id: bundle_id.clone(),
            from_state: None,
            to_state: BundleLifecycleState::Active,
            previous_active_bundle_id: self.previous_bundle_id.clone(),
        });
        self.bundles.insert(
            bundle_id.clone(),
            BundleRecord {
                snapshot,
                access_handle,
                inventory: BundleInventoryArtifacts {
                    catalog: BTreeMap::new(),
                    packs_by_path: BTreeMap::new(),
                },
                runtime: Some(BundleRuntimeArtifacts {
                    registry,
                    wiring_plan,
                }),
            },
        );
        bundle_id
    }

    pub fn activate(&mut self, bundle_id: &str) -> anyhow::Result<()> {
        let Some(target) = self.bundles.get(bundle_id) else {
            anyhow::bail!("unknown bundle id {bundle_id}");
        };
        let target_state = target.snapshot.state;
        if target.snapshot.state != BundleLifecycleState::Ready
            && target.snapshot.state != BundleLifecycleState::Draining
            && target.snapshot.state != BundleLifecycleState::Active
        {
            anyhow::bail!("bundle {bundle_id} is not ready for activation");
        }
        if !target.snapshot.warm_report.blocking_failures.is_empty() {
            anyhow::bail!("bundle {bundle_id} has blocking warm failures");
        }

        let previous_active = self.active_bundle_id.clone();
        if let Some(current_id) = previous_active.as_ref()
            && let Some(current) = self.bundles.get_mut(current_id)
        {
            current.snapshot.state = BundleLifecycleState::Draining;
        }
        if let Some(next) = self.bundles.get_mut(bundle_id) {
            next.snapshot.state = BundleLifecycleState::Active;
        }
        self.previous_bundle_id = previous_active;
        self.active_bundle_id = Some(bundle_id.to_string());
        self.events.push(BundleLifecycleEvent {
            kind: "activate".to_string(),
            bundle_id: bundle_id.to_string(),
            from_state: Some(target_state),
            to_state: BundleLifecycleState::Active,
            previous_active_bundle_id: self.previous_bundle_id.clone(),
        });
        Ok(())
    }

    pub fn rollback(&mut self) -> anyhow::Result<()> {
        let Some(previous_id) = self.previous_bundle_id.clone() else {
            anyhow::bail!("no previous bundle available for rollback");
        };
        let Some(current_id) = self.active_bundle_id.clone() else {
            anyhow::bail!("no active bundle to roll back from");
        };
        let previous_state = self
            .bundles
            .get(&previous_id)
            .map(|record| record.snapshot.state)
            .unwrap_or(BundleLifecycleState::Draining);
        if current_id == previous_id {
            anyhow::bail!("active bundle already matches previous bundle");
        }
        if let Some(current) = self.bundles.get_mut(&current_id) {
            current.snapshot.state = BundleLifecycleState::Draining;
        }
        if let Some(previous) = self.bundles.get_mut(&previous_id) {
            previous.snapshot.state = BundleLifecycleState::Active;
        }
        self.active_bundle_id = Some(previous_id.clone());
        self.previous_bundle_id = Some(current_id);
        self.events.push(BundleLifecycleEvent {
            kind: "rollback".to_string(),
            bundle_id: previous_id,
            from_state: Some(previous_state),
            to_state: BundleLifecycleState::Active,
            previous_active_bundle_id: self.previous_bundle_id.clone(),
        });
        Ok(())
    }

    pub fn complete_drain(&mut self, bundle_id: &str) -> anyhow::Result<()> {
        let Some(record) = self.bundles.get_mut(bundle_id) else {
            anyhow::bail!("unknown bundle id {bundle_id}");
        };
        if self.active_bundle_id.as_deref() == Some(bundle_id) {
            anyhow::bail!("cannot retire active bundle {bundle_id}");
        }
        if record.snapshot.state != BundleLifecycleState::Draining {
            anyhow::bail!("bundle {bundle_id} is not draining");
        }
        record.snapshot.state = BundleLifecycleState::Retired;
        if self.previous_bundle_id.as_deref() == Some(bundle_id) {
            self.previous_bundle_id = None;
        }
        self.events.push(BundleLifecycleEvent {
            kind: "complete_drain".to_string(),
            bundle_id: bundle_id.to_string(),
            from_state: Some(BundleLifecycleState::Draining),
            to_state: BundleLifecycleState::Retired,
            previous_active_bundle_id: self.active_bundle_id.clone(),
        });
        Ok(())
    }

    pub fn snapshot(&self) -> BundleLifecycleSnapshot {
        BundleLifecycleSnapshot {
            active_bundle_id: self.active_bundle_id.clone(),
            previous_bundle_id: self.previous_bundle_id.clone(),
            bundles: self
                .bundles
                .values()
                .map(|record| record.snapshot.clone())
                .collect(),
            events: self.events.clone(),
        }
    }
}

fn stage_bundle(bundle_ref: &Path) -> anyhow::Result<BundleRecord> {
    let bundle_access =
        BundleAccessHandle::open(bundle_ref, &operator_bundle_access_config(bundle_ref))?;
    let access = bundle_access.diagnostics().clone();
    let bundle_id = bundle_id_for(&access, bundle_ref);
    let cbor_only = discovery::bundle_cbor_only(bundle_ref, bundle_access.active_root());
    let provider_packs = discover_provider_packs(bundle_access.active_root(), cbor_only)?;
    let inventory = build_inventory(bundle_ref, bundle_access.active_root(), cbor_only)?;
    let mut warm_report =
        build_warm_report(&bundle_id, &access, &provider_packs, None, None, Vec::new());
    warm_report
        .warnings
        .push("bundle staged but not warmed".to_string());
    let snapshot = BundleRecordSnapshot {
        bundle_id: bundle_id.clone(),
        bundle_ref: bundle_ref.to_path_buf(),
        state: BundleLifecycleState::Staged,
        access,
        warm_report,
    };
    Ok(BundleRecord {
        snapshot,
        access_handle: bundle_access,
        inventory,
        runtime: None,
    })
}

fn warm_bundle(
    bundle_ref: &Path,
    requirements: &[RuntimeProviderRequirement],
) -> anyhow::Result<BundleRecord> {
    let bundle_access =
        BundleAccessHandle::open(bundle_ref, &operator_bundle_access_config(bundle_ref))?;
    let access = bundle_access.diagnostics().clone();
    let bundle_id = bundle_id_for(&access, bundle_ref);
    let cbor_only = discovery::bundle_cbor_only(bundle_ref, bundle_access.active_root());
    let provider_packs = discover_provider_packs(bundle_access.active_root(), cbor_only)?;
    let inventory = build_inventory(bundle_ref, bundle_access.active_root(), cbor_only)?;
    let duplicate_pack_conflicts = duplicate_pack_conflicts(&provider_packs);

    let mut pack_index = BTreeMap::new();
    for pack in &provider_packs {
        let Some(domain) = infer_pack_domain(bundle_access.active_root(), pack) else {
            continue;
        };
        pack_index.insert(
            pack.path.clone(),
            CapabilityPackRecord {
                pack_id: pack.pack_id.clone(),
                domain,
            },
        );
    }

    let (registry, wiring_plan, mut blocking_failures) = if duplicate_pack_conflicts.is_empty() {
        let registry = RuntimeCapabilityRegistry::discover(&pack_index)?;
        let wiring_plan = registry.build_wiring_plan(requirements);
        let blocking_failures = wiring_plan.blocking_failures.clone();
        (Some(registry), Some(wiring_plan), blocking_failures)
    } else {
        (None, None, duplicate_pack_conflicts.clone())
    };

    let snapshot = BundleRecordSnapshot {
        bundle_id: bundle_id.clone(),
        bundle_ref: bundle_ref.to_path_buf(),
        state: if blocking_failures.is_empty() {
            BundleLifecycleState::Ready
        } else {
            BundleLifecycleState::Staged
        },
        warm_report: build_warm_report(
            &bundle_id,
            &access,
            &provider_packs,
            registry.as_ref(),
            wiring_plan.as_ref(),
            duplicate_pack_conflicts,
        ),
        access,
    };
    blocking_failures.clear();

    Ok(BundleRecord {
        snapshot,
        access_handle: bundle_access,
        inventory,
        runtime: registry
            .zip(wiring_plan)
            .map(|(registry, wiring_plan)| BundleRuntimeArtifacts {
                registry,
                wiring_plan,
            }),
    })
}

fn build_inventory(
    bundle_ref: &Path,
    active_root: &Path,
    cbor_only: bool,
) -> anyhow::Result<BundleInventoryArtifacts> {
    let runtime_discovery = if active_root == bundle_ref {
        discovery::discover(bundle_ref)?
    } else {
        discovery::discover_runtime_bundle(bundle_ref, active_root)?
    };
    let provider_map = runtime_discovery
        .providers
        .iter()
        .map(|provider| (provider.pack_path.clone(), provider.provider_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut catalog = BTreeMap::new();
    let mut packs_by_path = BTreeMap::new();
    for domain in [
        Domain::Messaging,
        Domain::Events,
        Domain::Secrets,
        Domain::OAuth,
    ] {
        let packs = domains::discover_provider_packs_with_options(active_root, domain, cbor_only)?;
        for pack in packs {
            packs_by_path.insert(pack.path.clone(), pack.clone());
            let provider_type = provider_map
                .get(&pack.path)
                .cloned()
                .unwrap_or_else(|| pack.pack_id.clone());
            catalog.insert((domain, provider_type.clone()), pack.clone());
            if provider_type != pack.pack_id {
                catalog.insert((domain, pack.pack_id.clone()), pack.clone());
            }
        }
    }
    Ok(BundleInventoryArtifacts {
        catalog,
        packs_by_path,
    })
}

fn build_warm_report(
    bundle_id: &str,
    access: &BundleAccessDiagnostics,
    provider_packs: &[ProviderPack],
    registry: Option<&RuntimeCapabilityRegistry>,
    wiring_plan: Option<&RuntimeWiringPlan>,
    duplicate_pack_conflicts: Vec<String>,
) -> BundleWarmReport {
    let warnings = wiring_plan
        .map(|plan| plan.warnings.clone())
        .unwrap_or_default();
    let selected_provider_roles = wiring_plan
        .map(|plan| plan.selected_providers.keys().cloned().collect())
        .unwrap_or_default();
    let hook_chain_keys = wiring_plan
        .map(|plan| plan.hook_chains.keys().cloned().collect())
        .unwrap_or_default();
    let subscription_contracts = wiring_plan
        .map(|plan| plan.subscriptions_by_contract.keys().cloned().collect())
        .unwrap_or_default();
    let mut blocking_failures = duplicate_pack_conflicts.clone();
    if let Some(plan) = wiring_plan {
        blocking_failures.extend(plan.blocking_failures.clone());
    }
    BundleWarmReport {
        bundle_id: bundle_id.to_string(),
        access: access.clone(),
        provider_pack_count: provider_packs.len(),
        capability_count: registry
            .map(|registry| registry.discovered_capabilities().len())
            .unwrap_or_default(),
        hook_count: registry
            .map(|registry| registry.discovered_hook_count())
            .unwrap_or_default(),
        subscription_count: registry
            .map(|registry| registry.discovered_subscription_count())
            .unwrap_or_default(),
        selected_provider_roles,
        hook_chain_keys,
        subscription_contracts,
        duplicate_pack_conflicts,
        warnings,
        blocking_failures,
    }
}

fn duplicate_pack_conflicts(provider_packs: &[ProviderPack]) -> Vec<String> {
    let mut by_pack_id: BTreeMap<&str, BTreeSet<&Path>> = BTreeMap::new();
    for pack in provider_packs {
        by_pack_id
            .entry(&pack.pack_id)
            .or_default()
            .insert(pack.path.as_path());
    }
    by_pack_id
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(pack_id, paths)| {
            format!(
                "duplicate provider pack id {pack_id} across {}",
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect()
}

fn discover_provider_packs(root: &Path, cbor_only: bool) -> anyhow::Result<Vec<ProviderPack>> {
    let mut packs = Vec::new();
    for domain in [
        Domain::Messaging,
        Domain::Events,
        Domain::Secrets,
        Domain::OAuth,
    ] {
        packs.extend(domains::discover_provider_packs_with_options(
            root, domain, cbor_only,
        )?);
    }
    Ok(packs)
}

fn infer_pack_domain(root: &Path, pack: &ProviderPack) -> Option<Domain> {
    [
        Domain::Messaging,
        Domain::Events,
        Domain::Secrets,
        Domain::OAuth,
    ]
    .into_iter()
    .find(|domain| {
        let providers_root = root.join(domains::config(*domain).providers_dir);
        pack.path.starts_with(&providers_root)
    })
    .or_else(|| {
        pack.path
            .parent()
            .filter(|parent| *parent == root.join("packs"))
            .map(|_| Domain::Messaging)
    })
}

fn bundle_id_for(access: &BundleAccessDiagnostics, bundle_ref: &Path) -> String {
    access
        .bundle_digest_sha256
        .clone()
        .unwrap_or_else(|| bundle_ref.display().to_string())
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
    use crate::runtime_core::{CAP_SESSION_PROVIDER_V1, CONTRACT_SESSION_PROVIDER_V1};

    fn write_test_pack(
        path: &Path,
        pack_id: &str,
        _domain: Domain,
        capability_extension: serde_json::Value,
        offers_extension: serde_json::Value,
    ) {
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
                inline: Some(ExtensionInline::Other(offers_extension)),
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
        zip.write_all(&encoded).expect("write manifest");
        zip.finish().expect("finish zip");
    }

    #[test]
    fn warm_bundle_discovers_capabilities_and_wiring() {
        let tmp = tempdir().expect("tempdir");
        let providers = tmp.path().join("providers").join("messaging");
        std::fs::create_dir_all(&providers).expect("providers dir");
        let pack_path = providers.join("session.gtpack");
        write_test_pack(
            &pack_path,
            "session.provider",
            Domain::Messaging,
            json!({
                "schema_version": 1,
                "offers": [{
                    "offer_id": "session.default",
                    "cap_id": CAP_SESSION_PROVIDER_V1,
                    "version": CONTRACT_SESSION_PROVIDER_V1,
                    "provider": {"component_ref": "session.component", "op": "session.dispatch"},
                    "priority": 10
                }]
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
        );

        let mut registry = BundleLifecycleRegistry::default();
        let bundle_id = registry
            .warm_bundle(
                tmp.path(),
                &crate::runtime_core::default_provider_requirements(),
            )
            .expect("warm bundle");
        let snapshot = registry.snapshot();
        let record = snapshot
            .bundles
            .iter()
            .find(|bundle| bundle.bundle_id == bundle_id)
            .expect("bundle record");

        assert_eq!(record.state, BundleLifecycleState::Ready);
        assert_eq!(record.warm_report.provider_pack_count, 1);
        assert_eq!(record.warm_report.capability_count, 1);
        assert_eq!(record.warm_report.hook_count, 1);
        assert_eq!(record.warm_report.subscription_count, 1);
        assert_eq!(
            record.warm_report.selected_provider_roles,
            vec!["session".to_string()]
        );
        assert_eq!(
            record.warm_report.hook_chain_keys,
            vec!["post_ingress:greentic.hook.control.v1".to_string()]
        );
        assert_eq!(
            record.warm_report.subscription_contracts,
            vec!["greentic.event.bundle_lifecycle.v1".to_string()]
        );
        assert!(record.warm_report.blocking_failures.is_empty());
        assert_eq!(snapshot.events.len(), 1);
        assert_eq!(snapshot.events[0].kind, "warm");
        assert_eq!(snapshot.events[0].bundle_id, bundle_id);
        assert_eq!(
            snapshot.events[0].from_state,
            Some(BundleLifecycleState::Warming)
        );
        assert_eq!(snapshot.events[0].to_state, BundleLifecycleState::Ready);
    }

    #[test]
    fn warm_bundle_reports_duplicate_provider_conflicts() {
        let tmp = tempdir().expect("tempdir");
        let providers = tmp.path().join("providers").join("messaging");
        std::fs::create_dir_all(&providers).expect("providers dir");
        write_test_pack(
            &providers.join("a.gtpack"),
            "dup.provider",
            Domain::Messaging,
            json!({"schema_version": 1, "offers": []}),
            json!({"offers": []}),
        );
        write_test_pack(
            &providers.join("b.gtpack"),
            "dup.provider",
            Domain::Messaging,
            json!({"schema_version": 1, "offers": []}),
            json!({"offers": []}),
        );

        let mut registry = BundleLifecycleRegistry::default();
        let bundle_id = registry
            .warm_bundle(
                tmp.path(),
                &crate::runtime_core::default_provider_requirements(),
            )
            .expect("warm bundle");
        let snapshot = registry.snapshot();
        let record = snapshot
            .bundles
            .iter()
            .find(|bundle| bundle.bundle_id == bundle_id)
            .expect("bundle record");

        assert_eq!(record.state, BundleLifecycleState::Staged);
        assert_eq!(record.warm_report.duplicate_pack_conflicts.len(), 1);
        assert!(record.warm_report.blocking_failures[0].contains("duplicate provider pack id"));
    }

    #[test]
    fn activate_and_rollback_swap_active_bundle() {
        let tmp = tempdir().expect("tempdir");
        let providers_a = tmp
            .path()
            .join("bundle-a")
            .join("providers")
            .join("messaging");
        let providers_b = tmp
            .path()
            .join("bundle-b")
            .join("providers")
            .join("messaging");
        std::fs::create_dir_all(&providers_a).expect("providers a");
        std::fs::create_dir_all(&providers_b).expect("providers b");
        write_test_pack(
            &providers_a.join("session-a.gtpack"),
            "session.a",
            Domain::Messaging,
            json!({"schema_version": 1, "offers": []}),
            json!({"offers": []}),
        );
        write_test_pack(
            &providers_b.join("session-b.gtpack"),
            "session.b",
            Domain::Messaging,
            json!({"schema_version": 1, "offers": []}),
            json!({"offers": []}),
        );

        let access_a = BundleAccessDiagnostics {
            mode: crate::bundle_access::BundleAccessMode::Directory,
            bundle_ref: tmp.path().join("bundle-a"),
            active_root: tmp.path().join("bundle-a"),
            fallback_reason: None,
            bundle_digest_sha256: Some("a".to_string()),
            warm_status: "not_applicable".to_string(),
        };
        let registry = RuntimeCapabilityRegistry::default();
        let plan = RuntimeWiringPlan::default();
        let mut bundles = BundleLifecycleRegistry::default();
        let handle_a = BundleAccessHandle::open(
            tmp.path().join("bundle-a"),
            &operator_bundle_access_config(&tmp.path().join("bundle-a")),
        )
        .expect("bundle access a");
        let active = bundles.register_active_bundle(
            handle_a,
            tmp.path().join("bundle-a"),
            access_a,
            registry.clone(),
            plan.clone(),
            Vec::new(),
        );
        let ready = bundles
            .warm_bundle(
                &tmp.path().join("bundle-b"),
                &crate::runtime_core::default_provider_requirements(),
            )
            .expect("warm bundle b");

        bundles.activate(&ready).expect("activate");
        let snapshot = bundles.snapshot();
        assert_eq!(snapshot.active_bundle_id.as_deref(), Some(ready.as_str()));
        assert_eq!(
            snapshot.previous_bundle_id.as_deref(),
            Some(active.as_str())
        );
        assert_eq!(
            snapshot.events.last().map(|event| event.kind.as_str()),
            Some("activate")
        );

        bundles.rollback().expect("rollback");
        let snapshot = bundles.snapshot();
        assert_eq!(snapshot.active_bundle_id.as_deref(), Some(active.as_str()));
        assert_eq!(snapshot.previous_bundle_id.as_deref(), Some(ready.as_str()));
        let rollback = snapshot.events.last().expect("rollback event");
        assert_eq!(rollback.kind, "rollback");
        assert_eq!(rollback.bundle_id, active);
        assert_eq!(rollback.to_state, BundleLifecycleState::Active);
    }

    #[test]
    fn complete_drain_retires_previous_bundle() {
        let tmp = tempdir().expect("tempdir");
        let providers_a = tmp
            .path()
            .join("bundle-a")
            .join("providers")
            .join("messaging");
        let providers_b = tmp
            .path()
            .join("bundle-b")
            .join("providers")
            .join("messaging");
        std::fs::create_dir_all(&providers_a).expect("providers a");
        std::fs::create_dir_all(&providers_b).expect("providers b");
        write_test_pack(
            &providers_a.join("session-a.gtpack"),
            "session.a",
            Domain::Messaging,
            json!({"schema_version": 1, "offers": []}),
            json!({"offers": []}),
        );
        write_test_pack(
            &providers_b.join("session-b.gtpack"),
            "session.b",
            Domain::Messaging,
            json!({"schema_version": 1, "offers": []}),
            json!({"offers": []}),
        );

        let access_a = BundleAccessDiagnostics {
            mode: crate::bundle_access::BundleAccessMode::Directory,
            bundle_ref: tmp.path().join("bundle-a"),
            active_root: tmp.path().join("bundle-a"),
            fallback_reason: None,
            bundle_digest_sha256: Some("a".to_string()),
            warm_status: "not_applicable".to_string(),
        };
        let registry = RuntimeCapabilityRegistry::default();
        let plan = RuntimeWiringPlan::default();
        let mut bundles = BundleLifecycleRegistry::default();
        let handle_a = BundleAccessHandle::open(
            tmp.path().join("bundle-a"),
            &operator_bundle_access_config(&tmp.path().join("bundle-a")),
        )
        .expect("bundle access a");
        let active = bundles.register_active_bundle(
            handle_a,
            tmp.path().join("bundle-a"),
            access_a,
            registry.clone(),
            plan.clone(),
            Vec::new(),
        );
        let ready = bundles
            .warm_bundle(
                &tmp.path().join("bundle-b"),
                &crate::runtime_core::default_provider_requirements(),
            )
            .expect("warm bundle b");

        bundles.activate(&ready).expect("activate");
        bundles
            .complete_drain(&active)
            .expect("retire drained bundle");

        let snapshot = bundles.snapshot();
        assert_eq!(snapshot.active_bundle_id.as_deref(), Some(ready.as_str()));
        assert_eq!(snapshot.previous_bundle_id, None);
        let retired = snapshot
            .bundles
            .iter()
            .find(|bundle| bundle.bundle_id == active)
            .expect("retired bundle");
        assert_eq!(retired.state, BundleLifecycleState::Retired);
        let drain = snapshot.events.last().expect("complete_drain event");
        assert_eq!(drain.kind, "complete_drain");
        assert_eq!(drain.bundle_id, active);
        assert_eq!(drain.to_state, BundleLifecycleState::Retired);
    }
}
