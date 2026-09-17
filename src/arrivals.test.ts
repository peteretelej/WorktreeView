import { test } from "node:test";
import assert from "node:assert/strict";
import { arrivalChangeLabel, arrivalProjectLabel, arrivalSentenceBody, olderArrivalsSuffix } from "./arrivals.ts";

test("head arrivals shorten to their token", () => {
  assert.equal(arrivalChangeLabel("head", "9d0f16e4a7b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5"), "9d0f16e");
  assert.equal(arrivalChangeLabel("head", "refs/heads/feature"), "feature");
});

test("worktree arrivals shorten to the path basename", () => {
  assert.equal(arrivalChangeLabel("worktree", "/repos/demo/.worktrees/fix-login"), "fix-login");
  assert.equal(arrivalChangeLabel("worktree", "C:\\repos\\demo\\.worktrees\\fix-login"), "fix-login");
});

test("project labels prefer the registered repo name", () => {
  const repos = [{ path: "/repos/demo", name: "Demo" }];
  assert.equal(arrivalProjectLabel(repos, "/repos/demo"), "Demo");
});

test("unregistered projects fall back to the path basename", () => {
  assert.equal(arrivalProjectLabel([], "/repos/demo"), "demo");
  assert.equal(arrivalProjectLabel([{ path: "/repos/other", name: "Other" }], "D:\\code\\demo"), "demo");
});

test("sentences name the moment with the change and project", () => {
  assert.equal(arrivalSentenceBody("delivery", "fix-login", "Demo"), "delivered a review on fix-login (Demo)");
  assert.equal(arrivalSentenceBody("announce", "fix-login", "Demo"), "started reviewing fix-login (Demo)");
});

test("the stacking suffix counts only the older entries", () => {
  assert.equal(olderArrivalsSuffix(0), "");
  assert.equal(olderArrivalsSuffix(3), " (+3 older)");
});
