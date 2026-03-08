use std::path::Path;

use crate::bundle_access::{BundleAccessHandle, operator_bundle_access_config};
use crate::offers::{OfferRegistry, discover_gtpacks};

pub fn demo_doctor(bundle_root: &Path, pack_command: &Path) -> anyhow::Result<()> {
    let bundle_access =
        BundleAccessHandle::open(bundle_root, &operator_bundle_access_config(bundle_root))?;
    let packs_root = bundle_access.active_root().join("packs");
    if !packs_root.exists() {
        return Err(anyhow::anyhow!("Bundle packs directory not found."));
    }

    let mut packs = Vec::new();
    collect_gtpacks(&packs_root, &mut packs)?;
    if packs.is_empty() {
        return Err(anyhow::anyhow!("No .gtpack files found in bundle."));
    }

    for pack in packs {
        let status = std::process::Command::new(pack_command)
            .args(["doctor", pack.to_str().unwrap_or_default()])
            .status()?;
        if !status.success() {
            return Err(anyhow::anyhow!(
                "greentic-pack doctor failed for {}",
                pack.display()
            ));
        }
    }

    let discovered = discover_gtpacks(&packs_root)?;
    let offers = OfferRegistry::from_pack_refs(&discovered)?;
    println!(
        "offer.registry.loaded total={} packs={}",
        offers.offers_total(),
        discovered.len()
    );
    for (kind, count) in offers.kind_counts() {
        println!("  kind={kind} count={count}");
    }
    for (stage, contract, count) in offers.hook_counts_by_stage_contract() {
        println!("  hooks stage={stage} contract={contract} count={count}");
    }
    for (contract, count) in offers.subs_counts_by_contract() {
        println!("  subs contract={contract} count={count}");
    }

    Ok(())
}

fn collect_gtpacks(dir: &Path, packs: &mut Vec<std::path::PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_gtpacks(&path, packs)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("gtpack") {
            packs.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::bundle_access::operator_bundle_access_config;

    #[test]
    fn doctor_bundle_access_config_uses_bundle_state_for_directory_bundles() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = operator_bundle_access_config(tmp.path());
        assert_eq!(
            config.runtime_dir,
            tmp.path().join("state").join("runtime").join("bundle_fs")
        );
    }

    #[test]
    fn doctor_bundle_access_config_uses_temp_root_for_image_bundles() {
        let sqsh = std::path::Path::new("/tmp/demo-bundle.sqsh");
        let config = operator_bundle_access_config(sqsh);
        assert!(config.runtime_dir.starts_with(std::env::temp_dir()));
        assert!(config.runtime_dir.ends_with("bundle_fs"));
    }
}
