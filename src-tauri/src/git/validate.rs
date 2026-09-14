use super::exec::{
    git_args, git_execution_error, repo_stdin_git_command, run_git, run_git_with_env,
    run_git_with_stdin,
};
use crate::CommandError;
use std::path::Path;

// Git localizes diagnostics via gettext, so only invocations whose stderr is
// machine-parsed run in the C locale; everything else keeps the user's
// language for human-readable errors.
const DIAGNOSTIC_ENV: &[(&str, &str)] = &[("LC_ALL", "C"), ("LANG", "C")];

pub(crate) fn validate_ref(value: &str, name: &str) -> Result<(), CommandError> {
    if value.starts_with('-') {
        return Err(CommandError::new(
            "invalid_path",
            format!("The {name} must not start with '-'."),
        ));
    }
    Ok(())
}

// Fetch positionals must stay positionals: nothing option-shaped, no refspec
// separator or force marker, and only refname-safe characters, so a hostile
// repository cannot turn its own configured remote name into fetch options
// or smuggle a `src:dst` refspec that rewrites local branches.
pub(crate) fn validate_fetch_name(value: &str, name: &str) -> Result<(), CommandError> {
    validate_ref(value, name)?;
    let unsafe_name = value.starts_with('+')
        || value.contains(':')
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control());
    if unsafe_name {
        return Err(CommandError::new(
            "invalid_path",
            format!("The {name} contains characters that are not valid in a ref name."),
        ));
    }
    Ok(())
}

pub(crate) fn validate_file(value: &str) -> Result<(), CommandError> {
    let path = Path::new(value);
    if value.is_empty()
        || value.starts_with('-')
        || !path.is_relative()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(CommandError::new(
            "invalid_path",
            "The file must be a relative path without '..' segments.",
        ));
    }
    Ok(())
}

pub(crate) fn validate_untracked_combination(
    untracked: bool,
    committed_only: bool,
    reversed: bool,
) -> Result<(), CommandError> {
    if untracked && (committed_only || reversed) {
        return Err(CommandError::new(
            "git_execution",
            "Untracked files cannot be used with committed-only or reversed review data.",
        ));
    }
    Ok(())
}

pub(crate) async fn ensure_work_tree(canonical: &Path) -> Result<(), CommandError> {
    let (exit_code, stdout, stderr) = run_git_with_env(
        canonical,
        &["rev-parse", "--is-inside-work-tree"],
        DIAGNOSTIC_ENV,
    )
    .await?;
    if exit_code != 0 && !is_not_a_repository_diagnostic(&stderr) {
        return Err(git_execution_error(&stderr));
    }
    if exit_code != 0 || std::str::from_utf8(&stdout).map(|value| value.trim()) != Ok("true") {
        return Err(CommandError::new(
            "not_git_repository",
            "The selected folder is not a Git repository and was not added.",
        ));
    }
    Ok(())
}

fn is_not_a_repository_diagnostic(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr)
        .to_ascii_lowercase()
        .contains("not a git repository")
}

pub(crate) fn validate_scope_combination(
    base: &str,
    effective_head_ref: &str,
    committed_only: bool,
) -> Result<(), CommandError> {
    if !committed_only && effective_head_ref != "HEAD" {
        return Err(CommandError::new(
            "scope_requires_worktree",
            "All changes scope requires the worktree's checked-out state.",
        ));
    }
    if !committed_only && base == "empty-tree" {
        return Err(CommandError::new(
            "unresolvable_ref",
            "The empty-tree base is only valid for committed-only reviews.",
        ));
    }
    Ok(())
}

pub(crate) fn effective_head_ref(head_ref: Option<String>) -> String {
    head_ref
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "HEAD".into())
}

pub(crate) async fn resolve_ref(path: &Path, name: &str) -> Result<String, CommandError> {
    let commit = format!("{name}^{{commit}}");
    let owned_args = vec![
        "rev-parse".into(),
        "--verify".into(),
        "--quiet".into(),
        commit,
    ];
    let args = git_args(&owned_args);
    let (exit_code, stdout, _) = run_git(path, &args).await?;
    if exit_code != 0 || stdout.is_empty() {
        return Err(CommandError::new(
            "unresolvable_ref",
            format!("The ref '{name}' does not resolve to a commit."),
        ));
    }
    let value = std::str::from_utf8(&stdout)
        .map_err(|_| CommandError::new("git_output_malformed", "Git returned invalid ref data."))?
        .trim();
    if value.is_empty() {
        return Err(CommandError::new(
            "unresolvable_ref",
            format!("The ref '{name}' does not resolve to a commit."),
        ));
    }
    Ok(value.to_string())
}

