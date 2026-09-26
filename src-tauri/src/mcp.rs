use crate::agents::AgentIdentity;
use crate::commands::{list_worktrees_in_pool, refresh_repo};
use crate::events::{list_events_in_pool, EventQuery, EVENT_LIST_LIMIT};
use crate::overview::branch_inventory;
use crate::portal::{
    list_portal_threads_in_pool, PortalThreadQuery, THREAD_STATE_OPEN, THREAD_STATE_RESOLVED,
};
use crate::requests::{
    announce_review_in_pool, claim_request_in_pool, create_request_in_pool, re_request_in_pool,
    set_request_verdict_in_pool, withdraw_request_in_pool, Actor as RequestActor, RequestDraft,
    ReviewRequest, APPROVED, CHANGES_REQUESTED, IN_REVIEW, REQUESTED, WITHDRAWN,
};
use crate::reviews::{
    create_unbound_comment_in_pool, delete_comment_in_pool, edit_comment_in_pool,
    list_comments_in_pool, list_submissions_in_pool, reply_comment_in_pool,
    MAX_AGENT_MODEL_CHARS,
    review_identity_of_comment, set_comment_resolved_in_pool, Actor,
};
use crate::store::{list_repo_rows_in_pool, now_millis, open_repo_path};
use crate::transport::{
    rpc_error, CommentChange, TransportState, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND,
    PARSE_ERROR,
};
use crate::CommandError;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

// The one protocol revision this face speaks. Tools requests carry it in
// `_meta["io.modelcontextprotocol/protocolVersion"]`; a missing one is
// treated as this revision.
const PROTOCOL_VERSION: &str = "2026-07-28";
const PROTOCOL_VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
// An `initialize` client names the revision it speaks. The face behaves the
// same under every revision (stateless, per-request version default), so any
// well-formed revision is echoed back to let the client proceed; only an
// absent or malformed name gets the supported revision.
fn negotiated_protocol_version(requested: Option<&str>) -> &str {
    let well_formed = requested
        .map(str::as_bytes)
        .is_some_and(|bytes| bytes.len() == 10 && bytes[4] == b'-' && bytes[7] == b'-');
    if well_formed {
        requested.unwrap()
    } else {
        PROTOCOL_VERSION
    }
}
// The spec's unsupported-version error.
const UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;
// The tool set is static per boot, so a generous private cache hint is
// honest; no tool result can change without an app restart.
const TOOL_CACHE_TTL_MS: u64 = 3_600_000;

// The activity cursor's default page: one bounded poll answers a useful
// slice, and the store's cap bounds whatever the caller asks for.
const LIST_ACTIVITY_DEFAULT_LIMIT: i64 = 100;

// The request statuses agents can filter by, mirroring the engine's
// lifecycle vocabulary.
const REQUEST_STATUSES: [&str; 5] = [
    REQUESTED,
    IN_REVIEW,
    CHANGES_REQUESTED,
    APPROVED,
    WITHDRAWN,
];

// One grouped, indexed query over the identity join: each request joins its
// identity's single review row and its comments, so the triage counts come
// back with the stored fields and no Git runs on this path. The status
// filter pins one exact status; without it the open queue applies
// (everything except approved and withdrawn).
const LIST_REVIEW_REQUESTS_QUERY: &str = "SELECT rq.id, rq.repo_path, rq.base_sha, rq.target_key, \
     rq.target_kind, rq.requester_token_id, rq.status, rq.note, rq.lenses, rq.reviewers, \
     rq.max_rounds, rq.round, rq.head_sha, rq.created_at, rq.updated_at, \
     t.name AS requester_name, \
     COUNT(c.id) AS comment_count, \
     SUM(CASE WHEN c.severity = 'P0' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p0, \
     SUM(CASE WHEN c.severity = 'P1' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p1, \
     SUM(CASE WHEN c.severity = 'P2' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p2, \
     SUM(CASE WHEN c.severity = 'P3' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p3 \
     FROM review_requests AS rq \
     LEFT JOIN reviews AS rv ON rv.repo_path = rq.repo_path AND rv.base_sha = rq.base_sha \
       AND rv.target_key = rq.target_key AND rv.target_kind = rq.target_kind \
     LEFT JOIN comments AS c ON c.review_id = rv.id \
     LEFT JOIN agent_tokens AS t ON t.id = rq.requester_token_id \
     WHERE (?1 IS NULL OR rq.repo_path = ?1) \
       AND (rq.status = ?2 OR (?2 IS NULL AND rq.status NOT IN ('withdrawn', 'approved'))) \
     GROUP BY rq.id ORDER BY rq.created_at DESC, rq.id DESC";

fn server_info() -> Value {
    json!({ "name": "worktreeview", "version": env!("CARGO_PKG_VERSION") })
}

fn rpc_result(id: &Value, result: Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
    )
        .into_response()
}

// Tool outcomes are never JSON-RPC errors: a failure is a complete result
// with isError and the shared implementation's message in content, so
// agents can tell "your call ran and was refused" from "the call was
// malformed" (that one is -32602 before anything executes).
type ToolOutcome = Result<Value, String>;

fn tool_result(id: &Value, outcome: ToolOutcome) -> Response {
    let (is_error, text) = match outcome {
        Ok(value) => (
            false,
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string()),
        ),
        Err(message) => (true, message),
    };
    rpc_result(
        id,
        json!({
            "resultType": "complete",
            "isError": is_error,
            "content": [{ "type": "text", "text": text }],
            "_meta": { "io.modelcontextprotocol/serverInfo": server_info() },
        }),
    )
}

fn payload<T: serde::Serialize>(value: T) -> ToolOutcome {
    serde_json::to_value(value)
        .map_err(|error| format!("The result could not be serialized: {error}"))
}

// Arguments that fail a tool's schema are shape errors: a -32602 before
// anything executes, never an execution-failure result.
fn tool_args<T: for<'de> Deserialize<'de>>(id: &Value, arguments: &Value) -> Result<T, Response> {
    serde_json::from_value(arguments.clone()).map_err(|error| {
        rpc_error(
            id,
            StatusCode::OK,
            INVALID_PARAMS,
            format!("Invalid tool arguments: {error}"),
        )
    })
}

