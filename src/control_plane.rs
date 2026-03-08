use std::{path::PathBuf, sync::Arc};

use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    header::CONTENT_TYPE,
};
use serde_json::{Value, json};

use crate::demo::runner_host::DemoRunnerHost;
use crate::runtime_core::{AdminAction, AuthorizationDecision};

pub type ControlPlaneResponse = Response<Full<Bytes>>;

pub async fn handle_control_plane_http_request(
    req: Request<Incoming>,
    path: &str,
    runner_host: &Arc<DemoRunnerHost>,
) -> ControlPlaneResponse {
    let method = req.method().clone();
    match read_json_body(req).await {
        Ok(body) => handle_control_plane_request(method, path, body, runner_host),
        Err(response) => response,
    }
}

pub fn handle_control_plane_request(
    method: Method,
    path: &str,
    body: Value,
    runner_host: &Arc<DemoRunnerHost>,
) -> ControlPlaneResponse {
    match (method.clone(), path) {
        (Method::GET, "/healthz") => json_response(StatusCode::OK, runner_host.healthz_snapshot()),
        (Method::GET, "/readyz") => {
            let (ready, payload) = runner_host.readyz_snapshot();
            json_response(
                if ready {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                },
                payload,
            )
        }
        _ => {
            if let Some(response) = authorize_control_plane(path, &method, runner_host) {
                return response;
            }
            dispatch_control_plane_request(method, path, body, runner_host)
        }
    }
}

fn authorize_control_plane(
    path: &str,
    method: &Method,
    runner_host: &Arc<DemoRunnerHost>,
) -> Option<ControlPlaneResponse> {
    let action = AdminAction {
        action: format!(
            "control.{}.{}",
            method.as_str().to_ascii_lowercase(),
            path.trim_end_matches('/')
        ),
        actor: "control_plane".to_string(),
        resource: Some(path.to_string()),
    };
    match runner_host.authorize_admin_action(action) {
        Ok(AuthorizationDecision::Allow) => None,
        Ok(AuthorizationDecision::Deny { reason }) => {
            Some(error_response(StatusCode::FORBIDDEN, reason))
        }
        Err(err) => Some(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            err.to_string(),
        )),
    }
}

