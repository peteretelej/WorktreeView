//! The human command API: the typed command surface served over HTTP as
//! uniform `POST /api/<command>` routes, one dispatch table over the same
//! shared implementations the IPC adapters call. Request bodies are JSON
//! objects whose keys are the commands' argument names in the same
//! camelCase the webview IPC uses; responses are the command's result
//! serialized exactly as the IPC layer returns it, or a command error as
//! the IPC layer serializes it (`{"code", "message"}`) with HTTP status
//! 400. Transport-level refusals share that JSON shape: a failed bearer
//! evaluation or an agent token on these routes answers 401 `unauthorized`,
//! a non-admin on an admin-only route answers 403 `forbidden`, and a body
//! that does not parse into the command's arguments answers 400
//! `invalid_arguments`.
//!
//! Coverage mirrors the shipped 45-command IPC surface, classified once:
//!
//! 33 straight dispatch routes (the human review domain): `open_repo`,
//! `list_repos`, `list_worktrees`, `list_worktree_status`, `remove_repo`,
//! `get_branch_inventory`, `fetch_project`, `fetch_review_objects`,
//! `list_refs`, `list_commits`, `describe_commit`, `list_review_changes`,
//! `list_surfaces`, `list_attention`, `list_portal_reviews`,
//! `list_portal_threads`, `get_portal_thread`, `search_portal`,
//! `list_portal_activity`, `list_requests`, `create_review_request`,
//! `update_review_request`, `read_review_patch`, `read_review_file`,
//! `read_review_file_bytes`, `create_comment`, `list_comments`,
//! `list_submissions`, `reply_comment`, `set_comment_resolved`,
//! `edit_comment`, `delete_comment`, `match_comment_anchors`. These run
//! for any member. `remove_repo` is any-member and cascades shared review
//! state server-side; `fetch_project` and `fetch_review_objects` run the
//! server's existing bounded fetch paths (the only Git writes,
//! human-initiated); the review-request trio rides the existing
//! human-parity rules in `requests.rs`; `read_review_file_bytes` returns
//! raw bytes (`application/octet-stream`) exactly as the IPC command
//! returns them, not JSON.
//!
//! 3 agent-token routes, admin-only with human-actor attribution:
//! `list_agent_tokens`, `create_agent_token` (plaintext printed once,
//! never stored), `delete_agent_token`.
//!
//! 6 new management routes over the identity store, admin-only:
//! `list_users`, `create_user` (initial token printed once), `delete_user`
//! (refuses the last admin), and the user-token lifecycle `list_user_tokens`,
//! `create_user_token` (rotation; printed once), `delete_user_token`
//! (revoke a leaked token without deleting the account).
//!
//! 9 IPC commands are excluded from HTTP, each with the reason it has no
//! server meaning: `set_repo_pinned` and `set_surface_pinned`
//! (presentation prefs stay client-local), `get_settings` and
//! `set_settings` (desktop-local settings; server bind config is
//! ops-level, documented in docs/server.md), `mark_activity_seen` (the
//! seen cursor would become one shared cross-user watermark),
//! `open_review_file` and `open_log_dir` (act on the server host's OS,
//! not the viewer's machine), `get_mcp_status` and `restart_mcp`
//! (endpoint lifecycle is ops-level via systemd/Docker, documented in
//! docs/server.md).

use crate::agents::{
    create_agent_token_in_pool, delete_agent_token_in_pool, list_agent_tokens_in_pool,
};
use crate::commands::{
    create_request_as_human, fetch_review_objects, list_worktree_status, portal_activity_page,
    refresh_repo, update_request_in_pool, RequestAction,
};
use crate::identity::{
    create_user_in_pool, create_user_token_in_pool, delete_user_in_pool,
    delete_user_token_in_pool, list_user_tokens_in_pool, list_users_in_pool, User,
};
use crate::requests::{list_attention_in_pool, list_requests_in_pool};
use crate::reviews::{
    create_comment_in_pool, delete_comment_in_pool, edit_comment_in_pool, list_comments_in_pool,
    list_submissions_in_pool, match_comment_anchors_in_pool, reply_comment_in_pool,
    set_comment_resolved_in_pool, Actor, CommentDraft, PatchLine,
};
use crate::store::{load_repos, open_repo_path, remove_repo_in_pool};
use crate::transport::TransportState;
use crate::{canonical_path, CommandError};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) fn unauthorized(message: &'static str) -> Response {
    error_response(StatusCode::UNAUTHORIZED, "unauthorized", message)
}

