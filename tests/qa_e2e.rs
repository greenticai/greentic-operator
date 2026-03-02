//! End-to-end tests for the QA setup integration.
//!
//! These tests exercise the FormSpec pipeline:
//! 1. setup.yaml → FormSpec conversion
//! 2. FormSpec-based answer validation
//! 3. Secret field identification and filtering
//! 4. Config persistence (non-secret fields)

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use greentic_operator::qa_persist;
use greentic_operator::qa_setup_wizard;
use greentic_operator::setup_input::SetupInputAnswers;
use greentic_operator::setup_to_formspec;
use qa_spec::{FormSpec, QuestionSpec, QuestionType};
use serde_json::json;

/// Create a test .gtpack file with a setup.yaml inside.
fn create_test_pack_with_setup(dir: &Path, provider_id: &str, yaml: &str) -> std::path::PathBuf {
    let pack_path = dir.join(format!("{provider_id}.gtpack"));
    let file = std::fs::File::create(&pack_path).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    // Write setup.yaml
    writer.start_file("assets/setup.yaml", options).unwrap();
    writer.write_all(yaml.as_bytes()).unwrap();

    // Write minimal manifest.cbor
    let manifest = json!({
        "schema_version": "1.0.0",
        "pack_id": provider_id,
        "name": provider_id,
        "version": "1.0.0",
        "kind": "provider",
        "publisher": "tests",
        "components": [{
            "id": provider_id,
            "version": "1.0.0",
            "supports": ["provider"],
            "world": "greentic:component/component-v0-v6-v0@0.6.0",
            "profiles": {},
            "capabilities": { "provides": ["messaging"], "requires": [] },
            "configurators": null,
            "operations": [],
            "config_schema": {"type": "object"},
            "resources": {},
            "dev_flows": {}
        }],
        "flows": [],
        "dependencies": [],
        "capabilities": [],
        "secret_requirements": [],
        "signatures": [],
        "extensions": {}
    });
    let cbor_bytes = greentic_types::cbor::canonical::to_canonical_cbor(&manifest).unwrap();
    writer.start_file("manifest.cbor", options).unwrap();
    writer.write_all(&cbor_bytes).unwrap();

    writer.finish().unwrap();
    pack_path
}

#[test]
fn e2e_setup_yaml_to_formspec_and_validate() {
    let temp = tempfile::tempdir().unwrap();
    let yaml = r#"
title: "Telegram Setup"
questions:
  - name: enabled
    kind: boolean
    required: true
    help: "Enable this provider"
    default: true
    title: "Enable provider"
  - name: public_base_url
    kind: string
    required: true
    help: "Public URL for webhook callbacks"
  - name: bot_token
    kind: string
    required: true
    help: "Telegram bot token"
    secret: true
    title: "Bot Token"
  - name: default_chat_id
    kind: string
    required: false
    help: "Default chat ID for egress"
"#;
    let pack_path = create_test_pack_with_setup(temp.path(), "messaging-telegram", yaml);

    // Step 1: Convert to FormSpec
    let form_spec = setup_to_formspec::pack_to_form_spec(&pack_path, "messaging-telegram");
    assert!(
        form_spec.is_some(),
        "FormSpec should be built from setup.yaml"
    );
    let form_spec = form_spec.unwrap();

    assert_eq!(form_spec.id, "messaging-telegram-setup");
    assert_eq!(form_spec.title, "Telegram Setup");
    assert_eq!(form_spec.questions.len(), 4);

    // Step 2: Verify secret detection
    let secret_ids: Vec<&str> = form_spec
        .questions
        .iter()
        .filter(|q| q.secret)
        .map(|q| q.id.as_str())
        .collect();
    assert!(
        secret_ids.contains(&"bot_token"),
        "bot_token should be marked as secret"
    );

    // Step 3: Verify URL constraint on public_base_url
    let url_q = form_spec
        .questions
        .iter()
        .find(|q| q.id == "public_base_url")
        .unwrap();
    assert!(
        url_q.constraint.is_some(),
        "public_base_url should have URL constraint"
    );

    // Step 4: Validate good answers
    let good_answers = json!({
        "enabled": true,
        "public_base_url": "https://example.com/webhook",
        "bot_token": "123:ABC"
    });
    assert!(qa_setup_wizard::validate_answers_against_form_spec(&form_spec, &good_answers).is_ok());

    // Step 5: Validate bad URL
    let bad_url_answers = json!({
        "enabled": true,
        "public_base_url": "not-a-url",
        "bot_token": "123:ABC"
    });
    let err = qa_setup_wizard::validate_answers_against_form_spec(&form_spec, &bad_url_answers);
    assert!(err.is_err(), "should reject invalid URL");
    assert!(err.unwrap_err().to_string().contains("pattern"));

    // Step 6: Validate missing required
    let missing_required = json!({
        "enabled": true,
        "public_base_url": "https://example.com"
    });
    let err = qa_setup_wizard::validate_answers_against_form_spec(&form_spec, &missing_required);
    assert!(err.is_err(), "should reject missing required field");
    assert!(err.unwrap_err().to_string().contains("bot_token"));
}

