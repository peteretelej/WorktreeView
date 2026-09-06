import { spawn, spawnSync } from "node:child_process";
import {
  closeSync,
  ftruncateSync,
  mkdirSync,
  openSync,
  readSync,
  rmSync,
  statSync,
  writeFileSync,
  writeSync,
} from "node:fs";
import net from "node:net";

const backendLog = "/artifacts/backend.log";
const driverPid = "/tmp/worktreeview-tauri-driver.pid";
let driver = null;
let driverPgid = null;
let backendFd = null;
let backendMonitor = null;
let backendOverflow = false;
let stopPromise = null;

function signalDriver(pgid, signal) {
  if (!pgid) return;
  try {
    process.kill(-pgid, signal);
  } catch (error) {
    if (error.code !== "ESRCH") throw error;
  }
}

function captureScreenshot() {
  if (process.env.DISPLAY) {
    spawnSync("scrot", ["/artifacts/worktreeview.png"], {
      stdio: "inherit",
      timeout: 2_000,
      killSignal: "SIGKILL",
    });
  }
}

function waitForDriver(child, timeout) {
  if (!child?.pid || child.exitCode !== null || child.signalCode !== null) return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => {
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(done, timeout);
    child.once("close", done);
    child.once("error", done);
  });
}

function trimBackendLog() {
  const size = statSync(backendLog).size;
  if (size <= 1_048_576) return;
  const tail = Buffer.alloc(1_048_576);
  const input = openSync(backendLog, "r");
  readSync(input, tail, 0, tail.length, size - tail.length);
  closeSync(input);
  const output = openSync(backendLog, "r+");
  ftruncateSync(output, 0);
  writeSync(output, tail, 0, tail.length, 0);
  closeSync(output);
}

function enforceBackendLimit() {
  if (statSync(backendLog).size <= 1_048_576 || backendOverflow) return;
  backendOverflow = true;
  signalDriver(driverPgid, "SIGKILL");
  captureScreenshot();
  void stopDriver(true).finally(() => {
    trimBackendLog();
    process.stderr.write("backend.log exceeded 1 MiB\n");
    process.exit(1);
  });
}

function waitForPort() {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + 15_000;
    const attempt = () => {
      const socket = net.createConnection({ host: "127.0.0.1", port: 4444 });
      socket.once("connect", () => {
        socket.destroy();
        resolve();
      });
      socket.once("error", () => {
        socket.destroy();
        if (Date.now() >= deadline) reject(new Error("tauri-driver did not listen on port 4444"));
        else setTimeout(attempt, 100);
      });
    };
    attempt();
  });
}

function stopDriver(force = false) {
  if (stopPromise !== null) {
    if (force) signalDriver(driverPgid, "SIGKILL");
    return stopPromise;
  }
  stopPromise = (async () => {
    const child = driver;
    const pgid = driverPgid;
    driver = null;
    signalDriver(pgid, force ? "SIGKILL" : "SIGTERM");
    if (child?.pid && child.exitCode === null && child.signalCode === null) {
      await waitForDriver(child, force ? 1_000 : 5_000);
    }
    signalDriver(pgid, "SIGKILL");
    if (child?.pid && child.exitCode === null && child.signalCode === null) {
      await waitForDriver(child, 1_000);
    }
    rmSync(driverPid, { force: true });
    if (backendFd !== null) {
      closeSync(backendFd);
      backendFd = null;
    }
    if (backendMonitor !== null) clearInterval(backendMonitor);
    backendMonitor = null;
    driverPgid = null;
  })();
  return stopPromise;
}

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, () => {
    void stopDriver().finally(() => process.exit(128 + (signal === "SIGINT" ? 2 : 15)));
  });
}
process.once("exit", () => {
  signalDriver(driverPgid, "SIGKILL");
  if (backendFd !== null) closeSync(backendFd);
});

export const config = {
  runner: "local",
  hostname: "127.0.0.1",
  port: 4444,
  specs: ["./specs/*.e2e.mjs"],
  maxInstances: 1,
  capabilities: [{
    "tauri:options": {
      application: "/app/worktreeview",
    },
  }],
  framework: "mocha",
  reporters: [["spec", { addConsoleLogs: true }]],
  autoXvfb: false,
  connectionRetryTimeout: 30_000,
  connectionRetryCount: 0,
  waitforTimeout: 15_000,
  mochaOpts: {
    timeout: 180_000,
  },
  onPrepare() {
    rmSync("/tmp/worktreeview-e2e-data", { recursive: true, force: true });
    rmSync("/tmp/worktreeview-e2e-fixtures", { recursive: true, force: true });
    rmSync("/tmp/worktreeview-e2e-selection", { force: true });
    mkdirSync("/tmp/worktreeview-e2e-data", { recursive: true });
    mkdirSync("/tmp/worktreeview-e2e-fixtures", { recursive: true });
    mkdirSync("/tmp/worktreeview-e2e-git-home", { recursive: true });
  },
  async beforeSession() {
    try {
      backendOverflow = false;
      backendFd = openSync(backendLog, "a", 0o600);
      backendMonitor = setInterval(enforceBackendLimit, 25);
      driver = spawn("tauri-driver", ["--port", "4444"], {
        stdio: ["ignore", "ignore", backendFd],
        env: process.env,
        detached: true,
      });
      driverPgid = driver.pid;
      driver.unref();
      driver.once("error", (error) => {
        process.stderr.write(`tauri-driver failed to start: ${error.message}\n`);
      });
      writeFileSync(driverPid, `${driver.pid}\n`);
      await waitForPort();
    } catch (error) {
      await stopDriver(true);
      throw error;
    }
  },
  async afterTest(_test, _context, result) {
    if (!result.passed) captureScreenshot();
  },
  async afterSession() {
    await stopDriver();
  },
};
