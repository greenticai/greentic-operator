use std::fs::File;
use std::io::Write;
use std::path::Path;

use greentic_operator::domains::{self, Domain, DomainAction};

fn write_pack(path: &Path, pack_id: &str, entry_flows: &[&str]) -> anyhow::Result<()> {
    let file = File::create(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::FileOptions::<()>::default();
    zip.start_file("pack.manifest.json", options)?;
    let manifest = serde_json::json!({
        "meta": {
            "pack_id": pack_id,
            "entry_flows": entry_flows,
        }
    });
    zip.write_all(serde_json::to_string(&manifest)?.as_bytes())?;
    zip.finish()?;
    Ok(())
}

#[test]
fn discover_provider_packs_sorted() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let providers = root.join("providers").join("messaging");
    std::fs::create_dir_all(&providers).unwrap();

    write_pack(&providers.join("b.gtpack"), "pack-b", &["setup_default"]).unwrap();
    write_pack(&providers.join("a.gtpack"), "pack-a", &["setup_default"]).unwrap();

    let packs = domains::discover_provider_packs(root, Domain::Messaging).unwrap();
    let names: Vec<String> = packs.into_iter().map(|pack| pack.file_name).collect();
    assert_eq!(names, vec!["a.gtpack", "b.gtpack"]);
}

#[test]
fn plan_generation_respects_flow_presence() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let providers = root.join("providers").join("events");
    std::fs::create_dir_all(&providers).unwrap();

    write_pack(
        &providers.join("event.gtpack"),
        "event-pack",
        &["diagnostics"],
    )
    .unwrap();

    let packs = domains::discover_provider_packs(root, Domain::Events).unwrap();

    let setup =
        domains::plan_runs(Domain::Events, DomainAction::Setup, &packs, None, true).unwrap();
    assert!(setup.is_empty());

    let diagnostics = domains::plan_runs(
        Domain::Events,
        DomainAction::Diagnostics,
        &packs,
        None,
        true,
    )
    .unwrap();
    assert_eq!(diagnostics.len(), 1);

    let missing_setup =
        domains::plan_runs(Domain::Events, DomainAction::Setup, &packs, None, false);
    assert!(missing_setup.is_err());
}

#[test]
fn oauth_domain_discovery_uses_oauth_provider_dir() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let providers = root.join("providers").join("oauth");
    std::fs::create_dir_all(&providers).unwrap();
    write_pack(
        &providers.join("oauth-provider.gtpack"),
        "oauth-provider",
        &["setup_default", "diagnostics"],
    )
    .unwrap();

    let packs = domains::discover_provider_packs(root, Domain::OAuth).unwrap();
    assert_eq!(packs.len(), 1);
    assert_eq!(packs[0].pack_id, "oauth-provider");
}
