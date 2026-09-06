import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import {
  appendFileSync,
  chmodSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  readlinkSync,
  renameSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";

const fixture = "/fixtures/worktreeview";
const fixtureRoot = "/tmp/worktreeview-e2e-fixtures";
const gitHome = "/tmp/worktreeview-e2e-git-home";
const selector = "/tmp/worktreeview-e2e-selection";
const hookMarker = "/artifacts/hook-marker";

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

async function projectPaths() {
  const paths = [];
  for (const row of await $$("nav.project-list button")) paths.push(await row.getAttribute("title"));
  return paths;
}

function descendantPids(rootPid) {
  const parents = new Map();
  for (const entry of readdirSync("/proc")) {
    if (!/^\d+$/.test(entry)) continue;
    try {
      const status = readFileSync(`/proc/${entry}/status`, "utf8");
      const parent = status.match(/^PPid:\s+(\d+)$/m)?.[1];
      if (parent) parents.set(Number(entry), Number(parent));
    } catch {
      // Processes can exit while /proc is being inspected.
    }
  }
  const descendants = new Set([rootPid]);
  let changed = true;
  while (changed) {
    changed = false;
    for (const [pid, parent] of parents) {
      if (descendants.has(parent) && !descendants.has(pid)) {
        descendants.add(pid);
        changed = true;
      }
    }
  }
  descendants.delete(rootPid);
  return [...descendants];
}

async function proveBackendStderr() {
  await browser.waitUntil(() => {
    const driverPid = Number(readFileSync("/tmp/worktreeview-tauri-driver.pid", "utf8").trim());
    const backend = statSync("/artifacts/backend.log");
    return descendantPids(driverPid).some((pid) => {
      try {
        const command = readFileSync(`/proc/${pid}/comm`, "utf8").trim();
        if (command !== "worktreeview") return false;
        const stderr = statSync(`/proc/${pid}/fd/2`);
        return stderr.dev === backend.dev && stderr.ino === backend.ino;
      } catch {
        return false;
      }
    });
  }, { timeout: 15_000, timeoutMsg: "application stderr did not inherit backend.log" });
  appendFileSync("/artifacts/runner.log", "backend-stderr-proven\n");
}

describe("bundled desktop lifecycle", () => {
  it("persists valid repositories and handles failures and large collections", async () => {
    const heading = await $("h1=Worktrees");
    await heading.waitForDisplayed();
    await expect($("strong=No repositories")).toBeDisplayed();
    await proveBackendStderr();

    select(fixture);
    const branch = git(["branch", "--show-current"], fixture) || "detached";
    const head = git(["rev-parse", "HEAD"], fixture);
    await openSelectedRepository(fixture);
    await $(".worktree-row").waitForDisplayed();
    await expect($(`.worktree-row[title="${fixture}"]`)).toBeDisplayed();
    await expect($(`strong=${branch}`)).toBeDisplayed();
    await expect($(`code=${head.slice(0, 7)}`)).toBeDisplayed();

    const sidebarRepository = path.join(fixtureRoot, "sidebar-repository");
    createRepository(sidebarRepository);
    git(["branch", "e2e-sidebar-branch"], sidebarRepository);
    select(sidebarRepository);
    await openSelectedRepository(sidebarRepository);
    await $(`nav.project-list button[title="${sidebarRepository}"]`).click();
    await expect($(`.sidebar-children button.sidebar-worktree-row`)).toBeDisplayed();
    await expect($(`.sidebar-children button.sidebar-branch-row`)).toBeDisplayed();
    await $(`.sidebar-children button.sidebar-branch-row`).click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await expect($('span[aria-label="Review scope"]')).toHaveText("Committed only");
    await $('button=Worktrees').click();
    await $(`.sidebar-children button.sidebar-worktree-row`).click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await $('button=All changes').click();
    await $('button=Worktrees').click();
    await $(`button[aria-label="Pin repository"]`).click();
    await expect($("div.nav-section-label=Pinned")).toBeDisplayed();
    await browser.refresh();
    await expect($("div.nav-section-label=Pinned")).toBeDisplayed();
    await expect($(`nav.project-list button[title="${sidebarRepository}"]`)).toBeDisplayed();
    await $(`button[aria-label="Unpin repository"]`).click();
    await expect($("div.nav-section-label=Recent")).toBeDisplayed();

    await browser.refresh();
    await $(".worktree-row").waitForDisplayed();
    await expect($(`.worktree-row[title="${sidebarRepository}"]`)).toBeDisplayed();

    const firstSession = browser.sessionId;
    await browser.reloadSession();
    expect(browser.sessionId).not.toBe(firstSession);
    await $(".worktree-row").waitForDisplayed();
    await expect($(`.worktree-row[title="${sidebarRepository}"]`)).toBeDisplayed();

    const detached = path.join(fixtureRoot, "detached-repository");
    createRepository(detached);
    git(["checkout", "-q", "--detach"], detached);
    select(detached);
    await openSelectedRepository(detached);
    await expect($("strong=detached")).toBeDisplayed();

    const invalid = path.join(fixtureRoot, "not-a-repository");
    mkdirSync(invalid);
    select(invalid);
    await openSelectedRepository();
    await expect($(".operation-error")).toHaveText("The selected folder is not a Git repository and was not added.");
    assert.equal((await projectPaths()).includes(invalid), false);

    const removed = path.join(fixtureRoot, "removed-repository");
    createRepository(removed);
    select(removed);
    await openSelectedRepository(removed);
    rmSync(removed, { recursive: true });
    await browser.refresh();
    await expect($("strong=Worktrees unavailable")).toBeDisplayed();
    await expect($("h1=Worktrees")).toBeDisplayed();

    const large = path.join(fixtureRoot, "large-search-repository");
    createRepository(large, "e2e-search-main");
    const hooks = path.join(large, ".git", "hooks");
    mkdirSync(hooks, { recursive: true });
    const hook = path.join(hooks, "post-checkout");
    writeFileSync(hook, `#!/bin/sh\n: > ${hookMarker}\n`, { mode: 0o700 });
    for (let index = 1; index <= 100; index += 1) {
      const suffix = String(index).padStart(3, "0");
      git(["worktree", "add", "-q", "-b", `e2e-search-${suffix}`, path.join(fixtureRoot, `worktree-${suffix}`)], large);
    }
    expect(existsSync(hookMarker)).toBe(false);
    select(large);
    await openSelectedRepository(large);
    await browser.waitUntil(async () => (await $$(".worktree-row")).length === 100);
    await expect($('[aria-label="Worktree pages"]')).toHaveText(expect.stringContaining("1-100 of 101"));

    await $('button[aria-label="Find repositories and worktrees"]').click();
    await $('.palette-input-row input').setValue("e2e-search");
    await browser.waitUntil(async () => (await $$(".palette-results > button")).length === 50);
    await expect($('[aria-label="Search result pages"]')).toHaveText(expect.stringContaining("1-50 of 101"));
    await $('button[aria-label="Close search"]').click();
    await expect($("h1=Worktrees")).toBeDisplayed();
    expect(existsSync(hookMarker)).toBe(false);

    if (process.env.WORKTREEVIEW_E2E_FORCE_FAILURE === "1") {
      expect("forced failure").toBe("successful run");
    }

    appendFileSync("/artifacts/runner.log", "screenshot-state-proven\n");
    const screenshot = spawnSync("scrot", ["/artifacts/worktreeview.png"], { stdio: "inherit" });
    expect(screenshot.status).toBe(0);
    expect(lstatSync("/artifacts/worktreeview.png").isFile()).toBe(true);
    expect(readlinkSync(selector)).toBe(large);
  });

  it("opens a worktree review and changes scope and base", async () => {
    const repository = path.join(fixtureRoot, "review-repository");
    createRepository(repository);
    writeFileSync(path.join(repository, "README.md"), "main\n");
    writeFileSync(path.join(repository, ".gitignore"), "ignored.txt\n");
    git(["add", "README.md", ".gitignore"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "main file"], repository);
    git(["checkout", "-q", "-b", "feature"], repository);
    writeFileSync(path.join(repository, "feature.txt"), "feature committed\n");
    writeFileSync(path.join(repository, "large-hunk.txt"), Array.from({ length: 1200 }, (_, index) => `large line ${index + 1}`).join("\n") + "\n");
    git(["add", "feature.txt", "large-hunk.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "feature"], repository);
    git(["checkout", "-q", "main"], repository);
    git(["checkout", "-q", "-b", "other"], repository);
    writeFileSync(path.join(repository, "other.txt"), "other committed\n");
    git(["add", "other.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "other"], repository);
    git(["checkout", "-q", "main"], repository);
    for (let index = 1; index <= 120; index += 1) git(["branch", `review-base-${String(index).padStart(3, "0")}`], repository);
    const worktree = path.join(fixtureRoot, "review-feature");
    git(["worktree", "add", "-q", worktree, "feature"], repository);
    writeFileSync(path.join(worktree, "README.md"), "main\nuncommitted\n");
    writeFileSync(path.join(worktree, "untracked.txt"), "untracked\n");
    writeFileSync(path.join(worktree, "ignored.txt"), "must not appear\n");
    const switchRepository = path.join(fixtureRoot, "review-switch-repository");
    createRepository(switchRepository);
    select(repository);
    await openSelectedRepository(repository);
    const featureRow = await $('//button[contains(@class, "worktree-row")][.//strong[normalize-space()="feature"]]');
    await expect(featureRow).toBeDisplayed();
    await featureRow.click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await expect($(".review-counts")).toHaveText("4 files, +1202 -0");
    const initialFiles = await browser.execute(() => Array.from(document.querySelectorAll(".file-row span"), (file) => file.textContent ?? ""));
    assert.deepEqual(initialFiles.sort(), ["README.md", "feature.txt", "large-hunk.txt", "untracked.txt"].sort());
    assert.equal(await $("span=ignored.txt").isExisting(), false);

    select(switchRepository);
    await openSelectedRepository(switchRepository);
    await expect($("h1=Worktrees")).toBeDisplayed();
    assert.equal(await $('section[aria-label="Code review"]').isExisting(), false);

    await $('button[aria-label="Find repositories and worktrees"]').click();
    await $(".palette-input-row input").setValue("review-feature");
    const worktreePaletteResult = await $(`//div[contains(@class, "palette-results")]/button[@title="${worktree}"]`);
    await expect(worktreePaletteResult).toBeDisplayed();
    await worktreePaletteResult.click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await expect($(".review-heading h1")).toHaveText("feature");
    await expect($(".review-counts")).toHaveText("4 files, +1202 -0");

    await $('button[aria-label="Find repositories and worktrees"]').click();
    await $(".palette-input-row input").setValue("review-switch-repository");
    const repositoryPaletteResult = await $(`//div[contains(@class, "palette-results")]/button[@title="${switchRepository}"]`);
    await expect(repositoryPaletteResult).toBeDisplayed();
    await repositoryPaletteResult.click();
    await expect($("h1=Worktrees")).toBeDisplayed();
    assert.equal(await $('section[aria-label="Code review"]').isExisting(), false);

    await $('button[aria-label="Find repositories and worktrees"]').click();
    await $(".palette-input-row input").setValue("review-feature");
    const reopenedWorktreePaletteResult = await $(`//div[contains(@class, "palette-results")]/button[@title="${worktree}"]`);
    await expect(reopenedWorktreePaletteResult).toBeDisplayed();
    await reopenedWorktreePaletteResult.click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await expect($(".review-counts")).toHaveText("4 files, +1202 -0");

    const baseSearch = await $('input[aria-controls="review-base-options"]');
    await baseSearch.click();
    await browser.waitUntil(async () => (await $$('#review-base-options > button[role="option"]')).length === 50);
    await expect($('#review-base-options [aria-label="Review base branch pages"]')).toHaveText(expect.stringContaining("1-50 of 123"));
    await browser.keys("Escape");
    await browser.execute(() => {
      const buttons = Array.from(document.querySelectorAll(".scope-toggle button"));
      buttons.find((button) => button.textContent === "Committed only")?.click();
      buttons.find((button) => button.textContent === "All changes")?.click();
      buttons.find((button) => button.textContent === "Committed only")?.click();
    });
    await browser.waitUntil(async () => !await $("span=untracked.txt").isExisting());
    await browser.pause(500);
    assert.equal(await $("span=untracked.txt").isExisting(), false);
    assert.equal(await $("span=README.md").isExisting(), false);
    await expect($("span=feature.txt")).toBeDisplayed();
    await expect($("span=large-hunk.txt")).toBeDisplayed();
    await expect($(".review-counts")).toHaveText("2 files, +1201 -0");
    await $('button=All changes').click();
    await expect($("span=untracked.txt")).toBeDisplayed();
    await expect($("span=README.md")).toBeDisplayed();
    assert.equal(await $("span=ignored.txt").isExisting(), false);
    await expect($(".review-counts")).toHaveText("4 files, +1202 -0");
    const trackedFile = await $('//button[contains(@class, "file-row")][.//span[normalize-space()="README.md"]]');
    await expect(trackedFile).toBeDisplayed();
    await trackedFile.click();
    await expect($('//code[contains(., "uncommitted")]')).toBeDisplayed();
    const readmeGutters = await browser.execute(() => Array.from(document.querySelectorAll(".diff-line"), (line) =>
      Array.from(line.querySelectorAll(".line-number"), (gutter) => gutter.textContent?.trim() ?? "")));
    assert.deepEqual(readmeGutters, [["1", "1"], ["", "2"]]);

    const largeFile = await $('//button[contains(@class, "file-row")][.//span[normalize-space()="large-hunk.txt"]]');
    await largeFile.click();
    await browser.waitUntil(async () => (await $$(".diff-line")).length === 500);
    await browser.execute(() => {
      const button = Array.from(document.querySelectorAll(".file-row")).find((row) => row.textContent?.includes("large-hunk.txt"));
      if (button instanceof HTMLElement) button.focus();
    });
    await browser.waitUntil(async () => await browser.execute(() => document.activeElement?.querySelector("span")?.textContent) === "large-hunk.txt");
    await browser.keys("ArrowUp");
    await browser.waitUntil(async () => await browser.execute(() => document.activeElement?.querySelector("span")?.textContent) === "feature.txt");
    await expect($('//button[contains(@class, "file-row")][.//span[normalize-space()="feature.txt"]]')).toHaveAttribute("aria-selected", "true");
    await browser.waitUntil(async () => await browser.execute(() => document.querySelector(".patch-heading code")?.textContent) === "feature.txt");
    await browser.keys("ArrowDown");
    await browser.waitUntil(async () => await browser.execute(() => document.activeElement?.querySelector("span")?.textContent) === "large-hunk.txt");
    await expect(largeFile).toHaveAttribute("aria-selected", "true");
    await browser.waitUntil(async () => await browser.execute(() => document.querySelector(".patch-heading code")?.textContent) === "large-hunk.txt");
    await browser.waitUntil(async () => (await $$(".diff-line")).length === 500);
    await expect($('[aria-label="Patch pages"]')).toHaveText(expect.stringContaining("Page 1 of 3"));
    let largeGutters = await browser.execute(() => Array.from(document.querySelectorAll(".diff-line"), (line) =>
      Array.from(line.querySelectorAll(".line-number"), (gutter) => gutter.textContent?.trim() ?? "")));
    assert.deepEqual(largeGutters[0], ["", "1"]);
    assert.deepEqual(largeGutters.at(-1), ["", "500"]);
    await $('//div[@aria-label="Patch pages"]//button[normalize-space()="Next"]').click();
    await expect($('[aria-label="Patch pages"]')).toHaveText(expect.stringContaining("Page 2 of 3"));
    largeGutters = await browser.execute(() => Array.from(document.querySelectorAll(".diff-line"), (line) =>
      Array.from(line.querySelectorAll(".line-number"), (gutter) => gutter.textContent?.trim() ?? "")));
    assert.deepEqual(largeGutters[0], ["", "501"]);
    assert.deepEqual(largeGutters.at(-1), ["", "1000"]);
    await $('//div[@aria-label="Patch pages"]//button[normalize-space()="Next"]').click();
    await expect($('[aria-label="Patch pages"]')).toHaveText(expect.stringContaining("Page 3 of 3"));
    largeGutters = await browser.execute(() => Array.from(document.querySelectorAll(".diff-line"), (line) =>
      Array.from(line.querySelectorAll(".line-number"), (gutter) => gutter.textContent?.trim() ?? "")));
    assert.deepEqual(largeGutters.at(-1), ["", "1200"]);

    await browser.executeAsync((done) => {
      Array.from(document.querySelectorAll(".file-row")).find((row) => row.textContent?.includes("large-hunk.txt"))?.click();
      setTimeout(() => {
        Array.from(document.querySelectorAll(".file-row")).find((row) => row.textContent?.includes("README.md"))?.click();
        done();
      }, 0);
    });
    await browser.waitUntil(async () => await browser.execute(() => document.querySelector(".patch-heading code")?.textContent) === "README.md");
    await browser.pause(500);
    await expect($('//code[contains(., "uncommitted")]')).toBeDisplayed();
    rmSync(path.join(worktree, "untracked.txt"));
    await $('//button[contains(@class, "file-row")][.//span[normalize-space()="untracked.txt"]]').click();
    await expect($("strong=Patch not rendered")).toBeDisplayed();
    await expect($("span=The selected file is not an untracked review file.")).toBeDisplayed();
    await baseSearch.setValue("other");
    assert.equal(await $("span=other.txt").isExisting(), false);
    await $('//div[@id="review-base-options"]//button[normalize-space()="other"]').click();
    await expect($("span=other.txt")).toBeDisplayed();
    await expect($(".review-counts")).toHaveText("4 files, +1202 -1");
    git(["config", "core.filemode", "true"], repository);
    chmodSync(path.join(worktree, ".gitignore"), 0o755);
    await $('button=Committed only').click();
    await $('button=All changes').click();
    await expect($(".review-counts")).toHaveText("5 files, +1202 -1");
    assert.equal(await $("span=ignored.txt").isExisting(), false);
    await $('//button[contains(@class, "file-row")][.//span[normalize-space()=".gitignore"]]').click();
    await expect($('pre.patch-metadata')).toHaveText(expect.stringContaining("old mode 100644"));
    await expect($('pre.patch-metadata')).toHaveText(expect.stringContaining("new mode 100755"));
    const filterMarker = path.join(fixtureRoot, "filter-ran");
    const filterHelper = path.join(fixtureRoot, "filter-helper.sh");
    writeFileSync(filterHelper, `#!/bin/sh\n: > ${filterMarker}\ncat\n`, { mode: 0o700 });
    writeFileSync(path.join(worktree, ".gitattributes"), "*.txt filter=e2e\n");
    git(["config", "filter.e2e.clean", filterHelper], repository);
    await $('button=Committed only').click();
    await $('button=All changes').click();
    await expect($("strong=Review unavailable")).toBeDisplayed();
    await expect($("span=This review cannot run because Git conversion filters apply to files in this review.")).toBeDisplayed();
    expect(existsSync(filterMarker)).toBe(false);
  });
});
