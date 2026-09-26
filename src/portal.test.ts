import { test } from "node:test";
import assert from "node:assert/strict";
import {
  activityActorLabel,
  activityDayGroups,
  activityDividerIndex,
  activityKindFamily,
  activityKindLabel,
  normalizeReviewsSearch,
  normalizeThreadsText,
  projectOptions,
  reviewEventText,
  reviewsStateCounts,
  reviewsStatusChip,
  rowsForProject,
  rowsForReviewsState,
  rowsForThreadsState,
  rowsForThreadsVoice,
  searchResultCount,
  shortSha,
  threadIdentityRef,
  threadRouteLabel,
  threadsStateCounts,
  REVIEWS_STATE_FILTERS,
  THREADS_STATE_FILTERS,
  THREADS_VOICE_FILTERS,
  type PortalActivityEvent,
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
    requester_kind: "agent",
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
    last_event: "review requested by coder-bot",
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
  assert.deepEqual(reviewsStatusChip(row({ state: "open", status: "changes_requested" })), { label: "changes requested", tone: "warn" });
  assert.deepEqual(reviewsStatusChip(row({ state: "open", status: "in_review" })), { label: "in review", tone: "plain" });
  assert.deepEqual(reviewsStatusChip(row({ state: "settled", status: "approved" })), { label: "approved", tone: "ok" });
  assert.deepEqual(reviewsStatusChip(row({ state: "settled", status: "withdrawn" })), { label: "withdrawn", tone: "muted" });
  assert.deepEqual(reviewsStatusChip(row({ state: "stale", status: "approved" })), { label: "approved · stale", tone: "warn" });
  assert.deepEqual(reviewsStatusChip(row({ state: "no_request", status: "" })), { label: "no request", tone: "muted" });
});

test("the chip vocabulary is the filter set the tab renders", () => {
  assert.deepEqual(REVIEWS_STATE_FILTERS.map((chip) => chip.id), ["all", "open", "settled", "stale", "no_request"]);
});

test("the last-event line joins the narrated summary with the age", () => {
  const now = 1_000_000;
  assert.equal(reviewEventText(row({ last_event: "approved by you", age_basis: now }), now), "approved by you · just now");
  assert.equal(reviewEventText(row({ last_event: "claimed by reviewer-bot", age_basis: now - 45 * 60_000 }), now), "claimed by reviewer-bot · 45m ago");
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
    head_sha: "recorded-head",
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

test("thread rows carry the same identity refs review rows open through", () => {
  const worktree = thread({ head_sha: "recorded-head" });
  assert.deepEqual(threadIdentityRef(worktree), { repo_path: "/demo", base_sha: worktree.base_sha, target_key: "/wt-a", target_kind: "worktree", head_sha: "recorded-head" });
  const head = thread({ target_kind: "head", target_key: "abc123", head_sha: null });
  assert.deepEqual(threadIdentityRef(head), { repo_path: "/demo", base_sha: head.base_sha, target_key: "abc123", target_kind: "head", head_sha: null });
});

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

// ===== Activity helpers =====

let nextEvent = 0;

function event(overrides: Partial<PortalActivityEvent> = {}): PortalActivityEvent {
  nextEvent += 1;
  return {
    id: nextEvent,
    repo_path: "/demo",
    kind: "comment_posted",
    base_sha: "base",
    target_key: "/wt",
    target_kind: "worktree",
    request_id: null,
    comment_id: null,
    actor_kind: "agent",
    actor_name: "codex",
    summary: "codex posted a finding",
    created_at: 1000,
    ...overrides,
  };
}

test("event kinds label and fold into five icon families", () => {
  assert.equal(activityKindLabel("request_created"), "review requested");
  assert.equal(activityKindLabel("review_announced"), "review started");
  assert.equal(activityKindLabel("comment_replied"), "reply");
  assert.equal(activityKindLabel("surface_head_moved"), "head moved");
  assert.equal(activityKindLabel("repo_added"), "project added");
  // Unknown kinds fall back to the raw kind, never disappear.
  assert.equal(activityKindLabel("something_new"), "something new");
  assert.equal(activityKindFamily("request_verdict"), "request");
  assert.equal(activityKindFamily("review_announced"), "request");
  assert.equal(activityKindFamily("comment_resolved"), "comment");
  assert.equal(activityKindFamily("submission_delivered"), "submission");
  assert.equal(activityKindFamily("surface_head_moved"), "surface");
  assert.equal(activityKindFamily("repo_added"), "project");
});

test("the feed groups into day buckets newest-first", () => {
  const now = new Date(2026, 8, 14, 12, 0, 0).getTime(); // a fixed local noon
  const today = now - 3_600_000;
  const yesterday = now - 26 * 3_600_000;
  const older = now - 50 * 3_600_000;
  const groups = activityDayGroups(
    [event({ id: 4, created_at: today }), event({ id: 3, created_at: today }), event({ id: 2, created_at: yesterday }), event({ id: 1, created_at: older })],
    now,
  );
  assert.deepEqual(groups.map((group) => group.label).slice(0, 2), ["Today", "Yesterday"]);
  assert.equal(groups.length, 3, "older days fall back to a locale date label");
  assert.deepEqual(groups[0].events.map((row) => row.id), [4, 3]);
  assert.deepEqual(groups[1].events.map((row) => row.id), [2]);
  assert.deepEqual(groups[2].events.map((row) => row.id), [1]);
  assert.deepEqual(activityDayGroups([], now), []);
});

test("the divider sits at the first event past the seen watermark", () => {
  const events = [event({ id: 8 }), event({ id: 7 }), event({ id: 2 })];
  // A descending feed puts the unseen block at the top.
  assert.equal(activityDividerIndex(events, 0), 0);
  assert.equal(activityDividerIndex(events, 7), 0, "the newest event is still unseen");
  // Everything seen leaves no divider.
  assert.equal(activityDividerIndex(events, 8), -1);
  assert.equal(activityDividerIndex([], 0), -1);
  // Array order decides, not id order: the divider tracks the first row
  // past the watermark wherever the feed puts it.
  assert.equal(activityDividerIndex([event({ id: 2 }), event({ id: 7 })], 5), 1);
});

test("activity actors read as you locally and by name from a server", () => {
  assert.equal(activityActorLabel(event({ actor_kind: "human", actor_name: "you" })), "you");
  // A server-backed human event names the member who acted; agents keep
  // their names on both backends.
  assert.equal(activityActorLabel(event({ actor_kind: "human", actor_name: "ops", connection_id: 2 })), "ops");
  assert.equal(activityActorLabel(event({ actor_kind: "agent", actor_name: "codex", connection_id: 2 })), "codex");
});
