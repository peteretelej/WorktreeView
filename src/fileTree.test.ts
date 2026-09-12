import { test } from "node:test";
import assert from "node:assert/strict";
import { allTreeDirPaths, buildFileTree, filterChangedFiles, flattenFileTree, splitFilePath } from "./fileTree.ts";
import type { ChangedFile } from "./navigation.ts";

function file(path: string): ChangedFile {
  return { path, status: "M", untracked: false };
}

const DEMO = [
  "app/src/main/MainActivity.kt",
  "app/src/main/ui/ViewModel.kt",
  "app/src/test/MainActivityTest.kt",
  "README.md",
].map(file);

test("filter matches case-insensitive substrings and ignores blank queries", () => {
  assert.deepEqual(filterChangedFiles(DEMO, "main").map((entry) => entry.path), [DEMO[0].path, DEMO[1].path, DEMO[2].path]);
  assert.deepEqual(filterChangedFiles(DEMO, "readme").map((entry) => entry.path), [DEMO[3].path]);
  assert.deepEqual(filterChangedFiles(DEMO, "  ").map((entry) => entry.path), DEMO.map((entry) => entry.path));
  assert.deepEqual(filterChangedFiles(DEMO, "zzz"), []);
});

test("splitFilePath separates the directory half from the filename", () => {
  assert.deepEqual(splitFilePath("app/src/Main.kt"), ["app/src/", "Main.kt"]);
  assert.deepEqual(splitFilePath("README.md"), ["", "README.md"]);
});

test("tree rows compress single-child chains and count nested files", () => {
  const rows = flattenFileTree(buildFileTree(DEMO), null);
  const dirs = rows.filter((row) => row.kind === "dir");
  assert.deepEqual(dirs.map((row) => (row.kind === "dir" ? row.label : "")), ["app/src", "main", "ui", "test"]);
  const compressed = dirs.find((row) => row.kind === "dir" && row.path === "app/src");
  assert.ok(compressed && compressed.kind === "dir");
  assert.equal(compressed.label, "app/src");
  assert.equal(compressed.count, 3);
  const files = rows.filter((row) => row.kind === "file");
  assert.deepEqual(files.map((row) => (row.kind === "file" ? row.file.path : "")), [
    "app/src/main/ui/ViewModel.kt",
    "app/src/main/MainActivity.kt",
    "app/src/test/MainActivityTest.kt",
    "README.md",
  ]);
});

test("collapsing a directory hides its descendants while files keep their depth", () => {
  const rows = flattenFileTree(buildFileTree(DEMO), new Set(["app/src", "app/src/test"]));
  assert.deepEqual(rows.map((row) => (row.kind === "dir" ? row.path : `file:${row.kind === "file" ? row.file.path : ""}`)), [
    "app/src",
    "app/src/main",
    "app/src/test",
    "file:app/src/test/MainActivityTest.kt",
    "file:README.md",
  ]);
  const main = rows.find((row) => row.kind === "dir" && row.path === "app/src/main");
  assert.ok(main && main.kind === "dir");
  assert.equal(main.count, 2);
});

test("allTreeDirPaths covers compressed children for expand-all", () => {
  const paths = allTreeDirPaths(buildFileTree(DEMO));
  assert.ok(paths.includes("app/src/main/ui"));
  assert.ok(paths.includes("app/src/main"));
  assert.equal(paths.includes("app/src/test/ui"), false);
});

test("very deep paths flatten without recursing on path depth", () => {
  const deep = Array.from({ length: 5000 }, (_, i) => `d${i}`).join("/") + "/leaf.txt";
  const rows = flattenFileTree(buildFileTree([file(deep)]), null);
  assert.equal(rows.length, 2);
  const dir = rows[0];
  assert.ok(dir.kind === "dir");
  assert.equal(dir.count, 1);
  assert.equal(dir.depth, 0);
});
