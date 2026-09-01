import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import {
  appendFileSync,
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

async function openSelectedRepository() {
  await $('button[aria-label="Open repository"]').click();
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
    await openSelectedRepository();
    await $(".worktree-row").waitForDisplayed();
    await expect($("body")).toHaveText(expect.stringContaining(fixture));
    await expect($(`strong=${branch}`)).toBeDisplayed();
    await expect($("body")).toHaveText(expect.stringContaining(head));

    await browser.refresh();
    await $(".worktree-row").waitForDisplayed();
    await expect($("body")).toHaveText(expect.stringContaining(fixture));

    const firstSession = browser.sessionId;
    await browser.reloadSession();
    expect(browser.sessionId).not.toBe(firstSession);
    await $(".worktree-row").waitForDisplayed();
    await expect($("body")).toHaveText(expect.stringContaining(fixture));

    const detached = path.join(fixtureRoot, "detached-repository");
    createRepository(detached);
    git(["checkout", "-q", "--detach"], detached);
    select(detached);
    await openSelectedRepository();
    await browser.waitUntil(async () => (await projectPaths()).includes(detached));
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
    await openSelectedRepository();
    await browser.waitUntil(async () => (await projectPaths()).includes(removed));
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
    await openSelectedRepository();
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
    git(["add", "README.md"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "main file"], repository);
    git(["checkout", "-q", "-b", "feature"], repository);
    writeFileSync(path.join(repository, "feature.txt"), "feature committed\n");
    git(["add", "feature.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "feature"], repository);
    git(["checkout", "-q", "main"], repository);
    git(["checkout", "-q", "-b", "other"], repository);
    writeFileSync(path.join(repository, "other.txt"), "other committed\n");
    git(["add", "other.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "other"], repository);
    git(["checkout", "-q", "main"], repository);
    const worktree = path.join(fixtureRoot, "review-feature");
    git(["worktree", "add", "-q", worktree, "feature"], repository);
    writeFileSync(path.join(worktree, "README.md"), "main\nuncommitted\n");
    writeFileSync(path.join(worktree, "untracked.txt"), "untracked\n");
    writeFileSync(path.join(worktree, ".gitignore"), "ignored.txt\n");
    writeFileSync(path.join(worktree, "ignored.txt"), "must not appear\n");
    select(repository);
    await openSelectedRepository();
    await $(".worktree-row").waitForDisplayed();
    let row;
    for (const item of await $$(".worktree-row")) {
      if ((await item.getText()).includes("feature")) { row = item; break; }
    }
    assert.ok(row, "feature worktree row should be present");
    await row.click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await browser.waitUntil(async () => (await $(".file-index").getText()).includes("feature.txt"));
    const initialIndex = await $(".file-index").getText();
    assert.match(initialIndex, /README\.md/);
    assert.match(initialIndex, /untracked\.txt/);
    assert.doesNotMatch(initialIndex, /ignored\.txt/);
    await $('button=Committed only').click();
    await browser.waitUntil(async () => !(await $(".file-index").getText()).includes("untracked.txt"));
    assert.doesNotMatch(await $(".file-index").getText(), /README\.md/);
    await $('button=All changes').click();
    await browser.waitUntil(async () => (await $(".file-index").getText()).includes("untracked.txt"));
    let file;
    for (const item of await $$(".file-row")) {
      if ((await item.getText()).includes("README.md")) { file = item; break; }
    }
    assert.ok(file, "tracked modification should be selectable");
    await file.click();
    await browser.waitUntil(async () => (await $(".patch-pane").getText()).includes("uncommitted"));
    await $('select[aria-label="Review base"]').selectByAttribute("value", "other");
    await browser.waitUntil(async () => (await $(".file-index").getText()).includes("other.txt"));
    await expect($(".review-counts")).toHaveText(expect.stringContaining("files"));
  });
});
