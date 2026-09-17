import { shortToken } from "./format.ts";

// Labels and sentences for the arrival cue and OS toasts. Both surfaces
// describe one moment (a delivered review or a started review) with labels
// derived webview-side from the event payload.

export type ArrivalMoment = "delivery" | "announce";

// Mirrors the backend's identity_change_label fallback: heads shorten to
// their token, worktree paths to their basename.
export function arrivalChangeLabel(targetKind: string, targetKey: string) {
  if (targetKind === "head") return shortToken(targetKey);
  return targetKey.slice(Math.max(targetKey.lastIndexOf("/"), targetKey.lastIndexOf("\\")) + 1);
}

// The registered repo's display name wins; an unregistered path falls back
// to its basename.
export function arrivalProjectLabel(repos: Array<{ path: string; name: string }>, repoPath: string) {
  const repo = repos.find((candidate) => candidate.path === repoPath);
  if (repo) return repo.name;
  return repoPath.slice(Math.max(repoPath.lastIndexOf("/"), repoPath.lastIndexOf("\\")) + 1);
}

// The sentence body after the actor name; the cue and the toast body both
// prefix the actor.
export function arrivalSentenceBody(moment: ArrivalMoment, change: string, project: string) {
  return moment === "delivery"
    ? `delivered a review on ${change} (${project})`
    : `started reviewing ${change} (${project})`;
}

// Older queued entries stack behind the newest sentence.
export function olderArrivalsSuffix(count: number) {
  return count > 0 ? ` (+${count} older)` : "";
}
