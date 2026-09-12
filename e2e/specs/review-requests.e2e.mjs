import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import { mkdirSync, readFileSync, renameSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import path from "node:path";

const fixtureRoot = "/tmp/worktreeview-e2e-fixtures";
const gitHome = "/tmp/worktreeview-e2e-git-home";
const selector = "/tmp/worktreeview-e2e-selection";
const discoveryPath = path.join("/tmp/worktreeview-e2e-data", "config.json");
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

// One more local commit on the reviewed worktree: every round and campaign
// below needs a real, distinct head to record.
function commitHead(worktree, file, content, message) {
  writeFileSync(path.join(worktree, file), content);
  git(["add", file], worktree);
  git([
    "-c", "user.name=WorktreeView E2E",
    "-c", "user.email=e2e@example.invalid",
    "commit", "-q", "-m", message,
  ], worktree);
  return git(["rev-parse", "HEAD"], worktree);
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

// Text reads go through in-page scriptContent: this stack's Get Element
// Text command returns "" for spans inside strip cards and chips.
async function textOf(selector_) {
  return browser.execute((query) => document.querySelector(query)?.textContent?.trim() ?? null, selector_);
}

// The request row the human created from the header form, tracked by its
// requester badge; the chip text is the lifecycle as the header renders it.
async function humanRequestStatus() {
  return browser.execute(() => {
    const row = Array.from(document.querySelectorAll(".request-row"))
      .find((node) => Array.from(node.querySelectorAll(".comment-badge"))
        .some((badge) => badge.getAttribute("title") === "Requested by human"));
    return row?.querySelector(".status-chip")?.textContent?.trim() ?? null;
  });
}

async function openAttentionTab(label) {
  await $('button.nav-tab[aria-label^="Attention"]').click();
  await $(".attention-pane").waitForDisplayed();
  const tab = await $(`//button[contains(@class, "overview-tab")][contains(normalize-space(.), "${label}")]`);
  await tab.click();
  await browser.waitUntil(async () => (await tab.getAttribute("aria-selected")) === "true", {
    timeoutMsg: `attention tab did not activate: ${label}`,
  });
}

// One stateless MCP tools/call against the app's loopback face. Returns
// the parsed JSON payload of the tool result.
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

// The same call where the engine's refusal is the expected outcome: the
// face answers a complete, isError result carrying the message.
async function callRefusedTool(endpoint, token, name, args, messagePart) {
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
  assert.equal(payload.result?.resultType, "complete", `${name} did not complete`);
  assert.equal(payload.result.isError, true, `${name} should have been refused: ${payload.result?.content?.[0]?.text}`);
  const message = payload.result.content[0].text;
  assert.ok(message.includes(messagePart), `${name} refusal did not mention ${messagePart}: ${message}`);
  return message;
}

// A review submission over the raw JSON-RPC face; the stored id comes back
// synchronously and its findings advance every open request on the identity.
async function postSubmission(endpoint, token, identity, submission) {
  const response = await fetch(endpoint, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "post_review",
      params: { ...identity, ...submission },
    }),
    signal: AbortSignal.timeout(10_000),
  });
  assert.equal(response.status, 200, "post_review answered a non-200");
  const payload = await response.json();
  assert.ok(Number.isInteger(payload.result?.submission_id), `submission refused: ${JSON.stringify(payload).slice(0, 300)}`);
  return payload.result.submission_id;
}

async function listRequests(endpoint, token, filters = {}) {
  return callTool(endpoint, token, "list_review_requests", filters);
}

async function listStatus(endpoint, token, status) {
  const rows = await listRequests(endpoint, token, { status });
  return rows.map((row) => row.id);
}

