use crate::cache;
use crate::git::{
    acceptable_diff_exit, configured_filter_names, effective_head_ref, filter_override_args,
    git_args, git_execution_error, parse_commits, parse_name_status, parse_numstat,
    parse_untracked_paths, primary_branch, reject_applicable_filters, resolve_empty_tree,
    resolve_ref, run_git, run_git_with_stdin, stdin_git_command, validate_file, validate_ref,
    validate_scope_combination, validate_untracked_combination, ChangedFile, CommitInfo,
    CommitPage, MAX_OUTPUT,
};
use crate::retrospection;
use crate::{canonical_path, CommandError};
use cap_fs_ext::OpenOptionsSyncExt;
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use serde::Serialize;
use sqlx::SqlitePool;
use std::{io::Read, path::Path};

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct RefInventory {
    heads: Vec<String>,
    remotes: Vec<String>,
    tags: Vec<String>,
    default_base: Option<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct ReviewIndex {
    pub(crate) files: Vec<ChangedFile>,
    additions: i64,
    deletions: i64,
    base_sha: String,
    target_sha: String,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct FilePatch {
    pub(crate) binary: bool,
    pub(crate) text: String,
}

fn review_index_args(format: &str, range: &str, reversed: bool) -> Vec<String> {
    let mut args = vec!["diff".into()];
    if reversed {
        args.push("-R".into());
    }
    args.extend([
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        "--no-color".into(),
        format.into(),
        "-z".into(),
        range.into(),
        "--".into(),
    ]);
    args
}

async fn capture_untracked_file(root: &Path, file: &str) -> Result<Vec<u8>, CommandError> {
    capture_untracked_file_before_open(root, file, || {}).await
}

async fn capture_untracked_file_before_open<F>(
    root: &Path,
    file: &str,
    before_open: F,
) -> Result<Vec<u8>, CommandError>
where
    F: FnOnce(),
{
    let (exit_code, stdout, stderr) =
        run_git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    if !parse_untracked_paths(&stdout)?
        .iter()
        .any(|untracked| untracked == file)
    {
        return Err(CommandError::new(
            "invalid_path",
            "The selected file is not an untracked review file.",
        ));
    }
    before_open();
    open_untracked_file(root, file)
}

fn open_untracked_file(root: &Path, file: &str) -> Result<Vec<u8>, CommandError> {
    let root = Dir::open_ambient_dir(root, ambient_authority()).map_err(|_| {
        CommandError::new("invalid_path", "The selected folder could not be opened.")
    })?;
    let mut options = OpenOptions::new();
    options.read(true).nonblock(true);
    let mut opened = root
        .open_with(file, &options)
        .map_err(|_| CommandError::new("invalid_path", "The selected file could not be opened."))?;
    if !opened
        .metadata()
        .map_err(|_| CommandError::new("invalid_path", "The selected file could not be read."))?
        .file_type()
        .is_file()
    {
        return Err(CommandError::new(
            "invalid_path",
            "The selected path is not a regular file.",
        ));
    }
    let mut output = Vec::new();
    (&mut opened)
        .take((MAX_OUTPUT + 1) as u64)
        .read_to_end(&mut output)
        .map_err(|_| CommandError::new("invalid_path", "The selected file could not be read."))?;
    if output.len() > MAX_OUTPUT {
        return Err(CommandError::new(
            "git_output_too_large",
            "Git output was too large.",
        ));
    }
    Ok(output)
}

pub(crate) async fn refs_inventory(
    path: String,
    worktree_branch: Option<String>,
) -> Result<RefInventory, CommandError> {
    if let Some(worktree_branch) = &worktree_branch {
        validate_ref(worktree_branch, "worktree_branch")?;
    }
    let path = canonical_path(&path)?;
    let (exit_code, stdout, stderr) = run_git(
        &path,
        &[
            "for-each-ref",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
            "--format=%(refname)",
        ],
    )
    .await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let text = std::str::from_utf8(&stdout)
        .map_err(|_| CommandError::new("git_output_malformed", "Git returned invalid ref data."))?;
    let mut heads = Vec::new();
    let mut remotes = Vec::new();
    let mut tags = Vec::new();
    for reference in text.lines() {
        if reference.starts_with("refs/heads/") {
            heads.push(reference.to_string());
        } else if reference.starts_with("refs/remotes/") {
            remotes.push(reference.to_string());
        } else if reference.starts_with("refs/tags/") {
            tags.push(reference.to_string());
        }
    }
    let primary = primary_branch(&heads);
    let default_base = if worktree_branch.as_deref() == Some("detached") {
        None
    } else if let Some(worktree_branch) = worktree_branch {
        if let Some(primary) = primary {
            let verify_args = vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                worktree_branch.clone(),
            ];
            let verify_args = git_args(&verify_args);
            if run_git(&path, &verify_args).await?.0 != 0 {
                None
            } else {
                let args = vec!["merge-base".into(), worktree_branch, primary.into()];
                let args = git_args(&args);
                match run_git(&path, &args).await? {
                    (0, stdout, _) => std::str::from_utf8(&stdout)
                        .ok()
                        .map(|value| value.trim().to_string()),
                    _ => None,
                }
            }
        } else {
            None
        }
    } else {
        primary.map(str::to_string)
    };
    Ok(RefInventory {
        heads,
        remotes,
        tags,
        default_base,
    })
}

// One bounded probe per page: `rev-list --no-walk <page SHAs> --not <against>`
// lists page commits that are NOT ancestors of `against`; everything absent
// from the output is an ancestor. A probe failure is an error, never a silent
// `false` flag, because the frontend default-base heuristic depends on it.
async fn mark_default_base_ancestors(
    path: &Path,
    commits: &mut [CommitInfo],
    against_sha: &str,
) -> Result<(), CommandError> {
    if commits.is_empty() {
        return Ok(());
    }
    let mut owned_args: Vec<String> = Vec::with_capacity(commits.len() + 4);
    owned_args.push("rev-list".into());
    owned_args.push("--no-walk".into());
    owned_args.extend(commits.iter().map(|commit| commit.sha.clone()));
    owned_args.push("--not".into());
    owned_args.push(against_sha.into());
    let args = git_args(&owned_args);
    let (exit_code, stdout, stderr) = run_git(path, &args).await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let excluded = std::str::from_utf8(&stdout).map_err(|_| {
        CommandError::new(
            "git_output_malformed",
            "Git returned invalid ancestry data.",
        )
    })?;
    let non_ancestors: Vec<&str> = excluded.split_whitespace().collect();
    for commit in commits.iter_mut() {
        commit.default_base_ancestor = !non_ancestors.contains(&commit.sha.as_str());
    }
    Ok(())
}

// Exactly 40 ASCII hex characters: a literal commit SHA, whose content is
// immutable, versus a symbolic ref that must resolve fresh every time.
fn literal_sha(reference: &str) -> bool {
    reference.len() == 40 && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
}

// The opened page already carries `%D` decorations: the `HEAD ->` entry
// names the opened worktree's branch without another spawn. Empty when
// detached or when the page has no commits.
fn head_decoration_label(commits: &[CommitInfo]) -> String {
    let Some(first) = commits.first() else {
        return String::new();
    };
    for reference in &first.refs {
        if let Some(("HEAD", target)) = reference.split_once(" -> ") {
            if let Some(branch) = target.strip_prefix("refs/heads/") {
                return branch.to_string();
            }
        }
    }
    String::new()
}

pub(crate) async fn commit_page(
    pool: &SqlitePool,
    repo_path: &str,
    path: String,
    start_ref: Option<String>,
    against: Option<String>,
    skip: Option<u32>,
    limit: Option<u16>,
) -> Result<CommitPage, CommandError> {
    if let Some(start_ref) = &start_ref {
        validate_ref(start_ref, "start_ref")?;
    }
    if let Some(against) = &against {
        validate_ref(against, "against")?;
    }
    let path = canonical_path(&path)?;
    let explicit_start = start_ref.is_some();
    let start_ref = effective_head_ref(start_ref);
    // Only explicit `refs/heads/` start refs name a surface worth recording;
    // tags, raw SHAs, and rev expressions are not surfaces.
    let branch_start = if explicit_start && start_ref.starts_with("refs/heads/") {
        Some(start_ref.clone())
    } else {
        None
    };
    let skip = skip.unwrap_or(0);
    let limit = usize::from(limit.unwrap_or(100).min(100));

    // Symbolic start refs resolve first: the fresh resolution is the cache
    // key and its freshness proof. Literal start SHAs are content-addressed,
    // so the cache is consulted before resolving and a hit spawns nothing;
    // only a miss pays for a resolution (a gc-pruned SHA fails there).
    // `against` resolves before every cache use: a symbolic base must be
    // fresh, and a literal-SHA base is used as-is.
    let start_sha: String;
    let against_sha: Option<String>;
    let cached: Option<(Vec<CommitInfo>, bool)>;
    if literal_sha(&start_ref) {
        against_sha = match &against {
            Some(against) => Some(resolve_ref(&path, against).await?),
            None => None,
        };
        cached = cache::lookup_log_page(
            pool,
            repo_path,
            &start_ref,
            against_sha.as_deref().unwrap_or(""),
            skip,
            limit,
        )
        .await;
        if cached.is_some() {
            start_sha = start_ref.clone();
        } else {
            start_sha = resolve_ref(&path, &start_ref).await.map_err(|error| {
                if error.code == "unresolvable_ref" {
                    // The literal SHA is the recorded head of an opened
                    // surface: when Git can no longer resolve it (reflog
                    // expiry then gc), the content is gone rather than the
                    // ref being wrong.
                    CommandError::new(
                        "content_unavailable",
                        "This surface's content is no longer available in the repository.",
                    )
                } else {
                    error
                }
            })?;
        }
    } else {
        start_sha = resolve_ref(&path, &start_ref).await?;
        against_sha = match &against {
            Some(against) => Some(resolve_ref(&path, against).await?),
            None => None,
        };
        cached = cache::lookup_log_page(
            pool,
            repo_path,
            &start_sha,
            against_sha.as_deref().unwrap_or(""),
            skip,
            limit,
        )
        .await;
    }

    let page = if let Some((mut commits, has_more)) = cached {
        if let Some(against_sha) = &against_sha {
            let shas: Vec<String> = commits.iter().map(|commit| commit.sha.clone()).collect();
            let non_ancestors = match cache::lookup_marks(pool, &shas, against_sha).await {
                Some(non_ancestors) => non_ancestors,
                None => {
                    mark_default_base_ancestors(&path, &mut commits, against_sha).await?;
                    let non_ancestors: Vec<bool> = commits
                        .iter()
                        .map(|commit| !commit.default_base_ancestor)
                        .collect();
                    cache::store_marks(pool, &shas, against_sha, &non_ancestors).await;
                    non_ancestors
                }
            };
            for (commit, non_ancestor) in commits.iter_mut().zip(non_ancestors) {
                commit.default_base_ancestor = !non_ancestor;
            }
        }
        CommitPage { commits, has_more }
    } else {
        let mut owned_args = vec![
            "log".into(),
            format!("--skip={skip}"),
            format!("--max-count={}", limit + 1),
            // Decorations feed the opened worktree's recorded label and must
            // be full refnames regardless of tty and log.decorate defaults.
            "--decorate=full".into(),
            "--format=%H%x1f%s%x1f%an%x1f%aI%x1f%D%x1f%P%x1e".into(),
        ];
        if explicit_start {
            owned_args.push(start_sha.clone());
        }
        let args = git_args(&owned_args);
        let (exit_code, stdout, stderr) = run_git(&path, &args).await?;
        if exit_code != 0 {
            return Err(git_execution_error(&stderr));
        }
        let mut commits = parse_commits(&stdout)?;
        let has_more = commits.len() > limit;
        commits.truncate(limit);
        if let Some(against_sha) = &against_sha {
            mark_default_base_ancestors(&path, &mut commits, against_sha).await?;
        }
        // The cache is an optimization: a failed write (a full disk, for one)
        // never fails the open.
        let page_shas: Vec<String> = commits.iter().map(|commit| commit.sha.clone()).collect();
        cache::store_log_page(
            pool,
            repo_path,
            &start_sha,
            against_sha.as_deref().unwrap_or(""),
            skip,
            limit,
            &commits,
            has_more,
        )
        .await;
        if let Some(against_sha) = &against_sha {
            let non_ancestors: Vec<bool> = commits
                .iter()
                .map(|commit| !commit.default_base_ancestor)
                .collect();
            cache::store_marks(pool, &page_shas, against_sha, &non_ancestors).await;
        }
        CommitPage { commits, has_more }
    };
    // Open-time retrospection: record a surface only for opens that resolved
    // a real one. A HEAD-based open is the worktree surface, labeled from the
    // page's `HEAD ->` decoration (empty when detached, keeping any recorded
    // label); an explicit `refs/heads/` open is the branch surface; tags,
    // raw SHAs, and rev expressions record nothing, so reopening a gone
    // surface by SHA leaves `last_seen` at "last open while alive".
    if let Some(branch_ref) = branch_start {
        let label = branch_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&branch_ref);
        retrospection::record_surface_open(
            pool,
            repo_path,
            "branch",
            &branch_ref,
            label,
            &branch_ref,
            &start_sha,
        )
        .await;
    } else if !explicit_start {
        let worktree_key = path.to_str().unwrap_or_default();
        let label = head_decoration_label(&page.commits);
        retrospection::record_surface_open(
            pool,
            repo_path,
            "worktree",
            worktree_key,
            &label,
            worktree_key,
            &start_sha,
        )
        .await;
    }
    Ok(page)
}

