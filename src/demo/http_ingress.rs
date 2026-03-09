use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use anyhow::{Context, Result};
use base64::Engine as _;
use greentic_types::ChannelMessageEnvelope;
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    header::{CONTENT_TYPE, HeaderName, HeaderValue},
    server::conn::http1::Builder as Http1Builder,
    service::service_fn,
};
use hyper_util::rt::tokio::TokioIo;
use serde_json::json;
use tokio::{net::TcpListener, runtime::Runtime, sync::oneshot};

use tracing::info_span;

use crate::demo::event_router::route_events_to_default_flow;
use crate::demo::ingress_dispatch::dispatch_http_ingress;
use crate::demo::ingress_types::{IngressHttpResponse, IngressRequestV1};
use crate::demo::runner_host::{DemoRunnerHost, OperatorContext};
use crate::domains::{self, Domain};
use crate::messaging_universal::{app, dto::ProviderPayloadV1, egress};
use crate::operator_log;

/// Operator-level store for bot reply activities.
///
/// The WASM webchat component's `send_payload` is supposed to append bot replies
/// to the conversation state store, but due to a key mismatch between the
/// hardcoded context in the component and the operator's tenant context, the
/// write silently fails.  This store lets the operator inject bot replies
/// directly into GET /activities responses.
#[derive(Clone, Default)]
struct BotActivityStore {
    /// conversation_id → list of pending bot activities (JSON values)
    pending: Arc<Mutex<HashMap<String, Vec<serde_json::Value>>>>,
}

impl BotActivityStore {
    fn push(&self, conversation_id: &str, activity: serde_json::Value) {
        let mut map = self.pending.lock().unwrap();
        map.entry(conversation_id.to_string())
            .or_default()
            .push(activity);
    }

    fn drain(&self, conversation_id: &str) -> Vec<serde_json::Value> {
        let mut map = self.pending.lock().unwrap();
        map.remove(conversation_id).unwrap_or_default()
    }
}

/// Per-chat form state for Telegram text-input cards.
///
/// Telegram doesn't support native form inputs like Teams/Slack.
/// When we send a card with Input.Text fields, we store the expected
/// input IDs and the submit action data. When the user replies with
/// text (is_form_reply=true), we inject the text as the input value
/// and auto-trigger the submit action.
#[derive(Clone, Default)]
struct TelegramFormStore {
    /// chat_id → pending form state
    pending: Arc<Mutex<HashMap<String, TelegramFormState>>>,
}

#[derive(Clone)]
struct TelegramFormState {
    /// Input field IDs expected from the user, e.g., ["github_token"]
    input_ids: Vec<String>,
    /// The submit action's data payload, e.g., {"action": "save_token"}
    submit_data: HashMap<String, String>,
}

impl TelegramFormStore {
    fn store(&self, chat_id: &str, state: TelegramFormState) {
        let mut map = self.pending.lock().unwrap();
        map.insert(chat_id.to_string(), state);
    }

    fn take(&self, chat_id: &str) -> Option<TelegramFormState> {
        let mut map = self.pending.lock().unwrap();
        map.remove(chat_id)
    }
}

/// Extract form state from an outgoing Adaptive Card envelope.
/// Returns Some(TelegramFormState) if the card has Input.Text fields.
fn extract_form_state_from_card(
    envelope: &ChannelMessageEnvelope,
) -> Option<TelegramFormState> {
    let ac_raw = envelope.metadata.get("adaptive_card")?;
    let card: serde_json::Value = serde_json::from_str(ac_raw).ok()?;
    let body = card.get("body")?.as_array()?;

    // Recursively find Input.Text elements
    let mut input_ids = Vec::new();
    collect_input_ids(body, &mut input_ids);
    if input_ids.is_empty() {
        return None;
    }

    // Find the first Action.Submit with data
    let mut submit_data = HashMap::new();
    if let Some(actions) = card.get("actions").and_then(|a| a.as_array()) {
        for action in actions {
            let atype = action
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or_default();
            if atype == "Action.Submit" {
                if let Some(data) = action.get("data").and_then(|d| d.as_object()) {
                    for (k, v) in data {
                        let val = match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        submit_data.insert(k.clone(), val);
                    }
                    break; // use first submit action
                }
            }
        }
    }

    Some(TelegramFormState {
        input_ids,
        submit_data,
    })
}

