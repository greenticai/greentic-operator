//! Capability bootstrap: discovery report and telemetry upgrade.
//!
//! Extracted from `cli.rs` to keep the CLI module focused on argument
//! parsing and command dispatch.

use std::path::Path;

use anyhow::Result;

use crate::capabilities::ResolveScope;
use crate::demo::runner_host::{DemoRunnerHost, OperatorContext};
use crate::domains::Domain;
use crate::operator_i18n;
use crate::operator_log;
use crate::secrets_setup::{SecretsSetup, resolve_env};

// ---------------------------------------------------------------------------
// Capability expectations & bootstrap report
// ---------------------------------------------------------------------------

enum CapabilityPriority {
    Required,
    Recommended,
}

struct CapabilityExpectation {
    cap_id: &'static str,
    priority: CapabilityPriority,
}

fn capability_expectations_for_domains(domains: &[Domain]) -> Vec<CapabilityExpectation> {
    let mut out = Vec::new();
    let has_messaging = domains.contains(&Domain::Messaging);
    let has_events = domains.contains(&Domain::Events);
    let has_secrets = domains.contains(&Domain::Secrets);

    if has_messaging {
        out.push(CapabilityExpectation {
            cap_id: "greentic.cap.messaging.provider.v1",
            priority: CapabilityPriority::Required,
        });
    }
    if has_events {
        out.push(CapabilityExpectation {
            cap_id: "greentic.cap.events.provider.v1",
            priority: CapabilityPriority::Required,
        });
    }
    if has_secrets {
        out.push(CapabilityExpectation {
            cap_id: "greentic.cap.secrets.store.v1",
            priority: CapabilityPriority::Required,
        });
    }
    if has_messaging || has_events {
        out.push(CapabilityExpectation {
            cap_id: "greentic.cap.oauth.broker.v1",
            priority: CapabilityPriority::Recommended,
        });
        out.push(CapabilityExpectation {
            cap_id: "greentic.cap.mcp.exec.v1",
            priority: CapabilityPriority::Recommended,
        });
    }

    out
}

/// Log a report of resolved vs missing capabilities for the given domains.
pub fn log_capability_bootstrap_report(
    runner_host: &DemoRunnerHost,
    ctx: &OperatorContext,
    domains: &[Domain],
) {
    let scope = ResolveScope {
        env: std::env::var("GREENTIC_ENV").ok(),
        tenant: Some(ctx.tenant.clone()),
        team: ctx.team.clone(),
    };
    let expectations = capability_expectations_for_domains(domains);
    let mut missing_required = Vec::new();
    let mut missing_recommended = Vec::new();
    for item in &expectations {
        let resolved = runner_host.resolve_capability(item.cap_id, None, scope.clone());
        if resolved.is_none()
            && item.cap_id == "greentic.cap.secrets.store.v1"
            && domains.contains(&Domain::Secrets)
            && runner_host.has_provider_packs_for_domain(Domain::Secrets)
        {
            operator_log::info(
                module_path!(),
                "capability bootstrap: using legacy secrets providers fallback for greentic.cap.secrets.store.v1",
            );
            continue;
        }
        if resolved.is_none() {
            match item.priority {
                CapabilityPriority::Required => missing_required.push(item.cap_id.to_string()),
                CapabilityPriority::Recommended => {
                    missing_recommended.push(item.cap_id.to_string())
                }
            }
        }
    }

    let pending_setup = runner_host.capability_setup_plan(ctx);
    if pending_setup.is_empty() {
        operator_log::info(
            module_path!(),
            "capability setup plan: no capabilities requiring setup found",
        );
    } else {
        let ids = pending_setup
            .iter()
            .map(|binding| format!("{}@{}", binding.cap_id, binding.stable_id))
            .collect::<Vec<_>>()
            .join(", ");
        operator_log::info(
            module_path!(),
            format!(
                "capability setup plan pending={} [{}]",
                pending_setup.len(),
                ids
            ),
        );
    }

    if !missing_required.is_empty() {
        let joined = missing_required.join(", ");
        operator_log::warn(
            module_path!(),
            format!("missing required capabilities for setup/start: {joined}"),
        );
        eprintln!(
            "{}",
            operator_i18n::trf(
                "cli.capability.bootstrap.missing_required",
                "Warning: missing required capabilities: {}",
                &[&joined]
            )
        );
    }
    if !missing_recommended.is_empty() {
        let joined = missing_recommended.join(", ");
        operator_log::warn(
            module_path!(),
            format!("missing recommended capabilities for setup/start: {joined}"),
        );
        eprintln!(
            "{}",
            operator_i18n::trf(
                "cli.capability.bootstrap.missing_recommended",
                "Note: missing recommended capabilities: {}",
                &[&joined]
            )
        );
    }
}

