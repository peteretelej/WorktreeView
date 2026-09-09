import { test } from "node:test";
import assert from "node:assert/strict";
import { buildFileRows, buildPatchRows, computeWindow, createHeightModel, scrollTopForRow, SLIM_GAP_MAX_LINES } from "./stream.ts";
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

test("height model offsets ride the default height until measured", () => {
  const model = createHeightModel(4, 20);
  assert.equal(model.total(), 80);
  assert.equal(model.offset(2), 40);
  assert.equal(model.offset(4), 80);
  model.measure(0, 40);
  assert.equal(model.offset(1), 40);
  assert.equal(model.offset(2), 60);
  assert.equal(model.total(), 100);
});

test("height model maps an offset to the row containing it", () => {
  const model = createHeightModel(3, 10);
  model.measure(1, 25);
  assert.equal(model.indexAt(0), 0);
  assert.equal(model.indexAt(9), 0);
  assert.equal(model.indexAt(10), 1);
  assert.equal(model.indexAt(34), 1);
  assert.equal(model.indexAt(35), 2);
  assert.equal(model.indexAt(999), 2);
});

test("measuring near-identical heights is a no-op and reset drops overrides", () => {
  const model = createHeightModel(2, 20);
  model.measure(0, 24);
  assert.equal(model.offset(1), 24);
  model.measure(0, 24.2);
  assert.equal(model.offset(1), 24);
  model.reset();
  assert.equal(model.offset(1), 20);
  assert.equal(model.total(), 40);
});

test("computeWindow pads by overscan and clamps to the row count", () => {
  const model = createHeightModel(100, 10);
  assert.deepEqual(computeWindow(model, 0, 100, 20), { start: 0, end: 13 });
  assert.deepEqual(computeWindow(model, 500, 100, 20), { start: 48, end: 63 });
  assert.deepEqual(computeWindow(model, 950, 100, 20), { start: 93, end: 100 });
  assert.deepEqual(computeWindow(createHeightModel(0, 10), 0, 100, 20), { start: 0, end: 0 });
});

test("scrollTopForRow leads the target row and clamps to the top", () => {
  const model = createHeightModel(10, 10);
  assert.equal(scrollTopForRow(model, 5, 40), 10);
  assert.equal(scrollTopForRow(model, 0, 40), 0);
  assert.equal(scrollTopForRow(model, 9, 0), 90);
});

test("patch rows stream headers, lines, and unexpanded gaps in order", () => {
  const hunks = [hunk("@@ -1,2 +1,2 @@", [line(" a", 1, 1), line("+b", null, 2)]), hunk("@@ -10,1 +11,1 @@", [line(" c", 10, 11)])];
  const gaps = [gap("leading", 0, true, 1, 2), gap("between:0", 0, false, 3, 5)];
  const rows = buildPatchRows(hunksWithExpandedGaps(hunks, gaps, new Set(), null), gaps, new Set(), null, false);
  assert.deepEqual(rows.map((row) => row.kind), ["gap", "header", "line", "line", "gap", "header", "line"]);
  assert.deepEqual(rows.filter((row) => row.kind === "gap").map((row) => (row.kind === "gap" ? row.gap.id : "")), ["leading", "between:0"]);
});

test("expanded gaps splice into their host hunk instead of a gap row", () => {
  const hunks = [hunk("@@ -3,1 +3,1 @@", [line(" a", 3, 3)])];
  const leading = gap("leading", 0, true, 1, SLIM_GAP_MAX_LINES);
  const expandedSet = new Set(["leading"]);
  const expanded = buildPatchRows(hunksWithExpandedGaps(hunks, [leading], expandedSet, ["x", "y", "z", "a"]), [leading], expandedSet, ["x", "y", "z", "a"], false);
  assert.deepEqual(expanded.map((row) => row.kind), ["header", "line", "line", "line", "line"]);
  assert.equal(expanded[1].kind === "line" && expanded[1].line.text, " x");
  assert.equal(expanded.some((row) => row.kind === "gap"), false);
});

test("small gaps go slim and large gaps keep the full control", () => {
  const hunks = [hunk("@@ -4,1 +4,1 @@", [line(" a", 4, 4)])];
  const gaps = [gap("leading", 0, true, 4, SLIM_GAP_MAX_LINES), gap("trailing", 0, false, 5, SLIM_GAP_MAX_LINES + 1)];
  const rows = buildPatchRows(hunksWithExpandedGaps(hunks, gaps, new Set(), null), gaps, new Set(), null, false);
  assert.deepEqual(rows.map((row) => row.kind), ["gap", "header", "line", "gap"]);
  assert.deepEqual(rows.filter((row) => row.kind === "gap").map((row) => (row.kind === "gap" ? row.slim : null)), [true, false]);
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
