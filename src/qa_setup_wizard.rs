//! QA-aware setup wizard that unifies the WASM-based `qa-spec` op and legacy
//! `setup.yaml` into a single FormSpec-driven flow.
//!
//! This replaces the direct `collect_setup_answers()` call in the CLI with a
//! FormSpec-aware path that can validate answers, infer types, and provide
//! richer prompts.

use std::collections::HashMap;
use std::io::{self, BufRead, Write as _};
use std::path::Path;

use anyhow::{Result, anyhow};
use qa_spec::spec::form::ProgressPolicy;
use qa_spec::{FormSpec, QuestionSpec, QuestionType, build_render_payload, render_card};
use rpassword::prompt_password;
use serde_json::{Map as JsonMap, Value};

use crate::demo::card::{CardView, detect_adaptive_card_view, print_card_summary};
use crate::demo::commands::{DemoCommand, parse_command};
use crate::demo::qa_bridge;
use crate::setup_input::SetupInputAnswers;
use crate::setup_to_formspec;

/// Run the QA setup wizard for a provider pack.
///
/// Builds a `FormSpec` from `setup.yaml` (or a pre-built one may be passed in
/// from a component `qa-spec` invocation), then collects and validates answers.
///
/// Returns `(answers, form_spec)` where `form_spec` is `Some` if one was found.
pub fn run_qa_setup(
    pack_path: &Path,
    provider_id: &str,
    setup_input: Option<&SetupInputAnswers>,
    interactive: bool,
    qa_form_spec: Option<FormSpec>,
) -> Result<(Value, Option<FormSpec>)> {
    // Use provided FormSpec or fall back to setup.yaml → FormSpec
    let form_spec =
        qa_form_spec.or_else(|| setup_to_formspec::pack_to_form_spec(pack_path, provider_id));

    // Collect answers based on available info
    let answers = if let Some(input) = setup_input {
        if let Some(value) = input.answers_for_provider(provider_id) {
            let answers = ensure_object(value.clone())?;
            if let Some(ref spec) = form_spec {
                validate_answers_against_form_spec(spec, &answers)?;
            }
            answers
        } else if has_required_questions(form_spec.as_ref()) {
            return Err(anyhow!("setup input missing answers for {provider_id}"));
        } else {
            Value::Object(JsonMap::new())
        }
    } else if let Some(ref spec) = form_spec {
        if spec.questions.is_empty() {
            Value::Object(JsonMap::new())
        } else if interactive {
            prompt_form_spec_answers(spec, provider_id)?
        } else {
            return Err(anyhow!(
                "setup answers required for {provider_id} but run is non-interactive"
            ));
        }
    } else {
        Value::Object(JsonMap::new())
    };

    Ok((answers, form_spec))
}

/// Render a QA setup step as an Adaptive Card v1.3.
///
/// Calls `qa_spec::build_render_payload()` and `qa_spec::render_card()` to produce
/// a card with Input.* elements and an Action.Submit button.
///
/// Returns `(card_json, next_question_id)` where `next_question_id` is `None` when
/// all visible questions have been answered.
pub fn render_qa_card(form_spec: &FormSpec, answers: &Value) -> (Value, Option<String>) {
    // Ensure skip_answered is enabled so next_question advances past filled answers.
    let mut spec = form_spec.clone();
    spec.progress_policy = Some(
        spec.progress_policy
            .map(|mut p| {
                p.skip_answered = true;
                p
            })
            .unwrap_or(ProgressPolicy {
                skip_answered: true,
                ..ProgressPolicy::default()
            }),
    );

    let ctx = serde_json::json!({});
    let payload = build_render_payload(&spec, &ctx, answers);
    let next_id = payload.next_question_id.clone();
    let mut card = render_card(&payload);

    // Ensure the Action.Submit has an `id` field so the REPL's @click can target it.
    if let Some(actions) = card.get_mut("actions").and_then(Value::as_array_mut) {
        for action in actions.iter_mut() {
            if action.get("id").is_none() {
                action["id"] = Value::String("submit".into());
            }
        }
    }

    (card, next_id)
}

