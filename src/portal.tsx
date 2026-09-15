import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Bot, Check, Copy, ExternalLink, FolderGit2, GitBranch, Inbox, ListTree, MessageSquare, Search, User } from "lucide-react";
import { attentionAge, isNarrowAttention } from "./requests.ts";
import { copyText } from "./clipboard.ts";
import { CommentThreadView, type CommentsApi } from "./comments.tsx";
import { anchorLabel } from "./comments.ts";
import {
  activityDayGroups,
  activityDividerIndex,
  activityKindFamily,
  activityKindLabel,
  projectOptions,
  reviewEventText,
  reviewsStateCounts,
  reviewsStatusChip,
  rowsForProject,
  rowsForReviewsState,
  rowsForThreadsState,
  rowsForThreadsVoice,
  shortSha,
  threadIdentityRef,
  threadRouteLabel,
  threadsStateCounts,
  REVIEWS_STATE_FILTERS,
  THREADS_STATE_FILTERS,
  THREADS_VOICE_FILTERS,
  type PortalActivityEvent,
  type PortalReviewRow,
  type PortalThreadDetail,
  type PortalThreadGroup,
  type PortalThreadRow,
  type ReviewIdentityRef,
  type ReviewsStateFilter,
  type ThreadsStateFilter,
  type ThreadsVoiceFilter,
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
  const projects = projectOptions(rows);
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
  return <section className="inbox-pane attention-pane reviews-pane" ref={paneRef} aria-label="Reviews">
    <div className="reviews-filter-row">
      <div className="overview-tabs" role="tablist" aria-label="Review states">{REVIEWS_STATE_FILTERS.map((chip) => <button key={chip.id} role="tab" type="button" aria-selected={reviewsState === chip.id} className={`overview-tab ${reviewsState === chip.id ? "active" : ""}`} onClick={() => onState(chip.id)}>{chip.label}<span className="tab-count">{counts[chip.id]}</span></button>)}</div>
      <label className="overview-filter-input reviews-project"><select aria-label="Filter by project" value={reviewsProject} onChange={(event) => onProject(event.currentTarget.value)}><option value="">All projects</option>{projects.map((path) => <option key={path} value={path}>{repoNames.get(path) ?? path}</option>)}</select></label>
      <div className="overview-filter-input"><Search size={12} /><input type="text" aria-label="Search reviews" placeholder="Branch, sha, requester, note" value={reviewsSearch} onChange={(event) => onSearch(event.currentTarget.value)} /></div>
    </div>
    {payload?.error ? <ReviewsEmpty title="Reviews could not be loaded" detail={payload.error} /> : payload === null || (payload.loading && rows.length === 0) ? <ReviewsEmpty title="Loading reviews..." detail="Reading stored review activity." /> : rows.length === 0 ? <ReviewsEmpty title="No review activity yet" detail="Reviews appear here once a request, comment, or submission is recorded." /> : <>
      <div className={`table-header attention-head ${narrow ? "attention-narrow" : ""}`} aria-hidden="true"><span>Project</span><span>Change</span><span>Requester</span><span>Status</span><span>Round</span><span>Findings</span>{!narrow && <span>Last event</span>}{!narrow && <span className="attention-age">Age</span>}</div>
      <div className={`worktree-list attention-list ${narrow ? "attention-narrow" : ""}`}>{visible.map((row) => {
        const chip = reviewsStatusChip(row);
        const openRow = () => onOpenRow(row);
        const p0 = row.unresolved_finding_counts.P0;
        const p1 = row.unresolved_finding_counts.P1;
        const chipClass = chip.tone === "ok" ? "reviews-ok" : chip.tone === "warn" ? "reviews-warn" : chip.tone === "muted" ? "reviews-muted" : "";
        const event = reviewEventText(row, now);
        return <div key={`${row.repo_path}:${row.base_sha}:${row.target_key}:${row.target_kind}`} className="attention-row" role="button" tabIndex={0} onClick={openRow} onKeyDown={(event) => { if (event.target !== event.currentTarget) return; if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openRow(); } }}>
          <div className="attention-project"><strong title={row.repo_path}>{repoNames.get(row.repo_path) ?? row.repo_path}</strong></div>
          <div className="attention-change"><strong>{row.change_label}</strong><span className="attention-meta">{row.target_kind === "head"
            ? <code title={row.target_key}>{shortSha(row.target_key)}</code>
            : <span title={row.target_key}>worktree</span>}
            {narrow && <span className="attention-age">{attentionAge(row.age_basis, now)}</span>}</span></div>
          <div className="attention-requester">{row.requester
            ? <span className={`comment-badge requester-badge requester-${row.requester_kind}`} title={row.requester_kind === "human" ? `Requested by ${row.requester}` : `Requested by agent ${row.requester}`}>{row.requester_kind === "human" ? <User size={10} /> : <Bot size={10} />}{row.requester}</span>
            : <span className="attention-none">none</span>}</div>
          <div className="attention-status"><span className={`status-chip ${chipClass}`}>{chip.label}</span></div>
          <div className="attention-round">{row.max_rounds > 0 && <code title={`Round ${row.round} of ${row.max_rounds}`}>{row.round}/{row.max_rounds}</code>}</div>
          <div className="attention-findings reviews-findings">{narrow
            ? p0 + p1 > 0 && <span className="comment-badge severity-P1" aria-label={`${p0 + p1} blocking findings`}>{p0 + p1}</span>
            : <>{p0 > 0 && <span className="finding-p0" aria-label={`${p0} P0 findings`}>{p0} P0</span>}{p0 > 0 && p1 > 0 && <span> · </span>}{p1 > 0 && <span className="finding-p1" aria-label={`${p1} P1 findings`}>{p1} P1</span>}{p0 + p1 === 0 && <span className="finding-none">0</span>}</>}</div>
          {!narrow && <div className="attention-event" title={event}>{event}</div>}
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

// The Threads tab's fetched listing; null until the first load lands.
export type PortalThreadsPayload = { groups: PortalThreadGroup[]; loading: boolean; error: string };

// The Pulse Threads tab: every root comment across every review identity,
// grouped by change. Grouping, filters, ordering, and bounds arrive from
// the backend; this view counts, narrows by carried fields, and renders.
export function PortalThreadsTab({ payload, repoNames, threadsState, threadsVoice, threadsProject, threadsText, onState, onVoice, onProject, onText, onOpenThread }: {
  payload: PortalThreadsPayload | null;
  repoNames: Map<string, string>;
  threadsState: ThreadsStateFilter;
  threadsVoice: ThreadsVoiceFilter;
  threadsProject: string;
  threadsText: string;
  onState: (state: ThreadsStateFilter) => void;
  onVoice: (voice: ThreadsVoiceFilter) => void;
  onProject: (project: string) => void;
  onText: (text: string) => void;
  onOpenThread: (thread: PortalThreadRow) => void;
}) {
  const groups = payload?.groups ?? [];
  const projectGroups = groups
    .map((group) => ({ ...group, threads: rowsForProject(group.threads, threadsProject) }))
    .filter((group) => group.threads.length > 0);
  const flat = projectGroups.flatMap((group) => group.threads);
  const counts = threadsStateCounts(flat);
  const visibleGroups = projectGroups
    .map((group) => ({ ...group, threads: rowsForThreadsVoice(rowsForThreadsState(group.threads, threadsState), threadsVoice) }))
    .filter((group) => group.threads.length > 0);
  const activeChip = THREADS_STATE_FILTERS.find((chip) => chip.id === threadsState) ?? THREADS_STATE_FILTERS[0];
  const projects = projectOptions(groups.flatMap((group) => group.threads));
  const now = Date.now();
  return <section className="inbox-pane attention-pane threads-pane" aria-label="Threads">
    <div className="reviews-filter-row">
      <div className="overview-tabs" role="tablist" aria-label="Thread states">{THREADS_STATE_FILTERS.map((chip) => <button key={chip.id} role="tab" type="button" aria-selected={threadsState === chip.id} className={`overview-tab ${threadsState === chip.id ? "active" : ""}`} onClick={() => onState(chip.id)}>{chip.label}<span className="tab-count">{counts[chip.id]}</span></button>)}</div>
      <div className="overview-tabs" role="tablist" aria-label="Thread voices">{THREADS_VOICE_FILTERS.map((chip) => <button key={chip.id} role="tab" type="button" aria-selected={threadsVoice === chip.id} className={`overview-tab ${threadsVoice === chip.id ? "active" : ""}`} onClick={() => onVoice(chip.id)}>{chip.label}</button>)}</div>
      <label className="overview-filter-input reviews-project"><select aria-label="Filter by project" value={threadsProject} onChange={(event) => onProject(event.currentTarget.value)}><option value="">All projects</option>{projects.map((path) => <option key={path} value={path}>{repoNames.get(path) ?? path}</option>)}</select></label>
      <div className="overview-filter-input"><Search size={12} /><input type="text" aria-label="Search threads" placeholder="Search thread text" value={threadsText} onChange={(event) => onText(event.currentTarget.value)} /></div>
    </div>
    {payload?.error ? <ReviewsEmpty title="Threads could not be loaded" detail={payload.error} /> : payload === null || (payload.loading && groups.length === 0) ? <ReviewsEmpty title="Loading threads..." detail="Reading stored conversations." /> : flat.length === 0 ? <ReviewsEmpty title="No threads yet" detail="Threads appear here once a review carries comments." /> : <>
      {visibleGroups.map((group) => <div key={`${group.repo_path}:${group.target_key}`} className="thread-group">
        <div className="thread-group-head">
          <strong title={group.change_label}>{group.change_label}</strong>
          <span className="thread-group-meta">
            <span className="path-text" title={group.repo_path}>{repoNames.get(group.repo_path) ?? group.repo_path}</span>
            {group.target_kind === "head"
              ? <code title={group.target_key}>{shortSha(group.target_key)}</code>
              : <span className="path-text" title={group.target_key}>{group.target_key}</span>}
            <span>{group.open_count} open</span>
          </span>
        </div>
        <div className="worktree-list attention-list">{group.threads.map((thread) => {
          const openThread = () => onOpenThread(thread);
          const resolved = thread.resolved_at !== null;
          return <div key={thread.root_comment_id} className={`attention-row thread-row ${resolved ? "thread-resolved" : ""}`} role="button" tabIndex={0} onClick={openThread} onKeyDown={(event) => { if (event.target !== event.currentTarget) return; if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openThread(); } }}>
            <div className="thread-excerpt"><span className={`thread-spine severity-${thread.severity ?? "none"}`} aria-hidden="true" /><strong title={thread.excerpt}>{thread.excerpt}</strong></div>
            <div className="thread-anchor">{thread.anchor && <code className="path-text" title={thread.anchor}>{thread.anchor}</code>}</div>
            <div className="thread-participants">{thread.participants.map((participant) => <span key={`${participant.author_kind}:${participant.author_name}`} className="comment-badge" title={`${participant.author_kind} ${participant.author_name}`}>{participant.author_name}</span>)}</div>
            <div className="thread-reply-count">{thread.reply_count > 0 && <span title={`${thread.reply_count} ${thread.reply_count === 1 ? "reply" : "replies"}`}>{thread.reply_count}</span>}</div>
            <div className="thread-badges">{resolved && <span className="comment-badge comment-resolved-badge">resolved</span>}{thread.head_moved && <span className="comment-badge comment-state-badge" title="The surface's head moved past the reviewed head">moved</span>}</div>
            <div className="thread-age">{attentionAge(thread.last_activity_at, now)}</div>
          </div>;
        })}</div>
      </div>)}
      {visibleGroups.reduce((total, group) => total + group.threads.length, 0) === 0 && <div className="filter-empty">{activeChip.empty}</div>}
    </>}
  </section>;
}

// A portal thread's fetched detail; null until the load lands.
export type PortalThreadPayload = { detail: PortalThreadDetail | null; loading: boolean; error: string };

// The Copy link action: hash serialization is deferred, so this shares the
// thread's in-app route label as text; a tooltip says so, and no URL is
// implied.
function CopyRouteButton({ rootCommentId }: { rootCommentId: number }) {
  const [copied, setCopied] = useState(false);
  const label = `Copy in-app path ${threadRouteLabel(rootCommentId)}`;
  return <button className={`copy-button ${copied ? "copied" : ""}`} type="button" aria-label={label} title={copied ? "Copied" : `${label} (an in-app path, not a URL)`} onClick={async (event) => { event.stopPropagation(); if (await copyText(threadRouteLabel(rootCommentId))) { setCopied(true); setTimeout(() => setCopied(false), 1200); } }}>{copied ? <Check size={12} /> : <Copy size={12} />}</button>;
}

// The portal thread detail: the stored conversation, resolve/reopen and
// reply through the same comment commands the review surface uses, the
// anchored snippet as stored, and the change's other threads alongside.
export function PortalThreadDetail({ payload, groups, repoNames, onOpenThread, onOpenReview, onChanged }: {
  payload: PortalThreadPayload;
  groups: PortalThreadGroup[];
  repoNames: Map<string, string>;
  onOpenThread: (rootCommentId: number) => void;
  onOpenReview: (row: ReviewIdentityRef, focusedCommentId: number) => void;
  onChanged: () => void;
}) {
  const { detail } = payload;
  const group = detail
    ? groups.find((candidate) => candidate.repo_path === detail.repo_path && candidate.target_key === detail.target_key && candidate.target_kind === detail.target_kind)
    : null;
  const otherThreads = group?.threads.filter((thread) => thread.root_comment_id !== detail?.root_comment_id) ?? [];
  if (payload.error && !detail) return <section className="inbox-pane attention-pane threads-pane" aria-labelledby="thread-detail-heading"><div className="section-heading"><div className="project-heading"><h1 id="thread-detail-heading">Thread</h1></div></div><ReviewsEmpty title="Thread could not be loaded" detail={payload.error} /></section>;
  if (!detail) return <section className="inbox-pane attention-pane threads-pane" aria-labelledby="thread-detail-heading"><div className="section-heading"><div className="project-heading"><h1 id="thread-detail-heading">Thread</h1></div></div><ReviewsEmpty title="Loading thread..." detail="Reading the stored conversation." /></section>;
  const resolved = detail.resolved_at !== null;
  // The conversation renders through the review surface's own thread view;
  // its actions route to the same store commands, so the portal adds no
  // new mutation path.
  const commentsApi: CommentsApi = {
    key: null,
    threads: [{ comment: detail.root, replies: detail.replies }],
    visibleThreads: [],
    statuses: {},
    author: "all",
    setAuthor() { },
    composer: null,
    openComposer() { },
    openFileComposer() { },
    closeComposer() { },
    async refresh() { onChanged(); },
    async create() { },
    async reply(parentId, body) {
      await invoke("reply_comment", { parentId, body, severity: null });
      onChanged();
    },
    async setResolved(commentId, resolvedValue) {
      await invoke("set_comment_resolved", { commentId, resolved: resolvedValue });
      onChanged();
    },
    async edit(commentId, body) {
      await invoke("edit_comment", { commentId, body });
      onChanged();
    },
    async remove(commentId) {
      await invoke("delete_comment", { commentId });
      onChanged();
    },
  };
  const openReview = () => onOpenReview(threadIdentityRef(detail), detail.root_comment_id);
  return <section className="inbox-pane attention-pane threads-pane thread-detail" aria-labelledby="thread-detail-heading">
    <div className="thread-breadcrumb" aria-label="Thread location">
      <span title={detail.repo_path}>{repoNames.get(detail.repo_path) ?? detail.repo_path}</span>
      <span className="crumb-sep">/</span>
      <span title={detail.change_label}>{detail.change_label}</span>
      {detail.root.file_path && <><span className="crumb-sep">/</span><code className="path-text" title={detail.root.file_path}>{detail.root.file_path}{detail.root.start_line !== null ? `:${detail.root.start_line}` : ""}</code></>}
      <button className="secondary-button thread-open-review" type="button" title="Open the review this thread lives on" onClick={openReview}><ExternalLink size={12} />Open in review</button>
      <span className={`status-chip ${resolved ? "clean" : "reviews-muted"}`}>{resolved ? "resolved" : "open"}</span>
      {detail.head_moved && <span className="comment-badge comment-state-badge" title="The surface's head moved past the reviewed head">moved</span>}
      <span className="thread-breadcrumb-actions">
        <CopyRouteButton rootCommentId={detail.root_comment_id} />
      </span>
    </div>
    <div className="thread-detail-body">
      <div className="thread-conversation">
        {detail.root.snippet && <div className="thread-snippet">
          <span className="thread-snippet-label" title="The anchored lines as stored when the comment was written"><code className="path-text">{anchorLabel(detail.root)}</code></span>
          <pre className="comment-snippet"><code>{detail.root.snippet}</code></pre>
        </div>}
        <CommentThreadView thread={{ comment: detail.root, replies: detail.replies }} status={null} comments={commentsApi} />
      </div>
      <aside className="thread-others" aria-label="Other threads on this change">
        <div className="pane-heading"><strong>Other threads</strong><span>{otherThreads.length}</span></div>
        <div className="thread-others-list">
          {otherThreads.length === 0 && <div className="filter-empty">No other threads on this change</div>}
          {otherThreads.map((thread) => {
            const openThread = () => onOpenThread(thread.root_comment_id);
            return <div key={thread.root_comment_id} className={`thread-others-row ${thread.resolved_at !== null ? "thread-resolved" : ""}`} role="button" tabIndex={0} onClick={openThread} onKeyDown={(event) => { if (event.target !== event.currentTarget) return; if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openThread(); } }}>
              <strong title={thread.excerpt}>{thread.excerpt}</strong>
              <span className="thread-others-meta">{thread.resolved_at !== null ? "resolved" : "open"} · {thread.reply_count} {thread.reply_count === 1 ? "reply" : "replies"}</span>
            </div>;
          })}
        </div>
      </aside>
    </div>
  </section>;
}

// The Activity tab's fetched page; null until the first load lands. The
// seen watermark rides the page, so the divider is frozen as of load and
// only moves when Mark all seen advances it.
export type PortalActivityPayload = { events: PortalActivityEvent[]; seen_id: number; loading: boolean; error: string };

const ACTIVITY_ICONS = {
  request: ListTree,
  comment: MessageSquare,
  submission: Inbox,
  surface: GitBranch,
  project: FolderGit2,
} as const;

// The Pulse Activity tab: the store's event log as a day-grouped feed.
// Membership, ordering, and bounds arrive from the backend; this view
// groups by day, narrows by the carried repo_path, and renders. The
// divider sits at the seen watermark as of load and clears only through
// Mark all seen.
export function PortalActivityTab({ payload, repoNames, activityProject, onProject, onMarkSeen, marking }: {
  payload: PortalActivityPayload | null;
  repoNames: Map<string, string>;
  activityProject: string;
  onProject: (project: string) => void;
  onMarkSeen: () => void;
  marking: boolean;
}) {
  const events = payload?.events ?? [];
  const projectEvents = rowsForProject(events, activityProject);
  const projects = projectOptions(events);
  const groups = activityDayGroups(projectEvents, Date.now());
  const divider = payload ? activityDividerIndex(projectEvents, payload.seen_id) : -1;
  const now = Date.now();
  let flatIndex = -1;
  return <section className="inbox-pane attention-pane activity-pane" aria-label="Activity">
    <div className="reviews-filter-row">
      <label className="overview-filter-input reviews-project"><select aria-label="Filter by project" value={activityProject} onChange={(event) => onProject(event.currentTarget.value)}><option value="">All projects</option>{projects.map((path) => <option key={path} value={path}>{repoNames.get(path) ?? path}</option>)}</select></label>
      <button className="secondary-button activity-seen" type="button" disabled={!payload || marking} onClick={onMarkSeen}>{marking ? "Marking..." : "Mark all seen"}</button>
    </div>
    {payload?.error ? <ReviewsEmpty title="Activity could not be loaded" detail={payload.error} /> : payload === null ? <ReviewsEmpty title="Loading activity..." detail="Reading the stored event log." /> : events.length === 0 ? <ReviewsEmpty title="No activity yet" detail="Review requests, comments, and submissions land here as they happen." /> : <>
      {groups.map((group) => <div key={`${group.label}:${group.events[0].id}`} className="activity-day">
        <div className="activity-day-label">{group.label}</div>
        <div className="worktree-list attention-list activity-list">{group.events.map((event) => {
          flatIndex += 1;
          const family = activityKindFamily(event.kind);
          const Icon = ACTIVITY_ICONS[family];
          return <div key={event.id}>
            {flatIndex === divider && <div className="activity-divider" role="separator" aria-label="New since your last visit"><span>New since your last visit</span></div>}
            <div className="activity-row">
              <span className={`activity-kind activity-kind-${family}`} title={activityKindLabel(event.kind)}><Icon size={13} /><span>{activityKindLabel(event.kind)}</span></span>
              <span className="activity-body">
                <strong title={event.summary}>{event.summary}</strong>
                <span className="activity-meta">
                  <span className="path-text" title={event.repo_path}>{repoNames.get(event.repo_path) ?? event.repo_path}</span>
                  <span>{event.actor_kind === "human" ? "you" : event.actor_name}</span>
                  <span>{attentionAge(event.created_at, now)}</span>
                </span>
              </span>
            </div>;
          </div>;
        })}</div>
      </div>)}
    </>}
  </section>;
}
