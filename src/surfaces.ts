import type { GoneSurface, SurfacePinRef, Worktree } from "./navigation";

// One sidebar child row the pin/archive logic operates on. Live rows come
// from the worktree and branch inventory, gone rows from retrospection.
export type SurfaceRow = {
  kind: "worktree" | "branch";
  identityKey: string;
  label: string;
  startRef: string | null;
  worktreePath: string | null;
  pinnedAt: number | null;
  gone: boolean;
};

// Live inventory reports forward-slash porcelain paths while stored pin keys
// are canonical plain paths; compare both on a shared separator form.
export function worktreeKey(path: string): string {
  return path.split("\\").join("/");
}

function pinKey(kind: string, identityKey: string): string {
  return `${kind}:${kind === "worktree" ? worktreeKey(identityKey) : identityKey}`;
}

export function surfacePinIndex(pins: SurfacePinRef[]): Map<string, number> {
  return new Map(pins.map((pin) => [pinKey(pin.kind, pin.identity_key), pin.pinned_at]));
}

function shortRef(ref: string): string {
  return ref.startsWith("refs/heads/") ? ref.slice("refs/heads/".length) : ref;
}

// Build the repo's child rows: live worktrees, live branches, then recorded
// gone surfaces (label falls back to the identity for rows recorded while
// detached). Live worktree pins match through separator-normalized keys.
export function surfaceRows(worktrees: Worktree[], branches: string[], gone: GoneSurface[], pins: SurfacePinRef[]): SurfaceRow[] {
  const index = surfacePinIndex(pins);
  const rows: SurfaceRow[] = [];
  for (const worktree of worktrees) {
    rows.push({
      kind: "worktree",
      identityKey: worktree.path,
      label: shortRef(worktree.branch),
      startRef: null,
      worktreePath: worktree.path,
      pinnedAt: index.get(`worktree:${worktreeKey(worktree.path)}`) ?? null,
      gone: false,
    });
  }
  for (const branch of branches) {
    rows.push({
      kind: "branch",
      identityKey: branch,
      label: shortRef(branch),
      startRef: branch,
      worktreePath: null,
      pinnedAt: index.get(`branch:${branch}`) ?? null,
      gone: false,
    });
  }
  for (const surface of gone) {
    rows.push({
      kind: surface.kind,
      identityKey: surface.identity_key,
      label: surface.label || shortRef(surface.identity_key),
      startRef: surface.head_sha,
      worktreePath: null,
      pinnedAt: surface.pinned_at,
      gone: true,
    });
  }
  return rows;
}

// Pinned surfaces sort first among a repo's children by pinned_at desc;
// unpinned gone surfaces partition into the Archived section instead of
// rendering inline.
export function splitPinned(rows: SurfaceRow[]): { pinned: SurfaceRow[]; live: SurfaceRow[]; archived: SurfaceRow[] } {
  const pinned: SurfaceRow[] = [];
  const live: SurfaceRow[] = [];
  const archived: SurfaceRow[] = [];
  for (const row of rows) {
    if (row.pinnedAt !== null) pinned.push(row);
    else if (row.gone) archived.push(row);
    else live.push(row);
  }
  pinned.sort((left, right) => (right.pinnedAt ?? 0) - (left.pinnedAt ?? 0));
  return { pinned, live, archived };
}

// Archived search: case-insensitive substring match on label and detail.
export function filterGoneSurfaces(surfaces: GoneSurface[], query: string): GoneSurface[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return surfaces;
  return surfaces.filter((surface) => surface.label.toLowerCase().includes(needle) || surface.detail.toLowerCase().includes(needle));
}
