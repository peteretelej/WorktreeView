import { spawn, spawnSync } from "node:child_process";
import net from "node:net";
import assert from "node:assert/strict";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";

const fixtureRoot = "/tmp/worktreeview-e2e-fixtures";
const gitHome = "/tmp/worktreeview-e2e-git-home";
const serverBinary = "/app/worktreeview-server";
const serverHome = "/tmp/worktreeview-e2e-server-home";
const serverConfigPath = path.join(serverHome, "config.json");
const serverUrl = "http://127.0.0.1:9890";
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

// The server process is this spec's own resource: killed on worker exit
// and in the after hook, and its home starts fresh with the run.
let server = null;
let serverOutput = "";
function stopServer() {
  if (server?.pid && server.exitCode === null && server.signalCode === null) {
    server.kill("SIGKILL");
  }
  server = null;
}
process.once("exit", stopServer);

function waitReady(configPath, port, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      if (Date.now() >= deadline) {
        reject(new Error(`the server did not become ready; output tail: ${serverOutput.slice(-2000)}`));
        return;
      }
      let config = null;
      try {
        config = JSON.parse(readFileSync(configPath, "utf8"));
      } catch { /* the config lands after the bind */ }
      if (!config || config.port !== port || typeof config.token !== "string") {
        setTimeout(attempt, 250);
        return;
      }
      const socket = net.createConnection({ host: "127.0.0.1", port });
      socket.once("connect", () => {
        socket.destroy();
        resolve(config);
      });
      socket.once("error", () => {
        socket.destroy();
        setTimeout(attempt, 250);
      });
    };
    attempt();
  });
}

// One stateless MCP tools/call against the server's loopback face.
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

// One command-API call against the server with the admin's human token;
// refusals fail the spec instead of hiding a broken shared state.
async function callCommand(command, token, args) {
  const response = await fetch(`${serverUrl}/api/${command}`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify(args),
    signal: AbortSignal.timeout(10_000),
  });
  const body = await response.text();
  assert.equal(response.status, 200, `${command} answered HTTP ${response.status}: ${body}`);
  return JSON.parse(body);
}

