import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { call, type RemoteSource, type SourceResolver } from "./remote.ts";
import { Check, Copy, FoldHorizontal, PanelRightClose, Trash2, UnfoldHorizontal } from "lucide-react";
import type { DiffLine } from "./diff.ts";
import type { ReviewIdentity } from "./navigation.ts";
import { copyText } from "./clipboard.ts";
import { CommentBody } from "./markdown.tsx";
import { MarkdownComposer } from "./composer.tsx";
import {
  anchorLabel,
  commentThreads,
  degradedReviewKey,
  draftFromSelection,
  exportThreadsMarkdown,
  filterThreadsByAuthor,
  formatCommentForCopy,
  inlinePlacement,
  quoteExcerpt,
  reviewKeyOf,
  toPatchLines,
  type AnchorStatus,
  type AuthorFilter,
  type CommentDraft,
  type CommentSelection,
  type CommentSeverity,
  type CommentThread,
  type DisplaySide,
  type ReviewComment,
  type ReviewIndexSummary,
  type ReviewKey,
} from "./comments.ts";

// The comment layer attaches to any rendered review identity: one hook at
// the app root owns comment state and drift statuses, and the patch panes
// and stream panels render from it.
export type ComposerState = { kind: "review" } | { kind: "file"; filePath: string } | null;

export type CommentsApi = {
  key: ReviewKey | null;
  threads: CommentThread[];
  visibleThreads: CommentThread[];
  statuses: Record<number, AnchorStatus>;
  author: AuthorFilter;
  setAuthor(author: AuthorFilter): void;
  composer: ComposerState;
  openComposer(kind: "review"): void;
  openFileComposer(filePath: string): void;
  closeComposer(): void;
  refresh(): Promise<void>;
  create(draft: CommentDraft): Promise<void>;
  reply(parentId: number, body: string): Promise<void>;
  setResolved(commentId: number, resolved: boolean): Promise<void>;
  edit(commentId: number, body: string): Promise<void>;
  remove(commentId: number): Promise<void>;
};