#[derive(Deserialize)]
struct IncomingRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListReviewTargetsArgs {
    repo_path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewIdentityArgs {
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateCommentArgs {
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    body: String,
    severity: Option<String>,
    author_model: Option<String>,
    file_path: Option<String>,
    side: Option<String>,
    start_line: Option<u32>,
    end_line: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyCommentArgs {
    parent_comment_id: i64,
    body: String,
    author_model: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveThreadArgs {
    root_comment_id: i64,
    resolved: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditOwnCommentArgs {
    comment_id: i64,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteOwnCommentArgs {
    comment_id: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddRepoArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshRepoArgs {
    repo_path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestReviewArgs {
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    note: Option<String>,
    head_sha: String,
    lenses: Option<Vec<String>>,
    reviewers: Option<Vec<String>>,
    max_rounds: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnnounceReviewArgs {
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    head_sha: String,
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListReviewRequestsArgs {
    repo_path: Option<String>,
    status: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListThreadsArgs {
    repo_path: Option<String>,
    state: Option<String>,
    since: Option<i64>,
    participant: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListActivityArgs {
    since_id: Option<i64>,
    repo_path: Option<String>,
    limit: Option<i64>,
}

// The transition vocabulary is a closed set: serde refuses any other value
// as a shape error before anything executes.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum UpdateRequestAction {
    Claim,
    Approve,
    RequestChanges,
    Withdraw,
    ReRequest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateReviewRequestArgs {
    id: i64,
    action: UpdateRequestAction,
    note: Option<String>,
    head_sha: Option<String>,
}

// The MCP face's entry: auth already ran once at the listener and `bytes`
// is the transport-guarded body. One request, one independent JSON answer;
// no sessions, no SSE, no batching.
pub(crate) async fn serve(
    state: &TransportState,
    identity: AgentIdentity,
    bytes: &[u8],
) -> Response {
    let null = Value::Null;
    let parsed: Value = match serde_json::from_slice(bytes) {
        Ok(parsed) => parsed,
        Err(_) => {
            return rpc_error(
                &null,
                StatusCode::OK,
                PARSE_ERROR,
                "The request body is not valid JSON.",
            );
        }
    };
    if parsed.is_array() {
        return rpc_error(
            &null,
            StatusCode::OK,
            INVALID_REQUEST,
            "Batch requests are not supported; send one request at a time.",
        );
    }
    // The id presence is read before the parsed body moves into the request.
    let has_id = parsed.get("id").is_some();
    let request: IncomingRequest = match serde_json::from_value(parsed) {
        Ok(request) => request,
        Err(_) => {
            return rpc_error(
                &null,
                StatusCode::OK,
                INVALID_REQUEST,
                "The request is not a valid JSON-RPC 2.0 request.",
            );
        }
    };
    if request.jsonrpc != "2.0" {
        return rpc_error(
            &request.id,
            StatusCode::OK,
            INVALID_REQUEST,
            "The request is not JSON-RPC 2.0.",
        );
    }
    // A notification (no id member) expects no JSON-RPC answer: 202 with an
    // empty body acknowledges receipt without inventing a response id.
    if !has_id {
        return StatusCode::ACCEPTED.into_response();
    }
    // The discovery probe needs no protocol version: negotiating one is its
    // purpose. Unknown and removed methods are refused before any version
    // check; only the real tool methods consult the version, and a missing
    // one counts as the supported revision.
    match request.method.as_str() {
        "server/discover" => {
            return rpc_result(
                &request.id,
                json!({
                    "protocolVersions": [PROTOCOL_VERSION],
                    "capabilities": { "tools": {} },
                    "serverInfo": server_info(),
                }),
            );
        }
        // Clients on pre-2026-07-28 revisions refuse to proceed without a
        // successful handshake, so the face answers one: echo a revision the
        // client named, default to the supported one, and store nothing.
        "initialize" => {
            let version = negotiated_protocol_version(
                request.params.get("protocolVersion").and_then(Value::as_str),
            );
            return rpc_result(
                &request.id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": server_info(),
                }),
            );
        }
        "ping" => return rpc_result(&request.id, json!({})),
        "tools/list" | "tools/call" => {
            if let Err(response) = check_protocol_version(&request.id, &request.params) {
                return response;
            }
        }
        _ => {
            return rpc_error(
                &request.id,
                StatusCode::OK,
                METHOD_NOT_FOUND,
                "Unknown method; the MCP face accepts server/discover, initialize, ping, tools/list, and tools/call.",
            );
        }
    }
    if request.method == "tools/list" {
        rpc_result(&request.id, json!({ "tools": tool_descriptors() }))
    } else {
        match handle_tools_call(state, &request.id, request.params, identity).await {
            Ok(response) | Err(response) => response,
        }
    }
}

// Clients predating the per-request `_meta` version omit it entirely, so a
// missing version counts as the supported revision; an explicit version
// must name it exactly.
fn check_protocol_version(id: &Value, params: &Value) -> Result<(), Response> {
    match params
        .get("_meta")
        .and_then(|meta| meta.get(PROTOCOL_VERSION_KEY))
    {
        None => Ok(()),
        Some(version) if version.as_str() == Some(PROTOCOL_VERSION) => Ok(()),
        Some(_) => Err(rpc_error(
            id,
            StatusCode::OK,
            UNSUPPORTED_PROTOCOL_VERSION,
            format!("Unsupported protocol version; this face supports {PROTOCOL_VERSION}."),
        )),
    }
}

fn tool_descriptors() -> Value {
    let schema = |properties: Value, required: &[&str]| {
        json!({ "type": "object", "properties": properties, "required": required })
    };
    let tool = |name: &str, description: &str, input_schema: Value| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": input_schema,
            "ttlMs": TOOL_CACHE_TTL_MS,
            "cacheScope": "private",
        })
    };
    let path_arg = |description: &str| json!({ "type": "string", "description": description });
    let review_identity = || {
        json!({
            "repo_path": path_arg("Exact repository path as listed by list_repos."),
            "base_sha": path_arg("Resolved base SHA the review keys on."),
            "target_key": path_arg(
                "Worktree path for target_kind \"worktree\"; resolved target SHA for \"head\".",
            ),
            "target_kind": { "type": "string", "enum": ["worktree", "head"] },
        })
    };
    // Deterministic order: reads, writes, action.
    json!([
        tool(
            "list_repos",
            "List repositories open in WorktreeView with path, name, timestamps, and pin state.",
            schema(json!({}), &[]),
        ),
        tool(
            "list_review_targets",
            "List one open repository's reviewable surfaces: its worktrees, local branches, and remote-tracking refs.",
            schema(
                json!({ "repo_path": path_arg("Exact repository path as listed by list_repos.") }),
                &["repo_path"],
            ),
        ),
        tool(
            "list_comments",
            "List stored comments for one review identity. Anchor fields are reported as stored; drift is not recomputed against Git.",
            schema(review_identity(), &["repo_path", "base_sha", "target_key", "target_kind"]),
        ),
        tool(
            "list_submissions",
            "List stored agent submissions with their typed sections for one review identity.",
            schema(review_identity(), &["repo_path", "base_sha", "target_key", "target_kind"]),
        ),
        tool(
            "list_review_requests",
            "List review requests with their stored fields and derived triage fields (age, comment count, unresolved finding counts by severity, requester). With no filters this is the open cross-repo queue: poll it on a sparse cadence to pick up work.",
            schema(
                json!({
                    "repo_path": path_arg("Optional exact repository path as listed by list_repos; omit for every open repository."),
                    "status": {
                        "type": "string",
                        "enum": ["requested", "in_review", "changes_requested", "approved", "withdrawn"],
                        "description": "Optional exact status filter; omitting it answers the open queue (everything except approved and withdrawn).",
                    },
                }),
                &[],
            ),
        ),
        tool(
            "list_threads",
            "List review threads (root comments and their replies) across every open repository, grouped by change. Anchor fields are reported as stored; drift is not recomputed against Git.",
            schema(
                json!({
                    "repo_path": path_arg("Optional exact repository path as listed by list_repos; omit for every open repository."),
                    "state": {
                        "type": "string",
                        "enum": ["open", "resolved"],
                        "description": "Optional thread state filter; omitting it answers open threads.",
                    },
                    "since": {
                        "type": "integer",
                        "description": "Optional epoch-milliseconds floor: only threads whose last activity is at or after this instant.",
                    },
                    "participant": {
                        "type": "string",
                        "description": "Optional exact participant name (human or agent) that authored the root or a reply.",
                    },
                }),
                &[],
            ),
        ),
        tool(
            "list_activity",
            "List review activity events (requests, submissions, comments, surface head moves, repo adds) across every open repository, ascending by id with a next_cursor. Poll with the cursor to answer exactly the events you have not seen yet.",
            schema(
                json!({
                    "since_id": {
                        "type": "integer",
                        "description": "Optional event-id cursor: only events with a greater id are answered; omit it (or pass 0) to start from the beginning of the log.",
                    },
                    "repo_path": path_arg("Optional exact repository path as listed by list_repos; omit for every open repository."),
                    "limit": {
                        "type": "integer",
                        "description": "Optional page size (default 100); the answer never exceeds the store's event page cap.",
                    },
                }),
                &[],
            ),
        ),
        tool(
            "create_comment",
            "Create a comment on a review; the author is your agent token, so you can edit and delete it later. Anchor shapes: no file for review-level, file only for file-level, file with side and start_line for line-level.",
            schema(
                json!({
                    "repo_path": path_arg("Exact repository path as listed by list_repos."),
                    "base_sha": path_arg("Resolved base SHA the review keys on."),
                    "target_key": path_arg(
                        "Worktree path for target_kind \"worktree\"; resolved target SHA for \"head\".",
                    ),
                    "target_kind": { "type": "string", "enum": ["worktree", "head"] },
                    "body": { "type": "string", "description": "Non-empty comment body." },
                    "severity": { "type": "string", "enum": ["P0", "P1", "P2", "P3"] },
                    "author_model": {
                        "type": "string",
                        "maxLength": 200,
                        "description": "Optional self-reported model label shown next to your agent name, e.g. \"GPT Luna medium\".",
                    },
                    "file_path": {
                        "type": "string",
                        "description": "Repo-relative file path; omit for a review-level comment.",
                    },
                    "side": {
                        "type": "string",
                        "enum": ["LEFT", "RIGHT"],
                        "description": "Diff side of a line anchor; LEFT is the old side.",
                    },
                    "start_line": { "type": "integer", "minimum": 1 },
                    "end_line": { "type": "integer", "minimum": 1, "description": "Defaults to start_line." },
                }),
                &["repo_path", "base_sha", "target_key", "target_kind", "body"],
            ),
        ),
        tool(
            "reply_comment",
            "Reply to a thread's root comment; replies to replies are refused.",
            schema(
                json!({
                    "parent_comment_id": {
                        "type": "integer",
                        "description": "The thread's root comment id.",
                    },
                    "body": { "type": "string", "description": "Non-empty reply body." },
                    "author_model": {
                        "type": "string",
                        "maxLength": 200,
                        "description": "Optional self-reported model label shown next to your agent name.",
                    },
                }),
                &["parent_comment_id", "body"],
            ),
        ),
        tool(
            "resolve_thread",
            "Resolve or reopen a thread by its root comment id; any agent may resolve any thread.",
            schema(
                json!({
                    "root_comment_id": { "type": "integer" },
                    "resolved": { "type": "boolean" },
                }),
                &["root_comment_id", "resolved"],
            ),
        ),
        tool(
            "edit_own_comment",
            "Edit the body of a comment your token authored; other tokens' comments, human comments, and legacy unowned comments are refused.",
            schema(
                json!({
                    "comment_id": { "type": "integer" },
                    "body": {
                        "type": "string",
                        "description": "Replacement body; edits are last-write-wins.",
                    },
                }),
                &["comment_id", "body"],
            ),
        ),
        tool(
            "delete_own_comment",
            "Delete a comment your token authored (deleting a root also removes its replies); other tokens' comments, human comments, and legacy unowned comments are refused.",
            schema(
                json!({ "comment_id": { "type": "integer" } }),
                &["comment_id"],
            ),
        ),
        tool(
            "add_repo",
            "Open a local Git repository by absolute path (idempotent if already open) so its worktrees, branches, reviews, and comments become readable. Repositories cannot be removed through this API.",
            schema(
                json!({ "path": path_arg("Absolute path to the Git repository or worktree folder.") }),
                &["path"],
            ),
        ),
        tool(
            "request_review",
            "Ask the fleet to review one review identity at a recorded head; your token becomes the requester. The same identity and head on your open request updates it in place instead of stacking duplicate pickups, and optional fields you omit keep their stored values.",
            schema(
                json!({
                    "repo_path": path_arg("Exact repository path as listed by list_repos."),
                    "base_sha": path_arg("Resolved base SHA the review keys on."),
                    "target_key": path_arg(
                        "Worktree path for target_kind \"worktree\"; resolved target SHA for \"head\".",
                    ),
                    "target_kind": { "type": "string", "enum": ["worktree", "head"] },
                    "note": {
                        "type": "string",
                        "maxLength": 2000,
                        "description": "Optional note: what changed, why, and what kind of review you need.",
                    },
                    "head_sha": path_arg(
                        "The head you want reviewed; a re-request must later record a different head.",
                    ),
                    "lenses": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": ["security", "correctness", "design", "performance", "tests"],
                        },
                        "description": "Optional focus areas; duplicates are refused.",
                    },
                    "reviewers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional named agent tokens; when set, only these tokens can claim the review.",
                    },
                    "max_rounds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 3,
                        "description": "Optional review round budget, default 2.",
                    },
                }),
                &["repo_path", "base_sha", "target_key", "target_kind", "head_sha"],
            ),
        ),
        tool(
            "announce_review",
            "Record that you are reviewing one identity at a recorded head: your token becomes the requester, the request enters review immediately, and its delivery settles the review. Use it when a verbal ask (or your own open request) starts your review; to pick up someone else's queue request, claim it with update_review_request instead.",
            schema(
                json!({
                    "repo_path": path_arg("Exact repository path as listed by list_repos."),
                    "base_sha": path_arg("Resolved base SHA the review keys on."),
                    "target_key": path_arg(
                        "Worktree path for target_kind \"worktree\"; resolved target SHA for \"head\".",
                    ),
                    "target_kind": { "type": "string", "enum": ["worktree", "head"] },
                    "head_sha": path_arg("The head you are reviewing."),
                    "note": {
                        "type": "string",
                        "maxLength": 2000,
                        "description": "Optional note: what changed, why, and what kind of review you need.",
                    },
                }),
                &["repo_path", "base_sha", "target_key", "target_kind", "head_sha"],
            ),
        ),
        tool(
            "update_review_request",
            "Advance one review request through its lifecycle: claim it, give a verdict (approve or request_changes), withdraw your own request, or re-request with a new head after changes.",
            schema(
                json!({
                    "id": {
                        "type": "integer",
                        "description": "The review request id from request_review or list_review_requests.",
                    },
                    "action": {
                        "type": "string",
                        "enum": ["claim", "approve", "request_changes", "withdraw", "re_request"],
                    },
                    "note": {
                        "type": "string",
                        "maxLength": 2000,
                        "description": "Replacement note; only valid with re_request.",
                    },
                    "head_sha": path_arg(
                        "The new head; required with re_request and it must differ from the head that received the changes.",
                    ),
                }),
                &["id", "action"],
            ),
        ),
        tool(
            "refresh_repo",
            "Ask the app to refresh one open repository's remote-tracking refs (the same bounded fetch the refresh button runs).",
            schema(
                json!({ "repo_path": path_arg("Exact repository path as listed by list_repos.") }),
                &["repo_path"],
            ),
        ),
    ])
}

async fn handle_tools_call(
    state: &TransportState,
    id: &Value,
    params: Value,
    identity: AgentIdentity,
) -> Result<Response, Response> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err(rpc_error(
            id,
            StatusCode::OK,
            INVALID_PARAMS,
            "tools/call params must name a tool.",
        ));
    };
    let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let outcome: ToolOutcome = match name {
        "add_repo" => {
            let args = tool_args(id, &arguments)?;
            add_repo(state, args).await
        }
        "list_repos" => list_repos(state).await,
        "list_review_targets" => {
            let args = tool_args(id, &arguments)?;
            list_review_targets(state, args).await
        }
        "list_comments" => {
            let args = tool_args(id, &arguments)?;
            list_comments(state, args).await
        }
        "list_submissions" => {
            let args = tool_args(id, &arguments)?;
            list_submissions(state, args).await
        }
        "list_review_requests" => {
            let args = tool_args(id, &arguments)?;
            list_review_requests(state, args).await
        }
        "list_threads" => {
            let args = tool_args(id, &arguments)?;
            list_threads(state, args).await
        }
        "list_activity" => {
            let args = tool_args(id, &arguments)?;
            list_activity(state, args).await
        }
        "create_comment" => {
            let args = tool_args(id, &arguments)?;
            create_comment(state, args, &identity).await
        }
        "reply_comment" => {
            let args = tool_args(id, &arguments)?;
            reply_comment(state, args, &identity).await
        }
        "resolve_thread" => {
            let args = tool_args(id, &arguments)?;
            resolve_thread(state, args, &identity).await
        }
        "edit_own_comment" => {
            let args = tool_args(id, &arguments)?;
            edit_own_comment(state, args, &identity).await
        }
        "delete_own_comment" => {
            let args = tool_args(id, &arguments)?;
            delete_own_comment(state, args, &identity).await
        }
        "refresh_repo" => {
            let args = tool_args(id, &arguments)?;
            refresh_repo_tool(state, args).await
        }
        "request_review" => {
            let args = tool_args(id, &arguments)?;
            request_review(state, args, &identity).await
        }
        "announce_review" => {
            let args = tool_args(id, &arguments)?;
            announce_review(state, args, &identity).await
        }
        "update_review_request" => {
            let args = tool_args(id, &arguments)?;
            update_review_request(state, args, &identity).await
        }
        _ => {
            return Err(rpc_error(
                id,
                StatusCode::OK,
                INVALID_PARAMS,
                format!("Unknown tool: {name}."),
            ));
        }
    };
    Ok(tool_result(id, outcome))
}

// The unknown-repo gate every repo-keyed tool shares, mirroring the ingest's
// check and message so a typo'd path fails cleanly instead of as a foreign
// key error. Reads beyond it stay find-only: no row is created by a read.
async fn ensure_repo_open(state: &TransportState, repo_path: &str) -> Result<(), CommandError> {
    let known: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repos WHERE path = ?")
        .bind(repo_path)
        .fetch_one(&state.pool)
        .await?;
    if known == 0 {
        return Err(CommandError::new(
            "unknown_review_target",
            "No repository with that path is open in WorktreeView.",
        ));
    }
    Ok(())
}

// The one state-writing tool agents get: registering a repository is the
// same read-only-over-Git open the UI's folder dialog runs, and the
// project-refreshed announce keeps the open app's sidebar current. Removal
// stays a human action in the UI.
async fn add_repo(state: &TransportState, args: AddRepoArgs) -> ToolOutcome {
    let repo = open_repo_path(&args.path, &state.pool)
        .await
        .map_err(|error| error.message)?;
    (state.refreshes)(repo.path());
    payload(repo)
}

async fn list_repos(state: &TransportState) -> ToolOutcome {
    let rows = list_repo_rows_in_pool(&state.pool)
        .await
        .map_err(|error| error.message)?;
    payload(rows)
}

async fn list_review_targets(state: &TransportState, args: ListReviewTargetsArgs) -> ToolOutcome {
    ensure_repo_open(state, &args.repo_path)
        .await
        .map_err(|error| error.message)?;
    let worktrees = list_worktrees_in_pool(&state.pool, &args.repo_path)
        .await
        .map_err(|error| error.message)?;
    let inventory = branch_inventory(&state.pool, args.repo_path.clone())
        .await
        .map_err(|error| error.message)?;
    payload(json!({
        "worktrees": worktrees,
        "branches": inventory.branches,
        "remote_branches": inventory.remote_branches,
    }))
}

async fn list_comments(state: &TransportState, args: ReviewIdentityArgs) -> ToolOutcome {
    let comments = list_comments_in_pool(
        &state.pool,
        &args.repo_path,
        &args.base_sha,
        &args.target_key,
        &args.target_kind,
    )
    .await
    .map_err(|error| error.message)?;
    payload(comments)
}

async fn list_submissions(state: &TransportState, args: ReviewIdentityArgs) -> ToolOutcome {
    let submissions = list_submissions_in_pool(
        &state.pool,
        &args.repo_path,
        &args.base_sha,
        &args.target_key,
        &args.target_kind,
    )
    .await
    .map_err(|error| error.message)?;
    payload(submissions)
}

// The tool result's request row: the stored fields plus the requester
// display. The engine owns every mutation; this only renders.
fn request_row_value(request: &ReviewRequest, requester: &str) -> Value {
    json!({
        "id": request.id,
        "repo_path": request.repo_path,
        "base_sha": request.base_sha,
        "target_key": request.target_key,
        "target_kind": request.target_kind,
        "status": request.status,
        "note": request.note,
        "lenses": request.lenses,
        "reviewers": request.reviewers,
        "max_rounds": request.max_rounds,
        "round": request.round,
        "head_sha": request.head_sha,
        "created_at": request.created_at,
        "updated_at": request.updated_at,
        "requester": requester,
    })
}

// The requester display: the token's name, or human for human-keyed rows
// (a deleted requester token also lands here, since the store nulls the
// key and the row keeps behaving as human-keyed). A store failure is an
// error, not a silent "human": attribution is fleet-visible.
async fn requester_display(
    pool: &sqlx::SqlitePool,
    request: &ReviewRequest,
) -> Result<String, String> {
    let name: Option<String> = match request.requester_token_id {
        None => None,
        Some(token_id) => sqlx::query_scalar("SELECT name FROM agent_tokens WHERE id = ?")
            .bind(token_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("The requester could not be read: {error}"))?,
    };
    Ok(name.unwrap_or_else(|| "human".to_string()))
}

async fn request_review(
    state: &TransportState,
    args: RequestReviewArgs,
    identity: &AgentIdentity,
) -> ToolOutcome {
    ensure_repo_open(state, &args.repo_path)
        .await
        .map_err(|error| error.message)?;
    let draft = RequestDraft {
        note: args.note,
        lenses: args.lenses.unwrap_or_default(),
        reviewers: args.reviewers.unwrap_or_default(),
        max_rounds: args.max_rounds,
        head_sha: args.head_sha,
    };
    let request = create_request_in_pool(
        &state.pool,
        &args.repo_path,
        &args.base_sha,
        &args.target_key,
        &args.target_kind,
        &draft,
        &RequestActor::Agent(identity.token_id),
        &state.request_changes,
    )
    .await
    .map_err(|error| error.message)?;
    let requester = requester_display(&state.pool, &request).await?;
    payload(request_row_value(&request, &requester))
}

// The announce tool: same engine entry as the raw face's method, same
// error mapping as request_review.
async fn announce_review(
    state: &TransportState,
    args: AnnounceReviewArgs,
    identity: &AgentIdentity,
) -> ToolOutcome {
    ensure_repo_open(state, &args.repo_path)
        .await
        .map_err(|error| error.message)?;
    let request = announce_review_in_pool(
        &state.pool,
        &args.repo_path,
        &args.base_sha,
        &args.target_key,
        &args.target_kind,
        &args.head_sha,
        args.note.as_deref(),
        &RequestActor::Agent(identity.token_id),
        &state.request_changes,
    )
    .await
    .map_err(|error| error.message)?;
    let requester = requester_display(&state.pool, &request).await?;
    payload(request_row_value(&request, &requester))
}

async fn list_review_requests(state: &TransportState, args: ListReviewRequestsArgs) -> ToolOutcome {
    if let Some(status) = args.status.as_deref() {
        if !REQUEST_STATUSES.contains(&status) {
            return Err(format!(
                "Unknown status '{status}'; statuses are {REQUESTED}, {IN_REVIEW}, \
                 {CHANGES_REQUESTED}, {APPROVED}, or {WITHDRAWN}."
            ));
        }
    }
    let now = now_millis();
    let rows = sqlx::query(LIST_REVIEW_REQUESTS_QUERY)
        .bind(args.repo_path)
        .bind(args.status)
        .fetch_all(&state.pool)
        .await
        .map_err(|error| format!("The review requests could not be listed: {error}"))?;
    let listed: Vec<Value> = rows
        .iter()
        .map(|row| request_list_row(row, now))
        .collect::<Result<Vec<_>, String>>()?;
    payload(listed)
}

// The cross-repo thread sweep: the portal's grouped listing with the
// tool's own state vocabulary (omitted state answers open threads), then
// the since and participant filters narrow the grouped rows. Reads stay
// find-only.
async fn list_threads(state: &TransportState, args: ListThreadsArgs) -> ToolOutcome {
    if let Some(state_filter) = args.state.as_deref() {
        if state_filter != THREAD_STATE_OPEN && state_filter != THREAD_STATE_RESOLVED {
            return Err(format!(
                "Unknown state '{state_filter}'; states are {THREAD_STATE_OPEN} or {THREAD_STATE_RESOLVED}."
            ));
        }
    }
    if let Some(repo_path) = &args.repo_path {
        ensure_repo_open(state, repo_path).await.map_err(|error| error.message)?;
    }
    let query = PortalThreadQuery {
        repo_path: args.repo_path,
        // The tool's omitted state is the portal's default: open threads.
        state: Some(args.state.unwrap_or_else(|| THREAD_STATE_OPEN.to_string())),
        voice: None,
        text: None,
    };
    let mut groups = list_portal_threads_in_pool(&state.pool, &query)
        .await
        .map_err(|error| error.message)?;
    for group in &mut groups {
        group.threads.retain(|thread| {
            args.since.map_or(true, |since| thread.last_activity_at >= since)
                && args.participant.as_deref().map_or(true, |name| {
                    thread.participants.iter().any(|known| known.author_name == name)
                })
        });
    }
    groups.retain(|group| !group.threads.is_empty());
    payload(groups)
}

// The bounded activity cursor: everything after the caller's last seen
// event id, ascending, so a sparse poll replays nothing and cannot pull
// the whole log. An empty answer keeps the cursor where it was.
async fn list_activity(state: &TransportState, args: ListActivityArgs) -> ToolOutcome {
    if let Some(repo_path) = &args.repo_path {
        ensure_repo_open(state, repo_path)
            .await
            .map_err(|error| error.message)?;
    }
    let since_id = args.since_id.unwrap_or(0).max(0);
    let limit = args
        .limit
        .unwrap_or(LIST_ACTIVITY_DEFAULT_LIMIT)
        .clamp(1, EVENT_LIST_LIMIT as i64) as usize;
    let events = list_events_in_pool(
        &state.pool,
        &EventQuery {
            since_id,
            repo_path: args.repo_path,
            limit,
            ascending: true,
        },
    )
    .await
    .map_err(|error| format!("The activity log could not be read: {error}"))?;
    let next_cursor = events.last().map_or(since_id, |event| event.id);
    payload(json!({ "events": events, "next_cursor": next_cursor }))
}

// One stored request plus its derived triage fields from the grouped query.
fn request_list_row(row: &sqlx::sqlite::SqliteRow, now: i64) -> Result<Value, String> {
    let field = |error: sqlx::Error| format!("A stored review request could not be read: {error}");
    let parse_list = |column: &str, raw: String| {
        serde_json::from_str::<Vec<String>>(&raw)
            .map_err(|error| format!("The stored {column} could not be read: {error}"))
    };
    let lenses: String = row.try_get("lenses").map_err(field)?;
    let reviewers: String = row.try_get("reviewers").map_err(field)?;
    let created_at: i64 = row.try_get("created_at").map_err(field)?;
    let unresolved = |name: &str| -> Result<i64, String> {
        row.try_get(name).map_err(field)
    };
    let request = ReviewRequest {
        id: row.try_get("id").map_err(field)?,
        repo_path: row.try_get("repo_path").map_err(field)?,
        base_sha: row.try_get("base_sha").map_err(field)?,
        target_key: row.try_get("target_key").map_err(field)?,
        target_kind: row.try_get("target_kind").map_err(field)?,
        requester_token_id: row.try_get("requester_token_id").map_err(field)?,
        status: row.try_get("status").map_err(field)?,
        note: row.try_get("note").map_err(field)?,
        lenses: parse_list("lenses", lenses)?,
        reviewers: parse_list("reviewers", reviewers)?,
        max_rounds: row.try_get("max_rounds").map_err(field)?,
        round: row.try_get("round").map_err(field)?,
        head_sha: row.try_get("head_sha").map_err(field)?,
        created_at,
        updated_at: row.try_get("updated_at").map_err(field)?,
    };
    let requester_name: Option<String> = row.try_get("requester_name").map_err(field)?;
    let mut value = request_row_value(&request, &requester_name.unwrap_or_else(|| "human".into()));
    let fields = value.as_object_mut().expect("request rows are objects");
    fields.insert("age_ms".into(), json!((now - created_at).max(0)));
    fields.insert(
        "comment_count".into(),
        json!(row.try_get::<i64, _>("comment_count").map_err(field)?),
    );
    fields.insert(
        "unresolved_finding_counts".into(),
        json!({
            "P0": unresolved("unresolved_p0")?,
            "P1": unresolved("unresolved_p1")?,
            "P2": unresolved("unresolved_p2")?,
            "P3": unresolved("unresolved_p3")?,
        }),
    );
    Ok(value)
}

async fn update_review_request(
    state: &TransportState,
    args: UpdateReviewRequestArgs,
    identity: &AgentIdentity,
) -> ToolOutcome {
    let actor = RequestActor::Agent(identity.token_id);
    // The note refresh and the new head only mean something on a
    // re-request; refuse them elsewhere instead of silently ignoring
    // what the caller sent.
    if !matches!(args.action, UpdateRequestAction::ReRequest) {
        if args.note.is_some() {
            return Err("Only a re-request refreshes the note.".to_string());
        }
        if args.head_sha.is_some() {
            return Err("Only a re-request takes a new head.".to_string());
        }
    }
    if let Some(note) = &args.note {
        crate::requests::validate_request_note(note).map_err(|error| error.message)?;
    }
    let request = match args.action {
        UpdateRequestAction::Claim => {
            claim_request_in_pool(&state.pool, args.id, &actor, &state.request_changes)
                .await
                .map_err(|error| error.message)?
        }
        UpdateRequestAction::Approve => {
            set_request_verdict_in_pool(&state.pool, args.id, true, &actor, &state.request_changes)
                .await
                .map_err(|error| error.message)?
        }
        UpdateRequestAction::RequestChanges => {
            set_request_verdict_in_pool(&state.pool, args.id, false, &actor, &state.request_changes)
                .await
                .map_err(|error| error.message)?
        }
        UpdateRequestAction::Withdraw => {
            withdraw_request_in_pool(&state.pool, args.id, &actor, &state.request_changes)
                .await
                .map_err(|error| error.message)?
        }
        UpdateRequestAction::ReRequest => {
            // An absent head runs into the engine's non-empty check, so
            // the requirement's message lives in one place. The note rides
            // the same guarded UPDATE; an absent note keeps the stored one.
            let head = args.head_sha.as_deref().unwrap_or("");
            re_request_in_pool(
                &state.pool,
                args.id,
                head,
                args.note.as_deref(),
                &actor,
                &state.request_changes,
            )
            .await
            .map_err(|error| error.message)?
        }
    };
    let requester = requester_display(&state.pool, &request).await?;
    payload(request_row_value(&request, &requester))
}

// One announcement per successful agent comment mutation: the review
// identity joins from the store, so the reply/resolve/edit handlers need
// only the mutated comment's id. A failed read only skips the event; the
// mutation itself already succeeded.
async fn announce_comment_change(
    state: &TransportState,
    comment_id: i64,
    action: &'static str,
    agent_name: &str,
) {
    if let Ok(Some(review)) = review_identity_of_comment(&state.pool, comment_id).await {
        (state.comment_changes)(CommentChange {
            repo_path: review.repo_path,
            base_sha: review.base_sha,
            target_key: review.target_key,
            target_kind: review.target_kind,
            comment_id,
            action,
            agent_name: agent_name.to_string(),
        });
    }
}

async fn create_comment(
    state: &TransportState,
    args: CreateCommentArgs,
    identity: &AgentIdentity,
) -> ToolOutcome {
    ensure_repo_open(state, &args.repo_path)
        .await
        .map_err(|error| error.message)?;
    let author_model = normalize_model_label(args.author_model.as_deref())?;
    let comment = create_unbound_comment_in_pool(
        &state.pool,
        &args.repo_path,
        &args.base_sha,
        &args.target_key,
        &args.target_kind,
        &args.body,
        args.severity,
        args.file_path,
        args.side,
        args.start_line,
        args.end_line,
        &Actor::Agent(identity.clone()),
        author_model.as_deref(),
    )
    .await
    .map_err(|error| error.message)?;
    (state.comment_changes)(CommentChange {
        repo_path: args.repo_path,
        base_sha: args.base_sha,
        target_key: args.target_key,
        target_kind: args.target_kind,
        comment_id: comment.id(),
        action: "created",
        agent_name: identity.name.clone(),
    });
    payload(comment)
}

async fn reply_comment(
    state: &TransportState,
    args: ReplyCommentArgs,
    identity: &AgentIdentity,
) -> ToolOutcome {
    let author_model = normalize_model_label(args.author_model.as_deref())?;
    let comment = reply_comment_in_pool(
        &state.pool,
        args.parent_comment_id,
        &args.body,
        None,
        &Actor::Agent(identity.clone()),
        author_model.as_deref(),
    )
    .await
    .map_err(|error| error.message)?;
    announce_comment_change(state, comment.id(), "replied", &identity.name).await;
    payload(comment)
}

// The model label is self-reported and display-only: it renders next to
// the token's name and never affects ownership or authentication.
fn normalize_model_label(raw: Option<&str>) -> Result<Option<String>, String> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(label) if label.chars().count() <= MAX_AGENT_MODEL_CHARS => {
            Ok(Some(label.to_string()))
        }
        Some(_) => Err(format!(
            "The author_model label exceeds {} characters.",
            MAX_AGENT_MODEL_CHARS
        )),
    }
}