#[test]
fn e2e_run_qa_setup_with_preloaded_answers() {
    let temp = tempfile::tempdir().unwrap();
    let yaml = r#"
title: "Slack Setup"
questions:
  - name: slack_bot_token
    kind: string
    required: true
    secret: true
    title: "Slack Bot Token"
  - name: channel_id
    kind: string
    required: false
    title: "Default Channel"
"#;
    let pack_path = create_test_pack_with_setup(temp.path(), "messaging-slack", yaml);

    let raw = json!({
        "messaging-slack": {
            "slack_bot_token": "xoxb-123456",
            "channel_id": "C12345"
        }
    });
    let provider_keys = BTreeSet::from(["messaging-slack".to_string()]);
    let answers = SetupInputAnswers::new(raw, provider_keys).unwrap();

    let (result, form_spec) = qa_setup_wizard::run_qa_setup(
        &pack_path,
        "messaging-slack",
        Some(&answers),
        false, // non-interactive
        None,
    )
    .unwrap();

    assert_eq!(result.get("slack_bot_token").unwrap(), "xoxb-123456");
    assert_eq!(result.get("channel_id").unwrap(), "C12345");
    assert!(form_spec.is_some(), "FormSpec should be available");
}

#[test]
fn e2e_secret_filtering() {
    let form_spec = FormSpec {
        id: "test-setup".into(),
        title: "Test".into(),
        version: "1.0.0".into(),
        description: None,
        presentation: None,
        progress_policy: None,
        secrets_policy: None,
        store: vec![],
        validations: vec![],
        includes: vec![],
        questions: vec![
            QuestionSpec {
                id: "url".into(),
                kind: QuestionType::String,
                title: "URL".into(),
                title_i18n: None,
                description: None,
                description_i18n: None,
                required: true,
                choices: None,
                default_value: None,
                secret: false,
                visible_if: None,
                constraint: None,
                list: None,
                computed: None,
                policy: Default::default(),
                computed_overridable: false,
            },
            QuestionSpec {
                id: "token".into(),
                kind: QuestionType::String,
                title: "Token".into(),
                title_i18n: None,
                description: None,
                description_i18n: None,
                required: true,
                choices: None,
                default_value: None,
                secret: true,
                visible_if: None,
                constraint: None,
                list: None,
                computed: None,
                policy: Default::default(),
                computed_overridable: false,
            },
        ],
    };

    let config = json!({
        "url": "https://api.example.com",
        "token": "secret123",
        "extra": "data"
    });

    // persist_qa_config writes to a temp dir — verify it filters secrets
    let temp = tempfile::tempdir().unwrap();
    let pack_path = create_test_pack_with_setup(temp.path(), "test-provider", "questions: []");
    let providers_root = temp.path().join("state/runtime/demo/providers");

    let result = qa_persist::persist_qa_config(
        &providers_root,
        "test-provider",
        &config,
        &pack_path,
        &form_spec,
        false,
    );
    assert!(result.is_ok(), "persist_qa_config should succeed");

    // Read back the envelope and verify token is filtered out
    let envelope = greentic_operator::provider_config_envelope::read_provider_config_envelope(
        &providers_root,
        "test-provider",
    )
    .unwrap();
    assert!(envelope.is_some(), "config envelope should be written");
    let envelope = envelope.unwrap();
    assert_eq!(
        envelope.config.get("url").unwrap(),
        "https://api.example.com"
    );
    assert!(
        envelope.config.get("token").is_none(),
        "secret field 'token' should be filtered from config envelope"
    );
    assert_eq!(envelope.config.get("extra").unwrap(), "data");
}