export function useReviewComments(identity: ReviewIdentity | null, index: ReviewIndexSummary | null, file: { path: string; lines: DiffLine[] } | null, reversed: boolean, resolve: SourceResolver = () => ({ kind: "local" }) satisfies RemoteSource): CommentsApi {
  // The resolved key wins once the index lands; until then (or when it
  // never resolves), the identity's recorded refs key the stored
  // conversation, so a review stays readable after its Git surface is gone.
  const key = useMemo(() => (identity ? (index && reviewKeyOf(identity, index)) ?? degradedReviewKey(identity) : null), [identity, index]);
  const [comments, setComments] = useState<ReviewComment[]>([]);
  const [statuses, setStatuses] = useState<Record<number, AnchorStatus>>({});
  const [author, setAuthor] = useState<AuthorFilter>("all");
  const [composer, setComposer] = useState<ComposerState>(null);
  const identityKey = key ? `${key.repoPath}\0${key.baseSha}\0${key.targetKey}\0${key.targetKind}` : "";
  // The resolver is held through a ref so the subscription effects keep
  // their identity-key deps and never resubscribe on a new closure.
  const resolveRef = useRef(resolve);
  useEffect(() => { resolveRef.current = resolve; });

  useEffect(() => {
    if (!key) {
      setComments([]);
      setComposer(null);
      return;
    }
    let cancelled = false;
    setComments([]);
    setComposer(null);
    void call<ReviewComment[]>("list_comments", { repoPath: key.repoPath, baseSha: key.baseSha, targetKey: key.targetKey, targetKind: key.targetKind }, resolveRef.current(key.repoPath))
      .then((loaded) => { if (!cancelled) setComments(loaded); })
      .catch(() => { if (!cancelled) setComments([]); });
    return () => { cancelled = true; };
  }, [identityKey]);

  // One batched match per loaded file: the patch's parsed lines go to the
  // backend matcher, which returns every comment's status in a single call.
  useEffect(() => {
    if (!key || !file || file.lines.length === 0) {
      setStatuses({});
      return;
    }
    let cancelled = false;
    void call<AnchorStatus[]>("match_comment_anchors", {
      repoPath: key.repoPath,
      baseSha: key.baseSha,
      targetKey: key.targetKey,
      targetKind: key.targetKind,
      filePath: file.path,
      lines: toPatchLines(file.lines, reversed),
    }, resolveRef.current(key.repoPath))
      .then((matched) => { if (!cancelled) setStatuses(Object.fromEntries(matched.map((status) => [status.comment_id, status]))); })
      .catch(() => { if (!cancelled) setStatuses({}); });
    return () => { cancelled = true; };
  }, [identityKey, file?.path, file?.lines, reversed, comments]);

  async function refresh(current: ReviewKey) {
    try {
      setComments(await call<ReviewComment[]>("list_comments", { repoPath: current.repoPath, baseSha: current.baseSha, targetKey: current.targetKey, targetKind: current.targetKind }, resolveRef.current(current.repoPath)));
    } catch { /* keep the last listing; the next navigation reloads */ }
  }
  const threads = useMemo(() => commentThreads(comments), [comments]);
  return {
    key,
    threads,
    visibleThreads: filterThreadsByAuthor(threads, author),
    statuses,
    author,
    setAuthor,
    composer,
    openComposer(kind: "review") { setComposer({ kind }); },
    openFileComposer(filePath: string) { setComposer({ kind: "file", filePath }); },
    closeComposer() { setComposer(null); },
    async refresh() { if (key) await refresh(key); },
    async create(draft: CommentDraft) {
      if (!key) return;
      await call("create_comment", { repoPath: key.repoPath, baseSha: key.baseSha, targetKey: key.targetKey, targetKind: key.targetKind, draft }, resolveRef.current(key.repoPath));
      await refresh(key);
    },
    async reply(parentId: number, body: string) {
      if (!key) return;
      await call("reply_comment", { parentId, body }, resolveRef.current(key.repoPath));
      if (key) await refresh(key);
    },
    async setResolved(commentId: number, resolved: boolean) {
      if (!key) return;
      await call("set_comment_resolved", { commentId, resolved }, resolveRef.current(key.repoPath));
      if (key) await refresh(key);
    },
    async edit(commentId: number, body: string) {
      if (!key) return;
      await call("edit_comment", { commentId, body }, resolveRef.current(key.repoPath));
      if (key) await refresh(key);
    },
    async remove(commentId: number) {
      if (!key) return;
      await call("delete_comment", { commentId }, resolveRef.current(key.repoPath));
      if (key) await refresh(key);
    },
  };
}

function severityLabel(severity: string) {
  return `severity ${severity}`;
}

// Copy feedback shared by the per-card and copy-all buttons; the check
// mark outlives the copy by a beat so the click reads as done.
function useCopied() {
  const [copied, setCopied] = useState(false);
  function copy(text: string) {
    void copyText(text).then((ok) => {
      if (!ok) return;
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    });
  }
  return { copied, copy };
}

function CopyMark({ copied }: { copied: boolean }) {
  return copied ? <Check size={12} /> : <Copy size={12} />;
}