// The repository's empty tree object: parentless commits diff against it. The
// hash is computed inside the repository (not from a temp directory) so it
// follows the repository's object format, and without `-w` nothing is written.
pub(crate) async fn resolve_empty_tree(path: &Path) -> Result<String, CommandError> {
    let (exit_code, stdout, stderr) = run_git_with_stdin(
        repo_stdin_git_command(path, &["hash-object", "-t", "tree", "--stdin"]),
        Vec::new(),
    )
    .await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let sha = std::str::from_utf8(&stdout).map_err(|_| {
        CommandError::new(
            "git_output_malformed",
            "Git returned an invalid empty-tree hash.",
        )
    })?;
    let sha = sha.trim();
    if sha.is_empty() {
        return Err(CommandError::new(
            "git_output_malformed",
            "Git returned no empty-tree hash.",
        ));
    }
    Ok(sha.to_string())
}

pub(crate) fn primary_branch(branches: &[String]) -> Option<&str> {
    if branches.iter().any(|branch| branch == "refs/heads/main") {
        Some("refs/heads/main")
    } else if branches.iter().any(|branch| branch == "refs/heads/master") {
        Some("refs/heads/master")
    } else {
        None
    }
}

// A partial clone stores no blob content until something fetches it, and
// review reads refuse lazy fetches, so a diff over unfetched content fails
// with a distinctive promisor diagnostic. Git localizes that stderr, so the
// failing diff is re-run once in the C locale before matching, and the
// repository must also actually configure a promisor remote, so an
// unrelated failure never masquerades as one.
pub(crate) async fn partial_clone_failure(path: &Path, failing_args: &[&str]) -> Option<CommandError> {
    let (exit_code, stdout, _) = run_git_with_env(
        path,
        &["config", "--get-regexp", r"^remote\..*\.promisor$"],
        DIAGNOSTIC_ENV,
    )
    .await
    .ok()?;
    if exit_code != 0 {
        return None;
    }
    let configured = std::str::from_utf8(&stdout).ok()?;
    let is_promisor = configured
        .lines()
        .any(|line| line.split_whitespace().last() == Some("true"));
    if !is_promisor {
        return None;
    }
    let (exit_code, _, rerun_stderr) = run_git_with_env(path, failing_args, DIAGNOSTIC_ENV).await.ok()?;
    if exit_code == 0 || !is_promisor_failure(&rerun_stderr) {
        return None;
    }
    Some(CommandError::new(
        "partial_clone_content",
        "This repository is a partial clone, and the file content for this review is not stored on disk. Fetch the branch content to continue.",
    ))
}

fn is_promisor_failure(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    text.contains("from promisor remote")
        || (text.contains("promisor") && text.contains("could not fetch"))
}

