use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_cbor::Value as CborValue;
use zip::result::ZipError;

use crate::bundle_access::{BundleAccessHandle, operator_bundle_access_config};
use crate::domains::{self, Domain};
use crate::runtime_state::write_json;

#[derive(Clone, Debug, Serialize)]
pub struct DiscoveryResult {
    pub domains: DetectedDomains,
    pub providers: Vec<DetectedProvider>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DetectedDomains {
    pub messaging: bool,
    pub events: bool,
    pub oauth: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct DetectedProvider {
    pub provider_id: String,
    pub domain: String,
    pub pack_path: PathBuf,
    pub id_source: ProviderIdSource,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderIdSource {
    Manifest,
    Filename,
}

#[derive(Default)]
pub struct DiscoveryOptions {
    pub cbor_only: bool,
}

pub fn bundle_cbor_only(bundle_root: &Path, bundle_read_root: &Path) -> bool {
    bundle_root.join("greentic.demo.yaml").exists()
        || bundle_read_root.join("greentic.demo.yaml").exists()
}

pub fn discover(root: &Path) -> anyhow::Result<DiscoveryResult> {
    discover_bundle_with_options(root, DiscoveryOptions::default())
}

pub fn discover_bundle_with_options(
    bundle_ref: &Path,
    options: DiscoveryOptions,
) -> anyhow::Result<DiscoveryResult> {
    let bundle_access =
        BundleAccessHandle::open(bundle_ref, &operator_bundle_access_config(bundle_ref))?;
    discover_with_options(bundle_access.active_root(), options)
}

pub fn discover_bundle_auto(bundle_ref: &Path) -> anyhow::Result<DiscoveryResult> {
    let bundle_access =
        BundleAccessHandle::open(bundle_ref, &operator_bundle_access_config(bundle_ref))?;
    discover_runtime_bundle(bundle_ref, bundle_access.active_root())
}

pub fn discover_bundle_cbor_only(bundle_ref: &Path) -> anyhow::Result<DiscoveryResult> {
    discover_bundle_with_options(bundle_ref, DiscoveryOptions { cbor_only: true })
}

pub fn discover_validated_bundle_cbor_only(bundle_ref: &Path) -> anyhow::Result<DiscoveryResult> {
    domains::ensure_bundle_cbor_packs(bundle_ref)?;
    discover_bundle_cbor_only(bundle_ref)
}

pub fn discover_with_options(
    root: &Path,
    options: DiscoveryOptions,
) -> anyhow::Result<DiscoveryResult> {
    let mut providers = Vec::new();
    for domain in [Domain::Messaging, Domain::Events, Domain::OAuth] {
        let cfg = domains::config(domain);
        let providers_dir = root.join(cfg.providers_dir);
        if !providers_dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(&providers_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("gtpack") {
                continue;
            }
            let (provider_id, id_source) = match if options.cbor_only {
                read_pack_id_from_manifest_cbor_only(&path)?
            } else {
                read_pack_id_from_manifest(&path)?
            } {
                Some(pack_id) => (pack_id, ProviderIdSource::Manifest),
                None => {
                    let stem = path
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default()
                        .to_string();
                    (stem, ProviderIdSource::Filename)
                }
            };
            providers.push(DetectedProvider {
                provider_id,
                domain: domains::domain_name(domain).to_string(),
                pack_path: path,
                id_source,
            });
        }
    }
    providers.sort_by(|a, b| a.pack_path.cmp(&b.pack_path));
    let domains = DetectedDomains {
        messaging: providers
            .iter()
            .any(|provider| provider.domain == "messaging"),
        events: providers.iter().any(|provider| provider.domain == "events"),
        oauth: providers.iter().any(|provider| provider.domain == "oauth"),
    };
    Ok(DiscoveryResult { domains, providers })
}

pub fn discover_runtime_bundle(
    bundle_root: &Path,
    bundle_read_root: &Path,
) -> anyhow::Result<DiscoveryResult> {
    discover_with_options(
        bundle_read_root,
        DiscoveryOptions {
            cbor_only: bundle_cbor_only(bundle_root, bundle_read_root),
        },
    )
}

pub fn persist(root: &Path, tenant: &str, discovery: &DiscoveryResult) -> anyhow::Result<()> {
    let runtime_root = root.join("state").join("runtime").join(tenant);
    let domains_path = runtime_root.join("detected_domains.json");
    let providers_path = runtime_root.join("detected_providers.json");
    write_json(&domains_path, &discovery.domains)?;
    write_json(&providers_path, &discovery.providers)?;
    Ok(())
}

fn read_pack_id_from_manifest(path: &Path) -> anyhow::Result<Option<String>> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    if let Some(parsed) = read_manifest_cbor_for_discovery(&mut archive).map_err(|err| {
        anyhow::anyhow!(
            "failed to decode manifest.cbor in {}: {err}",
            path.display()
        )
    })? {
        return extract_pack_id(parsed);
    }
    if let Some(parsed) = read_manifest_json_for_discovery(&mut archive, "pack.manifest.json")
        .map_err(|err| {
            anyhow::anyhow!(
                "failed to decode pack.manifest.json in {}: {err}",
                path.display()
            )
        })?
    {
        return extract_pack_id(parsed);
    }
    Ok(None)
}

fn read_pack_id_from_manifest_cbor_only(path: &Path) -> anyhow::Result<Option<String>> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    if let Some(parsed) = read_manifest_cbor_for_discovery(&mut archive).map_err(|err| {
        anyhow::anyhow!(
            "failed to decode manifest.cbor in {}: {err}",
            path.display()
        )
    })? {
        return extract_pack_id(parsed);
    }
    Err(missing_cbor_error(path))
}

fn extract_pack_id(parsed: domains::PackManifestForDiscovery) -> anyhow::Result<Option<String>> {
    if let Some(meta) = parsed.meta {
        return Ok(Some(meta.pack_id));
    }
    if let Some(pack_id) = parsed.pack_id {
        return Ok(Some(pack_id));
    }
    Ok(None)
}

fn read_manifest_cbor_for_discovery(
    archive: &mut zip::ZipArchive<std::fs::File>,
) -> anyhow::Result<Option<domains::PackManifestForDiscovery>> {
    let mut file = match archive.by_name("manifest.cbor") {
        Ok(file) => file,
        Err(ZipError::FileNotFound) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    let value: CborValue = serde_cbor::from_slice(&bytes)?;
    if let Some(pack_id) = extract_pack_id_from_value(&value)? {
        return Ok(Some(domains::PackManifestForDiscovery {
            meta: None,
            pack_id: Some(pack_id),
        }));
    }
    Ok(None)
}

fn read_manifest_json_for_discovery(
    archive: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
) -> anyhow::Result<Option<domains::PackManifestForDiscovery>> {
    let mut file = match archive.by_name(name) {
        Ok(file) => file,
        Err(ZipError::FileNotFound) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut contents = String::new();
    std::io::Read::read_to_string(&mut file, &mut contents)?;
    let parsed: domains::PackManifestForDiscovery = serde_json::from_str(&contents)?;
    Ok(Some(parsed))
}

fn extract_pack_id_from_value(value: &CborValue) -> anyhow::Result<Option<String>> {
    let CborValue::Map(map) = value else {
        return Ok(None);
    };
    let symbols = match map_get(map, "symbols") {
        Some(CborValue::Map(map)) => Some(map),
        _ => None,
    };

    if let Some(pack_id) = map_get(map, "pack_id")
        && let Some(value) = resolve_string_symbol(pack_id, symbols, "pack_ids")?
    {
        return Ok(Some(value));
    }

    if let Some(CborValue::Map(meta)) = map_get(map, "meta")
        && let Some(pack_id) = map_get(meta, "pack_id")
        && let Some(value) = resolve_string_symbol(pack_id, symbols, "pack_ids")?
    {
        return Ok(Some(value));
    }

    Ok(None)
}

fn resolve_string_symbol(
    value: &CborValue,
    symbols: Option<&std::collections::BTreeMap<CborValue, CborValue>>,
    symbol_key: &str,
) -> anyhow::Result<Option<String>> {
    match value {
        CborValue::Text(text) => Ok(Some(text.clone())),
        CborValue::Integer(idx) => {
            let Some(symbols) = symbols else {
                return Ok(Some(idx.to_string()));
            };
            let Some(CborValue::Array(values)) = map_get(symbols, symbol_key)
                .or_else(|| map_get(symbols, symbol_key.strip_suffix('s').unwrap_or(symbol_key)))
            else {
                return Ok(Some(idx.to_string()));
            };
            let idx = usize::try_from(*idx).unwrap_or(usize::MAX);
            match values.get(idx) {
                Some(CborValue::Text(text)) => Ok(Some(text.clone())),
                _ => Ok(Some(idx.to_string())),
            }
        }
        _ => Ok(None),
    }
}

fn map_get<'a>(
    map: &'a std::collections::BTreeMap<CborValue, CborValue>,
    key: &str,
) -> Option<&'a CborValue> {
    map.iter().find_map(|(k, v)| match k {
        CborValue::Text(text) if text == key => Some(v),
        _ => None,
    })
}

fn missing_cbor_error(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "ERROR: demo packs must be CBOR-only (.gtpack must contain manifest.cbor). Rebuild the pack with greentic-pack build (do not use --dev). Missing in {}",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::{bundle_cbor_only, discover_runtime_bundle, discover_validated_bundle_cbor_only};
    use std::fs;
    use zip::ZipWriter;
    use zip::write::FileOptions;

    #[test]
    fn bundle_cbor_only_checks_original_and_read_roots() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bundle_root = tmp.path().join("bundle");
        let bundle_read_root = tmp.path().join("bundle-read");
        fs::create_dir_all(&bundle_root).expect("bundle root");
        fs::create_dir_all(&bundle_read_root).expect("bundle read root");

        assert!(!bundle_cbor_only(&bundle_root, &bundle_read_root));

        fs::write(bundle_root.join("greentic.demo.yaml"), "demo: true\n").expect("demo marker");
        assert!(bundle_cbor_only(&bundle_root, &bundle_read_root));

        fs::remove_file(bundle_root.join("greentic.demo.yaml")).expect("remove demo marker");
        fs::write(bundle_read_root.join("greentic.demo.yaml"), "demo: true\n")
            .expect("read-root demo marker");
        assert!(bundle_cbor_only(&bundle_root, &bundle_read_root));
    }

