//! Outbound egress helpers for the universal pipeline.

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use greentic_types::ChannelMessageEnvelope;
use rand::{RngExt, rng};
use serde_json::{Value as JsonValue, json};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::demo::runner_host::{DemoRunnerHost, FlowOutcome, OperatorContext};
use crate::domains::Domain;
use crate::messaging_universal::app;
use crate::messaging_universal::dlq;
use crate::messaging_universal::dto::{
    EncodeInV1, ProviderPayloadV1, RenderPlanInV1, SendPayloadInV1, TenantHint,
};
use crate::messaging_universal::retry::{EgressJob, RetryPolicy};
use crate::operator_log;
use crate::runtime_state::RuntimePaths;
use crate::secrets_gate::SecretsManagerHandle;

pub fn build_render_plan_input(message: serde_json::Value) -> RenderPlanInV1 {
    RenderPlanInV1 { v: 1, message }
}

pub fn build_encode_input(message: serde_json::Value, plan: serde_json::Value) -> EncodeInV1 {
    EncodeInV1 {
        v: 1,
        message,
        plan,
    }
}

pub fn build_send_payload(
    payload: ProviderPayloadV1,
    provider_type: impl Into<String>,
    tenant: impl Into<String>,
    team: Option<String>,
) -> SendPayloadInV1 {
    SendPayloadInV1 {
        v: 1,
        provider_type: provider_type.into(),
        payload,
        tenant: crate::messaging_universal::dto::TenantHint {
            tenant: tenant.into(),
            team,
            user: None,
            correlation_id: None,
        },
        reply_scope: None,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run_end_to_end(
    envelopes: Vec<ChannelMessageEnvelope>,
    provider: &str,
    bundle: &Path,
    ctx: &OperatorContext,
    runner_binary: Option<PathBuf>,
    app_pack: Option<String>,
    send_payload_flag: bool,
    dry_run: bool,
    retries: u32,
    secrets_handle: SecretsManagerHandle,
) -> anyhow::Result<()> {
    if envelopes.is_empty() {
        return Ok(());
    }
    let team = ctx.team.as_deref();
    let app_pack_path = app::resolve_app_pack_path(bundle, &ctx.tenant, team, app_pack.as_deref())
        .context("failed to resolve app pack")?;
    let pack_info =
        app::load_app_pack_info(&app_pack_path).context("failed to load app pack manifest")?;
    let flow = app::select_app_flow(&pack_info).context("unable to select default app flow")?;
    if let Some(pack) = app_pack.as_deref() {
        operator_log::info(
            module_path!(),
            format!("[demo ingress] overriding app pack selection: {pack}"),
        );
    }
    operator_log::debug(
        module_path!(),
        format!(
            "[demo ingress] resolved app pack={} flow={}",
            app_pack_path.display(),
            flow.id
        ),
    );

    let mut processed_envelopes = Vec::new();
    for envelope in envelopes {
        let mut outputs = app::run_app_flow(
            bundle,
            ctx,
            &app_pack_path,
            &pack_info.pack_id,
            &flow.id,
            &envelope,
        )
        .context("failed to run app flow")?;
        if outputs.is_empty() {
            processed_envelopes.push(envelope);
        } else {
            processed_envelopes.append(&mut outputs);
        }
    }
    let envelopes = processed_envelopes;
    let discovery = crate::discovery::discover_with_options(
        bundle,
        crate::discovery::DiscoveryOptions { cbor_only: true },
    )?;
    let runner_host = DemoRunnerHost::new(
        bundle.to_path_buf(),
        &discovery,
        runner_binary,
        secrets_handle.clone(),
        false,
    )?;
    let policy = RetryPolicy {
        max_attempts: retries.saturating_add(1).max(1),
        ..Default::default()
    };
    let runtime_paths = RuntimePaths::new(
        bundle.join("state"),
        &ctx.tenant,
        ctx.team.clone().unwrap_or_else(|| "default".to_string()),
    );

    for envelope in envelopes {
        let mut job = EgressJob::new(provider, envelope.clone(), policy.max_attempts);
        let message_value = serde_json::to_value(&envelope)?;
        let mut rng = rng();
        loop {
            job.increment_attempt();
            let plan = match render_plan(&runner_host, ctx, provider, message_value.clone()) {
                Ok(value) => value,
                Err(err) => {
                    operator_log::error(
                        module_path!(),
                        format!("render_plan failed provider={} err={err}", provider),
                    );
                    job.record_error(err.to_string());
                    return Err(err);
                }
            };
            job.with_plan(plan.clone());

            let payload =
                match encode_payload(&runner_host, ctx, provider, message_value.clone(), plan) {
                    Ok(payload) => payload,
                    Err(err) => {
                        operator_log::warn(
                            module_path!(),
                            format!(
                                "encode failed for provider={}: {err}; using fallback payload",
                                provider
                            ),
                        );
                        ProviderPayloadV1 {
                            content_type: "application/json".to_string(),
                            body_b64: STANDARD
                                .encode(serde_json::to_vec(&payload_from_message(&message_value))?),
                            metadata_json: Some(serde_json::to_string(&message_value)?),
                            metadata: None,
                        }
                    }
                };

            if !send_payload_flag || dry_run {
                operator_log::info(
                    module_path!(),
                    format!(
                        "[demo ingress] dry-run mode (send={}) provider={} attempt={} payload={}",
                        send_payload_flag, provider, job.attempt, payload.content_type
                    ),
                );
                break;
            }

            let canonical_type =
                runner_host.canonical_provider_type(Domain::Messaging, provider);
            let send_input = SendPayloadInV1 {
                v: 1,
                provider_type: canonical_type,
                payload,
                tenant: tenant_hint(ctx),
                reply_scope: None,
            };
            let send_outcome = invoke_flow(
                &runner_host,
                ctx,
                provider,
                "send_payload",
                serde_json::to_value(&send_input)?,
            )?;

            if send_outcome.success {
                operator_log::info(
                    module_path!(),
                    format!(
                        "[demo ingress] provider send succeeded provider={} attempt={}",
                        provider, job.attempt
                    ),
                );
                break;
            }

            let node_error = NodeErrorDetails::from_outcome(&send_outcome);
            job.record_error(node_error.message.clone());
            if job.attempt >= job.max_attempts || !node_error.retryable {
                operator_log::error(
                    module_path!(),
                    format!(
                        "[demo ingress] final send failure provider={} attempt={} err={}",
                        provider, job.attempt, node_error.message
                    ),
                );
                let entry = dlq::build_dlq_entry(
                    &job.job_id.to_string(),
                    provider,
                    &ctx.tenant,
                    ctx.team.as_deref(),
                    None,
                    ctx.correlation_id.as_deref(),
                    job.attempt,
                    job.max_attempts,
                    node_error.to_json(),
                    message_summary(&envelope),
                );
                dlq::append_dlq_entry(&runtime_paths.dlq_log_path(), &entry)?;
                break;
            }
            let delay = node_error.backoff_ms.map_or_else(
                || policy.delay_with_jitter(job.attempt, rng.random_range(0..=policy.jitter_ms)),
                Duration::from_millis,
            );
            let delay_ms = delay.as_millis().min(u128::from(u64::MAX)) as u64;
            job.schedule_next(delay_ms);
            operator_log::info(
                module_path!(),
                format!(
                    "[demo ingress] retrying send provider={} attempt={} after {:?}",
                    provider, job.attempt, delay
                ),
            );
            thread::sleep(delay);
        }
    }
    Ok(())
}

pub fn render_plan(
    runner_host: &DemoRunnerHost,
    ctx: &OperatorContext,
    provider: &str,
    message: JsonValue,
) -> anyhow::Result<JsonValue> {
    let input = build_render_plan_input(message);
    let outcome = invoke_flow(
        runner_host,
        ctx,
        provider,
        "render_plan",
        serde_json::to_value(&input)?,
    )?;
    let validated = ensure_success(&outcome, provider, "render_plan")?;
    Ok(validated.output.clone().unwrap_or_else(|| json!({})))
}

pub fn encode_payload(
    runner_host: &DemoRunnerHost,
    ctx: &OperatorContext,
    provider: &str,
    message: JsonValue,
    plan: JsonValue,
) -> anyhow::Result<ProviderPayloadV1> {
    let input = build_encode_input(message, plan);
    let outcome = invoke_flow(
        runner_host,
        ctx,
        provider,
        "encode",
        serde_json::to_value(&input)?,
    )?;
    let validated = ensure_success(&outcome, provider, "encode")?;
    let value = validated.output.clone().unwrap_or_else(|| json!({}));
    // Providers wrap the payload in {"ok": true, "payload": {...}} — unwrap if present.
    let payload_value = value.get("payload").cloned().unwrap_or(value);
    serde_json::from_value(payload_value)
        .context("failed to parse ProviderPayloadV1 from encode output")
}

fn invoke_flow(
    runner_host: &DemoRunnerHost,
    ctx: &OperatorContext,
    provider: &str,
    op: &str,
    payload: JsonValue,
) -> anyhow::Result<FlowOutcome> {
    let input_bytes = serde_json::to_vec(&payload)?;
    runner_host.invoke_provider_op(Domain::Messaging, provider, op, &input_bytes, ctx)
}

fn ensure_success<'a>(
    outcome: &'a FlowOutcome,
    provider: &str,
    op: &str,
) -> anyhow::Result<&'a FlowOutcome> {
    if outcome.success {
        Ok(outcome)
    } else {
        Err(anyhow::anyhow!(
            "{provider}.{op} failed: {}",
            outcome
                .error
                .clone()
                .unwrap_or_else(|| "unknown error".to_string())
        ))
    }
}

