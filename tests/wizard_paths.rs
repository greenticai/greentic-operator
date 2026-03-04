use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::json;

fn write_test_pack(path: &Path, pack_id: &str) -> anyhow::Result<()> {
    let file = File::create(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::FileOptions::<()>::default();
    zip.start_file("pack.manifest.json", options)?;
    let manifest = json!({
        "meta": {
            "pack_id": pack_id,
            "entry_flows": ["setup_default"],
        }
    });
    zip.write_all(serde_json::to_string(&manifest)?.as_bytes())?;
    zip.finish()?;
    Ok(())
}

fn wizard_command(args: &[String], stdin_payload: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_greentic-operator"));
    command.args(args);
    command.env("LC_ALL", "en_US.UTF-8");
    command.env("LANG", "en_US.UTF-8");
    command.env("LANGUAGE", "en");
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin_payload.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().expect("spawn greentic-operator");
    if let Some(payload) = stdin_payload {
        child
            .stdin
            .as_mut()
            .expect("stdin handle")
            .write_all(payload.as_bytes())
            .expect("write stdin payload");
    }
    child.wait_with_output().expect("wait command output")
}

fn provider_registry_fixture() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("provider-registry.local.json");
    format!("file://{}", path.display())
}

fn build_create_answers(path: &Path, bundle: &Path, pack_path: &Path) {
    let payload = json!({
        "bundle_path": bundle.display().to_string(),
        "bundle_name": "wizard path test",
        "locale": "en",
        "tenant": "demo",
        "team": "default",
        "pack_refs": [
            {
                "pack_ref": pack_path.display().to_string(),
                "access_scope": "all_tenants",
                "make_default_pack": "y"
            }
        ],
        "providers": [],
        "execution_mode": "dry run"
    });
    std::fs::write(path, serde_json::to_string_pretty(&payload).unwrap()).unwrap();
}

fn build_answer_document(path: &Path, answers: serde_json::Value, schema_version: &str) {
    let payload = json!({
        "wizard_id": "greentic-operator.wizard.demo",
        "schema_id": "greentic-operator.demo.wizard",
        "schema_version": schema_version,
        "locale": "en",
        "answers": answers,
        "locks": {}
    });
    std::fs::write(path, serde_json::to_string_pretty(&payload).unwrap()).unwrap();
}

