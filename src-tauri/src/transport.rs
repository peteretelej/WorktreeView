use crate::reviews::{ingest_submission_in_pool, SubmissionPayload};
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;

// Coarse pre-parse guard: the ingest's 2 MiB payload cap is the
// authoritative size rule; this only bounds what the transport reads, with
// 1 MiB of slack for JSON framing and identity fields so an ingest-legal
// submission is never size-rejected here.
const TRANSPORT_BODY_GUARD_BYTES: usize = 3 * 1024 * 1024;

// A connection that neither delivers nor closes inside this window is cut;
// real deliveries are single sub-3 MiB loopback requests.
const CONNECTION_STALL_LIMIT: Duration = Duration::from_secs(30);

// Our responses are small JSON or plain text; anything larger is a bug.
const RESPONSE_BODY_LIMIT: usize = 1024 * 1024;
// A request head beyond this is refused before parsing continues.
const REQUEST_HEAD_LIMIT: usize = 64 * 1024;

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;
const UNAUTHORIZED: i32 = -32001;
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

#[derive(Clone)]
pub(crate) struct TransportState {
    pool: SqlitePool,
    token: String,
    arrivals: ArrivalSink,
}

#[derive(Deserialize)]
struct ReviewIdentityParams {
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
}

#[derive(Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

