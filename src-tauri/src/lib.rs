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

async fn run_git(path: &Path, args: &[&str]) -> Result<(Vec<u8>, Vec<u8>), CommandError> {
    let path = path.to_str().ok_or_else(|| {
        CommandError::new("invalid_path", "The selected path is not valid UTF-8.")
    })?;
    let mut command = Command::new("git");
    if args.first() == Some(&"-c") {
        command.args(args).args(["-C", path]);
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
        Ok(Ok((status, stdout, stderr))) if status.success() => Ok((stdout, stderr)),
        Ok(Ok((_, _, stderr))) => {
            let message = if stderr.is_empty() {
                "Git command failed.".to_string()
            } else {
                String::from_utf8_lossy(&stderr).into_owned()
            };
            Err(CommandError::new("git_execution", message))
        }
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

#[tauri::command]
async fn open_repo(path: String, state: tauri::State<'_, AppState>) -> Result<Repo, CommandError> {
    let canonical = canonical_path(&path)?;
    let (stdout, _) = run_git(&canonical, &["rev-parse", "--is-inside-work-tree"]).await?;
    if std::str::from_utf8(&stdout).map(|value| value.trim()) != Ok("true") {
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
    upsert_repo(&state.pool, path, name, now_millis()).await?;
    Ok(Repo {
        path: path.into(),
        name: name.into(),
        worktrees: Vec::new(),
    })
}

#[tauri::command]
async fn list_repos(state: tauri::State<'_, AppState>) -> Result<Vec<Repo>, CommandError> {
    let rows = sqlx::query(
        "SELECT path, name FROM repos ORDER BY last_opened_at DESC, created_at DESC, path ASC",
    )
    .fetch_all(&state.pool)
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
async fn list_worktrees(path: String) -> Result<Vec<Worktree>, CommandError> {
    let path = canonical_path(&path)?;
    let (stdout, _) = run_git(
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
    parse_worktrees(&stdout)
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
            list_worktrees
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
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
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE repos (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, name TEXT NOT NULL, last_opened_at INTEGER NOT NULL, created_at INTEGER NOT NULL)").execute(&pool).await.unwrap();
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
    }
}
