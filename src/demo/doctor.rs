use std::collections::BTreeMap;
use std::path::Path;

use crate::capabilities::{CAP_MESSAGING_V1, CapabilityPackRecord, CapabilityRegistry};
use crate::domains::Domain;
use crate::offers::{OfferRegistry, discover_gtpacks};

pub fn demo_doctor(bundle_root: &Path, pack_command: &Path) -> anyhow::Result<()> {
    let packs_root = bundle_root.join("packs");
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

    // Messaging capability lint check
    let pack_index: BTreeMap<_, _> = discovered
        .iter()
        .map(|path| {
            let pack_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            (
                path.clone(),
                CapabilityPackRecord {
                    pack_id,
                    domain: Domain::Messaging,
                },
            )
        })
        .collect();
    if let Ok(cap_registry) = CapabilityRegistry::build_from_pack_index(&pack_index) {
        let messaging_offers = cap_registry.offers_for_capability(CAP_MESSAGING_V1);
        if messaging_offers.is_empty() {
            println!(
                "  INFO: no {} offers found — no messaging providers registered",
                CAP_MESSAGING_V1
            );
        } else {
            println!(
                "  messaging.providers registered={}",
                messaging_offers.len()
            );
        }
        for warning in cap_registry.validate_messaging_offers() {
            println!("  WARN: {warning}");
        }
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
