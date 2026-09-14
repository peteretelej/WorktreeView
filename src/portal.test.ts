import { test } from "node:test";
import assert from "node:assert/strict";
import {
  normalizeReviewsSearch,
  normalizeThreadsText,
  projectOptions,
  reviewsStateCounts,
  reviewsStatusChip,
  rowsForProject,
  rowsForReviewsState,
  rowsForThreadsState,
  rowsForThreadsVoice,
  searchResultCount,
  shortSha,
  threadRouteLabel,
  threadsStateCounts,
  REVIEWS_STATE_FILTERS,
  THREADS_STATE_FILTERS,
  THREADS_VOICE_FILTERS,
  type PortalReviewRow,
  type PortalSearchMatches,
  type PortalThreadRow,
} from "./portal.ts";

let nextBase = 0;

function row(overrides: Partial<PortalReviewRow> = {}): PortalReviewRow {
  nextBase += 1;
  return {
    repo_path: "/demo",
    base_sha: `base-${nextBase}`,
    target_key: "/demo",
    target_kind: "worktree",
    change_label: "feature",
    head_sha: "recorded-head",
    requester: "coder-bot",
    status: "requested",
    note: "",
    round: 0,
    max_rounds: 2,
    unresolved_finding_counts: { P0: 0, P1: 0, P2: 0, P3: 0 },
    comment_count: 0,
    submission_count: 0,
    last_activity_at: 1000,
    state: "open",
    age_basis: 1000,
    ...overrides,
  };
}

test("state counts cover every chip and the all total", () => {
  const rows = [
    row({ state: "open" }),
    row({ state: "open" }),
    row({ state: "settled" }),
    row({ state: "stale" }),
    row({ state: "no_request" }),
  ];
  assert.deepEqual(reviewsStateCounts(rows), { open: 2, settled: 1, stale: 1, no_request: 1, all: 5 });
  assert.deepEqual(reviewsStateCounts([]), { open: 0, settled: 0, stale: 0, no_request: 0, all: 0 });
});

test("the state chip and project select narrow rows and keep payload order", () => {
  const rows = [
    row({ state: "open", repo_path: "/demo", last_activity_at: 300 }),
    row({ state: "settled", repo_path: "/other", last_activity_at: 200 }),
    row({ state: "open", repo_path: "/demo", last_activity_at: 100 }),
  ];
  assert.equal(rowsForReviewsState(rows, "all").length, 3);
  assert.deepEqual(rowsForReviewsState(rows, "open").map((row) => row.last_activity_at), [300, 100]);
  assert.deepEqual(rowsForProject(rows, "/other").map((row) => row.state), ["settled"]);
  assert.equal(rowsForProject(rows, "").length, 3);
});

test("project options list distinct paths sorted", () => {
  const rows = [row({ repo_path: "/zeta" }), row({ repo_path: "/alpha" }), row({ repo_path: "/zeta" })];
  assert.deepEqual(projectOptions(rows), ["/alpha", "/zeta"]);
  assert.deepEqual(projectOptions([]), []);
});

test("status chips read the backend state, not a re-derived rule", () => {
  assert.deepEqual(reviewsStatusChip(row({ state: "open", status: "changes_requested" })), { label: "changes requested", tone: "open" });
  assert.deepEqual(reviewsStatusChip(row({ state: "open", status: "in_review" })), { label: "in review", tone: "open" });
  assert.deepEqual(reviewsStatusChip(row({ state: "settled", status: "approved" })), { label: "approved", tone: "settled" });
  assert.deepEqual(reviewsStatusChip(row({ state: "settled", status: "withdrawn" })), { label: "withdrawn", tone: "settled" });
  assert.deepEqual(reviewsStatusChip(row({ state: "stale", status: "approved" })), { label: "changed since review", tone: "stale" });
  assert.deepEqual(reviewsStatusChip(row({ state: "no_request", status: "" })), { label: "no request", tone: "quiet" });
});

test("the chip vocabulary is the filter set the tab renders", () => {
  assert.deepEqual(REVIEWS_STATE_FILTERS.map((chip) => chip.id), ["open", "settled", "stale", "no_request", "all"]);
});

