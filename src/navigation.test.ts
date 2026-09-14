import { test } from "node:test";
import assert from "node:assert/strict";
import { createNavigationHistory, DEFAULT_PORTAL_FILTERS, sameAppLocation, type AppLocation, type ChangedFile, type PortalFilters, type ReviewIdentity, type ReviewTarget } from "./navigation.ts";
import type { AttentionCategory } from "./requests.ts";

function worktreeTarget(path: string): ReviewTarget {
  return { kind: "worktree", worktree: { path, branch: `refs/heads/${path}`, head: "head" } };
}
function refTarget(name: string): ReviewTarget {
  return { kind: "ref", name };
}
function reviewIdentity(overrides: Partial<ReviewIdentity> & { target: ReviewTarget }): ReviewIdentity {
  return { repoPath: "/repo", base: "main", scope: "committed", reversed: false, ...overrides };
}
function reviewEntry(overrides: Partial<Extract<AppLocation, { kind: "review" }>> & { identity: ReviewIdentity }): AppLocation {
  return { kind: "review", selectedFile: null, focusedCommentId: null, ...overrides };
}
function threadEntry(commentId: number): AppLocation {
  return { kind: "thread", commentId };
}
function threadsEntry(overrides: Partial<PortalFilters> = {}): AppLocation {
  return { kind: "portal", tab: "threads", filters: { ...DEFAULT_PORTAL_FILTERS, ...overrides } };
}
function activityEntry(overrides: Partial<PortalFilters> = {}): AppLocation {
  return { kind: "portal", tab: "activity", filters: { ...DEFAULT_PORTAL_FILTERS, ...overrides } };
}
function file(path: string): ChangedFile {
  return { path, status: "M", untracked: false };
}
function pushInbox(history: ReturnType<typeof createNavigationHistory>): void {
  history.push({ kind: "inbox" });
}
function portalEntry(category: AttentionCategory): AppLocation {
  return { kind: "portal", tab: "inbox", filters: { ...DEFAULT_PORTAL_FILTERS, category } };
}
function reviewsEntry(overrides: Partial<PortalFilters> = {}): AppLocation {
  return { kind: "portal", tab: "reviews", filters: { ...DEFAULT_PORTAL_FILTERS, ...overrides } };
}

test("navigation starts at the inbox entry", () => {
  const history = createNavigationHistory();
  assert.deepEqual(history.current(), { kind: "inbox" });
  assert.equal(history.canBack(), false);
  assert.equal(history.canForward(), false);
});

test("push adds entries and back/forward walk them", () => {
  const history = createNavigationHistory();
  const a = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }) });
  const b = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-b") }) });
  history.push(a);
  history.push(b);
  assert.equal(history.canBack(), true);
  assert.equal(history.canForward(), false);
  assert.equal(history.back(), a);
  assert.equal(history.canForward(), true);
  assert.equal(history.forward(), b);
  assert.equal(history.canForward(), false);
  assert.equal(history.back(), a);
  assert.deepEqual(history.back(), { kind: "inbox" });
  assert.equal(history.canBack(), false);
});

test("back and forward clamp at the stack bounds", () => {
  const history = createNavigationHistory();
  const start = history.current();
  assert.equal(history.back(), start);
  assert.equal(history.canBack(), false);
  history.push({ kind: "commit-history", repoPath: "/repo", startPointLabel: "main", startRef: "main", worktreePath: null, selectedCommit: null, selectedFile: null });
  const top = history.current();
  assert.equal(history.forward(), top);
  assert.equal(history.canForward(), false);
});

test("pushing from a mid-history position discards forward entries", () => {
  const history = createNavigationHistory();
  const a = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }) });
  const b = reviewEntry({ identity: reviewIdentity({ target: refTarget("refs/heads/b") }) });
  const c = reviewEntry({ identity: reviewIdentity({ target: refTarget("refs/heads/c") }) });
  history.push(a);
  history.push(b);
  history.back();
  history.push(c);
  assert.equal(history.canForward(), false);
  assert.equal(history.back(), a);
  assert.equal(history.forward(), c);
  assert.equal(history.back(), a);
  assert.deepEqual(history.back(), { kind: "inbox" });
  assert.equal(history.canBack(), false);
});

test("consecutive duplicate pushes collapse into one entry", () => {
  const history = createNavigationHistory();
  const a = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }) });
  history.push(a);
  history.push(reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }) }));
  history.push(reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }) }));
  history.back();
  assert.deepEqual(history.current(), { kind: "inbox" });
  assert.equal(history.canBack(), false);
  assert.equal(history.forward(), a);
  history.push(reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }) }));
  assert.equal(history.current(), a);
  assert.equal(history.canBack(), true);
  assert.equal(history.canForward(), false);
  history.back();
  pushInbox(history);
  assert.deepEqual(history.current(), { kind: "inbox" });
  assert.equal(history.canBack(), false);
  assert.equal(history.canForward(), true);
});

