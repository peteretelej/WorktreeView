use crate::agents::{
    create_agent_token_in_pool, list_agent_tokens_in_pool, delete_agent_token_in_pool,
    AgentToken, CreatedAgentToken,
};
use crate::git::{
    batch_fragment, fetch_remote_branch, fetch_remotes, git_args, git_execution_error, new_nonce,
    parse_status_count, parse_worktrees, read_target, read_worktree_target, repo_is_remote,
    remote_branch_of_tracking_ref, run_git, run_remote_batch_with, run_remote_fetch_with,
    run_remote_read_with, validate_fetch_name, validate_file, validate_ref,
    validate_work_tree_with, BatchFragment, CommitPage, ReadTarget, RemoteTarget, Worktree,
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
    get_settings_in_pool, load_repos, mark_activity_seen_in_pool, now_millis,
    open_remote_repo_path, open_repo_path, remove_repo_in_pool, set_repo_pinned_in_pool,
    set_settings_in_pool, Repo, Settings,
};
use crate::transport::{
    restart, ListenerConfig, ListenerOwner, ListenerStatus, McpStatusHandle, TransportDeps,
};
use crate::{canonical_path, plain_path, AppState, CommandError};
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::Manager;

// The backend-owned freshness taxonomy for remote project loads. A local
// load serializes exactly as before; a remote load carries the explicit
// state, the age of the last successful read, and an actionable message on
// failure, so the webview renders badges without inferring from errors.
#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(untagged)]
pub(crate) enum ReadOutcome<T> {
    Local(T),
    Remote(RemoteRead<T>),
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct RemoteRead<T> {
    state: ReadState,
    last_success_age_ms: Option<i64>,
    message: Option<String>,
    data: Option<T>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ReadState {
    Live,
    Offline,
    Stale,
}

// The stale threshold reuses the webview's quiet-interval re-read cadence
// (src/App.tsx OVERVIEW_RELOAD_INTERVAL_MS): a load failing while the last
// successful read is still fresh shows offline; once that read ages past
// one quiet interval, the surface shows stale instead.
const STALE_AFTER_MS: i64 = 20_000;

// Connection-level failure codes: the offline/stale taxonomy, never raw
// error surfaces. Host-side git failures keep their own errors.
fn is_offline_failure(error: &CommandError) -> bool {
    matches!(
        error.code.as_str(),
        "auth_failed" | "unreachable_host" | "remote_spawn" | "remote_timeout" | "remote_framing"
    )
}

// Last successful remote read per stored repo path. Session-scoped by
// design: statelessness makes reconnection free, and a fresh app start
// reports no age until the first read lands.
fn remote_read_successes() -> &'static Mutex<HashMap<String, i64>> {
    static TRACKER: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(HashMap::new()))
}

fn note_remote_read_success(repo_path: &str, now: i64) {
    remote_read_successes()
        .lock()
        .unwrap()
        .insert(repo_path.to_string(), now);
}

fn remote_read_success_age(repo_path: &str, now: i64) -> Option<i64> {
    remote_read_successes()
        .lock()
        .unwrap()
        .get(repo_path)
        .map(|then| (now - *then).max(0))
}

fn local_outcome<T>(result: Result<T, CommandError>) -> Result<ReadOutcome<T>, CommandError> {
    result.map(ReadOutcome::Local)
}

fn remote_outcome<T>(
    repo_path: &str,
    result: Result<T, CommandError>,
) -> Result<ReadOutcome<T>, CommandError> {
    match result {
        Ok(data) => {
            note_remote_read_success(repo_path, now_millis());
            Ok(ReadOutcome::Remote(RemoteRead {
                state: ReadState::Live,
                last_success_age_ms: Some(0),
                message: None,
                data: Some(data),
            }))
        }
        Err(error) if is_offline_failure(&error) => {
            let age = remote_read_success_age(repo_path, now_millis());
            let state = if age.is_some_and(|age| age > STALE_AFTER_MS) {
                ReadState::Stale
            } else {
                ReadState::Offline
            };
            Ok(ReadOutcome::Remote(RemoteRead {
                state,
                last_success_age_ms: age,
                message: Some(error.message),
                data: None,
            }))
        }
        Err(error) => Err(error),
    }
}

