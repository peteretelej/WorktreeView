mod commands;
mod git;
mod review;
mod store;
#[cfg(test)]
mod testutil;

use commands::{
    get_settings, list_commits, list_refs, list_repos, list_review_changes, list_worktrees,
    open_repo, read_review_patch, set_repo_pinned, set_settings,
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
            set_repo_pinned,
            list_refs,
            list_commits,
            list_review_changes,
            read_review_patch,
            get_settings,
            set_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
