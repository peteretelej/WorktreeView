// The engine's mutation surface is inert until its caller faces land (the
// MCP tools and human IPC commands); the submission observation is the one
// live path this phase.
#![allow(dead_code)]

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
// caller face shares; Phase 2's MCP schema mirrors it.
#[derive(Debug, Clone)]
pub(crate) struct RequestDraft {
    pub(crate) note: String,
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
    if draft.note.trim().is_empty() {
        return Err(invalid_request(
            "A review request needs a non-empty note.",
        ));
    }
    if draft.note.chars().count() > MAX_NOTE_CHARS {
        return Err(invalid_request(format!(
            "The note exceeds {MAX_NOTE_CHARS} characters."
        )));
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
// announce from their own sites.
fn fire_change(notify: &RequestChangeSink, request: &ReviewRequest) {
    notify(RequestChange {
        id: request.id,
        repo_path: request.repo_path.clone(),
        base_sha: request.base_sha.clone(),
        target_key: request.target_key.clone(),
        target_kind: request.target_kind.clone(),
        status: request.status.clone(),
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

fn requester_key(actor: &Actor) -> Option<i64> {
    match actor {
        Actor::Human => None,
        Actor::Agent(token_id) => Some(*token_id),
    }
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
        sqlx::query(
            "UPDATE review_requests SET note = ?, lenses = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&draft.note)
        .bind(json_array(&draft.lenses)?)
        .bind(now_millis())
        .bind(existing.id)
        .execute(pool)
        .await?;
        let updated = require_request(pool, existing.id).await?;
        fire_change(notify, &updated);
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
        sqlx::query("UPDATE review_requests SET note = ?, head_sha = ?, updated_at = ? WHERE id = ?")
            .bind(&draft.note)
            .bind(&draft.head_sha)
            .bind(now_millis())
            .bind(existing.id)
            .execute(pool)
            .await?;
        let updated = require_request(pool, existing.id).await?;
        fire_change(notify, &updated);
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
    .bind(&draft.note)
    .bind(json_array(&draft.lenses)?)
    .bind(json_array(&draft.reviewers)?)
    .bind(max_rounds)
    .bind(Some(&draft.head_sha))
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    let created = require_request(pool, result.last_insert_rowid()).await?;
    fire_change(notify, &created);
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
    set_status(pool, request.id, IN_REVIEW).await?;
    let claimed = require_request(pool, request.id).await?;
    fire_change(notify, &claimed);
    Ok(claimed)
}

// Explicit verdicts close a round and are valid only from in review, for
// any agent token or the human. There is no approved -> changes_requested
// edge: within a round the first blocking verdict wins, so a verdict after
// approved starts nothing.
pub(crate) async fn set_request_verdict_in_pool(
    pool: &SqlitePool,
    request_id: i64,
    approve: bool,
    _actor: &Actor,
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
    set_status(pool, request.id, status).await?;
    let updated = require_request(pool, request.id).await?;
    fire_change(notify, &updated);
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
    fire_change(notify, &withdrawn);
    Ok(withdrawn)
}

// The requester's reset, the only edge out of changes_requested: the round
// increments and the new head must differ from the one that received the
// changes. A refused re-request past the budget leaves the row untouched;
// needs-human is then changes_requested with round >= max_rounds.
pub(crate) async fn re_request_in_pool(
    pool: &SqlitePool,
    request_id: i64,
    head_sha: &str,
    actor: &Actor,
    notify: &RequestChangeSink,
) -> Result<ReviewRequest, CommandError> {
    if head_sha.trim().is_empty() {
        return Err(invalid_request("A re-request needs a non-empty head."));
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
    sqlx::query(
        "UPDATE review_requests \
         SET status = ?, round = round + 1, head_sha = ?, updated_at = ? WHERE id = ?",
    )
    .bind(IN_REVIEW)
    .bind(head_sha)
    .bind(now_millis())
    .bind(request.id)
    .execute(pool)
    .await?;
    let updated = require_request(pool, request.id).await?;
    fire_change(notify, &updated);
    Ok(updated)
}

// The engine's submission input, called by the ingest path after a
// submission and its findings are stored: every open request on the
// identity advances independently. A requested request is implicitly
// claimed first (round unchanged), then the submission's verdict applies:
// blocking sets changes_requested, clean approves a request now in review.
// Submissions on changes_requested, approved, or withdrawn requests change
// no status, so those requests are not in the open set at all.
pub(crate) async fn observe_submission_in_pool(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
    blocking: bool,
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
        set_status(pool, request.id, status).await?;
        request.status = status.to_string();
        fire_change(notify, &request);
    }
    Ok(())
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
            note: "Please review my changes.".into(),
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
        re_request_in_pool(pool, request_id, head, actor, &noop_notify())
            .await
            .unwrap()
    }

    async fn observe(pool: &SqlitePool, blocking: bool) {
        observe_submission_in_pool(pool, REPO, BASE, KEY, KIND, blocking, &noop_notify())
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
            re_request_in_pool(&pool, withdrawn.id, "head-11", &Actor::Human, &noop_notify()).await,
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
        updated.note = "Updated note.".into();
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
        in_review_update.note = "Still waiting.".into();
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
        moved.note = "Rebased.".into();
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
        boundary.note = "x".repeat(2000);
        assert!(attempt(boundary.clone()).await.is_ok());
        boundary.note = "x".repeat(2001);
        assert_eq!(
            attempt(boundary).await.unwrap_err().code,
            "invalid_request"
        );

        let mut empty_note = draft("head-1");
        empty_note.note = "   ".into();
        assert_eq!(attempt(empty_note).await.unwrap_err().code, "invalid_request");

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
        observe_submission_in_pool(&pool, REPO, BASE, KEY, KIND, true, &notify)
            .await
            .unwrap();
        assert_eq!(fires.lock().unwrap().len(), 2);
        assert_eq!(get(&pool, alpha.id).await.status, CHANGES_REQUESTED);
        assert_eq!(get(&pool, beta.id).await.status, CHANGES_REQUESTED);

        // Alpha re-requests; a clean submission then approves only alpha
        // while beta keeps its sticky changes_requested.
        re_request(&pool, alpha.id, "head-3", &Actor::Agent(first)).await;
        let (notify, fires) = recording_notify();
        observe_submission_in_pool(&pool, REPO, BASE, KEY, KIND, false, &notify)
            .await
            .unwrap();
        assert_eq!(fires.lock().unwrap().len(), 1);
        assert_eq!(fires.lock().unwrap()[0].id, alpha.id);
        assert_eq!(get(&pool, alpha.id).await.status, APPROVED);
        assert_eq!(get(&pool, beta.id).await.status, CHANGES_REQUESTED);

        // Submissions on terminal requests change no status.
        let (notify, fires) = recording_notify();
        observe_submission_in_pool(&pool, REPO, BASE, KEY, KIND, true, &notify)
            .await
            .unwrap();
        assert!(fires.lock().unwrap().is_empty());
        assert_eq!(get(&pool, alpha.id).await.status, APPROVED);
        assert_eq!(get(&pool, beta.id).await.status, CHANGES_REQUESTED);

        // An identity without requests observes as a no-op.
        let (notify, fires) = recording_notify();
        observe_submission_in_pool(&pool, REPO, "other-base", KEY, KIND, true, &notify)
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
        updated.note = "Updated.".into();
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

        re_request_in_pool(&pool, request.id, "head-2", &Actor::Agent(coder), &notify)
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
        observe_submission_in_pool(&pool, REPO, BASE, KEY, KIND, false, &notify)
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
}