describe("desktop review request lifecycle", () => {
  const shared = {};

  it("a coder agent requests review on a two-worktree fixture", async () => {
    const repository = path.join(fixtureRoot, "review-requests-repository");
    const worktree = path.join(fixtureRoot, "review-requests-feature");
    createRepository(repository);
    writeFileSync(path.join(repository, "reviewed.txt"), "line one\nline two\nline three\n");
    git(["add", "reviewed.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "base file"], repository);
    git(["checkout", "-q", "-b", "feature"], repository);
    writeFileSync(path.join(repository, "reviewed.txt"), "line one\nline two changed\nline three\nline four\n");
    git(["add", "reviewed.txt"], repository);
    git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", "feature change"], repository);
    const head = git(["rev-parse", "HEAD"], repository);
    // The root checkout returns to main so the feature branch can be checked
    // out in the linked worktree: the two-worktree fixture the fleet reviews.
    git(["checkout", "-q", "main"], repository);
    git(["worktree", "add", "-q", worktree, "feature"], repository);
    const baseSha = git(["merge-base", "feature", "main"], repository);
    select(repository);

    await openSelectedRepository(repository);

    // The app published its loopback endpoint and startup token.
    const discovery = JSON.parse(readFileSync(discoveryPath, "utf8"));
    assert.equal(typeof discovery.port, "number");
    const endpoint = `http://127.0.0.1:${discovery.port}/mcp`;
    shared.endpoint = endpoint;
    shared.rawEndpoint = `http://127.0.0.1:${discovery.port}/`;
    shared.token = discovery.token;
    shared.repository = repository;
    shared.worktree = worktree;
    shared.baseSha = baseSha;
    shared.identity = {
      repo_path: repository,
      base_sha: baseSha,
      target_key: worktree,
      target_kind: "worktree",
    };
    console.log(`review-requests-e2e: discovery read, port=${discovery.port}`);

    // Two named reviewer tokens, minted by the human in Settings the way
    // real fleet members get theirs.
    await $('button[aria-label="Open settings"]').click();
    await $("button=Agent API").click();
    async function mintToken(name) {
      await $('input[aria-label="Token name"]').setValue(name);
      await $("button=Create token").click();
      await browser.waitUntil(async () => {
        const text = await browser.execute(() => document.querySelector(".settings-reveal")?.textContent ?? "");
        return text.includes(name);
      }, { timeoutMsg: `token reveal did not appear: ${name}` });
      return browser.execute(() => document.querySelector(".settings-reveal code")?.textContent?.trim() ?? "");
    }
    shared.reviewerOne = await mintToken("reviewer-1");
    shared.reviewerTwo = await mintToken("reviewer-2");
    assert.match(shared.reviewerOne, /^wv[0-9a-f]{64}$/, "reviewer-1 secret shape");
    assert.match(shared.reviewerTwo, /^wv[0-9a-f]{64}$/, "reviewer-2 secret shape");
    await $(".back-button").click();
    console.log("review-requests-e2e: two reviewer tokens minted");

    // The coder opens the feature worktree review and requests it, naming
    // both reviewers and recording the head exactly as committed.
    const featureRow = await inventoryRow("feature");
    await expect(featureRow).toBeDisplayed();
    await featureRow.$(".branch-title").click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await $('aside[aria-label="Comments"]').waitForDisplayed();

    const created = await callTool(endpoint, shared.token, "request_review", {
      ...shared.identity,
      note: "Feature change ready for fleet review; correctness focus.",
      head_sha: head,
      lenses: ["correctness"],
      reviewers: ["reviewer-1", "reviewer-2"],
    });
    assert.ok(Number.isInteger(created.id), "the stored request carries an id");
    assert.equal(created.status, "requested");
    assert.equal(created.round, 0);
    assert.equal(created.max_rounds, 2, "default round budget");
    assert.equal(created.requester, "agent", "the coder token is the requester");
    assert.deepEqual(created.reviewers, ["reviewer-1", "reviewer-2"]);
    assert.equal(created.head_sha, head);
    shared.request = created.id;
    shared.head = head;

    // The row is visible on the cross-repo queue without any filters.
    const queued = await listRequests(endpoint, shared.token);
    const row = queued.find((entry) => entry.id === created.id);
    assert.ok(row, "the new request is on the open queue");
    assert.equal(row.status, "requested");
    assert.ok(Number.isInteger(row.age_ms), "the queue row carries its age");
    assert.deepEqual(row.unresolved_finding_counts, { P0: 0, P1: 0, P2: 0, P3: 0 });
    // The open review's header strip shows the same request live.
    await browser.waitUntil(async () => (await textOf(".request-row .status-chip")) === "requested", {
      timeoutMsg: "the header strip did not show the request",
    });
    console.log(`review-requests-e2e: request ${created.id} created and visible`);
  });

  it("two reviewer tokens claim and submit; the blocking verdict sticks", async () => {
    const { endpoint } = shared;

    // reviewer-1 claims the open pickup first; reviewer-2's claim then hits
    // the engine: a requested review has one claim in it.
    const claimed = await callTool(endpoint, shared.reviewerOne, "update_review_request", {
      id: shared.request,
      action: "claim",
    });
    assert.equal(claimed.status, "in_review");
    await callRefusedTool(endpoint, shared.reviewerTwo, "update_review_request", {
      id: shared.request,
      action: "claim",
    }, "Only a requested review can be claimed");

    // reviewer-1 submits blind with one blocking (P1) and one minor (P2)
    // finding; the blocking one flips the request.
    const blockingId = await postSubmission(shared.rawEndpoint, shared.reviewerOne, shared.identity, {
      agent_name: "reviewer-one",
      agent_model: "reviewer-model",
      command_context: "review-requests e2e",
      sections: [{ kind: "brief", title: "Summary", body: "Found one blocking issue and one nit." }],
      findings: [
        { title: "Blocking bug", body: "This change breaks the invariant.", file: "reviewed.txt", start: 2, end: 2, priority: "P1" },
        { title: "Style nit", body: "The naming could be clearer.", priority: "P2" },
      ],
    });
    assert.ok(Number.isInteger(blockingId));
    assert.deepEqual(await listStatus(endpoint, shared.token, "changes_requested"), [shared.request]);
    const blocked = (await listRequests(endpoint, shared.token)).find((row) => row.id === shared.request);
    assert.equal(blocked.round, 0, "a submission verdict does not spend a round");
    assert.deepEqual(blocked.unresolved_finding_counts.P1, 1);

    // reviewer-2's clean submission arrives after the blocking verdict:
    // sticky within the round, it must not flip the request to approved.
    await postSubmission(shared.rawEndpoint, shared.reviewerTwo, shared.identity, {
      agent_name: "reviewer-two",
      agent_model: "reviewer-model",
      command_context: "review-requests e2e",
      sections: [{ kind: "brief", title: "Summary", body: "Clean pass; concurring with the blocking finding." }],
      findings: [],
    });
    assert.deepEqual(await listStatus(endpoint, shared.token, "changes_requested"), [shared.request]);
    assert.deepEqual(await listStatus(endpoint, shared.token, "approved"), [], "the clean submission must not flip the sticky verdict");
    console.log("review-requests-e2e: blocking verdict stuck against a later clean submission");
  });

  it("the requester aggregates findings and re-requests with a new head", async () => {
    const { endpoint } = shared;

    // The verification pass: read the finding threads, accept with
    // evidence, rebut the nit with evidence, resolve what was addressed.
    const comments = await callTool(endpoint, shared.token, "list_comments", shared.identity);
    const blocking = comments.find((comment) => comment.severity === "P1" && comment.body.includes("breaks the invariant"));
    const nit = comments.find((comment) => comment.severity === "P2" && comment.body.includes("naming could be clearer"));
    assert.ok(blocking && nit, "both finding threads are on the identity");
    await callTool(endpoint, shared.token, "reply_comment", {
      parent_comment_id: blocking.id,
      body: "Accepted: the second write drops the invariant guard; fixed in the follow-up commit.",
    });
    await callTool(endpoint, shared.token, "reply_comment", {
      parent_comment_id: nit.id,
      body: "Rebutted: the name mirrors the upstream API field it wraps; renaming would break the mapping.",
    });
    // The addressed thread resolves; the rebutted nit stays visibly open.
    await callTool(endpoint, shared.token, "resolve_thread", { root_comment_id: blocking.id, resolved: true });

    // The consolidated response, posted as the requester's own submission.
    await postSubmission(shared.rawEndpoint, shared.token, shared.identity, {
      agent_name: "coder-agent",
      agent_model: "coder-model",
      command_context: "review-requests e2e",
      sections: [{
        kind: "brief",
        title: "Response to review",
        body: "Accepted the blocking finding and committed the guard fix; rebutted the naming nit with the upstream-mapping rationale.",
      }],
      findings: [],
    });
    console.log("review-requests-e2e: aggregation replies, resolve, and consolidated response posted");

    // Fixes are committed locally (fixtures never touch remotes) and the
    // requester re-requests with the new head: round increments.
    const fixedHead = commitHead(shared.worktree, "fix.txt", "guard restored\n", "fix: restore the invariant guard");
    const reRequested = await callTool(endpoint, shared.token, "update_review_request", {
      id: shared.request,
      action: "re_request",
      head_sha: fixedHead,
      note: "Invariant guard restored; please re-check the second write.",
    });
    assert.equal(reRequested.status, "in_review");
    assert.equal(reRequested.round, 1, "the re-request spends round 1");
    assert.equal(reRequested.head_sha, fixedHead);
    assert.equal(reRequested.note, "Invariant guard restored; please re-check the second write.");
    assert.deepEqual(await listStatus(endpoint, shared.token, "in_review"), [shared.request]);
    shared.fixedHead = fixedHead;
  });

  it("a reviewer approves the re-requested round within budget", async () => {
    const approved = await callTool(shared.endpoint, shared.reviewerTwo, "update_review_request", {
      id: shared.request,
      action: "approve",
    });
    assert.equal(approved.status, "approved");
    assert.equal(approved.round, 1);
    assert.deepEqual(await listStatus(shared.endpoint, shared.token, "approved"), [shared.request]);
    console.log(`review-requests-e2e: request ${shared.request} approved in round 1`);
  });

  it("the attention feed drains once work settles", async () => {
    // No filters means the open queue: the approved request is excluded.
    assert.deepEqual(await listRequests(shared.endpoint, shared.token), []);
    // Excluded does not mean lost: the status filters still find it, and
    // nothing was ever withdrawn.
    assert.deepEqual(await listStatus(shared.endpoint, shared.token, "approved"), [shared.request]);
    assert.deepEqual(await listStatus(shared.endpoint, shared.token, "withdrawn"), []);
    console.log("review-requests-e2e: open feed drained, settled rows reachable by filter only");
  });

  it("an exhausted round budget surfaces needs human", async () => {
    const budgetHead = commitHead(shared.worktree, "budget.txt", "budget head\n", "notes: budget round head");
    const created = await callTool(shared.endpoint, shared.token, "request_review", {
      ...shared.identity,
      note: "One-round budget check.",
      head_sha: budgetHead,
      max_rounds: 1,
    });
    assert.equal(created.max_rounds, 1);
    shared.budgetRequest = created.id;
    const blocking = {
      agent_name: "reviewer-one",
      agent_model: "reviewer-model",
      command_context: "review-requests e2e",
      sections: [{ kind: "brief", title: "Summary", body: "Blocking: the budget head repeats the bug." }],
      findings: [
        { title: "Still broken", body: "The guard is missing again.", file: "budget.txt", start: 1, end: 1, priority: "P0" },
      ],
    };

    // Round 0: a blocking review sends the work back; the one re-request
    // the budget allows spends round 1.
    await callTool(shared.endpoint, shared.reviewerOne, "update_review_request", { id: created.id, action: "claim" });
    await postSubmission(shared.rawEndpoint, shared.reviewerOne, shared.identity, blocking);
    const roundOneHead = commitHead(shared.worktree, "budget.txt", "budget head, fixed\n", "notes: budget fix attempt");
    const roundOne = await callTool(shared.endpoint, shared.token, "update_review_request", {
      id: created.id,
      action: "re_request",
      head_sha: roundOneHead,
      note: "Guard restored for the budget head.",
    });
    assert.equal(roundOne.status, "in_review");
    assert.equal(roundOne.round, 1);

    // Round 1 blocks again: the budget is now spent (1 + 1 > 1), so the
    // next re-request is refused with the takeover error and the row stays
    // untouched at changes_requested.
    await postSubmission(shared.rawEndpoint, shared.reviewerOne, shared.identity, blocking);
    const nextHead = commitHead(shared.worktree, "budget.txt", "budget head, fixed again\n", "notes: budget second fix attempt");
    await callRefusedTool(shared.endpoint, shared.token, "update_review_request", {
      id: created.id,
      action: "re_request",
      head_sha: nextHead,
    }, "a human needs to take over");
    const rows = await listRequests(shared.endpoint, shared.token, { status: "changes_requested" });
    const row = rows.find((entry) => entry.id === created.id);
    assert.ok(row, "the request stays on the queue");
    assert.equal(row.round, 1);
    assert.equal(row.head_sha, roundOneHead, "the refused re-request left the head untouched");

    // The human's Attention view carries the same signal: the Needs human
    // category lists exactly this request with its alarm chip.
    await openAttentionTab("Needs human");
    await browser.waitUntil(async () => (await $$(".attention-row")).length === 1, {
      timeoutMsg: "the needs-human category did not list exactly one request",
    });
    await browser.waitUntil(async () => (await textOf(".attention-row .status-chip")) === "needs human", {
      timeoutMsg: "the needs-human chip did not render",
    });
    await $('button.nav-tab[aria-label="Projects"]').click();
    console.log(`review-requests-e2e: request ${created.id} needs human after budget refusal`);
  });

  it("two open requests on one identity run concurrently", async () => {
    const { endpoint } = shared;
    const firstHead = commitHead(shared.worktree, "campaign-one.txt", "first campaign\n", "notes: campaign one head");
    const secondHead = commitHead(shared.worktree, "campaign-two.txt", "second campaign\n", "notes: campaign two head");

    // Different requesters stack independent campaigns on one identity.
    const first = await callTool(endpoint, shared.token, "request_review", {
      ...shared.identity,
      note: "Campaign one.",
      head_sha: firstHead,
    });
    const second = await callTool(endpoint, shared.reviewerTwo, "request_review", {
      ...shared.identity,
      note: "Campaign two.",
      head_sha: secondHead,
    });
    assert.notEqual(first.id, second.id);
    assert.equal(first.requester, "agent");
    assert.equal(second.requester, "reviewer-2");
    assert.equal((await listRequests(endpoint, shared.token)).length, 3, "budget request plus two campaigns");

    // Transitions touch one request at a time.
    const claimed = await callTool(endpoint, shared.reviewerOne, "update_review_request", { id: first.id, action: "claim" });
    assert.equal(claimed.status, "in_review");
    let rows = await listRequests(endpoint, shared.token);
    assert.equal(rows.find((row) => row.id === second.id).status, "requested", "the sibling campaign did not move");

    const approved = await callTool(endpoint, shared.reviewerTwo, "update_review_request", { id: first.id, action: "approve" });
    assert.equal(approved.status, "approved");
    rows = await listRequests(endpoint, shared.token);
    assert.equal(rows.find((row) => row.id === second.id).status, "requested");

    // The second campaign's requester withdraws their own request.
    const withdrawn = await callTool(endpoint, shared.reviewerTwo, "update_review_request", { id: second.id, action: "withdraw" });
    assert.equal(withdrawn.status, "withdrawn");

    // The open feed keeps only the budget request: approved and withdrawn
    // are excluded, and each remains reachable through its status filter.
    const open = await listRequests(endpoint, shared.token);
    assert.deepEqual(open.map((row) => row.id), [shared.budgetRequest]);
    assert.deepEqual((await listStatus(endpoint, shared.token, "approved")).sort(), [shared.request, first.id].sort());
    assert.deepEqual(await listStatus(endpoint, shared.token, "withdrawn"), [second.id]);
    console.log("review-requests-e2e: concurrent campaigns transitioned independently");
  });

  it("a human initiates a request from the header and gives the verdict", async () => {
    const { endpoint } = shared;

    // The human reopens the feature review; the header strip carries the
    // requests the agents made (the withdrawn campaign is gone).
    await $('button.nav-tab[aria-label="Projects"]').click();
    const featureRow = await inventoryRow("feature");
    await expect(featureRow).toBeDisplayed();
    await featureRow.$(".branch-title").click();
    await expect($('section[aria-label="Code review"]')).toBeDisplayed();
    await $(".request-surface").waitForDisplayed();
    assert.equal(await humanRequestStatus(), null, "no human request exists yet");

    // The inline form records the displayed head, exactly as an agent does.
    await $(".request-bar-side button.request-action").click();
    await $(".request-form").waitForDisplayed();
    await $('textarea[aria-label="Request note"]').setValue("Human-requested second opinion on this worktree.");
    await $(".request-form-actions button.request-action").click();
    await browser.waitUntil(async () => !await $(".request-form").isExisting(), {
      timeoutMsg: "the form did not close after sending",
    });

    // The store holds the human request; the header shows it live.
    let humanId = null;
    await browser.waitUntil(async () => {
      const rows = await listRequests(endpoint, shared.token);
      const human = rows.find((row) => row.requester === "human");
      if (!human) return false;
      humanId = human.id;
      return human.status === "requested";
    }, { timeoutMsg: "the human request never reached the queue" });
    await browser.waitUntil(async () => (await humanRequestStatus()) === "requested", {
      timeoutMsg: "the header did not render the human request",
    });

    // A reviewer claims it; the badge follows the event without a reload.
    await callTool(endpoint, shared.reviewerOne, "update_review_request", { id: humanId, action: "claim" });
    await browser.waitUntil(async () => (await humanRequestStatus()) === "in review", {
      timeoutMsg: "the header badge did not follow the claim",
    });

    // The human gives the verdict from the header: changes requested. The
    // row handle is re-queried: the live update swapped the rendered rows.
    const humanRow = await $('//div[contains(@class, "request-row")][.//span[normalize-space(@title)="Requested by human"]]');
    await humanRow.$("button=Request changes").click();
    await browser.waitUntil(async () => (await humanRequestStatus()) === "changes requested", {
      timeoutMsg: "the header badge did not follow the human verdict",
    });
    assert.ok((await listStatus(endpoint, shared.token, "changes_requested")).includes(humanId));
    console.log(`review-requests-e2e: human request ${humanId} created from the header and sent back by the human`);
  });
});
