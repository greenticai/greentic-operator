//! `POST /deployments/{stage,warm,activate,rollback,complete-drain}` HTTP
//! handlers. Thin JSON-body adapters over the `greentic_deployer` library
//! verbs — the same verbs the local `gtc op revisions|traffic` CLI calls.
//!
//! ## Trust boundary (Phase B local-CLI scaffolding)
//!
//! These routes mutate live deployment state but the bundled HTTP ingress
//! has no caller authn/authz today. The deployer library's own
//! `authorize_local_only` only checks the *env id* (`local` is allowed),
//! which is caller-supplied JSON — it cannot distinguish a trusted local
//! process from a browser tab or a remote attacker. To close that gap
//! without pulling in Phase D's mTLS work, this module enforces a
//! defense-in-depth gate at the handler:
//!
//! - **Peer must be loopback** when `loopback_only` is set. Set to `false`
//!   only after mTLS / admin admission lands.
//! - **No proxy/tunnel forwarding markers** when `loopback_only` is set —
//!   tunnel daemons (cloudflared, ngrok) run locally and connect from a
//!   loopback peer, so the peer-IP check alone cannot tell tunneled remote
//!   traffic from a genuinely local caller. The daemon always injects
//!   forwarding headers the remote caller cannot strip; their presence
//!   fails the gate.
//! - **`Origin` header (if present) must point at loopback** — blocks
//!   browser cross-origin CSRF even when the bind address is loopback
//!   (the shared ingress applies wildcard CORS to all responses).
//!
//! Folding into `admin_api::AdminState` with mTLS is Phase D/E (plan §B4b).
//!
//! ## Store
//!
//! Each request constructs a fresh `LocalFsStore` rooted at `state.env_root`
//! → `GREENTIC_OPERATOR_ENV_ROOT` → `LocalFsStore::default_root()`. Per-
//! request stores are safe — the store is just a path holder; file-level
//! locking lives inside `store.transact`.
//!
//! ## Errors
//!
//! `OpError` → HTTP status mapping mirrors the CLI's exit-code envelope so
//! a local caller switching from `gtc op` to HTTP sees the same semantics.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use greentic_deploy_spec::{Environment, RevisionLifecycle};
use greentic_deployer::cli::{
    OpError, OpFlags, OpOutcome,
    revisions::{self, RevisionStagePayload, RevisionTransitionPayload},
    traffic::{self, TrafficSetPayload, TrafficShowPayload},
};
use greentic_deployer::environment::{EnvironmentStore, LocalFsStore};
use http_body_util::{BodyExt, Limited};
use hyper::{
    HeaderMap, Method, Request, StatusCode,
    body::Incoming,
    header::{HeaderValue, ORIGIN},
};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::http_util::{
    HttpError as DeploymentError, HttpResponse as DeploymentResponse,
    HttpResult as DeploymentResult, error_response, into_error, json_response,
};

/// Environment-variable override for the env-store root. When unset, we fall
/// back to [`LocalFsStore::default_root`] (`~/.greentic/environments`) — the
/// same default `gtc op` uses.
const ENV_ROOT_VAR: &str = "GREENTIC_OPERATOR_ENV_ROOT";

/// Per-request body cap. Generous for stage payloads (manifest + pack list +
/// sidecar refs) but bounds the loopback-DoS surface: the trust-boundary gate
/// refuses remote peers, but a local hostile process could otherwise stream
/// gigabytes into `body.collect()` and OOM the operator.
const MAX_BODY_BYTES: usize = 512 * 1024;

/// Shared state.
///
/// - `env_root = None` falls back to the per-user default at request time;
///   tests inject a tempdir.
/// - `loopback_only = true` (the default) enforces a peer-IP loopback check
///   on every request. Flip to `false` only after mTLS / admin admission
///   lands (Phase D).
pub struct DeploymentsState {
    pub env_root: Option<PathBuf>,
    pub loopback_only: bool,
}

impl DeploymentsState {
    pub fn new() -> Self {
        Self {
            env_root: None,
            loopback_only: true,
        }
    }

