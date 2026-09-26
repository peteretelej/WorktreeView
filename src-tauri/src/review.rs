use crate::cache;
use crate::git::{
    acceptable_diff_exit, batch_fragment, configured_filter_names, effective_head_ref,
    ensure_work_tree, filter_override_args, fragment_sha, git_args, git_execution_error,
    no_index_diff_fragment, parse_applicable_filter_paths, parse_commits, parse_configured_filter_names,
    parse_name_status, parse_numstat, parse_untracked_paths, partial_clone_failure,
    primary_branch, read_target, read_worktree_target, reject_applicable_filters,
    resolve_empty_tree, resolve_ref, run_git, run_git_with_stdin, run_remote_batch,
    run_remote_batch_with, run_remote_read_with, stdin_git_command, unsupported_filter_error,
    validate_file, validate_ref, validate_scope_combination, validate_untracked_combination,
    validate_work_tree_with, BatchFragment, ChangedFile, CommitInfo, CommitPage, ReadTarget,
    RemoteRead, RemoteTarget, MAX_OUTPUT,
};
use crate::retrospection;
use crate::CommandError;
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

// Same shape as FilePatch: the renderable content of the reviewed file on the
// patch's new side, for context expansion and the full-file view.
#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct FileContent {
    pub(crate) binary: bool,
    pub(crate) text: String,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct CommitDetail {
    pub sha: String,
    pub author: String,
    pub date: String,
    pub subject: String,
    pub body: String,
    pub parents: Vec<String>,
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
    // The pathspec is literal, so a name containing glob metacharacters
    // still matches itself; bounding the check to the selected path avoids
    // a full worktree walk on every untracked patch read.
    let pathspec = format!(":(literal){file}");
    let (exit_code, stdout, stderr) = run_git(
        root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            &pathspec,
        ],
    )
    .await?;
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

enum BoundedRead {
    File(Vec<u8>),
    Missing,
}

// Reads a file under `root` through cap-std so a relative path cannot escape
// the root, refuses anything that is not a regular file, and bounds the read
// at MAX_OUTPUT like every other review output.
fn open_bounded_file(root: &Path, file: &str) -> Result<BoundedRead, CommandError> {
    let root = Dir::open_ambient_dir(root, ambient_authority()).map_err(|_| {
        CommandError::new("invalid_path", "The selected folder could not be opened.")
    })?;
    let mut options = OpenOptions::new();
    options.read(true).nonblock(true);
    let mut opened = match root.open_with(file, &options) {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BoundedRead::Missing),
        Err(_) => {
            return Err(CommandError::new(
                "invalid_path",
                "The selected file could not be opened.",
            ))
        }
    };
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
    Ok(BoundedRead::File(output))
}

fn open_untracked_file(root: &Path, file: &str) -> Result<Vec<u8>, CommandError> {
    match open_bounded_file(root, file)? {
        BoundedRead::File(output) => Ok(output),
        BoundedRead::Missing => Err(CommandError::new(
            "invalid_path",
            "The selected file could not be opened.",
        )),
    }
}

// The remote-tracking ref a remote branch's review defaults against: the
// remote's own HEAD when the clone recorded it, else its main/master/develop.
// The local primary checkout is often weeks stale on machines that review
// remote branches without checking them out, so the remote's own state is
// the fork point that shows only the branch's changes.
fn remote_default_ref<'a>(remotes: &'a [String], target_ref: &str) -> Option<&'a str> {
    let (remote, _) = target_ref.strip_prefix("refs/remotes/")?.split_once('/')?;
    let candidates = [
        format!("refs/remotes/{remote}/HEAD"),
        format!("refs/remotes/{remote}/main"),
        format!("refs/remotes/{remote}/master"),
        format!("refs/remotes/{remote}/develop"),
    ];
    candidates.iter().find_map(|candidate| {
        remotes
            .iter()
            .find(|reference| **reference == *candidate)
            .map(|reference| reference.as_str())
    })
}

pub(crate) async fn refs_inventory(
    pool: &SqlitePool,
    path: String,
    worktree_branch: Option<String>,
    target_ref: Option<String>,
) -> Result<RefInventory, CommandError> {
    if let Some(worktree_branch) = &worktree_branch {
        validate_ref(worktree_branch, "worktree_branch")?;
    }
    if let Some(target_ref) = &target_ref {
        validate_ref(target_ref, "target_ref")?;
    }
    match read_target(pool, &path).await? {
        ReadTarget::Local(path) => refs_inventory_local(path, worktree_branch, target_ref).await,
        ReadTarget::Remote(target) => refs_inventory_remote(&target, worktree_branch, target_ref).await,
    }
}

fn split_ref_classes(stdout: &[u8]) -> Result<(Vec<String>, Vec<String>, Vec<String>), CommandError> {
    let text = std::str::from_utf8(stdout)
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
    Ok((heads, remotes, tags))
}

