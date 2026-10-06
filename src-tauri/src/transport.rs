use crate::agents::{authenticate_token_in_pool, provision_default_token_in_pool, AgentIdentity};
use crate::commands::refresh_repo;
use crate::identity::{self, verify_user_token_in_pool};
use crate::requests::{announce_review_in_pool, Actor as RequestActor};
use crate::reviews::{ingest_submission_in_pool, Actor, SubmissionPayload};
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::broadcast;

// Coarse pre-parse guard: the ingest's 2 MiB payload cap is the
// authoritative size rule; this only bounds what the transport reads, with
// 1 MiB of slack for JSON framing and identity fields so an ingest-legal
// submission is never size-rejected here.
const TRANSPORT_BODY_GUARD_BYTES: usize = 3 * 1024 * 1024;

// A connection that neither delivers nor closes inside this window is cut;
// real deliveries are single sub-3 MiB loopback requests.
const CONNECTION_STALL_LIMIT: Duration = Duration::from_secs(30);

// Idle poll period for the accept loop's exit check; a restart or app exit
// breaks the loop within this window.
const ACCEPT_POLL: Duration = Duration::from_millis(200);

// The agent faces answer small JSON or plain text; anything larger is a bug.
const RESPONSE_BODY_LIMIT: usize = 1024 * 1024;
// The command API is the human face over the same Git-backed reads local
// projects take through IPC without a cap, so its answers follow the Git
// layer's 16 MiB output ceiling (docs/performance.md), doubled to admit
// JSON escaping's worst case (every byte escaped).
const COMMAND_BODY_LIMIT: usize = 2 * crate::git::MAX_OUTPUT;
// A request head beyond this is refused before parsing continues.
const REQUEST_HEAD_LIMIT: usize = 64 * 1024;

// A quiet SSE subscriber is cut once no frame and no keepalive has been
// written for this long; every write, keepalives included, restarts the
// window.
const SSE_IDLE_LIMIT: Duration = CONNECTION_STALL_LIMIT;
// Must stay below SSE_IDLE_LIMIT, or an idle stream is cut before its
// first keepalive.
const SSE_KEEPALIVE_PERIOD: Duration = Duration::from_secs(15);

pub(crate) const PARSE_ERROR: i32 = -32700;
pub(crate) const INVALID_REQUEST: i32 = -32600;
pub(crate) const METHOD_NOT_FOUND: i32 = -32601;
pub(crate) const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;
pub(crate) const UNAUTHORIZED: i32 = -32001;
const UNKNOWN_REVIEW_TARGET: i32 = -32002;
const REQUEST_TOO_LARGE: i32 = -32003;

// Webview notification pushed after a successful ingest.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SubmissionArrival {
    pub(crate) repo_path: String,
    pub(crate) base_sha: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
    pub(crate) submission_id: i64,
    pub(crate) agent_name: String,
}

// The webview notification is injected as a sink so the handler matrix is
// testable without a running app; production wires the Tauri emit.
pub(crate) type ArrivalSink = Arc<dyn Fn(SubmissionArrival) + Send + Sync>;

// Announces a completed refresh or, after `add_repo`, a newly registered
// repository (the `project-refreshed` event); injected
// like ArrivalSink so the refresh path is testable without an app.
pub(crate) type RefreshSink = Arc<dyn Fn(&str) + Send + Sync>;

// Webview notification pushed after a successful agent comment mutation on
// the MCP face. Human IPC mutations announce nothing: the invoking renderer
// owns its refetch, and a submission's arrival keeps announcing itself
// through `submission-received`.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct CommentChange {
    pub(crate) repo_path: String,
    pub(crate) base_sha: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
    pub(crate) comment_id: i64,
    pub(crate) action: &'static str,
    pub(crate) agent_name: String,
}

// Announces a comment change (the `comment-changed` event); injected like
// ArrivalSink so the tool handlers are testable without an app.
pub(crate) type CommentSink = Arc<dyn Fn(CommentChange) + Send + Sync>;

// Webview notification pushed after a successful review-request mutation.
// The request engine fires it centrally, so every surface (the MCP tools
// and the human IPC commands) inherits emission without per-site wiring.
// The event names the log kind that narrates the mutation; it is None
// when the mutation updated in place without narrating.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct RequestChange {
    // The webview payload names the row's key `request_id`.
    #[serde(rename = "request_id")]
    pub(crate) id: i64,
    pub(crate) repo_path: String,
    pub(crate) base_sha: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
    pub(crate) status: String,
    pub(crate) event: Option<String>,
}

// Announces a review-request change (the `review-request-changed` event);
// injected like CommentSink so the request engine is testable without an
// app.
pub(crate) type RequestChangeSink = Arc<dyn Fn(RequestChange) + Send + Sync>;

// One push on the SSE stream: the same event name and JSON payload the
// desktop bridge emits, fanned out through a broadcast channel on the
// server.
#[derive(Debug, Clone)]
pub(crate) struct PushEvent {
    pub(crate) kind: &'static str,
    pub(crate) payload: Value,
}

// Shared, live view of the listener for the Settings MCP section. The
// startup path writes it and the get_mcp_status command reads it.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ListenerStatus {
    pub(crate) enabled: bool,
    pub(crate) running: bool,
    pub(crate) address: String,
    pub(crate) port: u16,
    pub(crate) error: Option<String>,
    // Constant per process: where agents find the endpoint config file and
    // the published skill copy. Set once at startup, carried across
    // restarts.
    pub(crate) config_path: String,
    pub(crate) skill_dir: String,
}

#[derive(Clone)]
pub(crate) struct McpStatusHandle(pub(crate) Arc<Mutex<ListenerStatus>>);

impl McpStatusHandle {
    // One handle serves the listener's whole lifetime, including restarts:
    // startup and the restart command share it so get_mcp_status readers
    // never hold a stale object.
    pub(crate) fn for_config(config: &ListenerConfig) -> Self {
        let status = Self(Arc::new(Mutex::new(ListenerStatus {
            enabled: false,
            running: false,
            address: String::new(),
            port: 0,
            error: None,
            config_path: String::new(),
            skill_dir: String::new(),
        })));
        status.reset_from_config(config);
        status
    }

    // The paths never change for a running process, so startup sets them
    // once and every reset keeps them.
    pub(crate) fn set_paths(&self, data_dir: &Path) {
        let mut live = self.lock_status();
        live.config_path = endpoint_config_path(data_dir).display().to_string();
        live.skill_dir = crate::home::skill_dir(data_dir).display().to_string();
    }

    // Every listener start, boot or restart, resets the shared status
    // from the config it is given.
    pub(crate) fn reset_from_config(&self, config: &ListenerConfig) {
        let mut live = self.lock_status();
        live.enabled = config.enabled;
        live.address = config.address.clone();
        live.port = config.port;
        live.running = false;
        live.error = None;
    }

    pub(crate) fn lock_status(&self) -> std::sync::MutexGuard<'_, ListenerStatus> {
        self.0.lock().expect("listener status mutex poisoned")
    }

    fn set(&self, running: bool, error: Option<String>) {
        let mut status = self.lock_status();
        status.running = running;
        status.error = error;
    }
}

// One summary line for the tray menu, the one listener surface that cannot
// read the JSON status; boot and the restart command refresh it.
pub(crate) fn tray_status_line(status: &ListenerStatus) -> String {
    if !status.enabled {
        "Agent API is off".into()
    } else if status.running {
        format!("Agent API at {}:{}", status.address, status.port)
    } else {
        "Agent API not running".into()
    }
}

// What the listener needs to bind: everything comes from Settings.
#[derive(Debug, Clone)]
pub(crate) struct ListenerConfig {
    pub(crate) enabled: bool,
    pub(crate) address: String,
    pub(crate) port: u16,
}

// The fixed surroundings a listener start (boot or restart) needs beyond
// the config: captured once at app startup and managed so the restart
// command can rebuild the listener with the same sinks.
#[derive(Clone)]
pub(crate) struct TransportDeps {
    pub(crate) data_dir: PathBuf,
    pub(crate) arrivals: ArrivalSink,
    pub(crate) refreshes: RefreshSink,
    pub(crate) comment_changes: CommentSink,
    pub(crate) request_changes: RequestChangeSink,
}

#[derive(Clone)]
pub(crate) struct TransportState {
    // The MCP face shares the pool and the event sinks; auth has already
    // resolved the identity by the time it sees the state. All fields are
    // crate-visible so handler tests in sibling modules can build a state.
    pub(crate) pool: SqlitePool,
    pub(crate) arrivals: ArrivalSink,
    pub(crate) refreshes: RefreshSink,
    pub(crate) comment_changes: CommentSink,
    pub(crate) request_changes: RequestChangeSink,
    pub(crate) status: McpStatusHandle,
    // Fan-out for the push events on the server, wired where the desktop
    // wires its sinks to the webview; the SSE route subscribes to it. None
    // on the desktop, where GET /events keeps its generic answer.
    pub(crate) events: Option<broadcast::Sender<PushEvent>>,
}

#[derive(Deserialize)]
struct ReviewIdentityParams {
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshRepoParams {
    repo_path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnnounceReviewParams {
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    head_sha: String,
    note: Option<String>,
}

#[derive(Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

pub(crate) fn rpc_error(id: &Value, status: StatusCode, code: i32, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message.into() }
        })),
    )
        .into_response()
}

// The presented secret, extracted before any await: the request body's
// trait object is not Sync, and connection tasks are spawned, so no borrow
// of a request may cross an await.
fn bearer_secret(request: &Request) -> Option<String> {
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
}

// Bearer authentication is evaluated only here, once per request: the
// presented secret is hashed and matched against the agent_tokens table
// first, then the user_tokens rows. The two secret spaces are disjoint
// (independent 256-bit draws), so a secret resolves to exactly one actor
// kind. Revoked, unknown, and missing secrets all answer identically so
// callers cannot probe which tokens exist.
async fn authenticate(state: &TransportState, secret: Option<String>) -> Option<AuthenticatedActor> {
    let secret = secret?;
    if let Some(identity) = authenticate_token_in_pool(&state.pool, &secret).await {
        return Some(AuthenticatedActor::Agent(identity));
    }
    verify_user_token_in_pool(&state.pool, &secret)
        .await
        .map(AuthenticatedActor::Human)
}

// The caller a request's bearer resolved to. Each face accepts one kind:
// the agent JSON-RPC/MCP face runs on agent tokens only, the command API
// accepts human tokens, and SSE accepts both kinds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AuthenticatedActor {
    Agent(AgentIdentity),
    Human(identity::User),
}

