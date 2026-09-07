import { test } from "node:test";
import assert from "node:assert/strict";
import { autoReviewBase, workingChangesBase } from "./reviewPresets.ts";
import type { RefInventory, ReviewTarget } from "./navigation.ts";

function refs(overrides: Partial<RefInventory> = {}): RefInventory {
  return { heads: [], remotes: [], tags: [], default_base: null, ...overrides };
}

function worktreeTarget(branch: string): ReviewTarget {
  return { kind: "worktree", worktree: { path: "/tmp/wt", branch, head: "abc1234567890" } };
}

test("worktree reviews prefer a unique matching upstream over the default base", () => {
  const inventory = refs({
    remotes: ["refs/remotes/origin/feature", "refs/remotes/origin/main"],
    default_base: "refs/heads/main",
  });
  assert.equal(autoReviewBase(worktreeTarget("refs/heads/feature"), inventory), "refs/remotes/origin/feature");
  assert.equal(autoReviewBase(worktreeTarget("refs/heads/main"), inventory), "refs/remotes/origin/main");
});

test("worktree reviews fall back to the default base without a unique upstream", () => {
  const inventory = refs({
    remotes: ["refs/remotes/upstream/main", "refs/remotes/origin/main"],
    default_base: "refs/heads/main",
  });
  assert.equal(autoReviewBase(worktreeTarget("refs/heads/main"), inventory), "refs/heads/main");
  assert.equal(autoReviewBase(worktreeTarget("refs/heads/main"), refs()), "");
});

test("ref targets skip the default base when it is the target itself", () => {
  const inventory = refs({ default_base: "refs/heads/main" });
  assert.equal(autoReviewBase({ kind: "ref", name: "refs/heads/main" }, inventory), "");
  assert.equal(autoReviewBase({ kind: "ref", name: "refs/heads/feature" }, inventory), "refs/heads/main");
});

test("commit targets use the default base unless the commit is already its ancestor", () => {
  const inventory = refs({ default_base: "refs/heads/main" });
  const fresh: ReviewTarget = { kind: "commit", sha: "aaa", parents: ["ppp"], defaultBaseAncestor: false };
  const merged: ReviewTarget = { kind: "commit", sha: "aaa", parents: ["ppp"], defaultBaseAncestor: true };
  const parentless: ReviewTarget = { kind: "commit", sha: "aaa", parents: [], defaultBaseAncestor: true };
  assert.equal(autoReviewBase(fresh, inventory), "refs/heads/main");
  assert.equal(autoReviewBase(merged, inventory), "ppp");
  assert.equal(autoReviewBase(merged, refs()), "ppp");
  assert.equal(autoReviewBase(parentless, refs()), "empty-tree");
});

test("working changes pin the branch tip, or the HEAD sha when detached", () => {
  assert.equal(workingChangesBase({ path: "/tmp/wt", branch: "refs/heads/feature", head: "abc" }), "refs/heads/feature");
  assert.equal(workingChangesBase({ path: "/tmp/wt", branch: "", head: "abc1234" }), "abc1234");
});
