// Mirrors of the serde types behind the list_attention command, plus the
// pure helpers the Attention queue renders through: the backend owns
// category membership, the webview only counts, filters, sorts, and
// labels from the payload.

export type AttentionCategory =
  | "requested"
  | "changes_requested"
  | "needs_human"
  | "unresolved_findings"
  | "changed_since_review";

export type AttentionRow = {
  request_id: number | null;
  repo_path: string;
  base_sha: string;
  target_key: string;
  target_kind: "worktree" | "head";
  change_label: string;
  requester: string;
  status: string;
  round: number;
  max_rounds: number;
  unresolved_p0: number;
  unresolved_p1: number;
  category: AttentionCategory;
  needs_human: boolean;
  age_basis: number;
  head_sha: string | null;
};

export type AttentionRepoGroup = { repo_path: string; repo_name: string; rows: AttentionRow[] };
export type AttentionQueue = { repos: AttentionRepoGroup[] };

// Mirrors the payload of the Rust `review-request-changed` event, the
// queue's live-update signal for every request mutation.
export type RequestChange = {
  request_id: number;
  repo_path: string;
  base_sha: string;
  target_key: string;
  target_kind: "worktree" | "head";
  status: string;
};

// Tab order and copy; the queue's empty state reuses the per-tab copy.
export const ATTENTION_TABS: Array<{ id: AttentionCategory; label: string; empty: string }> = [
  { id: "requested", label: "Review requested", empty: "No open review requests" },
  { id: "changes_requested", label: "Changes requested", empty: "Nothing is waiting on fixes" },
  { id: "needs_human", label: "Needs human", empty: "Nothing needs a human" },
  { id: "unresolved_findings", label: "Unresolved findings", empty: "No unresolved findings" },
  { id: "changed_since_review", label: "Changed since review", empty: "Nothing changed after review" },
];

export function attentionRows(queue: AttentionQueue | null): AttentionRow[] {
  return queue ? queue.repos.flatMap((group) => group.rows) : [];
}

export function attentionTabCounts(rows: AttentionRow[]): Record<AttentionCategory, number> {
  const counts: Record<AttentionCategory, number> = {
    requested: 0,
    changes_requested: 0,
    needs_human: 0,
    unresolved_findings: 0,
    changed_since_review: 0,
  };
  for (const row of rows) counts[row.category] += 1;
  return counts;
}

// Stable within a category: P0 count desc, then age desc (oldest first),
// then request id, so a refreshed payload with the same rows never
// reshuffles what the user is reading.
export function sortAttentionRows(rows: AttentionRow[]): AttentionRow[] {
  return [...rows].sort((left, right) =>
    right.unresolved_p0 - left.unresolved_p0
    || left.age_basis - right.age_basis
    || (left.request_id ?? 0) - (right.request_id ?? 0));
}

export function rowsForAttentionTab(rows: AttentionRow[], tab: AttentionCategory): AttentionRow[] {
  return sortAttentionRows(rows.filter((row) => row.category === tab));
}

function sameIdentity(left: AttentionRow, right: AttentionRow): boolean {
  return left.repo_path === right.repo_path
    && left.base_sha === right.base_sha
    && left.target_key === right.target_key
    && left.target_kind === right.target_kind;
}

// Concurrent requests on one review identity share the change label; the
// request count rides the label so duplicates read as intentional.
export function groupedChangeLabel(row: AttentionRow, rows: AttentionRow[]): string {
  const shared = rows.filter((other) => sameIdentity(other, row)).length;
  return shared > 1 ? `${row.change_label} (${shared} requests)` : row.change_label;
}

// Below ~800px of content width the findings columns collapse into one
// chip and Age folds into the row meta line.
export function isNarrowAttention(contentWidth: number): boolean {
  return contentWidth < 800;
}

// Compact relative age from the row's age basis (ms epoch).
export function attentionAge(ageBasis: number, now: number): string {
  const minutes = Math.floor(Math.max(0, now - ageBasis) / 60000);
  if (minutes < 1) return "now";
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d`;
  return `${Math.floor(days / 7)}w`;
}

// The status chip's text and tone: a needs-human row reads in the
// attention color, a lifecycle status reads as itself, and a row with no
// request behind it renders no chip.
export function attentionStatus(row: AttentionRow): { label: string; tone: "needs-human" | "status" } | null {
  if (row.needs_human) return { label: "needs human", tone: "needs-human" };
  if (row.status === "") return null;
  return { label: row.status.replace(/_/g, " "), tone: "status" };
}