/// Validate answers against a FormSpec, checking required fields and constraints.
pub fn validate_answers_against_form_spec(spec: &FormSpec, answers: &Value) -> Result<()> {
    let map = answers
        .as_object()
        .ok_or_else(|| anyhow!("setup answers must be an object"))?;

    for question in &spec.questions {
        if question.required {
            match map.get(&question.id) {
                Some(value) if !value.is_null() => {}
                _ => {
                    return Err(anyhow!(
                        "missing required setup answer for '{}'{}",
                        question.id,
                        question
                            .description
                            .as_ref()
                            .map(|d| format!(" ({d})"))
                            .unwrap_or_default()
                    ));
                }
            }
        }

        // Validate constraint if present
        if let Some(value) = map.get(&question.id)
            && let Some(s) = value.as_str()
            && let Some(ref constraint) = question.constraint
            && let Some(ref pattern) = constraint.pattern
            && !matches_pattern(s, pattern)
        {
            return Err(anyhow!(
                "answer for '{}' does not match pattern: {}",
                question.id,
                pattern
            ));
        }
    }

    Ok(())
}

/// Interactively prompt the user using FormSpec questions.
pub fn prompt_form_spec_answers(spec: &FormSpec, provider_id: &str) -> Result<Value> {
    let display = provider_id
        .strip_prefix("messaging-")
        .or_else(|| provider_id.strip_prefix("events-"))
        .unwrap_or(provider_id);
    println!("\nConfiguring {display}: {}", spec.title);
    if let Some(ref pres) = spec.presentation
        && let Some(ref intro) = pres.intro
    {
        println!("{intro}");
    }

    let mut answers = JsonMap::new();
    for question in &spec.questions {
        if question.id.is_empty() {
            continue;
        }
        if let Some(value) = ask_form_spec_question(question)? {
            answers.insert(question.id.clone(), value);
        }
    }
    Ok(Value::Object(answers))
}

/// Ask a single FormSpec question interactively.
fn ask_form_spec_question(question: &QuestionSpec) -> Result<Option<Value>> {
    if let Some(ref desc) = question.description
        && !desc.is_empty()
    {
        println!("  {desc}");
    }
    if let Some(ref choices) = question.choices {
        println!("  Choices:");
        for (idx, choice) in choices.iter().enumerate() {
            println!("    {}) {choice}", idx + 1);
        }
    }

    loop {
        let prompt = build_form_spec_prompt(question);
        let input = read_input(&prompt, question.secret)?;
        let trimmed = input.trim();

        if trimmed.is_empty() {
            if let Some(ref default) = question.default_value {
                return Ok(Some(parse_typed_value(question.kind, default)));
            }
            if question.required {
                println!("  This field is required.");
                continue;
            }
            return Ok(None);
        }

        // Normalize boolean answers
        let normalized = qa_bridge::normalize_answer(trimmed, question.kind);

        // Validate constraint
        if let Some(ref constraint) = question.constraint
            && let Some(ref pattern) = constraint.pattern
            && !matches_pattern(&normalized, pattern)
        {
            println!("  Invalid format. Expected pattern: {pattern}");
            continue;
        }

        // Validate choice
        if let Some(ref choices) = question.choices
            && !choices.is_empty()
        {
            if let Ok(idx) = normalized.parse::<usize>()
                && let Some(choice) = choices.get(idx - 1)
            {
                return Ok(Some(Value::String(choice.clone())));
            }
            if !choices.contains(&normalized) {
                println!("  Invalid choice. Options: {}", choices.join(", "));
                continue;
            }
        }

        return Ok(Some(parse_typed_value(question.kind, &normalized)));
    }
}

fn build_form_spec_prompt(question: &QuestionSpec) -> String {
    let marker = if question.required { "*" } else { "" };
    let mut prompt = format!("{}{marker}", question.title);
    match question.kind {
        QuestionType::Boolean => prompt.push_str(" [boolean]"),
        QuestionType::Number | QuestionType::Integer => prompt.push_str(" [number]"),
        QuestionType::Enum => prompt.push_str(" [choice]"),
        _ => {}
    }
    if let Some(ref default) = question.default_value {
        prompt = format!("{prompt} [default: {default}]");
    }
    prompt.push_str(": ");
    prompt
}

fn read_input(prompt: &str, secret: bool) -> Result<String> {
    if secret {
        prompt_password(prompt).map_err(|err| anyhow!("read secret: {err}"))
    } else {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut buffer = String::new();
        io::stdin().read_line(&mut buffer)?;
        Ok(buffer)
    }
}

/// Simple pattern matching for common constraint patterns.
/// Supports the URL pattern `^https?://\S+` used by setup specs.
fn matches_pattern(value: &str, pattern: &str) -> bool {
    if pattern == r"^https?://\S+" {
        (value.starts_with("http://") || value.starts_with("https://"))
            && value.len() > 8
            && !value.contains(char::is_whitespace)
    } else {
        // Unknown pattern — accept (validation is best-effort)
        true
    }
}

