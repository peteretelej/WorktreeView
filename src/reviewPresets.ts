import type { RefInventory, ReviewTarget, Worktree } from "./navigation";

// The base a new review starts from before the user picks one: a worktree
// reviews against its unique upstream when one matches the checked-out
// branch, ref targets fall back to the repository default (unless the target
// is that default), and commits prefer the default base unless the commit is
// already its ancestor, in which case the parent diff wins.
export function autoReviewBase(target: ReviewTarget, refs: RefInventory): string {
  if (target.kind === "worktree") {
    const checkedOut = target.worktree.branch;
    const remoteBases = refs.remotes.filter((ref) => ref.endsWith(`/${checkedOut.replace(/^refs\/heads\//, "")}`));
    return checkedOut.startsWith("refs/heads/") && remoteBases.length === 1 ? remoteBases[0] : refs.default_base ?? "";
  }
  if (target.kind === "ref") return refs.default_base === target.name ? "" : refs.default_base ?? "";
  return refs.default_base && !target.defaultBaseAncestor ? refs.default_base : target.parents[0] ?? "empty-tree";
}

// The working-changes review pins the base to the checked-out branch tip (or
// the recorded HEAD sha when detached) so the diff covers only content that
// is not committed yet.
export function workingChangesBase(worktree: Worktree): string {
  return worktree.branch.startsWith("refs/heads/") ? worktree.branch : worktree.head;
}

export type WorktreeReviewPreset = "working" | "all" | "committed";
