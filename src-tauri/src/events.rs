use crate::store::now_millis;
use serde::Serialize;
use sqlx::{Executor, Row, Sqlite, SqlitePool};

// The closed kind vocabulary; the migration's CHECK enforces the same set,
// so a kind constant and the schema drift together or not at all.
pub(crate) const REQUEST_CREATED: &str = "request_created";
pub(crate) const REQUEST_CLAIMED: &str = "request_claimed";
pub(crate) const REQUEST_VERDICT: &str = "request_verdict";
pub(crate) const REQUEST_RE_REQUESTED: &str = "request_re_requested";
pub(crate) const REQUEST_WITHDRAWN: &str = "request_withdrawn";
pub(crate) const SUBMISSION_DELIVERED: &str = "submission_delivered";
pub(crate) const COMMENT_POSTED: &str = "comment_posted";
pub(crate) const COMMENT_REPLIED: &str = "comment_replied";
pub(crate) const COMMENT_RESOLVED: &str = "comment_resolved";
pub(crate) const COMMENT_REOPENED: &str = "comment_reopened";
pub(crate) const SURFACE_HEAD_MOVED: &str = "surface_head_moved";
pub(crate) const REPO_ADDED: &str = "repo_added";

pub(crate) const ACTOR_HUMAN: &str = "human";
pub(crate) const ACTOR_AGENT: &str = "agent";

// The hard row bound on every listing; callers pass a page size and this
// clamps it, so no caller can turn a poll into an unbounded read.
pub(crate) const EVENT_LIST_LIMIT: usize = 500;

// One event row, written at the mutation that narrates it. The executor is
// the caller's connection or transaction, so the event commits (or rolls
// back) with the mutation it belongs to.
#[derive(Debug, Clone)]
pub(crate) struct NewEvent {
    pub(crate) repo_path: String,
    pub(crate) kind: &'static str,
    pub(crate) base_sha: Option<String>,
    pub(crate) target_key: Option<String>,
    pub(crate) target_kind: Option<String>,
    pub(crate) request_id: Option<i64>,
    pub(crate) comment_id: Option<i64>,
    pub(crate) actor_kind: &'static str,
    pub(crate) actor_name: String,
    pub(crate) summary: String,
}

