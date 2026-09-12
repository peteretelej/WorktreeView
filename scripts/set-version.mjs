// One command keeps every version declaration in sync: package.json,
// package-lock.json, src-tauri/tauri.conf.json, and src-tauri/Cargo.toml.
// The release workflow runs it with the pushed tag, and release prep runs
// it locally (`npm run set-version -- 1.2.3`) so development builds report
// the version that is about to ship. Cargo.lock is not edited here; cargo
// rewrites the workspace entry on the next build.
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const version = process.argv[2];
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version ?? "")) {
  console.error(`Usage: node scripts/set-version.mjs <x.y.z[-pre]>; got "${version ?? ""}"`);
  process.exit(1);
}

const readJson = (relative) => JSON.parse(readFileSync(path.join(root, relative), "utf8"));
const writeJson = (relative, value) => writeFileSync(path.join(root, relative), JSON.stringify(value, null, 2) + "\n");

const pkg = readJson("package.json");
pkg.version = version;
writeJson("package.json", pkg);

const lock = readJson("package-lock.json");
lock.version = version;
lock.packages[""].version = version;
writeJson("package-lock.json", lock);

const conf = readJson("src-tauri/tauri.conf.json");
conf.version = version;
writeJson("src-tauri/tauri.conf.json", conf);

const manifestPath = path.join(root, "src-tauri", "Cargo.toml");
const manifest = readFileSync(manifestPath, "utf8");
if (!/^version = "/m.test(manifest)) throw new Error("no package version line in src-tauri/Cargo.toml");
writeFileSync(manifestPath, manifest.replace(/^version = "[^"]*"/m, `version = "${version}"`));

console.log(`set version to ${version}`);