/// Recursively collect Input.Text element IDs from an Adaptive Card body.
fn collect_input_ids(items: &[serde_json::Value], out: &mut Vec<String>) {
    for item in items {
        let etype = item
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        match etype {
            "Input.Text" => {
                if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                    out.push(id.to_string());
                }
            }
            "Container" | "Column" => {
                if let Some(sub) = item.get("items").and_then(|i| i.as_array()) {
                    collect_input_ids(sub, out);
                }
            }
            "ColumnSet" => {
                if let Some(cols) = item.get("columns").and_then(|c| c.as_array()) {
                    for col in cols {
                        if let Some(sub) = col.get("items").and_then(|i| i.as_array()) {
                            collect_input_ids(sub, out);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone)]
pub struct HttpIngressConfig {
    pub bind_addr: SocketAddr,
    pub domains: Vec<Domain>,
    pub runner_host: Arc<DemoRunnerHost>,
    /// Optional directory containing built webchat SPA assets to serve.
    pub webchat_spa_dir: Option<PathBuf>,
}

pub struct HttpIngressServer {
    shutdown: Option<oneshot::Sender<()>>,
    handle: Option<thread::JoinHandle<Result<()>>>,
}

impl HttpIngressServer {
    pub fn start(config: HttpIngressConfig) -> Result<Self> {
        let debug_enabled = config.runner_host.debug_enabled();
        let domains = config.domains;
        let runner_host = config.runner_host;
        let webchat_spa_dir = config.webchat_spa_dir.clone();
        let state = Arc::new(HttpIngressState {
            runner_host,
            domains,
            webchat_spa_dir,
            bot_activities: BotActivityStore::default(),
            tg_form_store: TelegramFormStore::default(),
        });
        let (tx, rx) = oneshot::channel();
        let addr = config.bind_addr;
        let handle = thread::Builder::new()
            .name("demo-ingress".to_string())
            .spawn(move || -> Result<()> {
                let runtime = Runtime::new().context("failed to create ingress runtime")?;
                runtime.block_on(async move {
                    let listener = TcpListener::bind(addr)
                        .await
                        .context("failed to bind ingress listener")?;
                    operator_log::info(
                        module_path!(),
                        format!("demo ingress listening on http://{}", addr),
                    );
                    if debug_enabled {
                        let domain_list = state
                            .domains
                            .iter()
                            .map(|domain| domains::domain_name(*domain))
                            .collect::<Vec<_>>()
                            .join(",");
                        operator_log::debug(
                            module_path!(),
                            format!(
                                "[demo dev] ingress server bound={} domains={}",
                                addr, domain_list
                            ),
                        );
                    }
                    let mut shutdown = rx;
                    loop {
                        tokio::select! {
                            _ = &mut shutdown => break,
                            accept = listener.accept() => match accept {
                                Ok((stream, _peer)) => {
                                    let connection_state = state.clone();
                                    tokio::spawn(async move {
                                        let service = service_fn(move |req| {
                                            handle_request(req, connection_state.clone())
                                        });
                                        let http = Http1Builder::new();
                                        let stream = TokioIo::new(stream);
                                        if let Err(err) = http
                                            .serve_connection(stream, service)
                                            .await
                                        {
                                            operator_log::error(
                                                module_path!(),
                                                format!(
                                                    "demo ingress connection error: {err}"
                                                ),
                                            );
                                        }
                                    });
                                }
                                Err(err) => {
                                    operator_log::error(
                                        module_path!(),
                                        format!("demo ingress accept error: {err}"),
                                    );
                                }
                            },
                        }
                    }
                    Ok(())
                })
            })?;
        Ok(Self {
            shutdown: Some(tx),
            handle: Some(handle),
        })
    }

    pub fn stop(mut self) -> Result<()> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let joined = handle
                .join()
                .map_err(|err| anyhow::anyhow!("ingress server panicked: {err:?}"))?;
            joined?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct HttpIngressState {
    runner_host: Arc<DemoRunnerHost>,
    domains: Vec<Domain>,
    webchat_spa_dir: Option<PathBuf>,
    bot_activities: BotActivityStore,
    tg_form_store: TelegramFormStore,
}

async fn handle_request(
    req: Request<Incoming>,
    state: Arc<HttpIngressState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let response = match handle_request_inner(req, state).await {
        Ok(response) => with_cors(response),
        Err(response) => with_cors(response),
    };
    Ok(response)
}

async fn handle_request_inner(
    req: Request<Incoming>,
    state: Arc<HttpIngressState>,
) -> Result<Response<Full<Bytes>>, Response<Full<Bytes>>> {
    // CORS preflight
    if req.method() == Method::OPTIONS {
        return Ok(cors_preflight_response());
    }
    if req.method() != Method::POST && req.method() != Method::GET {
        return Err(error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "only GET/POST/OPTIONS allowed",
        ));
    }

    let path = req.uri().path().to_string();
    tracing::info!(
        method = %req.method(),
        path = %path,
        "http_ingress request"
    );

    // Onboard API routes: /api/onboard/*
    if path.starts_with("/api/onboard") {
        return crate::onboard::api::handle_onboard_request(req, &path, &state.runner_host)
            .await
            .map_err(|err| *err);
    }

    // Direct Line routes: /token, /v3/directline/*, /directline/*
    if path == "/token" || path.starts_with("/v3/directline") || path.starts_with("/directline") {
        return handle_directline_request(req, &path, state).await;
    }

    // Serve provider GUI assets: /v1/messaging/{provider}/{tenant}/{team}/gui/[...]
    if let Some(spa_dir) = state.webchat_spa_dir.as_ref() {
        match parse_gui_route(&path) {
            Some(gui) => return serve_spa_file(spa_dir, &gui.asset_path, Some(&gui.tenant)),
            None if path_needs_gui_trailing_slash(&path) => {
                // Redirect /gui → /gui/ so relative asset paths resolve correctly.
                let location = format!("{}/", path);
                return Ok(Response::builder()
                    .status(StatusCode::MOVED_PERMANENTLY)
                    .header("Location", &location)
                    .body(Full::from(Bytes::new()))
                    .unwrap());
            }
            _ => {}
        }
    }

    let method = req.method().clone();
    let parsed = match parse_route_segments(req.uri().path()) {
        Some(value) => value,
        None => {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "expected /v1/{domain}/ingress/{provider}/{tenant}/{team?}/{handler?}",
            ));
        }
    };
    let domain = parsed.domain;
    if !state.domains.contains(&domain) {
        return Err(error_response(StatusCode::NOT_FOUND, "domain disabled"));
    }
    if !state
        .runner_host
        .supports_op(domain, &parsed.provider, "ingest_http")
    {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "no ingest_http handler available",
        ));
    }

    operator_log::info(
        module_path!(),
        format!(
            "[ingress] accepted method={} provider={} tenant={} team={}",
            method, parsed.provider, parsed.tenant, parsed.team,
        ),
    );

    let correlation_id = req
        .headers()
        .get("x-correlation-id")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    let headers = collect_headers(req.headers());
    let queries = collect_queries(req.uri().query());
    let payload_bytes = req
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();

    let context = OperatorContext {
        tenant: parsed.tenant.clone(),
        team: Some(parsed.team.clone()),
        correlation_id: correlation_id.clone(),
    };
    let debug_enabled = state.runner_host.debug_enabled();
    if debug_enabled {
        operator_log::debug(
            module_path!(),
            format!(
                "[demo dev] ingress request method={} path={} domain={} provider={} tenant={} team={} corr_id={:?} payload_len={}",
                method,
                path,
                domains::domain_name(domain),
                parsed.provider,
                context.tenant,
                context.team.as_deref().unwrap_or("default"),
                context.correlation_id.as_deref().unwrap_or("none"),
                payload_bytes.len(),
            ),
        );
    }

    let ingress_request = IngressRequestV1 {
        v: 1,
        domain: domains::domain_name(domain).to_string(),
        provider: parsed.provider.clone(),
        handler: parsed.handler.clone(),
        tenant: parsed.tenant.clone(),
        team: Some(parsed.team.clone()),
        method: method.as_str().to_string(),
        path: path.clone(),
        query: queries,
        headers,
        body: payload_bytes.to_vec(),
        correlation_id: correlation_id.clone(),
        remote_addr: None,
    };

    let result = {
        let _dispatch_span = tracing::info_span!(
            "ingress_dispatch",
            provider = %parsed.provider,
            tenant = %parsed.tenant,
            team = %parsed.team,
            domain = %domains::domain_name(domain),
        )
        .entered();
        dispatch_http_ingress(
            state.runner_host.as_ref(),
            domain,
            &ingress_request,
            &context,
        )
        .map_err(|err| {
            tracing::error!(provider = %parsed.provider, error = %err, "ingress dispatch failed");
            operator_log::error(
                module_path!(),
                format!("[ingress] dispatch failed provider={}: {err}", parsed.provider),
            );
            error_response(StatusCode::BAD_GATEWAY, err.to_string())
        })?
    };
    operator_log::info(
        module_path!(),
        format!(
            "[ingress] dispatch ok provider={} events={} envelopes={}",
            parsed.provider, result.events.len(), result.messaging_envelopes.len(),
        ),
    );
    if !result.events.is_empty() {
        operator_log::info(
            module_path!(),
            format!(
                "[demo ingress] parsed {} event(s) from provider={} tenant={} team={}",
                result.events.len(),
                parsed.provider,
                parsed.tenant,
                parsed.team
            ),
        );
    }
    if domain == Domain::Events && !result.events.is_empty() {
        route_events_to_default_flow(state.runner_host.bundle_root(), &context, &result.events)
            .map_err(|err| error_response(StatusCode::BAD_GATEWAY, err.to_string()))?;
    }
    if domain == Domain::Messaging && !result.messaging_envelopes.is_empty() {
        // Filter out bot self-messages to prevent echo loops (e.g. Webex bots see
        // their own replies as new webhook events).
        let envelopes: Vec<_> = result
            .messaging_envelopes
            .iter()
            .filter(|env| {
                let dominated_by_bot = env
                    .from
                    .as_ref()
                    .map(|f| f.id.ends_with(".bot") || f.id.ends_with("@webex.bot"))
                    .unwrap_or(false);
                if dominated_by_bot {
                    operator_log::debug(
                        module_path!(),
                        format!(
                            "[demo ingress] skipping bot self-message from={:?} id={}",
                            env.from, env.id
                        ),
                    );
                }
                !dominated_by_bot
            })
            .cloned()
            .collect();
        if envelopes.is_empty() {
            // All envelopes were bot self-messages — skip pipeline.
            return build_http_response(&result.response)
                .map_err(|err| error_response(StatusCode::INTERNAL_SERVER_ERROR, err));
        }
        let provider = parsed.provider.clone();
        let bundle = state.runner_host.bundle_root().to_path_buf();
        let ctx = context.clone();
        let runner_host = state.runner_host.clone();
        let tg_forms = state.tg_form_store.clone();
        // Run messaging pipeline in a background thread to avoid blocking the HTTP response.
        std::thread::spawn(move || {
            if let Err(err) =
                route_messaging_envelopes(&bundle, &runner_host, &provider, &ctx, envelopes, None, Some(&tg_forms))
            {
                operator_log::error(
                    module_path!(),
                    format!(
                        "[demo ingress] messaging pipeline failed provider={} err={err}",
                        provider
                    ),
                );
            }
        });
    }

    if debug_enabled {
        operator_log::debug(
            module_path!(),
            format!(
                "[demo dev] ingress outcome domain={} provider={} tenant={} team={} corr_id={:?} events={}",
                domains::domain_name(domain),
                parsed.provider,
                context.tenant,
                context.team.as_deref().unwrap_or("default"),
                correlation_id.as_deref().unwrap_or("none"),
                result.events.len(),
            ),
        );
    }

    build_http_response(&result.response)
        .map_err(|err| error_response(StatusCode::INTERNAL_SERVER_ERROR, err))
}

