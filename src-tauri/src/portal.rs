// The Pulse portal's Reviews tab projection: every review identity that
// carries a review request or any comment/submission activity, one row per
// identity, including settled and request-less ones. Store-only like the
// attention queue (no Git on the path): state classification, search
// matching, ordering, and bounds are backend rules the webview renders
// verbatim.

use crate::requests::{identity_change_label, surface_head_moved, FindingCounts, APPROVED, WITHDRAWN};
use crate::retrospection::{recorded_worktree_head, SurfaceHead};
use crate::CommandError;
use serde::Serialize;
use sqlx::{Row, SqlitePool};

// The portal review states; the strings are the filter ids the webview
// sends back verbatim, and the backend is their single owner.
pub(crate) const STATE_OPEN: &str = "open";
const STATE_SETTLED: &str = "settled";
const STATE_STALE: &str = "stale";
const STATE_NO_REQUEST: &str = "no_request";
const STATE_ALL: &str = "all";

// The listing stays bounded: only the newest 500 identities by last
// activity are listed, no matter how much stored activity accumulates.
const ROW_LIMIT: usize = 500;

// One aggregate row per stored review identity: activity counts, the
// identity's unresolved severity counts, and the newest comment and
// submission timestamps. Inclusion (a request, a comment, or a submission
// anywhere on the identity) is applied after assembly.
const IDENTITY_ACTIVITY_QUERY: &str = "SELECT rv.id AS review_id, rv.repo_path, rv.base_sha, \
     rv.target_key, rv.target_kind, rv.created_at AS review_created_at, \
     COUNT(c.id) AS comment_count, \
     SUM(CASE WHEN c.severity = 'P0' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p0, \
     SUM(CASE WHEN c.severity = 'P1' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p1, \
     SUM(CASE WHEN c.severity = 'P2' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p2, \
     SUM(CASE WHEN c.severity = 'P3' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p3, \
     MAX(c.created_at) AS last_comment_at, \
     (SELECT COUNT(*) FROM submissions AS s WHERE s.review_id = rv.id) AS submission_count, \
     (SELECT MAX(s.created_at) FROM submissions AS s WHERE s.review_id = rv.id) AS last_submission_at, \
     (SELECT COUNT(*) FROM review_requests AS rq WHERE rq.repo_path = rv.repo_path \
        AND rq.base_sha = rv.base_sha AND rq.target_key = rv.target_key \
        AND rq.target_kind = rv.target_kind) AS request_count \
     FROM reviews AS rv \
     LEFT JOIN comments AS c ON c.review_id = rv.id \
     GROUP BY rv.id";

// Every stored request newest-first, so the identity's latest request is
// simply the first row seen per identity: the ordering is the freshness
// rule (by updated_at, then id).
const LATEST_REQUEST_QUERY: &str = "SELECT rq.repo_path, rq.base_sha, rq.target_key, \
     rq.target_kind, rq.status, rq.note, rq.round, rq.max_rounds, rq.head_sha, rq.updated_at, \
     t.name AS requester_name \
     FROM review_requests AS rq \
     LEFT JOIN agent_tokens AS t ON t.id = rq.requester_token_id \
     ORDER BY rq.updated_at DESC, rq.id DESC";

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct PortalReviewRow {
    pub(crate) repo_path: String,
    pub(crate) base_sha: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
    pub(crate) change_label: String,
    pub(crate) head_sha: Option<String>,
    pub(crate) requester: String,
    pub(crate) status: String,
    pub(crate) note: String,
    pub(crate) round: i64,
    pub(crate) max_rounds: i64,
    pub(crate) unresolved_finding_counts: FindingCounts,
    pub(crate) comment_count: i64,
    pub(crate) submission_count: i64,
    pub(crate) last_activity_at: i64,
    pub(crate) state: String,
    pub(crate) age_basis: i64,
}

// The latest request's rendered fields; a request-less identity carries
// none and renders empty strings and zeros like the attention rows do.
struct LatestRequest {
    status: String,
    note: String,
    round: i64,
    max_rounds: i64,
    head_sha: Option<String>,
    requester: String,
    updated_at: i64,
}