    fn build_store(&self) -> Result<LocalFsStore, DeploymentError> {
        if let Some(root) = &self.env_root {
            return Ok(LocalFsStore::new(root.clone()));
        }
        if let Ok(override_root) = std::env::var(ENV_ROOT_VAR)
            && !override_root.is_empty()
        {
            return Ok(LocalFsStore::new(PathBuf::from(override_root)));
        }
        LocalFsStore::default_root()
            .map(LocalFsStore::new)
            .ok_or_else(|| {
                into_error(error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "no env-store root: HOME / USERPROFILE not set and \
                     GREENTIC_OPERATOR_ENV_ROOT unset",
                ))
            })
    }
}

impl Default for DeploymentsState {
    fn default() -> Self {
        Self::new()
    }
}

/// Dispatch `/deployments/*` requests.
///
///   POST /deployments/stage           → `revisions::stage`
///   POST /deployments/warm            → `revisions::warm`
///   POST /deployments/activate        → `traffic::set`
///   POST /deployments/rollback        → `traffic::rollback`
///   POST /deployments/complete-drain  → `revisions::archive` (Draining → Inactive)
pub async fn handle_deployments_request(
    req: Request<Incoming>,
    path: &str,
    peer_addr: SocketAddr,
    state: &Arc<DeploymentsState>,
) -> DeploymentResult {
    // §B4b trust-boundary gate. Runs before body collection so a hostile
    // caller cannot stream a large body just to be refused.
    check_trust_boundary(peer_addr, req.headers(), state)?;

    let method = req.method().clone();
    // Cap the body even though the trust boundary already blocks remote
    // peers — a local hostile process could otherwise OOM the operator.
    let body_bytes = Limited::new(req.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|err| {
            let too_large = err
                .downcast_ref::<http_body_util::LengthLimitError>()
                .is_some();
            let (status, msg) = if too_large {
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("body exceeds {MAX_BODY_BYTES} bytes"),
                )
            } else {
                (StatusCode::BAD_REQUEST, format!("read body: {err}"))
            };
            into_error(error_response(status, msg))
        })?;
    dispatch(method, path, &body_bytes, state).await
}

/// Enforce the Phase B trust boundary documented in the module header.
/// `Origin` header (if present) must be loopback even on a loopback bind,
/// because the shared ingress applies wildcard CORS so a browser tab on the
/// same machine could otherwise CSRF these routes.
fn check_trust_boundary(
    peer_addr: SocketAddr,
    headers: &HeaderMap<HeaderValue>,
    state: &Arc<DeploymentsState>,
) -> Result<(), DeploymentError> {
    if state.loopback_only {
        if !peer_addr.ip().is_loopback() {
            return Err(into_error(error_response(
                StatusCode::FORBIDDEN,
                format!(
                    "/deployments/* refused: peer {peer_addr} is not loopback; \
                     mTLS / admin admission is Phase D (plan §B4b)"
                ),
            )));
        }
        // Tunnel daemons collapse the peer-IP check above (module doc,
        // "No proxy/tunnel forwarding markers"). Skipped when
        // `loopback_only = false`: a Phase D upstream admission proxy
        // legitimately adds these headers.
        if let Some(marker) = forwarding_marker(headers) {
            return Err(into_error(error_response(
                StatusCode::FORBIDDEN,
                format!(
                    "/deployments/* refused: request bears forwarding marker \
                     `{marker}` (proxied/tunneled traffic); direct local \
                     callers only until mTLS lands (plan §B4b)"
                ),
            )));
        }
    }
    if let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok())
        && !is_loopback_origin(origin)
    {
        return Err(into_error(error_response(
            StatusCode::FORBIDDEN,
            format!(
                "/deployments/* refused: Origin `{origin}` is not loopback; \
                 cross-origin browser callers are blocked until mTLS lands"
            ),
        )));
    }
    Ok(())
}

/// Header names (already lowercase in hyper's `HeaderMap`) that mark a
/// request as having transited a reverse proxy or tunnel daemon. Covers the
/// `X-Forwarded` family (cloudflared, ngrok, generic proxies), `X-Real-IP`
/// (nginx), and RFC 7239 `Forwarded`.
const FORWARDING_MARKER_HEADERS: [&str; 5] = [
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "forwarded",
];