function CommentCard({ comment, status, reversed = false, actions }: { comment: ReviewComment; status: AnchorStatus | null; reversed?: boolean; actions?: ReactNode }) {
  const state = comment.parent_id === null ? status?.state ?? null : null;
  const { copied, copy } = useCopied();
  return <article className={`comment-card comment-state-${state ?? "none"} ${comment.resolved_at !== null ? "comment-resolved" : ""}`}>
    <header className="comment-head">
      <span className="comment-author" title={comment.author_name}>{comment.author_name}</span>
      {comment.author_kind === "agent" && <span className="comment-badge">agent</span>}
      {comment.author_model && <span className="comment-model" title={comment.author_model}>{comment.author_model}</span>}
      {comment.severity && <span className={`comment-badge comment-severity severity-${comment.severity}`} title={severityLabel(comment.severity)}>{comment.severity}</span>}
      {state === "moved" && <span className="comment-badge comment-state-badge">moved</span>}
      {state === "outdated" && <span className="comment-badge comment-state-badge">outdated</span>}
      {comment.resolved_at !== null && <span className="comment-badge comment-resolved-badge">resolved</span>}
      <button className="comment-copy" type="button" aria-label="Copy comment as markdown" title="Copy as markdown" onClick={() => copy(formatCommentForCopy(comment, reversed))}>{<CopyMark copied={copied} />}</button>
    </header>
    <CommentBody text={comment.body} />
    {state === "outdated" && comment.snippet && <pre className="comment-snippet"><code>{comment.snippet}</code></pre>}
    <div className="comment-foot">
      {actions}
      <span className="comment-time" title={new Date(comment.created_at).toLocaleString()}>{new Date(comment.created_at).toLocaleDateString()}</span>
    </div>
  </article>;
}

function MiniComposer({ placeholder, submitLabel, busy, initialBody = "", footerExtra, onSubmit, onCancel }: { placeholder: string; submitLabel: string; busy: boolean; initialBody?: string; footerExtra?: ReactNode; onSubmit: (body: string) => void; onCancel: () => void }) {
  const [body, setBody] = useState(initialBody);
  return <div className="comment-composer">
    <MarkdownComposer value={body} onChange={setBody} placeholder={placeholder} autoFocus onSubmit={() => { if (body.trim() !== "") onSubmit(body); }} onCancel={onCancel} />
    <div className="comment-composer-actions">
      {footerExtra}
      <button className="secondary-button" type="button" onClick={onCancel}>Cancel</button>
      <button className="primary-button" type="button" title={`${submitLabel} (Ctrl+Enter)`} aria-label={submitLabel} aria-keyshortcuts="Control+Enter Meta+Enter" disabled={busy || body.trim() === ""} onClick={() => onSubmit(body)}><kbd>Ctrl</kbd><kbd>↵</kbd></button>
    </div>
  </div>;
}

function severityOptions(): Array<CommentSeverity | ""> {
  return ["", "P0", "P1", "P2", "P3"] as Array<CommentSeverity | "">;
}

export function DraftComposer({ placeholder, submitLabel, initialBody = "", onSubmit, onCancel }: { placeholder: string; submitLabel: string; initialBody?: string; onSubmit: (draft: { body: string; severity: CommentSeverity | null }) => void; onCancel: () => void }) {
  const [body, setBody] = useState(initialBody);
  const [severity, setSeverity] = useState<CommentSeverity | "">("");
  return <div className="comment-composer">
    <MarkdownComposer value={body} onChange={setBody} placeholder={placeholder} autoFocus onSubmit={() => { if (body.trim() !== "") onSubmit({ body, severity: severity === "" ? null : severity }); }} onCancel={onCancel} />
    <div className="comment-composer-actions">
      <select className={`severity-select ${severity === "" ? "" : "set"}`} aria-label="Comment priority" title="Priority" value={severity} onChange={(event) => setSeverity(event.currentTarget.value as CommentSeverity | "")}>
        {severityOptions().map((option) => <option key={option || "none"} value={option}>{option === "" ? "Priority" : option}</option>)}
      </select>
      <button className="secondary-button" type="button" onClick={onCancel}>Cancel</button>
      <button className="primary-button" type="button" title={`${submitLabel} (Ctrl+Enter)`} aria-label={submitLabel} aria-keyshortcuts="Control+Enter Meta+Enter" disabled={body.trim() === ""} onClick={() => onSubmit({ body, severity: severity === "" ? null : severity })}><kbd>Ctrl</kbd><kbd>↵</kbd></button>
    </div>
  </div>;
}