async fn resolve_thread(
    state: &TransportState,
    args: ResolveThreadArgs,
    identity: &AgentIdentity,
) -> ToolOutcome {
    let comment = set_comment_resolved_in_pool(
        &state.pool,
        args.root_comment_id,
        args.resolved,
        &Actor::Agent(identity.clone()),
    )
    .await
    .map_err(|error| error.message)?;
    announce_comment_change(
        state,
        comment.id(),
        if args.resolved { "resolved" } else { "unresolved" },
        &identity.name,
    )
    .await;
    payload(comment)
}

async fn edit_own_comment(
    state: &TransportState,
    args: EditOwnCommentArgs,
    identity: &AgentIdentity,
) -> ToolOutcome {
    let comment = edit_comment_in_pool(
        &state.pool,
        args.comment_id,
        &args.body,
        &Actor::Agent(identity.clone()),
    )
    .await
    .map_err(|error| error.message)?;
    announce_comment_change(state, comment.id(), "edited", &identity.name).await;
    payload(comment)
}

async fn delete_own_comment(
    state: &TransportState,
    args: DeleteOwnCommentArgs,
    identity: &AgentIdentity,
) -> ToolOutcome {
    // The row is gone after the delete, so the announcement's review
    // identity is read first and the event fires only on success.
    let review = review_identity_of_comment(&state.pool, args.comment_id)
        .await
        .ok()
        .flatten();
    delete_comment_in_pool(&state.pool, args.comment_id, &Actor::Agent(identity.clone()))
        .await
        .map_err(|error| error.message)?;
    if let Some(review) = review {
        (state.comment_changes)(CommentChange {
            repo_path: review.repo_path,
            base_sha: review.base_sha,
            target_key: review.target_key,
            target_kind: review.target_kind,
            comment_id: args.comment_id,
            action: "deleted",
            agent_name: identity.name.clone(),
        });
    }
    payload(json!({ "deleted": true }))
}

