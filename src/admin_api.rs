//! Admin API handler with mTLS support for setup/update/remove operations.
//!
//! Uses types from `greentic_setup::admin` for request/response structures.
//! Provides HTTP endpoints for bundle lifecycle management.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use greentic_setup::admin::routes::{
    AdminResponse, BundleDeployRequest, BundleRemoveRequest, BundleStatus, BundleStatusResponse,
};
use greentic_setup::admin::tls::AdminTlsConfig;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value, json};

/// Shared state for the admin API handler.
pub struct AdminState {
    pub tls_config: AdminTlsConfig,
    pub bundle_root: PathBuf,
    pub tenant: String,
    pub team: Option<String>,
    pub env: String,
}

/// Handle an incoming admin API request.
///
/// Routes:
/// - `GET  /admin/status`          → bundle deployment status
/// - `POST /admin/deploy`          → deploy/update a bundle
/// - `POST /admin/remove`          → remove bundle components
/// - `POST /admin/qa/spec`         → get QA form spec for a provider
/// - `POST /admin/qa/validate`     → validate QA answers
/// - `POST /admin/qa/submit`       → submit QA answers and persist
pub async fn handle_admin_request(
    req: Request<Incoming>,
    path: &str,
    state: &Arc<AdminState>,
) -> Result<Response<Full<Bytes>>> {
    let method = req.method().clone();
    let sub_path = path
        .strip_prefix("/admin")
        .unwrap_or("")
        .trim_end_matches('/');

    match (method, sub_path) {
        (Method::GET, "/status") => handle_status(state).await,
        (Method::POST, "/deploy") => handle_deploy(req, state).await,
        (Method::POST, "/remove") => handle_remove(req, state).await,
        (Method::POST, "/qa/spec") => handle_qa_spec(req, state).await,
        (Method::POST, "/qa/validate") => handle_qa_validate(req, state).await,
        (Method::POST, "/qa/submit") => handle_qa_submit(req, state).await,
        _ => json_response(
            StatusCode::NOT_FOUND,
            &AdminResponse::<()>::err("not found"),
        ),
    }
}

async fn handle_status(state: &Arc<AdminState>) -> Result<Response<Full<Bytes>>> {
    let bundle_exists = state.bundle_root.exists();
    let providers_dir = state.bundle_root.join("providers");

    let mut provider_count = 0usize;
    if providers_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&providers_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if !name.starts_with('_') && !name.starts_with('.') {
                            provider_count += 1;
                        }
                    }
                }
            }
        }
    }

    let status = BundleStatusResponse {
        status: if bundle_exists {
            BundleStatus::Active
        } else {
            BundleStatus::Error
        },
        bundle_path: state.bundle_root.clone(),
        pack_count: 0,
        tenant_count: 1,
        provider_count,
    };

    json_response(StatusCode::OK, &AdminResponse::ok(status))
}

async fn handle_deploy(
    req: Request<Incoming>,
    state: &Arc<AdminState>,
) -> Result<Response<Full<Bytes>>> {
    let body = read_body(req).await?;
    let request: BundleDeployRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &AdminResponse::<()>::err(format!("invalid request: {e}")),
            );
        }
    };

    // Use greentic-setup's SetupEngine for plan building
    let engine = greentic_setup::SetupEngine::new(greentic_setup::engine::SetupConfig {
        tenant: state.tenant.clone(),
        team: state.team.clone(),
        env: state.env.clone(),
        offline: false,
        verbose: false,
    });

    let setup_request = greentic_setup::engine::SetupRequest {
        bundle: request.bundle_path,
        bundle_name: request.bundle_name,
        pack_refs: request.pack_refs,
        tenants: request.tenants,
        ..Default::default()
    };

    let mode = if state.bundle_root.exists() {
        greentic_setup::SetupMode::Update
    } else {
        greentic_setup::SetupMode::Create
    };

    match engine.plan(mode, &setup_request, false) {
        Ok(plan) => {
            let summary = json!({
                "mode": format!("{mode:?}"),
                "steps": plan.steps.len(),
                "bundle": state.bundle_root.display().to_string(),
            });
            json_response(StatusCode::OK, &AdminResponse::ok(summary))
        }
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &AdminResponse::<()>::err(e.to_string()),
        ),
    }
}

async fn handle_remove(
    req: Request<Incoming>,
    state: &Arc<AdminState>,
) -> Result<Response<Full<Bytes>>> {
    let body = read_body(req).await?;
    let request: BundleRemoveRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &AdminResponse::<()>::err(format!("invalid request: {e}")),
            );
        }
    };

    if !state.bundle_root.exists() {
        return json_response(
            StatusCode::NOT_FOUND,
            &AdminResponse::<()>::err("bundle not found"),
        );
    }

    let engine = greentic_setup::SetupEngine::new(greentic_setup::engine::SetupConfig {
        tenant: state.tenant.clone(),
        team: state.team.clone(),
        env: state.env.clone(),
        offline: true,
        verbose: false,
    });

    let setup_request = greentic_setup::engine::SetupRequest {
        bundle: state.bundle_root.clone(),
        ..Default::default()
    };

    match engine.plan(greentic_setup::SetupMode::Remove, &setup_request, false) {
        Ok(plan) => {
            let _ = &request; // consumed for future use
            let summary = json!({
                "mode": "remove",
                "steps": plan.steps.len(),
            });
            json_response(StatusCode::OK, &AdminResponse::ok(summary))
        }
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &AdminResponse::<()>::err(e.to_string()),
        ),
    }
}

