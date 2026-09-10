import { test } from "node:test";
import assert from "node:assert/strict";
import { buildFileRows, buildPatchRows, imageMimeForPath, SLIM_GAP_MAX_LINES } from "./stream.ts";
import { hunksWithExpandedGaps, type DiffLine, type HunkLike, type PatchGap } from "./diff.ts";

function line(text: string, oldLine: number | null = null, newLine: number | null = null): DiffLine {
  return { text, oldLine, newLine };
}

function hunk(header: string, lines: DiffLine[]): HunkLike {
  return { header, lines };
}

function gap(id: PatchGap["id"], hostHunk: number, before: boolean, newStart: number, lines: number): PatchGap {
  return { id, hostHunk, before, oldStart: newStart, newStart, lines };
}

test("patch rows stream headers, lines, and unexpanded gaps in order", () => {
  const hunks = [hunk("@@ -1,2 +1,2 @@", [line(" a", 1, 1), line("+b", null, 2)]), hunk("@@ -10,1 +11,1 @@", [line(" c", 10, 11)])];
  const gaps = [gap("leading", 0, true, 1, 2), gap("between:0", 0, false, 3, 5)];
  const rows = buildPatchRows(hunksWithExpandedGaps(hunks, gaps, new Set(), null), gaps, new Set(), null, false);
  assert.deepEqual(rows.map((row) => row.kind), ["gap", "header", "line", "line", "gap", "header", "line"]);
  assert.deepEqual(rows.filter((row) => row.kind === "gap").map((row) => (row.kind === "gap" ? row.gap.id : "")), ["leading", "between:0"]);
});

test("expanded gaps splice into their host hunk and the seam header joins", () => {
  const hunks = [hunk("@@ -3,1 +3,1 @@", [line(" a", 3, 3)])];
  const leading = gap("leading", 0, true, 1, SLIM_GAP_MAX_LINES);
  const expandedSet = new Set(["leading"]);
  const expanded = buildPatchRows(hunksWithExpandedGaps(hunks, [leading], expandedSet, ["x", "y", "z", "a"]), [leading], expandedSet, ["x", "y", "z", "a"], false);
  // The gap row and the hunk header both vanish: the expanded context joins
  // the hunk seamlessly from the first line.
  assert.deepEqual(expanded.map((row) => row.kind), ["line", "line", "line", "line"]);
  assert.equal(expanded[0].kind === "line" && expanded[0].line.text, " x");
  assert.equal(expanded.some((row) => row.kind === "gap"), false);
});

test("small gaps go slim and large gaps keep the full control", () => {
  const hunks = [hunk("@@ -4,1 +4,1 @@", [line(" a", 4, 4)])];
  const gaps = [gap("leading", 0, true, 4, SLIM_GAP_MAX_LINES), gap("trailing", 0, false, 5, SLIM_GAP_MAX_LINES + 1)];
  const rows = buildPatchRows(hunksWithExpandedGaps(hunks, gaps, new Set(), null), gaps, new Set(), null, false);
  assert.deepEqual(rows.map((row) => row.kind), ["gap", "header", "line", "gap"]);
  assert.deepEqual(rows.filter((row) => row.kind === "gap").map((row) => (row.kind === "gap" ? row.slim : null)), [true, false]);
});

test("a hunk header marks a seam: expanded joins suppress it", () => {
  const hunks = [hunk("@@ -1,1 +1,1 @@", [line(" a", 1, 1)]), hunk("@@ -5,1 +5,1 @@", [line(" b", 5, 5)])];
  const between = gap("between:0", 0, false, 2, 3);
  const lines = ["a", "x", "y", "z", "b"];
  const expandedSet = new Set(["between:0"]);
  const rows = buildPatchRows(hunksWithExpandedGaps(hunks, [between], expandedSet, lines), [between], expandedSet, lines, false);
  assert.deepEqual(rows.filter((row) => row.kind === "header").map((row) => (row.kind === "header" ? row.hunkIndex : null)), [0]);
  assert.deepEqual(rows.map((row) => row.kind), ["header", "line", "line", "line", "line", "line"]);
});

test("an unexpanded seam keeps its hunk header", () => {
  const hunks = [hunk("@@ -1,1 +1,1 @@", [line(" a", 1, 1)]), hunk("@@ -5,1 +5,1 @@", [line(" b", 5, 5)])];
  const between = gap("between:0", 0, false, 2, 3);
  const rows = buildPatchRows(hunksWithExpandedGaps(hunks, [between], new Set(), null), [between], new Set(), null, false);
  assert.deepEqual(rows.filter((row) => row.kind === "header").map((row) => (row.kind === "header" ? row.hunkIndex : null)), [0, 1]);
});

test("split mode pairs each hunk into side-by-side rows", () => {
  const hunks = [hunk("@@ -1,2 +1,2 @@", [line("-a", 1), line("+b", null, 1)])];
  const rows = buildPatchRows(hunksWithExpandedGaps(hunks, [], new Set(), null), [], new Set(), null, true);
  assert.deepEqual(rows.map((row) => row.kind), ["header", "split"]);
  const split = rows[1];
  assert.ok(split.kind === "split");
  assert.equal(split.old?.text, "-a");
  assert.equal(split.next?.text, "+b");
});

test("file rows carry the new-side numbering with a context prefix", () => {
  const rows = buildFileRows(["first", "second"]);
  assert.equal(rows.length, 2);
  const first = rows[0];
  assert.ok(first.kind === "line");
  assert.equal(first.line.text, " first");
  assert.equal(first.line.newLine, 1);
  assert.equal(rows[1].kind === "line" && rows[1].line.newLine, 2);
});

test("image mime detection covers renderable assets only", () => {
  assert.equal(imageMimeForPath("docs/logo.SVG"), "image/svg+xml");
  assert.equal(imageMimeForPath("a/b/shot.JPG"), "image/jpeg");
  assert.equal(imageMimeForPath("src/App.tsx"), null);
  assert.equal(imageMimeForPath("no-extension"), null);
});
