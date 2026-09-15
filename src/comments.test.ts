import { test } from "node:test";
import assert from "node:assert/strict";
import type { DiffLine } from "./diff.ts";
import type { ReviewIdentity } from "./navigation.ts";
import {
  anchorLabel,
  commentThreads,
  degradedReviewKey,
  draftFromSelection,
  exportThreadsMarkdown,
  filterThreadsByAuthor,
  formatCommentForCopy,
  formatThreadForCopy,
  inlinePlacement,
  lineContent,
  logicalSide,
  quoteExcerpt,
  reviewKeyOf,
  toPatchLines,
  type ReviewComment,
} from "./comments.ts";

function line(text: string, oldLine: number | null = null, newLine: number | null = null): DiffLine {
  return { text, oldLine, newLine };
}

function comment(overrides: Partial<ReviewComment> & { id: number; parent_id: number | null }): ReviewComment {
  return {
    review_id: 1,
    author_kind: "human",
    author_name: "you",
    author_model: null,
    body: "body",
    file_path: null,
    side: null,
    start_line: null,
    end_line: null,
    anchor_hash: null,
    snippet: null,
    severity: null,
    submission_id: null,
    resolved_at: null,
    edited_at: null,
    created_at: 1,
    ...overrides,
  };
}

const identity: ReviewIdentity = {
  repoPath: "/repo",
  base: "refs/heads/main",
  target: { kind: "worktree", worktree: { path: "/repo-wt", branch: "refs/heads/feature", head: "sha" } },
  scope: "all",
  reversed: false,
};

test("line content strips the unified diff marker", () => {
  assert.equal(lineContent("+added"), "added");
  assert.equal(lineContent("-removed"), "removed");
  assert.equal(lineContent(" kept"), "kept");
  assert.equal(lineContent("\\ No newline at end of file"), "\\ No newline at end of file");
});

test("patch line conversion strips markers and keeps logical numbers", () => {
  const lines = [line(" context", 3, 7), line("-gone", 4, null), line("+new", null, 8)];
  assert.deepEqual(toPatchLines(lines, false), [
    { text: "context", old_line: 3, new_line: 7 },
    { text: "gone", old_line: 4, new_line: null },
    { text: "new", old_line: null, new_line: 8 },
  ]);
});

test("patch line conversion swaps old and new fields when reversed", () => {
  const lines = [line("-gone", 4, null), line("+new", null, 8), line(" context", 3, 7)];
  assert.deepEqual(toPatchLines(lines, true), [
    { text: "gone", old_line: null, new_line: 4 },
    { text: "new", old_line: 8, new_line: null },
    { text: "context", old_line: 7, new_line: 3 },
  ]);
});

test("review keys use resolved shas and the worktree path for targets", () => {
  const index = { base_sha: "b".repeat(40), target_sha: "t".repeat(40) };
  assert.deepEqual(reviewKeyOf(identity, index), { repoPath: "/repo", baseSha: index.base_sha, targetKey: "/repo-wt", targetKind: "worktree" });
  const commit = { ...identity, target: { kind: "commit", sha: "c".repeat(40), parents: [], defaultBaseAncestor: false } as ReviewIdentity["target"] };
  assert.deepEqual(reviewKeyOf(commit, index), { repoPath: "/repo", baseSha: index.base_sha, targetKey: index.target_sha, targetKind: "head" });
  assert.equal(reviewKeyOf(identity, { base_sha: "", target_sha: "" }), null);
  // A stand-in target sessions on the stored identity it reopens.
  const standIn = { ...commit, recordedKey: { targetKey: "/repo-wt", targetKind: "worktree" as const } };
  assert.deepEqual(reviewKeyOf(standIn, index), { repoPath: "/repo", baseSha: index.base_sha, targetKey: "/repo-wt", targetKind: "worktree" });
});

test("degraded keys come from the identity's recorded refs", () => {
  const recordedBase = "b".repeat(40);
  // A worktree target keys by path, whatever its recorded head is.
  assert.deepEqual(degradedReviewKey({ ...identity, base: recordedBase }), { repoPath: "/repo", baseSha: recordedBase, targetKey: "/repo-wt", targetKind: "worktree" });
  const commit = { ...identity, base: recordedBase, target: { kind: "commit", sha: "c".repeat(40), parents: [], defaultBaseAncestor: false } as ReviewIdentity["target"] };
  assert.deepEqual(degradedReviewKey(commit), { repoPath: "/repo", baseSha: recordedBase, targetKey: "c".repeat(40), targetKind: "head" });
  // A stand-in target keys by its stored identity.
  assert.deepEqual(degradedReviewKey({ ...commit, recordedKey: { targetKey: "/repo-wt", targetKind: "worktree" } }), { repoPath: "/repo", baseSha: recordedBase, targetKey: "/repo-wt", targetKind: "worktree" });
  // Symbolic bases and abbreviated shas have no stored key; ref targets never do.
  assert.equal(degradedReviewKey(identity), null);
  assert.equal(degradedReviewKey({ ...identity, base: "abc123" }), null);
  assert.equal(degradedReviewKey({ ...identity, base: recordedBase, target: { kind: "commit", sha: "abc123", parents: [], defaultBaseAncestor: false } as ReviewIdentity["target"] }), null);
  assert.equal(degradedReviewKey({ ...identity, base: recordedBase, target: { kind: "ref", name: "refs/remotes/origin/feature" } }), null);
});