// The listing's optional filters; `state` is one of the STATE_* ids or
// "all" (the default), matching the webview's chips verbatim.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PortalReviewQuery {
    pub(crate) repo_path: Option<String>,
    pub(crate) state: Option<String>,
    pub(crate) search: Option<String>,
}

fn invalid_filter(message: impl Into<String>) -> CommandError {
    CommandError::new("invalid_request", message)
}

// First match wins, evaluated against the identity's latest request: no
// request at all is request-less, then the attention queue's
// changed-since-review condition makes the row stale even when the latest
// request settled, then settled statuses, and everything else is open.
fn classify_state(latest: Option<&LatestRequest>, recorded: Option<&SurfaceHead>) -> &'static str {
    let Some(latest) = latest else {
        return STATE_NO_REQUEST;
    };
    if surface_head_moved(recorded, latest.head_sha.as_deref()) {
        return STATE_STALE;
    }
    if latest.status == APPROVED || latest.status == WITHDRAWN {
        return STATE_SETTLED;
    }
    STATE_OPEN
}

// The search needle matches as a plain ASCII-case-insensitive substring
// over the change label, shas, requester, and note: `%` and `_` in the
// needle match literally, which is the semantics escaped-wildcard LIKE
// would produce, so no needle escaping is needed on this path.
fn matches_search(row: &PortalReviewRow, needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase();
    [row.change_label.as_str(), row.base_sha.as_str(), row.head_sha.as_deref().unwrap_or(""), row.requester.as_str(), row.note.as_str()]
        .iter()
        .any(|field| field.to_ascii_lowercase().contains(&needle))
}

// One identity's stored activity, gathered from the reviews-driven
// aggregate or synthesized as all-zero for an identity that only ever
// carried requests: a reviews row only materializes with the first
// comment or submission, so request-only identities have no row to join.
struct IdentityActivity {
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    unresolved: FindingCounts,
    comment_count: i64,
    submission_count: i64,
    last_comment_at: Option<i64>,
    last_submission_at: Option<i64>,
    review_created_at: Option<i64>,
}

impl IdentityActivity {
    fn has_activity(&self) -> bool {
        self.comment_count > 0 || self.submission_count > 0
    }