// The one refusal shape for a failed bearer evaluation, shared by the
// request handlers and the SSE route.
fn unauthorized_response() -> Response {
    rpc_error(
        &Value::Null,
        StatusCode::UNAUTHORIZED,
        UNAUTHORIZED,
        "Missing, wrong, or deleted bearer token. Discovery clients: re-read the config file for the current boot.",
    )
}

pub(crate) async fn handle(State(state): State<TransportState>, request: Request) -> Response {
    // The presented secret is extracted before any await: connection tasks
    // are spawned and the request body's trait object is not Sync, so no
    // borrow of a request may cross an await.
    let secret = bearer_secret(&request);
    let path = request.uri().path().to_string();
    let actor = match authenticate(&state, secret).await {
        Some(actor) => actor,
        None => {
            // The presented secret is never logged, only the miss. Each
            // face refuses in its own shape; the resolution stays shared.
            log::warn!("endpoint rejected an unauthorized request to {}", path);
            if path.starts_with("/api/") {
                return crate::dispatch::unauthorized(
                    "Missing, wrong, or deleted bearer token.",
                );
            }
            return unauthorized_response();
        }
    };
    let bytes = match to_bytes(request.into_body(), TRANSPORT_BODY_GUARD_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return rpc_error(
                &Value::Null,
                StatusCode::PAYLOAD_TOO_LARGE,
                REQUEST_TOO_LARGE,
                format!("The request body exceeds the {TRANSPORT_BODY_GUARD_BYTES} byte guard."),
            );
        }
    };
    // One listener, three faces: the MCP and raw faces require an agent
    // actor, the command API requires a human actor, and each face's
    // refusal keeps its own shape (the agent face's JSON-RPC error matrix
    // is unchanged).
    if let Some(command) = path.strip_prefix("/api/") {
        return match actor {
            AuthenticatedActor::Human(user) => {
                crate::dispatch::serve(&state, &user, command, &bytes).await
            }
            AuthenticatedActor::Agent(_) => {
                log::warn!("command endpoint rejected an agent token calling {}", path);
                crate::dispatch::unauthorized(
                    "An agent token cannot call the command API; authenticate with a human token.",
                )
            }
        };
    }
    let identity = match actor {
        AuthenticatedActor::Agent(identity) => identity,
        AuthenticatedActor::Human(_) => {
            log::warn!("agent endpoint rejected a human token calling {}", path);
            return unauthorized_response();
        }
    };
    if path == "/mcp" {
        return crate::mcp::serve(&state, identity, &bytes).await;
    }
    let RpcRequest { jsonrpc, id, method, params } = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(_) => {
            return rpc_error(
                &Value::Null,
                StatusCode::OK,
                PARSE_ERROR,
                "The request body is not valid JSON.",
            );
        }
    };
    if jsonrpc != "2.0" {
        return rpc_error(&id, StatusCode::OK, INVALID_REQUEST, "The request is not JSON-RPC 2.0.");
    }
    match method.as_str() {
        "post_review" => handle_post_review(state, id, params, identity).await,
        "announce_review" => handle_announce_review(state, id, params, identity).await,
        "refresh_repo" => handle_refresh_repo(state, id, params).await,
        _ => rpc_error(
            &id,
            StatusCode::OK,
            METHOD_NOT_FOUND,
            "Unknown method; the endpoint accepts post_review and refresh_repo, plus announce_review.",
        ),
    }
}

async fn handle_post_review(
    state: TransportState,
    id: Value,
    params: Value,
    identity: AgentIdentity,
) -> Response {
    // serde flatten would silently absorb unknown keys, defeating the
    // payload's strict schema: validate the identity fields alone, strip
    // them, and let the payload's own deny_unknown_fields hold the line.
    let identity_fields: ReviewIdentityParams = match serde_json::from_value(params.clone()) {
        Ok(identity_fields) => identity_fields,
        Err(error) => {
            return rpc_error(&id, StatusCode::OK, INVALID_PARAMS, format!("Invalid post_review params: {error}"));
        }
    };
    if identity_fields.target_kind != "worktree" && identity_fields.target_kind != "head" {
        return rpc_error(
            &id,
            StatusCode::OK,
            INVALID_PARAMS,
            r#"target_kind must be "worktree" or "head"."#,
        );
    }
    let Value::Object(mut fields) = params else {
        return rpc_error(&id, StatusCode::OK, INVALID_PARAMS, "post_review params must be an object.");
    };
    for key in ["repo_path", "base_sha", "target_key", "target_kind"] {
        fields.remove(key);
    }
    let submission: SubmissionPayload = match serde_json::from_value(Value::Object(fields)) {
        Ok(submission) => submission,
        Err(error) => {
            return rpc_error(&id, StatusCode::OK, INVALID_PARAMS, format!("Invalid submission payload: {error}"));
        }
    };
    match ingest_submission_in_pool(
        &state.pool,
        &identity_fields.repo_path,
        &identity_fields.base_sha,
        &identity_fields.target_key,
        &identity_fields.target_kind,
        &submission,
        &Actor::Agent(identity),
        &state.request_changes,
    )
    .await
    {
        Ok(submission_id) => {
            (state.arrivals)(SubmissionArrival {
                repo_path: identity_fields.repo_path,
                base_sha: identity_fields.base_sha,
                target_key: identity_fields.target_key,
                target_kind: identity_fields.target_kind,
                submission_id,
                agent_name: submission.agent_name().to_string(),
            });
            (
                StatusCode::OK,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "submission_id": submission_id }
                })),
            )
                .into_response()
        }
        Err(error) if error.code == "invalid_submission" => {
            rpc_error(&id, StatusCode::OK, INVALID_PARAMS, error.message)
        }
        Err(error) if error.code == "unknown_review_target" => {
            rpc_error(&id, StatusCode::OK, UNKNOWN_REVIEW_TARGET, error.message)
        }
        Err(error) => rpc_error(&id, StatusCode::OK, INTERNAL_ERROR, error.message),
    }
}

// The raw face's announce method: one strict params schema (identity
// fields and payload together), the shared unknown-repo gate, and the
// engine entry the MCP tool calls; refusals pass through like
// post_review's.
async fn handle_announce_review(
    state: TransportState,
    id: Value,
    params: Value,
    identity: AgentIdentity,
) -> Response {
    let params: AnnounceReviewParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return rpc_error(
                &id,
                StatusCode::OK,
                INVALID_PARAMS,
                format!("Invalid announce_review params: {error}"),
            );
        }
    };
    if params.target_kind != "worktree" && params.target_kind != "head" {
        return rpc_error(
            &id,
            StatusCode::OK,
            INVALID_PARAMS,
            r#"target_kind must be "worktree" or "head"."#,
        );
    }
    let known: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repos WHERE path = ?")
        .bind(&params.repo_path)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
    if known == 0 {
        return rpc_error(
            &id,
            StatusCode::OK,
            UNKNOWN_REVIEW_TARGET,
            "No repository with that path is open in WorktreeView.",
        );
    }
    match announce_review_in_pool(
        &state.pool,
        &params.repo_path,
        &params.base_sha,
        &params.target_key,
        &params.target_kind,
        &params.head_sha,
        params.note.as_deref(),
        &RequestActor::Agent(identity.token_id),
        &state.request_changes,
    )
    .await
    {
        Ok(request) => (
            StatusCode::OK,
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "request": request }
            })),
        )
            .into_response(),
        Err(error) if error.code == "invalid_request" => {
            rpc_error(&id, StatusCode::OK, INVALID_PARAMS, error.message)
        }
        Err(error) => rpc_error(&id, StatusCode::OK, INTERNAL_ERROR, error.message),
    }
}

async fn handle_refresh_repo(state: TransportState, id: Value, params: Value) -> Response {
    let params: RefreshRepoParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return rpc_error(&id, StatusCode::OK, INVALID_PARAMS, format!("Invalid refresh_repo params: {error}"));
        }
    };
    let known: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repos WHERE path = ?")
        .bind(&params.repo_path)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
    if known == 0 {
        return rpc_error(
            &id,
            StatusCode::OK,
            UNKNOWN_REVIEW_TARGET,
            "No repository with that path is open in WorktreeView.",
        );
    }
    match refresh_repo(&state.pool, &params.repo_path, &state.refreshes).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "ok": true }
            })),
        )
            .into_response(),
        Err(error) => rpc_error(&id, StatusCode::OK, INTERNAL_ERROR, error.message),
    }
}

pub(crate) struct TransportHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TransportHandle {
    // Best-effort exit signal at app exit: the accept loop breaks within
    // one idle poll period.
    pub(crate) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    // Full stop for restarts: signal, then wait for the thread so the
    // socket is released and config cleanup has run before a new
    // listener may rebind the same address. Bounded by one idle poll
    // period; connection tasks end with the runtime and an in-flight
    // request is cut at its next await rather than waited out.
    pub(crate) fn stop(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// The running listener's stop handle, held by the app so a restart can
// take the current listener out before binding a new one. The lock is
// async-aware and held across a whole restart so overlapping restart
// commands serialize instead of orphaning a listener outside the owner.
#[derive(Default)]
pub(crate) struct ListenerOwner(pub(crate) tokio::sync::Mutex<Option<TransportHandle>>);

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct EndpointDiscovery {
    port: u16,
    token: String,
}

// The endpoint payload is the whole config file today; later keys may join
// it. One name for every channel: with their default homes, debug and
// release builds do not share a directory, so they cannot read or delete
// each other's registration.
pub(crate) fn endpoint_config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.json")
}

// A later-started instance may have overwritten the config file with its
// own registration; removing that would orphan its endpoint, so only delete
// a file that still describes this instance.
fn remove_endpoint_config_if_owned(path: &Path, port: u16, token: &str) {
    let Ok(payload) = std::fs::read(path) else {
        return;
    };
    if let Ok(discovery) = serde_json::from_slice::<EndpointDiscovery>(&payload) {
        if discovery.port == port && discovery.token == token {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn write_endpoint_config(path: &Path, port: u16, token: &str) -> Result<(), String> {
    let payload = serde_json::to_vec(&EndpointDiscovery { port, token: token.to_string() })
        .map_err(|error| format!("Could not serialize the endpoint config file: {error}"))?;
    std::fs::write(path, payload)
        .map_err(|error| format!("Could not write the endpoint config file: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
            format!("Could not restrict the endpoint config file: {error}")
        })?;
    }
    Ok(())
}

fn status_reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        _ => "",
    }
}

fn plain_error_response(status: StatusCode, message: &str) -> Response {
    (status, format!("{message}\n")).into_response()
}

// The only origins whose preflights the endpoint answers: the bundled
// webview's engine origins. Every webview fetch carries an Authorization
// header, so each one is preflighted, and without an answered preflight the
// engine refuses to send the request at all. Agents and other API clients
// send no Origin header and keep the header-free responses they see today.
const WEBVIEW_ORIGINS: [&str; 3] =
    ["tauri://localhost", "http://tauri.localhost", "https://tauri.localhost"];

// The reflected origin a response may carry for this request, or None when
// the caller is not an allow-listed webview on a webview-consumed face: the
// agent face and origin-less callers never get CORS headers.
fn webview_cors_origin<'a>(path: &str, origin: Option<&'a str>) -> Option<&'a str> {
    let origin = origin?;
    if !WEBVIEW_ORIGINS.contains(&origin) {
        return None;
    }
    if path == "/events" || path.starts_with("/api/") {
        return Some(origin);
    }
    None
}