test("portal entries equal on tab and filters category", () => {
  const requested = portalEntry("requested");
  assert.equal(sameAppLocation(requested, portalEntry("requested")), true);
  assert.equal(sameAppLocation(requested, portalEntry("needs_human")), false);
  assert.equal(sameAppLocation(requested, { kind: "inbox" }), false);
});

test("back and forward traverse portal category entries", () => {
  const history = createNavigationHistory();
  const requested = portalEntry("requested");
  const needsHuman = portalEntry("needs_human");
  history.push(requested);
  history.push(needsHuman);
  assert.equal(history.canForward(), false);
  assert.equal(history.back(), requested);
  assert.equal(history.canForward(), true);
  assert.equal(history.forward(), needsHuman);
  assert.equal(history.back(), requested);
  assert.deepEqual(history.back(), { kind: "inbox" });
  assert.equal(history.canBack(), false);
});

test("portal pushes collapse only when tab and category match", () => {
  const history = createNavigationHistory();
  history.push(portalEntry("requested"));
  history.push(portalEntry("needs_human"));
  history.push(portalEntry("needs_human"));
  assert.equal(history.canForward(), false);
  assert.equal(history.canBack(), true);
  history.push(portalEntry("requested"));
  assert.deepEqual(history.back(), { kind: "portal", tab: "inbox", filters: { ...DEFAULT_PORTAL_FILTERS, category: "needs_human" } });
  assert.deepEqual(history.forward(), { kind: "portal", tab: "inbox", filters: { ...DEFAULT_PORTAL_FILTERS, category: "requested" } });
});

test("reviews filter fields are all discriminating", () => {
  const base = reviewsEntry();
  assert.equal(sameAppLocation(base, reviewsEntry()), true);
  assert.equal(sameAppLocation(base, reviewsEntry({ reviewsState: "settled" })), false);
  assert.equal(sameAppLocation(base, reviewsEntry({ reviewsProject: "/repo" })), false);
  assert.equal(sameAppLocation(base, reviewsEntry({ reviewsSearch: "feat" })), false);
  // The inbox tab's own filter fields ride along, so they discriminate too.
  assert.equal(sameAppLocation(base, reviewsEntry({ category: "needs_human" })), false);
});

test("threads filter fields are all discriminating", () => {
  const base = threadsEntry();
  assert.equal(sameAppLocation(base, threadsEntry()), true);
  assert.equal(sameAppLocation(base, threadsEntry({ threadsState: "resolved" })), false);
  assert.equal(sameAppLocation(base, threadsEntry({ threadsVoice: "agents" })), false);
  assert.equal(sameAppLocation(base, threadsEntry({ threadsProject: "/repo" })), false);
  assert.equal(sameAppLocation(base, threadsEntry({ threadsText: "loop" })), false);
  assert.equal(sameAppLocation(base, reviewsEntry()), false, "the tab itself discriminates");
});

test("activity filter fields are discriminating and the tab joins the union", () => {
  const base = activityEntry();
  assert.equal(sameAppLocation(base, activityEntry()), true);
  assert.equal(sameAppLocation(base, activityEntry({ activityProject: "/repo" })), false);
  assert.equal(sameAppLocation(base, threadsEntry()), false, "the tab itself discriminates");
  // The default filters carry the activity field, so a push from another
  // tab never compares undefined against string.
  assert.equal(DEFAULT_PORTAL_FILTERS.activityProject, "");
  const history = createNavigationHistory();
  history.push(activityEntry());
  history.push(activityEntry({ activityProject: "/repo" }));
  assert.equal(history.canForward(), false);
  assert.deepEqual(history.back(), activityEntry());
});

test("thread entries equal on the root comment id alone", () => {
  assert.equal(sameAppLocation(threadEntry(7), threadEntry(7)), true);
  assert.equal(sameAppLocation(threadEntry(7), threadEntry(8)), false);
  assert.equal(sameAppLocation(threadEntry(7), { kind: "inbox" }), false);
  const history = createNavigationHistory();
  history.push(threadEntry(7));
  history.push(threadEntry(8));
  assert.deepEqual(history.back(), threadEntry(7));
  assert.deepEqual(history.forward(), threadEntry(8));
  // The same thread pushed twice collapses.
  history.push(threadEntry(8));
  assert.equal(history.canForward(), false);
});

