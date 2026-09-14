import { useEffect, useRef, useState } from "react";
import { MessageSquare, Search } from "lucide-react";
import { attentionAge, isNarrowAttention } from "./requests.ts";
import {
  reviewsProjectOptions,
  reviewsStateCounts,
  reviewsStatusChip,
  rowsForProject,
  rowsForReviewsState,
  shortSha,
  REVIEWS_STATE_FILTERS,
  type ReviewIdentityRef,
  type ReviewsStateFilter,
  type PortalReviewRow,
} from "./portal.ts";

// The Reviews tab's fetched listing; null until the first load lands.
export type PortalReviewsPayload = { rows: PortalReviewRow[]; loading: boolean; error: string };

// The Pulse Reviews tab: every review identity with a request or any
// comment/submission activity. Membership, states, search, and ordering
// arrive from the backend; this view counts, narrows by carried fields,
// and renders.
export function PortalReviewsTab({ payload, repoNames, reviewsState, reviewsProject, reviewsSearch, onState, onProject, onSearch, onOpenRow }: {
  payload: PortalReviewsPayload | null;
  repoNames: Map<string, string>;
  reviewsState: ReviewsStateFilter;
  reviewsProject: string;
  reviewsSearch: string;
  onState: (state: ReviewsStateFilter) => void;
  onProject: (project: string) => void;
  onSearch: (search: string) => void;
  onOpenRow: (row: ReviewIdentityRef) => void;
}) {
  const rows = payload?.rows ?? [];
  const projectRows = rowsForProject(rows, reviewsProject);
  const counts = reviewsStateCounts(projectRows);
  const visible = rowsForReviewsState(projectRows, reviewsState);
  const projects = reviewsProjectOptions(rows);
  const activeChip = REVIEWS_STATE_FILTERS.find((chip) => chip.id === reviewsState) ?? REVIEWS_STATE_FILTERS[0];
  const paneRef = useRef<HTMLElement | null>(null);
  const [paneWidth, setPaneWidth] = useState(0);
  useEffect(() => {
    const pane = paneRef.current;
    if (!pane || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => setPaneWidth(entries[0].contentRect.width));
    observer.observe(pane);
    return () => observer.disconnect();
  }, []);
  const narrow = paneWidth > 0 && isNarrowAttention(paneWidth);
  const now = Date.now();
  return <section className="inbox-pane attention-pane reviews-pane" ref={paneRef} aria-labelledby="reviews-heading">
    <div className="section-heading"><div className="project-heading"><h1 id="reviews-heading">Reviews</h1></div></div>
    <div className="reviews-filter-row">
      <div className="overview-tabs" role="tablist" aria-label="Review states">{REVIEWS_STATE_FILTERS.map((chip) => <button key={chip.id} role="tab" type="button" aria-selected={reviewsState === chip.id} className={`overview-tab ${reviewsState === chip.id ? "active" : ""}`} onClick={() => onState(chip.id)}>{chip.label}<span className="tab-count">{counts[chip.id]}</span></button>)}</div>
      <label className="overview-filter-input reviews-project"><select aria-label="Filter by project" value={reviewsProject} onChange={(event) => onProject(event.currentTarget.value)}><option value="">All projects</option>{projects.map((path) => <option key={path} value={path}>{repoNames.get(path) ?? path}</option>)}</select></label>
      <div className="overview-filter-input"><Search size={12} /><input type="text" aria-label="Search reviews" placeholder="Search label, sha, requester, note" value={reviewsSearch} onChange={(event) => onSearch(event.currentTarget.value)} /></div>
    </div>
    {payload?.error ? <ReviewsEmpty title="Reviews could not be loaded" detail={payload.error} /> : payload === null || (payload.loading && rows.length === 0) ? <ReviewsEmpty title="Loading reviews..." detail="Reading stored review activity." /> : rows.length === 0 ? <ReviewsEmpty title="No review activity yet" detail="Reviews appear here once a request, comment, or submission is recorded." /> : <>
      <div className={`table-header attention-head ${narrow ? "attention-narrow" : ""}`} aria-hidden="true"><span>Project</span><span>Change</span><span>Requester</span><span>Status</span><span>Round</span><span>Findings</span>{!narrow && <span>Age</span>}</div>
      <div className={`worktree-list attention-list ${narrow ? "attention-narrow" : ""}`}>{visible.map((row) => {
        const chip = reviewsStatusChip(row);
        const openRow = () => onOpenRow(row);
        const p0 = row.unresolved_finding_counts.P0;
        const p1 = row.unresolved_finding_counts.P1;
        const chipClass = chip.tone === "stale" ? "reviews-stale" : chip.tone === "settled" ? "clean" : chip.tone === "quiet" ? "reviews-quiet" : "";
        return <div key={`${row.repo_path}:${row.base_sha}:${row.target_key}:${row.target_kind}`} className="attention-row" role="button" tabIndex={0} onClick={openRow} onKeyDown={(event) => { if (event.target !== event.currentTarget) return; if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openRow(); } }}>
          <div className="attention-project"><strong title={row.repo_path}>{repoNames.get(row.repo_path) ?? row.repo_path}</strong></div>
          <div className="attention-change"><strong>{row.change_label}</strong><span className="attention-meta">{row.target_kind === "head"
            ? <code title={row.target_key}>{shortSha(row.target_key)}</code>
            : <><span className="path-text" title={row.target_key}>{row.target_key}</span>{row.head_sha && <code title={`Head as last recorded: ${row.head_sha}`}>{shortSha(row.head_sha)}</code>}</>}
            {row.comment_count > 0 && <span>{row.comment_count} {row.comment_count === 1 ? "comment" : "comments"}</span>}
            {row.submission_count > 0 && <span>{row.submission_count} {row.submission_count === 1 ? "submission" : "submissions"}</span>}
            {narrow && <span className="attention-age">{attentionAge(row.age_basis, now)}</span>}</span></div>
          <div className="attention-requester">{row.requester && <span className="comment-badge" title={`Latest request by ${row.requester}`}>{row.requester}</span>}</div>
          <div className="attention-status"><span className={`status-chip ${chipClass}`}>{chip.label}</span></div>
          <div className="attention-round">{row.max_rounds > 0 && <code title={`Round ${row.round} of ${row.max_rounds}`}>{row.round}/{row.max_rounds}</code>}</div>
          <div className="attention-findings">{narrow
            ? p0 + p1 > 0 && <span className="comment-badge severity-P1" aria-label={`${p0 + p1} blocking findings`}>{p0 + p1}</span>
            : <>{p0 > 0 && <span className="comment-badge severity-P0" aria-label={`${p0} P0 findings`}>{p0} P0</span>}{p1 > 0 && <span className="comment-badge severity-P1" aria-label={`${p1} P1 findings`}>{p1} P1</span>}</>}</div>
          {!narrow && <div className="attention-age">{attentionAge(row.age_basis, now)}</div>}
        </div>;
      })}</div>
      {visible.length === 0 && <div className="filter-empty">{activeChip.empty}</div>}
    </>}
  </section>;
}

// The queue's narrow collapse: below ~800px of pane width the findings
// columns fold into one chip and Age folds into the row meta line.

function ReviewsEmpty({ title, detail }: { title: string; detail: string }) {
  return <div className="empty-state"><MessageSquare size={24} /><strong>{title}</strong><span>{detail}</span></div>;
}
