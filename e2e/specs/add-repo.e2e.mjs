import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";

const fixtureRoot = "/tmp/worktreeview-e2e-fixtures";
const gitHome = "/tmp/worktreeview-e2e-git-home";
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

// Unattended setup: a repository the UI has never opened is registered
// through the MCP face alone, and the running app's sidebar follows the
// announce without any UI action.
describe("desktop unattended repository setup", () => {
  it("adds a repository over the MCP face and the sidebar follows", async () => {
    const repository = path.join(fixtureRoot, "add-repo-repository");
    rmSync(repository, { force: true, recursive: true });
    mkdirSync(repository, { recursive: true });
    git(["init", "-q", "-b", "main"], repository);
    writeFileSync(path.join(repository, "README.md"), "# add-repo\n");
    git(["add", "README.md"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "readme"], repository);

    // The app publishes its loopback endpoint and per-boot token at
    // startup, before any repository is open.
    await browser.waitUntil(() => existsSync(discoveryPath), {
      timeout: 20_000,
      timeoutMsg: "the discovery file never appeared",
    });
    const discovery = JSON.parse(readFileSync(discoveryPath, "utf8"));
    const endpoint = `http://127.0.0.1:${discovery.port}/mcp`;

    // Wait until the webview is interactive before the add: the sidebar
    // follow relies on the app's event listener being mounted.
    await $('button[aria-label="Open repository"]').waitForDisplayed({ timeout: 20_000 });

    const added = await callTool(endpoint, discovery.token, "add_repo", { path: repository });
    assert.equal(added.path, repository);
    assert.equal(added.name, "add-repo-repository");

    const repos = await callTool(endpoint, discovery.token, "list_repos", {});
    assert.equal(repos.length, 1);
    assert.equal(repos[0].path, repository);

    // The open app's sidebar follows the announce without a reload.
    await browser.waitUntil(
      async () => (await browser.execute(() => document.querySelector("nav.project-list")?.textContent ?? ""))
        .includes("add-repo-repository"),
      { timeout: 15_000, timeoutMsg: "the added repository did not surface in the sidebar" },
    );
  });
});
