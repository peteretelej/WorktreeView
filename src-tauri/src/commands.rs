use crate::git::{
    git_execution_error, parse_status_count, parse_worktrees, run_git, CommitPage, Worktree,
};
use crate::review::{
    commit_page, refs_inventory, review_changes, review_patch, FilePatch, RefInventory,
    ReviewIndex,
};
use crate::retrospection::{list_surfaces_in_pool, set_surface_pinned_in_pool, SurfaceListing};
use crate::store::{
    get_settings_in_pool, load_repos, open_repo_path, set_repo_pinned_in_pool,
    set_settings_in_pool, Repo, Settings,
};
use crate::{canonical_path, AppState, CommandError};
use serde::Serialize;
use std::path::Path;

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

#[cfg(test)]
mod tests {
    use super::worktree_change_count;
    use crate::testutil::{test_git, test_path, test_repo};
    use std::process::Command as StdCommand;

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
