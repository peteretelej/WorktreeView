import type { DiffLine } from "./diff.ts";
import type { ReviewIdentity } from "./navigation.ts";

// Mirrors of the serde types behind the comment IPC commands. Anchors are
// logical (unreversed): LEFT is the old side of the unreversed diff, and
// line numbers refer to that unreversed numbering regardless of layout.
export type CommentSide = "LEFT" | "RIGHT";
export type DisplaySide = CommentSide;
export type CommentSeverity = "P0" | "P1" | "P2" | "P3";
export type ReviewComment = {
  id: number;
  review_id: number;
  parent_id: number | null;
  author_kind: "human" | "agent";
  author_name: string;
  author_model: string | null;
  body: string;
  file_path: string | null;
  side: CommentSide | null;
  start_line: number | null;
  end_line: number | null;
  anchor_hash: string | null;
  snippet: string | null;
  severity: CommentSeverity | null;
  submission_id: number | null;
  resolved_at: number | null;
  edited_at: number | null;
  created_at: number;
};
export type CommentDraft = {
  body: string;
  severity: CommentSeverity | null;
  file_path: string | null;
  side: CommentSide | null;
  start_line: number | null;
  end_line: number | null;
  lines: string[];
};
export type AnchorState = "current" | "moved" | "outdated";
export type AnchorStatus = { comment_id: number; state: AnchorState; moved_line: number | null };
// Marker-free patch line with logical line numbers, as match_comment_anchors
// expects: LEFT matches old_line, RIGHT matches new_line.
export type PatchLine = { text: string; old_line: number | null; new_line: number | null };
export type ReviewKey = { repoPath: string; baseSha: string; targetKey: string; targetKind: "worktree" | "head" };
export type ReviewIndexSummary = { base_sha: string; target_sha: string };
export type RowSelection = { displaySide: DisplaySide; start: number; end: number };
export type AuthorFilter = "all" | "human" | "agent";
export type CommentThread = { comment: ReviewComment; replies: ReviewComment[] };

// The comment session keys on resolved SHAs from the loaded index, never
// the symbolic base: a moved branch starts a new session by construction.
export function reviewKeyOf(identity: ReviewIdentity, index: ReviewIndexSummary): ReviewKey | null {
  if (!index.base_sha || !index.target_sha) return null;
  const target = identity.target;
  if (target.kind === "worktree") return { repoPath: identity.repoPath, baseSha: index.base_sha, targetKey: target.worktree.path, targetKind: "worktree" };
  return { repoPath: identity.repoPath, baseSha: index.base_sha, targetKey: index.target_sha, targetKind: "head" };
}

// Unified-diff lines carry a one-character marker (+, -, or space) that
// flips under reversal; anchors hash and store the marker-free content.
export function lineContent(text: string): string {
  return text.startsWith("+") || text.startsWith("-") || text.startsWith(" ") ? text.slice(1) : text;
}

// DiffLine rows are display rows; PatchLines are logical. Reversal swaps
// the fields, which keeps the logical numbering reversal-invariant.
export function toPatchLines(lines: DiffLine[], reversed: boolean): PatchLine[] {
  return lines.map((line) => reversed
    ? { text: lineContent(line.text), old_line: line.newLine, new_line: line.oldLine }
    : { text: lineContent(line.text), old_line: line.oldLine, new_line: line.newLine });
}

export function logicalSide(displaySide: DisplaySide, reversed: boolean): CommentSide {
  if (!reversed) return displaySide;
  return displaySide === "LEFT" ? "RIGHT" : "LEFT";
}

// Display gutters under reversal already number the swapped logical side,
// so a selection keeps its line numbers and only its side label flips.
export function draftFromSelection(selection: RowSelection, lines: DiffLine[], reversed: boolean, filePath: string): CommentDraft | null {
  const start = Math.min(selection.start, selection.end);
  const end = Math.max(selection.start, selection.end);
  const selected: string[] = [];
  for (const line of lines) {
    const number = selection.displaySide === "LEFT" ? line.oldLine : line.newLine;
    if (number === null || number < start || number > end) continue;
    if (/^[+-\s]/.test(line.text) && line.text !== "") selected.push(lineContent(line.text));
  }
  if (selected.length === 0) return null;
  return { body: "", severity: null, file_path: filePath, side: logicalSide(selection.displaySide, reversed), start_line: start, end_line: end, lines: selected };
}

// Roots in post order with their flat replies; replies stay attached to
// their root even when the author filter would hide them.
export function commentThreads(comments: ReviewComment[]): CommentThread[] {
  const roots = comments.filter((comment) => comment.parent_id === null)
    .sort((left, right) => left.created_at - right.created_at || left.id - right.id);
  return roots.map((comment) => ({
    comment,
    replies: comments.filter((reply) => reply.parent_id === comment.id)
      .sort((left, right) => left.created_at - right.created_at || left.id - right.id),
  }));
}

export function filterThreadsByAuthor(threads: CommentThread[], author: AuthorFilter): CommentThread[] {
  if (author === "all") return threads;
  return threads.filter((thread) => thread.comment.author_kind === author);
}

// Where a comment renders inline: current at the anchor's last line, moved
// at its new position (side flipped for a reversed layout), and null for
// outdated comments, which render only in the stream with their snippet.
export function inlinePlacement(comment: ReviewComment, status: AnchorStatus | null, reversed: boolean): { side: DisplaySide; line: number; state: "current" | "moved" } | null {
  if (comment.file_path === null || comment.side === null || comment.start_line === null) return null;
  const state = status?.state ?? "current";
  if (state === "outdated") return null;
  const line = state === "moved" ? status?.moved_line ?? comment.start_line : comment.end_line ?? comment.start_line;
  const side = reversed ? (comment.side === "LEFT" ? "RIGHT" : "LEFT") : comment.side;
  return { side, line, state };
}
