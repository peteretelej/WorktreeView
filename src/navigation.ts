import type { AttentionCategory } from "./requests";

export type Worktree = { path: string; branch: string; head: string };
export type GoneSurface = { kind: "worktree" | "branch"; identity_key: string; label: string; detail: string; head_sha: string; last_seen_at: number; pinned_at: number | null };
export type SurfacePinRef = { kind: "worktree" | "branch"; identity_key: string; pinned_at: number };
export type SurfaceListing = { gone: GoneSurface[]; pinned: SurfacePinRef[] };
export type ChangedFile = { path: string; status: string; untracked: boolean };
export type RefInventory = { heads: string[]; remotes: string[]; tags: string[]; default_base: string | null };
// One for-each-ref record for the project page: ahead/behind are null when
// unknown (no upstream, no fallback base, or a gone upstream); merged is
// whether the default branch already contains the branch tip.
export type BranchSummary = { ref_name: string; head: string; author: string; subject: string; commit_date: number; upstream: string | null; ahead: number | null; behind: number | null; merged: boolean };
export type BranchInventory = { default_branch: string | null; origin_url: string | null; remote_branch_count: number; branches: BranchSummary[]; remote_branches: BranchSummary[] };
export type ReviewScope = "all" | "committed";
export type ReviewTarget = { kind: "worktree"; worktree: Worktree } | { kind: "ref"; name: string } | { kind: "commit"; sha: string; parents: string[]; defaultBaseAncestor: boolean };
// originRef records the remote-tracking ref a review came from (a remote
// branch target itself, or the history a commit was opened from), so a
// partial-clone review failure can offer to fetch that branch's content.
export type ReviewIdentity = { repoPath: string; base: string; target: ReviewTarget; scope: ReviewScope; reversed: boolean; originRef?: string };
export type CommitInfo = { sha: string; subject: string; author: string; date: string; refs: string[]; parents: string[]; default_base_ancestor: boolean };
// One describe_commit record: a rev (abbreviated hashes included) resolved to
// its full SHA plus author, git's default formatted commit date, title, body,
// and parents.
export type CommitDetail = { sha: string; author: string; date: string; subject: string; body: string; parents: string[] };

export type PortalTab = "inbox" | "reviews" | "threads";
// The portal tabs' filter fields; each tab reads its own fields and the
// rest ride along. Discrete selections push history entries, text edits
// replace the current entry in place.
export type ReviewsStateFilter = "open" | "settled" | "stale" | "no_request" | "all";
export type ThreadsStateFilter = "open" | "resolved" | "all";
export type ThreadsVoiceFilter = "all" | "human" | "agents";
export type PortalFilters = {
  category: AttentionCategory;
  reviewsState: ReviewsStateFilter;
  reviewsProject: string;
  reviewsSearch: string;
  threadsState: ThreadsStateFilter;
  threadsVoice: ThreadsVoiceFilter;
  threadsProject: string;
  threadsText: string;
};

export const DEFAULT_PORTAL_FILTERS: PortalFilters = {
  category: "requested",
  reviewsState: "all",
  reviewsProject: "",
  reviewsSearch: "",
  threadsState: "open",
  threadsVoice: "all",
  threadsProject: "",
  threadsText: "",
};

export type AppLocation =
  | { kind: "inbox" }
  | { kind: "settings" }
  | { kind: "portal"; tab: PortalTab; filters: PortalFilters }
  // A portal thread detail keyed on its root comment id.
  | { kind: "thread"; commentId: number }
  // focusedCommentId opens a review with one comment thread scrolled into
  // view and highlighted; null keeps the plain open.
  | { kind: "review"; identity: ReviewIdentity; selectedFile: ChangedFile | null; focusedCommentId: number | null }
  | { kind: "commit-history"; repoPath: string; startPointLabel: string; startRef: string | null; worktreePath: string | null; selectedCommit: CommitInfo | null; selectedFile: ChangedFile | null };

export type ReviewLocation = Extract<AppLocation, { kind: "review" }>;
export type CommitHistoryLocation = Extract<AppLocation, { kind: "commit-history" }>;

