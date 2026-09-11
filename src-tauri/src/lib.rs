mod agents;
mod cache;
mod commands;
mod git;
mod mcp;
mod overview;
mod retrospection;
mod review;
mod reviews;
mod store;
#[cfg(test)]
mod testutil;
mod transport;

use commands::{
    create_agent_token, create_comment, delete_comment, describe_commit, edit_comment,
    fetch_project, get_branch_inventory, get_mcp_status, get_settings, list_agent_tokens,
    list_comments, list_commits, list_refs, list_repos, list_review_changes, list_submissions,
    list_surfaces, list_worktree_status, list_worktrees, match_comment_anchors, open_repo,
    open_review_file, read_review_file, read_review_file_bytes, read_review_patch,
    remove_repo, reply_comment, restart_mcp,
    delete_agent_token, set_comment_resolved, set_repo_pinned, set_settings, set_surface_pinned,
};
use serde::Serialize;
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{Emitter, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

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

// A store recorded by a newer binary contains schema this build cannot
// read; rebuilding over it would destroy that data, so opening refuses
// and the caller decides between updating the app and setting it aside.
#[derive(Debug)]
enum StoreOpenError {
    NewerStore,
    Other(String),
}

async fn connect_store_pool(
    db_path: &Path,
    options: &SqliteConnectOptions,
) -> Result<SqlitePool, StoreOpenError> {
    let pool = sqlx::SqlitePool::connect_with(options.clone())
        .await
        .map_err(|error| {
            StoreOpenError::Other(format!("Could not connect to repository storage: {error}"))
        })?;
    let error = match sqlx::migrate!().run(&pool).await {
        Ok(()) => return Ok(pool),
        Err(error) => error,
    };
    pool.close().await;
    if matches!(error, sqlx::migrate::MigrateError::VersionMissing(_)) {
        return Err(StoreOpenError::NewerStore);
    }
    // A checksum mismatch means the embedded migration set diverged from the
    // one that wrote the store, e.g. after a migration squash. The store
    // holds saved repos, pins, settings, comments, and submissions plus
    // regenerable caches; set-aside keeps all of it in one fixed backup and
    // rebuilds rather than blocking startup.
    if !matches!(error, sqlx::migrate::MigrateError::VersionMismatch(_)) {
        return Err(StoreOpenError::Other(format!(
            "Could not migrate repository storage: {error}"
        )));
    }
    set_aside_store(db_path).map_err(|error| {
        StoreOpenError::Other(format!(
            "Could not set aside incompatible repository storage at {}: {error}",
            db_path.display()
        ))
    })?;
    let pool = sqlx::SqlitePool::connect_with(options.clone())
        .await
        .map_err(|error| {
            StoreOpenError::Other(format!("Could not connect to repository storage: {error}"))
        })?;
    sqlx::migrate!()
        .run(&pool)
        .await
        .map_err(|error| {
            StoreOpenError::Other(format!("Could not migrate repository storage: {error}"))
        })?;
    Ok(pool)
}

// The user never touches the store file: a newer-version store ends in an
// explicit choice, and Start fresh reuses the set-aside so the newer
// version can pick the data back up from the backup.
async fn newer_store_resolution(
    app: &tauri::App,
    db_path: &Path,
    options: &SqliteConnectOptions,
) -> Result<SqlitePool, String> {
    let start_fresh = app
        .dialog()
        .message(format!(
            "Repository storage {} was written by a newer version of WorktreeView. \
             Update the app to keep using it, or start fresh: the current store is \
             kept as {}.bak and a new one is created.",
            db_path.display(),
            db_path.display()
        ))
        .title("WorktreeView")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Start fresh".to_string(),
            "Quit".to_string(),
        ))
        .blocking_show();
    if !start_fresh {
        // Nothing has been initialized yet, so quitting directly avoids a
        // second dialog from the setup error path.
        std::process::exit(0);
    }
    set_aside_store(db_path).map_err(|error| {
        format!(
            "Could not set aside incompatible repository storage at {}: {error}",
            db_path.display()
        )
    })?;
    match connect_store_pool(db_path, options).await {
        Ok(pool) => Ok(pool),
        Err(StoreOpenError::Other(message)) => Err(message),
        Err(StoreOpenError::NewerStore) => Err(format!(
            "Repository storage {} still reports a newer version after being \
             set aside",
            db_path.display()
        )),
    }
}

