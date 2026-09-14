// Mirrors of the serde types behind the list_portal_reviews command, plus
// the pure helpers the Reviews tab renders through: the backend owns
// inclusion, state classification, search matching, and ordering, and the
// webview only counts, narrows by carried fields, and labels from the
// payload.

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

export function rowsForProject(rows: PortalReviewRow[], project: string): PortalReviewRow[] {
  return project === "" ? rows : rows.filter((row) => row.repo_path === project);
}

// Distinct project paths in the payload, sorted for a stable select.
export function reviewsProjectOptions(rows: PortalReviewRow[]): string[] {
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
