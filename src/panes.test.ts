import assert from "node:assert/strict";
import { test } from "node:test";
import { MIN_PANE_WIDTH, PATCH_FLOOR_PX, capPaneWidths } from "./panes.ts";

test("unmeasured bodies pass stored widths through", () => {
  assert.deepEqual(capPaneWidths(0, 430, 1000, true, true), [430, 1000]);
});

test("widths that fit pass through untouched", () => {
  assert.deepEqual(capPaneWidths(1241, 300, 300, true, true), [300, 300]);
  assert.deepEqual(capPaneWidths(1241, 300, 500, true, true), [300, 500]);
});

test("side panes yield proportionally so the patch keeps its floor", () => {
  const [files, comments] = capPaneWidths(1241, 430, 1000, true, true);
  assert.equal(files + comments + 2 * 5 + PATCH_FLOOR_PX <= 1241, true);
  assert.deepEqual([files, comments], [256, 595]);
});

test("a single open pane is capped at its budget, a collapsed rail is reserved", () => {
  // Files open, comments collapsed: one handle track + one 44px rail.
  const [files] = capPaneWidths(1241, 2000, 300, true, false);
  assert.equal(files, 1241 - 44 - 5 - PATCH_FLOOR_PX);
  // Comments open, files collapsed.
  const [, comments] = capPaneWidths(1241, 2000, 2000, false, true);
  assert.equal(comments, 1241 - 44 - 5 - PATCH_FLOOR_PX);
});

test("caps never drop below the minimum pane width", () => {
  const [files, comments] = capPaneWidths(500, 2000, 2000, true, true);
  assert.equal(files, MIN_PANE_WIDTH);
  assert.equal(comments, MIN_PANE_WIDTH);
});