// Splits a remote-tracking ref ("refs/remotes/origin/feature") into its
// remote and the branch on that remote. Remote names may contain slashes,
// so the configured remotes disambiguate by longest match; a name no remote
// claims yields None and the caller keeps the generic failure.
pub(crate) async fn remote_branch_of_tracking_ref(
    path: &Path,
    ref_name: &str,
) -> Option<(String, String)> {
    let remainder = ref_name.strip_prefix("refs/remotes/")?;
    let (exit_code, stdout, _) = run_git_with_env(path, &["remote"], DIAGNOSTIC_ENV)
        .await
        .ok()?;
    if exit_code != 0 {
        return None;
    }
    let listing = std::str::from_utf8(&stdout).ok()?;
    let mut remotes: Vec<&str> = listing
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    remotes.sort_by_key(|name| std::cmp::Reverse(name.len()));
    let remote = remotes
        .into_iter()
        .find(|name| remainder.starts_with(&format!("{name}/")))?;
    let branch = &remainder[remote.len() + 1..];
    if branch.is_empty() {
        return None;
    }
    Some((remote.to_string(), branch.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::review_changes;
    use crate::testutil::{test_pool, test_repo, test_git};

    #[test]
    fn validates_review_arguments() {
        assert!(validate_ref("-main", "base").is_err());
        assert!(validate_ref("-branch", "worktree_branch").is_err());
        assert!(validate_file("-file.txt").is_err());
        assert!(validate_file("../file.txt").is_err());
        assert!(validate_file("src/file.txt").is_ok());
    }

    #[test]
    fn matches_only_not_a_repository_diagnostics() {
        assert!(is_not_a_repository_diagnostic(
            b"fatal: not a git repository (or any of the parent directories): .git"
        ));
        // Localized output does not match, which is why the probe pins the locale.
        assert!(!is_not_a_repository_diagnostic(
            b"fatal: kein Git-Repository (oder eines der \\303\\274bergeordneten Verzeichnisse): .git"
        ));
        assert!(!is_not_a_repository_diagnostic(
            b"fatal: unable to access '.git': Permission denied"
        ));
        assert!(!is_not_a_repository_diagnostic(b""));
    }
    #[test]
    fn selects_main_then_master_as_primary_branch() {
        assert_eq!(
            primary_branch(&["refs/heads/master".into(), "refs/heads/main".into()]),
            Some("refs/heads/main")
        );
        assert_eq!(
            primary_branch(&["refs/heads/master".into()]),
            Some("refs/heads/master")
        );
        assert_eq!(primary_branch(&["refs/heads/develop".into()]), None);
        assert_eq!(
            primary_branch(&["refs/tags/main".into(), "refs/heads/master".into()]),
            Some("refs/heads/master")
        );
    }

    #[test]
    fn rejects_invalid_untracked_combinations() {
        assert!(validate_untracked_combination(true, true, false).is_err());
        assert!(validate_untracked_combination(true, false, true).is_err());
        assert!(validate_untracked_combination(true, false, false).is_ok());
    }

    #[tokio::test]
    async fn splits_remote_tracking_refs_by_longest_configured_remote() {
        let repo = test_repo("tracking-ref-split");
        test_git(&repo, &["remote", "add", "origin", "https://example.com/origin.git"]);
        test_git(&repo, &["remote", "add", "feature/one", "https://example.com/feature.git"]);
        assert_eq!(
            remote_branch_of_tracking_ref(&repo, "refs/remotes/feature/one/topic").await,
            Some(("feature/one".into(), "topic".into()))
        );
        assert_eq!(
            remote_branch_of_tracking_ref(&repo, "refs/remotes/origin/main").await,
            Some(("origin".into(), "main".into()))
        );
        // Local refs, bare remote namespaces, and unclaimed remotes do not split.
        assert_eq!(remote_branch_of_tracking_ref(&repo, "refs/heads/main").await, None);
        assert_eq!(remote_branch_of_tracking_ref(&repo, "refs/remotes/origin").await, None);
        assert_eq!(remote_branch_of_tracking_ref(&repo, "refs/remotes/unknown/main").await, None);
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn matches_only_promisor_failure_markers() {
        // Real git output for a blocked lazy fetch, byte for byte.
        assert!(is_promisor_failure(
            b"warning: lazy fetching disabled; some objects may not be available\n\
              fatal: could not fetch abc123 from promisor remote"
        ));
        assert!(!is_promisor_failure(b"fatal: bad object HEAD"));
        assert!(!is_promisor_failure(b"error: could not fetch refs/heads/main"));
        assert!(!is_promisor_failure(b""));
    }

    #[tokio::test]
    async fn classifies_promisor_failures_only_with_promisor_config() {
        let repo = test_repo("promisor-classify");
        // A failing diff with no promisor config stays generic: the config
        // probe finds nothing, so no re-run or classification happens.
        let failing = ["diff", "--numstat", "refs/heads/main~1..refs/heads/main"];
        assert!(partial_clone_failure(&repo, &failing).await.is_none());
        test_git(&repo, &["config", "remote.origin.promisor", "true"]);
        test_git(&repo, &["config", "remote.origin.url", "https://example.com/repo.git"]);
        // With promisor config but a re-run whose stderr carries no promisor
        // marker, the failure stays generic too.
        let not_a_diff = ["rev-parse", "--verify", "refs/heads/missing-probe"];
        assert!(partial_clone_failure(&repo, &not_a_diff).await.is_none());
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn rejects_non_worktree_all_changes_scope() {
        let error =
            validate_scope_combination("refs/heads/main", "refs/heads/main", false).unwrap_err();
        assert_eq!(error.code, "scope_requires_worktree");
        assert!(validate_scope_combination("refs/heads/main", "HEAD", false).is_ok());
        assert!(validate_scope_combination("refs/heads/main", "refs/heads/main", true).is_ok());
    }
    #[test]
    fn rejects_empty_tree_base_outside_committed_scope() {
        let error = validate_scope_combination("empty-tree", "HEAD", false).unwrap_err();
        assert_eq!(error.code, "unresolvable_ref");
        assert!(validate_scope_combination("empty-tree", "refs/heads/main", true).is_ok());
    }
    #[tokio::test]
    async fn rejects_unresolvable_review_refs() {
        let repo = test_repo("unresolvable-ref");
        let pool = test_pool().await;
        let error = review_changes(
            &pool,
            repo.to_str().unwrap(),
            repo.to_str().unwrap().into(),
            "refs/heads/missing".into(),
            None,
            true,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "unresolvable_ref");
        assert!(error.message.contains("refs/heads/missing"));
        std::fs::remove_dir_all(repo).unwrap();
    }
}
