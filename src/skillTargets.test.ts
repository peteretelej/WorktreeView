import { test } from "node:test";
import assert from "node:assert/strict";
import { skillPromptOf } from "./skillTargets.ts";
import type { SkillTarget } from "./skillTargets.ts";

function target(path: string, status: SkillTarget["status"]): SkillTarget {
  return { path, label: "Agent skills", status };
}

test("silence is the default when every target is current", () => {
  const prompt = skillPromptOf([target("/h/.agents/skills/worktreeview", "up_to_date")], "");
  assert.equal(prompt.visible, false);
});

test("an empty detection never prompts", () => {
  assert.equal(skillPromptOf([], "").visible, false);
});

test("first run with nothing installed asks once", () => {
  const targets = [target("/h/.agents/skills/worktreeview", "not_installed")];
  const prompt = skillPromptOf(targets, "");
  assert.equal(prompt.visible, true);
  assert.equal(prompt.mode, "install");
});

test("a dismissal sticks until the target state changes", () => {
  const targets = [target("/h/.claude/skills/worktreeview", "not_installed")];
  const { key } = skillPromptOf(targets, "");
  assert.equal(skillPromptOf(targets, key).visible, false);
  const installed = [target("/h/.claude/skills/worktreeview", "up_to_date")];
  assert.equal(skillPromptOf(installed, key).visible, false, "installing also retires the cue");
  const stale = [target("/h/.claude/skills/worktreeview", "differs")];
  assert.equal(skillPromptOf(stale, key).visible, true, "a new app version re-arms the cue");
});

test("any differing copy asks for an update regardless of the rest", () => {
  const targets = [
    target("/h/.agents/skills/worktreeview", "up_to_date"),
    target("/h/.claude/skills/worktreeview", "differs"),
  ];
  const prompt = skillPromptOf(targets, "");
  assert.equal(prompt.visible, true);
  assert.equal(prompt.mode, "update");
});

test("the dismissal key is order-independent and status-sensitive", () => {
  const a = skillPromptOf([
    target("/h/.claude/skills/worktreeview", "up_to_date"),
    target("/h/.agents/skills/worktreeview", "not_installed"),
  ], "").key;
  const b = skillPromptOf([
    target("/h/.agents/skills/worktreeview", "not_installed"),
    target("/h/.claude/skills/worktreeview", "up_to_date"),
  ], "").key;
  assert.equal(a, b);
  const changed = skillPromptOf([
    target("/h/.agents/skills/worktreeview", "up_to_date"),
    target("/h/.claude/skills/worktreeview", "up_to_date"),
  ], "").key;
  assert.notEqual(a, changed);
});