fn dispatch_control_plane_request(
    method: Method,
    path: &str,
    body: Value,
    runner_host: &Arc<DemoRunnerHost>,
) -> ControlPlaneResponse {
    match (method, path) {
        (Method::GET, "/status") => {
            json_response(StatusCode::OK, runner_host.control_plane_snapshot())
        }
        (Method::POST, "/runtime/drain") => {
            if let Some(response) = ensure_leader(runner_host) {
                response
            } else {
                json_response(StatusCode::OK, runner_host.set_runtime_draining(true))
            }
        }
        (Method::POST, "/runtime/resume") => {
            if let Some(response) = ensure_leader(runner_host) {
                response
            } else {
                json_response(StatusCode::OK, runner_host.set_runtime_draining(false))
            }
        }
        (Method::POST, "/runtime/shutdown") => {
            if let Some(response) = ensure_leader(runner_host) {
                response
            } else {
                json_response(StatusCode::ACCEPTED, runner_host.request_runtime_shutdown())
            }
        }
        (Method::POST, "/deployments/stage") => {
            if let Some(response) = ensure_leader(runner_host) {
                response
            } else if let Some(response) = ensure_request_policy(
                runner_host,
                "control_plane.deployments.stage",
                &["bundle_source", "bundle_resolver", "bundle_fs"],
                3,
            ) {
                response
            } else {
                match deployment_bundle_ref("stage", &body) {
                    Ok((bundle_ref, payload)) => {
                        let _ = runner_host.record_deployment_request("stage", payload.clone());
                        match runner_host.stage_bundle_ref(&bundle_ref) {
                            Ok(bundle_id) => json_response(
                                StatusCode::OK,
                                json!({
                                    "ok": true,
                                    "applied": true,
                                    "action": "stage",
                                    "bundle_id": bundle_id,
                                    "state": "staged",
                                    "bundle_ref": payload.get("bundle_ref").cloned().unwrap_or(Value::Null),
                                    "lifecycle": runner_host.control_plane_snapshot().pointer("/runtime/bundle/lifecycle").cloned().unwrap_or(Value::Null),
                                }),
                            ),
                            Err(err) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
                        }
                    }
                    Err(message) => error_response(StatusCode::BAD_REQUEST, message),
                }
            }
        }
        (Method::POST, "/deployments/warm") => {
            if let Some(response) = ensure_leader(runner_host) {
                response
            } else if let Some(response) = ensure_request_policy(
                runner_host,
                "control_plane.deployments.warm",
                &["bundle_source", "bundle_resolver", "bundle_fs"],
                3,
            ) {
                response
            } else {
                match deployment_target("warm", &body) {
                    Ok(DeploymentTarget::BundleRef(bundle_ref, payload)) => {
                        let _ = runner_host.record_deployment_request("warm", payload.clone());
                        match runner_host.warm_bundle_ref(&bundle_ref) {
                            Ok(bundle_id) => json_response(
                                StatusCode::OK,
                                json!({
                                    "ok": true,
                                    "applied": true,
                                    "action": "warm",
                                    "bundle_id": bundle_id,
                                    "bundle_ref": payload.get("bundle_ref").cloned().unwrap_or(Value::Null),
                                    "lifecycle": runner_host.control_plane_snapshot().pointer("/runtime/bundle/lifecycle").cloned().unwrap_or(Value::Null),
                                }),
                            ),
                            Err(err) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
                        }
                    }
                    Ok(DeploymentTarget::BundleId(bundle_id, payload)) => {
                        let _ = runner_host.record_deployment_request("warm", payload.clone());
                        match runner_host.warm_bundle_id(&bundle_id) {
                            Ok(bundle_id) => json_response(
                                StatusCode::OK,
                                json!({
                                    "ok": true,
                                    "applied": true,
                                    "action": "warm",
                                    "bundle_id": bundle_id,
                                    "lifecycle": runner_host.control_plane_snapshot().pointer("/runtime/bundle/lifecycle").cloned().unwrap_or(Value::Null),
                                }),
                            ),
                            Err(err) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
                        }
                    }
                    Err(message) => error_response(StatusCode::BAD_REQUEST, message),
                }
            }
        }
        (Method::POST, "/deployments/activate") => {
            if let Some(response) = ensure_leader(runner_host) {
                response
            } else if let Some(response) = ensure_request_policy(
                runner_host,
                "control_plane.deployments.activate",
                &["bundle_source", "bundle_resolver", "bundle_fs"],
                3,
            ) {
                response
            } else {
                match deployment_target("activate", &body) {
                    Ok(DeploymentTarget::BundleRef(bundle_ref, payload)) => {
                        let _ = runner_host.record_deployment_request("activate", payload.clone());
                        match runner_host.activate_bundle_ref(&bundle_ref) {
                            Ok(bundle_id) => json_response(
                                StatusCode::OK,
                                json!({
                                    "ok": true,
                                    "applied": true,
                                    "action": "activate",
                                    "bundle_id": bundle_id,
                                    "bundle_ref": payload.get("bundle_ref").cloned().unwrap_or(Value::Null),
                                    "lifecycle": runner_host.control_plane_snapshot().pointer("/runtime/bundle/lifecycle").cloned().unwrap_or(Value::Null),
                                }),
                            ),
                            Err(err) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
                        }
                    }
                    Ok(DeploymentTarget::BundleId(bundle_id, payload)) => {
                        let _ = runner_host.record_deployment_request("activate", payload.clone());
                        match runner_host.activate_bundle_id(&bundle_id) {
                            Ok(bundle_id) => json_response(
                                StatusCode::OK,
                                json!({
                                    "ok": true,
                                    "applied": true,
                                    "action": "activate",
                                    "bundle_id": bundle_id,
                                    "lifecycle": runner_host.control_plane_snapshot().pointer("/runtime/bundle/lifecycle").cloned().unwrap_or(Value::Null),
                                }),
                            ),
                            Err(err) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
                        }
                    }
                    Err(message) => error_response(StatusCode::BAD_REQUEST, message),
                }
            }
        }
        (Method::POST, "/deployments/rollback") => {
            if let Some(response) = ensure_leader(runner_host) {
                response
            } else if let Some(response) = ensure_request_policy(
                runner_host,
                "control_plane.deployments.rollback",
                &["bundle_source", "bundle_resolver", "bundle_fs"],
                3,
            ) {
                response
            } else {
                let _ = runner_host.record_deployment_request("rollback", body.clone());
                match runner_host.rollback_active_bundle() {
                    Ok(()) => json_response(
                        StatusCode::OK,
                        json!({
                            "ok": true,
                            "applied": true,
                            "action": "rollback",
                            "lifecycle": runner_host.control_plane_snapshot().pointer("/runtime/bundle/lifecycle").cloned().unwrap_or(Value::Null),
                        }),
                    ),
                    Err(err) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
                }
            }
        }
        (Method::POST, "/deployments/complete-drain") => {
            if let Some(response) = ensure_leader(runner_host) {
                response
            } else if let Some(response) = ensure_request_policy(
                runner_host,
                "control_plane.deployments.complete_drain",
                &["bundle_source", "bundle_resolver", "bundle_fs"],
                3,
            ) {
                response
            } else {
                let Some(bundle_id) = body
                    .get("bundle_id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                else {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "complete-drain request requires non-empty bundle_id",
                    );
                };
                let _ = runner_host.record_deployment_request("complete-drain", body.clone());
                match runner_host.complete_bundle_drain(bundle_id) {
                    Ok(()) => json_response(
                        StatusCode::OK,
                        json!({
                            "ok": true,
                            "applied": true,
                            "action": "complete-drain",
                            "bundle_id": bundle_id,
                            "lifecycle": runner_host.control_plane_snapshot().pointer("/runtime/bundle/lifecycle").cloned().unwrap_or(Value::Null),
                        }),
                    ),
                    Err(err) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
                }
            }
        }
        (Method::POST, "/config/publish") => {
            if let Some(response) = ensure_leader(runner_host) {
                response
            } else if let Some(response) =
                ensure_request_policy(runner_host, "control_plane.config.publish", &["state"], 2)
            {
                response
            } else {
                match runner_host.apply_config_publish(body) {
                    Ok(payload) => json_response(StatusCode::OK, payload),
                    Err(err) => error_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
                }
            }
        }
        (Method::POST, "/cache/invalidate") => {
            if let Some(response) = ensure_leader(runner_host) {
                response
            } else if let Some(response) =
                ensure_request_policy(runner_host, "control_plane.cache.invalidate", &["state"], 2)
            {
                response
            } else {
                match runner_host.apply_cache_invalidate(body) {
                    Ok(payload) => json_response(StatusCode::OK, payload),
                    Err(err) => error_response(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
                }
            }
        }
        (Method::GET, "/observability/log-level") => json_response(
            StatusCode::OK,
            json!({
                "log_level": runner_host
                    .control_plane_snapshot()
                    .get("log_level")
                    .cloned()
                    .unwrap_or(Value::Null),
            }),
        ),
        (Method::POST, "/observability/log-level") => {
            if let Some(response) = ensure_leader(runner_host) {
                response
            } else {
                let Some(level) = body.get("level").and_then(Value::as_str) else {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "observability/log-level requires string field `level`",
                    );
                };
                match runner_host.apply_log_level(level) {
                    Ok(payload) => json_response(StatusCode::OK, payload),
                    Err(err) => error_response(StatusCode::BAD_REQUEST, err.to_string()),
                }
            }
        }
        _ => error_response(
            StatusCode::NOT_FOUND,
            format!("unknown control-plane endpoint: {path}"),
        ),
    }
}

