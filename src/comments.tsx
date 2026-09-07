import { useEffect, useMemo, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { DiffLine } from "./diff.ts";
import type { ReviewIdentity } from "./navigation.ts";
import { CommentBody } from "./markdown.tsx";
import {
  commentThreads,
  draftFromSelection,
  filterThreadsByAuthor,
  inlinePlacement,
  reviewKeyOf,
  toPatchLines,
  type AnchorStatus,
  type AuthorFilter,
  type CommentDraft,
  type CommentSeverity,
  type CommentThread,
  type DisplaySide,
  type ReviewComment,
  type ReviewIndexSummary,
  type ReviewKey,
  type RowSelection,
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
  create(draft: CommentDraft): Promise<void>;
  reply(parentId: number, body: string): Promise<void>;
  setResolved(commentId: number, resolved: boolean): Promise<void>;
  edit(commentId: number, body: string): Promise<void>;
};

export function useReviewComments(identity: ReviewIdentity | null, index: ReviewIndexSummary | null, file: { path: string; lines: DiffLine[] } | null, reversed: boolean): CommentsApi {
  const key = useMemo(() => (identity && index ? reviewKeyOf(identity, index) : null), [identity, index]);
  const [comments, setComments] = useState<ReviewComment[]>([]);
  const [statuses, setStatuses] = useState<Record<number, AnchorStatus>>({});
  const [author, setAuthor] = useState<AuthorFilter>("all");
  const [composer, setComposer] = useState<ComposerState>(null);
  const identityKey = key ? `${key.repoPath}\0${key.baseSha}\0${key.targetKey}\0${key.targetKind}` : "";

  useEffect(() => {
    if (!key) {
      setComments([]);
      setComposer(null);
      return;
    }
    let cancelled = false;
    setComments([]);
    setComposer(null);
    void invoke<ReviewComment[]>("list_comments", { repoPath: key.repoPath, baseSha: key.baseSha, targetKey: key.targetKey, targetKind: key.targetKind })
      .then((loaded) => { if (!cancelled) setComments(loaded); })
      .catch(() => { if (!cancelled) setComments([]); });
    return () => { cancelled = true; };
  }, [identityKey]);

  // One batched match per loaded file: the patch's parsed lines go to the
  // Rust matcher, which returns every comment's status in a single call.
  useEffect(() => {
    if (!key || !file || file.lines.length === 0) {
      setStatuses({});
      return;
    }
    let cancelled = false;
    void invoke<AnchorStatus[]>("match_comment_anchors", {
      repoPath: key.repoPath,
      baseSha: key.baseSha,
      targetKey: key.targetKey,
      targetKind: key.targetKind,
      filePath: file.path,
      lines: toPatchLines(file.lines, reversed),
    })
      .then((matched) => { if (!cancelled) setStatuses(Object.fromEntries(matched.map((status) => [status.comment_id, status]))); })
      .catch(() => { if (!cancelled) setStatuses({}); });
    return () => { cancelled = true; };
  }, [identityKey, file?.path, file?.lines, reversed, comments]);

  async function refresh(current: ReviewKey) {
    try {
      setComments(await invoke<ReviewComment[]>("list_comments", { repoPath: current.repoPath, baseSha: current.baseSha, targetKey: current.targetKey, targetKind: current.targetKind }));
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
    async create(draft: CommentDraft) {
      if (!key) return;
      await invoke("create_comment", { repoPath: key.repoPath, baseSha: key.baseSha, targetKey: key.targetKey, targetKind: key.targetKind, draft });
      await refresh(key);
    },
    async reply(parentId: number, body: string) {
      await invoke("reply_comment", { parentId, body });
      if (key) await refresh(key);
    },
    async setResolved(commentId: number, resolved: boolean) {
      await invoke("set_comment_resolved", { commentId, resolved });
      if (key) await refresh(key);
    },
    async edit(commentId: number, body: string) {
      await invoke("edit_comment", { commentId, body });
      if (key) await refresh(key);
    },
  };
}

function severityLabel(severity: string) {
  return `severity ${severity}`;
}

function CommentCard({ comment, status, actions }: { comment: ReviewComment; status: AnchorStatus | null; actions?: ReactNode }) {
  const state = comment.parent_id === null ? status?.state ?? null : null;
  return <article className={`comment-card comment-state-${state ?? "none"} ${comment.resolved_at !== null ? "comment-resolved" : ""}`}>
    <header className="comment-head">
      <span className="comment-author">{comment.author_name}</span>
      {comment.author_kind === "agent" && <span className="comment-badge">agent</span>}
      {comment.severity && <span className={`comment-badge comment-severity severity-${comment.severity}`} title={severityLabel(comment.severity)}>{comment.severity}</span>}
      {state === "moved" && <span className="comment-badge comment-state-badge">moved</span>}
      {state === "outdated" && <span className="comment-badge comment-state-badge">outdated</span>}
      {comment.resolved_at !== null && <span className="comment-badge comment-resolved-badge">resolved</span>}
      <span className="comment-time" title={new Date(comment.created_at).toLocaleString()}>{new Date(comment.created_at).toLocaleDateString()}</span>
    </header>
    <CommentBody text={comment.body} />
    {state === "outdated" && comment.snippet && <pre className="comment-snippet"><code>{comment.snippet}</code></pre>}
    {actions}
  </article>;
}

function MiniComposer({ placeholder, submitLabel, busy, onSubmit, onCancel }: { placeholder: string; submitLabel: string; busy: boolean; onSubmit: (body: string) => void; onCancel: () => void }) {
  const [body, setBody] = useState("");
  return <div className="comment-composer">
    <textarea aria-label={placeholder} placeholder={placeholder} value={body} rows={3} onChange={(event) => setBody(event.currentTarget.value)} />
    <div className="comment-composer-actions">
      <button className="primary-button" type="button" disabled={busy || body.trim() === ""} onClick={() => onSubmit(body)}>{submitLabel}</button>
      <button className="secondary-button" type="button" onClick={onCancel}>Cancel</button>
    </div>
  </div>;
}

function severityOptions(): Array<CommentSeverity | ""> {
  return ["", "P0", "P1", "P2", "P3"] as Array<CommentSeverity | "">;
}

export function DraftComposer({ placeholder, submitLabel, onSubmit, onCancel }: { placeholder: string; submitLabel: string; onSubmit: (draft: { body: string; severity: CommentSeverity | null }) => void; onCancel: () => void }) {
  const [body, setBody] = useState("");
  const [severity, setSeverity] = useState<CommentSeverity | "">("");
  return <div className="comment-composer">
    <textarea aria-label={placeholder} placeholder={placeholder} value={body} rows={3} onChange={(event) => setBody(event.currentTarget.value)} />
    <div className="comment-composer-actions">
      <select aria-label="Comment severity" value={severity} onChange={(event) => setSeverity(event.currentTarget.value as CommentSeverity | "")}>
        {severityOptions().map((option) => <option key={option || "none"} value={option}>{option === "" ? "No severity" : option}</option>)}
      </select>
      <button className="primary-button" type="button" disabled={body.trim() === ""} onClick={() => onSubmit({ body, severity: severity === "" ? null : severity })}>{submitLabel}</button>
      <button className="secondary-button" type="button" onClick={onCancel}>Cancel</button>
    </div>
  </div>;
}

// A thread with its actions: resolve/reopen on the root only, flat replies,
// an inline reply composer, and last-write-wins editing. Exported for the
// inline cards the patch panes render at anchored rows.
export function CommentThreadView({ thread, status, comments }: { thread: CommentThread; status: AnchorStatus | null; comments: CommentsApi }) {
  const [replying, setReplying] = useState(false);
  const [editing, setEditing] = useState<number | null>(null);
  const root = thread.comment;
  const resolved = root.resolved_at !== null;
  const editControls = (comment: ReviewComment) => editing === comment.id
    ? <MiniComposer placeholder="Edit comment" submitLabel="Save" busy={false} onSubmit={(body) => { setEditing(null); void comments.edit(comment.id, body); }} onCancel={() => setEditing(null)} />
    : <div className="comment-actions">
      <button type="button" onClick={() => setEditing(comment.id)}>Edit</button>
    </div>;
  return <div className="comment-thread" data-comment-id={root.id}>
    <CommentCard comment={root} status={status} actions={<>
      <div className="comment-actions">
        {root.file_path !== null && <span className="comment-anchor"><code>{root.file_path}</code>{root.side !== null && root.start_line !== null && <span> {root.side} {root.start_line}{root.end_line !== null && root.end_line !== root.start_line ? `-${root.end_line}` : ""}</span>}</span>}
        <button type="button" onClick={() => void comments.setResolved(root.id, !resolved)}>{resolved ? "Reopen" : "Resolve"}</button>
        <button type="button" onClick={() => setReplying(!replying)}>{replying ? "Cancel" : "Reply"}</button>
      </div>
      {editControls(root)}
    </>} />
    {thread.replies.map((reply) => <CommentCard key={reply.id} comment={reply} status={null} actions={editControls(reply)} />)}
    {replying && <MiniComposer placeholder="Reply" submitLabel="Reply" busy={false} onSubmit={(body) => { setReplying(false); void comments.reply(root.id, body); }} onCancel={() => setReplying(false)} />}
  </div>;
}

export function CommentStream({ comments, strip }: { comments: CommentsApi; strip?: ReactNode }) {
  if (!comments.key) return null;
  return <aside className="comment-stream" aria-label="Comments">
    <div className="pane-heading"><strong>Comments</strong><span>{comments.threads.length}</span></div>
    <div className="comment-stream-body">
      {strip}
      <div className="comment-filter" role="group" aria-label="Filter comments by author">
        {(["all", "human", "agent"] as const).map((option) => <button key={option} type="button" className={comments.author === option ? "active" : ""} onClick={() => comments.setAuthor(option)}>{option}</button>)}
      </div>
      {comments.composer?.kind === "review" && <div className="comment-composer-panel">
        <p className="eyebrow">New review comment</p>
        <DraftComposer placeholder="Comment on this review" submitLabel="Comment" onSubmit={({ body, severity }) => { void comments.create({ body, severity, file_path: null, side: null, start_line: null, end_line: null, lines: [] }).then(comments.closeComposer); }} onCancel={comments.closeComposer} />
      </div>}
      {comments.visibleThreads.length === 0
        ? <div className="comment-empty">No comments{comments.author === "all" ? " yet" : ` from ${comments.author} authors`}. Select a diff line or use a comment button.</div>
        : comments.visibleThreads.map((thread) => <CommentThreadView key={thread.comment.id} thread={thread} status={comments.statuses[thread.comment.id] ?? null} comments={comments} />)}
    </div>
  </aside>;
}

// Selection-driven inline composer: rendered by the patch panes right under
// the selected row and converted to a logical-side draft on submit.
export function InlineCommentComposer({ selection, filePath, lines, reversed, comments, onDone }: { selection: RowSelection; filePath: string; lines: DiffLine[]; reversed: boolean; comments: CommentsApi; onDone: () => void }) {
  return <div className="comment-composer-panel inline">
    <p className="eyebrow">Comment on {filePath} {selection.start === selection.end ? `line ${selection.start}` : `lines ${selection.start}-${selection.end}`}</p>
    <DraftComposer
      placeholder="Add comment"
      submitLabel="Comment"
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