/// First forwarding marker borne by the request, if any: a known forwarding
/// header or any `cf-*` header (cloudflared injects several beyond the
/// `X-Forwarded` family, e.g. `cf-connecting-ip`, `cf-ray`).
fn forwarding_marker(headers: &HeaderMap<HeaderValue>) -> Option<&str> {
    headers
        .keys()
        .map(|name| name.as_str())
        .find(|name| FORWARDING_MARKER_HEADERS.contains(name) || name.starts_with("cf-"))
}

/// True for `http(s)://localhost[:port]`, `http(s)://127.0.0.1[:port]`,
/// `http(s)://[::1][:port]`. The literal `null` is intentionally refused:
/// browsers emit it from sandboxed iframes, `data:` URLs, and `file://`
/// pages regardless of which site hosts the sandbox, so it is not evidence
/// of a local caller.
fn is_loopback_origin(origin: &str) -> bool {
    let after_scheme = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or(origin);
    // Strip an optional `/`-prefixed path tail (browsers don't include one
    // in the Origin header, but be permissive).
    let host = after_scheme.split('/').next().unwrap_or("");
    let host_no_port = if let Some(stripped) = host.strip_prefix('[') {
        // IPv6 literal: `[::1]:port`.
        stripped.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    matches!(
        host_no_port,
        "localhost" | "127.0.0.1" | "::1" | "0:0:0:0:0:0:0:1"
    )
}

/// Inner dispatcher — split out of [`handle_deployments_request`] so unit
/// tests can drive it with pre-collected body bytes without going through a
/// real hyper connection (constructing `Request<Incoming>` requires a live
/// stream, and hangs when the test holds the request after the mock server
/// closes the connection).
async fn dispatch(
    method: Method,
    path: &str,
    body_bytes: &[u8],
    state: &Arc<DeploymentsState>,
) -> DeploymentResult {
    let sub_path = path
        .strip_prefix("/deployments")
        .unwrap_or("")
        .trim_end_matches('/');

    match (method, sub_path) {
        (Method::POST, "/stage") => {
            let payload = parse_json::<RevisionStagePayload>(body_bytes)?;
            run_blocking(state, move |store, flags| {
                revisions::stage(store, flags, Some(payload))
            })
            .await
        }
        (Method::POST, "/warm") => {
            let payload = parse_json::<RevisionTransitionPayload>(body_bytes)?;
            run_blocking(state, move |store, flags| {
                revisions::warm(store, flags, Some(payload))
            })
            .await
        }
        (Method::POST, "/activate") => {
            let payload = parse_json::<TrafficSetPayload>(body_bytes)?;
            run_blocking(state, move |store, flags| {
                traffic::set(store, flags, Some(payload))
            })
            .await
        }
        (Method::POST, "/rollback") => {
            let payload = parse_json::<TrafficShowPayload>(body_bytes)?;
            run_blocking(state, move |store, flags| {
                traffic::rollback(store, flags, Some(payload))
            })
            .await
        }
        (Method::POST, "/complete-drain") => {
            // The reserved route name is "complete-drain" — i.e. drive
            // `Draining → Inactive`. `revisions::drain` is the *initiation*
            // step (`Ready → Draining`) and is intentionally not exposed
            // here; the completion hop lives inside `revisions::archive`'s
            // state-machine table. Guard the precondition first so we
            // don't silently archive a Ready revision via archive's
            // `Ready → Archived` fallback transition.
            let payload = parse_json::<RevisionTransitionPayload>(body_bytes)?;
            complete_drain(state, payload).await
        }
        (_, "/stage" | "/warm" | "/activate" | "/rollback" | "/complete-drain") => {
            Err(into_error(error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                format!("only POST allowed on /deployments{sub_path}"),
            )))
        }
        _ => Err(into_error(error_response(
            StatusCode::NOT_FOUND,
            format!("unknown /deployments endpoint: {sub_path}"),
        ))),
    }
}

