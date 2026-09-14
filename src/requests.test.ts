import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ATTENTION_TABS,
  attentionAge,
  attentionPreview,
  attentionRows,
  attentionStatus,
  attentionTabCounts,
  canReRequest,
  canVerdict,
  canWithdraw,
  groupedChangeLabel,
  isNarrowAttention,
  requestStatusLabel,
  rowsForAttentionTab,
  sortAttentionRows,
  validateRequestForm,
  REQUEST_LENS_OPTIONS,
  REQUEST_NOTE_LIMIT,
  type AttentionCategory,
  type AttentionQueue,
  type AttentionRow,
} from "./requests.ts";

let nextId = 1;

function row(overrides: Partial<AttentionRow> & { category: AttentionCategory }): AttentionRow {
  return {
    request_id: nextId++,
    repo_path: "/demo",
    base_sha: "base",
    target_key: "/demo",
    target_kind: "worktree",
    change_label: "feature",
    requester: "coder-bot",
    status: "requested",
    round: 0,
    max_rounds: 2,
    unresolved_p0: 0,
    unresolved_p1: 0,
    needs_human: false,
    age_basis: 1000,
    head_sha: null,
    last_activity_at: 1000,
    last_activity_kind: "request",
    last_activity_actor: "coder-bot",
    open_thread_count: 0,
    ...overrides,
  };
}

const queue: AttentionQueue = {
  repos: [
    {
      repo_path: "/demo",
      repo_name: "demo",
      rows: [
        row({ category: "requested" }),
        row({ category: "requested", target_key: "/demo", base_sha: "base" }),
        row({ category: "changes_requested" }),
        row({ category: "needs_human" }),
        row({ category: "unresolved_findings", repo_path: "/other" }),
      ],
    },
    { repo_path: "/other", repo_name: "other", rows: [row({ category: "changed_since_review" })] },
  ],
};

test("tab counts aggregate rows per category across repos", () => {
  const rows = attentionRows(queue);
  assert.equal(rows.length, 6);
  assert.deepEqual(attentionTabCounts(rows), {
    requested: 2,
    changes_requested: 1,
    needs_human: 1,
    unresolved_findings: 1,
    changed_since_review: 1,
    recent_comments: 0,
  });
  assert.deepEqual(attentionTabCounts(attentionRows(null)), {
    requested: 0,
    changes_requested: 0,
    needs_human: 0,
    unresolved_findings: 0,
    changed_since_review: 0,
    recent_comments: 0,
  });
});

test("the active tab filters rows by category", () => {
  const rows = attentionRows(queue);
  assert.equal(rowsForAttentionTab(rows, "requested").length, 2);
  assert.equal(rowsForAttentionTab(rows, "needs_human").length, 1);
  assert.equal(rowsForAttentionTab(rows, "unresolved_findings")[0].repo_path, "/other");
  assert.ok(ATTENTION_TABS.every((tab) => rowsForAttentionTab(rows, tab.id).every((matched) => matched.category === tab.id)));
});

test("ordering is stable across refreshes and re-sorts when severity changes", () => {
  const base = [
    row({ category: "requested", unresolved_p0: 1, age_basis: 2000, request_id: 3 }),
    row({ category: "requested", unresolved_p0: 0, age_basis: 1000, request_id: 1 }),
    row({ category: "requested", unresolved_p0: 0, age_basis: 5000, request_id: 2 }),
  ];
  const shuffled = [base[2], base[1], base[0]];
  const first = sortAttentionRows(shuffled);
  assert.deepEqual(first.map((row) => row.request_id), [3, 1, 2], "P0 desc, then age desc");
  // The same rows in another arrival order produce the same order, so a
  // background refresh never reorders what the user is reading.
  const second = sortAttentionRows([...shuffled].reverse());
  assert.deepEqual(second.map((row) => row.request_id), first.map((row) => row.request_id));
  // A changed severity re-sorts within the category.
  const escalated = sortAttentionRows(base.map((row) => row.request_id === 1 ? { ...row, unresolved_p0: 9 } : row));
  assert.deepEqual(escalated.map((row) => row.request_id), [1, 3, 2]);
  // Equal keys fall back to the request id, never input order.
  const tied = sortAttentionRows([
    row({ category: "requested", request_id: 8, age_basis: 1000 }),
    row({ category: "requested", request_id: 7, age_basis: 1000 }),
  ]);
  assert.deepEqual(tied.map((row) => row.request_id), [7, 8]);
});

test("concurrent requests on one identity share a labelled change", () => {
  const concurrent = [
    row({ category: "requested", change_label: "feature" }),
    row({ category: "requested", change_label: "feature" }),
    row({ category: "changes_requested", change_label: "other", base_sha: "base-2", target_key: "/two" }),
  ];
  assert.equal(groupedChangeLabel(concurrent[0], concurrent), "feature (2 requests)");
  assert.equal(groupedChangeLabel(concurrent[1], concurrent), "feature (2 requests)");
  assert.equal(groupedChangeLabel(concurrent[2], concurrent), "other");
});

test("narrow content engages below 800px of content width", () => {
  assert.equal(isNarrowAttention(799), true);
  assert.equal(isNarrowAttention(800), false);
  assert.equal(isNarrowAttention(1200), false);
});

test("ages render compactly and never go negative", () => {
  const now = 10_000_000;
  assert.equal(attentionAge(now, now), "now");
  assert.equal(attentionAge(now - 5 * 60_000, now), "5m");
  assert.equal(attentionAge(now - 3 * 3_600_000, now), "3h");
  assert.equal(attentionAge(now - 2 * 86_400_000, now), "2d");
  assert.equal(attentionAge(now - 14 * 86_400_000, now), "2w");
  assert.equal(attentionAge(now + 60_000, now), "now");
});

