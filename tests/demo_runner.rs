use anyhow::Result;
use greentic_operator::demo::{DemoBlockedOn, DemoRunner, UserEvent};
use greentic_operator::runtime_core::{
    RuntimeHealth, RuntimeHealthStatus, ScopedStateKey, SessionKey, SessionProvider, SessionRecord,
    StateProvider,
};
use greentic_runner_host::secrets::default_manager;
use greentic_types::flow::{ComponentRef as FlowComponentRef, FlowHasher};
use greentic_types::{
    ComponentId, Flow, FlowId, FlowKind, FlowMetadata, InputMapping, Node, NodeId, OutputMapping,
    PackFlowEntry, PackId, PackKind, PackManifest, PackSignatures, Routing, TelemetryHints,
    encode_pack_manifest,
};
use indexmap::IndexMap;
use semver::Version;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::tempdir;
use zip::{ZipWriter, write::FileOptions};

struct RecordingSessionProvider {
    puts: Arc<Mutex<usize>>,
}

#[async_trait::async_trait]
impl SessionProvider for RecordingSessionProvider {
    async fn get(&self, _key: &SessionKey) -> anyhow::Result<Option<SessionRecord>> {
        Ok(None)
    }

    async fn put(&self, _key: &SessionKey, _record: SessionRecord) -> anyhow::Result<()> {
        *self.puts.lock().expect("session puts lock poisoned") += 1;
        Ok(())
    }

    async fn compare_and_set(
        &self,
        _key: &SessionKey,
        _expected_revision: u64,
        _record: SessionRecord,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn delete(&self, _key: &SessionKey) -> anyhow::Result<()> {
        Ok(())
    }

    async fn health(&self) -> anyhow::Result<RuntimeHealth> {
        Ok(RuntimeHealth {
            status: RuntimeHealthStatus::Available,
            reason: None,
        })
    }
}

struct RecordingStateProvider;

#[async_trait::async_trait]
impl StateProvider for RecordingStateProvider {
    async fn get(&self, _key: &ScopedStateKey) -> anyhow::Result<Option<Value>> {
        Ok(None)
    }

    async fn put(&self, _key: &ScopedStateKey, _value: Value) -> anyhow::Result<()> {
        Ok(())
    }

    async fn delete(&self, _key: &ScopedStateKey) -> anyhow::Result<()> {
        Ok(())
    }

    async fn health(&self) -> anyhow::Result<RuntimeHealth> {
        Ok(RuntimeHealth {
            status: RuntimeHealthStatus::Available,
            reason: None,
        })
    }
}

#[test]
fn demo_runner_blocks_and_completes_flow() -> Result<()> {
    let bundle = tempdir()?;
    let pack_path = create_blocking_pack(bundle.path(), "demo-blocking")?;
    let secrets = default_manager()?;
    let mut runner = DemoRunner::new(
        pack_path,
        "demo",
        Some("default".to_string()),
        json!({"trigger": "start"}),
        secrets,
    )?;

    let wait_state = runner.run_until_blocked();
    match wait_state {
        DemoBlockedOn::Waiting { reason, .. } => {
            assert!(
                reason.is_some(),
                "waiting state should expose a reason for the prompt"
            );
        }
        other => panic!("expected waiting state, got {other:?}"),
    }

    runner.submit_user_event(UserEvent::raw(json!({"response": "ok"})));
    let completion = runner.run_until_blocked();
    if let DemoBlockedOn::Finished(_) = completion {
        // success
    } else {
        panic!("expected finished state, got {completion:?}");
    }

    Ok(())
}

#[test]
fn demo_runner_routes_session_state_through_provider_contracts() -> Result<()> {
    let bundle = tempdir()?;
    let pack_path = create_blocking_pack(bundle.path(), "demo-blocking")?;
    let secrets = default_manager()?;
    let mut runner = DemoRunner::with_entry_flow_and_providers(
        pack_path.clone(),
        "demo",
        Some("default".to_string()),
        ("demo.wait".to_string(), "demo-blocking".to_string()),
        json!({"trigger": "start"}),
        (
            Arc::new(RecordingSessionProvider {
                puts: Arc::new(Mutex::new(0usize)),
            }),
            Arc::new(RecordingStateProvider),
        ),
        secrets,
    )?;

    let wait_state = runner.run_until_blocked();
    assert!(matches!(wait_state, DemoBlockedOn::Waiting { .. }));

    Ok(())
}

fn create_blocking_pack(root: &Path, pack_id: &str) -> Result<PathBuf> {
    let pack_path = root.join(format!("{pack_id}.gtpack"));
    let file = File::create(&pack_path)?;
    let mut zip = ZipWriter::new(file);
    let options: FileOptions<'_, ()> = FileOptions::default();
    zip.start_file("manifest.cbor", options)?;
    let manifest = blocking_pack_manifest(pack_id)?;
    zip.write_all(&encode_pack_manifest(&manifest)?)?;
    zip.finish()?;
    Ok(pack_path)
}

fn blocking_pack_manifest(pack_id: &str) -> Result<PackManifest> {
    let mut entrypoints = BTreeMap::new();
    entrypoints.insert("default".to_string(), json!({}));

    let mut nodes = IndexMap::with_hasher(FlowHasher::default());
    nodes.insert(
        NodeId::new("wait").unwrap(),
        Node {
            id: NodeId::new("wait").unwrap(),
            component: FlowComponentRef {
                id: ComponentId::new("session.wait").unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({"reason": "await user"}),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            routing: Routing::Next {
                node_id: NodeId::new("final").unwrap(),
            },
            telemetry: TelemetryHints::default(),
        },
    );
    nodes.insert(
        NodeId::new("final").unwrap(),
        Node {
            id: NodeId::new("final").unwrap(),
            component: FlowComponentRef {
                id: ComponentId::new("emit.response").unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({"payload": {"status": "done"}}),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
        },
    );

    let flow = Flow {
        schema_version: "flow-v1".into(),
        id: FlowId::new("demo.wait").unwrap(),
        kind: FlowKind::Messaging,
        entrypoints: entrypoints.clone(),
        nodes,
        metadata: FlowMetadata::default(),
    };

    Ok(PackManifest {
        schema_version: "pack-v1".into(),
        pack_id: PackId::new(pack_id).unwrap(),
        name: None,
        version: Version::parse("0.1.0").unwrap(),
        kind: PackKind::Provider,
        publisher: "demo".to_string(),
        components: Vec::new(),
        flows: vec![PackFlowEntry {
            id: FlowId::new("demo.wait").unwrap(),
            kind: FlowKind::Messaging,
            flow,
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
