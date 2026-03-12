//! Capability bootstrap: discovery report, telemetry upgrade, and state store upgrade.
//!
//! Extracted from `cli.rs` to keep the CLI module focused on argument
//! parsing and command dispatch.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use greentic_state::redis_store::RedisStateStore;

use crate::capabilities::ResolveScope;
use crate::demo::runner_host::{DemoRunnerHost, OperatorContext};
use crate::domains::Domain;
use crate::operator_i18n;
use crate::operator_log;
use crate::secrets_gate::SecretsManagerHandle;
use crate::secrets_setup::{SecretsSetup, resolve_env};
use greentic_runner_host::storage::DynStateStore;

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
const DEFAULT_TELEMETRY_SERVICE_NAME: &str = "greentic-operator";

#[derive(Debug, Clone, serde::Deserialize)]
struct LegacyTelemetryProviderConfig {
    #[serde(default)]
    service_name: Option<String>,
    #[serde(default)]
    export_mode: Option<String>,
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default, alias = "otlp_endpoint")]
    otlp_endpoint: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default, alias = "otlp_headers")]
    otlp_headers: HashMap<String, String>,
    #[serde(default)]
    sampling_ratio: Option<f64>,
    #[serde(default)]
    compression: Option<String>,
}

impl LegacyTelemetryProviderConfig {
    fn service_name(&self) -> &str {
        self.service_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(DEFAULT_TELEMETRY_SERVICE_NAME)
    }

    fn endpoint(&self) -> Option<&str> {
        self.endpoint
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                self.otlp_endpoint
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
            })
    }

    fn merged_headers(&self) -> HashMap<String, String> {
        let mut headers = self.otlp_headers.clone();
        headers.extend(self.headers.clone());
        headers
    }

    fn to_runtime_config(
        &self,
    ) -> Result<(
        greentic_telemetry::TelemetryConfig,
        greentic_telemetry::export::ExportConfig,
    )> {
        use greentic_telemetry::export::{
            Compression as RuntimeCompression, ExportConfig, ExportMode,
            Sampling as RuntimeSampling,
        };

        let preset = self.preset.as_deref().map(parse_cloud_preset).transpose()?;
        let preset_config = preset
            .map(greentic_telemetry::presets::load_preset)
            .transpose()?
            .unwrap_or_default();

        let mode = if let Some(export_mode) = self.export_mode.as_deref() {
            parse_export_mode(export_mode)?
        } else {
            preset_config.export_mode.unwrap_or(ExportMode::JsonStdout)
        };

        let endpoint = self
            .endpoint()
            .map(str::to_owned)
            .or(preset_config.otlp_endpoint);

        let mut headers = preset_config.otlp_headers;
        headers.extend(self.merged_headers());

        let sampling = match self.sampling_ratio {
            Some(ratio) if !(0.0..=1.0).contains(&ratio) => {
                return Err(anyhow!(
                    "telemetry.configure returned sampling_ratio outside 0.0..=1.0: {ratio}"
                ));
            }
            Some(ratio) if ratio <= 0.0 => RuntimeSampling::AlwaysOff,
            Some(ratio) if ratio >= 1.0 => RuntimeSampling::AlwaysOn,
            Some(ratio) => RuntimeSampling::TraceIdRatio(ratio),
            None => RuntimeSampling::Parent,
        };

        let compression = self
            .compression
            .as_deref()
            .map(parse_compression)
            .transpose()?;

        Ok((
            greentic_telemetry::TelemetryConfig {
                service_name: self.service_name().to_string(),
            },
            ExportConfig {
                mode,
                endpoint,
                headers,
                sampling,
                compression: compression.map(|value| match value {
                    CompressionCompat::Gzip => RuntimeCompression::Gzip,
                }),
            },
        ))
    }
}

#[derive(Clone, Copy, Debug)]
enum CompressionCompat {
    Gzip,
}

fn parse_export_mode(value: &str) -> Result<greentic_telemetry::export::ExportMode> {
    use greentic_telemetry::export::ExportMode;

    match value.trim().to_ascii_lowercase().as_str() {
        "json-stdout" | "json_stdout" | "stdout" => Ok(ExportMode::JsonStdout),
        "otlp-grpc" | "otlp_grpc" => Ok(ExportMode::OtlpGrpc),
        "otlp-http" | "otlp_http" => Ok(ExportMode::OtlpHttp),
        other => Err(anyhow!("unsupported telemetry export_mode '{other}'")),
    }
}

fn parse_cloud_preset(value: &str) -> Result<greentic_telemetry::presets::CloudPreset> {
    use greentic_telemetry::presets::CloudPreset;

    match value.trim().to_ascii_lowercase().as_str() {
        "aws" => Ok(CloudPreset::Aws),
        "gcp" => Ok(CloudPreset::Gcp),
        "azure" => Ok(CloudPreset::Azure),
        "datadog" => Ok(CloudPreset::Datadog),
        "loki" => Ok(CloudPreset::Loki),
        "none" => Ok(CloudPreset::None),
        other => Err(anyhow!("unsupported telemetry preset '{other}'")),
    }
}