test("status chips read needs-human first and skip request-less rows", () => {
  assert.deepEqual(attentionStatus(row({ category: "needs_human", needs_human: true, status: "changes_requested" })), { label: "needs human", tone: "needs-human" });
  assert.deepEqual(attentionStatus(row({ category: "requested", status: "requested" })), { label: "requested", tone: "status" });
  assert.deepEqual(attentionStatus(row({ category: "changes_requested", status: "changes_requested" })), { label: "changes requested", tone: "status" });
  assert.equal(attentionStatus(row({ category: "unresolved_findings", status: "" })), null);
});

test("preview lines narrate the backend's last-activity facts", () => {
  const now = 10_000_000;
  assert.equal(
    attentionPreview(row({ category: "recent_comments", last_activity_kind: "comment", last_activity_actor: "codex", last_activity_at: now - 12 * 60_000, open_thread_count: 3 }), now),
    "codex commented 12m ago · 3 threads open",
  );
  assert.equal(
    attentionPreview(row({ category: "requested", last_activity_kind: "submission", last_activity_actor: "reviewer-bot", last_activity_at: now - 2 * 60_000, open_thread_count: 0 }), now),
    "reviewer-bot delivered a review 2m ago",
  );
  assert.equal(
    attentionPreview(row({ category: "changes_requested", last_activity_kind: "request", last_activity_actor: "human", last_activity_at: now, open_thread_count: 1 }), now),
    "human updated the request just now · 1 thread open",
  );
  // A row with no recorded actor still formats as its activity kind.
  assert.equal(
    attentionPreview(row({ category: "recent_comments", last_activity_kind: "comment", last_activity_actor: "", last_activity_at: now - 90 * 60_000, open_thread_count: 0 }), now),
    "commented 1h ago",
  );
});

test("the recent comments tab is part of the rendered chip set", () => {
  assert.ok(ATTENTION_TABS.some((tab) => tab.id === "recent_comments"));
  assert.equal(ATTENTION_TABS.length, 6);
});

test("verdicts speak only from in review", () => {
  assert.equal(canVerdict("requested"), false);
  assert.equal(canVerdict("in_review"), true);
  assert.equal(canVerdict("changes_requested"), false);
  assert.equal(canVerdict("approved"), false);
  assert.equal(canVerdict("withdrawn"), false);
});

test("re-request restarts only in-budget changes_requested rounds", () => {
  assert.equal(canReRequest({ status: "requested", needs_human: false }), false);
  assert.equal(canReRequest({ status: "in_review", needs_human: false }), false);
  assert.equal(canReRequest({ status: "changes_requested", needs_human: false }), true);
  // Round budget exhausted: the needs-human state refuses further rounds.
  assert.equal(canReRequest({ status: "changes_requested", needs_human: true }), false);
});

test("withdrawal exits any non-terminal request", () => {
  assert.equal(canWithdraw("requested"), true);
  assert.equal(canWithdraw("in_review"), true);
  assert.equal(canWithdraw("changes_requested"), true);
  assert.equal(canWithdraw("approved"), false);
  assert.equal(canWithdraw("withdrawn"), false);
});

test("statuses label for chips without jargon", () => {
  assert.equal(requestStatusLabel("in_review"), "in review");
  assert.equal(requestStatusLabel("changes_requested"), "changes requested");
  assert.equal(requestStatusLabel("requested"), "requested");
});

test("form validation treats the note as optional and gates on the note bound", () => {
  const valid = { note: "Please review the auth paths.", lenses: ["security"], max_rounds: 2 };
  assert.deepEqual(validateRequestForm(valid), {});
  assert.deepEqual(validateRequestForm({ ...valid, note: "" }), {});
  assert.deepEqual(validateRequestForm({ ...valid, note: "   " }), {});
  assert.deepEqual(validateRequestForm({ ...valid, note: "".padEnd(REQUEST_NOTE_LIMIT + 1, "a") }).note, `The note exceeds ${REQUEST_NOTE_LIMIT} characters.`);
  assert.equal(validateRequestForm({ ...valid, note: "a".repeat(REQUEST_NOTE_LIMIT) }).note, undefined);
});

test("form validation bounds the round budget", () => {
  const valid = { note: "note", lenses: [], max_rounds: 1 };
  assert.deepEqual(validateRequestForm(valid), {});
  assert.match(validateRequestForm({ ...valid, max_rounds: 0 }).max_rounds ?? "", /between 1 and 3/);
  assert.match(validateRequestForm({ ...valid, max_rounds: 4 }).max_rounds ?? "", /between 1 and 3/);
  assert.match(validateRequestForm({ ...valid, max_rounds: 2.5 }).max_rounds ?? "", /between 1 and 3/);
});

test("form validation rejects unknown and duplicate lenses", () => {
  const valid = { note: "note", lenses: ["security", "tests"], max_rounds: 2 };
  assert.deepEqual(validateRequestForm(valid), {});
  assert.match(validateRequestForm({ ...valid, lenses: ["style"] }).lenses ?? "", /Unknown lens 'style'/);
  assert.match(validateRequestForm({ ...valid, lenses: ["security", "security"] }).lenses ?? "", /duplicated/);
  // The checkbox vocabulary is the engine's lens set.
  assert.deepEqual([...REQUEST_LENS_OPTIONS], ["security", "correctness", "design", "performance", "tests"]);
});
