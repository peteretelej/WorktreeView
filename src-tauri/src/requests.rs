use crate::events;
use crate::store::now_millis;
use crate::transport::{RequestChange, RequestChangeSink};
use crate::CommandError;
use serde::Serialize;
use sqlx::{Row, SqlitePool};

// The lifecycle statuses; only this module's transitions write them, so
// the store carries no CHECK and the state machine is the single writer.
pub(crate) const REQUESTED: &str = "requested";
pub(crate) const IN_REVIEW: &str = "in_review";
pub(crate) const CHANGES_REQUESTED: &str = "changes_requested";
pub(crate) const APPROVED: &str = "approved";
pub(crate) const WITHDRAWN: &str = "withdrawn";

// The severities that count as blocking, single site for the package: the
// ingest path computes its observation flag from this predicate and the
// attention queries build their severity predicates from this constant.
pub(crate) const BLOCKING_SEVERITIES: [&str; 2] = ["P0", "P1"];

pub(crate) fn is_blocking_severity(severity: &str) -> bool {
    BLOCKING_SEVERITIES.contains(&severity)
}

const LENS_VOCABULARY: [&str; 5] = ["security", "correctness", "design", "performance", "tests"];
const MAX_NOTE_CHARS: usize = 2000;
const DEFAULT_MAX_ROUNDS: i64 = 2;
const MIN_MAX_ROUNDS: i64 = 1;
const MAX_MAX_ROUNDS: i64 = 3;

