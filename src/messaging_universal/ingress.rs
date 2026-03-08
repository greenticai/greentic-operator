//! HTTP ingress helpers for the universal pipeline.

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use greentic_types::ChannelMessageEnvelope;
use std::path::{Path, PathBuf};

use crate::demo::runner_host::{DemoRunnerHost, OperatorContext};
use crate::discovery;
use crate::domains::Domain;
use crate::hooks::runner::apply_post_ingress_hooks_http;
use crate::messaging_universal::dto::{HttpInV1, HttpOutV1};
use crate::secrets_gate::SecretsManagerHandle;

/// Construct an `HttpInV1` payload for the given request data.
#[allow(clippy::too_many_arguments)]
pub fn build_ingress_request(
    provider: &str,
    route: Option<String>,
    method: &str,
    path: &str,
    headers: Vec<(String, String)>,
    query: Vec<(String, String)>,
    body: &[u8],
    binding_id: Option<String>,
    tenant_hint: Option<String>,
    team_hint: Option<String>,
) -> HttpInV1 {
    HttpInV1 {
        v: 1,
        provider: provider.to_string(),
        route,
        binding_id,
        tenant_hint,
        team_hint,
        method: method.to_string(),
        path: path.to_string(),
        query,
        headers,
        body_b64: STANDARD.encode(body),
    }
}

pub fn parse_ingress_response(response: &HttpOutV1) -> Result<(), anyhow::Error> {
    let _ = response;
    Ok(())
}

pub fn run_ingress(
    bundle: &Path,
    provider: &str,
    request: &HttpInV1,
    ctx: &OperatorContext,
    runner_binary: Option<PathBuf>,
    secrets_handle: SecretsManagerHandle,
) -> anyhow::Result<(HttpOutV1, Vec<ChannelMessageEnvelope>)> {
    let discovery = discovery::discover_bundle_with_options(
        bundle,
        discovery::DiscoveryOptions { cbor_only: true },
    )?;
    let runner_host = DemoRunnerHost::new(
        bundle.to_path_buf(),
        &discovery,
        runner_binary,
        secrets_handle.clone(),
        false,
    )?;
    let input_bytes = serde_json::to_vec(request)?;
    let response_outcome = runner_host.invoke_provider_op(
        Domain::Messaging,
        provider,
        "ingest_http",
        &input_bytes,
        ctx,
    )?;
    let output = match response_outcome.output {
        Some(value) => value,
        None => serde_json::json!({}),
    };
    let mut response = serde_json::from_value::<HttpOutV1>(output)
        .with_context(|| "failed to deserialize HttpOutV1 response")?;
    apply_post_ingress_hooks_http(
        &runner_host.bundle_read_root(),
        &runner_host,
        request,
        &mut response,
        ctx,
    )?;
    let mut envelopes = Vec::new();
    for event in response.events.iter() {
        let envelope: ChannelMessageEnvelope =
            serde_json::from_value(event.clone()).with_context(|| "invalid envelope")?;
        envelopes.push(envelope);
    }
    Ok((response, envelopes))
}