#[tauri::command]
pub(crate) async fn open_repo(
    path: String,
    state: tauri::State<'_, AppState>,
) -> Result<Repo, CommandError> {
    open_repo_path(&path, &state.pool).await
}

// The add-project remote form's command: typed target fields in (the
// backend owns normalization), the stored Repo out, or one of the typed
// validation failures (unreachable host, auth, not a work tree).
#[tauri::command]
pub(crate) async fn open_remote_repo(
    user: Option<String>,
    host: String,
    port: Option<u16>,
    path: String,
    state: tauri::State<'_, AppState>,
) -> Result<Repo, CommandError> {
    let target = RemoteTarget::from_parts(user.as_deref(), &host, port, &path)?;
    open_remote_repo_path(&state.pool, &target).await
}

#[tauri::command]
pub(crate) async fn list_repos(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<Repo>, CommandError> {
    load_repos(&state.pool).await
}

// Path-scoped worktree listing behind the list_worktrees command: the MCP
// face's list_review_targets reuses the same inventory the UI shows.
pub(crate) async fn list_worktrees_in_pool(
    pool: &SqlitePool,
    path: &str,
) -> Result<Vec<Worktree>, CommandError> {
    match read_target(pool, path).await? {
        ReadTarget::Local(path) => list_worktrees_local(&path).await,
        ReadTarget::Remote(target) => list_worktrees_remote(&target).await,
    }
}

async fn list_worktrees_local(path: &Path) -> Result<Vec<Worktree>, CommandError> {
    let (exit_code, stdout, stderr) = run_git(
        path,
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

async fn list_worktrees_remote(target: &RemoteTarget) -> Result<Vec<Worktree>, CommandError> {
    list_worktrees_remote_with(&crate::git::process_spawner(), target).await
}

async fn list_worktrees_remote_with(
    spawner: &crate::git::Spawner,
    target: &RemoteTarget,
) -> Result<Vec<Worktree>, CommandError> {
    let output = run_remote_batch_with(spawner, target, &[crate::git::worktree_list_fragment()]).await?;
    parse_worktrees(&output.ok(0)?.stdout)
}

#[tauri::command]
pub(crate) async fn list_worktrees(
    path: String,
    state: tauri::State<'_, AppState>,
) -> Result<ReadOutcome<Vec<Worktree>>, CommandError> {
    let remote = matches!(read_target(&state.pool, &path).await?, ReadTarget::Remote(_));
    wrap_outcome(&path, remote, list_worktrees_in_pool(&state.pool, &path).await)
}

// Local loads serialize exactly as before; remote loads carry the explicit
// offline/stale/live state.
fn wrap_outcome<T>(
    repo_path: &str,
    remote: bool,
    result: Result<T, CommandError>,
) -> Result<ReadOutcome<T>, CommandError> {
    if remote {
        remote_outcome(repo_path, result)
    } else {
        local_outcome(result)
    }
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct WorktreeStatus {
    pub path: String,
    // None marks a worktree whose change count is unknown (the status probe
    // failed); u32::MAX marks one that exceeded the output bound.
    pub changes: Option<u32>,
}

#[tauri::command]
pub(crate) async fn list_worktree_status(
    path: String,
    state: tauri::State<'_, AppState>,
) -> Result<ReadOutcome<Vec<WorktreeStatus>>, CommandError> {
    let remote = matches!(read_target(&state.pool, &path).await?, ReadTarget::Remote(_));
    wrap_outcome(
        &path,
        remote,
        list_worktree_status_in_pool(&state.pool, &path).await,
    )
}

pub(crate) async fn list_worktree_status_in_pool(
    pool: &SqlitePool,
    path: &str,
) -> Result<Vec<WorktreeStatus>, CommandError> {
    match read_target(pool, path).await? {
        ReadTarget::Local(path) => list_worktree_status_local(path).await,
        ReadTarget::Remote(target) => list_worktree_status_remote(&target).await,
    }
}

async fn list_worktree_status_local(path: std::path::PathBuf) -> Result<Vec<WorktreeStatus>, CommandError> {
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

// The worktree statuses batch into one invocation after the enumeration:
// the probes depend on the listed host paths, so the group is two round
// trips regardless of worktree count.
async fn list_worktree_status_remote(
    target: &RemoteTarget,
) -> Result<Vec<WorktreeStatus>, CommandError> {
    list_worktree_status_remote_with(&crate::git::process_spawner(), target).await
}

async fn list_worktree_status_remote_with(
    spawner: &crate::git::Spawner,
    target: &RemoteTarget,
) -> Result<Vec<WorktreeStatus>, CommandError> {
    let listed = run_remote_batch_with(spawner, target, &[crate::git::worktree_list_fragment()]).await?;
    let worktrees = parse_worktrees(&listed.ok(0)?.stdout)?;
    let mut statuses = Vec::with_capacity(worktrees.len());
    if worktrees.is_empty() {
        return Ok(statuses);
    }
    let probes: Vec<BatchFragment> = worktrees
        .iter()
        .map(|worktree| crate::git::batch_fragment_at(&worktree.path, change_count_args()))
        .collect();
    let probed = run_remote_batch_with(spawner, target, &probes).await?;
    for (index, worktree) in worktrees.iter().enumerate() {
        let changes = match probed.ok(index) {
            Ok(fragment) => Some(parse_status_count(&fragment.stdout)),
            // A count beyond the output bound is itself the signal; other
            // failures leave that worktree's count unknown without sinking
            // the badges of every healthy worktree.
            Err(error) if error.code == "git_output_too_large" => Some(u32::MAX),
            Err(_) => None,
        };
        statuses.push(WorktreeStatus {
            path: worktree.path.clone(),
            changes,
        });
    }
    Ok(statuses)
}

fn change_count_args() -> Vec<String> {
    [
        "--no-optional-locks",
        "status",
        "--porcelain=v1",
        "--no-renames",
        "--untracked-files=all",
        "-z",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect()
}

async fn worktree_change_count(worktree_path: &Path) -> Result<u32, CommandError> {
    // --no-optional-locks keeps the status probe from taking the index lock
    // or refreshing the index, so the read-only guarantee holds.
    let args = change_count_args();
    let args = git_args(&args);
    let (exit_code, stdout, stderr) = run_git(worktree_path, &args).await?;
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
    state: tauri::State<'_, AppState>,
) -> Result<ReadOutcome<BranchInventory>, CommandError> {
    let remote = matches!(read_target(&state.pool, &path).await?, ReadTarget::Remote(_));
    let result = branch_inventory(&state.pool, path.clone()).await;
    wrap_outcome(&path, remote, result)
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
// announce through the sink. Remote projects run the same fetch one hop
// away on the host, against the host's own remotes.
pub(crate) async fn refresh_repo(
    pool: &SqlitePool,
    path: &str,
    refreshes: &RefreshSink,
) -> Result<(), CommandError> {
    refresh_repo_with(&crate::git::process_spawner(), pool, path, refreshes).await
}

pub(crate) async fn refresh_repo_with(
    spawner: &crate::git::Spawner,
    pool: &SqlitePool,
    path: &str,
    refreshes: &RefreshSink,
) -> Result<(), CommandError> {
    if repo_is_remote(pool, path).await? {
        let target = RemoteTarget::parse_identity(path)?;
        return refresh_remote_repo_with(spawner, &target, refreshes).await;
    }
    refresh_local_repo(Path::new(path), refreshes).await
}

async fn refresh_local_repo(path: &Path, refreshes: &RefreshSink) -> Result<(), CommandError> {
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

// The remote branch of the shared fetch: the same `remote` listing and the
// same `fetch --all --prune`, executed on the host with the remote-tracking
// refs only. The one-Git-write invariant is unchanged except for running
// one hop away; the sink announces with the stored identity.
async fn refresh_remote_repo_with(
    spawner: &crate::git::Spawner,
    target: &RemoteTarget,
    refreshes: &RefreshSink,
) -> Result<(), CommandError> {
    let listed =
        run_remote_batch_with(spawner, target, &[batch_fragment(vec!["remote".into()])]).await?;
    if listed.ok(0)?.stdout.iter().all(|byte| byte.is_ascii_whitespace()) {
        refreshes(&target.identity());
        return Ok(());
    }
    run_remote_fetch_with(spawner, target).await?;
    refreshes(&target.identity());
    Ok(())
}

#[tauri::command]
pub(crate) async fn fetch_project(
    path: String,
    state: tauri::State<'_, AppState>,
    refreshes: tauri::State<'_, RefreshSink>,
) -> Result<(), CommandError> {
    refresh_repo(&state.pool, &path, &refreshes).await
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
    state: tauri::State<'_, AppState>,
) -> Result<ReadOutcome<RefInventory>, CommandError> {
    let remote = matches!(read_target(&state.pool, &path).await?, ReadTarget::Remote(_));
    let result = refs_inventory(&state.pool, path.clone(), worktree_branch, target_ref).await;
    wrap_outcome(&path, remote, result)
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
) -> Result<ReadOutcome<CommitPage>, CommandError> {
    let remote = matches!(
        read_target(&state.pool, &repo_path).await?,
        ReadTarget::Remote(_)
    );
    let result = commit_page(
        &state.pool,
        &repo_path,
        path,
        start_ref,
        against,
        skip,
        limit,
    )
    .await;
    wrap_outcome(&repo_path, remote, result)
}

#[tauri::command]
pub(crate) async fn list_surfaces(
    path: String,
    state: tauri::State<'_, AppState>,
) -> Result<ReadOutcome<SurfaceListing>, CommandError> {
    let remote = matches!(read_target(&state.pool, &path).await?, ReadTarget::Remote(_));
    wrap_outcome(&path, remote, list_surfaces_in_pool(&state.pool, &path).await)
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
) -> Result<ReadOutcome<ReviewIndex>, CommandError> {
    let remote = matches!(
        read_target(&state.pool, &repo_path).await?,
        ReadTarget::Remote(_)
    );
    let result = review_changes(
        &state.pool,
        &repo_path,
        path,
        base,
        head_ref,
        committed_only,
        reversed,
    )
    .await;
    wrap_outcome(&repo_path, remote, result)
}

#[tauri::command]
pub(crate) async fn read_review_patch(
    path: String,
    repo_path: Option<String>,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
    state: tauri::State<'_, AppState>,
) -> Result<ReadOutcome<FilePatch>, CommandError> {
    let repo_key = repo_path.clone().unwrap_or_else(|| path.clone());
    let remote = matches!(
        read_worktree_target(&state.pool, &repo_key, &path).await?,
        ReadTarget::Remote(_)
    );
    let result = review_patch(
        &state.pool,
        path,
        repo_path,
        base,
        head_ref,
        committed_only,
        reversed,
        file,
        untracked,
    )
    .await;
    if remote {
        remote_outcome(&repo_key, result)
    } else {
        local_outcome(result)
    }
}

// The reviewed file's content on the patch's new side, for context expansion
// and the full-file view; same review identity arguments as the patch read.
#[tauri::command]
pub(crate) async fn read_review_file(
    path: String,
    repo_path: Option<String>,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
    state: tauri::State<'_, AppState>,
) -> Result<ReadOutcome<FileContent>, CommandError> {
    let repo_key = repo_path.clone().unwrap_or_else(|| path.clone());
    let remote = matches!(
        read_worktree_target(&state.pool, &repo_key, &path).await?,
        ReadTarget::Remote(_)
    );
    let result = review_file_content(
        &state.pool,
        path,
        repo_path,
        base,
        head_ref,
        committed_only,
        reversed,
        file,
        untracked,
    )
    .await;
    if remote {
        remote_outcome(&repo_key, result)
    } else {
        local_outcome(result)
    }
}

// The reviewed file's raw bytes on the patch's new side: renderable assets
// (images) need them without the binary check or UTF-8 decode. Same review
// identity arguments as the text read. Raw bytes cannot carry the taxonomy,
// so a connection failure surfaces as its typed offline error.
#[tauri::command]
pub(crate) async fn read_review_file_bytes(
    path: String,
    repo_path: Option<String>,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
    state: tauri::State<'_, AppState>,
) -> Result<tauri::ipc::Response, CommandError> {
    let bytes = review_file_bytes(
        &state.pool,
        path,
        repo_path,
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
    state: tauri::State<'_, AppState>,
) -> Result<ReadOutcome<CommitDetail>, CommandError> {
    let remote = matches!(read_target(&state.pool, &path).await?, ReadTarget::Remote(_));
    let result = commit_detail(&state.pool, path.clone(), rev).await;
    wrap_outcome(&path, remote, result)
}

// Handing a reviewed file to the OS shell is user-initiated and must stay
// inside the reviewed worktree: the renderer supplies only a diff-relative
// path, the command joins the app-known worktree root, and anything
// resolving outside that root (absolute paths, `..`, symlinks out) is
// refused before the opener runs. Remote projects resolve on the app side:
// the file's content is fetched under the same bounds, copied to a local
// temp file, and that copy is what the OS opens.
#[tauri::command]
pub(crate) async fn open_review_file(
    worktree_path: String,
    path: String,
    reveal: bool,
    repo_path: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<(), CommandError> {
    if path.trim().is_empty() {
        return Err(CommandError::new("invalid_path", "Select a non-empty file."));
    }
    let target = match repo_path.as_deref() {
        Some(repo) => Some(read_worktree_target(&state.pool, repo, &worktree_path).await?),
        None => None,
    };
    match target {
        Some(ReadTarget::Remote(target)) => {
            let temp = copy_review_file_to_temp(&target, &path).await?;
            open_with_os(&temp, reveal)
        }
        _ => open_local_review_file(&worktree_path, &path, reveal),
    }
}

fn open_local_review_file(
    worktree_path: &str,
    path: &str,
    reveal: bool,
) -> Result<(), CommandError> {
    let root = Path::new(worktree_path);
    if !root.is_dir() {
        return Err(CommandError::new(
            "invalid_path",
            "The worktree folder does not exist.",
        ));
    }
    let root = plain_path(&root.canonicalize().map_err(|_| {
        CommandError::new("invalid_path", "The worktree folder could not be resolved.")
    })?);
    let resolved = plain_path(&root.join(path).canonicalize().map_err(|_| {
        CommandError::new("invalid_path", "The file is no longer in the worktree.")
    })?);
    if !resolved.starts_with(&root) {
        return Err(CommandError::new(
            "invalid_path",
            "The file is outside the worktree.",
        ));
    }
    open_with_os(&resolved, reveal)
}

fn open_with_os(path: &Path, reveal: bool) -> Result<(), CommandError> {
    let opened = if reveal {
        tauri_plugin_opener::reveal_item_in_dir(path)
    } else {
        tauri_plugin_opener::open_path(path, None::<&str>)
    };
    opened.map_err(|error| {
        CommandError::new("open_failed", format!("The file could not be opened: {error}"))
    })
}

// The remote copy-to-temp: the reviewed worktree file is fetched under the
// same content bounds as every other remote read, after the same worktree
// pin the local open's containment check enforces, and written to a fresh
// nonce-named folder under the app's temp location, named after the file.
// The renderer never supplies a resolved path; every fetch failure surfaces
// as its typed error for the frontend to hide the action with.
async fn copy_review_file_to_temp(
    target: &RemoteTarget,
    file: &str,
) -> Result<PathBuf, CommandError> {
    copy_review_file_to_temp_with(&crate::git::process_spawner(), target, file).await
}

async fn copy_review_file_to_temp_with(
    spawner: &crate::git::Spawner,
    target: &RemoteTarget,
    file: &str,
) -> Result<PathBuf, CommandError> {
    validate_file(file)?;
    validate_work_tree_with(spawner, target).await?;
    let bytes = match run_remote_read_with(spawner, target, file).await? {
        crate::git::RemoteRead::File(bytes) => bytes,
        crate::git::RemoteRead::Missing => {
            return Err(CommandError::new(
                "invalid_path",
                "The file is no longer in the worktree.",
            ))
        }
    };
    let name = file.rsplit('/').next().unwrap_or(file);
    let dir = std::env::temp_dir()
        .join(format!("worktreeview-open-{}", new_nonce()?));
    std::fs::create_dir_all(&dir).map_err(|_| {
        CommandError::new("open_failed", "The temporary folder could not be created.")
    })?;
    let temp = dir.join(name);
    std::fs::write(&temp, bytes).map_err(|_| {
        CommandError::new("open_failed", "The temporary copy could not be written.")
    })?;
    Ok(temp)
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
        copy_review_file_to_temp_with, create_request_as_human, fetch_remote_branch,
        is_offline_failure, list_worktrees_in_pool, list_worktrees_remote_with,
        list_worktree_status_remote_with, local_outcome, note_remote_read_success, read_target,
        refresh_repo, refresh_repo_with, remote_outcome, update_request_in_pool,
        worktree_change_count, CommandError, ReadOutcome, ReadState, ReadTarget, RemoteRead,
        RemoteTarget, RequestAction, RefreshSink, Worktree, WorktreeStatus, STALE_AFTER_MS,
    };
    use crate::git::batch_fake_spawner;
    use crate::store::now_millis;
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
        let pool = test_pool().await;
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
        refresh_repo(&pool, clone.to_str().unwrap(), &refreshes)
            .await
            .unwrap();
        let inventory = branch_inventory(&pool, clone.to_str().unwrap().into())
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
        refresh_repo(&pool, clone.to_str().unwrap(), &refreshes)
            .await
            .unwrap();
        let inventory = branch_inventory(&pool, clone.to_str().unwrap().into())
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
        refresh_repo(&pool, bare.to_str().unwrap(), &refreshes)
            .await
            .unwrap();
        assert_eq!(announced.lock().unwrap().len(), 3);

        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
        let _ = std::fs::remove_dir_all(&bare);
    }

    // The review-content fetch materializes exactly the named branch's
    // objects in a partial clone: that branch becomes reviewable while
    // another branch's new blob stays unfetched, proving the operation
    // never converts the whole clone. Blobless (blob:none) and treeless
    // (tree:0) clones both go through the same recovery.
    async fn review_fetch_recovers_a_filtered_clone(filter: &str) {
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

        // Blob SHAs come from the origin: a treeless clone cannot resolve
        // `<rev>:<path>` itself because it lacks the tree.
        let side_blob = test_rev_parse(&origin, "side:side.txt");
        let main_blob = test_rev_parse(&origin, "main:other.txt");

        let clone = test_path("review-fetch-clone");
        let cloned = StdCommand::new("git")
            .args(["clone", "--quiet", "--no-local", filter, "--no-checkout"])
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
        let blob_on_disk = |sha: &str| {
            StdCommand::new("git")
                .arg("-C")
                .arg(&clone)
                .args(["cat-file", "-e", sha])
                .env("GIT_NO_LAZY_FETCH", "1")
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(!blob_on_disk(&side_blob));

        // The full review flow on the unfetched branch: the review index
        // classifies the missing content as partial-clone content, and the
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
        assert!(blob_on_disk(&side_blob));
        assert!(!blob_on_disk(&main_blob));
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
    async fn review_fetch_downloads_only_the_named_branchs_blobs() {
        review_fetch_recovers_a_filtered_clone("--filter=blob:none").await;
    }

    #[tokio::test]
    async fn review_fetch_recovers_treeless_clones() {
        review_fetch_recovers_a_filtered_clone("--filter=tree:0").await;
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

    #[tokio::test]
    async fn worktree_listing_routes_local_and_remote_targets() {
        let pool = test_pool().await;
        // A local target keeps the hardened local builder end to end.
        let repo = test_repo("routing-local");
        let worktrees = list_worktrees_in_pool(&pool, repo.to_str().unwrap())
            .await
            .unwrap();
        let expected = crate::plain_path(&repo.canonicalize().unwrap());
        assert!(worktrees
            .iter()
            .any(|worktree| std::path::Path::new(&worktree.path) == expected.as_path()));

        // A remote row routes its identity through the runner even though
        // the host-side path cannot exist locally: canonical_path is never
        // reached with it.
        let identity = "dev@host.example:/srv/re po";
        sqlx::query(
            "INSERT INTO repos (path, name, remote, last_opened_at, created_at) \
             VALUES (?, 'demo', 1, 1, 1)",
        )
        .bind(identity)
        .execute(&pool)
        .await
        .unwrap();
        let target = match read_target(&pool, identity).await.unwrap() {
            ReadTarget::Remote(target) => target,
            ReadTarget::Local(_) => panic!("remote identity must route to the runner"),
        };
        let (spawner, scripts) = batch_fake_spawner(vec![vec![(
            0,
            b"worktree /srv/main\nHEAD abc\nbranch refs/heads/main\n\n".to_vec(),
        )]]);
        let worktrees = list_worktrees_remote_with(&spawner, &target).await.unwrap();
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].path, "/srv/main");
        assert_eq!(scripts.lock().unwrap().len(), 1);
        std::fs::remove_dir_all(repo).unwrap();
    }

    // The per-worktree status probes batch into one invocation after the
    // enumeration; failing probes leave that worktree's count unknown
    // without sinking the badges of healthy worktrees.
    #[tokio::test]
    async fn worktree_statuses_batch_into_enumeration_plus_probes() {
        let (spawner, scripts) = batch_fake_spawner(vec![
            vec![(
                0,
                b"worktree /srv/main\nHEAD a\nbranch refs/heads/main\n\nworktree /srv/fe ature\nHEAD b\ndetached\n\n".to_vec(),
            )],
            vec![(0, Vec::new()), (1, Vec::new())],
        ]);
        let target =
            RemoteTarget::from_parts(Some("dev"), "host.example", None, "/srv/repo").unwrap();
        let statuses = list_worktree_status_remote_with(&spawner, &target)
            .await
            .unwrap();
        // Two round trips regardless of worktree count, each probe its own
        // frame with the host path quoted.
        assert_eq!(scripts.lock().unwrap().len(), 2);
        let probes = scripts.lock().unwrap()[1].clone();
        assert_eq!(probes.matches("-begin").count(), 2);
        assert!(probes.contains("'/srv/fe ature'"));
        assert!(probes.contains("--no-optional-locks"));
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].path, "/srv/main");
        assert_eq!(statuses[0].changes, Some(0));
        assert_eq!(statuses[1].changes, None);
    }

    // The backend-owned taxonomy: connection-level failures carry the
    // explicit offline/stale state with the actionable message, host-side
    // git failures stay error surfaces, and local loads keep bare payloads.
    #[test]
    fn remote_load_failures_map_to_the_offline_stale_taxonomy() {
        let repo = "/taxonomy-demo";
        let error = CommandError::new("unreachable_host", "The remote host could not be reached.");
        match remote_outcome::<Vec<Worktree>>(repo, Err(error)).unwrap() {
            ReadOutcome::Remote(read) => {
                assert!(matches!(read.state, ReadState::Offline));
                assert!(read.data.is_none());
                assert!(read.message.unwrap().contains("The remote host could not be reached."));
            }
            ReadOutcome::Local(_) => panic!("remote loads must carry the envelope"),
        }

        // A last success older than the quiet-interval cadence flips the
        // state to stale.
        note_remote_read_success(repo, now_millis() - (STALE_AFTER_MS + 5_000));
        let error = CommandError::new("remote_timeout", "The remote host did not respond in time.");
        match remote_outcome::<Vec<Worktree>>(repo, Err(error)).unwrap() {
            ReadOutcome::Remote(read) => {
                assert!(matches!(read.state, ReadState::Stale));
                assert!(read.last_success_age_ms.unwrap() > STALE_AFTER_MS);
            }
            ReadOutcome::Local(_) => panic!("remote loads must carry the envelope"),
        }

        // Host-side git failures are never mapped onto offline/stale.
        let error = CommandError::new("git_execution", "fatal: bad object");
        assert!(remote_outcome::<Vec<Worktree>>(repo, Err(error)).is_err());

        // The connection-level code set is exactly the offline taxonomy.
        for code in [
            "auth_failed",
            "unreachable_host",
            "remote_spawn",
            "remote_timeout",
            "remote_framing",
        ] {
            assert!(is_offline_failure(&CommandError::new(code, "x")), "{code}");
        }
        for code in ["git_execution", "git_timeout", "unresolvable_ref"] {
            assert!(!is_offline_failure(&CommandError::new(code, "x")), "{code}");
        }

        // Local loads keep their payloads bare.
        assert!(matches!(
            local_outcome::<Vec<Worktree>>(Ok(Vec::new())).unwrap(),
            ReadOutcome::Local(_)
        ));
    }

    // Serialization contract: a local payload is byte-for-byte today's JSON
    // and a remote payload carries the explicit state and age.
    #[test]
    fn local_payloads_serialize_bare_and_remote_payloads_carry_state() {
        let payload = vec![WorktreeStatus {
            path: "/w".into(),
            changes: Some(0),
        }];
        let local = serde_json::to_string(&ReadOutcome::Local(payload.clone())).unwrap();
        assert_eq!(local, serde_json::to_string(&payload).unwrap());
        let remote = serde_json::to_string(&ReadOutcome::Remote(RemoteRead {
            state: ReadState::Live,
            last_success_age_ms: Some(0),
            message: None,
            data: Some(payload),
        }))
        .unwrap();
        assert!(remote.contains("\"state\":\"live\""));
        assert!(remote.contains("\"last_success_age_ms\":0"));
    }

    async fn remote_row(pool: &sqlx::SqlitePool, identity: &str) {
        sqlx::query(
            "INSERT INTO repos (path, name, remote, last_opened_at, created_at) \
             VALUES (?, 'demo', 1, 1, 1)",
        )
        .bind(identity)
        .execute(pool)
        .await
        .unwrap();
    }

    // The shared fetch path routes remote projects through the runner: one
    // batched `remote` listing, then the exact local fetch argv executed on
    // the host. Both the IPC command and the endpoint method call this same
    // refresh_repo, so the routing holds for either caller.
    #[tokio::test]
    async fn refresh_repo_routes_remote_projects_through_the_runner() {
        let pool = test_pool().await;
        let identity = "dev@host.example:/srv/re po";
        remote_row(&pool, identity).await;
        let (spawner, scripts) = crate::git::hybrid_fake_spawner(
            vec![vec![(0, b"origin\n".to_vec())]],
            vec![(0, Vec::new(), Vec::new())],
        );
        let (refreshes, announced) = recording_sink();
        refresh_repo_with(&spawner, &pool, identity, &refreshes)
            .await
            .unwrap();
        // The guard is scoped: a held `scripts` lock relocks below.
        let (listing, fetch) = {
            let recorded = scripts.lock().unwrap();
            assert_eq!(recorded.len(), 2);
            (recorded[0].clone(), recorded[1].clone())
        };
        assert!(listing.contains("-begin"));
        assert!(listing.contains("remote"));
        assert!(fetch.contains("'fetch' '--all' '--prune' '--quiet'"));
        assert!(fetch.contains("'/srv/re po'"));
        assert_eq!(
            announced.lock().unwrap().as_slice(),
            [identity]
        );
    }

    // A remote project without remotes fetches nothing: the no-op announces
    // exactly as the local one does, and no fetch spawns.
    #[tokio::test]
    async fn refresh_repo_noops_for_a_remote_project_without_remotes() {
        let pool = test_pool().await;
        let identity = "dev@host.example:/srv/repo";
        remote_row(&pool, identity).await;
        let (spawner, scripts) =
            crate::git::hybrid_fake_spawner(vec![vec![(0, Vec::new())]], vec![]);
        let (refreshes, announced) = recording_sink();
        refresh_repo_with(&spawner, &pool, identity, &refreshes)
            .await
            .unwrap();
        assert_eq!(scripts.lock().unwrap().len(), 1);
        assert_eq!(announced.lock().unwrap().as_slice(), [identity]);
    }

    // The remote OS-open copies the reviewed file to a fresh temp folder
    // under the app's temp location, after the same worktree pin the local
    // open enforces; a missing file is the typed failure the frontend hides
    // the action with.
    #[tokio::test]
    async fn remote_open_copies_the_reviewed_file_to_a_temp_copy() {
        let bytes = b"file\x00bytes".to_vec();
        let (spawner, scripts) = crate::git::hybrid_fake_spawner(
            vec![],
            vec![
                (0, b"true\n".to_vec(), Vec::new()),
                (0, bytes.clone(), Vec::new()),
            ],
        );
        let target = RemoteTarget::from_parts(Some("dev"), "host.example", None, "/srv/repo")
            .unwrap();
        let temp = copy_review_file_to_temp_with(&spawner, &target, "src/doc.txt")
            .await
            .unwrap();
        assert_eq!(temp.file_name().unwrap(), "doc.txt");
        assert!(std::fs::read(&temp).unwrap() == bytes);
        assert!(temp.starts_with(std::env::temp_dir()));
        assert_eq!(scripts.lock().unwrap().len(), 2);
        assert!(scripts.lock().unwrap()[0].contains("'rev-parse' '--is-inside-work-tree'"));
        assert!(scripts.lock().unwrap()[1].contains("cat '/srv/repo/src/doc.txt'"));
        std::fs::remove_dir_all(temp.parent().unwrap()).unwrap();

        let (spawner, _scripts) = crate::git::hybrid_fake_spawner(
            vec![],
            vec![
                (0, b"true\n".to_vec(), Vec::new()),
                (
                    1,
                    Vec::new(),
                    b"cat: /srv/repo/gone.txt: No such file or directory".to_vec(),
                ),
            ],
        );
        let error = copy_review_file_to_temp_with(&spawner, &target, "gone.txt")
            .await
            .unwrap_err();
        assert_eq!(error.code, "invalid_path");
    }
}
