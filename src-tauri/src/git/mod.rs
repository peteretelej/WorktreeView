mod exec;
mod filters;
mod parse;
mod remote;
mod validate;

use crate::canonical_path;
use crate::CommandError;
use sqlx::SqlitePool;
use std::path::PathBuf;

pub(crate) use exec::{
    acceptable_diff_exit, fetch_remote_branch, fetch_remotes, git_args, git_execution_error,
    run_git, run_git_with_stdin, stdin_git_command, MAX_OUTPUT,
};
#[cfg(test)]
pub(crate) use exec::{read_bounded, spawn_counted};
#[cfg(test)]
pub(crate) use remote::tests::{batch_fake_spawner, hybrid_fake_spawner};
pub(crate) use filters::{
    configured_filter_names, filter_override_args, parse_applicable_filter_paths,
    parse_configured_filter_names, reject_applicable_filters, unsupported_filter_error,
};
pub(crate) use parse::{
    parse_branch_records, parse_commits, parse_name_status, parse_numstat, parse_status_count,
    parse_untracked_paths, parse_worktrees, BranchRecord, BranchSummary, ChangedFile, CommitInfo,
    CommitPage, Worktree,
};
pub(crate) use remote::{
    new_nonce, no_index_diff_fragment, process_spawner, run_remote_batch, run_remote_batch_with,
    run_remote_fetch_with, run_remote_read_with, validate_work_tree, validate_work_tree_with,
    BatchFragment, BatchOutput, RemoteRead, RemoteTarget, Spawner,
};

// Shared allowlisted fragment shapes the plumbing modules compose into
// their read groups.
pub(crate) fn batch_fragment(args: Vec<String>) -> BatchFragment {
    BatchFragment {
        cwd: None,
        args,
        stdin: None,
    }
}

pub(crate) fn batch_fragment_at(cwd: &str, args: Vec<String>) -> BatchFragment {
    BatchFragment {
        cwd: Some(cwd.to_string()),
        args,
        stdin: None,
    }
}

pub(crate) fn worktree_list_fragment() -> BatchFragment {
    batch_fragment(
        [
            "-c",
            "core.quotePath=false",
            "worktree",
            "list",
            "--porcelain",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect(),
    )
}

// A fragment carrying one resolved ref: a nonzero exit or empty output maps
// to the caller's "does not resolve to a commit" signal, exactly as the
// local probe reports it.
fn fragment_output_sha(
    batch: &BatchOutput,
    slot: usize,
    name: &str,
) -> Result<String, CommandError> {
    let unresolvable = || {
        CommandError::new(
            "unresolvable_ref",
            format!("The ref '{name}' does not resolve to a commit."),
        )
    };
    let fragment = batch.ok(slot).map_err(|_| unresolvable())?;
    let sha = String::from_utf8_lossy(&fragment.stdout).trim().to_string();
    if sha.is_empty() {
        return Err(unresolvable());
    }
    Ok(sha)
}

// A resolved-ref fragment read out of a larger batch: same contract as
// `remote_resolve_ref`.
pub(crate) fn fragment_sha(
    batch: &BatchOutput,
    slot: usize,
    name: &str,
) -> Result<String, CommandError> {
    fragment_output_sha(batch, slot, name)
}

pub(crate) use validate::{
    effective_head_ref, ensure_work_tree, partial_clone_failure, primary_branch,
    remote_branch_of_tracking_ref, resolve_empty_tree, resolve_ref, validate_fetch_name,
    validate_file, validate_ref, validate_scope_combination, validate_untracked_combination,
};

// The read target one plumbing group runs against: the repo kind is resolved
// from the store row's remote marker before any path handling, so a remote
// identity never reaches canonical_path and the dispatch decision stays at
// this boundary instead of spreading across the call sites.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReadTarget {
    Local(PathBuf),
    Remote(RemoteTarget),
}

pub(crate) async fn repo_is_remote(pool: &SqlitePool, repo_path: &str) -> Result<bool, CommandError> {
    let remote: Option<i64> = sqlx::query_scalar("SELECT remote FROM repos WHERE path = ?")
        .bind(repo_path)
        .fetch_optional(pool)
        .await?;
    Ok(remote == Some(1))
}