test("degraded keys come from the identity's recorded refs", () => {
  const recordedBase = "b".repeat(40);
  // A worktree target keys by path, whatever its recorded head is.
  assert.deepEqual(degradedReviewKey({ ...identity, base: recordedBase }), { repoPath: "/repo", baseSha: recordedBase, targetKey: "/repo-wt", targetKind: "worktree" });
  const commit = { ...identity, base: recordedBase, target: { kind: "commit", sha: "c".repeat(40), parents: [], defaultBaseAncestor: false } as ReviewIdentity["target"] };
  assert.deepEqual(degradedReviewKey(commit), { repoPath: "/repo", baseSha: recordedBase, targetKey: "c".repeat(40), targetKind: "head" });
  // Symbolic bases and abbreviated shas have no stored key; ref targets never do.
  assert.equal(degradedReviewKey(identity), null);
  assert.equal(degradedReviewKey({ ...identity, base: "abc123" }), null);
  assert.equal(degradedReviewKey({ ...identity, base: recordedBase, target: { kind: "commit", sha: "abc123", parents: [], defaultBaseAncestor: false } as ReviewIdentity["target"] }), null);
  assert.equal(degradedReviewKey({ ...identity, base: recordedBase, target: { kind: "ref", name: "refs/remotes/origin/feature" } }), null);
});

test("logical side flips only under reversal", () => {
  assert.equal(logicalSide("LEFT", false), "LEFT");
  assert.equal(logicalSide("RIGHT", false), "RIGHT");
  assert.equal(logicalSide("LEFT", true), "RIGHT");
  assert.equal(logicalSide("RIGHT", true), "LEFT");
});

test("drafts come from a row selection with markers stripped", () => {
  const lines = [line(" one", 1, 1), line("-two", 2, null), line("+deux", null, 2), line(" three", 3, 3)];
  const draft = draftFromSelection({ displaySide: "LEFT", start: 2, end: 3 }, lines, false, "file.txt");
  assert.deepEqual(draft, {
    body: "",
    severity: null,
    file_path: "file.txt",
    side: "LEFT",
    start_line: 2,
    end_line: 3,
    lines: ["two", "three"],
  });
});

test("draft selection normalizes inverted ranges and ignores other sides", () => {
  const lines = [line("+a", null, 1), line("+b", null, 2), line("+c", null, 3)];
  const draft = draftFromSelection({ displaySide: "RIGHT", start: 3, end: 1 }, lines, false, "f");
  assert.equal(draft?.side, "RIGHT");
  assert.equal(draft?.start_line, 1);
  assert.equal(draft?.end_line, 3);
  assert.deepEqual(draft?.lines, ["a", "b", "c"]);
});

test("reversed drafts flip the side but keep the selection numbers", () => {
  // Under reversal the display row parsed as (old 4, new 1) carries logical
  // numbers 1 (new side); the draft flips LEFT to RIGHT and keeps numbers.
  const lines = [line("-two", 4, null)];
  const draft = draftFromSelection({ displaySide: "LEFT", start: 4, end: 4 }, lines, true, "f");
  assert.equal(draft?.side, "RIGHT");
  assert.deepEqual(draft?.lines, ["two"]);
  assert.equal(draft?.start_line, 4);
});

test("drafts without selectable rows are null", () => {
  const lines = [line("\\ No newline at end of file")];
  assert.equal(draftFromSelection({ displaySide: "LEFT", start: 1, end: 2 }, lines, false, "f"), null);
});

test("threads pair roots with flat replies in post order", () => {
  const root = comment({ id: 2, parent_id: null, created_at: 5 });
  const lateRoot = comment({ id: 1, parent_id: null, created_at: 9 });
  const reply = comment({ id: 3, parent_id: 2, created_at: 6 });
  const nested = comment({ id: 4, parent_id: 3, created_at: 7 });
  const threads = commentThreads([nested, reply, lateRoot, root]);
  assert.deepEqual(threads.map((thread) => thread.comment.id), [2, 1]);
  assert.deepEqual(threads[0].replies.map((reply) => reply.id), [3]);
  // A reply of a reply is orphaned in the flat model, never nested.
  assert.deepEqual(threads[1].replies, []);
});

