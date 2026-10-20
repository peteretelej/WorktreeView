import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import { mkdirSync, renameSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
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

// The All files scope lives on commit-target reviews, so the flow opens the
// tip commit through the palette's commit results.
async function openCommitReviewWithScopeChip() {
  await $('button[aria-label="Find repositories and worktrees"]').click();
  const paletteInput = await $(".palette input");
  await paletteInput.waitForDisplayed();
  await paletteInput.setValue("feature file");
  const commitRow = await $('//div[contains(@class, "palette-results")]//button[contains(., "feature file")]');
  await expect(commitRow).toBeDisplayed();
  await commitRow.click();
  await expect($('section[aria-label="Code review"]')).toBeDisplayed();
  const scopeToggle = await $('[aria-label="Commit review preset"]');
  await expect(scopeToggle).toBeDisplayed();
  return scopeToggle;
}

describe("desktop all-files scope", () => {
  it("lists the whole tree from a commit review and keeps comments attached", async () => {
    const repository = path.join(fixtureRoot, "all-files-repository");
    createRepository(repository);
    // Two files across two commits: This commit on the tip shows one file,
    // All files shows the whole tree, so the scopes stay distinguishable.
    writeFileSync(path.join(repository, "first.txt"), "root content\n");
    git(["add", "first.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "root file"], repository);
    git(["checkout", "-q", "-b", "feature"], repository);
    writeFileSync(path.join(repository, "second.txt"), "feature content\n");
    git(["add", "second.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "feature file"], repository);
    select(repository);

    await openSelectedRepository(repository);
    const scopeToggle = await openCommitReviewWithScopeChip();

    // All files: the chip swaps the base to the empty tree, the range chip
    // renames itself, and the counts lose the +/- totals.
    await scopeToggle.$("button=All files").click();
    await browser.waitUntil(async () => /All files · /.test(await $(".range-chip code").getText()), { timeoutMsg: "range chip did not switch to All files" });
    await browser.waitUntil(async () => (await $$("button.file-row")).length >= 2, { timeoutMsg: "all-files tree did not list every file" });
    const counts = await $(".review-counts code").getText();
    assert.match(counts, /^\d+ files$/);
    assert.doesNotMatch(counts, /\+\d+/);

    // Comments written on the all-files identity come back on it.
    const fileRow = await $('//button[contains(@class, "file-row")][.//span[normalize-space()="second.txt"]]');
    await fileRow.click();
    await expect($('code[title="second.txt"]')).toBeDisplayed();
    const addedRow = await $('//div[contains(@class, "diff-line")][contains(@class, "addition")][contains(., "feature content")]');
    await expect(addedRow).toBeDisplayed();
    await addedRow.$(".line-number").click();
    const chip = await $(".selection-comment-chip");
    await chip.waitForDisplayed();
    await chip.click();
    const composer = await $('textarea[aria-label="Add comment"]');
    await composer.waitForDisplayed();
    await composer.setValue("all-files note");
    await $('button[aria-label="Comment on second.txt lines 1-1"], .comment-composer-panel.inline .primary-button').click();
    await browser.waitUntil(async () => (await $$(".inline-comment .comment-card")).length >= 1, { timeoutMsg: "inline comment did not render" });

    // Back to This commit, then All files again: the comment stays on the
    // all-files identity and the tree returns without refetch surprises.
    await $('[aria-label="Commit review preset"]').$("button=This commit").click();
    await browser.waitUntil(async () => /\.\.\./.test(await $(".range-chip code").getText()), { timeoutMsg: "range chip did not switch back" });
    await $('[aria-label="Commit review preset"]').$("button=All files").click();
    await browser.waitUntil(async () => (await $$("button.file-row")).length >= 2, { timeoutMsg: "all-files tree did not return" });
    await $('//button[contains(@class, "file-row")][.//span[normalize-space()="second.txt"]]').click();
    await expect($('code[title="second.txt"]')).toBeDisplayed();
    await browser.waitUntil(async () => (await $$(".inline-comment .comment-card")).length >= 1, { timeoutMsg: "all-files comment did not persist across scope switches" });
  });
});
