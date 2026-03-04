use indexmap::IndexMap;
use semver::Version;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use greentic_types::flow::{FlowHasher, Routing};
use greentic_types::{
    ComponentId, Flow, FlowComponentRef, FlowId, FlowKind, FlowMetadata, InputMapping, Node,
    NodeId, OutputMapping, PackFlowEntry, PackId, PackKind, PackManifest, PackSignatures,
    TelemetryHints,
};

fn write_pack(path: &std::path::Path, pack_id: &str, entry_flows: &[&str]) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::FileOptions::<()>::default();
    zip.start_file("manifest.cbor", options)?;
    let bytes = greentic_types::encode_pack_manifest(&build_manifest(pack_id, entry_flows)?)?;
    std::io::Write::write_all(&mut zip, &bytes)?;
    zip.finish()?;
    Ok(())
}

fn build_manifest(pack_id: &str, flows: &[&str]) -> anyhow::Result<PackManifest> {
    let mut entries = Vec::new();
    for flow_id in flows {
        entries.push(PackFlowEntry {
            id: FlowId::new(flow_id).unwrap(),
            kind: FlowKind::Messaging,
            flow: simple_flow(flow_id)?,
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
    })
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
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
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
fn demo_setup_runs_all_domains() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let messaging = root.join("providers").join("messaging");
    let events = root.join("providers").join("events");
    std::fs::create_dir_all(&messaging).unwrap();
    std::fs::create_dir_all(&events).unwrap();
    write_pack(&messaging.join("a.gtpack"), "msg-a", &["setup_default"]).unwrap();
    write_pack(&events.join("b.gtpack"), "evt-b", &["setup_default"]).unwrap();

    let status = Command::new(fake_bin("greentic-operator"))
        .args([
            "demo",
            "setup",
            "--bundle",
            root.to_string_lossy().as_ref(),
            "--tenant",
            "demo",
            "--domain",
            "all",
            "--runner-binary",
            fake_bin("fake_runner").to_string_lossy().as_ref(),
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let providers_root = root
        .join("state")
        .join("runtime")
        .join("demo")
        .join("providers");
    assert!(providers_root.join("msg-a.setup.json").exists());
    assert!(providers_root.join("evt-b.setup.json").exists());
    assert!(
        providers_root
            .join("msg-a")
            .join("config.envelope.cbor")
            .exists()
    );
    assert!(
        providers_root
            .join("msg-a")
            .join("answers")
            .join("setup.answers.json")
            .exists()
    );
    assert!(
        providers_root
            .join("msg-a")
            .join("answers")
            .join("setup.answers.cbor")
            .exists()
    );
    assert!(
        providers_root
            .join("evt-b")
            .join("config.envelope.cbor")
            .exists()
    );
    assert!(
        providers_root
            .join("evt-b")
            .join("answers")
            .join("setup.answers.json")
            .exists()
    );
    assert!(
        providers_root
            .join("evt-b")
            .join("answers")
            .join("setup.answers.cbor")
            .exists()
    );
}

#[test]
fn demo_setup_best_effort_skips_missing_setup() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let messaging = root.join("providers").join("messaging");
    std::fs::create_dir_all(&messaging).unwrap();
    write_pack(
        &messaging.join("good.gtpack"),
        "msg-good",
        &["setup_default"],
    )
    .unwrap();
    write_pack(&messaging.join("bad.gtpack"), "msg-bad", &["diagnostics"]).unwrap();

    let status = Command::new(fake_bin("greentic-operator"))
        .args([
            "demo",
            "setup",
            "--bundle",
            root.to_string_lossy().as_ref(),
            "--tenant",
            "demo",
            "--domain",
            "messaging",
            "--runner-binary",
            fake_bin("fake_runner").to_string_lossy().as_ref(),
            "--best-effort",
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let providers_root = root
        .join("state")
        .join("runtime")
        .join("demo")
        .join("providers");
    assert!(providers_root.join("msg-good.setup.json").exists());
    assert!(!providers_root.join("msg-bad.setup.json").exists());
    assert!(
        providers_root
            .join("msg-good")
            .join("config.envelope.cbor")
            .exists()
    );
    assert!(
        providers_root
            .join("msg-good")
            .join("answers")
            .join("setup.answers.json")
            .exists()
    );
    assert!(
        providers_root
            .join("msg-good")
            .join("answers")
            .join("setup.answers.cbor")
            .exists()
    );
    assert!(
        !providers_root
            .join("msg-bad")
            .join("config.envelope.cbor")
            .exists()
    );
}

fn fake_bin(name: &str) -> PathBuf {
    if name == "greentic-operator" {
        return PathBuf::from(env!("CARGO_BIN_EXE_greentic-operator"));
    }
    example_bin(name)
}

fn binary_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn example_bin(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.file_name().and_then(|name| name.to_str()) == Some("deps") {
        path.pop();
    }
    let candidate = path.join("examples").join(binary_name(name));
    if candidate.exists() {
        return candidate;
    }
    let status = Command::new("cargo")
        .args(["build", "--example", name])
        .status()
        .expect("failed to build example binary");
    assert!(status.success(), "failed to build example binary");
    candidate
}
