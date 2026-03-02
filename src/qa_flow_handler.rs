//! Operator-side handler for `qa.process` flow nodes.
//!
//! When the operator encounters a flow node whose component type is a QA
//! processor, this module intercepts the invocation and runs the QA
//! collect→validate→apply flow inline, returning the result as the node's output.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::component_qa_ops::{self, QaMode};
use crate::demo::runner_host::OperatorContext;
use crate::domains::{Domain, ProviderPack};
use crate::qa_setup_wizard;
use crate::setup_to_formspec;

/// Check whether a node component identifier represents a QA processor.
pub fn is_qa_process_node(component_id: &str) -> bool {
    component_id == "component-qa"
        || component_id == "ai.greentic.component-qa"
        || component_id.ends_with("/component-qa")
        || component_id.contains("qa.process")
}

/// Handle a QA process flow node.
///
/// This runs the full QA wizard inline:
/// 1. Determine provider from node config
/// 2. Load FormSpec from the provider pack
/// 3. Collect answers (from node config or interactively)
/// 4. Call apply-answers on the provider WASM component
/// 5. Return the config output as node result
pub fn handle_qa_process_node(
    root: &Path,
    node_config: &Value,
    provider_pack: &ProviderPack,
    provider_id: &str,
    domain: Domain,
    ctx: &OperatorContext,
    interactive: bool,
) -> Result<Value> {
    // Extract mode from node config (default: "setup")
    let mode_str = node_config
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("setup");
    let mode = match mode_str {
        "remove" => QaMode::Remove,
        "upgrade" => QaMode::Upgrade,
        "default" => QaMode::Default,
        _ => QaMode::Setup,
    };

    // Extract pre-supplied answers from node config
    let supplied_answers = node_config.get("answers").cloned();

    // Build FormSpec from the provider pack
    let form_spec = setup_to_formspec::pack_to_form_spec(&provider_pack.path, provider_id);

    // Collect answers
    let answers = if let Some(answers) = supplied_answers {
        // Validate pre-supplied answers against FormSpec
        if let Some(ref spec) = form_spec {
            qa_setup_wizard::validate_answers_against_form_spec(spec, &answers)?;
        }
        answers
    } else if interactive {
        // Run interactive wizard
        let (answers, _spec) = qa_setup_wizard::run_qa_setup(
            &provider_pack.path,
            provider_id,
            None,
            true,
            form_spec.clone(),
        )?;
        answers
    } else {
        json!({})
    };

    // Read current config
    let providers_root = root
        .join("state")
        .join("runtime")
        .join(&ctx.tenant)
        .join("providers");
    let current_config = crate::provider_config_envelope::read_provider_config_envelope(
        &providers_root,
        provider_id,
    )?
    .map(|envelope| envelope.config);

    // Call apply-answers via component QA
    match component_qa_ops::apply_answers_via_component_qa(
        root,
        domain,
        &ctx.tenant,
        ctx.team.as_deref(),
        provider_pack,
        provider_id,
        mode,
        current_config.as_ref(),
        &answers,
    ) {
        Ok(Some(config)) => Ok(json!({
            "status": "ok",
            "config": config,
            "mode": mode_str,
            "provider": provider_id,
        })),
        Ok(None) => Ok(json!({
            "status": "skip",
            "reason": "provider does not support QA contract",
            "provider": provider_id,
        })),
        Err(diag) => Ok(json!({
            "status": "error",
            "code": diag.code.as_str(),
            "message": diag.message,
            "provider": provider_id,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_qa_process_node() {
        assert!(is_qa_process_node("component-qa"));
        assert!(is_qa_process_node("ai.greentic.component-qa"));
        assert!(is_qa_process_node("root:component/component-qa"));
        assert!(is_qa_process_node("qa.process.setup"));
        assert!(!is_qa_process_node("component-llm-openai"));
        assert!(!is_qa_process_node("messaging-telegram"));
    }
}
