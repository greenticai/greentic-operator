use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::domains::Domain;

pub fn run_dir(
    root: &Path,
    domain: Domain,
    pack_label: &str,
    flow_id: &str,
) -> anyhow::Result<PathBuf> {
    let timestamp = timestamp_secs()?;
    let domain_name = domain_name(domain);
    Ok(root
        .join("state")
        .join("runs")
        .join(domain_name)
        .join(pack_label)
        .join(flow_id)
        .join(format!("{timestamp}")))
}

pub fn runtime_run_dir(
    root: &Path,
    runtime_bundle_id: &str,
    domain: Domain,
    pack_label: &str,
    flow_id: &str,
) -> anyhow::Result<PathBuf> {
    let timestamp = timestamp_secs()?;
    let domain_name = domain_name(domain);
    Ok(root
        .join("state")
        .join("runtime")
        .join("bundles")
        .join(sanitize_path_segment(runtime_bundle_id))
        .join("runs")
        .join(domain_name)
        .join(pack_label)
        .join(flow_id)
        .join(format!("{timestamp}")))
}

pub fn runtime_bundle_state_root(root: &Path, runtime_bundle_id: &str) -> PathBuf {
    root.join("state")
        .join("runtime")
        .join("bundles")
        .join(sanitize_path_segment(runtime_bundle_id))
}

pub fn secrets_log_path(root: &Path, action: &str) -> anyhow::Result<PathBuf> {
    let timestamp = timestamp_secs()?;
    Ok(root
        .join("state")
        .join("logs")
        .join("secrets")
        .join(format!("{action}-{timestamp}.log")))
}

fn domain_name(domain: Domain) -> &'static str {
    match domain {
        Domain::Messaging => "messaging",
        Domain::Events => "events",
        Domain::Secrets => "secrets",
        Domain::OAuth => "oauth",
    }
}

fn timestamp_secs() -> anyhow::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| anyhow::anyhow!("timestamp error: {err}"))?
        .as_secs())
}

pub fn sanitize_path_segment(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    if sanitized.is_empty() {
        "_".to_string()
    } else {
        sanitized
    }
}
