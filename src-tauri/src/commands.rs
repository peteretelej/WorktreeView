use crate::git::{git_execution_error, parse_worktrees, run_git, CommitPage, Worktree};
use crate::review::{
    commit_page, refs_inventory, review_changes, review_patch, FilePatch, RefInventory,
    ReviewIndex,
};
use crate::retrospection::{list_surfaces_in_pool, SurfaceListing};
use crate::store::{
    get_settings_in_pool, load_repos, open_repo_path, set_repo_pinned_in_pool,
    set_settings_in_pool, Repo, Settings,
};
use crate::{canonical_path, AppState, CommandError};

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

#[tauri::command]
pub(crate) async fn list_worktrees(path: String) -> Result<Vec<Worktree>, CommandError> {
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
    parse_worktrees(&stdout)
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
pub(crate) async fn list_refs(
    path: String,
    worktree_branch: Option<String>,
) -> Result<RefInventory, CommandError> {
    refs_inventory(path, worktree_branch).await
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
