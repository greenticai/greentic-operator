use std::{
    collections::HashMap,
    error::Error,
    fmt,
    io::{self, BufRead},
};

use anyhow::{Context, Result};
use serde_json::Value as JsonValue;

use crate::demo::{
    card::{CardView, detect_adaptive_card_view, print_card_summary},
    commands::{CommandParseError, DemoCommand, parse_command},
    help::print_help,
    history::{DemoHistory, Snapshot},
    runner::DemoRunner,
    types::{DemoBlockedOn, UserEvent},
};
use crate::operator_i18n;

#[derive(Debug)]
struct DemoReplQuit;

impl fmt::Display for DemoReplQuit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "user requested exit")
    }
}

impl Error for DemoReplQuit {}

pub struct DemoRepl {
    runner: DemoRunner,
    pending_inputs: HashMap<String, String>,
    last_output: Option<JsonValue>,
    current_card: Option<CardView>,
    history: DemoHistory,
}

impl DemoRepl {
    pub fn new(runner: DemoRunner) -> Self {
        Self {
            runner,
            pending_inputs: HashMap::new(),
            last_output: None,
            current_card: None,
            history: DemoHistory::new(),
        }
    }

    pub fn run(&mut self) -> Result<()> {
        loop {
            let blocked = self.runner.run_until_blocked();
            match blocked {
                DemoBlockedOn::Waiting { output, reason, .. } => {
                    self.current_card = detect_adaptive_card_view(&output);
                    if let Some(card) = &self.current_card {
                        let snapshot = Snapshot::new(
                            output.clone(),
                            Some(card.clone()),
                            self.pending_inputs.clone(),
                        );
                        self.history.push(snapshot);
                        self.last_output = Some(output.clone());
                        print_card_summary(card);
                        match self.command_loop() {
                            Ok(_) => {}
                            Err(err) => {
                                if err.downcast_ref::<DemoReplQuit>().is_some() {
                                    return Ok(());
                                }
                                return Err(err);
                            }
                        }
                    } else {
                        let snapshot =
                            Snapshot::new(output.clone(), None, self.pending_inputs.clone());
                        self.history.push(snapshot);
                        self.last_output = Some(output.clone());
                        if let Some(reason) = reason {
                            println!(
                                "{}",
                                operator_i18n::trf(
                                    "demo.repl.waiting_for_input",
                                    "Waiting for input: {}",
                                    &[&reason]
                                )
                            );
                        } else {
                            println!(
                                "{}",
                                operator_i18n::tr(
                                    "demo.repl.waiting_no_card",
                                    "Flow is waiting for input (no adaptive card detected)."
                                )
                            );
                        }
                        match self.command_loop() {
                            Ok(_) => {}
                            Err(err) => {
                                if err.downcast_ref::<DemoReplQuit>().is_some() {
                                    return Ok(());
                                }
                                return Err(err);
                            }
                        }
                    }
                }
                DemoBlockedOn::Finished(output) => {
                    let output = humanize_output(&output);
                    println!(
                        "{}",
                        operator_i18n::tr(
                            "demo.repl.finished_with_output",
                            "Flow finished with output:"
                        )
                    );
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&output)
                            .unwrap_or_else(|_| "<invalid json>".into())
                    );
                    return Ok(());
                }
                DemoBlockedOn::Error(err) => return Err(err),
            }
        }
    }

    fn command_loop(&mut self) -> Result<()> {
        let stdin = io::stdin();
        loop {
            let mut line = String::new();
            stdin
                .lock()
                .read_line(&mut line)
                .context("read command from stdin")?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match parse_command(trimmed) {
                Ok(DemoCommand::Show) => {
                    self.display_card_summary();
                }
                Ok(DemoCommand::Json) => {
                    self.print_json();
                }
                Ok(DemoCommand::Back) => {
                    if let Some(snapshot) = self.history.go_back() {
                        self.pending_inputs = snapshot.pending_inputs.clone();
                        self.last_output = Some(snapshot.output.clone());
                        self.current_card = snapshot.card.clone();
                        println!(
                            "{}",
                            operator_i18n::tr(
                                "demo.repl.restored_previous_state",
                                "Restored previous blocked state."
                            )
                        );
                        self.display_card_summary();
                    } else {
                        println!(
                            "{}",
                            operator_i18n::tr(
                                "demo.repl.already_earliest_state",
                                "Already at the earliest blocked state."
                            )
                        );
                    }
                }
                Ok(DemoCommand::Setup { provider }) => {
                    if let Err(err) = self.handle_setup(provider.as_deref()) {
                        println!("Setup error: {err}");
                    }
                }
                Ok(DemoCommand::Help) => {
                    print_help();
                }
                Ok(DemoCommand::Quit) => {
                    return Err(DemoReplQuit.into());
                }
                Ok(DemoCommand::Input { field, value }) => {
                    if let Some(card) = &self.current_card
                        && !card.inputs.iter().any(|input| input.id == field)
                    {
                        println!(
                            "{}",
                            operator_i18n::trf(
                                "demo.repl.unknown_input",
                                "Unknown input '{}'. Available inputs: {}",
                                &[&field, &self.list_input_ids(card)]
                            )
                        );
                        continue;
                    }
                    self.pending_inputs.insert(field.clone(), value.clone());
                    println!(
                        "{}",
                        operator_i18n::trf("demo.repl.set_input", "Set {}={}", &[&field, &value])
                    );
                }
                Ok(DemoCommand::Click { action_id }) => {
                    if let Some(card) = &self.current_card
                        && !card.actions.iter().any(|action| action.id == action_id)
                    {
                        println!(
                            "{}",
                            operator_i18n::trf(
                                "demo.repl.unknown_action",
                                "Unknown action '{}'. Available actions: {}",
                                &[&action_id, &self.list_action_ids(card)]
                            )
                        );
                        continue;
                    }
                    let fields = self
                        .pending_inputs
                        .iter()
                        .map(|(k, v)| (k.clone(), JsonValue::String(v.clone())))
                        .collect::<serde_json::Map<_, _>>();
                    self.pending_inputs.clear();
                    self.runner
                        .submit_user_event(UserEvent::card_submit(action_id, fields));
                    break;
                }
                Err(CommandParseError::Unknown(_)) => {
                    println!(
                        "{}",
                        operator_i18n::tr(
                            "demo.repl.unknown_command",
                            "Unknown command. See @help."
                        )
                    );
                    print_help();
                }
                Err(err) => {
                    println!("{err}");
                    print_help();
                }
            }
        }
        Ok(())
    }

    fn display_card_summary(&self) {
        if let Some(card) = &self.current_card {
            print_card_summary(card);
            return;
        }
        println!(
            "{}",
            operator_i18n::tr("demo.repl.no_card", "No adaptive card to show.")
        );
    }

    fn print_json(&self) {
        if let Some(last_output) = &self.last_output {
            if let Ok(pretty) = serde_json::to_string_pretty(last_output) {
                println!("{pretty}");
            } else {
                println!("{}", last_output);
            }
        } else {
            println!(
                "{}",
                operator_i18n::tr("demo.repl.no_output", "No output available.")
            );
        }
    }

    fn list_input_ids(&self, card: &CardView) -> String {
        card.inputs
            .iter()
            .map(|input| input.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn list_action_ids(&self, card: &CardView) -> String {
        card.actions
            .iter()
            .map(|action| action.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Handle the `@setup [provider]` command.
    ///
    /// Renders Adaptive Cards step-by-step using `qa_spec::render_card()` and
    /// collects answers via the REPL `@input`/`@click` commands.  Once all
    /// visible questions are answered the result is validated and printed.
    fn handle_setup(&mut self, provider: Option<&str>) -> Result<()> {
        let pack_path = self.runner.pack_path().to_owned();
        let pack_id = provider.unwrap_or_else(|| self.runner.pack_id()).to_owned();

        // Build FormSpec from pack's setup.yaml (or qa-spec WASM output).
        let form_spec = crate::setup_to_formspec::pack_to_form_spec(&pack_path, &pack_id)
            .ok_or_else(|| anyhow::anyhow!("no setup.yaml found in pack for {pack_id}"))?;

        let mut answers = serde_json::json!({});

        loop {
            // Render the card for the current answer state.
            let (card_json, next_question_id) =
                crate::qa_setup_wizard::render_qa_card(&form_spec, &answers);

            if next_question_id.is_none() {
                // All visible questions answered — done.
                break;
            }

            let question_id = next_question_id.unwrap();

            // Display card via existing REPL infrastructure.
            let card_view = detect_adaptive_card_view(&card_json);
            if let Some(ref card) = card_view {
                self.current_card = card_view.clone();
                self.pending_inputs.clear();
                print_card_summary(card);
            }

            // Mini command-loop: collect @input / @click until the user submits.
            self.collect_setup_input()?;

            // Merge the collected input for this question into answers.
            if let Some(value) = self.pending_inputs.get(&question_id) {
                answers[&question_id] = JsonValue::String(value.clone());
            } else {
                // The user may have set the value under a different field id
                // (e.g. the AC input id matches the question id).  Merge all
                // pending inputs into answers so nothing is lost.
                for (key, value) in &self.pending_inputs {
                    answers[key] = JsonValue::String(value.clone());
                }
            }
        }

        // Validate the collected answers.
        crate::qa_setup_wizard::validate_answers_against_form_spec(&form_spec, &answers)?;

        println!("\nSetup complete for {pack_id}:");
        println!(
            "{}",
            serde_json::to_string_pretty(&answers).unwrap_or_else(|_| "<invalid>".into())
        );
        println!(
            "{}",
            operator_i18n::tr(
                "demo.repl.setup_validated",
                "Answers validated against FormSpec."
            )
        );

        Ok(())
    }

    /// Block until the user provides `@input` values and submits via `@click`.
    ///
    /// Handles the same subset of REPL commands as the main `command_loop` but
    /// returns on `@click` instead of forwarding a `UserEvent` to the runner.
    fn collect_setup_input(&mut self) -> Result<()> {
        let stdin = io::stdin();
        loop {
            let mut line = String::new();
            stdin
                .lock()
                .read_line(&mut line)
                .context("read command from stdin")?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match parse_command(trimmed) {
                Ok(DemoCommand::Show) => {
                    self.display_card_summary();
                }
                Ok(DemoCommand::Json) => {
                    // Show the raw card JSON for the current setup step.
                    self.print_json();
                }
                Ok(DemoCommand::Input { field, value }) => {
                    if let Some(card) = &self.current_card
                        && !card.inputs.iter().any(|input| input.id == field)
                    {
                        println!(
                            "{}",
                            operator_i18n::trf(
                                "demo.repl.unknown_input",
                                "Unknown input '{}'. Available inputs: {}",
                                &[&field, &self.list_input_ids(card)]
                            )
                        );
                        continue;
                    }
                    self.pending_inputs.insert(field.clone(), value.clone());
                    println!(
                        "{}",
                        operator_i18n::trf("demo.repl.set_input", "Set {}={}", &[&field, &value])
                    );
                }
                Ok(DemoCommand::Click { .. }) => {
                    // Any click advances the setup wizard.
                    break;
                }
                Ok(DemoCommand::Quit) => {
                    return Err(DemoReplQuit.into());
                }
                Ok(DemoCommand::Help) => {
                    print_help();
                }
                Ok(_) => {
                    println!(
                        "{}",
                        operator_i18n::tr(
                            "demo.repl.setup_only_input_click",
                            "During setup use @input <field>=<value> and @click submit."
                        )
                    );
                }
                Err(CommandParseError::Unknown(_)) => {
                    println!(
                        "{}",
                        operator_i18n::tr(
                            "demo.repl.unknown_command",
                            "Unknown command. See @help."
                        )
                    );
                }
                Err(err) => {
                    println!("{err}");
                }
            }
        }
        Ok(())
    }
}

fn humanize_output(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(map) => {
            let mut updated = serde_json::Map::new();
            for (key, value) in map {
                if (key == "metadata" || key == "payload")
                    && let Some(bytes) = json_array_to_bytes(value)
                    && let Ok(text) = String::from_utf8(bytes)
                {
                    if let Ok(parsed) = serde_json::from_str::<JsonValue>(&text) {
                        updated.insert(key.clone(), parsed);
                        continue;
                    }
                    updated.insert(key.clone(), JsonValue::String(text));
                    continue;
                }
                updated.insert(key.clone(), humanize_output(value));
            }
            JsonValue::Object(updated)
        }
        JsonValue::Array(items) => JsonValue::Array(items.iter().map(humanize_output).collect()),
        other => other.clone(),
    }
}

fn json_array_to_bytes(value: &JsonValue) -> Option<Vec<u8>> {
    let JsonValue::Array(items) = value else {
        return None;
    };
    let mut bytes = Vec::with_capacity(items.len());
    for item in items {
        let value = item.as_u64()?;
        if value > u8::MAX as u64 {
            return None;
        }
        bytes.push(value as u8);
    }
    Some(bytes)
}
