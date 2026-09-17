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
  reviewsBackLabel,
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
    stale: false,
    reviews_expected: 0,
    reviews_delivered: 0,
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
        row({ category: "waiting_on_you" }),
        row({ category: "waiting_on_you", target_key: "/demo", base_sha: "base" }),
        row({ category: "in_flight", status: "in_review", requester: "" }),
      ],
    },
    { repo_path: "/other", repo_name: "other", rows: [row({ category: "recent", repo_path: "/other", status: "approved" }), row({ category: "waiting_on_you", repo_path: "/other", status: "" })] },
  ],
};

test("tab counts aggregate rows per bucket across repos", () => {
  const rows = attentionRows(queue);
  assert.equal(rows.length, 5);
  assert.deepEqual(attentionTabCounts(rows), {
    waiting_on_you: 3,
    in_flight: 1,
    recent: 1,
  });
  assert.deepEqual(attentionTabCounts(attentionRows(null)), {
    waiting_on_you: 0,
    in_flight: 0,
    recent: 0,
  });
});

test("the active tab filters rows by bucket", () => {
  const rows = attentionRows(queue);
  assert.equal(rowsForAttentionTab(rows, "waiting_on_you").length, 3);
  assert.equal(rowsForAttentionTab(rows, "in_flight").length, 1);
  assert.equal(rowsForAttentionTab(rows, "recent")[0].repo_path, "/other");
  assert.ok(ATTENTION_TABS.every((tab) => rowsForAttentionTab(rows, tab.id).every((matched) => matched.category === tab.id)));
});

test("ordering is stable across refreshes and re-sorts when severity changes", () => {
  const base = [
    row({ category: "waiting_on_you", unresolved_p0: 1, age_basis: 2000, request_id: 3 }),
    row({ category: "waiting_on_you", unresolved_p0: 0, age_basis: 1000, request_id: 1 }),
    row({ category: "waiting_on_you", unresolved_p0: 0, age_basis: 5000, request_id: 2 }),
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
    row({ category: "waiting_on_you", request_id: 8, age_basis: 1000 }),
    row({ category: "waiting_on_you", request_id: 7, age_basis: 1000 }),
  ]);
  assert.deepEqual(tied.map((row) => row.request_id), [7, 8]);
});

test("concurrent requests on one identity share a labelled change", () => {
  const concurrent = [
    row({ category: "waiting_on_you", change_label: "feature" }),
    row({ category: "waiting_on_you", change_label: "feature" }),
    row({ category: "in_flight", change_label: "other", base_sha: "base-2", target_key: "/two" }),
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
  assert.deepEqual(attentionStatus(row({ category: "waiting_on_you", needs_human: true, status: "changes_requested" })), { label: "needs human", tone: "needs-human" });
  assert.deepEqual(attentionStatus(row({ category: "waiting_on_you", status: "requested" })), { label: "requested", tone: "status" });
  assert.deepEqual(attentionStatus(row({ category: "waiting_on_you", status: "changes_requested" })), { label: "changes requested", tone: "status" });
  assert.equal(attentionStatus(row({ category: "recent", status: "" })), null);
});

test("row metadata renders the stale badge and named-reviewer progress", () => {
  assert.equal(reviewsBackLabel(row({ category: "waiting_on_you" })), null, "unnamed reviewers render nothing");
  assert.equal(reviewsBackLabel(row({ category: "waiting_on_you", reviews_expected: 2, reviews_delivered: 0 })), "0 of 2 reviews back");
  assert.equal(reviewsBackLabel(row({ category: "waiting_on_you", reviews_expected: 2, reviews_delivered: 1 })), "1 of 2 reviews back");
  assert.equal(reviewsBackLabel(row({ category: "waiting_on_you", reviews_expected: 1, reviews_delivered: 1 })), "1 of 1 reviews back");
});

test("preview lines narrate the backend's last-activity facts", () => {
  const now = 10_000_000;
  assert.equal(
    attentionPreview(row({ category: "recent", last_activity_kind: "comment", last_activity_actor: "codex", last_activity_at: now - 12 * 60_000, open_thread_count: 3 }), now),
    "codex commented 12m ago · 3 threads open",
  );
  assert.equal(
    attentionPreview(row({ category: "waiting_on_you", last_activity_kind: "submission", last_activity_actor: "reviewer-bot", last_activity_at: now - 2 * 60_000, open_thread_count: 0 }), now),
    "reviewer-bot delivered a review 2m ago",
  );
  assert.equal(
    attentionPreview(row({ category: "waiting_on_you", last_activity_kind: "request", last_activity_actor: "human", last_activity_at: now, open_thread_count: 1 }), now),
    "human updated the request just now · 1 thread open",
  );
  // A row with no recorded actor still formats as its activity kind.
  assert.equal(
    attentionPreview(row({ category: "recent", last_activity_kind: "comment", last_activity_actor: "", last_activity_at: now - 90 * 60_000, open_thread_count: 0 }), now),
    "commented 1h ago",
  );
});

test("the inbox renders exactly the three buckets", () => {
  assert.deepEqual(ATTENTION_TABS.map((tab) => tab.id), ["waiting_on_you", "in_flight", "recent"]);
  assert.deepEqual(ATTENTION_TABS.map((tab) => tab.label), ["Waiting on you", "In flight", "Recent"]);
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
