import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdirSync, readFileSync, renameSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import path from "node:path";

const fixtureRoot = "/tmp/worktreeview-e2e-fixtures";
const gitHome = "/tmp/worktreeview-e2e-git-home";
const selector = "/tmp/worktreeview-e2e-selection";
const discoveryPath = path.join("/tmp/worktreeview-e2e-data", "com.etelej.worktreeview", "agent-endpoint.json");

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

const INBOX_TERMINAL_STATES = ["No repositories", "No worktrees", "Worktrees unavailable", "Repositories could not be loaded"];

async function showWorktreeList() {
  await browser.waitUntil(async () => {
    if (await $("main .back-button").isExisting()
      && !await $('section[aria-label="Code review"]').isExisting()) return true;
    for (const heading of INBOX_TERMINAL_STATES) {
      if (await $(`strong=${heading}`).isExisting()) return true;
    }
    return false;
  }, { timeoutMsg: "repository surface did not settle" });
  if (await $("main .back-button").isExisting()) {
    await $("main .back-button").click();
    await $(".inbox-pane").waitForDisplayed();
  }
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

// Text reads go through in-page scriptContent: this stack's Get Element
// Text command returns "" for spans inside the reviews strip card.
async function textOf(selector_) {
  return browser.execute((query) => document.querySelector(query)?.textContent?.trim() ?? null, selector_);
}

async function waitText(selector_, expected, label) {
  await browser.waitUntil(async () => (await textOf(selector_)) === expected, {
    timeoutMsg: `${label}: got ${JSON.stringify(await textOf(selector_))}`,
  });
}

// Loopback sink for the html block's probe image: if the sandboxed frame's
// inherited CSP ever lets an outside image through, this server sees the
// request and the no-fetch assertion fails. Errors and timeouts reject so a
// broken probe fails the spec loudly instead of hanging it.
function startProbeServer() {
  const hits = [];
  return new Promise((resolve, reject) => {
    const server = createServer((request, response) => {
      hits.push(request.url ?? "");
      response.writeHead(404);
      response.end();
    });
    const timeout = setTimeout(() => {
      server.close();
      reject(new Error("probe server did not start listening within 5s"));
    }, 5000);
    server.once("close", () => clearTimeout(timeout));
    server.once("error", (error) => {
      clearTimeout(timeout);
      server.close();
      reject(new Error(`probe server failed to listen: ${error.message}`));
    });
    server.listen(0, "127.0.0.1", () => {
      clearTimeout(timeout);
      resolve({ server, hits, port: server.address().port });
    });
  });
}

describe("desktop agent submissions", () => {
  it("delivers a submission over loopback that renders, cues, and persists", async () => {
    const repository = path.join(fixtureRoot, "submissions-repository");
    createRepository(repository);
    writeFileSync(path.join(repository, "reviewed.txt"), "line one\nline two\nline three\n");
    git(["add", "reviewed.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "base file"], repository);
    git(["checkout", "-q", "-b", "feature"], repository);
    writeFileSync(path.join(repository, "reviewed.txt"), "line one\nline two changed\nline three\nline four\n");
    git(["add", "reviewed.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "feature change"], repository);
    const baseSha = git(["merge-base", "feature", "main"], repository);
    select(repository);

    // The repository is open (its worktrees hydrate), so the endpoint will
    // accept a delivery for it; the delivery itself happens from the history
    // surface and the arrival cue then opens the targeted review.
    await openSelectedRepository(repository);

    // The app published its loopback endpoint and per-boot token at startup.
    const discovery = JSON.parse(readFileSync(discoveryPath, "utf8"));
    assert.equal(typeof discovery.port, "number");
    assert.match(discovery.token, /^[0-9a-f]{64}$/);
    console.log(`submissions-e2e: discovery read, port=${discovery.port}`);
    const endpoint = `http://127.0.0.1:${discovery.port}/`;

    console.log("submissions-e2e: starting probe server for the html block");
    const probe = await startProbeServer();
    console.log(`submissions-e2e: probe listening on 127.0.0.1:${probe.port}`);
    const dataImage = Buffer.from(
      '<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="teal"/></svg>',
    ).toString("base64");
    const htmlBody = [
      "<p>static canvas</p>",
      '<img src="https://e2e.example.invalid/track.png" alt="external">',
      `<img src="http://127.0.0.1:${probe.port}/probe.png" alt="probe">`,
      `<img src="data:image/svg+xml;base64,${dataImage}" alt="inline">`,
    ].join("");
    const payload = {
      jsonrpc: "2.0",
      id: 1,
      method: "post_review",
      params: {
        repo_path: repository,
        base_sha: baseSha,
        target_key: repository,
        target_kind: "worktree",
        agent_name: "e2e-agent",
        agent_model: "e2e-model",
        command_context: "submissions e2e",
        sections: [
          { kind: "brief", title: "Summary", body: "Loopback delivery landed." },
          { kind: "html", title: "Canvas", body: htmlBody },
        ],
        findings: [
          { title: "Review note", body: "Review-level finding.", file: null, start: null, end: null, priority: "P1" },
          { title: "Line note", body: "Line finding on the changed line.", file: "reviewed.txt", start: 2, end: 2, priority: "P0" },
        ],
      },
    };

    // A stale or wrong token is refused before anything is stored.
    const denied = await fetch(endpoint, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(payload),
      signal: AbortSignal.timeout(10_000),
    });
    assert.equal(denied.status, 401);
    assert.equal((await denied.json()).error.code, -32001);
    console.log("submissions-e2e: unauthorized probe returned 401");

    const response = await fetch(endpoint, {
      method: "POST",
      headers: { authorization: `Bearer ${discovery.token}`, "content-type": "application/json" },
      body: JSON.stringify(payload),
      signal: AbortSignal.timeout(10_000),
    });
    console.log(`submissions-e2e: authorized POST returned ${response.status}`);
    assert.equal(response.status, 200);
    const accepted = await response.json();
    assert.equal(accepted.id, 1);
    assert.ok(Number.isInteger(accepted.result?.submission_id), "the response acknowledges the submission id");
    console.log(`submissions-e2e: accepted submission id ${accepted.result.submission_id}`);

    // The arrival cue names the agent.
    const cue = await $('[aria-label="Review arrivals"]');
    await cue.waitForDisplayed();
    await expect(cue).toHaveText(expect.stringContaining("e2e-agent"));

    // Activating the cue opens the targeted review; the reviews strip shows
    // the agent card with its section cards.
    await $(".arrival-open").click();
    await $('aside[aria-label="Comments"] section[aria-label="Agent reviews"]').waitForDisplayed();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await waitText(".submission-agent", "e2e-agent", "agent name");
    await $(".submission-toggle").click();
    await $(".submission-sections").waitForDisplayed();
    await waitText(".submission-agent", "e2e-agent", "agent name after expand");
    const sectionsText = await textOf(".submission-sections");
    assert.ok(sectionsText?.includes("Summary"), `brief title missing: ${sectionsText}`);
    assert.ok(sectionsText?.includes("Loopback delivery landed."), `brief body missing: ${sectionsText}`);

    // The html block renders badged, and its iframe carries the structural
    // empty sandbox; the inherited CSP denies the outside images.
    const htmlBlock = await $(".html-block");
    await htmlBlock.waitForDisplayed();
    await waitText(".html-block-badge", "client content", "html badge");
    assert.equal(await $(".html-block iframe").getAttribute("sandbox"), "");
    await browser.pause(2000);
    assert.deepEqual(probe.hits, [], "the sandboxed frame must not fetch the probe image");
    probe.server.close();

    // Activating the cue cleared it, and the targeted review stays open with
    // the delivered submission and its finding comments.
    await browser.waitUntil(async () => !await $('[aria-label="Review arrivals"]').isExisting(), {
      timeoutMsg: "the arrival cue did not clear on view",
    });
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await expect($('section[aria-label="Agent reviews"]')).toBeDisplayed();
    await waitText(".submission-agent", "e2e-agent", "agent name after view");
    const stream = await textOf('aside[aria-label="Comments"]');
    assert.ok(stream?.includes("Review-level finding."), `review-level finding missing: ${stream?.slice(0, 300)}`);
    assert.ok(stream?.includes("Line finding on the changed line."), `line finding missing: ${stream?.slice(0, 300)}`);

    // The submission and its finding comments survive an app restart.
    await browser.reloadSession();
    await openSelectedRepository(repository);
    await $('//button[contains(@class, "worktree-row")][.//strong[normalize-space()="feature"]]').click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await $('section[aria-label="Agent reviews"]').waitForDisplayed();
    await waitText(".submission-agent", "e2e-agent", "agent name after restart");
    const persistedStream = await textOf('aside[aria-label="Comments"]');
    assert.ok(persistedStream?.includes("Review-level finding."), `review-level finding lost: ${persistedStream?.slice(0, 300)}`);
    assert.ok(persistedStream?.includes("Line finding on the changed line."), `line finding lost: ${persistedStream?.slice(0, 300)}`);
    await expect($(".comment-severity.severity-P0")).toBeDisplayed();
  });
});
