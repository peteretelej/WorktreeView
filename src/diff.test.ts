import { test } from "node:test";
import assert from "node:assert/strict";
import { changeRegions, deletionTicks, hunksWithExpandedGaps, pairHunkLines, parseHunkHeader, patchGaps, splitFileLines, type DiffLine, type HunkLike } from "./diff.ts";

function line(text: string, oldLine: number | null = null, newLine: number | null = null): DiffLine {
  return { text, oldLine, newLine };
}

test("pairing an empty hunk yields no rows", () => {
  assert.deepEqual(pairHunkLines([]), []);
});

test("context lines appear on both sides with their line numbers", () => {
  const rows = pairHunkLines([line(" unchanged", 3, 7)]);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].old, rows[0].new);
  assert.equal(rows[0].old?.oldLine, 3);
  assert.equal(rows[0].new?.newLine, 7);
});

test("deletion and addition runs pair row-wise", () => {
  const rows = pairHunkLines([line("-a", 1), line("-b", 2), line("+c", null, 1), line("+d", null, 2)]);
  assert.deepEqual(rows.map((row) => [row.old?.text ?? null, row.new?.text ?? null]), [
    ["-a", "+c"],
    ["-b", "+d"],
  ]);
});

test("a longer run leaves the shorter side blank", () => {
  const rows = pairHunkLines([line("-a", 1), line("-b", 2), line("-c", 3), line("+d", null, 1)]);
  assert.deepEqual(rows.map((row) => [row.old?.text ?? null, row.new?.text ?? null]), [
    ["-a", "+d"],
    ["-b", null],
    ["-c", null],
  ]);
  const reversed = pairHunkLines([line("+a", null, 1), line("+b", null, 2)]);
  assert.deepEqual(reversed.map((row) => [row.old?.text ?? null, row.new?.text ?? null]), [
    [null, "+a"],
    [null, "+b"],
  ]);
});

test("context separates blocks so unrelated runs do not pair", () => {
  const rows = pairHunkLines([line("-a", 1), line(" ctx", 2, 1), line("+b", null, 2)]);
  assert.deepEqual(rows.map((row) => [row.old?.text ?? null, row.new?.text ?? null]), [
    ["-a", null],
    [" ctx", " ctx"],
    [null, "+b"],
  ]);
});

test("metadata after deletions lands on the old side", () => {
  const noNewline = "\\ No newline at end of file";
  const rows = pairHunkLines([line("-a", 1), line(noNewline), line("+b", null, 1)]);
  assert.deepEqual(rows.map((row) => [row.old?.text ?? null, row.new?.text ?? null]), [
    ["-a", "+b"],
    [noNewline, null],
  ]);
});

test("metadata after additions lands on the new side", () => {
  const noNewline = "\\ No newline at end of file";
  const rows = pairHunkLines([line("+a", null, 1), line(noNewline)]);
  assert.deepEqual(rows.map((row) => [row.old?.text ?? null, row.new?.text ?? null]), [
    [null, "+a"],
    [null, noNewline],
  ]);
});

test("metadata after a context line appears on both sides", () => {
  const noNewline = "\\ No newline at end of file";
  const rows = pairHunkLines([line(" a", 1, 1), line(noNewline)]);
  assert.equal(rows.length, 2);
  assert.equal(rows[1].old?.text, noNewline);
  assert.equal(rows[1].new?.text, noNewline);
});

test("pairing keeps additions from pairing across separate change blocks", () => {
  const rows = pairHunkLines([line("-a", 1), line("+b", null, 1), line("-c", 2), line("+d", null, 2)]);
  assert.deepEqual(rows.map((row) => [row.old?.text ?? null, row.new?.text ?? null]), [
    ["-a", "+b"],
    ["-c", "+d"],
  ]);
});

test("parsing a hunk header defaults omitted counts to one", () => {
  assert.deepEqual(parseHunkHeader("@@ -5 +9,3 @@"), { oldStart: 5, oldCount: 1, newStart: 9, newCount: 3 });
  assert.deepEqual(parseHunkHeader("@@ -1,5 +0,0 @@"), { oldStart: 1, oldCount: 5, newStart: 0, newCount: 0 });
  assert.equal(parseHunkHeader("not a header"), null);
});

test("gaps cover the space before, between, and after hunks", () => {
  const headers = ["@@ -5,3 +3,3 @@", "@@ -15,3 +13,3 @@"];
  assert.deepEqual(patchGaps(headers, 20), [
    { id: "leading", hostHunk: 0, before: true, oldStart: 3, newStart: 1, lines: 2 },
    { id: "between:0", hostHunk: 0, before: false, oldStart: 8, newStart: 6, lines: 7 },
    { id: "trailing", hostHunk: 1, before: false, oldStart: 18, newStart: 16, lines: 5 },
  ]);
});

test("count-0 hunks resume one past the printed line number", () => {
  const deletion = ["@@ -10,5 +9,0 @@", "@@ -29,3 +24,3 @@"];
  assert.deepEqual(patchGaps(deletion, 30), [
    { id: "leading", hostHunk: 0, before: true, oldStart: 2, newStart: 1, lines: 8 },
    { id: "between:0", hostHunk: 0, before: false, oldStart: 15, newStart: 10, lines: 14 },
    { id: "trailing", hostHunk: 1, before: false, oldStart: 32, newStart: 27, lines: 4 },
  ]);
  const addition = ["@@ -5,0 +6,2 @@", "@@ -20,3 +22,3 @@"];
  assert.deepEqual(patchGaps(addition, 26), [
    { id: "leading", hostHunk: 0, before: true, oldStart: 1, newStart: 1, lines: 5 },
    { id: "between:0", hostHunk: 0, before: false, oldStart: 6, newStart: 8, lines: 14 },
    { id: "trailing", hostHunk: 1, before: false, oldStart: 23, newStart: 25, lines: 2 },
  ]);
  // A whole-file deletion has no new-side lines to expand.
  assert.deepEqual(patchGaps(["@@ -1,5 +0,0 @@"], 0), []);
});