// A thread with its actions: resolve/reopen on the root only, flat replies,
// an inline reply composer, and last-write-wins editing. Exported for the
// inline cards the patch panes render at anchored rows. An open-anchor
// handler turns the root's anchor label into a jump-to-diff button; inline
// cards omit it because they already sit in the diff.
export function CommentThreadView({ thread, status, comments, reversed = false, onOpenAnchor }: { thread: CommentThread; status: AnchorStatus | null; comments: CommentsApi; reversed?: boolean; onOpenAnchor?: (comment: ReviewComment) => void }) {
  const [replying, setReplying] = useState(false);
  const [editing, setEditing] = useState<number | null>(null);
  const root = thread.comment;
  const resolved = root.resolved_at !== null;
  const editControls = (comment: ReviewComment) => editing === comment.id
    ? <MiniComposer
      placeholder="Edit comment"
      submitLabel="Save"
      busy={false}
      initialBody={comment.body}
      footerExtra={<button className="danger-button" type="button" onClick={() => { setEditing(null); void comments.remove(comment.id); }}><Trash2 size={12} /> Delete</button>}
      onSubmit={(body) => { setEditing(null); void comments.edit(comment.id, body); }}
      onCancel={() => setEditing(null)}
    />
    : <div className="comment-actions">
      <button type="button" onClick={() => setEditing(comment.id)}>Edit</button>
    </div>;
  const anchor = <code>{anchorLabel(root, reversed)}</code>;
  return <div className="comment-thread" data-comment-id={root.id}>
    <CommentCard comment={root} status={status} reversed={reversed} actions={<>
      <div className="comment-actions">
        {root.file_path !== null && (onOpenAnchor
          ? <button className="comment-anchor" type="button" title={`Open ${anchorLabel(root, reversed)} in the diff`} onClick={() => onOpenAnchor(root)}>{anchor}</button>
          : <span className="comment-anchor" title={`${root.side === "LEFT" ? "Old" : "New"} side of ${root.file_path}`}>{anchor}</span>)}
        <button type="button" onClick={() => void comments.setResolved(root.id, !resolved)}>{resolved ? "Reopen" : "Resolve"}</button>
        <button type="button" onClick={() => setReplying(!replying)}>{replying ? "Cancel" : "Reply"}</button>
      </div>
      {editControls(root)}
    </>} />
    {thread.replies.map((reply) => <CommentCard key={reply.id} comment={reply} status={null} reversed={reversed} actions={editControls(reply)} />)}
    {replying && <MiniComposer placeholder="Reply" submitLabel="Reply" busy={false} onSubmit={(body) => { setReplying(false); void comments.reply(root.id, body); }} onCancel={() => setReplying(false)} />}
  </div>;
}

export function CommentStream({ comments, reversed = false, strip, onCollapse, wide = false, onToggleWide, onOpenAnchor }: { comments: CommentsApi; reversed?: boolean; strip?: ReactNode; onCollapse?: () => void; wide?: boolean; onToggleWide?: () => void; onOpenAnchor?: (comment: ReviewComment) => void }) {
  const { copied, copy } = useCopied();
  if (!comments.key) return null;
  return <aside className="comment-stream" aria-label="Comments">
    <div className="pane-heading">
      <span className="pane-heading-tools">
        {onCollapse && <button className="comment-copy" type="button" aria-label="Hide Comments" title="Hide Comments" onClick={onCollapse}><PanelRightClose size={12} /></button>}
        {onToggleWide && <button className="comment-copy" type="button" aria-label={wide ? "Narrow Comments" : "Widen Comments"} title={wide ? "Narrow Comments" : "Widen Comments"} onClick={onToggleWide}>{wide ? <FoldHorizontal size={12} /> : <UnfoldHorizontal size={12} />}</button>}
      </span>
      <span className="pane-heading-actions">
        <strong>Comments</strong>
        <span>{comments.threads.length}</span>
        <button className="comment-copy" type="button" aria-label="Copy all comments as markdown" title="Copy all comments as markdown" disabled={comments.visibleThreads.length === 0} onClick={() => copy(exportThreadsMarkdown(comments.visibleThreads, reversed))}>{<CopyMark copied={copied} />}</button>
      </span>
    </div>
    <div className="comment-stream-body">
      {strip}
      <div className="comment-filter" role="group" aria-label="Filter comments by author">
        {(["all", "human", "agent"] as const).map((option) => <button key={option} type="button" className={comments.author === option ? "active" : ""} onClick={() => comments.setAuthor(option)}>{option}</button>)}
      </div>
      {comments.composer?.kind === "review" && <div className="comment-composer-panel">
        <p className="eyebrow">New review comment</p>
        <DraftComposer placeholder="Summary, question, or finding…" submitLabel="Comment" onSubmit={({ body, severity }) => { void comments.create({ body, severity, file_path: null, side: null, start_line: null, end_line: null, lines: [] }).then(comments.closeComposer); }} onCancel={comments.closeComposer} />
      </div>}
      {comments.visibleThreads.length === 0
        ? <div className="comment-empty">No comments{comments.author === "all" ? " yet" : ` from ${comments.author} authors`}. Click a diff line to comment; shift-click or drag the line numbers for a range.</div>
        : comments.visibleThreads.map((thread) => <CommentThreadView key={thread.comment.id} thread={thread} status={comments.statuses[thread.comment.id] ?? null} comments={comments} reversed={reversed} onOpenAnchor={onOpenAnchor} />)}
    </div>
  </aside>;
}