// Who a request operation acts as: the MCP face always arrives as the
// authenticated token id, the human IPC commands always pass Human. Human
// parity lives here: a human performs any reviewer transition, never
// gated by a request's named reviewers list.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Actor {
    Agent(i64),
    Human,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct ReviewRequest {
    pub(crate) id: i64,
    pub(crate) repo_path: String,
    pub(crate) base_sha: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
    pub(crate) requester_token_id: Option<i64>,
    pub(crate) status: String,
    pub(crate) note: String,
    pub(crate) lenses: Vec<String>,
    pub(crate) reviewers: Vec<String>,
    pub(crate) max_rounds: i64,
    pub(crate) round: i64,
    pub(crate) head_sha: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

// The create payload. Strict validation here is the one rule set every
// caller face shares; Phase 2's MCP schema mirrors it. An absent note is
// a general ask: it inserts empty and preserves the stored note when a
// create refreshes an existing request.
#[derive(Debug, Clone)]
pub(crate) struct RequestDraft {
    pub(crate) note: Option<String>,
    pub(crate) lenses: Vec<String>,
    pub(crate) reviewers: Vec<String>,
    pub(crate) max_rounds: Option<i64>,
    pub(crate) head_sha: String,
}

fn invalid_request(message: impl Into<String>) -> CommandError {
    CommandError::new("invalid_request", message)
}

// A refused transition names the allowed actor: the error is the contract
// surfaces guide against, never a silent no-op.
fn invalid_transition(message: impl Into<String>) -> CommandError {
    CommandError::new("invalid_transition", message)
}

fn unknown_request() -> CommandError {
    CommandError::new("unknown_request", "The review request does not exist.")
}

// The same target vocabulary every identity-taking tool accepts.
fn validate_identity(target_key: &str, target_kind: &str) -> Result<(), CommandError> {
    if target_key.trim().is_empty() {
        return Err(invalid_request("The target key must be a non-empty string."));
    }
    if target_kind != "worktree" && target_kind != "head" {
        return Err(invalid_request(
            r#"The target kind must be "worktree" or "head"."#,
        ));
    }
    Ok(())
}

fn validate_draft(draft: &RequestDraft) -> Result<(), CommandError> {
    if let Some(note) = &draft.note {
        if note.chars().count() > MAX_NOTE_CHARS {
            return Err(invalid_request(format!(
                "The note exceeds {MAX_NOTE_CHARS} characters."
            )));
        }
    }
    if draft.head_sha.trim().is_empty() {
        return Err(invalid_request(
            "A review request needs a non-empty head.",
        ));
    }
    let mut seen_lenses = std::collections::BTreeSet::new();
    for lens in &draft.lenses {
        if !LENS_VOCABULARY.contains(&lens.as_str()) {
            return Err(invalid_request(format!(
                "Unknown lens '{lens}'; lenses are security, correctness, design, performance, or tests."
            )));
        }
        if !seen_lenses.insert(lens.as_str()) {
            return Err(invalid_request(format!("The lens '{lens}' is duplicated.")));
        }
    }
    let mut seen_reviewers = std::collections::BTreeSet::new();
    for reviewer in &draft.reviewers {
        if !seen_reviewers.insert(reviewer.as_str()) {
            return Err(invalid_request(format!(
                "The reviewer '{reviewer}' is duplicated."
            )));
        }
    }
    if let Some(max_rounds) = draft.max_rounds {
        if !(MIN_MAX_ROUNDS..=MAX_MAX_ROUNDS).contains(&max_rounds) {
            return Err(invalid_request(format!(
                "The round budget must be between {MIN_MAX_ROUNDS} and {MAX_MAX_ROUNDS}."
            )));
        }
    }
    Ok(())
}

// Reviewers are stored as names, not foreign keys: deleting a token leaves
// the name in place and claim-time validation checks current token names,
// so only names that exist at create time are accepted.
async fn validate_reviewers_exist(
    pool: &SqlitePool,
    reviewers: &[String],
) -> Result<(), CommandError> {
    for reviewer in reviewers {
        let known: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_tokens WHERE name = ?")
            .bind(reviewer)
            .fetch_one(pool)
            .await?;
        if known == 0 {
            return Err(invalid_request(format!(
                "The reviewer '{reviewer}' does not name an agent token."
            )));
        }
    }
    Ok(())
}

fn json_array(value: &[String]) -> Result<String, CommandError> {
    serde_json::to_string(value)
        .map_err(|_| invalid_request("The request fields could not be serialized."))
}

fn request_from_row(row: &sqlx::sqlite::SqliteRow) -> sqlx::Result<ReviewRequest> {
    let lenses: String = row.try_get("lenses")?;
    let reviewers: String = row.try_get("reviewers")?;
    let decode = |column: &str, error: serde_json::Error| sqlx::Error::ColumnDecode {
        index: column.to_string(),
        source: Box::new(error),
    };
    Ok(ReviewRequest {
        id: row.try_get("id")?,
        repo_path: row.try_get("repo_path")?,
        base_sha: row.try_get("base_sha")?,
        target_key: row.try_get("target_key")?,
        target_kind: row.try_get("target_kind")?,
        requester_token_id: row.try_get("requester_token_id")?,
        status: row.try_get("status")?,
        note: row.try_get("note")?,
        lenses: serde_json::from_str(&lenses).map_err(|error| decode("lenses", error))?,
        reviewers: serde_json::from_str(&reviewers).map_err(|error| decode("reviewers", error))?,
        max_rounds: row.try_get("max_rounds")?,
        round: row.try_get("round")?,
        head_sha: row.try_get("head_sha")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

const REQUEST_BY_ID: &str = "SELECT id, repo_path, base_sha, target_key, target_kind, \
     requester_token_id, status, note, lenses, reviewers, max_rounds, round, head_sha, \
     created_at, updated_at FROM review_requests WHERE id = ?";

const REQUEST_SAME_HEAD: &str = "SELECT id, repo_path, base_sha, target_key, target_kind, \
     requester_token_id, status, note, lenses, reviewers, max_rounds, round, head_sha, \
     created_at, updated_at FROM review_requests \
     WHERE repo_path = ? AND base_sha = ? AND target_key = ? AND target_kind = ? \
       AND requester_token_id IS ? AND head_sha = ? ORDER BY id";

const REQUEST_REFRESHABLE: &str = "SELECT id, repo_path, base_sha, target_key, target_kind, \
     requester_token_id, status, note, lenses, reviewers, max_rounds, round, head_sha, \
     created_at, updated_at FROM review_requests \
     WHERE repo_path = ? AND base_sha = ? AND target_key = ? AND target_kind = ? \
       AND requester_token_id IS ? AND status = 'requested' AND round = 0 \
     ORDER BY updated_at DESC, id DESC LIMIT 1";

const REQUEST_OPEN_ON_IDENTITY: &str = "SELECT id, repo_path, base_sha, target_key, target_kind, \
     requester_token_id, status, note, lenses, reviewers, max_rounds, round, head_sha, \
     created_at, updated_at FROM review_requests \
     WHERE repo_path = ? AND base_sha = ? AND target_key = ? AND target_kind = ? \
       AND status IN ('requested', 'in_review') ORDER BY id";

async fn load_request(pool: &SqlitePool, id: i64) -> Result<Option<ReviewRequest>, sqlx::Error> {
    sqlx::query(REQUEST_BY_ID)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .map(|row| request_from_row(&row))
        .transpose()
}

async fn require_request(pool: &SqlitePool, id: i64) -> Result<ReviewRequest, CommandError> {
    load_request(pool, id)
        .await?
        .ok_or_else(unknown_request)
}

// One fire per successful mutation, centrally here: callers never
// announce from their own sites. The event names the log kind that
// narrates the mutation, or None when the mutation updates in place
// without narrating, so the webview can key copy on what happened.
fn fire_change(
    notify: &RequestChangeSink,
    request: &ReviewRequest,
    event: Option<&'static str>,
) {
    log::info!("review request {} -> {} (round {})", request.id, request.status, request.round);
    notify(RequestChange {
        id: request.id,
        repo_path: request.repo_path.clone(),
        base_sha: request.base_sha.clone(),
        target_key: request.target_key.clone(),
        target_kind: request.target_kind.clone(),
        status: request.status.clone(),
        event: event.map(|event| event.to_string()),
    });
}

async fn set_status(pool: &SqlitePool, id: i64, status: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE review_requests SET status = ?, updated_at = ? WHERE id = ?")
        .bind(status)
        .bind(now_millis())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// A transition whose status precondition rides the UPDATE: two racing
// writers cannot both apply, and the loser sees zero rows instead of
// silently overwriting the winner.
async fn set_status_guarded(
    pool: &SqlitePool,
    id: i64,
    expected: &str,
    status: &str,
) -> Result<bool, sqlx::Error> {
    let result =
        sqlx::query("UPDATE review_requests SET status = ?, updated_at = ? WHERE id = ? AND status = ?")
            .bind(status)
            .bind(now_millis())
            .bind(id)
            .bind(expected)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() > 0)
}

fn requester_key(actor: &Actor) -> Option<i64> {
    match actor {
        Actor::Human => None,
        Actor::Agent(token_id) => Some(*token_id),
    }
}

// Event attribution: the engine knows only the acting token id, so the
// name resolves at emission; human transitions are human.
async fn event_actor(
    pool: &SqlitePool,
    actor: &Actor,
) -> Result<(&'static str, String), sqlx::Error> {
    match actor {
        Actor::Human => Ok((events::ACTOR_HUMAN, "human".to_string())),
        Actor::Agent(token_id) => {
            let name: Option<String> =
                sqlx::query_scalar("SELECT name FROM agent_tokens WHERE id = ?")
                    .bind(token_id)
                    .fetch_optional(pool)
                    .await?;
            Ok((events::ACTOR_AGENT, name.unwrap_or_else(|| "agent".to_string())))
        }
    }
}

// One event per successful lifecycle transition, written on the same
// connection after the guarded UPDATE wins; a failed insert fails the
// mutation, so narration never silently trails the store.
async fn emit_request_event(
    pool: &SqlitePool,
    kind: &'static str,
    request: &ReviewRequest,
    actor_kind: &'static str,
    actor_name: &str,
    summary: String,
) -> Result<(), CommandError> {
    events::record_event(
        pool,
        events::NewEvent {
            repo_path: request.repo_path.clone(),
            kind,
            base_sha: Some(request.base_sha.clone()),
            target_key: Some(request.target_key.clone()),
            target_kind: Some(request.target_kind.clone()),
            request_id: Some(request.id),
            comment_id: None,
            actor_kind,
            actor_name: actor_name.to_string(),
            summary,
        },
    )
    .await?;
    Ok(())
}

// Create resolves against the identity's requests of the same requester
// (dedup keys on identity, requester, head; different requesters never
// dedup against each other): a same-head open request updates in place, a
// same-head changes_requested refusal sends the caller to re-request, a
// same-head approved row is returned as already satisfied, and anything
// else starts a new campaign. A new head refreshes a never-claimed request
// in place instead of stacking duplicate pickups.
pub(crate) async fn create_request_in_pool(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
    draft: &RequestDraft,
    actor: &Actor,
    notify: &RequestChangeSink,
) -> Result<ReviewRequest, CommandError> {
    validate_identity(target_key, target_kind)?;
    validate_draft(draft)?;
    validate_reviewers_exist(pool, &draft.reviewers).await?;
    let requester_token_id = requester_key(actor);
    let max_rounds = draft.max_rounds.unwrap_or(DEFAULT_MAX_ROUNDS);

    let same_head: Vec<ReviewRequest> = sqlx::query(REQUEST_SAME_HEAD)
        .bind(repo_path)
        .bind(base_sha)
        .bind(target_key)
        .bind(target_kind)
        .bind(requester_token_id)
        .bind(&draft.head_sha)
        .fetch_all(pool)
        .await?
        .iter()
        .map(request_from_row)
        .collect::<Result<Vec<_>, _>>()?;

    if let Some(existing) = same_head
        .iter()
        .find(|request| request.status == REQUESTED || request.status == IN_REVIEW)
    {
        // A re-ask restates the ask; a field the caller omitted (empty
        // note, empty arrays, absent budget) keeps its stored value.
        sqlx::query(
            "UPDATE review_requests SET note = COALESCE(?, note), \
             lenses = COALESCE(NULLIF(?, '[]'), lenses), \
             reviewers = COALESCE(NULLIF(?, '[]'), reviewers), \
             max_rounds = COALESCE(?, max_rounds), updated_at = ? WHERE id = ?",
        )
        .bind(&draft.note)
        .bind(json_array(&draft.lenses)?)
        .bind(json_array(&draft.reviewers)?)
        .bind(draft.max_rounds)
        .bind(now_millis())
        .bind(existing.id)
        .execute(pool)
        .await?;
        let updated = require_request(pool, existing.id).await?;
        fire_change(notify, &updated, None);
        return Ok(updated);
    }
    if same_head
        .iter()
        .any(|request| request.status == CHANGES_REQUESTED)
    {
        return Err(invalid_request(
            "This head was already sent for review and received changes; \
             re-request the review with a new head instead.",
        ));
    }
    if let Some(existing) = same_head
        .iter()
        .find(|request| request.status == APPROVED)
    {
        // The ask is already satisfied; the row is returned untouched.
        return Ok(existing.clone());
    }

    // A withdrawn same-head row or no same-head match: a new campaign. A
    // genuinely new head still refreshes an open, never-claimed request.
    let refreshable = if same_head.is_empty() {
        sqlx::query(REQUEST_REFRESHABLE)
            .bind(repo_path)
            .bind(base_sha)
            .bind(target_key)
            .bind(target_kind)
            .bind(requester_token_id)
            .fetch_optional(pool)
            .await?
            .map(|row| request_from_row(&row))
            .transpose()?
    } else {
        None
    };
    if let Some(existing) = refreshable {
        // Same omission rule as the same-head refresh: omitted fields keep
        // their stored values; the head always moves.
        sqlx::query(
            "UPDATE review_requests SET note = COALESCE(?, note), head_sha = ?, \
             lenses = COALESCE(NULLIF(?, '[]'), lenses), \
             reviewers = COALESCE(NULLIF(?, '[]'), reviewers), \
             max_rounds = COALESCE(?, max_rounds), updated_at = ? WHERE id = ?",
        )
        .bind(&draft.note)
        .bind(&draft.head_sha)
        .bind(json_array(&draft.lenses)?)
        .bind(json_array(&draft.reviewers)?)
        .bind(draft.max_rounds)
        .bind(now_millis())
        .bind(existing.id)
        .execute(pool)
        .await?;
        let updated = require_request(pool, existing.id).await?;
        fire_change(notify, &updated, None);
        return Ok(updated);
    }

    let now = now_millis();
    let result = sqlx::query(
        "INSERT INTO review_requests (repo_path, base_sha, target_key, target_kind, \
                requester_token_id, status, note, lenses, reviewers, max_rounds, round, \
                head_sha, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?)",
    )
    .bind(repo_path)
    .bind(base_sha)
    .bind(target_key)
    .bind(target_kind)
    .bind(requester_token_id)
    .bind(REQUESTED)
    .bind(draft.note.as_deref().unwrap_or(""))
    .bind(json_array(&draft.lenses)?)
    .bind(json_array(&draft.reviewers)?)
    .bind(max_rounds)
    .bind(Some(&draft.head_sha))
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    let created = require_request(pool, result.last_insert_rowid()).await?;
    let (actor_kind, actor_name) = event_actor(pool, actor).await?;
    emit_request_event(
        pool,
        events::REQUEST_CREATED,
        &created,
        actor_kind,
        &actor_name,
        format!("review requested by {actor_name}"),
    )
    .await?;
    fire_change(notify, &created, Some(events::REQUEST_CREATED));
    Ok(created)
}

// The announce entry: the caller records "I am reviewing this identity at
// this head" and the row enters the lifecycle already in review, with the
// caller's token as the requester of record. It shares create's ladder
// with the entry state moved: a same-head open request updates in place
// and moves to in review without narrating, a same-head changes_requested
// row refuses with create's message, a same-head approved row returns
// satisfied untouched, and otherwise the caller's refreshable row
// refreshes into in review (an announce is also a self-claim) or a fresh
// row inserts directly in review. No lenses, reviewers, or budget: the
// announce is not an ask.
pub(crate) async fn announce_review_in_pool(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
    head_sha: &str,
    note: Option<&str>,
    actor: &Actor,
    notify: &RequestChangeSink,
) -> Result<ReviewRequest, CommandError> {
    validate_identity(target_key, target_kind)?;
    if let Some(note) = note {
        validate_request_note(note)?;
    }
    if head_sha.trim().is_empty() {
        return Err(invalid_request(
            "A review request needs a non-empty head.",
        ));
    }
    let requester_token_id = requester_key(actor);

    let same_head: Vec<ReviewRequest> = sqlx::query(REQUEST_SAME_HEAD)
        .bind(repo_path)
        .bind(base_sha)
        .bind(target_key)
        .bind(target_kind)
        .bind(requester_token_id)
        .bind(head_sha)
        .fetch_all(pool)
        .await?
        .iter()
        .map(request_from_row)
        .collect::<Result<Vec<_>, _>>()?;

    if let Some(existing) = same_head
        .iter()
        .find(|request| request.status == REQUESTED || request.status == IN_REVIEW)
    {
        // The announce restates the ask; an absent note keeps the stored
        // one. The dedup update narrates nothing, like create's.
        sqlx::query(
            "UPDATE review_requests SET note = COALESCE(?, note), status = ?, \
             updated_at = ? WHERE id = ?",
        )
        .bind(note)
        .bind(IN_REVIEW)
        .bind(now_millis())
        .bind(existing.id)
        .execute(pool)
        .await?;
        let updated = require_request(pool, existing.id).await?;
        fire_change(notify, &updated, None);
        return Ok(updated);
    }
    if same_head
        .iter()
        .any(|request| request.status == CHANGES_REQUESTED)
    {
        return Err(invalid_request(
            "This head was already sent for review and received changes; \
             re-request the review with a new head instead.",
        ));
    }
    if let Some(existing) = same_head
        .iter()
        .find(|request| request.status == APPROVED)
    {
        // The review already happened; the row is returned untouched.
        return Ok(existing.clone());
    }

    // A withdrawn same-head row or no same-head match: the caller's own
    // never-claimed requested row refreshes into in review.
    let refreshable = if same_head.is_empty() {
        sqlx::query(REQUEST_REFRESHABLE)
            .bind(repo_path)
            .bind(base_sha)
            .bind(target_key)
            .bind(target_kind)
            .bind(requester_token_id)
            .fetch_optional(pool)
            .await?
            .map(|row| request_from_row(&row))
            .transpose()?
    } else {
        None
    };
    if let Some(existing) = refreshable {
        // Same omission rule as create's refresh: an absent note keeps the
        // stored one; the head always moves.
        sqlx::query(
            "UPDATE review_requests SET note = COALESCE(?, note), head_sha = ?, \
             status = ?, updated_at = ? WHERE id = ?",
        )
        .bind(note)
        .bind(head_sha)
        .bind(IN_REVIEW)
        .bind(now_millis())
        .bind(existing.id)
        .execute(pool)
        .await?;
        let updated = require_request(pool, existing.id).await?;
        let (actor_kind, actor_name) = event_actor(pool, actor).await?;
        emit_request_event(
            pool,
            events::REVIEW_ANNOUNCED,
            &updated,
            actor_kind,
            &actor_name,
            format!("announced by {actor_name}"),
        )
        .await?;
        fire_change(notify, &updated, Some(events::REVIEW_ANNOUNCED));
        return Ok(updated);
    }

    let now = now_millis();
    let result = sqlx::query(
        "INSERT INTO review_requests (repo_path, base_sha, target_key, target_kind, \
                requester_token_id, status, note, lenses, reviewers, max_rounds, round, \
                head_sha, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, '[]', '[]', ?, 0, ?, ?, ?)",
    )
    .bind(repo_path)
    .bind(base_sha)
    .bind(target_key)
    .bind(target_kind)
    .bind(requester_token_id)
    .bind(IN_REVIEW)
    .bind(note.unwrap_or(""))
    .bind(DEFAULT_MAX_ROUNDS)
    .bind(Some(head_sha))
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    let created = require_request(pool, result.last_insert_rowid()).await?;
    let (actor_kind, actor_name) = event_actor(pool, actor).await?;
    emit_request_event(
        pool,
        events::REVIEW_ANNOUNCED,
        &created,
        actor_kind,
        &actor_name,
        format!("announced by {actor_name}"),
    )
    .await?;
    fire_change(notify, &created, Some(events::REVIEW_ANNOUNCED));
    Ok(created)
}

// Claiming is the only action the named reviewers list gates, and it is
// checked against current token names: a name whose token was deleted can
// never claim. Humans do not claim; their verdict is the signal.
pub(crate) async fn claim_request_in_pool(
    pool: &SqlitePool,
    request_id: i64,
    actor: &Actor,
    notify: &RequestChangeSink,
) -> Result<ReviewRequest, CommandError> {
    let request = require_request(pool, request_id).await?;
    if request.status != REQUESTED {
        return Err(invalid_transition("Only a requested review can be claimed."));
    }
    let token_id = match actor {
        Actor::Agent(token_id) => *token_id,
        Actor::Human => {
            return Err(invalid_transition(
                "Only an agent token can claim a requested review; humans give verdicts.",
            ));
        }
    };
    if !request.reviewers.is_empty() {
        let name: Option<String> =
            sqlx::query_scalar("SELECT name FROM agent_tokens WHERE id = ?")
                .bind(token_id)
                .fetch_optional(pool)
                .await?;
        let named = name
            .as_deref()
            .is_some_and(|name| request.reviewers.iter().any(|reviewer| reviewer == name));
        if !named {
            return Err(invalid_transition(format!(
                "Only the named reviewers ({}) can claim this review.",
                request.reviewers.join(", ")
            )));
        }
    }
    if !set_status_guarded(pool, request.id, REQUESTED, IN_REVIEW).await? {
        return Err(invalid_transition("Only a requested review can be claimed."));
    }
    let claimed = require_request(pool, request.id).await?;
    let (actor_kind, actor_name) = event_actor(pool, actor).await?;
    emit_request_event(
        pool,
        events::REQUEST_CLAIMED,
        &claimed,
        actor_kind,
        &actor_name,
        format!("claimed by {actor_name}"),
    )
    .await?;
    fire_change(notify, &claimed, Some(events::REQUEST_CLAIMED));
    Ok(claimed)
}

// Explicit verdicts close a round and are valid only from in review, for
// any agent token or the human. There is no approved -> changes_requested
// edge: within a round the first blocking verdict wins, so a verdict after
// approved starts nothing. The status precondition rides the UPDATE, so
// two racing writers cannot both apply.
pub(crate) async fn set_request_verdict_in_pool(
    pool: &SqlitePool,
    request_id: i64,
    approve: bool,
    actor: &Actor,
    notify: &RequestChangeSink,
) -> Result<ReviewRequest, CommandError> {
    let request = require_request(pool, request_id).await?;
    if request.status != IN_REVIEW {
        let verdict = if approve { "approved" } else { "sent back" };
        return Err(invalid_transition(format!(
            "A review can be {verdict} only while it is in review; a requested review \
             reaches a verdict through a submission."
        )));
    }
    let status = if approve { APPROVED } else { CHANGES_REQUESTED };
    if !set_status_guarded(pool, request.id, IN_REVIEW, status).await? {
        return Err(invalid_transition(
            "The review already reached a verdict in this round.",
        ));
    }
    let updated = require_request(pool, request.id).await?;
    let (actor_kind, actor_name) = event_actor(pool, actor).await?;
    let verdict = if approve { "approved" } else { "changes requested" };
    emit_request_event(
        pool,
        events::REQUEST_VERDICT,
        &updated,
        actor_kind,
        &actor_name,
        format!("{verdict} by {actor_name}"),
    )
    .await?;
    fire_change(notify, &updated, Some(events::REQUEST_VERDICT));
    Ok(updated)
}

// Withdrawal is the requester's (or a human's) exit from any open state;
// approved and withdrawn requests are settled and cannot be withdrawn.
pub(crate) async fn withdraw_request_in_pool(
    pool: &SqlitePool,
    request_id: i64,
    actor: &Actor,
    notify: &RequestChangeSink,
) -> Result<ReviewRequest, CommandError> {
    let request = require_request(pool, request_id).await?;
    if matches!(request.status.as_str(), APPROVED | WITHDRAWN) {
        return Err(invalid_transition(
            "An approved or withdrawn review is settled and cannot be withdrawn.",
        ));
    }
    let allowed = match actor {
        Actor::Human => true,
        Actor::Agent(token_id) => request.requester_token_id == Some(*token_id),
    };
    if !allowed {
        return Err(invalid_transition(
            "Only the requester (or a human) can withdraw a review request.",
        ));
    }
    set_status(pool, request.id, WITHDRAWN).await?;
    let withdrawn = require_request(pool, request.id).await?;
    let (actor_kind, actor_name) = event_actor(pool, actor).await?;
    emit_request_event(
        pool,
        events::REQUEST_WITHDRAWN,
        &withdrawn,
        actor_kind,
        &actor_name,
        format!("withdrawn by {actor_name}"),
    )
    .await?;
    fire_change(notify, &withdrawn, Some(events::REQUEST_WITHDRAWN));
    Ok(withdrawn)
}

// The requester's reset, the only edge out of changes_requested: the round
// increments and the new head must differ from the one that received the
// changes. A refused re-request past the budget leaves the row untouched;
// needs-human is then changes_requested with round >= max_rounds. An
// expressed note rides the same UPDATE; an absent note keeps the stored one.
pub(crate) async fn re_request_in_pool(
    pool: &SqlitePool,
    request_id: i64,
    head_sha: &str,
    note: Option<&str>,
    actor: &Actor,
    notify: &RequestChangeSink,
) -> Result<ReviewRequest, CommandError> {
    if head_sha.trim().is_empty() {
        return Err(invalid_request("A re-request needs a non-empty head."));
    }
    if let Some(note) = note {
        validate_request_note(note)?;
    }
    let request = require_request(pool, request_id).await?;
    if request.status != CHANGES_REQUESTED {
        return Err(invalid_transition(
            "Only a review with requested changes can be re-requested.",
        ));
    }
    let allowed = match actor {
        Actor::Human => true,
        Actor::Agent(token_id) => request.requester_token_id == Some(*token_id),
    };
    if !allowed {
        return Err(invalid_transition(
            "Only the requester can re-request a review.",
        ));
    }
    if request.round + 1 > request.max_rounds {
        return Err(CommandError::new(
            "round_budget_exceeded",
            format!(
                "The review reached its round budget of {}; a human needs to take over.",
                request.max_rounds
            ),
        ));
    }
    if request.head_sha.as_deref() == Some(head_sha) {
        return Err(invalid_request(
            "A re-request needs a head different from the one that received the changes.",
        ));
    }
    let result = sqlx::query(
        "UPDATE review_requests \
         SET status = ?, round = round + 1, head_sha = ?, note = COALESCE(?, note), updated_at = ? \
         WHERE id = ? AND status = ?",
    )
    .bind(IN_REVIEW)
    .bind(head_sha)
    .bind(note)
    .bind(now_millis())
    .bind(request.id)
    .bind(CHANGES_REQUESTED)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(invalid_transition(
            "Only a review with requested changes can be re-requested.",
        ));
    }
    let updated = require_request(pool, request.id).await?;
    let (actor_kind, actor_name) = event_actor(pool, actor).await?;
    emit_request_event(
        pool,
        events::REQUEST_RE_REQUESTED,
        &updated,
        actor_kind,
        &actor_name,
        format!("re-requested by {actor_name}"),
    )
    .await?;
    fire_change(notify, &updated, Some(events::REQUEST_RE_REQUESTED));
    Ok(updated)
}

// The engine's submission input, called by the ingest path after a
// submission and its findings are stored: every open request on the
// identity advances independently. A requested request is implicitly
// claimed first (round unchanged), then the submission's verdict applies:
// blocking sets changes_requested, clean approves a request now in review.
// Submissions on changes_requested, approved, or withdrawn requests change
// no status, so those requests are not in the open set at all. The
// submitter's own request is skipped only while it is still requested: a
// verdict needs a second party, but an in-review request settles on any
// other delivery, including its requester's (an announce is a self-claim,
// and a claim transfers the working review to the claimer).
pub(crate) async fn observe_submission_in_pool(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
    blocking: bool,
    submitter_token_id: Option<i64>,
    notify: &RequestChangeSink,
) -> Result<(), CommandError> {
    let open: Vec<ReviewRequest> = sqlx::query(REQUEST_OPEN_ON_IDENTITY)
        .bind(repo_path)
        .bind(base_sha)
        .bind(target_key)
        .bind(target_kind)
        .fetch_all(pool)
        .await?
        .iter()
        .map(request_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let status = if blocking { CHANGES_REQUESTED } else { APPROVED };
    for mut request in open {
        if let Some(submitter) = submitter_token_id {
            if request.status == REQUESTED && request.requester_token_id == Some(submitter) {
                continue;
            }
        }
        // A lost race means another writer advanced the request first;
        // first-writer-wins is the sticky-verdict rule, not a failure.
        if !set_status_guarded(pool, request.id, &request.status, status).await? {
            continue;
        }
        request.status = status.to_string();
        // The delivery's own narration rides the ingest's
        // submission_delivered event; the settle pushes without one.
        fire_change(notify, &request, None);
    }
    Ok(())
}

// The human header's request row: the tool face's row fields plus the
// Rust-derived needs-human flag, so the webview renders badges and
// enablement guidance without re-deriving any engine rule.
#[derive(Debug, Serialize)]
pub(crate) struct RequestRow {
    pub(crate) id: i64,
    pub(crate) repo_path: String,
    pub(crate) base_sha: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
    pub(crate) status: String,
    pub(crate) note: String,
    pub(crate) lenses: Vec<String>,
    pub(crate) reviewers: Vec<String>,
    pub(crate) max_rounds: i64,
    pub(crate) round: i64,
    pub(crate) head_sha: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) requester: String,
    pub(crate) age_ms: i64,
    pub(crate) comment_count: i64,
    pub(crate) unresolved_finding_counts: FindingCounts,
    pub(crate) needs_human: bool,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct FindingCounts {
    #[serde(rename = "P0")]
    pub(crate) p0: i64,
    #[serde(rename = "P1")]
    pub(crate) p1: i64,
    #[serde(rename = "P2")]
    pub(crate) p2: i64,
    #[serde(rename = "P3")]
    pub(crate) p3: i64,
}

// The tool face's grouped identity join scoped to one review identity and
// every non-withdrawn request on it, newest first.
const IDENTITY_REQUESTS_QUERY: &str = "SELECT rq.id, rq.repo_path, rq.base_sha, rq.target_key, \
     rq.target_kind, rq.status, rq.note, rq.lenses, rq.reviewers, \
     rq.max_rounds, rq.round, rq.head_sha, rq.created_at, rq.updated_at, \
     t.name AS requester_name, \
     COUNT(c.id) AS comment_count, \
     SUM(CASE WHEN c.severity = 'P0' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p0, \
     SUM(CASE WHEN c.severity = 'P1' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p1, \
     SUM(CASE WHEN c.severity = 'P2' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p2, \
     SUM(CASE WHEN c.severity = 'P3' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p3 \
     FROM review_requests AS rq \
     LEFT JOIN reviews AS rv ON rv.repo_path = rq.repo_path AND rv.base_sha = rq.base_sha \
       AND rv.target_key = rq.target_key AND rv.target_kind = rq.target_kind \
     LEFT JOIN comments AS c ON c.review_id = rv.id \
     LEFT JOIN agent_tokens AS t ON t.id = rq.requester_token_id \
     WHERE rq.repo_path = ?1 AND rq.base_sha = ?2 AND rq.target_key = ?3 AND rq.target_kind = ?4 \
       AND rq.status != 'withdrawn' \
     GROUP BY rq.id ORDER BY rq.created_at DESC, rq.id DESC";

const REQUEST_ROW_BY_ID_QUERY: &str = "SELECT rq.id, rq.repo_path, rq.base_sha, rq.target_key, \
     rq.target_kind, rq.status, rq.note, rq.lenses, rq.reviewers, \
     rq.max_rounds, rq.round, rq.head_sha, rq.created_at, rq.updated_at, \
     t.name AS requester_name, \
     COUNT(c.id) AS comment_count, \
     SUM(CASE WHEN c.severity = 'P0' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p0, \
     SUM(CASE WHEN c.severity = 'P1' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p1, \
     SUM(CASE WHEN c.severity = 'P2' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p2, \
     SUM(CASE WHEN c.severity = 'P3' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p3 \
     FROM review_requests AS rq \
     LEFT JOIN reviews AS rv ON rv.repo_path = rq.repo_path AND rv.base_sha = rq.base_sha \
       AND rv.target_key = rq.target_key AND rv.target_kind = rq.target_kind \
     LEFT JOIN comments AS c ON c.review_id = rv.id \
     LEFT JOIN agent_tokens AS t ON t.id = rq.requester_token_id \
     WHERE rq.id = ?1 \
     GROUP BY rq.id";

fn request_row_from_row(row: &sqlx::sqlite::SqliteRow, now: i64) -> Result<RequestRow, CommandError> {
    let field = |error: sqlx::Error| {
        CommandError::new(
            "persistence",
            format!("A stored review request could not be read: {error}"),
        )
    };
    let parse_list = |column: &str, raw: String| {
        serde_json::from_str::<Vec<String>>(&raw).map_err(|error| {
            CommandError::new(
                "persistence",
                format!("The stored {column} could not be read: {error}"),
            )
        })
    };
    let created_at: i64 = row.try_get("created_at").map_err(field)?;
    let unresolved = |name: &str| -> Result<i64, CommandError> { row.try_get(name).map_err(field) };
    let requester_name: Option<String> = row.try_get("requester_name").map_err(field)?;
    Ok(RequestRow {
        id: row.try_get("id").map_err(field)?,
        repo_path: row.try_get("repo_path").map_err(field)?,
        base_sha: row.try_get("base_sha").map_err(field)?,
        target_key: row.try_get("target_key").map_err(field)?,
        target_kind: row.try_get("target_kind").map_err(field)?,
        status: row.try_get("status").map_err(field)?,
        note: row.try_get("note").map_err(field)?,
        lenses: parse_list("lenses", row.try_get("lenses").map_err(field)?)?,
        reviewers: parse_list("reviewers", row.try_get("reviewers").map_err(field)?)?,
        max_rounds: row.try_get("max_rounds").map_err(field)?,
        round: row.try_get("round").map_err(field)?,
        head_sha: row.try_get("head_sha").map_err(field)?,
        created_at,
        updated_at: row.try_get("updated_at").map_err(field)?,
        requester: requester_name.unwrap_or_else(|| "human".to_string()),
        age_ms: (now - created_at).max(0),
        comment_count: row.try_get("comment_count").map_err(field)?,
        unresolved_finding_counts: FindingCounts {
            p0: unresolved("unresolved_p0")?,
            p1: unresolved("unresolved_p1")?,
            p2: unresolved("unresolved_p2")?,
            p3: unresolved("unresolved_p3")?,
        },
        needs_human: open_request_category(
            &row.try_get::<String, _>("status").map_err(field)?,
            row.try_get("round").map_err(field)?,
            row.try_get("max_rounds").map_err(field)?,
            unresolved("unresolved_p0")?,
            unresolved("unresolved_p1")?,
        ) == Some(CATEGORY_NEEDS_HUMAN),
    })
}

// The open review header's list: every non-withdrawn request on the
// identity, read-only with no Git on the path.
pub(crate) async fn list_requests_in_pool(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
) -> Result<Vec<RequestRow>, CommandError> {
    let now = now_millis();
    let rows = sqlx::query(IDENTITY_REQUESTS_QUERY)
        .bind(repo_path)
        .bind(base_sha)
        .bind(target_key)
        .bind(target_kind)
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|row| request_row_from_row(row, now))
        .collect()
}

// The mutated request's row for a create/update response, carrying the
// same shape the header lists.
pub(crate) async fn request_row_by_id(pool: &SqlitePool, id: i64) -> Result<RequestRow, CommandError> {
    let now = now_millis();
    sqlx::query(REQUEST_ROW_BY_ID_QUERY)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .map(|row| request_row_from_row(&row, now))
        .transpose()?
        .ok_or_else(unknown_request)
}

// The refreshed note on an update shares the create-time note bound, so
// both caller faces refuse identically. A note is optional: an empty ask
// is a general review request.
pub(crate) fn validate_request_note(note: &str) -> Result<(), CommandError> {
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(invalid_request(format!(
            "The note exceeds {MAX_NOTE_CHARS} characters."
        )));
    }
    Ok(())
}

// Writes only the note column (lifecycle statuses stay engine-owned) and
// returns the row's new updated_at.
pub(crate) async fn refresh_request_note(
    pool: &SqlitePool,
    id: i64,
    note: &str,
) -> Result<i64, CommandError> {
    let now = now_millis();
    sqlx::query("UPDATE review_requests SET note = ?, updated_at = ? WHERE id = ?")
        .bind(note)
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(now)
}

// The attention queue's categories; the strings are the tab ids the
// webview filters rows by verbatim, and the backend is their single
// owner: the webview never re-derives membership.
pub(crate) const CATEGORY_REQUESTED: &str = "requested";
pub(crate) const CATEGORY_CHANGES_REQUESTED: &str = "changes_requested";
pub(crate) const CATEGORY_NEEDS_HUMAN: &str = "needs_human";
pub(crate) const CATEGORY_UNRESOLVED_FINDINGS: &str = "unresolved_findings";
pub(crate) const CATEGORY_CHANGED_SINCE_REVIEW: &str = "changed_since_review";
pub(crate) const CATEGORY_RECENT_COMMENTS: &str = "recent_comments";

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct AttentionRow {
    pub(crate) request_id: Option<i64>,
    pub(crate) repo_path: String,
    pub(crate) base_sha: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
    pub(crate) change_label: String,
    pub(crate) requester: String,
    pub(crate) status: String,
    pub(crate) round: i64,
    pub(crate) max_rounds: i64,
    pub(crate) unresolved_p0: i64,
    pub(crate) unresolved_p1: i64,
    pub(crate) category: String,
    pub(crate) needs_human: bool,
    pub(crate) age_basis: i64,
    pub(crate) head_sha: Option<String>,
    // The row's preview-line facts: what happened last on this identity,
    // who did it, when, and how many root threads still sit unresolved.
    // The webview formats the sentence; the backend owns the facts.
    pub(crate) last_activity_at: i64,
    pub(crate) last_activity_kind: String,
    pub(crate) last_activity_actor: String,
    pub(crate) open_thread_count: i64,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct AttentionRepoGroup {
    pub(crate) repo_path: String,
    pub(crate) repo_name: String,
    pub(crate) rows: Vec<AttentionRow>,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub(crate) struct AttentionQueue {
    pub(crate) repos: Vec<AttentionRepoGroup>,
}

// The attention category of one open request from its stored fields and
// its identity's unresolved blocking counts, first match wins: needs-human
// (exhausted rounds, or a P0 sitting on a requested/in-review request),
// then the lifecycle statuses, then P1 findings for an in-review request.
// An in-review request with no trigger returns None: it was claimed and
// nothing needs attention yet.
fn open_request_category(
    status: &str,
    round: i64,
    max_rounds: i64,
    unresolved_p0: i64,
    unresolved_p1: i64,
) -> Option<&'static str> {
    if (status == CHANGES_REQUESTED && round >= max_rounds)
        || ((status == REQUESTED || status == IN_REVIEW) && unresolved_p0 > 0)
    {
        return Some(CATEGORY_NEEDS_HUMAN);
    }
    match status {
        REQUESTED => Some(CATEGORY_REQUESTED),
        CHANGES_REQUESTED => Some(CATEGORY_CHANGES_REQUESTED),
        IN_REVIEW => (unresolved_p1 > 0).then_some(CATEGORY_UNRESOLVED_FINDINGS),
        _ => None,
    }
}

// One review request with its identity-joined triage counts. Comments ride
// the review identity, so every request on the same identity sees the same
// counts; the requester name resolves at query time (a deleted token reads
// as human, matching the tool face).
struct RequestTriage {
    id: i64,
    repo_path: String,
    base_sha: String,
    target_key: String,
    target_kind: String,
    requester: String,
    status: String,
    round: i64,
    max_rounds: i64,
    head_sha: Option<String>,
    created_at: i64,
    updated_at: i64,
    unresolved_p0: i64,
    unresolved_p1: i64,
}

// Every stored request with its identity's blocking counts, newest first.
const ATTENTION_REQUESTS_QUERY: &str = "SELECT rq.id, rq.repo_path, rq.base_sha, rq.target_key, \
     rq.target_kind, rq.status, rq.max_rounds, rq.round, rq.head_sha, rq.created_at, rq.updated_at, \
     t.name AS requester_name, \
     SUM(CASE WHEN c.severity = 'P0' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p0, \
     SUM(CASE WHEN c.severity = 'P1' AND c.resolved_at IS NULL THEN 1 ELSE 0 END) AS unresolved_p1 \
     FROM review_requests AS rq \
     LEFT JOIN reviews AS rv ON rv.repo_path = rq.repo_path AND rv.base_sha = rq.base_sha \
       AND rv.target_key = rq.target_key AND rv.target_kind = rq.target_kind \
     LEFT JOIN comments AS c ON c.review_id = rv.id \
     LEFT JOIN agent_tokens AS t ON t.id = rq.requester_token_id \
     GROUP BY rq.id ORDER BY rq.updated_at DESC, rq.id DESC";

// Identities carrying unresolved P0/P1 comments with no open request: the
// store-only late-findings path that survives after a review settles (or
// when no request was ever created). Identities with any request are
// excluded outright - the request-derived rows below cover them.
const ATTENTION_FREE_FINDINGS_QUERY: &str = "SELECT rv.repo_path, rv.base_sha, rv.target_key, \
     rv.target_kind, \
     SUM(CASE WHEN c.severity = 'P0' THEN 1 ELSE 0 END) AS unresolved_p0, \
     SUM(CASE WHEN c.severity = 'P1' THEN 1 ELSE 0 END) AS unresolved_p1, \
     MAX(c.created_at) AS last_finding_at \
     FROM reviews AS rv \
     JOIN comments AS c ON c.review_id = rv.id \
       AND c.severity IN ('P0', 'P1') AND c.resolved_at IS NULL \
     WHERE NOT EXISTS ( \
       SELECT 1 FROM review_requests AS rq \
       WHERE rq.repo_path = rv.repo_path AND rq.base_sha = rv.base_sha \
         AND rq.target_key = rv.target_key AND rq.target_kind = rv.target_kind) \
     GROUP BY rv.repo_path, rv.base_sha, rv.target_key, rv.target_kind";

// Request-less identities' newest comment: the recent_comments pool's
// candidates, any severity, discussion included. The free-findings pass
// keeps first-match precedence over this one.
const ATTENTION_RECENT_COMMENTS_QUERY: &str = "SELECT rv.repo_path, rv.base_sha, rv.target_key, \
     rv.target_kind, MAX(c.created_at) AS last_comment_at \
     FROM reviews AS rv \
     JOIN comments AS c ON c.review_id = rv.id \
     WHERE NOT EXISTS ( \
       SELECT 1 FROM review_requests AS rq \
       WHERE rq.repo_path = rv.repo_path AND rq.base_sha = rv.base_sha \
         AND rq.target_key = rv.target_key AND rq.target_kind = rv.target_kind) \
     GROUP BY rv.repo_path, rv.base_sha, rv.target_key, rv.target_kind";

// Every identity's last activity, one winner per source (comment,
// submission, request); the queue pass picks the newest of what an
// identity has. Human comments store the author name the review surface
// shows ("you"), so preview lines narrate it the same way.
const ATTENTION_ACTIVITY_QUERY: &str = "SELECT repo_path, base_sha, target_key, target_kind, \
     kind, actor, created_at FROM ( \
     SELECT rv.repo_path, rv.base_sha, rv.target_key, rv.target_kind, \
            'comment' AS kind, c.author_name AS actor, c.created_at AS created_at, \
            ROW_NUMBER() OVER (PARTITION BY rv.repo_path, rv.base_sha, rv.target_key, rv.target_kind \
                               ORDER BY c.created_at DESC, c.id DESC) AS rn \
     FROM comments c JOIN reviews rv ON rv.id = c.review_id \
     UNION ALL \
     SELECT rv.repo_path, rv.base_sha, rv.target_key, rv.target_kind, \
            'submission' AS kind, s.agent_name AS actor, s.created_at AS created_at, \
            ROW_NUMBER() OVER (PARTITION BY rv.repo_path, rv.base_sha, rv.target_key, rv.target_kind \
                               ORDER BY s.created_at DESC, s.id DESC) AS rn \
     FROM submissions s JOIN reviews rv ON rv.id = s.review_id \
     UNION ALL \
     SELECT rq.repo_path, rq.base_sha, rq.target_key, rq.target_kind, \
            'request' AS kind, COALESCE(t.name, 'human') AS actor, rq.updated_at AS created_at, \
            ROW_NUMBER() OVER (PARTITION BY rq.repo_path, rq.base_sha, rq.target_key, rq.target_kind \
                               ORDER BY rq.updated_at DESC, rq.id DESC) AS rn \
     FROM review_requests rq LEFT JOIN agent_tokens t ON t.id = rq.requester_token_id) \
     AS activity WHERE rn = 1";

// Unresolved root comment threads per identity: the preview line's
// "threads open" fact.
const ATTENTION_OPEN_THREADS_QUERY: &str = "SELECT rv.repo_path, rv.base_sha, rv.target_key, \
     rv.target_kind, COUNT(*) AS open_threads \
     FROM comments c JOIN reviews rv ON rv.id = c.review_id \
     WHERE c.parent_id IS NULL AND c.resolved_at IS NULL \
     GROUP BY rv.repo_path, rv.base_sha, rv.target_key, rv.target_kind";

#[derive(Debug, Clone, Default)]
struct IdentityActivity {
    last_activity_at: i64,
    last_activity_kind: String,
    last_activity_actor: String,
    open_thread_count: i64,
}

type ActivityMap = std::collections::HashMap<IdentityKey, IdentityActivity>;

fn activity_rank(kind: &str) -> i8 {
    match kind {
        "comment" => 2,
        "submission" => 1,
        "request" => 0,
        _ => -1,
    }
}

// The preview facts for every identity with any comment, submission, or
// request activity; a row-producing identity always has at least one of
// these, so map misses stay theoretical.
async fn identity_activity_in_pool(pool: &SqlitePool) -> Result<ActivityMap, sqlx::Error> {
    let mut map: ActivityMap = ActivityMap::new();
    for row in sqlx::query(ATTENTION_ACTIVITY_QUERY).fetch_all(pool).await? {
        let key: IdentityKey = (
            row.try_get("repo_path")?,
            row.try_get("base_sha")?,
            row.try_get("target_key")?,
            row.try_get("target_kind")?,
        );
        let at: i64 = row.try_get("created_at")?;
        let kind: String = row.try_get("kind")?;
        let actor: String = row.try_get("actor")?;
        let entry = map.entry(key).or_default();
        let newer = at > entry.last_activity_at
            || (at == entry.last_activity_at && activity_rank(&kind) > activity_rank(&entry.last_activity_kind));
        if newer {
            entry.last_activity_at = at;
            entry.last_activity_kind = kind;
            entry.last_activity_actor = actor;
        }
    }
    for row in sqlx::query(ATTENTION_OPEN_THREADS_QUERY).fetch_all(pool).await? {
        let key: IdentityKey = (
            row.try_get("repo_path")?,
            row.try_get("base_sha")?,
            row.try_get("target_key")?,
            row.try_get("target_kind")?,
        );
        if let Some(entry) = map.get_mut(&key) {
            entry.open_thread_count = row.try_get("open_threads")?;
        }
    }
    Ok(map)
}

// The seen watermark's comparison time: the referenced event's created_at,
// or the epoch for the not-yet-marked cursor (0), which is the intended
// first-run flood. Events are append-only with no pruning, so a marked id
// always resolves; a miss falls back to the epoch, which only ever
// over-fires Recent comments.
async fn activity_watermark_time(pool: &SqlitePool) -> Result<i64, CommandError> {
    let seen_id = crate::store::get_settings_in_pool(pool).await?.activity_seen_id;
    if seen_id <= 0 {
        return Ok(0);
    }
    Ok(sqlx::query_scalar::<_, i64>("SELECT created_at FROM events WHERE id = ?")
        .bind(seen_id)
        .fetch_optional(pool)
        .await?
        .unwrap_or(0))
}

type IdentityKey = (String, String, String, String);

fn identity_of(request: &RequestTriage) -> IdentityKey {
    (
        request.repo_path.clone(),
        request.base_sha.clone(),
        request.target_key.clone(),
        request.target_kind.clone(),
    )
}

fn is_open(status: &str) -> bool {
    matches!(status, REQUESTED | IN_REVIEW | CHANGES_REQUESTED)
}

// The recorded retrospection head of a request's surface; head-kind
// identities have no surface record (commits record nothing), so they
// never read as moved.
fn recorded_head<'a>(
    heads: &'a [crate::retrospection::SurfaceHead],
    request: &RequestTriage,
) -> Option<&'a crate::retrospection::SurfaceHead> {
    if request.target_kind != "worktree" {
        return None;
    }
    crate::retrospection::recorded_worktree_head(heads, &request.repo_path, &request.target_key)
}

fn head_moved(head: &crate::retrospection::SurfaceHead, request: &RequestTriage) -> bool {
    surface_head_moved(Some(head), request.head_sha.as_deref())
}

// The changed-since-review comparison the attention queue and the portal
// listing share: a surface whose recorded head differs from the reviewed
// head moved, and an unrecorded request head never reads as moved.
pub(crate) fn surface_head_moved(
    recorded: Option<&crate::retrospection::SurfaceHead>,
    head_sha: Option<&str>,
) -> bool {
    match (recorded, head_sha) {
        (Some(head), Some(sha)) => sha != head.head_sha,
        _ => false,
    }
}

pub(crate) fn short_sha(sha: &str) -> String {
    let hex = sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit());
    if hex { sha[..7].to_string() } else { sha.to_string() }
}

