import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import { mkdirSync, readFileSync, renameSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import path from "node:path";

const fixtureRoot = "/tmp/worktreeview-e2e-fixtures";
const gitHome = "/tmp/worktreeview-e2e-git-home";
const selector = "/tmp/worktreeview-e2e-selection";
const discoveryPath = path.join("/tmp/worktreeview-e2e-data", "com.etelej.worktreeview", "agent-endpoint.json");
const protocolVersion = "2026-07-28";

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

async function textOf(selector_) {
  return browser.execute((query) => document.querySelector(query)?.textContent?.trim() ?? null, selector_);
}

async function waitText(selector_, expected, label) {
  await browser.waitUntil(async () => (await textOf(selector_)) === expected, {
    timeoutMsg: `${label}: got ${JSON.stringify(await textOf(selector_))}`,
  });
}

// One stateless MCP tools/call against the app's loopback face, authorized
// by the discovery file's default token (the same auth every face
// and token shares). Returns the parsed JSON payload of the tool result.
async function callTool(endpoint, token, name, args) {
  const response = await fetch(endpoint, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "tools/call",
      params: {
        name,
        arguments: args,
        _meta: { "io.modelcontextprotocol/protocolVersion": protocolVersion },
      },
    }),
    signal: AbortSignal.timeout(10_000),
  });
  assert.equal(response.status, 200, `${name} answered HTTP ${response.status}`);
  const payload = await response.json();
  assert.equal(payload.result?.resultType, "complete", `${name} did not complete: ${JSON.stringify(payload).slice(0, 300)}`);
  assert.equal(payload.result.isError, false, `${name} failed: ${payload.result?.content?.[0]?.text}`);
  return JSON.parse(payload.result.content[0].text);
}

describe("desktop agent comment collaboration", () => {
  it("drives the MCP face on a loaded review and the stream follows without a reload", async () => {
    const repository = path.join(fixtureRoot, "agent-collab-repository");
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

    await openSelectedRepository(repository);

    // The app published its loopback endpoint and startup token.
    const discovery = JSON.parse(readFileSync(discoveryPath, "utf8"));
    assert.equal(typeof discovery.port, "number");
    assert.match(discovery.token, /^wv[0-9a-f]{64}$/);
    const endpoint = `http://127.0.0.1:${discovery.port}/mcp`;
    console.log(`agent-collab-e2e: discovery read, port=${discovery.port}`);

    // A missing token is refused before anything runs, on the MCP face too.
    const denied = await fetch(endpoint, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list", params: {} }),
      signal: AbortSignal.timeout(10_000),
    });
    assert.equal(denied.status, 401);
    assert.equal((await denied.json()).error.code, -32001);
    console.log("agent-collab-e2e: unauthorized probe returned 401");

    // Open the review the agent will collaborate on, and hold its identity:
    // the review keys on the resolved base SHA, the worktree path, and the
    // worktree kind, exactly what the tools must address.
    const featureRow = await inventoryRow("feature");
    await expect(featureRow).toBeDisplayed();
    await featureRow.$(".branch-title").click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await $('aside[aria-label="Comments"]').waitForDisplayed();
    const reviewKey = {
      repo_path: repository,
      base_sha: baseSha,
      target_key: repository,
      target_kind: "worktree",
    };

    // The agent creates a line comment through the MCP face; the open
    // review's stream picks it up without any reload.
    const root = await callTool(endpoint, discovery.token, "create_comment", {
      ...reviewKey,
      body: "agent collab root comment",
      severity: "P1",
      file_path: "reviewed.txt",
      side: "RIGHT",
      start_line: 2,
    });
    assert.ok(Number.isInteger(root.id), "the tool result carries the stored comment id");
    await browser.waitUntil(async () => (await $$(".comment-thread")).length >= 1, {
      timeoutMsg: "the agent comment did not surface in the stream",
    });
    // The refetch swaps comment state, so thread and card handles are
    // re-queried on every read instead of held across mutations.
    const thread = await $(".comment-thread");
    const rootCard = await thread.$(".comment-card");
    assert.ok((await rootCard.getText()).includes("agent collab root comment"), "root body missing from the stream");

    // Attribution rides the existing author rendering: the token's agent
    // name with the agent badge, and the author filter keeps agent threads.
    // The author span ellipsizes via overflow hidden, which WebKitGTK's
    // rendered-text extraction skips, so read the DOM text instead.
    assert.equal(await textOf(".comment-thread .comment-author"), "agent");
    await waitText(".comment-thread .comment-badge", "agent", "agent badge");
    const agentFilter = await $('//div[contains(@class, "comment-filter")]/button[normalize-space()="agent"]');
    await agentFilter.click();
    await browser.waitUntil(async () => (await $$(".comment-thread")).length >= 1, {
      timeoutMsg: "the agent thread vanished under the agent author filter",
    });

    // The reply lands in the same thread, again without a reload.
    const reply = await callTool(endpoint, discovery.token, "reply_comment", {
      parent_comment_id: root.id,
      body: "agent collab reply",
    });
    assert.ok(Number.isInteger(reply.id), "the tool result carries the reply id");
    await browser.waitUntil(async () => {
      const cards = await (await $(".comment-thread")).$$(".comment-card");
      if (cards.length < 2) return false;
      const text = await cards[1].getText();
      return text.includes("agent collab reply");
    }, { timeoutMsg: "the agent reply did not surface in the thread" });

    // Resolving the thread renders the resolved state live.
    await callTool(endpoint, discovery.token, "resolve_thread", {
      root_comment_id: root.id,
      resolved: true,
    });
    await waitText(".comment-thread .comment-resolved-badge", "resolved", "resolved badge");
    console.log("agent-collab-e2e: create, reply, and resolve all surfaced live");
  });
});
