use serde::Serialize;
use sqlx::{sqlite::SqliteConnectOptions, Row, SqlitePool};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::Manager;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    time::{timeout, Duration},
};

const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

struct AppState {
    pool: SqlitePool,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Repo {
    path: String,
    name: String,
    worktrees: Vec<Worktree>,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Worktree {
    path: String,
    branch: String,
    head: String,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct BranchInventory {
    branches: Vec<String>,
    default_base: Option<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct ChangedFile {
    path: String,
    status: String,
    untracked: bool,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct ReviewIndex {
    files: Vec<ChangedFile>,
    additions: i64,
    deletions: i64,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct FilePatch {
    binary: bool,
    text: String,
}

#[derive(Debug, Serialize)]
pub struct CommandError {
    code: String,
    message: String,
}

impl CommandError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: sanitize(&message.into()),
        }
    }
}

impl From<sqlx::Error> for CommandError {
    fn from(error: sqlx::Error) -> Self {
        Self::new("persistence", format!("Repository storage failed: {error}"))
    }
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(512)
        .collect()
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn canonical_path(path: &str) -> Result<PathBuf, CommandError> {
    if path.trim().is_empty() {
        return Err(CommandError::new(
            "invalid_path",
            "Select a non-empty folder.",
        ));
    }
    let path = Path::new(path);
    if !path.is_dir() {
        return Err(CommandError::new(
            "invalid_path",
            "The selected folder does not exist.",
        ));
    }
    let canonical = path.canonicalize().map_err(|_| {
        CommandError::new("invalid_path", "The selected folder could not be resolved.")
    })?;
    if canonical.file_name().is_none() || canonical.to_str().is_none() {
        return Err(CommandError::new(
            "invalid_path",
            "The selected path is not valid UTF-8.",
        ));
    }
    Ok(canonical)
}

async fn read_bounded<R: AsyncRead + Unpin>(mut reader: R) -> Result<Vec<u8>, CommandError> {
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|_| CommandError::new("git_execution", "Git output could not be read."))?;
        if count == 0 {
            return Ok(output);
        }
        if output.len() + count > MAX_OUTPUT {
            return Err(CommandError::new(
                "git_output_too_large",
                "Git output was too large.",
            ));
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

async fn reap_after_kill(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn git_execution_error(stderr: &[u8]) -> CommandError {
    let message = if stderr.is_empty() {
        "Git command failed.".to_string()
    } else {
        String::from_utf8_lossy(stderr).into_owned()
    };
    CommandError::new("git_execution", message)
}

async fn run_git(path: &Path, args: &[&str]) -> Result<(i32, Vec<u8>, Vec<u8>), CommandError> {
    let path = path.to_str().ok_or_else(|| {
        CommandError::new("invalid_path", "The selected path is not valid UTF-8.")
    })?;
    let mut command = Command::new("git");
    command.args(["-c", "core.fsmonitor=false"]);
    if args.starts_with(&["-c", "core.quotePath=false"]) {
        command
            .args(["-c", "core.quotePath=false", "-C", path])
            .args(&args[2..]);
    } else {
        command.args(["-C", path]).args(args);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| CommandError::new("git_execution", "Git could not be started."))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CommandError::new("git_execution", "Git stdout was unavailable."))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CommandError::new("git_execution", "Git stderr was unavailable."))?;
    let stdout_task = tokio::spawn(read_bounded(stdout));
    let stderr_task = tokio::spawn(read_bounded(stderr));
    let result = timeout(GIT_TIMEOUT, async {
        let status = child
            .wait()
            .await
            .map_err(|_| CommandError::new("git_execution", "Git did not finish."))?;
        let stdout = stdout_task
            .await
            .map_err(|_| CommandError::new("git_execution", "Git stdout failed."))??;
        let stderr = stderr_task
            .await
            .map_err(|_| CommandError::new("git_execution", "Git stderr failed."))??;
        Ok::<_, CommandError>((status, stdout, stderr))
    })
    .await;
    match result {
        Ok(Ok((status, stdout, stderr))) => Ok((status.code().unwrap_or(-1), stdout, stderr)),
        Ok(Err(error)) => {
            reap_after_kill(&mut child).await;
            Err(error)
        }
        Err(_) => {
            reap_after_kill(&mut child).await;
            Err(CommandError::new(
                "git_timeout",
                "Git did not respond within 10 seconds.",
            ))
        }
    }
}

fn validate_ref(value: &str, name: &str) -> Result<(), CommandError> {
    if value.starts_with('-') {
        return Err(CommandError::new(
            "invalid_path",
            format!("The {name} must not start with '-'."),
        ));
    }
    Ok(())
}

fn validate_file(value: &str) -> Result<(), CommandError> {
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

fn validate_untracked_combination(
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

fn primary_branch(branches: &[String]) -> Option<&str> {
    if branches.iter().any(|branch| branch == "main") {
        Some("main")
    } else if branches.iter().any(|branch| branch == "master") {
        Some("master")
    } else {
        None
    }
}

fn parse_untracked_paths(output: &[u8]) -> Result<Vec<String>, CommandError> {
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path).map(str::to_string).map_err(|_| {
                CommandError::new(
                    "git_output_malformed",
                    "Git returned invalid untracked path data.",
                )
            })
        })
        .collect()
}

fn acceptable_diff_exit(exit_code: i32, no_index: bool) -> bool {
    exit_code == 0 || (no_index && exit_code == 1)
}

fn git_args<'a>(args: &'a [String]) -> Vec<&'a str> {
    args.iter().map(String::as_str).collect()
}

async fn reject_configured_filters(path: &Path) -> Result<(), CommandError> {
    let (exit_code, stdout, stderr) = run_git(
        path,
        &[
            "config",
            "--get-regexp",
            r"^filter\..*\.(clean|process)$",
        ],
    )
    .await?;
    match exit_code {
        1 => Ok(()),
        0 if stdout.is_empty() => Ok(()),
        0 => Err(CommandError::new(
            "git_filter_unsupported",
            "This review cannot run because Git conversion filters are configured.",
        )),
        _ => Err(git_execution_error(&stderr)),
    }
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

fn parse_name_status(output: &[u8]) -> Result<Vec<ChangedFile>, CommandError> {
    let mut fields = output.split(|byte| *byte == 0);
    let mut files = Vec::new();
    while let Some(status) = fields.next() {
        if status.is_empty() {
            continue;
        }
        let status = std::str::from_utf8(status).map_err(|_| {
            CommandError::new("git_output_malformed", "Git returned invalid change data.")
        })?;
        let path = fields.next().ok_or_else(|| {
            CommandError::new(
                "git_output_malformed",
                "Git returned incomplete change data.",
            )
        })?;
        let path = std::str::from_utf8(path).map_err(|_| {
            CommandError::new("git_output_malformed", "Git returned invalid change data.")
        })?;
        let path = if status.starts_with('R') || status.starts_with('C') {
            let new_path = fields.next().ok_or_else(|| {
                CommandError::new(
                    "git_output_malformed",
                    "Git returned incomplete rename data.",
                )
            })?;
            std::str::from_utf8(new_path).map_err(|_| {
                CommandError::new("git_output_malformed", "Git returned invalid change data.")
            })?
        } else {
            path
        };
        files.push(ChangedFile {
            path: path.to_string(),
            status: status.to_string(),
            untracked: false,
        });
    }
    Ok(files)
}

fn parse_numstat(output: &[u8]) -> Result<(i64, i64, bool), CommandError> {
    let malformed = || {
        CommandError::new(
            "git_output_malformed",
            "Git returned malformed numstat data.",
        )
    };
    let mut fields = output.split(|byte| *byte == 0);
    let mut additions = 0;
    let mut deletions = 0;
    let mut binary = false;
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let mut parts = record.splitn(3, |byte| *byte == b'\t');
        let added =
            std::str::from_utf8(parts.next().ok_or_else(malformed)?).map_err(|_| malformed())?;
        let deleted =
            std::str::from_utf8(parts.next().ok_or_else(malformed)?).map_err(|_| malformed())?;
        let path = parts.next().ok_or_else(malformed)?;
        let parse_count = |value: &str| {
            if value == "-" {
                Ok(None)
            } else {
                value.parse::<i64>().map(Some).map_err(|_| malformed())
            }
        };
        match (parse_count(added)?, parse_count(deleted)?) {
            (Some(added), Some(deleted)) => {
                additions += added;
                deletions += deleted;
            }
            _ => binary = true,
        }
        if path.is_empty() {
            let old_path = fields.next().ok_or_else(malformed)?;
            let new_path = fields.next().ok_or_else(malformed)?;
            if old_path.is_empty() || new_path.is_empty() {
                return Err(malformed());
            }
            std::str::from_utf8(old_path).map_err(|_| malformed())?;
            std::str::from_utf8(new_path).map_err(|_| malformed())?;
        } else {
            std::str::from_utf8(path).map_err(|_| malformed())?;
        }
    }
    Ok((additions, deletions, binary))
}

fn resolve_untracked_path(root: &Path, file: &str) -> Result<PathBuf, CommandError> {
    let candidate = root.join(file);
    let file_name = candidate
        .file_name()
        .ok_or_else(|| CommandError::new("invalid_path", "The selected file path is invalid."))?;
    let parent = candidate
        .parent()
        .ok_or_else(|| CommandError::new("invalid_path", "The selected file path is invalid."))?
        .canonicalize()
        .map_err(|_| {
            CommandError::new(
                "invalid_path",
                "The selected file path could not be resolved.",
            )
        })?;
    if !parent.starts_with(root) {
        return Err(CommandError::new(
            "invalid_path",
            "The selected file must remain inside the worktree.",
        ));
    }
    Ok(parent.join(file_name))
}

async fn verified_untracked_path(root: &Path, file: &str) -> Result<PathBuf, CommandError> {
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
    resolve_untracked_path(root, file)
}

fn decode_git_path(value: &str) -> Result<String, ()> {
    if !value.starts_with('"') {
        return Ok(value.to_string());
    }
    if !value.ends_with('"') || value.len() < 2 {
        return Err(());
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::new();
    let mut index = 1;
    while index < bytes.len() - 1 {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        if index >= bytes.len() - 1 {
            return Err(());
        }
        match bytes[index] {
            b'a' => decoded.push(7),
            b'b' => decoded.push(8),
            b't' => decoded.push(9),
            b'n' => decoded.push(10),
            b'v' => decoded.push(11),
            b'f' => decoded.push(12),
            b'r' => decoded.push(13),
            b'"' => decoded.push(b'"'),
            b'\\' => decoded.push(b'\\'),
            digit @ b'0'..=b'7' => {
                let mut number = digit - b'0';
                for _ in 0..2 {
                    index += 1;
                    if index >= bytes.len() - 1 || !(b'0'..=b'7').contains(&bytes[index]) {
                        return Err(());
                    }
                    number = number * 8 + bytes[index] - b'0';
                }
                decoded.push(number);
            }
            _ => return Err(()),
        }
        index += 1;
    }
    String::from_utf8(decoded).map_err(|_| ())
}

fn parse_worktrees(output: &[u8]) -> Result<Vec<Worktree>, CommandError> {
    let text = std::str::from_utf8(output)
        .map_err(|_| CommandError::new("git_output_malformed", "Git returned invalid text."))?;
    let mut records = Vec::new();
    let mut current = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            if !current.is_empty() {
                records.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        records.push(current);
    }
    let mut worktrees = Vec::new();
    for record in records {
        if record.iter().any(|line| *line == "bare") {
            continue;
        }
        if !record
            .first()
            .is_some_and(|line| line.starts_with("worktree "))
        {
            return Err(CommandError::new(
                "git_output_malformed",
                "Git returned malformed worktree data.",
            ));
        }
        let mut path = None;
        let mut head = None;
        let mut branch = None;
        let mut detached = false;
        for line in record {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = decode_git_path(value)
                    .ok()
                    .filter(|value| !value.is_empty());
            } else if let Some(value) = line.strip_prefix("HEAD ") {
                if value.is_empty() {
                    return Err(CommandError::new(
                        "git_output_malformed",
                        "Git returned an empty worktree HEAD.",
                    ));
                }
                head = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("branch ") {
                if value.is_empty() {
                    return Err(CommandError::new(
                        "git_output_malformed",
                        "Git returned an empty worktree branch.",
                    ));
                }
                branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_string(),
                );
            } else if line == "detached" {
                detached = true;
            } else if line == "locked"
                || line.starts_with("locked ")
                || line.starts_with("prunable ")
                || line
                    .split_once(' ')
                    .is_some_and(|(key, value)| !key.is_empty() && !value.is_empty())
            {
                continue;
            } else {
                return Err(CommandError::new(
                    "git_output_malformed",
                    "Git returned malformed worktree data.",
                ));
            }
        }
        let path = path.ok_or_else(|| {
            CommandError::new(
                "git_output_malformed",
                "Git returned a worktree without a path.",
            )
        })?;
        let head = head.ok_or_else(|| {
            CommandError::new(
                "git_output_malformed",
                "Git returned a worktree without a HEAD.",
            )
        })?;
        let branch = if detached {
            "detached".into()
        } else {
            branch.ok_or_else(|| {
                CommandError::new(
                    "git_output_malformed",
                    "Git returned a worktree without a branch.",
                )
            })?
        };
        worktrees.push(Worktree { path, branch, head });
    }
    Ok(worktrees)
}

async fn upsert_repo(
    pool: &SqlitePool,
    path: &str,
    name: &str,
    clock: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO repos (path, name, last_opened_at, created_at) VALUES (?, ?, MAX(?, COALESCE((SELECT MAX(last_opened_at) + 1 FROM repos), ?)), ?) ON CONFLICT(path) DO UPDATE SET name = excluded.name, last_opened_at = excluded.last_opened_at").bind(path).bind(name).bind(clock).bind(clock).bind(clock).execute(pool).await?;
    Ok(())
}

async fn load_repos(pool: &SqlitePool) -> Result<Vec<Repo>, CommandError> {
    let rows = sqlx::query(
        "SELECT path, name FROM repos ORDER BY last_opened_at DESC, created_at DESC, path ASC",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Repo {
                path: row.try_get("path")?,
                name: row.try_get("name")?,
                worktrees: Vec::new(),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

#[tauri::command]
async fn open_repo(path: String, state: tauri::State<'_, AppState>) -> Result<Repo, CommandError> {
    open_repo_path(&path, &state.pool).await
}

async fn open_repo_path(path: &str, pool: &SqlitePool) -> Result<Repo, CommandError> {
    let canonical = canonical_path(path)?;
    let (exit_code, stdout, stderr) =
        run_git(&canonical, &["rev-parse", "--is-inside-work-tree"]).await?;
    if exit_code != 0
        && !String::from_utf8_lossy(&stderr)
            .to_ascii_lowercase()
            .contains("not a git repository")
    {
        return Err(git_execution_error(&stderr));
    }
    if exit_code != 0 || std::str::from_utf8(&stdout).map(|value| value.trim()) != Ok("true") {
        return Err(CommandError::new(
            "not_git_repository",
            "The selected folder is not a Git repository and was not added.",
        ));
    }
    let path = canonical.to_str().ok_or_else(|| {
        CommandError::new("invalid_path", "The selected path is not valid UTF-8.")
    })?;
    let name = canonical
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            CommandError::new("invalid_path", "The selected folder has no valid name.")
        })?;
    upsert_repo(pool, path, name, now_millis()).await?;
    Ok(Repo {
        path: path.into(),
        name: name.into(),
        worktrees: Vec::new(),
    })
}

#[tauri::command]
async fn list_repos(state: tauri::State<'_, AppState>) -> Result<Vec<Repo>, CommandError> {
    load_repos(&state.pool).await
}

#[tauri::command]
async fn list_worktrees(path: String) -> Result<Vec<Worktree>, CommandError> {
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
async fn list_branches(
    path: String,
    worktree_branch: String,
) -> Result<BranchInventory, CommandError> {
    validate_ref(&worktree_branch, "worktree_branch")?;
    let path = canonical_path(&path)?;
    let (exit_code, stdout, stderr) = run_git(
        &path,
        &["for-each-ref", "refs/heads", "--format=%(refname:short)"],
    )
    .await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let text = std::str::from_utf8(&stdout).map_err(|_| {
        CommandError::new("git_output_malformed", "Git returned invalid branch data.")
    })?;
    let branches = text.lines().map(str::to_string).collect::<Vec<_>>();
    let primary = primary_branch(&branches);
    let default_base = if worktree_branch == "detached" {
        None
    } else if let Some(primary) = primary {
        let verify_args = vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("refs/heads/{worktree_branch}"),
        ];
        let verify_args = git_args(&verify_args);
        if run_git(&path, &verify_args).await?.0 != 0 {
            return Ok(BranchInventory {
                branches,
                default_base: None,
            });
        }
        let args = vec!["merge-base".into(), worktree_branch.clone(), primary.into()];
        let args = git_args(&args);
        match run_git(&path, &args).await? {
            (0, stdout, _) => std::str::from_utf8(&stdout)
                .ok()
                .map(|value| value.trim().to_string()),
            _ => None,
        }
    } else {
        None
    };
    Ok(BranchInventory {
        branches,
        default_base,
    })
}

#[tauri::command]
async fn list_review_changes(
    path: String,
    base: String,
    committed_only: bool,
    reversed: bool,
) -> Result<ReviewIndex, CommandError> {
    validate_ref(&base, "base")?;
    let path = canonical_path(&path)?;
    if !committed_only {
        reject_configured_filters(&path).await?;
    }
    let range = if committed_only {
        format!("{base}...HEAD")
    } else {
        base.clone()
    };
    let name_args = review_index_args("--name-status", &range, reversed);
    let name_args = git_args(&name_args);
    let (exit_code, stdout, stderr) = run_git(&path, &name_args).await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let mut files = parse_name_status(&stdout)?;

    let num_args = review_index_args("--numstat", &range, reversed);
    let num_args = git_args(&num_args);
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
    })
}