// ---------------------------------------------------------------------------
// Telemetry capability upgrade
// ---------------------------------------------------------------------------

const CAP_TELEMETRY_V1: &str = "greentic.cap.telemetry.v1";
const TELEMETRY_CONFIGURE_OP: &str = "telemetry.configure";

/// Seed secrets for the telemetry capability pack, invoke the WASM component,
/// and initialize the OTel pipeline with the returned config.
///
/// Returns `Ok(true)` if telemetry was upgraded, `Ok(false)` if no capability found.
pub fn try_upgrade_telemetry(
    bundle: &Path,
    runner_host: &DemoRunnerHost,
    tenant: &str,
    team: Option<&str>,
    env_override: Option<&str>,
) -> Result<bool> {
    let env = resolve_env(env_override);
    let scope = ResolveScope {
        env: Some(env.clone()),
        tenant: Some(tenant.to_string()),
        team: team.map(|t| t.to_string()),
    };

    // 1. Resolve the telemetry capability
    let Some(binding) = runner_host.resolve_capability(CAP_TELEMETRY_V1, None, scope) else {
        tracing::debug!("no telemetry capability found — skipping upgrade");
        return Ok(false);
    };
    tracing::info!(
        pack_id = %binding.pack_id,
        stable_id = %binding.stable_id,
        "resolved telemetry capability"
    );

    // 2. Seed secrets for the telemetry pack
    if let Ok(secrets_setup) = SecretsSetup::new(bundle, &env, tenant, team) {
        if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            if let Err(e) =
                rt.block_on(secrets_setup.ensure_pack_secrets(&binding.pack_path, &binding.pack_id))
            {
                tracing::warn!(
                    pack_id = %binding.pack_id,
                    error = %e,
                    "telemetry capability secret seeding failed"
                );
            }
        }
    }

    // 3. Invoke the WASM component
    let ctx = OperatorContext {
        tenant: tenant.to_string(),
        team: team.map(|t| t.to_string()),
        correlation_id: None,
    };
    let payload = serde_json::json!({});
    let payload_bytes = serde_json::to_vec(&payload)?;

    let outcome =
        runner_host.invoke_capability(CAP_TELEMETRY_V1, TELEMETRY_CONFIGURE_OP, &payload_bytes, &ctx)?;

    if !outcome.success {
        let error_msg = outcome
            .error
            .unwrap_or_else(|| "unknown error".to_string());
        tracing::warn!(error = %error_msg, "telemetry.configure capability invocation failed");
        return Ok(false);
    }

    // 4. Parse the TelemetryProviderConfig from the outcome
    let raw_output = match outcome.output {
        Some(value) => value,
        None => {
            tracing::warn!("telemetry.configure returned no output");
            return Ok(false);
        }
    };

    tracing::debug!(config = %raw_output, "telemetry provider config received");

    // The WASM component wraps output in {"ok":true,"output":{...}}
    // Extract the inner "output" field if present.
    let config_json = if let Some(inner) = raw_output.get("output") {
        inner.clone()
    } else {
        raw_output
    };

    let config: greentic_telemetry::provider::TelemetryProviderConfig =
        serde_json::from_value(config_json)?;

    // 5. Validate config
    let warnings = greentic_telemetry::provider::validate_telemetry_config(&config);
    for warning in &warnings {
        tracing::warn!(warning = %warning, "telemetry config validation");
    }

    // 6. Initialize OTel pipeline
    greentic_telemetry::provider::init_from_provider_config(&config)?;

    eprintln!(
        "telemetry upgraded from capability provider (export_mode={}, preset={:?})",
        config.export_mode,
        config.preset,
    );

    Ok(true)
}