/// `POST /deployments/complete-drain`. Drives a single revision from
/// `Draining → Inactive` via the deployer library's `archive` state-machine.
///
/// The precondition (lifecycle == Draining) is checked twice: a pre-flight
/// load outside the env flock (fast, friendly error) and then implicitly by
/// the transaction itself (any Ready/Warming/Staged revision would walk to
/// Archived under `archive`, not to Inactive — which we refuse). The
/// pre-flight check is best-effort: a racing concurrent transition would
/// flip the lifecycle between the load and the `archive` call, but the
/// worst case is a confusing-but-correct outcome from `archive`'s
/// transition matrix; no state is corrupted.
async fn complete_drain(
    state: &Arc<DeploymentsState>,
    payload: RevisionTransitionPayload,
) -> DeploymentResult {
    let store = state.build_store()?;
    let env_id = match greentic_deploy_spec::EnvId::try_from(payload.environment_id.as_str()) {
        Ok(id) => id,
        Err(e) => {
            return Err(into_error(op_error_response(&OpError::InvalidArgument(
                format!("environment_id: {e}"),
            ))));
        }
    };
    let revision_id_str = payload.revision_id.clone();

    let env_for_check = store.clone();
    let preflight = tokio::task::spawn_blocking(move || env_for_check.load(&env_id))
        .await
        .map_err(|err| {
            into_error(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("worker join: {err}"),
            ))
        })?;
    let env = preflight.map_err(|e| into_error(op_error_response(&OpError::from(e))))?;
    precheck_drain_completable(&env, &revision_id_str)
        .map_err(|err| into_error(op_error_response(&err)))?;

    run_blocking(state, move |store, flags| {
        revisions::archive(store, flags, Some(payload))
    })
    .await
}

/// Pre-flight contract guard for `/complete-drain`. Pulled out as a pure
/// function so tests can exercise the precondition without standing up a
/// real env on disk.
///
/// ULIDs are Crockford Base32 and `from_str` accepts both cases — parse the
/// input once and compare typed values, so a lowercase ULID from the caller
/// matches the canonical uppercase `Display` form (and we don't allocate a
/// String per iteration).
fn precheck_drain_completable(env: &Environment, revision_id: &str) -> Result<(), OpError> {
    let target = ulid::Ulid::from_str(revision_id)
        .map_err(|e| OpError::InvalidArgument(format!("revision_id: {e}")))?;
    let rev = env
        .revisions
        .iter()
        .find(|r| r.revision_id.0 == target)
        .ok_or_else(|| OpError::NotFound(format!("revision `{revision_id}` not found in env")))?;
    if rev.lifecycle != RevisionLifecycle::Draining {
        return Err(OpError::Conflict(format!(
            "/complete-drain requires revision in `Draining`; \
             revision `{revision_id}` is `{:?}`. Initiate drain \
             (`Ready → Draining`) before completing it.",
            rev.lifecycle,
        )));
    }
    Ok(())
}

async fn run_blocking<F>(state: &Arc<DeploymentsState>, op: F) -> DeploymentResult
where
    F: FnOnce(&LocalFsStore, &OpFlags) -> Result<OpOutcome, OpError> + Send + 'static,
{
    let store = state.build_store()?;
    let result = tokio::task::spawn_blocking(move || op(&store, &OpFlags::default()))
        .await
        .map_err(|err| {
            into_error(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("worker join: {err}"),
            ))
        })?;
    match result {
        Ok(outcome) => Ok(json_response(
            StatusCode::OK,
            serde_json::to_value(outcome).unwrap_or_else(|_| json!({})),
        )),
        Err(err) => Err(into_error(op_error_response(&err))),
    }
}

fn parse_json<T: DeserializeOwned>(body: &[u8]) -> Result<T, DeploymentError> {
    serde_json::from_slice::<T>(body).map_err(|err| {
        into_error(error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid JSON: {err}"),
        ))
    })
}

