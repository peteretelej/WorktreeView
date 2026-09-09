use crate::agents::AgentIdentity;
use crate::commands::{list_worktrees_in_path, refresh_repo};
use crate::overview::branch_inventory;
use crate::reviews::{
    create_unbound_comment_in_pool, delete_comment_in_pool, edit_comment_in_pool,
    list_comments_in_pool, list_submissions_in_pool, reply_comment_in_pool,
    MAX_AGENT_MODEL_CHARS,
    review_identity_of_comment, set_comment_resolved_in_pool, Actor,
};
use crate::store::{list_repo_rows_in_pool, open_repo_path};
use crate::transport::{
    rpc_error, CommentChange, TransportState, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND,
    PARSE_ERROR,
};
use crate::CommandError;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;

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
    let worktrees = list_worktrees_in_path(&args.repo_path)
        .await
        .map_err(|error| error.message)?;
    let inventory = branch_inventory(args.repo_path.clone())
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
    refresh_repo(Path::new(&args.repo_path), &state.refreshes)
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
        discovery_path, handle, start, CommentChange, CommentSink, ListenerConfig, ListenerStatus,
        McpStatusHandle, RefreshSink, UNAUTHORIZED,
    };
    use axum::body::{to_bytes, Body};
    use axum::extract::State;
    use axum::http::{header, Request};
    use std::sync::{Arc, Mutex};

    const OWNERSHIP_MESSAGE: &str =
        "Only the agent token that authored a comment can edit or delete it.";
    const UNKNOWN_REPO_MESSAGE: &str = "No repository with that path is open in WorktreeView.";
    const TOOL_NAMES: [&str; 11] = [
        "list_repos",
        "list_review_targets",
        "list_comments",
        "list_submissions",
        "create_comment",
        "reply_comment",
        "resolve_thread",
        "edit_own_comment",
        "delete_own_comment",
        "add_repo",
        "refresh_repo",
    ];

    fn noop_refreshes() -> RefreshSink {
        Arc::new(|_| {})
    }

    fn noop_comment_changes() -> CommentSink {
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
        let mcp = start(
            pool,
            &dir,
            Arc::new(|_| {}),
            noop_refreshes(),
            noop_comment_changes(),
            ListenerConfig { enabled: true, address: "127.0.0.1".into(), port: free_port() },
        )
        .await
        .unwrap();
        let discovery: Value =
            serde_json::from_slice(&std::fs::read(discovery_path(&dir)).unwrap()).unwrap();
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
        mcp.handle.unwrap().shutdown();
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
        let mcp = start(
            pool.clone(),
            &dir,
            Arc::new(|_| {}),
            refreshes,
            noop_comment_changes(),
            ListenerConfig { enabled: true, address: "127.0.0.1".into(), port: free_port() },
        )
        .await
        .unwrap();
        let discovery: Value =
            serde_json::from_slice(&std::fs::read(discovery_path(&dir)).unwrap()).unwrap();
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

        mcp.handle.unwrap().shutdown();
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