    #[test]
    fn discover_runtime_bundle_uses_json_manifest_for_plain_bundle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bundle_root = tmp.path().join("bundle");
        let bundle_read_root = tmp.path().join("bundle-read");
        fs::create_dir_all(bundle_root.join("providers").join("messaging"))
            .expect("bundle providers");
        fs::create_dir_all(bundle_read_root.join("providers").join("messaging"))
            .expect("read providers");

        let pack_path = bundle_read_root
            .join("providers")
            .join("messaging")
            .join("plain.gtpack");
        let file = fs::File::create(&pack_path).expect("create gtpack");
        let mut zip = ZipWriter::new(file);
        zip.start_file("pack.manifest.json", FileOptions::<()>::default())
            .expect("start manifest");
        let manifest = serde_json::json!({
            "meta": {
                "pack_id": "plain-pack",
                "entry_flows": []
            }
        });
        use std::io::Write as _;
        zip.write_all(manifest.to_string().as_bytes())
            .expect("write manifest");
        zip.finish().expect("finish zip");

        let discovery =
            discover_runtime_bundle(&bundle_root, &bundle_read_root).expect("runtime discovery");
        assert_eq!(discovery.providers.len(), 1);
        assert_eq!(discovery.providers[0].provider_id, "plain-pack");
    }

    #[test]
    fn discover_validated_bundle_cbor_only_rejects_demo_bundle_without_cbor_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bundle_root = tmp.path();
        fs::create_dir_all(bundle_root.join("providers").join("messaging")).expect("providers dir");
        fs::write(bundle_root.join("greentic.demo.yaml"), "demo: true\n").expect("demo marker");

        let pack_path = bundle_root
            .join("providers")
            .join("messaging")
            .join("broken.gtpack");
        let file = fs::File::create(&pack_path).expect("create gtpack");
        let mut zip = ZipWriter::new(file);
        zip.start_file("pack.manifest.json", FileOptions::<()>::default())
            .expect("start manifest");
        let manifest = serde_json::json!({
            "meta": {
                "pack_id": "broken-pack",
                "entry_flows": []
            }
        });
        use std::io::Write as _;
        zip.write_all(manifest.to_string().as_bytes())
            .expect("write manifest");
        zip.finish().expect("finish zip");

        let err = discover_validated_bundle_cbor_only(bundle_root).expect_err("should fail");
        assert!(err.to_string().contains("manifest.cbor"));
    }
}
