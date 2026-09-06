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
pub struct Settings {
    pub theme: Theme,
    pub diff_layout: DiffLayout,
    pub whitespace_visible: bool,
    pub line_wrap: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            diff_layout: DiffLayout::Unified,
            whitespace_visible: false,
            line_wrap: false,
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

fn now_millis() -> i64 {
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
        sqlx::query("DELETE FROM repos WHERE path = ?")
            .bind(&path)
            .execute(pool)
            .await?;
    }
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
            _ => {}
        }
    }
    Ok(settings)
}

pub(crate) async fn set_settings_in_pool(
    pool: &SqlitePool,
    settings: &Settings,
) -> Result<Settings, CommandError> {
    let values = [
        ("theme", settings.theme.as_value()),
        ("diff_layout", settings.diff_layout.as_value()),
        (
            "whitespace_visible",
            settings_bool_value(settings.whitespace_visible),
        ),
        ("line_wrap", settings_bool_value(settings.line_wrap)),
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
    Ok(settings.clone())
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
        };
        let persisted = set_settings_in_pool(&pool, &settings).await.unwrap();
        assert_eq!(persisted, settings);
        assert_eq!(get_settings_in_pool(&pool).await.unwrap(), settings);
    }

    #[tokio::test]
    async fn corrupt_settings_rows_fall_back_to_defaults() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ('theme', 'neon'), ('diff_layout', 'fancy'), ('whitespace_visible', 'maybe'), ('line_wrap', 'sometimes')",
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
}
