use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use greentic_types::{ExtensionInline, decode_pack_manifest};
use serde::Deserialize;
use zip::ZipArchive;

use crate::domains::{self, Domain};

pub const EXT_STATIC_ROUTES_V1: &str = "greentic.static-routes.v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteScopeSegment {
    Literal(String),
    Tenant,
    Team,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheStrategy {
    None,
    PublicMaxAge { max_age_seconds: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticRouteDescriptor {
    pub route_id: String,
    pub pack_id: String,
    pub pack_path: PathBuf,
    pub public_path: String,
    pub source_root: String,
    pub index_file: Option<String>,
    pub spa_fallback: Option<String>,
    pub tenant_scoped: bool,
    pub team_scoped: bool,
    pub cache_strategy: CacheStrategy,
    pub route_segments: Vec<RouteScopeSegment>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StaticRoutePlan {
    pub routes: Vec<StaticRouteDescriptor>,
    pub warnings: Vec<String>,
    pub blocking_failures: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReservedRouteSet {
    exact_paths: BTreeSet<String>,
    prefix_paths: BTreeSet<String>,
}

impl ReservedRouteSet {
    pub fn operator_defaults() -> Self {
        let mut reserved = Self::default();
        for path in [
            "/healthz",
            "/readyz",
            "/status",
            "/runtime/drain",
            "/runtime/resume",
            "/runtime/shutdown",
            "/deployments/stage",
            "/deployments/warm",
            "/deployments/activate",
            "/deployments/rollback",
            "/deployments/complete-drain",
            "/config/publish",
            "/cache/invalidate",
            "/observability/log-level",
        ] {
            reserved.insert_exact(path);
        }
        reserved.insert_prefix("/api/onboard");
        reserved.insert_prefix("/runtime");
        reserved.insert_prefix("/deployments");
        reserved.insert_prefix("/config");
        reserved.insert_prefix("/cache");
        reserved.insert_prefix("/observability");
        for domain in [
            Domain::Messaging,
            Domain::Events,
            Domain::Secrets,
            Domain::OAuth,
        ] {
            let name = domains::domain_name(domain);
            reserved.insert_prefix(&format!("/v1/{name}/ingress"));
            reserved.insert_prefix(&format!("/{name}/ingress"));
        }
        reserved
    }

    pub fn insert_exact(&mut self, path: &str) {
        self.exact_paths.insert(normalize_public_path(path));
    }

    pub fn insert_prefix(&mut self, path: &str) {
        self.prefix_paths.insert(normalize_public_path(path));
    }

    pub fn conflicts_with(&self, public_path: &str) -> bool {
        let normalized = normalize_public_path(public_path);
        self.exact_paths.contains(&normalized)
            || self
                .prefix_paths
                .iter()
                .any(|prefix| path_has_prefix(&normalized, prefix))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticRouteMatch<'a> {
    pub descriptor: &'a StaticRouteDescriptor,
    pub asset_path: String,
    pub request_is_directory: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActiveRouteTable {
    routes: Vec<StaticRouteDescriptor>,
}

impl ActiveRouteTable {
    pub fn from_plan(plan: &StaticRoutePlan) -> Self {
        let mut routes = plan.routes.clone();
        routes.sort_by(|a, b| {
            b.route_segments
                .len()
                .cmp(&a.route_segments.len())
                .then_with(|| a.public_path.cmp(&b.public_path))
        });
        Self { routes }
    }

    pub fn routes(&self) -> &[StaticRouteDescriptor] {
        &self.routes
    }

    pub fn match_request<'a>(&'a self, request_path: &str) -> Option<StaticRouteMatch<'a>> {
        let normalized = request_path
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        let request_is_directory = request_path.ends_with('/');
        for descriptor in &self.routes {
            if normalized.len() < descriptor.route_segments.len() {
                continue;
            }
            let mut matched = true;
            for (route_segment, request_segment) in
                descriptor.route_segments.iter().zip(normalized.iter())
            {
                match route_segment {
                    RouteScopeSegment::Literal(expected) if expected != request_segment => {
                        matched = false;
                        break;
                    }
                    RouteScopeSegment::Literal(_)
                    | RouteScopeSegment::Tenant
                    | RouteScopeSegment::Team => {}
                }
            }
            if !matched {
                continue;
            }
            let asset_path = normalized[descriptor.route_segments.len()..].join("/");
            return Some(StaticRouteMatch {
                descriptor,
                asset_path,
                request_is_directory,
            });
        }
        None
    }
}

#[derive(Debug, Deserialize)]
struct StaticRoutesExtensionV1 {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default)]
    routes: Vec<StaticRouteRecord>,
}

#[derive(Clone, Debug, Deserialize)]
struct StaticRouteRecord {
    #[serde(default)]
    id: Option<String>,
    public_path: String,
    source_root: String,
    #[serde(default)]
    index_file: Option<String>,
    #[serde(default)]
    spa_fallback: Option<String>,
    #[serde(default)]
    tenant: bool,
    #[serde(default)]
    team: bool,
    #[serde(default)]
    cache: Option<StaticRouteCacheRecord>,
}

#[derive(Clone, Debug, Deserialize)]
struct StaticRouteCacheRecord {
    strategy: String,
    #[serde(default)]
    max_age_seconds: Option<u64>,
}

fn default_schema_version() -> u32 {
    1
}

pub fn discover_from_bundle(
    bundle_root: &Path,
    reserved_routes: &ReservedRouteSet,
) -> anyhow::Result<StaticRoutePlan> {
    let mut plan = StaticRoutePlan::default();
    let pack_paths = collect_runtime_pack_paths(bundle_root)?;
    for pack_path in pack_paths {
        let descriptors = match read_pack_static_routes(&pack_path) {
            Ok(Some(descriptors)) => descriptors,
            Ok(None) => continue,
            Err(err) => {
                plan.blocking_failures.push(err.to_string());
                continue;
            }
        };
        plan.routes.extend(descriptors);
    }
    validate_plan(&mut plan, reserved_routes);
    Ok(plan)
}

pub fn resolve_asset_path(route_match: &StaticRouteMatch<'_>) -> Option<String> {
    if route_match.asset_path.is_empty() || route_match.request_is_directory {
        return route_match.descriptor.index_file.clone();
    }
    Some(route_match.asset_path.clone())
}

pub fn fallback_asset_path(route_match: &StaticRouteMatch<'_>) -> Option<String> {
    route_match.descriptor.spa_fallback.clone()
}

pub fn cache_control_value(strategy: &CacheStrategy) -> Option<String> {
    match strategy {
        CacheStrategy::None => None,
        CacheStrategy::PublicMaxAge { max_age_seconds } => {
            Some(format!("public, max-age={max_age_seconds}"))
        }
    }
}

fn collect_runtime_pack_paths(bundle_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut by_path = BTreeMap::new();
    let discover = if bundle_root.join("greentic.demo.yaml").exists() {
        domains::discover_provider_packs_cbor_only
    } else {
        domains::discover_provider_packs
    };
    for domain in [
        Domain::Messaging,
        Domain::Events,
        Domain::Secrets,
        Domain::OAuth,
    ] {
        for pack in discover(bundle_root, domain)? {
            by_path.entry(pack.path.clone()).or_insert(pack.path);
        }
    }
    Ok(by_path.into_values().collect())
}

fn read_pack_static_routes(pack_path: &Path) -> anyhow::Result<Option<Vec<StaticRouteDescriptor>>> {
    let file = std::fs::File::open(pack_path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut manifest_entry = archive.by_name("manifest.cbor").map_err(|err| {
        anyhow::anyhow!(
            "failed to open manifest.cbor in {}: {err}",
            pack_path.display()
        )
    })?;
    let mut bytes = Vec::new();
    manifest_entry.read_to_end(&mut bytes)?;
    let manifest = decode_pack_manifest(&bytes)
        .with_context(|| format!("failed to decode pack manifest in {}", pack_path.display()))?;
    let Some(extension) = manifest
        .extensions
        .as_ref()
        .and_then(|extensions| extensions.get(EXT_STATIC_ROUTES_V1))
    else {
        return Ok(None);
    };
    let inline = extension
        .inline
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("static-routes extension inline payload missing"))?;
    let ExtensionInline::Other(value) = inline else {
        anyhow::bail!("static-routes extension inline payload has unexpected type");
    };
    let decoded: StaticRoutesExtensionV1 = serde_json::from_value(value.clone())
        .with_context(|| "failed to parse greentic.static-routes.v1 payload")?;
    if decoded.schema_version != 1 {
        anyhow::bail!(
            "unsupported static-routes extension schema_version={} in {}",
            decoded.schema_version,
            pack_path.display()
        );
    }
    let pack_id = manifest.pack_id.as_str().to_string();
    let mut routes = Vec::new();
    for (idx, route) in decoded.routes.into_iter().enumerate() {
        routes.push(normalize_route_descriptor(&pack_id, pack_path, idx, route)?);
    }
    Ok(Some(routes))
}

fn normalize_route_descriptor(
    pack_id: &str,
    pack_path: &Path,
    idx: usize,
    route: StaticRouteRecord,
) -> anyhow::Result<StaticRouteDescriptor> {
    if route.team && !route.tenant {
        anyhow::bail!(
            "static route {} in {} sets team=true but tenant=false",
            route.id.as_deref().unwrap_or("<unnamed>"),
            pack_path.display()
        );
    }
    let public_path = normalize_public_path(&route.public_path);
    let route_segments = parse_route_segments(&public_path)?;
    let uses_tenant = route_segments
        .iter()
        .any(|segment| matches!(segment, RouteScopeSegment::Tenant));
    let uses_team = route_segments
        .iter()
        .any(|segment| matches!(segment, RouteScopeSegment::Team));
    if route.tenant != uses_tenant {
        anyhow::bail!(
            "static route {} in {} has inconsistent tenant flag/public_path",
            route.id.as_deref().unwrap_or("<unnamed>"),
            pack_path.display()
        );
    }
    if route.team != uses_team {
        anyhow::bail!(
            "static route {} in {} has inconsistent team flag/public_path",
            route.id.as_deref().unwrap_or("<unnamed>"),
            pack_path.display()
        );
    }

    let source_root = normalize_relative_asset_path(&route.source_root).ok_or_else(|| {
        anyhow::anyhow!(
            "static route {} in {} has invalid source_root {}",
            route.id.as_deref().unwrap_or("<unnamed>"),
            pack_path.display(),
            route.source_root
        )
    })?;
    let index_file = normalize_optional_relative_asset_path(route.index_file)?;
    let spa_fallback = normalize_optional_relative_asset_path(route.spa_fallback)?;
    let cache_strategy =
        normalize_cache_strategy(route.cache.as_ref(), pack_path, route.id.as_deref())?;

    Ok(StaticRouteDescriptor {
        route_id: route.id.unwrap_or_else(|| format!("{pack_id}::{idx}")),
        pack_id: pack_id.to_string(),
        pack_path: pack_path.to_path_buf(),
        public_path,
        source_root,
        index_file,
        spa_fallback,
        tenant_scoped: route.tenant,
        team_scoped: route.team,
        cache_strategy,
        route_segments,
    })
}

fn normalize_cache_strategy(
    cache: Option<&StaticRouteCacheRecord>,
    pack_path: &Path,
    route_id: Option<&str>,
) -> anyhow::Result<CacheStrategy> {
    let Some(cache) = cache else {
        return Ok(CacheStrategy::None);
    };
    match cache.strategy.trim() {
        "" | "none" => Ok(CacheStrategy::None),
        "public-max-age" => Ok(CacheStrategy::PublicMaxAge {
            max_age_seconds: cache.max_age_seconds.ok_or_else(|| {
                anyhow::anyhow!(
                    "static route {} in {} uses public-max-age without max_age_seconds",
                    route_id.unwrap_or("<unnamed>"),
                    pack_path.display()
                )
            })?,
        }),
        other => anyhow::bail!(
            "static route {} in {} uses unsupported cache.strategy {}",
            route_id.unwrap_or("<unnamed>"),
            pack_path.display(),
            other
        ),
    }
}

fn normalize_optional_relative_asset_path(value: Option<String>) -> anyhow::Result<Option<String>> {
    match value {
        Some(value) => normalize_relative_asset_path(&value)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("invalid asset path {}", value)),
        None => Ok(None),
    }
}

fn normalize_relative_asset_path(path: &str) -> Option<String> {
    let mut segments = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(segment) => segments.push(segment.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(segments.join("/"))
}

fn parse_route_segments(path: &str) -> anyhow::Result<Vec<RouteScopeSegment>> {
    let segments = path
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        anyhow::bail!("public_path must not be /");
    }
    let mut parsed = Vec::new();
    for segment in segments {
        match segment {
            "{tenant}" => parsed.push(RouteScopeSegment::Tenant),
            "{team}" => parsed.push(RouteScopeSegment::Team),
            _ if segment.contains('{') || segment.contains('}') => {
                anyhow::bail!("unsupported public_path segment {}", segment)
            }
            _ => parsed.push(RouteScopeSegment::Literal(segment.to_string())),
        }
    }
    let team_pos = parsed
        .iter()
        .position(|segment| matches!(segment, RouteScopeSegment::Team));
    let tenant_pos = parsed
        .iter()
        .position(|segment| matches!(segment, RouteScopeSegment::Tenant));
    if let Some(team_pos) = team_pos {
        let Some(tenant_pos) = tenant_pos else {
            anyhow::bail!("public_path uses {{team}} without {{tenant}}");
        };
        if team_pos <= tenant_pos {
            anyhow::bail!("public_path must place {{team}} after {{tenant}}");
        }
    }
    Ok(parsed)
}

fn validate_plan(plan: &mut StaticRoutePlan, reserved_routes: &ReservedRouteSet) {
    let mut seen_paths = BTreeMap::<String, String>::new();
    for route in &plan.routes {
        if reserved_routes.conflicts_with(&route.public_path) {
            plan.blocking_failures.push(format!(
                "static route {} conflicts with reserved operator path space at {}",
                route.route_id, route.public_path
            ));
        }
        if let Some(existing) = seen_paths.insert(route.public_path.clone(), route.route_id.clone())
        {
            plan.blocking_failures.push(format!(
                "static route {} duplicates public_path {} already claimed by {}",
                route.route_id, route.public_path, existing
            ));
        }
    }
    for i in 0..plan.routes.len() {
        for j in (i + 1)..plan.routes.len() {
            let left = &plan.routes[i];
            let right = &plan.routes[j];
            if paths_overlap(&left.public_path, &right.public_path) {
                plan.blocking_failures.push(format!(
                    "static routes {} ({}) and {} ({}) overlap ambiguously",
                    left.route_id, left.public_path, right.route_id, right.public_path
                ));
            }
        }
    }
}

fn paths_overlap(left: &str, right: &str) -> bool {
    path_has_prefix(left, right) || path_has_prefix(right, left)
}

fn path_has_prefix(path: &str, prefix: &str) -> bool {
    if path == prefix {
        return true;
    }
    let prefix = prefix.trim_end_matches('/');
    path.strip_prefix(prefix)
        .map(|rest| rest.starts_with('/'))
        .unwrap_or(false)
}

fn normalize_public_path(path: &str) -> String {
    let trimmed = path.trim();
    let normalized = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    };
    if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write;

    use greentic_types::{ExtensionRef, PackId, PackKind, PackManifest, PackSignatures};
    use semver::Version;
    use serde_json::json;
    use tempfile::tempdir;
    use zip::write::FileOptions;

    use super::*;

    fn write_pack(path: &Path, extension_payload: serde_json::Value) {
        let mut extensions = BTreeMap::new();
        extensions.insert(
            EXT_STATIC_ROUTES_V1.to_string(),
            ExtensionRef {
                kind: EXT_STATIC_ROUTES_V1.to_string(),
                version: "v1".to_string(),
                digest: None,
                location: None,
                inline: Some(greentic_types::ExtensionInline::Other(extension_payload)),
            },
        );
        let manifest = PackManifest {
            schema_version: "pack-v1".to_string(),
            pack_id: PackId::new("web-pack").expect("pack id"),
            name: None,
            version: Version::parse("0.1.0").expect("version"),
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
        let file = std::fs::File::create(path).expect("create pack");
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("manifest.cbor", FileOptions::<()>::default())
            .expect("start manifest");
        let bytes = greentic_types::encode_pack_manifest(&manifest).expect("manifest bytes");
        zip.write_all(&bytes).expect("write manifest");
        zip.finish().expect("finish zip");
    }

    #[test]
    fn discovers_static_routes_from_manifest_extension() {
        let tmp = tempdir().expect("tempdir");
        let providers = tmp.path().join("providers").join("messaging");
        std::fs::create_dir_all(&providers).expect("providers dir");
        let pack_path = providers.join("web.gtpack");
        write_pack(
            &pack_path,
            json!({
                "schema_version": 1,
                "routes": [{
                    "id": "docs",
                    "public_path": "/v1/web/docs",
                    "source_root": "assets/site",
                    "index_file": "index.html",
                    "spa_fallback": "index.html",
                    "cache": {"strategy": "public-max-age", "max_age_seconds": 60}
                }]
            }),
        );

        let plan = discover_from_bundle(tmp.path(), &ReservedRouteSet::operator_defaults())
            .expect("discover plan");
        assert!(
            plan.blocking_failures.is_empty(),
            "{:?}",
            plan.blocking_failures
        );
        assert_eq!(plan.routes.len(), 1);
        assert_eq!(plan.routes[0].public_path, "/v1/web/docs");
        assert_eq!(
            plan.routes[0].cache_strategy,
            CacheStrategy::PublicMaxAge {
                max_age_seconds: 60
            }
        );
    }

    #[test]
    fn rejects_reserved_and_overlapping_routes() {
        let reserved = ReservedRouteSet::operator_defaults();
        let mut plan = StaticRoutePlan {
            routes: vec![
                StaticRouteDescriptor {
                    route_id: "one".into(),
                    pack_id: "pack".into(),
                    pack_path: PathBuf::from("one.gtpack"),
                    public_path: "/api/onboard/docs".into(),
                    source_root: "assets".into(),
                    index_file: None,
                    spa_fallback: None,
                    tenant_scoped: false,
                    team_scoped: false,
                    cache_strategy: CacheStrategy::None,
                    route_segments: parse_route_segments("/api/onboard/docs").expect("segments"),
                },
                StaticRouteDescriptor {
                    route_id: "two".into(),
                    pack_id: "pack".into(),
                    pack_path: PathBuf::from("two.gtpack"),
                    public_path: "/v1/web/docs/admin".into(),
                    source_root: "assets".into(),
                    index_file: None,
                    spa_fallback: None,
                    tenant_scoped: false,
                    team_scoped: false,
                    cache_strategy: CacheStrategy::None,
                    route_segments: parse_route_segments("/v1/web/docs/admin").expect("segments"),
                },
                StaticRouteDescriptor {
                    route_id: "three".into(),
                    pack_id: "pack".into(),
                    pack_path: PathBuf::from("three.gtpack"),
                    public_path: "/v1/web/docs".into(),
                    source_root: "assets".into(),
                    index_file: None,
                    spa_fallback: None,
                    tenant_scoped: false,
                    team_scoped: false,
                    cache_strategy: CacheStrategy::None,
                    route_segments: parse_route_segments("/v1/web/docs").expect("segments"),
                },
            ],
            warnings: Vec::new(),
            blocking_failures: Vec::new(),
        };
        validate_plan(&mut plan, &reserved);
        assert_eq!(plan.blocking_failures.len(), 2);
    }

    #[test]
    fn active_route_table_matches_placeholders() {
        let route = StaticRouteDescriptor {
            route_id: "tenant-gui".into(),
            pack_id: "web".into(),
            pack_path: PathBuf::from("web.gtpack"),
            public_path: "/v1/web/webchat/{tenant}/{team}".into(),
            source_root: "assets/webchat".into(),
            index_file: Some("index.html".into()),
            spa_fallback: Some("index.html".into()),
            tenant_scoped: true,
            team_scoped: true,
            cache_strategy: CacheStrategy::None,
            route_segments: parse_route_segments("/v1/web/webchat/{tenant}/{team}")
                .expect("segments"),
        };
        let table = ActiveRouteTable::from_plan(&StaticRoutePlan {
            routes: vec![route],
            warnings: Vec::new(),
            blocking_failures: Vec::new(),
        });
        let matched = table
            .match_request("/v1/web/webchat/demo/default/app.js")
            .expect("route match");
        assert_eq!(matched.asset_path, "app.js");
    }
}