fn assert_bundle_generated(bundle: &Path) {
    assert!(bundle.join("greentic.demo.yaml").exists());
    let resolved_entries = std::fs::read_dir(bundle.join("resolved"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("yaml"))
        .count();
    assert!(
        resolved_entries > 0,
        "expected resolved/*.yaml to be written"
    );
    let state_resolved_entries = std::fs::read_dir(bundle.join("state").join("resolved"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("yaml"))
        .count();
    assert!(
        state_resolved_entries > 0,
        "expected state/resolved/*.yaml to be written"
    );
    assert!(bundle.join("default.gtpack").exists());
    let providers_json = bundle.join("providers").join("providers.json");
    assert!(providers_json.exists());
    let registry_raw = std::fs::read_to_string(providers_json).unwrap();
    let registry_json: serde_json::Value = serde_json::from_str(&registry_raw).unwrap();
    let providers = registry_json
        .get("providers")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert_eq!(providers.len(), 1);
}

#[test]
fn wizard_dry_run_emit_answers_collects_payload_and_replays_bundle_create() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let pack_path = root.join("local-pack.gtpack");
    write_test_pack(&pack_path, "local-wizard-pack").unwrap();

    let bundle = root.join("bundle");
    let seed_answers = root.join("seed-answers.json");
    let emitted_answers = root.join("answers.json");
    build_create_answers(&seed_answers, &bundle, &pack_path);

    let dry_args = vec![
        "wizard".to_string(),
        "--mode".to_string(),
        "create".to_string(),
        "--qa-answers".to_string(),
        seed_answers.display().to_string(),
        "--dry-run".to_string(),
        "--emit-answers".to_string(),
        emitted_answers.display().to_string(),
        "--provider-registry".to_string(),
        provider_registry_fixture(),
    ];
    let dry_output = wizard_command(&dry_args, None);
    assert!(
        dry_output.status.success(),
        "dry-run failed stdout={} stderr={}",
        String::from_utf8_lossy(&dry_output.stdout),
        String::from_utf8_lossy(&dry_output.stderr)
    );
    assert!(emitted_answers.exists(), "answers file was not written");

    let emitted_raw = std::fs::read_to_string(&emitted_answers).unwrap();
    let emitted_json: serde_json::Value = serde_json::from_str(&emitted_raw).unwrap();
    assert_eq!(
        emitted_json
            .get("wizard_id")
            .and_then(serde_json::Value::as_str),
        Some("greentic-operator.wizard.demo")
    );
    assert_eq!(
        emitted_json
            .get("schema_id")
            .and_then(serde_json::Value::as_str),
        Some("greentic-operator.demo.wizard")
    );
    assert_eq!(
        emitted_json
            .get("schema_version")
            .and_then(serde_json::Value::as_str),
        Some("1.0.0")
    );
    let answers = emitted_json
        .get("answers")
        .and_then(serde_json::Value::as_object)
        .expect("answers object");
    assert_eq!(
        answers
            .get("bundle")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from),
        Some(bundle.clone())
    );
    assert_eq!(
        answers
            .get("bundle_name")
            .and_then(serde_json::Value::as_str),
        Some("wizard path test")
    );
    assert_eq!(
        answers.get("locale").and_then(serde_json::Value::as_str),
        Some("en")
    );
    assert_eq!(
        answers.get("tenant").and_then(serde_json::Value::as_str),
        Some("demo")
    );
    assert_eq!(
        answers.get("team").and_then(serde_json::Value::as_str),
        Some("default")
    );
    assert_eq!(
        answers
            .get("execution_mode")
            .and_then(serde_json::Value::as_str),
        Some("dry run")
    );
    let pack_refs = answers
        .get("pack_refs")
        .and_then(serde_json::Value::as_array)
        .expect("pack_refs array");
    assert_eq!(pack_refs.len(), 1, "expected one pack_ref");
    assert_eq!(
        pack_refs
            .first()
            .and_then(|value| value.get("pack_ref"))
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from),
        Some(pack_path.clone())
    );
    assert_eq!(
        answers
            .get("providers")
            .and_then(serde_json::Value::as_array)
            .map(|items| items.len()),
        Some(0)
    );

    let exec_args = vec![
        "wizard".to_string(),
        "--mode".to_string(),
        "create".to_string(),
        "--answers".to_string(),
        emitted_answers.display().to_string(),
        "--provider-registry".to_string(),
        provider_registry_fixture(),
        "--execute".to_string(),
        "--offline".to_string(),
    ];
    let exec_output = wizard_command(&exec_args, None);
    assert!(
        exec_output.status.success(),
        "execute failed stdout={} stderr={}",
        String::from_utf8_lossy(&exec_output.stdout),
        String::from_utf8_lossy(&exec_output.stderr)
    );
    assert_bundle_generated(&bundle);
}

#[test]
fn wizard_dry_run_writes_answers_then_replay_executes_bundle() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    let pack_path = root.join("local-pack.gtpack");
    write_test_pack(&pack_path, "local-wizard-pack").unwrap();

    let bundle = root.join("bundle");
    let seed_answers = root.join("seed-answers.json");
    let saved_answers = root.join("answers-replay.json");
    build_create_answers(&seed_answers, &bundle, &pack_path);

    let dry_args = vec![
        "wizard".to_string(),
        "--mode".to_string(),
        "create".to_string(),
        "--qa-answers".to_string(),
        seed_answers.display().to_string(),
        "--provider-registry".to_string(),
        provider_registry_fixture(),
    ];
    let dry_output = wizard_command(&dry_args, Some(&format!("{}\n", saved_answers.display())));
    assert!(
        dry_output.status.success(),
        "dry-run failed stdout={} stderr={}",
        String::from_utf8_lossy(&dry_output.stdout),
        String::from_utf8_lossy(&dry_output.stderr)
    );
    assert!(saved_answers.exists(), "answers file was not written");

    let replay_raw = std::fs::read_to_string(&saved_answers).unwrap();
    let replay_json: serde_json::Value = serde_json::from_str(&replay_raw).unwrap();
    assert_eq!(
        replay_json
            .get("wizard_id")
            .and_then(serde_json::Value::as_str),
        Some("greentic-operator.wizard.demo")
    );
    assert_eq!(
        replay_json
            .get("schema_version")
            .and_then(serde_json::Value::as_str),
        Some("1.0.0")
    );
    assert_eq!(
        replay_json
            .get("answers")
            .and_then(|value| value.get("execution_mode"))
            .and_then(serde_json::Value::as_str),
        Some("dry run")
    );

    let exec_args = vec![
        "wizard".to_string(),
        "--mode".to_string(),
        "create".to_string(),
        "--answers".to_string(),
        saved_answers.display().to_string(),
        "--provider-registry".to_string(),
        provider_registry_fixture(),
        "--execute".to_string(),
        "--offline".to_string(),
    ];
    let exec_output = wizard_command(&exec_args, None);
    assert!(
        exec_output.status.success(),
        "execute failed stdout={} stderr={}",
        String::from_utf8_lossy(&exec_output.stdout),
        String::from_utf8_lossy(&exec_output.stderr)
    );

    assert_bundle_generated(&bundle);
}

