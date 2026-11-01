import { test } from "node:test";
import assert from "node:assert/strict";
import { DETAILS_ROW_HEIGHT, ONE_LINE_ROW_HEIGHT, OVERSCAN_ROWS, rowHeightFor, scrollTopForRow, totalHeight, visibleRange } from "./fileWindow.ts";

test("visibleRange windows a mid-list scroll offset with overscan on both sides", () => {
  // Viewport of 10 rows, scrolled so row 50 sits exactly at the top.
  const range = visibleRange(500, 50 * ONE_LINE_ROW_HEIGHT, 10 * ONE_LINE_ROW_HEIGHT, ONE_LINE_ROW_HEIGHT);
  assert.deepEqual(range, { start: 50 - OVERSCAN_ROWS, end: 50 - OVERSCAN_ROWS + 10 + 2 * OVERSCAN_ROWS });
});

test("visibleRange at zero scroll starts at the top and still extends past the viewport", () => {
  const range = visibleRange(500, 0, 10 * ONE_LINE_ROW_HEIGHT, ONE_LINE_ROW_HEIGHT);
  assert.deepEqual(range, { start: 0, end: 10 + 2 * OVERSCAN_ROWS });
});

test("visibleRange scrolled to the bottom reaches the last row", () => {
  const viewport = 10 * ONE_LINE_ROW_HEIGHT;
  const range = visibleRange(500, totalHeight(500, ONE_LINE_ROW_HEIGHT) - viewport, viewport, ONE_LINE_ROW_HEIGHT);
  assert.equal(range.end, 500);
  assert.equal(range.start, 500 - 10 - OVERSCAN_ROWS);
});

test("visibleRange never extends start below 0 or end past totalRows", () => {
  assert.equal(visibleRange(500, 1, 10 * ONE_LINE_ROW_HEIGHT, ONE_LINE_ROW_HEIGHT).start, 0);
  const deep = visibleRange(30, 29 * ONE_LINE_ROW_HEIGHT, 10 * ONE_LINE_ROW_HEIGHT, ONE_LINE_ROW_HEIGHT);
  assert.equal(deep.start, 29 - OVERSCAN_ROWS);
  assert.equal(deep.end, 30);
});

test("visibleRange with zero total rows stays empty", () => {
  assert.deepEqual(visibleRange(0, 0, 270, ONE_LINE_ROW_HEIGHT), { start: 0, end: 0 });
});

test("rowHeightFor shares the one-line height for tree and list and pins details", () => {
  assert.equal(rowHeightFor("tree"), ONE_LINE_ROW_HEIGHT);
  assert.equal(rowHeightFor("list"), ONE_LINE_ROW_HEIGHT);
  assert.equal(rowHeightFor("details"), DETAILS_ROW_HEIGHT);
  assert.notEqual(ONE_LINE_ROW_HEIGHT, DETAILS_ROW_HEIGHT);
});

test("scrollTopForRow keeps a fully visible row untouched", () => {
  assert.equal(scrollTopForRow(10, 10 * ONE_LINE_ROW_HEIGHT, 10 * ONE_LINE_ROW_HEIGHT, ONE_LINE_ROW_HEIGHT), 10 * ONE_LINE_ROW_HEIGHT);
});

test("scrollTopForRow scrolls up to rows above the viewport", () => {
  assert.equal(scrollTopForRow(2, 10 * ONE_LINE_ROW_HEIGHT, 10 * ONE_LINE_ROW_HEIGHT, ONE_LINE_ROW_HEIGHT), 2 * ONE_LINE_ROW_HEIGHT);
});

test("scrollTopForRow scrolls down to rows below the viewport", () => {
  const viewport = 10 * ONE_LINE_ROW_HEIGHT;
  assert.equal(scrollTopForRow(20, 10 * ONE_LINE_ROW_HEIGHT, viewport, ONE_LINE_ROW_HEIGHT), 21 * ONE_LINE_ROW_HEIGHT - viewport);
});

test("scrollTopForRow clamps at 0", () => {
  assert.equal(scrollTopForRow(0, 10 * ONE_LINE_ROW_HEIGHT, 10 * ONE_LINE_ROW_HEIGHT, ONE_LINE_ROW_HEIGHT), 0);
});

test("totalHeight scales with rows and is 0 for none", () => {
  assert.equal(totalHeight(0, ONE_LINE_ROW_HEIGHT), 0);
  assert.equal(totalHeight(7, DETAILS_ROW_HEIGHT), 7 * DETAILS_ROW_HEIGHT);
});
