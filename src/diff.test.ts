import { test } from "node:test";
import assert from "node:assert/strict";
import { pairHunkLines, type DiffLine } from "./diff.ts";

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