fn parse_typed_value(kind: QuestionType, input: &str) -> Value {
    match kind {
        QuestionType::Boolean => match input.to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "1" | "on" => Value::Bool(true),
            "false" | "no" | "n" | "0" | "off" => Value::Bool(false),
            _ => Value::String(input.to_string()),
        },
        QuestionType::Number | QuestionType::Integer => {
            if let Ok(n) = input.parse::<i64>() {
                Value::Number(n.into())
            } else if let Ok(n) = input.parse::<f64>() {
                serde_json::Number::from_f64(n)
                    .map(Value::Number)
                    .unwrap_or_else(|| Value::String(input.to_string()))
            } else {
                Value::String(input.to_string())
            }
        }
        _ => Value::String(input.to_string()),
    }
}

fn ensure_object(value: Value) -> Result<Value> {
    match value {
        Value::Object(_) => Ok(value),
        other => Err(anyhow!(
            "setup answers must be a JSON object, got {}",
            other
        )),
    }
}

fn has_required_questions(spec: Option<&FormSpec>) -> bool {
    spec.map(|s| s.questions.iter().any(|q| q.required))
        .unwrap_or(false)
}

/// Run a standalone interactive card-based setup wizard for a provider pack.
///
/// Loads the pack's `setup.yaml` → `FormSpec`, then loops through Adaptive Card
/// steps — rendering each card, displaying it via `print_card_summary()`, and
/// collecting `@input`/`@click` commands from stdin — until all questions are
/// answered. Validates and prints the final answers as JSON.
pub fn run_interactive_card_wizard(pack_path: &Path, provider_id: &str) -> Result<Value> {
    let form_spec = setup_to_formspec::pack_to_form_spec(pack_path, provider_id)
        .ok_or_else(|| anyhow!("no setup.yaml found in pack for {provider_id}"))?;

    println!("Setup wizard for {provider_id}");
    if let Some(ref pres) = form_spec.presentation
        && let Some(ref intro) = pres.intro
    {
        println!("{intro}");
    }

    let mut answers = serde_json::json!({});
    let mut current_card: Option<CardView> = None;
    let mut pending_inputs: HashMap<String, String> = HashMap::new();
    let mut current_card_json: Option<Value> = None;

    loop {
        let (card_json, next_question_id) = render_qa_card(&form_spec, &answers);

        if next_question_id.is_none() {
            break;
        }

        let question_id = next_question_id.unwrap();

        // Display the rendered card.
        let card_view = detect_adaptive_card_view(&card_json);
        if let Some(ref card) = card_view {
            current_card = card_view.clone();
            current_card_json = Some(card_json.clone());
            pending_inputs.clear();
            print_card_summary(card);
        }

        // Mini command-loop: collect @input / @click until the user submits.
        let eof = collect_card_inputs(
            &current_card,
            &current_card_json,
            &mut pending_inputs,
        )?;
        if eof {
            return Err(anyhow!("wizard cancelled (EOF)"));
        }

        // Merge collected inputs into answers.
        if let Some(value) = pending_inputs.get(&question_id) {
            answers[&question_id] = Value::String(value.clone());
        } else {
            for (key, value) in &pending_inputs {
                answers[key] = Value::String(value.clone());
            }
        }
    }

    // Validate the collected answers.
    validate_answers_against_form_spec(&form_spec, &answers)?;

    println!("\nSetup answers collected for {provider_id}:");
    println!(
        "{}",
        serde_json::to_string_pretty(&answers).unwrap_or_else(|_| "<invalid>".into())
    );

    Ok(answers)
}

