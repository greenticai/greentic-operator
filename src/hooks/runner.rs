use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use greentic_types::cbor::canonical;
use serde_json::{Value as JsonValue, json};

use crate::demo::ingress_types::{IngressDispatchResult, IngressHttpResponse, IngressRequestV1};
use crate::demo::runner_host::{DemoRunnerHost, OperatorContext, PhaseEventSpec};
use crate::domains::{self, Domain, ProviderPack};
use crate::ingress::control_directive::{
    ControlDirective, DispatchTarget, IngressReply, try_parse_control_directive,
};
use crate::messaging_universal::dto::{HttpInV1, HttpOutV1};
use crate::offers::registry::{HOOK_CONTRACT_CONTROL_V1, HOOK_STAGE_POST_INGRESS};
use crate::operator_log;
use crate::runner_exec::{self, RunRequest};
use crate::runtime_core::RuntimeHookDescriptor;

pub fn apply_post_ingress_hooks_http(
    bundle: &Path,
    runner_host: &DemoRunnerHost,
    request: &HttpInV1,
    response: &mut HttpOutV1,
    ctx: &OperatorContext,
) -> anyhow::Result<()> {
    let mut body = HookIngressBody {
        request: serde_json::to_value(request)?,
        response_status: response.status,
        response_headers: response.headers.clone(),
        response_body: response
            .body_b64
            .as_ref()
            .and_then(|value| STANDARD.decode(value).ok()),
        events: response.events.clone(),
    };
    apply_post_ingress_hooks_json(
        bundle,
        runner_host,
        Domain::Messaging,
        provider_from_http_in(request),
        &mut body,
        ctx,
    )?;
    response.status = body.response_status;
    response.headers = body.response_headers;
    response.body_b64 = body.response_body.map(|value| STANDARD.encode(value));
    response.events = body.events;
    Ok(())
}

pub fn apply_post_ingress_hooks_dispatch(
    bundle: &Path,
    runner_host: &DemoRunnerHost,
    domain: Domain,
    request: &IngressRequestV1,
    result: &mut IngressDispatchResult,
    ctx: &OperatorContext,
) -> anyhow::Result<()> {
    if domain == Domain::Events && !event_hooks_enabled() {
        return Ok(());
    }
    let mut body = HookIngressBody {
        request: serde_json::to_value(request)?,
        response_status: result.response.status,
        response_headers: result.response.headers.clone(),
        response_body: result.response.body.clone(),
        events: result
            .events
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?,
    };
    apply_post_ingress_hooks_json(
        bundle,
        runner_host,
        domain,
        request.provider.as_str(),
        &mut body,
        ctx,
    )?;
    result.response = IngressHttpResponse {
        status: body.response_status,
        headers: body.response_headers,
        body: body.response_body,
    };
    result.events = body
        .events
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| "hook output events were not valid ingress event envelopes")?;
    Ok(())
}

struct HookIngressBody {
    request: JsonValue,
    response_status: u16,
    response_headers: Vec<(String, String)>,
    response_body: Option<Vec<u8>>,
    events: Vec<JsonValue>,
}