export function sameReviewTarget(left: ReviewTarget, right: ReviewTarget): boolean {
  if (left.kind !== right.kind) return false;
  if (left.kind === "ref" && right.kind === "ref") return left.name === right.name;
  if (left.kind === "commit" && right.kind === "commit") return left.sha === right.sha;
  return left.kind === "worktree" && right.kind === "worktree" && left.worktree.path === right.worktree.path;
}

function sameChangedFile(left: ChangedFile | null, right: ChangedFile | null): boolean {
  if (left === null || right === null) return left === right;
  return left.path === right.path && left.untracked === right.untracked;
}

export function sameAppLocation(left: AppLocation, right: AppLocation): boolean {
  if (left.kind !== right.kind) return false;
  if (left.kind === "portal" && right.kind === "portal") {
    return left.tab === right.tab
      && left.filters.category === right.filters.category
      && left.filters.reviewsState === right.filters.reviewsState
      && left.filters.reviewsProject === right.filters.reviewsProject
      && left.filters.reviewsSearch === right.filters.reviewsSearch
      && left.filters.threadsState === right.filters.threadsState
      && left.filters.threadsVoice === right.filters.threadsVoice
      && left.filters.threadsProject === right.filters.threadsProject
      && left.filters.threadsText === right.filters.threadsText;
  }
  if (left.kind === "thread" && right.kind === "thread") {
    return left.commentId === right.commentId;
  }
  if (left.kind === "review" && right.kind === "review") {
    return left.identity.repoPath === right.identity.repoPath
      && left.identity.base === right.identity.base
      && sameReviewTarget(left.identity.target, right.identity.target)
      && left.identity.scope === right.identity.scope
      && left.identity.reversed === right.identity.reversed
      && left.focusedCommentId === right.focusedCommentId
      && sameChangedFile(left.selectedFile, right.selectedFile);
  }
  if (left.kind === "commit-history" && right.kind === "commit-history") {
    return left.repoPath === right.repoPath
      && left.startRef === right.startRef
      && left.worktreePath === right.worktreePath
      && (left.selectedCommit === null) === (right.selectedCommit === null)
      && (left.selectedCommit === null || right.selectedCommit === null || left.selectedCommit.sha === right.selectedCommit.sha)
      && sameChangedFile(left.selectedFile, right.selectedFile);
  }
  return true;
}

export type NavigationListener = () => void;

export type NavigationHistory = {
  push(entry: AppLocation): void;
  replace(entry: AppLocation): void;
  back(): AppLocation;
  forward(): AppLocation;
  peekBack(): AppLocation | null;
  canBack(): boolean;
  canForward(): boolean;
  current(): AppLocation;
  subscribe(listener: NavigationListener): () => void;
};

const MAX_HISTORY_ENTRIES = 100;

export function createNavigationHistory(): NavigationHistory {
  let entries: AppLocation[] = [{ kind: "inbox" }];
  let index = 0;
  const listeners = new Set<NavigationListener>();
  function notify() { for (const listener of listeners) listener(); }
  return {
    push(entry) {
      if (sameAppLocation(entries[index], entry)) return;
      entries = [...entries.slice(0, index + 1), entry];
      if (entries.length > MAX_HISTORY_ENTRIES) entries = entries.slice(entries.length - MAX_HISTORY_ENTRIES);
      index = entries.length - 1;
      notify();
    },
    replace(entry) {
      if (sameAppLocation(entries[index], entry)) return;
      entries = [...entries.slice(0, index), entry, ...entries.slice(index + 1)];
      notify();
    },
    back() { if (index > 0) { index -= 1; notify(); } return entries[index]; },
    peekBack() { return index > 0 ? entries[index - 1] : null; },
    forward() { if (index < entries.length - 1) { index += 1; notify(); } return entries[index]; },
    canBack() { return index > 0; },
    canForward() { return index < entries.length - 1; },
    current() { return entries[index]; },
    subscribe(listener) { listeners.add(listener); return () => { listeners.delete(listener); }; },
  };
}