fn forbidden() -> Response {
    error_response(StatusCode::FORBIDDEN, "forbidden", "This command is admin-only.")
}

fn error_response(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "code": code, "message": message.into() }))).into_response()
}

// One dispatch entry's answer: the command's result serialized as IPC
// returns it, or the command error in the IPC error shape at status 400.
fn json_result<R: Serialize>(result: Result<R, CommandError>) -> Result<Response, Response> {
    match result {
        Ok(value) => Ok((StatusCode::OK, Json(value)).into_response()),
        Err(error) => Err(error_response(StatusCode::BAD_REQUEST, &error.code, error.message)),
    }
}

fn command_error_response(error: CommandError) -> Response {
    error_response(StatusCode::BAD_REQUEST, &error.code, error.message)
}

fn bytes_result(result: Result<Vec<u8>, CommandError>) -> Result<Response, Response> {
    match result {
        Ok(bytes) => Ok((StatusCode::OK, [(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response()),
        Err(error) => Err(error_response(StatusCode::BAD_REQUEST, &error.code, error.message)),
    }
}

fn parse_args<T: DeserializeOwned>(body: &[u8]) -> Result<T, Response> {
    let value: Value = serde_json::from_slice(body).map_err(|_| {
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_arguments",
            "The command body must be a JSON object of arguments.",
        )
    })?;
    serde_json::from_value(value).map_err(|error| {
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_arguments",
            format!("Invalid command arguments: {error}"),
        )
    })
}

// The server is the enforcement authority: management routes check the
// flag the bearer resolution produced, never a client claim.
fn require_admin(user: &User) -> Result<(), Response> {
    if user.is_admin {
        Ok(())
    } else {
        Err(forbidden())
    }
}

pub(crate) async fn serve(state: &TransportState, user: &User, command: &str, body: &[u8]) -> Response {
    match route(state, user, command, body).await {
        Ok(response) | Err(response) => response,
    }
}

async fn route(
    state: &TransportState,
    user: &User,
    command: &str,
    body: &[u8],
) -> Result<Response, Response> {
    match command {
        // Straight dispatch: the human review domain, any member.
        "open_repo" => {
            let args: PathArgs = parse_args(body)?;
            json_result(open_repo_path(&args.path, &state.pool).await)
        }
        "list_repos" => {
            let _: NoArgs = parse_args(body)?;
            json_result(load_repos(&state.pool).await)
        }
        "list_worktrees" => {
            let args: PathArgs = parse_args(body)?;
            json_result(crate::commands::list_worktrees(args.path).await)
        }
        "list_worktree_status" => {
            let args: PathArgs = parse_args(body)?;
            json_result(list_worktree_status(args.path).await)
        }
        "remove_repo" => {
            let args: PathArgs = parse_args(body)?;
            json_result(remove_repo_in_pool(&state.pool, &args.path).await)
        }
        "get_branch_inventory" => {
            let args: PathArgs = parse_args(body)?;
            json_result(crate::commands::get_branch_inventory(args.path).await)
        }
        "fetch_project" => {
            let args: PathArgs = parse_args(body)?;
            let path = canonical_path(&args.path).map_err(command_error_response)?;
            json_result(refresh_repo(&path, &state.refreshes).await)
        }
        "fetch_review_objects" => {
            let args: RefFetchArgs = parse_args(body)?;
            json_result(fetch_review_objects(args.path, args.target_ref).await)
        }
        "list_refs" => {
            let args: RefsArgs = parse_args(body)?;
            json_result(crate::commands::list_refs(args.path, args.worktree_branch, args.target_ref).await)
        }
        "list_commits" => {
            let args: CommitsArgs = parse_args(body)?;
            json_result(
                crate::review::commit_page(
                    &state.pool,
                    &args.repo_path,
                    args.path,
                    args.start_ref,
                    args.against,
                    args.skip,
                    args.limit,
                )
                .await,
            )
        }
        "describe_commit" => {
            let args: RevArgs = parse_args(body)?;
            json_result(crate::review::commit_detail(args.path, args.rev).await)
        }
        "list_review_changes" => {
            let args: ReviewChangesArgs = parse_args(body)?;
            json_result(
                crate::review::review_changes(
                    &state.pool,
                    &args.repo_path,
                    args.path,
                    args.base,
                    args.head_ref,
                    args.committed_only,
                    args.reversed,
                )
                .await,
            )
        }
        "list_surfaces" => {
            let args: PathArgs = parse_args(body)?;
            json_result(crate::retrospection::list_surfaces_in_pool(&state.pool, &args.path).await)
        }
        "list_attention" => {
            let _: NoArgs = parse_args(body)?;
            json_result(list_attention_in_pool(&state.pool).await)
        }
        "list_portal_reviews" => {
            let args: ReviewsQueryArgs = parse_args(body)?;
            json_result(
                crate::portal::list_portal_reviews_in_pool(
                    &state.pool,
                    &crate::portal::PortalReviewQuery {
                        repo_path: args.repo_path,
                        state: args.state_filter,
                        search: args.search,
                    },
                )
                .await,
            )
        }
        "list_portal_threads" => {
            let args: ThreadsQueryArgs = parse_args(body)?;
            json_result(
                crate::portal::list_portal_threads_in_pool(
                    &state.pool,
                    &crate::portal::PortalThreadQuery {
                        repo_path: args.repo_path,
                        state: args.state_filter,
                        voice: args.voice,
                        text: args.text,
                    },
                )
                .await,
            )
        }
        "get_portal_thread" => {
            let args: ThreadArgs = parse_args(body)?;
            json_result(crate::portal::get_portal_thread_in_pool(&state.pool, args.root_comment_id).await)
        }
        "search_portal" => {
            let args: SearchArgs = parse_args(body)?;
            json_result(crate::portal::search_portal_in_pool(&state.pool, &args.needle).await)
        }
        "list_portal_activity" => {
            let args: ActivityArgs = parse_args(body)?;
            json_result(portal_activity_page(&state.pool, args.repo_path, args.limit).await)
        }
        "list_requests" => {
            let args: ReviewIdentityArgs = parse_args(body)?;
            json_result(
                list_requests_in_pool(
                    &state.pool,
                    &args.repo_path,
                    &args.base_sha,
                    &args.target_key,
                    &args.target_kind,
                )
                .await,
            )
        }
        "create_review_request" => {
            let args: CreateRequestArgs = parse_args(body)?;
            json_result(
                create_request_as_human(
                    &state.pool,
                    &args.repo_path,
                    &args.base_sha,
                    &args.target_key,
                    &args.target_kind,
                    args.note,
                    args.lenses,
                    args.reviewers,
                    args.max_rounds,
                    args.head_sha,
                    &user.name,
                    &state.request_changes,
                )
                .await,
            )
        }
        "update_review_request" => {
            let args: UpdateRequestArgs = parse_args(body)?;
            json_result(
                update_request_in_pool(
                    &state.pool,
                    args.id,
                    args.action,
                    args.note,
                    args.head_sha,
                    &user.name,
                    &state.request_changes,
                )
                .await,
            )
        }
        "read_review_patch" => {
            let args: ReviewReadArgs = parse_args(body)?;
            json_result(
                crate::review::review_patch(
                    args.path,
                    args.base,
                    args.head_ref,
                    args.committed_only,
                    args.reversed,
                    args.file,
                    args.untracked,
                )
                .await,
            )
        }
        "read_review_file" => {
            let args: ReviewReadArgs = parse_args(body)?;
            json_result(
                crate::review::review_file_content(
                    args.path,
                    args.base,
                    args.head_ref,
                    args.committed_only,
                    args.reversed,
                    args.file,
                    args.untracked,
                )
                .await,
            )
        }
        "read_review_file_bytes" => {
            let args: ReviewReadArgs = parse_args(body)?;
            bytes_result(
                crate::review::review_file_bytes(
                    args.path,
                    args.base,
                    args.head_ref,
                    args.committed_only,
                    args.reversed,
                    args.file,
                    args.untracked,
                )
                .await,
            )
        }
        "create_comment" => {
            let args: CreateCommentArgs = parse_args(body)?;
            json_result(
                create_comment_in_pool(
                    &state.pool,
                    &args.repo_path,
                    &args.base_sha,
                    &args.target_key,
                    &args.target_kind,
                    &args.draft,
                    &Actor::Human(user.name.clone()),
                    None,
                )
                .await,
            )
        }
        "list_comments" => {
            let args: ReviewIdentityArgs = parse_args(body)?;
            json_result(
                list_comments_in_pool(
                    &state.pool,
                    &args.repo_path,
                    &args.base_sha,
                    &args.target_key,
                    &args.target_kind,
                )
                .await,
            )
        }
        "list_submissions" => {
            let args: ReviewIdentityArgs = parse_args(body)?;
            json_result(
                list_submissions_in_pool(
                    &state.pool,
                    &args.repo_path,
                    &args.base_sha,
                    &args.target_key,
                    &args.target_kind,
                )
                .await,
            )
        }
        "reply_comment" => {
            let args: ReplyArgs = parse_args(body)?;
            json_result(
                reply_comment_in_pool(
                    &state.pool,
                    args.parent_id,
                    &args.body,
                    args.severity,
                    &Actor::Human(user.name.clone()),
                    None,
                )
                .await,
            )
        }
        "set_comment_resolved" => {
            let args: ResolveArgs = parse_args(body)?;
            json_result(
                set_comment_resolved_in_pool(
                    &state.pool,
                    args.comment_id,
                    args.resolved,
                    &Actor::Human(user.name.clone()),
                )
                .await,
            )
        }
        "edit_comment" => {
            let args: EditArgs = parse_args(body)?;
            json_result(
                edit_comment_in_pool(
                    &state.pool,
                    args.comment_id,
                    &args.body,
                    &Actor::Human(user.name.clone()),
                )
                .await,
            )
        }
        "delete_comment" => {
            let args: CommentArgs = parse_args(body)?;
            json_result(
                delete_comment_in_pool(
                    &state.pool,
                    args.comment_id,
                    &Actor::Human(user.name.clone()),
                )
                .await,
            )
        }
        "match_comment_anchors" => {
            let args: AnchorsArgs = parse_args(body)?;
            json_result(
                match_comment_anchors_in_pool(
                    &state.pool,
                    &args.repo_path,
                    &args.base_sha,
                    &args.target_key,
                    &args.target_kind,
                    &args.file_path,
                    &args.lines,
                )
                .await,
            )
        }

        // Agent-token management: admin-only, human-attributed.
        "list_agent_tokens" => {
            require_admin(user)?;
            let _: NoArgs = parse_args(body)?;
            json_result(list_agent_tokens_in_pool(&state.pool).await)
        }
        "create_agent_token" => {
            require_admin(user)?;
            let args: NameArgs = parse_args(body)?;
            json_result(create_agent_token_in_pool(&state.pool, &args.name).await)
        }
        "delete_agent_token" => {
            require_admin(user)?;
            let args: IdArgs = parse_args(body)?;
            json_result(delete_agent_token_in_pool(&state.pool, args.id).await)
        }

        // Member and user-token management: admin-only over identity.rs.
        "list_users" => {
            require_admin(user)?;
            let _: NoArgs = parse_args(body)?;
            json_result(list_users_in_pool(&state.pool).await)
        }
        "create_user" => {
            require_admin(user)?;
            let args: NameArgs = parse_args(body)?;
            json_result(create_user_in_pool(&state.pool, &args.name).await)
        }
        "delete_user" => {
            require_admin(user)?;
            let args: IdArgs = parse_args(body)?;
            json_result(delete_user_in_pool(&state.pool, args.id).await)
        }
        "list_user_tokens" => {
            require_admin(user)?;
            let args: UserTokenListArgs = parse_args(body)?;
            json_result(list_user_tokens_in_pool(&state.pool, args.user_id).await)
        }
        "create_user_token" => {
            require_admin(user)?;
            let args: UserTokenCreateArgs = parse_args(body)?;
            json_result(create_user_token_in_pool(&state.pool, args.user_id).await)
        }
        "delete_user_token" => {
            require_admin(user)?;
            let args: IdArgs = parse_args(body)?;
            json_result(delete_user_token_in_pool(&state.pool, args.id).await)
        }

        _ => Err((
            StatusCode::NOT_FOUND,
            "Unknown command route; POST /api/<command> names a dispatched command.\n",
        )
            .into_response()),
    }
}

// Argument shapes mirror the IPC adapters; keys arrive in the webview's
// camelCase argument convention.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefFetchArgs {
    path: String,
    target_ref: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefsArgs {
    path: String,
    worktree_branch: Option<String>,
    target_ref: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitsArgs {
    path: String,
    repo_path: String,
    start_ref: Option<String>,
    against: Option<String>,
    skip: Option<u32>,
    limit: Option<u16>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevArgs {
    path: String,
    rev: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewChangesArgs {
    path: String,
    repo_path: String,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewsQueryArgs {
    repo_path: Option<String>,
    state_filter: Option<String>,
    search: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadsQueryArgs {
    repo_path: Option<String>,
    state_filter: Option<String>,
    voice: Option<String>,
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadArgs {
    root_comment_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchArgs {
    needle: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityArgs {
    repo_path: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewIdentityArgs {
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateRequestArgs {
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    note: String,
    lenses: Option<Vec<String>>,
    reviewers: Option<Vec<String>>,
    max_rounds: Option<i64>,
    head_sha: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateRequestArgs {
    id: i64,
    action: RequestAction,
    note: Option<String>,
    head_sha: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewReadArgs {
    path: String,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateCommentArgs {
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    draft: CommentDraft,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplyArgs {
    parent_id: i64,
    body: String,
    severity: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveArgs {
    comment_id: i64,
    resolved: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditArgs {
    comment_id: i64,
    body: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentArgs {
    comment_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnchorsArgs {
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    file_path: String,
    lines: Vec<PatchLine>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NameArgs {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdArgs {
    id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserTokenListArgs {
    user_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserTokenCreateArgs {
    user_id: i64,
}

#[derive(Deserialize)]
struct NoArgs {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{create_first_admin_in_pool, verify_user_token_in_pool};
    use crate::testutil::{seed_repo, test_pool};
    use crate::transport::{
        CommentSink, ListenerStatus, McpStatusHandle, ArrivalSink, RefreshSink, RequestChangeSink,
    };
    use std::sync::{Arc, Mutex};

    fn noop_arrivals() -> ArrivalSink {
        Arc::new(|_| {})
    }

    fn noop_refreshes() -> RefreshSink {
        Arc::new(|_| {})
    }

    fn noop_comment_changes() -> CommentSink {
        Arc::new(|_| {})
    }

    fn noop_request_changes() -> RequestChangeSink {
        Arc::new(|_| {})
    }

    fn status_handle() -> McpStatusHandle {
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

    async fn test_state(pool: sqlx::SqlitePool) -> TransportState {
        TransportState {
            pool,
            arrivals: noop_arrivals(),
            refreshes: noop_refreshes(),
            comment_changes: noop_comment_changes(),
            request_changes: noop_request_changes(),
            status: status_handle(),
            events: None,
        }
    }

    // Response body helpers: routes answer JSON, errors carry the IPC
    // error shape.
    async fn body(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn admin_and_member(pool: &sqlx::SqlitePool) -> (User, User) {
        let admin_created = create_first_admin_in_pool(pool, "ops").await.unwrap();
        let admin = verify_user_token_in_pool(pool, &admin_created.secret).await.unwrap();
        let member_created = create_user_in_pool(pool, "dana").await.unwrap();
        let member = verify_user_token_in_pool(pool, &member_created.secret).await.unwrap();
        (admin, member)
    }

    // A command route round-trips the shared implementation: the seeded
    // repo comes back exactly as the IPC command returns it.
    #[tokio::test]
    async fn command_route_returns_the_shared_implementation_result() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let state = test_state(pool.clone()).await;
        let (admin, _) = admin_and_member(&pool).await;

        let response = serve(&state, &admin, "list_repos", b"{}").await;
        let (status, payload) = body(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload[0]["path"], "/demo");
    }

    // A command error maps to the IPC error shape at status 400, and an
    // unparseable body is invalid_arguments, not a panic.
    #[tokio::test]
    async fn command_errors_map_to_the_ipc_error_shape() {
        let pool = test_pool().await;
        let state = test_state(pool.clone()).await;
        let (admin, _) = admin_and_member(&pool).await;

        let response = serve(&state, &admin, "open_repo", br#"{"path": "/definitely/missing"}"#).await;
        let (status, payload) = body(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(payload["code"], "invalid_path");

        let response = serve(&state, &admin, "open_repo", b"not json").await;
        let (status, payload) = body(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(payload["code"], "invalid_arguments");

        let response = serve(&state, &admin, "no_such_command", b"{}").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // The human mutation attribution: a comment created through the route
    // records the authenticated user's name exactly as the desktop records
    // its own.
    #[tokio::test]
    async fn human_comment_mutation_attributes_the_authenticated_user() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let state = test_state(pool.clone()).await;
        let (admin, member) = admin_and_member(&pool).await;

        let args = serde_json::json!({
            "repoPath": "/demo",
            "baseSha": "base",
            "targetKey": "/demo",
            "targetKind": "worktree",
            "draft": { "body": "a human note", "lines": [] },
        });
        let response = serve(&state, &admin, "create_comment", args.to_string().as_bytes()).await;
        let (status, payload) = body(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["author_kind"], "human");
        assert_eq!(payload["author_name"], "ops");

        // Any member may edit or delete any human-authored comment; the
        // team's all-equal ownership rule runs through one gate.
        let edit = serde_json::json!({ "commentId": payload["id"], "body": "edited by dana" });
        let response = serve(&state, &member, "edit_comment", edit.to_string().as_bytes()).await;
        let (status, payload) = body(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["author_name"], "ops");
        assert_eq!(payload["body"], "edited by dana");
    }

    // Admin enforcement: the agent-token and management routes refuse a
    // plain member with 403 forbidden and let an admin through; created
    // secrets ride the response exactly once (print-once).
    #[tokio::test]
    async fn admin_routes_refuse_members_and_serve_admins() {
        let pool = test_pool().await;
        let state = test_state(pool.clone()).await;
        let (admin, member) = admin_and_member(&pool).await;

        for command in ["list_agent_tokens", "list_users"] {
            let response = serve(&state, &member, command, b"{}").await;
            let (status, payload) = body(response).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{command}");
            assert_eq!(payload["code"], "forbidden", "{command}");
        }

        let response = serve(&state, &member, "create_user", br#"{"name": "kim"}"#).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = serve(&state, &admin, "create_agent_token", br#"{"name": "codex"}"#).await;
        let (status, payload) = body(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["token"]["name"], "codex");
        assert!(!payload["secret"].as_str().unwrap().is_empty(), "print-once secret");

        let response = serve(&state, &admin, "create_user", br#"{"name": "kim"}"#).await;
        let (status, payload) = body(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["user"]["name"], "kim");
        assert!(!payload["secret"].as_str().unwrap().is_empty());

        // The last-admin refusal surfaces through the route, while a
        // missing user keeps its own code.
        let kim_id = payload["user"]["id"].as_i64().unwrap();
        let missing = serde_json::json!({ "id": kim_id + 1000 });
        let response = serve(&state, &admin, "delete_user", missing.to_string().as_bytes()).await;
        let (_, payload) = body(response).await;
        assert_eq!(payload["code"], "invalid_user");
        let ops_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE name = 'ops'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let delete_ops = serde_json::json!({ "id": ops_id });
        let response = serve(&state, &admin, "delete_user", delete_ops.to_string().as_bytes()).await;
        let (status, payload) = body(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(payload["code"], "last_admin");

        // User-token rotation and revocation through the routes.
        let rotation = serde_json::json!({ "userId": ops_id });
        let response = serve(&state, &admin, "create_user_token", rotation.to_string().as_bytes()).await;
        let (status, payload) = body(response).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!payload["secret"].as_str().unwrap().is_empty());
        let token_id = payload["token"]["id"].as_i64().unwrap();
        let listed = serve(&state, &admin, "list_user_tokens", rotation.to_string().as_bytes()).await;
        let (status, payload) = body(listed).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload.as_array().unwrap().len(), 2);
        let revoke = serde_json::json!({ "id": token_id });
        let response = serve(&state, &admin, "delete_user_token", revoke.to_string().as_bytes()).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}