fn apply_post_ingress_hooks_json(
    bundle: &Path,
    runner_host: &DemoRunnerHost,
    domain: Domain,
    provider: &str,
    body: &mut HookIngressBody,
    ctx: &OperatorContext,
) -> anyhow::Result<()> {
    runner_host
        .enforce_request_policy(
            "hook.post_ingress",
            &[
                "session",
                "state",
                "bundle_source",
                "bundle_resolver",
                "bundle_fs",
            ],
            1,
        )
        .map_err(|refusal| anyhow::anyhow!("runtime request refused: {}", refusal.message))?;
    if !hooks_enabled() {
        return Ok(());
    }
    let selected =
        runner_host.resolve_runtime_hook_chain(HOOK_STAGE_POST_INGRESS, HOOK_CONTRACT_CONTROL_V1);
    emit_runtime_hook_registry_loaded(runner_host, &selected, provider, domain, ctx);
    for offer in &selected {
        emit_hook_invoked(runner_host, offer, provider, domain, ctx);
        let payload = canonical::to_canonical_cbor(&json!({
            "stage": HOOK_STAGE_POST_INGRESS,
            "contract": HOOK_CONTRACT_CONTROL_V1,
            "provider": provider,
            "request": body.request.clone(),
            "response": {
                "status": body.response_status,
                "headers": body.response_headers.clone(),
                "body_b64": body.response_body.as_ref().map(|value| STANDARD.encode(value)),
            },
            "events": body.events.clone(),
            "tenant": ctx.tenant.clone(),
            "team": ctx.team.clone(),
            "correlation_id": ctx.correlation_id.clone(),
        }))
        .with_context(|| "encode hook post_ingress payload")?;

        let pack = offer_pack(offer.pack_path.clone(), offer.provider_pack.clone())?;
        let outcome = runner_host.invoke_provider_component_op_direct(
            domain,
            &pack,
            &offer.provider_pack,
            &offer.entrypoint,
            &payload,
            ctx,
        )?;
        if !outcome.success {
            emit_hook_invocation_failed(
                runner_host,
                offer,
                provider,
                domain,
                ctx,
                outcome.error.as_deref().unwrap_or("unknown error"),
            );
            operator_log::warn(
                module_path!(),
                format!(
                    "hook invocation failed offer_key={} err={}",
                    offer.offer_key,
                    outcome.error.unwrap_or_else(|| "unknown error".to_string())
                ),
            );
            continue;
        }
        let Some(output) = outcome.output else {
            continue;
        };
        let Some(directive) = try_parse_control_directive(&output) else {
            emit_hook_parse_error(
                runner_host,
                offer,
                provider,
                domain,
                ctx,
                "missing_or_invalid_action",
            );
            continue;
        };
        if matches!(directive, ControlDirective::Continue) {
            continue;
        }
        let action = directive_action(&directive);
        let action_target = directive_target_for_audit(&directive);
        match apply_control_directive(bundle, runner_host, domain, body, ctx, directive) {
            Ok(()) => {
                emit_hook_applied(
                    runner_host,
                    offer,
                    provider,
                    domain,
                    ctx,
                    action,
                    action_target,
                );
                break;
            }
            Err(err) => {
                emit_hook_parse_error(
                    runner_host,
                    offer,
                    provider,
                    domain,
                    ctx,
                    &format!("apply_failed:{err}"),
                );
                continue;
            }
        }
    }
    Ok(())
}

fn apply_control_directive(
    bundle: &Path,
    runner_host: &DemoRunnerHost,
    domain: Domain,
    body: &mut HookIngressBody,
    ctx: &OperatorContext,
    directive: ControlDirective,
) -> anyhow::Result<()> {
    apply_control_directive_with_dispatcher(
        bundle,
        domain,
        body,
        ctx,
        directive,
        |bundle, domain, body, ctx, target| {
            dispatch_to_target(bundle, runner_host, domain, body, ctx, target)
        },
    )
}

fn apply_control_directive_with_dispatcher<F>(
    bundle: &Path,
    domain: Domain,
    body: &mut HookIngressBody,
    ctx: &OperatorContext,
    directive: ControlDirective,
    mut dispatch_fn: F,
) -> anyhow::Result<()>
where
    F: FnMut(
        &Path,
        Domain,
        &HookIngressBody,
        &OperatorContext,
        &DispatchTarget,
    ) -> anyhow::Result<()>,
{
    match directive {
        ControlDirective::Continue => {}
        ControlDirective::Respond { reply } => {
            apply_reply(body, reply, false)?;
        }
        ControlDirective::Deny { reply } => {
            apply_reply(body, reply, true)?;
        }
        ControlDirective::Dispatch { target } => {
            dispatch_fn(bundle, domain, body, ctx, &target)?;
            body.response_status = 202;
            body.response_headers =
                vec![("content-type".to_string(), "application/json".to_string())];
            body.response_body = Some(serde_json::to_vec(&json!({
                "ok": true,
                "dispatched": true,
                "target": {
                    "tenant": target.tenant,
                    "team": target.team,
                    "pack": target.pack,
                    "flow": target.flow,
                    "node": target.node,
                }
            }))?);
            body.events.clear();
        }
    }
    Ok(())
}