fn deployment_bundle_ref(action: &str, body: &Value) -> Result<(PathBuf, Value), String> {
    let bundle_ref = body
        .get("bundle_ref")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{action} request requires non-empty bundle_ref"))?;
    let bundle_path = PathBuf::from(bundle_ref);
    if !bundle_path.exists() {
        return Err(format!("bundle_ref does not exist: {bundle_ref}"));
    }
    Ok((
        bundle_path.clone(),
        json!({
            "bundle_ref": bundle_ref,
            "resolved_bundle_ref": bundle_path.display().to_string(),
            "request": body,
        }),
    ))
}

enum DeploymentTarget {
    BundleRef(PathBuf, Value),
    BundleId(String, Value),
}

fn deployment_target(action: &str, body: &Value) -> Result<DeploymentTarget, String> {
    if let Some(bundle_id) = body
        .get("bundle_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(DeploymentTarget::BundleId(
            bundle_id.to_string(),
            json!({
                "bundle_id": bundle_id,
                "request": body,
            }),
        ));
    }
    deployment_bundle_ref(action, body)
        .map(|(path, payload)| DeploymentTarget::BundleRef(path, payload))
}

fn ensure_leader(runner_host: &Arc<DemoRunnerHost>) -> Option<ControlPlaneResponse> {
    if runner_host.is_leader() {
        return None;
    }
    Some(json_response(
        StatusCode::CONFLICT,
        json!({
            "ok": false,
            "code": "not_leader",
            "message": "mutating control-plane operation requires leader role",
        }),
    ))
}