// At most one backup set exists at a time; a later reset overwrites the
// previous backup instead of accumulating files. Sidecars rename first so
// an interrupted set-aside never leaves a stale WAL beside a fresh store.
fn set_aside_store(db_path: &Path) -> std::io::Result<()> {
    for suffix in ["-wal", "-shm", ""] {
        let source = PathBuf::from(format!("{}{suffix}", db_path.display()));
        let backup = PathBuf::from(format!("{}{suffix}.bak", db_path.display()));
        if let Err(error) = std::fs::remove_file(&backup) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error);
            }
        }
        if let Err(error) = std::fs::rename(&source, &backup) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error);
            }
        }
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            initialize(app).map_err(|error| {
                // Setup errors surface only as a stderr panic, which a
                // Windows release build has no console to show; without
                // this dialog an aborting startup looks like a silent
                // crash.
                app.dialog()
                    .message(error.clone())
                    .title("WorktreeView")
                    .blocking_show();
                error
            })?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            open_repo,
            list_repos,
            list_worktrees,
            list_worktree_status,
            remove_repo,
            get_branch_inventory,
            fetch_project,
            set_repo_pinned,
            set_surface_pinned,
            list_refs,
            list_commits,
            describe_commit,
            list_review_changes,
            list_surfaces,
            read_review_patch,
            read_review_file, read_review_file_bytes,
            open_review_file,
            get_settings,
            set_settings,
            list_agent_tokens,
            create_agent_token,
            delete_agent_token,
            get_mcp_status,
            restart_mcp,
            create_comment,
            list_comments,
            list_submissions,
            reply_comment,
            set_comment_resolved,
            edit_comment,
            delete_comment,
            match_comment_anchors
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            // Best-effort: stop the loopback listener so the server task
            // removes the discovery file; nothing joins the task. A
            // restart holding the owner lock is skipped rather than
            // waited on: the exiting process takes the listener down
            // regardless.
            if let Some(owner) = app_handle.try_state::<transport::ListenerOwner>() {
                if let Ok(mut owned) = owner.0.try_lock() {
                    if let Some(endpoint) = owned.take() {
                        endpoint.shutdown();
                    }
                }
            }
        }
    });
}

// Dev builds run from their own checkout's target directory, so the binary's
// folder identifies the checkout. The derived key must be stable across runs
// and toolchain updates, which rules out DefaultHasher; FNV-1a is the same
// deterministic hash the comment anchor uses.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn checkout_store_key(exe_dir: &Path) -> String {
    format!("{:016x}", fnv1a64(exe_dir.to_string_lossy().to_ascii_lowercase().as_bytes()))
}

// A readable checkout label when the binary sits in the standard
// `<checkout>/src-tauri/target/debug` layout; the hash stays the uniqueness
// guarantee for every other layout.
fn checkout_label(exe_dir: &Path) -> Option<String> {
    fn named(path: &Path, expected: &str) -> bool {
        path.file_name().and_then(|name| name.to_str()) == Some(expected)
    }
    let mut ancestors = exe_dir.ancestors();
    if !named(ancestors.next()?, "debug")
        || !named(ancestors.next()?, "target")
        || !named(ancestors.next()?, "src-tauri")
    {
        return None;
    }
    let label: String = ancestors
        .next()?
        .file_name()?
        .to_str()?
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '-' })
        .collect();
    Some(label)
}