/// Run the messaging pipeline for ingress envelopes: app flow → render_plan → encode → send_payload.
fn route_messaging_envelopes(
    bundle: &Path,
    runner_host: &DemoRunnerHost,
    provider: &str,
    ctx: &OperatorContext,
    envelopes: Vec<ChannelMessageEnvelope>,
    bot_activities: Option<&BotActivityStore>,
    tg_form_store: Option<&TelegramFormStore>,
) -> anyhow::Result<()> {
    let _span = tracing::info_span!(
        "messaging_pipeline",
        provider = %provider,
        tenant = %ctx.tenant,
        team = ?ctx.team,
        envelope_count = envelopes.len(),
    )
    .entered();
    let team = ctx.team.as_deref();
    let app_pack_result = app::resolve_app_pack_path(bundle, &ctx.tenant, team, None);
    eprintln!("[directline] resolve_app_pack_path tenant={} team={:?} result={:?}", ctx.tenant, team, app_pack_result.as_ref().map(|p| p.display().to_string()));

    // Resolve pack path separately from flow — card routing only needs the pack.
    let app_pack_path = app_pack_result.ok();

    // Try to load flow info (may fail if pack has no flows — that's fine for card-only packs).
    let app_flow_context = app_pack_path.as_ref().and_then(|pack_path| {
        let pack_info = match app::load_app_pack_info(pack_path) {
            Ok(info) => {
                eprintln!("[directline] pack_info loaded: pack_id={} flows={:?}", info.pack_id, info.flows.iter().map(|f| &f.id).collect::<Vec<_>>());
                info
            }
            Err(e) => {
                eprintln!("[directline] load_app_pack_info failed: {e}");
                return None;
            }
        };
        match app::select_app_flow(&pack_info).map(|f| f.clone()) {
            Ok(flow) => {
                eprintln!("[directline] selected flow: id={} kind={}", flow.id, flow.kind);
                Some((pack_info, flow))
            }
            Err(e) => {
                eprintln!("[directline] select_app_flow failed: {e} — card-only mode");
                None
            }
        }
    });

    if let Some((ref pack_info, ref flow)) = app_flow_context {
        operator_log::info(
            module_path!(),
            format!(
                "[demo messaging] routing {} envelope(s) through app flow={} pack={}",
                envelopes.len(),
                flow.id,
                pack_info.pack_id
            ),
        );
    } else if app_pack_path.is_some() {
        eprintln!("[directline] app pack found but no flow, card-only mode for {} envelope(s)", envelopes.len());
    } else {
        eprintln!("[directline] no app pack found, using echo fallback for {} envelope(s)", envelopes.len());
    }

    // Pre-process Telegram form replies: when a user replies to a ForceReply
    // prompt (text input card), inject the typed text as the input field value
    // and auto-trigger the submit action.
    let envelopes: Vec<ChannelMessageEnvelope> = envelopes
        .into_iter()
        .map(|mut envelope| {
            let is_form_reply = envelope
                .metadata
                .get("is_form_reply")
                .map(|s| s.as_str())
                == Some("true");
            // Check for form reply (explicit reply-to-bot) or fallback
            // (plain text message while form is pending).
            let has_pending_form = is_form_reply
                || (tg_form_store.is_some()
                    && envelope.metadata.get("chat_id").is_some()
                    && envelope.metadata.get("action").is_none()
                    && envelope.metadata.get("routeToCardId").is_none()
                    && envelope.text.is_some());
            if has_pending_form {
                if let Some(store) = tg_form_store {
                    if let Some(chat_id) = envelope.metadata.get("chat_id").cloned() {
                        if let Some(form_state) = store.take(&chat_id) {
                            // Inject user's text as the first input field value
                            if let Some(text) = envelope.text.clone() {
                                if let Some(first_id) = form_state.input_ids.first() {
                                    eprintln!(
                                        "[telegram-form] injecting input {}={} from {} (chat_id={})",
                                        first_id,
                                        if text.len() > 8 {
                                            format!("{}...", &text[..8])
                                        } else {
                                            text.clone()
                                        },
                                        if is_form_reply { "form reply" } else { "pending form" },
                                        chat_id,
                                    );
                                    envelope
                                        .metadata
                                        .insert(first_id.clone(), text);
                                }
                            }
                            // Inject submit action data (e.g., action=save_token)
                            for (k, v) in form_state.submit_data {
                                envelope.metadata.insert(k, v);
                            }
                        }
                    }
                }
            }
            envelope
        })
        .collect();

    for envelope in &envelopes {
        let outputs = {
            // MCP tool dispatch: action=mcp triggers a real GitHub API call.
            if envelope.metadata.get("action").map(|s| s.as_str()) == Some("mcp") {
                let tool = envelope.metadata.get("tool").map(|s| s.as_str()).unwrap_or("");
                let owner = envelope.metadata.get("owner").cloned().unwrap_or_default();

                // Build args: for create_issue, assemble from form fields;
                // for other tools, parse the pre-built args JSON string.
                let args: serde_json::Value = if tool == "create_issue" {
                    let mut a = json!({});
                    // repo_choice format: "owner:repo" from dynamic form
                    if let Some(repo_choice) = envelope.metadata.get("repo_choice") {
                        if let Some((o, r)) = repo_choice.split_once(':') {
                            a["owner"] = json!(o);
                            a["repo"] = json!(r);
                        }
                    } else if let Some(repo) = envelope.metadata.get("repo") {
                        // Fallback: old static form with separate owner/repo
                        a["owner"] = json!(owner);
                        a["repo"] = json!(repo);
                    }
                    if let Some(title) = envelope.metadata.get("issueTitle") {
                        a["title"] = json!(title);
                    }
                    if let Some(body) = envelope.metadata.get("issueBody") {
                        if !body.is_empty() {
                            a["body"] = json!(body);
                        }
                    }
                    if let Some(labels) = envelope.metadata.get("labels") {
                        if !labels.is_empty() {
                            let label_list: Vec<&str> = labels.split(',').collect();
                            a["labels"] = json!(label_list);
                        }
                    }
                    a
                } else {
                    let args_str = envelope.metadata.get("args").cloned().unwrap_or_default();
                    serde_json::from_str(&args_str).unwrap_or(json!({}))
                };
                eprintln!("[directline] MCP dispatch tool={tool} args={args}");

                // Read GitHub token from secrets
                let token = read_github_token(bundle, ctx);
                match token {
                    Some(tok) => {
                        match crate::demo::github_mcp::call_tool(tool, &args, &tok) {
                            Ok(result) => {
                                let card = crate::demo::github_mcp::render_card(tool, &result, &owner);
                                eprintln!("[directline] MCP tool={tool} succeeded, rendering card");
                                build_card_reply(envelope, &card, &format!("mcp-{tool}"))
                            }
                            Err(err) => {
                                eprintln!("[directline] MCP tool={tool} failed: {err}");
                                let card = json!({
                                    "type": "AdaptiveCard", "version": "1.3",
                                    "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                                    "body": [
                                        {"type": "Container", "style": "attention", "items": [
                                            {"type": "TextBlock", "text": "\u{274c} Error", "size": "large", "weight": "bolder"},
                                            {"type": "TextBlock", "text": err, "wrap": true, "size": "small"}
                                        ]}
                                    ],
                                    "actions": [
                                        {"type": "Action.Submit", "title": "\u{2190} Back", "data": {"routeToCardId": "GH-connected"}}
                                    ]
                                });
                                build_card_reply(envelope, &card, "mcp-error")
                            }
                        }
                    }
                    None => {
                        eprintln!("[directline] no GitHub token found, showing auth card");
                        let card = json!({
                            "type": "AdaptiveCard", "version": "1.3",
                            "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                            "body": [
                                {"type": "Container", "style": "attention", "items": [
                                    {"type": "TextBlock", "text": "\u{1f511} Authentication Required", "size": "large", "weight": "bolder"},
                                    {"type": "TextBlock", "text": "No GitHub token found. Please set a Personal Access Token.", "wrap": true, "size": "small"}
                                ]},
                                {"type": "Container", "spacing": "medium", "items": [
                                    {"type": "TextBlock", "text": "GitHub Token (PAT)", "size": "small", "weight": "bolder"},
                                    {"type": "Input.Text", "id": "github_token", "placeholder": "ghp_..."}
                                ]}
                            ],
                            "actions": [
                                {"type": "Action.Submit", "title": "\u{1f511} Save Token", "style": "positive", "data": {"action": "save_token"}},
                                {"type": "Action.Submit", "title": "\u{2190} Back", "data": {"routeToCardId": "GH-welcome"}}
                            ]
                        });
                        build_card_reply(envelope, &card, "auth-required")
                    }
                }
            }
            // Save GitHub token action
            else if envelope.metadata.get("action").map(|s| s.as_str()) == Some("save_token") {
                if let Some(token) = envelope.metadata.get("github_token") {
                    if !token.is_empty() {
                        save_github_token(bundle, ctx, token);
                        eprintln!("[directline] GitHub token saved ({} chars)", token.len());

                        // Verify token and get username, then show connected card
                        match crate::demo::github_mcp::get_authenticated_user(token) {
                            Ok(username) => {
                                eprintln!("[directline] GitHub authenticated as: {username}");
                                let card = crate::demo::github_mcp::build_connected_card(&username);
                                build_card_reply(envelope, &card, "token-saved-connected")
                            }
                            Err(err) => {
                                eprintln!("[directline] GitHub token verification failed: {err}");
                                let card = json!({
                                    "type": "AdaptiveCard", "version": "1.3",
                                    "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                                    "body": [{"type": "Container", "style": "attention", "items": [
                                        {"type": "TextBlock", "text": "\u{274c} Token Invalid", "size": "large", "weight": "bolder"},
                                        {"type": "TextBlock", "text": format!("Could not authenticate: {err}"), "wrap": true, "size": "small"}
                                    ]}],
                                    "actions": [
                                        {"type": "Action.Submit", "title": "\u{1f511} Try Again", "data": {"routeToCardId": "GH-oauth-personal"}},
                                        {"type": "Action.Submit", "title": "\u{2190} Back", "data": {"routeToCardId": "GH-welcome"}}
                                    ]
                                });
                                build_card_reply(envelope, &card, "token-invalid")
                            }
                        }
                    } else {
                        echo_fallback(envelope)
                    }
                } else {
                    echo_fallback(envelope)
                }
            }
            // Special case: "GH-connected" generates a dynamic card with the user's GitHub info.
            else if envelope.metadata.get("routeToCardId").map(|s| s.as_str()) == Some("GH-connected") {
                let token = read_github_token(bundle, ctx);
                match token.and_then(|t| crate::demo::github_mcp::get_authenticated_user(&t).ok()) {
                    Some(username) => {
                        let card = crate::demo::github_mcp::build_connected_card(&username);
                        build_card_reply(envelope, &card, "GH-connected")
                    }
                    None => {
                        // No valid token — show welcome card instead
                        if let Some(pack_path) = &app_pack_path {
                            match read_card_from_pack(pack_path, "GH-welcome") {
                                Some(card_json) => build_card_reply(envelope, &card_json, "GH-welcome"),
                                None => echo_fallback(envelope),
                            }
                        } else {
                            echo_fallback(envelope)
                        }
                    }
                }
            }
            // GH-oauth-personal: generate dynamic token input card (never use static pack card)
            else if envelope.metadata.get("routeToCardId").map(|s| s.as_str()) == Some("GH-oauth-personal") {
                let card = json!({
                    "type": "AdaptiveCard", "version": "1.3",
                    "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                    "body": [
                        {"type": "Container", "style": "emphasis", "items": [
                            {"type": "TextBlock", "text": "\u{1f510} Connect GitHub (Personal)", "size": "large", "weight": "bolder", "wrap": true},
                            {"type": "TextBlock", "text": "Enter your Personal Access Token (PAT) to connect your GitHub account.", "size": "small", "isSubtle": true, "wrap": true, "spacing": "none"}
                        ]},
                        {"type": "Container", "spacing": "large", "items": [
                            {"type": "TextBlock", "text": "GitHub Personal Access Token", "size": "small", "weight": "bolder"},
                            {"type": "Input.Text", "id": "github_token", "placeholder": "ghp_xxxxxxxxxxxxxxxxxxxx"},
                            {"type": "TextBlock", "text": "Create a token at github.com \u{2192} Settings \u{2192} Developer settings \u{2192} Personal access tokens", "size": "small", "isSubtle": true, "wrap": true, "spacing": "small"}
                        ]},
                        {"type": "Container", "style": "accent", "spacing": "medium", "items": [
                            {"type": "TextBlock", "text": "Recommended scopes:", "weight": "bolder", "size": "small"},
                            {"type": "FactSet", "facts": [
                                {"title": "\u{2705} repo", "value": "Full repository access"},
                                {"title": "\u{2705} read:user", "value": "Read user profile"},
                                {"title": "\u{2705} notifications", "value": "Access notifications"}
                            ]}
                        ]}
                    ],
                    "actions": [
                        {"type": "Action.Submit", "title": "\u{1f511} Save & Connect", "style": "positive", "data": {"action": "save_token"}},
                        {"type": "Action.Submit", "title": "\u{2190} Back", "data": {"routeToCardId": "GH-welcome", "step": "back"}}
                    ]
                });
                build_card_reply(envelope, &card, "GH-oauth-personal")
            }
            // Card routing: if the envelope has routeToCardId and we have a pack, load the card.
            else if let (Some(route_to_card), Some(pack_path)) =
                (envelope.metadata.get("routeToCardId"), &app_pack_path)
            {
                match read_card_from_pack(pack_path, route_to_card) {
                    Some(card_json) => {
                        operator_log::info(
                            module_path!(),
                            format!(
                                "[demo messaging] card routing: {} -> card asset found",
                                route_to_card
                            ),
                        );
                        build_card_reply(envelope, &card_json, route_to_card)
                    }
                    None => {
                        operator_log::warn(
                            module_path!(),
                            format!(
                                "[demo messaging] card routing: {} -> card asset NOT found",
                                route_to_card
                            ),
                        );
                        if let (Some((pack_info, flow)), Some(pack_path)) =
                            (&app_flow_context, &app_pack_path)
                        {
                            run_app_flow_safe(bundle, ctx, pack_path, pack_info, flow, envelope)
                        } else {
                            echo_fallback(envelope)
                        }
                    }
                }
            } else if let Some(pack_path) = &app_pack_path {
                // No routeToCardId: try showing a welcome card first, then fall
                // back to running the app flow or echo.
                match read_card_from_pack(pack_path, "GH-welcome") {
                    Some(card_json) => {
                        eprintln!("[directline] showing GH-welcome card (first message)");
                        build_card_reply(envelope, &card_json, "GH-welcome")
                    }
                    None => {
                        if let Some((pack_info, flow)) = &app_flow_context {
                            run_app_flow_safe(bundle, ctx, pack_path, pack_info, flow, envelope)
                        } else {
                            echo_fallback(envelope)
                        }
                    }
                }
            } else {
                echo_fallback(envelope)
            }
        };

        for out_envelope in outputs {
            eprintln!(
                "[directline] processing reply envelope text={:?} id={} session_id={} channel={}",
                out_envelope.text.as_deref().unwrap_or(""),
                out_envelope.id,
                out_envelope.session_id,
                out_envelope.channel,
            );

            // For webchat card replies, skip the WASM egress pipeline entirely.
            // The WASM component's send_payload also writes to state store, which
            // creates a duplicate activity with slightly different format that
            // renders as "1 attachment" instead of the card.  We inject directly
            // via BotActivityStore which produces the correct format.
            let has_card = out_envelope.metadata.contains_key("adaptive_card");

            // Capture Telegram form state: when sending a card with Input.Text
            // fields to Telegram, store the expected inputs so we can inject
            // the user's reply text as the input value later.
            if provider == "messaging-telegram" && has_card {
                if let Some(store) = tg_form_store {
                    if let Some(form_state) = extract_form_state_from_card(&out_envelope) {
                        let chat_id = out_envelope
                            .metadata
                            .get("chat_id")
                            .or_else(|| out_envelope.to.first().map(|d| &d.id));
                        if let Some(chat_id) = chat_id {
                            eprintln!(
                                "[telegram-form] stored form state for chat_id={} inputs={:?}",
                                chat_id, form_state.input_ids
                            );
                            store.store(chat_id, form_state);
                        }
                    }
                }
            }

            if provider == "messaging-webchat" && has_card {
                if let Some(store) = bot_activities {
                    let conv_id = &out_envelope.session_id;
                    let activity_id = format!("bot-{}", uuid::Uuid::new_v4());
                    let mut activity = json!({
                        "type": "message",
                        "id": activity_id,
                        "from": {"id": "bot", "name": "Bot", "role": "bot"},
                        "conversation": {"id": conv_id},
                        "recipient": {"id": "user", "role": "user"},
                        "timestamp": chrono::Utc::now().to_rfc3339(),
                    });
                    if let Some(ac_json) = out_envelope.metadata.get("adaptive_card") {
                        if let Ok(ac_value) = serde_json::from_str::<serde_json::Value>(ac_json) {
                            activity["attachments"] = json!([{
                                "contentType": "application/vnd.microsoft.card.adaptive",
                                "content": ac_value,
                            }]);
                            activity["attachmentLayout"] = json!("list");
                        }
                    }
                    eprintln!("[directline] webchat card reply → direct inject (skip egress) conv={}", conv_id);
                    store.push(conv_id, activity);
                }
                continue;
            }

            let message_value = serde_json::to_value(&out_envelope)?;

            let plan = {
                let _span = info_span!("egress.render_plan", messaging.provider = %provider).entered();
                match egress::render_plan(runner_host, ctx, provider, message_value.clone())
                {
                    Ok(plan) => plan,
                    Err(err) => {
                        operator_log::warn(
                            module_path!(),
                            format!("[demo messaging] render_plan failed: {err}; using empty plan"),
                        );
                        json!({})
                    }
                }
            };

            let payload = match egress::encode_payload(
                runner_host,
                ctx,
                provider,
                message_value.clone(),
                plan,
            ) {
                Ok(payload) => payload,
                Err(err) => {
                    operator_log::warn(
                        module_path!(),
                        format!("[demo messaging] encode failed: {err}; using fallback payload"),
                    );
                    let body_bytes = serde_json::to_vec(&message_value)?;
                    ProviderPayloadV1 {
                        content_type: "application/json".to_string(),
                        body_b64: base64::engine::general_purpose::STANDARD.encode(&body_bytes),
                        metadata_json: Some(serde_json::to_string(&message_value)?),
                        metadata: None,
                    }
                }
            };

            let provider_type = runner_host.canonical_provider_type(Domain::Messaging, provider);
            let send_input =
                egress::build_send_payload(payload, &provider_type, &ctx.tenant, ctx.team.clone());
            let send_bytes = serde_json::to_vec(&send_input)?;
            let outcome = {
                let _span = info_span!("egress.send_payload", messaging.provider = %provider).entered();
                runner_host.invoke_provider_op(
                    Domain::Messaging,
                    provider,
                    "send_payload",
                    &send_bytes,
                    ctx,
                )?
            };

            let provider_ok = outcome
                .output
                .as_ref()
                .and_then(|v| v.get("ok"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if outcome.success && provider_ok {
                eprintln!("[directline] send succeeded provider={} envelope_id={}", provider, out_envelope.id);

                // For webchat text-only replies, also store in BotActivityStore
                if provider == "messaging-webchat" {
                    if let Some(store) = bot_activities {
                        let conv_id = &out_envelope.session_id;
                        let reply_text = out_envelope.text.as_deref().unwrap_or("");
                        let activity_id = format!("bot-{}", uuid::Uuid::new_v4());
                        let mut activity = json!({
                            "type": "message",
                            "id": activity_id,
                            "from": {"id": "bot", "name": "Bot", "role": "bot"},
                            "conversation": {"id": conv_id},
                            "recipient": {"id": "user", "role": "user"},
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                        });
                        if !reply_text.is_empty() {
                            activity["text"] = serde_json::Value::String(reply_text.to_string());
                        }
                        store.push(conv_id, activity);
                    }
                }
            } else {
                let provider_msg = outcome
                    .output
                    .as_ref()
                    .and_then(|v| v.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let err_msg = outcome
                    .error
                    .clone()
                    .unwrap_or_else(|| provider_msg.to_string());
                eprintln!("[directline] send FAILED provider={} err={}", provider, err_msg);
                operator_log::error(
                    module_path!(),
                    format!(
                        "[demo messaging] send failed provider={} provider_ok={} err={}",
                        provider, provider_ok, err_msg
                    ),
                );
            }
        }
    }
    Ok(())
}

/// Build a reply envelope containing an adaptive card.
fn build_card_reply(
    envelope: &greentic_types::ChannelMessageEnvelope,
    card_json: &serde_json::Value,
    card_key: &str,
) -> Vec<greentic_types::ChannelMessageEnvelope> {
    let mut reply = envelope.clone();
    reply.metadata.insert(
        "adaptive_card".to_string(),
        serde_json::to_string(card_json).unwrap_or_default(),
    );
    let summary = card_json
        .get("body")
        .and_then(|b| b.as_array())
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or(card_key)
        .to_string();
    reply.text = Some(summary);
    reply.id = uuid::Uuid::new_v4().to_string();
    vec![reply]
}

/// Echo fallback: reply with the same text prefixed with "[echo]".
fn echo_fallback(
    envelope: &greentic_types::ChannelMessageEnvelope,
) -> Vec<greentic_types::ChannelMessageEnvelope> {
    let mut reply = envelope.clone();
    let original = envelope.text.as_deref().unwrap_or("");
    reply.text = Some(format!("[echo] {}", original));
    reply.id = uuid::Uuid::new_v4().to_string();
    vec![reply]
}

/// Read a card JSON from the app pack's assets directory.
fn read_card_from_pack(pack_path: &std::path::Path, card_key: &str) -> Option<serde_json::Value> {
    let file = std::fs::File::open(pack_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let asset_path = format!("assets/cards/{card_key}.json");
    let mut entry = archive.by_name(&asset_path).ok()?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

/// Read GitHub token from the demo secrets store.
fn read_github_token(
    bundle: &std::path::Path,
    ctx: &crate::demo::runner_host::OperatorContext,
) -> Option<String> {
    let secrets_path = bundle
        .join(".greentic/dev/.dev.secrets.env");
    if let Ok(content) = std::fs::read_to_string(&secrets_path) {
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with("GITHUB_TOKEN=") || line.starts_with("github_token=") {
                let val = line.splitn(2, '=').nth(1)?.trim().to_string();
                if !val.is_empty() {
                    return Some(val);
                }
            }
        }
    }
    // Also check a dedicated token file
    let token_file = bundle.join(format!(
        ".greentic/dev/github_token_{}",
        ctx.tenant
    ));
    std::fs::read_to_string(&token_file).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Save GitHub token to the demo secrets store.
fn save_github_token(
    bundle: &std::path::Path,
    ctx: &crate::demo::runner_host::OperatorContext,
    token: &str,
) {
    let dir = bundle.join(".greentic/dev");
    let _ = std::fs::create_dir_all(&dir);
    let token_file = dir.join(format!("github_token_{}", ctx.tenant));
    let _ = std::fs::write(&token_file, token.trim());
}

/// Run the app flow, returning outputs or a fallback clone of the input envelope.
fn run_app_flow_safe(
    bundle: &std::path::Path,
    ctx: &crate::demo::runner_host::OperatorContext,
    app_pack_path: &std::path::Path,
    pack_info: &crate::messaging_universal::app::AppPackInfo,
    flow: &crate::messaging_universal::app::AppFlowInfo,
    envelope: &greentic_types::ChannelMessageEnvelope,
) -> Vec<greentic_types::ChannelMessageEnvelope> {
    match app::run_app_flow(
        bundle,
        ctx,
        app_pack_path,
        &pack_info.pack_id,
        &flow.id,
        envelope,
    ) {
        Ok(outputs) => outputs,
        Err(err) => {
            operator_log::error(
                module_path!(),
                format!("[demo messaging] app flow failed: {err}"),
            );
            vec![envelope.clone()]
        }
    }
}

/// Handle Direct Line API requests: /token, /v3/directline/*, /directline/*
async fn handle_directline_request(
    req: Request<Incoming>,
    path: &str,
    state: Arc<HttpIngressState>,
) -> Result<Response<Full<Bytes>>, Response<Full<Bytes>>> {
    let method = req.method().clone();
    let queries = collect_queries(req.uri().query());

    // Extract tenant from query param, JWT Authorization header, or default
    let tenant = queries
        .iter()
        .find(|(k, _)| k == "tenant")
        .map(|(_, v)| v.clone())
        .or_else(|| extract_tenant_from_jwt(req.headers()))
        .unwrap_or_else(|| "default".to_string());

    let provider = "messaging-webchat".to_string();
    if !state.domains.contains(&Domain::Messaging) {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "messaging domain disabled",
        ));
    }

    // Map /token to the Direct Line tokens/generate path
    let dl_path = if path == "/token" {
        "/v3/directline/tokens/generate".to_string()
    } else {
        path.to_string()
    };

    // Extract conversation_id from the path before dl_path is moved
    let conv_id = dl_path
        .strip_prefix("/v3/directline/conversations/")
        .and_then(|rest| rest.split('/').next())
        .map(|s| s.to_string());

    let headers = collect_headers(req.headers());
    let payload_bytes = req
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();

    let context = OperatorContext {
        tenant: tenant.clone(),
        team: Some("default".to_string()),
        correlation_id: None,
    };

    let ingress_request = IngressRequestV1 {
        v: 1,
        domain: "messaging".to_string(),
        provider: provider.clone(),
        handler: None,
        tenant: tenant.clone(),
        team: Some("default".to_string()),
        method: method.as_str().to_string(),
        path: dl_path,
        query: queries,
        headers,
        body: payload_bytes.to_vec(),
        correlation_id: None,
        remote_addr: None,
    };

    let result = dispatch_http_ingress(
        state.runner_host.as_ref(),
        Domain::Messaging,
        &ingress_request,
        &context,
    )
    .map_err(|err| {
        eprintln!("[directline] dispatch FAILED path={path} err={err}");
        error_response(StatusCode::BAD_GATEWAY, err.to_string())
    })?;

    eprintln!(
        "[directline] dispatch ok path={path} status={} envelopes={} body_len={}",
        result.response.status,
        result.messaging_envelopes.len(),
        result.response.body.as_ref().map(|b| b.len()).unwrap_or(0),
    );
    if let Some(ref body) = result.response.body {
        if body.len() < 2000 {
            eprintln!("[directline] body={}", String::from_utf8_lossy(body));
        }
    }

    // Route messaging envelopes through the pipeline (app flow → encode → send).
    // Run synchronously so bot activities are available for the next GET poll.
    if !result.messaging_envelopes.is_empty() {
        let envelopes = result.messaging_envelopes.clone();
        let bundle = state.runner_host.bundle_root().to_path_buf();
        eprintln!("[directline] routing {} envelope(s) through messaging pipeline", envelopes.len());
        if let Err(err) =
            route_messaging_envelopes(&bundle, &state.runner_host, &provider, &context, envelopes, Some(&state.bot_activities), Some(&state.tg_form_store))
        {
            eprintln!("[directline] messaging pipeline FAILED err={err}");
        } else {
            eprintln!("[directline] messaging pipeline completed ok");
        }
    }

    // For GET /activities requests, inject any pending bot activities
    let mut response = result.response;
    if method == Method::GET {
        if let Some(ref cid) = conv_id {
            let pending = state.bot_activities.drain(cid);
            if !pending.is_empty() {
                eprintln!("[directline] injecting {} bot activities for conv={} body_before={}", pending.len(), cid, response.body.as_ref().map(|b| b.len()).unwrap_or(0));
                if let Some(ref body_bytes) = response.body {
                    if let Ok(mut body_json) = serde_json::from_slice::<serde_json::Value>(body_bytes) {
                        if let Some(activities) = body_json.get_mut("activities").and_then(|a| a.as_array_mut()) {
                            activities.extend(pending);
                        }
                        // Increment watermark so Direct Line client recognises new activities
                        if let Some(wm) = body_json.get("watermark").and_then(|w| w.as_str()) {
                            if let Ok(n) = wm.parse::<u64>() {
                                body_json["watermark"] = json!((n + 1).to_string());
                            }
                        }
                        if let Ok(new_body) = serde_json::to_vec(&body_json) {
                            eprintln!("[directline] body_after_inject len={}", new_body.len());
                            // Dump to file for debugging
                            let _ = std::fs::write("/tmp/dl-inject-debug.json", serde_json::to_string_pretty(&body_json).unwrap_or_default());
                            response.body = Some(new_body);
                            // Remove stale Content-Length so hyper recalculates it.
                            response.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-length"));
                        }
                    }
                }
            }
        }
    }

    build_http_response(&response)
        .map_err(|err| error_response(StatusCode::INTERNAL_SERVER_ERROR, err))
}

/// Extract tenant from JWT Authorization bearer token claims.
///
/// The JWT payload contains `{"ctx":{"tenant":"..."}}`; we decode the payload
/// segment (base64url, no signature verification needed here since the WASM
/// component validates the token) and pull out the tenant field.
fn extract_tenant_from_jwt(headers: &hyper::HeaderMap) -> Option<String> {
    let auth = headers.get("authorization")?.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ")?.trim();
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(parts[1]))
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    value
        .get("ctx")
        .and_then(|ctx| ctx.get("tenant"))
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
}

