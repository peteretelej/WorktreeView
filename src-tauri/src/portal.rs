// The Pulse portal's store projections: the Reviews tab's identity
// listing, the Threads tab's grouped conversations, and the palette's
// cross-store search. Store-only like the attention queue (no Git on the
// path): inclusion, state classification, search matching, ordering, and
// bounds are backend rules the webview renders verbatim.

use crate::events::{ACTOR_AGENT, ACTOR_HUMAN};
use crate::requests::{identity_change_label, surface_head_moved, FindingCounts, APPROVED, REQUESTED, WITHDRAWN};
use crate::retrospection::{recorded_worktree_head, SurfaceHead};
use crate::reviews::{list_comments_in_pool, Comment};
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

// The newest narrated event per identity, the Reviews tab's last-event
// text. Identity-scoped events only: repo additions carry no identity
// columns. The bare `summary` column rides SQLite's min/max rule, so it
// reads from the MAX(id) row.
const IDENTITY_LAST_EVENT_QUERY: &str = "SELECT repo_path, base_sha, target_key, target_kind, \
     summary, MAX(id) \
     FROM events \
     WHERE base_sha IS NOT NULL AND target_key IS NOT NULL AND target_kind IS NOT NULL \
     GROUP BY repo_path, base_sha, target_key, target_kind";

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct PortalReviewRow {
    pub(crate) repo_path: String,
    pub(crate) base_sha: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
    pub(crate) change_label: String,
    pub(crate) head_sha: Option<String>,
    pub(crate) requester: String,
    pub(crate) requester_kind: String,
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
    pub(crate) last_event: String,
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
    requester_kind: &'static str,
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

// The review identity every request-and-activity projection keys on.
type IdentityKey = (String, String, String, String);

// Every stored request newest-first with its identity, so consumers see
// each identity's latest request as the first row they meet: the ordering
// is the freshness rule (by updated_at, then id).
async fn latest_requests_newest_first(
    pool: &SqlitePool,
) -> Result<Vec<(IdentityKey, LatestRequest)>, CommandError> {
    let mut rows = Vec::new();
    for row in sqlx::query(LATEST_REQUEST_QUERY).fetch_all(pool).await? {
        // The token join misses for the human's own requests: a hit is an
        // agent token, a miss is the human.
        let (requester, requester_kind) = match row.try_get::<Option<String>, _>("requester_name")? {
            Some(name) => (name, ACTOR_AGENT),
            None => ("human".to_string(), ACTOR_HUMAN),
        };
        rows.push((
            (
                row.try_get("repo_path")?,
                row.try_get("base_sha")?,
                row.try_get("target_key")?,
                row.try_get("target_kind")?,
            ),
            LatestRequest {
                status: row.try_get("status")?,
                note: row.try_get("note")?,
                round: row.try_get("round")?,
                max_rounds: row.try_get("max_rounds")?,
                head_sha: row.try_get("head_sha")?,
                requester,
                requester_kind,
                updated_at: row.try_get("updated_at")?,
            },
        ));
    }
    Ok(rows)
}

// The newest narrated event per identity, keyed like every request-and-
// activity projection.
async fn last_events_by_identity(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<IdentityKey, String>, sqlx::Error> {
    let mut events: std::collections::HashMap<IdentityKey, String> =
        std::collections::HashMap::new();
    for row in sqlx::query(IDENTITY_LAST_EVENT_QUERY).fetch_all(pool).await? {
        events.insert(
            (
                row.try_get("repo_path")?,
                row.try_get("base_sha")?,
                row.try_get("target_key")?,
                row.try_get("target_kind")?,
            ),
            row.try_get("summary")?,
        );
    }
    Ok(events)
}

// The last-event text for identities whose activity predates the event
// log, composed from the row's own facts: the latest request's wording,
// then the activity counts. Every listed row carries a request or
// activity, so the parts never all drop out.
fn fallback_event(latest: Option<&LatestRequest>, comment_count: i64, submission_count: i64) -> String {
    let mut parts: Vec<String> = Vec::new();
    match latest {
        Some(request) if request.status == REQUESTED && !request.note.is_empty() => {
            parts.push(format!("requested with note: {}", request.note));
        }
        Some(request) => parts.push(request.status.replace('_', " ")),
        None => {}
    }
    if comment_count > 0 {
        parts.push(format!(
            "{comment_count} {}",
            if comment_count == 1 { "comment" } else { "comments" }
        ));
    }
    if submission_count > 0 {
        parts.push(format!(
            "{submission_count} {}",
            if submission_count == 1 { "submission" } else { "submissions" }
        ));
    }
    parts.join(" · ")
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

    let mut latest_by_identity: std::collections::HashMap<IdentityKey, LatestRequest> =
        std::collections::HashMap::new();
    for (identity, request) in latest_requests_newest_first(pool).await? {
        latest_by_identity.entry(identity).or_insert(request);
    }
    let heads = crate::retrospection::recorded_surface_heads_in_pool(pool).await?;
    let last_events = last_events_by_identity(pool).await?;

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
        let last_event = last_events.get(&identity).cloned().unwrap_or_else(|| {
            fallback_event(latest, activity.comment_count, activity.submission_count)
        });
        let row = PortalReviewRow {
            repo_path: activity.repo_path,
            base_sha: activity.base_sha,
            target_key: activity.target_key,
            target_kind: activity.target_kind,
            change_label,
            head_sha,
            requester: latest.map_or(String::new(), |request| request.requester.clone()),
            requester_kind: latest
                .map_or(ACTOR_HUMAN.to_string(), |request| request.requester_kind.to_string()),
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
            last_event,
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

// ===== Threads tab =====

// The thread states; "open" means the root comment is unresolved. The
// strings are the filter ids the webview and the list_threads tool send
// back verbatim, and the backend is their single owner.
pub(crate) const THREAD_STATE_OPEN: &str = "open";
pub(crate) const THREAD_STATE_RESOLVED: &str = "resolved";

// The voice filter reads a thread's participants: a thread answers the
// human or agents voice when any participant (root author or replier)
// carries that author_kind.
pub(crate) const THREAD_VOICE_ALL: &str = "all";
pub(crate) const THREAD_VOICE_HUMAN: &str = "human";
pub(crate) const THREAD_VOICE_AGENTS: &str = "agents";

// The threads listing stays bounded like the reviews listing: only the
// newest 500 threads by last activity are assembled, no matter how much
// stored conversation accumulates.
const THREAD_ROW_LIMIT: usize = 500;

// One pass over root comments joined with their review identity; reply
// aggregates ride as correlated subqueries, so grouping across bases under
// one change stays a single bounded query (no new index or migration).
const THREAD_ROOTS_QUERY: &str = "SELECT c.id AS root_id, c.body, c.author_kind, c.author_name, \
     c.file_path, c.start_line, c.severity, c.resolved_at, c.created_at, \
     rv.repo_path, rv.base_sha, rv.target_key, rv.target_kind, \
     (SELECT COUNT(*) FROM comments AS r WHERE r.parent_id = c.id) AS reply_count, \
     (SELECT MAX(r.created_at) FROM comments AS r WHERE r.parent_id = c.id) AS last_reply_at \
     FROM comments AS c \
     JOIN reviews AS rv ON rv.id = c.review_id \
     WHERE c.parent_id IS NULL";

// Thread rows sort by last activity (root created_at vs newest reply), the
// same key that bounds and orders the listing.
const THREAD_ROOTS_ORDER: &str = " ORDER BY MAX(c.created_at, COALESCE(\
     (SELECT MAX(r.created_at) FROM comments AS r WHERE r.parent_id = c.id), c.created_at)) DESC, \
     c.id DESC";

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct PortalParticipant {
    pub(crate) author_kind: String,
    pub(crate) author_name: String,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct PortalThreadRow {
    pub(crate) root_comment_id: i64,
    pub(crate) repo_path: String,
    pub(crate) base_sha: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
    pub(crate) head_sha: Option<String>,
    pub(crate) excerpt: String,
    pub(crate) severity: Option<String>,
    pub(crate) anchor: Option<String>,
    pub(crate) participants: Vec<PortalParticipant>,
    pub(crate) reply_count: i64,
    pub(crate) resolved_at: Option<i64>,
    pub(crate) last_activity_at: i64,
    pub(crate) head_moved: bool,
}

// One change's thread group: every thread whose review identity shares the
// (repo_path, target_key, target_kind) key, so threads survive base
// changes; the display label never participates in grouping.
#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct PortalThreadGroup {
    pub(crate) repo_path: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
    pub(crate) change_label: String,
    pub(crate) open_count: i64,
    pub(crate) threads: Vec<PortalThreadRow>,
}

// The threads listing's optional filters; state defaults to open and voice
// to all, matching the webview's chips verbatim.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PortalThreadQuery {
    pub(crate) repo_path: Option<String>,
    pub(crate) state: Option<String>,
    pub(crate) voice: Option<String>,
    pub(crate) text: Option<String>,
}

// The text needle becomes a bounded LIKE pattern: `%` and `_` in the needle
// match literally through the ESCAPE clause, and SQLite's LIKE is
// ASCII-case-insensitive by default.
fn like_pattern(needle: &str) -> String {
    format!(
        "%{}%",
        needle
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}

// Mono anchor label for a thread row: file:start for line anchors, the file
// alone for file-level anchors, and none for review-level threads.
fn anchor_label(file_path: Option<&str>, start_line: Option<i64>) -> Option<String> {
    match (file_path, start_line) {
        (Some(file), Some(line)) => Some(format!("{file}:{line}")),
        (Some(file), None) => Some(file.to_string()),
        (None, _) => None,
    }
}

// A thread's participant list: the root author first, then reply authors in
// reply order, deduplicated by (author_kind, author_name).
fn merge_participants(
    root: (&str, &str),
    reply_participants: &[PortalParticipant],
) -> Vec<PortalParticipant> {
    let mut participants = vec![PortalParticipant {
        author_kind: root.0.to_string(),
        author_name: root.1.to_string(),
    }];
    for participant in reply_participants {
        if !participants
            .iter()
            .any(|known| known.author_kind == participant.author_kind && known.author_name == participant.author_name)
        {
            participants.push(participant.clone());
        }
    }
    participants
}

// Reply participants for the assembled roots in one bounded IN-list query;
// the roots' own authors join in merge_participants. QueryBuilder carries
// the dynamic id list: every id rides as a bound parameter.
async fn reply_participants_for_roots(
    pool: &SqlitePool,
    root_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<PortalParticipant>>, CommandError> {
    let mut participants: std::collections::HashMap<i64, Vec<PortalParticipant>> =
        std::collections::HashMap::new();
    if root_ids.is_empty() {
        return Ok(participants);
    }
    let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT parent_id, author_kind, author_name FROM comments WHERE parent_id IN (",
    );
    for (index, id) in root_ids.iter().enumerate() {
        if index > 0 {
            builder.push(",");
        }
        builder.push_bind(id);
    }
    builder.push(") ORDER BY created_at, id");
    for row in builder.build().fetch_all(pool).await? {
        participants
            .entry(row.try_get("parent_id")?)
            .or_default()
            .push(PortalParticipant {
                author_kind: row.try_get("author_kind")?,
                author_name: row.try_get("author_name")?,
            });
    }
    Ok(participants)
}

// The threads tab's listing: every root comment grouped by change, newest
// activity first, optionally narrowed by exact repo, state, participant
// voice, and a LIKE needle over root and reply bodies. Open counts count
// each group's open threads regardless of the state filter so the chips
// keep their meaning, and the state filter only narrows the rows.
pub(crate) async fn list_portal_threads_in_pool(
    pool: &SqlitePool,
    query: &PortalThreadQuery,
) -> Result<Vec<PortalThreadGroup>, CommandError> {
    let state = query.state.as_deref().unwrap_or(THREAD_STATE_OPEN);
    if state != THREAD_STATE_OPEN && state != THREAD_STATE_RESOLVED && state != STATE_ALL {
        return Err(invalid_filter(format!(
            "Unknown state filter '{state}'; states are {THREAD_STATE_OPEN}, {THREAD_STATE_RESOLVED}, or {STATE_ALL}."
        )));
    }
    let voice = query.voice.as_deref().unwrap_or(THREAD_VOICE_ALL);
    if voice != THREAD_VOICE_ALL && voice != THREAD_VOICE_HUMAN && voice != THREAD_VOICE_AGENTS {
        return Err(invalid_filter(format!(
            "Unknown voice filter '{voice}'; voices are {THREAD_VOICE_HUMAN}, {THREAD_VOICE_AGENTS}, or {THREAD_VOICE_ALL}."
        )));
    }

    // The dynamic pieces come from a fixed vocabulary and every value rides
    // as a bound parameter, so QueryBuilder carries the composed query.
    let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(THREAD_ROOTS_QUERY);
    if let Some(repo) = &query.repo_path {
        builder.push(" AND rv.repo_path = ").push_bind(repo.clone());
    }
    // The voice id is plural ("agents"); the stored author_kind is singular.
    let voice_kind = match voice {
        THREAD_VOICE_HUMAN => Some("human"),
        THREAD_VOICE_AGENTS => Some("agent"),
        _ => None,
    };
    if let Some(kind) = voice_kind {
        builder.push(&format!(
            " AND (c.author_kind = '{kind}' OR EXISTS(SELECT 1 FROM comments AS r \
             WHERE r.parent_id = c.id AND r.author_kind = '{kind}'))"
        ));
    }
    let needle = query
        .text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    if let Some(text) = needle {
        // push_bind emits the placeholder itself: the LIKE patterns ride as
        // binds between the pushed SQL fragments, never inline `?` marks.
        builder
            .push(" AND (c.body LIKE ")
            .push_bind(like_pattern(text))
            .push(" ESCAPE '\\' OR EXISTS(SELECT 1 FROM comments AS r \
                   WHERE r.parent_id = c.id AND r.body LIKE ")
            .push_bind(like_pattern(text))
            .push(" ESCAPE '\\'))");
    }
    builder
        .push(THREAD_ROOTS_ORDER)
        .push(" LIMIT ")
        .push_bind(THREAD_ROW_LIMIT as i64);

    // Roots carry their author alongside the row so each participant list
    // can start with the root author.
    let mut roots: Vec<(PortalThreadRow, PortalParticipant)> = Vec::new();
    for row in builder.build().fetch_all(pool).await? {
        let created_at: i64 = row.try_get("created_at")?;
        let last_reply_at: Option<i64> = row.try_get("last_reply_at")?;
        let root_author = PortalParticipant {
            author_kind: row.try_get("author_kind")?,
            author_name: row.try_get("author_name")?,
        };
        let root = PortalThreadRow {
            root_comment_id: row.try_get("root_id")?,
            repo_path: row.try_get("repo_path")?,
            base_sha: row.try_get("base_sha")?,
            target_key: row.try_get("target_key")?,
            target_kind: row.try_get("target_kind")?,
            head_sha: None,
            excerpt: row
                .try_get::<String, _>("body")?
                .lines()
                .next()
                .unwrap_or_default()
                .trim_end()
                .to_string(),
            severity: row.try_get("severity")?,
            anchor: anchor_label(
                row.try_get::<Option<String>, _>("file_path")?.as_deref(),
                row.try_get("start_line")?,
            ),
            participants: Vec::new(),
            reply_count: row.try_get("reply_count")?,
            resolved_at: row.try_get("resolved_at")?,
            last_activity_at: last_reply_at.max(Some(created_at)).unwrap_or(created_at),
            head_moved: false,
        };
        roots.push((root, root_author));
    }

    // The moved-badge basis is the same store-only comparison the attention
    // queue's changed-since-review rule uses: the identity's latest request
    // head against the surface's recorded head. Request-less identities
    // show no moved badge, and precise anchor drift stays the review
    // surface's job.
    let mut latest_by_identity: std::collections::HashMap<IdentityKey, Option<String>> =
        std::collections::HashMap::new();
    for (identity, request) in latest_requests_newest_first(pool).await? {
        latest_by_identity.entry(identity).or_insert(request.head_sha);
    }
    let heads = crate::retrospection::recorded_surface_heads_in_pool(pool).await?;

    let root_ids: Vec<i64> = roots.iter().map(|(root, _)| root.root_comment_id).collect();
    let mut reply_participants = reply_participants_for_roots(pool, &root_ids).await?;

    // Group before the state filter so a group's open count covers all of
    // its threads no matter which state chip is active. Group order follows
    // the rows' newest-first activity: a group enters when its newest
    // thread is seen.
    let mut groups: Vec<PortalThreadGroup> = Vec::new();
    let mut group_index: std::collections::HashMap<(String, String, String), usize> =
        std::collections::HashMap::new();
    for (mut row, root_author) in roots {
        let recorded = if row.target_kind == "worktree" {
            crate::retrospection::recorded_worktree_head(&heads, &row.repo_path, &row.target_key)
        } else {
            None
        };
        // The recorded head when the surface was retrospected, else the
        // identity itself for head-kind rows; the same open path the
        // reviews listing's rows carry.
        row.head_sha = recorded
            .map(|head| head.head_sha.clone())
            .or_else(|| (row.target_kind == "head").then(|| row.target_key.clone()));
        row.head_moved = latest_by_identity
            .get(&(
                row.repo_path.clone(),
                row.base_sha.clone(),
                row.target_key.clone(),
                row.target_kind.clone(),
            ))
            .and_then(|head| head.as_deref())
            .map_or(false, |head| surface_head_moved(recorded, Some(head)));
        row.participants = merge_participants(
            (
                root_author.author_kind.as_str(),
                root_author.author_name.as_str(),
            ),
            &reply_participants
                .remove(&row.root_comment_id)
                .unwrap_or_default(),
        );
        let key = (
            row.repo_path.clone(),
            row.target_key.clone(),
            row.target_kind.clone(),
        );
        let index = match group_index.get(&key) {
            Some(index) => *index,
            None => {
                groups.push(PortalThreadGroup {
                    repo_path: row.repo_path.clone(),
                    target_key: row.target_key.clone(),
                    target_kind: row.target_kind.clone(),
                    change_label: identity_change_label(&row.target_kind, &row.target_key, recorded),
                    open_count: 0,
                    threads: Vec::new(),
                });
                group_index.insert(key, groups.len() - 1);
                groups.len() - 1
            }
        };
        if row.resolved_at.is_none() {
            groups[index].open_count += 1;
        }
        let matches_state = state == STATE_ALL
            || (state == THREAD_STATE_OPEN && row.resolved_at.is_none())
            || (state == THREAD_STATE_RESOLVED && row.resolved_at.is_some());
        if matches_state {
            groups[index].threads.push(row);
        }
    }
    groups.retain(|group| !group.threads.is_empty());
    Ok(groups)
}

// ===== Thread detail =====

// A thread's full conversation: the stored root and replies verbatim (the
// same serde shape the review surface's list_comments returns, so anchors
// and snippets read as stored), the identity and its label, and the same
// moved-badge basis as the listing. No Git access.
#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct PortalThreadDetail {
    pub(crate) root_comment_id: i64,
    pub(crate) repo_path: String,
    pub(crate) base_sha: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
    pub(crate) change_label: String,
    // The recorded head when the surface was last retrospected (else the
    // identity itself for head-kind rows), so the webview can open the
    // review the same way the reviews rows do.
    pub(crate) head_sha: Option<String>,
    pub(crate) head_moved: bool,
    pub(crate) resolved_at: Option<i64>,
    pub(crate) root: Comment,
    pub(crate) replies: Vec<Comment>,
    pub(crate) participants: Vec<PortalParticipant>,
}

pub(crate) async fn get_portal_thread_in_pool(
    pool: &SqlitePool,
    root_comment_id: i64,
) -> Result<PortalThreadDetail, CommandError> {
    let row = sqlx::query(
        "SELECT c.resolved_at, c.author_kind, c.author_name, \
         rv.repo_path, rv.base_sha, rv.target_key, rv.target_kind \
         FROM comments AS c JOIN reviews AS rv ON rv.id = c.review_id \
         WHERE c.id = ? AND c.parent_id IS NULL",
    )
    .bind(root_comment_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| CommandError::new("invalid_comment", "The thread does not exist."))?;
    let resolved_at: Option<i64> = row.try_get("resolved_at")?;
    let repo_path: String = row.try_get("repo_path")?;
    let base_sha: String = row.try_get("base_sha")?;
    let target_key: String = row.try_get("target_key")?;
    let target_kind: String = row.try_get("target_kind")?;

    // The stored comments come from the same find-only listing the review
    // surface renders; the thread's slice of that listing is its root and
    // ordered replies.
    let comments = list_comments_in_pool(pool, &repo_path, &base_sha, &target_key, &target_kind)
        .await?;
    let root = comments
        .iter()
        .find(|comment| comment.id() == root_comment_id)
        .cloned()
        .ok_or_else(|| CommandError::new("invalid_comment", "The comment does not exist."))?;
    let reply_ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM comments WHERE parent_id = ? ORDER BY created_at, id")
            .bind(root_comment_id)
            .fetch_all(pool)
            .await?;
    let replies: Vec<Comment> = comments
        .into_iter()
        .filter(|comment| reply_ids.contains(&comment.id()))
        .collect();

    // Participants: the root author first, then reply authors in reply
    // order, deduplicated like the listing rows. The authors read straight
    // from the store; the comment payloads stay the review surface's type.
    let mut participants = vec![PortalParticipant {
        author_kind: row.try_get("author_kind")?,
        author_name: row.try_get("author_name")?,
    }];
    let reply_row = sqlx::query(
        "SELECT author_kind, author_name FROM comments WHERE parent_id = ? ORDER BY created_at, id",
    )
    .bind(root_comment_id)
    .fetch_all(pool)
    .await?;
    for author in reply_row {
        let candidate = PortalParticipant {
            author_kind: author.try_get("author_kind")?,
            author_name: author.try_get("author_name")?,
        };
        if !participants
            .iter()
            .any(|known| known.author_kind == candidate.author_kind && known.author_name == candidate.author_name)
        {
            participants.push(candidate);
        }
    }

    let heads = crate::retrospection::recorded_surface_heads_in_pool(pool).await?;
    let recorded = if target_kind == "worktree" {
        crate::retrospection::recorded_worktree_head(&heads, &repo_path, &target_key)
    } else {
        None
    };
    let latest_head = latest_requests_newest_first(pool)
        .await?
        .into_iter()
        .find(|(identity, _)| {
            identity.0 == repo_path
                && identity.1 == base_sha
                && identity.2 == target_key
                && identity.3 == target_kind
        })
        .and_then(|(_, request)| request.head_sha);
    let head_moved = latest_head
        .as_deref()
        .map_or(false, |head| surface_head_moved(recorded, Some(head)));

    Ok(PortalThreadDetail {
        root_comment_id: root.id(),
        change_label: identity_change_label(&target_kind, &target_key, recorded),
        head_sha: recorded
            .map(|head| head.head_sha.clone())
            .or_else(|| (target_kind == "head").then(|| target_key.clone())),
        head_moved,
        resolved_at,
        repo_path,
        base_sha,
        target_key,
        target_kind,
        root,
        replies,
        participants,
    })
}

// ===== Portal search =====

// Each search scope is capped independently; the palette pages the combined
// result through its existing page size, so the total stays bounded.
const SEARCH_SCOPE_LIMIT: usize = 50;
// The commit scan reads a bounded newest slice before attribution, so a
// huge commit cache cannot turn one keystroke into an unbounded pass.
const SEARCH_COMMIT_SCAN_LIMIT: i64 = 200;

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct PortalSearchComment {
    pub(crate) comment_id: i64,
    pub(crate) root_comment_id: i64,
    pub(crate) excerpt: String,
    pub(crate) repo_path: String,
    pub(crate) change_label: String,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct PortalSearchRequest {
    pub(crate) repo_path: String,
    pub(crate) base_sha: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
    pub(crate) head_sha: Option<String>,
    pub(crate) change_label: String,
    pub(crate) status: String,
    pub(crate) requester: String,
    pub(crate) note: String,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct PortalSearchCommit {
    pub(crate) repo_path: String,
    pub(crate) sha: String,
    pub(crate) subject: String,
    pub(crate) parents: Vec<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct PortalSearchMatches {
    pub(crate) comments: Vec<PortalSearchComment>,
    pub(crate) requests: Vec<PortalSearchRequest>,
    pub(crate) commits: Vec<PortalSearchCommit>,
}

// Bounded case-insensitive search across the three store scopes, answered
// together: matched comment bodies, matched requests (over the note and the
// derived change label), and matched cached commit subjects. A blank needle
// answers empty scopes.
pub(crate) async fn search_portal_in_pool(
    pool: &SqlitePool,
    needle: &str,
) -> Result<PortalSearchMatches, CommandError> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Ok(PortalSearchMatches {
            comments: Vec::new(),
            requests: Vec::new(),
            commits: Vec::new(),
        });
    }
    let heads = crate::retrospection::recorded_surface_heads_in_pool(pool).await?;
    // Labels derive per identity; matched rows repeat identities, so the
    // derivation is memoized per identity within the call.
    let mut labels: std::collections::HashMap<IdentityKey, String> = std::collections::HashMap::new();
    let mut change_label = |repo_path: &str,
                            base_sha: &str,
                            target_key: &str,
                            target_kind: &str|
     -> Result<String, CommandError> {
        let key: IdentityKey = (
            repo_path.to_string(),
            base_sha.to_string(),
            target_key.to_string(),
            target_kind.to_string(),
        );
        if let Some(label) = labels.get(&key) {
            return Ok(label.clone());
        }
        let recorded = if target_kind == "worktree" {
            crate::retrospection::recorded_worktree_head(&heads, repo_path, target_key)
        } else {
            None
        };
        let label = identity_change_label(target_kind, target_key, recorded);
        labels.insert(key, label.clone());
        Ok(label)
    };

    let pattern = like_pattern(needle);
    let mut comments = Vec::new();
    let comment_rows = sqlx::query(
        "SELECT c.id, c.parent_id, c.body, rv.repo_path, rv.base_sha, rv.target_key, rv.target_kind \
         FROM comments AS c JOIN reviews AS rv ON rv.id = c.review_id \
         WHERE c.body LIKE ? ESCAPE '\\' \
         ORDER BY c.created_at DESC, c.id DESC LIMIT ?",
    )
    .bind(&pattern)
    .bind(SEARCH_SCOPE_LIMIT as i64)
    .fetch_all(pool)
    .await?;
    for row in comment_rows {
        let comment_id: i64 = row.try_get("id")?;
        let repo_path: String = row.try_get("repo_path")?;
        let base_sha: String = row.try_get("base_sha")?;
        let target_key: String = row.try_get("target_key")?;
        let target_kind: String = row.try_get("target_kind")?;
        comments.push(PortalSearchComment {
            comment_id,
            root_comment_id: row
                .try_get::<Option<i64>, _>("parent_id")?
                .unwrap_or(comment_id),
            excerpt: row
                .try_get::<String, _>("body")?
                .lines()
                .next()
                .unwrap_or_default()
                .trim_end()
                .to_string(),
            change_label: change_label(&repo_path, &base_sha, &target_key, &target_kind)?,
            repo_path,
        });
    }

    // The request scope matches the derived change label, which only exists
    // in Rust (its derivation reads the retrospection store), so matching
    // runs in Rust with literal `%`/`_` — the same semantics escaped-LIKE
    // produces and the same rule the reviews listing's search uses.
    let lowered = needle.to_ascii_lowercase();
    let mut requests = Vec::new();
    let mut seen: std::collections::HashSet<IdentityKey> = std::collections::HashSet::new();
    for (identity, request) in latest_requests_newest_first(pool).await? {
        if requests.len() >= SEARCH_SCOPE_LIMIT {
            break;
        }
        if seen.contains(&identity) {
            continue;
        }
        seen.insert(identity.clone());
        let recorded = if identity.3 == "worktree" {
            crate::retrospection::recorded_worktree_head(&heads, &identity.0, &identity.2)
        } else {
            None
        };
        let change_label_value = identity_change_label(&identity.3, &identity.2, recorded);
        let matches = [change_label_value.as_str(), request.note.as_str()]
            .iter()
            .any(|field| field.to_ascii_lowercase().contains(&lowered));
        if !matches {
            continue;
        }
        let head_sha = recorded
            .map(|head| head.head_sha.clone())
            .or_else(|| (identity.3 == "head").then(|| identity.2.clone()));
        requests.push(PortalSearchRequest {
            repo_path: identity.0,
            base_sha: identity.1,
            target_key: identity.2,
            target_kind: identity.3,
            head_sha,
            change_label: change_label_value,
            status: request.status,
            requester: request.requester,
            note: request.note,
        });
    }

    // Cached log commits carry no repo column; the log pages map each
    // commit sha back to the repositories whose pages hold it.
    let mut repos_by_sha: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for row in sqlx::query("SELECT repo_path, commit_shas FROM log_pages")
        .fetch_all(pool)
        .await?
    {
        let repo_path: String = row.try_get("repo_path")?;
        let shas: Vec<String> =
            serde_json::from_str(&row.try_get::<String, _>("commit_shas")?).unwrap_or_default();
        for sha in shas {
            let repos = repos_by_sha.entry(sha).or_default();
            if !repos.contains(&repo_path) {
                repos.push(repo_path.clone());
            }
        }
    }
    let mut commits = Vec::new();
    let commit_rows = sqlx::query(
        "SELECT sha, subject, parents FROM commits \
         WHERE subject LIKE ? ESCAPE '\\' ORDER BY date DESC LIMIT ?",
    )
    .bind(&pattern)
    .bind(SEARCH_COMMIT_SCAN_LIMIT)
    .fetch_all(pool)
    .await?;
    for row in commit_rows {
        let sha: String = row.try_get("sha")?;
        let parents: Vec<String> =
            serde_json::from_str(&row.try_get::<String, _>("parents")?).unwrap_or_default();
        for repo_path in repos_by_sha.get(&sha).cloned().unwrap_or_default() {
            if commits.len() >= SEARCH_SCOPE_LIMIT {
                break;
            }
            commits.push(PortalSearchCommit {
                repo_path,
                subject: row.try_get("subject")?,
                parents: parents.clone(),
                sha: sha.clone(),
            });
        }
        if commits.len() >= SEARCH_SCOPE_LIMIT {
            break;
        }
    }

    Ok(PortalSearchMatches {
        comments,
        requests,
        commits,
    })
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

    // Inserts a request directly, so no lifecycle event rides along and the
    // fallback composition is what the row exposes.
    async fn seed_bare_request(pool: &SqlitePool, repo: &str, base: &str, key: &str, head: &str, note: &str) {
        sqlx::query(
            "INSERT INTO review_requests (repo_path, base_sha, target_key, target_kind, \
                    status, note, head_sha, lenses, reviewers, max_rounds, round, created_at, updated_at) \
             VALUES (?, ?, ?, 'worktree', 'requested', ?, ?, '[]', '[]', 2, 0, 1, 1)",
        )
        .bind(repo)
        .bind(base)
        .bind(key)
        .bind(note)
        .bind(head)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn list(pool: &SqlitePool, query: &PortalReviewQuery) -> Vec<PortalReviewRow> {
        list_portal_reviews_in_pool(pool, query).await.unwrap()
    }

    // One root thread, optionally line-anchored, stored like the ingest
    // path stores it; the new id is the review's highest comment id.
    async fn seed_thread(
        pool: &SqlitePool,
        repo: &str,
        base: &str,
        key: &str,
        author: (&str, &str),
        body: &str,
        created_at: i64,
        anchor: Option<(&str, &str, i64)>,
    ) -> i64 {
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
        let review = review_id(pool, repo, base, key).await;
        match anchor {
            Some((file, side, line)) => {
                sqlx::query(
                    "INSERT INTO comments (review_id, parent_id, author_kind, author_name, body, \
                            file_path, side, start_line, end_line, created_at) \
                     VALUES (?, NULL, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(review)
                .bind(author.0)
                .bind(author.1)
                .bind(body)
                .bind(file)
                .bind(side)
                .bind(line)
                .bind(line)
                .bind(created_at)
                .execute(pool)
                .await
                .unwrap();
            }
            None => {
                sqlx::query(
                    "INSERT INTO comments (review_id, parent_id, author_kind, author_name, body, created_at) \
                     VALUES (?, NULL, ?, ?, ?, ?)",
                )
                .bind(review)
                .bind(author.0)
                .bind(author.1)
                .bind(body)
                .bind(created_at)
                .execute(pool)
                .await
                .unwrap();
            }
        }
        sqlx::query_scalar("SELECT MAX(id) FROM comments WHERE review_id = ?")
            .bind(review)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn seed_reply(
        pool: &SqlitePool,
        root_id: i64,
        author: (&str, &str),
        body: &str,
        created_at: i64,
    ) -> i64 {
        sqlx::query(
            "INSERT INTO comments (review_id, parent_id, author_kind, author_name, body, created_at) \
             VALUES ((SELECT review_id FROM comments WHERE id = ?), ?, ?, ?, ?, ?)",
        )
        .bind(root_id)
        .bind(root_id)
        .bind(author.0)
        .bind(author.1)
        .bind(body)
        .bind(created_at)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query_scalar("SELECT MAX(id) FROM comments WHERE parent_id = ?")
            .bind(root_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn resolve_root(pool: &SqlitePool, root_id: i64, resolved_at: i64) {
        sqlx::query("UPDATE comments SET resolved_at = ? WHERE id = ?")
            .bind(resolved_at)
            .bind(root_id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn set_severity(pool: &SqlitePool, root_id: i64, severity: &str) {
        sqlx::query("UPDATE comments SET severity = ? WHERE id = ?")
            .bind(severity)
            .bind(root_id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn threads(pool: &SqlitePool, query: &PortalThreadQuery) -> Vec<PortalThreadGroup> {
        list_portal_threads_in_pool(pool, query).await.unwrap()
    }

    fn thread_of<'a>(groups: &'a [PortalThreadGroup], base: &str) -> &'a PortalThreadRow {
        groups
            .iter()
            .flat_map(|group| group.threads.iter())
            .find(|thread| thread.base_sha == base)
            .unwrap_or_else(|| panic!("no portal thread for {base}"))
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
        set_request_verdict_in_pool(&pool, approved.id, true, &Actor::Human("human".into()), &noop_notify())
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
        set_request_verdict_in_pool(&pool, approved_moved.id, true, &Actor::Human("human".into()), &noop_notify())
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
        set_request_verdict_in_pool(&pool, approved.id, true, &Actor::Human("human".into()), &noop_notify())
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
    async fn the_last_event_rides_the_newest_identity_event() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let reviewer = agent_token(&pool, "reviewer-bot").await;
        let created = create(&pool, REPO, "base-event", "/wt-event", "head-event", &Actor::Agent(coder)).await;
        // The create itself narrates; a claim then becomes the newest event
        // and the row's last-event text.
        claim_request_in_pool(&pool, created.id, &Actor::Agent(reviewer), &noop_notify())
            .await
            .unwrap();

        let rows = list(&pool, &PortalReviewQuery::default()).await;
        let row = row_of(&rows, "base-event");
        assert_eq!(row.last_event, "claimed by reviewer-bot");
        assert_eq!(row.requester, "coder-bot");
        assert_eq!(row.requester_kind, ACTOR_AGENT);
    }

    #[tokio::test]
    async fn identities_without_events_compose_their_last_event_from_facts() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        // A requested row with a note reads the note; a note-less one reads
        // its status; a request-less commented identity reads its counts.
        seed_bare_request(&pool, REPO, "base-note", "/wt-note", "head-note", "Please review my changes.").await;
        seed_bare_request(&pool, REPO, "base-plain", "/wt-plain", "head-plain", "").await;
        seed_finding(&pool, REPO, "base-comments", "/wt-c", "P1", 1).await;
        seed_finding(&pool, REPO, "base-comments", "/wt-c", "P2", 2).await;

        let rows = list(&pool, &PortalReviewQuery::default()).await;
        let noted = row_of(&rows, "base-note");
        assert_eq!(noted.last_event, "requested with note: Please review my changes.");
        assert_eq!(noted.requester, "human");
        assert_eq!(noted.requester_kind, ACTOR_HUMAN);
        assert_eq!(row_of(&rows, "base-plain").last_event, "requested");
        assert_eq!(row_of(&rows, "base-comments").last_event, "2 comments");
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

    #[tokio::test]
    async fn threads_group_across_bases_under_one_change() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        // Two identities on the same worktree at different bases: one
        // change, one group, both threads inside it.
        let older = seed_thread(&pool, REPO, "base-one", "/wt-a", ("human", "dana"), "first look", 1_000, None).await;
        let newer = seed_thread(&pool, REPO, "base-two", "/wt-a", ("agent", "reviewer-bot"), "second look", 2_000, None).await;
        // A different worktree is a different group even at the same base.
        seed_thread(&pool, REPO, "base-two", "/wt-b", ("human", "dana"), "other change", 3_000, None).await;

        crate::retrospection::record_surface_open(
            &pool, REPO, "worktree", "/wt-a", "feature-a", "/wt-a", "head-a",
        )
        .await;

        let groups = threads(&pool, &PortalThreadQuery { state: Some(STATE_ALL.into()), ..Default::default() }).await;
        assert_eq!(groups.len(), 2, "grouping keys on the change, never the base or label");
        let group = groups.iter().find(|group| group.target_key == "/wt-a").unwrap();
        assert_eq!(group.change_label, "feature-a");
        assert_eq!(group.repo_path, REPO);
        assert_eq!(group.open_count, 2);
        assert_eq!(group.threads.len(), 2);
        assert_eq!(group.threads[0].root_comment_id, newer, "newest activity first inside the group");
        assert_eq!(group.threads[1].root_comment_id, older);
        assert_eq!(groups[0].target_key, "/wt-b", "groups order by their newest thread");
    }

    #[tokio::test]
    async fn state_voice_text_and_repo_filters_narrow_threads() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        seed_repo(&pool, "/other").await;
        // Human-rooted with an agent reply on /demo; agent-rooted on
        // /other; a resolved human thread on /demo.
        let human = seed_thread(&pool, REPO, "base-h", "/wt-h", ("human", "dana"), "how does this hold up", 1_000, None).await;
        seed_reply(&pool, human, ("agent", "reviewer-bot"), "checked the zebra path", 1_500).await;
        seed_thread(&pool, "/other", "base-a", "/wt-a", ("agent", "coder-bot"), "agent note", 2_000, None).await;
        let settled = seed_thread(&pool, REPO, "base-r", "/wt-r", ("human", "dana"), "resolved already", 3_000, None).await;
        resolve_root(&pool, settled, 3_500).await;

        // The default is open: the resolved thread drops.
        let open = threads(&pool, &PortalThreadQuery::default()).await;
        assert_eq!(open.iter().flat_map(|group| group.threads.iter()).count(), 2);

        let all = threads(&pool, &PortalThreadQuery { state: Some(STATE_ALL.into()), ..Default::default() }).await;
        assert_eq!(all.iter().flat_map(|group| group.threads.iter()).count(), 3);
        let resolved = threads(&pool, &PortalThreadQuery { state: Some(THREAD_STATE_RESOLVED.into()), ..Default::default() }).await;
        let resolved_rows: Vec<_> = resolved.iter().flat_map(|group| group.threads.iter()).collect();
        assert_eq!(resolved_rows.len(), 1);
        assert_eq!(resolved_rows[0].root_comment_id, settled);

        // Voice reads any participant: the mixed thread answers both voices.
        let humans = threads(&pool, &PortalThreadQuery { voice: Some(THREAD_VOICE_HUMAN.into()), state: Some(STATE_ALL.into()), ..Default::default() }).await;
        assert_eq!(humans.iter().flat_map(|group| group.threads.iter()).filter(|thread| thread.root_comment_id == human).count(), 1);
        let agents = threads(&pool, &PortalThreadQuery { voice: Some(THREAD_VOICE_AGENTS.into()), state: Some(STATE_ALL.into()), ..Default::default() }).await;
        assert_eq!(agents.iter().flat_map(|group| group.threads.iter()).filter(|thread| thread.root_comment_id == human).count(), 1);

        // Text matches root and reply bodies, escaped wildcards match
        // literally, and a blank needle keeps everything.
        let text = threads(&pool, &PortalThreadQuery { text: Some("zebra".into()), state: Some(STATE_ALL.into()), ..Default::default() }).await;
        assert_eq!(text.iter().flat_map(|group| group.threads.iter()).map(|thread| thread.root_comment_id).collect::<Vec<_>>(), vec![human]);
        let wildcard = threads(&pool, &PortalThreadQuery { text: Some("o_e".into()), state: Some(STATE_ALL.into()), ..Default::default() }).await;
        assert_eq!(wildcard.iter().flat_map(|group| group.threads.iter()).count(), 0);
        let percent = threads(&pool, &PortalThreadQuery { text: Some("%".into()), state: Some(STATE_ALL.into()), ..Default::default() }).await;
        assert_eq!(percent.iter().flat_map(|group| group.threads.iter()).count(), 0);
        let blank = threads(&pool, &PortalThreadQuery { text: Some("  ".into()), state: Some(STATE_ALL.into()), ..Default::default() }).await;
        assert_eq!(blank.iter().flat_map(|group| group.threads.iter()).count(), 3);

        // Exact repo filter.
        let by_repo = threads(&pool, &PortalThreadQuery { repo_path: Some("/other".into()), state: Some(STATE_ALL.into()), ..Default::default() }).await;
        assert_eq!(by_repo.len(), 1);
        assert_eq!(by_repo[0].repo_path, "/other");

        // Unknown state and voice values are refused like the reviews
        // listing refuses unknown states.
        let error = list_portal_threads_in_pool(&pool, &PortalThreadQuery { state: Some("closed".into()), ..Default::default() }).await.unwrap_err();
        assert_eq!(error.code, "invalid_request");
        let error = list_portal_threads_in_pool(&pool, &PortalThreadQuery { voice: Some("robots".into()), ..Default::default() }).await.unwrap_err();
        assert_eq!(error.code, "invalid_request");
    }

    #[tokio::test]
    async fn thread_rows_carry_anchors_badges_and_participants() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;

        // An anchored thread on an identity whose recorded head moved past
        // the reviewed head: the moved badge fires.
        let anchored = seed_thread(
            &pool, REPO, "base-moved", "/wt-m", ("agent", "reviewer-bot"), "P1: broken loop", 1_000,
            Some(("src/app.ts", "RIGHT", 41)),
        ).await;
        set_severity(&pool, anchored, "P1").await;
        seed_reply(&pool, anchored, ("agent", "reviewer-bot"), "confirmed on my side", 1_200).await;
        seed_reply(&pool, anchored, ("agent", "reviewer-bot"), "confirmed twice", 1_300).await;
        create(&pool, REPO, "base-moved", "/wt-m", "old-head", &Actor::Agent(coder)).await;
        crate::retrospection::record_surface_open(
            &pool, REPO, "worktree", "/wt-m", "feature-m", "/wt-m", "moved-head",
        )
        .await;

        // A review-level thread with no request ever: no anchor, no badge.
        let review_level = seed_thread(&pool, REPO, "base-plain", "/wt-p", ("human", "dana"), "overall note", 2_000, None).await;

        // A head-kind thread carries the identity itself as its head.
        sqlx::query(
            "INSERT OR IGNORE INTO reviews (repo_path, base_sha, target_key, target_kind, created_at) \
             VALUES (?, 'base-head', 'head-sha-9', 'head', 1)",
        )
        .bind(REPO)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO comments (review_id, parent_id, author_kind, author_name, body, created_at) \
             VALUES ((SELECT id FROM reviews WHERE repo_path = ? AND base_sha = 'base-head'), NULL, 'human', 'dana', 'head note', 4_000)",
        )
        .bind(REPO)
        .execute(&pool)
        .await
        .unwrap();

        let groups = threads(&pool, &PortalThreadQuery { state: Some(STATE_ALL.into()), ..Default::default() }).await;
        let moved = thread_of(&groups, "base-moved");
        assert_eq!(moved.anchor.as_deref(), Some("src/app.ts:41"));
        assert_eq!(moved.severity.as_deref(), Some("P1"));
        assert!(moved.head_moved, "the recorded head differs from the reviewed head");
        assert_eq!(moved.head_sha.as_deref(), Some("moved-head"), "worktree rows carry their recorded head");
        assert_eq!(moved.reply_count, 2);
        assert_eq!(moved.last_activity_at, 1_300, "root plus newest reply");
        assert_eq!(
            moved.participants,
            vec![PortalParticipant { author_kind: "agent".into(), author_name: "reviewer-bot".into() }],
            "participants dedupe by kind and name"
        );

        let plain = thread_of(&groups, "base-plain");
        assert_eq!(plain.anchor, None, "review-level threads carry no anchor");
        assert_eq!(plain.severity, None);
        assert!(!plain.head_moved, "request-less identities show no moved badge");
        assert_eq!(plain.head_sha, None, "worktree rows without a recorded head carry none");
        assert_eq!(review_level, plain.root_comment_id);

        let head_kind = thread_of(&groups, "base-head");
        assert_eq!(head_kind.head_sha.as_deref(), Some("head-sha-9"), "head-kind rows carry the identity itself");
    }

    #[tokio::test]
    async fn thread_detail_returns_the_stored_conversation() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let root = seed_thread(
            &pool, REPO, "base-detail", "/wt-d", ("human", "dana"), "thread root", 1_000,
            Some(("src/lib.rs", "LEFT", 7)),
        ).await;
        let first = seed_reply(&pool, root, ("agent", "reviewer-bot"), "first reply", 1_100).await;
        let second = seed_reply(&pool, root, ("human", "dana"), "second reply", 1_200).await;
        create(&pool, REPO, "base-detail", "/wt-d", "head-d", &Actor::Agent(coder)).await;

        let detail = get_portal_thread_in_pool(&pool, root).await.unwrap();
        assert_eq!(detail.root_comment_id, root);
        assert_eq!(detail.repo_path, REPO);
        assert_eq!(detail.change_label, "/wt-d".rsplit('/').next().unwrap());
        assert!(!detail.head_moved, "the recorded head matches the reviewed head");
        assert_eq!(detail.resolved_at, None);
        assert_eq!(detail.root.id(), root);
        assert_eq!(detail.replies.iter().map(|reply| reply.id()).collect::<Vec<_>>(), vec![first, second], "replies in stored order");
        assert_eq!(
            detail.participants,
            vec![
                PortalParticipant { author_kind: "human".into(), author_name: "dana".into() },
                PortalParticipant { author_kind: "agent".into(), author_name: "reviewer-bot".into() },
            ]
        );

        resolve_root(&pool, root, 2_000).await;
        let detail = get_portal_thread_in_pool(&pool, root).await.unwrap();
        assert_eq!(detail.resolved_at, Some(2_000));

        // Unknown and non-root ids are refused with the comment error code.
        let error = get_portal_thread_in_pool(&pool, 99_999).await.unwrap_err();
        assert_eq!(error.code, "invalid_comment");
        let error = get_portal_thread_in_pool(&pool, first).await.unwrap_err();
        assert_eq!(error.code, "invalid_comment");
    }

    #[tokio::test]
    async fn the_thread_listing_is_bounded_by_the_thread_cap() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        for index in 0..(THREAD_ROW_LIMIT + 5) {
            seed_thread(
                &pool, REPO, &format!("base-{index:04}"), &format!("/wt-{index:04}"),
                ("human", "dana"), "thread", 1_000 + index as i64, None,
            ).await;
        }
        let groups = threads(&pool, &PortalThreadQuery { state: Some(STATE_ALL.into()), ..Default::default() }).await;
        let listed: Vec<_> = groups.iter().flat_map(|group| group.threads.iter()).collect();
        assert_eq!(listed.len(), THREAD_ROW_LIMIT);
        assert_eq!(listed[0].last_activity_at, 1_000 + THREAD_ROW_LIMIT as i64 + 4, "newest activity first");
        assert_eq!(listed[THREAD_ROW_LIMIT - 1].last_activity_at, 1_005, "the five oldest threads drop");
    }

    #[tokio::test]
    async fn search_matches_comments_requests_and_commits() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;

        let root = seed_thread(&pool, REPO, "base-c", "/wt-c", ("human", "dana"), "quicksort edge case", 1_000, None).await;
        seed_reply(&pool, root, ("agent", "reviewer-bot"), "the quicksort loop overflows", 1_100).await;
        let mut zebras = draft("head-search");
        zebras.note = Some("watch the quicksort rewrite".into());
        create_request_in_pool(&pool, REPO, "base-r", "/wt-r", "worktree", &zebras, &Actor::Agent(coder), &noop_notify())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO commits (sha, subject, author, date, refs, parents) \
             VALUES (?, ?, 'dana', '2026-01-02T03:04:05+00:00', '[]', '[]')",
        )
        .bind("a".repeat(40))
        .bind("fix quicksort pivot")
        .execute(&pool)
        .await
        .unwrap();
        // The log page attributes the commit to the repo, like the cache
        // stores it: commit_shas is a JSON array.
        sqlx::query(
            "INSERT OR IGNORE INTO log_pages (repo_path, start_sha, against_sha, skip, limit_value, commit_shas, has_more) \
             VALUES (?, 'start', '', 0, 1, ?, 0)",
        )
        .bind(REPO)
        .bind(format!("[\"{}\"]", "a".repeat(40)))
        .execute(&pool)
        .await
        .unwrap();

        let matches = search_portal_in_pool(&pool, "quicksort").await.unwrap();
        assert_eq!(matches.comments.len(), 2, "root and reply bodies both match");
        assert!(matches.comments.iter().all(|row| row.root_comment_id == root));
        assert_eq!(matches.requests.len(), 1, "the request note matches");
        assert_eq!(matches.requests[0].note, "watch the quicksort rewrite");
        assert_eq!(matches.commits.len(), 1, "the cached commit subject matches");
        assert_eq!(matches.commits[0].sha, "a".repeat(40));
        assert_eq!(matches.commits[0].repo_path, REPO);
        assert_eq!(matches.commits[0].parents, Vec::<String>::new());

        // The request's change label matches too: the label comes from the
        // worktree path's folder name with no recorded surface.
        let matches = search_portal_in_pool(&pool, "WT-R").await.unwrap();
        assert_eq!(matches.requests.len(), 1);
        assert!(matches.comments.is_empty());
        assert!(matches.commits.is_empty());

        // Wildcards match literally, and a blank needle answers empty.
        let matches = search_portal_in_pool(&pool, "q_icksort").await.unwrap();
        assert!(matches.comments.is_empty());
        let matches = search_portal_in_pool(&pool, "   ").await.unwrap();
        assert!(matches.comments.is_empty() && matches.requests.is_empty() && matches.commits.is_empty());
    }

    #[tokio::test]
    async fn the_comment_search_scope_stays_bounded() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        for index in 0..(SEARCH_SCOPE_LIMIT + 2) {
            seed_thread(
                &pool, REPO, &format!("base-{index:04}"), &format!("/wt-{index:04}"),
                ("human", "dana"), "needle haystack", index as i64, None,
            ).await;
        }
        let matches = search_portal_in_pool(&pool, "needle").await.unwrap();
        assert_eq!(matches.comments.len(), SEARCH_SCOPE_LIMIT, "each scope caps at its constant");
    }
}