// Repo-scoped plumbing (worktree enumeration, branch inventory, history of
// the repository path itself) resolves against the stored identity.
pub(crate) async fn read_target(
    pool: &SqlitePool,
    repo_path: &str,
) -> Result<ReadTarget, CommandError> {
    if repo_is_remote(pool, repo_path).await? {
        return Ok(ReadTarget::Remote(RemoteTarget::parse_identity(repo_path)?));
    }
    Ok(ReadTarget::Local(canonical_path(repo_path)?))
}

// Worktree-scoped plumbing (reviews, history pages) resolves the kind from
// the review's repo identity, then addresses the caller's worktree path:
// on the host for a remote target, canonicalized locally otherwise.
pub(crate) async fn read_worktree_target(
    pool: &SqlitePool,
    repo_path: &str,
    worktree_path: &str,
) -> Result<ReadTarget, CommandError> {
    if repo_is_remote(pool, repo_path).await? {
        if worktree_path.trim().is_empty() {
            return Err(CommandError::new(
                "invalid_path",
                "Select a non-empty folder.",
            ));
        }
        let mut target = RemoteTarget::parse_identity(repo_path)?;
        // Ref and commit reviews have no worktree of their own: their
        // callers pass the repo identity as the worktree path, and the
        // identity already carries the host-side repository path.
        if worktree_path != repo_path {
            target.path = worktree_path.to_string();
        }
        return Ok(ReadTarget::Remote(target));
    }
    Ok(ReadTarget::Local(canonical_path(worktree_path)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{test_pool, test_repo};

    // The store row's remote marker classifies the target before any path
    // handling: local rows canonicalize an existing folder, and a remote
    // identity routes to the runner even though its host-side path can
    // never exist locally, so canonical_path is never reached with it.
    #[tokio::test]
    async fn store_row_classifies_the_target_before_path_handling() {
        let pool = test_pool().await;
        let repo = test_repo("target-local");
        match read_target(&pool, repo.to_str().unwrap()).await.unwrap() {
            ReadTarget::Local(path) => {
                assert_eq!(path, crate::plain_path(&repo.canonicalize().unwrap()))
            }
            ReadTarget::Remote(_) => panic!("local rows must stay local"),
        }

        let identity = "dev@host.example:/srv/re po";
        sqlx::query(
            "INSERT INTO repos (path, name, remote, last_opened_at, created_at) \
             VALUES (?, 'demo', 1, 1, 1)",
        )
        .bind(identity)
        .execute(&pool)
        .await
        .unwrap();
        match read_target(&pool, identity).await.unwrap() {
            ReadTarget::Remote(target) => assert_eq!(target.identity(), identity),
            ReadTarget::Local(_) => panic!("a remote identity must never be canonicalized"),
        }
        // Worktree-scoped reads address the caller's host-side worktree.
        match read_worktree_target(&pool, identity, "/srv/re po wt")
            .await
            .unwrap()
        {
            ReadTarget::Remote(target) => assert_eq!(target.path, "/srv/re po wt"),
            ReadTarget::Local(_) => panic!("remote worktree reads must stay remote"),
        }
        // A ref or commit review passes the identity as its worktree path:
        // the host-side path stays the identity's own repository path
        // instead of being addressed by the identity string itself.
        match read_worktree_target(&pool, identity, identity).await.unwrap() {
            ReadTarget::Remote(target) => assert_eq!(target.path, "/srv/re po"),
            ReadTarget::Local(_) => panic!("remote ref reads must stay remote"),
        }
        std::fs::remove_dir_all(repo).unwrap();
    }

    // Without a store row and with a real folder, the dispatch keeps
    // today's local builder path.
    #[tokio::test]
    async fn unregistered_paths_stay_local() {
        let pool = test_pool().await;
        let repo = test_repo("target-unregistered");
        match read_worktree_target(&pool, repo.to_str().unwrap(), repo.to_str().unwrap())
            .await
            .unwrap()
        {
            ReadTarget::Local(_) => {}
            ReadTarget::Remote(_) => panic!("unregistered folders are local"),
        }
        std::fs::remove_dir_all(repo).unwrap();
    }
}
