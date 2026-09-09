import { test } from "node:test";
import assert from "node:assert/strict";
import { createNavigationHistory, sameAppLocation, type AppLocation, type ChangedFile, type ReviewIdentity, type ReviewTarget } from "./navigation.ts";

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
  return { kind: "review", selectedFile: null, filePage: 0, ...overrides };
}
function file(path: string): ChangedFile {
  return { path, status: "M", untracked: false };
}
function pushInbox(history: ReturnType<typeof createNavigationHistory>): void {
  history.push({ kind: "inbox" });
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
  history.push({ kind: "commit-history", repoPath: "/repo", startPointLabel: "main", startRef: "main", worktreePath: null, selectedCommit: null, selectedFile: null, filePage: 0 });
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
  history.push(reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }), filePage: 0 }));
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

test("entry equality ignores identity object shape but honors discriminating fields", () => {
  const left = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a"), reversed: true }) });
  const same = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a"), reversed: true }) });
  const otherFile = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a"), reversed: true }), selectedFile: file("src/a.ts") });
  const otherPage = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a"), reversed: true }), filePage: 2 });
  const otherBase = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a"), reversed: true, base: "develop" }) });
  assert.equal(sameAppLocation(left, same), true);
  assert.equal(sameAppLocation(left, otherFile), false);
  assert.equal(sameAppLocation(left, otherPage), false);
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
  const withFile = reviewEntry({ identity: reviewIdentity({ target: worktreeTarget("/wt-a") }), selectedFile: file("src/a.ts"), filePage: 1 });
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
