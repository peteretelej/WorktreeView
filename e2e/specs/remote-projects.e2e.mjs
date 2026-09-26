import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import { mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import path from "node:path";

const fixtureRoot = "/tmp/worktreeview-e2e-fixtures";
const fixture = path.join(fixtureRoot, "remote-projects");
const repository = path.join(fixture, "remote-repo");
const origin = path.join(fixture, "origin-repo");
const clientKey = path.join(fixture, "clientkey");
const hostKey = path.join(fixture, "hostkey");
const authorizedKeys = path.join(fixture, "authorized_keys");
const sshdConfigPath = path.join(fixture, "sshd_config");
const sshdLogPath = path.join(fixture, "sshd.log");
const sshdPidPath = path.join(fixture, "sshd.pid");
const sshDir = path.join("/tmp/worktreeview-e2e-home", ".ssh");
const gitHome = "/tmp/worktreeview-e2e-git-home";

let sshdPort = null;
let sshdRunning = false;

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

function run(command, args) {
  const result = spawnSync(command, args, { encoding: "utf8" });
  if (result.status !== 0) throw new Error(`${command} ${args.join(" ")} failed: ${result.stderr}`);
  return result.stdout.trim();
}

function commit(message, cwd) {
  git(["-c", "user.name=WorktreeView E2E", "-c", "user.email=e2e@example.invalid", "commit", "-q", "-m", message], cwd);
}

function probePort(port) {
  return new Promise((resolve) => {
    const socket = net.createConnection({ host: "127.0.0.1", port });
    const settled = (outcome) => {
      socket.destroy();
      resolve(outcome);
    };
    socket.once("connect", () => settled(true));
    socket.once("error", () => settled(false));
  });
}

async function waitForPort(port, expected, timeoutMs, timeoutMsg) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if ((await probePort(port)) === expected) return;
    if (Date.now() >= deadline) throw new Error(`${timeoutMsg}${sshdLogTail()}`);
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

function sshdLogTail() {
  try {
    return `\nsshd log tail:\n${readFileSync(sshdLogPath, "utf8").trimEnd().split("\n").slice(-15).join("\n")}`;
  } catch {
    return "";
  }
}

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const port = server.address().port;
      server.close(() => resolve(port));
    });
  });
}

// The container's runtime user is the host's numeric uid, resolved through
// the image's NSS synthesis module as the account "e2e" whose uid is the
// sshd's own, so the non-root sshd can start its sessions. The throwaway
// keypair is the only credential material in play.
function writeSshdConfig(port) {
  writeFileSync(
    sshdConfigPath,
    [
      `Port ${port}`,
      "ListenAddress 127.0.0.1",
      `HostKey ${hostKey}`,
      `PidFile ${sshdPidPath}`,
      "PasswordAuthentication no",
      "KbdInteractiveAuthentication no",
      "PubkeyAuthentication yes",
      `AuthorizedKeysFile ${authorizedKeys}`,
      // The fixture tree lives under the world-writable /tmp, which the
      // strict-modes ownership walk rejects for every path it checks.
      "StrictModes no",
      "UsePAM no",
      "UseDNS no",
      "",
    ].join("\n"),
    { mode: 0o600 },
  );
  // The privilege-separation directory must exist; a non-root runtime cannot
  // create the root-owned default, and an existing one satisfies sshd.
  try {
    mkdirSync("/run/sshd");
  } catch {
    // Already present, or not creatable: sshd reports either way.
  }
}

function startSshd(port) {
  writeSshdConfig(port);
  run("/usr/sbin/sshd", ["-f", sshdConfigPath, "-E", sshdLogPath]);
  sshdRunning = true;
}

