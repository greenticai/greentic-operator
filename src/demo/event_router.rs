use std::path::Path;

use anyhow::Context;
use serde_json::{Value as JsonValue, json};

use crate::demo::ingress_types::EventEnvelopeV1;
use crate::demo::runner_host::{DemoRunnerHost, OperatorContext};
use crate::domains::Domain;
use crate::messaging_universal::app;
use crate::operator_log;
use crate::runner_exec::{self, RunRequest};

pub fn route_events_to_default_flow(
    runner_host: &DemoRunnerHost,
    bundle_read_root: &Path,
    state_root: &Path,
    ctx: &OperatorContext,
    events: &[EventEnvelopeV1],
) -> anyhow::Result<usize> {
    if events.is_empty() {
        return Ok(0);
    }
    runner_host
        .enforce_request_policy(
            "event_router.default_flow",
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
    let team = ctx.team.as_deref();
    let app_pack_path = app::resolve_app_pack_path(bundle_read_root, &ctx.tenant, team, None)
        .context("resolve default app pack for event routing")?;
    let pack_info = app::load_app_pack_info(&app_pack_path).context("load app pack manifest")?;
    let flow = app::select_app_flow(&pack_info).context("select app default flow")?;

    let mut routed = 0usize;
    for event in events {
        let input = build_event_flow_input(event, ctx);
        let request = RunRequest {
            root: state_root.to_path_buf(),
            run_dir: None,
            domain: Domain::Events,
            pack_path: app_pack_path.clone(),
            pack_label: pack_info.pack_id.clone(),
            flow_id: flow.id.clone(),
            tenant: ctx.tenant.clone(),
            team: ctx.team.clone(),
            input,
            dist_offline: true,
        };
        runner_exec::run_provider_pack_flow(request)
            .with_context(|| format!("route event {} -> {}", event.event_type, flow.id))?;
        routed += 1;
    }
    operator_log::info(
        module_path!(),
        format!(
            "event router delivered {} event(s) to pack={} flow={}",
            routed, pack_info.pack_id, flow.id
        ),
    );
    Ok(routed)
}

fn build_event_flow_input(event: &EventEnvelopeV1, ctx: &OperatorContext) -> JsonValue {
    json!({
        "event": event,
        "events": [event],
        "tenant": ctx.tenant,
        "team": ctx.team,
        "correlation_id": event.correlation_id.clone().or(ctx.correlation_id.clone()),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tempfile::tempdir;

    use super::*;
    use crate::runtime_core::{
        RuntimeCore, RuntimeHealth, RuntimeHealthStatus, ScopedStateKey, StateProvider,
    };

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

    #[test]
    fn route_events_to_default_flow_refuses_when_safe_mode_blocks_event_router() {
        let tmp = tempdir().expect("tempdir");
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

        let event = EventEnvelopeV1 {
            event_id: "evt-1".to_string(),
            event_type: "demo.event".to_string(),
            occurred_at: "2026-03-07T00:00:00Z".to_string(),
            source: crate::demo::ingress_types::EventSourceV1 {
                domain: "events".to_string(),
                provider: "provider-a".to_string(),
                handler_id: None,
            },
            scope: crate::demo::ingress_types::EventScopeV1 {
                tenant: "tenant-a".to_string(),
                team: Some("default".to_string()),
            },
            correlation_id: Some("corr-event".to_string()),
            payload: json!({"ok": true}),
            http: None,
            raw: None,
        };
        let ctx = OperatorContext {
            tenant: "tenant-a".to_string(),
            team: Some("default".to_string()),
            correlation_id: Some("corr-event".to_string()),
        };

        let err = route_events_to_default_flow(&host, tmp.path(), tmp.path(), &ctx, &[event])
            .expect_err("safe mode should refuse event routing");
        assert!(err.to_string().contains("runtime request refused"));
    }
}
