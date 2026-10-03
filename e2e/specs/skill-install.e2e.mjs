import assert from "node:assert/strict";
import { existsSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import path from "node:path";

// The e2e home is the container user's profile; the skill targets live
// beside it exactly as they do on a real machine.
const home = process.env.HOME ?? "/tmp/worktreeview-e2e-home";
const skillsDir = path.join(home, ".agents", "skills");
const installedSkill = path.join(skillsDir, "worktreeview", "SKILL.md");

// One onboarding flow: the prompt offers the bundled skill when an agent
// skills folder exists and nothing is installed, one click copies it, and
// the Settings row reflects the result.
describe("agent skill install", () => {
  it("offers, installs, and clears the bundled skill in one click", async () => {
    // The app mounted before this spec ran, so the skills folder appears
    // mid-session; a reload re-runs the mount-time detection.
    await $('button[aria-label="Open settings"]').waitForDisplayed({ timeout: 20_000 });
    mkdirSync(skillsDir, { recursive: true });
    await browser.refresh();
    await $('button[aria-label="Open settings"]').waitForDisplayed({ timeout: 20_000 });

    const cue = $('div[aria-label="Agent skill install"]');
    await cue.waitForDisplayed({ timeout: 20_000, timeoutMsg: "the skill install prompt never appeared" });

    await $('button[aria-label="Install agent skill"]').click();
    await cue.waitForDisplayed({ timeout: 20_000, reverse: true, timeoutMsg: "the skill install prompt never cleared after installing" });

    assert.equal(existsSync(installedSkill), true, "SKILL.md never landed in the detected skills folder");
    assert.match(readFileSync(installedSkill, "utf8"), /WorktreeView/);
    assert.equal(existsSync(path.join(skillsDir, "worktreeview", "performing-review.md")), true);

    // The Settings backbone shows the same target, now current, with no
    // install action left to take.
    await $('button[aria-label="Open settings"]').click();
    await $("button=Agent API").click();
    await browser.waitUntil(async () => {
      const text = await browser.execute(() => document.querySelector("#settings-mcp")?.textContent ?? "");
      return text.includes("Skill installs") && text.includes(".agents") && !text.includes("Install");
    }, { timeoutMsg: "the skill installs row did not report the installed target" });

    rmSync(path.join(home, ".agents"), { recursive: true, force: true });
  });
});