test("author filters keep whole threads of matching roots", () => {
  const human = comment({ id: 1, parent_id: null, author_kind: "human" });
  const agent = comment({ id: 2, parent_id: null, author_kind: "agent" });
  const agentReply = comment({ id: 3, parent_id: 2, author_kind: "agent" });
  const threads = commentThreads([human, agent, agentReply]);
  assert.equal(filterThreadsByAuthor(threads, "all").length, 2);
  assert.deepEqual(filterThreadsByAuthor(threads, "human").map((thread) => thread.comment.id), [1]);
  const agentThreads = filterThreadsByAuthor(threads, "agent");
  assert.deepEqual(agentThreads.map((thread) => thread.comment.id), [2]);
  assert.deepEqual(agentThreads[0].replies.map((reply) => reply.id), [3]);
});

test("inline placement follows statuses and flips under reversal", () => {
  const anchored = comment({ id: 1, parent_id: null, file_path: "f", side: "LEFT", start_line: 4, end_line: 6 });
  assert.deepEqual(inlinePlacement(anchored, null, false), { side: "LEFT", line: 6, state: "current" });
  assert.deepEqual(inlinePlacement(anchored, { comment_id: 1, state: "current", moved_line: null }, true), { side: "RIGHT", line: 6, state: "current" });
  assert.deepEqual(inlinePlacement(anchored, { comment_id: 1, state: "moved", moved_line: 9 }, false), { side: "LEFT", line: 9, state: "moved" });
  assert.equal(inlinePlacement(anchored, { comment_id: 1, state: "outdated", moved_line: null }, false), null);
  const reviewLevel = comment({ id: 2, parent_id: null });
  assert.equal(inlinePlacement(reviewLevel, null, false), null);
  const fileLevel = comment({ id: 3, parent_id: null, file_path: "f" });
  assert.equal(inlinePlacement(fileLevel, null, false), null);
});

test("quoteExcerpt block-quotes selected text and drops trailing blanks", () => {
  assert.equal(quoteExcerpt("part of a line"), "> part of a line\n\n");
  assert.equal(quoteExcerpt("two\nlines\r\n\n"), "> two\n> lines\n\n");
  assert.equal(quoteExcerpt(""), "");
  assert.equal(quoteExcerpt("  \n"), "");
});

test("anchor labels use display sides and collapse single-line ranges", () => {
  assert.equal(anchorLabel({ file_path: "f", side: "LEFT", start_line: 3, end_line: 3 }), "f L3");
  assert.equal(anchorLabel({ file_path: "f", side: "RIGHT", start_line: 3, end_line: 5 }), "f R3-5");
  assert.equal(anchorLabel({ file_path: "f", side: "LEFT", start_line: 3, end_line: 5 }, true), "f R3-5");
  assert.equal(anchorLabel({ file_path: "f", side: null, start_line: null, end_line: null }), "f");
  assert.equal(anchorLabel({ file_path: null, side: null, start_line: null, end_line: null }), "review");
});

test("comment copy keeps author, severity, anchor, and body context", () => {
  const body = formatCommentForCopy(comment({ id: 1, parent_id: null, author_name: "you", severity: "P1", file_path: "f.ts", side: "RIGHT", start_line: 3, end_line: 5, body: "line **one**\nline two" }));
  assert.equal(body, "**you** (P1) on f.ts R3-5:\n\nline **one**\nline two\n");
  const agent = formatCommentForCopy(comment({ id: 2, parent_id: null, author_kind: "agent", author_name: "agent-x", author_model: "model-1", body: "hi" }));
  assert.equal(agent, "**agent-x (model-1)** on review:\n\nhi\n");
});

test("thread copy quotes replies under the root", () => {
  const root = comment({ id: 1, parent_id: null, body: "root" });
  const reply = comment({ id: 2, parent_id: 1, author_name: "agent", body: "first\nsecond" });
  assert.equal(formatThreadForCopy({ comment: root, replies: [reply] }), "**you** on review:\n\nroot\n\n> **agent**:\n> \n> first\n> second\n");
});

test("export numbers threads and counts open ones", () => {
  const open = comment({ id: 1, parent_id: null, file_path: "a.ts", side: "RIGHT", start_line: 2, severity: "P0", created_at: 1000, body: "first" });
  const resolved = comment({ id: 2, parent_id: null, resolved_at: 2000, created_at: 1500, body: "done" });
  const text = exportThreadsMarkdown([{ comment: open, replies: [] }, { comment: resolved, replies: [] }]);
  assert.ok(text.startsWith("## Review comments: 2 threads, 1 open\n"));
  assert.ok(text.includes("### 1. a.ts R2, open, P0"));
  assert.ok(text.includes("### 2. review, resolved"));
  assert.equal(exportThreadsMarkdown([]), "No comments.\n");
});
