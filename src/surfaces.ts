import type { ChangedFile, GoneSurface, ReviewTarget, SurfacePinRef, Worktree } from "./navigation";

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
  return ref.startsWith("refs/heads/")
    ? ref.slice("refs/heads/".length)
    : ref.startsWith("refs/remotes/")
      ? ref.slice("refs/remotes/".length)
      : ref;
}

// A gone worktree reads as its worktree folder name; the recorded branch
// label (often "main") would read as the repository's main branch.
export function goneSurfaceLabel(surface: GoneSurface): string {
  if (surface.kind === "worktree") {
    const tail = surface.identity_key.split(/[\\/]/).filter(Boolean).pop();
    if (tail) return tail;
  }
  return shortRef(surface.label || surface.identity_key);
}

// Build the repo's child rows: live worktrees, live branches (local and
// remote; the refs/remotes/ prefix is stripped so "origin/main" shows), then
// recorded gone surfaces. Live worktree pins match through
// separator-normalized keys.
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
      label: goneSurfaceLabel(surface),
      startRef: surface.head_sha,
      worktreePath: null,
      pinnedAt: surface.pinned_at,
      gone: true,
    });
  }
  return rows;
}

// Pinned surfaces sort by pinned_at desc; the sidebar renders only these
// plus the capped active worktrees, so unpinned rows stay unpreserved here.
export function pinnedSurfaces(rows: SurfaceRow[]): SurfaceRow[] {
  const pinned = rows.filter((row) => row.pinnedAt !== null);
  pinned.sort((left, right) => (right.pinnedAt ?? 0) - (left.pinnedAt ?? 0));
  return pinned;
}

// Archived search: case-insensitive substring match on label and detail.
export function filterGoneSurfaces(surfaces: GoneSurface[], query: string): GoneSurface[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return surfaces;
  return surfaces.filter((surface) => surface.label.toLowerCase().includes(needle) || surface.detail.toLowerCase().includes(needle));
}

// Sidebar children are capped: pinned surfaces stay explicit and everything
// else shows the most recently committed worktrees. Worktrees on branches
// with unknown dates (detached HEAD, inventory not loaded yet) keep their
// list order behind dated ones.
export function recentWorktrees(worktrees: Worktree[], dateByBranch: Map<string, number>, pinnedKeys: ReadonlySet<string>, limit: number): Worktree[] {
  return worktrees
    .filter((worktree) => !pinnedKeys.has(worktreeKey(worktree.path)))
    .map((worktree, index) => ({ worktree, index, date: dateByBranch.get(worktree.branch) ?? 0 }))
    .sort((left, right) => right.date - left.date || left.index - right.index)
    .slice(0, limit)
    .map((entry) => entry.worktree);
}

// Open/reveal only needs a plausible checkout root; the backend re-validates
// the joined path at click time and refuses a missing file, so existence is
// never probed at render. Prefer the checkout that owns the reviewed
// content: the reviewed worktree itself, or the worktree with a ref target's
// branch checked out. Everything else (commits, remote refs, gone surfaces)
// falls back to the repository's main worktree folder.
export function reviewFileRoot(target: ReviewTarget, file: ChangedFile | null, worktrees: Worktree[] | undefined, repoPath: string): string | null {
  if (file === null) return null;
  if (target.kind === "worktree") return target.worktree.path;
  if (target.kind === "ref") {
    const owning = worktrees?.find((worktree) => worktree.branch === target.name);
    if (owning) return owning.path;
  }
  return repoPath;
}