#[tauri::command]
async fn read_review_patch(
    path: String,
    base: String,
    committed_only: bool,
    reversed: bool,
    file: String,
    untracked: bool,
) -> Result<FilePatch, CommandError> {
    validate_ref(&base, "base")?;
    validate_file(&file)?;
    validate_untracked_combination(untracked, committed_only, reversed)?;
    let path = canonical_path(&path)?;
    if !committed_only && !untracked {
        reject_configured_filters(&path).await?;
    }
    let range = if committed_only {
        format!("{base}...HEAD")
    } else {
        base
    };
    let untracked_path = if untracked {
        Some(verified_untracked_path(&path, &file).await?)
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
    let (exit_code, stdout, stderr) = if let Some(file_path) = &untracked_path {
        let file_path = file_path.to_str().ok_or_else(|| {
            CommandError::new("invalid_path", "The selected file path is not valid UTF-8.")
        })?;
        let args = vec![
            "diff".into(),
            "--no-index".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--numstat".into(),
            "-z".into(),
            "--".into(),
            "/dev/null".into(),
            file_path.into(),
        ];
        let args = git_args(&args);
        run_git(&path, &args).await?
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
    let (exit_code, stdout, stderr) = if let Some(file_path) = &untracked_path {
        let file_path = file_path.to_str().ok_or_else(|| {
            CommandError::new("invalid_path", "The selected file path is not valid UTF-8.")
        })?;
        let args = vec![
            "diff".into(),
            "--no-index".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--no-color".into(),
            "-U3".into(),
            "--".into(),
            "/dev/null".into(),
            file_path.into(),
        ];
        let args = git_args(&args);
        run_git(&path, &args).await?
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir().map_err(|error| {
                format!("Could not resolve application data directory: {error}")
            })?;
            std::fs::create_dir_all(&data_dir)
                .map_err(|error| format!("Could not create application data directory: {error}"))?;
            let options = SqliteConnectOptions::new()
                .filename(data_dir.join("worktreeview.sqlite3"))
                .create_if_missing(true);
            let pool = tauri::async_runtime::block_on(async {
                let pool = sqlx::SqlitePool::connect_with(options)
                    .await
                    .map_err(|error| format!("Could not connect to repository storage: {error}"))?;
                sqlx::migrate!()
                    .run(&pool)
                    .await
                    .map_err(|error| format!("Could not migrate repository storage: {error}"))?;
                Ok::<_, String>(pool)
            })?;
            app.manage(AppState { pool });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            open_repo,
            list_repos,
            list_worktrees,
            list_branches,
            list_review_changes,
            read_review_patch
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::{
        process::Command as StdCommand,
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE repos (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, name TEXT NOT NULL, last_opened_at INTEGER NOT NULL, created_at INTEGER NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    fn test_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "worktreeview-{label}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn test_git(path: &Path, args: &[&str]) {
        let output = StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn test_repo(label: &str) -> PathBuf {
        let path = test_path(label);
        std::fs::create_dir(&path).unwrap();
        test_git(&path, &["init", "--quiet"]);
        test_git(&path, &["config", "user.name", "WorktreeView Tests"]);
        test_git(&path, &["config", "user.email", "tests@example.com"]);
        std::fs::write(path.join("tracked.txt"), "original\n").unwrap();
        test_git(&path, &["add", "tracked.txt"]);
        test_git(&path, &["commit", "--quiet", "-m", "initial"]);
        path
    }

    #[test]
    fn parses_porcelain_records() {
        let output = br#"worktree /tmp/main
HEAD abc123
branch refs/heads/main
locked reason
unknown value

worktree "/tmp/space\040name"
HEAD def456
detached
prunable stale

bare
"#;
        assert_eq!(
            parse_worktrees(output).unwrap(),
            vec![
                Worktree {
                    path: "/tmp/main".into(),
                    branch: "main".into(),
                    head: "abc123".into()
                },
                Worktree {
                    path: "/tmp/space name".into(),
                    branch: "detached".into(),
                    head: "def456".into()
                }
            ]
        );
    }
    #[test]
    fn rejects_incomplete_porcelain() {
        assert!(parse_worktrees(b"worktree /tmp/main\nbranch refs/heads/main\n").is_err());
    }
    #[test]
    fn parses_name_status_records_and_renames() {
        let output = b"A\0new.txt\0M\0changed.txt\0R100\0old.txt\0new.txt\0";
        let files = parse_name_status(output).unwrap();
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[1].status, "M");
        assert_eq!(files[2].path, "new.txt");
        assert!(parse_name_status(b"A\0\xff\0").is_err());
    }
    #[test]
    fn parses_numstat_records_and_binary_rows() {
        let output = b"3\t2\tfile.txt\00\t0\t\0old.txt\0new.txt\0-\t-\timage.png\01\t0\t\0/dev/null\0new-file.txt\0";
        assert_eq!(parse_numstat(output).unwrap(), (4, 2, true));
        assert!(parse_numstat(b"3\t2\t\0old.txt\0").is_err());
    }
    #[test]
    fn parses_untracked_paths() {
        assert_eq!(
            parse_untracked_paths(b"a.txt\0dir/b.txt\0").unwrap(),
            ["a.txt", "dir/b.txt"]
        );
        assert!(parse_untracked_paths(b"a\0\xff\0").is_err());
    }
    #[test]
    fn accepts_only_no_index_difference_exit_code() {
        assert!(acceptable_diff_exit(0, false));
        assert!(acceptable_diff_exit(1, true));
        assert!(!acceptable_diff_exit(1, false));
        assert!(!acceptable_diff_exit(2, true));
    }
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
            primary_branch(&["master".into(), "main".into()]),
            Some("main")
        );
        assert_eq!(primary_branch(&["master".into()]), Some("master"));
        assert_eq!(primary_branch(&["develop".into()]), None);
    }
    #[test]
    fn rejects_invalid_untracked_combinations() {
        assert!(validate_untracked_combination(true, true, false).is_err());
        assert!(validate_untracked_combination(true, false, true).is_err());
        assert!(validate_untracked_combination(true, false, false).is_ok());
    }
    #[tokio::test]
    async fn untracked_patch_requires_current_git_inventory() {
        let repo = test_repo("untracked-inventory");
        std::fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(repo.join("ignored.txt"), "ignored\n").unwrap();
        std::fs::write(repo.join("new.txt"), "new\n").unwrap();

        let patch = read_review_patch(
            repo.to_str().unwrap().into(),
            "HEAD".into(),
            false,
            false,
            "new.txt".into(),
            true,
        )
        .await
        .unwrap();
        assert!(patch.text.contains("new.txt"));

        let ignored = read_review_patch(
            repo.to_str().unwrap().into(),
            "HEAD".into(),
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
        let error = read_review_patch(
            arbitrary.to_str().unwrap().into(),
            "HEAD".into(),
            false,
            false,
            "new.txt".into(),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "git_execution");

        std::fs::remove_dir_all(repo).unwrap();
        std::fs::remove_dir_all(arbitrary).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configured_filters_are_rejected_without_execution() {
        use std::os::unix::fs::PermissionsExt;

        let repo = test_repo("configured-filter");
        let marker = repo.join("filter-ran");
        let helper = repo.join("filter.sh");
        std::fs::write(repo.join(".gitattributes"), "tracked.txt filter=marker\n").unwrap();
        test_git(&repo, &["add", ".gitattributes"]);
        test_git(&repo, &["commit", "--quiet", "-m", "attributes"]);
        std::fs::write(&helper, format!("#!/bin/sh\n: > '{}'\ncat\n", marker.display())).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        test_git(&repo, &["config", "filter.marker.clean", helper.to_str().unwrap()]);
        std::fs::write(repo.join("tracked.txt"), "changed\n").unwrap();

        let _ = StdCommand::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["diff", "--no-ext-diff", "--no-textconv", "--", "tracked.txt"])
            .output()
            .unwrap();
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();

        let index_error = list_review_changes(
            repo.to_str().unwrap().into(),
            "HEAD".into(),
            false,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(index_error.code, "git_filter_unsupported");
        assert!(!marker.exists());

        let patch_error = read_review_patch(
            repo.to_str().unwrap().into(),
            "HEAD".into(),
            false,
            false,
            "tracked.txt".into(),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(patch_error.code, "git_filter_unsupported");
        assert!(!marker.exists());

        test_git(&repo, &["config", "--unset", "filter.marker.clean"]);
        test_git(&repo, &["config", "filter.marker.process", helper.to_str().unwrap()]);
        let _ = StdCommand::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["diff", "--no-ext-diff", "--no-textconv", "--", "tracked.txt"])
            .output()
            .unwrap();
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();
        let process_error = list_review_changes(
            repo.to_str().unwrap().into(),
            "HEAD".into(),
            false,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(process_error.code, "git_filter_unsupported");
        assert!(!marker.exists());

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn untracked_paths_reject_symlinked_parents() {
        use std::os::unix::fs::symlink;

        let root = test_path("symlink-root");
        let outside = test_path("symlink-outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("file.txt"), "outside\n").unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        symlink(outside.join("file.txt"), root.join("final-link")).unwrap();
        let root = root.canonicalize().unwrap();

        assert!(resolve_untracked_path(&root, "escape/file.txt").is_err());
        assert_eq!(
            resolve_untracked_path(&root, "final-link").unwrap(),
            root.join("final-link")
        );

        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn review_commands_disable_repository_helpers() {
        use std::os::unix::fs::PermissionsExt;

        let repo = test_repo("repository-helpers");
        let marker = repo.join("helper-ran");
        let helper = repo.join("helper.sh");
        std::fs::write(&helper, format!("#!/bin/sh\n: > '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        test_git(
            &repo,
            &["config", "core.fsmonitor", helper.to_str().unwrap()],
        );
        test_git(
            &repo,
            &["config", "diff.external", helper.to_str().unwrap()],
        );
        std::fs::write(repo.join("tracked.txt"), "changed\n").unwrap();

        let _ = StdCommand::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["status", "--short"])
            .output()
            .unwrap();
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();

        let _ = StdCommand::new("git")
            .args(["-c", "core.fsmonitor=false", "-C"])
            .arg(&repo)
            .args(["diff", "--", "tracked.txt"])
            .output()
            .unwrap();
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();

        let index = list_review_changes(repo.to_str().unwrap().into(), "HEAD".into(), false, false)
            .await
            .unwrap();
        assert!(index.files.iter().any(|file| file.path == "tracked.txt"));
        assert!(!marker.exists());

        let patch = read_review_patch(
            repo.to_str().unwrap().into(),
            "HEAD".into(),
            false,
            false,
            "tracked.txt".into(),
            false,
        )
        .await
        .unwrap();
        assert!(patch.text.contains("changed"));
        assert!(!marker.exists());

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
    }
    #[tokio::test]
    async fn upsert_is_monotonic_and_preserves_creation() {
        let pool = test_pool().await;
        upsert_repo(&pool, "/a", "a", 100).await.unwrap();
        upsert_repo(&pool, "/b", "b", 99).await.unwrap();
        let created: i64 = sqlx::query_scalar("SELECT created_at FROM repos WHERE path = '/a'")
            .fetch_one(&pool)
            .await
            .unwrap();
        upsert_repo(&pool, "/a", "new", 1).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM repos WHERE path = '/a'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        let row =
            sqlx::query("SELECT name, last_opened_at, created_at FROM repos WHERE path = '/a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.get::<String, _>("name"), "new");
        assert_eq!(row.get::<i64, _>("last_opened_at"), 102);
        assert_eq!(row.get::<i64, _>("created_at"), created);

        sqlx::query("INSERT INTO repos (path, name, last_opened_at, created_at) VALUES ('/d', 'd', 50, 40), ('/c', 'c', 50, 40), ('/e', 'e', 50, 41)")
            .execute(&pool)
            .await
            .unwrap();
        let paths: Vec<_> = load_repos(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|repo| repo.path)
            .collect();
        assert_eq!(paths, ["/a", "/b", "/e", "/c", "/d"]);
    }

    #[tokio::test]
    async fn invalid_repositories_are_not_persisted() {
        let pool = test_pool().await;
        let non_git = test_path("non-git");
        std::fs::create_dir(&non_git).unwrap();
        let missing = test_path("missing");

        let non_git_error = open_repo_path(non_git.to_str().unwrap(), &pool)
            .await
            .unwrap_err();
        let missing_error = open_repo_path(missing.to_str().unwrap(), &pool)
            .await
            .unwrap_err();

        assert_eq!(non_git_error.code, "not_git_repository");
        assert_eq!(missing_error.code, "invalid_path");
        assert!(load_repos(&pool).await.unwrap().is_empty());
        std::fs::remove_dir(non_git).unwrap();
    }
}
