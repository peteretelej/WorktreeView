// `tauri dev` needs vite's port and build.devUrl to agree, but the fixed
// port collides whenever another dev instance (or another worktree's
// session) is already running. This launcher picks the first free port and
// pins both sides to it via a config overlay, so `npm run dev` always
// starts. Pin a port yourself with WORKTREEVIEW_DEV_PORT.
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const cliPath = path.join(root, "node_modules", "@tauri-apps", "cli", "tauri.js");

function canBind(host, port) {
  return new Promise((resolve) => {
    const server = net.createServer();
    server.once("error", (error) => resolve(error.code === "EADDRNOTAVAIL"));
    server.once("listening", () => server.close(() => resolve(true)));
    server.listen(port, host);
  });
}

// Both loopback stacks must take the port: vite binds `localhost`, which
// resolves to ::1 first on Windows, so a port held on ::1 alone is busy
// even though 127.0.0.1 accepts a probe. EADDRNOTAVAIL on ::1 means this
// host has no IPv6 loopback at all, which counts as free.
async function isFree(port) {
  if (!(await canBind("127.0.0.1", port))) return false;
  return canBind("::1", port);
}

const basePort = Number(process.env.WORKTREEVIEW_DEV_PORT) || 1420;
// The HMR socket rides the next port when TAURI_DEV_HOST is set, so the
// pair must be free together.
const needsPair = Boolean(process.env.TAURI_DEV_HOST);
let port = basePort;
while (true) {
  const first = await isFree(port);
  const second = !needsPair || (first && (await isFree(port + 1)));
  if (first && second) break;
  port += 1;
}

// A private per-run directory keeps the overlay from following a planted
// symlink or colliding with a squatter file in a shared temp location.
const overlayDir = mkdtempSync(path.join(os.tmpdir(), "worktreeview-dev-"));
const overlayPath = path.join(overlayDir, "dev-url-overlay.json");
// A distinct dev identifier keeps the dev build out of the installed app's
// single-instance registration (Windows keys that mutex on the identifier),
// and the per-checkout hash keeps two parallel dev checkouts registered
// separately while a same-checkout relaunch still focuses the running app.
const checkoutKey = createHash("sha256").update(root.toLowerCase()).digest("hex").slice(0, 8);
writeFileSync(overlayPath, JSON.stringify({
  build: { devUrl: `http://localhost:${port}` },
  identifier: `com.etelej.worktreeview.dev-${checkoutKey}`,
}));

if (port !== basePort) {
  console.log(`Port ${basePort} is busy; starting the dev server on ${port}.`);
}

// Tray residency keeps a dev app alive after its window closes, so a bare
// restart would hand the launch to the stale binary instead of the new
// build. Stop only this checkout's app, matched by its target-directory
// path, so the installed app and other worktrees are never touched.
function stopStaleApp() {
  if (process.platform === "win32") {
    const script = `Get-Process worktreeview -ErrorAction SilentlyContinue | Where-Object { $_.Path -like '${root}\\*' } | ForEach-Object { $_.Id; Stop-Process -Force -Id $_.Id }`;
    const result = spawnSync("powershell", ["-NoProfile", "-NonInteractive", "-Command", script], { encoding: "utf8" });
    const pids = (result.stdout ?? "").split(/\s+/).filter(Boolean);
    if (pids.length > 0) {
      console.log(`Stopped this checkout's running dev app (pid ${pids.join(", ")}) so the new build takes over.`);
    }
    return;
  }
  const listed = spawnSync("pgrep", ["-f", `${root}/src-tauri/target/`], { encoding: "utf8" }).stdout ?? "";
  if (listed.split(/\s+/).filter(Boolean).length > 0) {
    spawnSync("pkill", ["-f", `${root}/src-tauri/target/`]);
    console.log("Stopped this checkout's running dev app so the new build takes over.");
  }
}

stopStaleApp();

const child = spawn(
  process.execPath,
  [cliPath, "dev", "--config", overlayPath],
  { stdio: "inherit", env: { ...process.env, WORKTREEVIEW_DEV_PORT: String(port) }, cwd: root },
);

function cleanup() {
  try { rmSync(overlayDir, { recursive: true, force: true }); } catch { /* best effort */ }
}

child.on("exit", (code) => {
  cleanup();
  process.exitCode = code ?? 0;
});
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    child.kill(signal);
    cleanup();
  });
}
