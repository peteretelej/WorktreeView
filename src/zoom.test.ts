import { test } from "node:test";
import assert from "node:assert/strict";
import { DEFAULT_ZOOM, ZOOM_LEVELS, snapZoom, stepZoom, zoomShortcut } from "./zoom.ts";

test("snapZoom keeps supported levels", () => {
  for (const level of ZOOM_LEVELS) assert.equal(snapZoom(level), level);
});

test("snapZoom maps arbitrary values to the nearest level", () => {
  assert.equal(snapZoom(1.05), 1);
  assert.equal(snapZoom(1.06), 1.1);
  assert.equal(snapZoom(0.2), 0.67);
  assert.equal(snapZoom(9), 2);
});

test("snapZoom falls back to the default for non-finite values", () => {
  assert.equal(snapZoom(Number.NaN), DEFAULT_ZOOM);
  assert.equal(snapZoom(Number.POSITIVE_INFINITY), DEFAULT_ZOOM);
});

test("stepZoom moves through the levels and clamps at the ends", () => {
  assert.equal(stepZoom(1, 1), 1.1);
  assert.equal(stepZoom(1, -1), 0.9);
  assert.equal(stepZoom(2, 1), 2);
  assert.equal(stepZoom(0.67, -1), 0.67);
});

test("stepZoom recovers off-level values before stepping", () => {
  // 1.05 ties between 1 and 1.1 and snaps to the lower level.
  assert.equal(stepZoom(1.05, 1), 1.1);
  assert.equal(stepZoom(1.05, -1), 0.9);
});

test("zoomShortcut matches the standard keys and their variants", () => {
  assert.equal(zoomShortcut("="), "in");
  assert.equal(zoomShortcut("+"), "in");
  assert.equal(zoomShortcut("-"), "out");
  assert.equal(zoomShortcut("_"), "out");
  assert.equal(zoomShortcut("0"), "reset");
  assert.equal(zoomShortcut("k"), null);
  assert.equal(zoomShortcut(""), null);
});
