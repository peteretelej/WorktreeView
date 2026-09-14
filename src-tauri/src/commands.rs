use crate::agents::{
    create_agent_token_in_pool, list_agent_tokens_in_pool, delete_agent_token_in_pool,
    AgentToken, CreatedAgentToken,
};
use crate::git::{
    fetch_remote_branch, fetch_remotes, git_execution_error, parse_status_count, parse_worktrees,
    remote_branch_of_tracking_ref, run_git, validate_fetch_name, validate_ref, CommitPage,
    Worktree,
};
use crate::overview::{branch_inventory, BranchInventory};
use crate::portal::{
    get_portal_thread_in_pool, list_portal_reviews_in_pool, list_portal_threads_in_pool,
    search_portal_in_pool, PortalReviewQuery, PortalReviewRow, PortalSearchMatches,
    PortalThreadDetail, PortalThreadGroup, PortalThreadQuery,
};
use crate::requests::{list_attention_in_pool, AttentionQueue};
use crate::review::{
    commit_detail, commit_page, refs_inventory, review_changes, review_file_bytes,
    review_file_content, review_patch, CommitDetail, FileContent, FilePatch, RefInventory,
    ReviewIndex,
};
use crate::retrospection::{list_surfaces_in_pool, set_surface_pinned_in_pool, SurfaceListing};
use crate::requests::{
    create_request_in_pool, list_requests_in_pool, refresh_request_note, request_row_by_id,
    re_request_in_pool, set_request_verdict_in_pool, validate_request_note,
    withdraw_request_in_pool, RequestDraft, RequestRow,
};
use crate::reviews::{
    create_comment_in_pool, delete_comment_in_pool, edit_comment_in_pool, list_comments_in_pool,
    list_submissions_in_pool, match_comment_anchors_in_pool, reply_comment_in_pool,
    set_comment_resolved_in_pool, Actor, AnchorStatus, Comment, CommentDraft, PatchLine,
    Submission,
};
use crate::store::{
    get_settings_in_pool, load_repos, mark_activity_seen_in_pool, open_repo_path,
    remove_repo_in_pool, set_repo_pinned_in_pool, set_settings_in_pool, Repo, Settings,
};
use crate::transport::{
    restart, ListenerConfig, ListenerOwner, ListenerStatus, McpStatusHandle, TransportDeps,
};
use crate::{canonical_path, plain_path, AppState, CommandError};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;
use tauri::Manager;

#[tauri::command]
pub(crate) async fn open_repo(
    path: String,
    state: tauri::State<'_, AppState>,
) -> Result<Repo, CommandError> {
    open_repo_path(&path, &state.pool).await
}

#[tauri::command]
pub(crate) async fn list_repos(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<Repo>, CommandError> {
    load_repos(&state.pool).await
}

// Path-scoped worktree listing behind the list_worktrees command: the MCP
// face's list_review_targets reuses the same inventory the UI shows.
pub(crate) async fn list_worktrees_in_path(path: &str) -> Result<Vec<Worktree>, CommandError> {
    let path = canonical_path(path)?;
    let (exit_code, stdout, stderr) = run_git(
        &path,
        &[
            "-c",
            "core.quotePath=false",
            "worktree",
            "list",
            "--porcelain",
        ],
    )
    .await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    parse_worktrees(&stdout)
}

#[tauri::command]
pub(crate) async fn list_worktrees(path: String) -> Result<Vec<Worktree>, CommandError> {
    list_worktrees_in_path(&path).await
}

#[derive(Debug, Serialize)]
pub(crate) struct WorktreeStatus {
    pub path: String,
    // None marks a worktree whose change count is unknown (the status probe
    // failed); u32::MAX marks one that exceeded the output bound.
    pub changes: Option<u32>,
}

#[tauri::command]
pub(crate) async fn list_worktree_status(
    path: String,
) -> Result<Vec<WorktreeStatus>, CommandError> {
    let path = canonical_path(&path)?;
    let (exit_code, stdout, stderr) = run_git(
        &path,
        &[
            "-c",
            "core.quotePath=false",
            "worktree",
            "list",
            "--porcelain",
        ],
    )
    .await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let mut statuses = Vec::new();
    for worktree in parse_worktrees(&stdout)? {
        let changes = match worktree_change_count(Path::new(&worktree.path)).await {
            Ok(changes) => Some(changes),
            // A count beyond the output bound is itself the signal; other
            // failures leave that worktree's count unknown without sinking
            // the badges of every healthy worktree.
            Err(error) if error.code == "git_output_too_large" => Some(u32::MAX),
            Err(_) => None,
        };
        statuses.push(WorktreeStatus {
            path: worktree.path,
            changes,
        });
    }
    Ok(statuses)
}

async fn worktree_change_count(worktree_path: &Path) -> Result<u32, CommandError> {
    // --no-optional-locks keeps the status probe from taking the index lock
    // or refreshing the index, so the read-only guarantee holds.
    let (exit_code, stdout, stderr) = run_git(
        worktree_path,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--no-renames",
            "--untracked-files=all",
            "-z",
        ],
    )
    .await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    Ok(parse_status_count(&stdout))
}