fn parse_compression(value: &str) -> Result<CompressionCompat> {
    match value.trim().to_ascii_lowercase().as_str() {
        "gzip" => Ok(CompressionCompat::Gzip),
        other => Err(anyhow!("unsupported telemetry compression '{other}'")),
    }
}

fn validate_telemetry_config(config: &LegacyTelemetryProviderConfig) -> Vec<String> {
    let mut warnings = Vec::new();

    if config.export_mode.is_none() && config.preset.is_none() {
        warnings.push(
            "telemetry.configure returned no export_mode or preset; defaulting to json-stdout"
                .to_string(),
        );
    }

    if matches!(
        config.export_mode
            .as_deref()
            .map(|value| value.trim().to_ascii_lowercase()),
        Some(mode) if mode == "otlp-grpc" || mode == "otlp_grpc" || mode == "otlp-http" || mode == "otlp_http"
    ) && config.endpoint().is_none()
        && config.preset.is_none()
    {
        warnings.push(
            "telemetry.configure returned OTLP mode without endpoint or preset; runtime defaults will be used"
                .to_string(),
        );
    }

    warnings
}

/// Seed secrets for the telemetry capability pack, invoke the WASM component,
/// and initialize the OTel pipeline with the returned config.
///
/// When `setup_answers` is provided (e.g. from `--setup-input`), the values are
/// persisted to the dev secrets store so the WASM component can read them via
/// `read_secret()`.  Without this step the component receives empty secrets and
/// the exporter has no valid connection string / endpoint.
///
/// Returns `Ok(true)` if telemetry was upgraded, `Ok(false)` if no capability found.
pub fn try_upgrade_telemetry(
    bundle: &Path,
    runner_host: &DemoRunnerHost,
    tenant: &str,
    team: Option<&str>,
    env_override: Option<&str>,
    setup_answers: Option<&serde_json::Value>,
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

    // 2b. Persist setup_answers as secrets so the WASM component can read them.
    //     The wizard execute flow only persists secrets for domain providers
    //     (Messaging/Events/Secrets/OAuth) but capability packs like
    //     telemetry-otlp are not in any domain, so their config values never
    //     reach the dev store.
    if let Some(answers) = setup_answers {
        if answers.as_object().is_some_and(|m| !m.is_empty()) {
            let pack_path_ref = Some(binding.pack_path.as_path());
            let persist_rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(rt) = persist_rt {
                match rt.block_on(crate::qa_persist::persist_all_config_as_secrets(
                    bundle,
                    &env,
                    tenant,
                    team,
                    &binding.pack_id,
                    answers,
                    pack_path_ref,
                )) {
                    Ok(saved) if !saved.is_empty() => {
                        tracing::info!(
                            pack_id = %binding.pack_id,
                            count = saved.len(),
                            keys = ?saved,
                            "persisted telemetry setup answers as secrets"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            pack_id = %binding.pack_id,
                            error = %e,
                            "failed to persist telemetry setup answers"
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    // 3. Mark the telemetry capability as ready before invoking.
    //    The telemetry bootstrap runs before the server starts, so there is
    //    no prior QA wizard step that would create the install record.
    //    Without this, invoke_capability returns capability_not_installed.
    let ctx = OperatorContext {
        tenant: tenant.to_string(),
        team: team.map(|t| t.to_string()),
        correlation_id: None,
    };
    if let Err(e) = runner_host.mark_capability_ready(&ctx, &binding) {
        tracing::warn!(error = %e, "failed to mark telemetry capability as ready (non-fatal)");
    }

    // 4. Invoke the WASM component
    let payload = serde_json::json!({});
    let payload_bytes = serde_json::to_vec(&payload)?;

    let outcome = runner_host.invoke_capability(
        CAP_TELEMETRY_V1,
        TELEMETRY_CONFIGURE_OP,
        &payload_bytes,
        &ctx,
    )?;

    if !outcome.success {
        let error_msg = outcome.error.unwrap_or_else(|| "unknown error".to_string());
        tracing::warn!(error = %error_msg, "telemetry.configure capability invocation failed");
        return Ok(false);
    }

    // 5. Parse the telemetry provider output using a local compatibility shim.
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

    let config: LegacyTelemetryProviderConfig = serde_json::from_value(config_json)?;

    // 6. Validate config
    let warnings = validate_telemetry_config(&config);
    for warning in &warnings {
        tracing::warn!(warning = %warning, "telemetry config validation");
    }

    // 7. Initialize OTel pipeline
    let (telemetry_config, export_config) = config.to_runtime_config()?;
    greentic_telemetry::init_telemetry_from_config(telemetry_config, export_config)?;

    tracing::info!(
        export_mode = ?config.export_mode,
        preset = ?config.preset,
        endpoint = ?config.endpoint(),
        sampling_ratio = config.sampling_ratio,
        "telemetry upgraded from capability provider"
    );

    Ok(true)
}

// ---------------------------------------------------------------------------
// State store capability upgrade (in-memory → Redis)
// ---------------------------------------------------------------------------

const CAP_STATE_KV_V1: &str = "greentic.cap.state.kv.v1";

/// Try to upgrade the state store from in-memory to Redis by reading
/// the `redis_url` secret from the state-redis capability pack.
///
/// When `setup_answers` is provided, the values are persisted to the dev
/// secrets store first (same pattern as telemetry).
///
/// Returns `Ok(Some(store))` with a Redis-backed state store on success,
/// `Ok(None)` if no state capability found or Redis URL unavailable.
pub fn try_upgrade_state_store(
    bundle: &Path,
    runner_host: &DemoRunnerHost,
    secrets_handle: &SecretsManagerHandle,
    tenant: &str,
    team: Option<&str>,
    env_override: Option<&str>,
    setup_answers: Option<&serde_json::Value>,
) -> Result<Option<DynStateStore>> {
    let env = resolve_env(env_override);
    let scope = ResolveScope {
        env: Some(env.clone()),
        tenant: Some(tenant.to_string()),
        team: team.map(|t| t.to_string()),
    };

    // 1. Resolve the state.kv capability
    let Some(binding) = runner_host.resolve_capability(CAP_STATE_KV_V1, None, scope) else {
        eprintln!(
            "[state-store] no capability '{}' found — using in-memory",
            CAP_STATE_KV_V1
        );
        return Ok(None);
    };
    eprintln!(
        "[state-store] resolved capability: pack_id={} stable_id={}",
        binding.pack_id, binding.stable_id
    );

    // 2. Seed secrets for the state pack
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
                    "state capability secret seeding failed"
                );
            }
        }
    }

    // 2b. Persist setup_answers as secrets (same as telemetry — capability packs
    //     are not in any domain, so the wizard doesn't persist their config).
    if let Some(answers) = setup_answers {
        if answers.as_object().is_some_and(|m| !m.is_empty()) {
            let pack_path_ref = Some(binding.pack_path.as_path());
            let persist_rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(rt) = persist_rt {
                match rt.block_on(crate::qa_persist::persist_all_config_as_secrets(
                    bundle,
                    &env,
                    tenant,
                    team,
                    &binding.pack_id,
                    answers,
                    pack_path_ref,
                )) {
                    Ok(saved) if !saved.is_empty() => {
                        tracing::info!(
                            pack_id = %binding.pack_id,
                            count = saved.len(),
                            keys = ?saved,
                            "persisted state-redis setup answers as secrets"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            pack_id = %binding.pack_id,
                            error = %e,
                            "failed to persist state-redis setup answers"
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    // 3. Read the redis_url secret
    let canonical_team = crate::secrets_manager::canonical_team(team);
    let secret_uri = format!(
        "secrets://{}/{}/{}/{}/redis_url",
        env, tenant, canonical_team, binding.pack_id
    );

    eprintln!("[state-store] reading secret: {}", secret_uri);
    let redis_url = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let manager = secrets_handle.manager();
        match rt.block_on(manager.read(&secret_uri)) {
            Ok(bytes) => {
                let url = String::from_utf8(bytes).ok();
                eprintln!(
                    "[state-store] redis_url secret found (len={})",
                    url.as_ref().map_or(0, |s| s.len())
                );
                url
            }
            Err(e) => {
                eprintln!("[state-store] failed to read redis_url secret: {e}");
                None
            }
        }
    };

    let Some(redis_url) = redis_url else {
        // Fallback: try REDIS_URL environment variable
        match std::env::var("REDIS_URL") {
            Ok(url) => {
                tracing::info!("using REDIS_URL environment variable for state store");
                return create_redis_store(&url);
            }
            Err(_) => {
                tracing::warn!(
                    "redis_url secret not found and REDIS_URL env not set — using in-memory state store"
                );
                return Ok(None);
            }
        }
    };

    create_redis_store(&redis_url)
}

fn create_redis_store(redis_url: &str) -> Result<Option<DynStateStore>> {
    match RedisStateStore::from_url(redis_url) {
        Ok(store) => {
            let store: DynStateStore = Arc::new(store);
            eprintln!("[state-store] ✓ upgraded to Redis: {}", redis_url);
            tracing::info!(
                redis_url = %redis_url,
                "state store upgraded to Redis"
            );
            Ok(Some(store))
        }
        Err(e) => {
            eprintln!("[state-store] ✗ failed to create Redis store: {e}");
            tracing::warn!(
                error = %e,
                "failed to create Redis state store — using in-memory fallback"
            );
            Ok(None)
        }
    }
}