/// Collect `@input`/`@click` commands for a single card step.
///
/// Returns `true` if EOF was reached (stdin closed).
fn collect_card_inputs(
    current_card: &Option<CardView>,
    current_card_json: &Option<Value>,
    pending_inputs: &mut HashMap<String, String>,
) -> Result<bool> {
    let stdin = io::stdin();
    loop {
        print!("wizard> ");
        io::stdout().flush()?;
        let mut line = String::new();
        let bytes = stdin.lock().read_line(&mut line)?;
        if bytes == 0 {
            // EOF — stdin pipe closed.
            return Ok(true);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match parse_command(trimmed) {
            Ok(DemoCommand::Show) => {
                if let Some(card) = current_card {
                    print_card_summary(card);
                }
            }
            Ok(DemoCommand::Json) => {
                if let Some(json) = current_card_json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(json)
                            .unwrap_or_else(|_| "<invalid>".into())
                    );
                }
            }
            Ok(DemoCommand::Input { field, value }) => {
                if let Some(card) = current_card
                    && !card.inputs.iter().any(|input| input.id == field)
                {
                    let ids: Vec<&str> =
                        card.inputs.iter().map(|i| i.id.as_str()).collect();
                    println!("Unknown input '{field}'. Available: {}", ids.join(", "));
                    continue;
                }
                pending_inputs.insert(field.clone(), value.clone());
                println!("Set {field}={value}");
            }
            Ok(DemoCommand::Click { .. }) => {
                return Ok(false);
            }
            Ok(DemoCommand::Quit) => {
                println!("Wizard cancelled.");
                std::process::exit(0);
            }
            Ok(DemoCommand::Help) => {
                println!("@input <field>=<value>  Set an input value");
                println!("@click <action_id>     Submit the current card step");
                println!("@show                  Re-display the current card");
                println!("@json                  Show raw card JSON");
                println!("@quit                  Cancel the wizard");
            }
            Ok(_) => {
                println!("Use @input, @click, @show, @json, @help, or @quit.");
            }
            Err(e) => {
                println!("{e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_form_spec() -> FormSpec {
        FormSpec {
            id: "test-setup".into(),
            title: "Test Setup".into(),
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
                    id: "api_url".into(),
                    kind: QuestionType::String,
                    title: "API URL".into(),
                    title_i18n: None,
                    description: None,
                    description_i18n: None,
                    required: true,
                    choices: None,
                    default_value: None,
                    secret: false,
                    visible_if: None,
                    constraint: Some(qa_spec::spec::Constraint {
                        pattern: Some(r"^https?://\S+".into()),
                        min: None,
                        max: None,
                        min_len: None,
                        max_len: None,
                    }),
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
                QuestionSpec {
                    id: "optional".into(),
                    kind: QuestionType::String,
                    title: "Optional Field".into(),
                    title_i18n: None,
                    description: None,
                    description_i18n: None,
                    required: false,
                    choices: None,
                    default_value: Some("default_val".into()),
                    secret: false,
                    visible_if: None,
                    constraint: None,
                    list: None,
                    computed: None,
                    policy: Default::default(),
                    computed_overridable: false,
                },
            ],
        }
    }

    #[test]
    fn validates_required_answers() {
        let spec = test_form_spec();
        let answers = json!({"api_url": "https://example.com", "token": "abc"});
        assert!(validate_answers_against_form_spec(&spec, &answers).is_ok());
    }

    #[test]
    fn rejects_missing_required() {
        let spec = test_form_spec();
        let answers = json!({"api_url": "https://example.com"});
        let err = validate_answers_against_form_spec(&spec, &answers).unwrap_err();
        assert!(err.to_string().contains("token"));
    }

    #[test]
    fn rejects_invalid_url_pattern() {
        let spec = test_form_spec();
        let answers = json!({"api_url": "not-a-url", "token": "abc"});
        let err = validate_answers_against_form_spec(&spec, &answers).unwrap_err();
        assert!(err.to_string().contains("pattern"));
    }

    #[test]
    fn render_card_has_inputs() {
        let spec = test_form_spec();
        let answers = json!({});
        let (card, next_q) = render_qa_card(&spec, &answers);
        assert_eq!(next_q.as_deref(), Some("api_url"));
        let view = crate::demo::card::detect_adaptive_card_view(&card)
            .expect("card should be detected");
        assert!(!view.inputs.is_empty(), "card should have inputs");
        assert_eq!(view.inputs[0].id, "api_url");
    }

    #[test]
    fn render_card_advances_on_answer() {
        let spec = test_form_spec();
        let answers = json!({"api_url": "https://example.com"});
        let (card, next_q) = render_qa_card(&spec, &answers);
        assert_eq!(next_q.as_deref(), Some("token"));
        let view = crate::demo::card::detect_adaptive_card_view(&card)
            .expect("card should be detected");
        assert!(view.inputs.iter().any(|i| i.id == "token"));
    }

    #[test]
    fn render_card_completes_when_all_answered() {
        let spec = test_form_spec();
        let answers = json!({
            "api_url": "https://example.com",
            "token": "abc",
            "optional": "val"
        });
        let (_card, next_q) = render_qa_card(&spec, &answers);
        assert!(next_q.is_none(), "all questions answered — should be done");
    }

    #[test]
    fn parse_typed_values() {
        assert_eq!(
            parse_typed_value(QuestionType::Boolean, "true"),
            Value::Bool(true)
        );
        assert_eq!(
            parse_typed_value(QuestionType::Boolean, "no"),
            Value::Bool(false)
        );
        assert_eq!(parse_typed_value(QuestionType::Number, "42"), json!(42));
        assert_eq!(
            parse_typed_value(QuestionType::String, "hello"),
            Value::String("hello".into())
        );
    }
}