// Display label for any identity row: the recorded branch label when the
// surface was retrospected, the worktree folder name as a fallback, and a
// short sha for head-kind identities.
pub(crate) fn identity_change_label(
    target_kind: &str,
    target_key: &str,
    recorded: Option<&crate::retrospection::SurfaceHead>,
) -> String {
    if target_kind == "head" {
        return short_sha(target_key);
    }
    match recorded {
        Some(head) if !head.label.is_empty() => head.label.clone(),
        _ => target_key
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(target_key)
            .to_string(),
    }
}

fn change_label(request: &RequestTriage, recorded: Option<&crate::retrospection::SurfaceHead>) -> String {
    identity_change_label(&request.target_kind, &request.target_key, recorded)
}

fn request_row(
    request: &RequestTriage,
    recorded: Option<&crate::retrospection::SurfaceHead>,
    category: &str,
    activity: &ActivityMap,
) -> AttentionRow {
    let found = activity
        .get(&identity_of(request))
        .cloned()
        .unwrap_or_default();
    AttentionRow {
        request_id: Some(request.id),
        repo_path: request.repo_path.clone(),
        base_sha: request.base_sha.clone(),
        target_key: request.target_key.clone(),
        target_kind: request.target_kind.clone(),
        change_label: change_label(request, recorded),
        requester: request.requester.clone(),
        status: request.status.clone(),
        round: request.round,
        max_rounds: request.max_rounds,
        unresolved_p0: request.unresolved_p0,
        unresolved_p1: request.unresolved_p1,
        category: category.to_string(),
        needs_human: category == CATEGORY_NEEDS_HUMAN,
        // A requested row ages from its creation; everything else ages
        // from its last move.
        age_basis: if request.status == REQUESTED {
            request.created_at
        } else {
            request.updated_at
        },
        head_sha: recorded.map(|head| head.head_sha.clone()).or_else(|| {
            (request.target_kind == "head").then(|| request.target_key.clone())
        }),
        last_activity_at: found.last_activity_at,
        last_activity_kind: found.last_activity_kind,
        last_activity_actor: found.last_activity_actor,
        open_thread_count: found.open_thread_count,
    }
}

