// Mirrors of the serde types behind the portal commands (list_portal_reviews,
// list_portal_threads, get_portal_thread, search_portal), plus the pure
// helpers the portal views render through: the backend owns inclusion,
// grouping, state classification, search matching, and bounds, and the
// webview only counts, narrows by carried fields, and labels from the
// payload.

import type { ReviewComment } from "./comments.ts";
import { requestStatusLabel } from "./requests.ts";

export type PortalReviewState = "open" | "settled" | "stale" | "no_request";
export type ReviewsStateFilter = PortalReviewState | "all";

export type PortalReviewRow = {
  repo_path: string;
  base_sha: string;
  target_key: string;
  target_kind: "worktree" | "head";
  change_label: string;
  head_sha: string | null;
  requester: string;
  status: string;
  note: string;
  round: number;
  max_rounds: number;
  unresolved_finding_counts: { P0: number; P1: number; P2: number; P3: number };
  comment_count: number;
  submission_count: number;
  last_activity_at: number;
  state: PortalReviewState;
  age_basis: number;
};

// The identity fields a row click needs to open the review at its recorded
// head and base; AttentionRow is structurally compatible.
export type ReviewIdentityRef = Pick<PortalReviewRow, "repo_path" | "base_sha" | "target_key" | "target_kind" | "head_sha">;

// Chip order and copy; the tab's empty state reuses the All chip's copy.
export const REVIEWS_STATE_FILTERS: Array<{ id: ReviewsStateFilter; label: string; empty: string }> = [
  { id: "open", label: "Open", empty: "No open reviews" },
  { id: "settled", label: "Settled", empty: "No settled reviews" },
  { id: "stale", label: "Stale", empty: "Nothing changed after review" },
  { id: "no_request", label: "No request", empty: "No request-less activity" },
  { id: "all", label: "All", empty: "No review activity yet" },
];

// The search needle is matched server-side as a plain substring; only
// trimming happens here, so a whitespace-only needle skips the command's
// search pass entirely.
export function normalizeReviewsSearch(raw: string): string {
  return raw.trim();
}

// Full hashes shorten to 7 characters; anything else (a branch name, an
// abbreviated sha) displays as-is.
export function shortSha(sha: string): string {
  return /^[0-9a-f]{40}$/i.test(sha) ? sha.slice(0, 7) : sha;
}

export function reviewsStateCounts(rows: PortalReviewRow[]): Record<ReviewsStateFilter, number> {
  const counts: Record<ReviewsStateFilter, number> = { open: 0, settled: 0, stale: 0, no_request: 0, all: rows.length };
  for (const row of rows) counts[row.state] += 1;
  return counts;
}

export function rowsForReviewsState(rows: PortalReviewRow[], filter: ReviewsStateFilter): PortalReviewRow[] {
  return filter === "all" ? rows : rows.filter((row) => row.state === filter);
}

// Shared project narrowing for both portal listings; any row carrying its
// repo_path works.
export function rowsForProject<T extends { repo_path: string }>(rows: T[], project: string): T[] {
  return project === "" ? rows : rows.filter((row) => row.repo_path === project);
}

// Distinct project paths in the payload, sorted for a stable select.
export function projectOptions<T extends { repo_path: string }>(rows: T[]): string[] {
  return [...new Set(rows.map((row) => row.repo_path))].sort((left, right) => left.localeCompare(right));
}

// The row's status chip: the backend's state decides the tone, an open row
// reads as its lifecycle status, and a request-less row says so.
export function reviewsStatusChip(row: PortalReviewRow): { label: string; tone: "stale" | "settled" | "open" | "quiet" } {
  switch (row.state) {
    case "stale": return { label: "changed since review", tone: "stale" };
    case "settled": return { label: requestStatusLabel(row.status), tone: "settled" };
    case "open": return { label: requestStatusLabel(row.status), tone: "open" };
    case "no_request": return { label: "no request", tone: "quiet" };
  }
}

// ===== Threads tab =====

export type ThreadsStateFilter = "open" | "resolved" | "all";
export type ThreadsVoiceFilter = "all" | "human" | "agents";

export type PortalParticipant = { author_kind: "human" | "agent"; author_name: string };

export type PortalThreadRow = {
  root_comment_id: number;
  repo_path: string;
  base_sha: string;
  target_key: string;
  target_kind: "worktree" | "head";
  excerpt: string;
  severity: "P0" | "P1" | "P2" | "P3" | null;
  anchor: string | null;
  participants: PortalParticipant[];
  reply_count: number;
  resolved_at: number | null;
  last_activity_at: number;
  head_moved: boolean;
};

// One change's thread group: threads grouped by (repo_path, target_key,
// target_kind), so they survive base changes.
export type PortalThreadGroup = {
  repo_path: string;
  target_key: string;
  target_kind: "worktree" | "head";
  change_label: string;
  open_count: number;
  threads: PortalThreadRow[];
};

// A thread's full conversation; the root and replies are the review
// surface's stored comments, anchors and snippets included.
export type PortalThreadDetail = {
  root_comment_id: number;
  repo_path: string;
  base_sha: string;
  target_key: string;
  target_kind: "worktree" | "head";
  change_label: string;
  head_sha: string | null;
  head_moved: boolean;
  resolved_at: number | null;
  root: ReviewComment;
  replies: ReviewComment[];
  participants: PortalParticipant[];
};

// Chip order and copy; the tab's empty state reuses the All chip's copy.
export const THREADS_STATE_FILTERS: Array<{ id: ThreadsStateFilter; label: string; empty: string }> = [
  { id: "open", label: "Open", empty: "No open threads" },
  { id: "resolved", label: "Resolved", empty: "No resolved threads" },
  { id: "all", label: "All", empty: "No threads yet" },
];