fn rpc_error(id: &Value, status: StatusCode, code: i32, message: impl Into<String>) -> Response {
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

pub(crate) async fn handle(State(state): State<TransportState>, request: Request) -> Response {
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {}", state.token));
    if !authorized {
        return rpc_error(
            &Value::Null,
            StatusCode::UNAUTHORIZED,
            UNAUTHORIZED,
            "Missing or wrong bearer token; re-read the discovery file for the current boot.",
        );
    }
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
    eprintln!("HANDLE: body {} bytes parsed framing", bytes.len());
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
    if method != "post_review" {
        return rpc_error(
            &id,
            StatusCode::OK,
            METHOD_NOT_FOUND,
            "Unknown method; the endpoint accepts only post_review.",
        );
    }
    // serde flatten would silently absorb unknown keys, defeating the
    // payload's strict schema: validate the identity fields alone, strip
    // them, and let the payload's own deny_unknown_fields hold the line.
    let identity: ReviewIdentityParams = match serde_json::from_value(params.clone()) {
        Ok(identity) => identity,
        Err(error) => {
            return rpc_error(&id, StatusCode::OK, INVALID_PARAMS, format!("Invalid post_review params: {error}"));
        }
    };
    if identity.target_kind != "worktree" && identity.target_kind != "head" {
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
        &identity.repo_path,
        &identity.base_sha,
        &identity.target_key,
        &identity.target_kind,
        &submission,
    )
    .await
    {
        Ok(submission_id) => {
            (state.arrivals)(SubmissionArrival {
                repo_path: identity.repo_path,
                base_sha: identity.base_sha,
                target_key: identity.target_key,
                target_kind: identity.target_kind,
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

pub(crate) struct TransportHandle {
    shutdown: Arc<AtomicBool>,
}

impl TransportHandle {
    pub(crate) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

pub(crate) fn generate_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("Could not generate the endpoint token: {error}"))?;
    let mut token = String::with_capacity(64);
    for byte in bytes {
        token.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        token.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    Ok(token)
}

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct EndpointDiscovery {
    port: u16,
    token: String,
}

pub(crate) fn discovery_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent-endpoint.json")
}

fn write_discovery_file(path: &Path, port: u16, token: &str) -> Result<(), String> {
    let payload = serde_json::to_vec(&EndpointDiscovery { port, token: token.to_string() })
        .map_err(|error| format!("Could not serialize the endpoint discovery file: {error}"))?;
    std::fs::write(path, payload)
        .map_err(|error| format!("Could not write the endpoint discovery file: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
            format!("Could not restrict the endpoint discovery file: {error}")
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

// One connection: exactly one request, one response, then close. Serving
// inline keeps the whole request path free of cross-task scheduling; the
// stall limit bounds a client that neither delivers nor closes.
async fn serve_connection(stream: &mut tokio::net::TcpStream, state: TransportState) {
    let handled = async {
        let mut buffer: Vec<u8> = Vec::with_capacity(1024);
        let head_len = read_request_head(stream, &mut buffer).await.map_err(|_| "malformed head")?;
        let (method, path, content_length) = {
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut parsed = httparse::Request::new(&mut headers);
            let httparse::Status::Complete(_) = parsed
                .parse(&buffer)
                .map_err(|_| "malformed head")?
            else {
                return Err("malformed head");
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
            (method, path, content_length)
        };
        let path = path.split('?').next().unwrap_or("/").to_string();
        if method != "POST" {
            let response = plain_error_response(StatusCode::METHOD_NOT_ALLOWED, "POST / is the only endpoint");
            write_response(stream, response).await.map_err(|_| "write failed")?;
            return Ok(());
        }
        if path != "/" {
            let response = plain_error_response(StatusCode::NOT_FOUND, "POST / is the only endpoint");
            write_response(stream, response).await.map_err(|_| "write failed")?;
            return Ok(());
        }
        if content_length > TRANSPORT_BODY_GUARD_BYTES {
            let response = rpc_error(
                &Value::Null,
                StatusCode::PAYLOAD_TOO_LARGE,
                REQUEST_TOO_LARGE,
                format!("The request body exceeds the {TRANSPORT_BODY_GUARD_BYTES} byte guard."),
            );
            write_response(stream, response).await.map_err(|_| "write failed")?;
            return Ok(());
        }
        while buffer.len() - head_len < content_length {
            let mut chunk = [0u8; 4096];
            let read = stream.read(&mut chunk).await.map_err(|_| "read failed")?;
            if read == 0 {
                return Err("connection closed mid-body");
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let body = buffer[head_len..head_len + content_length].to_vec();

        let mut builder = Request::builder().method(method.as_str()).uri(path.as_str());
        for header in buffer_headers(&buffer, head_len).map_err(|_| "malformed head")? {
            builder = builder.header(header.0, header.1);
        }
        let request = builder
            .body(axum::body::Body::from(body))
            .map_err(|_| "unrepresentable request")?;
        let response = handle(State(state), request).await;
        write_response(stream, response).await.map_err(|_| "write failed")
    };
    let _ = tokio::time::timeout(CONNECTION_STALL_LIMIT, handled).await;
}

fn status_line(status: StatusCode) -> String {
    format!("HTTP/1.1 {} {}\r\n", status.as_u16(), status_reason(status.as_u16()))
}

async fn write_response(stream: &mut tokio::net::TcpStream, response: Response) -> Result<(), ()> {
    use tokio::io::AsyncWriteExt;
    let (parts, body) = response.into_parts();
    let body = to_bytes(body, RESPONSE_BODY_LIMIT).await.map_err(|_| ())?;
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

pub(crate) fn start(pool: SqlitePool, data_dir: &Path, arrivals: ArrivalSink) -> Result<TransportHandle, String> {
    let token = generate_token()?;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("Could not bind the agent endpoint to loopback: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("Could not resolve the agent endpoint port: {error}"))?
        .port();
    let discovery = discovery_path(data_dir);
    write_discovery_file(&discovery, port, &token)?;
    let state = TransportState { pool, token, arrivals };
    let shutdown = Arc::new(AtomicBool::new(false));
    let exit = Arc::clone(&shutdown);
    // The listener runs on its own single-thread runtime on a dedicated
    // thread and serves connections INLINE (no spawned per-connection
    // tasks): the endpoint is one write-only method whose deliveries are
    // bounded by the body guard, so serial handling cannot queue behind
    // scheduler wakeups, and a stalled client is cut by the stall limit.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("Could not start the agent endpoint runtime: {error}"))?;
    std::thread::Builder::new()
        .name("agent-endpoint".into())
        .spawn(move || {
            let _ = runtime.block_on(async move {
                let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                    let _ = std::fs::remove_file(&discovery);
                    return;
                };
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }
                    let (mut stream, _) = match listener.accept().await {
                        Ok(accepted) => accepted,
                        Err(_) => break,
                    };
                    serve_connection(&mut stream, state.clone()).await;
                }
                let _ = std::fs::remove_file(&discovery);
            });
        })
        .map_err(|error| format!("Could not start the agent endpoint thread: {error}"))?;
    Ok(TransportHandle { shutdown })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{seed_repo, test_pool};
    use axum::body::Body;
    use std::sync::Mutex;

    fn test_state(pool: SqlitePool, arrivals: ArrivalSink) -> TransportState {
        TransportState { pool, token: "test-token".into(), arrivals }
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

    fn recording_sink() -> (ArrivalSink, Arc<Mutex<Vec<SubmissionArrival>>>) {
        let received: Arc<Mutex<Vec<SubmissionArrival>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        (Arc::new(move |arrival| sink.lock().unwrap().push(arrival)), received)
    }

    #[test]
    fn token_is_64_lowercase_hex_and_unique_per_boot() {
        let first = generate_token().unwrap();
        let second = generate_token().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase()));
        assert_ne!(first, second);
    }

    #[test]
    fn discovery_file_round_trips_port_and_token() {
        let dir = crate::testutil::test_path("transport-discovery");
        std::fs::create_dir_all(&dir).unwrap();
        let path = discovery_path(&dir);
        write_discovery_file(&path, 45123, "abc123").unwrap();
        let parsed: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(parsed, EndpointDiscovery { port: 45123, token: "abc123".into() });
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the discovery file must be owner-only");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_or_wrong_bearer_token_is_unauthorized() {
        let pool = test_pool().await;
        let state = test_state(pool, Arc::new(|_| {}));
        let body = rpc_body(json!(1), "post_review", review_params("/demo"));
        let (status, payload) = post(state.clone(), None, &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["error"]["code"], UNAUTHORIZED);
        let (status, payload) = post(state, Some("wrong-token"), &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["error"]["code"], UNAUTHORIZED);
    }

    #[tokio::test]
    async fn malformed_json_is_a_parse_error() {
        let pool = test_pool().await;
        let state = test_state(pool, Arc::new(|_| {}));
        let (status, payload) = post(state, Some("test-token"), "{not json").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["error"]["code"], PARSE_ERROR);
    }

    #[tokio::test]
    async fn wrong_jsonrpc_version_is_an_invalid_request() {
        let pool = test_pool().await;
        let state = test_state(pool, Arc::new(|_| {}));
        let body = json!({ "jsonrpc": "1.0", "id": 2, "method": "post_review", "params": review_params("/demo") })
            .to_string();
        let (status, payload) = post(state, Some("test-token"), &body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["error"]["code"], INVALID_REQUEST);
        assert_eq!(payload["id"], 2);
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let pool = test_pool().await;
        let state = test_state(pool, Arc::new(|_| {}));
        let (status, payload) =
            post(state, Some("test-token"), &rpc_body(json!(3), "list_reviews", json!({}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(payload["id"], 3);
    }

    #[tokio::test]
    async fn oversized_body_is_rejected_without_parsing() {
        let pool = test_pool().await;
        let state = test_state(pool, Arc::new(|_| {}));
        let body = "x".repeat(TRANSPORT_BODY_GUARD_BYTES + 1);
        let (status, payload) = post(state, Some("test-token"), &body).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(payload["error"]["code"], REQUEST_TOO_LARGE);
    }

    #[tokio::test]
    async fn valid_post_review_stores_through_the_real_pool_and_announces() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (arrivals, received) = recording_sink();
        let state = test_state(pool.clone(), arrivals);
        let (status, payload) =
            post(state, Some("test-token"), &rpc_body(json!(7), "post_review", review_params("/demo"))).await;
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
        let state = test_state(pool, arrivals);
        let (status, payload) =
            post(state, Some("test-token"), &rpc_body(json!(8), "post_review", review_params("/missing"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["error"]["code"], UNKNOWN_REVIEW_TARGET);
        assert_eq!(received.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn submission_shape_violations_are_invalid_params() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let state = test_state(pool, Arc::new(|_| {}));

        let mut bad_priority = review_params("/demo");
        bad_priority["findings"][0]["priority"] = json!("P9");
        let (_, payload) = post(state.clone(), Some("test-token"), &rpc_body(json!(9), "post_review", bad_priority)).await;
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);

        let mut unknown_field = review_params("/demo");
        unknown_field["surprise"] = json!(true);
        let (_, payload) =
            post(state.clone(), Some("test-token"), &rpc_body(json!(10), "post_review", unknown_field)).await;
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);

        let mut bad_target_kind = review_params("/demo");
        bad_target_kind["target_kind"] = json!("branch");
        let (_, payload) = post(state, Some("test-token"), &rpc_body(json!(11), "post_review", bad_target_kind)).await;
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);
    }

    // End-to-end over a real loopback socket: start() binds, writes the
    // discovery file with the bound port and current token, and the served
    // handler answers an authorized POST. This is the desktop e2e's transport
    // path without a Tauri app. The test runtime stays multi-thread so the
    // pool's connection workers stay alive while the handler runs on the
    // endpoint's own dedicated runtime, mirroring production (Tauri's
    // runtime owns the pool; it never sleeps).
    #[tokio::test(flavor = "multi_thread")]
    async fn started_endpoint_serves_post_review_over_a_live_socket() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::time::{Duration, Instant};

        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (arrivals, received) = recording_sink();
        let dir = crate::testutil::test_path("transport-socket");
        std::fs::create_dir_all(&dir).unwrap();
        let handle = start(pool.clone(), &dir, arrivals).unwrap();
        let discovery: EndpointDiscovery =
            serde_json::from_slice(&std::fs::read(discovery_path(&dir)).unwrap()).unwrap();

        let body = rpc_body(json!(1), "post_review", review_params("/demo"));
        let request = format!(
            "POST / HTTP/1.1\r\nhost: 127.0.0.1\r\nauthorization: Bearer {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            discovery.token,
            body.len(),
        );
        // Blocking std client on its own thread: no async runtime involved.
        let client = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut stream = loop {
                match TcpStream::connect(("127.0.0.1", discovery.port)) {
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
            response
        });
        let response = String::from_utf8(client.join().unwrap()).unwrap();
        handle.shutdown();
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
}
