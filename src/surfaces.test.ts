import { test } from "node:test";
import assert from "node:assert/strict";
import { filterGoneSurfaces, splitPinned, surfaceRows, worktreeKey, type SurfaceRow } from "./surfaces.ts";
import type { GoneSurface } from "./navigation.ts";

function goneSurface(overrides: Partial<GoneSurface> & { identity_key: string }): GoneSurface {
  return { kind: "branch", label: "", detail: "", head_sha: "sha", last_seen_at: 1, pinned_at: null, ...overrides };
}

function row(overrides: Partial<SurfaceRow> & { kind: "worktree" | "branch"; identityKey: string }): SurfaceRow {
  return { label: overrides.identityKey, startRef: null, worktreePath: null, pinnedAt: null, gone: false, ...overrides };
}

test("archived search filters gone surfaces by label and detail", () => {
  const surfaces = [
    goneSurface({ identity_key: "refs/heads/feature", label: "feature", detail: "refs/heads/feature" }),
    goneSurface({ identity_key: "refs/heads/fix/auth", label: "fix/auth", detail: "refs/heads/fix/auth" }),
    goneSurface({ kind: "worktree", identity_key: "/tmp/experiment", label: "experiment", detail: "/tmp/experiment" }),
  ];
  assert.equal(filterGoneSurfaces(surfaces, "").length, 3);
  assert.equal(filterGoneSurfaces(surfaces, "   ").length, 3);
  assert.deepEqual(
    filterGoneSurfaces(surfaces, "FIX").map((surface) => surface.label),
    ["fix/auth"],
  );
  assert.deepEqual(
    filterGoneSurfaces(surfaces, "/TMP/").map((surface) => surface.label),
    ["experiment"],
  );
  assert.deepEqual(filterGoneSurfaces(surfaces, "nomatch"), []);
});

test("pin splitting puts pinned surfaces first by pinned_at and partitions archived", () => {
  const rows = [
    row({ kind: "branch", identityKey: "refs/heads/plain" }),
    row({ kind: "branch", identityKey: "refs/heads/gone-unpinned", gone: true, startRef: "sha" }),
    row({ kind: "worktree", identityKey: "/tmp/b", label: "b", pinnedAt: 5 }),
    row({ kind: "worktree", identityKey: "/tmp/a", label: "a", pinnedAt: 9 }),
    row({ kind: "branch", identityKey: "refs/heads/gone-pinned", gone: true, startRef: "sha", pinnedAt: 1 }),
  ];
  const { pinned, live, archived } = splitPinned(rows);
  assert.deepEqual(
    pinned.map((surface) => surface.label),
    ["a", "b", "refs/heads/gone-pinned"],
  );
  assert.deepEqual(
    live.map((surface) => surface.identityKey),
    ["refs/heads/plain"],
  );
  assert.deepEqual(
    archived.map((surface) => surface.identityKey),
    ["refs/heads/gone-unpinned"],
  );
});

test("surface rows carry pin state from the pin index and gone rows keep their own", () => {
  const rows = surfaceRows(
    [
      { path: "C:/repos/demo-wt", branch: "refs/heads/demo-wt", head: "sha" },
      { path: "/tmp/plain", branch: "refs/heads/plain", head: "sha" },
    ],
    ["refs/heads/pinned-branch", "refs/heads/loose"],
    [goneSurface({ identity_key: "/tmp/gone-wt", kind: "worktree", label: "", pinned_at: 3 })],
    [
      { kind: "worktree", identity_key: "C:\\repos\\demo-wt", pinned_at: 7 },
      { kind: "branch", identity_key: "refs/heads/pinned-branch", pinned_at: 2 },
    ],
  );
  const byIdentity = new Map(rows.map((surface) => [surface.identityKey, surface]));
  assert.equal(byIdentity.get("C:/repos/demo-wt")?.pinnedAt, 7);
  assert.equal(byIdentity.get("/tmp/plain")?.pinnedAt, null);
  assert.equal(byIdentity.get("refs/heads/pinned-branch")?.pinnedAt, 2);
  assert.equal(byIdentity.get("refs/heads/loose")?.pinnedAt, null);
  const goneRow = byIdentity.get("/tmp/gone-wt");
  assert.equal(goneRow?.pinnedAt, 3);
  assert.equal(goneRow?.gone, true);
  // A gone row recorded while detached has no label and falls back to its
  // identity key for display.
  assert.equal(goneRow?.label, "/tmp/gone-wt");
});

test("worktree keys normalize separators for pin comparison", () => {
  assert.equal(worktreeKey("C:\\repos\\demo"), worktreeKey("C:/repos/demo"));
  assert.equal(worktreeKey("/tmp/repo"), "/tmp/repo");
});
