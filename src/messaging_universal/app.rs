use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use greentic_types::ChannelMessageEnvelope;
use serde_cbor::Value as CborValue;
use serde_json::Value as JsonValue;
use serde_json::json;
use zip::ZipArchive;

use crate::demo::runner_host::OperatorContext;
use crate::runner_exec::{self, RunRequest};

#[derive(Clone, Debug)]
pub struct AppPackInfo {
    pub pack_id: String,
    pub flows: Vec<AppFlowInfo>,
}

#[derive(Clone, Debug)]
pub struct AppFlowInfo {
    pub id: String,
    pub kind: String,
}

pub fn resolve_app_pack_path(
    bundle: &Path,
    tenant: &str,
    team: Option<&str>,
    override_path: Option<&str>,
) -> Result<PathBuf> {
    if let Some(override_value) = override_path {
        let candidate = PathBuf::from(override_value);
        if candidate.exists() {
            return Ok(candidate);
        }
        bail!("APP_PACK_NOT_FOUND override path {override_value} does not exist");
    }

    let packs_root = bundle.join("packs");
    let mut tried = Vec::new();
    if let Some(team_id) = team {
        let candidate = packs_root.join(tenant).join(team_id).join("default.gtpack");
        tried.push(candidate.clone());
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    let candidate = packs_root.join(tenant).join("default.gtpack");
    tried.push(candidate.clone());
    if candidate.exists() {
        return Ok(candidate);
    }
    let candidate = packs_root.join("default.gtpack");
    tried.push(candidate.clone());
    if candidate.exists() {
        return Ok(candidate);
    }

    let paths = tried
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!("APP_PACK_NOT_FOUND; tried {paths}");
}

pub fn load_app_pack_info(pack_path: &Path) -> Result<AppPackInfo> {
    let file = File::open(pack_path).with_context(|| format!("unable to open {pack_path:?}"))?;
    let mut archive = ZipArchive::new(file)?;
    let mut manifest = archive
        .by_name("manifest.cbor")
        .with_context(|| format!("pack {pack_path:?} missing manifest.cbor"))?;
    let mut buf = Vec::new();
    manifest.read_to_end(&mut buf)?;
    let value: CborValue = serde_cbor::from_slice(&buf)?;
    let pack_id = extract_text_or_symbol(&value, "pack_id", "pack_ids")
        .ok_or_else(|| anyhow::anyhow!("pack manifest missing pack id in {pack_path:?}"))?;
    let flows = extract_flows(&value);
    Ok(AppPackInfo { pack_id, flows })
}

pub fn select_app_flow(info: &AppPackInfo) -> Result<&AppFlowInfo> {
    if let Some(flow) = info.flows.iter().find(|flow| flow.id == "default") {
        return Ok(flow);
    }
    let messaging_flows: Vec<_> = info
        .flows
        .iter()
        .filter(|flow| flow.kind.eq_ignore_ascii_case("messaging"))
        .collect();
    if messaging_flows.len() == 1 {
        return Ok(messaging_flows[0]);
    }
    let available = info
        .flows
        .iter()
        .map(|flow| flow.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    bail!("APP_FLOW_NOT_RESOLVED; available flows: {available}");
}

pub fn run_app_flow(
    bundle: &Path,
    ctx: &OperatorContext,
    pack_path: &Path,
    pack_id: &str,
    flow_id: &str,
    envelope: &ChannelMessageEnvelope,
) -> Result<Vec<ChannelMessageEnvelope>> {
    let request = RunRequest {
        root: bundle.to_path_buf(),
        domain: crate::domains::Domain::Messaging,
        pack_path: pack_path.to_path_buf(),
        pack_label: pack_id.to_string(),
        flow_id: flow_id.to_string(),
        tenant: ctx.tenant.clone(),
        team: ctx.team.clone(),
        input: json!({
            "message": envelope,
            "tenant": ctx.tenant,
            "team": ctx.team,
            "correlation_id": ctx.correlation_id,
        }),
        dist_offline: true,
    };

    let output = runner_exec::run_provider_pack_flow(request)?;
    // Check if the envelope contains AC action routing metadata (routeToCardId/toCardId).
    // If so, select the matching card node output from the transcript instead of the default.
    let target_node = envelope
        .metadata
        .get("routeToCardId")
        .or_else(|| envelope.metadata.get("toCardId"));
    let value = collect_transcript_outputs(&output.run_dir, target_node.map(|s| s.as_str()))?
        .ok_or_else(|| anyhow::anyhow!("app flow produced no outputs"))?;
    parse_envelopes(&value, envelope)
}

/// Extract a text field that may be stored as a symbol index (integer)
/// referencing the symbols table in the manifest.
fn extract_text_or_symbol(value: &CborValue, key: &str, symbol_table: &str) -> Option<String> {
    let map = match value {
        CborValue::Map(map) => map,
        _ => return None,
    };
    let cbor_key = CborValue::Text(key.to_string());
    match map.get(&cbor_key)? {
        CborValue::Text(text) => Some(text.clone()),
        CborValue::Integer(idx) => {
            let idx = *idx as usize;
            let symbols_key = CborValue::Text("symbols".to_string());
            let table_key = CborValue::Text(symbol_table.to_string());
            let symbols = map.get(&symbols_key)?;
            if let CborValue::Map(sym_map) = symbols {
                if let Some(CborValue::Array(entries)) = sym_map.get(&table_key) {
                    if let Some(CborValue::Text(resolved)) = entries.get(idx) {
                        return Some(resolved.clone());
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_flows(value: &CborValue) -> Vec<AppFlowInfo> {
    let mut flows = Vec::new();
    if let CborValue::Map(map) = value {
        let flows_key = CborValue::Text("flows".to_string());
        if let Some(CborValue::Array(entries)) = map.get(&flows_key) {
            for entry in entries {
                if let Some(flow) = parse_flow_entry(entry) {
                    flows.push(flow);
                }
            }
        }
    }
    flows
}

fn parse_flow_entry(value: &CborValue) -> Option<AppFlowInfo> {
    let map = match value {
        CborValue::Map(map) => map,
        _ => return None,
    };
    let id = extract_text_from_map(map, "id")?;
    let kind = if let Some(flow_map) =
        map.get(&CborValue::Text("flow".to_string()))
            .and_then(|v| match v {
                CborValue::Map(flow_map) => Some(flow_map),
                _ => None,
            }) {
        extract_text_from_map(flow_map, "kind")
    } else {
        extract_text_from_map(map, "kind")
    };
    let kind = kind.unwrap_or_else(|| "messaging".to_string());
    Some(AppFlowInfo { id, kind })
}

fn extract_text_from_map(map: &BTreeMap<CborValue, CborValue>, key: &str) -> Option<String> {
    map.get(&CborValue::Text(key.to_string()))
        .and_then(|value| match value {
            CborValue::Text(text) => Some(text.clone()),
            _ => None,
        })
}

fn collect_transcript_outputs(
    run_dir: &Path,
    target_node_id: Option<&str>,
) -> Result<Option<JsonValue>> {
    let path = run_dir.join("transcript.jsonl");
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path)?;
    let mut first = None;
    let mut targeted = None;
    for line in contents.lines() {
        if let Ok(value) = serde_json::from_str::<JsonValue>(line)
            && let Some(outputs) = value.get("outputs")
            && !outputs.is_null()
        {
            if first.is_none() {
                first = Some(outputs.clone());
            }
            // If targeting a specific card node, check node_id match.
            if let Some(target) = target_node_id {
                if let Some(node_id) = value.get("node_id").and_then(|n| n.as_str()) {
                    if node_id == target {
                        targeted = Some(outputs.clone());
                    }
                }
            }
        }
    }
    // Targeted node takes priority; otherwise return first card (not last).
    Ok(targeted.or(first))
}

fn parse_envelopes(
    value: &JsonValue,
    ingress_envelope: &ChannelMessageEnvelope,
) -> Result<Vec<ChannelMessageEnvelope>> {
    if let Some(v) = value.as_array() {
        return parse_envelope_array(v);
    }
    if let Some(events) = value.get("events").and_then(|v| v.as_array()) {
        return parse_envelope_array(events);
    }
    if let Some(envelope) = value.get("message") {
        let envelope: ChannelMessageEnvelope = serde_json::from_value(envelope.clone())
            .context("app flow message payload is not a ChannelMessageEnvelope")?;
        return Ok(vec![envelope]);
    }
    // Handle component-adaptive-card output: AdaptiveCardResult with renderedCard.
    // Store the rendered AC JSON in metadata["adaptive_card"] (matches Teams/Slack pattern).
    if let Some(rendered_card) = value.get("renderedCard") {
        if !rendered_card.is_null() {
            let mut reply = ingress_envelope.clone();
            // Extract a brief title from the first body element for text fallback.
            let title = rendered_card
                .get("body")
                .and_then(|b| b.as_array())
                .and_then(|arr| arr.first())
                .and_then(|e| e.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("Adaptive Card");
            reply.text = Some(title.to_string());
            if let Ok(ac_json) = serde_json::to_string(rendered_card) {
                reply.metadata.insert("adaptive_card".to_string(), ac_json);
            }
            return Ok(vec![reply]);
        }
    }

    // Fallback: wrap simple text output in a reply envelope based on the ingress message.
    // Check payload.text (runner transcript format) then text then raw string.
    if let Some(text) = value
        .get("payload")
        .and_then(|p| p.get("text"))
        .and_then(JsonValue::as_str)
        .or_else(|| value.get("text").and_then(JsonValue::as_str))
        .or_else(|| value.as_str())
    {
        let mut reply = ingress_envelope.clone();
        reply.text = Some(text.to_string());
        return Ok(vec![reply]);
    }
    Err(anyhow::anyhow!(
        "app flow output did not produce envelope(s)"
    ))
}

fn parse_envelope_array(array: &[JsonValue]) -> Result<Vec<ChannelMessageEnvelope>> {
    let mut envelopes = Vec::new();
    for element in array {
        let envelope: ChannelMessageEnvelope = serde_json::from_value(element.clone())
            .context("app flow output array contains invalid channel envelope")?;
        envelopes.push(envelope);
    }
    Ok(envelopes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolve_app_pack_paths_priority() -> Result<()> {
        let dir = tempdir()?;
        let bundle = dir.path().join("bundle");
        let packs = bundle.join("packs");
        std::fs::create_dir_all(packs.join("tenant").join("team"))?;
        std::fs::create_dir_all(packs.join("tenant"))?;
        std::fs::create_dir_all(&packs)?;
        let team_file = packs.join("tenant").join("team").join("default.gtpack");
        std::fs::write(&team_file, "team")?;
        let tenant_file = packs.join("tenant").join("default.gtpack");
        std::fs::write(&tenant_file, "tenant")?;
        let default_file = packs.join("default.gtpack");
        std::fs::write(&default_file, "default")?;

        let resolved = resolve_app_pack_path(&bundle, "tenant", Some("team"), None)?;
        assert_eq!(resolved, team_file);

        let resolved = resolve_app_pack_path(&bundle, "tenant", Some("nope"), None)?;
        assert_eq!(resolved, tenant_file);

        std::fs::remove_file(&default_file)?;
        let resolved = resolve_app_pack_path(&bundle, "missing", None, None);
        assert!(resolved.is_err());
        Ok(())
    }

    #[test]
    fn select_app_flow_default_precedence() {
        let info = AppPackInfo {
            pack_id: "id".to_string(),
            flows: vec![
                AppFlowInfo {
                    id: "alpha".to_string(),
                    kind: "messaging".to_string(),
                },
                AppFlowInfo {
                    id: "default".to_string(),
                    kind: "messaging".to_string(),
                },
            ],
        };
        let flow = select_app_flow(&info).unwrap();
        assert_eq!(flow.id, "default");
    }

    fn test_envelope() -> ChannelMessageEnvelope {
        serde_json::from_value(json!({
            "id": "test-1",
            "tenant": {"env": "dev", "tenant": "t", "tenant_id": "t", "attempt": 0},
            "channel": "telegram",
            "session_id": "s1",
            "text": "hello"
        }))
        .unwrap()
    }

    #[test]
    fn parse_envelopes_payload_text_fallback() {
        let ingress = test_envelope();
        // Runner transcript format: {"payload": {"text": "..."}, "control": {...}}
        let output = json!({
            "control": {"routing": "out"},
            "payload": {"text": "Echo: received your message"},
            "state_updates": {}
        });
        let result = parse_envelopes(&output, &ingress).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].text.as_deref(),
            Some("Echo: received your message")
        );
    }

    #[test]
    fn parse_envelopes_direct_text_fallback() {
        let ingress = test_envelope();
        let output = json!({"text": "simple reply"});
        let result = parse_envelopes(&output, &ingress).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].text.as_deref(), Some("simple reply"));
    }

    #[test]
    fn select_app_flow_single_messaging() {
        let info = AppPackInfo {
            pack_id: "id".to_string(),
            flows: vec![AppFlowInfo {
                id: "single".to_string(),
                kind: "messaging".to_string(),
            }],
        };
        let flow = select_app_flow(&info).unwrap();
        assert_eq!(flow.id, "single");
    }
}
