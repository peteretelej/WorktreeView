use crate::agents::{
    create_agent_token_in_pool, list_agent_tokens_in_pool, delete_agent_token_in_pool,
    AgentToken, CreatedAgentToken,
};
use crate::git::{
    fetch_remotes, git_execution_error, parse_status_count, parse_worktrees, run_git, CommitPage,
    Worktree,
};
use crate::overview::{branch_inventory, BranchInventory};
use crate::review::{
    commit_detail, commit_page, refs_inventory, review_changes, review_file_bytes,
    review_file_content, review_patch, CommitDetail, FileContent, FilePatch, RefInventory,
    ReviewIndex,
};
use crate::retrospection::{list_surfaces_in_pool, set_surface_pinned_in_pool, SurfaceListing};
use crate::reviews::{
    create_comment_in_pool, delete_comment_in_pool, edit_comment_in_pool, list_comments_in_pool,
    list_submissions_in_pool, match_comment_anchors_in_pool, reply_comment_in_pool,
    set_comment_resolved_in_pool, Actor, AnchorStatus, Comment, CommentDraft, PatchLine,
    Submission,
};
use crate::store::{
    get_settings_in_pool, load_repos, open_repo_path, remove_repo_in_pool, set_repo_pinned_in_pool,
    set_settings_in_pool, Repo, Settings,
};
use crate::transport::{ListenerStatus, McpStatusHandle};
use crate::{canonical_path, plain_path, AppState, CommandError};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

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
    use super::{refresh_repo, worktree_change_count, RefreshSink};
    use crate::overview::branch_inventory;
    use crate::testutil::{test_git, test_path, test_repo};
    use std::process::Command as StdCommand;
    use std::sync::{Arc, Mutex};

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
}