pub(crate) async fn record_event<'e, E>(executor: E, event: NewEvent) -> Result<i64, sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    let result = sqlx::query(
        "INSERT INTO events (repo_path, kind, base_sha, target_key, target_kind, \
                request_id, comment_id, actor_kind, actor_name, summary, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&event.repo_path)
    .bind(event.kind)
    .bind(&event.base_sha)
    .bind(&event.target_key)
    .bind(&event.target_kind)
    .bind(event.request_id)
    .bind(event.comment_id)
    .bind(event.actor_kind)
    .bind(&event.actor_name)
    .bind(&event.summary)
    .bind(now_millis())
    .execute(executor)
    .await?;
    Ok(result.last_insert_rowid())
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct EventRow {
    pub(crate) id: i64,
    pub(crate) repo_path: String,
    pub(crate) kind: String,
    pub(crate) base_sha: Option<String>,
    pub(crate) target_key: Option<String>,
    pub(crate) target_kind: Option<String>,
    pub(crate) request_id: Option<i64>,
    pub(crate) comment_id: Option<i64>,
    pub(crate) actor_kind: String,
    pub(crate) actor_name: String,
    pub(crate) summary: String,
    pub(crate) created_at: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct EventQuery {
    // Strictly-greater cursor: a poll answers everything after the caller's
    // last seen id, so re-polling never replays.
    pub(crate) since_id: i64,
    pub(crate) repo_path: Option<String>,
    pub(crate) limit: usize,
    pub(crate) ascending: bool,
}

// Composed with QueryBuilder: sqlx refuses runtime-built strings, and the
// pushed fragments carry no literal `?` marks (push_bind emits its own).
pub(crate) async fn list_events_in_pool(
    pool: &SqlitePool,
    query: &EventQuery,
) -> Result<Vec<EventRow>, sqlx::Error> {
    let mut builder = sqlx::QueryBuilder::new(
        "SELECT id, repo_path, kind, base_sha, target_key, target_kind, request_id, \
                comment_id, actor_kind, actor_name, summary, created_at \
         FROM events WHERE id > ",
    );
    builder.push_bind(query.since_id);
    if let Some(repo_path) = &query.repo_path {
        builder.push(" AND repo_path = ").push_bind(repo_path);
    }
    builder
        .push(" ORDER BY id ")
        .push(if query.ascending { "ASC" } else { "DESC" })
        .push(" LIMIT ")
        .push_bind(query.limit.min(EVENT_LIST_LIMIT) as i64);
    builder
        .build()
        .fetch_all(pool)
        .await?
        .iter()
        .map(event_from_row)
        .collect()
}

fn event_from_row(row: &sqlx::sqlite::SqliteRow) -> sqlx::Result<EventRow> {
    Ok(EventRow {
        id: row.try_get("id")?,
        repo_path: row.try_get("repo_path")?,
        kind: row.try_get("kind")?,
        base_sha: row.try_get("base_sha")?,
        target_key: row.try_get("target_key")?,
        target_kind: row.try_get("target_kind")?,
        request_id: row.try_get("request_id")?,
        comment_id: row.try_get("comment_id")?,
        actor_kind: row.try_get("actor_kind")?,
        actor_name: row.try_get("actor_name")?,
        summary: row.try_get("summary")?,
        created_at: row.try_get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_pool;

    fn event(repo_path: &str, kind: &'static str, summary: &str) -> NewEvent {
        NewEvent {
            repo_path: repo_path.into(),
            kind,
            base_sha: Some("base".into()),
            target_key: Some("/demo".into()),
            target_kind: Some("worktree".into()),
            request_id: None,
            comment_id: None,
            actor_kind: ACTOR_HUMAN,
            actor_name: "human".into(),
            summary: summary.into(),
        }
    }

    async fn count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM events")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn record_writes_every_column_and_list_round_trips_them() {
        let pool = test_pool().await;
        crate::testutil::seed_repo(&pool, "/demo").await;
        // The request link is a real foreign key, so the round trip seeds a
        // request row and points the event at it.
        sqlx::query(
            "INSERT INTO review_requests (repo_path, base_sha, target_key, target_kind, \
                    status, note, lenses, reviewers, max_rounds, round, created_at, updated_at) \
             VALUES ('/demo', 'base', '/demo', 'worktree', 'requested', '', '[]', '[]', 2, 0, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let request_id: i64 =
            sqlx::query_scalar("SELECT id FROM review_requests").fetch_one(&pool).await.unwrap();
        let id = record_event(
            &pool,
            NewEvent {
                request_id: Some(request_id),
                comment_id: None,
                actor_kind: ACTOR_AGENT,
                actor_name: "codex".into(),
                summary: "approved by codex".into(),
                ..event("/demo", REQUEST_VERDICT, "unused summary")
            },
        )
        .await
        .unwrap();
        assert_eq!(id, 1);
        // Every kind in the closed vocabulary inserts cleanly.
        for kind in [
            REQUEST_CREATED,
            REQUEST_CLAIMED,
            REQUEST_VERDICT,
            REQUEST_RE_REQUESTED,
            REQUEST_WITHDRAWN,
            SUBMISSION_DELIVERED,
            COMMENT_POSTED,
            COMMENT_REPLIED,
            COMMENT_RESOLVED,
            COMMENT_REOPENED,
            SURFACE_HEAD_MOVED,
            REPO_ADDED,
        ] {
            record_event(&pool, event("/demo", kind, "s")).await.unwrap();
        }
        assert_eq!(count(&pool).await, 13);

        let rows = list_events_in_pool(
            &pool,
            &EventQuery { since_id: 0, repo_path: None, limit: 100, ascending: true },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 13);
        let first = &rows[0];
        assert_eq!(first.id, 1);
        assert_eq!(first.repo_path, "/demo");
        assert_eq!(first.kind, REQUEST_VERDICT);
        assert_eq!(first.base_sha.as_deref(), Some("base"));
        assert_eq!(first.target_key.as_deref(), Some("/demo"));
        assert_eq!(first.target_kind.as_deref(), Some("worktree"));
        assert_eq!(first.request_id, Some(request_id));
        assert_eq!(first.comment_id, None);
        assert_eq!(first.actor_kind, "agent");
        assert_eq!(first.actor_name, "codex");
        assert_eq!(first.summary, "approved by codex");
        assert!(first.created_at > 0);
        // Ascending order means strictly growing ids.
        assert!(rows.windows(2).all(|pair| pair[0].id < pair[1].id));
    }

    #[tokio::test]
    async fn listing_respects_since_repo_order_and_the_cap() {
        let pool = test_pool().await;
        for index in 0..6 {
            record_event(
                &pool,
                event(if index % 2 == 0 { "/a" } else { "/b" }, COMMENT_POSTED, "s"),
            )
            .await
            .unwrap();
        }

        // The cursor is strictly-greater: everything after id 4, never 4 itself.
        let since = list_events_in_pool(
            &pool,
            &EventQuery { since_id: 4, repo_path: None, limit: 100, ascending: true },
        )
        .await
        .unwrap();
        assert_eq!(since.iter().map(|row| row.id).collect::<Vec<_>>(), [5, 6]);

        // Repo filter is exact.
        let repo_a = list_events_in_pool(
            &pool,
            &EventQuery { since_id: 0, repo_path: Some("/a".into()), limit: 100, ascending: true },
        )
        .await
        .unwrap();
        assert_eq!(repo_a.iter().map(|row| row.id).collect::<Vec<_>>(), [1, 3, 5]);

        // Descending order serves the feed page: newest first.
        let newest_first = list_events_in_pool(
            &pool,
            &EventQuery { since_id: 0, repo_path: None, limit: 100, ascending: false },
        )
        .await
        .unwrap();
        let ids: Vec<i64> = newest_first.iter().map(|row| row.id).collect();
        assert_eq!(ids, [6, 5, 4, 3, 2, 1]);

        // The limit clamps at the named cap instead of erroring.
        let bounded = list_events_in_pool(
            &pool,
            &EventQuery { since_id: 0, repo_path: None, limit: 4, ascending: true },
        )
        .await
        .unwrap();
        assert_eq!(bounded.len(), 4);
        let clamped = list_events_in_pool(
            &pool,
            &EventQuery {
                since_id: 0,
                repo_path: None,
                limit: EVENT_LIST_LIMIT + 25,
                ascending: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(clamped.len(), 6);
    }
}