// The preflight answer for a webview-consumed face: an empty 200 with the
// reflected origin, the one method the face needs, and the headers the
// webview's fetches send. The token stays the authentication boundary; this
// only lets the browser engine deliver the request at all.
fn preflight_response(origin: &str, face_method: &str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin)
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, face_method)
        .header(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            "Authorization, Content-Type",
        )
        .header(header::ACCESS_CONTROL_MAX_AGE, "600")
        .body(axum::body::Body::empty())
        .expect("static preflight response is representable")
}

// Attaches the reflected origin to a webview-consumed face's response: the
// engine checks the actual response too, not just the preflight.
fn with_webview_cors(response: Response, origin: Option<&str>) -> Response {
    let Some(origin) = origin else { return response };
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::HeaderValue::from_str(origin)
            .expect("an allow-listed origin is a valid header value"),
    );
    Response::from_parts(parts, body)
}

// Reads the socket until a complete HTTP/1.1 head is buffered; returns the
// head's length inside the buffer. The parse is httparse, hyper's own
// request parser: malformed heads and oversized heads are connection errors.
async fn read_request_head(
    stream: &mut tokio::net::TcpStream,
    buffer: &mut Vec<u8>,
) -> Result<usize, ()> {
    let mut chunk = [0u8; 4096];
    loop {
        let head_len = {
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut parsed = httparse::Request::new(&mut headers);
            match parsed.parse(buffer) {
                Ok(httparse::Status::Complete(head_len)) => head_len,
                Ok(httparse::Status::Partial) => 0,
                Err(_) => return Err(()),
            }
        };
        if head_len > 0 {
            return Ok(head_len);
        }
        if buffer.len() > REQUEST_HEAD_LIMIT {
            return Err(());
        }
        let read = stream.read(&mut chunk).await.map_err(|_| ())?;
        if read == 0 {
            return Err(());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

// Re-parses the buffered head to forward the client's headers (notably
// authorization and content-type) to the handler as owned values.
fn buffer_headers(buffer: &[u8], head_len: usize) -> Result<Vec<(String, Vec<u8>)>, ()> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    parsed.parse(&buffer[..head_len]).map_err(|_| ())?;
    Ok(parsed
        .headers
        .iter()
        .map(|header| (header.name.to_string(), header.value.to_vec()))
        .collect())
}

// One connection's parsed head, kept so the request can be routed before
// any body is read.
struct RequestHead {
    buffer: Vec<u8>,
    head_len: usize,
    method: String,
    path: String,
    content_length: usize,
    origin: Option<String>,
}

// Reads the socket until a complete HTTP/1.1 head is buffered and parsed;
// malformed heads and oversized heads are connection errors.
async fn read_request_head_full(
    stream: &mut tokio::net::TcpStream,
) -> Result<RequestHead, ()> {
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let head_len = read_request_head(stream, &mut buffer).await.map_err(|_| ())?;
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    let httparse::Status::Complete(_) = parsed
        .parse(&buffer)
        .map_err(|_| ())?
    else {
        return Err(());
    };
    let method = parsed.method.unwrap_or("").to_string();
    let path = parsed.path.unwrap_or("/").to_string();
    let content_length = parsed
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-length"))
        .and_then(|header| std::str::from_utf8(header.value).ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let origin = parsed
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("origin"))
        .and_then(|header| std::str::from_utf8(header.value).ok())
        .map(str::to_string);
    Ok(RequestHead { buffer, head_len, method, path, content_length, origin })
}

// One connection: the head read is bounded by the stall limit for every
// request shape, then either the SSE stream runs on an idle deadline or a
// single request gets one response and the connection closes under the
// stall limit.
async fn serve_connection(mut stream: tokio::net::TcpStream, state: TransportState) {
    let mut head =
        match tokio::time::timeout(CONNECTION_STALL_LIMIT, read_request_head_full(&mut stream)).await
        {
            Ok(Ok(head)) => head,
            _ => return,
        };
    let path = head.path.split('?').next().unwrap_or("/").to_string();
    let cors_origin = webview_cors_origin(&path, head.origin.as_deref());
    if head.method == "GET" && path == "/events" && state.events.is_some() {
        stream_events(&mut stream, &state, &head.buffer, head.head_len, cors_origin).await;
        return;
    }
    let handled = async {
        if head.method == "OPTIONS" {
            // A preflight for a webview-consumed face is answered here so
            // the engine sends the real request; every other OPTIONS keeps
            // the plain answers below.
            if let Some(origin) = cors_origin {
                let face_method = if path == "/events" { "GET" } else { "POST" };
                write_response(&mut stream, preflight_response(origin, face_method), RESPONSE_BODY_LIMIT)
                    .await
                    .map_err(|_| "write failed")?;
                return Ok(());
            }
        }
        if head.method != "POST" {
            let response = if path == "/" || path == "/mcp" || path.starts_with("/api/") {
                plain_error_response(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "POST /, POST /mcp, and POST /api/<command> are the endpoints",
                )
            } else {
                plain_error_response(
                    StatusCode::NOT_FOUND,
                    "POST /, POST /mcp, and POST /api/<command> are the endpoints",
                )
            };
            write_response(&mut stream, response, RESPONSE_BODY_LIMIT)
                .await
                .map_err(|_| "write failed")?;
            return Ok(());
        }
        if path != "/" && path != "/mcp" && !path.starts_with("/api/") {
            let response = plain_error_response(
                StatusCode::NOT_FOUND,
                "POST /, POST /mcp, and POST /api/<command> are the endpoints",
            );
            write_response(&mut stream, response, RESPONSE_BODY_LIMIT)
                .await
                .map_err(|_| "write failed")?;
            return Ok(());
        }
        if head.content_length > TRANSPORT_BODY_GUARD_BYTES {
            let response = with_webview_cors(
                rpc_error(
                    &Value::Null,
                    StatusCode::PAYLOAD_TOO_LARGE,
                    REQUEST_TOO_LARGE,
                    format!("The request body exceeds the {TRANSPORT_BODY_GUARD_BYTES} byte guard."),
                ),
                cors_origin,
            );
            write_response(&mut stream, response, RESPONSE_BODY_LIMIT)
                .await
                .map_err(|_| "write failed")?;
            return Ok(());
        }
        while head.buffer.len() - head.head_len < head.content_length {
            let mut chunk = [0u8; 4096];
            let read = stream.read(&mut chunk).await.map_err(|_| "read failed")?;
            if read == 0 {
                return Err("connection closed mid-body");
            }
            head.buffer.extend_from_slice(&chunk[..read]);
        }
        let body = head.buffer[head.head_len..head.head_len + head.content_length].to_vec();

        let mut builder = Request::builder().method(head.method.as_str()).uri(path.as_str());
        for header in buffer_headers(&head.buffer, head.head_len).map_err(|_| "malformed head")? {
            builder = builder.header(header.0, header.1);
        }
        let request = builder
            .body(axum::body::Body::from(body))
            .map_err(|_| "unrepresentable request")?;
        let response = handle(State(state), request).await;
        write_response(
            &mut stream,
            with_webview_cors(response, cors_origin),
            response_body_limit(&path),
        )
        .await
        .map_err(|_| "write failed")
    };
    let _ = tokio::time::timeout(CONNECTION_STALL_LIMIT, handled).await;
}

// Builds just enough request for the bearer evaluation: only the forwarded
// authorization header matters. The shared resolution accepts both actor
// kinds, so human tokens stream events exactly as agent tokens do.
async fn sse_identity(
    state: &TransportState,
    buffer: &[u8],
    head_len: usize,
) -> Option<AuthenticatedActor> {
    let mut builder = Request::builder().method("GET").uri("/events");
    for header in buffer_headers(buffer, head_len).ok()? {
        builder = builder.header(header.0, header.1);
    }
    let request = builder.body(axum::body::Body::empty()).ok()?;
    authenticate(state, bearer_secret(&request)).await
}

// GET /events: the same bearer gate, then the push-event stream. The head
// goes out once (no content-length, no close) and frames follow as the
// sinks push them, with keepalive comments marking an idle stream. Every
// write resets the idle deadline; a lagged subscriber's task ends and the
// client reconnects with backoff (docs/server.md).
async fn stream_events(
    stream: &mut tokio::net::TcpStream,
    state: &TransportState,
    buffer: &[u8],
    head_len: usize,
    cors_origin: Option<&str>,
) {
    use tokio::io::AsyncWriteExt;
    if sse_identity(state, buffer, head_len).await.is_none() {
        // The presented secret is never logged, only the miss.
        log::warn!("agent endpoint rejected an unauthorized request to /events");
        let _ = write_response(stream, unauthorized_response(), RESPONSE_BODY_LIMIT).await;
        return;
    }
    let Some(events) = &state.events else { return };
    let mut receiver = events.subscribe();
    if write_event_stream_head(stream, cors_origin).await.is_err() {
        return;
    }
    let mut last_write = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(last_write + SSE_IDLE_LIMIT) => return,
            pushed = receiver.recv() => match pushed {
                Ok(event) => {
                    if write_push_event(stream, &event).await.is_err() {
                        return;
                    }
                    last_write = tokio::time::Instant::now();
                }
                // A subscriber that fell behind by more than the channel
                // capacity, or a shut-down sender, ends the stream.
                Err(broadcast::error::RecvError::Lagged(_)) => return,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = tokio::time::sleep_until(last_write + SSE_KEEPALIVE_PERIOD) => {
                if stream.write_all(b": keepalive\n\n").await.is_err() {
                    return;
                }
                if stream.flush().await.is_err() {
                    return;
                }
                last_write = tokio::time::Instant::now();
            }
        }
    }
}

// The SSE head is written once; the connection stays open while the
// subscriber keeps up. A webview's stream carries the reflected origin so
// the engine hands the stream to the fetch reader; origin-less callers keep
// the bare SSE head.
async fn write_event_stream_head(
    stream: &mut tokio::net::TcpStream,
    cors_origin: Option<&str>,
) -> Result<(), ()> {
    use tokio::io::AsyncWriteExt;
    let cors = cors_origin
        .map(|origin| format!("access-control-allow-origin: {origin}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "{}{cors}content-type: text/event-stream\r\n\r\n",
        status_line(StatusCode::OK)
    );
    stream.write_all(head.as_bytes()).await.map_err(|_| ())?;
    stream.flush().await.map_err(|_| ())
}

async fn write_push_event(stream: &mut tokio::net::TcpStream, event: &PushEvent) -> Result<(), ()> {
    use tokio::io::AsyncWriteExt;
    let data = serde_json::to_string(&event.payload).map_err(|_| ())?;
    let frame = format!("event: {}\ndata: {}\n\n", event.kind, data);
    stream.write_all(frame.as_bytes()).await.map_err(|_| ())?;
    stream.flush().await.map_err(|_| ())
}

fn status_line(status: StatusCode) -> String {
    format!("HTTP/1.1 {} {}\r\n", status.as_u16(), status_reason(status.as_u16()))
}

// The body limit rides with the route: the command API serves Git-derived
// reads, the other faces small answers.
fn response_body_limit(path: &str) -> usize {
    if path.starts_with("/api/") {
        COMMAND_BODY_LIMIT
    } else {
        RESPONSE_BODY_LIMIT
    }
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    response: Response,
    body_limit: usize,
) -> Result<(), ()> {
    use tokio::io::AsyncWriteExt;
    let (parts, body) = response.into_parts();
    let body = to_bytes(body, body_limit).await.map_err(|_| ())?;
    let mut head = status_line(parts.status);
    for (name, value) in &parts.headers {
        let name = name.as_str();
        if matches!(name, "content-length" | "transfer-encoding" | "connection") {
            continue;
        }
        let Ok(value) = value.to_str() else { continue };
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("connection: close\r\n");
    head.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
    stream.write_all(head.as_bytes()).await.map_err(|_| ())?;
    stream.write_all(&body).await.map_err(|_| ())?;
    stream.flush().await.map_err(|_| ())
}

// One startup attempt for the agent endpoint, driven by the Settings MCP
// keys. Nothing except a broken runtime/thread environment fails app
// startup: a disabled listener serves nothing, and a failed bind is a
// normal user-visible condition that never writes a config file or
// provisions a default token, so nothing advertises a dead endpoint.
// On a successful bind the same startup path provisions the default
// token and writes the config file with its secret. The status
// handle is shared across restarts and reset from the config on every
// call.
pub(crate) async fn start(
    pool: SqlitePool,
    deps: TransportDeps,
    config: ListenerConfig,
    status: McpStatusHandle,
) -> Result<Option<TransportHandle>, String> {
    start_with_events(pool, deps, config, status, None).await
}

// The server binary's start: the same listener start path with the push
// events also fanned out to SSE subscribers through the given channel.
pub(crate) async fn start_with_events(
    pool: SqlitePool,
    deps: TransportDeps,
    config: ListenerConfig,
    status: McpStatusHandle,
    events: Option<broadcast::Sender<PushEvent>>,
) -> Result<Option<TransportHandle>, String> {
    status.reset_from_config(&config);
    let TransportDeps { data_dir, arrivals, refreshes, comment_changes, request_changes } = deps;
    let state = TransportState {
        pool,
        arrivals,
        refreshes,
        comment_changes,
        request_changes,
        status: status.clone(),
        events,
    };
    if !config.enabled {
        return Ok(None);
    }
    let listener = match std::net::TcpListener::bind((config.address.as_str(), config.port)) {
        Ok(listener) => listener,
        Err(error) => {
            log::warn!("agent endpoint bind failed on {}:{}", config.address, config.port);
            status.set(false, Some(format!("Could not bind the agent endpoint to {address}:{port}: {error}", address = config.address, port = config.port)));
            return Ok(None);
        }
    };
    // tokio::net::TcpListener::from_std leaves the socket mode to the
    // caller; without this, accept runs blocking inside the future's poll
    // and an idle listener sits in the kernel beyond the reach of timers,
    // the exit flag, and every restart join.
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("Could not ready the agent endpoint socket: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("Could not resolve the agent endpoint port: {error}"))?
        .port();
    status.lock_status().port = port;
    log::info!("agent endpoint listening on {}:{}", config.address, port);
    // Provisioning and discovery come after the bind and stay one unit: if
    // either fails, the bound listener shuts and the error surfaces in the
    // Settings MCP section without a config file advertising it.
    let default_secret = match provision_default_token_in_pool(&state.pool).await {
        Ok(secret) => secret,
        Err(error) => {
            status.set(
                false,
                Some(format!("Could not provision the default agent token: {}", error.message)),
            );
            return Ok(None);
        }
    };
    let config_path = endpoint_config_path(&data_dir);
    if let Err(error) = write_endpoint_config(&config_path, port, &default_secret) {
        status.set(false, Some(error));
        return Ok(None);
    }
    status.set(true, None);
    let shutdown = Arc::new(AtomicBool::new(false));
    let exit = Arc::clone(&shutdown);
    // The listener runs on its own single-thread runtime on a dedicated
    // thread and serves connections as spawned tasks: every handler is
    // IO-bound, so tasks interleave, and an open SSE stream never gates
    // accepts or other connections. Per-connection caps (head size, header
    // count, body guard, stall timeout) hold inside each task.
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            let message = format!("Could not start the agent endpoint runtime: {error}");
            remove_endpoint_config_if_owned(&config_path, port, &default_secret);
            status.set(false, Some(message.clone()));
            return Err(message);
        }
    };
    let failure_config = config_path.clone();
    let failure_secret = default_secret.clone();
    let failure_status = status.clone();
    let failure_port = port;
    let thread = std::thread::Builder::new()
        .name("agent-endpoint".into())
        .spawn(move || {
                let _ = runtime.block_on(async move {
                    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                        remove_endpoint_config_if_owned(&config_path, port, &default_secret);
                        state.status.set(false, Some("Could not adopt the bound listener socket.".into()));
                        return;
                    };
                    // The exit flag is checked on an accept deadline rather
                    // than across a dedicated wake: shutdown then costs at
                    // most one poll period plus any in-flight request,
                    // which restarts rely on before rebinding the port.
                    loop {
                        if exit.load(Ordering::Relaxed) {
                            break;
                        }
                        match tokio::time::timeout(ACCEPT_POLL, listener.accept()).await {
                            Ok(Ok((stream, _))) => {
                                // Each connection is its own task on this
                                // single-threaded runtime: IO-bound work
                                // interleaves, so one open SSE stream cannot
                                // stall accepts or other connections.
                                let state = state.clone();
                                tokio::spawn(async move {
                                    serve_connection(stream, state).await;
                                });
                            }
                            // Timed out idle: re-check the flag. Accept errors
                            // mean the listener is gone.
                            Ok(Err(_)) => break,
                            Err(_) => continue,
                        }
                    }
                    remove_endpoint_config_if_owned(&config_path, port, &default_secret);
                    state.status.set(false, None);
                });
        })
        .map_err(|error| {
            // A dead thread must not leave a config file advertising the
            // bound endpoint or a status claiming it is running.
            remove_endpoint_config_if_owned(&failure_config, failure_port, &failure_secret);
            let message = format!("Could not start the agent endpoint thread: {error}");
            failure_status.set(false, Some(message.clone()));
            message
        })?;
    Ok(Some(TransportHandle { shutdown, thread: Some(thread) }))
}

// Stops the owned listener and starts a fresh one from the given config,
// reusing the same status handle. The old thread is joined before the new
// bind so a same-address restart cannot collide with its own predecessor;
// an in-flight request finishes first, bounded by the stall limit. The
// owner lock spans the whole sequence, so overlapping restart commands
// serialize instead of racing the take and the store.
pub(crate) async fn restart(
    pool: SqlitePool,
    deps: &TransportDeps,
    config: ListenerConfig,
    status: McpStatusHandle,
    owner: &ListenerOwner,
) -> Result<ListenerStatus, String> {
    let mut owned = owner.0.lock().await;
    if let Some(stopping) = owned.take() {
        // The join is a blocking wait on a dedicated thread so the async
        // caller never blocks the runtime.
        let _ = tokio::task::spawn_blocking(move || stopping.stop()).await;
    }
    let handle = start(pool, deps.clone(), config, status.clone()).await?;
    *owned = handle;
    drop(owned);
    Ok(status.lock_status().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::create_agent_token_in_pool;
    use crate::testutil::{seed_repo, test_pool};
    use axum::body::Body;
    use std::sync::Mutex;

    fn noop_refreshes() -> RefreshSink {
        Arc::new(|_| {})
    }

    fn noop_comment_changes() -> CommentSink {
        Arc::new(|_| {})
    }

    fn noop_request_changes() -> RequestChangeSink {
        Arc::new(|_| {})
    }

    fn dummy_status() -> McpStatusHandle {
        McpStatusHandle(Arc::new(Mutex::new(ListenerStatus {
            enabled: true,
            running: true,
            address: "127.0.0.1".into(),
            port: 0,
            error: None,
            config_path: String::new(),
            skill_dir: String::new(),
        })))
    }

    fn test_config(port: u16) -> ListenerConfig {
        ListenerConfig { enabled: true, address: "127.0.0.1".into(), port }
    }

    fn test_deps(
        dir: &std::path::Path,
        arrivals: ArrivalSink,
        refreshes: RefreshSink,
        comment_changes: CommentSink,
    ) -> TransportDeps {
        TransportDeps {
            data_dir: dir.to_path_buf(),
            arrivals,
            refreshes,
            comment_changes,
            request_changes: noop_request_changes(),
        }
    }

    // A handler state authenticated by a real token row: the pool-backed
    // lookup replaces the old boot-token comparison, so tests hold the
    // secret of the token they created.
    async fn test_state(pool: SqlitePool, arrivals: ArrivalSink) -> (TransportState, String) {
        let secret = create_agent_token_in_pool(&pool, "test-agent").await.unwrap().secret;
        let state = TransportState {
            pool,
            arrivals,
            refreshes: noop_refreshes(),
            comment_changes: noop_comment_changes(),
            request_changes: noop_request_changes(),
            status: dummy_status(),
            events: None,
        };
        (state, secret)
    }

    fn recording_sink() -> (ArrivalSink, Arc<Mutex<Vec<SubmissionArrival>>>) {
        let received: Arc<Mutex<Vec<SubmissionArrival>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        (Arc::new(move |arrival| sink.lock().unwrap().push(arrival)), received)
    }

    // The emitted payload is the webview's contract: the row's key is
    // `request_id`, not the struct's internal `id` field name, and the
    // narrating event kind rides along (null when the mutation updated in
    // place without narrating).
    #[test]
    fn request_change_serializes_with_the_webview_payload_keys() {
        let change = RequestChange {
            id: 3,
            repo_path: "/demo".into(),
            base_sha: "base".into(),
            target_key: "/demo".into(),
            target_kind: "worktree".into(),
            status: "requested".into(),
            event: Some("request_created".into()),
        };
        assert_eq!(
            serde_json::to_value(&change).unwrap(),
            json!({
                "request_id": 3,
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "status": "requested",
                "event": "request_created",
            })
        );
        let silent = RequestChange { event: None, ..change };
        assert_eq!(serde_json::to_value(&silent).unwrap()["event"], Value::Null);
    }

    fn recording_refreshes() -> (RefreshSink, Arc<Mutex<Vec<String>>>) {
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        (
            Arc::new(move |repo_path: &str| sink.lock().unwrap().push(repo_path.to_string())),
            received,
        )
    }

    async fn post(state: TransportState, token: Option<&str>, body: &str) -> (StatusCode, Value) {
        let mut builder = Request::builder().method("POST").uri("/");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        let request = builder.body(Body::from(body.to_string())).unwrap();
        let response = handle(State(state), request).await;
        let status = response.status();
        let payload = to_bytes(response.into_body(), RESPONSE_BODY_LIMIT).await.unwrap();
        (status, serde_json::from_slice(&payload).unwrap())
    }

    fn rpc_body(id: Value, method: &str, params: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
    }

    // POSTs to a command-API route through the same handler the socket
    // serves; auth lives in handle(), so no listener is needed here.
    async fn post_api(
        state: TransportState,
        token: Option<&str>,
        path: &str,
        body: &str,
    ) -> (StatusCode, Value) {
        let (status, raw) = post_api_raw(state, token, path, body).await;
        (status, serde_json::from_slice(raw.as_bytes()).unwrap())
    }

    async fn post_api_raw(
        state: TransportState,
        token: Option<&str>,
        path: &str,
        body: &str,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().method("POST").uri(path);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        let request = builder.body(Body::from(body.to_string())).unwrap();
        let response = handle(State(state), request).await;
        let status = response.status();
        let payload = to_bytes(response.into_body(), RESPONSE_BODY_LIMIT).await.unwrap();
        (status, String::from_utf8_lossy(&payload).into_owned())
    }

    // One admin and one plain member, with live bearer secrets.
    async fn admin_and_member(pool: &SqlitePool) -> (String, String) {
        let admin = crate::identity::create_first_admin_in_pool(pool, "ops").await.unwrap();
        let member = crate::identity::create_user_in_pool(pool, "dana").await.unwrap();
        (admin.secret, member.secret)
    }

    fn review_params(repo_path: &str) -> Value {
        json!({
            "repo_path": repo_path,
            "base_sha": "base",
            "target_key": "/demo",
            "target_kind": "worktree",
            "agent_name": "reviewer-bot",
            "agent_model": "test-model",
            "command_context": null,
            "sections": [{ "kind": "brief", "title": "Summary", "body": "All good." }],
            "findings": [
                { "title": "Note", "body": "body", "file": null, "start": null, "end": null, "priority": "P1" }
            ]
        })
    }

    #[test]
    fn config_file_round_trips_port_and_token() {
        let dir = crate::testutil::test_path("transport-discovery");
        std::fs::create_dir_all(&dir).unwrap();
        let path = endpoint_config_path(&dir);
        write_endpoint_config(&path, 45123, "abc123").unwrap();
        let parsed: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(parsed, EndpointDiscovery { port: 45123, token: "abc123".into() });
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the config file must be owner-only");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Exit cleanup must not evict a registration another instance wrote;
    // a stale file is survivable (clients re-read on refusal), an orphaned
    // live endpoint is not.
    #[test]
    fn config_file_is_removed_only_when_owned() {
        let dir = crate::testutil::test_path("transport-discovery-owned");
        std::fs::create_dir_all(&dir).unwrap();
        let path = endpoint_config_path(&dir);
        write_endpoint_config(&path, 45123, "abc123").unwrap();

        remove_endpoint_config_if_owned(&path, 1, "other-token");
        assert!(path.exists(), "a foreign registration must survive");
        remove_endpoint_config_if_owned(&path, 45123, "abc123");
        assert!(!path.exists(), "an owned registration is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Auth is the listener's own gate: a missing, wrong, or deleted secret
    // answers the same 401/-32001 shape, a valid one authenticates, and its
    // use is recorded on the token row.
    #[tokio::test]
    async fn valid_secret_authenticates_and_missing_wrong_or_deleted_are_unauthorized() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool.clone(), Arc::new(|_| {})).await;
        let body = rpc_body(json!(1), "post_review", review_params("/demo"));
        let (status, payload) = post(state.clone(), None, &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["error"]["code"], UNAUTHORIZED);
        let (status, payload) = post(state.clone(), Some("wrong-token"), &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["error"]["code"], UNAUTHORIZED);

        let (status, _) = post(state, Some(&secret), &body).await;
        assert_eq!(status, StatusCode::OK);
        let token_id: i64 = sqlx::query_scalar("SELECT id FROM agent_tokens WHERE name = 'test-agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let last_used_at: Option<i64> =
            sqlx::query_scalar("SELECT last_used_at FROM agent_tokens WHERE id = ?")
                .bind(token_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(last_used_at.is_some());

        // A deleted secret keeps the same 401 shape.
        sqlx::query("DELETE FROM agent_tokens WHERE id = ?")
            .bind(token_id)
            .execute(&pool)
            .await
            .unwrap();
        let (state, _) = test_state(pool, Arc::new(|_| {})).await;
        let (status, payload) = post(state, Some(&secret), &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["error"]["code"], UNAUTHORIZED);
    }

    #[tokio::test]
    async fn malformed_json_is_a_parse_error() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool, Arc::new(|_| {})).await;
        let (status, payload) = post(state, Some(&secret), "{not json").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["error"]["code"], PARSE_ERROR);
    }

    // The auth matrix across the faces: a human token is refused on the
    // agent face with the unchanged 401/-32001 shape, an agent token is
    // refused on the command API with the command face's own 401 shape,
    // and a human token runs a command route.
    #[tokio::test]
    async fn each_face_accepts_only_its_actor_kind() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, agent_secret) = test_state(pool.clone(), Arc::new(|_| {})).await;
        let (admin_secret, member_secret) = admin_and_member(&pool).await;

        // Human tokens (admin or not) on the agent face: refused.
        let body = rpc_body(json!(1), "post_review", review_params("/demo"));
        let (status, payload) = post(state.clone(), Some(&admin_secret), &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["error"]["code"], UNAUTHORIZED);
        let (status, _) = post(state.clone(), Some(&member_secret), &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Agent tokens on the command API: refused with the command face's
        // shape, not the agent face's.
        let (status, payload) =
            post_api(state.clone(), Some(&agent_secret), "/api/list_repos", "{}").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["code"], "unauthorized");

        // Missing tokens on the command API: the same refusal.
        let (status, payload) = post_api(state.clone(), None, "/api/list_repos", "{}").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["code"], "unauthorized");

        // A human token runs the command route and gets the shared
        // implementation's result.
        let (status, payload) =
            post_api(state.clone(), Some(&admin_secret), "/api/list_repos", "{}").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload[0]["path"], "/demo");

        // Admin-only routes refuse a plain member and serve an admin.
        let (status, payload) =
            post_api(state.clone(), Some(&member_secret), "/api/list_users", "{}").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(payload["code"], "forbidden");
        let (status, _) =
            post_api(state.clone(), Some(&member_secret), "/api/create_user", r#"{"name":"kim"}"#)
                .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, payload) =
            post_api(state, Some(&admin_secret), "/api/create_user", r#"{"name":"kim"}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["user"]["name"], "kim");
        assert!(!payload["secret"].as_str().unwrap().is_empty());
    }

    // Unknown command routes answer 404 without touching any command.
    #[tokio::test]
    async fn unknown_command_route_is_not_found() {
        let pool = test_pool().await;
        let admin = crate::identity::create_first_admin_in_pool(&pool, "ops").await.unwrap();
        let state = TransportState {
            pool,
            arrivals: Arc::new(|_| {}),
            refreshes: noop_refreshes(),
            comment_changes: noop_comment_changes(),
            request_changes: noop_request_changes(),
            status: dummy_status(),
            events: None,
        };
        let (status, _) =
            post_api_raw(state, Some(&admin.secret), "/api/no_such_command", "{}").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn wrong_jsonrpc_version_is_an_invalid_request() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool, Arc::new(|_| {})).await;
        let body = json!({ "jsonrpc": "1.0", "id": 2, "method": "post_review", "params": review_params("/demo") })
            .to_string();
        let (status, payload) = post(state, Some(&secret), &body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["error"]["code"], INVALID_REQUEST);
        assert_eq!(payload["id"], 2);
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool, Arc::new(|_| {})).await;
        let (status, payload) =
            post(state, Some(&secret), &rpc_body(json!(3), "list_reviews", json!({}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(payload["id"], 3);
    }

    #[tokio::test]
    async fn oversized_body_is_rejected_without_parsing() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool, Arc::new(|_| {})).await;
        let body = "x".repeat(TRANSPORT_BODY_GUARD_BYTES + 1);
        let (status, payload) = post(state, Some(&secret), &body).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(payload["error"]["code"], REQUEST_TOO_LARGE);
    }

    #[tokio::test]
    async fn valid_post_review_stores_through_the_real_pool_and_announces() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (arrivals, received) = recording_sink();
        let (state, secret) = test_state(pool.clone(), arrivals).await;
        let (status, payload) =
            post(state, Some(&secret), &rpc_body(json!(7), "post_review", review_params("/demo"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["id"], 7);
        let submission_id = payload["result"]["submission_id"].as_i64().unwrap();
        assert!(submission_id > 0);
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM submissions WHERE id = ?")
            .bind(submission_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, 1);
        // Ingested findings are owned by the authenticated token.
        let owner: Option<i64> =
            sqlx::query_scalar("SELECT author_token_id FROM comments WHERE submission_id = ?")
                .bind(submission_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(owner, Some(1), "the test token is the pool's first agent row");
        let arrivals = received.lock().unwrap();
        assert_eq!(arrivals.len(), 1);
        assert_eq!(arrivals[0].submission_id, submission_id);
        assert_eq!(arrivals[0].agent_name, "reviewer-bot");
        assert_eq!(arrivals[0].repo_path, "/demo");
        assert_eq!(arrivals[0].target_kind, "worktree");
    }

    #[tokio::test]
    async fn unknown_repo_is_unknown_review_target() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (arrivals, received) = recording_sink();
        let (state, secret) = test_state(pool, arrivals).await;
        let (status, payload) =
            post(state, Some(&secret), &rpc_body(json!(8), "post_review", review_params("/missing"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["error"]["code"], UNKNOWN_REVIEW_TARGET);
        assert_eq!(received.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn submission_shape_violations_are_invalid_params() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool, Arc::new(|_| {})).await;

        let mut bad_priority = review_params("/demo");
        bad_priority["findings"][0]["priority"] = json!("P9");
        let (_, payload) = post(state.clone(), Some(&secret), &rpc_body(json!(9), "post_review", bad_priority)).await;
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);

        let mut unknown_field = review_params("/demo");
        unknown_field["surprise"] = json!(true);
        let (_, payload) =
            post(state.clone(), Some(&secret), &rpc_body(json!(10), "post_review", unknown_field)).await;
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);

        let mut bad_target_kind = review_params("/demo");
        bad_target_kind["target_kind"] = json!("branch");
        let (_, payload) = post(state, Some(&secret), &rpc_body(json!(11), "post_review", bad_target_kind)).await;
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);
    }

    // The refresh ping: an authenticated method over the shared refresh
    // implementation. A repo without remotes is a no-op success that still
    // announces through the injected sink; an unknown repo keeps the
    // unknown_review_target code.
    #[tokio::test]
    async fn refresh_repo_is_a_noop_success_without_remotes_and_announces() {
        let pool = test_pool().await;
        let repo = crate::testutil::test_repo("transport-refresh-no-remote");
        seed_repo(&pool, repo.to_str().unwrap()).await;
        let (refreshes, announced) = recording_refreshes();
        let (state, secret) = test_state(pool, Arc::new(|_| {})).await;
        let state = TransportState { refreshes, ..state };
        let params = json!({ "repo_path": repo.to_str().unwrap() });
        let (status, payload) =
            post(state.clone(), Some(&secret), &rpc_body(json!(4), "refresh_repo", params)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["result"]["ok"], true);
        assert_eq!(
            announced.lock().unwrap().as_slice(),
            [repo.to_string_lossy().as_ref()]
        );

        let (status, payload) = post(
            state,
            Some(&secret),
            &rpc_body(json!(5), "refresh_repo", json!({ "repo_path": "/missing" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["error"]["code"], UNKNOWN_REVIEW_TARGET);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn refresh_repo_params_are_strict() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool, Arc::new(|_| {})).await;
        let (status, payload) = post(
            state,
            Some(&secret),
            &rpc_body(json!(6), "refresh_repo", json!({ "repo_path": "/demo", "surprise": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);
    }

    fn announce_params(repo_path: &str) -> Value {
        json!({
            "repo_path": repo_path,
            "base_sha": "base",
            "target_key": "/demo",
            "target_kind": "worktree",
            "head_sha": "head-1",
        })
    }

    // The raw face's announce method: the stored request row comes back
    // with the announce's direct in_review entry, the engine's dedup
    // ladder holds behind the method, and refusals map like post_review's.
    #[tokio::test]
    async fn announce_review_stores_enters_and_dedups_through_the_real_pool() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool.clone(), Arc::new(|_| {})).await;
        let (status, payload) = post(
            state.clone(),
            Some(&secret),
            &rpc_body(json!(20), "announce_review", announce_params("/demo")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let request = payload["result"]["request"].clone();
        assert_eq!(request["status"], "in_review");
        assert_eq!(request["head_sha"], "head-1");
        assert_eq!(request["round"], 0);
        assert_eq!(request["requester_token_id"], 1, "the caller owns the row");
        let id = request["id"].as_i64().unwrap();
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM review_requests WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, 1);

        // A second announce on the same head dedups in place: same row,
        // no second pickup.
        let (status, payload) = post(
            state.clone(),
            Some(&secret),
            &rpc_body(json!(21), "announce_review", announce_params("/demo")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["result"]["request"]["id"], json!(id));
        assert_eq!(payload["result"]["request"]["status"], "in_review");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM review_requests")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);

        // Shape violations never execute.
        let mut unknown_field = announce_params("/demo");
        unknown_field["surprise"] = json!(true);
        let (_, payload) = post(
            state.clone(),
            Some(&secret),
            &rpc_body(json!(22), "announce_review", unknown_field),
        )
        .await;
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);
        let mut bad_kind = announce_params("/demo");
        bad_kind["target_kind"] = json!("branch");
        let (_, payload) = post(
            state.clone(),
            Some(&secret),
            &rpc_body(json!(23), "announce_review", bad_kind),
        )
        .await;
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);

        // The engine's refusal passes through as INVALID_PARAMS.
        sqlx::query("UPDATE review_requests SET status = 'changes_requested' WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        let (_, payload) = post(
            state,
            Some(&secret),
            &rpc_body(json!(24), "announce_review", announce_params("/demo")),
        )
        .await;
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);
        assert!(payload["error"]["message"]
            .as_str()
            .unwrap()
            .contains("re-request the review with a new head"));
    }

    #[tokio::test]
    async fn announce_review_unknown_repo_is_unknown_review_target() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool, Arc::new(|_| {})).await;
        let (_, payload) = post(
            state,
            Some(&secret),
            &rpc_body(json!(25), "announce_review", announce_params("/missing")),
        )
        .await;
        assert_eq!(payload["error"]["code"], UNKNOWN_REVIEW_TARGET);
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    // The listener binds the configured address and port, provisions the
    // default token, and writes the config file whose secret is the
    // only copy of that default.
    #[tokio::test(flavor = "multi_thread")]
    async fn start_uses_the_configured_port_and_provisions_the_default() {
        let pool = test_pool().await;
        let dir = crate::testutil::test_path("transport-start-configured");
        std::fs::create_dir_all(&dir).unwrap();
        let port = free_port();
        let config = test_config(port);
        let status = McpStatusHandle::for_config(&config);
        let handle = start(
            pool.clone(),
            test_deps(&dir, Arc::new(|_| {}), noop_refreshes(), noop_comment_changes()),
            config,
            status.clone(),
        )
        .await
        .unwrap()
        .unwrap();
        let status = status.lock_status().clone();
        assert!(status.running);
        assert_eq!(status.port, port);
        assert_eq!(status.error, None);

        let discovery: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(&dir)).unwrap()).unwrap();
        assert_eq!(discovery.port, port);
        // The discovery secret authenticates: it is the provisioned default.
        let identity = crate::agents::authenticate_token_in_pool(&pool, &discovery.token).await;
        assert!(identity.is_some(), "the discovery secret is the live default token");
        let default_row: (String, bool) = sqlx::query_as(
            "SELECT name, is_default FROM agent_tokens WHERE is_default = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(default_row, ("agent".into(), true));
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A bind collision is a normal condition: the app keeps running, the
    // error surfaces in the status, and no config file or default token
    // is written for the failed listener.
    #[tokio::test]
    async fn bind_failure_is_not_fatal_and_writes_no_config() {
        let pool = test_pool().await;
        let dir = crate::testutil::test_path("transport-start-collision");
        std::fs::create_dir_all(&dir).unwrap();
        let occupier = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = occupier.local_addr().unwrap().port();
        let config = test_config(port);
        let status = McpStatusHandle::for_config(&config);
        let handle = start(
            pool.clone(),
            test_deps(&dir, Arc::new(|_| {}), noop_refreshes(), noop_comment_changes()),
            config,
            status.clone(),
        )
        .await
        .unwrap();
        assert!(handle.is_none());
        let status = status.lock_status().clone();
        assert!(!status.running);
        assert!(status.error.is_some());
        assert!(!endpoint_config_path(&dir).exists());
        let defaults: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_tokens WHERE is_default = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(defaults, 0, "no default token may be provisioned for a dead endpoint");
        drop(occupier);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn disabled_listener_starts_nothing_and_writes_no_config() {
        let pool = test_pool().await;
        let dir = crate::testutil::test_path("transport-start-disabled");
        std::fs::create_dir_all(&dir).unwrap();
        let config = ListenerConfig { enabled: false, address: "127.0.0.1".into(), port: free_port() };
        let status = McpStatusHandle::for_config(&config);
        let handle = start(
            pool,
            test_deps(&dir, Arc::new(|_| {}), noop_refreshes(), noop_comment_changes()),
            config,
            status.clone(),
        )
        .await
        .unwrap();
        assert!(handle.is_none());
        let status = status.lock_status().clone();
        assert!(!status.enabled && !status.running);
        assert!(status.error.is_none());
        assert!(!endpoint_config_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Sends one raw request over a real loopback socket and returns the raw
    // response; the blocking std client runs on its own thread so no async
    // runtime is involved.
    fn send_raw_over_socket(port: u16, request: String) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            use std::net::TcpStream;
            use std::time::{Duration, Instant};
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut stream = loop {
                match TcpStream::connect(("127.0.0.1", port)) {
                    Ok(socket) => break socket,
                    Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                    Err(error) => panic!("the endpoint never started listening: {error}"),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            let mut response = Vec::new();
            stream
                .read_to_end(&mut response)
                .expect("the endpoint did not answer within 10s");
            String::from_utf8(response).unwrap()
        })
    }

    // Sends one POST over a real loopback socket and returns the raw
    // response; the blocking std client runs on its own thread so no async
    // runtime is involved.
    fn send_over_socket(port: u16, bearer: &str, body: String) -> std::thread::JoinHandle<String> {
        let request = format!(
            "POST / HTTP/1.1\r\nhost: 127.0.0.1\r\nauthorization: Bearer {bearer}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len(),
        );
        send_raw_over_socket(port, request)
    }

    // End-to-end over a real loopback socket: start() binds, provisions the
    // default token, writes the config file with the bound port and its
    // secret, and the served handler answers an authorized POST. This is
    // the desktop e2e's transport path without a Tauri app. The test
    // runtime stays multi-thread so the pool's connection workers stay
    // alive while the handler runs on the endpoint's own dedicated runtime,
    // mirroring production (Tauri's runtime owns the pool; it never sleeps).
    #[tokio::test(flavor = "multi_thread")]
    async fn started_endpoint_serves_post_review_over_a_live_socket() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (arrivals, received) = recording_sink();
        let dir = crate::testutil::test_path("transport-socket");
        std::fs::create_dir_all(&dir).unwrap();
        let handle = start(
            pool,
            test_deps(&dir, arrivals, noop_refreshes(), noop_comment_changes()),
            test_config(0),
            McpStatusHandle::for_config(&test_config(0)),
        )
        .await
        .unwrap()
        .unwrap();
        let discovery: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(&dir)).unwrap()).unwrap();

        let body = rpc_body(json!(1), "post_review", review_params("/demo"));
        let response = send_over_socket(discovery.port, &discovery.token, body).join().unwrap();
        handle.stop();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "unexpected response: {response}");
        let payload = response.split("\r\n\r\n").nth(1).unwrap();
        let parsed: Value = serde_json::from_str(payload).unwrap();
        assert!(parsed["result"]["submission_id"].as_i64().unwrap() > 0);
        assert_eq!(received.lock().unwrap().len(), 1);
        // Storage itself is asserted by valid_post_review above (same ingest
        // path) and by the desktop e2e's restart-persistence check; asserting
        // the pool cross-runtime here would only exercise sqlx, not the
        // transport.
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The raw face's second method round-trips over the live socket: an
    // authorized refresh_repo against an open repo answers ok:true, and the
    // startup default from the config file authenticates it.
    #[tokio::test(flavor = "multi_thread")]
    async fn started_endpoint_serves_refresh_repo_over_a_live_socket() {
        let pool = test_pool().await;
        let repo = crate::testutil::test_repo("transport-socket-refresh");
        seed_repo(&pool, repo.to_str().unwrap()).await;
        let (refreshes, announced) = recording_refreshes();
        let dir = crate::testutil::test_path("transport-socket-refresh-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let handle = start(
            pool,
            test_deps(&dir, Arc::new(|_| {}), refreshes, noop_comment_changes()),
            test_config(0),
            McpStatusHandle::for_config(&test_config(0)),
        )
        .await
        .unwrap()
        .unwrap();
        let discovery: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(&dir)).unwrap()).unwrap();

        let body = rpc_body(
            json!(2),
            "refresh_repo",
            json!({ "repo_path": repo.to_str().unwrap() }),
        );
        let response = send_over_socket(discovery.port, &discovery.token, body).join().unwrap();
        handle.stop();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "unexpected response: {response}");
        let payload = response.split("\r\n\r\n").nth(1).unwrap();
        let parsed: Value = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed["result"]["ok"], true);
        assert_eq!(
            announced.lock().unwrap().as_slice(),
            [repo.to_string_lossy().as_ref()]
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&repo);
    }

    // The command API carries Git-backed reads local projects render through
    // IPC without a cap: a read over the agent faces' small response limit
    // must reach the webview whole, not die in the writer and close the
    // socket, or server-backed projects would lose files local ones render.
    #[tokio::test(flavor = "multi_thread")]
    async fn command_api_serves_file_reads_beyond_the_small_response_cap() {
        let pool = test_pool().await;
        let repo = crate::testutil::test_repo("transport-large-read");
        let base_sha = crate::testutil::test_rev_parse(&repo, "HEAD");
        // A working-changes read takes the worktree file, so the payload
        // needs no commit; the size sits between the small cap and the 16
        // MiB read ceiling the command answers follow.
        let payload = vec![b'x'; 1024 * 1024 + 4096];
        std::fs::write(repo.join("big.txt"), &payload).unwrap();
        let dir = crate::testutil::test_path("transport-large-read-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let handle = start(
            pool.clone(),
            test_deps(&dir, Arc::new(|_| {}), noop_refreshes(), noop_comment_changes()),
            test_config(0),
            McpStatusHandle::for_config(&test_config(0)),
        )
        .await
        .unwrap()
        .unwrap();
        let discovery: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(&dir)).unwrap()).unwrap();
        let admin = crate::identity::create_first_admin_in_pool(&pool, "ops").await.unwrap();

        let args = json!({
            "path": repo.to_str().unwrap(),
            "base": base_sha,
            "headRef": null,
            "committedOnly": false,
            "reversed": false,
            "file": "big.txt",
            "untracked": false,
        })
        .to_string();
        let response = send_raw_over_socket(
            discovery.port,
            format!(
                "POST /api/read_review_file_bytes HTTP/1.1\r\nhost: 127.0.0.1\r\nauthorization: Bearer {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{args}",
                admin.secret,
                args.len(),
            ),
        )
        .join()
        .unwrap();
        handle.stop();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "unexpected response head");
        let body = response.split("\r\n\r\n").nth(1).unwrap();
        assert_eq!(body.as_bytes(), payload.as_slice(), "the read must arrive whole");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&repo);
    }

    // Reads the GET /events stream on its own blocking thread: signals
    // through `head_landed` once the response head has arrived (the server
    // has subscribed), keeps reading until `marker` appears or the read
    // times out, and returns everything received.
    fn read_events_stream(
        port: u16,
        bearer: Option<&str>,
        origin: Option<&str>,
        marker: &'static str,
        head_landed: std::sync::mpsc::Sender<()>,
    ) -> std::thread::JoinHandle<String> {
        let authorization = bearer
            .map(|secret| format!("authorization: Bearer {secret}\r\n"))
            .unwrap_or_default();
        let origin_header = origin
            .map(|value| format!("origin: {value}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "GET /events HTTP/1.1\r\nhost: 127.0.0.1\r\naccept: text/event-stream\r\n{authorization}{origin_header}\r\n"
        );
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            use std::net::TcpStream;
            use std::time::{Duration, Instant};
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut stream = loop {
                match TcpStream::connect(("127.0.0.1", port)) {
                    Ok(socket) => break socket,
                    Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                    Err(error) => panic!("the endpoint never started listening: {error}"),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            let mut received = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut signaled = false;
            loop {
                if std::str::from_utf8(&received)
                    .map(|text| text.contains(marker))
                    .unwrap_or(false)
                {
                    break;
                }
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        received.extend_from_slice(&chunk[..read]);
                        if !signaled && received.windows(4).any(|window| window == b"\r\n\r\n") {
                            signaled = true;
                            let _ = head_landed.send(());
                        }
                    }
                    // Read timeout: return what the stream delivered so the
                    // assertion can name the miss.
                    Err(_) => break,
                }
            }
            String::from_utf8_lossy(&received).into_owned()
        })
    }

    // An open SSE connection coexists with a concurrent request/response
    // connection: the stream head lands (the server has subscribed), the
    // POST completes while the stream stays open, and the frame the sink
    // pushed arrives on the stream.
    #[tokio::test(flavor = "multi_thread")]
    async fn sse_stream_coexists_with_a_concurrent_request() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let dir = crate::testutil::test_path("transport-sse-coexist");
        std::fs::create_dir_all(&dir).unwrap();
        let (events, _receiver) = tokio::sync::broadcast::channel::<PushEvent>(16);
        let feed = events.clone();
        let arrivals: ArrivalSink = Arc::new(move |arrival| {
            let _ = feed.send(PushEvent {
                kind: "submission-received",
                payload: serde_json::to_value(&arrival).unwrap(),
            });
        });
        let handle = start_with_events(
            pool,
            test_deps(&dir, arrivals, noop_refreshes(), noop_comment_changes()),
            test_config(0),
            McpStatusHandle::for_config(&test_config(0)),
            Some(events),
        )
        .await
        .unwrap()
        .unwrap();
        let discovery: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(&dir)).unwrap()).unwrap();

        let (head_landed, head_received) = std::sync::mpsc::channel();
        let streamed = read_events_stream(
            discovery.port,
            Some(&discovery.token),
            None,
            "event: submission-received",
            head_landed,
        );
        head_received
            .recv_timeout(std::time::Duration::from_secs(15))
            .expect("the stream head never landed");

        let body = rpc_body(json!(1), "post_review", review_params("/demo"));
        let response = send_over_socket(discovery.port, &discovery.token, body).join().unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "unexpected response: {response}");

        let received = streamed.join().unwrap();
        assert!(
            received.contains("content-type: text/event-stream"),
            "unexpected stream head: {received}"
        );
        let payload_text = received
            .split("event: submission-received\ndata: ")
            .nth(1)
            .unwrap_or_else(|| panic!("no submission frame on the stream: {received}"))
            .split("\n\n")
            .next()
            .unwrap_or("");
        let payload: Value = serde_json::from_str(payload_text).unwrap();
        assert!(payload["submission_id"].as_i64().unwrap() > 0);
        assert_eq!(payload["agent_name"], "reviewer-bot");
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The SSE route rides the same bearer evaluation: no token and a wrong
    // token get the shared 401 shape, and the listener's own default token
    // opens the stream.
    #[tokio::test(flavor = "multi_thread")]
    async fn sse_stream_refuses_missing_or_wrong_tokens_and_accepts_the_default() {
        let pool = test_pool().await;
        let dir = crate::testutil::test_path("transport-sse-auth");
        std::fs::create_dir_all(&dir).unwrap();
        let (events, _receiver) = tokio::sync::broadcast::channel::<PushEvent>(16);
        let handle = start_with_events(
            pool,
            test_deps(&dir, Arc::new(|_| {}), noop_refreshes(), noop_comment_changes()),
            test_config(0),
            McpStatusHandle::for_config(&test_config(0)),
            Some(events),
        )
        .await
        .unwrap()
        .unwrap();
        let discovery: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(&dir)).unwrap()).unwrap();

        // The body marker keeps the reader waiting until the refusal's
        // body has landed, not just its head.
        let (missing_head, _) = std::sync::mpsc::channel();
        let missing = read_events_stream(discovery.port, None, None, "-32001", missing_head)
            .join()
            .unwrap();
        assert!(missing.contains("HTTP/1.1 401"), "unexpected response: {missing}");
        assert!(missing.contains("-32001"), "unexpected refusal: {missing}");

        let (wrong_head, _) = std::sync::mpsc::channel();
        let wrong = read_events_stream(discovery.port, Some("wrong-token"), None, "-32001", wrong_head)
            .join()
            .unwrap();
        assert!(wrong.contains("HTTP/1.1 401"), "unexpected response: {wrong}");

        let (opened_head, _) = std::sync::mpsc::channel();
        let opened = read_events_stream(
            discovery.port,
            Some(&discovery.token),
            None,
            "content-type: text/event-stream",
            opened_head,
        )
        .join()
        .unwrap();
        assert!(opened.contains("HTTP/1.1 200 OK"), "unexpected response: {opened}");
        assert!(
            opened.contains("content-type: text/event-stream"),
            "unexpected stream head: {opened}"
        );
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // SSE rides the shared bearer resolution, so a human token opens the
    // same stream: the admin's secret is accepted while a member's non-admin
    // status would change nothing about the stream.
    #[tokio::test(flavor = "multi_thread")]
    async fn sse_stream_accepts_a_human_token() {
        let pool = test_pool().await;
        let admin_secret = crate::identity::create_first_admin_in_pool(&pool, "ops")
            .await
            .unwrap()
            .secret;
        let dir = crate::testutil::test_path("transport-sse-human");
        std::fs::create_dir_all(&dir).unwrap();
        let (events, _receiver) = tokio::sync::broadcast::channel::<PushEvent>(16);
        let handle = start_with_events(
            pool,
            test_deps(&dir, Arc::new(|_| {}), noop_refreshes(), noop_comment_changes()),
            test_config(0),
            McpStatusHandle::for_config(&test_config(0)),
            Some(events),
        )
        .await
        .unwrap()
        .unwrap();
        let port = handle_local_port(&dir);

        let (head_landed, head_received) = std::sync::mpsc::channel();
        let opened = read_events_stream(
            port,
            Some(&admin_secret),
            None,
            "content-type: text/event-stream",
            head_landed,
        )
        .join()
        .unwrap();
        assert!(opened.contains("HTTP/1.1 200 OK"), "unexpected response: {opened}");
        assert!(
            opened.contains("content-type: text/event-stream"),
            "unexpected stream head: {opened}"
        );
        let _ = head_received.recv_timeout(std::time::Duration::from_secs(1));
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The bound port for a start with port 0: the config file records it.
    fn handle_local_port(dir: &std::path::Path) -> u16 {
        let discovery: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(dir)).unwrap()).unwrap();
        discovery.port
    }

    // The webview's CORS face: an allow-listed origin's preflight for a
    // consumed face is answered with the reflected origin and the face's
    // method, the actual command-API response carries the origin too, and
    // every other caller (foreign origin, no origin, the agent face) keeps
    // today's header-free answers.
    #[tokio::test(flavor = "multi_thread")]
    async fn webview_preflights_are_answered_and_other_callers_keep_the_plain_answers() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let dir = crate::testutil::test_path("transport-cors");
        std::fs::create_dir_all(&dir).unwrap();
        let (events, _receiver) = tokio::sync::broadcast::channel::<PushEvent>(16);
        let handle = start_with_events(
            pool.clone(),
            test_deps(&dir, Arc::new(|_| {}), noop_refreshes(), noop_comment_changes()),
            test_config(0),
            McpStatusHandle::for_config(&test_config(0)),
            Some(events),
        )
        .await
        .unwrap()
        .unwrap();
        let discovery: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(&dir)).unwrap()).unwrap();
        let admin = crate::identity::create_first_admin_in_pool(&pool, "ops").await.unwrap();

        let preflight = |origin: Option<&str>, path: &str, method: &str| {
            let origin_header = origin
                .map(|value| format!("origin: {value}\r\n"))
                .unwrap_or_default();
            send_raw_over_socket(
                discovery.port,
                format!(
                    "OPTIONS {path} HTTP/1.1\r\nhost: 127.0.0.1\r\n{origin_header}access-control-request-method: {method}\r\naccess-control-request-headers: authorization, content-type\r\nconnection: close\r\n\r\n"
                ),
            )
            .join()
            .unwrap()
        };

        let answered = preflight(Some("tauri://localhost"), "/api/list_repos", "POST");
        assert!(answered.starts_with("HTTP/1.1 200 OK"), "unexpected response: {answered}");
        assert!(
            answered.contains("access-control-allow-origin: tauri://localhost"),
            "unexpected preflight: {answered}"
        );
        assert!(
            answered.contains("access-control-allow-methods: POST"),
            "unexpected preflight: {answered}"
        );
        assert!(
            answered.contains("access-control-allow-headers: Authorization, Content-Type"),
            "unexpected preflight: {answered}"
        );
        assert!(answered.contains("access-control-max-age: 600"), "unexpected preflight: {answered}");
        assert_eq!(answered.split("\r\n\r\n").nth(1), Some(""));

        let events_preflight = preflight(Some("http://tauri.localhost"), "/events", "GET");
        assert!(events_preflight.starts_with("HTTP/1.1 200 OK"), "unexpected response: {events_preflight}");
        assert!(
            events_preflight.contains("access-control-allow-origin: http://tauri.localhost"),
            "unexpected preflight: {events_preflight}"
        );
        assert!(
            events_preflight.contains("access-control-allow-methods: GET"),
            "unexpected preflight: {events_preflight}"
        );

        let foreign = preflight(Some("https://evil.example"), "/api/list_repos", "POST");
        assert!(foreign.starts_with("HTTP/1.1 405"), "unexpected response: {foreign}");
        assert!(!foreign.contains("access-control"), "unexpected CORS headers: {foreign}");

        let originless = preflight(None, "/api/list_repos", "POST");
        assert!(originless.starts_with("HTTP/1.1 405"), "unexpected response: {originless}");
        assert!(!originless.contains("access-control"), "unexpected CORS headers: {originless}");

        // The actual command-API response carries the reflected origin so
        // the engine hands the body to the reader.
        let request = format!(
            "POST /api/list_repos HTTP/1.1\r\nhost: 127.0.0.1\r\nauthorization: Bearer {}\r\norigin: tauri://localhost\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}",
            admin.secret,
        );
        let served = send_raw_over_socket(discovery.port, request).join().unwrap();
        assert!(served.starts_with("HTTP/1.1 200 OK"), "unexpected response: {served}");
        assert!(
            served.contains("access-control-allow-origin: tauri://localhost"),
            "unexpected response: {served}"
        );

        // The agent face never gains CORS headers, webview origin included.
        let body = rpc_body(json!(1), "post_review", review_params("/demo"));
        let agent = send_raw_over_socket(
            discovery.port,
            format!(
                "POST / HTTP/1.1\r\nhost: 127.0.0.1\r\nauthorization: Bearer {}\r\norigin: tauri://localhost\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                discovery.token,
                body.len(),
            ),
        )
        .join()
        .unwrap();
        assert!(agent.starts_with("HTTP/1.1 200 OK"), "unexpected response: {agent}");
        assert!(!agent.contains("access-control"), "unexpected CORS headers: {agent}");
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The SSE head carries the reflected origin only for an allow-listed
    // webview; a foreign origin keeps the bare head the endpoint has always
    // written.
    #[tokio::test(flavor = "multi_thread")]
    async fn sse_head_carries_the_origin_only_for_webview_callers() {
        let pool = test_pool().await;
        let dir = crate::testutil::test_path("transport-sse-cors");
        std::fs::create_dir_all(&dir).unwrap();
        let (events, _receiver) = tokio::sync::broadcast::channel::<PushEvent>(16);
        let handle = start_with_events(
            pool,
            test_deps(&dir, Arc::new(|_| {}), noop_refreshes(), noop_comment_changes()),
            test_config(0),
            McpStatusHandle::for_config(&test_config(0)),
            Some(events),
        )
        .await
        .unwrap()
        .unwrap();
        let discovery: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(&dir)).unwrap()).unwrap();

        let (webview_head, _) = std::sync::mpsc::channel();
        let webview = read_events_stream(
            discovery.port,
            Some(&discovery.token),
            Some("https://tauri.localhost"),
            "content-type: text/event-stream",
            webview_head,
        )
        .join()
        .unwrap();
        assert!(webview.contains("HTTP/1.1 200 OK"), "unexpected response: {webview}");
        assert!(
            webview.contains("access-control-allow-origin: https://tauri.localhost"),
            "unexpected stream head: {webview}"
        );
        assert!(
            webview.contains("content-type: text/event-stream"),
            "unexpected stream head: {webview}"
        );

        let (bare_head, _) = std::sync::mpsc::channel();
        let bare = read_events_stream(
            discovery.port,
            Some(&discovery.token),
            Some("https://evil.example"),
            "content-type: text/event-stream",
            bare_head,
        )
        .join()
        .unwrap();
        assert!(bare.contains("HTTP/1.1 200 OK"), "unexpected response: {bare}");
        assert!(
            bare.contains("content-type: text/event-stream"),
            "unexpected stream head: {bare}"
        );
        assert!(!bare.contains("access-control"), "unexpected CORS headers: {bare}");
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A restart stops the idle listener without any inbound connection,
    // rebinds the same port, renews the default (the old secret stops
    // authenticating), rewrites the config file, and reports through
    // the same status handle while the owner carries the new listener.
    #[tokio::test(flavor = "multi_thread")]
    async fn restart_rebinds_the_same_port_and_renews_the_default() {
        let pool = test_pool().await;
        let dir = crate::testutil::test_path("transport-restart");
        std::fs::create_dir_all(&dir).unwrap();
        let port = free_port();
        let config = test_config(port);
        let deps = test_deps(&dir, Arc::new(|_| {}), noop_refreshes(), noop_comment_changes());
        let status = McpStatusHandle::for_config(&config);
        let owner = ListenerOwner::default();
        let first = start(pool.clone(), deps.clone(), config.clone(), status.clone())
            .await
            .unwrap()
            .unwrap();
        owner.0.lock().await.replace(first);
        let first_discovery: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(&dir)).unwrap()).unwrap();

        let status_after =
            restart(pool.clone(), &deps, config, status.clone(), &owner).await.unwrap();
        assert!(status_after.running);
        assert_eq!(status_after.port, port);
        assert_eq!(status_after.error, None);
        assert_eq!(status.lock_status().running, true, "the shared status handle follows the restart");
        let second_discovery: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(&dir)).unwrap()).unwrap();
        assert_eq!(second_discovery.port, port);
        assert_ne!(second_discovery.token, first_discovery.token);
        assert!(
            crate::agents::authenticate_token_in_pool(&pool, &first_discovery.token).await.is_none(),
            "the previous default must stop authenticating after a restart"
        );
        assert!(owner.0.lock().await.is_some(), "the restarted listener is owned again");
        owner.0.lock().await.take().unwrap().stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
