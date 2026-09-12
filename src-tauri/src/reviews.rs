use crate::store::now_millis;
use crate::transport::RequestChangeSink;
use crate::{agents::AgentIdentity, CommandError};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::BTreeMap;

// Who a mutating comment operation acts as: the human IPC always passes
// Human, the transport passes the authenticated agent identity. Ownership
// is enforced exactly here, never in the IPC adapters or transport
// handlers, so both faces share one policy site.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Actor {
    Human,
    Agent(AgentIdentity),
}

// Snippets capture the anchored lines at write time (uncapturable later);
// the bound keeps a comment on a huge range from bloating the store.
const SNIPPET_MAX_CHARS: usize = 2000;
// Drift re-anchoring searches this many lines on either side of the
// recorded start; beyond it the comment is outdated, not moved.
const MATCH_WINDOW: u32 = 25;

// Submission ingest caps. The 2 MiB payload cap is authoritative for
// submission size; the phase 3 transport only applies a coarser pre-parse
// guard, so an ingest-legal submission is never transport-rejected.
const MAX_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
const MAX_HTML_SECTION_BYTES: usize = 1024 * 1024;
const MAX_TEXT_BODY_BYTES: usize = 256 * 1024;
const MAX_AGENT_NAME_CHARS: usize = 200;
pub(crate) const MAX_AGENT_MODEL_CHARS: usize = 200;
const MAX_COMMAND_CONTEXT_CHARS: usize = 500;
const MAX_FINDING_TITLE_CHARS: usize = 500;
const MAX_SECTIONS: usize = 32;
const MAX_FINDINGS: usize = 200;

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Comment {
    id: i64,
    review_id: i64,
    parent_id: Option<i64>,
    author_kind: String,
    author_name: String,
    author_model: Option<String>,
    body: String,
    file_path: Option<String>,
    side: Option<String>,
    start_line: Option<u32>,
    end_line: Option<u32>,
    anchor_hash: Option<String>,
    snippet: Option<String>,
    severity: Option<String>,
    submission_id: Option<i64>,
    resolved_at: Option<i64>,
    edited_at: Option<i64>,
    created_at: i64,
}

impl Comment {
    // Read access for the transport's comment-change event; comment
    // semantics stay behind the store layer.
    pub(crate) fn id(&self) -> i64 {
        self.id
    }
}

#[derive(Debug, Deserialize)]
pub struct CommentDraft {
    body: String,
    severity: Option<String>,
    file_path: Option<String>,
    side: Option<String>,
    start_line: Option<u32>,
    end_line: Option<u32>,
    // Marker-free text of the anchored lines, top to bottom. Hash and
    // snippet are derived from it server-side.
    lines: Vec<String>,
}

// Marker-free patch line with LOGICAL line numbers: the frontend swaps
// old/new fields when the review renders reversed, so LEFT always matches
// `old_line` and RIGHT `new_line`.
#[derive(Debug, Deserialize)]
pub struct PatchLine {
    text: String,
    old_line: Option<u32>,
    new_line: Option<u32>,
}

// Sections are stored as sent (the vocabulary is app-owned and unknown
// kinds stay forward-compatible) and re-emitted typed over IPC, never as
// raw JSON.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StoredSection {
    pub kind: String,
    pub title: String,
    pub body: String,
}

// The one client-supplied block kind is `html`: static markup rendered in a
// sandboxed iframe, badged as client content.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmissionPayload {
    agent_name: String,
    agent_model: String,
    command_context: Option<String>,
    sections: Vec<StoredSection>,
    findings: Vec<SubmissionFinding>,
}

