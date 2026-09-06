use crate::git::CommitInfo;
use sqlx::{Row, SqlitePool};

// History cache keyed by resolved SHAs: a branch move changes the resolved
// start SHA, so the key changes and freshness needs no TTLs or invalidation.
// Reads and writes are optimizations: any failure degrades to a cache miss
// or a dropped write, never an error surfaced to the caller. Only
// `carry_repo_path` propagates, because it runs inside startup
// normalization, which owns the error contract.

pub(crate) async fn lookup_log_page(
    pool: &SqlitePool,
    repo_path: &str,
    start_sha: &str,
    against_sha: &str,
    skip: u32,
    limit: usize,
) -> Option<(Vec<CommitInfo>, bool)> {
    let Some(Some(row)) = sqlx::query(
        "SELECT commit_shas, has_more FROM log_pages \
         WHERE repo_path = ? AND start_sha = ? AND against_sha = ? AND skip = ? AND limit_value = ?",
    )
    .bind(repo_path)
    .bind(start_sha)
    .bind(against_sha)
    .bind(skip)
    .bind(limit as i64)
    .fetch_optional(pool)
    .await
    .ok() else {
        return None;
    };
    let has_more = row.try_get::<i64, _>("has_more").ok()? != 0;
    let commit_shas: String = row.try_get("commit_shas").ok()?;
    let shas: Vec<String> = serde_json::from_str(&commit_shas).ok()?;
    let mut commits = Vec::with_capacity(shas.len());
    for sha in &shas {
        let Some(Some(row)) =
            sqlx::query("SELECT subject, author, date, refs, parents FROM commits WHERE sha = ?")
                .bind(sha)
                .fetch_optional(pool)
                .await
                .ok()
        else {
            return None;
        };
        let refs: String = row.try_get("refs").ok()?;
        let parents: String = row.try_get("parents").ok()?;
        commits.push(CommitInfo {
            sha: sha.clone(),
            subject: row.try_get("subject").ok()?,
            author: row.try_get("author").ok()?,
            date: row.try_get("date").ok()?,
            refs: serde_json::from_str(&refs).ok()?,
            parents: serde_json::from_str(&parents).ok()?,
            default_base_ancestor: false,
        });
    }
    Some((commits, has_more))
}

