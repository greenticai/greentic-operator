use std::{
    collections::BTreeMap,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

use greentic_operator::cli::{demo_provider_pack_by_filter, demo_provider_packs};
use greentic_operator::domains::{self, Domain};
use greentic_types::flow::{FlowHasher, Routing};
use greentic_types::{
    ComponentId, Flow, FlowComponentRef, FlowId, FlowKind, FlowMetadata, InputMapping, Node,
    NodeId, OutputMapping, PackFlowEntry, PackId, PackKind, PackManifest, PackSignatures,
    TelemetryHints,
};
use indexmap::IndexMap;
use semver::Version;
use serde_json::{Value, json};
use tempfile::tempdir;
use zip::ZipWriter;
use zip::write::FileOptions;

fn write_pack(
    root: &Path,
    domain: Domain,
    pack_id: &str,
    flows: &[&str],
) -> anyhow::Result<PathBuf> {
    let providers_dir = root.join(domains::config(domain).providers_dir);
    std::fs::create_dir_all(&providers_dir)?;
    let options: FileOptions<'_, ()> = FileOptions::default();
    let pack_path = providers_dir.join(format!("{pack_id}.gtpack"));
    let manifest = build_manifest(pack_id, flows)?;
    let file = File::create(&pack_path)?;
    let mut zip = ZipWriter::new(file);
    zip.start_file("manifest.cbor", options)?;
    zip.write_all(&encode_manifest(&manifest)?)?;
    zip.finish()?;
    Ok(pack_path)
}

fn build_manifest(pack_id: &str, flows: &[&str]) -> anyhow::Result<PackManifest> {
    let mut entries = Vec::new();
    for flow_id in flows {
        let flow = simple_flow(flow_id)?;
        entries.push(PackFlowEntry {
            id: FlowId::new(flow_id).unwrap(),
            kind: FlowKind::Messaging,
            flow,
            tags: Vec::new(),
            entrypoints: vec!["default".to_string()],
        });
    }
    Ok(PackManifest {
        schema_version: "pack-v1".into(),
        pack_id: PackId::new(pack_id).unwrap(),
        name: None,
        version: Version::parse("0.1.0").unwrap(),
        kind: PackKind::Provider,
        publisher: "demo".into(),
        components: Vec::new(),
        flows: entries,
        dependencies: Vec::new(),
        capabilities: Vec::new(),
        secret_requirements: Vec::new(),
        signatures: PackSignatures::default(),
        bootstrap: None,
        extensions: None,
        agents: BTreeMap::new(),
    })
}

fn encode_manifest(manifest: &PackManifest) -> anyhow::Result<Vec<u8>> {
    Ok(greentic_types::encode_pack_manifest(manifest)?)
}

fn simple_flow(flow_id: &str) -> anyhow::Result<Flow> {
    let node_id = NodeId::new("start").unwrap();
    let mut nodes = IndexMap::with_hasher(FlowHasher::default());
    nodes.insert(
        node_id.clone(),
        Node {
            id: node_id.clone(),
            component: FlowComponentRef {
                id: ComponentId::new("emit.response").unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({"payload":{"status":"ok"}}),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
            conversational: false,
        },
    );
    let mut entrypoints = BTreeMap::new();
    entrypoints.insert("default".to_string(), Value::Null);
    Ok(Flow {
        schema_version: "flow-v1".into(),
        id: FlowId::new(flow_id).unwrap(),
        kind: FlowKind::Messaging,
        entrypoints,
        nodes,
        metadata: FlowMetadata::default(),
    })
}

#[test]
fn demo_list_packs_smoke() -> anyhow::Result<()> {
    let dir = tempdir()?;
    write_pack(dir.path(), Domain::Messaging, "demo-pack", &["flow-a"])?;
    let packs = demo_provider_packs(dir.path(), Domain::Messaging)?;
    assert!(packs.iter().any(|pack| pack.pack_id == "demo-pack"));
    Ok(())
}

#[test]
fn demo_list_flows_smoke() -> anyhow::Result<()> {
    let dir = tempdir()?;
    write_pack(
        dir.path(),
        Domain::Messaging,
        "demo-pack",
        &["alpha", "beta"],
    )?;
    let pack = demo_provider_pack_by_filter(dir.path(), Domain::Messaging, "demo-pack")?;
    assert!(pack.entry_flows.iter().any(|flow| flow == "alpha"));
    assert!(pack.entry_flows.iter().any(|flow| flow == "beta"));
    Ok(())
}
