use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};

use crate::gmap::{self, Policy};
use crate::project;

#[derive(Clone, Debug, Serialize)]
pub struct QaQuestion {
    pub id: String,
    pub title: String,
    pub required: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct QaSpec {
    pub mode: String,
    pub questions: Vec<QaQuestion>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WizardMode {
    Create,
    Update,
    Remove,
}

impl WizardMode {
    pub fn as_str(self) -> &'static str {
        match self {
            WizardMode::Create => "create",
            WizardMode::Update => "update",
            WizardMode::Remove => "remove",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WizardPlan {
    pub mode: String,
    pub dry_run: bool,
    pub bundle: PathBuf,
    pub steps: Vec<WizardPlanStep>,
    pub metadata: WizardPlanMetadata,
}

#[derive(Clone, Debug, Serialize)]
pub struct WizardPlanMetadata {
    pub bundle_name: Option<String>,
    pub pack_refs: Vec<String>,
    pub tenants: Vec<TenantSelection>,
    pub default_assignments: Vec<PackDefaultSelection>,
    pub providers: Vec<String>,
    pub update_ops: BTreeSet<WizardUpdateOp>,
    pub remove_targets: BTreeSet<WizardRemoveTarget>,
    pub packs_remove: Vec<PackRemoveSelection>,
    pub providers_remove: Vec<String>,
    pub tenants_remove: Vec<TenantSelection>,
    pub access_changes: Vec<AccessChangeSelection>,
    pub setup_answers: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WizardPlanStep {
    pub kind: WizardStepKind,
    pub description: String,
    pub details: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WizardStepKind {
    NoOp,
    ResolvePacks,
    CreateBundle,
    AddPacksToBundle,
    ApplyPackSetup,
    WriteGmapRules,
    RunResolver,
    CopyResolvedManifest,
    ValidateBundle,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackListing {
    pub id: String,
    pub label: String,
    pub reference: String,
}

pub trait CatalogSource {
    fn list(&self) -> Vec<PackListing>;
}

#[derive(Clone, Debug, Default)]
pub struct StaticCatalogSource;

impl CatalogSource for StaticCatalogSource {
    fn list(&self) -> Vec<PackListing> {
        // Listing only; fetching is delegated to distributor client in execution.
        vec![
            PackListing {
                id: "messaging-telegram".to_string(),
                label: "Messaging Telegram".to_string(),
                reference: "repo://messaging/providers/messaging-telegram@latest".to_string(),
            },
            PackListing {
                id: "messaging-slack".to_string(),
                label: "Messaging Slack".to_string(),
                reference: "repo://messaging/providers/messaging-slack@latest".to_string(),
            },
        ]
    }
}

pub fn load_catalog_from_file(path: &Path) -> anyhow::Result<Vec<PackListing>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read catalog file {}", path.display()))?;
    if let Ok(parsed) = serde_json::from_str::<Vec<PackListing>>(&raw)
        .or_else(|_| serde_yaml_bw::from_str::<Vec<PackListing>>(&raw))
    {
        return Ok(parsed);
    }
    let registry: ProviderRegistryFile = serde_json::from_str(&raw)
        .or_else(|_| serde_yaml_bw::from_str(&raw))
        .with_context(|| format!("parse catalog/provider registry file {}", path.display()))?;
    Ok(registry
        .items
        .into_iter()
        .map(|item| PackListing {
            id: item.id,
            label: item.label.fallback,
            reference: item.reference,
        })
        .collect())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProviderRegistryFile {
    #[serde(default)]
    registry_version: Option<String>,
    #[serde(default)]
    items: Vec<ProviderRegistryItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProviderRegistryItem {
    id: String,
    label: ProviderRegistryLabel,
    #[serde(alias = "ref")]
    reference: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProviderRegistryLabel {
    #[serde(default)]
    i18n_key: Option<String>,
    fallback: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TenantSelection {
    pub tenant: String,
    pub team: Option<String>,
    pub allow_paths: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WizardUpdateOp {
    PacksAdd,
    PacksRemove,
    ProvidersAdd,
    ProvidersRemove,
    TenantsAdd,
    TenantsRemove,
    AccessChange,
}

impl WizardUpdateOp {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "packs_add" => Some(Self::PacksAdd),
            "packs_remove" => Some(Self::PacksRemove),
            "providers_add" => Some(Self::ProvidersAdd),
            "providers_remove" => Some(Self::ProvidersRemove),
            "tenants_add" => Some(Self::TenantsAdd),
            "tenants_remove" => Some(Self::TenantsRemove),
            "access_change" => Some(Self::AccessChange),
            _ => None,
        }
    }
}

impl FromStr for WizardUpdateOp {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WizardRemoveTarget {
    Packs,
    Providers,
    TenantsTeams,
}

impl WizardRemoveTarget {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "packs" => Some(Self::Packs),
            "providers" => Some(Self::Providers),
            "tenants_teams" => Some(Self::TenantsTeams),
            _ => None,
        }
    }
}

impl FromStr for WizardRemoveTarget {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackScope {
    Bundle,
    Global,
    Tenant { tenant_id: String },
    Team { tenant_id: String, team_id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackRemoveSelection {
    pub pack_identifier: String,
    #[serde(default)]
    pub scope: Option<PackScope>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackDefaultSelection {
    pub pack_identifier: String,
    pub scope: PackScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessOperation {
    AllowAdd,
    AllowRemove,
}

impl AccessOperation {
    pub fn policy(self) -> Policy {
        match self {
            AccessOperation::AllowAdd => Policy::Public,
            AccessOperation::AllowRemove => Policy::Forbidden,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessChangeSelection {
    pub pack_id: String,
    pub operation: AccessOperation,
    pub tenant_id: String,
    #[serde(default)]
    pub team_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WizardCreateRequest {
    pub bundle: PathBuf,
    pub bundle_name: Option<String>,
    pub pack_refs: Vec<String>,
    pub tenants: Vec<TenantSelection>,
    pub default_assignments: Vec<PackDefaultSelection>,
    pub providers: Vec<String>,
    pub update_ops: BTreeSet<WizardUpdateOp>,
    pub remove_targets: BTreeSet<WizardRemoveTarget>,
    pub packs_remove: Vec<PackRemoveSelection>,
    pub providers_remove: Vec<String>,
    pub tenants_remove: Vec<TenantSelection>,
    pub access_changes: Vec<AccessChangeSelection>,
    /// Per-provider setup answers to seed as secrets during bundle creation.
    pub setup_answers: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolvedPackInfo {
    pub source_ref: String,
    pub mapped_ref: String,
    pub resolved_digest: String,
    pub pack_id: String,
    pub entry_flows: Vec<String>,
    pub cached_path: PathBuf,
    pub output_path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct WizardExecutionReport {
    pub bundle: PathBuf,
    pub resolved_packs: Vec<ResolvedPackInfo>,
    pub resolved_manifests: Vec<PathBuf>,
    pub provider_updates: usize,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct PacksMetadata {
    #[serde(default)]
    packs: Vec<PackMappingRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PackMappingRecord {
    pack_id: String,
    original_ref: String,
    local_path_in_bundle: String,
    #[serde(default)]
    digest: Option<String>,
}

pub fn spec(mode: WizardMode) -> QaSpec {
    QaSpec {
        mode: mode.as_str().to_string(),
        questions: vec![
            QaQuestion {
                id: "operator.bundle.path".to_string(),
                title: "Bundle output path".to_string(),
                required: true,
            },
            QaQuestion {
                id: "operator.packs.refs".to_string(),
                title: "Pack refs (catalog + custom)".to_string(),
                required: false,
            },
            QaQuestion {
                id: "operator.tenants".to_string(),
                title: "Tenants and optional teams".to_string(),
                required: true,
            },
            QaQuestion {
                id: "operator.allow.paths".to_string(),
                title: "Allow rules as PACK[/FLOW[/NODE]]".to_string(),
                required: false,
            },
        ],
    }
}

pub fn apply_create(request: &WizardCreateRequest, dry_run: bool) -> anyhow::Result<WizardPlan> {
    if request.tenants.is_empty() {
        return Err(anyhow!("at least one tenant selection is required"));
    }

    let mut pack_refs = request
        .pack_refs
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    pack_refs.sort();
    pack_refs.dedup();

    let mut tenants = request.tenants.clone();
    for tenant in &mut tenants {
        tenant.allow_paths.sort();
        tenant.allow_paths.dedup();
    }
    tenants.sort_by(|a, b| {
        a.tenant
            .cmp(&b.tenant)
            .then_with(|| a.team.cmp(&b.team))
            .then_with(|| a.allow_paths.cmp(&b.allow_paths))
    });

    let mut steps = Vec::new();
    if !pack_refs.is_empty() {
        steps.push(step(
            WizardStepKind::ResolvePacks,
            "Resolve selected pack refs via distributor client",
            [("count", pack_refs.len().to_string())],
        ));
    } else {
        steps.push(step(
            WizardStepKind::NoOp,
            "No pack refs selected; skipping pack resolution",
            [("reason", "empty_pack_refs".to_string())],
        ));
    }
    steps.push(step(
        WizardStepKind::CreateBundle,
        "Create demo bundle scaffold using existing conventions",
        [("bundle", request.bundle.display().to_string())],
    ));
    if !pack_refs.is_empty() {
        steps.push(step(
            WizardStepKind::AddPacksToBundle,
            "Copy fetched packs into bundle/packs",
            [("count", pack_refs.len().to_string())],
        ));
        steps.push(step(
            WizardStepKind::ApplyPackSetup,
            "Apply pack-declared setup outputs through internal setup hooks",
            [("status", "planned".to_string())],
        ));
    } else {
        steps.push(step(
            WizardStepKind::NoOp,
            "No fetched packs to add or setup",
            [("reason", "empty_pack_refs".to_string())],
        ));
    }
    steps.push(step(
        WizardStepKind::WriteGmapRules,
        "Write tenant/team allow rules to gmap",
        [("targets", tenants.len().to_string())],
    ));
    steps.push(step(
        WizardStepKind::RunResolver,
        "Run resolver pipeline (same as demo allow)",
        [("resolver", "project::sync_project".to_string())],
    ));
    steps.push(step(
        WizardStepKind::CopyResolvedManifest,
        "Copy state/resolved manifests into resolved/ for demo start",
        [("targets", tenants.len().to_string())],
    ));
    steps.push(step(
        WizardStepKind::ValidateBundle,
        "Validate bundle is loadable by internal demo pipeline",
        [("check", "resolved manifests present".to_string())],
    ));

    Ok(WizardPlan {
        mode: "create".to_string(),
        dry_run,
        bundle: request.bundle.clone(),
        steps,
        metadata: WizardPlanMetadata {
            bundle_name: request.bundle_name.clone(),
            pack_refs,
            tenants,
            default_assignments: request.default_assignments.clone(),
            providers: request.providers.clone(),
            update_ops: request.update_ops.clone(),
            remove_targets: request.remove_targets.clone(),
            packs_remove: request.packs_remove.clone(),
            providers_remove: request.providers_remove.clone(),
            tenants_remove: request.tenants_remove.clone(),
            access_changes: request.access_changes.clone(),
            setup_answers: request.setup_answers.clone(),
        },
    })
}

pub fn apply_update(request: &WizardCreateRequest, dry_run: bool) -> anyhow::Result<WizardPlan> {
    let mut pack_refs = request
        .pack_refs
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    pack_refs.sort();
    pack_refs.dedup();

    let mut tenants = request.tenants.clone();
    for tenant in &mut tenants {
        tenant.allow_paths.sort();
        tenant.allow_paths.dedup();
    }
    tenants.sort_by(|a, b| {
        a.tenant
            .cmp(&b.tenant)
            .then_with(|| a.team.cmp(&b.team))
            .then_with(|| a.allow_paths.cmp(&b.allow_paths))
    });

    let mut ops = request.update_ops.clone();
    if ops.is_empty() {
        if !pack_refs.is_empty() {
            ops.insert(WizardUpdateOp::PacksAdd);
        }
        if !request.providers.is_empty() {
            ops.insert(WizardUpdateOp::ProvidersAdd);
        }
        if !request.providers_remove.is_empty() {
            ops.insert(WizardUpdateOp::ProvidersRemove);
        }
        if !request.packs_remove.is_empty() {
            ops.insert(WizardUpdateOp::PacksRemove);
        }
        if !tenants.is_empty() {
            ops.insert(WizardUpdateOp::TenantsAdd);
        }
        if !request.tenants_remove.is_empty() {
            ops.insert(WizardUpdateOp::TenantsRemove);
        }
        if !request.access_changes.is_empty()
            || tenants.iter().any(|tenant| !tenant.allow_paths.is_empty())
        {
            ops.insert(WizardUpdateOp::AccessChange);
        }
    }

    let mut steps = vec![step(
        WizardStepKind::ValidateBundle,
        "Validate target bundle exists before update",
        [("mode", "update".to_string())],
    )];
    if ops.is_empty() {
        steps.push(step(
            WizardStepKind::NoOp,
            "No update operations selected",
            [("reason", "empty_update_ops".to_string())],
        ));
    }
    if ops.contains(&WizardUpdateOp::PacksAdd) {
        if pack_refs.is_empty() {
            steps.push(step(
                WizardStepKind::NoOp,
                "packs_add selected without pack refs",
                [("reason", "empty_pack_refs".to_string())],
            ));
        } else {
            steps.push(step(
                WizardStepKind::ResolvePacks,
                "Resolve selected pack refs via distributor client",
                [("count", pack_refs.len().to_string())],
            ));
            steps.push(step(
                WizardStepKind::AddPacksToBundle,
                "Copy fetched packs into bundle/packs",
                [("count", pack_refs.len().to_string())],
            ));
        }
    }
    if ops.contains(&WizardUpdateOp::PacksRemove) {
        if request.packs_remove.is_empty() {
            steps.push(step(
                WizardStepKind::NoOp,
                "packs_remove selected without targets",
                [("reason", "empty_packs_remove".to_string())],
            ));
        } else {
            steps.push(step(
                WizardStepKind::AddPacksToBundle,
                "Remove pack artifacts/default links from bundle",
                [("count", request.packs_remove.len().to_string())],
            ));
        }
    }
    if ops.contains(&WizardUpdateOp::ProvidersAdd) {
        if request.providers.is_empty() && pack_refs.is_empty() {
            steps.push(step(
                WizardStepKind::NoOp,
                "providers_add selected without providers or new packs",
                [("reason", "empty_providers_add".to_string())],
            ));
        } else {
            steps.push(step(
                WizardStepKind::ApplyPackSetup,
                "Enable providers in providers/providers.json",
                [("count", request.providers.len().to_string())],
            ));
        }
    }
    if ops.contains(&WizardUpdateOp::ProvidersRemove) {
        if request.providers_remove.is_empty() {
            steps.push(step(
                WizardStepKind::NoOp,
                "providers_remove selected without providers",
                [("reason", "empty_providers_remove".to_string())],
            ));
        } else {
            steps.push(step(
                WizardStepKind::ApplyPackSetup,
                "Disable/remove providers in providers/providers.json",
                [("count", request.providers_remove.len().to_string())],
            ));
        }
    }
    if ops.contains(&WizardUpdateOp::TenantsAdd) {
        if tenants.is_empty() {
            steps.push(step(
                WizardStepKind::NoOp,
                "tenants_add selected without tenant targets",
                [("reason", "empty_tenants_add".to_string())],
            ));
        } else {
            steps.push(step(
                WizardStepKind::WriteGmapRules,
                "Ensure tenant/team directories and allow rules",
                [("targets", tenants.len().to_string())],
            ));
        }
    }
    if ops.contains(&WizardUpdateOp::TenantsRemove) {
        if request.tenants_remove.is_empty() {
            steps.push(step(
                WizardStepKind::NoOp,
                "tenants_remove selected without tenant targets",
                [("reason", "empty_tenants_remove".to_string())],
            ));
        } else {
            steps.push(step(
                WizardStepKind::WriteGmapRules,
                "Remove tenant/team directories and related rules",
                [("targets", request.tenants_remove.len().to_string())],
            ));
        }
    }
    if ops.contains(&WizardUpdateOp::AccessChange) {
        let access_count = request.access_changes.len()
            + tenants
                .iter()
                .filter(|tenant| !tenant.allow_paths.is_empty())
                .count();
        if access_count == 0 {
            steps.push(step(
                WizardStepKind::NoOp,
                "access_change selected without mutations",
                [("reason", "empty_access_changes".to_string())],
            ));
        } else {
            steps.push(step(
                WizardStepKind::WriteGmapRules,
                "Apply access rule updates",
                [("changes", access_count.to_string())],
            ));
            steps.push(step(
                WizardStepKind::RunResolver,
                "Run resolver pipeline (same as demo allow/forbid)",
                [("resolver", "project::sync_project".to_string())],
            ));
            steps.push(step(
                WizardStepKind::CopyResolvedManifest,
                "Copy state/resolved manifests into resolved/ for demo start",
                [("targets", tenants.len().to_string())],
            ));
        }
    }
    steps.push(step(
        WizardStepKind::ValidateBundle,
        "Validate bundle is loadable by internal demo pipeline",
        [("check", "resolved manifests present".to_string())],
    ));

    Ok(WizardPlan {
        mode: WizardMode::Update.as_str().to_string(),
        dry_run,
        bundle: request.bundle.clone(),
        steps,
        metadata: WizardPlanMetadata {
            bundle_name: request.bundle_name.clone(),
            pack_refs,
            tenants,
            default_assignments: request.default_assignments.clone(),
            providers: request.providers.clone(),
            update_ops: ops,
            remove_targets: request.remove_targets.clone(),
            packs_remove: request.packs_remove.clone(),
            providers_remove: request.providers_remove.clone(),
            tenants_remove: request.tenants_remove.clone(),
            access_changes: request.access_changes.clone(),
            setup_answers: request.setup_answers.clone(),
        },
    })
}

pub fn apply_remove(request: &WizardCreateRequest, dry_run: bool) -> anyhow::Result<WizardPlan> {
    let mut tenants = request.tenants.clone();
    for tenant in &mut tenants {
        tenant.allow_paths.sort();
        tenant.allow_paths.dedup();
    }
    tenants.sort_by(|a, b| {
        a.tenant
            .cmp(&b.tenant)
            .then_with(|| a.team.cmp(&b.team))
            .then_with(|| a.allow_paths.cmp(&b.allow_paths))
    });

    let mut targets = request.remove_targets.clone();
    if targets.is_empty() {
        if !request.packs_remove.is_empty() {
            targets.insert(WizardRemoveTarget::Packs);
        }
        if !request.providers_remove.is_empty() {
            targets.insert(WizardRemoveTarget::Providers);
        }
        if !request.tenants_remove.is_empty() {
            targets.insert(WizardRemoveTarget::TenantsTeams);
        }
    }

    let mut steps = vec![step(
        WizardStepKind::ValidateBundle,
        "Validate target bundle exists before remove",
        [("mode", "remove".to_string())],
    )];
    if targets.is_empty() {
        steps.push(step(
            WizardStepKind::NoOp,
            "No remove targets selected",
            [("reason", "empty_remove_targets".to_string())],
        ));
    }
    if targets.contains(&WizardRemoveTarget::Packs) {
        if request.packs_remove.is_empty() {
            steps.push(step(
                WizardStepKind::NoOp,
                "packs target selected without pack identifiers",
                [("reason", "empty_packs_remove".to_string())],
            ));
        } else {
            steps.push(step(
                WizardStepKind::AddPacksToBundle,
                "Delete pack files/default links from bundle",
                [("count", request.packs_remove.len().to_string())],
            ));
        }
    }
    if targets.contains(&WizardRemoveTarget::Providers) {
        if request.providers_remove.is_empty() {
            steps.push(step(
                WizardStepKind::NoOp,
                "providers target selected without provider ids",
                [("reason", "empty_providers_remove".to_string())],
            ));
        } else {
            steps.push(step(
                WizardStepKind::ApplyPackSetup,
                "Remove provider entries from providers/providers.json",
                [("count", request.providers_remove.len().to_string())],
            ));
        }
    }
    if targets.contains(&WizardRemoveTarget::TenantsTeams) {
        if request.tenants_remove.is_empty() {
            steps.push(step(
                WizardStepKind::NoOp,
                "tenants_teams target selected without tenant/team ids",
                [("reason", "empty_tenants_remove".to_string())],
            ));
        } else {
            steps.push(step(
                WizardStepKind::WriteGmapRules,
                "Delete tenant/team directories and access rules",
                [("count", request.tenants_remove.len().to_string())],
            ));
            steps.push(step(
                WizardStepKind::RunResolver,
                "Run resolver pipeline after tenant/team removals",
                [("resolver", "project::sync_project".to_string())],
            ));
            steps.push(step(
                WizardStepKind::CopyResolvedManifest,
                "Copy state/resolved manifests into resolved/ for demo start",
                [("targets", tenants.len().to_string())],
            ));
        }
    }
    steps.push(step(
        WizardStepKind::ValidateBundle,
        "Validate bundle is loadable by internal demo pipeline",
        [("check", "resolved manifests present".to_string())],
    ));

    Ok(WizardPlan {
        mode: WizardMode::Remove.as_str().to_string(),
        dry_run,
        bundle: request.bundle.clone(),
        steps,
        metadata: WizardPlanMetadata {
            bundle_name: request.bundle_name.clone(),
            pack_refs: Vec::new(),
            tenants,
            default_assignments: request.default_assignments.clone(),
            providers: request.providers.clone(),
            update_ops: request.update_ops.clone(),
            remove_targets: targets,
            packs_remove: request.packs_remove.clone(),
            providers_remove: request.providers_remove.clone(),
            tenants_remove: request.tenants_remove.clone(),
            access_changes: request.access_changes.clone(),
            setup_answers: request.setup_answers.clone(),
        },
    })
}

pub fn apply(
    mode: WizardMode,
    request: &WizardCreateRequest,
    dry_run: bool,
) -> anyhow::Result<WizardPlan> {
    match mode {
        WizardMode::Create => apply_create(request, dry_run),
        WizardMode::Update => apply_update(request, dry_run),
        WizardMode::Remove => apply_remove(request, dry_run),
    }
}

pub fn normalize_request_for_plan(
    request: &WizardCreateRequest,
) -> anyhow::Result<WizardCreateRequest> {
    let mut normalized = request.clone();
    for selection in &mut normalized.default_assignments {
        selection.pack_identifier =
            canonical_pack_identifier(&normalized.bundle, &selection.pack_identifier)?;
    }
    for selection in &mut normalized.packs_remove {
        selection.pack_identifier =
            canonical_pack_identifier(&normalized.bundle, &selection.pack_identifier)?;
    }
    for change in &mut normalized.access_changes {
        change.pack_id = canonical_pack_identifier(&normalized.bundle, &change.pack_id)?;
    }
    Ok(normalized)
}

pub fn execute_plan(
    mode: WizardMode,
    plan: &WizardPlan,
    offline: bool,
) -> anyhow::Result<WizardExecutionReport> {
    match mode {
        WizardMode::Create => execute_create_plan(plan, offline),
        WizardMode::Update => execute_update_plan(plan, offline),
        WizardMode::Remove => execute_remove_plan(plan),
    }
}

fn validate_bundle_exists(bundle: &Path) -> anyhow::Result<()> {
    if !bundle.exists() {
        return Err(anyhow!("bundle path {} does not exist", bundle.display()));
    }
    if !bundle.join("greentic.demo.yaml").exists() {
        return Err(anyhow!(
            "bundle {} missing greentic.demo.yaml",
            bundle.display()
        ));
    }
    Ok(())
}

pub fn print_plan_summary(plan: &WizardPlan) {
    println!(
        "{} mode={} dry_run={}",
        crate::operator_i18n::tr("cli.wizard.plan_header", "wizard plan:"),
        plan.mode,
        plan.dry_run
    );
    println!(
        "{} {}",
        crate::operator_i18n::tr("cli.wizard.bundle", "bundle:"),
        plan.bundle.display()
    );
    let noop_count = plan
        .steps
        .iter()
        .filter(|step| step.kind == WizardStepKind::NoOp)
        .count();
    if noop_count > 0 {
        println!(
            "{} {}",
            crate::operator_i18n::tr("cli.wizard.noop_steps", "no-op steps:"),
            noop_count
        );
    }
    for (index, step) in plan.steps.iter().enumerate() {
        println!(
            "{}. {}",
            index + 1,
            localized_step_description(&step.description)
        );
    }
}

fn localized_step_description(description: &str) -> String {
    match description {
        "Resolve selected pack refs via distributor client" => crate::operator_i18n::tr(
            "cli.wizard.step.resolve_packs",
            "Resolve selected pack refs via distributor client",
        ),
        "Create demo bundle scaffold using existing conventions" => crate::operator_i18n::tr(
            "cli.wizard.step.create_bundle",
            "Create demo bundle scaffold using existing conventions",
        ),
        "Copy fetched packs into bundle/packs" => crate::operator_i18n::tr(
            "cli.wizard.step.copy_packs",
            "Copy fetched packs into bundle/packs",
        ),
        "Apply pack-declared setup outputs through internal setup hooks" => {
            crate::operator_i18n::tr(
                "cli.wizard.step.apply_pack_setup",
                "Apply pack-declared setup outputs through internal setup hooks",
            )
        }
        "Write tenant/team allow rules to gmap" => crate::operator_i18n::tr(
            "cli.wizard.step.write_gmap",
            "Write tenant/team allow rules to gmap",
        ),
        "Run resolver pipeline (same as demo allow)" => crate::operator_i18n::tr(
            "cli.wizard.step.run_resolver_create",
            "Run resolver pipeline (same as demo allow)",
        ),
        "Copy state/resolved manifests into resolved/ for demo start" => crate::operator_i18n::tr(
            "cli.wizard.step.copy_resolved",
            "Copy state/resolved manifests into resolved/ for demo start",
        ),
        "Validate bundle is loadable by internal demo pipeline" => crate::operator_i18n::tr(
            "cli.wizard.step.validate_bundle",
            "Validate bundle is loadable by internal demo pipeline",
        ),
        _ => description.to_string(),
    }
}

pub fn execute_create_plan(
    plan: &WizardPlan,
    offline: bool,
) -> anyhow::Result<WizardExecutionReport> {
    if plan.mode != WizardMode::Create.as_str() {
        return Err(anyhow!("unsupported wizard mode: {}", plan.mode));
    }

    if plan.bundle.exists() {
        return Err(anyhow!(
            "bundle path {} already exists",
            plan.bundle.display()
        ));
    }

    create_demo_bundle_structure(&plan.bundle, plan.metadata.bundle_name.as_deref())?;

    let mut resolved_packs = Vec::new();
    if !plan.metadata.pack_refs.is_empty() {
        let mut resolved = resolve_pack_refs(&plan.metadata.pack_refs, offline)
            .context("resolve pack refs via distributor-client")?;
        assign_pack_ids_and_persist_metadata(&plan.bundle, &mut resolved)?;
        for item in resolved {
            copy_pack_into_bundle(&plan.bundle, &item)?;
            resolved_packs.push(item);
        }
        link_packs_to_provider_dirs(&plan.bundle, &resolved_packs)?;
    }
    let mut warnings = Vec::new();
    let mut provider_updates = upsert_provider_registry(&plan.bundle, &resolved_packs)?;
    if !plan.metadata.default_assignments.is_empty() {
        apply_default_assignments(
            &plan.bundle,
            &plan.metadata.default_assignments,
            &mut warnings,
        )?;
    }
    if !plan.metadata.providers.is_empty() {
        provider_updates +=
            upsert_provider_ids(&plan.bundle, &plan.metadata.providers, &mut warnings)?;
    }

    // Seed secrets from setup_answers for each tenant.
    if !plan.metadata.setup_answers.is_empty() {
        seed_setup_answers(
            &plan.bundle,
            &plan.metadata.tenants,
            &plan.metadata.setup_answers,
            &mut warnings,
        )?;

        // Auto-register webhooks using answers (Telegram, Slack, Webex).
        run_webhook_setup_from_answers(
            &plan.bundle,
            &plan.metadata.tenants,
            &plan.metadata.setup_answers,
        );
    }

    let copied = apply_access_and_sync(
        &plan.bundle,
        &plan.metadata.tenants,
        &plan.metadata.access_changes,
        &mut warnings,
    )?;

    Ok(WizardExecutionReport {
        bundle: plan.bundle.clone(),
        resolved_packs,
        resolved_manifests: copied,
        provider_updates,
        warnings,
    })
}

pub fn execute_update_plan(
    plan: &WizardPlan,
    offline: bool,
) -> anyhow::Result<WizardExecutionReport> {
    if plan.mode != WizardMode::Update.as_str() {
        return Err(anyhow!("unsupported wizard mode: {}", plan.mode));
    }
    validate_bundle_exists(&plan.bundle)?;
    let mut warnings = Vec::new();

    let mut resolved_packs = Vec::new();
    let mut ops = plan.metadata.update_ops.clone();
    if ops.is_empty() {
        if !plan.metadata.pack_refs.is_empty() {
            ops.insert(WizardUpdateOp::PacksAdd);
        }
        if !plan.metadata.packs_remove.is_empty() {
            ops.insert(WizardUpdateOp::PacksRemove);
        }
        if !plan.metadata.providers.is_empty() {
            ops.insert(WizardUpdateOp::ProvidersAdd);
        }
        if !plan.metadata.providers_remove.is_empty() {
            ops.insert(WizardUpdateOp::ProvidersRemove);
        }
        if !plan.metadata.tenants.is_empty() {
            ops.insert(WizardUpdateOp::TenantsAdd);
        }
        if !plan.metadata.tenants_remove.is_empty() {
            ops.insert(WizardUpdateOp::TenantsRemove);
        }
        if !plan.metadata.access_changes.is_empty()
            || plan
                .metadata
                .tenants
                .iter()
                .any(|tenant| !tenant.allow_paths.is_empty())
        {
            ops.insert(WizardUpdateOp::AccessChange);
        }
    }

    if ops.contains(&WizardUpdateOp::PacksAdd) && !plan.metadata.pack_refs.is_empty() {
        let mut resolved = resolve_pack_refs(&plan.metadata.pack_refs, offline)
            .context("resolve pack refs via distributor-client")?;
        assign_pack_ids_and_persist_metadata(&plan.bundle, &mut resolved)?;
        for item in resolved {
            copy_pack_into_bundle(&plan.bundle, &item)?;
            resolved_packs.push(item);
        }
        link_packs_to_provider_dirs(&plan.bundle, &resolved_packs)?;
    }
    if !plan.metadata.default_assignments.is_empty() {
        apply_default_assignments(
            &plan.bundle,
            &plan.metadata.default_assignments,
            &mut warnings,
        )?;
    }
    if ops.contains(&WizardUpdateOp::PacksRemove) {
        for selection in &plan.metadata.packs_remove {
            apply_pack_remove(&plan.bundle, selection, &mut warnings)?;
        }
    }
    let mut provider_updates = upsert_provider_registry(&plan.bundle, &resolved_packs)?;
    if ops.contains(&WizardUpdateOp::ProvidersAdd) && !plan.metadata.providers.is_empty() {
        provider_updates +=
            upsert_provider_ids(&plan.bundle, &plan.metadata.providers, &mut warnings)?;
    }
    if ops.contains(&WizardUpdateOp::ProvidersRemove) && !plan.metadata.providers_remove.is_empty()
    {
        provider_updates +=
            remove_provider_ids(&plan.bundle, &plan.metadata.providers_remove, &mut warnings)?;
    }
    if ops.contains(&WizardUpdateOp::TenantsAdd) {
        for tenant in &plan.metadata.tenants {
            ensure_tenant_and_team(&plan.bundle, tenant)?;
        }
    }
    if ops.contains(&WizardUpdateOp::TenantsRemove) {
        for tenant in &plan.metadata.tenants_remove {
            remove_tenant_or_team(&plan.bundle, tenant, &mut warnings)?;
        }
    }

    let mut copied = Vec::new();
    if ops.contains(&WizardUpdateOp::AccessChange) {
        copied.extend(apply_access_and_sync(
            &plan.bundle,
            &plan.metadata.tenants,
            &plan.metadata.access_changes,
            &mut warnings,
        )?);
    }
    Ok(WizardExecutionReport {
        bundle: plan.bundle.clone(),
        resolved_packs,
        resolved_manifests: copied,
        provider_updates,
        warnings,
    })
}

pub fn execute_remove_plan(plan: &WizardPlan) -> anyhow::Result<WizardExecutionReport> {
    if plan.mode != WizardMode::Remove.as_str() {
        return Err(anyhow!("unsupported wizard mode: {}", plan.mode));
    }
    validate_bundle_exists(&plan.bundle)?;
    let mut warnings = Vec::new();

    let mut targets = plan.metadata.remove_targets.clone();
    if targets.is_empty() {
        if !plan.metadata.packs_remove.is_empty() {
            targets.insert(WizardRemoveTarget::Packs);
        }
        if !plan.metadata.providers_remove.is_empty() {
            targets.insert(WizardRemoveTarget::Providers);
        }
        if !plan.metadata.tenants_remove.is_empty() {
            targets.insert(WizardRemoveTarget::TenantsTeams);
        }
    }

    if targets.contains(&WizardRemoveTarget::Packs) {
        for selection in &plan.metadata.packs_remove {
            apply_pack_remove(&plan.bundle, selection, &mut warnings)?;
        }
    }
    let mut provider_updates = 0usize;
    if targets.contains(&WizardRemoveTarget::Providers) {
        provider_updates +=
            remove_provider_ids(&plan.bundle, &plan.metadata.providers_remove, &mut warnings)?;
    }
    if targets.contains(&WizardRemoveTarget::TenantsTeams) {
        for tenant in &plan.metadata.tenants_remove {
            remove_tenant_or_team(&plan.bundle, tenant, &mut warnings)?;
        }
    }
    Ok(WizardExecutionReport {
        bundle: plan.bundle.clone(),
        resolved_packs: Vec::new(),
        resolved_manifests: Vec::new(),
        provider_updates,
        warnings,
    })
}

fn step<const N: usize>(
    kind: WizardStepKind,
    description: &str,
    details: [(&str, String); N],
) -> WizardPlanStep {
    let mut map = BTreeMap::new();
    for (key, value) in details {
        map.insert(key.to_string(), value);
    }
    WizardPlanStep {
        kind,
        description: description.to_string(),
        details: map,
    }
}

fn create_demo_bundle_structure(root: &Path, bundle_name: Option<&str>) -> anyhow::Result<()> {
    let directories = [
        "",
        "providers",
        "providers/messaging",
        "providers/events",
        "providers/secrets",
        "providers/oauth",
        "packs",
        "resolved",
        "state",
        "state/resolved",
        "state/runs",
        "state/pids",
        "state/logs",
        "state/runtime",
        "state/doctor",
        "tenants",
        "tenants/default",
        "tenants/default/teams",
        "tenants/demo",
        "tenants/demo/teams",
        "tenants/demo/teams/default",
        "logs",
    ];
    for directory in directories {
        std::fs::create_dir_all(root.join(directory))?;
    }
    let mut demo_yaml = "version: \"1\"\nproject_root: \"./\"\n".to_string();
    if let Some(name) = bundle_name.filter(|value| !value.trim().is_empty()) {
        demo_yaml.push_str(&format!("bundle_name: \"{}\"\n", name.replace('"', "")));
    }
    write_if_missing(&root.join("greentic.demo.yaml"), &demo_yaml)?;
    write_if_missing(
        &root.join("tenants").join("default").join("tenant.gmap"),
        "_ = forbidden\n",
    )?;
    write_if_missing(
        &root.join("tenants").join("demo").join("tenant.gmap"),
        "_ = forbidden\n",
    )?;
    write_if_missing(
        &root
            .join("tenants")
            .join("demo")
            .join("teams")
            .join("default")
            .join("team.gmap"),
        "_ = forbidden\n",
    )?;
    Ok(())
}

fn write_if_missing(path: &Path, contents: &str) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}

fn ensure_tenant_and_team(bundle: &Path, selection: &TenantSelection) -> anyhow::Result<()> {
    project::add_tenant(bundle, &selection.tenant)?;
    if let Some(team) = selection.team.as_deref()
        && !team.is_empty()
    {
        project::add_team(bundle, &selection.tenant, team)?;
    }
    Ok(())
}

fn demo_bundle_gmap_path(bundle: &Path, tenant: &str, team: Option<&str>) -> PathBuf {
    let mut path = bundle.join("tenants").join(tenant);
    if let Some(team) = team {
        path = path.join("teams").join(team).join("team.gmap");
    } else {
        path = path.join("tenant.gmap");
    }
    path
}

fn resolved_manifest_filename(tenant: &str, team: Option<&str>) -> String {
    match team {
        Some(team) => format!("{tenant}.{team}.yaml"),
        None => format!("{tenant}.yaml"),
    }
}

fn resolve_pack_refs(pack_refs: &[String], offline: bool) -> anyhow::Result<Vec<ResolvedPackInfo>> {
    use greentic_distributor_client::{
        OciPackFetcher, PackFetchOptions, oci_packs::DefaultRegistryClient,
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for pack resolution")?;

    let mut opts = PackFetchOptions {
        allow_tags: true,
        offline,
        ..PackFetchOptions::default()
    };
    if let Ok(cache_dir) = std::env::var("GREENTIC_PACK_CACHE_DIR") {
        opts.cache_dir = PathBuf::from(cache_dir);
    }
    let fetcher: OciPackFetcher<DefaultRegistryClient> = OciPackFetcher::new(opts);

    let mut resolved = Vec::new();
    for reference in pack_refs {
        if let Some(local_path) = parse_local_pack_ref(reference) {
            let meta = crate::domains::read_pack_meta(&local_path)
                .with_context(|| format!("read pack meta from {}", local_path.display()))?;
            let digest = local_pack_digest(&local_path)?;
            let file_name = deterministic_pack_file_name(reference, &digest);
            resolved.push(ResolvedPackInfo {
                source_ref: reference.clone(),
                mapped_ref: local_path.display().to_string(),
                resolved_digest: digest,
                pack_id: meta.pack_id,
                entry_flows: meta.entry_flows,
                cached_path: local_path,
                output_path: PathBuf::from("packs").join(file_name),
            });
            continue;
        }
        let mapped_ref = map_pack_reference(reference)?;
        let fetched = rt
            .block_on(fetcher.fetch_pack_to_cache(&mapped_ref))
            .with_context(|| format!("fetch pack reference {reference}"))?;
        let meta = crate::domains::read_pack_meta(&fetched.path)
            .with_context(|| format!("read pack meta from {}", fetched.path.display()))?;
        let file_name = deterministic_pack_file_name(reference, &fetched.resolved_digest);
        resolved.push(ResolvedPackInfo {
            source_ref: reference.clone(),
            mapped_ref,
            resolved_digest: fetched.resolved_digest,
            pack_id: meta.pack_id,
            entry_flows: meta.entry_flows,
            cached_path: fetched.path,
            output_path: PathBuf::from("packs").join(file_name),
        });
    }
    resolved.sort_by(|a, b| a.source_ref.cmp(&b.source_ref));
    Ok(resolved)
}

fn parse_local_pack_ref(reference: &str) -> Option<PathBuf> {
    let trimmed = reference.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(path) = trimmed.strip_prefix("file://") {
        let local = PathBuf::from(path);
        if local.exists() {
            return Some(local);
        }
        return None;
    }
    if trimmed.contains("://") {
        return None;
    }
    let local = PathBuf::from(trimmed);
    if local.exists() { Some(local) } else { None }
}

fn local_pack_digest(path: &Path) -> anyhow::Result<String> {
    use std::hash::{Hash, Hasher};
    let metadata =
        std::fs::metadata(path).with_context(|| format!("stat local pack {}", path.display()))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.display().to_string().hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .hash(&mut hasher);
    Ok(format!("local:{:016x}", hasher.finish()))
}

fn map_pack_reference(reference: &str) -> anyhow::Result<String> {
    let trimmed = reference.trim();
    if let Some(rest) = trimmed.strip_prefix("oci://") {
        return Ok(rest.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("repo://") {
        return map_registry_target(rest, std::env::var("GREENTIC_REPO_REGISTRY_BASE").ok())
            .ok_or_else(|| {
                anyhow!(
                    "repo:// reference {trimmed} requires GREENTIC_REPO_REGISTRY_BASE to map to OCI"
                )
            });
    }
    if let Some(rest) = trimmed.strip_prefix("store://") {
        return map_registry_target(rest, std::env::var("GREENTIC_STORE_REGISTRY_BASE").ok())
            .ok_or_else(|| {
                anyhow!(
                    "store:// reference {trimmed} requires GREENTIC_STORE_REGISTRY_BASE to map to OCI"
                )
            });
    }
    Ok(trimmed.to_string())
}

fn map_registry_target(target: &str, base: Option<String>) -> Option<String> {
    if target.contains('/') && (target.contains('@') || target.contains(':')) {
        return Some(target.to_string());
    }
    let base = base?;
    let normalized_base = base.trim_end_matches('/');
    let normalized_target = target.trim_start_matches('/');
    Some(format!("{normalized_base}/{normalized_target}"))
}

fn deterministic_pack_file_name(reference: &str, digest: &str) -> String {
    let mut slug = String::new();
    for ch in reference.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else {
            slug.push('-');
        }
    }
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    slug = slug.trim_matches('-').to_string();
    if slug.len() > 40 {
        slug.truncate(40);
    }
    let short_digest = digest
        .trim_start_matches("sha256:")
        .chars()
        .take(12)
        .collect::<String>();
    format!("{slug}-{short_digest}.gtpack")
}

fn copy_pack_into_bundle(bundle: &Path, pack: &ResolvedPackInfo) -> anyhow::Result<()> {
    let src = pack.cached_path.clone();
    if !src.exists() {
        return Err(anyhow!("cached pack not found at {}", src.display()));
    }
    let dst = bundle.join(&pack.output_path);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

/// Copy each resolved pack into the corresponding `providers/{domain}/` directory
/// so that `discovery::discover()` can detect them at `demo start` time.
fn link_packs_to_provider_dirs(bundle: &Path, packs: &[ResolvedPackInfo]) -> anyhow::Result<()> {
    for pack in packs {
        let domain_dir = if pack.pack_id.starts_with("messaging-") {
            "messaging"
        } else if pack.pack_id.starts_with("events-") {
            "events"
        } else if pack.pack_id.starts_with("oauth-") {
            "oauth"
        } else {
            continue;
        };
        let src = bundle.join(&pack.output_path);
        if !src.exists() {
            continue;
        }
        let dest_dir = bundle.join("providers").join(domain_dir);
        std::fs::create_dir_all(&dest_dir)?;
        let file_name = src
            .file_name()
            .ok_or_else(|| anyhow!("bad pack path {}", src.display()))?;
        let dst = dest_dir.join(file_name);
        if !dst.exists() {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Seed secrets from the `setup_answers` map in the wizard answers.
///
/// For each provider in `setup_answers`, calls `persist_all_config_as_secrets`
/// so that WASM components can read the values via the secrets API at runtime.
fn seed_setup_answers(
    bundle: &Path,
    tenants: &[TenantSelection],
    setup_answers: &serde_json::Map<String, serde_json::Value>,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    let env = crate::secrets_setup::resolve_env(None);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for secret seeding")?;

    // Seed for each tenant declared in the plan.
    let tenant_ids: Vec<String> = if tenants.is_empty() {
        vec!["demo".to_string()]
    } else {
        tenants.iter().map(|t| t.tenant.clone()).collect()
    };

    for (provider_id, config) in setup_answers {
        if !config.is_object() || config.as_object().is_some_and(|m| m.is_empty()) {
            continue;
        }
        // Try to find the pack path so secret-requirements aliases are seeded.
        let pack_path = find_provider_pack_path(bundle, provider_id);
        for tenant in &tenant_ids {
            match rt.block_on(crate::qa_persist::persist_all_config_as_secrets(
                bundle,
                &env,
                tenant,
                None, // team
                provider_id,
                config,
                pack_path.as_deref(),
            )) {
                Ok(keys) => {
                    if !keys.is_empty() {
                        crate::operator_log::info(
                            module_path!(),
                            format!(
                                "seeded {} secret(s) for provider={} tenant={}",
                                keys.len(),
                                provider_id,
                                tenant
                            ),
                        );
                    }
                }
                Err(err) => {
                    warnings.push(format!(
                        "failed to seed secrets for provider={} tenant={}: {err}",
                        provider_id, tenant
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Locate a provider's .gtpack file in the bundle by provider_id stem.
fn find_provider_pack_path(bundle: &Path, provider_id: &str) -> Option<std::path::PathBuf> {
    // Search in providers/messaging, providers/events, packs
    for subdir in &["providers/messaging", "providers/events", "packs"] {
        let dir = bundle.join(subdir);
        let candidate = dir.join(format!("{provider_id}.gtpack"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Run webhook auto-setup for providers that have answers with public_base_url.
/// Called during wizard execute so webhooks are registered without needing demo start.
fn run_webhook_setup_from_answers(
    bundle: &Path,
    tenants: &[TenantSelection],
    setup_answers: &serde_json::Map<String, serde_json::Value>,
) {
    let tenant_ids: Vec<String> = if tenants.is_empty() {
        vec!["demo".to_string()]
    } else {
        tenants.iter().map(|t| t.tenant.clone()).collect()
    };

    for (provider_id, answers) in setup_answers {
        let Some(obj) = answers.as_object() else {
            continue;
        };
        if obj.is_empty() {
            continue;
        }
        // Need public_base_url to register webhooks
        let Some(public_url) = obj.get("public_base_url").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
            continue;
        };
        if !public_url.starts_with("https://") {
            crate::operator_log::info(
                module_path!(),
                format!(
                    "[wizard] webhook skipped provider={} (public_base_url is not HTTPS: {})",
                    provider_id, public_url
                ),
            );
            continue;
        }

        let pack_path = bundle.join("packs").join(format!("{provider_id}.gtpack"));
        let pack = crate::domains::ProviderPack {
            pack_id: provider_id.clone(),
            file_name: pack_path
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or_default()
                .to_string(),
            path: pack_path,
            entry_flows: Vec::new(),
        };

        for tenant in &tenant_ids {
            let config = serde_json::Value::Object(obj.clone());
            match crate::onboard::webhook_setup::try_provider_setup_webhook(
                bundle,
                crate::domains::Domain::Messaging,
                &pack,
                provider_id,
                tenant,
                None,
                &config,
            ) {
                Some(result) => {
                    let ok = result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    if ok {
                        crate::operator_log::info(
                            module_path!(),
                            format!(
                                "[wizard] webhook auto-setup ok provider={} tenant={} result={}",
                                provider_id, tenant, result
                            ),
                        );
                        println!(
                            "webhook: {} registered ({})",
                            provider_id,
                            result.get("webhook_url").and_then(|v| v.as_str()).unwrap_or("ok")
                        );
                    } else {
                        crate::operator_log::warn(
                            module_path!(),
                            format!(
                                "[wizard] webhook auto-setup failed provider={} tenant={} result={}",
                                provider_id, tenant, result
                            ),
                        );
                        let err = result.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
                        println!("webhook: {} failed ({})", provider_id, err);
                    }
                }
                None => {
                    crate::operator_log::info(
                        module_path!(),
                        format!(
                            "[wizard] webhook skipped provider={} (unsupported or missing config)",
                            provider_id
                        ),
                    );
                }
            }
        }
    }
}

fn upsert_provider_registry(bundle: &Path, packs: &[ResolvedPackInfo]) -> anyhow::Result<usize> {
    if packs.is_empty() {
        return Ok(0);
    }
    let path = bundle.join("providers").join("providers.json");
    let mut root = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read provider registry {}", path.display()))?;
        serde_json::from_str::<serde_json::Value>(&raw)
            .with_context(|| format!("parse provider registry {}", path.display()))?
    } else {
        serde_json::json!({ "providers": [] })
    };

    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("provider registry {} must be a JSON object", path.display()))?;
    if !root_obj.contains_key("providers") {
        root_obj.insert("providers".to_string(), serde_json::json!([]));
    }
    let providers = root_obj
        .get_mut("providers")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| {
            anyhow!(
                "provider registry {}.providers must be an array",
                path.display()
            )
        })?;

    let mut updates = 0usize;
    for pack in packs {
        let mut found = false;
        for entry in providers.iter_mut() {
            let Some(entry_obj) = entry.as_object_mut() else {
                continue;
            };
            let same_id = entry_obj
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(|id| id == pack.pack_id)
                .unwrap_or(false);
            if !same_id {
                continue;
            }
            found = true;
            let current_ref = entry_obj
                .get("ref")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let current_enabled = entry_obj
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if current_ref != pack.source_ref || !current_enabled {
                entry_obj.insert(
                    "ref".to_string(),
                    serde_json::Value::String(pack.source_ref.clone()),
                );
                entry_obj.insert("enabled".to_string(), serde_json::Value::Bool(true));
                updates += 1;
            }
            break;
        }
        if !found {
            providers.push(serde_json::json!({
                "id": pack.pack_id,
                "ref": pack.source_ref,
                "enabled": true
            }));
            updates += 1;
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_string_pretty(&root)
        .with_context(|| format!("serialize provider registry {}", path.display()))?;
    std::fs::write(&path, payload)
        .with_context(|| format!("write provider registry {}", path.display()))?;
    Ok(updates)
}

fn upsert_provider_ids(
    bundle: &Path,
    provider_ids: &[String],
    _warnings: &mut Vec<String>,
) -> anyhow::Result<usize> {
    if provider_ids.is_empty() {
        return Ok(0);
    }
    let path = bundle.join("providers").join("providers.json");
    let mut root = load_provider_registry_file(&path)?;
    let providers = ensure_provider_array_mut(&mut root, &path)?;
    let mut updates = 0usize;
    for provider_id in provider_ids {
        let id = provider_id.trim();
        if id.is_empty() {
            continue;
        }
        let mut found = false;
        for entry in providers.iter_mut() {
            let Some(entry_obj) = entry.as_object_mut() else {
                continue;
            };
            let same_id = entry_obj
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value == id);
            if !same_id {
                continue;
            }
            found = true;
            let enabled = entry_obj
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !enabled {
                entry_obj.insert("enabled".to_string(), serde_json::Value::Bool(true));
                updates += 1;
            }
            break;
        }
        if !found {
            providers.push(serde_json::json!({
                "id": id,
                "ref": id,
                "enabled": true
            }));
            updates += 1;
        }
    }
    write_provider_registry_file(&path, &root)?;
    Ok(updates)
}

fn remove_provider_ids(
    bundle: &Path,
    provider_ids: &[String],
    warnings: &mut Vec<String>,
) -> anyhow::Result<usize> {
    if provider_ids.is_empty() {
        return Ok(0);
    }
    let path = bundle.join("providers").join("providers.json");
    if !path.exists() {
        for provider_id in provider_ids {
            warnings.push(format!(
                "provider {provider_id} already absent (providers/providers.json missing)"
            ));
        }
        return Ok(0);
    }

    let mut root = load_provider_registry_file(&path)?;
    let providers = ensure_provider_array_mut(&mut root, &path)?;
    let mut updates = 0usize;
    for provider_id in provider_ids {
        let id = provider_id.trim();
        if id.is_empty() {
            continue;
        }
        let mut found = false;
        for entry in providers.iter_mut() {
            let Some(entry_obj) = entry.as_object_mut() else {
                continue;
            };
            let same_id = entry_obj
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value == id);
            if !same_id {
                continue;
            }
            found = true;
            let enabled = entry_obj
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if enabled {
                entry_obj.insert("enabled".to_string(), serde_json::Value::Bool(false));
                updates += 1;
            }
            break;
        }
        if !found {
            warnings.push(format!("provider {id} already absent"));
        }
    }
    write_provider_registry_file(&path, &root)?;
    Ok(updates)
}

fn load_provider_registry_file(path: &Path) -> anyhow::Result<serde_json::Value> {
    if path.exists() {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read provider registry {}", path.display()))?;
        serde_json::from_str::<serde_json::Value>(&raw)
            .with_context(|| format!("parse provider registry {}", path.display()))
    } else {
        Ok(serde_json::json!({ "providers": [] }))
    }
}

fn ensure_provider_array_mut<'a>(
    root: &'a mut serde_json::Value,
    path: &Path,
) -> anyhow::Result<&'a mut Vec<serde_json::Value>> {
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("provider registry {} must be a JSON object", path.display()))?;
    if !root_obj.contains_key("providers") {
        root_obj.insert("providers".to_string(), serde_json::json!([]));
    }
    root_obj
        .get_mut("providers")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| {
            anyhow!(
                "provider registry {}.providers must be an array",
                path.display()
            )
        })
}

fn write_provider_registry_file(path: &Path, root: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_string_pretty(root)
        .with_context(|| format!("serialize provider registry {}", path.display()))?;
    std::fs::write(path, payload).with_context(|| format!("write {}", path.display()))
}

fn apply_pack_remove(
    bundle: &Path,
    selection: &PackRemoveSelection,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    let pack_id = resolve_pack_identifier(bundle, &selection.pack_identifier)?;
    let mut removed_any = false;
    let packs_dir = bundle.join("packs");
    if packs_dir.exists() {
        for entry in std::fs::read_dir(&packs_dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name == pack_id || name.starts_with(&format!("{pack_id}.")) {
                removed_any = true;
                if path.is_dir() {
                    std::fs::remove_dir_all(&path)?;
                } else {
                    std::fs::remove_file(&path)?;
                }
            }
        }
    }
    let scope = selection.scope.as_ref().unwrap_or(&PackScope::Bundle);
    match scope {
        PackScope::Bundle => {
            mark_dangling_defaults(bundle, &pack_id, warnings)?;
        }
        PackScope::Global => {
            remove_if_exists(&bundle.join("default.gtpack"), &mut removed_any)?;
        }
        PackScope::Tenant { tenant_id } => {
            remove_if_exists(
                &bundle
                    .join("tenants")
                    .join(tenant_id)
                    .join("default.gtpack"),
                &mut removed_any,
            )?;
        }
        PackScope::Team { tenant_id, team_id } => {
            remove_if_exists(
                &bundle
                    .join("tenants")
                    .join(tenant_id)
                    .join("teams")
                    .join(team_id)
                    .join("default.gtpack"),
                &mut removed_any,
            )?;
        }
    }
    if !removed_any {
        warnings.push(format!(
            "pack {} already absent (scope={scope:?})",
            selection.pack_identifier
        ));
    }
    Ok(())
}

fn resolve_pack_identifier(bundle: &Path, identifier: &str) -> anyhow::Result<String> {
    canonical_pack_identifier(bundle, identifier)
}

fn canonical_pack_identifier(bundle: &Path, identifier: &str) -> anyhow::Result<String> {
    let trimmed = identifier.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("pack identifier must not be empty"));
    }
    if !trimmed.contains("://") && !trimmed.contains('/') && !trimmed.contains('.') {
        return Ok(trimmed.to_string());
    }
    let metadata = load_packs_metadata(bundle).unwrap_or_default();
    if let Some(record) = metadata
        .packs
        .iter()
        .find(|record| record.pack_id == trimmed)
    {
        return Ok(record.pack_id.clone());
    }
    if let Some(record) = metadata
        .packs
        .iter()
        .find(|record| record.original_ref == trimmed)
    {
        return Ok(record.pack_id.clone());
    }
    Ok(derive_pack_id_from_reference(trimmed))
}

fn mark_dangling_defaults(
    bundle: &Path,
    pack_id: &str,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    let global = bundle.join("default.gtpack");
    if default_mentions_pack(&global, pack_id)? {
        warnings.push(format!(
            "global default.gtpack references removed pack {pack_id} and may now be dangling"
        ));
    }
    let tenants_root = bundle.join("tenants");
    if !tenants_root.exists() {
        return Ok(());
    }
    for tenant in std::fs::read_dir(tenants_root)? {
        let tenant = tenant?;
        let tenant_path = tenant.path();
        let tenant_name = tenant.file_name().to_string_lossy().to_string();
        let tenant_default = tenant_path.join("default.gtpack");
        if default_mentions_pack(&tenant_default, pack_id)? {
            warnings.push(format!(
                "tenant {tenant_name} default.gtpack references removed pack {pack_id}"
            ));
        }
        let teams_root = tenant_path.join("teams");
        if !teams_root.exists() {
            continue;
        }
        for team in std::fs::read_dir(teams_root)? {
            let team = team?;
            let team_path = team.path();
            let team_name = team.file_name().to_string_lossy().to_string();
            let team_default = team_path.join("default.gtpack");
            if default_mentions_pack(&team_default, pack_id)? {
                warnings.push(format!(
                    "team {tenant_name}:{team_name} default.gtpack references removed pack {pack_id}"
                ));
            }
        }
    }
    Ok(())
}

fn default_mentions_pack(path: &Path, pack_id: &str) -> anyhow::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(raw.contains(pack_id))
}

fn remove_if_exists(path: &Path, removed: &mut bool) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    *removed = true;
    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn remove_tenant_or_team(
    bundle: &Path,
    selection: &TenantSelection,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    let path = if let Some(team) = selection.team.as_deref() {
        bundle
            .join("tenants")
            .join(&selection.tenant)
            .join("teams")
            .join(team)
    } else {
        bundle.join("tenants").join(&selection.tenant)
    };
    if !path.exists() {
        warnings.push(format!(
            "tenant/team {}:{} already absent",
            selection.tenant,
            selection.team.clone().unwrap_or_default()
        ));
        return Ok(());
    }
    std::fs::remove_dir_all(path)?;
    Ok(())
}

fn copy_resolved_for_targets<I>(
    bundle: &Path,
    targets: I,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<PathBuf>>
where
    I: IntoIterator<Item = (String, Option<String>)>,
{
    let mut copied = Vec::new();
    let mut seen = BTreeSet::new();
    for (tenant, team) in targets {
        if !seen.insert((tenant.clone(), team.clone())) {
            continue;
        }
        let filename = resolved_manifest_filename(&tenant, team.as_deref());
        let src = bundle.join("state").join("resolved").join(&filename);
        if !src.exists() {
            warnings.push(format!(
                "resolved manifest {} missing after resolver run",
                src.display()
            ));
            continue;
        }
        let dst = bundle.join("resolved").join(&filename);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, &dst)?;
        copied.push(dst);
    }
    Ok(copied)
}

fn apply_access_and_sync(
    bundle: &Path,
    tenants: &[TenantSelection],
    access_changes: &[AccessChangeSelection],
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut copy_targets: BTreeSet<(String, Option<String>)> = BTreeSet::new();
    for tenant in tenants {
        ensure_tenant_and_team(bundle, tenant)?;
        copy_targets.insert((tenant.tenant.clone(), tenant.team.clone()));
        for path in &tenant.allow_paths {
            if path.trim().is_empty() {
                continue;
            }
            let gmap_path = demo_bundle_gmap_path(bundle, &tenant.tenant, tenant.team.as_deref());
            gmap::upsert_policy(&gmap_path, path, Policy::Public)?;
        }
    }
    for change in access_changes {
        ensure_tenant_and_team(
            bundle,
            &TenantSelection {
                tenant: change.tenant_id.clone(),
                team: change.team_id.clone(),
                allow_paths: Vec::new(),
            },
        )?;
        copy_targets.insert((change.tenant_id.clone(), change.team_id.clone()));
        let gmap_path = demo_bundle_gmap_path(bundle, &change.tenant_id, change.team_id.as_deref());
        gmap::upsert_policy(&gmap_path, &change.pack_id, change.operation.policy())?;
    }
    if copy_targets.is_empty() {
        return Ok(Vec::new());
    }
    project::sync_project(bundle)?;
    copy_resolved_for_targets(bundle, copy_targets, warnings)
}

fn apply_default_assignments(
    bundle: &Path,
    defaults: &[PackDefaultSelection],
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    for assignment in defaults {
        let pack_id = resolve_pack_identifier(bundle, &assignment.pack_identifier)?;
        let pack_file = format!("packs/{pack_id}.gtpack");
        let target = match &assignment.scope {
            PackScope::Bundle => continue,
            PackScope::Global => bundle.join("default.gtpack"),
            PackScope::Tenant { tenant_id } => bundle
                .join("tenants")
                .join(tenant_id)
                .join("default.gtpack"),
            PackScope::Team { tenant_id, team_id } => bundle
                .join("tenants")
                .join(tenant_id)
                .join("teams")
                .join(team_id)
                .join("default.gtpack"),
        };
        if !bundle.join(&pack_file).exists() {
            warnings.push(format!(
                "default assignment for {} skipped: {} not found",
                assignment.pack_identifier, pack_file
            ));
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, format!("{pack_file}\n"))?;
    }
    Ok(())
}

fn assign_pack_ids_and_persist_metadata(
    bundle: &Path,
    packs: &mut [ResolvedPackInfo],
) -> anyhow::Result<()> {
    if packs.is_empty() {
        return Ok(());
    }

    let mut metadata = load_packs_metadata(bundle)?;
    let mut by_original_ref = BTreeMap::new();
    let mut used_ids = BTreeSet::new();
    for record in &metadata.packs {
        if !record.original_ref.trim().is_empty() {
            by_original_ref.insert(record.original_ref.clone(), record.pack_id.clone());
        }
        used_ids.insert(record.pack_id.clone());
    }

    for pack in packs.iter_mut() {
        let assigned_pack_id = if let Some(existing) = by_original_ref.get(&pack.source_ref) {
            existing.clone()
        } else {
            let base = derive_pack_id_from_reference(&pack.source_ref);
            let unique = allocate_unique_pack_id(&base, &used_ids);
            by_original_ref.insert(pack.source_ref.clone(), unique.clone());
            unique
        };
        used_ids.insert(assigned_pack_id.clone());
        pack.pack_id = assigned_pack_id.clone();
        pack.output_path = PathBuf::from("packs").join(format!("{assigned_pack_id}.gtpack"));
    }

    for pack in packs.iter() {
        upsert_pack_mapping(
            &mut metadata,
            PackMappingRecord {
                pack_id: pack.pack_id.clone(),
                original_ref: pack.source_ref.clone(),
                local_path_in_bundle: pack.output_path.display().to_string(),
                digest: Some(pack.resolved_digest.clone()),
            },
        );
    }
    metadata.packs.sort_by(|a, b| a.pack_id.cmp(&b.pack_id));
    write_packs_metadata(bundle, &metadata)?;
    Ok(())
}

fn upsert_pack_mapping(metadata: &mut PacksMetadata, next: PackMappingRecord) {
    if let Some(existing) = metadata
        .packs
        .iter_mut()
        .find(|record| record.pack_id == next.pack_id)
    {
        *existing = next;
        return;
    }
    metadata.packs.push(next);
}

fn packs_metadata_path(bundle: &Path) -> PathBuf {
    bundle.join(".greentic").join("packs.json")
}

fn load_packs_metadata(bundle: &Path) -> anyhow::Result<PacksMetadata> {
    let path = packs_metadata_path(bundle);
    if !path.exists() {
        return Ok(PacksMetadata::default());
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

fn write_packs_metadata(bundle: &Path, metadata: &PacksMetadata) -> anyhow::Result<()> {
    let path = packs_metadata_path(bundle);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_string_pretty(metadata)
        .with_context(|| format!("serialize {}", path.display()))?;
    std::fs::write(&path, payload).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn allocate_unique_pack_id(base: &str, used_ids: &BTreeSet<String>) -> String {
    if !used_ids.contains(base) {
        return base.to_string();
    }
    for index in 2.. {
        let candidate = format!("{base}-{index}");
        if !used_ids.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("unbounded index must eventually produce unique pack id")
}

fn derive_pack_id_from_reference(reference: &str) -> String {
    let trimmed = reference.trim();
    if trimmed.is_empty() {
        return "pack".to_string();
    }

    let value = if let Some(rest) = trimmed.strip_prefix("file://") {
        rest
    } else if let Some((_, rest)) = trimmed.split_once("://") {
        rest
    } else {
        trimmed
    };
    let (path_part, tag_part) = value
        .split_once('@')
        .map_or((value, None), |(p, t)| (p, Some(t)));
    let tail = path_part.rsplit('/').next().unwrap_or(path_part);
    let stem = tail.rsplit_once('.').map_or(tail, |(base, _)| base);

    let mut id = slug_for_pack_id(stem);
    if id.is_empty() {
        id = "pack".to_string();
    }
    if let Some(tag) = tag_part {
        let tag_slug = slug_for_tag(tag);
        if !tag_slug.is_empty() {
            id.push('-');
            id.push_str(&tag_slug);
        }
    }
    id
}

fn slug_for_pack_id(value: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn slug_for_tag(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_deterministic() {
        let req = WizardCreateRequest {
            bundle: PathBuf::from("bundle"),
            bundle_name: None,
            pack_refs: vec![
                "repo://zeta/pack@1".to_string(),
                "repo://alpha/pack@1".to_string(),
                "repo://alpha/pack@1".to_string(),
            ],
            tenants: vec![
                TenantSelection {
                    tenant: "demo".to_string(),
                    team: Some("default".to_string()),
                    allow_paths: vec!["pack/b".to_string(), "pack/a".to_string()],
                },
                TenantSelection {
                    tenant: "alpha".to_string(),
                    team: None,
                    allow_paths: vec!["x".to_string()],
                },
            ],
            default_assignments: Vec::new(),
            providers: Vec::new(),
            update_ops: BTreeSet::new(),
            remove_targets: BTreeSet::new(),
            packs_remove: Vec::new(),
            providers_remove: Vec::new(),
            tenants_remove: Vec::new(),
            access_changes: Vec::new(),
            setup_answers: serde_json::Map::new(),
        };
        let plan = apply_create(&req, true).unwrap();
        assert_eq!(
            plan.metadata.pack_refs,
            vec![
                "repo://alpha/pack@1".to_string(),
                "repo://zeta/pack@1".to_string()
            ]
        );
        assert_eq!(plan.metadata.tenants[0].tenant, "alpha");
        assert_eq!(
            plan.metadata.tenants[1].allow_paths,
            vec!["pack/a".to_string(), "pack/b".to_string()]
        );
    }

    #[test]
    fn dry_run_does_not_create_files() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("demo-bundle");
        let req = WizardCreateRequest {
            bundle: bundle.clone(),
            bundle_name: None,
            pack_refs: Vec::new(),
            tenants: vec![TenantSelection {
                tenant: "demo".to_string(),
                team: Some("default".to_string()),
                allow_paths: vec!["packs/default".to_string()],
            }],
            default_assignments: Vec::new(),
            providers: Vec::new(),
            update_ops: BTreeSet::new(),
            remove_targets: BTreeSet::new(),
            packs_remove: Vec::new(),
            providers_remove: Vec::new(),
            tenants_remove: Vec::new(),
            access_changes: Vec::new(),
            setup_answers: serde_json::Map::new(),
        };
        let _plan = apply_create(&req, true).unwrap();
        assert!(!bundle.exists());
    }

    #[test]
    fn execute_creates_bundle_and_resolved_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("demo-bundle");
        let req = WizardCreateRequest {
            bundle: bundle.clone(),
            bundle_name: None,
            pack_refs: Vec::new(),
            tenants: vec![TenantSelection {
                tenant: "demo".to_string(),
                team: Some("default".to_string()),
                allow_paths: vec!["packs/default".to_string()],
            }],
            default_assignments: Vec::new(),
            providers: Vec::new(),
            update_ops: BTreeSet::new(),
            remove_targets: BTreeSet::new(),
            packs_remove: Vec::new(),
            providers_remove: Vec::new(),
            tenants_remove: Vec::new(),
            access_changes: Vec::new(),
            setup_answers: serde_json::Map::new(),
        };
        let plan = apply_create(&req, false).unwrap();
        let report = execute_create_plan(&plan, true).unwrap();
        assert!(report.bundle.exists());
        assert!(
            bundle
                .join("state")
                .join("resolved")
                .join("demo.default.yaml")
                .exists()
        );
        assert!(bundle.join("resolved").join("demo.default.yaml").exists());
    }

    #[test]
    fn update_mode_executes() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("demo-bundle");
        let create_req = WizardCreateRequest {
            bundle: bundle.clone(),
            bundle_name: None,
            pack_refs: Vec::new(),
            tenants: vec![TenantSelection {
                tenant: "demo".to_string(),
                team: None,
                allow_paths: vec!["packs/default".to_string()],
            }],
            default_assignments: Vec::new(),
            providers: Vec::new(),
            update_ops: BTreeSet::new(),
            remove_targets: BTreeSet::new(),
            packs_remove: Vec::new(),
            providers_remove: Vec::new(),
            tenants_remove: Vec::new(),
            access_changes: Vec::new(),
            setup_answers: serde_json::Map::new(),
        };
        let create_plan = apply_create(&create_req, false).unwrap();
        let _ = execute_create_plan(&create_plan, true).unwrap();

        let req = WizardCreateRequest {
            bundle: bundle.clone(),
            bundle_name: None,
            pack_refs: Vec::new(),
            tenants: vec![TenantSelection {
                tenant: "demo".to_string(),
                team: None,
                allow_paths: vec!["packs/new".to_string()],
            }],
            default_assignments: Vec::new(),
            providers: Vec::new(),
            update_ops: BTreeSet::new(),
            remove_targets: BTreeSet::new(),
            packs_remove: Vec::new(),
            providers_remove: Vec::new(),
            tenants_remove: Vec::new(),
            access_changes: Vec::new(),
            setup_answers: serde_json::Map::new(),
        };
        let plan = apply_update(&req, false).unwrap();
        assert_eq!(plan.mode, "update");
        let report = execute_update_plan(&plan, true).unwrap();
        assert!(report.bundle.exists());
    }

    #[test]
    fn remove_mode_forbids_rule() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("demo-bundle");
        let create_req = WizardCreateRequest {
            bundle: bundle.clone(),
            bundle_name: None,
            pack_refs: Vec::new(),
            tenants: vec![TenantSelection {
                tenant: "demo".to_string(),
                team: None,
                allow_paths: vec!["packs/default".to_string()],
            }],
            default_assignments: Vec::new(),
            providers: Vec::new(),
            update_ops: BTreeSet::new(),
            remove_targets: BTreeSet::new(),
            packs_remove: Vec::new(),
            providers_remove: Vec::new(),
            tenants_remove: Vec::new(),
            access_changes: Vec::new(),
            setup_answers: serde_json::Map::new(),
        };
        let create_plan = apply_create(&create_req, false).unwrap();
        let _ = execute_create_plan(&create_plan, true).unwrap();

        let remove_req = WizardCreateRequest {
            bundle: bundle.clone(),
            bundle_name: None,
            pack_refs: Vec::new(),
            tenants: Vec::new(),
            default_assignments: Vec::new(),
            providers: Vec::new(),
            update_ops: BTreeSet::new(),
            remove_targets: [WizardRemoveTarget::TenantsTeams].into_iter().collect(),
            packs_remove: Vec::new(),
            providers_remove: Vec::new(),
            tenants_remove: vec![TenantSelection {
                tenant: "demo".to_string(),
                team: Some("default".to_string()),
                allow_paths: Vec::new(),
            }],
            access_changes: Vec::new(),
            setup_answers: serde_json::Map::new(),
        };
        let remove_plan = apply_remove(&remove_req, false).unwrap();
        let _ = execute_remove_plan(&remove_plan).unwrap();
        assert!(
            !bundle
                .join("tenants")
                .join("demo")
                .join("teams")
                .join("default")
                .exists()
        );
    }

    #[test]
    fn derive_pack_id_handles_oci_and_local_refs() {
        assert_eq!(
            derive_pack_id_from_reference("oci://ghcr.io/greentic/packs/sales@0.6.0"),
            "sales-0_6_0"
        );
        assert_eq!(
            derive_pack_id_from_reference("store://sales/lead-to-cash@latest"),
            "lead-to-cash-latest"
        );
        assert_eq!(
            derive_pack_id_from_reference("/tmp/local/foo-pack.gtpack"),
            "foo-pack"
        );
        assert_eq!(
            derive_pack_id_from_reference("file:///tmp/local/foo_pack.gtpack"),
            "foo-pack"
        );
    }

    #[test]
    fn metadata_assigns_stable_pack_ids() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("demo-bundle");
        std::fs::create_dir_all(&bundle).unwrap();

        let mut packs = vec![
            ResolvedPackInfo {
                source_ref: "oci://ghcr.io/greentic/packs/sales@0.6.0".to_string(),
                mapped_ref: "ghcr.io/greentic/packs/sales@0.6.0".to_string(),
                resolved_digest: "sha256:abc".to_string(),
                pack_id: "ignored".to_string(),
                entry_flows: Vec::new(),
                cached_path: temp.path().join("cached-a.gtpack"),
                output_path: PathBuf::from("packs/ignored-a.gtpack"),
            },
            ResolvedPackInfo {
                source_ref: "oci://ghcr.io/greentic/packs/sales@0.6.0".to_string(),
                mapped_ref: "ghcr.io/greentic/packs/sales@0.6.0".to_string(),
                resolved_digest: "sha256:def".to_string(),
                pack_id: "ignored2".to_string(),
                entry_flows: Vec::new(),
                cached_path: temp.path().join("cached-b.gtpack"),
                output_path: PathBuf::from("packs/ignored-b.gtpack"),
            },
        ];
        assign_pack_ids_and_persist_metadata(&bundle, &mut packs).unwrap();
        assert_eq!(packs[0].pack_id, "sales-0_6_0");
        assert_eq!(packs[1].pack_id, "sales-0_6_0");
        assert_eq!(
            packs[0].output_path,
            PathBuf::from("packs/sales-0_6_0.gtpack")
        );

        let metadata = load_packs_metadata(&bundle).unwrap();
        assert_eq!(metadata.packs.len(), 1);
        assert_eq!(metadata.packs[0].pack_id, "sales-0_6_0");
    }

    #[test]
    fn load_catalog_supports_provider_registry_shape() {
        let temp = tempfile::tempdir().unwrap();
        let registry_path = temp.path().join("providers.json");
        std::fs::write(
            &registry_path,
            r#"{
  "registry_version": "providers@1",
  "items": [
    {
      "id": "messaging.telegram",
      "label": {"i18n_key": "provider.telegram", "fallback": "Telegram"},
      "ref": "oci://ghcr.io/greentic/providers/messaging-telegram@0.6.0"
    }
  ]
}"#,
        )
        .unwrap();
        let loaded = load_catalog_from_file(&registry_path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "messaging.telegram");
        assert_eq!(loaded[0].label, "Telegram");
        assert_eq!(
            loaded[0].reference,
            "oci://ghcr.io/greentic/providers/messaging-telegram@0.6.0"
        );
    }

    #[test]
    fn provider_registry_upserts_by_id() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("demo-bundle");
        std::fs::create_dir_all(bundle.join("providers")).unwrap();
        std::fs::write(
            bundle.join("providers").join("providers.json"),
            r#"{
  "providers": [
    {"id":"messaging.telegram","ref":"oci://old","enabled":false,"extra":"keep"}
  ],
  "top_level":"keep"
}"#,
        )
        .unwrap();

        let packs = vec![ResolvedPackInfo {
            source_ref: "oci://ghcr.io/greentic/providers/messaging-telegram@0.6.0".to_string(),
            mapped_ref: "ghcr.io/greentic/providers/messaging-telegram@0.6.0".to_string(),
            resolved_digest: "sha256:abc".to_string(),
            pack_id: "messaging.telegram".to_string(),
            entry_flows: Vec::new(),
            cached_path: temp.path().join("cached.gtpack"),
            output_path: PathBuf::from("packs/messaging.telegram.gtpack"),
        }];

        let updates = upsert_provider_registry(&bundle, &packs).unwrap();
        assert_eq!(updates, 1);
        let raw = std::fs::read_to_string(bundle.join("providers").join("providers.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["top_level"], "keep");
        assert_eq!(parsed["providers"][0]["id"], "messaging.telegram");
        assert_eq!(
            parsed["providers"][0]["ref"],
            "oci://ghcr.io/greentic/providers/messaging-telegram@0.6.0"
        );
        assert_eq!(parsed["providers"][0]["enabled"], true);
        assert_eq!(parsed["providers"][0]["extra"], "keep");
    }

    #[test]
    fn local_pack_ref_detection_supports_path_and_file_scheme() {
        let temp = tempfile::tempdir().unwrap();
        let pack = temp.path().join("sample.gtpack");
        std::fs::write(&pack, "pack").unwrap();

        let direct = parse_local_pack_ref(pack.to_string_lossy().as_ref());
        assert_eq!(direct, Some(pack.clone()));

        let file_ref = format!("file://{}", pack.display());
        let scheme = parse_local_pack_ref(&file_ref);
        assert_eq!(scheme, Some(pack));
    }

    #[test]
    fn normalize_request_resolves_pack_ref_to_pack_id() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        std::fs::create_dir_all(bundle.join(".greentic")).unwrap();
        std::fs::write(
            bundle.join(".greentic").join("packs.json"),
            r#"{
  "packs": [
    {
      "pack_id": "sales-0_6_0",
      "original_ref": "oci://ghcr.io/greentic/packs/sales@0.6.0",
      "local_path_in_bundle": "packs/sales-0_6_0.gtpack",
      "digest": "sha256:abc"
    }
  ]
}"#,
        )
        .unwrap();
        let request = WizardCreateRequest {
            bundle: bundle.clone(),
            bundle_name: None,
            pack_refs: Vec::new(),
            tenants: Vec::new(),
            default_assignments: Vec::new(),
            providers: Vec::new(),
            update_ops: [WizardUpdateOp::AccessChange].into_iter().collect(),
            remove_targets: BTreeSet::new(),
            packs_remove: vec![PackRemoveSelection {
                pack_identifier: "oci://ghcr.io/greentic/packs/sales@0.6.0".to_string(),
                scope: None,
            }],
            providers_remove: Vec::new(),
            tenants_remove: Vec::new(),
            access_changes: vec![AccessChangeSelection {
                pack_id: "oci://ghcr.io/greentic/packs/sales@0.6.0".to_string(),
                operation: AccessOperation::AllowAdd,
                tenant_id: "demo".to_string(),
                team_id: None,
            }],
            setup_answers: serde_json::Map::new(),
        };
        let normalized = normalize_request_for_plan(&request).unwrap();
        assert_eq!(normalized.packs_remove[0].pack_identifier, "sales-0_6_0");
        assert_eq!(normalized.access_changes[0].pack_id, "sales-0_6_0");
    }

    #[test]
    fn remove_pack_already_absent_is_idempotent_warning() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        create_demo_bundle_structure(&bundle, None).unwrap();
        let request = WizardCreateRequest {
            bundle: bundle.clone(),
            bundle_name: None,
            pack_refs: Vec::new(),
            tenants: Vec::new(),
            default_assignments: Vec::new(),
            providers: Vec::new(),
            update_ops: BTreeSet::new(),
            remove_targets: [WizardRemoveTarget::Packs].into_iter().collect(),
            packs_remove: vec![PackRemoveSelection {
                pack_identifier: "missing-pack".to_string(),
                scope: None,
            }],
            providers_remove: Vec::new(),
            tenants_remove: Vec::new(),
            access_changes: Vec::new(),
            setup_answers: serde_json::Map::new(),
        };
        let plan = apply_remove(&request, false).unwrap();
        let report = execute_remove_plan(&plan).unwrap();
        assert_eq!(report.provider_updates, 0);
        assert!(!report.warnings.is_empty());
    }

    #[test]
    fn update_applies_global_default_assignment_and_bundle_name_written() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("demo-bundle");
        let create_request = WizardCreateRequest {
            bundle: bundle.clone(),
            bundle_name: Some("Demo Bundle".to_string()),
            pack_refs: Vec::new(),
            tenants: vec![TenantSelection {
                tenant: "demo".to_string(),
                team: Some("default".to_string()),
                allow_paths: vec!["packs/default".to_string()],
            }],
            default_assignments: Vec::new(),
            providers: Vec::new(),
            update_ops: BTreeSet::new(),
            remove_targets: BTreeSet::new(),
            packs_remove: Vec::new(),
            providers_remove: Vec::new(),
            tenants_remove: Vec::new(),
            access_changes: Vec::new(),
            setup_answers: serde_json::Map::new(),
        };
        let create_plan = apply_create(&create_request, false).unwrap();
        let _create_report = execute_create_plan(&create_plan, true).unwrap();
        std::fs::create_dir_all(bundle.join("packs")).unwrap();
        std::fs::write(bundle.join("packs").join("sales.gtpack"), "dummy").unwrap();

        let update_request = WizardCreateRequest {
            bundle: bundle.clone(),
            bundle_name: None,
            pack_refs: Vec::new(),
            tenants: vec![TenantSelection {
                tenant: "demo".to_string(),
                team: Some("default".to_string()),
                allow_paths: vec!["packs/default".to_string()],
            }],
            default_assignments: vec![PackDefaultSelection {
                pack_identifier: "sales".to_string(),
                scope: PackScope::Global,
            }],
            providers: Vec::new(),
            update_ops: BTreeSet::new(),
            remove_targets: BTreeSet::new(),
            packs_remove: Vec::new(),
            providers_remove: Vec::new(),
            tenants_remove: Vec::new(),
            access_changes: Vec::new(),
            setup_answers: serde_json::Map::new(),
        };
        let update_plan = apply_update(&update_request, false).unwrap();
        let _report = execute_update_plan(&update_plan, true).unwrap();
        let default_raw = std::fs::read_to_string(bundle.join("default.gtpack")).unwrap();
        assert!(default_raw.contains("packs/sales.gtpack"));
        let demo_yaml = std::fs::read_to_string(bundle.join("greentic.demo.yaml")).unwrap();
        assert!(demo_yaml.contains("bundle_name: \"Demo Bundle\""));
    }
}