#[test]
fn e2e_qa_bridge_form_spec_from_provider_output() {
    use greentic_operator::demo::qa_bridge;
    use std::collections::HashMap;

    let qa_output = json!({
        "mode": "setup",
        "title": {"key": "telegram.qa.setup.title"},
        "questions": [
            {"id": "enabled", "label": {"key": "telegram.qa.setup.enabled"}, "required": true},
            {"id": "bot_token", "label": {"key": "telegram.qa.setup.bot_token"}, "required": true},
            {"id": "public_base_url", "label": {"key": "telegram.qa.setup.public_base_url"}, "required": true},
        ]
    });

    let mut i18n = HashMap::new();
    i18n.insert("telegram.qa.setup.title".into(), "Telegram Setup".into());
    i18n.insert("telegram.qa.setup.enabled".into(), "Enable provider".into());
    i18n.insert("telegram.qa.setup.bot_token".into(), "Bot token".into());
    i18n.insert(
        "telegram.qa.setup.public_base_url".into(),
        "Public URL".into(),
    );

    let form_spec = qa_bridge::provider_qa_to_form_spec(&qa_output, &i18n, "messaging-telegram");

    assert_eq!(form_spec.id, "messaging-telegram-setup");
    assert_eq!(form_spec.questions.len(), 3);

    // Verify types inferred correctly
    assert_eq!(form_spec.questions[0].kind, QuestionType::Boolean); // enabled
    assert!(form_spec.questions[1].secret); // bot_token

    // Validate answers against this spec
    let answers = json!({
        "enabled": "true",
        "bot_token": "123:ABC",
        "public_base_url": "https://example.com"
    });
    assert!(qa_setup_wizard::validate_answers_against_form_spec(&form_spec, &answers).is_ok());
}

#[test]
fn e2e_render_qa_card_from_formspec() {
    let temp = tempfile::tempdir().unwrap();
    let yaml = r#"
title: "Telegram Setup"
questions:
  - name: enabled
    kind: boolean
    required: true
    help: "Enable this provider"
    default: true
    title: "Enable provider"
  - name: public_base_url
    kind: string
    required: true
    help: "Public URL for webhook callbacks"
  - name: bot_token
    kind: string
    required: true
    help: "Telegram bot token"
    secret: true
    title: "Bot Token"
"#;
    let pack_path = create_test_pack_with_setup(temp.path(), "messaging-telegram", yaml);

    // Build FormSpec
    let form_spec = setup_to_formspec::pack_to_form_spec(&pack_path, "messaging-telegram").unwrap();

    // Step 1: Render first card (no answers yet)
    let (card, next_id) = qa_setup_wizard::render_qa_card(&form_spec, &json!({}));
    assert_eq!(card["type"], "AdaptiveCard");
    assert_eq!(card["version"], "1.3");
    assert!(card.get("$schema").is_some(), "should have $schema");
    assert!(next_id.is_some(), "should have a next question");

    let first_question = next_id.unwrap();
    assert_eq!(
        first_question, "enabled",
        "first question should be 'enabled'"
    );

    // Verify card has body with inputs and actions with id
    let body = card["body"].as_array().expect("body should be array");
    assert!(!body.is_empty(), "body should not be empty");
    let actions = card["actions"].as_array().expect("actions should be array");
    assert!(!actions.is_empty(), "actions should not be empty");
    assert_eq!(
        actions[0]["id"].as_str(),
        Some("submit"),
        "action should have id=submit"
    );

    // Step 2: Answer first question, render next card
    let mut answers = json!({});
    answers["enabled"] = json!(true);
    let (card2, next_id2) = qa_setup_wizard::render_qa_card(&form_spec, &answers);
    assert_eq!(card2["type"], "AdaptiveCard");
    assert!(next_id2.is_some(), "should have second question");
    let second_question = next_id2.unwrap();
    assert_eq!(second_question, "public_base_url");

    // Step 3: Answer second question
    answers["public_base_url"] = json!("https://example.com/webhook");
    let (card3, next_id3) = qa_setup_wizard::render_qa_card(&form_spec, &answers);
    assert_eq!(card3["type"], "AdaptiveCard");
    assert!(next_id3.is_some(), "should have third question");
    assert_eq!(next_id3.unwrap(), "bot_token");

    // Step 4: Answer all questions — next_id should be None
    answers["bot_token"] = json!("123:ABC");
    let (card4, next_id4) = qa_setup_wizard::render_qa_card(&form_spec, &answers);
    assert_eq!(card4["type"], "AdaptiveCard");
    assert!(
        next_id4.is_none(),
        "all questions answered, next_id should be None"
    );

    // The completed card should have no actions (no more questions)
    let final_actions = card4["actions"].as_array().expect("actions array");
    assert!(final_actions.is_empty(), "no actions when complete");
}