fn ensure_request_policy(
    runner_host: &Arc<DemoRunnerHost>,
    request_class: &str,
    required_provider_classes: &[&'static str],
    max_degraded_level: u8,
) -> Option<ControlPlaneResponse> {
    match runner_host.enforce_request_policy(
        request_class,
        required_provider_classes,
        max_degraded_level,
    ) {
        Ok(()) => None,
        Err(refusal) => Some(json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "ok": false,
                "code": refusal.code,
                "request_class": refusal.request_class,
                "message": refusal.message,
                "safe_mode": refusal.safe_mode,
                "degraded_level": refusal.degraded_level,
                "blocking_provider_classes": refusal.blocking_provider_classes,
            }),
        )),
    }
}

fn json_response(status: StatusCode, value: Value) -> ControlPlaneResponse {
    let body = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::from(Bytes::from(body)))
        .unwrap_or_else(|err| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Full::from(Bytes::from(format!(
                    "failed to build response: {err}"
                ))))
                .unwrap()
        })
}

fn error_response(status: StatusCode, message: impl Into<String>) -> ControlPlaneResponse {
    json_response(
        status,
        json!({
            "success": false,
            "message": message.into()
        }),
    )
}

async fn read_json_body(req: Request<Incoming>) -> Result<Value, ControlPlaneResponse> {
    let payload_bytes = req
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .map_err(|err| error_response(StatusCode::BAD_REQUEST, format!("read body: {err}")))?;

    if payload_bytes.is_empty() {
        return Ok(json!({}));
    }

    serde_json::from_slice(&payload_bytes)
        .map_err(|err| error_response(StatusCode::BAD_REQUEST, format!("invalid JSON: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::demo::runner_host::DemoRunnerHost;
    use crate::runtime_core::{
        AdminAction, AdminAuthorizationHook, AuthorizationDecision, RuntimeCore, RuntimeHealth,
        RuntimeHealthStatus, ScopedStateKey, StateProvider,
    };

    struct DenyAdminHook;

    #[async_trait]
    impl AdminAuthorizationHook for DenyAdminHook {
        async fn authorize(&self, _action: &AdminAction) -> anyhow::Result<AuthorizationDecision> {
            Ok(AuthorizationDecision::Deny {
                reason: "forbidden".to_string(),
            })
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Available,
                reason: None,
            })
        }
    }

    struct UnavailableStateProvider;

    #[async_trait]
    impl StateProvider for UnavailableStateProvider {
        async fn get(&self, _key: &ScopedStateKey) -> anyhow::Result<Option<Value>> {
            Ok(None)
        }

        async fn put(&self, _key: &ScopedStateKey, _value: Value) -> anyhow::Result<()> {
            Ok(())
        }

        async fn delete(&self, _key: &ScopedStateKey) -> anyhow::Result<()> {
            Ok(())
        }

        async fn health(&self) -> anyhow::Result<RuntimeHealth> {
            Ok(RuntimeHealth {
                status: RuntimeHealthStatus::Unavailable,
                reason: Some("state provider offline".to_string()),
            })
        }
    }

    fn test_host() -> Arc<DemoRunnerHost> {
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
        Arc::new(
            DemoRunnerHost::new(
                tmp.path().to_path_buf(),
                &discovery,
                None,
                secrets_handle,
                false,
            )
            .expect("build host"),
        )
    }

    fn response_json(response: ControlPlaneResponse) -> Value {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let bytes = runtime
            .block_on(response.into_body().collect())
            .expect("collect body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("json body")
    }

    #[test]
    fn healthz_and_readyz_contracts() {
        let host = test_host();
        let health = handle_control_plane_request(Method::GET, "/healthz", json!({}), &host);
        assert_eq!(health.status(), StatusCode::OK);
        assert_eq!(
            response_json(health).get("ok").and_then(Value::as_bool),
            Some(true)
        );

        let ready = handle_control_plane_request(Method::GET, "/readyz", json!({}), &host);
        assert_eq!(ready.status(), StatusCode::OK);
        assert_eq!(
            response_json(ready).get("ready").and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn status_payload_includes_control_plane_fields() {
        let host = test_host();
        let response = handle_control_plane_request(Method::GET, "/status", json!({}), &host);
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response);
        assert!(body.get("node_id").and_then(Value::as_str).is_some());
        assert!(
            body.get("start_time_unix_ms")
                .and_then(Value::as_u64)
                .is_some()
        );
        assert!(body.get("active_bundle_id").is_some());
        assert!(body.get("active_bundle_access_mode").is_some());
        assert!(body.get("runtime").is_some());
    }

    #[test]
    fn control_plane_auth_hook_denies_mutations() {
        let host = test_host();
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.admin_authorization_hook = Some(Arc::new(DenyAdminHook));
        host.replace_runtime_core_for_test(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let response =
            handle_control_plane_request(Method::POST, "/runtime/drain", json!({}), &host);
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response_json(response)
                .get("message")
                .and_then(Value::as_str),
            Some("forbidden")
        );
    }

    #[test]
    fn control_plane_mutations_require_leader() {
        let host = test_host();
        host.set_is_leader_for_test(false);
        let response =
            handle_control_plane_request(Method::POST, "/runtime/drain", json!({}), &host);
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response_json(response).get("code").and_then(Value::as_str),
            Some("not_leader")
        );
    }

    #[test]
    fn drain_and_resume_update_ready_state() {
        let host = test_host();
        let drain = handle_control_plane_request(Method::POST, "/runtime/drain", json!({}), &host);
        assert_eq!(drain.status(), StatusCode::OK);
        assert_eq!(
            response_json(drain)
                .get("draining")
                .and_then(Value::as_bool),
            Some(true)
        );

        let ready = handle_control_plane_request(Method::GET, "/readyz", json!({}), &host);
        assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);

        let resume =
            handle_control_plane_request(Method::POST, "/runtime/resume", json!({}), &host);
        assert_eq!(resume.status(), StatusCode::OK);
        assert_eq!(
            response_json(resume)
                .get("draining")
                .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn minimal_admin_actions_remain_allowed_in_safe_mode() {
        let host = test_host();
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.state_provider = Some(Arc::new(UnavailableStateProvider));
        host.replace_runtime_core_for_test(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let ready = handle_control_plane_request(Method::GET, "/readyz", json!({}), &host);
        assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);

        let drain = handle_control_plane_request(Method::POST, "/runtime/drain", json!({}), &host);
        assert_eq!(drain.status(), StatusCode::OK);
        assert_eq!(
            response_json(drain)
                .get("draining")
                .and_then(Value::as_bool),
            Some(true)
        );

        let shutdown =
            handle_control_plane_request(Method::POST, "/runtime/shutdown", json!({}), &host);
        assert_eq!(shutdown.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response_json(shutdown)
                .get("shutdown_requested")
                .and_then(Value::as_bool),
            Some(true)
        );

        let log_level = handle_control_plane_request(
            Method::POST,
            "/observability/log-level",
            json!({ "level": "debug" }),
            &host,
        );
        assert_eq!(log_level.status(), StatusCode::OK);
        assert_eq!(
            response_json(log_level)
                .get("log_level")
                .and_then(Value::as_str),
            Some("debug")
        );

        let config = handle_control_plane_request(
            Method::POST,
            "/config/publish",
            json!({ "revision": "r-safe" }),
            &host,
        );
        assert_eq!(config.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn shutdown_request_marks_control_state() {
        let host = test_host();
        let response =
            handle_control_plane_request(Method::POST, "/runtime/shutdown", json!({}), &host);
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = response_json(response);
        assert_eq!(
            body.get("shutdown_requested").and_then(Value::as_bool),
            Some(true)
        );

        let status = response_json(handle_control_plane_request(
            Method::GET,
            "/status",
            json!({}),
            &host,
        ));
        assert_eq!(
            status.get("shutdown_requested").and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn deployment_requests_validate_bundle_ref_and_record_request() {
        let host = test_host();
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing =
            handle_control_plane_request(Method::POST, "/deployments/stage", json!({}), &host);
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

        let stage = handle_control_plane_request(
            Method::POST,
            "/deployments/stage",
            json!({ "bundle_ref": tmp.path().display().to_string() }),
            &host,
        );
        assert_eq!(stage.status(), StatusCode::OK);
        let stage_body = response_json(stage);
        assert_eq!(
            stage_body.get("action").and_then(Value::as_str),
            Some("stage")
        );
        assert_eq!(
            stage_body.get("state").and_then(Value::as_str),
            Some("staged")
        );
        let staged_bundle_id = stage_body
            .get("bundle_id")
            .and_then(Value::as_str)
            .expect("staged bundle id")
            .to_string();

        let warm = handle_control_plane_request(
            Method::POST,
            "/deployments/warm",
            json!({ "bundle_id": staged_bundle_id }),
            &host,
        );
        assert_eq!(warm.status(), StatusCode::OK);
        let warm_body = response_json(warm);
        assert_eq!(
            warm_body.get("action").and_then(Value::as_str),
            Some("warm")
        );

        let response = handle_control_plane_request(
            Method::POST,
            "/deployments/activate",
            json!({ "bundle_id": warm_body.get("bundle_id").cloned().unwrap_or(Value::Null) }),
            &host,
        );
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response);
        assert_eq!(body.get("applied").and_then(Value::as_bool), Some(true));
        assert_eq!(body.get("action").and_then(Value::as_str), Some("activate"));

        let status = response_json(handle_control_plane_request(
            Method::GET,
            "/status",
            json!({}),
            &host,
        ));
        assert!(
            status
                .pointer("/runtime/bundle/lifecycle/bundles")
                .and_then(Value::as_array)
                .is_some_and(|bundles| bundles.iter().any(|bundle| {
                    bundle.get("state").and_then(Value::as_str) == Some("active")
                }))
        );
        assert_eq!(
            status
                .pointer("/last_deployment_request/action")
                .and_then(Value::as_str),
            Some("activate")
        );
    }

    #[test]
    fn deployment_requests_are_refused_when_bundle_access_providers_are_unavailable() {
        let host = test_host();
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.bundle_source = None;
        host.replace_runtime_core_for_test(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let tmp = tempfile::tempdir().expect("tempdir");
        let response = handle_control_plane_request(
            Method::POST,
            "/deployments/stage",
            json!({ "bundle_ref": tmp.path().display().to_string() }),
            &host,
        );
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response_json(response);
        assert_eq!(
            body.get("code").and_then(Value::as_str),
            Some("runtime_request_refused")
        );
        assert_eq!(
            body.get("request_class").and_then(Value::as_str),
            Some("control_plane.deployments.stage")
        );
        assert!(
            body.get("blocking_provider_classes")
                .and_then(Value::as_array)
                .is_some_and(|items| items
                    .iter()
                    .any(|item| item.as_str() == Some("bundle_source")))
        );
    }

    #[test]
    fn config_and_cache_requests_are_applied() {
        let host = test_host();
        let contract_cache = host.bundle_root().join("providers/messaging/_contracts");
        std::fs::create_dir_all(&contract_cache).expect("create contract cache");
        std::fs::write(contract_cache.join("a.contract.cbor"), b"test")
            .expect("write contract cache");
        let provider_registry_cache = host
            .bundle_root()
            .join(".greentic/cache/provider-registry/by-digest");
        std::fs::create_dir_all(&provider_registry_cache).expect("create provider registry cache");
        std::fs::write(provider_registry_cache.join("cached.json"), b"{}")
            .expect("write provider registry cache");

        let config = handle_control_plane_request(
            Method::POST,
            "/config/publish",
            json!({ "revision": "r1", "digest": "sha256:abc" }),
            &host,
        );
        assert_eq!(config.status(), StatusCode::OK);
        let config_body = response_json(config);
        assert_eq!(
            config_body.get("applied").and_then(Value::as_bool),
            Some(true)
        );
        let latest_path = PathBuf::from(
            config_body
                .get("latest_path")
                .and_then(Value::as_str)
                .expect("latest path"),
        );
        assert!(latest_path.exists());

        let cache = handle_control_plane_request(
            Method::POST,
            "/cache/invalidate",
            json!({ "scope": "all" }),
            &host,
        );
        assert_eq!(cache.status(), StatusCode::OK);
        let cache_body = response_json(cache);
        assert_eq!(
            cache_body.get("applied").and_then(Value::as_bool),
            Some(true)
        );
        assert!(
            cache_body
                .get("removed_paths")
                .and_then(Value::as_array)
                .is_some_and(|paths| !paths.is_empty())
        );
        assert!(!contract_cache.exists());
        assert!(
            !host
                .bundle_root()
                .join(".greentic/cache/provider-registry")
                .exists()
        );

        let status = response_json(handle_control_plane_request(
            Method::GET,
            "/status",
            json!({}),
            &host,
        ));
        assert_eq!(
            status
                .pointer("/last_config_publish/action")
                .and_then(Value::as_str),
            Some("config.publish")
        );
        assert_eq!(
            status
                .pointer("/last_cache_invalidate/action")
                .and_then(Value::as_str),
            Some("cache.invalidate")
        );
    }

    #[test]
    fn config_publish_is_refused_when_state_provider_is_unavailable() {
        let host = test_host();
        let current = host.runtime_core();
        let mut seams = current.seams().clone();
        seams.state_provider = Some(Arc::new(UnavailableStateProvider));
        host.replace_runtime_core_for_test(RuntimeCore::new(
            current.registry().clone(),
            seams,
            current.wiring_plan().clone(),
        ));

        let response = handle_control_plane_request(
            Method::POST,
            "/config/publish",
            json!({ "revision": "r2" }),
            &host,
        );
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response_json(response);
        assert_eq!(
            body.get("request_class").and_then(Value::as_str),
            Some("control_plane.config.publish")
        );
        assert!(
            body.get("blocking_provider_classes")
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item.as_str() == Some("state")))
        );
    }

    #[test]
    fn complete_drain_retires_previous_bundle_by_id() {
        let host = test_host();
        let tmp_a = tempfile::tempdir().expect("tempdir");
        let tmp_b = tempfile::tempdir().expect("tempdir");

        let staged_a = response_json(handle_control_plane_request(
            Method::POST,
            "/deployments/stage",
            json!({ "bundle_ref": tmp_a.path().display().to_string() }),
            &host,
        ));
        let bundle_a = staged_a
            .get("bundle_id")
            .and_then(Value::as_str)
            .expect("bundle a")
            .to_string();
        let _ = handle_control_plane_request(
            Method::POST,
            "/deployments/warm",
            json!({ "bundle_id": bundle_a }),
            &host,
        );
        let activated_a = response_json(handle_control_plane_request(
            Method::POST,
            "/deployments/activate",
            json!({ "bundle_id": bundle_a }),
            &host,
        ));
        let bundle_a = activated_a
            .get("bundle_id")
            .and_then(Value::as_str)
            .expect("active bundle a")
            .to_string();

        let staged_b = response_json(handle_control_plane_request(
            Method::POST,
            "/deployments/stage",
            json!({ "bundle_ref": tmp_b.path().display().to_string() }),
            &host,
        ));
        let bundle_b = staged_b
            .get("bundle_id")
            .and_then(Value::as_str)
            .expect("bundle b")
            .to_string();
        let _ = handle_control_plane_request(
            Method::POST,
            "/deployments/warm",
            json!({ "bundle_id": bundle_b }),
            &host,
        );
        let _ = handle_control_plane_request(
            Method::POST,
            "/deployments/activate",
            json!({ "bundle_id": bundle_b }),
            &host,
        );

        let response = handle_control_plane_request(
            Method::POST,
            "/deployments/complete-drain",
            json!({ "bundle_id": bundle_a }),
            &host,
        );
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response);
        assert_eq!(
            body.get("action").and_then(Value::as_str),
            Some("complete-drain")
        );
        assert_eq!(
            body.get("bundle_id").and_then(Value::as_str),
            Some(bundle_a.as_str())
        );
    }

    #[test]
    fn observability_log_level_can_be_read_and_set() {
        let host = test_host();

        let current =
            handle_control_plane_request(Method::GET, "/observability/log-level", json!({}), &host);
        assert_eq!(current.status(), StatusCode::OK);

        let updated = handle_control_plane_request(
            Method::POST,
            "/observability/log-level",
            json!({ "level": "debug" }),
            &host,
        );
        assert_eq!(updated.status(), StatusCode::OK);
        let body = response_json(updated);
        assert_eq!(body.get("log_level").and_then(Value::as_str), Some("debug"));

        let current = response_json(handle_control_plane_request(
            Method::GET,
            "/observability/log-level",
            json!({}),
            &host,
        ));
        assert_eq!(
            current.get("log_level").and_then(Value::as_str),
            Some("debug")
        );
    }
}