fn initialize(app: &tauri::App) -> Result<(), String> {
    let data_dir = match std::env::var("WORKTREEVIEW_DATA_DIR") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir.trim()),
        _ => app.path().app_data_dir().map_err(|error| {
            format!("Could not resolve application data directory: {error}")
        })?,
    };
    std::fs::create_dir_all(&data_dir)
        .map_err(|error| format!("Could not create application data directory: {error}"))?;
    // Every parallel dev checkout gets its own store: the binary runs from
    // that checkout's target directory, so a per-checkout name keeps two
    // worktrees (which may carry different schema versions) from trading
    // migration skew through one shared dev store.
    let db_name = if cfg!(debug_assertions) {
        let exe = std::env::current_exe()
            .map_err(|error| format!("Could not locate the running binary: {error}"))?;
        let exe_dir = exe.parent().unwrap_or_else(|| exe.as_path());
        let key = checkout_store_key(exe_dir);
        match checkout_label(exe_dir) {
            Some(label) if !label.is_empty() => format!("worktreeview-dev-{label}-{key}.sqlite3"),
            _ => format!("worktreeview-dev-{key}.sqlite3"),
        }
    } else {
        "worktreeview.sqlite3".to_string()
    };
    let db_path = data_dir.join(db_name);
    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let pool = tauri::async_runtime::block_on(async {
        let pool = match connect_store_pool(&db_path, &options).await {
            Ok(pool) => pool,
            Err(StoreOpenError::NewerStore) => {
                newer_store_resolution(app, &db_path, &options).await?
            }
            Err(StoreOpenError::Other(message)) => return Err(message),
        };
        store::normalize_stored_paths(&pool).await.map_err(|error| {
            format!("Could not normalize stored repository paths: {error}")
        })?;
        Ok::<_, String>(pool)
    })?;
    let settings = tauri::async_runtime::block_on(store::get_settings_in_pool(&pool))
        .map_err(|error| format!("Could not read settings: {}", error.message))?;
    app.manage(AppState { pool: pool.clone() });
    let app_handle = app.handle().clone();
    let arrivals: transport::ArrivalSink = Arc::new(move |arrival| {
        let _ = app_handle.emit("submission-received", arrival);
    });
    let refresh_app_handle = app.handle().clone();
    let refreshes: transport::RefreshSink = Arc::new(move |repo_path| {
        let _ = refresh_app_handle.emit(
            "project-refreshed",
            serde_json::json!({ "repo_path": repo_path }),
        );
    });
    let comment_app_handle = app.handle().clone();
    let comment_changes: transport::CommentSink = Arc::new(move |change| {
        let _ = comment_app_handle.emit("comment-changed", change);
    });
    let config = transport::ListenerConfig {
        enabled: settings.mcp_enabled,
        address: settings.mcp_listen_address.clone(),
        port: settings.mcp_port,
    };
    // One status handle and one set of surroundings serve every listener
    // start, boot or restart: get_mcp_status readers never hold a stale
    // object, and the restart command rebuilds the listener with the same
    // sinks and data directory.
    let status = transport::McpStatusHandle::for_config(&config);
    let deps = transport::TransportDeps {
        data_dir,
        arrivals,
        refreshes: Arc::clone(&refreshes),
        comment_changes,
    };
    // A failed bind is a normal, user-visible condition (a configured port
    // collision): the app starts and the Settings MCP section shows the
    // error instead of startup failing.
    let endpoint = tauri::async_runtime::block_on(transport::start(
        pool.clone(),
        deps.clone(),
        config,
        status.clone(),
    ))
    .map_err(|error| format!("Could not start the agent endpoint: {error}"))?;
    app.manage(refreshes);
    app.manage(deps);
    app.manage(status);
    app.manage(transport::ListenerOwner(tokio::sync::Mutex::new(endpoint)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::plain_path;
    use super::{
        checkout_label, checkout_store_key, connect_store_pool, set_aside_store, StoreOpenError,
        SqliteConnectOptions,
    };
    use std::path::{Path, PathBuf};

    fn layout_path(parts: &[&str]) -> PathBuf {
        parts.iter().collect::<PathBuf>()
    }

    #[test]
    fn checkout_store_key_is_stable_and_case_insensitive() {
        let dir = layout_path(&["base", "X", "src-tauri", "target", "debug"]);
        assert_eq!(checkout_store_key(&dir), checkout_store_key(&dir));
        let upper = layout_path(&["BASE", "X", "SRC-TAURI", "TARGET", "DEBUG"]);
        assert_eq!(checkout_store_key(&dir), checkout_store_key(&upper));
        let other = layout_path(&["base", "worktree-ai", "src-tauri", "target", "debug"]);
        assert_ne!(checkout_store_key(&dir), checkout_store_key(&other));
    }

    #[test]
    fn checkout_label_reads_the_standard_dev_layout() {
        let dir = layout_path(&["base", "X", "src-tauri", "target", "debug"]);
        assert_eq!(checkout_label(&dir).as_deref(), Some("X"));
        // Non-standard layouts fall back to the hash-only name.
        let plain = layout_path(&["base", "debug"]);
        assert_eq!(checkout_label(&plain), None);
        let no_crate = layout_path(&["base", "X", "target", "debug"]);
        assert_eq!(checkout_label(&no_crate), None);
    }

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

    // A store from the pre-squash migration set: version 1 recorded
    // under a description and checksum the embedded baseline cannot
    // match, with tables the current schema does not define.
    async fn seed_presquash_store(options: &SqliteConnectOptions) {
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
    }

    #[tokio::test]
    async fn mismatched_store_is_backed_up_and_rebuilt() {
        let dir = crate::testutil::test_path("store-reset");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("worktreeview.sqlite3");
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        seed_presquash_store(&options).await;

        let pool = connect_store_pool(&db_path, &options).await.unwrap();
        let versions: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&pool)
                .await
                .unwrap();
        // The embedded set: the consolidated baseline plus 0002.
        assert_eq!(versions.len(), 2);
        sqlx::query("SELECT path, name, last_opened_at, created_at FROM repos LIMIT 1")
            .fetch_all(&pool)
            .await
            .unwrap();
        pool.close().await;

        let backup = SqliteConnectOptions::new()
            .filename(dir.join("worktreeview.sqlite3.bak"));
        let preserved: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&sqlx::SqlitePool::connect_with(backup).await.unwrap())
                .await
                .unwrap();
        assert_eq!(preserved, vec![(1,)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn repeated_mismatch_overwrites_the_backup() {
        let dir = crate::testutil::test_path("store-reset-overwrite");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("worktreeview.sqlite3");
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        seed_presquash_store(&options).await;

        connect_store_pool(&db_path, &options)
            .await
            .unwrap()
            .close()
            .await;

        // Corrupt the live store's recorded checksum to force a second
        // mismatch through the same path.
        let live = sqlx::SqlitePool::connect_with(options.clone()).await.unwrap();
        sqlx::query("UPDATE _sqlx_migrations SET checksum = X'7f' WHERE version = 1")
            .execute(&live)
            .await
            .unwrap();
        live.close().await;

        let pool = connect_store_pool(&db_path, &options).await.unwrap();
        pool.close().await;

        let backups: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_string())
            .filter(|name| name.contains(".bak"))
            .collect();
        assert_eq!(backups, vec!["worktreeview.sqlite3.bak"]);
        let backup = SqliteConnectOptions::new()
            .filename(dir.join("worktreeview.sqlite3.bak"));
        let preserved: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations")
                .fetch_one(&sqlx::SqlitePool::connect_with(backup).await.unwrap())
                .await
                .unwrap();
        // The overwritten backup held the first rebuilt store: the full
        // embedded migration set.
        assert_eq!(preserved.0, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A store recorded by a version this build predates: a migration
    // version beyond the embedded set, with its own marker table.
    async fn seed_future_store(options: &SqliteConnectOptions) {
        let future = sqlx::SqlitePool::connect_with(options.clone()).await.unwrap();
        sqlx::query(
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, \
             description TEXT NOT NULL, installed_on TIMESTAMP NOT NULL \
             DEFAULT CURRENT_TIMESTAMP, success BOOLEAN NOT NULL, \
             checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)",
        )
        .execute(&future)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) \
             VALUES (99, 'from-the-future', 1, X'00', 0)",
        )
        .execute(&future)
        .await
        .unwrap();
        future.close().await;
    }

    #[tokio::test]
    async fn store_from_newer_version_refuses_untouched() {
        let dir = crate::testutil::test_path("store-reset-refusal");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("worktreeview.sqlite3");
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        seed_future_store(&options).await;

        let error = connect_store_pool(&db_path, &options).await.unwrap_err();
        assert!(matches!(error, StoreOpenError::NewerStore));
        assert!(!dir.join("worktreeview.sqlite3.bak").exists());

        let untouched: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&sqlx::SqlitePool::connect_with(options).await.unwrap())
                .await
                .unwrap();
        assert_eq!(untouched, vec![(99,)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Start fresh consents to the same set-aside the checksum path uses,
    // so this build rebuilds while the newer version keeps its data.
    #[tokio::test]
    async fn newer_store_is_set_aside_and_rebuilt_after_consent() {
        let dir = crate::testutil::test_path("store-reset-newer");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("worktreeview.sqlite3");
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        seed_future_store(&options).await;

        let error = connect_store_pool(&db_path, &options).await.unwrap_err();
        assert!(matches!(error, StoreOpenError::NewerStore));

        set_aside_store(&db_path).unwrap();

        let pool = connect_store_pool(&db_path, &options).await.unwrap();
        let versions: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&pool)
                .await
                .unwrap();
        // The rebuilt store carries the full embedded migration set.
        assert_eq!(versions.len(), 2);
        sqlx::query("SELECT path, name, last_opened_at, created_at FROM repos LIMIT 1")
            .fetch_all(&pool)
            .await
            .unwrap();
        pool.close().await;

        let backup = SqliteConnectOptions::new()
            .filename(dir.join("worktreeview.sqlite3.bak"));
        let preserved: Vec<(i64,)> =
            sqlx::query_as("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&sqlx::SqlitePool::connect_with(backup).await.unwrap())
                .await
                .unwrap();
        assert_eq!(preserved, vec![(99,)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