async function stopSshd() {
  try {
    const pid = Number(readFileSync(sshdPidPath, "utf8").trim());
    if (pid > 1) process.kill(pid, "SIGTERM");
  } catch {
    sshdRunning = false;
    return;
  }
  // A non-root sshd dies without tearing down its established per-connection
  // sessions ("sshd: e2e [priv]" / "sshd: e2e@notty"), and the app's
  // ControlPersist multiplexed master rides one of them: while they live,
  // remote loads keep succeeding through the dead listener and the outage
  // never reaches the app. TERM every session by its process title; this
  // suite is the container's only sshd.
  for (const entry of readdirSync("/proc")) {
    if (!/^\d+$/.test(entry)) continue;
    let command;
    try {
      command = readFileSync(`/proc/${entry}/cmdline`, "utf8");
    } catch {
      continue;
    }
    if (command.startsWith("sshd:")) {
      try {
        process.kill(Number(entry), "SIGTERM");
      } catch {
        // Already gone.
      }
    }
  }
  sshdRunning = false;
  await waitForPort(sshdPort, false, 10_000, "sshd did not release its port");
}

// Build the client ssh profile the app's spawned ssh reads: the container
// HOME's .ssh with the fixture identity and the host key pre-accepted, so
// BatchMode never meets a prompt.
function writeClientProfile(port) {
  mkdirSync(sshDir, { recursive: true, mode: 0o700 });
  writeFileSync(path.join(sshDir, "known_hosts"), `[127.0.0.1]:${port} ${run("ssh-keygen", ["-y", "-f", hostKey])}\n`, {
    mode: 0o600,
  });
  writeFileSync(
    path.join(sshDir, "config"),
    [
      "Host 127.0.0.1",
      `    IdentityFile ${clientKey}`,
      "    IdentitiesOnly yes",
      `    UserKnownHostsFile ${path.join(sshDir, "known_hosts")}`,
      "    StrictHostKeyChecking yes",
      "",
    ].join("\n"),
    { mode: 0o600 },
  );
}

function buildFixtures() {
  mkdirSync(origin, { recursive: true });
  git(["init", "-q", "-b", "main"], origin);
  writeFileSync(path.join(origin, "origin.md"), "origin main\n");
  git(["add", "origin.md"], origin);
  commit("origin main", origin);

  rmSync(repository, { force: true, recursive: true });
  mkdirSync(repository, { recursive: true });
  git(["init", "-q", "-b", "main"], repository);
  writeFileSync(path.join(repository, "README.md"), "# remote-repo\n");
  git(["add", "README.md"], repository);
  commit("main readme", repository);
  git(["checkout", "-q", "-b", "feature"], repository);
  writeFileSync(path.join(repository, "feature.txt"), "feature committed\n");
  git(["add", "feature.txt"], repository);
  commit("feature", repository);
  // The app's fetch action is what turns this configured remote into
  // remote-tracking refs on the host.
  git(["remote", "add", "origin", origin], repository);
}

