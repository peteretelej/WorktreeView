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

function inventoryRow(name) {
  return $(`//div[contains(@class, "worktree-row")][.//strong[normalize-space()="${name}"]]`);
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

const INBOX_TERMINAL_STATES = ["No repositories", "No worktrees", "Worktrees unavailable", "Repositories could not be loaded"];

async function showWorktreeList() {
  // A repository lands on its project overview: the inbox pane showing the
  // worktree rows. Otherwise the inbox shows a terminal empty or error
  // state; loading states keep the wait polling.
  // Existence checks only: wdio element reads like getAttribute implicitly
  // re-find a vanished element for waitforTimeout before failing.
  await browser.waitUntil(async () => {
    if (await $(".inbox-pane .worktree-row").isExisting()) return true;
    for (const heading of INBOX_TERMINAL_STATES) {
      if (await $(`strong=${heading}`).isExisting()) return true;
    }
    return false;
  }, { timeoutMsg: "repository surface did not settle" });
}

async function openSelectedRepository(expectedPath) {
  await $('button[aria-label="Open repository"]').click();
  if (!expectedPath) return;
  await browser.waitUntil(async () => {
    const active = await $('nav.project-list button[aria-current="true"]');
    if (!await active.isExisting()) return false;
    return await active.getAttribute("title") === expectedPath;
  }, { timeoutMsg: `repository did not finish opening: ${expectedPath}` });
  await showWorktreeList();
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
    const subject = git(["log", "-1", "--format=%s"], fixture);
    await openSelectedRepository(fixture);
    await $(".worktree-row").waitForDisplayed();
    await expect($(`.worktree-row[title="${fixture}"]`)).toBeDisplayed();
    await expect($(`.worktree-row[title="${fixture}"] .branch-title strong`)).toHaveText(branch);
    // The branch inventory pass runs cold alongside the rest of hydration;
    // give the head-commit cell time to fill in. The text is read from the
    // DOM: the driver reports empty text for plain spans.
    await browser.waitUntil(async () => await browser.execute((rowTitle, expected) => {
      return document.querySelector(`.worktree-row[title="${rowTitle}"] .row-commit-subject`)?.textContent ?? "";
    }, fixture, subject) === subject, { timeout: 45_000, timeoutMsg: "worktree head commit did not render" });

    const sidebarRepository = path.join(fixtureRoot, "sidebar-repository");
    createRepository(sidebarRepository);
    git(["branch", "e2e-sidebar-branch"], sidebarRepository);
    select(sidebarRepository);
    await openSelectedRepository(sidebarRepository);
    await $(`nav.project-list button[title="${sidebarRepository}"]`).click();
    await expect($(`.sidebar-children button.sidebar-worktree-row`)).toBeDisplayed();
    // Unpinned branches stay off the sidebar; the overview's Branches tab
    // lists them.
    await $(`//button[contains(@class, "overview-tab")][contains(normalize-space(.), "Branches")]`).click();
    const sidebarBranchRow = await inventoryRow("e2e-sidebar-branch");
    await sidebarBranchRow.waitForDisplayed();
    // Click the branch title: the row's center column carries the status
    // chips, which open their own views.
    await sidebarBranchRow.$(".branch-title").click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await expect($('span[aria-label="Review scope"]')).toHaveText("Committed only");
    await $(".crumb-link").click();
    await $(`.sidebar-children button.sidebar-worktree-row`).click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await $('button=All changes').click();
    await $(".crumb-link").click();
    await $(`button[aria-label="Pin repository"]`).click();
    await expect($("div.nav-section-label=Pinned")).toBeDisplayed();
    await browser.refresh();
    await expect($("div.nav-section-label=Pinned")).toBeDisplayed();
    await expect($(`nav.project-list button[title="${sidebarRepository}"]`)).toBeDisplayed();
    await $(`button[aria-label="Unpin repository"]`).click();
    await expect($("div.nav-section-label=Recent")).toBeDisplayed();

    await browser.refresh();
    await showWorktreeList();
    await $(".worktree-row").waitForDisplayed();
    await expect($(`.worktree-row[title="${sidebarRepository}"]`)).toBeDisplayed();

    const firstSession = browser.sessionId;
    await browser.reloadSession();
    expect(browser.sessionId).not.toBe(firstSession);
    await showWorktreeList();
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
    await expect($("h1=removed-repository")).toBeDisplayed();

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
    await expect($("h1=large-search-repository")).toBeDisplayed();
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
    // README carries a shared filler body so the worktree's scattered edits
    // diff as nine separate change regions for the navigation assertions.
    const mainReadmeLines = ["main", "", ...Array.from({ length: 192 }, (_, index) => `filler-${index + 3}`)];
    writeFileSync(path.join(repository, "README.md"), mainReadmeLines.join("\n") + "\n");
    writeFileSync(path.join(repository, ".gitignore"), "ignored.txt\n");
    git(["add", "README.md", ".gitignore"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "main file"], repository);
    git(["checkout", "-q", "-b", "feature"], repository);
    writeFileSync(path.join(repository, "feature.txt"), "feature committed\n");
    writeFileSync(path.join(repository, "large-hunk.txt"), Array.from({ length: 6000 }, (_, index) => `large line ${index + 1}`).join("\n") + "\n");
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
    // Nine pure insertions (line 2, then every 24th line) into the shared
    // filler body: nine separate change regions for the navigation steps.
    const insertionLines = new Set([2, 26, 50, 74, 98, 122, 146, 170, 194]);
    const worktreeReadmeLines = [];
    let source = 0;
    for (let line = 1; line <= 194; line += 1) {
      if (insertionLines.has(line)) worktreeReadmeLines.push(line === 2 ? "uncommitted" : `scattered-${line}`);
      else if (source < mainReadmeLines.length) worktreeReadmeLines.push(mainReadmeLines[source++]);
    }
    while (source < mainReadmeLines.length) worktreeReadmeLines.push(mainReadmeLines[source++]);
    writeFileSync(path.join(worktree, "README.md"), worktreeReadmeLines.join("\n") + "\n");
    writeFileSync(path.join(worktree, "untracked.txt"), "untracked\n");
    writeFileSync(path.join(worktree, "ignored.txt"), "must not appear\n");
    // A renderable image asset and a file past the render cap, both
    // untracked so the review counts stay untouched.
    writeFileSync(path.join(worktree, "zz-tiny.png"), Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==", "base64"));
    writeFileSync(path.join(worktree, "zz-capped.txt"), Array.from({ length: 51000 }, (_, index) => `cap line ${index + 1}`).join("\n") + "\n");
    const switchRepository = path.join(fixtureRoot, "review-switch-repository");
    createRepository(switchRepository);
    select(repository);
    await openSelectedRepository(repository);
    const featureRow = await inventoryRow("feature");
    await expect(featureRow).toBeDisplayed();
    await featureRow.$(".branch-title").click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await expect($(".review-counts")).toHaveText("6 files, +6010 -0");
    const initialFiles = await browser.execute(() => Array.from(document.querySelectorAll(".file-row span"), (file) => file.textContent ?? ""));
    assert.deepEqual(initialFiles.sort(), ["README.md", "feature.txt", "large-hunk.txt", "untracked.txt", "zz-capped.txt", "zz-tiny.png"].sort());
    assert.equal(await $("span=ignored.txt").isExisting(), false);

    select(switchRepository);
    await openSelectedRepository(switchRepository);
    await expect($("h1=review-switch-repository")).toBeDisplayed();
    assert.equal(await $('section[aria-label="Code review"]').isExisting(), false);

    await $('button[aria-label="Find repositories and worktrees"]').click();
    await $(".palette-input-row input").setValue("review-feature");
    const worktreePaletteResult = await $(`//div[contains(@class, "palette-results")]/button[@title="${worktree}"]`);
    await expect(worktreePaletteResult).toBeDisplayed();
    await worktreePaletteResult.click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    // The commit row's title is a plain span: the driver reads its text from
    // the DOM instead of via getElementText, which reports empty for spans.
    await browser.waitUntil(async () => await browser.execute(() => document.querySelector(".commit-title")?.textContent) === "feature");
    await expect($(".review-counts")).toHaveText("6 files, +6010 -0");

    await $('button[aria-label="Find repositories and worktrees"]').click();
    await $(".palette-input-row input").setValue("review-switch-repository");
    const repositoryPaletteResult = await $(`//div[contains(@class, "palette-results")]/button[@title="${switchRepository}"]`);
    await expect(repositoryPaletteResult).toBeDisplayed();
    await repositoryPaletteResult.click();
    await showWorktreeList();
    await expect($("h1=review-switch-repository")).toBeDisplayed();
    assert.equal(await $('section[aria-label="Code review"]').isExisting(), false);

    await $('button[aria-label="Find repositories and worktrees"]').click();
    await $(".palette-input-row input").setValue("review-feature");
    const reopenedWorktreePaletteResult = await $(`//div[contains(@class, "palette-results")]/button[@title="${worktree}"]`);
    await expect(reopenedWorktreePaletteResult).toBeDisplayed();
    await reopenedWorktreePaletteResult.click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await expect($(".review-counts")).toHaveText("6 files, +6010 -0");

    // The pickers live behind the compare chip; opening it mounts them.
    await $(".range-chip").click();
    const baseSearch = await $('input[aria-controls="review-base-options"]');
    await baseSearch.click();
    await browser.waitUntil(async () => (await $$('#review-base-options > button[role="option"]')).length === 50);
    await expect($('#review-base-options [aria-label="Base branch pages"]')).toHaveText(expect.stringContaining("1-50 of 123"));
    await browser.keys("Escape");
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
    await expect($(".review-counts")).toHaveText("2 files, +6001 -0");
    await $('button=All changes').click();
    await expect($("span=untracked.txt")).toBeDisplayed();
    await expect($("span=README.md")).toBeDisplayed();
    assert.equal(await $("span=ignored.txt").isExisting(), false);
    await expect($(".review-counts")).toHaveText("6 files, +6010 -0");
    const trackedFile = await $('//button[contains(@class, "file-row")][.//span[normalize-space()="README.md"]]');
    await expect(trackedFile).toBeDisplayed();
    await trackedFile.click();
    await expect($('//code[contains(., "uncommitted")]')).toBeDisplayed();
    const readmeGutters = await browser.execute(() => Array.from(document.querySelectorAll(".diff-line"), (line) =>
      Array.from(line.querySelectorAll(".line-number"), (gutter) => gutter.textContent?.trim() ?? "")));
    assert.deepEqual(readmeGutters.slice(0, 2), [["1", "1"], ["", "2"]]);

    const largeFile = await $('//button[contains(@class, "file-row")][.//span[normalize-space()="large-hunk.txt"]]');
    await largeFile.click();
    await browser.waitUntil(async () => await browser.execute(() => document.querySelector(".patch-heading code")?.textContent) === "large-hunk.txt");
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
    // The whole stream renders in native flow: the full 6000-line patch is
    // in the DOM exactly once, and native scrolling covers it end to end.
    const streamGutters = () => browser.execute(() => Array.from(document.querySelectorAll(".diff-line"), (line) =>
      Array.from(line.querySelectorAll(".line-number"), (gutter) => gutter.textContent?.trim() ?? "")));
    await browser.waitUntil(async () => (await $$(".diff-line")).length === 6000);
    let largeGutters = await streamGutters();
    assert.deepEqual(largeGutters[0], ["", "1"]);
    await browser.execute(() => { const scroll = document.querySelector(".patch-scroll"); if (scroll) scroll.scrollTop += 2000; });
    await browser.pause(300);
    await browser.execute(() => { const scroll = document.querySelector(".patch-scroll"); if (scroll) scroll.scrollTop = 0; });
    await browser.waitUntil(async () => {
      const gutters = await streamGutters();
      return gutters[0]?.[1] === "1" && gutters.length === 6000;
    }, { timeoutMsg: "stream did not return intact after scrolling away and back" });
    await browser.execute(() => { const scroll = document.querySelector(".patch-scroll"); if (scroll) scroll.scrollTop = scroll.scrollHeight; });
    await browser.waitUntil(async () => {
      const gutters = await streamGutters();
      return gutters.at(-1)?.[1] === "6000";
    });

    // Full-file view: every row carries exactly one gutter, and the bottom
    // of the file is reachable by native scrolling.
    await $('button[title="Full file view"]').click();
    await browser.waitUntil(async () => (await $$(".file-view .diff-line")).length === 6000);
    const fileRowCount = await browser.execute(() => document.querySelectorAll(".file-view .diff-line").length);
    const fileGutterCount = await browser.execute(() => document.querySelectorAll(".file-view .diff-line .line-number").length);
    assert.equal(fileGutterCount, fileRowCount, "file view rows must carry exactly one line-number gutter");
    await browser.execute(() => { const scroll = document.querySelector(".patch-scroll"); if (scroll) scroll.scrollTop = scroll.scrollHeight; });
    await browser.waitUntil(async () => {
      const gutters = await streamGutters();
      return gutters.at(-1)?.[0] === "6000";
    });
    await $('button[title="Diff view"]').click();
    await browser.waitUntil(async () => (await $$(".diff-line")).length > 0);

    // Change navigation: Next moves the viewport to the first region below
    // it and the counter follows; Previous walks back. README.md carries
    // nine separate change regions.
    await browser.execute(() => {
      Array.from(document.querySelectorAll(".file-row")).find((row) => row.textContent?.includes("README.md"))?.click();
    });
    await browser.waitUntil(async () => await browser.execute(() => document.querySelector(".patch-heading code")?.textContent) === "README.md");
    await browser.waitUntil(async () => (await $$(".diff-line")).length > 0);
    const navCount = async () => {
      const label = await browser.execute(() => document.querySelector(".change-count")?.textContent?.trim() ?? "");
      const match = /(\d+) \//.exec(label);
      return match ? Number(match[1]) : -1;
    };
    await browser.waitUntil(async () => await navCount() >= 1);
    const scrollTopBeforeJump = await browser.execute(() => document.querySelector(".patch-scroll")?.scrollTop ?? -1);
    await $('button[aria-label="Next change"]').click();
    // From the very top the first Next lands on region 1 a row away; the
    // second press must step on to region 2.
    await browser.pause(300);
    const scrollTopAfterJump = await browser.execute(() => document.querySelector(".patch-scroll")?.scrollTop ?? -1);
    assert.ok(scrollTopAfterJump > scrollTopBeforeJump, "Next change must scroll the viewport down");
    await $('button[aria-label="Next change"]').click();
    await browser.waitUntil(async () => await navCount() > 1, { timeoutMsg: "Next change did not reach region 2" });
    const afterFirstNext = await navCount();
    await $('button[aria-label="Next change"]').click();
    await browser.waitUntil(async () => await navCount() > afterFirstNext, { timeoutMsg: "second Next change did not advance" });
    await browser.pause(300);
    const scrollTopBeforePrev = await browser.execute(() => document.querySelector(".patch-scroll")?.scrollTop ?? -1);
    await $('button[aria-label="Previous change"]').click();
    await browser.pause(500);
    const scrollTopAfterPrev = await browser.execute(() => document.querySelector(".patch-scroll")?.scrollTop ?? -1);
    assert.ok(scrollTopAfterPrev < scrollTopBeforePrev, "Previous change must scroll the viewport up");
    await browser.waitUntil(async () => await navCount() === afterFirstNext, { timeoutMsg: "Previous change did not step back" });

    // Renderable image assets preview in File view; files past the render
    // cap decline with open-externally actions instead of a frozen pane.
    await browser.execute(() => {
      Array.from(document.querySelectorAll(".file-row")).find((row) => row.textContent?.includes("zz-tiny.png"))?.click();
    });
    await browser.waitUntil(async () => await browser.execute(() => document.querySelector(".patch-heading code")?.textContent) === "zz-tiny.png");
    await $('button[title="Full file view"]').click();
    await browser.waitUntil(async () => (await $$(".image-preview")).length === 1, { timeoutMsg: "image preview did not render" });
    await browser.execute(() => {
      Array.from(document.querySelectorAll(".file-row")).find((row) => row.textContent?.includes("zz-capped.txt"))?.click();
    });
    await browser.waitUntil(async () => await browser.execute(() => document.querySelector(".patch-heading code")?.textContent) === "zz-capped.txt");
    await $('button[title="Full file view"]').click();
    await browser.waitUntil(async () => await $("strong=File too large to render").isDisplayed(), { timeoutMsg: "cap notice did not render" });
    await expect($('button=Open in default app')).toBeDisplayed();
    await expect($('button=Reveal in file explorer')).toBeDisplayed();

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
    await $(".range-chip").click();
    await $('input[aria-controls="review-base-options"]').setValue("other");
    assert.equal(await $("span=other.txt").isExisting(), false);
    await $('//div[@id="review-base-options"]//button[normalize-space()="other"]').click();
    await expect($("span=other.txt")).toBeDisplayed();
    await expect($(".review-counts")).toHaveText("6 files, +6010 -1");
    await browser.keys("Escape");
    git(["config", "core.filemode", "true"], repository);
    chmodSync(path.join(worktree, ".gitignore"), 0o755);
    // Wait out each toggle's recompute: a click issued into the loading
    // reflow can land on the neighboring preset button.
    await $('button=Committed only').click();
    await expect($(".review-counts")).toHaveText("2 files, +6001 -0");
    await $('button=All changes').click();
    await expect($(".review-counts")).toHaveText("7 files, +6010 -1");
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
    await expect($(".review-counts")).toHaveText("2 files, +6001 -0");
    await $('button=All changes').click();
    await expect($("strong=Review unavailable")).toBeDisplayed();
    await expect($("span=This review cannot run because Git conversion filters apply to files in this review.")).toBeDisplayed();
    expect(existsSync(filterMarker)).toBe(false);
  });
});