#[test]
fn wizard_execute_existing_bundle_abort_and_overwrite_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    let pack_path = root.join("local-pack.gtpack");
    write_test_pack(&pack_path, "local-wizard-pack").unwrap();

    let bundle = root.join("bundle");
    let answers = root.join("answers.json");
    build_create_answers(&answers, &bundle, &pack_path);

    let run_args = vec![
        "wizard".to_string(),
        "--mode".to_string(),
        "create".to_string(),
        "--answers".to_string(),
        answers.display().to_string(),
        "--provider-registry".to_string(),
        provider_registry_fixture(),
        "--execute".to_string(),
        "--offline".to_string(),
    ];

    let first = wizard_command(&run_args, None);
    assert!(
        first.status.success(),
        "first execute failed stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let marker = bundle.join("marker.txt");
    std::fs::write(&marker, "keep-if-no-overwrite").unwrap();

    let abort = wizard_command(&run_args, Some("n\n"));
    assert!(
        abort.status.success(),
        "abort path failed stdout={} stderr={}",
        String::from_utf8_lossy(&abort.stdout),
        String::from_utf8_lossy(&abort.stderr)
    );
    let abort_stdout = String::from_utf8_lossy(&abort.stdout);
    assert!(abort_stdout.contains("wizard execution aborted by user"));
    assert!(marker.exists(), "bundle should remain when overwrite=no");

    let overwrite = wizard_command(&run_args, Some("y\n"));
    assert!(
        overwrite.status.success(),
        "overwrite path failed stdout={} stderr={}",
        String::from_utf8_lossy(&overwrite.stdout),
        String::from_utf8_lossy(&overwrite.stderr)
    );
    let overwrite_stdout = String::from_utf8_lossy(&overwrite.stdout);
    assert!(overwrite_stdout.contains("wizard execute complete"));
    assert!(
        !marker.exists(),
        "bundle should be recreated when overwrite=yes"
    );
    assert!(bundle.join("default.gtpack").exists());
}

#[test]
fn wizard_migrate_accepts_older_schema_version() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let pack_path = root.join("local-pack.gtpack");
    write_test_pack(&pack_path, "local-wizard-pack").unwrap();
    let bundle = root.join("bundle");
    let answers_payload = json!({
        "bundle_path": bundle.display().to_string(),
        "bundle_name": "wizard path test",
        "locale": "en",
        "tenant": "demo",
        "team": "default",
        "pack_refs": [
            {
                "pack_ref": pack_path.display().to_string(),
                "access_scope": "all_tenants",
                "make_default_pack": "y"
            }
        ],
        "providers": [],
        "execution_mode": "dry run"
    });
    let answers_doc = root.join("answers-doc.json");
    build_answer_document(&answers_doc, answers_payload, "0.9.0");
    let migrated_out = root.join("migrated-answers.json");

    let args = vec![
        "wizard".to_string(),
        "--mode".to_string(),
        "create".to_string(),
        "--answers".to_string(),
        answers_doc.display().to_string(),
        "--migrate".to_string(),
        "--validate".to_string(),
        "--emit-answers".to_string(),
        migrated_out.display().to_string(),
        "--provider-registry".to_string(),
        provider_registry_fixture(),
    ];
    let output = wizard_command(&args, None);
    assert!(
        output.status.success(),
        "migrate validate failed stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = std::fs::read_to_string(migrated_out).unwrap();
    let migrated: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        migrated
            .get("schema_version")
            .and_then(serde_json::Value::as_str),
        Some("1.0.0")
    );
}