struct GuiRouteMatch {
    tenant: String,
    asset_path: String,
}

/// Parse `/v1/messaging/{provider}/{tenant}/{team}/gui/[rest]` and return the
/// tenant and asset sub-path.  Returns `None` when the URL does not match.
/// Requires trailing slash on `/gui/`.
fn parse_gui_route(path: &str) -> Option<GuiRouteMatch> {
    // Expected: /v1/messaging/{provider}/{tenant}/{team}/gui/[...]
    let segs: Vec<&str> = path.trim_start_matches('/').splitn(7, '/').collect();
    // segs: [v1, messaging, provider, tenant, team, gui, ...rest]
    if segs.len() < 6 {
        return None;
    }
    if segs[0] != "v1" || segs[1] != "messaging" || segs[5] != "gui" {
        return None;
    }
    // Must have trailing slash (segs.len() > 6 because splitn produces "" after "gui/").
    if segs.len() <= 6 {
        return None;
    }
    Some(GuiRouteMatch {
        tenant: segs[3].to_string(),
        asset_path: segs[6].to_string(),
    })
}

/// Returns true when the path is exactly `/v1/messaging/{provider}/{tenant}/{team}/gui`
/// (no trailing slash) so the caller can issue a redirect.
fn path_needs_gui_trailing_slash(path: &str) -> bool {
    let segs: Vec<&str> = path.trim_start_matches('/').splitn(7, '/').collect();
    segs.len() == 6 && segs[0] == "v1" && segs[1] == "messaging" && segs[5] == "gui"
}