    // Max of the latest request's updated_at, the newest comment and
    // submission, else the review row's creation.
    fn last_activity_at(&self, latest: Option<&LatestRequest>) -> i64 {
        [
            latest.map(|request| request.updated_at),
            self.last_comment_at,
            self.last_submission_at,
            self.review_created_at,
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or_default()
    }
}

// The Reviews tab's identity listing, optionally narrowed by exact repo
// path, state, and search needle; newest activity first, capped.
pub(crate) async fn list_portal_reviews_in_pool(
    pool: &SqlitePool,
    query: &PortalReviewQuery,
) -> Result<Vec<PortalReviewRow>, CommandError> {
    if let Some(state) = &query.state {
        if state != STATE_ALL
            && state != STATE_OPEN
            && state != STATE_SETTLED
            && state != STATE_STALE
            && state != STATE_NO_REQUEST
        {
            return Err(invalid_filter(format!(
                "Unknown state filter '{state}'; states are {STATE_OPEN}, {STATE_SETTLED}, {STATE_STALE}, {STATE_NO_REQUEST}, or {STATE_ALL}."
            )));
        }
    }

    let mut latest_by_identity: std::collections::HashMap<(String, String, String, String), LatestRequest> =
        std::collections::HashMap::new();
    for row in sqlx::query(LATEST_REQUEST_QUERY).fetch_all(pool).await? {
        let requester: Option<String> = row.try_get("requester_name")?;
        latest_by_identity
            .entry((
                row.try_get("repo_path")?,
                row.try_get("base_sha")?,
                row.try_get("target_key")?,
                row.try_get("target_kind")?,
            ))
            .or_insert(LatestRequest {
                status: row.try_get("status")?,
                note: row.try_get("note")?,
                round: row.try_get("round")?,
                max_rounds: row.try_get("max_rounds")?,
                head_sha: row.try_get("head_sha")?,
                requester: requester.unwrap_or_else(|| "human".to_string()),
                updated_at: row.try_get("updated_at")?,
            });
    }
    let heads = crate::retrospection::recorded_surface_heads_in_pool(pool).await?;

    let mut activities: Vec<IdentityActivity> = Vec::new();
    for row in sqlx::query(IDENTITY_ACTIVITY_QUERY).fetch_all(pool).await? {
        activities.push(IdentityActivity {
            repo_path: row.try_get("repo_path")?,
            base_sha: row.try_get("base_sha")?,
            target_key: row.try_get("target_key")?,
            target_kind: row.try_get("target_kind")?,
            unresolved: FindingCounts {
                p0: row.try_get("unresolved_p0")?,
                p1: row.try_get("unresolved_p1")?,
                p2: row.try_get("unresolved_p2")?,
                p3: row.try_get("unresolved_p3")?,
            },
            comment_count: row.try_get("comment_count")?,
            submission_count: row.try_get("submission_count")?,
            last_comment_at: row.try_get("last_comment_at")?,
            last_submission_at: row.try_get("last_submission_at")?,
            review_created_at: Some(row.try_get("review_created_at")?),
        });
    }
    // Identities known only through their requests join the listing with
    // zero activity; nothing else re-derives from the requests pass.
    let seen: std::collections::HashSet<_> = activities
        .iter()
        .map(|activity| {
            (
                activity.repo_path.clone(),
                activity.base_sha.clone(),
                activity.target_key.clone(),
                activity.target_kind.clone(),
            )
        })
        .collect();
    for (identity, _) in latest_by_identity.iter() {
        if seen.contains(identity) {
            continue;
        }
        activities.push(IdentityActivity {
            repo_path: identity.0.clone(),
            base_sha: identity.1.clone(),
            target_key: identity.2.clone(),
            target_kind: identity.3.clone(),
            unresolved: FindingCounts { p0: 0, p1: 0, p2: 0, p3: 0 },
            comment_count: 0,
            submission_count: 0,
            last_comment_at: None,
            last_submission_at: None,
            review_created_at: None,
        });
    }

    let mut rows = Vec::new();
    for activity in activities {
        let identity = (
            activity.repo_path.clone(),
            activity.base_sha.clone(),
            activity.target_key.clone(),
            activity.target_kind.clone(),
        );
        let latest = latest_by_identity.get(&identity);
        if let Some(repo) = &query.repo_path {
            if repo != &activity.repo_path {
                continue;
            }
        }
        // Inclusion: a request, a comment, or a submission anywhere on the
        // identity; a silent reviews row stays unlisted.
        if !activity.has_activity() && latest.is_none() {
            continue;
        }
        // Only worktree identities carry a recorded surface head; head-kind
        // identities compare through their own sha below.
        let recorded = if activity.target_kind == "worktree" {
            recorded_worktree_head(&heads, &activity.repo_path, &activity.target_key)
        } else {
            None
        };
        let last_activity_at = activity.last_activity_at(latest);
        let change_label = identity_change_label(&activity.target_kind, &activity.target_key, recorded);
        // The recorded head when the surface was retrospected, else the
        // identity itself for head-kind rows; identical to the attention
        // rows' open path.
        let head_sha = recorded
            .map(|head| head.head_sha.clone())
            .or_else(|| (activity.target_kind == "head").then(|| activity.target_key.clone()));
        let row = PortalReviewRow {
            repo_path: activity.repo_path,
            base_sha: activity.base_sha,
            target_key: activity.target_key,
            target_kind: activity.target_kind,
            change_label,
            head_sha,
            requester: latest.map_or(String::new(), |request| request.requester.clone()),
            status: latest.map_or(String::new(), |request| request.status.clone()),
            note: latest.map_or(String::new(), |request| request.note.clone()),
            round: latest.map_or(0, |request| request.round),
            max_rounds: latest.map_or(0, |request| request.max_rounds),
            unresolved_finding_counts: activity.unresolved,
            comment_count: activity.comment_count,
            submission_count: activity.submission_count,
            last_activity_at,
            // An identity-level row ages from its most recent activity,
            // which is also the payload's ordering key.
            state: classify_state(latest, recorded).to_string(),
            age_basis: last_activity_at,
        };
        if let Some(state) = &query.state {
            if state != STATE_ALL && &row.state != state {
                continue;
            }
        }
        if let Some(search) = &query.search {
            let needle = search.trim();
            if !needle.is_empty() && !matches_search(&row, needle) {
                continue;
            }
        }
        rows.push(row);
    }

    // Newest activity first; the identity tie-break keeps a refreshed
    // payload from reshuffling equally fresh rows.
    rows.sort_by(|left, right| {
        right
            .last_activity_at
            .cmp(&left.last_activity_at)
            .then_with(|| left.repo_path.cmp(&right.repo_path))
            .then_with(|| left.base_sha.cmp(&right.base_sha))
            .then_with(|| left.target_key.cmp(&right.target_key))
            .then_with(|| left.target_kind.cmp(&right.target_kind))
    });
    rows.truncate(ROW_LIMIT);
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requests::{
        claim_request_in_pool, create_request_in_pool, set_request_verdict_in_pool,
        withdraw_request_in_pool, Actor, RequestDraft, ReviewRequest, REQUESTED,
    };
    use crate::testutil::{seed_repo, test_pool};
    use crate::transport::RequestChangeSink;
    use std::sync::Arc;

    const REPO: &str = "/demo";

    fn noop_notify() -> RequestChangeSink {
        Arc::new(|_| {})
    }

    async fn agent_token(pool: &SqlitePool, name: &str) -> i64 {
        crate::agents::create_agent_token_in_pool(pool, name)
            .await
            .unwrap()
            .token
            .id
    }

    fn draft(head: &str) -> RequestDraft {
        RequestDraft {
            note: Some("Please review my changes.".into()),
            lenses: Vec::new(),
            reviewers: Vec::new(),
            max_rounds: None,
            head_sha: head.into(),
        }
    }

    async fn create(
        pool: &SqlitePool,
        repo: &str,
        base: &str,
        key: &str,
        head: &str,
        actor: &Actor,
    ) -> ReviewRequest {
        create_request_in_pool(pool, repo, base, key, "worktree", &draft(head), actor, &noop_notify())
            .await
            .unwrap()
    }

    async fn review_id(pool: &SqlitePool, repo: &str, base: &str, key: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT id FROM reviews WHERE repo_path = ? AND base_sha = ? \
             AND target_key = ? AND target_kind = 'worktree'",
        )
        .bind(repo)
        .bind(base)
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    // One unresolved (or resolved) finding on the identity, stored like the
    // ingest path would store it.
    async fn seed_finding(pool: &SqlitePool, repo: &str, base: &str, key: &str, severity: &str, created_at: i64) {
        sqlx::query(
            "INSERT OR IGNORE INTO reviews (repo_path, base_sha, target_key, target_kind, created_at) \
             VALUES (?, ?, ?, 'worktree', 1)",
        )
        .bind(repo)
        .bind(base)
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO comments (review_id, parent_id, author_kind, author_name, body, \
                    severity, resolved_at, created_at) \
             VALUES (?, NULL, 'agent', 'reviewer-bot', 'finding', ?, NULL, ?)",
        )
        .bind(review_id(pool, repo, base, key).await)
        .bind(severity)
        .bind(created_at)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_submission(pool: &SqlitePool, repo: &str, base: &str, key: &str, created_at: i64) {
        sqlx::query(
            "INSERT OR IGNORE INTO reviews (repo_path, base_sha, target_key, target_kind, created_at) \
             VALUES (?, ?, ?, 'worktree', 1)",
        )
        .bind(repo)
        .bind(base)
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO submissions (review_id, agent_name, agent_model, sections_json, created_at) \
             VALUES (?, 'coder-bot', 'test-model', '[]', ?)",
        )
        .bind(review_id(pool, repo, base, key).await)
        .bind(created_at)
        .execute(pool)
        .await
        .unwrap();
    }

