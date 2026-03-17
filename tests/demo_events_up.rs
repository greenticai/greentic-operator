use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

use greentic_start::config::load_demo_config;
use greentic_start::runtime::demo_up_services;
use greentic_start::runtime_state::RuntimePaths;
use greentic_start::supervisor;
use greentic_types::flow::{FlowHasher, Routing};
use greentic_types::{
    ComponentId, Flow, FlowComponentRef, FlowId, FlowKind, FlowMetadata, InputMapping, Node,
    NodeId, OutputMapping, PackFlowEntry, PackId, PackKind, PackManifest, PackSignatures,
    TelemetryHints,
};
use indexmap::IndexMap;
use semver::Version;
use serde_json::{Value, json};

fn write_pack(path: &std::path::Path, pack_id: &str) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::FileOptions::<()>::default();
    zip.start_file("manifest.cbor", options)?;
    let bytes = greentic_types::encode_pack_manifest(&build_manifest(pack_id)?)?;
    std::io::Write::write_all(&mut zip, &bytes)?;
    zip.finish()?;
    Ok(())
}

fn build_manifest(pack_id: &str) -> anyhow::Result<PackManifest> {
    Ok(PackManifest {
        schema_version: "pack-v1".into(),
        pack_id: PackId::new(pack_id).unwrap(),
        name: None,
        version: Version::parse("0.1.0").unwrap(),
        kind: PackKind::Provider,
        publisher: "demo".into(),
        components: Vec::new(),
        flows: vec![PackFlowEntry {
            id: FlowId::new("setup_default").unwrap(),
            kind: FlowKind::Messaging,
            flow: simple_flow("setup_default")?,
            tags: Vec::new(),
            entrypoints: vec!["default".to_string()],
        }],
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
    let mut entrypoints = std::collections::BTreeMap::new();
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
fn demo_up_uses_in_process_events_when_events_packs_exist() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join("providers").join("events")).unwrap();
    write_pack(
        &root.join("providers").join("events").join("events.gtpack"),
        "events-pack",
    )
    .unwrap();

    let config = format!(
        r#"tenant: demo
team: default
services:
  messaging:
    enabled: false
  nats:
    enabled: true
    url: "nats://127.0.0.1:4222"
    spawn:
      enabled: true
      binary: "{nats}"
      args: []
  gateway:
    binary: "{gateway}"
    listen_addr: "127.0.0.1"
    port: 8081
    args: []
  egress:
    binary: "{egress}"
    args: []
  subscriptions:
    msgraph:
      enabled: true
      binary: "{subs}"
      mode: "poll"
      args: []
  events:
    enabled: auto
    components:
      - id: events-ingress
        binary: "{ingress}"
      - id: events-worker
        binary: "{worker}"
"#,
        nats = fake_bin("fake_nats_server").display(),
        gateway = fake_bin("fake_gsm_gateway").display(),
        egress = fake_bin("fake_gsm_egress").display(),
        subs = fake_bin("fake_gsm_msgraph_subscriptions").display(),
        ingress = fake_bin("fake_events_ingress").display(),
        worker = fake_bin("fake_events_worker").display(),
    );
    std::fs::write(root.join("greentic.yaml"), config).unwrap();
    let config_path = root.join("greentic.yaml");
    let demo_config = load_demo_config(&config_path).unwrap();
    let log_dir = root.join("logs");

    demo_up_services(
        &config_path,
        &demo_config,
        &Default::default(),
        None,
        None,
        None,
        &BTreeSet::new(),
        None,
        &log_dir,
        true,
    )
    .unwrap();

    assert!(root.join("state").exists(), "state dir missing");
    let paths = RuntimePaths::new(root.join("state"), "demo", "default");
    assert!(
        !paths.pid_path("events-ingress").exists(),
        "events-ingress should not run as external process"
    );
    assert!(
        !paths.pid_path("events-worker").exists(),
        "events-worker should not run as external process"
    );
    let _ = supervisor::stop_pidfile(&paths.pid_path("subscriptions"), 1_000);
    let _ = supervisor::stop_pidfile(&paths.pid_path("egress"), 1_000);
    let _ = supervisor::stop_pidfile(&paths.pid_path("gateway"), 1_000);
    let _ = supervisor::stop_pidfile(&paths.pid_path("nats"), 1_000);
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