test("review entries discriminate on focusedCommentId", () => {
  const plain = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }) });
  const focused = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }), focusedCommentId: 42 });
  assert.equal(sameAppLocation(plain, focused), false);
  assert.equal(sameAppLocation(focused, reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }), focusedCommentId: 42 }),), true);
});

test("reviews chip picks push while search edits replace in place", () => {
  const history = createNavigationHistory();
  history.push(reviewsEntry());
  history.push(reviewsEntry({ reviewsState: "settled", reviewsProject: "/repo" }));
  // Typing after a chip pick updates that entry in place: text edits never
  // stack history entries, and an identical replace is a no-op.
  history.replace(reviewsEntry({ reviewsState: "settled", reviewsProject: "/repo", reviewsSearch: "f" }));
  history.replace(reviewsEntry({ reviewsState: "settled", reviewsProject: "/repo", reviewsSearch: "feat" }));
  history.replace(reviewsEntry({ reviewsState: "settled", reviewsProject: "/repo", reviewsSearch: "feat" }));
  assert.equal(history.canForward(), false);
  assert.deepEqual(history.back(), reviewsEntry());
  assert.deepEqual(history.forward(), reviewsEntry({ reviewsState: "settled", reviewsProject: "/repo", reviewsSearch: "feat" }));
  // The next discrete selection pushes a fresh entry on top.
  history.push(reviewsEntry({ reviewsState: "open" }));
  assert.deepEqual(history.back(), reviewsEntry({ reviewsState: "settled", reviewsProject: "/repo", reviewsSearch: "feat" }));
});

test("tab switches between inbox and reviews push history entries", () => {
  const history = createNavigationHistory();
  const inbox = portalEntry("requested");
  history.push(inbox);
  history.push(reviewsEntry({ category: "requested" }));
  assert.deepEqual(history.back(), inbox);
  assert.equal(history.canForward(), true);
  // Same tab and identical filters still collapse.
  history.push(reviewsEntry({ category: "requested" }));
  assert.deepEqual(history.current(), reviewsEntry({ category: "requested" }));
});

test("entry equality ignores identity object shape but honors discriminating fields", () => {
  const left = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a"), reversed: true }) });
  const same = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a"), reversed: true }) });
  const otherFile = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a"), reversed: true }), selectedFile: file("src/a.ts") });
  const otherBase = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a"), reversed: true, base: "develop" }) });
  assert.equal(sameAppLocation(left, same), true);
  assert.equal(sameAppLocation(left, otherFile), false);
  assert.equal(sameAppLocation(left, otherBase), false);
});

test("replace overwrites the current entry without truncating forward entries", () => {
  const history = createNavigationHistory();
  const a = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }) });
  const b = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-b") }) });
  history.push(a);
  history.push(b);
  history.back();
  const replaced = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a"), base: "develop" }) });
  history.replace(replaced);
  assert.equal(history.current(), replaced);
  assert.equal(history.canForward(), true);
  assert.equal(history.forward(), b);
});

test("replace is a no-op when the entry equals the current entry", () => {
  const history = createNavigationHistory();
  const a = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }) });
  const b = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-b") }) });
  history.push(a);
  history.push(b);
  history.back();
  history.replace(reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }) }));
  assert.equal(history.current(), a);
  assert.equal(history.canForward(), true);
  assert.equal(history.forward(), b);
});

test("position updates replace the current entry in place", () => {
  const history = createNavigationHistory();
  const a = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }) });
  history.push(a);
  const withFile = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }), selectedFile: file("src/a.ts") });
  history.replace(withFile);
  assert.equal(history.current(), withFile);
  assert.equal(history.canBack(), true);
  assert.equal(history.canForward(), false);
});

test("the stack is capped at 100 entries and drops the oldest", () => {
  const history = createNavigationHistory();
  const first = reviewEntry({ identity: reviewIdentity({ target: refTarget("refs/heads/r0") }) });
  history.push(first);
  for (let i = 1; i <= 100; i += 1) {
    history.push(reviewEntry({ identity: reviewIdentity({ target: refTarget(`refs/heads/r${i}`) }) }));
  }
  let steps = 0;
  while (history.canBack()) { history.back(); steps += 1; }
  assert.equal(steps, 99);
  assert.deepEqual(history.current(), reviewEntry({ identity: reviewIdentity({ target: refTarget("refs/heads/r1") }) }));
});

test("subscribe notifies on changes and unsubscribe stops notifications", () => {
  const history = createNavigationHistory();
  let changes = 0;
  const unsubscribe = history.subscribe(() => { changes += 1; });
  history.push(reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }) }));
  history.back();
  history.replace({ kind: "inbox" });
  const before = changes;
  unsubscribe();
  history.push({ kind: "settings" });
  assert.equal(changes, before);
});
