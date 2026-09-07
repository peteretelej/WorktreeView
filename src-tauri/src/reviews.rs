use crate::store::now_millis;
use crate::CommandError;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::BTreeMap;

// Snippets capture the anchored lines at write time (uncapturable later);
// the bound keeps a comment on a huge range from bloating the store.
const SNIPPET_MAX_CHARS: usize = 2000;
// Drift re-anchoring searches this many lines on either side of the
// recorded start; beyond it the comment is outdated, not moved.
const MATCH_WINDOW: u32 = 25;

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
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
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
    Line { side: String, start_line: u32, end_line: u32, content: String },
}

// The authoritative anchor-shape validator for this package: phase 2's
// ingest reuses it. Mirrors the comments table CHECK constraints with
// friendly errors, and binds the hashed content to the posted lines.
fn validate_anchor(draft: &CommentDraft) -> Result<Anchor, CommandError> {
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
    if draft.lines.is_empty() {
        return Err(invalid("A line comment needs the anchored lines' content."));
    }
    Ok(Anchor::Line {
        side,
        start_line,
        end_line,
        content: draft.lines.join("\n"),
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

#[allow(clippy::too_many_arguments)]
async fn insert_comment(
    pool: &SqlitePool,
    review_id: i64,
    parent_id: Option<i64>,
    body: &str,
    severity: Option<&str>,
    file_path: Option<String>,
    side: Option<String>,
    start_line: Option<u32>,
    end_line: Option<u32>,
    anchor: Option<Anchor>,
) -> Result<Comment, CommandError> {
    let (hash, snippet) = match anchor {
        Some(Anchor::Line { content, .. }) => (
            Some(anchor_hash(&content)),
            Some(truncate_chars(&content, SNIPPET_MAX_CHARS)),
        ),
        _ => (None, None),
    };
    let result = sqlx::query(
        "INSERT INTO comments (review_id, parent_id, author_kind, author_name, body, \
                file_path, side, start_line, end_line, anchor_hash, snippet, severity, created_at) \
         VALUES (?, ?, 'human', 'you', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(review_id)
    .bind(parent_id)
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

pub(crate) async fn create_comment_in_pool(
    pool: &SqlitePool,
    repo_path: &str,
    base_sha: &str,
    target_key: &str,
    target_kind: &str,
    draft: &CommentDraft,
) -> Result<Comment, CommandError> {
    let anchor = validate_anchor(draft)?;
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
pub(crate) async fn reply_comment_in_pool(
    pool: &SqlitePool,
    parent_id: i64,
    body: &str,
    severity: Option<String>,
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

// Resolve/reopen lives on the root; replies follow their thread.
pub(crate) async fn set_comment_resolved_in_pool(
    pool: &SqlitePool,
    comment_id: i64,
    resolved: bool,
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
) -> Result<Comment, CommandError> {
    require_body(body)?;
    load_comment(pool, comment_id).await?.ok_or_else(comment_not_found)?;
    sqlx::query("UPDATE comments SET body = ?, edited_at = ? WHERE id = ?")
        .bind(body)
        .bind(now_millis())
        .bind(comment_id)
        .execute(pool)
        .await?;
    load_comment(pool, comment_id).await?.ok_or_else(comment_not_found)
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
// when the range holds no rendered lines at all.
fn range_hash(lines: &BTreeMap<u32, &str>, start: u32, end: u32) -> Option<String> {
    let mut content = String::new();
    for (_, text) in lines.range(start..=end) {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(text);
    }
    if content.is_empty() {
        None
    } else {
        Some(anchor_hash(&content))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{seed_repo, test_pool};

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

    async fn review_id_for(pool: &SqlitePool, repo_path: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM reviews WHERE repo_path = ?")
            .bind(repo_path)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn comments_round_trip_through_the_store() {
        let pool = test_pool().await;
        seed_repo(&pool, "/demo").await;
        let review = resolve_review_id(&pool, "/demo", "base", "/demo", "worktree")
            .await
            .unwrap();
        create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &draft("review note"))
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
        )
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
            reply_comment_in_pool(&pool, line.id, "a reply", Some("P1".into())).await.unwrap();
        assert_eq!(reply.parent_id, Some(line.id));
        assert_eq!(reply.severity.as_deref(), Some("P1"));
        assert_eq!(reply.file_path, None);

        let resolved = set_comment_resolved_in_pool(&pool, line.id, true).await.unwrap();
        assert!(resolved.resolved_at.is_some());
        let reopened = set_comment_resolved_in_pool(&pool, line.id, false).await.unwrap();
        assert!(reopened.resolved_at.is_none());

        let edited = edit_comment_in_pool(&pool, line.id, "edited note").await.unwrap();
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
        let root = create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &draft("root"))
            .await
            .unwrap();
        let reply = reply_comment_in_pool(&pool, root.id, "first reply", None).await.unwrap();
        let nested = reply_comment_in_pool(&pool, reply.id, "nested", None).await;
        assert_eq!(nested.unwrap_err().code, "invalid_comment");
        let resolve_reply = set_comment_resolved_in_pool(&pool, reply.id, true).await;
        assert_eq!(resolve_reply.unwrap_err().code, "invalid_comment");
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
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &no_body)
                .await
                .unwrap_err()
                .code,
            "invalid_comment"
        );
        no_body.body = "ok".into();
        no_body.severity = Some("P9".into());
        assert_eq!(
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &no_body)
                .await
                .unwrap_err()
                .code,
            "invalid_comment"
        );
        let mut anchored_review = draft("review note");
        anchored_review.file_path = Some("file.txt".into());
        anchored_review.start_line = Some(1);
        assert_eq!(
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &anchored_review)
                .await
                .unwrap_err()
                .code,
            "invalid_comment"
        );
        let mut file_with_lines = draft("file note");
        file_with_lines.file_path = Some("file.txt".into());
        file_with_lines.lines = vec!["x".into()];
        assert_eq!(
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &file_with_lines)
                .await
                .unwrap_err()
                .code,
            "invalid_comment"
        );
        let backwards = line_draft("backwards", "LEFT", 5, 4, &["x"]);
        assert_eq!(
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &backwards)
                .await
                .unwrap_err()
                .code,
            "invalid_comment"
        );
        let no_lines = line_draft("empty", "RIGHT", 3, 3, &[]);
        assert_eq!(
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &no_lines)
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
        )
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
        let left = create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &old_side)
            .await
            .unwrap();
        let right = create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &new_side)
            .await
            .unwrap();
        assert_eq!(left.anchor_hash, right.anchor_hash);
        assert_eq!(left.anchor_hash.as_deref().map(str::len), Some(16));
        assert!(left.anchor_hash.as_ref().unwrap().bytes().all(|byte| byte.is_ascii_hexdigit()));

        let long = "é".repeat(2500);
        let mut oversized = line_draft("huge", "RIGHT", 1, 1, &[long.as_str()]);
        oversized.file_path = Some("big.txt".into());
        let stored =
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &oversized)
                .await
                .unwrap();
        assert_eq!(stored.snippet.as_deref().map(|text| text.chars().count()), Some(2000));
        let differing = line_draft("other", "RIGHT", 5, 5, &["x", "z"]);
        let other =
            create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &differing)
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
        )
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
        )
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
        )
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
        )
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
        )
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
        )
        .await
        .unwrap();
        create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &draft("review note"))
            .await
            .unwrap();
        let mut file_note = draft("file note");
        file_note.file_path = Some("file.txt".into());
        create_comment_in_pool(&pool, "/demo", "base", "/demo", "worktree", &file_note)
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
}