/// Serve a static file from the SPA assets directory, falling back to index.html
/// for SPA client-side routing.  When `tenant` is provided and the response is
/// index.html, a small `<script>` block is injected to set `window.__TENANT__`
/// and `window.__BASE_PATH__` so the SPA resolves skin/assets correctly.
fn serve_spa_file(
    spa_dir: &Path,
    request_path: &str,
    tenant: Option<&str>,
) -> Result<Response<Full<Bytes>>, Response<Full<Bytes>>> {
    // Normalise path and prevent directory traversal.
    let clean = request_path.trim_start_matches('/');
    if clean.contains("..") {
        return Err(error_response(StatusCode::BAD_REQUEST, "invalid path"));
    }

    // Try the exact file first; fall back to index.html (SPA routing).
    let serving_index;
    let file_path = if clean.is_empty() {
        serving_index = true;
        spa_dir.join("index.html")
    } else {
        let candidate = spa_dir.join(clean);
        if candidate.is_file() {
            serving_index = false;
            candidate
        } else {
            serving_index = true;
            spa_dir.join("index.html")
        }
    };

    let mut body = match std::fs::read(&file_path) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err(error_response(StatusCode::NOT_FOUND, "file not found"));
        }
    };

    // Inject tenant globals into index.html so the SPA knows which tenant to load.
    if serving_index {
        if let Some(tenant) = tenant {
            let inject = format!(
                "<script>window.__TENANT__=\"{}\";window.__BASE_PATH__=\"./\";</script>",
                tenant.replace('\\', "\\\\").replace('"', "\\\"")
            );
            let html = String::from_utf8_lossy(&body);
            let patched = html.replace("<head>", &format!("<head>{inject}"));
            body = patched.into_bytes();
        }
    }

    let content_type = match file_path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    };

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type)
        .body(Full::from(Bytes::from(body)))
        .unwrap())
}