test("gaps need no leading or between entry when hunks touch the edges", () => {
  const headers = ["@@ -1,3 +1,3 @@", "@@ -5,3 +5,3 @@"];
  assert.deepEqual(patchGaps(headers, 7), [
    { id: "between:0", hostHunk: 0, before: false, oldStart: 4, newStart: 4, lines: 1 },
  ]);
});

test("the trailing gap waits for the file length and malformed headers disable gaps", () => {
  const headers = ["@@ -1,3 +1,3 @@"];
  assert.deepEqual(patchGaps(headers, null), []);
  assert.deepEqual(patchGaps(headers, 3), []);
  assert.deepEqual(patchGaps(headers, 10), [
    { id: "trailing", hostHunk: 0, before: false, oldStart: 4, newStart: 4, lines: 7 },
  ]);
  assert.deepEqual(patchGaps(["@@ broken @@", "@@ -5,3 +5,3 @@"], 10), []);
});

test("expanding splices fetched gap lines into their host hunks as context", () => {
  const hunks: HunkLike[] = [
    { header: "@@ -5,3 +3,3 @@", lines: [line("-old", 5), line("+new", null, 3)] },
    { header: "@@ -15,3 +13,3 @@", lines: [line(" ctx", 15, 13)] },
  ];
  const gaps = patchGaps(hunks.map((hunk) => hunk.header), 20);
  const content = Array.from({ length: 20 }, (_, index) => `file ${index + 1}`);
  const expanded = hunksWithExpandedGaps(hunks, gaps, new Set(["leading", "between:0"]), content);
  // The leading gap prepends new lines 1-2 (old 3-4) before hunk 0.
  assert.deepEqual(expanded[0].lines.slice(0, 2).map((entry) => [entry.text, entry.oldLine, entry.newLine]), [
    [" file 1", 3, 1],
    [" file 2", 4, 2],
  ]);
  // Expanded rows are marked so the comment layer can keep them unmarkable.
  assert.equal(expanded[0].lines[0].expanded, true);
  assert.equal(expanded[0].lines[3].expanded, undefined);
  assert.deepEqual(expanded[0].lines.slice(2, 4).map((entry) => entry.text), ["-old", "+new"]);
  // The between gap appends new lines 6-12 (old 8-14) after hunk 0, so
  // hunk 0 carries its own 2 lines plus both expanded runs.
  assert.equal(expanded[0].lines.length, 11);
  assert.deepEqual(expanded[1].lines.map((entry) => entry.text), [" ctx"]);
  // Unexpanded gaps and missing content leave the hunks untouched.
  assert.equal(hunksWithExpandedGaps(hunks, gaps, new Set(["trailing"]), content)[0].lines.length, 2);
  assert.equal(hunksWithExpandedGaps(hunks, gaps, new Set(["leading"]), null), hunks);
});

test("file lines drop the phantom trailing entry a final newline produces", () => {
  assert.deepEqual(splitFileLines("a\nb\n"), ["a", "b"]);
  assert.deepEqual(splitFileLines("a\nb"), ["a", "b"]);
  assert.deepEqual(splitFileLines("a\n\n"), ["a", ""]);
  assert.deepEqual(splitFileLines(""), []);
});

test("change regions merge touching hunks and mark addition presence", () => {
  const headers = ["@@ -1,3 +1,4 @@", "@@ -5,2 +5,2 @@", "@@ -20,1 +22,1 @@"];
  assert.deepEqual(changeRegions(headers), [
    { start: 1, end: 6, added: true },
    { start: 22, end: 22, added: true },
  ]);
});

test("a deletion-only hunk marks the surviving line before the change", () => {
  assert.deepEqual(changeRegions(["@@ -4,2 +3,0 @@"]), [{ start: 3, end: 3, added: false }]);
  assert.deepEqual(changeRegions(["@@ -1,3 +0,0 @@"]), [{ start: 1, end: 1, added: false }]);
});

test("change regions stay empty for malformed or absent headers", () => {
  assert.deepEqual(changeRegions([]), []);
  assert.deepEqual(changeRegions(["not a header"]), []);
});

test("deletion ticks anchor to the surviving line below each deleted run", () => {
  const hunks: HunkLike[] = [
    {
      header: "@@ -5,4 +6,4 @@",
      lines: [
        line(" a", 5, 6),
        line("-old1", 6),
        line("-old2", 7),
        line("+new", null, 7),
        line(" b", 8, 8),
      ],
    },
  ];
  assert.deepEqual(deletionTicks(hunks), [6]);
});

test("deletion runs at hunk start, end, and count-0 hunks all anchor", () => {
  const hunks: HunkLike[] = [
    { header: "@@ -3,2 +4,2 @@", lines: [line("-top", 3), line(" ctx", 4, 4)] },
    { header: "@@ -10,2 +12,1 @@", lines: [line(" ctx", 10, 12), line("-end", 11)] },
    { header: "@@ -20,3 +21,0 @@", lines: [line("-gone", 20), line("-gone2", 21), line("-gone3", 22)] },
  ];
  assert.deepEqual(deletionTicks(hunks), [3, 12, 21]);
});