fn apply_reply(
    body: &mut HookIngressBody,
    reply: IngressReply,
    deny_default: bool,
) -> anyhow::Result<()> {
    let status_default = if deny_default { 403 } else { 200 };
    body.response_status = reply.status_code.unwrap_or(status_default);
    body.response_headers.clear();
    if let Some(code) = reply.reason_code {
        body.response_headers
            .push(("x-reason-code".to_string(), code));
    }
    body.events.clear();
    if let Some(card) = reply.card_cbor {
        body.response_headers
            .push(("content-type".to_string(), "application/json".to_string()));
        let payload = if let Some(text) = reply.text {
            json!({ "text": text, "card": card })
        } else {
            json!({ "card": card })
        };
        body.response_body = Some(serde_json::to_vec(&payload)?);
        return Ok(());
    }
    if let Some(text) = reply.text {
        body.response_headers
            .push(("content-type".to_string(), "text/plain".to_string()));
        body.response_body = Some(text.into_bytes());
    } else {
        body.response_body = None;
    }
    Ok(())
}

fn dispatch_to_target(
    _bundle: &Path,
    runner_host: &DemoRunnerHost,
    domain: Domain,
    body: &HookIngressBody,
    ctx: &OperatorContext,
    target: &DispatchTarget,
) -> anyhow::Result<()> {
    ensure_dispatch_target_safe(target)?;
    let pack_path = runner_host.resolve_bundle_pack_path(&target.pack)?;
    let meta = domains::read_pack_meta(&pack_path)?;
    let flow_id = match target.flow.as_deref() {
        Some(flow) => flow.to_string(),
        None => meta
            .entry_flows
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("dispatch target pack has no entry flows"))?,
    };
    let input = json!({
        "request": body.request,
        "response": {
            "status": body.response_status,
            "headers": body.response_headers,
            "body_b64": body.response_body.as_ref().map(|value| STANDARD.encode(value)),
        },
        "events": body.events,
        "hook_dispatch": {
            "tenant": target.tenant,
            "team": target.team,
            "pack": target.pack,
            "flow": target.flow,
            "node": target.node,
        }
    });
    let request = RunRequest {
        root: runner_host.bundle_runtime_root(),
        run_dir: None,
        domain,
        pack_path: pack_path.clone(),
        pack_label: meta.pack_id,
        flow_id,
        tenant: target.tenant.clone(),
        team: target.team.clone().or_else(|| ctx.team.clone()),
        input,
        dist_offline: true,
    };
    runner_exec::run_provider_pack_flow(request)
        .with_context(|| format!("hook dispatch failed for {}", pack_path.display()))?;
    Ok(())
}

fn offer_pack(path: PathBuf, pack_id: String) -> anyhow::Result<ProviderPack> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid offer pack file name: {}", path.display()))?
        .to_string();
    Ok(ProviderPack {
        pack_id,
        file_name,
        path,
        entry_flows: Vec::new(),
    })
}

fn provider_from_http_in(request: &HttpInV1) -> &str {
    request.provider.as_str()
}

fn hooks_enabled() -> bool {
    match std::env::var("GREENTIC_OPERATOR_HOOKS_ENABLED") {
        Ok(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            !(normalized == "0"
                || normalized == "false"
                || normalized == "no"
                || normalized == "off")
        }
        Err(_) => true,
    }
}