fn cors_preflight_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        .header(
            "Access-Control-Allow-Headers",
            "Content-Type, Authorization, X-Requested-With, x-ms-bot-agent",
        )
        .header("Access-Control-Max-Age", "86400")
        .body(Full::from(Bytes::new()))
        .unwrap()
}

fn with_cors(mut response: Response<Full<Bytes>>) -> Response<Full<Bytes>> {
    let headers = response.headers_mut();
    headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    headers.insert(
        "Access-Control-Allow-Methods",
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        "Access-Control-Allow-Headers",
        HeaderValue::from_static("Content-Type, Authorization, X-Requested-With, x-ms-bot-agent"),
    );
    response
}

fn build_http_response(response: &IngressHttpResponse) -> Result<Response<Full<Bytes>>, String> {
    let mut builder = Response::builder().status(response.status);
    let mut has_content_type = false;
    for (name, value) in &response.headers {
        if let (Ok(header_name), Ok(header_value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            if header_name == CONTENT_TYPE {
                has_content_type = true;
            }
            builder = builder.header(header_name, header_value);
        }
    }
    if !has_content_type {
        builder = builder.header(CONTENT_TYPE, "application/json");
    }
    let body = response.body.clone().unwrap_or_default();
    builder
        .body(Full::from(Bytes::from(body)))
        .map_err(|err| err.to_string())
}

fn collect_headers(headers: &hyper::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.to_string(), value.to_string()))
        })
        .collect()
}