describe("desktop app against a worktreeview-server", () => {
  const shared = {};

  after(() => {
    stopServer();
  });

  it("boots a headless server and connects the desktop app to it", async () => {
    // The reviewed repository with one feature worktree, built with local
    // Git only from the suite's fixture rules.
    const repository = path.join(fixtureRoot, "server-mode-repository");
    const worktree = path.join(fixtureRoot, "server-mode-feature");
    createRepository(repository);
    writeFileSync(path.join(repository, "reviewed.txt"), "line one\nline two\nline three\n");
    git(["add", "reviewed.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "base file"], repository);
    git(["checkout", "-q", "-b", "feature"], repository);
    writeFileSync(path.join(repository, "reviewed.txt"), "line one\nline two changed\nline three\nline four\n");
    git(["add", "reviewed.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "feature change"], repository);
    git(["checkout", "-q", "main"], repository);
    git(["worktree", "add", "-q", worktree, "feature"], repository);
    const baseSha = git(["merge-base", "feature", "main"], repository);
    shared.repository = repository;
    shared.worktree = worktree;
    shared.baseSha = baseSha;
    shared.identity = {
      repo_path: repository,
      base_sha: baseSha,
      target_key: worktree,
      target_kind: "worktree",
    };

    // The first admin is bootstrapped by the binary itself; the secret is
    // printed once and becomes the user token the desktop app connects with.
    rmSync(serverHome, { recursive: true, force: true });
    const bootstrapped = spawnSync(serverBinary, ["create-admin", "ops"], {
      encoding: "utf8",
      env: { ...process.env, WORKTREEVIEW_SERVER_HOME: serverHome },
    });
    assert.equal(bootstrapped.status, 0, `create-admin failed: ${bootstrapped.stderr}`);
    const secret = bootstrapped.stdout.match(/bearer token \(printed once\): (\S+)/)?.[1];
    assert.match(secret ?? "", /^wv[0-9a-f]{64}$/, "the admin secret shape");
    shared.adminToken = secret;

    server = spawn(serverBinary, ["serve"], {
      env: { ...process.env, WORKTREEVIEW_SERVER_HOME: serverHome },
      stdio: ["ignore", "pipe", "pipe"],
    });
    server.stdout.on("data", (chunk) => { serverOutput += chunk; });
    server.stderr.on("data", (chunk) => { serverOutput += chunk; });
    const config = await waitReady(serverConfigPath, 9890);
    shared.agentToken = config.token;
    console.log(`server-mode-e2e: server ready on port ${config.port}`);

    // The command API answers the admin token before any UI is driven.
    const repos = await callCommand("list_repos", shared.adminToken, {});
    assert.deepEqual(repos, [], "a fresh server store starts without repos");

    // Settings: connect with the server URL and the admin's user token.
    await $('button[aria-label="Open settings"]').click();
    await $("button=Servers").click();
    await $('input[aria-label="Server URL"]').setValue(serverUrl);
    await $('input[aria-label="User token"]').setValue(shared.adminToken);
    await $("button=Connect").click();
    await browser.waitUntil(async () => {
      const text = await browser.execute(() => document.querySelector("#settings-servers")?.textContent ?? "");
      return text.includes("127.0.0.1:9890");
    }, { timeoutMsg: "the connection row did not appear after saving" });

    // The sidebar's server group gains the project by its server-host path.
    await $('button.nav-tab[aria-label="Projects"]').click();
    await $('button.project-row[title="http://127.0.0.1:9890"]').click();
    await $('button[aria-label="Add project from this server"]').click();
    await $('input[aria-label="Project path on 127.0.0.1:9890"]').setValue(repository);
    await $(".sidebar-add-row button").click();
    await browser.waitUntil(async () => {
      const row = await $('//button[contains(@class, "project-row")][.//strong[normalize-space()="server-mode-repository"]]');
      return row.isExisting();
    }, { timeoutMsg: "the server project did not join the tree" });
    console.log("server-mode-e2e: connected and added the server project by path");
  });

  it("reviews a server-backed worktree and attributes the human comment", async () => {
    const featureRow = await $('//button[contains(@class, "project-row")][.//strong[normalize-space()="server-mode-repository"]]');
    await featureRow.click();
    const worktreeRow = await $('//div[contains(@class, "worktree-row")][.//strong[normalize-space()="feature"]]');
    await worktreeRow.waitForDisplayed();
    await worktreeRow.$(".branch-title").click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await $('aside[aria-label="Comments"]').waitForDisplayed();
    const fileRow = await $('//button[contains(@class, "file-row")][.//span[normalize-space()="reviewed.txt"]]');
    await fileRow.click();
    await expect($('code[title="reviewed.txt"]')).toBeDisplayed();

    // The human posts on an added line; the server attributes it to the
    // connected account, ops.
    const addedRow = await $('//div[contains(@class, "diff-line")][contains(@class, "addition")][contains(., "line two changed")]');
    await addedRow.$(".line-number").click();
    const chip = await $(".selection-comment-chip");
    await chip.waitForDisplayed();
    await chip.click();
    const composer = await $('textarea[aria-label="Add comment"]');
    await composer.waitForDisplayed();
    await composer.setValue("server-mode human comment");
    await $(".comment-composer-panel.inline .primary-button").click();
    await browser.waitUntil(async () => (await $$(".inline-comment .comment-card")).length >= 1, {
      timeoutMsg: "the server-backed comment did not render inline",
    });
    const author = await browser.execute(() => document.querySelector(".inline-comment .comment-card .comment-author")?.textContent?.trim() ?? "");
    assert.equal(author, "ops", "the human comment must carry the connected account's name");

    // The same conversation is on the server: one comment, attributed.
    const listed = await callCommand("list_comments", shared.adminToken, {
      repoPath: shared.identity.repo_path,
      baseSha: shared.identity.base_sha,
      targetKey: shared.identity.target_key,
      targetKind: shared.identity.target_kind,
    });
    assert.equal(listed.length, 1, "the server store holds the human comment");
    assert.equal(listed[0].author_name, "ops");
    assert.equal(listed[0].author_kind, "human");
    console.log("server-mode-e2e: human comment posted, attributed to ops, and read back from the server");
  });

  it("streams the agent's comments and submissions into the open review", async () => {
    // An agent posts through the server's loopback MCP face with the
    // token the server published for discovery clients.
    await callTool(`${serverUrl}/mcp`, shared.agentToken, "create_comment", {
      ...shared.identity,
      body: "server-mode agent finding",
    });

    // The open review updates live over the event stream; the session is
    // never reloaded between the post and the assertion.
    await browser.waitUntil(async () => {
      const text = await browser.execute(() => document.querySelector('aside[aria-label="Comments"]')?.textContent ?? "");
      return text.includes("server-mode agent finding");
    }, { timeoutMsg: "the agent comment did not arrive over the server event stream" });
    const threads = await browser.execute(() => Array.from(document.querySelectorAll('aside[aria-label="Comments"] .comment-thread .comment-author')).map((node) => node.textContent?.trim() ?? ""));
    assert.ok(threads.includes("agent"), "the agent comment carries the agent's name");

    // A full submission delivered on the raw face raises the arrival cue
    // through the same stream.
    const response = await fetch(`${serverUrl}/`, {
      method: "POST",
      headers: { authorization: `Bearer ${shared.agentToken}`, "content-type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "post_review",
        params: {
          ...shared.identity,
          agent_name: "server-agent",
          agent_model: "server-model",
          command_context: "server-mode e2e",
          sections: [{ kind: "brief", title: "Summary", body: "A review delivered through the headless server." }],
          findings: [],
        },
      }),
      signal: AbortSignal.timeout(10_000),
    });
    assert.equal(response.status, 200, "post_review answered a non-200");
    await browser.waitUntil(async () => await $(".arrival-cue").isExisting(), {
      timeoutMsg: "the submission did not raise the arrival cue over the event stream",
    });
    const cue = await browser.execute(() => document.querySelector(".arrival-open")?.textContent ?? "");
    assert.ok(cue.includes("server-agent"), "the cue names the delivering agent");
    console.log("server-mode-e2e: agent comment and submission arrived live with attribution");
  });
});