describe("desktop remote projects over loopback sshd", () => {
  after(async () => {
    if (sshdRunning) await stopSshd();
  });

  it("adds a remote project, reviews and fetches it, and rides out an outage", async () => {
    sshdPort = await freePort();
    rmSync(fixture, { force: true, recursive: true });
    mkdirSync(fixture, { recursive: true });
    run("ssh-keygen", ["-t", "ed25519", "-f", clientKey, "-N", "", "-C", "worktreeview-e2e-client"]);
    run("ssh-keygen", ["-t", "ed25519", "-f", hostKey, "-N", "", "-C", "worktreeview-e2e-host"]);
    writeFileSync(authorizedKeys, `${readFileSync(`${clientKey}.pub`, "utf8").trim()}\n`, { mode: 0o600 });
    writeClientProfile(sshdPort);
    buildFixtures();
    startSshd(sshdPort);
    await waitForPort(sshdPort, true, 15_000, "the fixture sshd never started listening");

    // The add form: typed remote identity in, backend validation and
    // normalization out. A non-default port keeps the ssh:// identity shape.
    const identity = `ssh://e2e@127.0.0.1:${sshdPort}${repository}`;
    await $('button[aria-label="Open repository"]').waitForDisplayed({ timeout: 20_000 });
    await $('button[aria-label="Add remote project"]').click();
    const form = $("form.remote-form");
    await form.waitForDisplayed({ timeoutMsg: "the add-remote form never opened" });
    await form.$('input[placeholder="host.example"]').setValue("127.0.0.1");
    await form.$('input[placeholder="optional"]').setValue("e2e");
    await form.$('input[placeholder="22"]').setValue(String(sshdPort));
    await form.$('input[placeholder="/srv/git/project"]').setValue(repository);
    await form.$('button[type="submit"]').click();
    await browser.waitUntil(
      async () => !(await $("form.remote-form").isExisting()),
      { timeoutMsg: "the add-remote form never closed" },
    );

    // The project row carries the normalized identity and the remote marker.
    const row = $(`nav.project-list button[title="${identity}"]`);
    await row.waitForDisplayed({ timeout: 30_000, timeoutMsg: `the project row for ${identity} never appeared` });
    await expect(row.$(".remote-badge")).toBeDisplayed();

    // The remote project overview lists the host worktree and its branch.
    await browser.waitUntil(
      async () => await $(".inbox-pane .worktree-row").isExisting(),
      { timeoutMsg: "the remote worktree list never rendered" },
    );
    await browser.waitUntil(
      async () =>
        (await browser.execute(
          (repoPath) => document.querySelector(`.worktree-row[title="${repoPath}"] .branch-title strong`)?.textContent ?? "",
          repository,
        )) === "feature",
      { timeoutMsg: "the remote worktree branch did not render" },
    );

    // The default review surface renders the committed change from the host.
    await $(`.worktree-row[title="${repository}"] .branch-title`).click();
    await $('section[aria-label="Code review"]').waitForDisplayed({ timeoutMsg: "the default review surface never opened" });
    await expect($('span=feature.txt')).toBeDisplayed();
    await browser.waitUntil(
      async () => (await browser.execute(() => document.querySelector(".review-counts")?.textContent ?? "")).includes("+1 -0"),
      { timeoutMsg: "the committed review counts never rendered" },
    );
    await $(".crumb-link").click();
    await browser.waitUntil(
      async () => await $(".inbox-pane .worktree-row").isExisting(),
      { timeoutMsg: "the project overview never returned" },
    );

    // Before the fetch the Remote tab is empty; the fetch action runs on the
    // host over ssh and the tab lists what it brought back.
    await $(`//button[contains(@class, "overview-tab")][contains(normalize-space(.), "Remote")]`).click();
    await browser.waitUntil(
      async () =>
        (await browser.execute(() => document.querySelector(".inbox-pane .filter-empty")?.textContent ?? "")).startsWith(
          "No remote branches",
        ),
      { timeoutMsg: "the Remote tab never showed its pre-fetch empty state" },
    );
    await $('button[aria-label="Fetch remote updates"]').click();
    await browser.waitUntil(
      async () => await $(".inbox-pane .worktree-row").isExisting(),
      { timeoutMsg: "the fetch never surfaced a remote branch row" },
    );
    const remoteRow = await browser.execute(() => document.querySelector(".inbox-pane .worktree-row")?.textContent ?? "");
    assert.ok(remoteRow.includes("origin"), `the remote branch row does not name origin: ${remoteRow}`);

    // Stopping sshd turns the next wrapped load into the offline state: the
    // project and its badges stay, no error surface replaces them.
    await stopSshd();
    await browser.refresh();
    await $(".remote-state-badge.offline").waitForDisplayed({
      timeout: 30_000,
      timeoutMsg: "the offline badge never appeared after sshd stopped",
    });
    await browser.waitUntil(
      async () => (await browser.execute(() => document.querySelector("h1")?.textContent ?? "")).includes("remote-repo"),
      { timeoutMsg: "the offline project heading was replaced" },
    );
    assert.equal(await $(".operation-error").isExisting(), false);
    await expect(row).toBeDisplayed();

    // sshd returning proves reconnection is stateless: the same identity
    // loads again and the health badges go quiet.
    startSshd(sshdPort);
    await waitForPort(sshdPort, true, 15_000, "the fixture sshd never restarted");
    await browser.refresh();
    await browser.waitUntil(
      async () => await $(".inbox-pane .worktree-row").isExisting(),
      { timeoutMsg: "the remote project never recovered after sshd returned" },
    );
    assert.equal(await $(".remote-state-badge").isExisting(), false);
  });
});