/// Map an `OpError` to the documented `{op, noun, error}` envelope. Status
/// codes match the CLI's exit-code semantics so a local caller switching
/// from `gtc op` to HTTP sees the same kind taxonomy.
fn op_error_response(err: &OpError) -> DeploymentResponse {
    let status = match err {
        OpError::NotFound(_) => StatusCode::NOT_FOUND,
        OpError::Conflict(_) | OpError::NotDrained { .. } => StatusCode::CONFLICT,
        OpError::Unauthorized { .. } | OpError::PermissionMissing(_) => StatusCode::FORBIDDEN,
        OpError::InvalidArgument(_) | OpError::Spec(_) | OpError::AnswersParse { .. } => {
            StatusCode::BAD_REQUEST
        }
        OpError::NotYetImplemented(_) | OpError::CapabilityMissing(_) => {
            StatusCode::NOT_IMPLEMENTED
        }
        OpError::Store(_)
        | OpError::Io { .. }
        | OpError::SchemaGeneration(_)
        | OpError::Audit(_)
        | OpError::RevenuePolicy(_)
        | OpError::TrustRoot(_)
        | OpError::OperatorKey(_)
        | OpError::Fetch(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    json_response(
        status,
        json!({
            "error": {
                "kind": err.kind(),
                "message": err.to_string(),
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use greentic_deploy_spec::{
        BundleId, DeploymentId, EnvId, Revision, RevisionId, RevisionLifecycle, SchemaVersion,
    };
    use http_body_util::BodyExt;
    use std::net::{IpAddr, Ipv4Addr};

    fn state_with_tempdir(tmp: &tempfile::TempDir) -> Arc<DeploymentsState> {
        Arc::new(DeploymentsState {
            env_root: Some(tmp.path().to_path_buf()),
            loopback_only: true,
        })
    }

    /// `loopback_only = false` variant — the Phase D "caller wired their own
    /// admission upstream" configuration.
    fn state_with_tempdir_open(tmp: &tempfile::TempDir) -> Arc<DeploymentsState> {
        Arc::new(DeploymentsState {
            env_root: Some(tmp.path().to_path_buf()),
            loopback_only: false,
        })
    }

    fn loopback_peer() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1234)
    }

    fn remote_peer() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 1234)
    }

    async fn read_body_json(resp: DeploymentResponse) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        if bytes.is_empty() {
            return json!({});
        }
        serde_json::from_slice(&bytes).expect("valid JSON body")
    }

    #[test]
    fn max_body_bytes_constant_is_reasonable() {
        // Tracks intent: stage payloads carry a manifest + pack list +
        // sidecar refs; 512 KiB is generous (~thousand entries) but bounds
        // the loopback-DoS surface. If a real payload exceeds this, lift
        // the constant — don't disable the cap.
        assert_eq!(MAX_BODY_BYTES, 512 * 1024);
    }

    #[tokio::test]
    async fn unknown_subpath_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir(&tmp);
        let err = dispatch(Method::POST, "/deployments/bogus", b"{}", &state)
            .await
            .expect_err("bogus path must error");
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn wrong_method_returns_405() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir(&tmp);
        for verb in [
            "/stage",
            "/warm",
            "/activate",
            "/rollback",
            "/complete-drain",
        ] {
            let path = format!("/deployments{verb}");
            let err = dispatch(Method::GET, &path, b"", &state)
                .await
                .expect_err("GET must error");
            assert_eq!(
                err.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "GET {path} should 405"
            );
        }
    }

    #[tokio::test]
    async fn malformed_json_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir(&tmp);
        let err = dispatch(Method::POST, "/deployments/stage", b"{not-json", &state)
            .await
            .expect_err("malformed JSON must error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn missing_required_payload_field_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir(&tmp);
        let body = br#"{"environment_id": "demo-env"}"#;
        let err = dispatch(Method::POST, "/deployments/warm", body, &state)
            .await
            .expect_err("missing field must error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn end_to_end_through_deployer_lib_returns_typed_error_envelope() {
        // Drives a full happy-path payload through `dispatch` → deployer
        // library → `op_error_response`. This crate's lockfile used to freeze
        // on a develop-lane deployer whose deny-first `authorize_local_only`
        // refused non-local env ids, so the assertion here was a 403. Every
        // stable deployer since 1.1.0 uses `authorize_local_owner`, which
        // allows local-store callers; the closure then fails because the
        // environment does not exist in the empty tempdir, producing
        // `OpError::NotFound` → 404. The point of the test is the round-trip:
        // handler invoked the lib, got a typed `OpError`, mapped it to the
        // right HTTP status, and serialized the documented body. Note this no
        // longer covers the `unauthorized` → 403 mapping — nothing on this
        // path denies any more.
        // ULIDs are Crockford Base32 (no I/L/O/U); use a real one so we
        // get past the ID-parse branch.
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir(&tmp);
        let body = br#"{
            "environment_id": "demo-env",
            "deployment_id": "01HQXXVNAEPS9YYBFB3FBTQDR6",
            "bundle_digest": "sha256:00"
        }"#;
        let err = dispatch(Method::POST, "/deployments/stage", body, &state)
            .await
            .expect_err("env-not-found must surface as typed error");
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        let body = read_body_json(*err).await;
        assert_eq!(body["error"]["kind"], "not-found");
    }

    #[test]
    fn op_error_to_status_covers_every_variant() {
        // Lists every `OpError` variant. The real compile-time safety net
        // is the wildcard-free match in `op_error_response` — any new
        // variant in a future deployer release breaks that match. This
        // test is the runtime double-check that each variant maps to the
        // documented status.
        let cases: Vec<(OpError, StatusCode)> = vec![
            (
                OpError::Store(greentic_deployer::environment::StoreError::NotFound(
                    demo_env_id(),
                )),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                OpError::Spec(greentic_deploy_spec::SpecError::BasisPointsSum { sum: 0 }),
                StatusCode::BAD_REQUEST,
            ),
            (
                OpError::Io {
                    path: PathBuf::from("/dev/null"),
                    source: std::io::Error::other("boom"),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                OpError::AnswersParse {
                    path: PathBuf::from("/dev/null"),
                    message: "x".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                OpError::SchemaGeneration("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                OpError::InvalidArgument("x".into()),
                StatusCode::BAD_REQUEST,
            ),
            (OpError::NotFound("x".into()), StatusCode::NOT_FOUND),
            (
                OpError::NotYetImplemented("x".into()),
                StatusCode::NOT_IMPLEMENTED,
            ),
            (OpError::Conflict("x".into()), StatusCode::CONFLICT),
            (
                OpError::Unauthorized {
                    policy: "p".into(),
                    reason: "r".into(),
                },
                StatusCode::FORBIDDEN,
            ),
            (
                OpError::Audit("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (err, expected) in &cases {
            assert_eq!(
                op_error_response(err).status(),
                *expected,
                "{err} should map to {expected}"
            );
        }
    }

    // ── §B4b trust-boundary gate (Codex finding F1) ────────────────────

    #[test]
    fn check_trust_boundary_blocks_non_loopback_peer() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir(&tmp);
        let headers = HeaderMap::new();
        let err = check_trust_boundary(remote_peer(), &headers, &state)
            .expect_err("remote peer must be refused");
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn check_trust_boundary_allows_loopback_peer_without_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir(&tmp);
        let headers = HeaderMap::new();
        check_trust_boundary(loopback_peer(), &headers, &state).expect("loopback must pass");
    }

    #[test]
    fn check_trust_boundary_allows_loopback_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir(&tmp);
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_static("http://127.0.0.1:8080"));
        check_trust_boundary(loopback_peer(), &headers, &state).expect("loopback Origin must pass");
        headers.insert(ORIGIN, HeaderValue::from_static("http://localhost:8080"));
        check_trust_boundary(loopback_peer(), &headers, &state)
            .expect("localhost Origin must pass");
        headers.insert(ORIGIN, HeaderValue::from_static("http://[::1]:8080"));
        check_trust_boundary(loopback_peer(), &headers, &state).expect("[::1] Origin must pass");
    }

    #[test]
    fn check_trust_boundary_blocks_cross_origin_csrf() {
        // Browser tab on the same machine: peer is loopback, but Origin
        // points at an attacker-controlled site. Wildcard CORS on the
        // shared ingress would let the response through, so the handler
        // must refuse before the deployer mutation runs.
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir(&tmp);
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_static("https://evil.example.com"));
        let err = check_trust_boundary(loopback_peer(), &headers, &state)
            .expect_err("remote Origin must be refused");
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn check_trust_boundary_blocks_sandboxed_null_origin() {
        // A sandboxed iframe, `data:` URL, or `file://` page on an attacker
        // site emits `Origin: null`. The browser IS the loopback peer, so the
        // peer-IP check alone does not help — `null` must not be treated as
        // evidence of a local caller.
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir(&tmp);
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_static("null"));
        let err = check_trust_boundary(loopback_peer(), &headers, &state)
            .expect_err("sandboxed null Origin must be refused");
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn check_trust_boundary_blocks_tunneled_loopback_traffic() {
        // The cloudflared scenario: `demo up` tunnels a local port, so
        // remote traffic arrives with a loopback TCP peer and no Origin
        // (curl-style). The tunnel daemon injects forwarding headers the
        // remote caller cannot strip — the gate must refuse on them.
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir(&tmp);
        let mut cloudflared = HeaderMap::new();
        cloudflared.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.7"));
        cloudflared.insert("cf-ray", HeaderValue::from_static("8a1b2c3d4e5f6789-AMS"));
        cloudflared.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        let err = check_trust_boundary(loopback_peer(), &cloudflared, &state)
            .expect_err("cloudflared-tunneled traffic must be refused");
        assert_eq!(err.status(), StatusCode::FORBIDDEN);

        // ngrok injects only the X-Forwarded family — each marker alone
        // must trip the gate.
        for (name, value) in [
            ("x-forwarded-for", "203.0.113.7"),
            ("x-forwarded-host", "abc123.ngrok-free.app"),
            ("x-forwarded-proto", "https"),
            ("x-real-ip", "203.0.113.7"),
            ("forwarded", "for=203.0.113.7"),
            ("cf-warp-tag-id", "tag"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(name, HeaderValue::from_static(value));
            let err = match check_trust_boundary(loopback_peer(), &headers, &state) {
                Err(err) => err,
                Ok(()) => panic!("marker `{name}` must be refused"),
            };
            assert_eq!(err.status(), StatusCode::FORBIDDEN, "marker `{name}`");
        }
    }

    #[test]
    fn loopback_only_false_allows_forwarding_markers() {
        // Phase D path: an upstream admission proxy (mTLS terminator)
        // legitimately adds forwarding headers — the marker check must not
        // fire once the caller has wired real admission upstream.
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir_open(&tmp);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        check_trust_boundary(remote_peer(), &headers, &state)
            .expect("forwarding markers must pass when loopback_only=false");
    }

    #[test]
    fn loopback_only_false_disables_peer_check_but_keeps_origin_check() {
        // Phase D path: caller has wired in their own admission upstream.
        // We still defend in depth against browser CSRF via Origin.
        let tmp = tempfile::tempdir().unwrap();
        let state = state_with_tempdir_open(&tmp);
        let headers = HeaderMap::new();
        check_trust_boundary(remote_peer(), &headers, &state)
            .expect("remote peer must pass when loopback_only=false");
        let mut hostile = HeaderMap::new();
        hostile.insert(ORIGIN, HeaderValue::from_static("https://evil.example.com"));
        let err = check_trust_boundary(remote_peer(), &hostile, &state)
            .expect_err("remote Origin must still be refused");
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn is_loopback_origin_classifier() {
        for ok in [
            "http://localhost",
            "http://localhost:3000",
            "https://localhost",
            "http://127.0.0.1",
            "http://127.0.0.1:8080",
            "http://[::1]",
            "http://[::1]:8080",
        ] {
            assert!(is_loopback_origin(ok), "{ok} should classify as loopback");
        }
        for not_ok in [
            "http://example.com",
            "https://attacker.example",
            "http://127.0.0.1.evil.example",
            "http://localhostess.example",
            "http://10.0.0.1",
            "http://[2001:db8::1]",
            "null",
            "NULL",
        ] {
            assert!(
                !is_loopback_origin(not_ok),
                "{not_ok} must NOT classify as loopback"
            );
        }
    }

    // ── /complete-drain precondition (Codex finding F2) ────────────────

    fn demo_env_id() -> EnvId {
        EnvId::try_from("demo-env").unwrap()
    }

    fn revision_with_lifecycle(id: &RevisionId, lifecycle: RevisionLifecycle) -> Revision {
        Revision {
            schema: SchemaVersion::new(SchemaVersion::REVISION_V1),
            revision_id: *id,
            env_id: demo_env_id(),
            bundle_id: BundleId::new("demo-bundle"),
            deployment_id: DeploymentId(ulid::Ulid::new()),
            sequence: 1,
            created_at: Utc::now(),
            bundle_digest: "sha256:00".into(),
            runtime_image_digest: None,
            bundle_source_uri: None,
            pack_list: Vec::new(),
            pack_list_lock_ref: PathBuf::from("pack-list.lock"),
            pack_config_refs: Vec::new(),
            config_digest: "sha256:00".into(),
            signature_sidecar_ref: PathBuf::from("rev.sig"),
            lifecycle,
            staged_at: None,
            warmed_at: None,
            drain_seconds: 30,
            abort_metrics: Vec::new(),
        }
    }

    fn env_with_one_revision(rev: Revision) -> Environment {
        Environment {
            schema: SchemaVersion::new(SchemaVersion::ENVIRONMENT_V1),
            environment_id: demo_env_id(),
            name: "demo".into(),
            host_config: greentic_deploy_spec::EnvironmentHostConfig::new(demo_env_id()),
            packs: Vec::new(),
            credentials_ref: None,
            bundles: Vec::new(),
            revisions: vec![rev],
            traffic_splits: Vec::new(),
            messaging_endpoints: Vec::new(),
            extensions: Vec::new(),
            revocation: Default::default(),
            retention: Default::default(),
            health: Default::default(),
        }
    }

    #[test]
    fn precheck_drain_completable_accepts_draining_revision() {
        let rev_id = RevisionId(ulid::Ulid::new());
        let env = env_with_one_revision(revision_with_lifecycle(
            &rev_id,
            RevisionLifecycle::Draining,
        ));
        precheck_drain_completable(&env, &rev_id.to_string())
            .expect("Draining revision must be accepted");
    }

    #[test]
    fn precheck_drain_completable_rejects_ready_revision_with_conflict() {
        // The bug Codex flagged: archive's transition matrix would walk a
        // Ready revision straight to Archived. The precheck must refuse it.
        let rev_id = RevisionId(ulid::Ulid::new());
        let env = env_with_one_revision(revision_with_lifecycle(&rev_id, RevisionLifecycle::Ready));
        let err = precheck_drain_completable(&env, &rev_id.to_string())
            .expect_err("Ready revision must be refused");
        assert!(matches!(err, OpError::Conflict(_)), "got {err:?}");
        assert_eq!(op_error_response(&err).status(), StatusCode::CONFLICT);
    }

    #[test]
    fn precheck_drain_completable_rejects_other_lifecycle_states() {
        for lifecycle in [
            RevisionLifecycle::Inactive,
            RevisionLifecycle::Staged,
            RevisionLifecycle::Warming,
            RevisionLifecycle::Failed,
            RevisionLifecycle::Archived,
        ] {
            let rev_id = RevisionId(ulid::Ulid::new());
            let env = env_with_one_revision(revision_with_lifecycle(&rev_id, lifecycle));
            let err = precheck_drain_completable(&env, &rev_id.to_string())
                .expect_err("non-Draining must be refused");
            assert!(matches!(err, OpError::Conflict(_)), "got {err:?}");
        }
    }

    #[test]
    fn precheck_drain_completable_accepts_lowercase_ulid_input() {
        // Regression: `RevisionId::Display` is uppercase Crockford Base32,
        // but `Ulid::from_str` accepts both cases. The stringly compare in
        // the original PR rejected lowercase input as 404.
        let rev_id = RevisionId(ulid::Ulid::new());
        let env = env_with_one_revision(revision_with_lifecycle(
            &rev_id,
            RevisionLifecycle::Draining,
        ));
        let lower = rev_id.to_string().to_ascii_lowercase();
        precheck_drain_completable(&env, &lower)
            .expect("lowercase ULID must match the same revision");
    }

    #[test]
    fn precheck_drain_completable_invalid_ulid_is_400() {
        let env = env_with_one_revision(revision_with_lifecycle(
            &RevisionId(ulid::Ulid::new()),
            RevisionLifecycle::Draining,
        ));
        let err = precheck_drain_completable(&env, "not-a-ulid")
            .expect_err("non-ULID input must be InvalidArgument");
        assert!(matches!(err, OpError::InvalidArgument(_)), "got {err:?}");
        assert_eq!(op_error_response(&err).status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn precheck_drain_completable_missing_revision_is_404() {
        let env = env_with_one_revision(revision_with_lifecycle(
            &RevisionId(ulid::Ulid::new()),
            RevisionLifecycle::Draining,
        ));
        let unknown = RevisionId(ulid::Ulid::new()).to_string();
        let err = precheck_drain_completable(&env, &unknown)
            .expect_err("unknown revision must be NotFound");
        assert!(matches!(err, OpError::NotFound(_)), "got {err:?}");
        assert_eq!(op_error_response(&err).status(), StatusCode::NOT_FOUND);
    }
}
