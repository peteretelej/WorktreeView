use crate::git::{git_execution_error, parse_worktrees, run_git};
use crate::store::now_millis;
use crate::{canonical_path, plain_path, CommandError};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct GoneSurface {
    kind: String,
    identity_key: String,
    label: String,
    detail: String,
    head_sha: String,
    last_seen_at: i64,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct SurfaceListing {
    gone: Vec<GoneSurface>,
}

// Open-time retrospection: remember the identity and last resolved head of
// every reviewed surface so it can still be listed and opened after it
// disappears. Recording is best-effort like every other cache write: a
// failed upsert never fails the open that produced it. An empty `label`
// means "unknown this open; keep the recorded one".
pub(crate) async fn record_surface_open(
    pool: &SqlitePool,
    repo_path: &str,
    kind: &str,
    identity_key: &str,
    label: &str,
    detail: &str,
    head_sha: &str,
) {
    let _ = sqlx::query(
        "INSERT INTO retrospected_surfaces \
         (repo_path, kind, identity_key, label, detail, head_sha, last_seen_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (repo_path, kind, identity_key) DO UPDATE SET \
         last_seen_at = excluded.last_seen_at, \
         head_sha = excluded.head_sha, \
         label = CASE WHEN excluded.label = '' \
                      THEN retrospected_surfaces.label ELSE excluded.label END, \
         detail = excluded.detail",
    )
    .bind(repo_path)
    .bind(kind)
    .bind(identity_key)
    .bind(label)
    .bind(detail)
    .bind(head_sha)
    .bind(now_millis())
    .execute(pool)
    .await;
}

// Gone detection reads live inventory on every listing; nothing here is
// cached and listing never inserts rows.
pub(crate) async fn list_surfaces_in_pool(
    pool: &SqlitePool,
    path: &str,
) -> Result<SurfaceListing, CommandError> {
    let repo = canonical_path(path)?;
    let repo_path = repo.to_str().ok_or_else(|| {
        CommandError::new("invalid_path", "The selected path is not valid UTF-8.")
    })?;
    let (exit_code, stdout, stderr) = run_git(
        &repo,
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
    let mut live_worktrees = HashSet::new();
    for worktree in parse_worktrees(&stdout)? {
        live_worktrees.insert(live_worktree_key(&worktree.path));
    }
    let (exit_code, stdout, stderr) = run_git(
        &repo,
        &["for-each-ref", "refs/heads", "--format=%(refname)"],
    )
    .await?;
    if exit_code != 0 {
        return Err(git_execution_error(&stderr));
    }
    let live_heads: HashSet<String> = std::str::from_utf8(&stdout)
        .map_err(|_| CommandError::new("git_output_malformed", "Git returned invalid ref data."))?
        .lines()
        .map(str::to_string)
        .collect();
    let rows = sqlx::query(
        "SELECT kind, identity_key, label, detail, head_sha, last_seen_at \
         FROM retrospected_surfaces WHERE repo_path = ? \
         ORDER BY last_seen_at DESC, kind ASC, identity_key ASC",
    )
    .bind(repo_path)
    .fetch_all(pool)
    .await?;
    let mut gone = Vec::new();
    for row in rows {
        let kind: String = row.try_get("kind")?;
        let identity_key: String = row.try_get("identity_key")?;
        let live = match kind.as_str() {
            "worktree" => live_worktrees.contains(&identity_key),
            "branch" => live_heads.contains(&identity_key),
            _ => false,
        };
        if !live {
            gone.push(GoneSurface {
                kind,
                identity_key,
                label: row.try_get("label")?,
                detail: row.try_get("detail")?,
                head_sha: row.try_get("head_sha")?,
                last_seen_at: row.try_get("last_seen_at")?,
            });
        }
    }
    Ok(SurfaceListing { gone })
}

// Recorded worktree keys are platform-canonical plain paths while
// `worktree list --porcelain` emits forward-slash paths; run every live
// path through the same canonicalization before comparing. A live path
// that fails canonicalization (vanishing between listing and conversion)
// is compared in its raw form as a fallback.
fn live_worktree_key(path: &str) -> String {
    Path::new(path)
        .canonicalize()
        .map(|canonical| plain_path(&canonical).to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

// Keep `Worktree`'s fields `pub(crate)` in `git::parse.rs` so retrospection
// can read them; no accessor layer needed.

// Re-key repo-scoped retrospection rows when normalize_stored_paths
// rewrites a verbatim path, matching the cache carry's precedent: plain
// rows win if both forms exist.
pub(crate) async fn carry_repo_path(
    pool: &SqlitePool,
    old_path: &str,
    new_path: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO retrospected_surfaces \
         (repo_path, kind, identity_key, label, detail, head_sha, last_seen_at) \
         SELECT ?, kind, identity_key, label, detail, head_sha, last_seen_at \
         FROM retrospected_surfaces WHERE repo_path = ?",
    )
    .bind(new_path)
    .bind(old_path)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM retrospected_surfaces WHERE repo_path = ?")
        .bind(old_path)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::spawn_counted;
    use crate::review::{commit_page, review_changes};
    use crate::testutil::{test_git, test_path, test_pool, test_repo, test_rev_parse};

    type SurfaceRow = (String, String, String, String, i64);

    async fn surface_rows(pool: &SqlitePool, repo_path: &str) -> Vec<SurfaceRow> {
        sqlx::query(
            "SELECT kind, identity_key, label, head_sha, last_seen_at \
             FROM retrospected_surfaces WHERE repo_path = ?",
        )
        .bind(repo_path)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.get("kind"),
                row.get("identity_key"),
                row.get("label"),
                row.get("head_sha"),
                row.get("last_seen_at"),
            )
        })
        .collect()
    }

    fn canonical_key(path: &Path) -> String {
        plain_path(&path.canonicalize().unwrap())
            .to_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn gone_worktree_lifecycle_lists_and_reopens() {
        let repo = test_repo("retrospect-worktree");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        test_git(&repo, &["branch", "-M", "main"]);
        let worktree = test_path("retrospect-worktree-wt");
        test_git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        let worktree_path = worktree.to_str().unwrap().to_string();
        let head_sha = test_rev_parse(&worktree, "HEAD");

        // Opening the worktree's HEAD-based history records its identity and
        // caches the page; against live inventory nothing is gone yet.
        let open = commit_page(
            &pool,
            &repo_path,
            worktree_path.clone(),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(list_surfaces_in_pool(&pool, &repo_path)
            .await
            .unwrap()
            .gone
            .is_empty());

        let worktree_key = canonical_key(&worktree);
        test_git(&repo, &["worktree", "remove", &worktree_path]);
        test_git(&repo, &["branch", "-D", "feature"]);
        let listing = list_surfaces_in_pool(&pool, &repo_path).await.unwrap();
        assert_eq!(listing.gone.len(), 1);
        assert_eq!(listing.gone[0].kind, "worktree");
        assert_eq!(listing.gone[0].label, "feature");
        assert_eq!(listing.gone[0].head_sha, head_sha);
        assert_eq!(listing.gone[0].identity_key, worktree_key);

        // Reopening by the recorded head serves the cached page with no git
        // at all, even though the surface no longer exists.
        let (spawns, reopened) = spawn_counted(commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some(head_sha),
            None,
            None,
            None,
        ))
        .await;
        assert_eq!(spawns, 0);
        assert_eq!(reopened.unwrap(), open);

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn gone_branch_lifecycle_lists_and_reopens() {
        let repo = test_repo("retrospect-branch");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        test_git(&repo, &["branch", "-M", "main"]);
        test_git(&repo, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(repo.join("feature-only.txt"), "feature\n").unwrap();
        test_git(&repo, &["add", "feature-only.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "feature commit"]);
        test_git(&repo, &["checkout", "--quiet", "main"]);
        let feature_sha = test_rev_parse(&repo, "refs/heads/feature");

        let open = commit_page(
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
        assert!(list_surfaces_in_pool(&pool, &repo_path)
            .await
            .unwrap()
            .gone
            .is_empty());

        test_git(&repo, &["branch", "-D", "feature"]);
        let listing = list_surfaces_in_pool(&pool, &repo_path).await.unwrap();
        assert_eq!(listing.gone.len(), 1);
        assert_eq!(listing.gone[0].kind, "branch");
        assert_eq!(listing.gone[0].identity_key, "refs/heads/feature");
        assert_eq!(listing.gone[0].label, "feature");
        assert_eq!(listing.gone[0].head_sha, feature_sha);

        let (spawns, reopened) = spawn_counted(commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some(feature_sha),
            None,
            None,
            None,
        ))
        .await;
        assert_eq!(spawns, 0);
        assert_eq!(reopened.unwrap(), open);

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn recording_shape_matches_open_kinds() {
        let repo = test_repo("retrospect-shape");
        let pool = test_pool().await;
        let repo_path = repo.to_str().unwrap().to_string();
        let repo_key = canonical_key(&repo);
        test_git(&repo, &["branch", "-M", "main"]);
        test_git(&repo, &["tag", "v1"]);
        std::fs::write(repo.join("tracked.txt"), "change\n").unwrap();
        test_git(&repo, &["add", "tracked.txt"]);
        test_git(&repo, &["commit", "--quiet", "-m", "second"]);
        let head_sha = test_rev_parse(&repo, "HEAD");

        // A HEAD-based open records the worktree surface, labeled from the
        // page's HEAD -> decoration.
        commit_page(&pool, &repo_path, repo_path.clone(), None, None, None, None)
            .await
            .unwrap();
        let rows = surface_rows(&pool, &repo_path).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "worktree");
        assert_eq!(rows[0].1, repo_key);
        assert_eq!(rows[0].2, "main");
        assert_eq!(rows[0].3, head_sha);

        // A branch open records the branch surface under the full ref.
        commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some("refs/heads/main".into()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let rows = surface_rows(&pool, &repo_path).await;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.0 == "branch"
            && row.1 == "refs/heads/main"
            && row.2 == "main"
            && row.3 == head_sha));

        // A detached worktree open records an empty label (no HEAD ->
        // decoration), so a later nicer label would be kept by the CASE. The
        // worktree gets its own commit so the open fetches freshly in the
        // detached worktree instead of hitting a cached page that carries
        // another worktree's decoration.
        let detached = test_path("retrospect-shape-detached");
        test_git(
            &repo,
            &["worktree", "add", "--detach", detached.to_str().unwrap()],
        );
        let detached_path = detached.to_str().unwrap().to_string();
        std::fs::write(detached.join("detached.txt"), "detached\n").unwrap();
        test_git(&detached, &["add", "detached.txt"]);
        test_git(&detached, &["commit", "--quiet", "-m", "detached commit"]);
        commit_page(
            &pool,
            &repo_path,
            detached_path.clone(),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let rows = surface_rows(&pool, &repo_path).await;
        assert_eq!(rows.len(), 3);
        let detached_row = rows
            .iter()
            .find(|row| row.0 == "worktree" && row.1 == canonical_key(&detached))
            .unwrap();
        assert_eq!(detached_row.2, "");

        // Tag and raw-SHA start refs are not surfaces: they record nothing.
        commit_page(
            &pool,
            &repo_path,
            repo_path.clone(),
            Some("refs/tags/v1".into()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        commit_page(
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
        assert_eq!(surface_rows(&pool, &repo_path).await.len(), 3);

        // A review open refreshes last_seen_at and keeps the recorded label
        // when the newer open has none; only the reviewed surface refreshes.
        sqlx::query("UPDATE retrospected_surfaces SET last_seen_at = 1000")
            .execute(&pool)
            .await
            .unwrap();
        review_changes(
            &pool,
            &repo_path,
            repo_path.clone(),
            "HEAD".into(),
            None,
            false,
            false,
        )
        .await
        .unwrap();
        review_changes(
            &pool,
            &repo_path,
            repo_path.clone(),
            "refs/heads/main".into(),
            Some("refs/heads/main".into()),
            true,
            false,
        )
        .await
        .unwrap();
        let rows = surface_rows(&pool, &repo_path).await;
        let worktree_row = rows
            .iter()
            .find(|row| row.0 == "worktree" && row.1 == repo_key)
            .unwrap();
        assert_eq!(worktree_row.2, "main");
        assert!(worktree_row.4 > 1000);
        let branch_row = rows.iter().find(|row| row.0 == "branch").unwrap();
        assert_eq!(branch_row.2, "main");
        assert!(branch_row.4 > 1000);
        let detached_row = rows
            .iter()
            .find(|row| row.0 == "worktree" && row.1 != repo_key)
            .unwrap();
        assert_eq!(detached_row.4, 1000);

        test_git(&repo, &["worktree", "remove", &detached_path]);
        std::fs::remove_dir_all(repo).unwrap();
    }
}
