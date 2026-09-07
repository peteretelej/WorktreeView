import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import { mkdirSync, renameSync, rmSync, symlinkSync } from "node:fs";
import path from "node:path";

const fixtureRoot = "/tmp/worktreeview-e2e-fixtures";
const gitHome = "/tmp/worktreeview-e2e-git-home";
const selector = "/tmp/worktreeview-e2e-selection";

function git(args, cwd) {
  const forbidden = new Set(["clone", "fetch", "pull", "push"]);
  if (args.some((argument) => forbidden.has(argument))) throw new Error("remote Git operations are forbidden");
  const result = spawnSync("git", ["-c", "core.hooksPath=/dev/null", "-C", cwd, ...args], {
    encoding: "utf8",
    env: {
      ...process.env,
      HOME: gitHome,
      GIT_CONFIG_NOSYSTEM: "1",
      GIT_CONFIG_GLOBAL: "/dev/null",
    },
  });
  if (result.status !== 0) throw new Error(`git ${args.join(" ")} failed: ${result.stderr}`);
  return result.stdout.trim();
}

function createRepository(directory, branch = "main") {
  mkdirSync(directory, { recursive: true });
  git(["init", "-q", "-b", branch], directory);
  git([
    "-c", "user.name=WorktreeView E2E",
    "-c", "user.email=e2e@example.invalid",
    "commit", "-q", "--allow-empty", "-m", "initial",
  ], directory);
}

function select(directory) {
  const replacement = `${selector}.${process.pid}`;
  rmSync(replacement, { force: true });
  symlinkSync(directory, replacement);
  renameSync(replacement, selector);
}

async function expandRepository(repository) {
  await $(`nav.project-list button[title="${repository}"]`).click();
  await $(".sidebar-children").waitForDisplayed();
}

async function openSelectedRepository(expectedPath) {
  await $('button[aria-label="Open repository"]').click();
  if (!expectedPath) return;
  await browser.waitUntil(async () => {
    const active = await $('nav.project-list button[aria-current="true"]');
    if (!await active.isExisting()) return false;
    return await active.getAttribute("title") === expectedPath
      && await $(".inbox-pane").getAttribute("aria-busy") === "false";
  }, { timeoutMsg: `repository did not finish opening: ${expectedPath}` });
}

describe("bundled desktop surface pins and archived surfaces", () => {
  it("persists pins across restarts, badges gone pins, and archives the rest", async () => {
    const repository = path.join(fixtureRoot, "archive-repository");
    createRepository(repository);
    git(["branch", "e2e-pin"], repository);
    git(["branch", "e2e-archive"], repository);
    git(["branch", "e2e-gone"], repository);
    select(repository);
    await openSelectedRepository(repository);
    await expandRepository(repository);
    await expect($('//button[contains(@class, "sidebar-branch-row")][.//span[normalize-space()="e2e-pin"]]')).toBeDisplayed();
    await expect($('//button[contains(@class, "sidebar-branch-row")][.//span[normalize-space()="e2e-archive"]]')).toBeDisplayed();

    // Pinning a branch writes only to the app's own database; the pin
    // affordance is hover-revealed, so assert existence over display.
    await $('button[aria-label="Pin branch e2e-pin"]').click();
    await expect($('button[aria-label="Unpin branch e2e-pin"]')).toBeExisting();

    // The pin survives an application restart.
    const firstSession = browser.sessionId;
    await browser.reloadSession();
    expect(browser.sessionId).not.toBe(firstSession);
    await $(".worktree-row").waitForDisplayed();
    await expandRepository(repository);
    await expect($('button[aria-label="Unpin branch e2e-pin"]')).toBeExisting();

    // Unpinning clears the pin.
    await $('button[aria-label="Unpin branch e2e-pin"]').click();
    await expect($('button[aria-label="Pin branch e2e-pin"]')).toBeExisting();

    // Record the archive candidate by opening its review, then delete the
    // branches in the writable fixture copy: the unpinned recorded branch
    // archives, the pinned branch stays inline with a gone badge.
    await $('//button[contains(@class, "sidebar-branch-row")][.//span[normalize-space()="e2e-archive"]]').click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await $(`button=Overview`).click();
    await $('button[aria-label="Pin branch e2e-gone"]').click();
    await expect($('button[aria-label="Unpin branch e2e-gone"]')).toBeExisting();
    git(["branch", "-D", "e2e-pin", "e2e-archive", "e2e-gone"], repository);
    await browser.refresh();
    await $(".worktree-row").waitForDisplayed();
    await expandRepository(repository);

    const goneRow = await $('//button[contains(@class, "sidebar-branch-row")][.//span[normalize-space()="e2e-gone"]][.//span[normalize-space()="gone"]]');
    await expect(goneRow).toBeDisplayed();
    await expect($(".archived-section")).toBeDisplayed();
    await expect($('//button[contains(@class, "archived-row")][.//span[normalize-space()="e2e-archive"]]')).toBeDisplayed();

    // The archived search filters by label, and pinned gone surfaces are not
    // archived.
    const search = await $('input[aria-label^="Search archived surfaces"]');
    await search.setValue("no-such-branch");
    await expect($("div=No archived surfaces match")).toBeDisplayed();
    assert.equal(await $('//button[contains(@class, "archived-row")][.//span[normalize-space()="e2e-archive"]]').isExisting(), false);
    await search.setValue("e2e-archive");
    await expect($('//button[contains(@class, "archived-row")][.//span[normalize-space()="e2e-archive"]]')).toBeDisplayed();
    await search.setValue("e2e-gone");
    await expect($("div=No archived surfaces match")).toBeDisplayed();
    await expect(goneRow).toBeDisplayed();
  });
});