async fn refs_inventory_local(
    path: std::path::PathBuf,
    worktree_branch: Option<String>,
    target_ref: Option<String>,
) -> Result<RefInventory, CommandError> {
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
    let (heads, remotes, tags) = split_ref_classes(&stdout)?;
    let primary = primary_branch(&heads);
    let remote_base = target_ref
        .as_deref()
        .and_then(|reference| remote_default_ref(&remotes, reference));
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
    } else if let (Some(target_ref), Some(base_ref)) = (&target_ref, remote_base) {
        // A remote branch reviews from its fork point: the merge-base
        // against the remote's own default ref, so the local primary's
        // staleness cannot inflate the change list. Unrelated histories
        // fall back to the plain primary-branch base.
        let args = vec!["merge-base".into(), target_ref.clone(), base_ref.into()];
        let args = git_args(&args);
        match run_git(&path, &args).await? {
            (0, stdout, _) => match std::str::from_utf8(&stdout).ok().map(str::trim) {
                Some(sha) if !sha.is_empty() => Some(sha.to_string()),
                _ => primary.map(str::to_string),
            },
            _ => primary.map(str::to_string),
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

// The remote inventory batches the conditional default-base probes into one
// second invocation: the rev-parse and merge-base fragments are independent
// of each other, and the caller reads them the way the local probes ran.
async fn refs_inventory_remote(
    target: &RemoteTarget,
    worktree_branch: Option<String>,
    target_ref: Option<String>,
) -> Result<RefInventory, CommandError> {
    let first = run_remote_batch(
        target,
        &[batch_fragment(vec![
            "for-each-ref".into(),
            "refs/heads".into(),
            "refs/remotes".into(),
            "refs/tags".into(),
            "--format=%(refname)".into(),
        ])],
    )
    .await?;
    let (heads, remotes, tags) = split_ref_classes(&first.ok(0)?.stdout)?;
    let primary = primary_branch(&heads);
    let remote_base = target_ref
        .as_deref()
        .and_then(|reference| remote_default_ref(&remotes, reference));
    let default_base = if worktree_branch.as_deref() == Some("detached") {
        None
    } else if let (Some(worktree_branch), Some(primary)) = (&worktree_branch, primary) {
        let second = run_remote_batch(
            target,
            &[
                batch_fragment(vec![
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    worktree_branch.clone(),
                ]),
                batch_fragment(vec![
                    "merge-base".into(),
                    worktree_branch.clone(),
                    primary.to_string(),
                ]),
            ],
        )
        .await?;
        if second.ok(0).is_err() {
            None
        } else {
            match second.ok(1) {
                Ok(fragment) => match String::from_utf8_lossy(&fragment.stdout).trim() {
                    "" => None,
                    sha => Some(sha.to_string()),
                },
                Err(_) => None,
            }
        }
    } else if let (Some(target_ref), Some(base_ref)) = (&target_ref, remote_base) {
        // A remote branch reviews from its fork point: the merge-base
        // against the remote's own default ref, so the primary's staleness
        // cannot inflate the change list. Unrelated histories fall back to
        // the plain primary-branch base.
        let second = run_remote_batch(
            target,
            &[batch_fragment(vec![
                "merge-base".into(),
                target_ref.clone(),
                base_ref.into(),
            ])],
        )
        .await?;
        match second.ok(0) {
            Ok(fragment) => match String::from_utf8_lossy(&fragment.stdout).trim() {
                "" => primary.map(str::to_string),
                sha => Some(sha.to_string()),
            },
            Err(_) => primary.map(str::to_string),
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

// One commit's identity: full SHA, author, commit time, title, body, and
// parents. The rev may be an abbreviated hash, so resolution runs before the
// read; an unresolvable rev is the caller's "no such commit" signal, not an
// execution failure.
pub(crate) async fn commit_detail(
    pool: &SqlitePool,
    path: String,
    rev: String,
) -> Result<CommitDetail, CommandError> {
    validate_ref(&rev, "rev")?;
    match read_target(pool, &path).await? {
        ReadTarget::Local(path) => commit_detail_local(&path, &rev).await,
        ReadTarget::Remote(target) => commit_detail_remote(&target, &rev).await,
    }
}

async fn commit_detail_local(path: &Path, rev: &str) -> Result<CommitDetail, CommandError> {
    let sha = resolve_ref(path, rev).await?;
    let owned_args = vec![
        "log".into(),
        "-1".into(),
        // Every field separates on NUL: git forbids NUL in identities and
        // subject lines, so a separator byte cannot appear inside an early
        // field and shift the record, and splitn keeps any remainder in the
        // body. The date rides as git's own default rendering (the C locale
        // keeps the abbreviations stable).
        "--format=%H%x00%an%x00%cd%x00%P%x00%s%x00%b".into(),
        sha.clone(),
    ];
    let args = git_args(&owned_args);
    let (exit_code, stdout, stderr) = run_git(path, &args).await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    parse_commit_detail(&stdout, &sha)
}

// The remote twin rides one batched invocation: the log fragment addresses
// the raw rev expression, which names the same commit the resolved SHA
// names, so resolution and read land together.
async fn commit_detail_remote(target: &RemoteTarget, rev: &str) -> Result<CommitDetail, CommandError> {
    let batch = run_remote_batch(
        target,
        &[
            batch_fragment(vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                format!("{rev}^{{commit}}"),
            ]),
            batch_fragment(vec![
                "log".into(),
                "-1".into(),
                "--format=%H%x00%an%x00%cd%x00%P%x00%s%x00%b".into(),
                rev.to_string(),
            ]),
        ],
    )
    .await?;
    let resolved = match batch.ok(0) {
        Ok(fragment) if !fragment.stdout.is_empty() => fragment,
        // The caller's "no such commit" signal, exactly as the local
        // resolution reports it.
        _ => {
            return Err(CommandError::new(
                "unresolvable_ref",
                format!("The ref '{rev}' does not resolve to a commit."),
            ))
        }
    };
    let sha = String::from_utf8_lossy(&resolved.stdout).trim().to_string();
    let fragment = batch.ok(1)?;
    parse_commit_detail(&fragment.stdout, &sha)
}

fn parse_commit_detail(output: &[u8], sha: &str) -> Result<CommitDetail, CommandError> {
    let malformed = || {
        CommandError::new(
            "git_output_malformed",
            "Git returned malformed commit data.",
        )
    };
    let text = std::str::from_utf8(output).map_err(|_| malformed())?;
    let record = text.trim_end_matches('\n');
    let fields: Vec<&str> = record.splitn(6, '\0').collect();
    if fields.len() < 6 {
        return Err(malformed());
    }
    let parents: Vec<String> = fields[3].split_whitespace().map(str::to_string).collect();
    // Parents are always full hex SHAs in %P; anything else means the record
    // is not shaped like a log entry.
    if parents
        .iter()
        .any(|parent| !parent.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(malformed());
    }
    Ok(CommitDetail {
        sha: sha.to_string(),
        author: fields[1].to_string(),
        date: fields[2].to_string(),
        subject: fields[4].to_string(),
        parents,
        body: fields[5].trim_end().to_string(),
    })
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
    match read_worktree_target(pool, repo_path, &path).await? {
        ReadTarget::Local(path) => {
            commit_page_local(pool, repo_path, path, start_ref, against, skip, limit).await
        }
        ReadTarget::Remote(target) => {
            commit_page_remote(pool, repo_path, &target, start_ref, against, skip, limit).await
        }
    }
}

// Open-time retrospection shared by the local and remote history pages:
// record a surface only for opens that resolved a real one. A HEAD-based
// open is the worktree surface, labeled from the page's `HEAD ->`
// decoration (empty when detached, keeping any recorded label); an explicit
// `refs/heads/` open is the branch surface; tags, raw SHAs, and rev
// expressions record nothing, so reopening a gone surface by SHA leaves
// `last_seen` at "last open while alive".
async fn record_page_surface(
    pool: &SqlitePool,
    repo_path: &str,
    branch_start: Option<String>,
    explicit_start: bool,
    worktree_key: &str,
    commits: &[CommitInfo],
    start_sha: &str,
) {
    if let Some(branch_ref) = branch_start {
        let label = branch_ref.strip_prefix("refs/heads/").unwrap_or(&branch_ref);
        retrospection::record_surface_open(
            pool,
            repo_path,
            "branch",
            &branch_ref,
            label,
            &branch_ref,
            start_sha,
        )
        .await;
    } else if !explicit_start {
        let label = head_decoration_label(commits);
        retrospection::record_surface_open(
            pool,
            repo_path,
            "worktree",
            worktree_key,
            &label,
            worktree_key,
            start_sha,
        )
        .await;
    }
}

async fn commit_page_local(
    pool: &SqlitePool,
    repo_path: &str,
    path: std::path::PathBuf,
    start_ref: Option<String>,
    against: Option<String>,
    skip: Option<u32>,
    limit: Option<u16>,
) -> Result<CommitPage, CommandError> {
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
    let worktree_key = path.to_str().unwrap_or_default().to_string();
    record_page_surface(
        pool,
        repo_path,
        branch_start,
        explicit_start,
        &worktree_key,
        &page.commits,
        &start_sha,
    )
    .await;
    Ok(page)
}

// The remote history page rides at most two batched invocations: the
// resolutions, the page itself, and the ancestry probe when the marks are
// not cached. The log fragment addresses the start ref expression directly,
// so a symbolic page needs only one round trip before the probe.
async fn commit_page_remote(
    pool: &SqlitePool,
    repo_path: &str,
    target: &RemoteTarget,
    start_ref: Option<String>,
    against: Option<String>,
    skip: Option<u32>,
    limit: Option<u16>,
) -> Result<CommitPage, CommandError> {
    let explicit_start = start_ref.is_some();
    let start_ref = effective_head_ref(start_ref);
    let branch_start = if explicit_start && start_ref.starts_with("refs/heads/") {
        Some(start_ref.clone())
    } else {
        None
    };
    let skip = skip.unwrap_or(0);
    let limit = usize::from(limit.unwrap_or(100).min(100));
    let literal = literal_sha(&start_ref);

    let mut fragments: Vec<BatchFragment> = Vec::new();
    let start_slot = if literal {
        None
    } else {
        let slot = fragments.len();
        fragments.push(batch_fragment(vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("{start_ref}^{{commit}}"),
        ]));
        Some(slot)
    };
    let against_slot = against.as_ref().map(|against| {
        let slot = fragments.len();
        fragments.push(batch_fragment(vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("{against}^{{commit}}"),
        ]));
        slot
    });
    let log_slot = {
        let slot = fragments.len();
        let mut owned_args = vec![
            "log".into(),
            format!("--skip={skip}"),
            format!("--max-count={}", limit + 1),
            "--decorate=full".into(),
            "--format=%H%x1f%s%x1f%an%x1f%aI%x1f%D%x1f%P%x1e".into(),
        ];
        if explicit_start {
            owned_args.push(start_ref.clone());
        }
        fragments.push(batch_fragment(owned_args));
        slot
    };
    let batch = run_remote_batch(target, &fragments).await?;

    let mut start_sha = match start_slot {
        Some(slot) => fragment_sha(&batch, slot, &start_ref)?,
        None => start_ref.clone(),
    };
    let against_sha = match against_slot {
        Some(slot) => Some(fragment_sha(&batch, slot, against.as_deref().unwrap_or(""))?),
        None => None,
    };
    // A literal start consults the cache before paying its resolution; a
    // miss resolves it once, so a gc-pruned SHA still fails as
    // content_unavailable rather than as a broken page.
    let cached = if literal {
        cache::lookup_log_page(
            pool,
            repo_path,
            &start_ref,
            against_sha.as_deref().unwrap_or(""),
            skip,
            limit,
        )
        .await
    } else {
        None
    };
    if literal && cached.is_none() {
        let resolution = run_remote_batch(
            target,
            &[batch_fragment(vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                format!("{start_ref}^{{commit}}"),
            ])],
        )
        .await?;
        start_sha = fragment_sha(&resolution, 0, &start_ref).map_err(|error| {
            if error.code == "unresolvable_ref" {
                CommandError::new(
                    "content_unavailable",
                    "This surface's content is no longer available in the repository.",
                )
            } else {
                error
            }
        })?;
    }

    let log_fragment = batch.ok(log_slot)?;
    let mut commits = parse_commits(&log_fragment.stdout)?;
    let mut has_more = commits.len() > limit;
    commits.truncate(limit);
    match cached {
        // The cached page outranks the probe just read; serve it, with the
        // same marks logic as the local hit path.
        Some((cached_commits, cached_has_more)) => {
            commits = cached_commits;
            has_more = cached_has_more;
            if let Some(against_sha) = &against_sha {
                let shas: Vec<String> = commits.iter().map(|commit| commit.sha.clone()).collect();
                if let Some(non_ancestors) = cache::lookup_marks(pool, &shas, against_sha).await {
                    for (commit, non_ancestor) in commits.iter_mut().zip(non_ancestors) {
                        commit.default_base_ancestor = !non_ancestor;
                    }
                } else {
                    mark_default_base_ancestors_remote(target, &mut commits, against_sha).await?;
                    let non_ancestors: Vec<bool> = commits
                        .iter()
                        .map(|commit| !commit.default_base_ancestor)
                        .collect();
                    cache::store_marks(pool, &shas, against_sha, &non_ancestors).await;
                }
            }
        }
        None => {
            if let Some(against_sha) = &against_sha {
                mark_default_base_ancestors_remote(target, &mut commits, against_sha).await?;
            }
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
        }
    }
    record_page_surface(
        pool,
        repo_path,
        branch_start,
        explicit_start,
        // The remote worktree key is the caller's host-side path, compared
        // raw because local canonicalization cannot resolve it.
        &target.path,
        &commits,
        &start_sha,
    )
    .await;
    Ok(CommitPage { commits, has_more })
}

async fn mark_default_base_ancestors_remote(
    target: &RemoteTarget,
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
    let batch = run_remote_batch(target, &[batch_fragment(owned_args)]).await?;
    let fragment = batch.ok(0)?;
    let excluded = String::from_utf8_lossy(&fragment.stdout).into_owned();
    let non_ancestors: Vec<&str> = excluded.split_whitespace().collect();
    for commit in commits.iter_mut() {
        commit.default_base_ancestor = !non_ancestors.contains(&commit.sha.as_str());
    }
    Ok(())
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
    match read_worktree_target(pool, repo_path, &path).await? {
        ReadTarget::Local(path) => {
            review_changes_local(
                pool,
                repo_path,
                path,
                base,
                head_ref,
                explicit_head,
                committed_only,
                reversed,
            )
            .await
        }
        ReadTarget::Remote(target) => {
            review_changes_remote(
                pool,
                repo_path,
                &target,
                base,
                head_ref,
                explicit_head,
                committed_only,
                reversed,
            )
            .await
        }
    }
}

// The worktree target records with an empty label (the branch name is not in
// hand without an extra spawn; the CASE keeps the nicer label commit_page
// recorded), a branch target records the full ref, commit targets record
// nothing. Shared by the local and remote review loads.
async fn record_review_surface(
    pool: &SqlitePool,
    repo_path: &str,
    explicit_head: bool,
    head_ref: &str,
    worktree_key: &str,
    target_sha: &str,
) {
    if !explicit_head {
        retrospection::record_surface_open(
            pool,
            repo_path,
            "worktree",
            worktree_key,
            "",
            worktree_key,
            target_sha,
        )
        .await;
    } else if head_ref.starts_with("refs/heads/") {
        let label = head_ref.strip_prefix("refs/heads/").unwrap_or(head_ref);
        retrospection::record_surface_open(
            pool,
            repo_path,
            "branch",
            head_ref,
            label,
            head_ref,
            target_sha,
        )
        .await;
    }
}

async fn review_changes_local(
    pool: &SqlitePool,
    repo_path: &str,
    path: std::path::PathBuf,
    base: String,
    head_ref: String,
    explicit_head: bool,
    committed_only: bool,
    reversed: bool,
) -> Result<ReviewIndex, CommandError> {
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
    let worktree_key = path.to_str().unwrap_or_default().to_string();
    record_review_surface(
        pool,
        repo_path,
        explicit_head,
        &head_ref,
        &worktree_key,
        &target_sha,
    )
    .await;
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
        return Err(match partial_clone_failure(&path, &name_args).await {
            Some(failure) => failure,
            None => git_execution_error(&stderr),
        });
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
        return Err(match partial_clone_failure(&path, &num_args).await {
            Some(failure) => failure,
            None => git_execution_error(&stderr),
        });
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

// The remote review index batches into at most three invocations: the
// resolutions plus the working-tree extras (filter config, untracked
// listing) in one; the two diffs, whose ranges and filter overrides depend
// on those results, in a second; and the filter applicability probe, which
// needs the changed-file list, in a third only when filters are configured.
// Committed reviews with plain refs ride a single invocation.
async fn review_changes_remote(
    pool: &SqlitePool,
    repo_path: &str,
    target: &RemoteTarget,
    base: String,
    head_ref: String,
    explicit_head: bool,
    committed_only: bool,
    reversed: bool,
) -> Result<ReviewIndex, CommandError> {
    review_changes_remote_with(
        &crate::git::process_spawner(),
        pool,
        repo_path,
        target,
        base,
        head_ref,
        explicit_head,
        committed_only,
        reversed,
    )
    .await
}

async fn review_changes_remote_with(
    spawner: &crate::git::Spawner,
    pool: &SqlitePool,
    repo_path: &str,
    target: &RemoteTarget,
    base: String,
    head_ref: String,
    explicit_head: bool,
    committed_only: bool,
    reversed: bool,
) -> Result<ReviewIndex, CommandError> {
    let empty_tree_base = base == "empty-tree";
    let mut first: Vec<BatchFragment> = Vec::new();
    let base_slot = Some(first.len());
    if empty_tree_base {
        first.push(batch_fragment(vec![
            "hash-object".into(),
            "-t".into(),
            "tree".into(),
            "--stdin".into(),
        ]));
    } else {
        first.push(batch_fragment(vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("{base}^{{commit}}"),
        ]));
    }
    let head_slot = first.len();
    first.push(batch_fragment(vec![
        "rev-parse".into(),
        "--verify".into(),
        "--quiet".into(),
        format!("{head_ref}^{{commit}}"),
    ]));
    let filters_slot = if committed_only {
        None
    } else {
        let slot = first.len();
        first.push(batch_fragment(vec![
            "config".into(),
            "--get-regexp".into(),
            r"^filter\..*\.(clean|process)$".into(),
        ]));
        Some(slot)
    };
    let untracked_slot = if committed_only || reversed {
        None
    } else {
        let slot = first.len();
        first.push(batch_fragment(vec![
            "ls-files".into(),
            "--others".into(),
            "--exclude-standard".into(),
            "-z".into(),
        ]));
        Some(slot)
    };
    // A committed review over plain refs has no data dependencies: its two
    // diff fragments ride the same invocation as the resolutions. The
    // empty-tree range needs its tree hash first, and the working-changes
    // diffs need the configured filter overrides, so those split in two.
    let committed_one_shot = committed_only && !empty_tree_base;
    let mut diff_name_owned = Vec::new();
    let mut diff_num_owned = Vec::new();
    let one_shot_name_slot;
    let one_shot_num_slot;
    if committed_one_shot {
        diff_name_owned.extend(review_index_args("--name-status", &format!("{base}...{head_ref}"), reversed));
        diff_num_owned.extend(review_index_args("--numstat", &format!("{base}...{head_ref}"), reversed));
        one_shot_name_slot = first.len();
        first.push(batch_fragment(diff_name_owned.clone()));
        one_shot_num_slot = first.len();
        first.push(batch_fragment(diff_num_owned.clone()));
    } else {
        one_shot_name_slot = 0;
        one_shot_num_slot = 0;
    }
    let first_batch = run_remote_batch_with(spawner, target, &first).await?;
    let base_sha = if empty_tree_base {
        let fragment = first_batch.ok(base_slot.unwrap())?;
        let sha = String::from_utf8_lossy(&fragment.stdout).trim().to_string();
        if sha.is_empty() {
            return Err(CommandError::new(
                "git_output_malformed",
                "Git returned no empty-tree hash.",
            ));
        }
        sha
    } else {
        fragment_sha(&first_batch, base_slot.unwrap(), &base)?
    };
    let target_sha = fragment_sha(&first_batch, head_slot, &head_ref)?;
    // Read before the batch value moves into the one-shot path below.
    let untracked_paths = match untracked_slot {
        Some(slot) => parse_untracked_paths(&first_batch.ok(slot)?.stdout)?,
        None => Vec::new(),
    };
    record_review_surface(
        pool,
        repo_path,
        explicit_head,
        &head_ref,
        &target.path,
        &target_sha,
    )
    .await;
    // Exit 1 means no configured filters, mirroring the local read.
    let filters: Vec<String> = match filters_slot {
        Some(slot) => match first_batch.code(slot) {
            1 => Vec::new(),
            0 => parse_configured_filter_names(&first_batch.fragments[slot].stdout),
            _ => return Err(git_execution_error(&first_batch.stderr)),
        },
        None => Vec::new(),
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
    diff_name_owned = filter_override_args(&filters);
    diff_name_owned.extend(review_index_args("--name-status", &range, reversed));
    diff_num_owned = filter_override_args(&filters);
    diff_num_owned.extend(review_index_args("--numstat", &range, reversed));
    let (second_batch, name_slot, num_slot) = if committed_one_shot {
        (first_batch, one_shot_name_slot, one_shot_num_slot)
    } else {
        let second = run_remote_batch_with(
            spawner,
            target,
            &[batch_fragment(diff_name_owned), batch_fragment(diff_num_owned)],
        )
        .await?;
        (second, 0, 1)
    };
    let name_fragment = second_batch.ok(name_slot)?;
    let mut files = parse_name_status(&name_fragment.stdout)?;
    if !filters.is_empty() {
        let paths: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
        if !paths.is_empty() {
            let check = run_remote_batch_with(
                spawner,
                target,
                &[BatchFragment {
                    cwd: None,
                    args: vec![
                        "check-attr".into(),
                        "-z".into(),
                        "--stdin".into(),
                        "filter".into(),
                    ],
                    stdin: Some(paths),
                }],
            )
            .await?;
            let applicable = parse_applicable_filter_paths(&check.ok(0)?.stdout, &filters);
            if !applicable.is_empty() {
                return Err(unsupported_filter_error());
            }
        }
    }
    let num_fragment = second_batch.ok(num_slot)?;
    let (additions, deletions, _) = parse_numstat(&num_fragment.stdout)?;

    if !untracked_paths.is_empty() {
        for file in untracked_paths {
            files.push(ChangedFile {
                path: file,
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
    pool: &SqlitePool,
    path: String,
    repo_path: Option<String>,
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
    let empty_tree_base = base == "empty-tree";
    // The review identity's repo key classifies the target when the caller
    // supplies it; otherwise the path itself is the store key.
    match read_worktree_target(pool, repo_path.as_deref().unwrap_or(&path), &path).await? {
        ReadTarget::Local(path) => {
            review_patch_local(
                path,
                base,
                head_ref,
                committed_only,
                reversed,
                file,
                untracked,
                empty_tree_base,
            )
            .await
        }
        ReadTarget::Remote(target) => {
            if untracked {
                return review_patch_untracked_remote(&target, &file).await;
            }
            review_patch_remote(
                &target,
                base,
                head_ref,
                committed_only,
                reversed,
                file,
                empty_tree_base,
            )
            .await
        }
    }
}

// The remote untracked patch: the same no-index mechanism as the local
// capture, executed on the host where the file lives. One invocation carries
// the literal untracked check and the two diffs; exit 1 is no-index's
// "differences found" code, accepted exactly as locally.
async fn review_patch_untracked_remote(
    target: &RemoteTarget,
    file: &str,
) -> Result<FilePatch, CommandError> {
    review_patch_untracked_remote_with(&crate::git::process_spawner(), target, file).await
}

async fn review_patch_untracked_remote_with(
    spawner: &crate::git::Spawner,
    target: &RemoteTarget,
    file: &str,
) -> Result<FilePatch, CommandError> {
    let check = batch_fragment(vec![
        "ls-files".into(),
        "--others".into(),
        "--exclude-standard".into(),
        "-z".into(),
        "--".into(),
        format!(":(literal){file}"),
    ]);
    let numstat = no_index_diff_fragment(&target.path, file, true);
    let patch = no_index_diff_fragment(&target.path, file, false);
    let batch = run_remote_batch_with(spawner, target, &[check, numstat, patch]).await?;
    let listed = parse_untracked_paths(&batch.ok(0)?.stdout)?;
    if !listed.iter().any(|untracked| untracked == file) {
        return Err(CommandError::new(
            "invalid_path",
            "The selected file is not an untracked review file.",
        ));
    }
    if !acceptable_diff_exit(batch.code(1), true) {
        return Err(git_execution_error(&batch.stderr));
    }
    let (_, _, binary) = parse_numstat(&batch.fragments[1].stdout)?;
    if binary {
        return Ok(FilePatch {
            binary: true,
            text: String::new(),
        });
    }
    if !acceptable_diff_exit(batch.code(2), true) {
        return Err(git_execution_error(&batch.stderr));
    }
    let text = String::from_utf8(batch.fragments[2].stdout.clone()).map_err(|_| {
        CommandError::new("git_output_malformed", "Git returned invalid patch text.")
    })?;
    Ok(FilePatch { binary: false, text })
}

async fn review_patch_local(
    path: std::path::PathBuf,
    base: String,
    head_ref: String,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
    empty_tree_base: bool,
) -> Result<FilePatch, CommandError> {
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

// The remote patch read composes the filter check (working scopes) with the
// two diffs. Committed reviews with literal SHA endpoints and no filter
// check ride one invocation; empty-tree bases need their tree hash first.
async fn review_patch_remote(
    target: &RemoteTarget,
    base: String,
    head_ref: String,
    committed_only: bool,
    reversed: bool,
    file: String,
    empty_tree_base: bool,
) -> Result<FilePatch, CommandError> {
    let mut first: Vec<BatchFragment> = Vec::new();
    let base_slot = if empty_tree_base {
        let slot = first.len();
        first.push(batch_fragment(vec![
            "hash-object".into(),
            "-t".into(),
            "tree".into(),
            "--stdin".into(),
        ]));
        Some(slot)
    } else if literal_sha(&base) {
        None
    } else {
        let slot = first.len();
        first.push(batch_fragment(vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("{base}^{{commit}}"),
        ]));
        Some(slot)
    };
    let head_slot = if literal_sha(&head_ref) {
        None
    } else {
        let slot = first.len();
        first.push(batch_fragment(vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("{head_ref}^{{commit}}"),
        ]));
        Some(slot)
    };
    let filters_slot = if committed_only {
        None
    } else {
        let slot = first.len();
        first.push(batch_fragment(vec![
            "config".into(),
            "--get-regexp".into(),
            r"^filter\..*\.(clean|process)$".into(),
        ]));
        Some(slot)
    };
    let first_batch = run_remote_batch(target, &first).await?;
    // A missing endpoint must still fail as unresolvable_ref, base first
    // exactly like the local resolution order.
    if let (Some(slot), false) = (base_slot, empty_tree_base) {
        fragment_sha(&first_batch, slot, &base)?;
    }
    if let Some(slot) = head_slot {
        fragment_sha(&first_batch, slot, &head_ref)?;
    }
    if !committed_only {
        // Exit 1 means no configured filters, mirroring the local read.
        let filters: Vec<String> = match filters_slot {
            Some(slot) => match first_batch.code(slot) {
                1 => Vec::new(),
                0 => parse_configured_filter_names(&first_batch.fragments[slot].stdout),
                _ => return Err(git_execution_error(&first_batch.stderr)),
            },
            None => Vec::new(),
        };
        if !filters.is_empty() {
            let check = run_remote_batch(
                target,
                &[BatchFragment {
                    cwd: None,
                    args: vec![
                        "check-attr".into(),
                        "-z".into(),
                        "--stdin".into(),
                        "filter".into(),
                    ],
                    stdin: Some(vec![file.clone()]),
                }],
            )
            .await?;
            let applicable = parse_applicable_filter_paths(&check.ok(0)?.stdout, &filters);
            if !applicable.is_empty() {
                return Err(unsupported_filter_error());
            }
        }
    }
    let base_sha = if empty_tree_base {
        let fragment = first_batch.ok(base_slot.unwrap())?;
        let sha = String::from_utf8_lossy(&fragment.stdout).trim().to_string();
        if sha.is_empty() {
            return Err(CommandError::new(
                "git_output_malformed",
                "Git returned no empty-tree hash.",
            ));
        }
        sha
    } else {
        String::new()
    };
    let range = if committed_only {
        if empty_tree_base {
            format!("{base_sha}..{head_ref}")
        } else {
            format!("{base}...{head_ref}")
        }
    } else {
        base
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
    num_args.extend([range.clone(), "--".into(), file.clone()]);
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
    patch_args.extend([range, "--".into(), file]);
    let second = run_remote_batch(
        target,
        &[batch_fragment(num_args), batch_fragment(patch_args)],
    )
    .await?;
    let num_fragment = second.ok(0)?;
    let (_, _, binary) = parse_numstat(&num_fragment.stdout)?;
    if binary {
        return Ok(FilePatch {
            binary: true,
            text: String::new(),
        });
    }
    let patch_fragment = second.ok(1)?;
    let text = String::from_utf8(patch_fragment.stdout.clone()).map_err(|_| {
        CommandError::new("git_output_malformed", "Git returned invalid patch text.")
    })?;
    Ok(FilePatch {
        binary: false,
        text,
    })
}

// Git's binary heuristic: a NUL in the leading bytes marks binary content.
fn content_is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&byte| byte == 0)
}

// The blob at `<rev>:<file>`, or empty when the file does not exist at that
// revision (a file deleted on this side has no content to show). Resolving
// through ls-tree keeps absence a plain empty result instead of a parsed
// error diagnostic, and the blob SHA means cat-file never sees a combined
// `rev:path` name.
async fn read_committed_file(
    root: &Path,
    rev: &str,
    file: &str,
) -> Result<Vec<u8>, CommandError> {
    let (exit_code, stdout, stderr) = run_git(root, &["ls-tree", "-z", rev, "--", file]).await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let entry = stdout.split(|&byte| byte == 0).next().unwrap_or(&[]);
    let meta = match entry.iter().position(|&byte| byte == b'\t') {
        Some(position) => &entry[..position],
        None => &[],
    };
    let fields = std::str::from_utf8(meta)
        .map_err(|_| CommandError::new("git_output_malformed", "Git returned invalid tree data."))?
        .split_whitespace()
        .collect::<Vec<_>>();
    let sha = match fields.as_slice() {
        [_, "blob", sha] => *sha,
        _ => return Ok(Vec::new()),
    };
    let (exit_code, stdout, stderr) = run_git(root, &["cat-file", "blob", sha]).await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    Ok(stdout)
}

// The merge base of two revs, or None when they share no history. Three-dot
// committed ranges diff from this point, and with no merge base Git diffs
// against the empty tree.
async fn merge_base_or_none(
    root: &Path,
    left: &str,
    right: &str,
) -> Result<Option<String>, CommandError> {
    let (exit_code, stdout, _) = run_git(root, &["merge-base", left, right]).await?;
    if exit_code != 0 {
        return Ok(None);
    }
    let sha = std::str::from_utf8(&stdout)
        .map_err(|_| CommandError::new("git_output_malformed", "Git returned invalid ref data."))?
        .trim()
        .to_string();
    Ok(if sha.is_empty() { None } else { Some(sha) })
}

// The remote twin of the local merge-base probe, through the injected
// spawner: None when the revs share no history.
async fn merge_base_or_none_remote_with(
    spawner: &crate::git::Spawner,
    target: &RemoteTarget,
    left: &str,
    right: &str,
) -> Result<Option<String>, CommandError> {
    let batch = run_remote_batch_with(
        spawner,
        target,
        &[batch_fragment(vec![
            "merge-base".into(),
            left.to_string(),
            right.to_string(),
        ])],
    )
    .await?;
    if batch.code(0) != 0 {
        return Ok(None);
    }
    let sha = String::from_utf8_lossy(&batch.fragments[0].stdout).trim().to_string();
    Ok(if sha.is_empty() { None } else { Some(sha) })
}

// The remote twin of the ls-tree-then-cat-file read: the blob SHA comes
// from ls-tree's output so no assembled `rev:path` name reaches Git.
async fn read_committed_file_remote_with(
    spawner: &crate::git::Spawner,
    target: &RemoteTarget,
    rev: &str,
    file: &str,
) -> Result<Vec<u8>, CommandError> {
    let listed = run_remote_batch_with(
        spawner,
        target,
        &[batch_fragment(vec![
            "ls-tree".into(),
            "-z".into(),
            rev.to_string(),
            "--".into(),
            file.to_string(),
        ])],
    )
    .await?;
    let fragment = listed.ok(0)?;
    let entry = fragment.stdout.split(|&byte| byte == 0).next().unwrap_or(&[]);
    let meta = match entry.iter().position(|&byte| byte == b'\t') {
        Some(position) => &entry[..position],
        None => &[],
    };
    let fields = std::str::from_utf8(meta)
        .map_err(|_| CommandError::new("git_output_malformed", "Git returned invalid tree data."))?
        .split_whitespace()
        .collect::<Vec<_>>();
    let sha = match fields.as_slice() {
        [_, "blob", sha] => *sha,
        _ => return Ok(Vec::new()),
    };
    let loaded = run_remote_batch_with(
        spawner,
        target,
        &[batch_fragment(vec![
            "cat-file".into(),
            "blob".into(),
            sha.to_string(),
        ])],
    )
    .await?;
    Ok(loaded.ok(0)?.stdout.clone())
}

// The file content the displayed patch's new side points at: the checked-out
// worktree file for working-changes scopes, otherwise the blob at the target
// (or, reversed, at the base, resolved through the merge base for three-dot
// committed ranges). Empty content means "no file on this side", not an
// error.
pub(crate) async fn review_file_content(
    pool: &SqlitePool,
    path: String,
    repo_path: Option<String>,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
) -> Result<FileContent, CommandError> {
    let bytes = resolve_review_file_bytes(
        pool,
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
    if content_is_binary(&bytes) {
        return Ok(FileContent {
            binary: true,
            text: String::new(),
        });
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        CommandError::new("git_output_malformed", "Git returned invalid file text.")
    })?;
    Ok(FileContent {
        binary: false,
        text,
    })
}

// The same new-side resolution as review_file_content, but returning the raw
// bytes: renderable assets (images) need them without the binary check or
// UTF-8 decode. Empty bytes mean "no file on this side", not an error.
pub(crate) async fn review_file_bytes(
    pool: &SqlitePool,
    path: String,
    repo_path: Option<String>,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
) -> Result<Vec<u8>, CommandError> {
    resolve_review_file_bytes(
        pool,
        path,
        repo_path,
        base,
        head_ref,
        committed_only,
        reversed,
        file,
        untracked,
    )
    .await
}

async fn resolve_review_file_bytes(
    pool: &SqlitePool,
    path: String,
    repo_path: Option<String>,
    base: String,
    head_ref: Option<String>,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
) -> Result<Vec<u8>, CommandError> {
    validate_ref(&base, "base")?;
    let head_ref = effective_head_ref(head_ref);
    validate_ref(&head_ref, "head_ref")?;
    validate_scope_combination(&base, &head_ref, committed_only)?;
    validate_file(&file)?;
    validate_untracked_combination(untracked, committed_only, reversed)?;
    match read_worktree_target(pool, repo_path.as_deref().unwrap_or(&path), &path).await? {
        ReadTarget::Local(path) => {
            resolve_review_file_bytes_local(path, base, head_ref, committed_only, reversed, file, untracked)
                .await
        }
        ReadTarget::Remote(target) => {
            resolve_review_file_bytes_remote(
                &target, base, head_ref, committed_only, reversed, file, untracked,
            )
            .await
        }
    }
}

async fn resolve_review_file_bytes_local(
    path: std::path::PathBuf,
    base: String,
    head_ref: String,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
) -> Result<Vec<u8>, CommandError> {
    if untracked {
        return capture_untracked_file(&path, &file).await;
    }
    if reversed {
        if base == "empty-tree" {
            return Ok(Vec::new());
        }
        if committed_only {
            return match merge_base_or_none(&path, &base, &head_ref).await? {
                Some(merge_base) => Ok(read_committed_file(&path, &merge_base, &file).await?),
                None => Ok(Vec::new()),
            };
        }
        return Ok(read_committed_file(&path, &base, &file).await?);
    }
    if !committed_only {
        // The only review read that touches the filesystem without Git doing
        // the walking: pin the root to a real worktree first so a
        // renderer-supplied directory cannot read arbitrary files.
        ensure_work_tree(&path).await?;
        return match open_bounded_file(&path, &file)? {
            BoundedRead::File(bytes) => Ok(bytes),
            BoundedRead::Missing => Ok(Vec::new()),
        };
    }
    Ok(read_committed_file(&path, &head_ref, &file).await?)
}

// The remote new side: committed and reversed scopes read pure Git
// (merge-base plus ls-tree and cat-file); working-changes and untracked
// content ride the bounded read command with the worktree pinned first,
// mirroring the local cap-std reads. Raw bytes return untouched, so the
// byte path serves images; the NUL binary check stays caller-side on the
// text path exactly as locally.
async fn resolve_review_file_bytes_remote(
    target: &RemoteTarget,
    base: String,
    head_ref: String,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
) -> Result<Vec<u8>, CommandError> {
    resolve_review_file_bytes_remote_with(
        &crate::git::process_spawner(),
        target,
        base,
        head_ref,
        committed_only,
        reversed,
        file,
        untracked,
    )
    .await
}

async fn resolve_review_file_bytes_remote_with(
    spawner: &crate::git::Spawner,
    target: &RemoteTarget,
    base: String,
    head_ref: String,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
) -> Result<Vec<u8>, CommandError> {
    if untracked {
        let listed = run_remote_batch_with(
            spawner,
            target,
            &[batch_fragment(vec![
                "ls-files".into(),
                "--others".into(),
                "--exclude-standard".into(),
                "-z".into(),
                "--".into(),
                format!(":(literal){file}"),
            ])],
        )
        .await?;
        let paths = parse_untracked_paths(&listed.ok(0)?.stdout)?;
        if !paths.iter().any(|path| path == &file) {
            return Err(CommandError::new(
                "invalid_path",
                "The selected file is not an untracked review file.",
            ));
        }
        return match run_remote_read_with(spawner, target, &file).await? {
            // A file that vanished between the inventory check and the read
            // fails like the local capture does.
            RemoteRead::Missing => Err(CommandError::new(
                "invalid_path",
                "The selected file could not be opened.",
            )),
            RemoteRead::File(bytes) => Ok(bytes),
        };
    }
    if !committed_only && !reversed {
        // The working-changes read pins the root to a verified worktree
        // first, exactly as the local cap-std read does.
        validate_work_tree_with(spawner, target).await?;
        return match run_remote_read_with(spawner, target, &file).await? {
            // Absence is empty content ("no file on this side"), never an
            // error.
            RemoteRead::Missing => Ok(Vec::new()),
            RemoteRead::File(bytes) => Ok(bytes),
        };
    }
    if reversed {
        if base == "empty-tree" {
            return Ok(Vec::new());
        }
        if committed_only {
            return match merge_base_or_none_remote_with(spawner, target, &base, &head_ref).await? {
                Some(merge_base) => {
                    read_committed_file_remote_with(spawner, target, &merge_base, &file).await
                }
                None => Ok(Vec::new()),
            };
        }
        return read_committed_file_remote_with(spawner, target, &base, &file).await;
    }
    read_committed_file_remote_with(spawner, target, &head_ref, &file).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::read_bounded;
    use crate::git::batch_fake_spawner;
    use crate::git::hybrid_fake_spawner;
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
            &pool,
            repo.to_str().unwrap().into(),
            Some("refs/heads/feature".into()),
            None,
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

        let ref_context = refs_inventory(&pool, repo.to_str().unwrap().into(), None, None)
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
            &pool,
            repo.to_str().unwrap().into(),
            None,
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
            let pool = test_pool().await;
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
        let pool = test_pool().await;
        std::fs::write(repo.join("tracked.txt"), "base\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "base"]);
        let base_sha = test_rev_parse(&repo, "HEAD");
        std::fs::write(repo.join("tracked.txt"), "changed\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "changed"]);
        let head_sha = test_rev_parse(&repo, "HEAD");

        let patch = review_patch(
            &pool,
            repo.to_str().unwrap().into(),
            None,
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
            &pool,
            repo.to_str().unwrap().into(),
            None,
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
            &pool,
            repo.to_str().unwrap().into(),
            None,
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
    async fn untracked_patch_checks_the_selected_path_literally() {
        let pool = test_pool().await;
        let repo = test_repo("untracked-literal-pathspec");
        std::fs::write(repo.join("foo[1].txt"), "bracket\n").unwrap();

        // A glob pathspec would not match this literal name; the literal
        // pathspec must, or bracketed names would render as invalid_path.
        let bracketed = review_patch(
            &pool,
            repo.to_str().unwrap().into(),
            None,
            "HEAD".into(),
            None,
            false,
            false,
            "foo[1].txt".into(),
            true,
        )
        .await
        .unwrap();
        assert!(bracketed.text.contains("bracket"));

        // A near-miss name that the path could glob-expand to stays invalid.
        let near_miss = review_patch(
            &pool,
            repo.to_str().unwrap().into(),
            None,
            "HEAD".into(),
            None,
            false,
            false,
            "foo1.txt".into(),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(near_miss.code, "invalid_path");

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn untracked_patch_requires_current_git_inventory() {
        let pool = test_pool().await;
        let repo = test_repo("untracked-inventory");
        std::fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(repo.join("ignored.txt"), "ignored\n").unwrap();
        std::fs::write(repo.join("new.txt"), "new\n").unwrap();
        std::fs::write(repo.join("binary.bin"), [b'a', 0, b'b']).unwrap();

        let patch = review_patch(
            &pool,
            repo.to_str().unwrap().into(),
            None,
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
            &pool,
            repo.to_str().unwrap().into(),
            None,
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
            &pool,
            repo.to_str().unwrap().into(),
            None,
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
            &pool,
            arbitrary.to_str().unwrap().into(),
            None,
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
            &pool,
            repo.to_str().unwrap().into(),
            None,
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

    #[tokio::test]
    async fn review_file_content_follows_the_patch_new_side() {
        let repo = test_repo("file-content-sides");
        let pool = test_pool().await;
        let base_sha = test_rev_parse(&repo, "HEAD");
        std::fs::write(repo.join("tracked.txt"), "committed change\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "advance"]);
        let head_sha = test_rev_parse(&repo, "HEAD");

        // Committed scope reads the blob at the head endpoint.
        let committed = review_file_content(
            &pool,
repo.to_str().unwrap().into(),
            None,
            base_sha.clone(),
            Some(head_sha.clone()),
            true,
            false,
            "tracked.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(committed.text, "committed change\n");

        // Reversed commits read the blob at the base endpoint.
        let reversed = review_file_content(
            &pool,
repo.to_str().unwrap().into(),
            None,
            base_sha.clone(),
            Some(head_sha.clone()),
            true,
            true,
            "tracked.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(reversed.text, "original\n");

        // Working-changes scope reads the checked-out file, uncommitted
        // edits included; a file deleted on this side reads as empty
        // content, not an error.
        std::fs::write(repo.join("tracked.txt"), "working edit\n").unwrap();
        let working = review_file_content(
            &pool,
repo.to_str().unwrap().into(),
            None,
            base_sha.clone(),
            None,
            false,
            false,
            "tracked.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(working.text, "working edit\n");
        std::fs::remove_file(repo.join("tracked.txt")).unwrap();
        let deleted = review_file_content(
            &pool,
repo.to_str().unwrap().into(),
            None,
            base_sha.clone(),
            None,
            false,
            false,
            "tracked.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert!(deleted.text.is_empty());
        assert!(!deleted.binary);

        // Untracked files read through the same verified capture as patches.
        std::fs::write(repo.join("fresh.txt"), "untracked\n").unwrap();
        let untracked = review_file_content(
            &pool,
repo.to_str().unwrap().into(),
            None,
            base_sha.clone(),
            None,
            false,
            false,
            "fresh.txt".into(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(untracked.text, "untracked\n");

        // Binary content keeps the binary flag instead of lossy text.
        std::fs::write(repo.join("blob.bin"), [b'a', 0, b'b']).unwrap();
        let binary = review_file_content(
            &pool,
repo.to_str().unwrap().into(),
            None,
            base_sha.clone(),
            None,
            false,
            false,
            "blob.bin".into(),
            true,
        )
        .await
        .unwrap();
        assert!(binary.binary);

        // Raw byte reads skip the binary check: renderable assets need the
        // exact bytes, not a lossy flag.
        let bytes = review_file_bytes(
            &pool,
repo.to_str().unwrap().into(),
            None,
            base_sha.clone(),
            None,
            false,
            false,
            "blob.bin".into(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(bytes, [b'a', 0, b'b']);

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn review_file_content_bounds_and_refuses_like_patches() {
        let pool = test_pool().await;
        let repo = test_repo("file-content-bounds");
        let base_sha = test_rev_parse(&repo, "HEAD");

        // A file beyond the output bound is refused, not truncated.
        std::fs::write(repo.join("huge.txt"), vec![b'x'; MAX_OUTPUT + 1]).unwrap();
        let oversized = review_file_content(
            &pool,
repo.to_str().unwrap().into(),
            None,
            base_sha.clone(),
            None,
            false,
            false,
            "huge.txt".into(),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(oversized.code, "git_output_too_large");

        // A path outside the worktree or with traversal is refused.
        let escape = review_file_content(
            &pool,
repo.to_str().unwrap().into(),
            None,
            base_sha.clone(),
            None,
            false,
            false,
            "../outside.txt".into(),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(escape.code, "invalid_path");

        // A file absent at the resolved revision reads as empty content.
        let absent = review_file_content(
            &pool,
repo.to_str().unwrap().into(),
            None,
            base_sha.clone(),
            Some(base_sha.clone()),
            true,
            false,
            "missing.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert!(absent.text.is_empty());

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn review_file_content_resolves_the_merge_base_for_reversed_commits() {
        let repo = test_repo("file-content-merge-base");
        let pool = test_pool().await;
        // Feature forks the initial commit; the base branch then advances
        // past the fork, so the displayed reversed patch's new side is the
        // merge base, not the base branch tip.
        test_git(&repo, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(repo.join("tracked.txt"), "feature change\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "feature"]);
        let feature_sha = test_rev_parse(&repo, "HEAD");
        test_git(&repo, &["checkout", "--quiet", "-"]);
        std::fs::write(repo.join("tracked.txt"), "base advance\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "advance"]);
        let base_tip = test_rev_parse(&repo, "HEAD");

        let reversed = review_file_content(
            &pool,
repo.to_str().unwrap().into(),
            None,
            base_tip,
            Some(feature_sha),
            true,
            true,
            "tracked.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(reversed.text, "original\n");

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn review_file_content_refuses_directories_outside_repositories() {
        // The working-changes branch reads the filesystem directly, so a
        // renderer-supplied plain folder must be refused like any other
        // non-repository path.
        let plain = test_path("file-content-not-repo");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join("secret.txt"), "nope\n").unwrap();
        let pool = test_pool().await;
        let refused = review_file_content(
            &pool,
            plain.to_str().unwrap().into(),
            None,
            "HEAD".into(),
            None,
            false,
            false,
            "secret.txt".into(),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(refused.code, "not_git_repository");

        std::fs::remove_dir_all(plain).unwrap();
    }

    #[tokio::test]
    async fn commit_detail_resolves_hashes_and_reads_bodies() {
        let pool = test_pool().await;
        let repo = test_repo("commit-detail");
        let root_sha = test_rev_parse(&repo, "HEAD");
        std::fs::write(repo.join("tracked.txt"), "changed\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(
            &repo,
            &[
                "commit",
                "--quiet",
                "-m",
                "titled commit\n\nfirst paragraph\n\nsecond paragraph\n",
            ],
        );
        let head_sha = test_rev_parse(&repo, "HEAD");

        let abbreviated = commit_detail(&pool, repo.to_str().unwrap().into(), head_sha[..7].into())
            .await
            .unwrap();
        assert_eq!(abbreviated.sha, head_sha);
        assert_eq!(abbreviated.subject, "titled commit");
        assert_eq!(abbreviated.body, "first paragraph\n\nsecond paragraph");
        assert_eq!(abbreviated.parents, [root_sha.clone()]);
        assert!(!abbreviated.author.is_empty());
        assert_eq!(abbreviated.date.split_whitespace().count(), 6);

        let root = commit_detail(&pool, repo.to_str().unwrap().into(), root_sha.clone())
            .await
            .unwrap();
        assert_eq!(root.subject, "initial");
        assert_eq!(root.body, "");
        assert!(root.parents.is_empty());

        // A separator byte inside the subject must not shift the parents out
        // of their field: the head reassembles the subject verbatim.
        std::fs::write(repo.join("tracked.txt"), "again\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "tricky\u{1f}subject"]);
        let tricky_sha = test_rev_parse(&repo, "HEAD");
        let tricky = commit_detail(&pool, repo.to_str().unwrap().into(), tricky_sha)
            .await
            .unwrap();
        assert_eq!(tricky.subject, "tricky\u{1f}subject");
        assert_eq!(tricky.parents, [head_sha]);

        let missing = commit_detail(&pool, repo.to_str().unwrap().into(), "deadbeef".into())
            .await
            .unwrap_err();
        assert_eq!(missing.code, "unresolvable_ref");

        let flagged = commit_detail(&pool, repo.to_str().unwrap().into(), "--exec=x".into())
            .await
            .unwrap_err();
        assert_eq!(flagged.code, "invalid_path");

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn remote_default_ref_prefers_the_target_remote_own_state() {
        let remotes = vec![
            "refs/remotes/origin/HEAD".to_string(),
            "refs/remotes/origin/main".to_string(),
            "refs/remotes/azure/develop".to_string(),
        ];
        assert_eq!(
            remote_default_ref(&remotes, "refs/remotes/origin/feature"),
            Some("refs/remotes/origin/HEAD")
        );
        assert_eq!(
            remote_default_ref(&remotes, "refs/remotes/azure/feature"),
            Some("refs/remotes/azure/develop")
        );
        // No candidate on the target's remote and no cross-remote pickup.
        assert_eq!(remote_default_ref(&remotes, "refs/remotes/upstream/main"), None);
        assert_eq!(remote_default_ref(&remotes, "refs/heads/main"), None);
        assert_eq!(remote_default_ref(&remotes, "HEAD"), None);
    }

    #[tokio::test]
    async fn remote_ref_target_defaults_to_its_fork_point() {
        let repo = test_repo("remote-fork-base");
        let pool = test_pool().await;
        test_git(&repo, &["branch", "-M", "main"]);
        let fork_sha = test_rev_parse(&repo, "HEAD");

        test_git(&repo, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(repo.join("feature.txt"), "feature\n").unwrap();
        test_git(&repo, &["add", "feature.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "feature work"]);
        let feature_sha = test_rev_parse(&repo, "HEAD");

        // The local primary moves past the fork point while the remote
        // tracking state stays at it: the review base must be the fork
        // point, not the stale local branch tip.
        test_git(&repo, &["checkout", "--quiet", "main"]);
        std::fs::write(repo.join("tracked.txt"), "changed\n").unwrap();
        test_git(&repo, &["commit", "--quiet", "-am", "main moves"]);

        test_git(
            &repo,
            &["update-ref", "refs/remotes/origin/main", &fork_sha],
        );
        test_git(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        test_git(
            &repo,
            &["update-ref", "refs/remotes/origin/feature", &feature_sha],
        );

        let inventory = refs_inventory(&pool, repo.to_str().unwrap().into(), None, None)
            .await
            .unwrap();
        assert_eq!(inventory.default_base, Some("refs/heads/main".into()));

        let inventory = refs_inventory(
            &pool,
repo.to_str().unwrap().into(),
            None,
            Some("refs/remotes/origin/feature".into()),
        )
        .await
        .unwrap();
        assert_eq!(inventory.default_base, Some(fork_sha.clone()));

        // Reviewing the remote default itself forks from its own tip.
        let inventory = refs_inventory(
            &pool,
repo.to_str().unwrap().into(),
            None,
            Some("refs/remotes/origin/main".into()),
        )
        .await
        .unwrap();
        assert_eq!(inventory.default_base, Some(fork_sha));

        // Local targets keep the primary-branch base.
        let inventory = refs_inventory(
            &pool,
repo.to_str().unwrap().into(),
            None,
            Some("refs/heads/feature".into()),
        )
        .await
        .unwrap();
        assert_eq!(inventory.default_base, Some("refs/heads/main".into()));

        std::fs::remove_dir_all(repo).unwrap();
    }

    // A committed review over plain refs composes its resolutions and both
    // diffs into one ssh invocation, with every variable slot shell-quoted.
    #[tokio::test]
    async fn remote_committed_review_rides_one_invocation() {
        let base_sha = "a".repeat(40);
        let head_sha = "b".repeat(40);
        let range = format!("'{base_sha}...{head_sha}'");
        let (spawner, scripts) = batch_fake_spawner(vec![vec![
            (0, format!("{base_sha}\n").into_bytes()),
            (0, format!("{head_sha}\n").into_bytes()),
            (0, b"M\0src/app.rs\0".to_vec()),
            (0, b"3\t1\tsrc/app.rs\0".to_vec()),
        ]]);
        let target =
            crate::git::RemoteTarget::from_parts(Some("dev"), "host.example", None, "/srv/re po")
                .unwrap();
        let pool = test_pool().await;
        let index = review_changes_remote_with(
            &spawner,
            &pool,
            "dev@host.example:/srv/repo",
            &target,
            base_sha.clone(),
            head_sha.clone(),
            false,
            true,
            false,
        )
        .await
        .unwrap();
        // One ssh invocation for the whole committed review load.
        assert_eq!(scripts.lock().unwrap().len(), 1);
        let script = scripts.lock().unwrap()[0].clone();
        assert_eq!(script.matches("-begin").count(), 4);
        assert!(script.contains("'/srv/re po'"));
        assert!(script.contains(&range));
        assert_eq!(index.base_sha, base_sha);
        assert_eq!(index.target_sha, head_sha);
        assert_eq!(index.files.len(), 1);
        assert_eq!(index.files[0].path, "src/app.rs");
        assert_eq!(index.additions, 3);
        assert_eq!(index.deletions, 1);
    }

    fn remote_target(path: &str) -> RemoteTarget {
        RemoteTarget::from_parts(Some("dev"), "host.example", None, path).unwrap()
    }

    // The remote untracked patch rides one invocation: the literal untracked
    // check at the worktree plus the two no-index diffs, which run with
    // their cwd outside the repository and the file operand as the only
    // substituted slot.
    #[tokio::test]
    async fn remote_untracked_patch_diffs_on_the_host() {
        let (spawner, scripts) = batch_fake_spawner(vec![vec![
            (0, b"new.txt\0".to_vec()),
            (1, b"2\t0\t/srv/re po/new.txt\0".to_vec()),
            (
                1,
                b"diff --git a/dev/null b/new.txt\nnew file mode 100644\n@@ -0,0 +1,2 @@\n"
                    .to_vec(),
            ),
        ]]);
        let target = remote_target("/srv/re po");
        let patch = review_patch_untracked_remote_with(&spawner, &target, "new.txt")
            .await
            .unwrap();
        assert_eq!(scripts.lock().unwrap().len(), 1);
        let script = scripts.lock().unwrap()[0].clone();
        assert_eq!(script.matches("-begin").count(), 3);
        assert!(script.contains("':(literal)new.txt'"));
        assert!(script.contains("-C '/tmp'"));
        assert!(script.contains("'/dev/null'"));
        assert!(script.contains("'/srv/re po/new.txt'"));
        assert!(script.contains("--no-index"));
        assert!(script.contains("--no-ext-diff"));
        assert!(script.contains("--no-textconv"));
        assert!(script.contains("GIT_ATTR_NOSYSTEM=1"));
        assert!(script.contains("LC_ALL=C LANG=C"));
        assert!(!patch.binary);
        assert!(patch.text.contains("new file mode"));
    }

    #[tokio::test]
    async fn remote_untracked_patch_reports_binary_without_patch_text() {
        let (spawner, _scripts) = batch_fake_spawner(vec![vec![
            (0, b"image.bin\0".to_vec()),
            (1, b"-\t-\t/srv/repo/image.bin\0".to_vec()),
            (1, Vec::new()),
        ]]);
        let target = remote_target("/srv/repo");
        let patch = review_patch_untracked_remote_with(&spawner, &target, "image.bin")
            .await
            .unwrap();
        assert!(patch.binary);
        assert!(patch.text.is_empty());
    }

    #[tokio::test]
    async fn remote_untracked_patch_requires_the_untracked_inventory() {
        let (spawner, _scripts) = batch_fake_spawner(vec![vec![
            (0, b"other.txt\0".to_vec()),
            (1, Vec::new()),
            (1, Vec::new()),
        ]]);
        let target = remote_target("/srv/repo");
        let error = review_patch_untracked_remote_with(&spawner, &target, "missing.txt")
            .await
            .unwrap_err();
        assert_eq!(error.code, "invalid_path");
    }

    // The working-changes read pins the root to a verified worktree, then
    // rides the fixed read command. NUL-bearing bytes return raw, so the
    // byte path serves images while the text path's caller-side NUL check
    // refuses the same content.
    #[tokio::test]
    async fn remote_working_content_reads_raw_bytes_after_the_worktree_probe() {
        let bytes = b"png\x00bytes\xff".to_vec();
        let (spawner, scripts) = hybrid_fake_spawner(
            vec![],
            vec![
                (0, b"true\n".to_vec(), Vec::new()),
                (0, bytes.clone(), Vec::new()),
            ],
        );
        let target = remote_target("/srv/re po");
        let read = resolve_review_file_bytes_remote_with(
            &spawner,
            &target,
            "HEAD".into(),
            "HEAD".into(),
            false,
            false,
            "src/asset.bin".into(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(read, bytes);
        assert_eq!(scripts.lock().unwrap().len(), 2);
        assert!(scripts.lock().unwrap()[0].contains("'rev-parse' '--is-inside-work-tree'"));
        let read_command = &scripts.lock().unwrap()[1];
        assert!(read_command.starts_with("LC_ALL=C LANG=C cat "));
        assert!(read_command.contains("'/srv/re po/src/asset.bin'"));
        // The text path's caller-side check refuses these bytes.
        assert!(content_is_binary(&bytes));
    }

    #[tokio::test]
    async fn remote_working_content_treats_absence_as_empty() {
        let (spawner, _scripts) = hybrid_fake_spawner(
            vec![],
            vec![
                (0, b"true\n".to_vec(), Vec::new()),
                (
                    1,
                    Vec::new(),
                    b"cat: /srv/re po/gone.txt: No such file or directory".to_vec(),
                ),
            ],
        );
        let target = remote_target("/srv/re po");
        let read = resolve_review_file_bytes_remote_with(
            &spawner,
            &target,
            "HEAD".into(),
            "HEAD".into(),
            false,
            false,
            "gone.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert!(read.is_empty());
    }

    #[tokio::test]
    async fn remote_working_content_refuses_a_non_worktree_root() {
        let (spawner, scripts) = hybrid_fake_spawner(
            vec![],
            vec![(128, Vec::new(), b"fatal: not a git repository".to_vec())],
        );
        let target = remote_target("/etc");
        let error = resolve_review_file_bytes_remote_with(
            &spawner,
            &target,
            "HEAD".into(),
            "HEAD".into(),
            false,
            false,
            "passwd".into(),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "not_a_work_tree");
        // The read command never ran against the refused root.
        assert_eq!(scripts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn remote_untracked_content_checks_inventory_then_reads() {
        let bytes = b"bracket\ntext\n".to_vec();
        let (spawner, scripts) = hybrid_fake_spawner(
            vec![vec![(0, b"new[1].txt\0".to_vec())]],
            vec![(0, bytes.clone(), Vec::new())],
        );
        let target = remote_target("/srv/re po");
        let read = resolve_review_file_bytes_remote_with(
            &spawner,
            &target,
            "HEAD".into(),
            "HEAD".into(),
            false,
            false,
            "new[1].txt".into(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(read, bytes);
        assert_eq!(scripts.lock().unwrap().len(), 2);
        assert!(scripts.lock().unwrap()[0].contains("':(literal)new[1].txt'"));
        assert!(scripts.lock().unwrap()[1].contains("cat '/srv/re po/new[1].txt'"));
    }

    #[tokio::test]
    async fn remote_untracked_content_refuses_a_missing_inventory_name() {
        let (spawner, _scripts) =
            hybrid_fake_spawner(vec![vec![(0, b"other\0".to_vec())]], vec![]);
        let target = remote_target("/srv/repo");
        let error = resolve_review_file_bytes_remote_with(
            &spawner,
            &target,
            "HEAD".into(),
            "HEAD".into(),
            false,
            false,
            "missing.txt".into(),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_path");
    }
}