#[tauri::command]
pub(crate) async fn set_repo_pinned(
    path: String,
    pinned: bool,
    state: tauri::State<'_, AppState>,
) -> Result<Option<i64>, CommandError> {
    set_repo_pinned_in_pool(&state.pool, &path, pinned).await
}

#[tauri::command]
pub(crate) async fn remove_repo(
    path: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), CommandError> {
    remove_repo_in_pool(&state.pool, &path).await
}

#[tauri::command]
pub(crate) async fn get_branch_inventory(
    path: String,
) -> Result<BranchInventory, CommandError> {
    branch_inventory(path).await
}

// Announces a completed refresh or, after `add_repo`, a newly registered
// repository; production emits the `project-refreshed` webview event, tests
// record. Injected so the shared refresh stays testable
// without a running app.
pub(crate) type RefreshSink = Arc<dyn Fn(&str) + Send + Sync>;

// The refresh action's one shared implementation, called by the human IPC
// command and the endpoint's `refresh_repo` method alike: fetch updates only
// remote-tracking refs (and prunes the deleted ones). It never runs as part
// of a review computation; the read-only probes always see whatever state
// the last fetch left behind. Without a configured remote there is nothing
// to fetch and no reason to spawn the network command at all; both outcomes
// announce through the sink.
pub(crate) async fn refresh_repo(path: &Path, refreshes: &RefreshSink) -> Result<(), CommandError> {
    let (exit_code, stdout, stderr) = run_git(path, &["remote"]).await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    if stdout.iter().all(|byte| byte.is_ascii_whitespace()) {
        refreshes(&path.to_string_lossy());
        return Ok(());
    }
    let (exit_code, _, stderr) = fetch_remotes(path).await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    refreshes(&path.to_string_lossy());
    Ok(())
}

#[tauri::command]
pub(crate) async fn fetch_project(
    path: String,
    refreshes: tauri::State<'_, RefreshSink>,
) -> Result<(), CommandError> {
    let path = canonical_path(&path)?;
    refresh_repo(&path, &refreshes).await
}

// The review-content fetch: one remote branch's objects with the configured
// partial-clone filter suspended, so a blobless clone gains the blobs a
// review needs without converting the whole clone. Like the refresh action
// it is user-initiated, never runs as part of a review computation, and
// every subsequent review reads whatever state it left behind.
#[tauri::command]
pub(crate) async fn fetch_review_objects(
    path: String,
    target_ref: String,
) -> Result<(), CommandError> {
    validate_ref(&target_ref, "target_ref")?;
    let path = canonical_path(&path)?;
    let Some((remote, branch)) = remote_branch_of_tracking_ref(&path, &target_ref).await else {
        return Err(CommandError::new(
            "unresolvable_ref",
            format!("'{target_ref}' does not name a remote branch."),
        ));
    };
    validate_fetch_name(&remote, "remote")?;
    validate_fetch_name(&branch, "branch")?;
    fetch_remote_branch(&path, &remote, &branch).await
}

#[tauri::command]
pub(crate) async fn set_surface_pinned(
    path: String,
    kind: String,
    identity_key: String,
    pinned: bool,
    state: tauri::State<'_, AppState>,
) -> Result<Option<i64>, CommandError> {
    set_surface_pinned_in_pool(&state.pool, &path, &kind, &identity_key, pinned).await
}

#[tauri::command]
pub(crate) async fn get_settings(
    state: tauri::State<'_, AppState>,
) -> Result<Settings, CommandError> {
    get_settings_in_pool(&state.pool).await
}

#[tauri::command]
pub(crate) async fn set_settings(
    state: tauri::State<'_, AppState>,
    settings: Settings,
) -> Result<Settings, CommandError> {
    set_settings_in_pool(&state.pool, &settings).await
}

// Marks everything currently in the event log seen and returns the new
// watermark (the cursor the Activity divider and Recent comments read).
#[tauri::command]
pub(crate) async fn mark_activity_seen(
    state: tauri::State<'_, AppState>,
) -> Result<i64, CommandError> {
    mark_activity_seen_in_pool(&state.pool).await
}