    // Overrides a request's timestamps so freshness and ties are exact.
    async fn set_updated_at(pool: &SqlitePool, id: i64, updated_at: i64) {
        sqlx::query("UPDATE review_requests SET updated_at = ? WHERE id = ?")
            .bind(updated_at)
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn list(pool: &SqlitePool, query: &PortalReviewQuery) -> Vec<PortalReviewRow> {
        list_portal_reviews_in_pool(pool, query).await.unwrap()
    }

    fn row_of<'a>(rows: &'a [PortalReviewRow], base: &str) -> &'a PortalReviewRow {
        rows.iter()
            .find(|row| row.base_sha == base)
            .unwrap_or_else(|| panic!("no portal row for {base}"))
    }

    #[tokio::test]
    async fn listing_includes_settled_and_requestless_identities() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let reviewer = agent_token(&pool, "reviewer-bot").await;

        // Approved: settled, still listed.
        let approved = create(&pool, REPO, "base-approved", "/wt-a", "head-a", &Actor::Agent(coder)).await;
        claim_request_in_pool(&pool, approved.id, &Actor::Agent(reviewer), &noop_notify())
            .await
            .unwrap();
        set_request_verdict_in_pool(&pool, approved.id, true, &Actor::Human, &noop_notify())
            .await
            .unwrap();
        set_updated_at(&pool, approved.id, 1000).await;

        // Withdrawn: settled, still listed.
        let withdrawn = create(&pool, REPO, "base-withdrawn", "/wt-b", "head-b", &Actor::Agent(coder)).await;
        withdraw_request_in_pool(&pool, withdrawn.id, &Actor::Agent(coder), &noop_notify())
            .await
            .unwrap();
        set_updated_at(&pool, withdrawn.id, 2000).await;

        // Comments with no request ever: request-less, listed.
        seed_finding(&pool, REPO, "base-comments", "/wt-c", "P1", 3000).await;

        // A submission with no request and no comments: request-less,
        // listed, and the submission rides last_activity.
        seed_submission(&pool, REPO, "base-submission", "/wt-d", 4000).await;

        // A silent reviews row: no request, comment, or submission, so it
        // stays unlisted.
        sqlx::query(
            "INSERT INTO reviews (repo_path, base_sha, target_key, target_kind, created_at) \
             VALUES (?, 'base-silent', '/wt-e', 'worktree', 1)",
        )
        .bind(REPO)
        .execute(&pool)
        .await
        .unwrap();

        let rows = list(&pool, &PortalReviewQuery::default()).await;
        assert_eq!(rows.len(), 4, "the silent identity is excluded");

        let settled = row_of(&rows, "base-approved");
        assert_eq!(settled.state, STATE_SETTLED);
        assert_eq!(settled.status, APPROVED);
        assert_eq!(settled.requester, "coder-bot");

        let withdrawn_row = row_of(&rows, "base-withdrawn");
        assert_eq!(withdrawn_row.state, STATE_SETTLED);
        assert_eq!(withdrawn_row.status, WITHDRAWN);

        let commentless = row_of(&rows, "base-comments");
        assert_eq!(commentless.state, STATE_NO_REQUEST);
        assert_eq!(commentless.status, "");
        assert_eq!(commentless.note, "");
        assert_eq!(commentless.round, 0);
        assert_eq!(commentless.max_rounds, 0);
        assert_eq!(commentless.requester, "");
        assert_eq!(commentless.comment_count, 1);
        assert_eq!(commentless.submission_count, 0);
        assert_eq!(commentless.unresolved_finding_counts.p1, 1);
        assert_eq!(commentless.last_activity_at, 3000);

        let submission = row_of(&rows, "base-submission");
        assert_eq!(submission.state, STATE_NO_REQUEST);
        assert_eq!(submission.comment_count, 0);
        assert_eq!(submission.submission_count, 1);
        assert_eq!(submission.last_activity_at, 4000);

        // Newest activity first across the mixed identities.
        assert_eq!(rows[0].base_sha, "base-submission");
        assert_eq!(rows[3].base_sha, "base-approved");
    }

    #[tokio::test]
    async fn states_follow_classification_precedence() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let reviewer = agent_token(&pool, "reviewer-bot").await;

        // An open lifecycle status with no surface movement: open.
        create(&pool, REPO, "base-open", "/wt-open", "head-open", &Actor::Agent(coder)).await;

        // An in-review request whose recorded surface head moved: stale,
        // the changed-since-review condition, not open.
        let moved = create(&pool, REPO, "base-stale-open", "/wt-stale-open", "head-so", &Actor::Agent(coder)).await;
        claim_request_in_pool(&pool, moved.id, &Actor::Agent(reviewer), &noop_notify())
            .await
            .unwrap();
        crate::retrospection::record_surface_open(
            &pool, REPO, "worktree", "/wt-stale-open", "feature-stale", "/wt-stale-open", "moved-head",
        )
        .await;

        // An approved request whose surface head moved afterwards: stale
        // wins over settled.
        let approved_moved = create(&pool, REPO, "base-stale-settled", "/wt-ss", "head-ss", &Actor::Agent(coder)).await;
        claim_request_in_pool(&pool, approved_moved.id, &Actor::Agent(reviewer), &noop_notify())
            .await
            .unwrap();
        set_request_verdict_in_pool(&pool, approved_moved.id, true, &Actor::Human, &noop_notify())
            .await
            .unwrap();
        crate::retrospection::record_surface_open(
            &pool, REPO, "worktree", "/wt-ss", "feature-ss", "/wt-ss", "moved-ss",
        )
        .await;

        // An approved request whose surface head still matches: settled.
        let approved = create(&pool, REPO, "base-approved", "/wt-ap", "head-ap", &Actor::Agent(coder)).await;
        claim_request_in_pool(&pool, approved.id, &Actor::Agent(reviewer), &noop_notify())
            .await
            .unwrap();
        set_request_verdict_in_pool(&pool, approved.id, true, &Actor::Human, &noop_notify())
            .await
            .unwrap();
        crate::retrospection::record_surface_open(
            &pool, REPO, "worktree", "/wt-ap", "feature-ap", "/wt-ap", "head-ap",
        )
        .await;

        // Comments-only activity on a moved surface: no request, so it is
        // request-less, never stale.
        seed_finding(&pool, REPO, "base-no-request", "/wt-nr", "P2", 1).await;
        crate::retrospection::record_surface_open(
            &pool, REPO, "worktree", "/wt-nr", "feature-nr", "/wt-nr", "moved-nr",
        )
        .await;

        let rows = list(&pool, &PortalReviewQuery::default()).await;
        assert_eq!(row_of(&rows, "base-open").state, STATE_OPEN);
        assert_eq!(row_of(&rows, "base-stale-open").state, STATE_STALE);
        assert_eq!(row_of(&rows, "base-stale-settled").state, STATE_STALE);
        assert_eq!(row_of(&rows, "base-approved").state, STATE_SETTLED);
        assert_eq!(row_of(&rows, "base-no-request").state, STATE_NO_REQUEST);
        // The stale rows carry the recorded (moved) head so opening them
        // lands on the surface as last seen.
        assert_eq!(row_of(&rows, "base-stale-open").head_sha.as_deref(), Some("moved-head"));
        assert_eq!(row_of(&rows, "base-stale-open").change_label, "feature-stale");
    }

    #[tokio::test]
    async fn the_latest_request_is_newest_by_updated_at_then_id() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let other = agent_token(&pool, "other-bot").await;

        // Two requests on one identity (different requesters never dedup);
        // the older id carries the newer updated_at, so it is the latest.
        let older = create(&pool, REPO, "base-latest", "/wt-l", "head-1", &Actor::Agent(coder)).await;
        let newer = create(&pool, REPO, "base-latest", "/wt-l", "head-2", &Actor::Agent(other)).await;
        set_updated_at(&pool, older.id, 5000).await;
        set_updated_at(&pool, newer.id, 1000).await;

        let rows = list(&pool, &PortalReviewQuery::default()).await;
        let row = row_of(&rows, "base-latest");
        assert_eq!(row.status, REQUESTED);
        assert_eq!(row.requester, "coder-bot", "the older id wins on the newer updated_at");

        // Tie on updated_at: the higher id is the latest.
        set_updated_at(&pool, older.id, 5000).await;
        set_updated_at(&pool, newer.id, 5000).await;
        let rows = list(&pool, &PortalReviewQuery::default()).await;
        assert_eq!(row_of(&rows, "base-latest").requester, "other-bot");
    }

    #[tokio::test]
    async fn search_matches_labels_shas_requesters_and_notes_literally() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        create(&pool, REPO, "abc1234fffffffffffffffffffffffffffffffff", "/wt-feat", "head-feat", &Actor::Agent(coder)).await;
        // The second identity's request carries a distinctive note.
        let mut zebras = draft("head-note");
        zebras.note = Some("holds a zebra fixture".into());
        create_request_in_pool(&pool, REPO, "base-note", "/wt-note", "worktree", &zebras, &Actor::Agent(coder), &noop_notify())
            .await
            .unwrap();

        let all = list(&pool, &PortalReviewQuery::default()).await;
        assert_eq!(all.len(), 2);

        // Case-insensitive over the recorded change label.
        let by_label = list(&pool, &PortalReviewQuery { search: Some("FEAT".into()), ..Default::default() }).await;
        assert_eq!(by_label.len(), 1);
        assert_eq!(by_label[0].base_sha, "abc1234fffffffffffffffffffffffffffffffff");

        // Over the base sha and the requester.
        assert_eq!(list(&pool, &PortalReviewQuery { search: Some("ABC1234".into()), ..Default::default() }).await.len(), 1);
        assert_eq!(list(&pool, &PortalReviewQuery { search: Some("coder-bot".into()), ..Default::default() }).await.len(), 2);

        // Over the note.
        assert_eq!(list(&pool, &PortalReviewQuery { search: Some("please review".into()), ..Default::default() }).await.len(), 1);
        assert_eq!(list(&pool, &PortalReviewQuery { search: Some("zebra".into()), ..Default::default() }).await.len(), 1);

        // Wildcard characters match literally: `o_e` does not open a LIKE
        // wildcard over "note"-bearing fields, and a percent matches only a
        // literal percent.
        assert_eq!(list(&pool, &PortalReviewQuery { search: Some("o_e".into()), ..Default::default() }).await.len(), 0);
        assert_eq!(list(&pool, &PortalReviewQuery { search: Some("%".into()), ..Default::default() }).await.len(), 0);

        // A blank needle keeps everything.
        assert_eq!(list(&pool, &PortalReviewQuery { search: Some("  ".into()), ..Default::default() }).await.len(), 2);
    }

    #[tokio::test]
    async fn filters_narrow_by_repo_and_state_and_refuse_unknown_states() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        seed_repo(&pool, "/other").await;
        let coder = agent_token(&pool, "coder-bot").await;
        create(&pool, REPO, "base-open", "/wt-open", "head-open", &Actor::Agent(coder)).await;
        let withdrawn = create(&pool, "/other", "base-withdrawn", "/wt-w", "head-w", &Actor::Agent(coder)).await;
        withdraw_request_in_pool(&pool, withdrawn.id, &Actor::Agent(coder), &noop_notify())
            .await
            .unwrap();

        let by_repo = list(&pool, &PortalReviewQuery { repo_path: Some("/other".into()), ..Default::default() }).await;
        assert_eq!(by_repo.len(), 1);
        assert_eq!(by_repo[0].repo_path, "/other");

        let settled = list(&pool, &PortalReviewQuery { state: Some(STATE_SETTLED.into()), ..Default::default() }).await;
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].base_sha, "base-withdrawn");

        let open = list(&pool, &PortalReviewQuery { state: Some(STATE_OPEN.into()), ..Default::default() }).await;
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].base_sha, "base-open");

        let error = list_portal_reviews_in_pool(
            &pool,
            &PortalReviewQuery { state: Some("closed".into()), ..Default::default() },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_request");
    }

    #[tokio::test]
    async fn the_listing_is_bounded_by_the_row_cap() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        // ROW_LIMIT + 5 comment-only identities with staggered activity;
        // the five oldest drop.
        for index in 0..(ROW_LIMIT + 5) {
            let base = format!("base-{index:04}");
            seed_finding(&pool, REPO, &base, &format!("/wt-{index:04}"), "P3", 1_000 + index as i64).await;
        }
        let rows = list(&pool, &PortalReviewQuery::default()).await;
        assert_eq!(rows.len(), ROW_LIMIT);
        assert_eq!(rows[0].base_sha, format!("base-{:04}", ROW_LIMIT + 4), "newest activity first");
        assert_eq!(rows[ROW_LIMIT - 1].base_sha, format!("base-{:04}", 5), "the five oldest identities drop");
    }
}