// Selection-driven inline composer: rendered by the patch panes right under
// the selected row and converted to a logical-side draft on submit. A text
// selection captured when the composer opened rides along as a quoted
// excerpt pre-filled above the comment body.
export function InlineCommentComposer({ selection, filePath, lines, reversed, comments, onDone }: { selection: CommentSelection; filePath: string; lines: DiffLine[]; reversed: boolean; comments: CommentsApi; onDone: () => void }) {
  const range = selection.start === selection.end ? `line ${selection.start}` : `lines ${selection.start}-${selection.end}`;
  return <div className="comment-composer-panel inline">
    <p className="eyebrow">Comment on {filePath} {range}</p>
    <DraftComposer
      placeholder="Add comment"
      submitLabel="Comment"
      initialBody={quoteExcerpt(selection.excerpt ?? "")}
      onSubmit={({ body, severity }) => {
        const draft = draftFromSelection(selection, lines, reversed, filePath);
        if (!draft) return;
        void comments.create({ ...draft, body, severity }).then(() => { comments.closeComposer(); onDone(); });
      }}
      onCancel={() => { comments.closeComposer(); onDone(); }}
    />
  </div>;
}

// Unified diff rows select their marker's side: deletions are display LEFT,
// everything else (additions and context) is display RIGHT; metadata rows
// are not selectable.
export function selectableRow(line: DiffLine): { side: DisplaySide; number: number } | null {
  if (line.text.startsWith("-") && line.oldLine !== null) return { side: "LEFT", number: line.oldLine };
  if (line.newLine !== null) return { side: "RIGHT", number: line.newLine };
  if (line.oldLine !== null) return { side: "LEFT", number: line.oldLine };
  return null;
}

// Inline comment cards for one rendered file, keyed by display side and
// line: current comments render at the anchor's last line, moved comments
// at their re-anchored position, and outdated ones only in the stream.
export function inlineCards(comments: CommentsApi, filePath: string, reversed: boolean): Map<string, Array<{ thread: CommentThread; state: "current" | "moved" }>> {
  const cards = new Map<string, Array<{ thread: CommentThread; state: "current" | "moved" }>>();
  if (!comments.key) return cards;
  for (const thread of comments.threads) {
    if (thread.comment.file_path !== filePath) continue;
    const place = inlinePlacement(thread.comment, comments.statuses[thread.comment.id] ?? null, reversed);
    if (!place) continue;
    const key = `${place.side}:${place.line}`;
    const existing = cards.get(key) ?? [];
    existing.push({ thread, state: place.state });
    cards.set(key, existing);
  }
  return cards;
}