pub(crate) async fn review_changes(
    pool: &SqlitePool,
    repo_path: &str,
    path: String,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
) -> Result<ReviewIndex, CommandError> {
    validate_ref(&base, "base")?;
    let explicit_head = head_ref.is_some();
    let head_ref = effective_head_ref(head_ref);
    validate_ref(&head_ref, "head_ref")?;
    validate_scope_combination(&base, &head_ref, committed_only)?;
    let path = canonical_path(&path)?;
    // "empty-tree" is a reserved base value for parentless commits: it resolves
    // to the repository's empty tree object (no ^{commit} resolution) and the
    // committed-only range becomes two-dot, because three-dot computes a
    // merge-base and cannot accept a tree.
    let empty_tree_base = base == "empty-tree";
    let base_sha = if empty_tree_base {
        resolve_empty_tree(&path).await?
    } else {
        resolve_ref(&path, &base).await?
    };
    let target_sha = resolve_ref(&path, &head_ref).await?;
    // Same conservative recording rules as commit_page: the worktree target
    // records with an empty label (the branch name is not in hand without an
    // extra spawn; the CASE keeps the nicer label commit_page recorded), a
    // branch target records the full ref, commit targets record nothing.
    if !explicit_head {
        let worktree_key = path.to_str().unwrap_or_default();
        retrospection::record_surface_open(
            pool,
            repo_path,
            "worktree",
            worktree_key,
            "",
            worktree_key,
            &target_sha,
        )
        .await;
    } else if head_ref.starts_with("refs/heads/") {
        let label = head_ref.strip_prefix("refs/heads/").unwrap_or(&head_ref);
        retrospection::record_surface_open(
            pool,
            repo_path,
            "branch",
            &head_ref,
            label,
            &head_ref,
            &target_sha,
        )
        .await;
    }
    // Filters configured in any scope are neutralized for inventory commands so
    // that enumeration never executes them; reviews are refused only when a
    // changed file's attributes actually map to a configured filter.
    let filters = if committed_only {
        Vec::new()
    } else {
        configured_filter_names(&path).await?
    };
    let range = if committed_only {
        if empty_tree_base {
            format!("{base_sha}..{head_ref}")
        } else {
            format!("{base}...{head_ref}")
        }
    } else {
        base.clone()
    };
    let mut name_owned = filter_override_args(&filters);
    name_owned.extend(review_index_args("--name-status", &range, reversed));
    let name_args = git_args(&name_owned);
    let (exit_code, stdout, stderr) = run_git(&path, &name_args).await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let mut files = parse_name_status(&stdout)?;
    if !filters.is_empty() {
        let paths: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
        reject_applicable_filters(&path, &paths, &filters).await?;
    }

    let mut num_owned = filter_override_args(&filters);
    num_owned.extend(review_index_args("--numstat", &range, reversed));
    let num_args = git_args(&num_owned);
    let (exit_code, stdout, stderr) = run_git(&path, &num_args).await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let (additions, deletions, _) = parse_numstat(&stdout)?;

    if !committed_only && !reversed {
        let (exit_code, stdout, stderr) =
            run_git(&path, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
        if exit_code != 0 {
            return Err(git_execution_error(&stderr));
        }
        for file in parse_untracked_paths(&stdout)? {
            files.push(ChangedFile {
                path: file.into(),
                status: "A".into(),
                untracked: true,
            });
        }
    }
    Ok(ReviewIndex {
        files,
        additions,
        deletions,
        base_sha,
        target_sha,
    })
}

pub(crate) async fn review_patch(
    path: String,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
) -> Result<FilePatch, CommandError> {
    validate_ref(&base, "base")?;
    let head_ref = effective_head_ref(head_ref);
    validate_ref(&head_ref, "head_ref")?;
    validate_scope_combination(&base, &head_ref, committed_only)?;
    validate_file(&file)?;
    validate_untracked_combination(untracked, committed_only, reversed)?;
    let path = canonical_path(&path)?;
    // Same reserved base as `review_changes`: parentless commits diff the
    // empty tree via the two-dot committed-only range.
    let empty_tree_base = base == "empty-tree";
    // Literal SHA endpoints are content-addressed: the diff validates them
    // by executing, so they spawn no resolution. Symbolic endpoints keep
    // resolving fresh, because a missing ref must still fail as
    // unresolvable_ref. The resolved base is only needed for the
    // empty-tree range.
    let base_sha = if empty_tree_base {
        resolve_empty_tree(&path).await?
    } else if literal_sha(&base) {
        String::new()
    } else {
        resolve_ref(&path, &base).await?
    };
    if !literal_sha(&head_ref) {
        resolve_ref(&path, &head_ref).await?;
    }
    if !committed_only && !untracked {
        let filters = configured_filter_names(&path).await?;
        reject_applicable_filters(&path, std::slice::from_ref(&file), &filters).await?;
    }
    let range = if committed_only {
        if empty_tree_base {
            format!("{base_sha}..{head_ref}")
        } else {
            format!("{base}...{head_ref}")
        }
    } else {
        base
    };
    let untracked_input = if untracked {
        Some(capture_untracked_file(&path, &file).await?)
    } else {
        None
    };
    let mut num_args = vec![
        "diff".into(),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        "--numstat".into(),
        "-z".into(),
    ];
    if reversed {
        num_args.push("-R".into());
    }
    let (exit_code, stdout, stderr) = if let Some(input) = &untracked_input {
        let args = vec![
            "diff".into(),
            "--no-index".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--numstat".into(),
            "-z".into(),
            "--".into(),
            "/dev/null".into(),
            "-".into(),
        ];
        let args = git_args(&args);
        run_git_with_stdin(stdin_git_command(&args), input.clone()).await?
    } else {
        num_args.extend([range.clone(), "--".into(), file.clone()]);
        let args = git_args(&num_args);
        run_git(&path, &args).await?
    };
    if !acceptable_diff_exit(exit_code, untracked) {
        return Err(git_execution_error(&stderr));
    }
    let (_, _, binary) = parse_numstat(&stdout)?;
    if binary {
        return Ok(FilePatch {
            binary: true,
            text: String::new(),
        });
    }
    let mut patch_args = vec![
        "diff".into(),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        "--no-color".into(),
        "-U3".into(),
    ];
    if reversed {
        patch_args.push("-R".into());
    }
    let (exit_code, stdout, stderr) = if let Some(input) = &untracked_input {
        let args = vec![
            "diff".into(),
            "--no-index".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--no-color".into(),
            "-U3".into(),
            "--".into(),
            "/dev/null".into(),
            "-".into(),
        ];
        let args = git_args(&args);
        run_git_with_stdin(stdin_git_command(&args), input.clone()).await?
    } else {
        patch_args.extend([range, "--".into(), file]);
        let args = git_args(&patch_args);
        run_git(&path, &args).await?
    };
    if !acceptable_diff_exit(exit_code, untracked) {
        return Err(git_execution_error(&stderr));
    }
    let text = String::from_utf8(stdout).map_err(|_| {
        CommandError::new("git_output_malformed", "Git returned invalid patch text.")
    })?;
    Ok(FilePatch {
        binary: false,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::read_bounded;
    use crate::git::spawn_counted;
    use crate::testutil::{seed_repo, test_git, test_path, test_pool, test_repo, test_rev_parse};
    #[cfg(unix)]
    use std::path::PathBuf;
    use std::process::Command as StdCommand;
    use tokio::time::Duration;

    #[test]
    fn review_index_arguments_disable_repository_diff_helpers() {
        assert_eq!(
            review_index_args("--name-status", "base", false),
            [
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--name-status",
                "-z",
                "base",
                "--"
            ]
        );
        assert_eq!(
            review_index_args("--numstat", "base...HEAD", true),
            [
                "diff",
                "-R",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--numstat",
                "-z",
                "base...HEAD",
                "--"
            ]
        );
    }
    #[tokio::test]
    async fn branch_tag_collision_uses_full_refs_for_review_data() {
        let repo = test_repo("branch-tag-collision");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        test_git(&repo, &["branch", "-M", "main"]);
        test_git(&repo, &["tag", "main", "HEAD"]);
        test_git(
            &repo,
            &["update-ref", "refs/remotes/origin/feature", "HEAD"],
        );
        test_git(&repo, &["update-ref", "refs/notes/review", "HEAD"]);
        std::fs::write(repo.join("main-only.txt"), "main\n").unwrap();
        test_git(&repo, &["add", "main-only.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "main change"]);
        test_git(&repo, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(repo.join("feature-only.txt"), "feature\n").unwrap();
        test_git(&repo, &["add", "feature-only.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "feature change"]);

        let inventory = refs_inventory(
            repo.to_str().unwrap().into(),
            Some("refs/heads/feature".into()),
        )
        .await
        .unwrap();
        assert!(inventory.heads.contains(&"refs/heads/main".into()));
        assert!(inventory.heads.contains(&"refs/heads/feature".into()));
        assert!(inventory.tags.contains(&"refs/tags/main".into()));
        assert!(inventory
            .remotes
            .contains(&"refs/remotes/origin/feature".into()));
        assert!(!inventory
            .heads
            .iter()
            .any(|reference| reference == "refs/notes/review"));

        let ref_context = refs_inventory(repo.to_str().unwrap().into(), None)
            .await
            .unwrap();
        assert_eq!(ref_context.default_base.as_deref(), Some("refs/heads/main"));

        let main_head = StdCommand::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "refs/heads/main"])
            .output()
            .unwrap();
        assert!(main_head.status.success());
        assert_eq!(
            inventory.default_base.as_deref(),
            Some(String::from_utf8_lossy(&main_head.stdout).trim())
        );

        let review = review_changes(
            &pool,
            &repo_path,
            repo.to_str().unwrap().into(),
            "refs/heads/main".into(),
            Some("refs/heads/feature".into()),
            true,
            false,
        )
        .await
        .unwrap();
        assert_eq!(review.files.len(), 1);
        assert_eq!(review.files[0].path, "feature-only.txt");
        assert_eq!(
            review.base_sha,
            main_head
                .stdout
                .iter()
                .map(|byte| *byte as char)
                .collect::<String>()
                .trim()
        );
        assert_eq!(
            review.target_sha,
            String::from_utf8_lossy(&{
                let output = StdCommand::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(["rev-parse", "HEAD"])
                    .output()
                    .unwrap();
                output.stdout
            })
            .trim()
        );
        let patch = review_patch(
            repo.to_str().unwrap().into(),
            "refs/heads/main".into(),
            Some("refs/heads/feature".into()),
            true,
            false,
            "feature-only.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert!(patch.text.contains("+feature"));
        assert!(!patch.text.contains("main-only"));

        std::fs::remove_dir_all(repo).unwrap();
    }
    #[tokio::test]
    async fn tracked_review_does_not_refresh_index() {
        let repo = test_repo("index-refresh");
        let pool = test_pool().await;
        let index = repo.join(".git/index");
        let before = std::fs::read(&index).unwrap();
        let tracked = std::fs::OpenOptions::new()
            .write(true)
            .open(repo.join("tracked.txt"))
            .unwrap();
        tracked
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() - Duration::from_secs(60)),
            )
            .unwrap();
        drop(tracked);

        review_changes(
            &pool,
            &repo.to_str().unwrap().to_string(),
            repo.to_str().unwrap().into(),
            "HEAD".into(),
            None,
            false,
            false,
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(&index).unwrap(), before);
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tracked_review_does_not_lazy_fetch_missing_objects() {
        use std::os::unix::fs::PermissionsExt;

        const CHILD_ENV: &str = "WORKTREEVIEW_NO_LAZY_FETCH_CHILD";
        const REPO_ENV: &str = "WORKTREEVIEW_NO_LAZY_FETCH_REPO";
        const BASE_ENV: &str = "WORKTREEVIEW_NO_LAZY_FETCH_BASE";
        const MARKER_ENV: &str = "WORKTREEVIEW_NO_LAZY_FETCH_MARKER";

        if std::env::var_os(CHILD_ENV).is_some() {
            let repo = PathBuf::from(std::env::var_os(REPO_ENV).unwrap());
            let base = std::env::var(BASE_ENV).unwrap();
            let marker = PathBuf::from(std::env::var_os(MARKER_ENV).unwrap());
            let error = review_patch(
                repo.to_str().unwrap().into(),
                base,
                None,
                true,
                false,
                "tracked.txt".into(),
                false,
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, "git_execution");
            assert!(!marker.exists());
            return;
        }

        let source = test_repo("lazy-fetch-source");
        let base = StdCommand::new("git")
            .arg("-C")
            .arg(&source)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(base.status.success());
        let base = String::from_utf8(base.stdout).unwrap().trim().to_string();
        std::fs::write(source.join("tracked.txt"), "changed\n").unwrap();
        test_git(&source, &["add", "tracked.txt"]);
        test_git(&source, &["commit", "--quiet", "-m", "changed"]);

        let origin = test_path("lazy-fetch-origin.git");
        let output = StdCommand::new("git")
            .args(["clone", "--quiet", "--bare", "--no-hardlinks"])
            .arg(&source)
            .arg(&origin)
            .output()
            .unwrap();
        assert!(output.status.success());
        let repo = test_path("lazy-fetch-clone");
        let output = StdCommand::new("git")
            .args(["clone", "--quiet", "--no-hardlinks"])
            .arg(&origin)
            .arg(&repo)
            .output()
            .unwrap();
        assert!(output.status.success());
        test_git(&repo, &["config", "remote.origin.promisor", "true"]);
        test_git(
            &repo,
            &["config", "remote.origin.partialclonefilter", "blob:none"],
        );

        let blob = StdCommand::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", &format!("{base}:tracked.txt")])
            .output()
            .unwrap();
        assert!(blob.status.success());
        let blob = String::from_utf8(blob.stdout).unwrap().trim().to_string();
        let object = repo.join(".git/objects").join(&blob[..2]).join(&blob[2..]);
        assert!(object.exists());
        std::fs::remove_file(object).unwrap();

        let marker = repo.join("lazy-fetch-ran");
        let helper = repo.join("upload-pack.sh");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\n: > '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        test_git(
            &repo,
            &[
                "config",
                "remote.origin.uploadpack",
                helper.to_str().unwrap(),
            ],
        );

        let probe = StdCommand::new("git")
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "diff.autoRefreshIndex=false",
                "-C",
            ])
            .arg(&repo)
            .args([
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--numstat",
                "-z",
                &format!("{base}...HEAD"),
                "--",
                "tracked.txt",
            ])
            .env_remove("GIT_NO_LAZY_FETCH")
            .output()
            .unwrap();
        assert!(!probe.status.success());
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();

        let output = StdCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "review::tests::tracked_review_does_not_lazy_fetch_missing_objects",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .env(REPO_ENV, &repo)
            .env(BASE_ENV, &base)
            .env(MARKER_ENV, &marker)
            .env_remove("GIT_NO_LAZY_FETCH")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "no-lazy-fetch child failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        std::fs::remove_dir_all(source).unwrap();
        std::fs::remove_dir_all(origin).unwrap();
        std::fs::remove_dir_all(repo).unwrap();
    }
    #[tokio::test]
    async fn literal_sha_patch_endpoints_spawn_no_resolutions() {
        let repo = test_repo("patch-literal-endpoints");
        std::fs::write(repo.join("tracked.txt"), "base\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "base"]);
        let base_sha = test_rev_parse(&repo, "HEAD");
        std::fs::write(repo.join("tracked.txt"), "changed\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "changed"]);
        let head_sha = test_rev_parse(&repo, "HEAD");

        let patch = review_patch(
            repo.to_str().unwrap().into(),
            base_sha.clone(),
            Some(head_sha.clone()),
            true,
            false,
            "tracked.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert!(patch.text.contains("+changed"));

        // Content-addressed endpoints are validated by the diff itself:
        // only the numstat probe and the patch spawn remain.
        let (spawns, again) = spawn_counted(review_patch(
            repo.to_str().unwrap().into(),
            base_sha.clone(),
            Some(head_sha.clone()),
            true,
            false,
            "tracked.txt".into(),
            false,
        ))
        .await;
        assert_eq!(spawns, 2);
        assert!(again.unwrap().text.contains("+changed"));

        // A missing literal SHA fails at the diff as a Git execution error
        // rather than the symbolic-ref unresolvable_ref contract.
        let missing = review_patch(
            repo.to_str().unwrap().into(),
            "a".repeat(40),
            Some(head_sha),
            true,
            false,
            "tracked.txt".into(),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(missing.code, "git_execution");

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn untracked_patch_requires_current_git_inventory() {
        let repo = test_repo("untracked-inventory");
        std::fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(repo.join("ignored.txt"), "ignored\n").unwrap();
        std::fs::write(repo.join("new.txt"), "new\n").unwrap();
        std::fs::write(repo.join("binary.bin"), [b'a', 0, b'b']).unwrap();

        let patch = review_patch(
            repo.to_str().unwrap().into(),
            "HEAD".into(),
            None,
            false,
            false,
            "new.txt".into(),
            true,
        )
        .await
        .unwrap();
        assert!(patch.text.contains("new"));

        let binary = review_patch(
            repo.to_str().unwrap().into(),
            "HEAD".into(),
            None,
            false,
            false,
            "binary.bin".into(),
            true,
        )
        .await
        .unwrap();
        assert!(binary.binary);
        assert!(binary.text.is_empty());

        let ignored = review_patch(
            repo.to_str().unwrap().into(),
            "HEAD".into(),
            None,
            false,
            false,
            "ignored.txt".into(),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(ignored.code, "invalid_path");

        let arbitrary = test_path("arbitrary-root");
        std::fs::create_dir(&arbitrary).unwrap();
        std::fs::write(arbitrary.join("new.txt"), "new\n").unwrap();
        let error = review_patch(
            arbitrary.to_str().unwrap().into(),
            "HEAD".into(),
            None,
            false,
            false,
            "new.txt".into(),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "unresolvable_ref");

        std::fs::remove_dir_all(repo).unwrap();
        std::fs::remove_dir_all(arbitrary).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn opened_untracked_files_remain_inside_the_root() {
        use std::os::unix::fs::symlink;

        let root = test_path("symlink-root");
        let outside = test_path("symlink-outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("file.txt"), "outside\n").unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        symlink(outside.join("file.txt"), root.join("final-link")).unwrap();
        std::fs::create_dir(root.join("inside-dir")).unwrap();
        std::fs::write(root.join("inside-dir/file.txt"), "contained\n").unwrap();
        symlink("inside-dir", root.join("inside-link")).unwrap();
        let root = root.canonicalize().unwrap();

        assert!(open_untracked_file(&root, "escape/file.txt").is_err());
        assert!(open_untracked_file(&root, "final-link").is_err());
        std::fs::write(root.join("inside.txt"), "inside\n").unwrap();
        assert_eq!(
            open_untracked_file(&root, "inside.txt").unwrap(),
            b"inside\n"
        );
        assert_eq!(
            open_untracked_file(&root, "inside-link/file.txt").unwrap(),
            b"contained\n"
        );

        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inventory_to_open_replacements_remain_contained() {
        use std::os::unix::fs::symlink;

        let repo = test_repo("replacement-containment");
        let outside = test_path("replacement-outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("file.txt"), "outside\n").unwrap();

        let descendant = repo.join("nested");
        std::fs::create_dir(&descendant).unwrap();
        std::fs::write(descendant.join("file.txt"), "inside\n").unwrap();
        let error = capture_untracked_file_before_open(&repo, "nested/file.txt", || {
            std::fs::remove_dir_all(&descendant).unwrap();
            symlink(&outside, &descendant).unwrap();
        })
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_path");

        std::fs::write(repo.join("final.txt"), "inside\n").unwrap();
        let final_path = repo.join("final.txt");
        let outside_file = outside.join("file.txt");
        let error = capture_untracked_file_before_open(&repo, "final.txt", || {
            std::fs::remove_file(&final_path).unwrap();
            symlink(&outside_file, &final_path).unwrap();
        })
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_path");

        std::fs::remove_dir_all(repo).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inventory_to_open_fifo_replacement_fails_promptly() {
        const CHILD_ENV: &str = "WORKTREEVIEW_FIFO_REPLACEMENT_CHILD";
        const REPO_ENV: &str = "WORKTREEVIEW_FIFO_REPLACEMENT_REPO";

        if std::env::var_os(CHILD_ENV).is_some() {
            let repo = PathBuf::from(std::env::var_os(REPO_ENV).unwrap());
            let fifo = repo.join("fifo.txt");
            let error = capture_untracked_file_before_open(&repo, "fifo.txt", || {
                std::fs::remove_file(&fifo).unwrap();
                let status = StdCommand::new("mkfifo").arg(&fifo).status().unwrap();
                assert!(status.success());
            })
            .await
            .unwrap_err();
            assert_eq!(error.code, "invalid_path");
            return;
        }

        let repo = test_repo("fifo-replacement");
        std::fs::write(repo.join("fifo.txt"), "regular\n").unwrap();
        let mut child = StdCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "review::tests::inventory_to_open_fifo_replacement_fails_promptly",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .env(REPO_ENV, &repo)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("opening a FIFO replacement blocked");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success());

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn inventory_to_open_special_and_missing_files_fail_closed() {
        let repo = test_repo("special-and-missing");
        let special = repo.join("special.txt");
        std::fs::write(&special, "regular\n").unwrap();
        let error = capture_untracked_file_before_open(&repo, "special.txt", || {
            std::fs::remove_file(&special).unwrap();
            std::fs::create_dir(&special).unwrap();
        })
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_path");

        let missing = repo.join("missing.txt");
        std::fs::write(&missing, "regular\n").unwrap();
        let error = capture_untracked_file_before_open(&repo, "missing.txt", || {
            std::fs::remove_file(&missing).unwrap();
        })
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_path");

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn bounded_output_accepts_ceiling_and_rejects_overflow() {
        use std::io::Cursor;
        assert_eq!(
            read_bounded(Cursor::new(vec![0; MAX_OUTPUT]))
                .await
                .unwrap()
                .len(),
            MAX_OUTPUT
        );
        assert_eq!(
            read_bounded(Cursor::new(vec![0; MAX_OUTPUT + 1]))
                .await
                .unwrap_err()
                .code,
            "git_output_too_large"
        );

        let root = test_path("bounded-input");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("exact"), vec![0; MAX_OUTPUT]).unwrap();
        std::fs::write(root.join("overflow"), vec![0; MAX_OUTPUT + 1]).unwrap();
        assert_eq!(
            open_untracked_file(&root, "exact").unwrap().len(),
            MAX_OUTPUT
        );
        assert_eq!(
            open_untracked_file(&root, "overflow").unwrap_err().code,
            "git_output_too_large"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn list_commits_pages_bounded_histories() {
        let repo = test_repo("commit-paging");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        for index in 0..104 {
            std::fs::write(repo.join("tracked.txt"), format!("change {index}\n")).unwrap();
            test_git(&repo, &["add", "tracked.txt"]);
            let message = format!("commit {index}");
            test_git(&repo, &["commit", "--quiet", "-m", &message]);
        }
        let head_sha = test_rev_parse(&repo, "HEAD");

        let first = commit_page(&pool, &repo_path, repo_path.clone(), None, None, None, None)
            .await
            .unwrap();
        assert_eq!(first.commits.len(), 100);
        assert!(first.has_more);
        assert_eq!(first.commits[0].sha, head_sha);

        let second = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            None,
            None,
            Some(100),
            None,
        )
        .await
        .unwrap();
        assert_eq!(second.commits.len(), 5);
        assert!(!second.has_more);
        assert!(!second
            .commits
            .iter()
            .any(|commit| first.commits.iter().any(|page| page.sha == commit.sha)));

        let capped = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            None,
            None,
            None,
            Some(1000),
        )
        .await
        .unwrap();
        assert_eq!(capped.commits.len(), 100);
        assert!(capped.has_more);

        let window = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            None,
            None,
            Some(2),
            Some(3),
        )
        .await
        .unwrap();
        assert_eq!(window.commits.len(), 3);
        assert_eq!(window.commits[0].sha, first.commits[2].sha);
        assert!(window.has_more);

        let exhausted = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            None,
            None,
            Some(200),
            None,
        )
        .await
        .unwrap();
        assert!(exhausted.commits.is_empty());
        assert!(!exhausted.has_more);

        let unresolvable = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some("refs/heads/missing".into()),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(unresolvable.code, "unresolvable_ref");

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn list_commits_marks_default_base_ancestry() {
        let repo = test_repo("commit-ancestry");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        test_git(&repo, &["branch", "-M", "main"]);
        let initial_sha = test_rev_parse(&repo, "HEAD");
        std::fs::write(repo.join("main-only.txt"), "main\n").unwrap();
        test_git(&repo, &["add", "main-only.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "main change"]);
        test_git(&repo, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(repo.join("feature-only.txt"), "feature\n").unwrap();
        test_git(&repo, &["add", "feature-only.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "feature change"]);
        let main_sha = test_rev_parse(&repo, "refs/heads/main");
        let feature_sha = test_rev_parse(&repo, "refs/heads/feature");

        let page = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some("refs/heads/feature".into()),
            Some("refs/heads/main".into()),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(page.commits.len(), 3);
        let marked = |sha: &str| {
            page.commits
                .iter()
                .find(|commit| commit.sha == sha)
                .unwrap()
                .default_base_ancestor
        };
        assert!(!marked(&feature_sha));
        assert!(marked(&main_sha));
        assert!(marked(&initial_sha));

        let unmarked = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some("refs/heads/feature".into()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(unmarked
            .commits
            .iter()
            .all(|commit| !commit.default_base_ancestor));

        let unresolvable_against = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            None,
            Some("refs/heads/missing".into()),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(unresolvable_against.code, "unresolvable_ref");

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn empty_tree_base_reviews_root_commits() {
        let repo = test_repo("empty-tree-base");
        let pool = test_pool().await;
        let root_sha = test_rev_parse(&repo, "HEAD");

        let index = review_changes(
            &pool,
            &repo.to_str().unwrap().to_string(),
            repo.to_str().unwrap().into(),
            "empty-tree".into(),
            Some(root_sha.clone()),
            true,
            false,
        )
        .await
        .unwrap();
        assert_eq!(index.files.len(), 1);
        assert_eq!(index.files[0].path, "tracked.txt");
        assert_eq!(index.files[0].status, "A");
        assert_eq!(
            index.base_sha,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
        assert_eq!(index.target_sha, root_sha);

        let patch = review_patch(
            repo.to_str().unwrap().into(),
            "empty-tree".into(),
            Some(root_sha),
            true,
            false,
            "tracked.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert!(!patch.binary);
        assert!(patch.text.contains("+original"));

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn merge_first_parent_base_reviews_merge_own_changes() {
        let repo = test_repo("merge-first-parent");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        test_git(&repo, &["branch", "-M", "main"]);
        std::fs::write(repo.join("main.txt"), "main\n").unwrap();
        test_git(&repo, &["add", "main.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "main commit"]);
        test_git(&repo, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(repo.join("feature.txt"), "feature\n").unwrap();
        test_git(&repo, &["add", "feature.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "feature commit"]);
        test_git(&repo, &["checkout", "--quiet", "main"]);
        test_git(
            &repo,
            &[
                "merge",
                "--no-ff",
                "--quiet",
                "-m",
                "merge feature",
                "feature",
            ],
        );
        let merge_sha = test_rev_parse(&repo, "HEAD");
        let first_parent = test_rev_parse(&repo, &format!("{merge_sha}^"));
        let second_parent = test_rev_parse(&repo, &format!("{merge_sha}^2"));
        assert_ne!(first_parent, second_parent);

        let index = review_changes(
            &pool,
            &repo_path,
            repo.to_str().unwrap().into(),
            first_parent.clone(),
            Some(merge_sha.clone()),
            true,
            false,
        )
        .await
        .unwrap();
        assert_eq!(index.files.len(), 1);
        assert_eq!(index.files[0].path, "feature.txt");
        assert_eq!(index.files[0].status, "A");
        assert_eq!(index.target_sha, merge_sha);

        let page = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some(merge_sha),
            None,
            None,
            Some(1),
        )
        .await
        .unwrap();
        assert_eq!(page.commits[0].parents, [first_parent, second_parent]);

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn cached_history_pages_skip_git_log_spawns() {
        let repo = test_repo("cache-hit");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        test_git(&repo, &["branch", "-M", "main"]);
        for index in 0..3 {
            std::fs::write(repo.join("tracked.txt"), format!("change {index}\n")).unwrap();
            test_git(&repo, &["add", "tracked.txt"]);
            let message = format!("commit {index}");
            test_git(&repo, &["commit", "--quiet", "-m", &message]);
        }

        let first = commit_page(&pool, &repo_path, repo_path.clone(), None, None, None, None)
            .await
            .unwrap();
        // A symbolic ref cannot skip its resolution (it is the freshness
        // proof); the hit must skip the `git log` fetch and nothing else.
        let (spawns, second) = spawn_counted(commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            None,
            None,
            None,
            None,
        ))
        .await;
        assert_eq!(spawns, 1);
        assert_eq!(first, second.unwrap());

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn literal_sha_hits_spawn_nothing() {
        let repo = test_repo("cache-literal");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        std::fs::write(repo.join("tracked.txt"), "change\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "second"]);
        let head_sha = test_rev_parse(&repo, "HEAD");

        let first = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some(head_sha.clone()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        // Content behind a SHA is immutable, so a literal-SHA hit resolves
        // nothing and spawns no git children at all.
        let (spawns, second) = spawn_counted(commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some(head_sha.clone()),
            None,
            None,
            None,
        ))
        .await;
        assert_eq!(spawns, 0);
        assert_eq!(first, second.unwrap());

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn head_movement_refetches_history() {
        let repo = test_repo("cache-head-movement");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        test_git(&repo, &["branch", "-M", "main"]);

        let first = commit_page(&pool, &repo_path, repo_path.clone(), None, None, None, None)
            .await
            .unwrap();
        let (warmup, _) = spawn_counted(commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            None,
            None,
            None,
            None,
        ))
        .await;
        assert_eq!(warmup, 1);

        std::fs::write(repo.join("tracked.txt"), "newer\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "newer commit"]);
        let new_head = test_rev_parse(&repo, "HEAD");

        // The moved HEAD resolves to a new key, so the open fetches again.
        let (spawns, moved) = spawn_counted(commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            None,
            None,
            None,
            None,
        ))
        .await;
        assert_eq!(spawns, 2);
        let moved = moved.unwrap();
        assert_eq!(moved.commits[0].sha, new_head);
        assert_ne!(moved.commits[0].sha, first.commits[0].sha);

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn ancestry_marks_are_reused_between_identical_opens() {
        let repo = test_repo("cache-ancestry");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        test_git(&repo, &["branch", "-M", "main"]);
        let initial_sha = test_rev_parse(&repo, "HEAD");
        std::fs::write(repo.join("main-only.txt"), "main\n").unwrap();
        test_git(&repo, &["add", "main-only.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "main change"]);
        test_git(&repo, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(repo.join("feature-only.txt"), "feature\n").unwrap();
        test_git(&repo, &["add", "feature-only.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "feature change"]);
        let main_sha = test_rev_parse(&repo, "refs/heads/main");
        let feature_sha = test_rev_parse(&repo, "refs/heads/feature");
        let marked = |page: &CommitPage, sha: &str| {
            page.commits
                .iter()
                .find(|commit| commit.sha == sha)
                .unwrap()
                .default_base_ancestor
        };

        let first = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some("refs/heads/feature".into()),
            Some("refs/heads/main".into()),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(first.commits.len(), 3);
        assert!(!marked(&first, &feature_sha));
        assert!(marked(&first, &main_sha));
        assert!(marked(&first, &initial_sha));

        // The second identical open resolves both refs but reuses the cached
        // marks: no `git log`, no `rev-list` probe.
        let (spawns, second) = spawn_counted(commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some("refs/heads/feature".into()),
            Some("refs/heads/main".into()),
            None,
            None,
        ))
        .await;
        assert_eq!(spawns, 2);
        assert_eq!(first, second.unwrap());

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn cache_keys_share_across_working_directories() {
        let repo = test_repo("cache-cwd");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        std::fs::create_dir(repo.join("nested")).unwrap();
        std::fs::write(repo.join("nested/file.txt"), "nested\n").unwrap();
        test_git(&repo, &["add", "nested/file.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "nested file"]);
        let subdir = repo.join("nested").to_str().unwrap().to_string();

        let root_page = commit_page(&pool, &repo_path, repo_path.clone(), None, None, None, None)
            .await
            .unwrap();
        // Same repo key under a different git working directory: the page
        // hits, proving the cache is repo-scoped, not cwd-scoped.
        let (spawns, subdir_page) = spawn_counted(commit_page(
            &pool, &repo_path, subdir, None, None, None, None,
        ))
        .await;
        assert_eq!(spawns, 1);
        assert_eq!(root_page, subdir_page.unwrap());

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn pruned_surface_degrades_to_content_unavailable() {
        let repo = test_repo("retrospect-gc");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        seed_repo(&pool, &repo_path).await;
        test_git(&repo, &["branch", "-M", "main"]);
        test_git(&repo, &["checkout", "--quiet", "-b", "feature"]);
        for index in 0..4 {
            std::fs::write(repo.join("tracked.txt"), format!("change {index}\n")).unwrap();
            test_git(&repo, &["add", "tracked.txt"]);
            let message = format!("feature commit {index}");
            test_git(&repo, &["commit", "--quiet", "-m", &message]);
        }
        let feature_sha = test_rev_parse(&repo, "refs/heads/feature");

        // Opening page 1 caches it under the recorded head.
        let first = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some("refs/heads/feature".into()),
            None,
            None,
            Some(2),
        )
        .await
        .unwrap();
        assert_eq!(first.commits.len(), 2);

        test_git(&repo, &["checkout", "--quiet", "main"]);
        test_git(&repo, &["branch", "-D", "feature"]);

        // Reflog expiry plus prune make the recorded head unresolvable, the
        // proposal's accepted-debt degradation point.
        test_git(&repo, &["reflog", "expire", "--expire=now", "--all"]);
        test_git(&repo, &["gc", "--prune=now"]);

        // The cached page still renders with no git at all.
        let (spawns, cached) = spawn_counted(commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some(feature_sha.clone()),
            None,
            None,
            Some(2),
        ))
        .await;
        assert_eq!(spawns, 0);
        assert_eq!(cached.unwrap(), first);

        // A deeper uncached page by the same SHA degrades to
        // content_unavailable instead of unresolvable_ref.
        let degraded = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some(feature_sha),
            None,
            Some(2),
            Some(2),
        )
        .await
        .unwrap_err();
        assert_eq!(degraded.code, "content_unavailable");

        // A symbolic start ref that fails keeps unresolvable_ref: a missing
        // ref is not "content gone".
        let symbolic = commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some("refs/heads/feature".into()),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(symbolic.code, "unresolvable_ref");

        std::fs::remove_dir_all(repo).unwrap();
    }
}