export const THREADS_VOICE_FILTERS: Array<{ id: ThreadsVoiceFilter; label: string }> = [
  { id: "all", label: "All" },
  { id: "human", label: "Human" },
  { id: "agents", label: "Agents" },
];

// The threads text needle narrows server-side like the reviews search;
// only trimming happens here.
export function normalizeThreadsText(raw: string): string {
  return raw.trim();
}

export function threadsStateCounts(rows: PortalThreadRow[]): Record<ThreadsStateFilter, number> {
  const counts: Record<ThreadsStateFilter, number> = { open: 0, resolved: 0, all: rows.length };
  for (const row of rows) counts[row.resolved_at === null ? "open" : "resolved"] += 1;
  return counts;
}

export function rowsForThreadsState(rows: PortalThreadRow[], filter: ThreadsStateFilter): PortalThreadRow[] {
  if (filter === "all") return rows;
  return rows.filter((row) => (filter === "open" ? row.resolved_at === null : row.resolved_at !== null));
}

// Voice reads any participant, exactly like the backend's filter: a thread
// answers the human or agents voice when any participant carries it. The
// voice id is plural ("agents"); the stored author_kind is singular.
export function rowsForThreadsVoice(rows: PortalThreadRow[], voice: ThreadsVoiceFilter): PortalThreadRow[] {
  if (voice === "all") return rows;
  const kind = voice === "human" ? "human" : "agent";
  return rows.filter((row) => row.participants.some((participant) => participant.author_kind === kind));
}

// The thread's in-app route label; hash serialization is deferred, so this
// is the path text the Copy link action shares, not a URL.
export function threadRouteLabel(rootCommentId: number): string {
  return `pulse/thread/${rootCommentId}`;
}

// ===== Palette search =====

export type PortalSearchComment = {
  comment_id: number;
  root_comment_id: number;
  excerpt: string;
  repo_path: string;
  change_label: string;
};

export type PortalSearchRequest = {
  repo_path: string;
  base_sha: string;
  target_key: string;
  target_kind: "worktree" | "head";
  head_sha: string | null;
  change_label: string;
  status: string;
  requester: string;
  note: string;
};

export type PortalSearchCommit = {
  repo_path: string;
  sha: string;
  subject: string;
  parents: string[];
};

export type PortalSearchMatches = {
  comments: PortalSearchComment[];
  requests: PortalSearchRequest[];
  commits: PortalSearchCommit[];
};

// The palette's combined result count: every scope rides one paged list.
export function searchResultCount(matches: PortalSearchMatches | null): number {
  if (!matches) return 0;
  return matches.comments.length + matches.requests.length + matches.commits.length;
}

// ===== Activity tab =====

// Mirrors the backend's EventRow: one narrated mutation from the store's
// append-only event log.
export type PortalActivityEvent = {
  id: number;
  repo_path: string;
  kind: string;
  base_sha: string | null;
  target_key: string | null;
  target_kind: string | null;
  request_id: number | null;
  comment_id: number | null;
  actor_kind: "human" | "agent";
  actor_name: string;
  summary: string;
  created_at: number;
};

// The list_portal_activity answer: the newest-first feed page plus the
// seen watermark from the same read, so the divider and the events it
// splits share one snapshot.
export type PortalActivityPage = { events: PortalActivityEvent[]; seen_id: number };

// The closed event vocabulary's display labels; unknown kinds fall back to
// the raw kind rather than disappearing.
export function activityKindLabel(kind: string): string {
  const labels: Record<string, string> = {
    request_created: "review requested",
    request_claimed: "review claimed",
    request_verdict: "verdict",
    request_re_requested: "re-requested",
    request_withdrawn: "withdrawn",
    submission_delivered: "submission",
    comment_posted: "comment",
    comment_replied: "reply",
    comment_resolved: "resolved",
    comment_reopened: "reopened",
    surface_head_moved: "head moved",
    repo_added: "project added",
  };
  return labels[kind] ?? kind.replace(/_/g, " ");
}

// The row glyph's family: the closed vocabulary folds into five icons.
export function activityKindFamily(kind: string): "request" | "comment" | "submission" | "surface" | "project" {
  if (kind.startsWith("request")) return "request";
  if (kind.startsWith("comment")) return "comment";
  if (kind === "submission_delivered") return "submission";
  if (kind === "surface_head_moved") return "surface";
  return "project";
}

function activityDayKey(at: number): string {
  const date = new Date(at);
  return `${date.getFullYear()}-${date.getMonth()}-${date.getDate()}`;
}

// Consecutive events on one calendar day share a group labeled Today,
// Yesterday, or the date; the feed arrives newest-first, so groups come
// out newest-first too.
export function activityDayGroups(events: PortalActivityEvent[], now: number): Array<{ label: string; events: PortalActivityEvent[] }> {
  const startOfToday = new Date(now).setHours(0, 0, 0, 0);
  const startOfYesterday = new Date(now).setHours(0, 0, 0, 0) - 86_400_000;
  const groups: Array<{ label: string; events: PortalActivityEvent[] }> = [];
  let currentKey = "";
  for (const event of events) {
    const key = activityDayKey(event.created_at);
    if (key !== currentKey) {
      const label = event.created_at >= startOfToday ? "Today"
        : event.created_at >= startOfYesterday ? "Yesterday"
        : new Date(event.created_at).toLocaleDateString(undefined, { month: "short", day: "numeric" });
      groups.push({ label, events: [] });
      currentKey = key;
    }
    groups[groups.length - 1].events.push(event);
  }
  return groups;
}

// The "new since your last visit" divider renders above the feed's first
// event past the seen watermark; -1 when everything visible is seen.
export function activityDividerIndex(events: PortalActivityEvent[], seenId: number): number {
  return events.findIndex((event) => event.id > seenId);
}
