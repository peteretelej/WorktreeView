use super::exec::{
    git_args, git_execution_error, repo_stdin_git_command, run_git, run_git_with_stdin,
};
use crate::CommandError;
use std::path::Path;

pub(crate) fn validate_ref(value: &str, name: &str) -> Result<(), CommandError> {
    if value.starts_with('-') {
        return Err(CommandError::new(
            "invalid_path",
            format!("The {name} must not start with '-'."),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::list_review_changes;
    use crate::testutil::test_repo;

    #[test]
    fn validates_review_arguments() {
        assert!(validate_ref("-main", "base").is_err());
        assert!(validate_ref("-branch", "worktree_branch").is_err());
        assert!(validate_file("-file.txt").is_err());
        assert!(validate_file("../file.txt").is_err());
        assert!(validate_file("src/file.txt").is_ok());
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
        let error = list_review_changes(
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
