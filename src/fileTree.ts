import type { ChangedFile } from "./navigation.ts";

// One rendered row of the changed-files pane: a (possibly compressed)
// directory row or a file row at its tree depth. Everything here derives
// from the file paths alone; no Git data beyond the existing list.
export type FileTreeRow =
  | { kind: "dir"; path: string; label: string; depth: number; count: number }
  | { kind: "file"; file: ChangedFile; depth: number };

export function filterChangedFiles(files: ChangedFile[], query: string): ChangedFile[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return files;
  return files.filter((file) => file.path.toLowerCase().includes(needle));
}

// Splits "app/src/Main.kt" into "app/src/" and "Main.kt"; root files get
// an empty directory half.
export function splitFilePath(path: string): [dir: string, name: string] {
  const at = path.lastIndexOf("/");
  return at < 0 ? ["", path] : [path.slice(0, at + 1), path.slice(at + 1)];
}

type TreeDir = { dirs: Map<string, TreeDir>; files: ChangedFile[]; path: string };

function emptyDir(path: string): TreeDir {
  return { dirs: new Map(), files: [], path };
}

export function buildFileTree(files: ChangedFile[]): TreeDir {
  const root = emptyDir("");
  for (const file of files) {
    const segments = file.path.split("/");
    let dir = root;
    for (let at = 1; at < segments.length; at++) {
      const path = segments.slice(0, at).join("/");
      let child = dir.dirs.get(path);
      if (!child) {
        child = emptyDir(path);
        dir.dirs.set(path, child);
      }
      dir = child;
    }
    dir.files.push(file);
  }
  return root;
}

// Single-child directories holding no direct files collapse into one row,
// so a deep chain like src/main/java/ke/co costs one row, not five.
type CompressedDir = { path: string; label: string; dir: TreeDir };

function compressedChildren(dir: TreeDir): CompressedDir[] {
  const rows: CompressedDir[] = [];
  for (const path of [...dir.dirs.keys()].sort()) {
    let child = dir.dirs.get(path);
    if (!child) continue;
    const chain = [path.slice(path.lastIndexOf("/") + 1)];
    while (child.dirs.size === 1 && child.files.length === 0) {
      const onlyPath = [...child.dirs.keys()][0];
      const next = child.dirs.get(onlyPath);
      if (!next) break;
      chain.push(onlyPath.slice(onlyPath.lastIndexOf("/") + 1));
      child = next;
    }
    rows.push({ path: child.path, label: chain.join("/"), dir: child });
  }
  return rows;
}

// Directory totals arrive per level, and Git reports unbounded path depth,
// so both passes are iterative: recursion here would track path depth on
// the call stack and overflow on crafted repositories.
function countAllDirs(root: TreeDir): Map<string, number> {
  const order: TreeDir[] = [];
  const stack: TreeDir[] = [root];
  while (stack.length > 0) {
    const dir = stack.pop();
    if (!dir) break;
    order.push(dir);
    for (const child of dir.dirs.values()) stack.push(child);
  }
  const counts = new Map<string, number>();
  for (const dir of order.reverse()) {
    let total = dir.files.length;
    for (const child of dir.dirs.values()) total += counts.get(child.path) ?? 0;
    counts.set(dir.path, total);
  }
  return counts;
}

// open === null expands every directory; the pane passes null until the
// user collapses something and while a filter is active.
export function flattenFileTree(root: TreeDir, open: Set<string> | null): FileTreeRow[] {
  const counts = countAllDirs(root);
  const rows: FileTreeRow[] = [];
  type Frame = { dirs: CompressedDir[]; at: number; files: ChangedFile[]; depth: number };
  const start = (dir: TreeDir, depth: number): Frame => ({
    dirs: compressedChildren(dir),
    at: 0,
    files: [...dir.files].sort((left, right) => left.path.localeCompare(right.path)),
    depth,
  });
  const stack: Frame[] = [start(root, 0)];
  while (stack.length > 0) {
    const frame = stack[stack.length - 1];
    if (!frame) break;
    if (frame.at < frame.dirs.length) {
      const info = frame.dirs[frame.at];
      frame.at += 1;
      rows.push({ kind: "dir", path: info.path, label: info.label, depth: frame.depth, count: counts.get(info.path) ?? 0 });
      if (open === null || open.has(info.path)) stack.push(start(info.dir, frame.depth + 1));
    } else {
      for (const file of frame.files) rows.push({ kind: "file", file, depth: frame.depth });
      stack.pop();
    }
  }
  return rows;
}

export function allTreeDirPaths(root: TreeDir): string[] {
  const paths: string[] = [];
  function collect(dir: TreeDir) {
    paths.push(dir.path);
    for (const child of dir.dirs.values()) collect(child);
  }
  for (const child of root.dirs.values()) collect(child);
  return paths;
}