async fn refresh_repo_tool(state: &TransportState, args: RefreshRepoArgs) -> ToolOutcome {
    ensure_repo_open(state, &args.repo_path)
        .await
        .map_err(|error| error.message)?;
    refresh_repo(&state.pool, &args.repo_path, &state.refreshes)
        .await
        .map_err(|error| error.message)?;
    payload(json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{create_agent_token_in_pool, delete_agent_token_in_pool};
    use crate::testutil::{seed_repo, test_pool, test_path, test_repo};
    use crate::transport::{
        endpoint_config_path, handle, start, CommentChange, CommentSink, ListenerConfig,
        ListenerStatus, McpStatusHandle, RefreshSink, RequestChange, RequestChangeSink,
        TransportDeps, UNAUTHORIZED,
    };
    use axum::body::{to_bytes, Body};
    use axum::extract::State;
    use axum::http::{header, Request};
    use std::sync::{Arc, Mutex};

    const OWNERSHIP_MESSAGE: &str =
        "Only the agent token that authored a comment can edit or delete it.";
    const UNKNOWN_REPO_MESSAGE: &str = "No repository with that path is open in WorktreeView.";
    const TOOL_NAMES: [&str; 17] = [
        "list_repos",
        "list_review_targets",
        "list_comments",
        "list_submissions",
        "list_review_requests",
        "list_threads",
        "list_activity",
        "create_comment",
        "reply_comment",
        "resolve_thread",
        "edit_own_comment",
        "delete_own_comment",
        "add_repo",
        "request_review",
        "announce_review",
        "update_review_request",
        "refresh_repo",
    ];

    fn noop_refreshes() -> RefreshSink {
        Arc::new(|_| {})
    }

    fn noop_comment_changes() -> CommentSink {
        Arc::new(|_| {})
    }

    fn noop_request_changes() -> RequestChangeSink {
        Arc::new(|_| {})
    }

    fn recording_comment_changes() -> (CommentSink, Arc<Mutex<Vec<CommentChange>>>) {
        let received: Arc<Mutex<Vec<CommentChange>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        (
            Arc::new(move |change| sink.lock().unwrap().push(change)),
            received,
        )
    }

    fn recording_request_changes() -> (RequestChangeSink, Arc<Mutex<Vec<RequestChange>>>) {
        let received: Arc<Mutex<Vec<RequestChange>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        (
            Arc::new(move |change| sink.lock().unwrap().push(change)),
            received,
        )
    }

    fn recording_refreshes() -> (RefreshSink, Arc<Mutex<Vec<String>>>) {
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        (
            Arc::new(move |repo_path: &str| sink.lock().unwrap().push(repo_path.to_string())),
            received,
        )
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

    // Handler state authenticated by a real token row, mirroring the raw
    // face's handler tests; the token's secret comes back for the calls.
    async fn test_state(pool: sqlx::SqlitePool) -> (TransportState, String) {
        let secret = create_agent_token_in_pool(&pool, "mcp-agent")
            .await
            .unwrap()
            .secret;
        let state = TransportState {
            pool,
            arrivals: Arc::new(|_| {}),
            refreshes: noop_refreshes(),
            comment_changes: noop_comment_changes(),
            request_changes: noop_request_changes(),
            status: dummy_status(),
        };
        (state, secret)
    }

    fn mcp_body(id: Value, method: &str, params: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
    }

    fn meta(version: &str) -> Value {
        let mut meta_map = serde_json::Map::new();
        meta_map.insert(PROTOCOL_VERSION_KEY.to_string(), json!(version));
        json!({ "_meta": Value::Object(meta_map) })
    }

    fn call_params(name: &str, arguments: Value) -> Value {
        let mut params = serde_json::Map::new();
        params.insert("name".to_string(), json!(name));
        params.insert("arguments".to_string(), arguments);
        params.insert("_meta".to_string(), meta(PROTOCOL_VERSION)["_meta"].clone());
        Value::Object(params)
    }

    fn call_tool(name: &str, arguments: Value) -> String {
        mcp_body(json!(1), "tools/call", call_params(name, arguments))
    }

    // A raw variant for responses that are not JSON (the 202 notification
    // acknowledgment answers an empty body).
    async fn post_mcp_bytes(
        state: &TransportState,
        token: Option<&str>,
        body: &str,
    ) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder().method("POST").uri("/mcp");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        let request = builder.body(Body::from(body.to_string())).unwrap();
        let response = handle(State(state.clone()), request).await;
        let status = response.status();
        let payload = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, payload.to_vec())
    }

    async fn post_mcp(
        state: &TransportState,
        token: Option<&str>,
        body: &str,
    ) -> (StatusCode, Value) {
        let (status, bytes) = post_mcp_bytes(state, token, body).await;
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn call_tool_raw(
        state: &TransportState,
        token: &str,
        name: &str,
        arguments: Value,
    ) -> Value {
        post_mcp(state, Some(token), &call_tool(name, arguments))
            .await
            .1
    }

    fn result_text(payload: &Value) -> Value {
        let text = payload["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    // One review with one agent-owned root comment, created through the tool
    // itself so ownership rows are exactly what production writes.
    async fn seed_review_with_comment(
        pool: &sqlx::SqlitePool,
        state: &TransportState,
        token: &str,
    ) -> i64 {
        call_tool_raw(
            state,
            token,
            "create_comment",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "body": "root comment",
            }),
        )
        .await;
        sqlx::query_scalar("SELECT id FROM comments ORDER BY id DESC LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn discover_reports_version_identity_and_the_full_cacheable_tool_set() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool).await;
        let (status, payload) = post_mcp(
            &state,
            Some(&secret),
            &mcp_body(json!(1), "server/discover", json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["result"]["protocolVersions"], json!(["2026-07-28"]));
        assert_eq!(payload["result"]["serverInfo"]["name"], "worktreeview");
        assert_eq!(
            payload["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(payload["result"]["capabilities"]["tools"].is_object());

        let (_, payload) = post_mcp(
            &state,
            Some(&secret),
            &mcp_body(json!(2), "tools/list", meta(PROTOCOL_VERSION)),
        )
        .await;
        let tools = payload["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, TOOL_NAMES, "the tool catalog is deterministic");
        for tool in tools {
            assert!(tool["ttlMs"].is_u64(), "every tool carries a cache ttl");
            assert_eq!(tool["cacheScope"], "private");
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(tool["description"].is_string());
        }
    }

    #[tokio::test]
    async fn missing_meta_version_defaults_and_explicit_versions_are_enforced() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool).await;
        // A missing _meta version counts as the supported revision.
        let (_, payload) = post_mcp(
            &state,
            Some(&secret),
            &mcp_body(json!(1), "tools/list", json!({})),
        )
        .await;
        assert!(payload["result"]["tools"].is_array());
        let (_, payload) = post_mcp(
            &state,
            Some(&secret),
            &mcp_body(json!(2), "tools/call", json!({ "name": "list_repos" })),
        )
        .await;
        assert_eq!(payload["result"]["isError"], false);
        // An explicit version must name the supported revision: even a known
        // legacy revision is refused on tools requests.
        let (_, payload) = post_mcp(
            &state,
            Some(&secret),
            &mcp_body(json!(3), "tools/list", meta("2025-06-18")),
        )
        .await;
        assert_eq!(payload["error"]["code"], UNSUPPORTED_PROTOCOL_VERSION);
        // The supported version passes the gate (server/discover needs none,
        // as covered by the discovery test).
        let (_, payload) = post_mcp(
            &state,
            Some(&secret),
            &mcp_body(json!(4), "tools/list", meta(PROTOCOL_VERSION)),
        )
        .await;
        assert!(payload["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn auth_covers_the_mcp_face() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool.clone()).await;
        let body = mcp_body(json!(1), "tools/list", meta(PROTOCOL_VERSION));
        let (status, payload) = post_mcp(&state, None, &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["error"]["code"], UNAUTHORIZED);
        let (status, payload) = post_mcp(&state, Some("wrong-secret"), &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["error"]["code"], UNAUTHORIZED);

        let token_id: i64 =
            sqlx::query_scalar("SELECT id FROM agent_tokens WHERE name = 'mcp-agent'")
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query("DELETE FROM agent_tokens WHERE id = ?")
            .bind(token_id)
            .execute(&pool)
            .await
            .unwrap();
        let (status, payload) = post_mcp(&state, Some(&secret), &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(payload["error"]["code"], UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_methods_and_batches_are_still_refused() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool).await;
        for method in ["resources/list", "prompts/list", "completion/complete"] {
            let (_, payload) = post_mcp(
                &state,
                Some(&secret),
                &mcp_body(json!(1), method, meta(PROTOCOL_VERSION)),
            )
            .await;
            assert_eq!(payload["error"]["code"], METHOD_NOT_FOUND, "{method}");
        }
        // Batching is out of scope: one request, one error.
        let (_, payload) = post_mcp(
            &state,
            Some(&secret),
            r#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"}]"#,
        )
        .await;
        assert_eq!(payload["error"]["code"], INVALID_REQUEST);
        assert_eq!(payload["id"], Value::Null);
    }

    // Clients on pre-2026-07-28 revisions cannot connect without a
    // successful initialize: the face answers one, echoing any well-formed
    // revision the client names, still without storing any session state.
    #[tokio::test]
    async fn initialize_echoes_well_formed_revisions_and_defaults_the_rest() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool).await;
        for version in [
            "2024-11-05",
            "2025-03-26",
            "2025-06-18",
            "2025-11-25",
            PROTOCOL_VERSION,
            "1999-01-01",
        ] {
            let (_, payload) = post_mcp(
                &state,
                Some(&secret),
                &mcp_body(json!(1), "initialize", json!({ "protocolVersion": version })),
            )
            .await;
            assert_eq!(payload["result"]["protocolVersion"], json!(version), "{version}");
            assert_eq!(payload["result"]["serverInfo"]["name"], "worktreeview");
            assert_eq!(
                payload["result"]["serverInfo"]["version"],
                env!("CARGO_PKG_VERSION")
            );
            assert!(payload["result"]["capabilities"]["tools"].is_object());
        }
        for version in ["not-a-revision", "2025-1-1"] {
            let (_, payload) = post_mcp(
                &state,
                Some(&secret),
                &mcp_body(json!(2), "initialize", json!({ "protocolVersion": version })),
            )
            .await;
            assert_eq!(
                payload["result"]["protocolVersion"],
                json!(PROTOCOL_VERSION),
                "{version}"
            );
        }
        // An absent version gets the supported revision too.
        let (_, payload) = post_mcp(
            &state,
            Some(&secret),
            &mcp_body(json!(3), "initialize", json!({})),
        )
        .await;
        assert_eq!(payload["result"]["protocolVersion"], json!(PROTOCOL_VERSION));
    }

    #[tokio::test]
    async fn ping_answers_the_standard_empty_result() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool).await;
        let (status, payload) =
            post_mcp(&state, Some(&secret), &mcp_body(json!(7), "ping", json!({}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["id"], 7);
        assert_eq!(payload["result"], json!({}));
    }

    // Any request without an id is a notification: 202 with an empty body,
    // never a JSON-RPC error, whatever the method.
    #[tokio::test]
    async fn notifications_answer_202_with_an_empty_body() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool).await;
        for method in ["notifications/initialized", "notifications/cancelled"] {
            let body = format!(r#"{{"jsonrpc":"2.0","method":"{method}"}}"#);
            let (status, bytes) = post_mcp_bytes(&state, Some(&secret), &body).await;
            assert_eq!(status, StatusCode::ACCEPTED, "{method}");
            assert!(bytes.is_empty(), "{method}");
        }
    }

    #[tokio::test]
    async fn unknown_tool_and_malformed_arguments_are_invalid_params() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool).await;
        let payload = call_tool_raw(&state, &secret, "nope", json!({})).await;
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);
        // A missing required field is a shape error, not an execution one.
        let payload = call_tool_raw(
            &state,
            &secret,
            "create_comment",
            json!({ "repo_path": "/demo", "base_sha": "base", "target_key": "/demo", "target_kind": "worktree" }),
        )
        .await;
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);
        // Unknown fields are rejected, as everywhere else in the app.
        let payload = call_tool_raw(
            &state,
            &secret,
            "delete_own_comment",
            json!({ "comment_id": 1, "surprise": true }),
        )
        .await;
        assert_eq!(payload["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn create_comment_attributes_to_the_calling_token() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool.clone()).await;
        let payload = call_tool_raw(
            &state,
            &secret,
            "create_comment",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "body": "found an issue",
                "severity": "P1",
                "file_path": "src/main.rs",
                "side": "RIGHT",
                "start_line": 3,
            }),
        )
        .await;
        assert_eq!(payload["result"]["resultType"], "complete");
        assert_eq!(payload["result"]["isError"], false);
        assert_eq!(payload["result"]["content"][0]["type"], "text");
        let comment = result_text(&payload);
        assert_eq!(comment["body"], "found an issue");
        assert_eq!(comment["severity"], "P1");
        let token_id: i64 =
            sqlx::query_scalar("SELECT id FROM agent_tokens WHERE name = 'mcp-agent'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let owner: Option<i64> =
            sqlx::query_scalar("SELECT author_token_id FROM comments WHERE id = ?")
                .bind(comment["id"].as_i64().unwrap())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(owner, Some(token_id));
        // Tool line anchors bind no content, like ingested findings.
        let hash: Option<String> =
            sqlx::query_scalar("SELECT anchor_hash FROM comments WHERE id = ?")
                .bind(comment["id"].as_i64().unwrap())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(hash, None);
    }

    #[tokio::test]
    async fn reads_are_find_only_and_never_create_rows() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool.clone()).await;
        let identity = json!({
            "repo_path": "/demo",
            "base_sha": "base",
            "target_key": "/demo",
            "target_kind": "worktree",
        });
        let payload = call_tool_raw(&state, &secret, "list_comments", identity.clone()).await;
        assert_eq!(payload["result"]["isError"], false);
        assert_eq!(result_text(&payload), json!([]));
        let payload = call_tool_raw(&state, &secret, "list_submissions", identity).await;
        assert_eq!(result_text(&payload), json!([]));
        let reviews: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reviews")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(reviews, 0, "a read must not create a review identity");
    }

    #[tokio::test]
    async fn list_threads_answers_through_the_endpoint_with_each_filter() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool.clone()).await;
        // One review identity with a human root and an agent reply, stored
        // like the ingest path stores them.
        sqlx::query(
            "INSERT INTO reviews (repo_path, base_sha, target_key, target_kind, created_at) \
             VALUES ('/demo', 'base', '/demo', 'worktree', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO comments (review_id, parent_id, author_kind, author_name, body, created_at) \
             VALUES ((SELECT id FROM reviews WHERE repo_path = '/demo'), NULL, 'human', 'dana', 'thread root', 1000)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let root: i64 = sqlx::query_scalar("SELECT MAX(id) FROM comments")
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO comments (review_id, parent_id, author_kind, author_name, body, created_at) \
             VALUES ((SELECT id FROM reviews WHERE repo_path = '/demo'), ?, 'agent', 'reviewer-bot', 'a reply', 1100)",
        )
        .bind(root)
        .execute(&pool)
        .await
        .unwrap();

        // No filters: open threads grouped by change, participants first.
        let payload = call_tool_raw(&state, &secret, "list_threads", json!({})).await;
        assert_eq!(payload["result"]["isError"], false);
        let groups = result_text(&payload);
        assert_eq!(groups[0]["change_label"], "demo");
        assert_eq!(groups[0]["open_count"], 1);
        assert_eq!(groups[0]["threads"][0]["reply_count"], 1);
        assert_eq!(groups[0]["threads"][0]["participants"][0]["author_name"], "dana");
        assert_eq!(groups[0]["threads"][0]["participants"][1]["author_name"], "reviewer-bot");

        // Participant filter: exact name over root and reply authors.
        let by_participant = call_tool_raw(
            &state,
            &secret,
            "list_threads",
            json!({ "participant": "reviewer-bot" }),
        )
        .await;
        assert_eq!(result_text(&by_participant)[0]["threads"].as_array().unwrap().len(), 1);
        let nobody = call_tool_raw(&state, &secret, "list_threads", json!({ "participant": "nobody" })).await;
        assert_eq!(result_text(&nobody), json!([]));

        // Since: a future instant answers nothing; a past one keeps the thread.
        let future = call_tool_raw(&state, &secret, "list_threads", json!({ "since": 9_999_999_999_999i64 })).await;
        assert_eq!(result_text(&future), json!([]));
        let past = call_tool_raw(&state, &secret, "list_threads", json!({ "since": 1000 })).await;
        assert_eq!(result_text(&past)[0]["threads"].as_array().unwrap().len(), 1);

        // Resolved state: nothing is resolved yet.
        let resolved = call_tool_raw(&state, &secret, "list_threads", json!({ "state": "resolved" })).await;
        assert_eq!(result_text(&resolved), json!([]));

        // Unknown state and unknown repo are clean failures.
        let bad_state = call_tool_raw(&state, &secret, "list_threads", json!({ "state": "all" })).await;
        assert_eq!(bad_state["result"]["isError"], true);
        assert!(bad_state["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unknown state 'all'"));
        let bad_repo = call_tool_raw(&state, &secret, "list_threads", json!({ "repo_path": "/nowhere" })).await;
        assert_eq!(bad_repo["result"]["isError"], true);
        assert_eq!(bad_repo["result"]["content"][0]["text"], UNKNOWN_REPO_MESSAGE);
    }

    #[tokio::test]
    async fn list_activity_answers_the_cursor_bounded_and_per_repo() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        seed_repo(&pool, "/other").await;
        let (state, secret) = test_state(pool.clone()).await;

        // Two production mutations on one repo narrate their events; a
        // foreign repo's row exists only so the repo filter has scope.
        call_tool_raw(
            &state,
            &secret,
            "request_review",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "head_sha": "head-1",
            }),
        )
        .await;
        call_tool_raw(
            &state,
            &secret,
            "create_comment",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "body": "root comment",
            }),
        )
        .await;
        sqlx::query(
            "INSERT INTO events (repo_path, kind, actor_kind, actor_name, summary, created_at) \
             VALUES ('/other', 'repo_added', 'human', 'human', 'repo added', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // No filters: ascending by id, attributed to the calling token.
        let payload = call_tool_raw(&state, &secret, "list_activity", json!({})).await;
        assert_eq!(payload["result"]["isError"], false);
        let answer = result_text(&payload);
        let events = answer["events"].as_array().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["kind"], "request_created");
        assert_eq!(events[0]["actor_kind"], "agent");
        assert_eq!(events[0]["actor_name"], "mcp-agent");
        assert_eq!(events[1]["kind"], "comment_posted");
        assert_eq!(events[1]["actor_name"], "mcp-agent");
        assert_eq!(events[2]["repo_path"], "/other");
        let cursor = answer["next_cursor"].as_i64().unwrap();
        assert_eq!(cursor, events[2]["id"].as_i64().unwrap());

        // The cursor is strictly greater and never moves backward.
        let replay = call_tool_raw(&state, &secret, "list_activity", json!({ "since_id": cursor })).await;
        let replayed = result_text(&replay);
        assert_eq!(replayed["events"].as_array().unwrap().len(), 0);
        assert_eq!(replayed["next_cursor"], json!(cursor));

        // The repo filter is exact.
        let other = call_tool_raw(&state, &secret, "list_activity", json!({ "repo_path": "/other" })).await;
        let other_answer = result_text(&other);
        let others = other_answer["events"].as_array().unwrap();
        assert_eq!(others.len(), 1);
        assert_eq!(others[0]["kind"], "repo_added");

        // The page size bounds the answer and the cursor bridges the pages.
        let paged = call_tool_raw(&state, &secret, "list_activity", json!({ "limit": 1 })).await;
        let page = result_text(&paged);
        assert_eq!(page["events"].as_array().unwrap().len(), 1);
        let first_cursor = page["next_cursor"].as_i64().unwrap();
        let rest = call_tool_raw(&state, &secret, "list_activity", json!({ "since_id": first_cursor })).await;
        assert_eq!(result_text(&rest)["events"].as_array().unwrap().len(), 2);

        // Unknown repo is a clean failure.
        let bad_repo = call_tool_raw(&state, &secret, "list_activity", json!({ "repo_path": "/nowhere" })).await;
        assert_eq!(bad_repo["result"]["isError"], true);
        assert_eq!(bad_repo["result"]["content"][0]["text"], UNKNOWN_REPO_MESSAGE);
    }

    #[tokio::test]
    async fn ownership_denials_are_error_results_and_own_comments_succeed() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool.clone()).await;
        let other = create_agent_token_in_pool(&pool, "other-agent").await.unwrap();
        let root_id = seed_review_with_comment(&pool, &state, &secret).await;

        // A foreign token's edit is an execution failure with the distinct
        // ownership message, not a -32602 shape error.
        let foreign = call_tool_raw(
            &state,
            &other.secret,
            "edit_own_comment",
            json!({ "comment_id": root_id, "body": "hijack" }),
        )
        .await;
        assert_eq!(foreign["result"]["resultType"], "complete");
        assert_eq!(foreign["result"]["isError"], true);
        assert_eq!(foreign["result"]["content"][0]["text"], OWNERSHIP_MESSAGE);

        // Legacy unowned comments are never agent-mutable.
        sqlx::query(
            "INSERT INTO comments (review_id, author_kind, author_name, body, created_at) \
             VALUES ((SELECT id FROM reviews LIMIT 1), 'agent', 'legacy', 'old', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let legacy_id: i64 =
            sqlx::query_scalar("SELECT id FROM comments WHERE author_name = 'legacy'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let legacy = call_tool_raw(
            &state,
            &secret,
            "edit_own_comment",
            json!({ "comment_id": legacy_id, "body": "edit" }),
        )
        .await;
        assert_eq!(legacy["result"]["isError"], true);
        assert_eq!(legacy["result"]["content"][0]["text"], OWNERSHIP_MESSAGE);

        // The authoring token edits and deletes its own comment.
        let own = call_tool_raw(
            &state,
            &secret,
            "edit_own_comment",
            json!({ "comment_id": root_id, "body": "edited" }),
        )
        .await;
        assert_eq!(own["result"]["isError"], false);
        assert_eq!(result_text(&own)["body"], "edited");
        let foreign_delete = call_tool_raw(
            &state,
            &other.secret,
            "delete_own_comment",
            json!({ "comment_id": root_id }),
        )
        .await;
        assert_eq!(foreign_delete["result"]["isError"], true);
        let own_delete = call_tool_raw(
            &state,
            &secret,
            "delete_own_comment",
            json!({ "comment_id": root_id }),
        )
        .await;
        assert_eq!(own_delete["result"]["isError"], false);
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM comments WHERE id = ?")
            .bind(root_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn refresh_repo_tool_noops_without_a_remote_and_maps_unknown_repo() {
        let pool = test_pool().await;
        let repo = test_repo("mcp-refresh-tool");
        seed_repo(&pool, repo.to_str().unwrap()).await;
        let (refreshes, announced) = recording_refreshes();
        let secret = create_agent_token_in_pool(&pool, "mcp-agent")
            .await
            .unwrap()
            .secret;
        let state = TransportState {
            pool,
            arrivals: Arc::new(|_| {}),
            refreshes,
            comment_changes: noop_comment_changes(),
            request_changes: noop_request_changes(),
            status: dummy_status(),
        };
        let payload = call_tool_raw(
            &state,
            &secret,
            "refresh_repo",
            json!({ "repo_path": repo.to_str().unwrap() }),
        )
        .await;
        assert_eq!(payload["result"]["isError"], false);
        assert_eq!(result_text(&payload), json!({ "ok": true }));
        assert_eq!(
            announced.lock().unwrap().as_slice(),
            [repo.to_string_lossy().as_ref()]
        );

        let unknown = call_tool_raw(
            &state,
            &secret,
            "refresh_repo",
            json!({ "repo_path": "/missing" }),
        )
        .await;
        assert_eq!(unknown["result"]["isError"], true);
        assert_eq!(
            unknown["result"]["content"][0]["text"],
            UNKNOWN_REPO_MESSAGE
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    // Every successful agent comment mutation announces exactly one
    // comment-change event carrying the review identity, the mutated
    // comment, the action, and the token's agent name.
    #[tokio::test]
    async fn agent_comment_mutations_announce_once_with_the_right_payload() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (changes, announced) = recording_comment_changes();
        let secret = create_agent_token_in_pool(&pool, "mcp-agent")
            .await
            .unwrap()
            .secret;
        let state = TransportState {
            pool: pool.clone(),
            arrivals: Arc::new(|_| {}),
            refreshes: noop_refreshes(),
            comment_changes: changes,
            request_changes: noop_request_changes(),
            status: dummy_status(),
        };
        let created = call_tool_raw(
            &state,
            &secret,
            "create_comment",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "body": "root comment",
            }),
        )
        .await;
        assert_eq!(created["result"]["isError"], false);
        let root_id = result_text(&created)["id"].as_i64().unwrap();

        let reply =
            call_tool_raw(&state, &secret, "reply_comment", json!({ "parent_comment_id": root_id, "body": "a reply" }))
                .await;
        assert_eq!(reply["result"]["isError"], false);
        let reply_id = result_text(&reply)["id"].as_i64().unwrap();
        let resolved = call_tool_raw(
            &state,
            &secret,
            "resolve_thread",
            json!({ "root_comment_id": root_id, "resolved": true }),
        )
        .await;
        assert_eq!(resolved["result"]["isError"], false);
        let unresolved = call_tool_raw(
            &state,
            &secret,
            "resolve_thread",
            json!({ "root_comment_id": root_id, "resolved": false }),
        )
        .await;
        assert_eq!(unresolved["result"]["isError"], false);
        let edited = call_tool_raw(
            &state,
            &secret,
            "edit_own_comment",
            json!({ "comment_id": root_id, "body": "edited body" }),
        )
        .await;
        assert_eq!(edited["result"]["isError"], false);
        let deleted = call_tool_raw(
            &state,
            &secret,
            "delete_own_comment",
            json!({ "comment_id": reply_id }),
        )
        .await;
        assert_eq!(deleted["result"]["isError"], false);

        let events = announced.lock().unwrap();
        let actions: Vec<&str> = events.iter().map(|change| change.action).collect();
        assert_eq!(
            actions,
            ["created", "replied", "resolved", "unresolved", "edited", "deleted"]
        );
        for change in events.iter() {
            assert_eq!(change.repo_path, "/demo");
            assert_eq!(change.base_sha, "base");
            assert_eq!(change.target_key, "/demo");
            assert_eq!(change.target_kind, "worktree");
            assert_eq!(change.agent_name, "mcp-agent");
        }
        assert_eq!(events[0].comment_id, root_id);
        assert_eq!(events[1].comment_id, reply_id);
        assert_eq!(events[2].comment_id, root_id);
        assert_eq!(events[3].comment_id, root_id);
        assert_eq!(events[4].comment_id, root_id);
        assert_eq!(events[5].comment_id, reply_id);
    }

    // Failed mutations announce nothing, and the human IPC path (the shared
    // implementations the commands call with Actor::Human) has no sink to
    // announce through: the invoking renderer owns its refetch.
    // Deleting the authoring token detaches its comments (author_token_id
    // drops to null), so they stay visible but no agent can mutate them.
    #[tokio::test]
    async fn add_repo_opens_a_real_repository_and_lists_it_once() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool).await;
        let repo_dir = test_repo("add-repo");

        let created = call_tool_raw(
            &state,
            &secret,
            "add_repo",
            json!({ "path": repo_dir.to_string_lossy() }),
        )
        .await;
        assert_eq!(created["result"]["isError"], false);

        // Re-opening the same repository is idempotent: one row, no error.
        let again = call_tool_raw(
            &state,
            &secret,
            "add_repo",
            json!({ "path": repo_dir.to_string_lossy() }),
        )
        .await;
        assert_eq!(again["result"]["isError"], false);
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT path FROM repos ORDER BY path").fetch_all(&state.pool).await.unwrap();
        assert_eq!(rows.len(), 1);

        let listed = call_tool_raw(&state, &secret, "list_repos", json!({})).await;
        assert_eq!(listed["result"]["isError"], false);
        assert!(result_text(&listed).to_string().contains("add-repo"));
        std::fs::remove_dir_all(repo_dir).unwrap();
    }

    #[tokio::test]
    async fn add_repo_refuses_folders_that_are_not_repositories() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool).await;
        let dir = test_path("add-repo-not-git");
        std::fs::create_dir(&dir).unwrap();

        let refused = call_tool_raw(
            &state,
            &secret,
            "add_repo",
            json!({ "path": dir.to_string_lossy() }),
        )
        .await;
        assert_eq!(refused["result"]["isError"], true);
        assert!(refused["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not a Git repository"));
        let rows: Vec<(String,)> = sqlx::query_as("SELECT path FROM repos").fetch_all(&state.pool).await.unwrap();
        assert!(rows.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn comment_writes_accept_a_self_reported_model_label() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool.clone()).await;
        let root_id = seed_review_with_comment(&pool, &state, &secret).await;

        let created = call_tool_raw(
            &state,
            &secret,
            "reply_comment",
            json!({ "parent_comment_id": root_id, "body": "from the frontier model", "author_model": "  GPT Luna medium  " }),
        )
        .await;
        assert_eq!(created["result"]["isError"], false);
        let reply: serde_json::Value = result_text(&created);
        assert_eq!(reply["author_model"], json!("GPT Luna medium"));

        let listed = call_tool_raw(
            &state,
            &secret,
            "list_comments",
            json!({ "repo_path": "/demo", "base_sha": "base", "target_key": "/demo", "target_kind": "worktree" }),
        )
        .await;
        let comments: serde_json::Value = result_text(&listed);
        let labeled = comments
            .as_array()
            .unwrap()
            .iter()
            .find(|comment| comment["id"] == reply["id"])
            .unwrap();
        assert_eq!(labeled["author_name"], json!("mcp-agent"));
        assert_eq!(labeled["author_model"], json!("GPT Luna medium"));

        // A blank label is stored as absent; over-long labels are shape errors.
        let blank = call_tool_raw(
            &state,
            &secret,
            "reply_comment",
            json!({ "parent_comment_id": root_id, "body": "no label", "author_model": "   " }),
        )
        .await;
        assert_eq!(blank["result"]["isError"], false);
        let no_label: serde_json::Value = result_text(&blank);
        assert_eq!(no_label["author_model"], serde_json::Value::Null);
        let long = call_tool_raw(
            &state,
            &secret,
            "reply_comment",
            json!({ "parent_comment_id": root_id, "body": "too long", "author_model": "x".repeat(201) }),
        )
        .await;
        assert_eq!(long["result"]["isError"], true);
        assert!(long["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("exceeds 200 characters"));
    }

    #[tokio::test]
    async fn deleting_the_authoring_token_leaves_its_comments_unowned() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool.clone()).await;
        let root_id = seed_review_with_comment(&pool, &state, &secret).await;
        let token_id: i64 = sqlx::query_scalar("SELECT id FROM agent_tokens WHERE name = 'mcp-agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
        delete_agent_token_in_pool(&pool, token_id).await.unwrap();

        let attached: Option<i64> = sqlx::query_scalar(
            "SELECT author_token_id FROM comments WHERE id = ?",
        )
        .bind(root_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(attached, None);

        let other = create_agent_token_in_pool(&pool, "other-agent").await.unwrap();
        let attempt = call_tool_raw(
            &state,
            &other.secret,
            "edit_own_comment",
            json!({ "comment_id": root_id, "body": "hijack" }),
        )
        .await;
        assert_eq!(attempt["result"]["isError"], true);
        assert_eq!(attempt["result"]["content"][0]["text"], OWNERSHIP_MESSAGE);

        // The deleted secret no longer authenticates at all.
        let (status, _) = post_mcp(&state, Some(&secret), &mcp_body(
            json!(9),
            "tools/list",
            json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" } }),
        ))
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn failed_tool_mutations_and_human_mutations_stay_silent() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (changes, announced) = recording_comment_changes();
        let secret = create_agent_token_in_pool(&pool, "mcp-agent")
            .await
            .unwrap()
            .secret;
        let state = TransportState {
            pool: pool.clone(),
            arrivals: Arc::new(|_| {}),
            refreshes: noop_refreshes(),
            comment_changes: changes,
            request_changes: noop_request_changes(),
            status: dummy_status(),
        };
        let root_id = seed_review_with_comment(&pool, &state, &secret).await;
        announced.lock().unwrap().clear();

        let other = create_agent_token_in_pool(&pool, "other-agent").await.unwrap();
        let foreign = call_tool_raw(
            &state,
            &other.secret,
            "edit_own_comment",
            json!({ "comment_id": root_id, "body": "hijack" }),
        )
        .await;
        assert_eq!(foreign["result"]["isError"], true);

        // The human path: the shared implementations as the IPC commands
        // call them, with no event of any kind.
        crate::reviews::reply_comment_in_pool(&pool, root_id, "human reply", None, &Actor::Human, None)
            .await
            .unwrap();
        crate::reviews::set_comment_resolved_in_pool(&pool, root_id, true, &Actor::Human)
            .await
            .unwrap();

        assert_eq!(announced.lock().unwrap().len(), 0);
    }

    // One request created through the tool itself so requester rows are
    // exactly what production writes.
    async fn seed_request(
        state: &TransportState,
        token: &str,
        repo_path: &str,
        head: &str,
    ) -> Value {
        let payload = call_tool_raw(
            state,
            token,
            "request_review",
            json!({
                "repo_path": repo_path,
                "base_sha": "base",
                "target_key": repo_path,
                "target_kind": "worktree",
                "note": "Please review.",
                "head_sha": head,
            }),
        )
        .await;
        assert_eq!(payload["result"]["isError"], false);
        result_text(&payload)
    }

    #[tokio::test]
    async fn request_review_creates_dedups_and_reports_engine_refusals() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool.clone()).await;
        let reviewer = create_agent_token_in_pool(&pool, "reviewer-bot").await.unwrap();
        let args = json!({
            "repo_path": "/demo",
            "base_sha": "base",
            "target_key": "/demo",
            "target_kind": "worktree",
            "note": "Please review the loader change.",
            "head_sha": "head-1",
            "lenses": ["correctness", "tests"],
            "reviewers": ["reviewer-bot"],
            "max_rounds": 3,
        });
        let payload = call_tool_raw(&state, &secret, "request_review", args.clone()).await;
        assert_eq!(payload["result"]["isError"], false);
        let row = result_text(&payload);
        assert_eq!(row["status"], "requested");
        assert_eq!(row["round"], 0);
        assert_eq!(row["max_rounds"], 3);
        assert_eq!(row["lenses"], json!(["correctness", "tests"]));
        assert_eq!(row["reviewers"], json!(["reviewer-bot"]));
        assert_eq!(row["head_sha"], "head-1");
        assert_eq!(row["requester"], "mcp-agent");
        assert_eq!(row["note"], "Please review the loader change.");
        let id = row["id"].as_i64().unwrap();

        // The same identity, requester, and head updates the open request
        // in place instead of stacking a duplicate pickup.
        let mut updated = args.clone();
        updated["note"] = json!("Updated note.");
        let again = call_tool_raw(&state, &secret, "request_review", updated).await;
        assert_eq!(again["result"]["isError"], false);
        let deduped = result_text(&again);
        assert_eq!(deduped["id"], json!(id));
        assert_eq!(deduped["note"], "Updated note.");

        // An unknown repo is an isError result carrying the shared message.
        let unknown = call_tool_raw(
            &state,
            &secret,
            "request_review",
            json!({
                "repo_path": "/missing",
                "base_sha": "base",
                "target_key": "/missing",
                "target_kind": "worktree",
                "note": "n",
                "head_sha": "head-9",
            }),
        )
        .await;
        assert_eq!(unknown["result"]["isError"], true);
        assert_eq!(unknown["result"]["content"][0]["text"], UNKNOWN_REPO_MESSAGE);

        // The engine's create-time validation renders as isError results,
        // never as JSON-RPC errors.
        let base = args.clone();
        let mut variant = |field: &str, value: Value| {
            let mut args = base.clone();
            args[field] = value;
            args
        };
        for (args, message) in [
            (
                variant("note", json!("x".repeat(2001))),
                "exceeds 2000 characters",
            ),
            (variant("lenses", json!(["style"])), "Unknown lens"),
            (
                variant("lenses", json!(["security", "security"])),
                "duplicated",
            ),
            (
                variant("reviewers", json!(["ghost-bot"])),
                "does not name an agent token",
            ),
            (
                variant("reviewers", json!(["reviewer-bot", "reviewer-bot"])),
                "duplicated",
            ),
            (variant("max_rounds", json!(4)), "round budget"),
            (variant("head_sha", json!("  ")), "non-empty head"),
        ] {
            let payload = call_tool_raw(&state, &secret, "request_review", args).await;
            assert_eq!(payload["result"]["isError"], true, "{message}");
            assert!(payload["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(message));
            assert_eq!(
                payload["error"],
                Value::Null,
                "execution failures are never JSON-RPC errors"
            );
        }

        // The same head against a changes_requested request is refused: a
        // new head and re_request is the way forward. The new head lands
        // on the never-claimed row (refreshed in place, named list
        // intact), so reviewer-bot is the one who can claim it.
        let second = seed_request(&state, &secret, "/demo", "head-2").await;
        let second_id = second["id"].as_i64().unwrap();
        assert_eq!(second_id, id, "a new head refreshes the open pickup");
        let claimed = call_tool_raw(
            &state,
            &reviewer.secret,
            "update_review_request",
            json!({ "id": second_id, "action": "claim" }),
        )
        .await;
        assert_eq!(claimed["result"]["isError"], false);
        let changed = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": second_id, "action": "request_changes" }),
        )
        .await;
        assert_eq!(changed["result"]["isError"], false);
        let refused = call_tool_raw(
            &state,
            &secret,
            "request_review",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "note": "Please review.",
                "head_sha": "head-2",
            }),
        )
        .await;
        assert_eq!(refused["result"]["isError"], true);
        assert!(refused["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("re-request the review with a new head"));

        // A note is optional now: the missing-note create lands on a fresh
        // identity with an empty note. Other missing required fields and
        // unknown fields stay -32602 shape errors and never execute.
        let mut missing_note = args.clone();
        missing_note.as_object_mut().unwrap().remove("note");
        missing_note["base_sha"] = json!("base-optional");
        let missing = call_tool_raw(&state, &secret, "request_review", missing_note).await;
        assert_eq!(missing["result"]["isError"], false);
        assert_eq!(missing["error"], Value::Null);
        let row: Value =
            serde_json::from_str(missing["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(row["note"], json!(""));
        let mut missing_head = args;
        missing_head.as_object_mut().unwrap().remove("head_sha");
        let missing_required = call_tool_raw(&state, &secret, "request_review", missing_head).await;
        assert_eq!(missing_required["error"]["code"], INVALID_PARAMS);
        let unknown_field = call_tool_raw(
            &state,
            &secret,
            "request_review",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "note": "n",
                "head_sha": "head-1",
                "surprise": true,
            }),
        )
        .await;
        assert_eq!(unknown_field["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn announce_review_enters_dedups_and_reports_engine_refusals() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool.clone()).await;
        let (changes, pushes) = recording_request_changes();
        let state = TransportState { request_changes: changes, ..state };
        let args = json!({
            "repo_path": "/demo",
            "base_sha": "base",
            "target_key": "/demo",
            "target_kind": "worktree",
            "head_sha": "head-1",
            "note": "Starting my review.",
        });
        let payload = call_tool_raw(&state, &secret, "announce_review", args.clone()).await;
        assert_eq!(payload["result"]["isError"], false);
        let row = result_text(&payload);
        assert_eq!(row["status"], "in_review");
        assert_eq!(row["round"], 0);
        assert_eq!(row["lenses"], json!([]));
        assert_eq!(row["reviewers"], json!([]));
        assert_eq!(row["head_sha"], "head-1");
        assert_eq!(row["requester"], "mcp-agent");
        assert_eq!(row["note"], "Starting my review.");
        let id = row["id"].as_i64().unwrap();
        // The narrating push kind rides the announce.
        assert_eq!(pushes.lock().unwrap().len(), 1);
        assert_eq!(pushes.lock().unwrap()[0].id, id);
        assert_eq!(
            pushes.lock().unwrap()[0].event.as_deref(),
            Some("review_announced")
        );

        // Create-then-announce dedup: the token's own requested row
        // updates in place and moves to in review; the absent note keeps
        // the stored one.
        let created = seed_request(&state, &secret, "/demo", "head-2").await;
        assert_eq!(created["status"], "requested");
        let created_id = created["id"].as_i64().unwrap();
        let announced = call_tool_raw(
            &state,
            &secret,
            "announce_review",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "head_sha": "head-2",
            }),
        )
        .await;
        assert_eq!(announced["result"]["isError"], false);
        let deduped = result_text(&announced);
        assert_eq!(deduped["id"], json!(created_id));
        assert_eq!(deduped["status"], "in_review");
        assert_eq!(deduped["note"], "Please review.");

        // The same head against changes_requested is refused.
        let changed = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": created_id, "action": "request_changes" }),
        )
        .await;
        assert_eq!(changed["result"]["isError"], false);
        let refused = call_tool_raw(
            &state,
            &secret,
            "announce_review",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "head_sha": "head-2",
            }),
        )
        .await;
        assert_eq!(refused["result"]["isError"], true);
        assert!(refused["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("re-request the review with a new head"));

        // Re-request then approve; the same head on the approved row
        // returns satisfied, untouched.
        let re_requested = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": created_id, "action": "re_request", "head_sha": "head-3" }),
        )
        .await;
        assert_eq!(re_requested["result"]["isError"], false);
        let approved = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": created_id, "action": "approve" }),
        )
        .await;
        assert_eq!(approved["result"]["isError"], false);
        let satisfied = call_tool_raw(
            &state,
            &secret,
            "announce_review",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "head_sha": "head-3",
            }),
        )
        .await;
        assert_eq!(satisfied["result"]["isError"], false);
        let settled = result_text(&satisfied);
        assert_eq!(settled["id"], json!(created_id));
        assert_eq!(settled["status"], "approved");

        // The refreshable self-claim path: the token's never-claimed row
        // on a new head refreshes in place into in review.
        let created = seed_request(&state, &secret, "/demo", "head-4").await;
        let refresh_id = created["id"].as_i64().unwrap();
        assert_eq!(created["status"], "requested");
        let announced = call_tool_raw(
            &state,
            &secret,
            "announce_review",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "head_sha": "head-5",
                "note": "Rebased; starting now.",
            }),
        )
        .await;
        assert_eq!(announced["result"]["isError"], false);
        let refreshed = result_text(&announced);
        assert_eq!(refreshed["id"], json!(refresh_id));
        assert_eq!(refreshed["status"], "in_review");
        assert_eq!(refreshed["head_sha"], "head-5");
        assert_eq!(refreshed["note"], "Rebased; starting now.");

        // Note validation and the unknown-repo gate render as isError
        // results, never as JSON-RPC errors.
        let over = call_tool_raw(
            &state,
            &secret,
            "announce_review",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "head_sha": "head-9",
                "note": "x".repeat(2001),
            }),
        )
        .await;
        assert_eq!(over["result"]["isError"], true);
        assert!(over["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("exceeds 2000 characters"));
        let unknown = call_tool_raw(
            &state,
            &secret,
            "announce_review",
            json!({
                "repo_path": "/missing",
                "base_sha": "base",
                "target_key": "/missing",
                "target_kind": "worktree",
                "head_sha": "head-9",
            }),
        )
        .await;
        assert_eq!(unknown["result"]["isError"], true);
        assert_eq!(unknown["result"]["content"][0]["text"], UNKNOWN_REPO_MESSAGE);

        // Missing required fields and unknown fields stay -32602 shape
        // errors and never execute.
        let mut missing_head = args;
        missing_head.as_object_mut().unwrap().remove("head_sha");
        let missing_required = call_tool_raw(&state, &secret, "announce_review", missing_head).await;
        assert_eq!(missing_required["error"]["code"], INVALID_PARAMS);
        let unknown_field = call_tool_raw(
            &state,
            &secret,
            "announce_review",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "head_sha": "head-1",
                "lenses": ["security"],
            }),
        )
        .await;
        assert_eq!(unknown_field["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn request_tools_reject_unknown_fields_and_values_as_declared() {
        let pool = test_pool().await;
        let (state, secret) = test_state(pool).await;
        // Unknown fields are shape errors for every new tool.
        let unknown =
            call_tool_raw(&state, &secret, "list_review_requests", json!({ "surprise": true }))
                .await;
        assert_eq!(unknown["error"]["code"], INVALID_PARAMS);
        let unknown = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": 1, "action": "claim", "surprise": true }),
        )
        .await;
        assert_eq!(unknown["error"]["code"], INVALID_PARAMS);
        // The action vocabulary is part of the schema: an unknown action
        // never executes.
        let unknown = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": 1, "action": "close" }),
        )
        .await;
        assert_eq!(unknown["error"]["code"], INVALID_PARAMS);
        // The status filter is validated at execution time and reports as
        // an isError result, like the other enum-valued fields.
        let unknown = call_tool_raw(
            &state,
            &secret,
            "list_review_requests",
            json!({ "status": "closed" }),
        )
        .await;
        assert_eq!(unknown["result"]["isError"], true);
        assert!(unknown["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unknown status"));
    }

    #[tokio::test]
    async fn list_review_requests_filters_and_derives_the_open_queue() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        seed_repo(&pool, "/other").await;
        let (state, secret) = test_state(pool.clone()).await;

        // The demo row is claimed so the later head-2 create starts a new
        // row instead of refreshing a never-claimed one.
        let demo = seed_request(&state, &secret, "/demo", "head-1").await;
        let demo_id = demo["id"].as_i64().unwrap();
        let claimed = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": demo_id, "action": "claim" }),
        )
        .await;
        assert_eq!(claimed["result"]["isError"], false);

        // The identity carries three comments: unresolved P1 and P3
        // findings plus a resolved P2.
        for (body, severity) in [("p1 finding", "P1"), ("p3 finding", "P3"), ("p2 finding", "P2")] {
            let created = call_tool_raw(
                &state,
                &secret,
                "create_comment",
                json!({
                    "repo_path": "/demo",
                    "base_sha": "base",
                    "target_key": "/demo",
                    "target_kind": "worktree",
                    "body": body,
                    "severity": severity,
                }),
            )
            .await;
            assert_eq!(created["result"]["isError"], false, "{body}");
        }
        let p2_id: i64 = sqlx::query_scalar("SELECT id FROM comments WHERE body = 'p2 finding'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let resolved = call_tool_raw(
            &state,
            &secret,
            "resolve_thread",
            json!({ "root_comment_id": p2_id, "resolved": true }),
        )
        .await;
        assert_eq!(resolved["result"]["isError"], false);

        // A settled (approved) row on /other and a second open one.
        let settled = seed_request(&state, &secret, "/other", "head-a").await;
        let settled_id = settled["id"].as_i64().unwrap();
        for action in ["claim", "approve"] {
            let advanced = call_tool_raw(
                &state,
                &secret,
                "update_review_request",
                json!({ "id": settled_id, "action": action }),
            )
            .await;
            assert_eq!(advanced["result"]["isError"], false, "{action}");
        }
        let open_other = seed_request(&state, &secret, "/other", "head-open").await;
        assert_ne!(open_other["id"].as_i64().unwrap(), settled_id);

        // A withdrawn row on /demo.
        let withdrawn = seed_request(&state, &secret, "/demo", "head-2").await;
        let withdrawn_id = withdrawn["id"].as_i64().unwrap();
        let claimed = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": withdrawn_id, "action": "claim" }),
        )
        .await;
        assert_eq!(claimed["result"]["isError"], false);
        let gave_up = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": withdrawn_id, "action": "withdraw" }),
        )
        .await;
        assert_eq!(gave_up["result"]["isError"], false);

        // No filters: the cross-repo open queue, without approved and
        // withdrawn rows.
        let every = call_tool_raw(&state, &secret, "list_review_requests", json!({})).await;
        assert_eq!(every["result"]["isError"], false);
        let rows = result_text(&every).as_array().unwrap().clone();
        assert_eq!(rows.len(), 2, "the open queue excludes settled rows");
        assert!(rows.iter().any(|row| row["id"] == json!(demo_id)));
        assert!(rows.iter().any(|row| row["id"] == open_other["id"]));
        assert!(rows
            .iter()
            .all(|row| row["status"] != "approved" && row["status"] != "withdrawn"));

        // The repo filter narrows to that repository's open queue; the
        // combined filters AND; a status filter pins one exact status
        // across repositories.
        let for_other = call_tool_raw(
            &state,
            &secret,
            "list_review_requests",
            json!({ "repo_path": "/other" }),
        )
        .await;
        let rows = result_text(&for_other).as_array().unwrap().clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["repo_path"], "/other");
        let combined = call_tool_raw(
            &state,
            &secret,
            "list_review_requests",
            json!({ "repo_path": "/other", "status": "approved" }),
        )
        .await;
        let rows = result_text(&combined).as_array().unwrap().clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], json!(settled_id));
        let approved = call_tool_raw(
            &state,
            &secret,
            "list_review_requests",
            json!({ "status": "withdrawn" }),
        )
        .await;
        let rows = result_text(&approved).as_array().unwrap().clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], json!(withdrawn_id));

        // Derived fields ride with the stored ones: triage in one call.
        let listed = rows.first().unwrap();
        assert_eq!(listed["requester"], "mcp-agent");
        assert_eq!(listed["comment_count"], 3);
        assert_eq!(
            listed["unresolved_finding_counts"],
            json!({ "P0": 0, "P1": 1, "P2": 0, "P3": 1 })
        );
        let age = listed["age_ms"].as_i64().unwrap();
        assert!(age >= 0 && age < 60_000, "unexpected age {age}");
        assert_eq!(listed["note"], "Please review.");
        let demo_row = result_text(&every)
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == json!(demo_id))
            .unwrap()
            .clone();
        assert_eq!(demo_row["status"], "in_review");
        assert_eq!(demo_row["round"], 0);
    }

    #[tokio::test]
    async fn update_review_request_transitions_and_reports_violations() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let other = create_agent_token_in_pool(&pool, "other-agent").await.unwrap();
        let (state, secret) = test_state(pool.clone()).await;

        // Open pickup: any agent can claim an unnamed request.
        let created = seed_request(&state, &secret, "/demo", "head-1").await;
        let id = created["id"].as_i64().unwrap();
        let claimed = call_tool_raw(
            &state,
            &other.secret,
            "update_review_request",
            json!({ "id": id, "action": "claim" }),
        )
        .await;
        assert_eq!(claimed["result"]["isError"], false);
        assert_eq!(result_text(&claimed)["status"], "in_review");
        let changed = call_tool_raw(
            &state,
            &other.secret,
            "update_review_request",
            json!({ "id": id, "action": "request_changes" }),
        )
        .await;
        assert_eq!(changed["result"]["isError"], false);
        assert_eq!(result_text(&changed)["status"], "changes_requested");

        // Re-request with the refused head is an isError; the requester's
        // new-head re-request refreshes the note and bumps the round.
        let same_head = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": id, "action": "re_request", "head_sha": "head-1" }),
        )
        .await;
        assert_eq!(same_head["result"]["isError"], true);
        assert!(same_head["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("different"));

        // Re-request is requester-only for agents, and the refusal names
        // the allowed actor.
        let foreign = call_tool_raw(
            &state,
            &other.secret,
            "update_review_request",
            json!({ "id": id, "action": "re_request", "head_sha": "head-3" }),
        )
        .await;
        assert_eq!(foreign["result"]["isError"], true);
        assert!(foreign["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Only the requester can re-request"));

        // Re-request with a new head and a note refresh resets the
        // requester's sticky verdict and bumps the round.
        let reset = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": id, "action": "re_request", "head_sha": "head-2", "note": "Rebased." }),
        )
        .await;
        assert_eq!(reset["result"]["isError"], false);
        let row = result_text(&reset);
        assert_eq!(row["status"], "in_review");
        assert_eq!(row["round"], 1);
        assert_eq!(row["head_sha"], "head-2");
        assert_eq!(row["note"], "Rebased.");

        // The named reviewers list gates the claim for every other token.
        let gated = call_tool_raw(
            &state,
            &secret,
            "request_review",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "note": "Named reviewers only.",
                "head_sha": "head-named",
                "reviewers": ["other-agent"],
            }),
        )
        .await;
        assert_eq!(gated["result"]["isError"], false);
        let gated_id = result_text(&gated)["id"].as_i64().unwrap();
        let outsider = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": gated_id, "action": "claim" }),
        )
        .await;
        assert_eq!(outsider["result"]["isError"], true);
        assert!(outsider["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Only the named reviewers"));
        let named_claim = call_tool_raw(
            &state,
            &other.secret,
            "update_review_request",
            json!({ "id": gated_id, "action": "claim" }),
        )
        .await;
        assert_eq!(named_claim["result"]["isError"], false);

        // Withdrawal is requester-only for agents.
        let foreign_withdraw = call_tool_raw(
            &state,
            &other.secret,
            "update_review_request",
            json!({ "id": id, "action": "withdraw" }),
        )
        .await;
        assert_eq!(foreign_withdraw["result"]["isError"], true);
        assert!(foreign_withdraw["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Only the requester"));

        // Round budget: one re-request passes at max_rounds 1, the next is
        // refused as an isError and the row stays at its cap.
        let tight = call_tool_raw(
            &state,
            &secret,
            "request_review",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "note": "One round only.",
                "head_sha": "head-t1",
                "max_rounds": 1,
            }),
        )
        .await;
        assert_eq!(tight["result"]["isError"], false);
        let tight_id = result_text(&tight)["id"].as_i64().unwrap();
        let advance = |action| json!({ "id": tight_id, "action": action });
        for action in ["claim", "request_changes"] {
            let step = call_tool_raw(&state, &secret, "update_review_request", advance(action))
                .await;
            assert_eq!(step["result"]["isError"], false, "{action}");
        }
        let first_retry = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": tight_id, "action": "re_request", "head_sha": "head-t2" }),
        )
        .await;
        assert_eq!(first_retry["result"]["isError"], false);
        assert_eq!(result_text(&first_retry)["round"], 1);
        let changed = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": tight_id, "action": "request_changes" }),
        )
        .await;
        assert_eq!(changed["result"]["isError"], false);
        let exhausted = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": tight_id, "action": "re_request", "head_sha": "head-t3" }),
        )
        .await;
        assert_eq!(exhausted["result"]["isError"], true);
        assert!(exhausted["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("round budget of 1"));

        // An unknown request is an isError result with the engine's
        // message, not a -32602.
        let unknown = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": 9999, "action": "claim" }),
        )
        .await;
        assert_eq!(unknown["result"]["isError"], true);
        assert!(unknown["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("does not exist"));

        // The note refresh and the new head only mean something on a
        // re-request; a re-request without a head is refused.
        let note_misuse = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": tight_id, "action": "claim", "note": "nope" }),
        )
        .await;
        assert_eq!(note_misuse["result"]["isError"], true);
        assert!(note_misuse["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Only a re-request refreshes the note"));
        let head_misuse = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": tight_id, "action": "withdraw", "head_sha": "head-x" }),
        )
        .await;
        assert_eq!(head_misuse["result"]["isError"], true);
        assert!(head_misuse["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Only a re-request takes a new head"));
        let headless = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": tight_id, "action": "re_request" }),
        )
        .await;
        assert_eq!(headless["result"]["isError"], true);
        assert!(headless["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("non-empty head"));
        let long_note = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": tight_id, "action": "re_request", "head_sha": "head-t4", "note": "x".repeat(2001) }),
        )
        .await;
        assert_eq!(long_note["result"]["isError"], true);
        assert!(long_note["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("exceeds 2000 characters"));
    }

    #[tokio::test]
    async fn request_tools_attribute_to_the_calling_token() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (state, secret) = test_state(pool.clone()).await;
        let other = create_agent_token_in_pool(&pool, "other-agent").await.unwrap();

        let mine = seed_request(&state, &secret, "/demo", "head-1").await;
        let theirs = seed_request(&state, &other.secret, "/demo", "head-1").await;
        assert_ne!(mine["id"].as_i64().unwrap(), theirs["id"].as_i64().unwrap());
        assert_eq!(mine["requester"], "mcp-agent");
        assert_eq!(theirs["requester"], "other-agent");

        // Dedup is per requester: my same-head create updates only my row.
        let updated = call_tool_raw(
            &state,
            &secret,
            "request_review",
            json!({
                "repo_path": "/demo",
                "base_sha": "base",
                "target_key": "/demo",
                "target_kind": "worktree",
                "note": "Mine again.",
                "head_sha": "head-1",
            }),
        )
        .await;
        assert_eq!(updated["result"]["isError"], false);
        assert_eq!(result_text(&updated)["id"], mine["id"]);
        assert_eq!(result_text(&updated)["note"], "Mine again.");

        let listed = call_tool_raw(
            &state,
            &secret,
            "list_review_requests",
            json!({ "repo_path": "/demo" }),
        )
        .await;
        assert_eq!(listed["result"]["isError"], false);
        let rows = result_text(&listed).as_array().unwrap().clone();
        assert_eq!(rows.len(), 2);
        let their_row = rows.iter().find(|row| row["id"] == theirs["id"]).unwrap();
        assert_eq!(their_row["note"], "Please review.");
        assert_eq!(their_row["requester"], "other-agent");
    }

    // Every successful request mutation announces exactly one change event
    // through the state's sink; refused mutations announce nothing. The
    // engine fires centrally, so the tools only thread the sink through.
    #[tokio::test]
    async fn request_mutations_announce_once_with_the_right_payload() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let (changes, announced) = recording_request_changes();
        let secret = create_agent_token_in_pool(&pool, "mcp-agent")
            .await
            .unwrap()
            .secret;
        let state = TransportState {
            pool: pool.clone(),
            arrivals: Arc::new(|_| {}),
            refreshes: noop_refreshes(),
            comment_changes: noop_comment_changes(),
            request_changes: changes,
            status: dummy_status(),
        };
        let created = seed_request(&state, &secret, "/demo", "head-1").await;
        let id = created["id"].as_i64().unwrap();
        let claimed = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": id, "action": "claim" }),
        )
        .await;
        assert_eq!(claimed["result"]["isError"], false);

        // A refused mutation announces nothing.
        let refused = call_tool_raw(
            &state,
            &secret,
            "update_review_request",
            json!({ "id": id, "action": "claim" }),
        )
        .await;
        assert_eq!(refused["result"]["isError"], true);

        let events = announced.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, id);
        assert_eq!(events[0].repo_path, "/demo");
        assert_eq!(events[0].base_sha, "base");
        assert_eq!(events[0].target_key, "/demo");
        assert_eq!(events[0].target_kind, "worktree");
        assert_eq!(events[0].status, "requested");
        assert_eq!(events[1].id, id);
        assert_eq!(events[1].status, "in_review");
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    // Sends one raw HTTP request over a real loopback socket; the blocking
    // std client runs on its own thread so no async runtime is involved.
    fn send_over_socket(
        port: u16,
        method: &str,
        path: &str,
        bearer: &str,
        body: String,
    ) -> std::thread::JoinHandle<String> {
        let request = format!(
            "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nauthorization: Bearer {bearer}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len(),
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
            let mut response = Vec::new();
            stream
                .read_to_end(&mut response)
                .expect("the endpoint did not answer within 10s");
            String::from_utf8(response).unwrap()
        })
    }

    fn body_of(raw: &str) -> Value {
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    // Routing: the MCP face answers POSTs, refuses other methods with 405,
    // and unknown paths stay 404.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mcp_is_refused_and_unknown_paths_stay_not_found() {
        let pool = test_pool().await;
        let dir = test_path("mcp-routing");
        std::fs::create_dir_all(&dir).unwrap();
        let config = ListenerConfig { enabled: true, address: "127.0.0.1".into(), port: free_port() };
        let listener = start(
            pool,
            TransportDeps {
                data_dir: dir.clone(),
                arrivals: Arc::new(|_| {}),
                refreshes: noop_refreshes(),
                comment_changes: noop_comment_changes(),
                request_changes: noop_request_changes(),
            },
            config.clone(),
            McpStatusHandle::for_config(&config),
        )
        .await
        .unwrap()
        .unwrap();
        let discovery: Value =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(&dir)).unwrap()).unwrap();
        let port = discovery["port"].as_u64().unwrap() as u16;
        let bearer = discovery["token"].as_str().unwrap().to_string();

        let response =
            send_over_socket(port, "GET", "/mcp", &bearer, String::new()).join().unwrap();
        assert!(
            response.starts_with("HTTP/1.1 405 Method Not Allowed"),
            "unexpected response: {response}"
        );
        let response = send_over_socket(
            port,
            "POST",
            "/nope",
            &bearer,
            mcp_body(json!(1), "server/discover", json!({})),
        )
        .join()
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 404 Not Found"), "unexpected response: {response}");
        let response = send_over_socket(
            port,
            "POST",
            "/mcp",
            &bearer,
            mcp_body(json!(1), "server/discover", json!({})),
        )
        .join()
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "unexpected response: {response}");
        listener.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // End-to-end over a real loopback socket, one connection per request
    // (the face is stateless): discovery, the tool catalog, a comment write
    // attributed to the calling token, and the refresh no-op.
    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_round_trip_over_a_live_socket() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let repo = test_repo("mcp-socket-refresh");
        seed_repo(&pool, repo.to_str().unwrap()).await;
        let (refreshes, announced) = recording_refreshes();
        let dir = test_path("mcp-socket");
        std::fs::create_dir_all(&dir).unwrap();
        let config = ListenerConfig { enabled: true, address: "127.0.0.1".into(), port: free_port() };
        let listener = start(
            pool.clone(),
            TransportDeps {
                data_dir: dir.clone(),
                arrivals: Arc::new(|_| {}),
                refreshes,
                comment_changes: noop_comment_changes(),
                request_changes: noop_request_changes(),
            },
            config.clone(),
            McpStatusHandle::for_config(&config),
        )
        .await
        .unwrap()
        .unwrap();
        let discovery: Value =
            serde_json::from_slice(&std::fs::read(endpoint_config_path(&dir)).unwrap()).unwrap();
        let port = discovery["port"].as_u64().unwrap() as u16;
        let bearer = discovery["token"].as_str().unwrap().to_string();

        let response = send_over_socket(
            port,
            "POST",
            "/mcp",
            &bearer,
            mcp_body(json!(1), "server/discover", json!({})),
        )
        .join()
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "unexpected response: {response}");
        let payload = body_of(&response);
        assert_eq!(payload["result"]["protocolVersions"], json!(["2026-07-28"]));
        assert_eq!(payload["result"]["serverInfo"]["name"], "worktreeview");

        let response = send_over_socket(
            port,
            "POST",
            "/mcp",
            &bearer,
            mcp_body(json!(2), "tools/list", meta(PROTOCOL_VERSION)),
        )
        .join()
        .unwrap();
        let payload = body_of(&response);
        let names: Vec<&str> = payload["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, TOOL_NAMES);
        assert_eq!(payload["result"]["tools"][0]["cacheScope"], "private");

        let response = send_over_socket(
            port,
            "POST",
            "/mcp",
            &bearer,
            call_tool(
                "create_comment",
                json!({
                    "repo_path": "/demo",
                    "base_sha": "base",
                    "target_key": "/demo",
                    "target_kind": "worktree",
                    "body": "socket comment",
                }),
            ),
        )
        .join()
        .unwrap();
        let payload = body_of(&response);
        assert_eq!(payload["result"]["resultType"], "complete");
        assert_eq!(payload["result"]["isError"], false);
        let comment = result_text(&payload);
        assert_eq!(comment["body"], "socket comment");
        let comment_id = comment["id"].as_i64().unwrap();

        let response = send_over_socket(
            port,
            "POST",
            "/mcp",
            &bearer,
            call_tool("refresh_repo", json!({ "repo_path": repo.to_str().unwrap() })),
        )
        .join()
        .unwrap();
        let payload = body_of(&response);
        assert_eq!(payload["result"]["isError"], false);
        assert_eq!(result_text(&payload), json!({ "ok": true }));
        assert_eq!(
            announced.lock().unwrap().as_slice(),
            [repo.to_string_lossy().as_ref()]
        );

        listener.stop();
        // Storage attribution is asserted after the endpoint stops serving:
        // querying the shared single-connection pool mid-service would only
        // exercise sqlx cross-runtime scheduling, as in the raw face's
        // socket test.
        let default_id: i64 =
            sqlx::query_scalar("SELECT id FROM agent_tokens WHERE is_default = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        let owner: Option<i64> =
            sqlx::query_scalar("SELECT author_token_id FROM comments WHERE id = ?")
                .bind(comment_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(owner, Some(default_id));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&repo);
    }
}