fn event_hooks_enabled() -> bool {
    match std::env::var("GREENTIC_OPERATOR_ENABLE_EVENT_HOOKS") {
        Ok(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            normalized == "1" || normalized == "true" || normalized == "yes" || normalized == "on"
        }
        Err(_) => false,
    }
}

fn ensure_dispatch_target_safe(target: &DispatchTarget) -> anyhow::Result<()> {
    if !is_safe_segment(&target.tenant) {
        anyhow::bail!("invalid dispatch tenant '{}'", target.tenant);
    }
    if let Some(team) = target.team.as_deref()
        && !is_safe_segment(team)
    {
        anyhow::bail!("invalid dispatch team '{}'", team);
    }
    if !is_safe_segment(&target.pack) {
        anyhow::bail!("invalid dispatch pack '{}'", target.pack);
    }
    if let Some(flow) = target.flow.as_deref()
        && !is_safe_segment(flow)
    {
        anyhow::bail!("invalid dispatch flow '{}'", flow);
    }
    if let Some(node) = target.node.as_deref()
        && !is_safe_segment(node)
    {
        anyhow::bail!("invalid dispatch node '{}'", node);
    }
    Ok(())
}

fn is_safe_segment(value: &str) -> bool {
    if value.is_empty() || value == "." || value == ".." {
        return false;
    }
    !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
        && !value.contains(':')
        && !value.starts_with('.')
}

fn emit_runtime_hook_registry_loaded(
    runner_host: &DemoRunnerHost,
    hooks: &[RuntimeHookDescriptor],
    provider: &str,
    domain: Domain,
    ctx: &OperatorContext,
) {
    let payload = json!({
        "event": "runtime.hook_chain.loaded",
        "domain": domains::domain_name(domain),
        "provider": provider,
        "tenant": ctx.tenant,
        "team": ctx.team.as_deref().unwrap_or("default"),
        "correlation_id": ctx.correlation_id,
        "stage": HOOK_STAGE_POST_INGRESS,
        "contract": HOOK_CONTRACT_CONTROL_V1,
        "hook_count": hooks.len(),
        "hooks": hooks
            .iter()
            .map(|hook| json!({
                "offer_key": hook.offer_key,
                "pack_id": hook.provider_pack,
                "provider_op": hook.entrypoint,
                "priority": hook.priority,
            }))
            .collect::<Vec<_>>(),
    });
    operator_log::info(module_path!(), payload.to_string());
    runner_host.publish_phase_event(PhaseEventSpec {
        event_type: "hook.chain_loaded",
        severity: "info",
        outcome: Some("loaded"),
        ctx,
        pack_id: Some(provider),
        flow_id: Some(HOOK_STAGE_POST_INGRESS),
        payload,
    });
}

fn emit_hook_invoked(
    runner_host: &DemoRunnerHost,
    offer: &RuntimeHookDescriptor,
    provider: &str,
    domain: Domain,
    ctx: &OperatorContext,
) {
    let payload = json!({
        "event": "hook.invoked",
        "offer_key": offer.offer_key,
        "pack_id": offer.provider_pack,
        "stage": HOOK_STAGE_POST_INGRESS,
        "contract": HOOK_CONTRACT_CONTROL_V1,
        "provider": provider,
        "provider_op": offer.entrypoint,
        "domain": domains::domain_name(domain),
        "tenant": ctx.tenant,
        "team": ctx.team.as_deref().unwrap_or("default"),
        "correlation_id": ctx.correlation_id,
    });
    operator_log::info(module_path!(), payload.to_string());
    runner_host.publish_phase_event(PhaseEventSpec {
        event_type: "hook.invoked",
        severity: "info",
        outcome: Some("invoked"),
        ctx,
        pack_id: Some(&offer.provider_pack),
        flow_id: Some(&offer.entrypoint),
        payload,
    });
}