fn payload_from_message(message: &JsonValue) -> JsonValue {
    message.clone()
}

fn tenant_hint(ctx: &OperatorContext) -> TenantHint {
    TenantHint {
        tenant: ctx.tenant.clone(),
        team: ctx.team.clone(),
        user: None,
        correlation_id: ctx.correlation_id.clone(),
    }
}

fn message_summary(envelope: &ChannelMessageEnvelope) -> JsonValue {
    json!({
        "id": envelope.id,
        "channel": envelope.channel,
        "session_id": envelope.session_id,
        "text": envelope.text,
        "attachments_count": envelope.attachments.len(),
        "correlation_id": envelope.correlation_id,
    })
}

struct NodeErrorDetails {
    code: String,
    message: String,
    retryable: bool,
    backoff_ms: Option<u64>,
    details: Option<JsonValue>,
}

impl NodeErrorDetails {
    fn from_outcome(outcome: &FlowOutcome) -> Self {
        parse_node_error(outcome)
    }

    fn to_json(&self) -> JsonValue {
        json!({
            "code": self.code,
            "message": self.message,
            "retryable": self.retryable,
            "backoff_ms": self.backoff_ms,
            "details": self.details,
        })
    }
}

fn parse_node_error(outcome: &FlowOutcome) -> NodeErrorDetails {
    let raw = outcome
        .error
        .as_ref()
        .or(outcome.raw.as_ref())
        .map(|value| value.as_str());
    if let Some(text) = raw
        && let Some(parsed) = parse_json_node_error(text)
    {
        return parsed;
    }
    NodeErrorDetails {
        code: "node-error".to_string(),
        message: raw.unwrap_or("unknown node error").to_string(),
        retryable: false,
        backoff_ms: None,
        details: None,
    }
}

fn parse_json_node_error(text: &str) -> Option<NodeErrorDetails> {
    let trimmed = text.trim();
    let candidate = if trimmed.starts_with('{') {
        trimmed
    } else if let Some(idx) = trimmed.find('{') {
        &trimmed[idx..]
    } else {
        return None;
    };
    #[derive(serde::Deserialize)]
    struct NodeErrorJson {
        code: Option<String>,
        message: Option<String>,
        retryable: Option<bool>,
        #[serde(rename = "backoff-ms")]
        backoff_ms: Option<u64>,
        details: Option<JsonValue>,
    }
    let parsed: NodeErrorJson = serde_json::from_str(candidate).ok()?;
    Some(NodeErrorDetails {
        code: parsed.code.unwrap_or_else(|| "node-error".to_string()),
        message: parsed.message.unwrap_or_else(|| text.to_string()),
        retryable: parsed.retryable.unwrap_or(false),
        backoff_ms: parsed.backoff_ms,
        details: parsed.details,
    })
}
