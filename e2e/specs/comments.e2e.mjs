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

function inventoryRow(name) {
  return $(`//div[contains(@class, "worktree-row")][.//strong[normalize-space()="${name}"]]`);
}

describe("desktop comments", () => {
  it("leaves a comment on a file line that survives restart", async () => {
    const repository = path.join(fixtureRoot, "comments-repository");
    createRepository(repository);
    writeFileSync(path.join(repository, "reviewed.txt"), "line one\nline two\nline three\n");
    git(["add", "reviewed.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "base file"], repository);
    git(["checkout", "-q", "-b", "feature"], repository);
    writeFileSync(path.join(repository, "reviewed.txt"), "line one\nline two changed\nline three\nline four\n");
    git(["add", "reviewed.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "feature change"], repository);
    select(repository);

    await openSelectedRepository(repository);
    const featureRow = await inventoryRow("feature");
    await expect(featureRow).toBeDisplayed();
    await featureRow.$(".branch-title").click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    const fileRow = await $('//button[contains(@class, "file-row")][.//span[normalize-space()="reviewed.txt"]]');
    await fileRow.click();
    await expect($('code[title="reviewed.txt"]')).toBeDisplayed();

    // Select the added line in the diff; the inline composer opens on the
    // selection and posts a comment anchored to that line.
    const addedRow = await $('//div[contains(@class, "diff-line")][contains(@class, "addition")][contains(., "line two changed")]');
    await expect(addedRow).toBeDisplayed();
    await addedRow.click();
    const composer = await $('textarea[aria-label="Add comment"]');
    await composer.waitForDisplayed();
    const body = "e2e says hello";
    await composer.setValue(body);
    await $('button[aria-label="Comment on reviewed.txt lines 2-2"], .comment-composer-panel.inline .primary-button').click();

    // Inline render at the anchored row plus a stream entry.
    await browser.waitUntil(async () => (await $$(".inline-comment .comment-card")).length >= 1, { timeoutMsg: "inline comment did not render" });
    const inlineCard = await $(".inline-comment .comment-card");
    assert.ok((await inlineCard.getText()).includes(body));
    await browser.waitUntil(async () => (await $$('aside[aria-label="Comments"] .comment-thread')).length >= 1, { timeoutMsg: "stream entry did not render" });
    const stream = await $('aside[aria-label="Comments"]');
    assert.ok((await stream.getText()).includes(body));

    // The comment survives an app restart: the store persists it under the
    // review identity, and a fresh session re-loads it.
    await browser.reloadSession();
    await openSelectedRepository(repository);
    const featureRowAfterRestart = await inventoryRow("feature");
    await featureRowAfterRestart.$(".branch-title").click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await $('//button[contains(@class, "file-row")][.//span[normalize-space()="reviewed.txt"]]').click();
    await expect($('code[title="reviewed.txt"]')).toBeDisplayed();
    await browser.waitUntil(async () => (await $$(".inline-comment .comment-card")).length >= 1, { timeoutMsg: "comment did not persist across restart" });
    const persisted = await $(".inline-comment .comment-card");
    assert.ok((await persisted.getText()).includes(body));
    const streamAfterRestart = await $('aside[aria-label="Comments"]');
    assert.ok((await streamAfterRestart.getText()).includes(body));
  });
});