impl SubmissionPayload {
    // Read access for the transport's arrival event; submission semantics
    // stay behind the ingest.
    pub(crate) fn agent_name(&self) -> &str {
        &self.agent_name
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmissionFinding {
    title: String,
    body: String,
    file: Option<String>,
    start: Option<u32>,
    end: Option<u32>,
    priority: String,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Submission {
    id: i64,
    review_id: i64,
    agent_name: String,
    agent_model: String,
    command_context: Option<String>,
    sections: Vec<StoredSection>,
    created_at: i64,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AnchorState {
    Current,
    Moved,
    Outdated,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct AnchorStatus {
    comment_id: i64,
    state: AnchorState,
    moved_line: Option<u32>,
}

// FNV-1a 64-bit over the anchored lines' content joined with '\n',
// lowercase hex. Stable across restarts and layouts, never cryptographic:
// hashing the marker-inclusive text would flip every hash under -R.
fn anchor_hash(content: &str) -> String {
    format!("{:016x}", crate::fnv1a64(content.as_bytes()))
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn require_body(body: &str) -> Result<(), CommandError> {
    if body.trim().is_empty() {
        return Err(CommandError::new(
            "invalid_comment",
            "A comment needs a non-empty body.",
        ));
    }
    Ok(())
}

fn validate_severity(severity: &Option<String>) -> Result<(), CommandError> {
    match severity {
        None => Ok(()),
        Some(value) if matches!(value.as_str(), "P0" | "P1" | "P2" | "P3") => Ok(()),
        Some(_) => Err(CommandError::new(
            "invalid_comment",
            "Comment severity must be P0, P1, P2, or P3.",
        )),
    }
}

enum Anchor {
    Review,
    File { file_path: String },
    // Human comments bind the anchored lines' content (hash and snippet);
    // agent findings have no line content, so nothing is bound.
    Line { side: String, start_line: u32, end_line: u32, content: Option<String> },
}

// Whether a line anchor must bind the draft's line content: required for
// human comments, absent for submission findings.
enum AnchorLines {
    FromDraft,
    Absent,
}

// The authoritative anchor-shape validator for this package: phase 2's
// ingest reuses it. Mirrors the comments table CHECK constraints with
// friendly errors, and binds the hashed content to the posted lines.
fn validate_anchor(draft: &CommentDraft) -> Result<Anchor, CommandError> {
    validate_anchor_with(draft, AnchorLines::FromDraft)
}

fn validate_anchor_with(draft: &CommentDraft, lines: AnchorLines) -> Result<Anchor, CommandError> {
    require_body(&draft.body)?;
    validate_severity(&draft.severity)?;
    let invalid = |message: &str| CommandError::new("invalid_comment", message);
    let Some(file_path) = draft.file_path.clone() else {
        if draft.side.is_some()
            || draft.start_line.is_some()
            || draft.end_line.is_some()
            || !draft.lines.is_empty()
        {
            return Err(invalid(
                "A review-level comment cannot carry a file or line anchor.",
            ));
        }
        return Ok(Anchor::Review);
    };
    if draft.side.is_none() && draft.start_line.is_none() && draft.end_line.is_none() {
        if !draft.lines.is_empty() {
            return Err(invalid("A file-level comment cannot carry line content."));
        }
        return Ok(Anchor::File { file_path });
    }
    let Some(side) = draft.side.clone() else {
        return Err(invalid("A line comment needs a diff side."));
    };
    if side != "LEFT" && side != "RIGHT" {
        return Err(invalid("A line comment side must be LEFT or RIGHT."));
    }
    let Some(start_line) = draft.start_line else {
        return Err(invalid("A line comment needs a start line."));
    };
    if start_line == 0 {
        return Err(invalid("Line anchors start at line 1."));
    }
    let end_line = draft.end_line.unwrap_or(start_line);
    if end_line < start_line {
        return Err(invalid("A line anchor's end line cannot precede its start."));
    }
    let content = match lines {
        AnchorLines::FromDraft => {
            if draft.lines.is_empty() {
                return Err(invalid("A line comment needs the anchored lines' content."));
            }
            Some(draft.lines.join("\n"))
        }
        AnchorLines::Absent => None,
    };
    Ok(Anchor::Line {
        side,
        start_line,
        end_line,
        content,
    })
}

async fn find_review_id(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM reviews \
         WHERE repo_path = ? AND base_sha = ? AND target_key = ? AND target_kind = ?",
    )
    .bind(repo_path)
    .bind(base_sha)
    .bind(target_key)
    .bind(target_kind)
    .fetch_optional(pool)
    .await
}

// Reviews are keyed on resolved SHAs (worktree targets on the worktree
// path), so a moved branch naturally starts a new comment session.
async fn resolve_review_id(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
) -> Result<i64, CommandError> {
    if let Some(id) = find_review_id(pool, repo_path, base_sha, target_key, target_kind).await? {
        return Ok(id);
    }
    sqlx::query(
        "INSERT INTO reviews (repo_path, base_sha, target_key, target_kind, created_at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (repo_path, base_sha, target_key, target_kind) DO NOTHING",
    )
    .bind(repo_path)
    .bind(base_sha)
    .bind(target_key)
    .bind(target_kind)
    .bind(now_millis())
    .execute(pool)
    .await?;
    find_review_id(pool, repo_path, base_sha, target_key, target_kind)
        .await?
        .ok_or_else(|| CommandError::new("persistence", "The review could not be recorded."))
}

fn comment_from_row(row: &sqlx::sqlite::SqliteRow) -> sqlx::Result<Comment> {
    let start_line: Option<i64> = row.try_get("start_line")?;
    let end_line: Option<i64> = row.try_get("end_line")?;
    Ok(Comment {
        id: row.try_get("id")?,
        review_id: row.try_get("review_id")?,
        parent_id: row.try_get("parent_id")?,
        author_kind: row.try_get("author_kind")?,
        author_name: row.try_get("author_name")?,
        author_model: row.try_get("author_model")?,
        body: row.try_get("body")?,
        file_path: row.try_get("file_path")?,
        side: row.try_get("side")?,
        start_line: start_line.map(u32::try_from).transpose().unwrap_or(None),
        end_line: end_line.map(u32::try_from).transpose().unwrap_or(None),
        anchor_hash: row.try_get("anchor_hash")?,
        snippet: row.try_get("snippet")?,
        severity: row.try_get("severity")?,
        submission_id: row.try_get("submission_id")?,
        resolved_at: row.try_get("resolved_at")?,
        edited_at: row.try_get("edited_at")?,
        created_at: row.try_get("created_at")?,
    })
}

async fn load_comment(pool: &SqlitePool, id: i64) -> Result<Option<Comment>, sqlx::Error> {
    sqlx::query(
        "SELECT id, review_id, parent_id, author_kind, author_name, author_model, body, \
                file_path, side, start_line, end_line, anchor_hash, snippet, severity, \
                submission_id, resolved_at, edited_at, created_at \
         FROM comments WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .map(|row| comment_from_row(&row))
    .transpose()
}

fn comment_not_found() -> CommandError {
    CommandError::new("invalid_comment", "The comment does not exist.")
}

// An agent may edit or delete only comments it authored through its own
// token; human comments and legacy unowned comments are never
// agent-mutable. The error code is distinct from shape errors so clients
// can tell "not your comment" from a malformed request.
fn ensure_agent_may_mutate(actor: &Actor, author_token_id: Option<i64>) -> Result<(), CommandError> {
    match actor {
        Actor::Human => Ok(()),
        Actor::Agent(identity) if author_token_id == Some(identity.token_id) => Ok(()),
        Actor::Agent(_) => Err(CommandError::new(
            "not_comment_owner",
            "Only the agent token that authored a comment can edit or delete it.",
        )),
    }
}

async fn comment_owner_token(pool: &SqlitePool, comment_id: i64) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT author_token_id FROM comments WHERE id = ?")
        .bind(comment_id)
        .fetch_one(pool)
        .await
}

// The review identity behind a comment, for the transport's comment-change
// event: the tool handlers know only a comment id. A delete reads it before
// the row goes, so the join must not outlive the comment.
pub(crate) struct ReviewIdentity {
    pub(crate) repo_path: String,
    pub(crate) base_sha: String,
    pub(crate) target_key: String,
    pub(crate) target_kind: String,
}

pub(crate) async fn review_identity_of_comment(
    pool: &SqlitePool,
    comment_id: i64,
) -> Result<Option<ReviewIdentity>, sqlx::Error> {
    let row: Option<(String, String, String, String)> = sqlx::query_as(
        "SELECT reviews.repo_path, reviews.base_sha, reviews.target_key, reviews.target_kind \
         FROM comments JOIN reviews ON reviews.id = comments.review_id \
         WHERE comments.id = ?",
    )
    .bind(comment_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(repo_path, base_sha, target_key, target_kind)| ReviewIdentity {
        repo_path,
        base_sha,
        target_key,
        target_kind,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn insert_comment(
    pool: &SqlitePool,
    review_id: i64,
    parent_id: Option<i64>,
    actor: &Actor,
    author_model: Option<&str>,
    body: &str,
    severity: Option<&str>,
    file_path: Option<String>,
    side: Option<String>,
    start_line: Option<u32>,
    end_line: Option<u32>,
    anchor: Option<Anchor>,
) -> Result<Comment, CommandError> {
    let (hash, snippet) = match anchor {
        Some(Anchor::Line { content: Some(content), .. }) => (
            Some(anchor_hash(&content)),
            Some(truncate_chars(&content, SNIPPET_MAX_CHARS)),
        ),
        _ => (None, None),
    };
    let (author_kind, author_name, author_token_id) = match actor {
        Actor::Human => ("human", "you", None),
        Actor::Agent(identity) => ("agent", identity.name.as_str(), Some(identity.token_id)),
    };
    let result = sqlx::query(
        "INSERT INTO comments (review_id, parent_id, author_kind, author_name, author_model, author_token_id, \
                body, file_path, side, start_line, end_line, anchor_hash, snippet, severity, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(review_id)
    .bind(parent_id)
    .bind(author_kind)
    .bind(author_name)
    .bind(author_model)
    .bind(author_token_id)
    .bind(body)
    .bind(file_path)
    .bind(side)
    .bind(start_line.map(i64::from))
    .bind(end_line.map(i64::from))
    .bind(hash)
    .bind(snippet)
    .bind(severity)
    .bind(now_millis())
    .execute(pool)
    .await?;
    load_comment(pool, result.last_insert_rowid())
        .await?
        .ok_or_else(comment_not_found)
}

async fn create_comment_with_anchor(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
    draft: &CommentDraft,
    anchor: Anchor,
    actor: &Actor,
    author_model: Option<&str>,
) -> Result<Comment, CommandError> {
    let review_id =
        resolve_review_id(pool, repo_path, base_sha, target_key, target_kind).await?;
    let (file_path, side, start_line, end_line) = match &anchor {
        Anchor::Review => (None, None, None, None),
        Anchor::File { file_path } => (Some(file_path.clone()), None, None, None),
        Anchor::Line {
            side,
            start_line,
            end_line,
            ..
        } => (
            draft.file_path.clone(),
            Some(side.clone()),
            Some(*start_line),
            Some(*end_line),
        ),
    };
    insert_comment(
        pool,
        review_id,
        None,
        actor,
        author_model,
        &draft.body,
        draft.severity.as_deref(),
        file_path,
        side,
        start_line,
        end_line,
        Some(anchor),
    )
    .await
}

pub(crate) async fn create_comment_in_pool(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
    draft: &CommentDraft,
    actor: &Actor,
    author_model: Option<&str>,
) -> Result<Comment, CommandError> {
    let anchor = validate_anchor(draft)?;
    create_comment_with_anchor(pool, repo_path, base_sha, target_key, target_kind, draft, anchor, actor, author_model)
        .await
}

// The MCP tool path's create: same validator, same caps, same anchor shapes
// as the human path, but the tool call carries no line content, so line
// anchors bind nothing (like ingested findings) and drift matching never
// applies to them.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_unbound_comment_in_pool(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
    body: &str,
    severity: Option<String>,
    file_path: Option<String>,
    side: Option<String>,
    start_line: Option<u32>,
    end_line: Option<u32>,
    actor: &Actor,
    author_model: Option<&str>,
) -> Result<Comment, CommandError> {
    let draft = CommentDraft {
        body: body.to_string(),
        severity,
        file_path,
        side,
        start_line,
        end_line,
        lines: Vec::new(),
    };
    let anchor = validate_anchor_with(&draft, AnchorLines::Absent)?;
    create_comment_with_anchor(pool, repo_path, base_sha, target_key, target_kind, &draft, anchor, actor, author_model)
        .await
}

pub(crate) async fn list_comments_in_pool(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
) -> Result<Vec<Comment>, CommandError> {
    let Some(review_id) =
        find_review_id(pool, repo_path, base_sha, target_key, target_kind).await?
    else {
        return Ok(Vec::new());
    };
    let rows = sqlx::query(
        "SELECT id, review_id, parent_id, author_kind, author_name, author_model, body, \
                file_path, side, start_line, end_line, anchor_hash, snippet, severity, \
                submission_id, resolved_at, edited_at, created_at \
         FROM comments WHERE review_id = ? ORDER BY created_at, id",
    )
    .bind(review_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(comment_from_row)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

// Threads stay one root plus flat replies: replying to a reply is refused.
// Any actor may reply to any comment; the reply records its actor.
pub(crate) async fn reply_comment_in_pool(
    pool: &SqlitePool,
    parent_id: i64,
    body: &str,
    severity: Option<String>,
    actor: &Actor,
    author_model: Option<&str>,
) -> Result<Comment, CommandError> {
    require_body(body)?;
    validate_severity(&severity)?;
    let parent = load_comment(pool, parent_id).await?.ok_or_else(comment_not_found)?;
    if parent.parent_id.is_some() {
        return Err(CommandError::new(
            "invalid_comment",
            "Replies can only be added to a thread's root comment.",
        ));
    }
    insert_comment(
        pool,
        parent.review_id,
        Some(parent.id),
        actor,
        author_model,
        body,
        severity.as_deref(),
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

// Resolve/reopen lives on the root; replies follow their thread. Resolving
// is visible and reversible, so any actor may resolve any thread.
pub(crate) async fn set_comment_resolved_in_pool(
    pool: &SqlitePool,
    comment_id: i64,
    resolved: bool,
    _actor: &Actor,
) -> Result<Comment, CommandError> {
    let comment = load_comment(pool, comment_id).await?.ok_or_else(comment_not_found)?;
    if comment.parent_id.is_some() {
        return Err(CommandError::new(
            "invalid_comment",
            "Only a thread's root comment can be resolved.",
        ));
    }
    let resolved_at = resolved.then(now_millis);
    sqlx::query("UPDATE comments SET resolved_at = ? WHERE id = ?")
        .bind(resolved_at)
        .bind(comment_id)
        .execute(pool)
        .await?;
    load_comment(pool, comment_id).await?.ok_or_else(comment_not_found)
}

// Edits are last-write-wins on the body; no history is kept.
pub(crate) async fn edit_comment_in_pool(
    pool: &SqlitePool,
    comment_id: i64,
    body: &str,
    actor: &Actor,
) -> Result<Comment, CommandError> {
    require_body(body)?;
    load_comment(pool, comment_id).await?.ok_or_else(comment_not_found)?;
    ensure_agent_may_mutate(actor, comment_owner_token(pool, comment_id).await?)?;
    sqlx::query("UPDATE comments SET body = ?, edited_at = ? WHERE id = ?")
        .bind(body)
        .bind(now_millis())
        .bind(comment_id)
        .execute(pool)
        .await?;
    load_comment(pool, comment_id).await?.ok_or_else(comment_not_found)
}

// Deletion is permanent and cascades: removing a root takes its replies.
pub(crate) async fn delete_comment_in_pool(
    pool: &SqlitePool,
    comment_id: i64,
    actor: &Actor,
) -> Result<(), CommandError> {
    load_comment(pool, comment_id).await?.ok_or_else(comment_not_found)?;
    ensure_agent_may_mutate(actor, comment_owner_token(pool, comment_id).await?)?;
    sqlx::query("DELETE FROM comments WHERE id = ?")
        .bind(comment_id)
        .execute(pool)
        .await?;
    Ok(())
}

struct LineAnchor {
    comment_id: i64,
    side: String,
    start_line: u32,
    end_line: u32,
    anchor_hash: String,
}

// Side-scoped view of the loaded patch: LEFT reads old_line, RIGHT reads
// new_line. Numbered lines only, which keeps metadata rows out.
fn side_lines<'a>(lines: &'a [PatchLine], side: &str) -> BTreeMap<u32, &'a str> {
    lines
        .iter()
        .filter_map(|line| {
            let number = if side == "LEFT" { line.old_line } else { line.new_line };
            number.map(|number| (number, line.text.as_str()))
        })
        .collect()
}

// Hash of the lines currently present in start..=end on one side; None
// when the range holds no rendered lines at all. Blank lines keep their
// empty content so the joined text matches the write-time hash exactly.
fn range_hash(lines: &BTreeMap<u32, &str>, start: u32, end: u32) -> Option<String> {
    let texts: Vec<&str> = lines.range(start..=end).map(|(_, text)| *text).collect();
    if texts.is_empty() {
        None
    } else {
        Some(anchor_hash(&texts.join("\n")))
    }
}

// Same line + same hash is current; the nearest equal-hash range within
// the window re-anchors the display; anything else is outdated. File-level
// and review-level comments never enter here.
fn match_anchor(anchor: &LineAnchor, lines: &BTreeMap<u32, &str>) -> AnchorStatus {
    if range_hash(lines, anchor.start_line, anchor.end_line)
        .is_some_and(|hash| hash == anchor.anchor_hash)
    {
        return AnchorStatus {
            comment_id: anchor.comment_id,
            state: AnchorState::Current,
            moved_line: None,
        };
    }
    let length = anchor.end_line - anchor.start_line;
    let lowest = anchor.start_line.saturating_sub(MATCH_WINDOW);
    let highest = anchor.start_line.saturating_add(MATCH_WINDOW);
    let mut nearest: Option<(u32, u32)> = None;
    for candidate in lowest..=highest {
        if candidate == anchor.start_line {
            continue;
        }
        let Some(end) = candidate.checked_add(length) else {
            continue;
        };
        let Some(hash) = range_hash(lines, candidate, end) else {
            continue;
        };
        if hash != anchor.anchor_hash {
            continue;
        }
        let distance = candidate.abs_diff(anchor.start_line);
        if nearest.is_none_or(|(best, _)| distance < best) {
            nearest = Some((distance, candidate));
        }
    }
    match nearest {
        Some((_, start)) => AnchorStatus {
            comment_id: anchor.comment_id,
            state: AnchorState::Moved,
            moved_line: Some(start),
        },
        None => AnchorStatus {
            comment_id: anchor.comment_id,
            state: AnchorState::Outdated,
            moved_line: None,
        },
    }
}

pub(crate) async fn match_comment_anchors_in_pool(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
    file_path: &str,
    lines: &[PatchLine],
) -> Result<Vec<AnchorStatus>, CommandError> {
    let Some(review_id) =
        find_review_id(pool, repo_path, base_sha, target_key, target_kind).await?
    else {
        return Ok(Vec::new());
    };
    let rows = sqlx::query(
        "SELECT id, side, start_line, COALESCE(end_line, start_line) AS end_line, anchor_hash \
         FROM comments \
         WHERE review_id = ? AND file_path = ? AND side IS NOT NULL \
               AND start_line IS NOT NULL AND anchor_hash IS NOT NULL \
         ORDER BY id",
    )
    .bind(review_id)
    .bind(file_path)
    .fetch_all(pool)
    .await?;
    let mut side_maps: BTreeMap<String, BTreeMap<u32, &str>> = BTreeMap::new();
    let mut statuses = Vec::new();
    for row in &rows {
        let start_line: i64 = row.try_get("start_line")?;
        let end_line: i64 = row.try_get("end_line")?;
        let anchor = LineAnchor {
            comment_id: row.try_get("id")?,
            side: row.try_get("side")?,
            start_line: u32::try_from(start_line).unwrap_or(0),
            end_line: u32::try_from(end_line).unwrap_or(0),
            anchor_hash: row.try_get("anchor_hash")?,
        };
        if anchor.end_line < anchor.start_line {
            continue;
        }
        let lines = side_maps
            .entry(anchor.side.clone())
            .or_insert_with(|| side_lines(lines, &anchor.side));
        statuses.push(match_anchor(&anchor, lines));
    }
    Ok(statuses)
}

fn invalid_submission(message: impl Into<String>) -> CommandError {
    CommandError::new("invalid_submission", message)
}

// Anchoring errors come from the shared comment validator under its
// invalid_comment code; a finding's anchor is a submission-shape violation,
// so the code is relabeled while the message passes through.
fn recode_submission(error: CommandError) -> CommandError {
    CommandError {
        code: "invalid_submission".into(),
        message: error.message,
    }
}

// Resolves a finding to its comment anchor columns through the shared
// validator. Line-anchored findings always anchor RIGHT (the new side); the
// payload carries no side field.
fn finding_anchor(
    finding: &SubmissionFinding,
) -> Result<(Option<String>, Option<String>, Option<i64>, Option<i64>), CommandError> {
    let line_anchored = finding.file.is_some() && finding.start.is_some();
    let draft = CommentDraft {
        body: finding.body.clone(),
        severity: Some(finding.priority.clone()),
        file_path: finding.file.clone(),
        side: line_anchored.then(|| "RIGHT".to_string()),
        start_line: finding.start,
        end_line: finding.end.or(finding.start),
        lines: Vec::new(),
    };
    let anchor = validate_anchor_with(&draft, AnchorLines::Absent).map_err(recode_submission)?;
    Ok(match anchor {
        Anchor::Review => (None, None, None, None),
        Anchor::File { file_path } => (Some(file_path), None, None, None),
        Anchor::Line {
            side,
            start_line,
            end_line,
            ..
        } => (
            draft.file_path.clone(),
            Some(side),
            Some(i64::from(start_line)),
            Some(i64::from(end_line)),
        ),
    })
}

fn require_non_empty(value: &str, what: &str) -> Result<(), CommandError> {
    if value.trim().is_empty() {
        return Err(invalid_submission(format!("{what} must be a non-empty string.")));
    }
    Ok(())
}

fn require_char_cap(value: &str, cap: usize, what: &str) -> Result<(), CommandError> {
    if value.chars().count() > cap {
        return Err(invalid_submission(format!("{what} exceeds {cap} characters.")));
    }
    Ok(())
}

fn validate_submission(payload: &SubmissionPayload) -> Result<(), CommandError> {
    let serialized = serde_json::to_vec(payload)
        .map_err(|_| invalid_submission("The submission could not be serialized."))?;
    if serialized.len() > MAX_PAYLOAD_BYTES {
        return Err(invalid_submission(
            "The submission exceeds the 2 MiB payload cap.",
        ));
    }
    require_non_empty(&payload.agent_name, "The agent name")?;
    require_char_cap(&payload.agent_name, MAX_AGENT_NAME_CHARS, "The agent name")?;
    require_non_empty(&payload.agent_model, "The agent model")?;
    require_char_cap(&payload.agent_model, MAX_AGENT_MODEL_CHARS, "The agent model")?;
    if let Some(context) = &payload.command_context {
        require_char_cap(context, MAX_COMMAND_CONTEXT_CHARS, "The command context")?;
    }
    if payload.sections.len() > MAX_SECTIONS {
        return Err(invalid_submission(format!(
            "The submission exceeds {MAX_SECTIONS} sections."
        )));
    }
    if payload.findings.len() > MAX_FINDINGS {
        return Err(invalid_submission(format!(
            "The submission exceeds {MAX_FINDINGS} findings."
        )));
    }
    for section in &payload.sections {
        let (cap, what) = if section.kind == "html" {
            (MAX_HTML_SECTION_BYTES, "The html section body")
        } else {
            (MAX_TEXT_BODY_BYTES, "A section body")
        };
        if section.body.len() > cap {
            return Err(invalid_submission(format!(
                "{what} exceeds the {cap} byte cap."
            )));
        }
    }
    for finding in &payload.findings {
        require_non_empty(&finding.title, "A finding title")?;
        require_char_cap(&finding.title, MAX_FINDING_TITLE_CHARS, "A finding title")?;
        if finding.body.len() > MAX_TEXT_BODY_BYTES {
            return Err(invalid_submission(format!(
                "A finding body exceeds the {MAX_TEXT_BODY_BYTES} byte cap."
            )));
        }
        finding_anchor(finding)?;
    }
    Ok(())
}

// The only writer of submissions and agent comments: the loopback
// transport's post_review handler calls this with the authenticated
// agent actor, so ingested finding comments are owned by that token.
// The submission and every finding comment commit atomically or not at
// all. After storage, the request engine observes the submission: every
// open review request on the identity advances (implicit claim plus the
// blocking verdict), flag computed here from the engine's single-site
// blocking definition.
pub(crate) async fn ingest_submission_in_pool(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
    payload: &SubmissionPayload,
    author: &Actor,
    request_changes: &RequestChangeSink,
) -> Result<i64, CommandError> {
    let author_token_id = match author {
        Actor::Human => None,
        Actor::Agent(identity) => Some(identity.token_id),
    };
    validate_submission(payload)?;
    // The target repository must exist before anything is written; reviews
    // key on a repos row that cascades away with it.
    let known: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repos WHERE path = ?")
        .bind(repo_path)
        .fetch_one(pool)
        .await?;
    if known == 0 {
        return Err(CommandError::new(
            "unknown_review_target",
            "No repository with that path is open in WorktreeView.",
        ));
    }
    let review_id =
        resolve_review_id(pool, repo_path, base_sha, target_key, target_kind).await?;
    let mut tx = pool.begin().await?;
    let result = sqlx::query(
        "INSERT INTO submissions (review_id, agent_name, agent_model, command_context, \
                sections_json, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(review_id)
    .bind(payload.agent_name.as_str())
    .bind(payload.agent_model.as_str())
    .bind(payload.command_context.as_deref())
    .bind(serde_json::to_string(&payload.sections).map_err(|_| {
        invalid_submission("The submission sections could not be serialized.")
    })?)
    .bind(now_millis())
    .execute(&mut *tx)
    .await?;
    let submission_id = result.last_insert_rowid();
    for finding in &payload.findings {
        let (file_path, side, start_line, end_line) = finding_anchor(finding)?;
        sqlx::query(
            "INSERT INTO comments (review_id, author_kind, author_name, author_model, \
                    author_token_id, body, file_path, side, start_line, end_line, severity, \
                    submission_id, created_at) \
             VALUES (?, 'agent', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(review_id)
        .bind(payload.agent_name.as_str())
        .bind(payload.agent_model.as_str())
        .bind(author_token_id)
        .bind(finding.body.as_str())
        .bind(file_path)
        .bind(side)
        .bind(start_line)
        .bind(end_line)
        .bind(Some(finding.priority.as_str()))
        .bind(submission_id)
        .bind(now_millis())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let blocking = payload
        .findings
        .iter()
        .any(|finding| crate::requests::is_blocking_severity(&finding.priority));
    crate::requests::observe_submission_in_pool(
        pool,
        repo_path,
        base_sha,
        target_key,
        target_kind,
        blocking,
        request_changes,
    )
    .await?;
    Ok(submission_id)
}

pub(crate) async fn list_submissions_in_pool(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
) -> Result<Vec<Submission>, CommandError> {
    let Some(review_id) =
        find_review_id(pool, repo_path, base_sha, target_key, target_kind).await?
    else {
        return Ok(Vec::new());
    };
    let rows = sqlx::query(
        "SELECT id, review_id, agent_name, agent_model, command_context, sections_json, created_at \
         FROM submissions WHERE review_id = ? ORDER BY created_at, id",
    )
    .bind(review_id)
    .fetch_all(pool)
    .await?;
    let mut submissions = Vec::new();
    for row in &rows {
        let sections_json: String = row.try_get("sections_json")?;
        let sections: Vec<StoredSection> = serde_json::from_str(&sections_json).map_err(|error| {
            CommandError::new(
                "persistence",
                format!("A stored submission could not be read: {error}"),
            )
        })?;
        submissions.push(Submission {
            id: row.try_get("id")?,
            review_id: row.try_get("review_id")?,
            agent_name: row.try_get("agent_name")?,
            agent_model: row.try_get("agent_model")?,
            command_context: row.try_get("command_context")?,
            sections,
            created_at: row.try_get("created_at")?,
        });
    }
    Ok(submissions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{seed_repo, test_pool};

    const HUMAN: Actor = Actor::Human;

    fn agent(token_id: i64) -> Actor {
        Actor::Agent(AgentIdentity { token_id, name: "reviewer-bot".into() })
    }

    fn draft(body: &str) -> CommentDraft {
        CommentDraft {
            body: body.into(),
            severity: None,
            file_path: None,
            side: None,
            start_line: None,
            end_line: None,
            lines: Vec::new(),
        }
    }

    fn line_draft(body: &str, side: &str, start: u32, end: u32, lines: &[&str]) -> CommentDraft {
        CommentDraft {
            body: body.into(),
            severity: None,
            file_path: Some("file.txt".into()),
            side: Some(side.into()),
            start_line: Some(start),
            end_line: Some(end),
            lines: lines.iter().map(|line| line.to_string()).collect(),
        }
    }

    fn patch(text: &str, old_line: Option<u32>, new_line: Option<u32>) -> PatchLine {
        PatchLine {
            text: text.into(),
            old_line,
            new_line,
        }
    }

    #[tokio::test]
    async fn comments_round_trip_through_the_store() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let review = resolve_review_id(&pool, "/demo", "base", "/demo", "worktree")
            .await
            .unwrap();
        create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &draft("review note"), &HUMAN, None)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO comments (review_id, author_kind, author_name, body, file_path, created_at) \
             VALUES (?, 'human', 'you', 'file note', 'file.txt', 1)",
        )
        .bind(review)
        .execute(&pool)
        .await
        .unwrap();
        let line = create_comment_in_pool(
            &pool,
            "/demo",
            "base",
            "/demo",
            "worktree",
            &line_draft("line note", "RIGHT", 3, 4, &["x", "y"]),
        &HUMAN, None)
        .await
        .unwrap();
        assert_eq!(line.side.as_deref(), Some("RIGHT"));
        assert_eq!(line.start_line, Some(3));
        assert_eq!(line.end_line, Some(4));
        assert!(line.anchor_hash.is_some());
        assert_eq!(line.snippet.as_deref(), Some("x\ny"));
        assert_eq!(line.author_kind, "human");
        assert_eq!(line.author_name, "you");

        let reply =
            reply_comment_in_pool(&pool, line.id, "a reply", Some("P1".into()), &HUMAN, None).await.unwrap();
        assert_eq!(reply.parent_id, Some(line.id));
        assert_eq!(reply.severity.as_deref(), Some("P1"));
        assert_eq!(reply.file_path, None);

        let resolved = set_comment_resolved_in_pool(&pool, line.id, true, &HUMAN).await.unwrap();
        assert!(resolved.resolved_at.is_some());
        let reopened = set_comment_resolved_in_pool(&pool, line.id, false, &HUMAN).await.unwrap();
        assert!(reopened.resolved_at.is_none());

        let edited = edit_comment_in_pool(&pool, line.id, "edited note", &HUMAN).await.unwrap();
        assert_eq!(edited.body, "edited note");
        assert!(edited.edited_at.is_some());

        let listed = list_comments_in_pool(&pool, "/demo", "base", "/demo", "worktree")
            .await
            .unwrap();
        assert_eq!(listed.len(), 4);
        let unknown = list_comments_in_pool(&pool, "/demo", "base", "/missing", "head")
            .await
            .unwrap();
        assert!(unknown.is_empty());
    }

    #[tokio::test]
    async fn reply_to_reply_and_resolve_on_reply_are_rejected() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let root = create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &draft("root"), &HUMAN, None)
            .await
            .unwrap();
        let reply = reply_comment_in_pool(&pool, root.id, "first reply", None, &HUMAN, None).await.unwrap();
        let nested = reply_comment_in_pool(&pool, reply.id, "nested", None, &HUMAN, None).await;
        assert_eq!(nested.unwrap_err().code, "invalid_comment");
        let resolve_reply = set_comment_resolved_in_pool(&pool, reply.id, true, &HUMAN).await;
        assert_eq!(resolve_reply.unwrap_err().code, "invalid_comment");
    }

    #[tokio::test]
    async fn deleting_a_root_takes_its_replies_and_unknown_ids_are_rejected() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let root = create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &draft("root"), &HUMAN, None)
            .await
            .unwrap();
        let reply = reply_comment_in_pool(&pool, root.id, "reply", None, &HUMAN, None).await.unwrap();
        delete_comment_in_pool(&pool, root.id, &HUMAN).await.unwrap();
        let listed = list_comments_in_pool(&pool, "/demo", "base", "/demo", "worktree")
            .await
            .unwrap();
        assert!(listed.iter().all(|comment| comment.id != root.id && comment.id != reply.id));
        let missing = delete_comment_in_pool(&pool, root.id, &HUMAN).await;
        assert_eq!(missing.unwrap_err().code, "invalid_comment");
    }

    #[tokio::test]
    async fn review_identity_reuses_rows_and_separates_targets() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let first = resolve_review_id(&pool, "/demo", "base", "/demo", "worktree")
            .await
            .unwrap();
        let again = resolve_review_id(&pool, "/demo", "base", "/demo", "worktree")
            .await
            .unwrap();
        assert_eq!(first, again);
        let head = resolve_review_id(&pool, "/demo", "base", "a".repeat(40).as_str(), "head")
            .await
            .unwrap();
        assert_ne!(first, head);
        let moved = resolve_review_id(&pool, "/demo", "base2", "/demo", "worktree")
            .await
            .unwrap();
        assert_ne!(first, moved);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM reviews")
                .fetch_one(&pool)
                .await
                .unwrap(),
            3
        );
    }

    #[tokio::test]
    async fn anchor_validation_rejects_broken_shapes() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let mut no_body = draft("  ");
        assert_eq!(
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &no_body, &HUMAN, None)
                .await
                .unwrap_err()
                .code,
            "invalid_comment"
        );
        no_body.body = "ok".into();
        no_body.severity = Some("P9".into());
        assert_eq!(
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &no_body, &HUMAN, None)
                .await
                .unwrap_err()
                .code,
            "invalid_comment"
        );
        let mut anchored_review = draft("review note");
        anchored_review.file_path = Some("file.txt".into());
        anchored_review.start_line = Some(1);
        assert_eq!(
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &anchored_review, &HUMAN, None)
                .await
                .unwrap_err()
                .code,
            "invalid_comment"
        );
        let mut file_with_lines = draft("file note");
        file_with_lines.file_path = Some("file.txt".into());
        file_with_lines.lines = vec!["x".into()];
        assert_eq!(
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &file_with_lines, &HUMAN, None)
                .await
                .unwrap_err()
                .code,
            "invalid_comment"
        );
        let backwards = line_draft("backwards", "LEFT", 5, 4, &["x"]);
        assert_eq!(
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &backwards, &HUMAN, None)
                .await
                .unwrap_err()
                .code,
            "invalid_comment"
        );
        let no_lines = line_draft("empty", "RIGHT", 3, 3, &[]);
        assert_eq!(
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &no_lines, &HUMAN, None)
                .await
                .unwrap_err()
                .code,
            "invalid_comment"
        );
    }

    #[tokio::test]
    async fn deleting_the_repo_cascades_reviews_submissions_and_comments() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let comment = create_comment_in_pool(
            &pool,
            "/demo",
            "base",
            "/demo",
            "worktree",
            &line_draft("note", "RIGHT", 1, 1, &["x"]),
        &HUMAN, None)
        .await
        .unwrap();
        let review = comment.review_id;
        sqlx::query(
            "INSERT INTO submissions (review_id, agent_name, agent_model, sections_json, created_at) \
             VALUES (?, 'agent', 'model', '{}', 1)",
        )
        .bind(review)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("DELETE FROM repos WHERE path = '/demo'")
            .execute(&pool)
            .await
            .unwrap();

        let review_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM reviews").fetch_one(&pool).await.unwrap();
        assert_eq!(review_count, 0, "review rows must cascade away with the repo");
        let submission_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM submissions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(submission_count, 0, "submission rows must cascade away with the repo");
        let comment_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM comments").fetch_one(&pool).await.unwrap();
        assert_eq!(comment_count, 0, "comment rows must cascade away with the repo");
    }

    #[tokio::test]
    async fn anchor_hash_is_layout_stable_and_snippet_is_bounded() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        // The same content arriving as an old-side line (unreversed layout)
        // and as a new-side line (reversed layout) must hash identically.
        let old_side = line_draft("left", "LEFT", 2, 3, &["x", "y"]);
        let new_side = line_draft("right", "RIGHT", 2, 3, &["x", "y"]);
        let left = create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &old_side, &HUMAN, None)
            .await
            .unwrap();
        let right = create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &new_side, &HUMAN, None)
            .await
            .unwrap();
        assert_eq!(left.anchor_hash, right.anchor_hash);
        assert_eq!(left.anchor_hash.as_deref().map(str::len), Some(16));
        assert!(left.anchor_hash.as_ref().unwrap().bytes().all(|byte| byte.is_ascii_hexdigit()));

        let long = "é".repeat(2500);
        let mut oversized = line_draft("huge", "RIGHT", 1, 1, &[long.as_str()]);
        oversized.file_path = Some("big.txt".into());
        let stored =
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &oversized, &HUMAN, None)
                .await
                .unwrap();
        assert_eq!(stored.snippet.as_deref().map(|text| text.chars().count()), Some(2000));
        let differing = line_draft("other", "RIGHT", 5, 5, &["x", "z"]);
        let other =
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &differing, &HUMAN, None)
                .await
                .unwrap();
        assert_ne!(other.anchor_hash, right.anchor_hash);
    }

    #[tokio::test]
    async fn matcher_reports_current_when_content_is_unchanged() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let comment = create_comment_in_pool(
            &pool,
            "/demo",
            "base",
            "/demo",
            "worktree",
            &line_draft("note", "RIGHT", 3, 4, &["x", "y"]),
        &HUMAN, None)
        .await
        .unwrap();
        let lines = [
            patch("a", Some(1), Some(1)),
            patch("b", Some(2), Some(2)),
            patch("x", Some(3), Some(3)),
            patch("y", Some(4), Some(4)),
        ];
        let statuses = match_comment_anchors_in_pool(
            &pool, "/demo", "base", "/demo", "worktree", "file.txt", &lines,
        )
        .await
        .unwrap();
        assert_eq!(
            statuses,
            [AnchorStatus {
                comment_id: comment.id,
                state: AnchorState::Current,
                moved_line: None,
            }]
        );
    }

    #[tokio::test]
    async fn matcher_matches_anchors_starting_on_blank_lines() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        // git renders blank diff rows as a one-space marker the frontend
        // strips to "", so the write-time content keeps those empty lines.
        let comment = create_comment_in_pool(
            &pool,
            "/demo",
            "base",
            "/demo",
            "worktree",
            &line_draft("note", "RIGHT", 2, 3, &["", "x"]),
        &HUMAN, None)
        .await
        .unwrap();
        let blank = create_comment_in_pool(
            &pool,
            "/demo",
            "base",
            "/demo",
            "worktree",
            &line_draft("blank note", "RIGHT", 5, 5, &[""]),
        &HUMAN, None)
        .await
        .unwrap();
        let lines = [
            patch("a", Some(1), Some(1)),
            patch("", Some(2), Some(2)),
            patch("x", Some(3), Some(3)),
            patch("y", Some(4), Some(4)),
            patch("", Some(5), Some(5)),
            patch("b", Some(6), Some(6)),
        ];
        let statuses = match_comment_anchors_in_pool(
            &pool, "/demo", "base", "/demo", "worktree", "file.txt", &lines,
        )
        .await
        .unwrap();
        assert_eq!(
            statuses,
            [
                AnchorStatus {
                    comment_id: comment.id,
                    state: AnchorState::Current,
                    moved_line: None,
                },
                AnchorStatus {
                    comment_id: blank.id,
                    state: AnchorState::Current,
                    moved_line: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn matcher_reanchors_to_the_nearest_duplicate_within_the_window() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let comment = create_comment_in_pool(
            &pool,
            "/demo",
            "base",
            "/demo",
            "worktree",
            &line_draft("note", "RIGHT", 10, 10, &["target"]),
        &HUMAN, None)
        .await
        .unwrap();
        let mut lines = Vec::new();
        for number in 1..=40u32 {
            let text = if number == 8 || number == 14 { "target" } else { "filler" };
            lines.push(patch(text, Some(number), Some(number)));
        }
        let statuses = match_comment_anchors_in_pool(
            &pool, "/demo", "base", "/demo", "worktree", "file.txt", &lines,
        )
        .await
        .unwrap();
        // Two equal-hash candidates exist; the nearer one wins.
        assert_eq!(
            statuses,
            [AnchorStatus {
                comment_id: comment.id,
                state: AnchorState::Moved,
                moved_line: Some(8),
            }]
        );
    }

    #[tokio::test]
    async fn matcher_marks_outdated_beyond_the_window() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let comment = create_comment_in_pool(
            &pool,
            "/demo",
            "base",
            "/demo",
            "worktree",
            &line_draft("note", "RIGHT", 5, 5, &["gone"]),
        &HUMAN, None)
        .await
        .unwrap();
        let mut lines = Vec::new();
        for number in 1..=45u32 {
            let text = if number == 40 { "gone" } else { "filler" };
            lines.push(patch(text, Some(number), Some(number)));
        }
        let statuses = match_comment_anchors_in_pool(
            &pool, "/demo", "base", "/demo", "worktree", "file.txt", &lines,
        )
        .await
        .unwrap();
        assert_eq!(
            statuses,
            [AnchorStatus {
                comment_id: comment.id,
                state: AnchorState::Outdated,
                moved_line: None,
            }]
        );
    }

    #[tokio::test]
    async fn matcher_matches_swapped_sides_for_reversed_input() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        // A reversed render swaps the parsed fields before they reach the
        // matcher: a display row read as (old 9, new 3) arrives as logical
        // (old 3, new 9). A comment anchored on the logical old side still
        // matches that swapped input.
        let comment = create_comment_in_pool(
            &pool,
            "/demo",
            "base",
            "/demo",
            "worktree",
            &line_draft("note", "LEFT", 4, 5, &["x", "y"]),
        &HUMAN, None)
        .await
        .unwrap();
        let display_rows = [
            ("keep", Some(9u32), Some(3u32)),
            ("x", Some(8), Some(4)),
            ("y", Some(7), Some(5)),
        ];
        let swapped: Vec<PatchLine> = display_rows
            .iter()
            .map(|(text, old, new)| patch(text, *new, *old))
            .collect();
        let statuses = match_comment_anchors_in_pool(
            &pool, "/demo", "base", "/demo", "worktree", "file.txt", &swapped,
        )
        .await
        .unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].comment_id, comment.id);
        assert_eq!(statuses[0].state, AnchorState::Current);
    }

    #[tokio::test]
    async fn matcher_handles_untracked_files_with_right_side_lines_only() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let right = create_comment_in_pool(
            &pool,
            "/demo",
            "base",
            "/demo",
            "worktree",
            &line_draft("right note", "RIGHT", 1, 1, &["added"]),
        &HUMAN, None)
        .await
        .unwrap();
        let untracked = [patch("added", None, Some(1)), patch("more", None, Some(2))];
        let statuses = match_comment_anchors_in_pool(
            &pool, "/demo", "base", "/demo", "worktree", "file.txt", &untracked,
        )
        .await
        .unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].comment_id, right.id);
        assert_eq!(statuses[0].state, AnchorState::Current);
    }

    #[tokio::test]
    async fn matcher_honors_end_line_ranges_and_excludes_non_line_comments() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let ranged = create_comment_in_pool(
            &pool,
            "/demo",
            "base",
            "/demo",
            "worktree",
            &line_draft("range note", "RIGHT", 2, 4, &["b", "c", "d"]),
        &HUMAN, None)
        .await
        .unwrap();
        create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &draft("review note"), &HUMAN, None)
            .await
            .unwrap();
        let mut file_note = draft("file note");
        file_note.file_path = Some("file.txt".into());
        create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &file_note, &HUMAN, None)
            .await
            .unwrap();
        // Shifting one line of the range changes the hashed content.
        let lines = [
            patch("a", Some(1), Some(1)),
            patch("b", Some(2), Some(2)),
            patch("c", Some(3), Some(3)),
            patch("d", Some(4), Some(4)),
        ];
        let statuses = match_comment_anchors_in_pool(
            &pool, "/demo", "base", "/demo", "worktree", "file.txt", &lines,
        )
        .await
        .unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].comment_id, ranged.id);
        assert_eq!(statuses[0].state, AnchorState::Current);

        let shifted = [
            patch("z", Some(1), Some(1)),
            patch("b", Some(2), Some(2)),
            patch("c", Some(3), Some(3)),
            patch("d", Some(4), Some(4)),
        ];
        let statuses = match_comment_anchors_in_pool(
            &pool, "/demo", "base", "/demo", "worktree", "file.txt", &shifted,
        )
        .await
        .unwrap();
        assert_eq!(statuses[0].state, AnchorState::Current);
        let partial = [
            patch("a", Some(1), Some(1)),
            patch("b", Some(2), Some(2)),
            patch("c", Some(3), Some(3)),
            patch("x", Some(4), Some(4)),
        ];
        let statuses = match_comment_anchors_in_pool(
            &pool, "/demo", "base", "/demo", "worktree", "file.txt", &partial,
        )
        .await
        .unwrap();
        assert_eq!(statuses[0].state, AnchorState::Outdated);
    }

    fn payload() -> SubmissionPayload {
        SubmissionPayload {
            agent_name: "reviewer-bot".into(),
            agent_model: "test-model".into(),
            command_context: Some("orchestrator".into()),
            sections: vec![
                StoredSection {
                    kind: "brief".into(),
                    title: "Summary".into(),
                    body: "All good.".into(),
                },
                StoredSection {
                    kind: "walkthrough".into(),
                    title: "Walkthrough".into(),
                    body: "Step by step.".into(),
                },
                StoredSection {
                    kind: "custom-diagram".into(),
                    title: "Diagram".into(),
                    body: "Unknown kinds are stored as sent.".into(),
                },
            ],
            findings: vec![
                SubmissionFinding {
                    title: "Review note".into(),
                    body: "Review-level finding.".into(),
                    file: None,
                    start: None,
                    end: None,
                    priority: "P1".into(),
                },
                SubmissionFinding {
                    title: "File note".into(),
                    body: "File-level finding.".into(),
                    file: Some("file.txt".into()),
                    start: None,
                    end: None,
                    priority: "P2".into(),
                },
                SubmissionFinding {
                    title: "Line note".into(),
                    body: "Line finding.".into(),
                    file: Some("file.txt".into()),
                    start: Some(3),
                    end: Some(4),
                    priority: "P0".into(),
                },
            ],
        }
    }

    async fn ingest(
        pool: &SqlitePool,
        repo_path: &str,
        payload: &SubmissionPayload,
        author: &Actor,
    ) -> Result<i64, CommandError> {
        ingest_submission_in_pool(
            pool,
            repo_path,
            "base",
            "/demo",
            "worktree",
            payload,
            author,
            &noop_request_changes(),
        )
        .await
    }

    fn noop_request_changes() -> crate::transport::RequestChangeSink {
        std::sync::Arc::new(|_| {})
    }

    #[tokio::test]
    async fn ingest_round_trips_sections_and_materializes_findings_as_agent_comments() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let submission_id = ingest(&pool, "/demo", &payload(), &HUMAN).await.unwrap();

        let submissions = list_submissions_in_pool(&pool, "/demo", "base", "/demo", "worktree")
            .await
            .unwrap();
        assert_eq!(submissions.len(), 1);
        let submission = &submissions[0];
        assert_eq!(submission.id, submission_id);
        assert_eq!(submission.agent_name, "reviewer-bot");
        assert_eq!(submission.agent_model, "test-model");
        assert_eq!(submission.command_context.as_deref(), Some("orchestrator"));
        // Sections come back typed and in order; the unknown kind is stored.
        assert_eq!(
            submission
                .sections
                .iter()
                .map(|section| section.kind.as_str())
                .collect::<Vec<_>>(),
            ["brief", "walkthrough", "custom-diagram"]
        );
        assert_eq!(submission.sections[2].title, "Diagram");

        let comments = list_comments_in_pool(&pool, "/demo", "base", "/demo", "worktree")
            .await
            .unwrap();
        assert_eq!(comments.len(), 3);
        assert!(comments.iter().all(|comment| comment.author_kind == "agent"));
        assert!(comments
            .iter()
            .all(|comment| comment.submission_id == Some(submission_id)));
        assert!(comments
            .iter()
            .all(|comment| comment.author_name == "reviewer-bot"));
        assert!(comments
            .iter()
            .all(|comment| comment.author_model.as_deref() == Some("test-model")));
        let review_level = &comments[0];
        assert_eq!(review_level.severity.as_deref(), Some("P1"));
        assert_eq!(review_level.body, "Review-level finding.");
        assert_eq!(review_level.file_path, None);
        let file_level = &comments[1];
        assert_eq!(file_level.severity.as_deref(), Some("P2"));
        assert_eq!(file_level.file_path.as_deref(), Some("file.txt"));
        assert_eq!(file_level.side, None);
        let line_level = &comments[2];
        assert_eq!(line_level.severity.as_deref(), Some("P0"));
        assert_eq!(line_level.file_path.as_deref(), Some("file.txt"));
        // Findings carry no side: line anchors are always the RIGHT side,
        // and with no bound lines there is no hash or snippet to drift-match.
        assert_eq!(line_level.side.as_deref(), Some("RIGHT"));
        assert_eq!(line_level.start_line, Some(3));
        assert_eq!(line_level.end_line, Some(4));
        assert_eq!(line_level.anchor_hash, None);
        assert_eq!(line_level.snippet, None);
        let missing = list_submissions_in_pool(&pool, "/demo", "base", "/gone", "head")
            .await
            .unwrap();
        assert!(missing.is_empty());
    }

    #[tokio::test]
    async fn ingest_rejects_payload_violations() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;

        let mut empty_name = payload();
        empty_name.agent_name = "  ".into();
        assert_eq!(
            ingest(&pool, "/demo", &empty_name, &HUMAN).await.unwrap_err().code,
            "invalid_submission"
        );

        let mut bad_priority = payload();
        bad_priority.findings[0].priority = "P9".into();
        assert_eq!(
            ingest(&pool, "/demo", &bad_priority, &HUMAN)
                .await
                .unwrap_err()
                .code,
            "invalid_submission"
        );

        // A line number without a file is a malformed anchor.
        let mut bad_anchor = payload();
        bad_anchor.findings[0].start = Some(3);
        assert_eq!(
            ingest(&pool, "/demo", &bad_anchor, &HUMAN).await.unwrap_err().code,
            "invalid_submission"
        );

        // An oversized total payload is rejected even though every section
        // body is under its own cap.
        let mut oversized = payload();
        oversized.sections = vec![StoredSection {
            kind: "brief".into(),
            title: "Big".into(),
            body: "a".repeat(MAX_PAYLOAD_BYTES + 1),
        }];
        assert_eq!(
            ingest(&pool, "/demo", &oversized, &HUMAN)
                .await
                .unwrap_err()
                .code,
            "invalid_submission"
        );

        // An html section body may exceed the text cap but not 1 MiB.
        let mut html = payload();
        html.sections = vec![StoredSection {
            kind: "html".into(),
            title: "Canvas".into(),
            body: format!("<p>{}</p>", "a".repeat(MAX_HTML_SECTION_BYTES)),
        }];
        assert_eq!(
            ingest(&pool, "/demo", &html, &HUMAN).await.unwrap_err().code,
            "invalid_submission"
        );
        let mut legal = payload();
        legal.sections = vec![StoredSection {
            kind: "html".into(),
            title: "Canvas".into(),
            body: "<p>static</p>".into(),
        }];
        legal.findings.truncate(1);
        assert!(ingest(&pool, "/demo", &legal, &HUMAN).await.is_ok());

        // Non-html section bodies cap at 256 KiB.
        let mut text = payload();
        text.sections = vec![StoredSection {
            kind: "notes".into(),
            title: "Notes".into(),
            body: "a".repeat(MAX_TEXT_BODY_BYTES + 1),
        }];
        assert_eq!(
            ingest(&pool, "/demo", &text, &HUMAN).await.unwrap_err().code,
            "invalid_submission"
        );

        // Finding bodies cap at 256 KiB as well.
        let mut finding_body = payload();
        finding_body.findings[0].body = "a".repeat(MAX_TEXT_BODY_BYTES + 1);
        assert_eq!(
            ingest(&pool, "/demo", &finding_body, &HUMAN)
                .await
                .unwrap_err()
                .code,
            "invalid_submission"
        );

        // Structural violations never leave rows behind.
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM reviews")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1,
            "only the round-trip review exists"
        );
    }

    #[tokio::test]
    async fn ingest_rejects_unknown_fields() {
        let rejected = serde_json::from_str::<SubmissionPayload>(
            r#"{
                "agent_name": "bot", "agent_model": "m", "sections": [], "findings": [],
                "unexpected": true
            }"#,
        )
        .unwrap_err();
        assert!(rejected.is_data());
        let rejected = serde_json::from_str::<SubmissionPayload>(
            r#"{
                "agent_name": "bot", "agent_model": "m", "sections": [], "findings": [],
                "command_context": null, "extra": 1
            }"#,
        )
        .unwrap_err();
        assert!(rejected.is_data());
        let section = serde_json::from_str::<StoredSection>(
            r#"{ "kind": "brief", "title": "t", "body": "b", "note": "x" }"#,
        )
        .unwrap_err();
        assert!(section.is_data());
        assert!(serde_json::from_str::<SubmissionPayload>(
            r#"{
                "agent_name": "bot", "agent_model": "m", "sections": [], "findings": [],
                "command_context": null
            }"#,
        )
        .is_ok());
    }

    #[tokio::test]
    async fn ingest_without_a_repo_row_is_unknown_review_target() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let error = ingest(&pool, "/missing", &payload(), &HUMAN).await.unwrap_err();
        assert_eq!(error.code, "unknown_review_target");
        // Nothing was written, not even a review shell for the target.
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM reviews")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM submissions")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn ingest_rolls_back_when_a_finding_insert_fails_midway() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        // A DB-level fault on the second finding's insert simulates a
        // mid-ingest failure the validator cannot see.
        sqlx::query(
            "CREATE TRIGGER force_agent_failure BEFORE INSERT ON comments \
             WHEN NEW.author_kind = 'agent' AND NEW.severity = 'P1' \
             BEGIN SELECT RAISE(ABORT, 'forced failure'); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        let error = ingest(&pool, "/demo", &payload(), &HUMAN).await.unwrap_err();
        assert_eq!(error.code, "persistence");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM submissions")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "the submission must roll back with its findings"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM comments")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "no finding comment may survive a failed ingest"
        );
    }

    // Token ids are real agent_tokens rows: comments reference their owner,
    // so the tests mint tokens and act through them.
    async fn agent_actor(pool: &SqlitePool, name: &str) -> Actor {
        let created = crate::agents::create_agent_token_in_pool(pool, name).await.unwrap();
        agent(created.token.id)
    }

    #[tokio::test]
    async fn agents_edit_and_delete_only_their_own_comments() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let own_actor = agent_actor(&pool, "agent-a").await;
        let foreign_actor = agent_actor(&pool, "agent-b").await;
        let own = create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &draft("mine"), &own_actor, None)
            .await
            .unwrap();
        assert_eq!(own.author_kind, "agent");
        let foreign = create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &draft("theirs"), &foreign_actor, None)
            .await
            .unwrap();
        // A legacy comment row with no owning token is never agent-mutable.
        let legacy = create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &draft("legacy"), &HUMAN, None)
            .await
            .unwrap();
        sqlx::query("UPDATE comments SET author_token_id = NULL WHERE id = ?")
            .bind(legacy.id)
            .execute(&pool)
            .await
            .unwrap();

        let edited = edit_comment_in_pool(&pool, own.id, "mine, edited", &own_actor).await.unwrap();
        assert_eq!(edited.body, "mine, edited");
        let denied = edit_comment_in_pool(&pool, foreign.id, "hijack", &own_actor).await;
        assert_eq!(denied.unwrap_err().code, "not_comment_owner");
        let denied_legacy = edit_comment_in_pool(&pool, legacy.id, "hijack", &own_actor).await;
        assert_eq!(denied_legacy.unwrap_err().code, "not_comment_owner");

        delete_comment_in_pool(&pool, own.id, &own_actor).await.unwrap();
        assert_eq!(
            delete_comment_in_pool(&pool, foreign.id, &own_actor).await.unwrap_err().code,
            "not_comment_owner"
        );
        assert_eq!(
            delete_comment_in_pool(&pool, legacy.id, &own_actor).await.unwrap_err().code,
            "not_comment_owner"
        );
        let remaining = list_comments_in_pool(&pool, "/demo", "base", "/demo", "worktree")
            .await
            .unwrap();
        assert_eq!(remaining.len(), 2, "only the owned comment was deleted");
    }

    #[tokio::test]
    async fn any_agent_replies_and_resolves_while_human_paths_stay_open() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let replier = agent_actor(&pool, "agent-c").await;
        let other = agent_actor(&pool, "agent-d").await;
        let root = create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &draft("root"), &HUMAN, None)
            .await
            .unwrap();
        // Replies from any agent land on any comment and record the actor.
        let reply = reply_comment_in_pool(&pool, root.id, "agent reply", Some("P2".into()), &replier, None)
            .await
            .unwrap();
        assert_eq!(reply.author_kind, "agent");
        let owner: Option<i64> = sqlx::query_scalar("SELECT author_token_id FROM comments WHERE id = ?")
            .bind(reply.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            owner,
            match &replier { Actor::Agent(identity) => Some(identity.token_id), Actor::Human => None }
        );
        let resolved = set_comment_resolved_in_pool(&pool, root.id, true, &replier).await.unwrap();
        assert!(resolved.resolved_at.is_some());
        let reopened = set_comment_resolved_in_pool(&pool, root.id, false, &other).await.unwrap();
        assert!(reopened.resolved_at.is_none());

        // The human IPC path is unchanged: edit and delete without checks.
        let edited = edit_comment_in_pool(&pool, root.id, "human edit", &HUMAN).await.unwrap();
        assert_eq!(edited.body, "human edit");
        delete_comment_in_pool(&pool, root.id, &HUMAN).await.unwrap();
    }

    #[tokio::test]
    async fn ingested_findings_record_the_calling_token() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let submitter = agent_actor(&pool, "agent-e").await;
        let submitter_id = match &submitter { Actor::Agent(identity) => identity.token_id, Actor::Human => unreachable!() };
        ingest(&pool, "/demo", &payload(), &submitter).await.unwrap();
        let comments = list_comments_in_pool(&pool, "/demo", "base", "/demo", "worktree")
            .await
            .unwrap();
        assert!(!comments.is_empty());
        let owners: Vec<Option<i64>> = sqlx::query_scalar("SELECT author_token_id FROM comments ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(owners.iter().all(|owner| *owner == Some(submitter_id)));
        // Without a token the findings stay unowned (not agent-mutable).
        ingest(&pool, "/demo", &payload(), &HUMAN).await.unwrap();
        let unowned: Vec<Option<i64>> = sqlx::query_scalar(
            "SELECT author_token_id FROM comments WHERE submission_id = \
             (SELECT MAX(id) FROM submissions)",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(!unowned.is_empty() && unowned.iter().all(|owner| owner.is_none()));
    }

    // The ingest path's observation hook rides the engine's single-site
    // blocking definition: a P2-only submission is clean and approves the
    // open request on its identity, a P0 finding blocks, and each
    // identity's requests advance independently.
    #[tokio::test]
    async fn ingesting_a_submission_observes_open_review_requests() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let coder = crate::agents::create_agent_token_in_pool(&pool, "coder-bot")
            .await
            .unwrap()
            .token
            .id;
        let request_draft = crate::requests::RequestDraft {
            note: "Please review.".into(),
            lenses: Vec::new(),
            reviewers: Vec::new(),
            max_rounds: None,
            head_sha: "head-1".into(),
        };
        let blocking_target = crate::requests::create_request_in_pool(
            &pool, "/demo", "base", "/demo", "worktree", &request_draft,
            &crate::requests::Actor::Agent(coder), &noop_request_changes(),
        )
        .await
        .unwrap();
        let mut clean_draft = request_draft.clone();
        clean_draft.head_sha = "head-2".into();
        let clean_target = crate::requests::create_request_in_pool(
            &pool, "/demo", "clean-base", "/demo", "worktree", &clean_draft,
            &crate::requests::Actor::Human, &noop_request_changes(),
        )
        .await
        .unwrap();
        let status = |id: i64| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT status FROM review_requests WHERE id = ?",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };

        let mut clean_payload = payload();
        clean_payload
            .findings
            .retain(|finding| finding.priority == "P2");
        ingest_submission_in_pool(
            &pool, "/demo", "clean-base", "/demo", "worktree", &clean_payload, &HUMAN,
            &noop_request_changes(),
        )
        .await
        .unwrap();
        assert_eq!(status(clean_target.id).await, "approved");
        assert_eq!(status(blocking_target.id).await, "requested");

        ingest_submission_in_pool(
            &pool, "/demo", "base", "/demo", "worktree", &payload(), &HUMAN,
            &noop_request_changes(),
        )
        .await
        .unwrap();
        assert_eq!(status(blocking_target.id).await, "changes_requested");
        assert_eq!(status(clean_target.id).await, "approved");
    }
}