test("search normalization trims and empties whitespace-only needles", () => {
  assert.equal(normalizeReviewsSearch("  feat  "), "feat");
  assert.equal(normalizeReviewsSearch("   "), "");
});

test("full hashes shorten and other labels keep their length", () => {
  assert.equal(shortSha("0123456789abcdef0123456789abcdef01234567"), "0123456");
  assert.equal(shortSha("ABCDEF0123456789abcdef0123456789abcdef01"), "ABCDEF0");
  assert.equal(shortSha("refs/heads/main"), "refs/heads/main");
  assert.equal(shortSha("0123456"), "0123456");
});

// ===== Threads helpers =====

let nextThread = 0;

function thread(overrides: Partial<PortalThreadRow> = {}): PortalThreadRow {
  nextThread += 1;
  return {
    root_comment_id: nextThread,
    repo_path: "/demo",
    base_sha: `base-${nextThread}`,
    target_key: "/wt-a",
    target_kind: "worktree",
    excerpt: "first line",
    severity: null,
    anchor: null,
    participants: [{ author_kind: "human", author_name: "dana" }],
    reply_count: 0,
    resolved_at: null,
    last_activity_at: 1000,
    head_moved: false,
    ...overrides,
  };
}

test("thread state counts cover every chip and the all total", () => {
  const rows = [thread(), thread({ resolved_at: 500 }), thread()];
  assert.deepEqual(threadsStateCounts(rows), { open: 2, resolved: 1, all: 3 });
  assert.deepEqual(threadsStateCounts([]), { open: 0, resolved: 0, all: 0 });
});

test("the state and voice filters narrow threads without re-deriving rules", () => {
  const rows = [
    thread({ root_comment_id: 1, resolved_at: null, participants: [{ author_kind: "human", author_name: "dana" }] }),
    thread({ root_comment_id: 2, resolved_at: 900, participants: [{ author_kind: "agent", author_name: "bot" }] }),
    thread({ root_comment_id: 3, resolved_at: null, participants: [{ author_kind: "human", author_name: "dana" }, { author_kind: "agent", author_name: "bot" }] }),
  ];
  assert.deepEqual(rowsForThreadsState(rows, "open").map((thread) => thread.root_comment_id), [1, 3]);
  assert.deepEqual(rowsForThreadsState(rows, "resolved").map((thread) => thread.root_comment_id), [2]);
  assert.equal(rowsForThreadsState(rows, "all").length, 3);
  // The mixed-participant thread answers both voices.
  assert.deepEqual(rowsForThreadsVoice(rows, "human").map((thread) => thread.root_comment_id), [1, 3]);
  assert.deepEqual(rowsForThreadsVoice(rows, "agents").map((thread) => thread.root_comment_id), [2, 3]);
  assert.equal(rowsForThreadsVoice(rows, "all").length, 3);
});

test("the chip vocabularies are the filter sets the tab renders", () => {
  assert.deepEqual(THREADS_STATE_FILTERS.map((chip) => chip.id), ["open", "resolved", "all"]);
  assert.deepEqual(THREADS_VOICE_FILTERS.map((chip) => chip.id), ["all", "human", "agents"]);
});

test("threads text normalization trims and empties whitespace-only needles", () => {
  assert.equal(normalizeThreadsText("  loop bug  "), "loop bug");
  assert.equal(normalizeThreadsText("   "), "");
});

test("the thread route label is the in-app path, not a URL", () => {
  assert.equal(threadRouteLabel(42), "pulse/thread/42");
});

test("the search result count covers every scope", () => {
  const matches: PortalSearchMatches = {
    comments: [{ comment_id: 1, root_comment_id: 1, excerpt: "e", repo_path: "/demo", change_label: "feature" }],
    requests: [{ repo_path: "/demo", base_sha: "b", target_key: "/wt", target_kind: "worktree", head_sha: null, change_label: "feature", status: "requested", requester: "bot", note: "" }],
    commits: [],
  };
  assert.equal(searchResultCount(matches), 2);
  assert.equal(searchResultCount(null), 0);
  assert.equal(searchResultCount({ comments: [], requests: [], commits: [] }), 0);
});