fn collect_queries(query: Option<&str>) -> Vec<(String, String)> {
    query
        .map(|value| {
            value
                .split('&')
                .filter_map(|pair| {
                    let mut pieces = pair.splitn(2, '=');
                    let key = pieces.next()?.trim();
                    if key.is_empty() {
                        return None;
                    }
                    let value = pieces.next().unwrap_or("").trim();
                    Some((key.to_string(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_domain(value: &str) -> Option<Domain> {
    match value.to_lowercase().as_str() {
        "messaging" => Some(Domain::Messaging),
        "events" => Some(Domain::Events),
        "secrets" => Some(Domain::Secrets),
        "oauth" => Some(Domain::OAuth),
        _ => None,
    }
}

#[derive(Clone, Debug)]
struct ParsedIngressRoute {
    domain: Domain,
    provider: String,
    tenant: String,
    team: String,
    handler: Option<String>,
}

fn parse_route_segments(path: &str) -> Option<ParsedIngressRoute> {
    let segments = path
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return None;
    }
    if segments[0].eq_ignore_ascii_case("v1") {
        return parse_v1_route(&segments);
    }
    parse_legacy_route(&segments)
}

fn parse_v1_route(segments: &[&str]) -> Option<ParsedIngressRoute> {
    if segments.len() < 5 || !segments[2].eq_ignore_ascii_case("ingress") {
        return None;
    }
    let domain = parse_domain(segments[1])?;
    let provider = segments[3].to_string();
    let tenant = segments[4].to_string();
    let team = segments.get(5).copied().unwrap_or("default").to_string();
    let handler = segments.get(6).map(|value| (*value).to_string());
    Some(ParsedIngressRoute {
        domain,
        provider,
        tenant,
        team,
        handler,
    })
}

fn parse_legacy_route(segments: &[&str]) -> Option<ParsedIngressRoute> {
    if segments.len() < 4 || !segments[1].eq_ignore_ascii_case("ingress") {
        return None;
    }
    let domain = parse_domain(segments[0])?;
    let provider = segments[2].to_string();
    let tenant = segments[3].to_string();
    let team = segments.get(4).copied().unwrap_or("default").to_string();
    let handler = segments.get(5).map(|value| (*value).to_string());
    Some(ParsedIngressRoute {
        domain,
        provider,
        tenant,
        team,
        handler,
    })
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response<Full<Bytes>> {
    let body = json!({
        "success": false,
        "message": message.into()
    });
    json_response(status, body)
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response<Full<Bytes>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v1_route_with_optional_segments() {
        let parsed = parse_route_segments("/v1/events/ingress/provider-a/tenant-x/team-y/h1")
            .expect("route should parse");
        assert_eq!(parsed.domain, Domain::Events);
        assert_eq!(parsed.provider, "provider-a");
        assert_eq!(parsed.tenant, "tenant-x");
        assert_eq!(parsed.team, "team-y");
        assert_eq!(parsed.handler.as_deref(), Some("h1"));
    }

    #[test]
    fn parses_legacy_route_for_compatibility() {
        let parsed = parse_route_segments("/messaging/ingress/provider-a/tenant-x")
            .expect("route should parse");
        assert_eq!(parsed.domain, Domain::Messaging);
        assert_eq!(parsed.team, "default");
    }
}
