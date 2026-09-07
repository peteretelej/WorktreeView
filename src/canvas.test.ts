import { test } from "node:test";
import assert from "node:assert/strict";
import { sectionView, type SubmissionSection } from "./canvas.ts";

function section(kind: string, title = "Title", body = "Body"): SubmissionSection {
  return { kind, title, body };
}

test("native section kinds map to their labels through markdown", () => {
  assert.deepEqual(sectionView(section("brief")), { mode: "markdown", label: "Brief", fallback: false });
  assert.deepEqual(sectionView(section("walkthrough")), { mode: "markdown", label: "Walkthrough", fallback: false });
  assert.deepEqual(sectionView(section("notes")), { mode: "markdown", label: "Notes", fallback: false });
});

test("unknown section kinds fall back to the raw kind label", () => {
  const view = sectionView(section("risk-map"));
  assert.deepEqual(view, { mode: "markdown", label: "risk-map", fallback: true });
  // The vocabulary match is exact: casing differences are unknown kinds.
  assert.deepEqual(sectionView(section("HTML")), { mode: "markdown", label: "HTML", fallback: true });
});

test("html sections map to the sandboxed block descriptor with its badge", () => {
  assert.deepEqual(sectionView(section("html")), { mode: "html", label: "html", badge: "client content" });
});
