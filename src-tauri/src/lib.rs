mod cache;
mod commands;
mod git;
mod retrospection;
mod review;
mod store;
#[cfg(test)]
mod testutil;

use commands::{
    get_settings, list_commits, list_refs, list_repos, list_review_changes, list_surfaces,
    list_worktree_status, list_worktrees, open_repo, read_review_patch, set_repo_pinned,
    set_settings, set_surface_pinned,
};
use serde::Serialize;
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::path::{Path, PathBuf};
use tauri::Manager;

struct AppState {
    pool: SqlitePool,
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

// std::fs::canonicalize returns Windows verbatim paths (\\?\C:\... and
// \\?\UNC\server\share for network shares). Plain paths work everywhere we
// hand them, but keep verbatim when the plain form would exceed the 260
// character limit legacy Win32 paths accept.
pub(crate) fn plain_path(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    let plain = if let Some(share) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{share}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        return path.to_path_buf();
    };
    if plain.chars().count() <= 260 {
        PathBuf::from(plain)
    } else {
        path.to_path_buf()
    }
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
    let canonical = plain_path(&path.canonicalize().map_err(|_| {
        CommandError::new("invalid_path", "The selected folder could not be resolved.")
    })?);
    if canonical.file_name().is_none() || canonical.to_str().is_none() {
        return Err(CommandError::new(
            "invalid_path",
            "The selected path is not valid UTF-8.",
        ));
    }
    Ok(canonical)
}

async fn connect_store_pool(
    db_path: &Path,
    options: &SqliteConnectOptions,
) -> Result<SqlitePool, String> {
    let pool = sqlx::SqlitePool::connect_with(options.clone())
        .await
        .map_err(|error| format!("Could not connect to repository storage: {error}"))?;
    let error = match sqlx::migrate!().run(&pool).await {
        Ok(()) => return Ok(pool),
        Err(error) => error,
    };
    // A store whose recorded migrations diverge from the embedded set (the
    // pre-squash 0.0.1 schema) cannot migrate forward. It only holds saved
    // repos, pins, and settings, so it is recreated rather than blocking
    // startup on every launch.
    if !matches!(
        error,
        sqlx::migrate::MigrateError::VersionMissing(_)
            | sqlx::migrate::MigrateError::VersionMismatch(_)
    ) {
        return Err(format!("Could not migrate repository storage: {error}"));
    }
    pool.close().await;
    for suffix in ["", "-wal", "-shm"] {
        let file = PathBuf::from(format!("{}{suffix}", db_path.display()));
        if let Err(error) = std::fs::remove_file(&file) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(format!(
                    "Could not reset incompatible repository storage: {error}"
                ));
            }
        }
    }
    let pool = sqlx::SqlitePool::connect_with(options.clone())
        .await
        .map_err(|error| format!("Could not connect to repository storage: {error}"))?;
    sqlx::migrate!()
        .run(&pool)
        .await
        .map_err(|error| format!("Could not migrate repository storage: {error}"))?;
    Ok(pool)
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
            let db_path = data_dir.join("worktreeview.sqlite3");
            let options = SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true);
            let pool = tauri::async_runtime::block_on(async {
                let pool = connect_store_pool(&db_path, &options).await?;
                store::normalize_stored_paths(&pool).await.map_err(|error| {
                    format!("Could not normalize stored repository paths: {error}")
                })?;
                Ok::<_, String>(pool)
            })?;
            app.manage(AppState { pool });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            open_repo,
            list_repos,
            list_worktrees,
            list_worktree_status,
            set_repo_pinned,
            set_surface_pinned,
            list_refs,
            list_commits,
            list_review_changes,
            list_surfaces,
            read_review_patch,
            get_settings,
            set_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::plain_path;
    use super::{connect_store_pool, SqliteConnectOptions};
    use std::path::{Path, PathBuf};

    #[test]
    fn verbatim_prefixes_are_stripped() {
        assert_eq!(
            plain_path(Path::new(r"\\?\C:\repos\demo")),
            PathBuf::from(r"C:\repos\demo")
        );
        assert_eq!(
            plain_path(Path::new(r"\\?\UNC\server\share\demo")),
            PathBuf::from(r"\\server\share\demo")
        );
        assert_eq!(plain_path(Path::new("/tmp/demo")), PathBuf::from("/tmp/demo"));
    }

    #[test]
    fn overlong_unc_paths_stay_verbatim() {
        let deep = format!(r"\\?\UNC\server\share\{}", "a/".repeat(200));
        assert_eq!(plain_path(Path::new(&deep)), PathBuf::from(&deep));
    }

    #[tokio::test]
    async fn incompatible_store_schema_is_recreated() {
        let dir = crate::testutil::test_path("store-reset");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("worktreeview.sqlite3");
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);

        // A store from the pre-squash migration set: version 1 recorded
        // under a description and checksum the embedded baseline cannot
        // match, with tables the current schema does not define.
        let old = sqlx::SqlitePool::connect_with(options.clone()).await.unwrap();
        sqlx::query("CREATE TABLE repos (id INTEGER PRIMARY KEY)")
            .execute(&old)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, \
             description TEXT NOT NULL, installed_on TIMESTAMP NOT NULL \
             DEFAULT CURRENT_TIMESTAMP, success BOOLEAN NOT NULL, \
             checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)",
        )
        .execute(&old)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) \
             VALUES (1, 'repos', 1, X'00', 0)",
        )
        .execute(&old)
        .await
        .unwrap();
        old.close().await;

        let pool = connect_store_pool(&db_path, &options).await.unwrap();
        let versions: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(versions.len(), 3);
        sqlx::query("SELECT path, name, last_opened_at, created_at FROM repos LIMIT 1")
            .fetch_all(&pool)
            .await
            .unwrap();
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
