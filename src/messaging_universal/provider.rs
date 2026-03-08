use std::path::Path;

use crate::discovery;
use crate::domains::{self, Domain};

pub struct ProviderComponent {
    pub provider_id: String,
    pub pack: domains::ProviderPack,
}

pub fn resolve_provider_component(
    bundle: &Path,
    provider: &str,
) -> anyhow::Result<ProviderComponent> {
    resolve_provider_component_with_roots(bundle, bundle, provider)
}

pub fn resolve_provider_component_with_roots(
    bundle_root: &Path,
    bundle_read_root: &Path,
    provider: &str,
) -> anyhow::Result<ProviderComponent> {
    let cbor_only = discovery::bundle_cbor_only(bundle_root, bundle_read_root);
    if cbor_only {
        domains::ensure_cbor_packs(bundle_read_root)?;
    }
    let discovery = discovery::discover_runtime_bundle(bundle_root, bundle_read_root)?;
    let packs = domains::discover_provider_packs_with_options(
        bundle_read_root,
        Domain::Messaging,
        cbor_only,
    )?;
    for pack in packs {
        if pack.pack_id == provider || pack.file_name == format!("{provider}.gtpack") {
            return Ok(ProviderComponent {
                provider_id: provider.to_string(),
                pack,
            });
        }
        let provider_map = discovery
            .providers
            .iter()
            .find(|entry| entry.pack_path == pack.path);
        if let Some(map_entry) = provider_map
            && map_entry.provider_id == provider
        {
            return Ok(ProviderComponent {
                provider_id: map_entry.provider_id.clone(),
                pack,
            });
        }
    }
    Err(anyhow::anyhow!("provider pack not found for {}", provider))
}

#[cfg(test)]
mod tests {
    use super::resolve_provider_component_with_roots;
    use std::collections::BTreeMap;
    use std::fs;
    use zip::ZipWriter;
    use zip::write::FileOptions;

    #[test]
    fn resolve_provider_component_uses_read_root_pack_inventory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_root = tmp.path().join("state-root");
        let read_root = tmp.path().join("read-root");
        fs::create_dir_all(state_root.join("packs")).expect("state packs");
        fs::create_dir_all(read_root.join("packs")).expect("read packs");
        fs::write(state_root.join("greentic.demo.yaml"), "demo: true\n").expect("demo marker");

        let pack_path = read_root.join("packs").join("messaging-telegram.gtpack");
        let file = fs::File::create(&pack_path).expect("create gtpack");
        let mut zip = ZipWriter::new(file);
        zip.start_file("manifest.cbor", FileOptions::<()>::default())
            .expect("start manifest");
        let mut manifest_map = BTreeMap::new();
        manifest_map.insert(
            serde_cbor::Value::Text("pack_id".to_string()),
            serde_cbor::Value::Text("messaging-telegram".to_string()),
        );
        manifest_map.insert(
            serde_cbor::Value::Text("flows".to_string()),
            serde_cbor::Value::Array(Vec::new()),
        );
        let manifest =
            serde_cbor::to_vec(&serde_cbor::Value::Map(manifest_map)).expect("manifest cbor");
        use std::io::Write as _;
        zip.write_all(&manifest).expect("write manifest");
        zip.finish().expect("finish zip");

        let resolved =
            resolve_provider_component_with_roots(&state_root, &read_root, "messaging-telegram")
                .expect("resolve component");
        assert_eq!(resolved.provider_id, "messaging-telegram");
        assert_eq!(resolved.pack.path, pack_path);
    }

    #[test]
    fn resolve_provider_component_allows_json_manifest_outside_demo_mode() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_root = tmp.path().join("state-root");
        let read_root = tmp.path().join("read-root");
        fs::create_dir_all(read_root.join("packs")).expect("read packs");
        fs::create_dir_all(read_root.join("providers").join("messaging")).expect("provider dir");
        fs::create_dir_all(&state_root).expect("state root");

        let pack_path = read_root
            .join("providers")
            .join("messaging")
            .join("custom.gtpack");
        let file = fs::File::create(&pack_path).expect("create gtpack");
        let mut zip = ZipWriter::new(file);
        zip.start_file("pack.manifest.json", FileOptions::<()>::default())
            .expect("start manifest");
        let manifest = serde_json::json!({
            "meta": {
                "pack_id": "messaging-custom",
                "entry_flows": []
            }
        });
        use std::io::Write as _;
        zip.write_all(manifest.to_string().as_bytes())
            .expect("write manifest");
        zip.finish().expect("finish zip");

        let resolved =
            resolve_provider_component_with_roots(&state_root, &read_root, "messaging-custom")
                .expect("resolve component");
        assert_eq!(resolved.provider_id, "messaging-custom");
        assert_eq!(resolved.pack.path, pack_path);
    }
}