fn sort_rows(rows: &mut Vec<AttentionRow>) {
    rows.sort_by(|left, right| {
        right
            .unresolved_p0
            .cmp(&left.unresolved_p0)
            .then(left.age_basis.cmp(&right.age_basis))
            .then(left.request_id.cmp(&right.request_id))
    });
}

// The cross-repo attention queue: one read-only store pass with no Git on
// the path. Open requests produce one row each; identities whose requests
// all settled (or that never had one) surface their late findings, a
// recorded retrospection head differing from a request's head reads as
// changed since review, and request-less identities whose newest comment
// postdates the seen watermark queue as recent comments. Categories are
// first-match in that order. Rows reflect the last retrospection pass.
pub(crate) async fn list_attention_in_pool(
    pool: &SqlitePool,
) -> Result<AttentionQueue, CommandError> {
    let mut requests = Vec::new();
    for row in sqlx::query(ATTENTION_REQUESTS_QUERY).fetch_all(pool).await? {
        requests.push(RequestTriage {
            id: row.try_get("id")?,
            repo_path: row.try_get("repo_path")?,
            base_sha: row.try_get("base_sha")?,
            target_key: row.try_get("target_key")?,
            target_kind: row.try_get("target_kind")?,
            requester: row
                .try_get::<Option<String>, _>("requester_name")?
                .unwrap_or_else(|| "human".to_string()),
            status: row.try_get("status")?,
            round: row.try_get("round")?,
            max_rounds: row.try_get("max_rounds")?,
            head_sha: row.try_get("head_sha")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            unresolved_p0: row.try_get("unresolved_p0")?,
            unresolved_p1: row.try_get("unresolved_p1")?,
        });
    }
    let heads = crate::retrospection::recorded_surface_heads_in_pool(pool).await?;
    let activity = identity_activity_in_pool(pool).await?;
    let watermark_at = activity_watermark_time(pool).await?;

    let mut rows_by_repo: std::collections::BTreeMap<String, Vec<AttentionRow>> =
        std::collections::BTreeMap::new();
    let mut open_identities: std::collections::HashSet<IdentityKey> =
        std::collections::HashSet::new();
    for request in &requests {
        if !is_open(&request.status) {
            continue;
        }
        open_identities.insert(identity_of(request));
        let recorded = recorded_head(&heads, request);
        let category = open_request_category(
            &request.status,
            request.round,
            request.max_rounds,
            request.unresolved_p0,
            request.unresolved_p1,
        )
        .or_else(|| {
            recorded
                .is_some_and(|head| head_moved(head, request))
                .then_some(CATEGORY_CHANGED_SINCE_REVIEW)
        });
        if let Some(category) = category {
            rows_by_repo
                .entry(request.repo_path.clone())
                .or_default()
                .push(request_row(request, recorded, category, &activity));
        }
    }

    // Settled identities group on their latest request: findings come from
    // the identity's counts, the changed baseline from the newest approved
    // request's head.
    let mut by_identity: std::collections::BTreeMap<IdentityKey, Vec<&RequestTriage>> =
        std::collections::BTreeMap::new();
    for request in &requests {
        if is_open(&request.status) {
            continue;
        }
        by_identity.entry(identity_of(request)).or_default().push(request);
    }
    for (identity, group) in by_identity {
        if open_identities.contains(&identity) {
            continue;
        }
        let latest = group[0];
        let recorded = recorded_head(&heads, latest);
        let blocking = latest.unresolved_p0 + latest.unresolved_p1;
        let latest_approved = group.iter().find(|request| request.status == APPROVED);
        let moved_at = latest_approved
            .and_then(|approved| recorded_head(&heads, approved))
            .filter(|head| latest_approved.is_some_and(|approved| head_moved(head, approved)))
            .map(|head| head.last_seen_at);
        let (category, moved_at) = if blocking > 0 {
            (CATEGORY_UNRESOLVED_FINDINGS, None)
        } else if let Some(moved_at) = moved_at {
            (CATEGORY_CHANGED_SINCE_REVIEW, Some(moved_at))
        } else {
            continue;
        };
        let mut row = request_row(latest, recorded, category, &activity);
        // A moved surface ages from when the retrospection pass recorded
        // the new head, not from the request's last move.
        if let Some(moved_at) = moved_at {
            row.age_basis = moved_at;
        }
        rows_by_repo
            .entry(latest.repo_path.clone())
            .or_default()
            .push(row);
    }

    // Identities with no request at all: only their unresolved blocking
    // findings can put them on the queue. Their keys are remembered so the
    // recent-comments pass below never double-claims them (first match).
    let mut claimed_free: std::collections::HashSet<IdentityKey> =
        std::collections::HashSet::new();
    for row in sqlx::query(ATTENTION_FREE_FINDINGS_QUERY)
        .fetch_all(pool)
        .await?
    {
        let repo_path: String = row.try_get("repo_path")?;
        let base_sha: String = row.try_get("base_sha")?;
        let target_key: String = row.try_get("target_key")?;
        let target_kind: String = row.try_get("target_kind")?;
        let probe = RequestTriage {
            id: 0,
            repo_path: repo_path.clone(),
            base_sha: base_sha.clone(),
            target_key: target_key.clone(),
            target_kind: target_kind.clone(),
            requester: String::new(),
            status: String::new(),
            round: 0,
            max_rounds: 0,
            head_sha: None,
            created_at: 0,
            updated_at: 0,
            unresolved_p0: row.try_get("unresolved_p0")?,
            unresolved_p1: row.try_get("unresolved_p1")?,
        };
        let recorded = recorded_head(&heads, &probe);
        claimed_free.insert(identity_of(&probe));
        let found = activity
            .get(&identity_of(&probe))
            .cloned()
            .unwrap_or_default();
        rows_by_repo.entry(repo_path.clone()).or_default().push(AttentionRow {
            request_id: None,
            repo_path,
            base_sha,
            target_key,
            target_kind,
            change_label: change_label(&probe, recorded),
            requester: String::new(),
            status: String::new(),
            round: 0,
            max_rounds: 0,
            unresolved_p0: probe.unresolved_p0,
            unresolved_p1: probe.unresolved_p1,
            category: CATEGORY_UNRESOLVED_FINDINGS.to_string(),
            needs_human: false,
            age_basis: row.try_get("last_finding_at")?,
            head_sha: recorded.map(|head| head.head_sha.clone()),
            last_activity_at: found.last_activity_at,
            last_activity_kind: found.last_activity_kind,
            last_activity_actor: found.last_activity_actor,
            open_thread_count: found.open_thread_count,
        });
    }

    // Request-less identities whose newest comment postdates the seen
    // watermark join the queue as recent_comments: any severity, discussion
    // included, drained for good once Mark all seen passes them (the queue
    // stays a queue; settled work moves on, never back in).
    for row in sqlx::query(ATTENTION_RECENT_COMMENTS_QUERY)
        .fetch_all(pool)
        .await?
    {
        let repo_path: String = row.try_get("repo_path")?;
        let base_sha: String = row.try_get("base_sha")?;
        let target_key: String = row.try_get("target_key")?;
        let target_kind: String = row.try_get("target_kind")?;
        let key: IdentityKey =
            (repo_path.clone(), base_sha.clone(), target_key.clone(), target_kind.clone());
        if claimed_free.contains(&key) {
            continue;
        }
        let last_comment_at: i64 = row.try_get("last_comment_at")?;
        if last_comment_at <= watermark_at {
            continue;
        }
        let probe = RequestTriage {
            id: 0,
            repo_path: repo_path.clone(),
            base_sha: base_sha.clone(),
            target_key: target_key.clone(),
            target_kind: target_kind.clone(),
            requester: String::new(),
            status: String::new(),
            round: 0,
            max_rounds: 0,
            head_sha: None,
            created_at: 0,
            updated_at: 0,
            unresolved_p0: 0,
            unresolved_p1: 0,
        };
        let recorded = recorded_head(&heads, &probe);
        let found = activity.get(&key).cloned().unwrap_or_default();
        rows_by_repo.entry(repo_path.clone()).or_default().push(AttentionRow {
            request_id: None,
            repo_path,
            base_sha,
            target_key,
            target_kind,
            change_label: change_label(&probe, recorded),
            requester: String::new(),
            status: String::new(),
            round: 0,
            max_rounds: 0,
            unresolved_p0: 0,
            unresolved_p1: 0,
            category: CATEGORY_RECENT_COMMENTS.to_string(),
            needs_human: false,
            age_basis: last_comment_at,
            head_sha: recorded.map(|head| head.head_sha.clone()),
            last_activity_at: found.last_activity_at,
            last_activity_kind: found.last_activity_kind,
            last_activity_actor: found.last_activity_actor,
            open_thread_count: found.open_thread_count,
        });
    }

    let repo_names: std::collections::HashMap<String, String> =
        sqlx::query("SELECT path, name FROM repos")
            .fetch_all(pool)
            .await?
            .iter()
            .map(|row| Ok((row.try_get("path")?, row.try_get("name")?)))
            .collect::<Result<_, sqlx::Error>>()?;
    let mut repos = Vec::new();
    for (repo_path, mut rows) in rows_by_repo {
        sort_rows(&mut rows);
        let repo_name = repo_names
            .get(&repo_path)
            .cloned()
            .unwrap_or_else(|| {
                repo_path
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&repo_path)
                    .to_string()
            });
        repos.push(AttentionRepoGroup {
            repo_path,
            repo_name,
            rows,
        });
    }
    Ok(AttentionQueue { repos })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{seed_repo, test_pool};
    use std::sync::{Arc, Mutex};

    const REPO: &str = "/demo";
    const BASE: &str = "base";
    const KEY: &str = "/demo";
    const KIND: &str = "worktree";

    fn noop_notify() -> RequestChangeSink {
        Arc::new(|_| {})
    }

    fn recording_notify() -> (RequestChangeSink, Arc<Mutex<Vec<RequestChange>>>) {
        let received: Arc<Mutex<Vec<RequestChange>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        (
            Arc::new(move |change| sink.lock().unwrap().push(change)),
            received,
        )
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

    async fn agent_token(pool: &SqlitePool, name: &str) -> i64 {
        crate::agents::create_agent_token_in_pool(pool, name)
            .await
            .unwrap()
            .token
            .id
    }

    async fn create(pool: &SqlitePool, actor: &Actor, head: &str) -> ReviewRequest {
        create_request_in_pool(pool, REPO, BASE, KEY, KIND, &draft(head), actor, &noop_notify())
            .await
            .unwrap()
    }

    async fn get(pool: &SqlitePool, id: i64) -> ReviewRequest {
        require_request(pool, id).await.unwrap()
    }

    async fn claim(pool: &SqlitePool, request_id: i64, actor: &Actor) -> ReviewRequest {
        claim_request_in_pool(pool, request_id, actor, &noop_notify())
            .await
            .unwrap()
    }

    async fn verdict(
        pool: &SqlitePool,
        request_id: i64,
        approve: bool,
        actor: &Actor,
    ) -> ReviewRequest {
        set_request_verdict_in_pool(pool, request_id, approve, actor, &noop_notify())
            .await
            .unwrap()
    }

    async fn withdraw(pool: &SqlitePool, request_id: i64, actor: &Actor) -> ReviewRequest {
        withdraw_request_in_pool(pool, request_id, actor, &noop_notify())
            .await
            .unwrap()
    }

    async fn re_request(
        pool: &SqlitePool,
        request_id: i64,
        head: &str,
        actor: &Actor,
    ) -> ReviewRequest {
        re_request_in_pool(pool, request_id, head, None, actor, &noop_notify())
            .await
            .unwrap()
    }

    async fn announce(
        pool: &SqlitePool,
        head: &str,
        note: Option<&str>,
        actor: &Actor,
    ) -> ReviewRequest {
        announce_review_in_pool(pool, REPO, BASE, KEY, KIND, head, note, actor, &noop_notify())
            .await
            .unwrap()
    }

    async fn observe(pool: &SqlitePool, blocking: bool) {
        observe_submission_in_pool(pool, REPO, BASE, KEY, KIND, blocking, None, &noop_notify())
            .await
            .unwrap()
    }

    async fn token_name(pool: &SqlitePool, id: i64) -> String {
        sqlx::query_scalar("SELECT name FROM agent_tokens WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_stores_a_requested_row_and_round_trips_the_parsed_lists() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let reviewer = agent_token(&pool, "reviewer-bot").await;
        let requester = agent_token(&pool, "coder-bot").await;
        let mut payload = draft("head-1");
        payload.lenses = vec!["security".into(), "tests".into()];
        payload.reviewers = vec![token_name(&pool, reviewer).await];
        payload.max_rounds = Some(3);
        let request = create_request_in_pool(
            &pool,
            REPO,
            BASE,
            KEY,
            KIND,
            &payload,
            &Actor::Agent(requester),
            &noop_notify(),
        )
        .await
        .unwrap();
        assert_eq!(request.status, REQUESTED);
        assert_eq!(request.round, 0);
        assert_eq!(request.max_rounds, 3);
        assert_eq!(request.lenses, ["security", "tests"]);
        assert_eq!(request.reviewers.len(), 1);
        assert_eq!(request.repo_path, REPO);
        assert_eq!(request.base_sha, BASE);
        assert_eq!(request.target_key, KEY);
        assert_eq!(request.target_kind, KIND);
        assert_eq!(request.requester_token_id, Some(requester));
        assert_eq!(request.head_sha.as_deref(), Some("head-1"));
        let reloaded = get(&pool, request.id).await;
        assert_eq!(reloaded, request);
    }

    #[tokio::test]
    async fn every_legal_transition_runs_for_agents_and_humans() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let reviewer = agent_token(&pool, "reviewer-bot").await;
        let other = agent_token(&pool, "other-bot").await;

        // Agent claim and agent verdicts on an open pickup.
        let request = create(&pool, &Actor::Agent(coder), "head-1").await;
        let claimed = claim(&pool, request.id, &Actor::Agent(other)).await;
        assert_eq!(claimed.status, IN_REVIEW);
        let approved = verdict(&pool, request.id, true, &Actor::Agent(other)).await;
        assert_eq!(approved.status, APPROVED);
        assert_eq!(approved.round, 0);

        // Requester re-request after changes, then another verdict round.
        let request = create(&pool, &Actor::Agent(coder), "head-2").await;
        claim(&pool, request.id, &Actor::Agent(reviewer)).await;
        let changed = verdict(&pool, request.id, false, &Actor::Human).await;
        assert_eq!(changed.status, CHANGES_REQUESTED);
        let again = re_request(&pool, request.id, "head-3", &Actor::Agent(coder)).await;
        assert_eq!(again.status, IN_REVIEW);
        assert_eq!(again.round, 1);
        assert_eq!(again.head_sha.as_deref(), Some("head-3"));
        let rejected = verdict(&pool, request.id, false, &Actor::Agent(reviewer)).await;
        assert_eq!(rejected.status, CHANGES_REQUESTED);
        let withdrawn = withdraw(&pool, request.id, &Actor::Agent(coder)).await;
        assert_eq!(withdrawn.status, WITHDRAWN);

        // Human-keyed creation and human withdraw of any open request.
        let human_request = create(&pool, &Actor::Human, "head-4").await;
        assert_eq!(human_request.requester_token_id, None);
        let human_withdrawn = withdraw(&pool, human_request.id, &Actor::Human).await;
        assert_eq!(human_withdrawn.status, WITHDRAWN);
        let agent_request = create(&pool, &Actor::Agent(coder), "head-5").await;
        let human_takes_over = withdraw(&pool, agent_request.id, &Actor::Human).await;
        assert_eq!(human_takes_over.status, WITHDRAWN);
    }

    #[tokio::test]
    async fn human_re_request_covers_agent_keyed_and_human_keyed_rows() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;

        // Requester-only includes the human: a human re-requests any open
        // changes_requested row, agent-keyed or not.
        let agent_request = create(&pool, &Actor::Agent(coder), "head-1").await;
        observe(&pool, true).await;
        assert_eq!(get(&pool, agent_request.id).await.status, CHANGES_REQUESTED);
        let reset = re_request(&pool, agent_request.id, "head-2", &Actor::Human).await;
        assert_eq!(reset.status, IN_REVIEW);
        assert_eq!(reset.round, 1);
        assert_eq!(reset.head_sha.as_deref(), Some("head-2"));

        // The same human parity on a human-keyed row.
        let human_request = create(&pool, &Actor::Human, "head-3").await;
        observe(&pool, true).await;
        assert_eq!(get(&pool, human_request.id).await.status, CHANGES_REQUESTED);
        let own_reset = re_request(&pool, human_request.id, "head-4", &Actor::Human).await;
        assert_eq!(own_reset.status, IN_REVIEW);
        assert_eq!(own_reset.round, 1);
        assert_eq!(own_reset.head_sha.as_deref(), Some("head-4"));
    }

    #[tokio::test]
    async fn illegal_transitions_are_refused_with_the_allowed_actor() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let reviewer = agent_token(&pool, "reviewer-bot").await;
        let outsider = agent_token(&pool, "outsider-bot").await;

        // Explicit verdicts from requested are refused for both actors.
        let request = create(&pool, &Actor::Agent(coder), "head-1").await;
        assert_eq!(
            set_request_verdict_in_pool(
                &pool, request.id, true, &Actor::Agent(reviewer), &noop_notify(),
            )
            .await
            .unwrap_err()
            .code,
            "invalid_transition"
        );
        assert_eq!(
            set_request_verdict_in_pool(&pool, request.id, false, &Actor::Human, &noop_notify())
                .await
                .unwrap_err()
                .code,
            "invalid_transition"
        );

        // The named reviewers list gates only the claim: an unnamed token
        // and the human are both refused; the named reviewer claims. The
        // request is human-keyed so it cannot be absorbed by the
        // never-claimed refresh of the coder's earlier row.
        let mut named = draft("head-2");
        named.reviewers = vec![token_name(&pool, reviewer).await];
        let gated = create_request_in_pool(
            &pool, REPO, BASE, KEY, KIND, &named, &Actor::Human, &noop_notify(),
        )
        .await
        .unwrap();
        assert_eq!(
            claim_request_in_pool(&pool, gated.id, &Actor::Agent(outsider), &noop_notify())
                .await
                .unwrap_err()
                .code,
            "invalid_transition"
        );
        assert_eq!(
            claim_request_in_pool(&pool, gated.id, &Actor::Human, &noop_notify())
                .await
                .unwrap_err()
                .code,
            "invalid_transition"
        );
        let claimed = claim(&pool, gated.id, &Actor::Agent(reviewer)).await;
        assert_eq!(claimed.status, IN_REVIEW);

        // Re-request is requester-only and only from changes_requested.
        let refused = re_request_in_pool(
            &pool,
            gated.id,
            "head-9",
            None,
            &Actor::Agent(outsider),
            &noop_notify(),
        )
        .await;
        assert_eq!(refused.unwrap_err().code, "invalid_transition");
        verdict(&pool, gated.id, false, &Actor::Human).await;
        let refused = re_request_in_pool(
            &pool,
            gated.id,
            "head-9",
            None,
            &Actor::Agent(outsider),
            &noop_notify(),
        )
        .await;
        assert_eq!(refused.unwrap_err().code, "invalid_transition");

        // Non-requester withdrawal is refused, including on human-keyed
        // rows whose requester is nobody.
        let human_request = create(&pool, &Actor::Human, "head-10").await;
        assert_eq!(
            withdraw_request_in_pool(
                &pool, human_request.id, &Actor::Agent(coder), &noop_notify(),
            )
            .await
            .unwrap_err()
            .code,
            "invalid_transition"
        );

        // Verdicts on and transitions out of withdrawn are refused.
        let withdrawn = withdraw(&pool, human_request.id, &Actor::Human).await;
        assert_eq!(withdrawn.status, WITHDRAWN);
        for attempt in [
            claim_request_in_pool(&pool, withdrawn.id, &Actor::Agent(reviewer), &noop_notify())
                .await,
            set_request_verdict_in_pool(&pool, withdrawn.id, true, &Actor::Human, &noop_notify())
                .await,
            set_request_verdict_in_pool(&pool, withdrawn.id, false, &Actor::Human, &noop_notify())
                .await,
            withdraw_request_in_pool(&pool, withdrawn.id, &Actor::Human, &noop_notify()).await,
            re_request_in_pool(&pool, withdrawn.id, "head-11", None, &Actor::Human, &noop_notify()).await,
        ] {
            assert_eq!(attempt.unwrap_err().code, "invalid_transition");
        }

        // Claiming an already-claimed request and withdrawing an approved
        // one are refused.
        let request = create(&pool, &Actor::Agent(outsider), "head-12").await;
        claim(&pool, request.id, &Actor::Agent(reviewer)).await;
        assert_eq!(
            claim_request_in_pool(&pool, request.id, &Actor::Agent(reviewer), &noop_notify())
                .await
                .unwrap_err()
                .code,
            "invalid_transition"
        );
        let approved = verdict(&pool, request.id, true, &Actor::Human).await;
        assert_eq!(
            withdraw_request_in_pool(&pool, approved.id, &Actor::Human, &noop_notify())
                .await
                .unwrap_err()
                .code,
            "invalid_transition"
        );
    }

    #[tokio::test]
    async fn sticky_verdicts_hold_within_a_round() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;

        // A blocking submission on requested implicitly claims and applies
        // the verdict; the requester's re-request resets and increments.
        let request = create(&pool, &Actor::Agent(coder), "head-1").await;
        observe(&pool, true).await;
        assert_eq!(get(&pool, request.id).await.status, CHANGES_REQUESTED);
        let again = re_request(&pool, request.id, "head-2", &Actor::Agent(coder)).await;
        assert_eq!(again.status, IN_REVIEW);
        assert_eq!(again.round, 1);
        let approved = verdict(&pool, request.id, true, &Actor::Human).await;
        assert_eq!(approved.status, APPROVED);
        // A blocking submission after approved leaves the badge.
        observe(&pool, true).await;
        assert_eq!(get(&pool, request.id).await.status, APPROVED);

        // Clean and blocking submissions never reset sticky
        // changes_requested; only the re-request resets it.
        let sticky = create(&pool, &Actor::Agent(coder), "head-3").await;
        observe(&pool, true).await;
        assert_eq!(get(&pool, sticky.id).await.status, CHANGES_REQUESTED);
        observe(&pool, false).await;
        observe(&pool, true).await;
        assert_eq!(get(&pool, sticky.id).await.status, CHANGES_REQUESTED);
        let reset = re_request(&pool, sticky.id, "head-4", &Actor::Agent(coder)).await;
        assert_eq!(reset.status, IN_REVIEW);
        assert_eq!(reset.round, 1);

        // A clean submission from in_review approves.
        observe(&pool, false).await;
        assert_eq!(get(&pool, sticky.id).await.status, APPROVED);

        // A blocking submission from a second requested request keeps the
        // round at zero: the implicit claim never bumps it.
        let fresh = create(&pool, &Actor::Agent(coder), "head-5").await;
        observe(&pool, true).await;
        let advanced = get(&pool, fresh.id).await;
        assert_eq!(advanced.status, CHANGES_REQUESTED);
        assert_eq!(advanced.round, 0);
    }

    #[tokio::test]
    async fn dedup_matrix_resolves_by_requester_head_and_status() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let reviewer = agent_token(&pool, "reviewer-bot").await;

        // Same head on requested: update note and lenses in place.
        let request = create(&pool, &Actor::Agent(coder), "head-1").await;
        let mut updated = draft("head-1");
        updated.note = Some("Updated note.".into());
        updated.lenses = vec!["correctness".into()];
        let refreshed = create_request_in_pool(
            &pool, REPO, BASE, KEY, KIND, &updated, &Actor::Agent(coder), &noop_notify(),
        )
        .await
        .unwrap();
        assert_eq!(refreshed.id, request.id);
        assert_eq!(refreshed.note, "Updated note.");
        assert_eq!(refreshed.lenses, ["correctness"]);
        assert_eq!(refreshed.head_sha.as_deref(), Some("head-1"));

        // Same head on in_review: the same in-place update.
        claim(&pool, request.id, &Actor::Agent(reviewer)).await;
        let mut in_review_update = draft("head-1");
        in_review_update.note = Some("Still waiting.".into());
        let refreshed = create_request_in_pool(
            &pool, REPO, BASE, KEY, KIND, &in_review_update, &Actor::Agent(coder), &noop_notify(),
        )
        .await
        .unwrap();
        assert_eq!(refreshed.id, request.id);
        assert_eq!(refreshed.note, "Still waiting.");
        assert_eq!(refreshed.status, IN_REVIEW);

        // Same head on changes_requested: refused (it is a re-request and
        // the head must differ).
        verdict(&pool, request.id, false, &Actor::Human).await;
        let error = create_request_in_pool(
            &pool, REPO, BASE, KEY, KIND, &draft("head-1"), &Actor::Agent(coder), &noop_notify(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_request");

        // New head on changes_requested: a new campaign row.
        let second = create(&pool, &Actor::Agent(coder), "head-2").await;
        assert_ne!(second.id, request.id);
        assert_eq!(second.status, REQUESTED);

        // New head on a claimed status: a new row.
        claim(&pool, second.id, &Actor::Agent(reviewer)).await;
        let third = create(&pool, &Actor::Agent(coder), "head-3").await;
        assert_ne!(third.id, second.id);
        claim(&pool, third.id, &Actor::Agent(reviewer)).await;

        // Same head on approved: the row returns unchanged.
        let approved = verdict(&pool, third.id, true, &Actor::Human).await;
        let satisfied = create(&pool, &Actor::Agent(coder), "head-3").await;
        assert_eq!(satisfied.id, approved.id);
        assert_eq!(satisfied, approved);

        // Same head on withdrawn: a new row.
        let withdrawn = withdraw(&pool, second.id, &Actor::Agent(coder)).await;
        assert_eq!(withdrawn.status, WITHDRAWN);
        let recreated = create(&pool, &Actor::Agent(coder), "head-2").await;
        assert_ne!(recreated.id, second.id);
        assert_eq!(recreated.status, REQUESTED);

        // New head on approved: a new row. The never-claimed recreated row
        // is withdrawn first so it cannot absorb the refresh.
        withdraw(&pool, recreated.id, &Actor::Agent(coder)).await;
        let after_approved = create(&pool, &Actor::Agent(coder), "head-4").await;
        assert_ne!(after_approved.id, approved.id);

        // A new head on a never-claimed requested row refreshes it in
        // place: same id, new head and note.
        withdraw(&pool, after_approved.id, &Actor::Agent(coder)).await;
        let never_claimed = create(&pool, &Actor::Agent(coder), "head-5").await;
        assert_eq!(never_claimed.status, REQUESTED);
        assert_eq!(never_claimed.round, 0);
        let mut moved = draft("head-6");
        moved.note = Some("Rebased.".into());
        let refreshed = create_request_in_pool(
            &pool, REPO, BASE, KEY, KIND, &moved, &Actor::Agent(coder), &noop_notify(),
        )
        .await
        .unwrap();
        assert_eq!(refreshed.id, never_claimed.id);
        assert_eq!(refreshed.head_sha.as_deref(), Some("head-6"));
        assert_eq!(refreshed.note, "Rebased.");
        assert_eq!(refreshed.status, REQUESTED);
    }

    #[tokio::test]
    async fn different_requesters_never_dedup_against_each_other() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let first = agent_token(&pool, "coder-a").await;
        let second = agent_token(&pool, "coder-b").await;

        let original = create(&pool, &Actor::Agent(first), "head-1").await;
        let twin = create(&pool, &Actor::Agent(second), "head-1").await;
        assert_ne!(twin.id, original.id);
        assert_eq!(twin.requester_token_id, Some(second));

        // Human requests dedup only with human-keyed rows.
        let human = create(&pool, &Actor::Human, "head-1").await;
        assert_ne!(human.id, original.id);
        assert_eq!(human.requester_token_id, None);
        let human_again = create(&pool, &Actor::Human, "head-1").await;
        assert_eq!(human_again.id, human.id);
    }

    #[tokio::test]
    async fn validation_rejects_broken_requests() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        agent_token(&pool, "reviewer-bot").await;

        let attempt = |draft: RequestDraft| {
            let pool = pool.clone();
            async move {
                create_request_in_pool(
                    &pool, REPO, BASE, KEY, KIND, &draft, &Actor::Agent(coder), &noop_notify(),
                )
                .await
                .map(|request| request.id)
            }
        };

        // The 2000-character note boundary.
        let mut boundary = draft("head-boundary");
        boundary.note = Some("x".repeat(2000));
        assert!(attempt(boundary.clone()).await.is_ok());
        boundary.note = Some("x".repeat(2001));
        assert_eq!(
            attempt(boundary).await.unwrap_err().code,
            "invalid_request"
        );

        // An expressed note stores as given, even whitespace-only.
        let mut empty_note = draft("head-1");
        empty_note.note = Some("   ".into());
        let empty = attempt(empty_note).await.unwrap();
        assert_eq!(get(&pool, empty).await.note, "   ");

        // An absent note on the same head re-ask preserves the stored note.
        let mut silent = draft("head-1");
        silent.note = None;
        let silent_id = attempt(silent).await.unwrap();
        assert_eq!(get(&pool, silent_id).await.note, "   ");

        let mut unknown_lens = draft("head-1");
        unknown_lens.lenses = vec!["style".into()];
        assert_eq!(
            attempt(unknown_lens).await.unwrap_err().code,
            "invalid_request"
        );

        let mut duplicate_lens = draft("head-1");
        duplicate_lens.lenses = vec!["security".into(), "security".into()];
        assert_eq!(
            attempt(duplicate_lens).await.unwrap_err().code,
            "invalid_request"
        );

        let mut unknown_reviewer = draft("head-1");
        unknown_reviewer.reviewers = vec!["ghost-bot".into()];
        assert_eq!(
            attempt(unknown_reviewer).await.unwrap_err().code,
            "invalid_request"
        );

        let mut duplicate_reviewer = draft("head-1");
        duplicate_reviewer.reviewers = vec!["reviewer-bot".into(), "reviewer-bot".into()];
        assert_eq!(
            attempt(duplicate_reviewer).await.unwrap_err().code,
            "invalid_request"
        );

        for budget in [0, 4] {
            let mut out_of_range = draft("head-1");
            out_of_range.max_rounds = Some(budget);
            assert_eq!(
                attempt(out_of_range).await.unwrap_err().code,
                "invalid_request"
            );
        }
        // A budget create is a fresh campaign for its own requester: the
        // earlier coder row is never-claimed and would otherwise absorb
        // the refresh, so each budget gets its own requester.
        for budget in [1, 3] {
            let mut custom = draft(&format!("head-budget-{budget}"));
            custom.max_rounds = Some(budget);
            let requester = agent_token(&pool, &format!("budget-bot-{budget}")).await;
            let created = create_request_in_pool(
                &pool, REPO, BASE, KEY, KIND, &custom, &Actor::Agent(requester), &noop_notify(),
            )
            .await
            .unwrap();
            assert_eq!(get(&pool, created.id).await.max_rounds, budget);
        }

        let mut empty_head = draft("head-1");
        empty_head.head_sha = "   ".into();
        assert_eq!(attempt(empty_head).await.unwrap_err().code, "invalid_request");

        assert_eq!(
            create_request_in_pool(
                &pool, REPO, BASE, "  ", KIND, &draft("head-1"), &Actor::Agent(coder),
                &noop_notify(),
            )
            .await
            .unwrap_err()
            .code,
            "invalid_request"
        );
        assert_eq!(
            create_request_in_pool(
                &pool, REPO, BASE, KEY, "branch", &draft("head-1"), &Actor::Agent(coder),
                &noop_notify(),
            )
            .await
            .unwrap_err()
            .code,
            "invalid_request"
        );
    }

    #[tokio::test]
    async fn round_budget_refuses_when_exhausted() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;

        // Default budget 2: two re-requests pass, the third refuses and
        // the request stays changes_requested at the cap.
        let request = create(&pool, &Actor::Agent(coder), "head-1").await;
        for head in ["head-2", "head-3"] {
            observe(&pool, true).await;
            re_request(&pool, request.id, head, &Actor::Agent(coder)).await;
        }
        observe(&pool, true).await;
        let error = re_request_in_pool(
            &pool,
            request.id,
            "head-4",
            None,
            &Actor::Agent(coder),
            &noop_notify(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "round_budget_exceeded");
        let after = get(&pool, request.id).await;
        assert_eq!(after.status, CHANGES_REQUESTED);
        assert_eq!(after.round, 2);
        assert_eq!(after.max_rounds, 2);
        // The needs-human predicate holds at exhaustion.
        assert!(after.status == CHANGES_REQUESTED && after.round >= after.max_rounds);

        // Custom budget 1: one re-request, then refusal.
        let mut tight = draft("head-tight");
        tight.max_rounds = Some(1);
        let tight = create_request_in_pool(
            &pool, REPO, BASE, KEY, KIND, &tight, &Actor::Agent(coder), &noop_notify(),
        )
        .await
        .unwrap();
        observe(&pool, true).await;
        re_request(&pool, tight.id, "head-tight-2", &Actor::Agent(coder)).await;
        observe(&pool, true).await;
        let error = re_request_in_pool(
            &pool,
            tight.id,
            "head-tight-3",
            None,
            &Actor::Agent(coder),
            &noop_notify(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "round_budget_exceeded");
        assert_eq!(get(&pool, tight.id).await.round, 1);

        // Custom budget 3: three re-requests, then refusal.
        let mut wide = draft("head-wide");
        wide.max_rounds = Some(3);
        let wide = create_request_in_pool(
            &pool, REPO, BASE, KEY, KIND, &wide, &Actor::Agent(coder), &noop_notify(),
        )
        .await
        .unwrap();
        for round in 1..=3 {
            observe(&pool, true).await;
            re_request(
                &pool,
                wide.id,
                &format!("head-wide-{round}"),
                &Actor::Agent(coder),
            )
            .await;
            assert_eq!(get(&pool, wide.id).await.round, round);
        }
        observe(&pool, true).await;
        let error = re_request_in_pool(
            &pool,
            wide.id,
            "head-wide-4",
            None,
            &Actor::Agent(coder),
            &noop_notify(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "round_budget_exceeded");
        assert_eq!(get(&pool, wide.id).await.round, 3);
    }

    #[tokio::test]
    async fn observation_advances_open_requests_independently() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let first = agent_token(&pool, "coder-a").await;
        let second = agent_token(&pool, "coder-b").await;

        let alpha = create(&pool, &Actor::Agent(first), "head-1").await;
        let beta = create(&pool, &Actor::Agent(second), "head-2").await;

        // A blocking submission advances both concurrent open requests,
        // one fire each.
        let (notify, fires) = recording_notify();
        observe_submission_in_pool(&pool, REPO, BASE, KEY, KIND, true, None, &notify)
            .await
            .unwrap();
        assert_eq!(fires.lock().unwrap().len(), 2);
        assert_eq!(get(&pool, alpha.id).await.status, CHANGES_REQUESTED);
        assert_eq!(get(&pool, beta.id).await.status, CHANGES_REQUESTED);

        // Alpha re-requests; a clean submission then approves only alpha
        // while beta keeps its sticky changes_requested.
        re_request(&pool, alpha.id, "head-3", &Actor::Agent(first)).await;
        let (notify, fires) = recording_notify();
        observe_submission_in_pool(&pool, REPO, BASE, KEY, KIND, false, None, &notify)
            .await
            .unwrap();
        assert_eq!(fires.lock().unwrap().len(), 1);
        assert_eq!(fires.lock().unwrap()[0].id, alpha.id);
        assert_eq!(get(&pool, alpha.id).await.status, APPROVED);
        assert_eq!(get(&pool, beta.id).await.status, CHANGES_REQUESTED);

        // Submissions on terminal requests change no status.
        let (notify, fires) = recording_notify();
        observe_submission_in_pool(&pool, REPO, BASE, KEY, KIND, true, None, &notify)
            .await
            .unwrap();
        assert!(fires.lock().unwrap().is_empty());
        assert_eq!(get(&pool, alpha.id).await.status, APPROVED);
        assert_eq!(get(&pool, beta.id).await.status, CHANGES_REQUESTED);

        // An identity without requests observes as a no-op.
        let (notify, fires) = recording_notify();
        observe_submission_in_pool(&pool, REPO, "other-base", KEY, KIND, true, None, &notify)
            .await
            .unwrap();
        assert!(fires.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_clean_submission_from_requested_claims_and_approves() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let request = create(&pool, &Actor::Agent(coder), "head-1").await;
        observe(&pool, false).await;
        let approved = get(&pool, request.id).await;
        assert_eq!(approved.status, APPROVED);
        assert_eq!(approved.round, 0, "the implicit claim never bumps the round");
    }

    #[tokio::test]
    async fn the_requesters_own_submission_never_settles_their_request() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let request = create(&pool, &Actor::Agent(coder), "head-1").await;

        let (notify, fires) = recording_notify();
        observe_submission_in_pool(&pool, REPO, BASE, KEY, KIND, false, Some(coder), &notify)
            .await
            .unwrap();
        assert!(fires.lock().unwrap().is_empty());
        assert_eq!(get(&pool, request.id).await.status, REQUESTED);

        // A second party's clean submission still approves it.
        let reviewer = agent_token(&pool, "reviewer-bot").await;
        observe_submission_in_pool(&pool, REPO, BASE, KEY, KIND, false, Some(reviewer), &notify)
            .await
            .unwrap();
        assert_eq!(get(&pool, request.id).await.status, APPROVED);
        assert_eq!(fires.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn announce_enters_in_review_with_the_caller_as_requester() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;

        let request = announce(&pool, "head-1", Some("Starting my review."), &Actor::Agent(coder))
            .await;
        assert_eq!(request.status, IN_REVIEW);
        assert_eq!(request.round, 0);
        assert_eq!(request.max_rounds, DEFAULT_MAX_ROUNDS);
        assert!(request.lenses.is_empty());
        assert!(request.reviewers.is_empty());
        assert_eq!(request.note, "Starting my review.");
        assert_eq!(request.head_sha.as_deref(), Some("head-1"));
        assert_eq!(request.requester_token_id, Some(coder));
        assert_eq!(get(&pool, request.id).await, request);

        // An absent note inserts empty on a fresh row.
        let silent = announce(&pool, "head-2", None, &Actor::Agent(coder)).await;
        assert_ne!(silent.id, request.id);
        assert_eq!(silent.note, "");
        assert_eq!(silent.status, IN_REVIEW);
    }

    #[tokio::test]
    async fn announce_dedup_ladder_follows_create() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;

        // Same head on the coder's requested row: updates in place and
        // moves to in review, restating the note.
        let request = create(&pool, &Actor::Agent(coder), "head-1").await;
        let announced = announce(&pool, "head-1", Some("On it."), &Actor::Agent(coder)).await;
        assert_eq!(announced.id, request.id);
        assert_eq!(announced.status, IN_REVIEW);
        assert_eq!(announced.note, "On it.");

        // A re-announce on the now in-review row updates in place again;
        // an absent note keeps the stored one.
        let again = announce(&pool, "head-1", None, &Actor::Agent(coder)).await;
        assert_eq!(again.id, request.id);
        assert_eq!(again.status, IN_REVIEW);
        assert_eq!(again.note, "On it.");

        // Same head after changes_requested: refused with create's message.
        verdict(&pool, request.id, false, &Actor::Human).await;
        let error = announce_review_in_pool(
            &pool, REPO, BASE, KEY, KIND, "head-1", None, &Actor::Agent(coder), &noop_notify(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "invalid_request");

        // Same head on approved: returns satisfied, untouched.
        re_request(&pool, request.id, "head-2", &Actor::Agent(coder)).await;
        let approved = verdict(&pool, request.id, true, &Actor::Human).await;
        assert_eq!(approved.status, APPROVED);
        let satisfied = announce(&pool, "head-2", Some("Again?"), &Actor::Agent(coder)).await;
        assert_eq!(satisfied.id, approved.id);
        assert_eq!(satisfied, approved, "the satisfied row returns untouched");
    }

    #[tokio::test]
    async fn announce_refreshes_the_callers_never_claimed_request_in_place() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;

        let request = create(&pool, &Actor::Agent(coder), "head-1").await;
        assert_eq!(request.status, REQUESTED);
        let announced = announce(&pool, "head-2", Some("Rebased; starting now."), &Actor::Agent(coder))
            .await;
        assert_eq!(announced.id, request.id, "an announce is also a self-claim");
        assert_eq!(announced.status, IN_REVIEW);
        assert_eq!(announced.head_sha.as_deref(), Some("head-2"));
        assert_eq!(announced.note, "Rebased; starting now.");
        assert_eq!(announced.round, 0);
    }

    #[tokio::test]
    async fn announced_requests_settle_on_a_second_party_delivery() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let reviewer = agent_token(&pool, "reviewer-bot").await;

        let request = announce(&pool, "head-1", None, &Actor::Agent(coder)).await;
        observe_submission_in_pool(&pool, REPO, BASE, KEY, KIND, false, Some(reviewer), &noop_notify())
            .await
            .unwrap();
        assert_eq!(get(&pool, request.id).await.status, APPROVED);
    }

    #[tokio::test]
    async fn the_narrowed_skip_keeps_requested_refusals_and_settles_shifted_edges() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let reviewer = agent_token(&pool, "reviewer-bot").await;

        // Preserved: a requester's own delivery cannot self-approve while
        // the row is still requested.
        let requested = create_on(&pool, &Actor::Agent(coder), "base-1", "head-1").await;
        observe_submission_in_pool(&pool, REPO, "base-1", KEY, KIND, false, Some(coder), &noop_notify())
            .await
            .unwrap();
        assert_eq!(get(&pool, requested.id).await.status, REQUESTED);

        // Shifted edge: the requester self-claims, then delivers; the row
        // settles where the old skip held it open.
        let self_claimed = create_on(&pool, &Actor::Agent(coder), "base-2", "head-2").await;
        claim(&pool, self_claimed.id, &Actor::Agent(coder)).await;
        observe_submission_in_pool(&pool, REPO, "base-2", KEY, KIND, false, Some(coder), &noop_notify())
            .await
            .unwrap();
        assert_eq!(get(&pool, self_claimed.id).await.status, APPROVED);

        // Shifted edge: the requester delivers after another agent
        // claimed; the claimer's working review settles.
        let claimed = create_on(&pool, &Actor::Agent(coder), "base-3", "head-3").await;
        claim(&pool, claimed.id, &Actor::Agent(reviewer)).await;
        observe_submission_in_pool(&pool, REPO, "base-3", KEY, KIND, true, Some(coder), &noop_notify())
            .await
            .unwrap();
        assert_eq!(get(&pool, claimed.id).await.status, CHANGES_REQUESTED);
    }

    #[tokio::test]
    async fn announce_narrates_the_actor_and_pushes_the_event_kind() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let (notify, fires) = recording_notify();

        // The fresh announce narrates once, naming the actor, and the push
        // carries the narrating kind.
        let request = announce_review_in_pool(
            &pool, REPO, BASE, KEY, KIND, "head-1", None, &Actor::Agent(coder), &notify,
        )
        .await
        .unwrap();
        let rows = listed_events(&pool).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, events::REVIEW_ANNOUNCED);
        assert_eq!(rows[0].actor_kind, "agent");
        assert_eq!(rows[0].actor_name, "coder-bot");
        assert!(rows[0].summary.contains("coder-bot"));
        assert_eq!(rows[0].request_id, Some(request.id));
        assert_eq!(fires.lock().unwrap().len(), 1);
        assert_eq!(
            fires.lock().unwrap()[0].event.as_deref(),
            Some(events::REVIEW_ANNOUNCED)
        );

        // The same-head dedup announce updates in place, narrates nothing,
        // and its push carries no event kind.
        announce_review_in_pool(
            &pool, REPO, BASE, KEY, KIND, "head-1", Some("On it."), &Actor::Agent(coder), &notify,
        )
        .await
        .unwrap();
        assert_eq!(listed_events(&pool).await.len(), 1);
        assert_eq!(fires.lock().unwrap().len(), 2);
        assert_eq!(fires.lock().unwrap()[1].event, None);

        // The caller's never-claimed row on a new head refreshes into in
        // review and narrates the caller's first announce for that head.
        create(&pool, &Actor::Agent(coder), "head-old").await;
        assert_eq!(listed_events(&pool).await.len(), 2);
        announce_review_in_pool(
            &pool, REPO, BASE, KEY, KIND, "head-new", None, &Actor::Agent(coder), &notify,
        )
        .await
        .unwrap();
        let rows = listed_events(&pool).await;
        assert_eq!(
            rows.iter().map(|row| row.kind.as_str()).collect::<Vec<_>>(),
            [events::REVIEW_ANNOUNCED, events::REQUEST_CREATED, events::REVIEW_ANNOUNCED]
        );
        assert_eq!(fires.lock().unwrap().len(), 3);
        assert_eq!(
            fires.lock().unwrap()[2].event.as_deref(),
            Some(events::REVIEW_ANNOUNCED)
        );
    }

    #[tokio::test]
    async fn deleting_the_requester_token_leaves_a_human_keyed_row() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let request = create(&pool, &Actor::Agent(coder), "head-1").await;

        sqlx::query("DELETE FROM agent_tokens WHERE id = ?")
            .bind(coder)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(get(&pool, request.id).await.requester_token_id, None);

        // The orphaned row dedups as human-keyed from then on.
        let human_match = create(&pool, &Actor::Human, "head-1").await;
        assert_eq!(human_match.id, request.id);
        let other = agent_token(&pool, "coder-b").await;
        let agent_row = create(&pool, &Actor::Agent(other), "head-1").await;
        assert_ne!(agent_row.id, request.id);
    }

    #[tokio::test]
    async fn deleting_the_repo_cascades_review_requests() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let request = create(&pool, &Actor::Agent(coder), "head-1").await;
        sqlx::query("DELETE FROM repos WHERE path = ?")
            .bind(REPO)
            .execute(&pool)
            .await
            .unwrap();
        assert!(load_request(&pool, request.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_requests_in_pool_lists_identity_rows_for_the_header() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;

        // A withdrawn human row drops out; an approved row stays listed
        // (only withdrawal leaves the list).
        let claimed = create(&pool, &Actor::Agent(coder), "head-1").await;
        claim(&pool, claimed.id, &Actor::Agent(coder)).await;
        let human = create(&pool, &Actor::Human, "head-1").await;
        assert_ne!(human.id, claimed.id, "dedup is per requester");
        withdraw(&pool, human.id, &Actor::Human).await;
        let latest = create(&pool, &Actor::Agent(coder), "head-2").await;
        // A distinct requester: a same-requester create would refresh the
        // never-claimed head-2 row in place instead of stacking a row.
        let other = agent_token(&pool, "other-bot").await;
        let settled = create(&pool, &Actor::Agent(other), "head-4").await;
        claim(&pool, settled.id, &Actor::Agent(coder)).await;
        verdict(&pool, settled.id, true, &Actor::Human).await;

        let rows = list_requests_in_pool(&pool, REPO, BASE, KEY, KIND).await.unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![settled.id, latest.id, claimed.id],
            "non-withdrawn rows only, newest first"
        );
        let in_review = rows.iter().find(|row| row.id == claimed.id).unwrap();
        assert_eq!(in_review.status, IN_REVIEW);
        assert_eq!(in_review.requester, "coder-bot");
        assert!(!in_review.needs_human);
        assert_eq!(in_review.round, 0);
        assert_eq!(in_review.max_rounds, 2);
        assert_eq!(in_review.head_sha.as_deref(), Some("head-1"));
        assert_eq!(in_review.comment_count, 0);
        assert_eq!(in_review.unresolved_finding_counts.p0, 0);
        assert_eq!(
            rows.iter().find(|row| row.id == settled.id).unwrap().status,
            APPROVED
        );

        // An unresolved P0 on an in-review request reads as needs human,
        // with the identity's counts riding every row on that identity.
        seed_finding(&pool, BASE, "P0", false).await;
        let rows = list_requests_in_pool(&pool, REPO, BASE, KEY, KIND).await.unwrap();
        let flagged = rows.iter().find(|row| row.id == claimed.id).unwrap();
        assert!(flagged.needs_human);
        assert_eq!(flagged.comment_count, 1);
        assert_eq!(flagged.unresolved_finding_counts.p0, 1);
        // The approved row never flags needs-human on findings alone.
        assert!(
            !rows.iter().find(|row| row.id == settled.id).unwrap().needs_human,
            "the needs-human rule stays the engine's"
        );

        // An exhausted round budget reads as needs human on its own
        // identity (observations advance every open request at once, so
        // this scenario uses a distinct base).
        let mut tight = draft("head-b1");
        tight.max_rounds = Some(1);
        let exhausted = create_request_in_pool(
            &pool, REPO, "base-b", KEY, KIND, &tight, &Actor::Agent(coder), &noop_notify(),
        )
        .await
        .unwrap();
        observe_on(&pool, "base-b", true).await;
        re_request(&pool, exhausted.id, "head-b2", &Actor::Agent(coder)).await;
        observe_on(&pool, "base-b", true).await;
        let rows = list_requests_in_pool(&pool, REPO, "base-b", KEY, KIND).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, CHANGES_REQUESTED);
        assert_eq!(rows[0].round, 1);
        assert!(rows[0].needs_human);
    }

    #[tokio::test]
    async fn the_update_note_shares_the_create_bound_and_refreshes_only_that_column() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let request = create(&pool, &Actor::Human, "head-1").await;

        assert!(validate_request_note("   ").is_ok());
        assert!(validate_request_note(&"a".repeat(MAX_NOTE_CHARS + 1)).is_err());
        assert!(validate_request_note(&"a".repeat(MAX_NOTE_CHARS)).is_ok());

        let before = get(&pool, request.id).await;
        let updated_at = refresh_request_note(&pool, request.id, "Refreshed note.")
            .await
            .unwrap();
        let after = get(&pool, request.id).await;
        assert_eq!(after.note, "Refreshed note.");
        assert_eq!(after.status, before.status);
        assert_eq!(after.updated_at, updated_at);
        assert!(after.updated_at >= before.updated_at);
    }

    #[tokio::test]
    async fn the_sink_fires_exactly_once_per_successful_mutation() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let (notify, fires) = recording_notify();

        let request = create_request_in_pool(
            &pool, REPO, BASE, KEY, KIND, &draft("head-1"), &Actor::Agent(coder), &notify,
        )
        .await
        .unwrap();
        assert_eq!(fires.lock().unwrap().len(), 1);
        let change = fires.lock().unwrap()[0].clone();
        assert_eq!(change.id, request.id);
        assert_eq!(change.repo_path, REPO);
        assert_eq!(change.base_sha, BASE);
        assert_eq!(change.target_key, KEY);
        assert_eq!(change.target_kind, KIND);
        assert_eq!(change.status, REQUESTED);

        // Same-head dedup update: one more fire.
        let mut updated = draft("head-1");
        updated.note = Some("Updated.".into());
        create_request_in_pool(
            &pool, REPO, BASE, KEY, KIND, &updated, &Actor::Agent(coder), &notify,
        )
        .await
        .unwrap();
        assert_eq!(fires.lock().unwrap().len(), 2);
        assert_eq!(fires.lock().unwrap()[1].status, REQUESTED);

        claim_request_in_pool(&pool, request.id, &Actor::Agent(coder), &notify)
            .await
            .unwrap();
        assert_eq!(fires.lock().unwrap().len(), 3);
        assert_eq!(fires.lock().unwrap()[2].status, IN_REVIEW);

        set_request_verdict_in_pool(&pool, request.id, false, &Actor::Human, &notify)
            .await
            .unwrap();
        assert_eq!(fires.lock().unwrap().len(), 4);
        assert_eq!(fires.lock().unwrap()[3].status, CHANGES_REQUESTED);

        re_request_in_pool(&pool, request.id, "head-2", None, &Actor::Agent(coder), &notify)
            .await
            .unwrap();
        assert_eq!(fires.lock().unwrap().len(), 5);
        assert_eq!(fires.lock().unwrap()[4].status, IN_REVIEW);

        set_request_verdict_in_pool(&pool, request.id, true, &Actor::Human, &notify)
            .await
            .unwrap();
        assert_eq!(fires.lock().unwrap().len(), 6);
        assert_eq!(fires.lock().unwrap()[5].status, APPROVED);

        // A satisfied re-create returns the approved row unchanged: no fire.
        create_request_in_pool(
            &pool, REPO, BASE, KEY, KIND, &draft("head-2"), &Actor::Agent(coder), &notify,
        )
        .await
        .unwrap();
        assert_eq!(fires.lock().unwrap().len(), 6, "a satisfied ask is not a mutation");

        // Terminal requests never fire.
        observe_submission_in_pool(&pool, REPO, BASE, KEY, KIND, false, None, &notify)
            .await
            .unwrap();
        assert_eq!(fires.lock().unwrap().len(), 6);
    }

    #[test]
    fn blocking_severity_matches_only_p0_and_p1() {
        assert!(is_blocking_severity("P0"));
        assert!(is_blocking_severity("P1"));
        assert!(!is_blocking_severity("P2"));
        assert!(!is_blocking_severity("P3"));
        assert!(!is_blocking_severity(""));
        assert_eq!(BLOCKING_SEVERITIES, ["P0", "P1"]);
    }

    #[test]
    fn open_request_category_follows_the_priority_rules() {
        let category = |status: &str, round: i64, max: i64, p0: i64, p1: i64| {
            open_request_category(status, round, max, p0, p1)
        };
        assert_eq!(category(REQUESTED, 0, 2, 0, 0), Some(CATEGORY_REQUESTED));
        // A P0 on a requested or in-review request needs a human.
        assert_eq!(category(REQUESTED, 0, 2, 1, 0), Some(CATEGORY_NEEDS_HUMAN));
        assert_eq!(category(IN_REVIEW, 1, 2, 2, 0), Some(CATEGORY_NEEDS_HUMAN));
        // Exhausted rounds need a human even with no findings.
        assert_eq!(
            category(CHANGES_REQUESTED, 2, 2, 0, 0),
            Some(CATEGORY_NEEDS_HUMAN)
        );
        assert_eq!(
            category(CHANGES_REQUESTED, 3, 2, 0, 0),
            Some(CATEGORY_NEEDS_HUMAN)
        );
        // In-budget changes_requested stays in its lifecycle category.
        assert_eq!(
            category(CHANGES_REQUESTED, 1, 2, 0, 0),
            Some(CATEGORY_CHANGES_REQUESTED)
        );
        assert_eq!(
            category(CHANGES_REQUESTED, 0, 2, 0, 5),
            Some(CATEGORY_CHANGES_REQUESTED)
        );
        // An in-review request queues only when P1 findings sit on it.
        assert_eq!(category(IN_REVIEW, 1, 2, 0, 0), None);
        assert_eq!(
            category(IN_REVIEW, 1, 2, 0, 3),
            Some(CATEGORY_UNRESOLVED_FINDINGS)
        );
        // Settled requests never queue through this predicate.
        assert_eq!(category(APPROVED, 0, 2, 9, 9), None);
        assert_eq!(category(WITHDRAWN, 0, 2, 9, 9), None);
    }

    async fn create_on(pool: &SqlitePool, actor: &Actor, base: &str, head: &str) -> ReviewRequest {
        create_request_in_pool(pool, REPO, base, KEY, KIND, &draft(head), actor, &noop_notify())
            .await
            .unwrap()
    }

    async fn observe_on(pool: &SqlitePool, base: &str, blocking: bool) {
        observe_submission_in_pool(pool, REPO, base, KEY, KIND, blocking, None, &noop_notify())
            .await
            .unwrap()
    }

    // One unresolved (or resolved) finding on a review identity under the
    // module's fixed repo/target identity.
    async fn seed_finding(pool: &SqlitePool, base: &str, severity: &str, resolved: bool) {
        sqlx::query(
            "INSERT OR IGNORE INTO reviews (repo_path, base_sha, target_key, target_kind, created_at) \
             VALUES (?, ?, ?, ?, 1)",
        )
        .bind(REPO)
        .bind(base)
        .bind(KEY)
        .bind(KIND)
        .execute(pool)
        .await
        .unwrap();
        let review_id: i64 = sqlx::query_scalar(
            "SELECT id FROM reviews WHERE repo_path = ? AND base_sha = ? \
             AND target_key = ? AND target_kind = ?",
        )
        .bind(REPO)
        .bind(base)
        .bind(KEY)
        .bind(KIND)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO comments (review_id, parent_id, author_kind, author_name, body, \
                    severity, submission_id, resolved_at, edited_at, created_at) \
             VALUES (?, NULL, 'agent', 'reviewer-bot', 'finding', ?, NULL, ?, NULL, 1)",
        )
        .bind(review_id)
        .bind(severity)
        .bind(resolved.then_some(1))
        .execute(pool)
        .await
        .unwrap();
    }

    fn row_of<'a>(queue: &'a AttentionQueue, base: &str) -> &'a AttentionRow {
        queue
            .repos
            .iter()
            .flat_map(|group| group.rows.iter())
            .find(|row| row.base_sha == base)
            .unwrap_or_else(|| panic!("no queue row for {base}"))
    }

    #[tokio::test]
    async fn attention_queue_assembles_categories_from_the_store_only() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        seed_repo(&pool, "/other").await;
        let reviewer = agent_token(&pool, "reviewer-bot").await;

        // Requested with a P1: stays in the requested category (only a P0
        // escalates), and its age basis is its creation.
        let coder_a = agent_token(&pool, "coder-a").await;
        create_on(&pool, &Actor::Agent(coder_a), "base-a", "head-a").await;
        seed_finding(&pool, "base-a", "P1", false).await;

        // In-budget changes_requested.
        let coder_b = agent_token(&pool, "coder-b").await;
        create_on(&pool, &Actor::Agent(coder_b), "base-b", "head-b").await;
        observe_on(&pool, "base-b", true).await;

        // Exhausted round budget: changes_requested at the cap is the
        // needs-human predicate, flagged on the row.
        let coder_c = agent_token(&pool, "coder-c").await;
        let mut tight = draft("head-c");
        tight.max_rounds = Some(1);
        let exhausted = create_request_in_pool(
            &pool, REPO, "base-c", KEY, KIND, &tight, &Actor::Agent(coder_c), &noop_notify(),
        )
        .await
        .unwrap();
        observe_on(&pool, "base-c", true).await;
        re_request(&pool, exhausted.id, "head-c2", &Actor::Agent(coder_c)).await;
        observe_on(&pool, "base-c", true).await;

        // An approved request whose identity carries an unresolved P0:
        // the late-findings path, still showing the approved status.
        let coder_d = agent_token(&pool, "coder-d").await;
        let approved = create_on(&pool, &Actor::Agent(coder_d), "base-d", "head-d").await;
        claim(&pool, approved.id, &Actor::Agent(reviewer)).await;
        verdict(&pool, approved.id, true, &Actor::Human).await;
        seed_finding(&pool, "base-d", "P0", false).await;

        // Findings on an identity that never had a request.
        seed_finding(&pool, "base-e", "P1", false).await;

        // An in-review request whose recorded surface head moved: changed
        // since review, derived from the retrospection store alone. Each
        // scenario records under its own worktree path, matching the
        // store's per-surface identity.
        let coder_f = agent_token(&pool, "coder-f").await;
        let moved = create_request_in_pool(
            &pool, REPO, "base-f", "/wt-f", KIND, &draft("head-f"), &Actor::Agent(coder_f),
            &noop_notify(),
        )
        .await
        .unwrap();
        claim(&pool, moved.id, &Actor::Agent(reviewer)).await;
        crate::retrospection::record_surface_open(
            &pool, REPO, "worktree", "/wt-f", "feature-f", "/wt-f", "moved-head",
        )
        .await;

        // An approved request whose surface head moved afterwards.
        let coder_h = agent_token(&pool, "coder-h").await;
        let approved_moved = create_request_in_pool(
            &pool, REPO, "base-h", "/wt-h", KIND, &draft("head-h"), &Actor::Agent(coder_h),
            &noop_notify(),
        )
        .await
        .unwrap();
        claim(&pool, approved_moved.id, &Actor::Agent(reviewer)).await;
        verdict(&pool, approved_moved.id, true, &Actor::Human).await;
        crate::retrospection::record_surface_open(
            &pool, REPO, "worktree", "/wt-h", "feature-h", "/wt-h", "moved-h",
        )
        .await;

        // Concurrent requests on one identity: two rows, one shared label.
        let coder_g1 = agent_token(&pool, "coder-g1").await;
        let coder_g2 = agent_token(&pool, "coder-g2").await;
        create_on(&pool, &Actor::Agent(coder_g1), "base-g", "head-g").await;
        create_on(&pool, &Actor::Agent(coder_g2), "base-g", "head-g").await;

        // A second repository keeps its own group.
        let coder_i = agent_token(&pool, "coder-i").await;
        create_request_in_pool(
            &pool, "/other", "ob", "/other-wt", KIND, &draft("head-i"), &Actor::Agent(coder_i),
            &noop_notify(),
        )
        .await
        .unwrap();

        // The queue path never spawns Git.
        let (spawns, queue) =
            crate::git::spawn_counted(list_attention_in_pool(&pool)).await;
        assert_eq!(spawns, 0);
        let queue = queue.unwrap();

        assert_eq!(queue.repos.len(), 2);
        let demo = queue.repos.iter().find(|group| group.repo_path == REPO).unwrap();
        assert_eq!(demo.repo_name, "test");
        let other = queue.repos.iter().find(|group| group.repo_path == "/other").unwrap();
        assert_eq!(other.rows.len(), 1);
        assert_eq!(other.rows[0].category, CATEGORY_REQUESTED);
        assert_eq!(other.rows[0].change_label, "other-wt");

        let requested = row_of(&queue, "base-a");
        assert_eq!(requested.category, CATEGORY_REQUESTED);
        assert!(!requested.needs_human);
        assert_eq!(requested.unresolved_p1, 1);
        assert_eq!(requested.unresolved_p0, 0);
        assert_eq!(requested.requester, "coder-a");
        assert_eq!(
            requested.age_basis,
            get(&pool, requested.request_id.unwrap()).await.created_at,
            "a requested row ages from its creation"
        );
        assert_eq!(requested.status, REQUESTED);
        assert_eq!(requested.round, 0);
        assert_eq!(requested.max_rounds, 2);
        assert_eq!(requested.change_label, "demo");

        let changed = row_of(&queue, "base-b");
        assert_eq!(changed.category, CATEGORY_CHANGES_REQUESTED);
        assert_eq!(changed.round, 0);
        assert_eq!(changed.status, CHANGES_REQUESTED);

        let needs_human = row_of(&queue, "base-c");
        assert_eq!(needs_human.category, CATEGORY_NEEDS_HUMAN);
        assert!(needs_human.needs_human);
        assert_eq!(needs_human.round, 1);
        assert_eq!(needs_human.max_rounds, 1);

        let findings = row_of(&queue, "base-d");
        assert_eq!(findings.category, CATEGORY_UNRESOLVED_FINDINGS);
        assert_eq!(findings.status, APPROVED);
        assert_eq!(findings.request_id, Some(approved.id));
        assert_eq!(findings.unresolved_p0, 1);
        // The only P0-carrying row sorts first within the repo group.
        assert_eq!(demo.rows[0].base_sha, "base-d");

        let free = row_of(&queue, "base-e");
        assert_eq!(free.category, CATEGORY_UNRESOLVED_FINDINGS);
        assert_eq!(free.request_id, None);
        assert_eq!(free.status, "");
        assert_eq!(free.requester, "");
        assert_eq!(free.unresolved_p1, 1);

        let changed_since = row_of(&queue, "base-f");
        assert_eq!(changed_since.category, CATEGORY_CHANGED_SINCE_REVIEW);
        assert_eq!(changed_since.status, IN_REVIEW);
        assert_eq!(changed_since.head_sha.as_deref(), Some("moved-head"));
        assert_eq!(changed_since.change_label, "feature-f");

        let approved_changed = row_of(&queue, "base-h");
        assert_eq!(approved_changed.category, CATEGORY_CHANGED_SINCE_REVIEW);
        assert_eq!(approved_changed.status, APPROVED);
        assert_eq!(approved_changed.head_sha.as_deref(), Some("moved-h"));

        // Concurrent requests share the identity and the change label.
        let concurrent: Vec<&AttentionRow> = demo
            .rows
            .iter()
            .filter(|row| row.base_sha == "base-g")
            .collect();
        assert_eq!(concurrent.len(), 2);
        assert!(concurrent.iter().all(|row| row.change_label == "demo"));
        assert_eq!(concurrent[0].requester, "coder-g1");
        assert_eq!(concurrent[1].requester, "coder-g2");
    }

    #[tokio::test]
    async fn resolved_findings_and_unchanged_settled_reviews_stay_off_the_queue() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let reviewer = agent_token(&pool, "reviewer-bot").await;

        // A resolved P0 does not escalate a requested row.
        let requested = create_on(&pool, &Actor::Agent(coder), "base-r", "head-r").await;
        seed_finding(&pool, "base-r", "P0", true).await;

        // An approved request with no findings and an unchanged recorded
        // head has nothing to report.
        let approved = create_on(&pool, &Actor::Agent(coder), "base-s", "head-s").await;
        claim(&pool, approved.id, &Actor::Agent(reviewer)).await;
        verdict(&pool, approved.id, true, &Actor::Human).await;
        crate::retrospection::record_surface_open(
            &pool, REPO, "worktree", KEY, "same", KEY, "head-s",
        )
        .await;

        let queue = list_attention_in_pool(&pool).await.unwrap();
        assert_eq!(queue.repos.len(), 1);
        let rows = &queue.repos[0].rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, Some(requested.id));
        assert_eq!(rows[0].category, CATEGORY_REQUESTED);
        assert_eq!(rows[0].unresolved_p0, 0);
    }

    async fn listed_events(pool: &SqlitePool) -> Vec<crate::events::EventRow> {
        crate::events::list_events_in_pool(
            pool,
            &crate::events::EventQuery {
                since_id: 0,
                repo_path: None,
                limit: 100,
                ascending: true,
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn lifecycle_transitions_narrate_events_with_actor_attribution() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let coder = agent_token(&pool, "coder-bot").await;
        let reviewer = agent_token(&pool, "reviewer-bot").await;

        // The create narrates once with the requester token's name; a
        // same-head re-ask restates the existing ask and narrates nothing;
        // a refused transition never narrates.
        let request = create(&pool, &Actor::Agent(coder), "head-1").await;
        create(&pool, &Actor::Agent(coder), "head-1").await;
        assert!(claim_request_in_pool(&pool, request.id, &Actor::Human, &noop_notify())
            .await
            .is_err());
        claim(&pool, request.id, &Actor::Agent(reviewer)).await;
        verdict(&pool, request.id, false, &Actor::Human).await;
        re_request(&pool, request.id, "head-2", &Actor::Agent(coder)).await;
        withdraw(&pool, request.id, &Actor::Agent(coder)).await;
        let human_request = create(&pool, &Actor::Human, "head-3").await;

        let rows = listed_events(&pool).await;
        let narrated: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|row| (row.kind.as_str(), row.actor_kind.as_str(), row.actor_name.as_str()))
            .collect();
        assert_eq!(
            narrated,
            [
                ("request_created", "agent", "coder-bot"),
                ("request_claimed", "agent", "reviewer-bot"),
                ("request_verdict", "human", "human"),
                ("request_re_requested", "agent", "coder-bot"),
                ("request_withdrawn", "agent", "coder-bot"),
                ("request_created", "human", "human"),
            ]
        );
        assert_eq!(rows[0].request_id, Some(request.id));
        assert_eq!(rows[5].request_id, Some(human_request.id));
        assert_eq!(rows[0].repo_path, REPO);
        assert_eq!(rows[0].base_sha.as_deref(), Some(BASE));
        assert_eq!(rows[0].target_key.as_deref(), Some(KEY));
        assert_eq!(rows[0].target_kind.as_deref(), Some(KIND));
        assert_eq!(rows[0].comment_id, None);
        assert!(rows[0].summary.contains("coder-bot"));
        assert!(rows[2].summary.starts_with("changes requested by "));
    }

    // A request-less identity's discussion comment (no severity) with a
    // controlled created_at, so watermark gating is testable.
    async fn seed_discussion(pool: &SqlitePool, base: &str, created_at: i64, author: &str) {
        sqlx::query(
            "INSERT OR IGNORE INTO reviews (repo_path, base_sha, target_key, target_kind, created_at) \
             VALUES (?, ?, ?, ?, 1)",
        )
        .bind(REPO)
        .bind(base)
        .bind(KEY)
        .bind(KIND)
        .execute(pool)
        .await
        .unwrap();
        let review_id: i64 = sqlx::query_scalar(
            "SELECT id FROM reviews WHERE repo_path = ? AND base_sha = ? \
             AND target_key = ? AND target_kind = ?",
        )
        .bind(REPO)
        .bind(base)
        .bind(KEY)
        .bind(KIND)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO comments (review_id, parent_id, author_kind, author_name, body, \
                    severity, created_at) \
             VALUES (?, NULL, 'agent', ?, 'a question', NULL, ?)",
        )
        .bind(review_id)
        .bind(author)
        .bind(created_at)
        .execute(pool)
        .await
        .unwrap();
    }

    // One narrated event at the real clock, so a subsequent mark_activity_seen
    // sets a watermark time the seeded comments can sit before or after.
    async fn seed_event_now(pool: &SqlitePool) {
        crate::events::record_event(
            pool,
            crate::events::NewEvent {
                repo_path: REPO.into(),
                kind: crate::events::COMMENT_POSTED,
                base_sha: None,
                target_key: None,
                target_kind: None,
                request_id: None,
                comment_id: None,
                actor_kind: crate::events::ACTOR_HUMAN,
                actor_name: "human".into(),
                summary: "s".into(),
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn recent_comments_gate_on_the_watermark_and_drain_on_mark_seen() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;
        let now = crate::store::now_millis();

        // A request-less identity with a discussion comment. The unmarked
        // watermark maps to the epoch, so the first run reads it as new.
        seed_discussion(&pool, "base-c1", now - 60_000, "codex").await;
        let queue = list_attention_in_pool(&pool).await.unwrap();
        let row = row_of(&queue, "base-c1");
        assert_eq!(row.category, CATEGORY_RECENT_COMMENTS);
        assert_eq!(row.request_id, None);
        assert_eq!(row.age_basis, now - 60_000);
        assert_eq!(row.last_activity_kind, "comment");
        assert_eq!(row.last_activity_actor, "codex");
        assert_eq!(row.last_activity_at, now - 60_000);

        // Mark all seen advances the watermark past the comment, and the
        // drained identity leaves the queue for good.
        seed_event_now(&pool).await;
        assert!(crate::store::mark_activity_seen_in_pool(&pool).await.unwrap() >= 1);
        let queue = list_attention_in_pool(&pool).await.unwrap();
        assert!(queue
            .repos
            .iter()
            .flat_map(|group| group.rows.iter())
            .all(|row| row.base_sha != "base-c1"));

        // Activity newer than the watermark fires the identity again.
        seed_discussion(&pool, "base-c1", now + 60_000, "codex").await;
        let queue = list_attention_in_pool(&pool).await.unwrap();
        let row = row_of(&queue, "base-c1");
        assert_eq!(row.category, CATEGORY_RECENT_COMMENTS);
        assert_eq!(row.last_activity_at, now + 60_000);
    }

    #[tokio::test]
    async fn recent_comments_yield_to_findings_and_rows_carry_preview_facts() {
        let pool = test_pool().await;
        seed_repo(&pool, REPO).await;

        // A request-less identity with blocking findings and fresh
        // discussion: first-match keeps it in unresolved_findings.
        seed_finding(&pool, "base-f", "P0", false).await;
        seed_discussion(&pool, "base-f", crate::store::now_millis() + 5_000, "codex").await;
        let queue = list_attention_in_pool(&pool).await.unwrap();
        let free = row_of(&queue, "base-f");
        assert_eq!(free.category, CATEGORY_UNRESOLVED_FINDINGS);
        assert_eq!(free.open_thread_count, 2, "the P0 root plus the discussion root");

        // An open request's preview rides the request mutation itself.
        let coder = agent_token(&pool, "coder-bot").await;
        let request = create_on(&pool, &Actor::Agent(coder), "base-p", "head-p").await;
        let queue = list_attention_in_pool(&pool).await.unwrap();
        let open = row_of(&queue, "base-p");
        assert_eq!(open.category, CATEGORY_REQUESTED);
        assert_eq!(open.last_activity_kind, "request");
        assert_eq!(open.last_activity_actor, "coder-bot");
        assert_eq!(open.last_activity_at, get(&pool, request.id).await.updated_at);
        assert_eq!(open.open_thread_count, 0);

        // A comment landing after the request moves the preview to the
        // comment and counts its thread.
        seed_discussion(&pool, "base-p", crate::store::now_millis() + 5_000, "reviewer").await;
        let queue = list_attention_in_pool(&pool).await.unwrap();
        let open = row_of(&queue, "base-p");
        assert_eq!(open.last_activity_kind, "comment");
        assert_eq!(open.last_activity_actor, "reviewer");
        assert!(open.last_activity_at > get(&pool, request.id).await.updated_at);
        assert_eq!(open.open_thread_count, 1);
    }
}