pub(crate) async fn store_log_page(
    pool: &SqlitePool,
    repo_path: &str,
    start_sha: &str,
    against_sha: &str,
    skip: u32,
    limit: usize,
    commits: &[CommitInfo],
    has_more: bool,
) {
    let Ok(commit_shas) = serde_json::to_string(
        &commits
            .iter()
            .map(|commit| commit.sha.as_str())
            .collect::<Vec<_>>(),
    ) else {
        return;
    };
    for commit in commits {
        let Ok(refs) = serde_json::to_string(&commit.refs) else {
            return;
        };
        let Ok(parents) = serde_json::to_string(&commit.parents) else {
            return;
        };
        // Commit content is immutable, so a stored SHA row is never updated.
        let _ = sqlx::query(
            "INSERT OR IGNORE INTO commits (sha, subject, author, date, refs, parents) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&commit.sha)
        .bind(&commit.subject)
        .bind(&commit.author)
        .bind(&commit.date)
        .bind(refs)
        .bind(parents)
        .execute(pool)
        .await;
    }
    let _ = sqlx::query(
        "INSERT INTO log_pages \
         (repo_path, start_sha, against_sha, skip, limit_value, commit_shas, has_more) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(repo_path, start_sha, against_sha, skip, limit_value) DO UPDATE SET \
         commit_shas = excluded.commit_shas, has_more = excluded.has_more",
    )
    .bind(repo_path)
    .bind(start_sha)
    .bind(against_sha)
    .bind(skip)
    .bind(limit as i64)
    .bind(commit_shas)
    .bind(i64::from(has_more))
    .execute(pool)
    .await;
}

pub(crate) async fn lookup_marks(
    pool: &SqlitePool,
    shas: &[String],
    against_sha: &str,
) -> Option<Vec<bool>> {
    let mut flags = Vec::with_capacity(shas.len());
    for sha in shas {
        let Some(Some(non_ancestor)) = sqlx::query_scalar::<_, i64>(
            "SELECT non_ancestor FROM ancestry_marks WHERE commit_sha = ? AND against_sha = ?",
        )
        .bind(sha)
        .bind(against_sha)
        .fetch_optional(pool)
        .await
        .ok() else {
            return None;
        };
        flags.push(non_ancestor != 0);
    }
    Some(flags)
}

pub(crate) async fn store_marks(
    pool: &SqlitePool,
    shas: &[String],
    against_sha: &str,
    non_ancestors: &[bool],
) {
    for (sha, non_ancestor) in shas.iter().zip(non_ancestors) {
        let _ = sqlx::query(
            "INSERT INTO ancestry_marks (commit_sha, against_sha, non_ancestor) \
             VALUES (?, ?, ?) \
             ON CONFLICT(commit_sha, against_sha) DO UPDATE SET \
             non_ancestor = excluded.non_ancestor",
        )
        .bind(sha)
        .bind(against_sha)
        .bind(i64::from(*non_ancestor))
        .execute(pool)
        .await;
    }
}

// Re-key repo-scoped cache rows when normalize_stored_paths rewrites a
// verbatim path. Plain-path rows win if both forms exist, matching that
// function's repos-table precedent.
pub(crate) async fn carry_repo_path(
    pool: &SqlitePool,
    old_path: &str,
    new_path: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO log_pages \
         (repo_path, start_sha, against_sha, skip, limit_value, commit_shas, has_more) \
         SELECT ?, start_sha, against_sha, skip, limit_value, commit_shas, has_more \
         FROM log_pages WHERE repo_path = ?",
    )
    .bind(new_path)
    .bind(old_path)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM log_pages WHERE repo_path = ?")
        .bind(old_path)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{seed_repo, test_pool};

    fn commit(sha: &str, subject: &str) -> CommitInfo {
        CommitInfo {
            sha: sha.into(),
            subject: subject.into(),
            author: "A U Thor".into(),
            date: "2026-01-02T03:04:05+00:00".into(),
            refs: if subject == "decorated" {
                vec!["refs/heads/main".into()]
            } else {
                Vec::new()
            },
            parents: Vec::new(),
            default_base_ancestor: false,
        }
    }

    #[tokio::test]
    async fn log_pages_round_trip_with_marks() {
        let pool = test_pool().await;
        seed_repo(&pool, "/repo").await;
        seed_repo(&pool, "/other").await;
        let commits = [
            commit(&"a".repeat(40), "decorated"),
            commit(&"b".repeat(40), "plain"),
        ];
        assert!(lookup_log_page(&pool, "/repo", &"a".repeat(40), "", 0, 2)
            .await
            .is_none());

        store_log_page(&pool, "/repo", &"a".repeat(40), "", 0, 2, &commits, true).await;
        let non_ancestors = [false, true];
        store_marks(
            &pool,
            &[commits[0].sha.clone(), commits[1].sha.clone()],
            &"c".repeat(40),
            &non_ancestors,
        )
        .await;

        let (cached, has_more) = lookup_log_page(&pool, "/repo", &"a".repeat(40), "", 0, 2)
            .await
            .unwrap();
        assert!(has_more);
        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].sha, commits[0].sha);
        assert_eq!(cached[0].refs, ["refs/heads/main"]);
        assert_eq!(cached[1].subject, "plain");

        let marks = lookup_marks(
            &pool,
            &[commits[0].sha.clone(), commits[1].sha.clone()],
            &"c".repeat(40),
        )
        .await
        .unwrap();
        assert_eq!(marks, [false, true]);

        // A different key misses, and missing mark rows read as incomplete.
        assert!(lookup_log_page(&pool, "/other", &"a".repeat(40), "", 0, 2)
            .await
            .is_none());
        assert!(
            lookup_marks(&pool, &[commits[0].sha.clone()], &"d".repeat(40))
                .await
                .is_none()
        );
        assert_eq!(
            lookup_marks(&pool, &[], &"c".repeat(40)).await,
            Some(Vec::new())
        );

        // Pages store payloads, not marks: an identical cached page without
        // `against` carries no ancestry flags.
        assert!(cached.iter().all(|commit| !commit.default_base_ancestor));
    }

    #[tokio::test]
    async fn stored_page_upsert_replaces_page_rows_and_ignores_commit_duplicates() {
        let pool = test_pool().await;
        seed_repo(&pool, "/repo").await;
        let sha = "a".repeat(40);
        let first = [commit(&sha, "original")];
        store_log_page(&pool, "/repo", &sha, "", 0, 1, &first, false).await;
        let reworded = [commit(&sha, "original")];
        store_log_page(&pool, "/repo", &sha, "", 0, 1, &reworded, true).await;

        let (cached, has_more) = lookup_log_page(&pool, "/repo", &sha, "", 0, 1)
            .await
            .unwrap();
        assert!(has_more);
        assert_eq!(cached[0].subject, "original");
    }
}