fn emit_hook_applied(
    runner_host: &DemoRunnerHost,
    offer: &RuntimeHookDescriptor,
    provider: &str,
    domain: Domain,
    ctx: &OperatorContext,
    action: &str,
    action_target: Option<JsonValue>,
) {
    let payload = json!({
        "event": "hook.directive.applied",
        "offer_key": offer.offer_key,
        "pack_id": offer.provider_pack,
        "action": action,
        "target": action_target,
        "provider": provider,
        "domain": domains::domain_name(domain),
        "tenant": ctx.tenant,
        "team": ctx.team.as_deref().unwrap_or("default"),
        "correlation_id": ctx.correlation_id,
    });
    operator_log::info(module_path!(), payload.to_string());
    runner_host.publish_phase_event(PhaseEventSpec {
        event_type: "hook.directive_applied",
        severity: "info",
        outcome: Some(action),
        ctx,
        pack_id: Some(&offer.provider_pack),
        flow_id: Some(&offer.entrypoint),
        payload,
    });
}

fn emit_hook_parse_error(
    runner_host: &DemoRunnerHost,
    offer: &RuntimeHookDescriptor,
    provider: &str,
    domain: Domain,
    ctx: &OperatorContext,
    err: &str,
) {
    let payload = json!({
        "event": "hook.directive.parse_error",
        "offer_key": offer.offer_key,
        "pack_id": offer.provider_pack,
        "provider": provider,
        "domain": domains::domain_name(domain),
        "tenant": ctx.tenant,
        "team": ctx.team.as_deref().unwrap_or("default"),
        "correlation_id": ctx.correlation_id,
        "error": err,
    });
    operator_log::warn(module_path!(), payload.to_string());
    runner_host.publish_phase_event(PhaseEventSpec {
        event_type: "hook.directive_error",
        severity: "warn",
        outcome: Some("error"),
        ctx,
        pack_id: Some(&offer.provider_pack),
        flow_id: Some(&offer.entrypoint),
        payload,
    });
}

fn emit_hook_invocation_failed(
    runner_host: &DemoRunnerHost,
    offer: &RuntimeHookDescriptor,
    provider: &str,
    domain: Domain,
    ctx: &OperatorContext,
    err: &str,
) {
    let payload = json!({
        "event": "hook.invocation.failed",
        "offer_key": offer.offer_key,
        "pack_id": offer.provider_pack,
        "provider": provider,
        "provider_op": offer.entrypoint,
        "domain": domains::domain_name(domain),
        "tenant": ctx.tenant,
        "team": ctx.team.as_deref().unwrap_or("default"),
        "correlation_id": ctx.correlation_id,
        "error": err,
    });
    operator_log::warn(module_path!(), payload.to_string());
    runner_host.publish_phase_event(PhaseEventSpec {
        event_type: "hook.invocation_failed",
        severity: "warn",
        outcome: Some("failed"),
        ctx,
        pack_id: Some(&offer.provider_pack),
        flow_id: Some(&offer.entrypoint),
        payload,
    });
}

fn directive_action(directive: &ControlDirective) -> &'static str {
    match directive {
        ControlDirective::Continue => "continue",
        ControlDirective::Dispatch { .. } => "dispatch",
        ControlDirective::Respond { .. } => "respond",
        ControlDirective::Deny { .. } => "deny",
    }
}