async fn handle_qa_spec(
    req: Request<Incoming>,
    state: &Arc<AdminState>,
) -> Result<Response<Full<Bytes>>> {
    let body = read_body(req).await?;
    let request: Value = serde_json::from_slice(&body).unwrap_or_default();
    let provider_id = request
        .get("provider_id")
        .and_then(Value::as_str)
        .unwrap_or("");

    if provider_id.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &AdminResponse::<()>::err("provider_id required"),
        );
    }

    // Try to find the pack and build a FormSpec
    let providers_dir = state.bundle_root.join("providers");
    let pack_path = find_provider_pack(&providers_dir, provider_id);

    match pack_path {
        Some(path) => {
            let form_spec =
                greentic_setup::setup_to_formspec::pack_to_form_spec(&path, provider_id);
            match form_spec {
                Some(spec) => {
                    let json = serde_json::to_value(&spec).unwrap_or_default();
                    json_response(StatusCode::OK, &AdminResponse::ok(json))
                }
                None => json_response(
                    StatusCode::NOT_FOUND,
                    &AdminResponse::<()>::err("no setup spec found for provider"),
                ),
            }
        }
        None => json_response(
            StatusCode::NOT_FOUND,
            &AdminResponse::<()>::err(format!("provider pack not found: {provider_id}")),
        ),
    }
}

async fn handle_qa_validate(
    req: Request<Incoming>,
    state: &Arc<AdminState>,
) -> Result<Response<Full<Bytes>>> {
    let body = read_body(req).await?;
    let request: Value = serde_json::from_slice(&body).unwrap_or_default();
    let provider_id = request
        .get("provider_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let answers = request.get("answers").cloned().unwrap_or_default();

    let providers_dir = state.bundle_root.join("providers");
    let pack_path = find_provider_pack(&providers_dir, provider_id);

    match pack_path {
        Some(path) => {
            let form_spec =
                greentic_setup::setup_to_formspec::pack_to_form_spec(&path, provider_id);
            match form_spec {
                Some(spec) => {
                    match greentic_setup::qa::wizard::validate_answers_against_form_spec(
                        &spec, &answers,
                    ) {
                        Ok(()) => json_response(
                            StatusCode::OK,
                            &AdminResponse::ok(json!({"valid": true})),
                        ),
                        Err(e) => json_response(
                            StatusCode::OK,
                            &AdminResponse::ok(json!({"valid": false, "error": e.to_string()})),
                        ),
                    }
                }
                None => json_response(
                    StatusCode::OK,
                    &AdminResponse::ok(json!({"valid": true, "note": "no spec found"})),
                ),
            }
        }
        None => json_response(
            StatusCode::NOT_FOUND,
            &AdminResponse::<()>::err(format!("provider not found: {provider_id}")),
        ),
    }
}

async fn handle_qa_submit(
    req: Request<Incoming>,
    state: &Arc<AdminState>,
) -> Result<Response<Full<Bytes>>> {
    let body = read_body(req).await?;
    let request: Value = serde_json::from_slice(&body).unwrap_or_default();
    let provider_id = request
        .get("provider_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let answers = request.get("answers").cloned().unwrap_or_default();

    if provider_id.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &AdminResponse::<()>::err("provider_id required"),
        );
    }

    // Persist answers as secrets
    let persisted = crate::qa_persist::persist_all_config_as_secrets(
        &state.bundle_root,
        &state.env,
        &state.tenant,
        state.team.as_deref(),
        &provider_id,
        &answers,
        None,
    )
    .await;

    match persisted {
        Ok(keys) => json_response(
            StatusCode::OK,
            &AdminResponse::ok(json!({
                "persisted_keys": keys,
                "provider_id": provider_id,
            })),
        ),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &AdminResponse::<()>::err(e.to_string()),
        ),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn find_provider_pack(providers_dir: &std::path::Path, provider_id: &str) -> Option<PathBuf> {
    // Check common pack locations
    for dir_name in &["messaging", "events", "oauth", "secrets", "mcp"] {
        let pack = providers_dir
            .join(dir_name)
            .join(format!("{provider_id}.gtpack"));
        if pack.exists() {
            return Some(pack);
        }
    }
    // Check flat layout
    let flat = providers_dir.join(format!("{provider_id}.gtpack"));
    if flat.exists() {
        return Some(flat);
    }
    None
}

fn json_response<T: serde::Serialize>(
    status: StatusCode,
    body: &T,
) -> Result<Response<Full<Bytes>>> {
    let json = serde_json::to_vec(body)?;
    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(json)))
        .unwrap())
}

async fn read_body(req: Request<Incoming>) -> Result<Vec<u8>> {
    use http_body_util::BodyExt;
    let body = req.into_body().collect().await?.to_bytes();
    Ok(body.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_provider_pack_returns_none_for_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_provider_pack(tmp.path(), "nonexistent").is_none());
    }

    #[test]
    fn find_provider_pack_finds_flat_pack() {
        let tmp = tempfile::tempdir().unwrap();
        let pack = tmp.path().join("messaging-telegram.gtpack");
        std::fs::write(&pack, "pack").unwrap();
        assert_eq!(
            find_provider_pack(tmp.path(), "messaging-telegram"),
            Some(pack)
        );
    }
}