#[tauri::command]
pub(crate) async fn list_agent_tokens(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<AgentToken>, CommandError> {
    list_agent_tokens_in_pool(&state.pool).await
}

#[tauri::command]
pub(crate) async fn create_agent_token(
    name: String,
    state: tauri::State<'_, AppState>,
) -> Result<CreatedAgentToken, CommandError> {
    create_agent_token_in_pool(&state.pool, &name).await
}

#[tauri::command]
pub(crate) async fn delete_agent_token(
    id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<(), CommandError> {
    delete_agent_token_in_pool(&state.pool, id).await
}

#[tauri::command]
pub(crate) async fn get_mcp_status(
    status: tauri::State<'_, McpStatusHandle>,
) -> Result<ListenerStatus, CommandError> {
    Ok(status.lock_status().clone())
}

// Applies the stored Agent API settings to the live listener without an
// app restart: the previous listener stops first (freeing its address)
// and a fresh one binds from the settings as persisted right now. The
// returned status is the post-restart view for the Settings section.
#[tauri::command]
pub(crate) async fn restart_mcp(
    state: tauri::State<'_, AppState>,
    deps: tauri::State<'_, TransportDeps>,
    status: tauri::State<'_, McpStatusHandle>,
    listener: tauri::State<'_, ListenerOwner>,
) -> Result<ListenerStatus, CommandError> {
    let settings = get_settings_in_pool(&state.pool).await?;
    let config = ListenerConfig {
        enabled: settings.mcp_enabled,
        address: settings.mcp_listen_address.clone(),
        port: settings.mcp_port,
    };
    restart(state.pool.clone(), &deps, config, (*status).clone(), &listener)
        .await
        .map_err(|message| CommandError::new("transport", message))
}

#[tauri::command]
pub(crate) async fn list_refs(
    path: String,
    worktree_branch: Option<String>,
    target_ref: Option<String>,
) -> Result<RefInventory, CommandError> {
    refs_inventory(path, worktree_branch, target_ref).await
}

#[tauri::command]
pub(crate) async fn list_commits(
    path: String,
    repo_path: String,
    start_ref: Option<String>,
    against: Option<String>,
    skip: Option<u32>,
    limit: Option<u16>,
    state: tauri::State<'_, AppState>,
) -> Result<CommitPage, CommandError> {
    commit_page(
        &state.pool,
        &repo_path,
        path,
        start_ref,
        against,
        skip,
        limit,
    )
    .await
}

#[tauri::command]
pub(crate) async fn list_surfaces(
    path: String,
    state: tauri::State<'_, AppState>,
) -> Result<SurfaceListing, CommandError> {
    list_surfaces_in_pool(&state.pool, &path).await
}

// The cross-repo attention queue: one read-only store pass, no Git runs on
// this path; Git-derived signals fill in from the retrospection store's
// last pass instead of being computed here.
#[tauri::command]
pub(crate) async fn list_attention(
    state: tauri::State<'_, AppState>,
) -> Result<AttentionQueue, CommandError> {
    list_attention_in_pool(&state.pool).await
}

// The Pulse Reviews tab's identity listing: store-only like the attention
// queue, with the backend owning inclusion, state classification, search
// matching, and bounds.
#[tauri::command]
pub(crate) async fn list_portal_reviews(
    repo_path: Option<String>,
    state_filter: Option<String>,
    search: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<PortalReviewRow>, CommandError> {
    list_portal_reviews_in_pool(
        &state.pool,
        &PortalReviewQuery {
            repo_path,
            state: state_filter,
            search,
        },
    )
    .await
}

// The Pulse Threads tab's grouped conversation listing: store-only like the
// reviews listing, with grouping, filters, ordering, and bounds owned by
// the backend.
#[tauri::command]
pub(crate) async fn list_portal_threads(
    repo_path: Option<String>,
    state_filter: Option<String>,
    voice: Option<String>,
    text: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<PortalThreadGroup>, CommandError> {
    list_portal_threads_in_pool(
        &state.pool,
        &PortalThreadQuery {
            repo_path,
            state: state_filter,
            voice,
            text,
        },
    )
    .await
}

// One thread's full conversation by its root comment id, with anchors and
// snippets exactly as stored.
#[tauri::command]
pub(crate) async fn get_portal_thread(
    root_comment_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<PortalThreadDetail, CommandError> {
    get_portal_thread_in_pool(&state.pool, root_comment_id).await
}

// The palette's cross-store search: matched comment bodies, requests, and
// cached commit subjects in one bounded answer.
#[tauri::command]
pub(crate) async fn search_portal(
    needle: String,
    state: tauri::State<'_, AppState>,
) -> Result<PortalSearchMatches, CommandError> {
    search_portal_in_pool(&state.pool, &needle).await
}

// The Activity tab's feed page: the store's event log newest-first, with
// the seen watermark from the same read, so the "new since your last
// visit" divider and the events it splits share one snapshot.
#[derive(Debug, Serialize)]
pub(crate) struct PortalActivityPage {
    pub(crate) events: Vec<crate::events::EventRow>,
    pub(crate) seen_id: i64,
}

#[tauri::command]
pub(crate) async fn list_portal_activity(
    repo_path: Option<String>,
    limit: Option<usize>,
    state: tauri::State<'_, AppState>,
) -> Result<PortalActivityPage, CommandError> {
    let events = crate::events::list_events_in_pool(
        &state.pool,
        &crate::events::EventQuery {
            since_id: 0,
            repo_path,
            limit: limit.unwrap_or(crate::events::EVENT_LIST_LIMIT),
            ascending: false,
        },
    )
    .await?;
    let seen_id = get_settings_in_pool(&state.pool).await?.activity_seen_id;
    Ok(PortalActivityPage { events, seen_id })
}

// The human review-header surface acts as Actor::Human on the shared
// request engine; the MCP face is not involved and gains no human caller.

// Who performs a human request update; a snake_case string on the wire,
// typed routing below.
#[derive(Debug, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequestAction {
    Approve,
    RequestChanges,
    Withdraw,
    ReRequest,
}

// The human create's shared implementation: payload flattening plus the
// Human routing, tested directly; the IPC command adds only typed state.
pub(crate) async fn create_request_as_human(
    pool: &sqlx::SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
    note: String,
    lenses: Option<Vec<String>>,
    reviewers: Option<Vec<String>>,
    max_rounds: Option<i64>,
    head_sha: String,
    notify: &crate::transport::RequestChangeSink,
) -> Result<RequestRow, CommandError> {
    // Same open-repo gate as the agent face: without it a stale or wrong
    // path surfaces as a raw foreign-key error from the INSERT.
    let open: Option<i64> =
        sqlx::query_scalar("SELECT id FROM repos WHERE path = ?").bind(repo_path).fetch_optional(pool).await?;
    if open.is_none() {
        return Err(CommandError::new(
            "invalid_request",
            "No repository with that path is open in WorktreeView.",
        ));
    }
    let draft = RequestDraft {
        // The form has no "absent" state, so an empty box means the human
        // expressed no note: it inserts empty and preserves on a refresh.
        note: if note.trim().is_empty() { None } else { Some(note) },
        lenses: lenses.unwrap_or_default(),
        reviewers: reviewers.unwrap_or_default(),
        max_rounds,
        head_sha,
    };
    let request = create_request_in_pool(
        pool,
        repo_path,
        base_sha,
        target_key,
        target_kind,
        &draft,
        &crate::requests::Actor::Human,
        notify,
    )
    .await?;
    request_row_by_id(pool, request.id).await
}

// The human update's shared implementation: transition routing plus the
// update-time note bound. Only a re-request takes a new head (the engine
// enforces head difference); the optional note refresh rides any action.
pub(crate) async fn update_request_in_pool(
    pool: &sqlx::SqlitePool,
    id: i64,
    action: RequestAction,
    note: Option<String>,
    head_sha: Option<String>,
    notify: &crate::transport::RequestChangeSink,
) -> Result<RequestRow, CommandError> {
    if !matches!(action, RequestAction::ReRequest) && head_sha.is_some() {
        return Err(CommandError::new(
            "invalid_request",
            "Only a re-request takes a new head.",
        ));
    }
    if let Some(note) = &note {
        validate_request_note(note)?;
    }
    let actor = crate::requests::Actor::Human;
    match action {
        RequestAction::Approve => {
            set_request_verdict_in_pool(pool, id, true, &actor, notify).await?;
        }
        RequestAction::RequestChanges => {
            set_request_verdict_in_pool(pool, id, false, &actor, notify).await?;
        }
        RequestAction::Withdraw => {
            withdraw_request_in_pool(pool, id, &actor, notify).await?;
        }
        RequestAction::ReRequest => {
            // An absent head runs into the engine's non-empty check, so
            // the requirement's message lives in one place. The note rides
            // the same guarded UPDATE; an absent note keeps the stored one.
            let head = head_sha.as_deref().unwrap_or("");
            re_request_in_pool(pool, id, head, note.as_deref(), &actor, notify).await?;
        }
    }
    if action != RequestAction::ReRequest {
        if let Some(note) = &note {
            refresh_request_note(pool, id, note).await?;
        }
    }
    request_row_by_id(pool, id).await
}

#[tauri::command]
pub(crate) async fn list_requests(
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<RequestRow>, CommandError> {
    list_requests_in_pool(&state.pool, &repo_path, &base_sha, &target_key, &target_kind).await
}

#[tauri::command]
pub(crate) async fn create_review_request(
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    note: String,
    lenses: Option<Vec<String>>,
    reviewers: Option<Vec<String>>,
    max_rounds: Option<i64>,
    head_sha: String,
    state: tauri::State<'_, AppState>,
    deps: tauri::State<'_, TransportDeps>,
) -> Result<RequestRow, CommandError> {
    create_request_as_human(
        &state.pool,
        &repo_path,
        &base_sha,
        &target_key,
        &target_kind,
        note,
        lenses,
        reviewers,
        max_rounds,
        head_sha,
        &deps.request_changes,
    )
    .await
}

#[tauri::command]
pub(crate) async fn update_review_request(
    id: i64,
    action: RequestAction,
    note: Option<String>,
    head_sha: Option<String>,
    state: tauri::State<'_, AppState>,
    deps: tauri::State<'_, TransportDeps>,
) -> Result<RequestRow, CommandError> {
    update_request_in_pool(
        &state.pool,
        id,
        action,
        note,
        head_sha,
        &deps.request_changes,
    )
    .await
}

#[tauri::command]
pub(crate) async fn list_review_changes(
    path: String,
    repo_path: String,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
    state: tauri::State<'_, AppState>,
) -> Result<ReviewIndex, CommandError> {
    review_changes(
        &state.pool,
        &repo_path,
        path,
        base,
        head_ref,
        committed_only,
        reversed,
    )
    .await
}

#[tauri::command]
pub(crate) async fn read_review_patch(
    path: String,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
) -> Result<FilePatch, CommandError> {
    review_patch(
        path,
        base,
        head_ref,
        committed_only,
        reversed,
        file,
        untracked,
    )
    .await
}

// The reviewed file's content on the patch's new side, for context expansion
// and the full-file view; same review identity arguments as the patch read.
#[tauri::command]
pub(crate) async fn read_review_file(
    path: String,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
) -> Result<FileContent, CommandError> {
    review_file_content(
        path,
        base,
        head_ref,
        committed_only,
        reversed,
        file,
        untracked,
    )
    .await
}

// The reviewed file's raw bytes on the patch's new side: renderable assets
// (images) need them without the binary check or UTF-8 decode. Same review
// identity arguments as the text read.
#[tauri::command]
pub(crate) async fn read_review_file_bytes(
    path: String,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
) -> Result<tauri::ipc::Response, CommandError> {
    let bytes = review_file_bytes(
        path,
        base,
        head_ref,
        committed_only,
        reversed,
        file,
        untracked,
    )
    .await?;
    Ok(tauri::ipc::Response::new(bytes))
}

// One commit's identity by rev (abbreviated hashes included): the ref picker's
// direct hash insert and the commit title/description surfaces both read it.
#[tauri::command]
pub(crate) async fn describe_commit(
    path: String,
    rev: String,
) -> Result<CommitDetail, CommandError> {
    commit_detail(path, rev).await
}

// Handing a reviewed file to the OS shell is user-initiated and must stay
// inside the reviewed worktree: the renderer supplies only a diff-relative
// path, the join is canonicalized, and anything resolving outside the root
// (absolute paths, `..`, symlinks out) is refused before the opener runs.
#[tauri::command]
pub(crate) fn open_review_file(
    worktree_path: String,
    path: String,
    reveal: bool,
) -> Result<(), CommandError> {
    if path.trim().is_empty() {
        return Err(CommandError::new("invalid_path", "Select a non-empty file."));
    }
    let root = Path::new(&worktree_path);
    if !root.is_dir() {
        return Err(CommandError::new(
            "invalid_path",
            "The worktree folder does not exist.",
        ));
    }
    let root = plain_path(&root.canonicalize().map_err(|_| {
        CommandError::new("invalid_path", "The worktree folder could not be resolved.")
    })?);
    let resolved = plain_path(&root.join(&path).canonicalize().map_err(|_| {
        CommandError::new("invalid_path", "The file is no longer in the worktree.")
    })?);
    if !resolved.starts_with(&root) {
        return Err(CommandError::new(
            "invalid_path",
            "The file is outside the worktree.",
        ));
    }
    let opened = if reveal {
        tauri_plugin_opener::reveal_item_in_dir(&resolved)
    } else {
        tauri_plugin_opener::open_path(&resolved, None::<&str>)
    };
    opened.map_err(|error| {
        CommandError::new("open_failed", format!("The file could not be opened: {error}"))
    })
}

// The settings page's log access resolves the same app log dir the log
// plugin writes to, creating it when no log exists yet, and hands it to the
// OS shell.
#[tauri::command]
pub(crate) fn open_log_dir(app: tauri::AppHandle) -> Result<(), CommandError> {
    let dir = app
        .path()
        .app_log_dir()
        .map_err(|_| CommandError::new("log_dir", "The log folder could not be resolved."))?;
    std::fs::create_dir_all(&dir)
        .map_err(|_| CommandError::new("log_dir", "The log folder could not be created."))?;
    tauri_plugin_opener::open_path(&dir, None::<&str>).map_err(|error| {
        CommandError::new("open_failed", format!("The folder could not be opened: {error}"))
    })
}

// Review identity keys on resolved SHAs: the frontend derives base_sha and
// target_key from the loaded ReviewIndex, never from symbolic ref names.
// Human IPC callers act as Actor::Human; ownership is enforced in the
// shared implementations, not here.
#[tauri::command]
pub(crate) async fn create_comment(
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    draft: CommentDraft,
    state: tauri::State<'_, AppState>,
) -> Result<Comment, CommandError> {
    create_comment_in_pool(
        &state.pool,
        &repo_path,
        &base_sha,
        &target_key,
        &target_kind,
        &draft,
        &Actor::Human,
        None,
    )
    .await
}

#[tauri::command]
pub(crate) async fn list_comments(
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<Comment>, CommandError> {
    list_comments_in_pool(&state.pool, &repo_path, &base_sha, &target_key, &target_kind).await
}

#[tauri::command]
pub(crate) async fn list_submissions(
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<Submission>, CommandError> {
    list_submissions_in_pool(&state.pool, &repo_path, &base_sha, &target_key, &target_kind).await
}

#[tauri::command]
pub(crate) async fn reply_comment(
    parent_id: i64,
    body: String,
    severity: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<Comment, CommandError> {
    reply_comment_in_pool(&state.pool, parent_id, &body, severity, &Actor::Human, None).await
}

#[tauri::command]
pub(crate) async fn set_comment_resolved(
    comment_id: i64,
    resolved: bool,
    state: tauri::State<'_, AppState>,
) -> Result<Comment, CommandError> {
    set_comment_resolved_in_pool(&state.pool, comment_id, resolved, &Actor::Human).await
}

#[tauri::command]
pub(crate) async fn edit_comment(
    comment_id: i64,
    body: String,
    state: tauri::State<'_, AppState>,
) -> Result<Comment, CommandError> {
    edit_comment_in_pool(&state.pool, comment_id, &body, &Actor::Human).await
}

#[tauri::command]
pub(crate) async fn delete_comment(
    comment_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<(), CommandError> {
    delete_comment_in_pool(&state.pool, comment_id, &Actor::Human).await
}

#[tauri::command]
pub(crate) async fn match_comment_anchors(
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    file_path: String,
    lines: Vec<PatchLine>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<AnchorStatus>, CommandError> {
    match_comment_anchors_in_pool(
        &state.pool,
        &repo_path,
        &base_sha,
        &target_key,
        &target_kind,
        &file_path,
        &lines,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        create_request_as_human, fetch_remote_branch, refresh_repo, update_request_in_pool,
        worktree_change_count, RequestAction, RefreshSink,
    };
    use crate::agents::create_agent_token_in_pool;
    use crate::overview::branch_inventory;
    use crate::requests::{create_request_in_pool, set_request_verdict_in_pool, Actor as RequestActor, RequestDraft};
    use crate::review::review_changes;
    use crate::testutil::{seed_repo, test_git, test_path, test_pool, test_repo, test_rev_parse};
    use crate::transport::RequestChangeSink;
    use std::process::Command as StdCommand;
    use std::sync::{Arc, Mutex};

    const REPO: &str = "/demo";

    fn noop_notify() -> RequestChangeSink {
        Arc::new(|_| {})
    }

    fn draft(head: &str) -> RequestDraft {
        RequestDraft {
            note: Some("Please review my changes.".into()),
            lenses: Vec::new(),
            reviewers: Vec::new(),
            max_rounds: None,
            head_sha: head.into(),
        }
    }

    // The no-op sink stands in for the app's event emit in the direct calls;
    // fetch_project (the IPC adapter) reads its sink from managed state.
    fn recording_sink() -> (RefreshSink, Arc<Mutex<Vec<String>>>) {
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        (
            Arc::new(move |repo_path: &str| sink.lock().unwrap().push(repo_path.to_string())),
            received,
        )
    }

    #[tokio::test]
    async fn refresh_updates_remote_tracking_refs_and_announces() {
        let origin = test_repo("fetch-origin");
        test_git(&origin, &["branch", "-M", "main"]);
        let clone = test_path("fetch-clone");
        std::fs::create_dir(&clone).unwrap();
        test_git(&clone, &["init", "--quiet", "-b", "main"]);
        test_git(
            &clone,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        let (refreshes, announced) = recording_sink();

        // A repository with no fetched refs yet picks up the remote's
        // branches on the first refresh.
        refresh_repo(&clone, &refreshes).await.unwrap();
        let inventory = branch_inventory(clone.to_str().unwrap().into())
            .await
            .unwrap();
        assert_eq!(inventory.remote_branch_count, 1);
        assert_eq!(
            inventory.remote_branches[0].ref_name,
            "refs/remotes/origin/main"
        );
        assert_eq!(
            announced.lock().unwrap().as_slice(),
            [clone.to_string_lossy().as_ref()]
        );

        // New remote branches and commits appear on the next refresh.
        test_git(&origin, &["checkout", "--quiet", "-b", "side"]);
        std::fs::write(origin.join("tracked.txt"), "changed\n").unwrap();
        test_git(&origin, &["commit", "--quiet", "-am", "advance"]);
        refresh_repo(&clone, &refreshes).await.unwrap();
        let inventory = branch_inventory(clone.to_str().unwrap().into())
            .await
            .unwrap();
        assert_eq!(inventory.remote_branch_count, 2);
        let side = inventory
            .remote_branches
            .iter()
            .find(|branch| branch.ref_name == "refs/remotes/origin/side")
            .unwrap();
        assert_eq!(side.subject, "advance");

        // A repository without remotes fetches nothing and still succeeds,
        // announcing the no-op like any completed refresh.
        let bare = test_repo("fetch-no-remote");
        refresh_repo(&bare, &refreshes).await.unwrap();
        assert_eq!(announced.lock().unwrap().len(), 3);

        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
        let _ = std::fs::remove_dir_all(&bare);
    }

    // The review-content fetch materializes exactly the named branch's blobs
    // in a partial clone: that branch becomes reviewable while another
    // branch's new blob stays unfetched, proving the operation never
    // converts the whole clone.
    #[tokio::test]
    async fn review_fetch_downloads_only_the_named_branchs_blobs() {
        let origin = test_repo("review-fetch-origin");
        test_git(&origin, &["branch", "-M", "main"]);
        test_git(&origin, &["config", "uploadpack.allowFilter", "true"]);
        test_git(&origin, &["checkout", "--quiet", "-b", "side"]);
        std::fs::write(origin.join("side.txt"), "side content\n").unwrap();
        test_git(&origin, &["add", "side.txt"]);
        test_git(&origin, &["commit", "--quiet", "-m", "side work"]);
        test_git(&origin, &["checkout", "--quiet", "main"]);
        std::fs::write(origin.join("other.txt"), "main only\n").unwrap();
        test_git(&origin, &["add", "other.txt"]);
        test_git(&origin, &["commit", "--quiet", "-m", "main work"]);

        let clone = test_path("review-fetch-clone");
        let cloned = StdCommand::new("git")
            .args([
                "clone",
                "--quiet",
                "--no-local",
                "--filter=blob:none",
                "--no-checkout",
            ])
            .arg(origin.to_str().unwrap())
            .arg(clone.to_str().unwrap())
            .output()
            .unwrap();
        assert!(
            cloned.status.success(),
            "{}",
            String::from_utf8_lossy(&cloned.stderr)
        );

        // GIT_NO_LAZY_FETCH keeps the probe itself from backfilling the blob
        // it checks for.
        let blob_on_disk = |revision: &str| {
            let sha = test_rev_parse(&clone, revision);
            StdCommand::new("git")
                .arg("-C")
                .arg(&clone)
                .args(["cat-file", "-e", &sha])
                .env("GIT_NO_LAZY_FETCH", "1")
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(!blob_on_disk("refs/remotes/origin/side:side.txt"));

        // The full review flow on the unfetched branch: the review index
        // classifies the missing blobs as partial-clone content, and the
        // scoped fetch makes the same review succeed.
        let pool = test_pool().await;
        seed_repo(&pool, clone.to_str().unwrap()).await;
        let side_parent = test_rev_parse(&clone, "refs/remotes/origin/side~1");
        let error = review_changes(
            &pool,
            clone.to_str().unwrap(),
            clone.to_str().unwrap().into(),
            side_parent.clone(),
            Some("refs/remotes/origin/side".into()),
            true,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "partial_clone_content");
        fetch_remote_branch(&clone, "origin", "side").await.unwrap();
        assert!(blob_on_disk("refs/remotes/origin/side:side.txt"));
        assert!(!blob_on_disk("refs/remotes/origin/main:other.txt"));
        let index = review_changes(
            &pool,
            clone.to_str().unwrap(),
            clone.to_str().unwrap().into(),
            side_parent,
            Some("refs/remotes/origin/side".into()),
            true,
            false,
        )
        .await
        .unwrap();
        assert!(index.files.iter().any(|file| file.path == "side.txt"));

        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
    }

    #[tokio::test]
    async fn change_count_covers_tracked_and_untracked_content() {
        let repo = test_repo("status-count-main");
        let linked = test_path("status-count-linked");
        std::fs::create_dir(&linked).unwrap();
        test_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature",
                linked.to_str().unwrap(),
            ],
        );

        std::fs::write(repo.join("staged.txt"), "staged\n").unwrap();
        test_git(&repo, &["add", "staged.txt"]);
        std::fs::write(repo.join("notes.txt"), "untracked\n").unwrap();
        std::fs::create_dir(repo.join("new-dir")).unwrap();
        std::fs::write(repo.join("new-dir/a.txt"), "a\n").unwrap();
        std::fs::write(repo.join("new-dir/b.txt"), "b\n").unwrap();
        // The staged rename counts as its split add/delete pair.
        test_git(&repo, &["mv", "tracked.txt", "renamed.txt"]);

        let main_count = worktree_change_count(&repo).await.unwrap();
        assert_eq!(main_count, 6);
        let linked_count = worktree_change_count(&linked).await.unwrap();
        assert_eq!(linked_count, 0);

        let _ = StdCommand::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "remove", linked.to_str().unwrap()])
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }

    // The human header's update: typed routing through the shared
    // transitions with Actor::Human, the update-time note bound, and the
    // re-request-only head rule.
    #[tokio::test]
    async fn human_request_update_routes_transitions_and_refreshes_the_note() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = create_agent_token_in_pool(&pool, "coder-bot")
            .await
            .unwrap()
            .token
            .id;
        let notify = noop_notify();

        let created = create_request_as_human(
            &pool, REPO, "base", "/demo", "worktree", "Please review my changes.".into(), None,
            None, None, "head-1".into(), &notify,
        )
        .await
        .unwrap();
        assert_eq!(created.status, "requested");
        assert_eq!(created.requester, "human");
        assert_eq!(created.max_rounds, 2);

        // A verdict before any reviewer claims is a transition violation.
        let refused = update_request_in_pool(&pool, created.id, RequestAction::Approve, None, None, &notify).await;
        assert_eq!(refused.unwrap_err().code, "invalid_transition");

        crate::requests::claim_request_in_pool(&pool, created.id, &RequestActor::Agent(coder), &notify)
            .await
            .unwrap();

        // A head rides only a re-request, refused before anything runs.
        let refused = update_request_in_pool(&pool, created.id, RequestAction::Withdraw, None, Some("head-2".into()), &notify).await;
        assert_eq!(refused.unwrap_err().code, "invalid_request");

        // Approve with the optional refreshed note.
        let approved = update_request_in_pool(&pool, created.id, RequestAction::Approve, Some("Looks good now.".into()), None, &notify).await.unwrap();
        assert_eq!(approved.status, "approved");
        assert_eq!(approved.note, "Looks good now.");

        // Settled requests cannot be withdrawn.
        let refused = update_request_in_pool(&pool, created.id, RequestAction::Withdraw, None, None, &notify).await;
        assert_eq!(refused.unwrap_err().code, "invalid_transition");

        // The changes_requested round: re-request needs a new head, and
        // the refreshed note rides the re-request.
        let second = create_request_in_pool(&pool, REPO, "base", "/demo", "worktree", &draft("head-2"), &RequestActor::Agent(coder), &notify).await.unwrap();
        crate::requests::claim_request_in_pool(&pool, second.id, &RequestActor::Agent(coder), &notify).await.unwrap();
        set_request_verdict_in_pool(&pool, second.id, false, &RequestActor::Human, &notify).await.unwrap();

        let refused = update_request_in_pool(&pool, second.id, RequestAction::ReRequest, None, None, &notify).await;
        assert_eq!(refused.unwrap_err().code, "invalid_request");
        let refused = update_request_in_pool(&pool, second.id, RequestAction::ReRequest, None, Some("head-2".into()), &notify).await;
        assert_eq!(refused.unwrap_err().code, "invalid_request");
        let restarted = update_request_in_pool(&pool, second.id, RequestAction::ReRequest, Some("New round.".into()), Some("head-3".into()), &notify).await.unwrap();
        assert_eq!(restarted.status, "in_review");
        assert_eq!(restarted.round, 1);
        assert_eq!(restarted.note, "New round.");

        // The note bound refuses before the transition runs.
        let refused = update_request_in_pool(&pool, second.id, RequestAction::Withdraw, Some("a".repeat(2001)), None, &notify).await;
        assert_eq!(refused.unwrap_err().code, "invalid_request");
    }
}