fn directive_target_for_audit(directive: &ControlDirective) -> Option<JsonValue> {
    match directive {
        ControlDirective::Dispatch { target } => Some(json!({
            "tenant": target.tenant,
            "team": target.team,
            "pack": target.pack,
            "flow": target.flow,
            "node": target.node,
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::json;

    use crate::runtime_core::{
        RuntimeCore, RuntimeEvent, RuntimeHealth, RuntimeHealthStatus, ScopedStateKey,
        StateProvider,
    };

    fn test_ctx() -> OperatorContext {
        OperatorContext {
            tenant: "demo".to_string(),
            team: Some("default".to_string()),
            correlation_id: Some("corr-1".to_string()),
        }
    }

    fn test_body() -> HookIngressBody {
        HookIngressBody {
            request: json!({"x": 1}),
            response_status: 200,
            response_headers: vec![("a".to_string(), "b".to_string())],
            response_body: Some(b"ok".to_vec()),
            events: vec![json!({"event":"x"})],
        }
    }

    #[derive(Default)]
    struct RecordingTelemetryProvider {
        events: Mutex<Vec<RuntimeEvent>>,
    }

    #[async_trait]
    impl crate::runtime_core::TelemetryProvider for RecordingTelemetryProvider {
        async fn emit(&self, event: RuntimeEvent) -> anyhow::Result<()> {
            self.events
                .lock()
                .expect("recording telemetry provider lock poisoned")
                .push(event);
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    #[derive(Default)]
    struct RecordingObserverSink {
        events: Mutex<Vec<RuntimeEvent>>,
    }

    #[async_trait]
    impl crate::runtime_core::ObserverHookSink for RecordingObserverSink {
        async fn publish(&self, event: RuntimeEvent) -> anyhow::Result<()> {
            self.events
                .lock()
                .expect("recording observer sink lock poisoned")
                .push(event);
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    struct DegradedStateProvider;

    #[async_trait]
    impl StateProvider for DegradedStateProvider {
        async fn get(&self, _key: &ScopedStateKey) -> anyhow::Result<Option<JsonValue>> {
            Ok(None)
        }

        async fn put(&self, _key: &ScopedStateKey, _value: JsonValue) -> anyhow::Result<()> {
            Ok(())
        }

        async fn delete(&self, _key: &ScopedStateKey) -> anyhow::Result<()> {
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Degraded,
                reason: Some("state provider latency high".to_string()),
            })
        }
    }

    fn build_host_with_recording_sinks() -> (
        DemoRunnerHost,
        Arc<RecordingTelemetryProvider>,
        Arc<RecordingObserverSink>,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let telemetry = Arc::new(RecordingTelemetryProvider::default());
        let observer = Arc::new(RecordingObserverSink::default());
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.telemetry_provider = Some(telemetry.clone());
        seams.observer_sink = Some(observer.clone());
        host.replace_runtime_core_for_test(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));
        (host, telemetry, observer)
    }

    fn sample_hook() -> RuntimeHookDescriptor {
        RuntimeHookDescriptor {
            offer_key: "observer.pack::post_ingress.audit".to_string(),
            provider_pack: "observer.pack".to_string(),
            pack_path: PathBuf::from("packs/observer.pack.gtpack"),
            stage: HOOK_STAGE_POST_INGRESS.to_string(),
            contract_id: HOOK_CONTRACT_CONTROL_V1.to_string(),
            entrypoint: "observer.post_ingress".to_string(),
            priority: 10,
            lifecycle_state: crate::runtime_core::RuntimeLifecycleState::Active,
            health: RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            },
        }
    }

    #[test]
    fn apply_continue_keeps_body() {
        let mut body = test_body();
        let before = (
            body.response_status,
            body.response_headers.clone(),
            body.response_body.clone(),
            body.events.clone(),
        );
        apply_control_directive_with_dispatcher(
            Path::new("/tmp"),
            Domain::Messaging,
            &mut body,
            &test_ctx(),
            ControlDirective::Continue,
            |_bundle, _domain, _body, _ctx, _target| Ok(()),
        )
        .expect("apply continue");
        let after = (
            body.response_status,
            body.response_headers.clone(),
            body.response_body.clone(),
            body.events.clone(),
        );
        assert_eq!(before, after);
    }

    #[test]
    fn emit_runtime_hook_registry_loaded_publishes_phase_event() {
        let (host, telemetry, observer) = build_host_with_recording_sinks();
        let hooks = vec![sample_hook()];

        emit_runtime_hook_registry_loaded(
            &host,
            &hooks,
            "telegram",
            Domain::Messaging,
            &test_ctx(),
        );

        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let observer_events = observer.events.lock().expect("observer events");
        let event = telemetry_events.last().expect("hook chain loaded event");
        assert_eq!(observer_events.last(), Some(event));
        assert_eq!(event.event_type, "hook.chain_loaded");
        assert_eq!(event.outcome.as_deref(), Some("loaded"));
        assert_eq!(event.pack_id.as_deref(), Some("telegram"));
        assert_eq!(event.flow_id.as_deref(), Some(HOOK_STAGE_POST_INGRESS));
        assert_eq!(
            event.payload.get("hook_count").and_then(JsonValue::as_u64),
            Some(1)
        );
    }

    #[test]
    fn emit_hook_applied_publishes_phase_event() {
        let (host, telemetry, observer) = build_host_with_recording_sinks();
        let offer = sample_hook();
        emit_hook_applied(
            &host,
            &offer,
            "telegram",
            Domain::Messaging,
            &test_ctx(),
            "dispatch",
            Some(json!({"pack": "app.pack"})),
        );

        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let observer_events = observer.events.lock().expect("observer events");
        let event = telemetry_events.last().expect("hook applied event");
        assert_eq!(observer_events.last(), Some(event));
        assert_eq!(event.event_type, "hook.directive_applied");
        assert_eq!(event.outcome.as_deref(), Some("dispatch"));
        assert_eq!(event.pack_id.as_deref(), Some("observer.pack"));
        assert_eq!(event.flow_id.as_deref(), Some("observer.post_ingress"));
    }

    #[test]
    fn emit_hook_invocation_failed_publishes_phase_event() {
        let (host, telemetry, observer) = build_host_with_recording_sinks();
        let offer = sample_hook();
        emit_hook_invocation_failed(
            &host,
            &offer,
            "telegram",
            Domain::Messaging,
            &test_ctx(),
            "boom",
        );

        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let observer_events = observer.events.lock().expect("observer events");
        let event = telemetry_events.last().expect("hook failed event");
        assert_eq!(observer_events.last(), Some(event));
        assert_eq!(event.event_type, "hook.invocation_failed");
        assert_eq!(event.outcome.as_deref(), Some("failed"));
        assert_eq!(
            event.payload.get("error").and_then(JsonValue::as_str),
            Some("boom")
        );
    }

    #[test]
    fn emit_hook_parse_error_publishes_phase_event() {
        let (host, telemetry, observer) = build_host_with_recording_sinks();
        let offer = sample_hook();
        emit_hook_parse_error(
            &host,
            &offer,
            "telegram",
            Domain::Messaging,
            &test_ctx(),
            "missing_or_invalid_action",
        );

        let telemetry_events = telemetry.events.lock().expect("telemetry events");
        let observer_events = observer.events.lock().expect("observer events");
        let event = telemetry_events.last().expect("hook parse error event");
        assert_eq!(observer_events.last(), Some(event));
        assert_eq!(event.event_type, "hook.directive_error");
        assert_eq!(event.outcome.as_deref(), Some("error"));
        assert_eq!(event.pack_id.as_deref(), Some("observer.pack"));
        assert_eq!(event.flow_id.as_deref(), Some("observer.post_ingress"));
        assert_eq!(
            event.payload.get("error").and_then(JsonValue::as_str),
            Some("missing_or_invalid_action")
        );
    }

    #[test]
    fn apply_post_ingress_hooks_refuses_when_safe_mode_blocks_hooks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let discovery = crate::discovery::DiscoveryResult {
            domains: crate::discovery::DetectedDomains {
                messaging: false,
                events: false,
                oauth: false,
            },
            providers: Vec::new(),
        };
        let secrets_handle =
            crate::secrets_gate::resolve_secrets_manager(tmp.path(), "default", None)
                .expect("resolve secrets manager");
        let host = DemoRunnerHost::new(
            tmp.path().to_path_buf(),
            &discovery,
            None,
            secrets_handle,
            false,
        )
        .expect("build host");

        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.state_provider = Some(Arc::new(DegradedStateProvider));
        host.replace_runtime_core_for_test(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let request = IngressRequestV1 {
            v: 1,
            domain: "messaging".to_string(),
            provider: "telegram".to_string(),
            handler: None,
            tenant: "demo".to_string(),
            team: Some("default".to_string()),
            method: "POST".to_string(),
            path: "/v1/messaging/ingress/telegram/demo/default".to_string(),
            query: Vec::new(),
            headers: Vec::new(),
            body: br#"{}"#.to_vec(),
            correlation_id: Some("corr-1".to_string()),
            remote_addr: None,
        };
        let mut result = IngressDispatchResult {
            response: IngressHttpResponse {
                status: 200,
                headers: vec![("a".to_string(), "b".to_string())],
                body: Some(b"ok".to_vec()),
            },
            events: Vec::new(),
            messaging_envelopes: Vec::new(),
        };
        let err = apply_post_ingress_hooks_dispatch(
            tmp.path(),
            &host,
            Domain::Messaging,
            &request,
            &mut result,
            &test_ctx(),
        )
        .expect_err("safe mode should refuse post-ingress hooks");
        assert!(err.to_string().contains("runtime request refused"));
    }

    #[test]
    fn apply_respond_overrides_response() {
        let mut body = test_body();
        apply_control_directive_with_dispatcher(
            Path::new("/tmp"),
            Domain::Messaging,
            &mut body,
            &test_ctx(),
            ControlDirective::Respond {
                reply: IngressReply {
                    text: Some("hello".to_string()),
                    card_cbor: None,
                    status_code: Some(201),
                    reason_code: None,
                },
            },
            |_bundle, _domain, _body, _ctx, _target| Ok(()),
        )
        .expect("apply respond");
        assert_eq!(body.response_status, 201);
        assert_eq!(
            body.response_headers,
            vec![("content-type".to_string(), "text/plain".to_string())]
        );
        assert_eq!(body.response_body, Some(b"hello".to_vec()));
        assert!(body.events.is_empty());
    }

    #[test]
    fn apply_deny_sets_reason_header() {
        let mut body = test_body();
        apply_control_directive_with_dispatcher(
            Path::new("/tmp"),
            Domain::Messaging,
            &mut body,
            &test_ctx(),
            ControlDirective::Deny {
                reply: IngressReply {
                    text: Some("denied".to_string()),
                    card_cbor: None,
                    status_code: None,
                    reason_code: Some("blocked".to_string()),
                },
            },
            |_bundle, _domain, _body, _ctx, _target| Ok(()),
        )
        .expect("apply deny");
        assert_eq!(body.response_status, 403);
        assert!(
            body.response_headers
                .iter()
                .any(|(k, v)| k == "x-reason-code" && v == "blocked")
        );
        assert!(body.events.is_empty());
    }

    #[test]
    fn apply_dispatch_calls_dispatcher_and_short_circuits() {
        let mut body = test_body();
        let mut called = false;
        apply_control_directive_with_dispatcher(
            Path::new("/tmp"),
            Domain::Messaging,
            &mut body,
            &test_ctx(),
            ControlDirective::Dispatch {
                target: DispatchTarget {
                    tenant: "acme".to_string(),
                    team: Some("team-1".to_string()),
                    pack: "pack-a".to_string(),
                    flow: Some("flow-x".to_string()),
                    node: None,
                },
            },
            |_bundle, _domain, _body, _ctx, _target| {
                called = true;
                Ok(())
            },
        )
        .expect("apply dispatch");
        assert!(called);
        assert_eq!(body.response_status, 202);
        assert!(body.events.is_empty());
    }
}
