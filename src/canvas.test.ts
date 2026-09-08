import { test } from "node:test";
import assert from "node:assert/strict";
import { formatSubmissionForCopy, sectionView, type Submission, type SubmissionSection } from "./canvas.ts";

function section(kind: string, title = "Title", body = "Body"): SubmissionSection {
  return { kind, title, body };
}

function submission(overrides: Partial<Submission> = {}): Submission {
  return {
    id: 1,
    review_id: 1,
    agent_name: "e2e-agent",
    agent_model: "model-1",
    command_context: null,
    sections: [section("brief", "", "All good.")],
    created_at: 0,
    ...overrides,
  };
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

test("submission copy keeps author context and section titles", () => {
  const text = formatSubmissionForCopy(submission({
    sections: [section("brief", "", "All good."), section("notes", "Risks", "Two things")],
    command_context: "review the diff",
  }));
  assert.ok(text.startsWith("Review submitted by **e2e-agent** (model-1) on 1970-01-01"));
  assert.ok(text.includes("Command context: review the diff"));
  assert.ok(text.includes("## Brief\n\nAll good."));
  assert.ok(text.includes("## Risks\n\nTwo things"));
});
