// Build the Windows app and package it as an MSIX for Microsoft Store
// submission or local testing.
//
//   node scripts/package-store.mjs               # build + unsigned MSIX (Store signs it)
//   node scripts/package-store.mjs --skip-build  # stage an existing release build
//   node scripts/package-store.mjs --cert devcert.pfx  # signed for local install
//
// The MSIX version comes from package.json (kept in sync with the release
// tag by scripts/set-version.mjs); Store submissions must strictly
// increase, so cut them in tag order. The unsigned output is exactly what
// Partner Center expects: the Store signs the package.
import { copyFileSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const skipBuild = process.argv.includes("--skip-build");

let cert = null;
const certArg = process.argv.slice(2).find((value) => value === "--cert" || value.startsWith("--cert="));
if (certArg) {
  const value = certArg.startsWith("--cert=") ? certArg.slice("--cert=".length) : process.argv[process.argv.indexOf(certArg) + 1];
  if (!value || !value.trim() || value.startsWith("--")) {
    console.error("--cert requires a path to a .pfx file");
    process.exit(1);
  }
  cert = value.trim();
}

const { version } = JSON.parse(readFileSync(path.join(root, "package.json"), "utf8"));
if (!/^\d+\.\d+\.\d+$/.test(version)) {
  console.error(`package.json version "${version}" is not x.y.z; run scripts/set-version.mjs first`);
  process.exit(1);
}
const msixVersion = `${version}.0`;

// The manifest declares x64, so the build target is pinned to it: on any
// other host the build fails loudly instead of staging a mislabeled exe.
const target = "x86_64-pc-windows-msvc";
const exe = path.join(root, "src-tauri", "target", target, "release", "worktreeview.exe");
if (skipBuild && !exists(exe)) {
  console.error(`--skip-build but no release build at ${exe}`);
  process.exit(1);
}

function run(command, args, label) {
  console.log(`\n== ${label}`);
  const result = spawnSync(command, args, { stdio: "inherit", cwd: root });
  if (result.error) {
    console.error(`${label} could not start: ${result.error.message}`);
    process.exit(1);
  }
  if (result.status !== 0) {
    console.error(`${label} failed with exit code ${result.status}`);
    process.exit(result.status ?? 1);
  }
}

if (!skipBuild) {
  // npm ships as a .cmd shim on Windows, which Node refuses to spawn
  // without a shell; drive npm's CLI through this Node instead so args
  // stay one argv array.
  const npmCli = path.join(path.dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js");
  if (!exists(npmCli)) {
    console.error(`npm CLI not found at ${npmCli}`);
    process.exit(1);
  }
  rmSync(exe, { force: true });
  run(process.execPath, [npmCli, "run", "tauri", "--", "build", "--no-bundle", "--target", target], "building the app (tauri build --no-bundle)");
}
if (!exists(exe)) {
  console.error(`build did not produce ${exe}`);
  process.exit(1);
}

const stage = path.join(root, "src-tauri", "target", "store-msix");
rmSync(stage, { recursive: true, force: true });
mkdirSync(path.join(stage, "assets"), { recursive: true });

const manifestSource = readFileSync(path.join(root, "packaging", "msix", "Package.appxmanifest"), "utf8");
if (!manifestSource.includes('Version="0.0.0.0"')) {
  console.error("packaging/msix/Package.appxmanifest must carry the Version=\"0.0.0.0\" placeholder");
  process.exit(1);
}
writeFileSync(path.join(stage, "Package.appxmanifest"), manifestSource.replace('Version="0.0.0.0"', `Version="${msixVersion}"`));
copyFileSync(exe, path.join(stage, "worktreeview.exe"));
for (const logo of ["StoreLogo.png", "Square44x44Logo.png", "Square71x71Logo.png", "Square150x150Logo.png", "Square310x310Logo.png"]) {
  copyFileSync(path.join(root, "src-tauri", "icons", logo), path.join(stage, "assets", logo));
}

const output = path.join(stage, `WorktreeView_${msixVersion}_x64.msix`);
const packageArgs = ["package", stage, "--manifest", path.join(stage, "Package.appxmanifest"), "--output", output];
if (cert) packageArgs.push("--cert", cert, "--install-cert");
run("winapp", packageArgs, cert ? "packaging a dev-signed MSIX for local install" : "packaging an unsigned MSIX for Store submission");

console.log(`\nMSIX ready: ${output}`);
if (!cert) {
  console.log("Unsigned for Store submission: upload it under Submit your product in Partner Center.");
  console.log("For a local install first sign it: node scripts/package-store.mjs --skip-build --cert devcert.pfx");
}

function exists(candidate) {
  try {
    return readFileSync(candidate).length > 0;
  } catch {
    return false;
  }
}
