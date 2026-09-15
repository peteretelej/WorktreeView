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
// A stand-in target (recordedKey set) sessions on the stored identity it
// reopens instead, so its conversation survives the swap.
export function reviewKeyOf(identity: ReviewIdentity, index: ReviewIndexSummary): ReviewKey | null {
  if (!index.base_sha || !index.target_sha) return null;
  if (identity.recordedKey) return { repoPath: identity.repoPath, baseSha: index.base_sha, ...identity.recordedKey };
  const target = identity.target;
  if (target.kind === "worktree") return { repoPath: identity.repoPath, baseSha: index.base_sha, targetKey: target.worktree.path, targetKind: "worktree" };
  return { repoPath: identity.repoPath, baseSha: index.base_sha, targetKey: index.target_sha, targetKind: "head" };
}

// When the index cannot resolve (moved project, gone surface), a review
// opened from stored rows still carries its recorded refs: a full-SHA base
// is its own resolution, worktree targets key by path, and head targets by
// the recorded sha. Symbolic ref targets have no stored key.
export function degradedReviewKey(identity: ReviewIdentity): ReviewKey | null {
  const target = identity.target;
  if (target.kind === "ref" || !/^[0-9a-f]{40}$/i.test(identity.base)) return null;
  if (identity.recordedKey) return { repoPath: identity.repoPath, baseSha: identity.base, ...identity.recordedKey };
  if (target.kind === "worktree") return { repoPath: identity.repoPath, baseSha: identity.base, targetKey: target.worktree.path, targetKind: "worktree" };
  return /^[0-9a-f]{40}$/i.test(target.sha) ? { repoPath: identity.repoPath, baseSha: identity.base, targetKey: target.sha, targetKind: "head" } : null;
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

// A row selection as the composer carries it: the raw range plus an
// optional text-selection excerpt captured when the composer opened.
export type CommentSelection = RowSelection & { excerpt?: string };

// Selected diff text becomes a blockquote pre-filled above the comment; an
// empty or whitespace-only selection quotes nothing.
export function quoteExcerpt(text: string): string {
  const lines = text.replace(/\r\n?/g, "\n").split("\n").map((line) => line.trimEnd());
  while (lines.length > 0 && lines[lines.length - 1] === "") lines.pop();
  if (lines.length === 0) return "";
  return lines.map((line) => (line === "" ? ">" : `> ${line}`)).join("\n") + "\n\n";
}

// Compact anchor label for cards and exports; the side letter follows the
// display side so a reversed layout labels rows as the user sees them.
export function anchorLabel(comment: Pick<ReviewComment, "file_path" | "side" | "start_line" | "end_line">, reversed = false): string {
  if (comment.file_path === null) return "review";
  if (comment.side === null || comment.start_line === null) return comment.file_path;
  const side = logicalSide(comment.side, reversed) === "LEFT" ? "L" : "R";
  const range = comment.end_line !== null && comment.end_line !== comment.start_line ? `-${comment.end_line}` : "";
  return `${comment.file_path} ${side}${comment.start_line}${range}`;
}

function authorLabel(comment: ReviewComment): string {
  return comment.author_kind === "agent" && comment.author_model ? `${comment.author_name} (${comment.author_model})` : comment.author_name;
}

function commentHeader(comment: ReviewComment, reversed: boolean): string {
  const severity = comment.severity ? ` (${comment.severity})` : "";
  return `**${authorLabel(comment)}**${severity} on ${anchorLabel(comment, reversed)}:`;
}

// One comment as markdown with its author and anchor context, ready to
// paste where the reader has no access to the review.
export function formatCommentForCopy(comment: ReviewComment, reversed = false): string {
  return `${commentHeader(comment, reversed)}\n\n${comment.body.trim()}\n`;
}

function quoteReply(comment: ReviewComment): string {
  const severity = comment.severity ? ` (${comment.severity})` : "";
  const body = comment.body.trim().split("\n").map((line) => (line === "" ? ">" : `> ${line}`)).join("\n");
  return `> **${authorLabel(comment)}**${severity}:\n> \n${body}\n`;
}

export function formatThreadForCopy(thread: CommentThread, reversed = false): string {
  const parts = [formatCommentForCopy(thread.comment, reversed)];
  for (const reply of thread.replies) parts.push(quoteReply(reply));
  return parts.join("\n");
}

// The stream's copy-all export: every visible thread with anchor, state,
// severity, and quoted replies, numbered for easy reference in a reply.
export function exportThreadsMarkdown(threads: CommentThread[], reversed = false): string {
  if (threads.length === 0) return "No comments.\n";
  const open = threads.filter((thread) => thread.comment.resolved_at === null).length;
  const blocks = threads.map((thread, index) => {
    const comment = thread.comment;
    const state = comment.resolved_at !== null ? "resolved" : "open";
    const severity = comment.severity ? `, ${comment.severity}` : "";
    const head = `### ${index + 1}. ${anchorLabel(comment, reversed)}, ${state}${severity}`;
    const date = new Date(comment.created_at).toISOString().slice(0, 10);
    return `${head}\n${commentHeader(comment, reversed)} ${date}\n\n${comment.body.trim()}\n${thread.replies.map((reply) => `\n${quoteReply(reply)}`).join("")}`;
  });
  return `## Review comments: ${threads.length} thread${threads.length === 1 ? "" : "s"}, ${open} open\n\n${blocks.join("\n")}`;
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
