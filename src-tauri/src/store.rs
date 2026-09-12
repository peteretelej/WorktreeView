use crate::git::{ensure_work_tree, Worktree};
use crate::{canonical_path, plain_path, CommandError};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Repo {
    path: String,
    name: String,
    worktrees: Vec<Worktree>,
    pinned_at: Option<i64>,
}

impl Repo {
    // The canonical path as stored, for announce payloads keyed on it.
    pub(crate) fn path(&self) -> &str {
        &self.path
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    System,
    Light,
    Dark,
}

impl Theme {
    fn as_value(&self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    fn from_value(value: &str) -> Option<Self> {
        match value {
            "system" => Some(Theme::System),
            "light" => Some(Theme::Light),
            "dark" => Some(Theme::Dark),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DiffLayout {
    Unified,
    Split,
}

impl DiffLayout {
    fn as_value(&self) -> &'static str {
        match self {
            DiffLayout::Unified => "unified",
            DiffLayout::Split => "split",
        }
    }

    fn from_value(value: &str) -> Option<Self> {
        match value {
            "unified" => Some(DiffLayout::Unified),
            "split" => Some(DiffLayout::Split),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ChangedFilesView {
    Tree,
    List,
    Details,
}

impl ChangedFilesView {
    fn as_value(&self) -> &'static str {
        match self {
            ChangedFilesView::Tree => "tree",
            ChangedFilesView::List => "list",
            ChangedFilesView::Details => "details",
        }
    }

    fn from_value(value: &str) -> Option<Self> {
        match value {
            "tree" => Some(ChangedFilesView::Tree),
            "list" => Some(ChangedFilesView::List),
            "details" => Some(ChangedFilesView::Details),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Settings {
    pub theme: Theme,
    pub diff_layout: DiffLayout,
    pub whitespace_visible: bool,
    pub line_wrap: bool,
    pub syntax_visible: bool,
    pub inline_comments_visible: bool,
    pub changed_files_view: ChangedFilesView,
    pub zoom: f64,
    pub mcp_enabled: bool,
    pub mcp_listen_address: String,
    pub mcp_port: u16,
}

// Bounds cover the frontend's zoom level set (src/zoom.ts); stored values
// outside them are clamped on both read and write.
const MIN_ZOOM: f64 = 0.5;
const MAX_ZOOM: f64 = 2.0;

fn clamp_zoom(zoom: f64) -> f64 {
    if zoom.is_finite() { zoom.clamp(MIN_ZOOM, MAX_ZOOM) } else { Settings::default().zoom }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            diff_layout: DiffLayout::Unified,
            whitespace_visible: false,
            line_wrap: false,
            syntax_visible: true,
            inline_comments_visible: true,
            changed_files_view: ChangedFilesView::Tree,
            zoom: 1.0,
            mcp_enabled: true,
            mcp_listen_address: "127.0.0.1".into(),
            mcp_port: 9888,
        }
    }
}

fn settings_bool_value(flag: bool) -> &'static str {
    if flag { "true" } else { "false" }
}

fn settings_bool_from_value(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
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

pub(crate) async fn load_repos(pool: &SqlitePool) -> Result<Vec<Repo>, CommandError> {
    let rows = sqlx::query(
        "SELECT path, name, pinned_at FROM repos ORDER BY last_opened_at DESC, created_at DESC, path ASC",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Repo {
                path: row.try_get("path")?,
                name: row.try_get("name")?,
                worktrees: Vec::new(),
                pinned_at: row.try_get("pinned_at")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

// The MCP face's repo rows: the full stored row (path, name, timestamps,
// pin), not the UI's Repo shape.
#[derive(Debug, Serialize)]
pub struct RepoRow {
    pub path: String,
    pub name: String,
    pub created_at: i64,
    pub last_opened_at: i64,
    pub pinned_at: Option<i64>,
}

pub(crate) async fn list_repo_rows_in_pool(pool: &SqlitePool) -> Result<Vec<RepoRow>, CommandError> {
    let rows = sqlx::query(
        "SELECT path, name, created_at, last_opened_at, pinned_at FROM repos \
         ORDER BY last_opened_at DESC, created_at DESC, path ASC",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(RepoRow {
                path: row.try_get("path")?,
                name: row.try_get("name")?,
                created_at: row.try_get("created_at")?,
                last_opened_at: row.try_get("last_opened_at")?,
                pinned_at: row.try_get("pinned_at")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

// Older versions stored Windows verbatim paths (\\?\C:\...) on open; move
// them to plain paths so the UI shows what the user selected. A plain-path
// row wins if both forms already exist.
pub(crate) async fn normalize_stored_paths(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM repos")
        .fetch_all(pool)
        .await?;
    for path in paths {
        let simplified = plain_path(Path::new(&path));
        let Some(simplified) = simplified.to_str() else {
            continue;
        };
        if simplified == path {
            continue;
        }
        sqlx::query(
            "INSERT OR IGNORE INTO repos (path, name, last_opened_at, created_at, pinned_at) \
             SELECT ?1, name, last_opened_at, created_at, pinned_at FROM repos WHERE path = ?2",
        )
        .bind(simplified)
        .bind(&path)
        .execute(pool)
        .await?;
        crate::cache::carry_repo_path(pool, &path, simplified).await?;
        crate::retrospection::carry_repo_path(pool, &path, simplified).await?;
        sqlx::query("DELETE FROM repos WHERE path = ?")
            .bind(&path)
            .execute(pool)
            .await?;
    }
    Ok(())
}

// Registry-only removal: the row delete cascades to repo-scoped cache and
// retrospection tables; nothing on disk is touched. Unknown paths are
// already gone, so removal is idempotent.
pub(crate) async fn remove_repo_in_pool(pool: &SqlitePool, path: &str) -> Result<(), CommandError> {
    sqlx::query("DELETE FROM repos WHERE path = ?")
        .bind(path)
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn open_repo_path(path: &str, pool: &SqlitePool) -> Result<Repo, CommandError> {
    let canonical = canonical_path(path)?;
    ensure_work_tree(&canonical).await?;
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
    let pinned_at = sqlx::query_scalar::<_, Option<i64>>("SELECT pinned_at FROM repos WHERE path = ?")
        .bind(path)
        .fetch_one(pool)
        .await?;
    Ok(Repo {
        path: path.into(),
        name: name.into(),
        worktrees: Vec::new(),
        pinned_at,
    })
}

pub(crate) async fn set_repo_pinned_in_pool(
    pool: &SqlitePool,
    path: &str,
    pinned: bool,
) -> Result<Option<i64>, CommandError> {
    let pinned_at = pinned.then(now_millis);
    let result = sqlx::query("UPDATE repos SET pinned_at = ? WHERE path = ?")
        .bind(pinned_at)
        .bind(path)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(CommandError::new(
            "persistence",
            "The repository is not stored and cannot be pinned.",
        ));
    }
    Ok(pinned_at)
}

// Missing or unparseable rows fall back to defaults; partial or corrupt
// storage never fails the read.
pub(crate) async fn get_settings_in_pool(pool: &SqlitePool) -> Result<Settings, CommandError> {
    let rows = sqlx::query("SELECT key, value FROM settings")
        .fetch_all(pool)
        .await?;
    let mut settings = Settings::default();
    for row in rows {
        let key: String = row.try_get("key")?;
        let value: String = row.try_get("value")?;
        match key.as_str() {
            "theme" => {
                if let Some(theme) = Theme::from_value(&value) {
                    settings.theme = theme;
                }
            }
            "diff_layout" => {
                if let Some(diff_layout) = DiffLayout::from_value(&value) {
                    settings.diff_layout = diff_layout;
                }
            }
            "whitespace_visible" => {
                if let Some(flag) = settings_bool_from_value(&value) {
                    settings.whitespace_visible = flag;
                }
            }
            "line_wrap" => {
                if let Some(flag) = settings_bool_from_value(&value) {
                    settings.line_wrap = flag;
                }
            }
            "syntax_visible" => {
                if let Some(flag) = settings_bool_from_value(&value) {
                    settings.syntax_visible = flag;
                }
            }
            "inline_comments_visible" => {
                if let Some(flag) = settings_bool_from_value(&value) {
                    settings.inline_comments_visible = flag;
                }
            }
            "changed_files_view" => {
                if let Some(view) = ChangedFilesView::from_value(&value) {
                    settings.changed_files_view = view;
                }
            }
            "zoom" => {
                if let Ok(zoom) = value.parse::<f64>() {
                    settings.zoom = clamp_zoom(zoom);
                }
            }
            "mcp_enabled" => {
                if let Some(flag) = settings_bool_from_value(&value) {
                    settings.mcp_enabled = flag;
                }
            }
            "mcp_listen_address" => {
                if !value.trim().is_empty() {
                    settings.mcp_listen_address = value;
                }
            }
            "mcp_port" => {
                if let Ok(port) = value.parse::<u16>() {
                    settings.mcp_port = port;
                }
            }
            _ => {}
        }
    }
    Ok(settings)
}

pub(crate) async fn set_settings_in_pool(
    pool: &SqlitePool,
    settings: &Settings,
) -> Result<Settings, CommandError> {
    let zoom = clamp_zoom(settings.zoom);
    let values = [
        ("theme", settings.theme.as_value().to_string()),
        ("diff_layout", settings.diff_layout.as_value().to_string()),
        (
            "whitespace_visible",
            settings_bool_value(settings.whitespace_visible).to_string(),
        ),
        ("line_wrap", settings_bool_value(settings.line_wrap).to_string()),
        (
            "syntax_visible",
            settings_bool_value(settings.syntax_visible).to_string(),
        ),
        (
            "inline_comments_visible",
            settings_bool_value(settings.inline_comments_visible).to_string(),
        ),
        (
            "changed_files_view",
            settings.changed_files_view.as_value().to_string(),
        ),
        ("zoom", zoom.to_string()),
        (
            "mcp_enabled",
            settings_bool_value(settings.mcp_enabled).to_string(),
        ),
        ("mcp_listen_address", settings.mcp_listen_address.clone()),
        ("mcp_port", settings.mcp_port.to_string()),
    ];
    let mut transaction = pool.begin().await?;
    for (key, value) in values {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    let mut persisted = settings.clone();
    persisted.zoom = zoom;
    Ok(persisted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{test_path, test_pool};

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

    #[tokio::test]
    async fn repo_pins_set_clear_and_load() {
        let pool = test_pool().await;
        upsert_repo(&pool, "/pinned", "pinned", 100).await.unwrap();
        let pinned = set_repo_pinned_in_pool(&pool, "/pinned", true).await.unwrap();
        assert!(pinned.is_some());
        assert_eq!(load_repos(&pool).await.unwrap()[0].pinned_at, pinned);
        assert_eq!(set_repo_pinned_in_pool(&pool, "/pinned", false).await.unwrap(), None);
        assert_eq!(load_repos(&pool).await.unwrap()[0].pinned_at, None);
    }

    #[tokio::test]
    async fn pinning_unknown_repo_returns_persistence_error() {
        let pool = test_pool().await;
        let error = set_repo_pinned_in_pool(&pool, "/missing", true).await.unwrap_err();
        assert_eq!(error.code, "persistence");
    }

    #[tokio::test]
    async fn settings_default_when_table_is_empty() {
        let pool = test_pool().await;
        assert_eq!(
            get_settings_in_pool(&pool).await.unwrap(),
            Settings::default()
        );
    }

    #[tokio::test]
    async fn settings_round_trip_persists_all_fields() {
        let pool = test_pool().await;
        let settings = Settings {
            theme: Theme::Dark,
            diff_layout: DiffLayout::Split,
            whitespace_visible: true,
            line_wrap: true,
            syntax_visible: true,
            inline_comments_visible: false,
            changed_files_view: ChangedFilesView::List,
            zoom: 1.25,
            mcp_enabled: false,
            mcp_listen_address: "0.0.0.0".into(),
            mcp_port: 9899,
        };
        let persisted = set_settings_in_pool(&pool, &settings).await.unwrap();
        assert_eq!(persisted, settings);
        assert_eq!(get_settings_in_pool(&pool).await.unwrap(), settings);
    }

    #[tokio::test]
    async fn out_of_range_zoom_is_clamped_on_write() {
        let pool = test_pool().await;
        let settings = Settings {
            zoom: 9.0,
            ..Settings::default()
        };
        let persisted = set_settings_in_pool(&pool, &settings).await.unwrap();
        assert_eq!(persisted.zoom, MAX_ZOOM);
        assert_eq!(get_settings_in_pool(&pool).await.unwrap().zoom, MAX_ZOOM);
    }

    #[tokio::test]
    async fn corrupt_settings_rows_fall_back_to_defaults() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ('theme', 'neon'), ('diff_layout', 'fancy'), ('whitespace_visible', 'maybe'), ('line_wrap', 'sometimes'), ('changed_files_view', 'columns'), ('zoom', 'huge'), ('mcp_enabled', 'perhaps'), ('mcp_listen_address', ''), ('mcp_port', 'not-a-port')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            get_settings_in_pool(&pool).await.unwrap(),
            Settings::default()
        );
    }

    #[tokio::test]
    async fn verbatim_repo_paths_are_rewritten_to_plain_paths() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO repos (path, name, last_opened_at, created_at, pinned_at) VALUES (?, 'demo', 50, 40, 7)")
            .bind(r"\\?\C:\repos\demo")
            .execute(&pool)
            .await
            .unwrap();
        normalize_stored_paths(&pool).await.unwrap();
        let row = sqlx::query("SELECT path, name, pinned_at FROM repos")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("path"), r"C:\repos\demo");
        assert_eq!(row.get::<String, _>("name"), "demo");
        assert_eq!(row.get::<Option<i64>, _>("pinned_at"), Some(7));
    }

    #[tokio::test]
    async fn unc_verbatim_repo_paths_keep_the_share_prefix() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO repos (path, name, last_opened_at, created_at, pinned_at) VALUES (?, 'share', 50, 40, NULL)")
            .bind(r"\\?\UNC\server\share\demo")
            .execute(&pool)
            .await
            .unwrap();
        normalize_stored_paths(&pool).await.unwrap();
        let path: String = sqlx::query_scalar("SELECT path FROM repos")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(path, r"\\server\share\demo");
    }

    #[tokio::test]
    async fn plain_rows_win_over_verbatim_duplicates() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO repos (path, name, last_opened_at, created_at) VALUES (?, 'demo', 51, 40)")
            .bind(r"C:\repos\demo")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO repos (path, name, last_opened_at, created_at) VALUES (?, 'demo', 50, 40)")
            .bind(r"\\?\C:\repos\demo")
            .execute(&pool)
            .await
            .unwrap();
        normalize_stored_paths(&pool).await.unwrap();
        let rows = load_repos(&pool).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, r"C:\repos\demo");
    }

    #[tokio::test]
    async fn normalization_carries_cached_pages_to_the_plain_path() {
        let pool = test_pool().await;
        let verbatim = r"\\?\C:\repos\demo";
        let plain = r"C:\repos\demo";
        sqlx::query("INSERT INTO repos (path, name, last_opened_at, created_at, pinned_at) VALUES (?, 'demo', 50, 40, 7)")
            .bind(verbatim)
            .execute(&pool)
            .await
            .unwrap();
        let start_sha = "a".repeat(40);
        let commits = [crate::git::CommitInfo {
            sha: start_sha.clone(),
            subject: "carried".into(),
            author: "A U Thor".into(),
            date: "2026-01-02T03:04:05+00:00".into(),
            refs: Vec::new(),
            parents: Vec::new(),
            default_base_ancestor: false,
        }];
        crate::cache::store_log_page(&pool, verbatim, &start_sha, "", 0, 1, &commits, false).await;

        normalize_stored_paths(&pool).await.unwrap();

        let (cached, has_more) = crate::cache::lookup_log_page(&pool, plain, &start_sha, "", 0, 1)
            .await
            .unwrap();
        assert!(!has_more);
        assert_eq!(cached[0].subject, "carried");
        assert!(
            crate::cache::lookup_log_page(&pool, verbatim, &start_sha, "", 0, 1)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn normalization_carries_retrospected_surfaces_to_the_plain_path() {
        let pool = test_pool().await;
        let verbatim = r"\\?\C:\repos\demo";
        let plain = r"C:\repos\demo";
        sqlx::query("INSERT INTO repos (path, name, last_opened_at, created_at, pinned_at) VALUES (?, 'demo', 50, 40, 7)")
            .bind(verbatim)
            .execute(&pool)
            .await
            .unwrap();
        crate::retrospection::record_surface_open(
            &pool,
            verbatim,
            "branch",
            "refs/heads/feature",
            "feature",
            "refs/heads/feature",
            &"a".repeat(40),
        )
        .await;
        sqlx::query(
            "UPDATE retrospected_surfaces SET pinned_at = 7, origin = 'pin' \
             WHERE repo_path = ? AND identity_key = 'refs/heads/feature'",
        )
        .bind(verbatim)
        .execute(&pool)
        .await
        .unwrap();

        normalize_stored_paths(&pool).await.unwrap();

        let rows: Vec<(String, Option<i64>, String)> =
            sqlx::query_as("SELECT repo_path, pinned_at, origin FROM retrospected_surfaces")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, [(plain.to_string(), Some(7), "pin".to_string())]);
    }

    #[tokio::test]
    async fn remove_repo_deletes_the_row_and_cascades() {
        let pool = test_pool().await;
        upsert_repo(&pool, "/kept", "kept", 1).await.unwrap();
        upsert_repo(&pool, "/dropped", "dropped", 2).await.unwrap();
        let commits = [crate::git::CommitInfo {
            sha: "a".repeat(40),
            subject: "cached".into(),
            author: "A U Thor".into(),
            date: "2026-01-02T03:04:05+00:00".into(),
            refs: Vec::new(),
            parents: Vec::new(),
            default_base_ancestor: false,
        }];
        crate::cache::store_log_page(
            &pool,
            "/dropped",
            &"a".repeat(40),
            "",
            0,
            1,
            &commits,
            false,
        )
        .await;
        crate::retrospection::record_surface_open(
            &pool,
            "/dropped",
            "branch",
            "refs/heads/dropped",
            "dropped",
            "refs/heads/dropped",
            &"a".repeat(40),
        )
        .await;

        remove_repo_in_pool(&pool, "/dropped").await.unwrap();
        // Unknown paths are already gone: removal is idempotent.
        remove_repo_in_pool(&pool, "/dropped").await.unwrap();

        assert_eq!(load_repos(&pool).await.unwrap()[0].path, "/kept");
        let pages: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM log_pages WHERE repo_path = '/dropped'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pages, 0);
        let surfaces: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM retrospected_surfaces WHERE repo_path = '/dropped'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(surfaces, 0);
    }

    #[tokio::test]
    async fn deleting_a_repo_cascades_to_its_cached_rows() {
        let pool = test_pool().await;
        upsert_repo(&pool, "/kept", "kept", 1).await.unwrap();
        upsert_repo(&pool, "/dropped", "dropped", 2).await.unwrap();
        let commits = [crate::git::CommitInfo {
            sha: "a".repeat(40),
            subject: "shared".into(),
            author: "A U Thor".into(),
            date: "2026-01-02T03:04:05+00:00".into(),
            refs: Vec::new(),
            parents: Vec::new(),
            default_base_ancestor: false,
        }];
        crate::cache::store_log_page(&pool, "/kept", &"a".repeat(40), "", 0, 1, &commits, false)
            .await;
        crate::cache::store_log_page(
            &pool,
            "/dropped",
            &"a".repeat(40),
            "",
            0,
            1,
            &commits,
            false,
        )
        .await;
        crate::retrospection::record_surface_open(
            &pool,
            "/kept",
            "branch",
            "refs/heads/kept",
            "kept",
            "refs/heads/kept",
            &"a".repeat(40),
        )
        .await;
        crate::retrospection::record_surface_open(
            &pool,
            "/dropped",
            "branch",
            "refs/heads/dropped",
            "dropped",
            "refs/heads/dropped",
            &"a".repeat(40),
        )
        .await;

        sqlx::query("DELETE FROM repos WHERE path = '/dropped'")
            .execute(&pool)
            .await
            .unwrap();

        // Repo-scoped rows cascade away with the repos row; shared content
        // rows keyed by SHA survive.
        let page_repos: Vec<String> =
            sqlx::query_scalar("SELECT repo_path FROM log_pages ORDER BY repo_path")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(page_repos, ["/kept"]);
        let surface_repos: Vec<String> =
            sqlx::query_scalar("SELECT repo_path FROM retrospected_surfaces ORDER BY repo_path")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(surface_repos, ["/kept"]);
        let commit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM commits")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(commit_count, 1);
    }

    // The consolidated baseline is create-only: the exact file the migration
    // runner embeds must build every table, with repo-scoped cascades, in
    // one step.
    #[tokio::test]
    async fn the_consolidated_baseline_cascades_repo_scoped_rows() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::new())
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0001_init.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO repos (path, name, last_opened_at, created_at) \
             VALUES ('/kept', 'kept', 1, 1), ('/dropped', 'dropped', 2, 2)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let shared_sha = "a".repeat(40);
        for repo_path in ["/kept", "/dropped"] {
            sqlx::query(
                "INSERT INTO log_pages (repo_path, start_sha, against_sha, skip, limit_value, commit_shas, has_more) \
                 VALUES (?, ?, '', 0, 1, ?, 0)",
            )
            .bind(repo_path)
            .bind(&shared_sha)
            .bind(format!("[\"{shared_sha}\"]"))
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO retrospected_surfaces (repo_path, kind, identity_key, label, detail, head_sha, last_seen_at) \
                 VALUES (?, 'branch', 'refs/heads/live', 'live', 'refs/heads/live', ?, 5)",
            )
            .bind(repo_path)
            .bind(&shared_sha)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO reviews (repo_path, base_sha, target_key, target_kind, created_at) \
                 VALUES (?, ?, ?, 'worktree', 3)",
            )
            .bind(repo_path)
            .bind(&shared_sha)
            .bind(repo_path)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO commits (sha, subject, author, date, refs, parents) \
             VALUES (?, 's', 'a', 'd', '[]', '[]')",
        )
        .bind(&shared_sha)
        .execute(&pool)
        .await
        .unwrap();
        let dropped_reviews: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM reviews WHERE repo_path = '/dropped'")
                .fetch_all(&pool)
                .await
                .unwrap();
        let dropped_review = dropped_reviews[0];
        sqlx::query(
            "INSERT INTO submissions (review_id, agent_name, agent_model, sections_json, created_at) \
             VALUES (?, 'agent', 'model', '{}', 4)",
        )
        .bind(dropped_review)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO comments (review_id, author_kind, author_name, body, created_at) \
             VALUES (?, 'human', 'you', 'note', 5)",
        )
        .bind(dropped_review)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("DELETE FROM repos WHERE path = '/dropped'")
            .execute(&pool)
            .await
            .unwrap();

        // Repo-scoped rows cascade away with the repos row; shared content
        // rows keyed by SHA survive.
        let page_repos: Vec<String> =
            sqlx::query_scalar("SELECT repo_path FROM log_pages ORDER BY repo_path")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(page_repos, ["/kept"]);
        let surface_repos: Vec<String> =
            sqlx::query_scalar("SELECT repo_path FROM retrospected_surfaces ORDER BY repo_path")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(surface_repos, ["/kept"]);
        let review_repos: Vec<String> =
            sqlx::query_scalar("SELECT repo_path FROM reviews ORDER BY repo_path")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(review_repos, ["/kept"]);
        let submission_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM submissions").fetch_one(&pool).await.unwrap();
        assert_eq!(submission_count, 0);
        let comment_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM comments").fetch_one(&pool).await.unwrap();
        assert_eq!(comment_count, 0);
        let commit_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM commits").fetch_one(&pool).await.unwrap();
        assert_eq!(commit_count, 1);
    }
}
