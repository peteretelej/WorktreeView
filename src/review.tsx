import { useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ArrowLeft, ArrowLeftRight, ArrowRight, ChevronDown, ChevronRight, ChevronUp, ChevronsDownUp, ChevronsUpDown, CircleDot, Code, Columns2, ExternalLink, FileDiff, FileWarning, FolderOpen, GitBranch, MessageSquare, PanelLeftClose, PanelLeftOpen, PanelRightOpen, RefreshCw, Search, Space, UnfoldVertical, WrapText } from "lucide-react";
import { call, type SourceResolver } from "./remote.ts";
import { sameReviewTarget, type ChangedFile, type CommitDetail, type RefInventory, type ReviewIdentity, type ReviewScope, type ReviewTarget, type Worktree } from "./navigation";
import { allTreeDirPaths, buildFileTree, filterChangedFiles, flattenFileTree, splitFilePath } from "./fileTree";
import { workingChangesBase, type WorktreeReviewPreset } from "./reviewPresets";
import { listAgentTokens, type AgentToken, type ChangedFilesView, type DiffLayout, type Settings } from "./settings";
import { changeRegions, deletionTicks, hunksWithExpandedGaps, parseHunkHeader, patchGaps, splitFileLines, type ChangeRegion, type DiffLine, type PatchGap } from "./diff";
import { buildFileRows, buildPatchRows, imageMimeForPath, MAX_RENDERED_ROWS, type RowSpec } from "./stream";
import { hunkSideSources, languageForPath, splitWhitespace, tokenizeHunk, type HighlightToken, type TokenLine } from "./highlight";
import { reviewFileRoot } from "./surfaces";
import { canReRequest, canVerdict, canWithdraw, requestStatusLabel, validateRequestForm, REQUEST_LENS_OPTIONS, REQUEST_NOTE_LIMIT, REQUEST_ROUNDS, type RequestAction, type RequestChange, type RequestLens, type ReviewRequestRow } from "./requests.ts";
import { CommentStream, CommentThreadView, DraftComposer, InlineCommentComposer, inlineCards, selectableRow, type CommentsApi } from "./comments.tsx";
import { CommentBody } from "./markdown.tsx";
import { ReviewsStrip } from "./canvas.tsx";
import { errorMessage, shortToken } from "./format";
import { unwrapLoad, type RemoteLoad } from "./remoteLoads";
import { CopyButton, Empty, Pager } from "./ui";
import type { CommentSelection, DisplaySide, ReviewComment, ReviewKey } from "./comments";
import { commentJumpTarget } from "./comments";

export const BRANCH_PAGE_SIZE = 50;

export const STREAM_TOP_PADDING = 12;
export const FILE_TOKEN_CHUNK_LINES = 1000;
// A stream-card anchor click or a portal-thread focus lands the patch on
// one row; the nonce lets repeated jumps to the same row retrigger.
export type AnchorJump = { filePath: string; side: DisplaySide; line: number; nonce: number };

// Producer half of the anchor-jump protocol shared by the review and
// history surfaces: selects the anchored file (preferring its row in the
// review index so untracked files keep their identity) and records where
// PatchPane should land. File-level comments only open the file.
export function useAnchorJump(reversed: boolean, selectedFile: ChangedFile | null, files: ChangedFile[], onFile: (file: ChangedFile) => void, comments: CommentsApi): { anchorJump: AnchorJump | null; openCommentAnchor: (comment: ReviewComment) => void } {
  const [anchorJump, setAnchorJump] = useState<AnchorJump | null>(null);
  function openCommentAnchor(comment: ReviewComment) {
    if (comment.file_path === null) return;
    if (selectedFile?.path !== comment.file_path) {
      const known = files.find((file) => file.path === comment.file_path);
      onFile(known ?? { path: comment.file_path, status: "", untracked: false });
    }
    const target = commentJumpTarget(comment, comments.statuses[comment.id] ?? null, reversed);
    if (!target) return;
    const filePath = comment.file_path;
    setAnchorJump((current) => ({ filePath, side: target.side, line: target.line, nonce: (current?.nonce ?? 0) + 1 }));
  }
  return { anchorJump, openCommentAnchor };
}
export type ReviewIndex = { files: ChangedFile[]; additions: number; deletions: number; base_sha: string; target_sha: string; error?: string; error_code?: string };
export type FilePatch = { binary: boolean; text: string };
export type FileContent = { binary: boolean; text: string };
type ParsedHunk = { header: string; lines: DiffLine[] };
export type DiffPreferences = { layout: DiffLayout; whitespaceVisible: boolean; lineWrap: boolean; syntaxVisible: boolean; inlineCommentsVisible: boolean };
export function parseHunks(text: string) {
  const lines = text.split("\n");
  const hunks: ParsedHunk[] = [];
  let current: ParsedHunk | null = null;
  let oldLine = 0;
  let newLine = 0;
  for (const line of lines) {
    const span = parseHunkHeader(line);
    if (span) {
      current = { header: line, lines: [] };
      hunks.push(current);
      oldLine = span.oldStart;
      newLine = span.newStart;
    } else if (current && !(line === "" && lines[lines.length - 1] === line)) {
      if (line.startsWith(" ")) {
        current.lines.push({ text: line, oldLine, newLine });
        oldLine += 1;
        newLine += 1;
      } else if (line.startsWith("-")) {
        current.lines.push({ text: line, oldLine, newLine: null });
        oldLine += 1;
      } else if (line.startsWith("+")) {
        current.lines.push({ text: line, oldLine: null, newLine });
        newLine += 1;
      } else if (line.startsWith("\\")) {
        current.lines.push({ text: line, oldLine: null, newLine: null });
      }
    }
  }
  return hunks;
}

// A DOM text selection mapped onto display rows. Commentable rows carry
// side/line data attributes; selections spanning both diff sides (split
// layout) or landing on non-row content map to nothing.
type TextSelectionRange = { displaySide: DisplaySide; start: number; end: number; text: string };

// The pane's in-progress row selection keeps the anchor/focus form; the
// composer receives it normalized to start/end with any excerpt attached.
type PaneSelection = { displaySide: DisplaySide; anchor: number; focus: number; excerpt?: string };

// The excerpt is read from the range rather than selection.toString():
// the floating Comment chip is inserted inside the end row, so the live
// range can expand over its label and toString() would quote it. Rows are
// the excerpt's line units; chip text never counts as selected.
function rangeExcerpt(range: Range): string {
  const slice = (node: Text): string => {
    let value = node.nodeValue ?? "";
    if (node === range.startContainer) value = value.slice(range.startOffset);
    if (node === range.endContainer) value = value.slice(0, node === range.startContainer ? range.endOffset - range.startOffset : range.endOffset);
    return value;
  };
  const root = range.commonAncestorContainer;
  if (root.nodeType === Node.TEXT_NODE) return slice(root as Text);
  const texts: string[] = [];
  let lastRow: Element | null = null;
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    if (!range.intersectsNode(node)) continue;
    if (node.parentElement?.closest(".selection-comment-chip")) continue;
    const row = node.parentElement?.closest(".diff-line") ?? null;
    if (row !== null && row !== lastRow && texts.length > 0) texts.push("\n");
    texts.push(slice(node as Text));
    lastRow = row;
  }
  return texts.join("");
}

function textSelectionRange(pane: HTMLElement | null): TextSelectionRange | null {
  const selection = document.getSelection();
  if (!pane || !selection || selection.isCollapsed || selection.rangeCount === 0) return null;
  const range = selection.getRangeAt(0);
  if (!pane.contains(range.commonAncestorContainer)) return null;
  const rowOf = (node: Node) => (node instanceof Element ? node : node.parentElement)?.closest<HTMLElement>(".diff-line[data-side]");
  const startRow = rowOf(range.startContainer);
  const endRow = rowOf(range.endContainer);
  if (!startRow || !endRow || startRow.dataset.side === undefined || startRow.dataset.side !== endRow.dataset.side) return null;
  const start = Number(startRow.dataset.line);
  const end = Number(endRow.dataset.line);
  if (!Number.isFinite(start) || !Number.isFinite(end)) return null;
  return { displaySide: startRow.dataset.side as DisplaySide, start: Math.min(start, end), end: Math.max(start, end), text: rangeExcerpt(range).slice(0, 2000) };
}

// Renders diff line text with visible whitespace marks when enabled; when
// disabled it returns the raw text so rendering stays byte-identical.
// Highlighted lines render the diff marker plus token spans instead.
function diffLineContent(text: string, whitespaceVisible: boolean, tokens?: TokenLine): React.ReactNode {
  if (tokens && tokens.length > 0) return [text.slice(0, 1), ...tokenLineNodes(tokens, whitespaceVisible)];
  if (!whitespaceVisible) return text || " ";
  const { parts } = splitWhitespace(text, true, 0);
  if (parts.length === 0) return " ";
  if (parts.length === 1 && parts[0].text !== undefined) return parts[0].text;
  return parts.map((part, index) => part.glyph !== undefined
    ? <span key={index} className="whitespace-glyph">{part.glyph}</span>
    : part.text);
}

function tokenLineNodes(tokens: TokenLine, whitespaceVisible: boolean): React.ReactNode[] {
  const nodes: React.ReactNode[] = [];
  let column = 1;
  tokens.forEach((token, index) => {
    if (!token.content) return;
    const key = `tk${index}`;
    const className = tokenClassName(token);
    const style = tokenStyle(token);
    const atLineEnd = index === tokens.length - 1;
    if (!whitespaceVisible) {
      nodes.push(<span key={key} className={className} style={style}>{token.content}</span>);
      return;
    }
    const { parts, endColumn } = splitWhitespace(token.content, atLineEnd, column);
    column = endColumn;
    parts.forEach((part, partIndex) => {
      const partKey = `${key}-${partIndex}`;
      if (part.glyph !== undefined) nodes.push(<span key={partKey} className="whitespace-glyph">{part.glyph}</span>);
      else nodes.push(<span key={partKey} className={className} style={style}>{part.text}</span>);
    });
  });
  return nodes;
}

function tokenClassName(token: HighlightToken) {
  let className = "shiki-token";
  if (token.italic) className += " italic";
  if (token.bold) className += " bold";
  if (token.underline) className += " underline";
  return className;
}

function tokenStyle(token: HighlightToken): React.CSSProperties | undefined {
  if (!token.light && !token.dark) return undefined;
  return { "--shiki-light": token.light ?? token.dark, "--shiki-dark": token.dark ?? token.light } as React.CSSProperties;
}

export function sameReview(left: ReviewIdentity | null, right: ReviewIdentity) {
  return left !== null
    && left.repoPath === right.repoPath
    && left.base === right.base
    && sameReviewTarget(left.target, right.target)
    && left.scope === right.scope
    && left.reversed === right.reversed;
}

export function patchIdentityOf(identity: ReviewIdentity, file: ChangedFile) {
  const target = identity.target;
  return `${identity.repoPath}\0${target.kind === "ref" ? target.name : target.kind === "commit" ? target.sha : target.worktree.path}\0${identity.base}\0${identity.scope}\0${identity.reversed}\0${file.path}\0${file.untracked}`;
}
const HASH_QUERY = /^[0-9a-f]{4,40}$/i;

export function RefPicker({ id, label, repoPath, refs, value, onChange, onCommitPick, exclude = [], resolve = () => ({ kind: "local" }) }: { id: string; label: string; repoPath: string; refs: string[]; value: string; onChange: (value: string) => void; onCommitPick?: (detail: CommitDetail) => void; exclude?: string[]; resolve?: SourceResolver }) {
  const [query, setQuery] = useState("");
  const [open, setOpen] = useState(false);
  const [page, setPage] = useState(0);
  const [commit, setCommit] = useState<CommitDetail | null>(null);
  const hashQuery = HASH_QUERY.test(query.trim());
  useEffect(() => {
    setCommit(null);
    if (!hashQuery) return;
    let cancelled = false;
    const timer = setTimeout(() => {
      call<CommitDetail | RemoteLoad<CommitDetail>>("describe_commit", { path: repoPath, rev: query.trim() }, resolve(repoPath))
        .then((payload) => { if (!cancelled) setCommit(unwrapLoad(payload).data); })
        .catch(() => { if (!cancelled) setCommit(null); });
    }, 250);
    return () => { cancelled = true; clearTimeout(timer); };
  }, [query, repoPath, hashQuery]);
  const search = query.trim().toLowerCase();
  const options = refs.filter((ref) => !exclude.includes(ref));
  const matches = search ? options.filter((ref) => ref.toLowerCase().includes(search)) : options;
  const pages = Math.max(1, Math.ceil(matches.length / BRANCH_PAGE_SIZE));
  const visiblePage = Math.min(page, pages - 1);
  const visibleRefs = matches.slice(visiblePage * BRANCH_PAGE_SIZE, (visiblePage + 1) * BRANCH_PAGE_SIZE);
  useEffect(() => { setQuery(""); setPage(0); }, [value]);
  // The options list closes on any click outside the picker, so two pickers
  // never hold their lists open at once and plain clicks elsewhere never
  // need an Escape first.
  const rootRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (!open) return;
    function onOutside(event: MouseEvent) {
      if (rootRef.current && event.target instanceof Node && !rootRef.current.contains(event.target)) setOpen(false);
    }
    document.addEventListener("mousedown", onOutside);
    return () => document.removeEventListener("mousedown", onOutside);
  }, [open]);
  return <div className="branch-picker" ref={rootRef}><label htmlFor={id}>{label}</label><code className="branch-selection">{value ? shortToken(value) : "No base selected"}</code><input id={id} role="combobox" aria-controls={`${id}-options`} aria-expanded={open} aria-autocomplete="list" value={query} placeholder="Search refs" onFocus={() => setOpen(true)} onChange={(event) => { setQuery(event.currentTarget.value); setPage(0); setOpen(true); }} onKeyDown={(event) => { if (event.key === "Escape") setOpen(false); }} />{open && <div className="branch-options" id={`${id}-options`} role="listbox" aria-label={`${label} branches`}>{commit && <button className="commit-option" type="button" role="option" aria-selected={value === commit.sha} title={commit.subject} onClick={() => { if (onCommitPick) onCommitPick(commit); else onChange(commit.sha); setOpen(false); }}><code>{shortToken(commit.sha)}</code><span>{commit.subject}</span></button>}{visibleRefs.map((ref) => <button key={ref} type="button" role="option" aria-selected={ref === value} onClick={() => { onChange(ref); setOpen(false); }}>{shortToken(ref)}</button>)}{matches.length === 0 && !commit && <span>{hashQuery ? "No matching refs or commit" : "No matching refs"}</span>}{pages > 1 && <Pager label={`${label} branch pages`} page={visiblePage} pages={pages} total={matches.length} size={BRANCH_PAGE_SIZE} onPage={setPage} />}</div>}</div>;
}

// Commit surfaces identify by title first: the row keeps the subject always
// visible and expands the body underneath it on demand.
export function useCommitSummary(repoPath: string, sha: string | null, resolve: SourceResolver = () => ({ kind: "local" })) {
  const [detail, setDetail] = useState<CommitDetail | null>(null);
  useEffect(() => {
    setDetail(null);
    if (!sha) return;
    let cancelled = false;
    call<CommitDetail | RemoteLoad<CommitDetail>>("describe_commit", { path: repoPath, rev: sha }, resolve(repoPath))
      .then((payload) => { if (!cancelled) setDetail(unwrapLoad(payload).data); })
      .catch(() => { /* the known subject and short hash still render */ });
    return () => { cancelled = true; };
  }, [repoPath, sha]);
  return detail;
}

export function CommitRow({ title, children }: { title: string; children: React.ReactNode }) {
  const [open, setOpen] = useState(false);
  return <div className="review-commit">
    <button className="commit-toggle" type="button" aria-expanded={open} aria-controls="review-commit-detail" onClick={() => setOpen((value) => !value)}>
      <ChevronRight size={12} className="chev" />
      <span className="commit-title" title={title}>{title}</span>
    </button>
    {open && <div className="commit-detail" id="review-commit-detail">{children}</div>}
  </div>;
}

// The review header's request surface for the open review identity:
// status badges plus human verdict/withdraw/re-request actions and the
// inline request form. Actions act on the local store and are always
// available (no listener or token has to exist), matching the queue's
// store-backed rendering; live updates ride the same
// review-request-changed event the queue follows.
function ReviewRequestBar({ identityKey, headSha }: { identityKey: ReviewKey; headSha: string }) {
  const [rows, setRows] = useState<ReviewRequestRow[]>([]);
  const [formOpen, setFormOpen] = useState(false);
  const [error, setError] = useState("");
  const identityRef = useRef(identityKey);
  useEffect(() => { identityRef.current = identityKey; });
  useEffect(() => {
    setError("");
    setFormOpen(false);
    let cancelled = false;
    void (async () => {
      try {
        const listed = await invoke<ReviewRequestRow[]>("list_requests", { repoPath: identityKey.repoPath, baseSha: identityKey.baseSha, targetKey: identityKey.targetKey, targetKind: identityKey.targetKind });
        if (!cancelled) setRows(listed);
      } catch (caught) {
        if (!cancelled) setError(errorMessage(caught));
      }
    })();
    return () => { cancelled = true; };
  }, [identityKey]);
  // One refetch per matching mutation (agent tool or human command);
  // failures keep the last payload, stale rather than gone.
  useEffect(() => {
    let disposed = false;
    const subscription = listen<RequestChange>("review-request-changed", (event) => {
      if (disposed) return;
      const key = identityRef.current;
      const change = event.payload;
      if (key.repoPath !== change.repo_path || key.baseSha !== change.base_sha || key.targetKey !== change.target_key || key.targetKind !== change.target_kind) return;
      void (async () => {
        try {
          const listed = await invoke<ReviewRequestRow[]>("list_requests", { repoPath: key.repoPath, baseSha: key.baseSha, targetKey: key.targetKey, targetKind: key.targetKind });
          if (!disposed) setRows(listed);
        } catch { /* keep the last payload */ }
      })();
    });
    return () => { disposed = true; void subscription.then((unsubscribe) => unsubscribe()); };
  }, []);
  async function runAction(row: ReviewRequestRow, action: RequestAction, note: string) {
    setError("");
    try {
      const updated = await invoke<ReviewRequestRow>("update_review_request", { id: row.id, action, note: note.trim() || null, headSha: action === "re_request" ? headSha : null });
      setRows((current) => current.map((item) => (item.id === updated.id ? updated : item)));
      return true;
    } catch (caught) {
      setError(errorMessage(caught));
      return false;
    }
  }
  function absorbCreated(created: ReviewRequestRow) {
    setRows((current) => current.some((row) => row.id === created.id) ? current.map((row) => (row.id === created.id ? created : row)) : [created, ...current]);
    setFormOpen(false);
  }
  return <div className="request-surface">
    <div className="request-bar">
      <div className="request-list">
        {rows.length === 0 && <span className="request-empty">No review requests</span>}
        {rows.map((row) => <RequestRowView key={row.id} row={row} headSha={headSha} onAction={runAction} />)}
      </div>
      <div className="request-bar-side">
        {error && <span className="request-error" role="status">{error}</span>}
        <button className="request-action" type="button" aria-expanded={formOpen} onClick={() => setFormOpen((open) => !open)}>{formOpen ? "Close form" : "Request review"}</button>
      </div>
    </div>
    {formOpen && <RequestForm identityKey={identityKey} headSha={headSha} onCreated={absorbCreated} />}
  </div>;
}

function RequestRowView({ row, headSha, onAction }: { row: ReviewRequestRow; headSha: string; onAction: (row: ReviewRequestRow, action: RequestAction, note: string) => Promise<boolean> }) {
  const [note, setNote] = useState("");
  const [armed, setArmed] = useState(false);
  const [noteOpen, setNoteOpen] = useState(false);
  const withdrawArmed = armed && canWithdraw(row.status);
  const actionable = canVerdict(row.status) || canReRequest(row) || canWithdraw(row.status);
  function withdraw() {
    if (withdrawArmed) {
      setArmed(false);
      void onAction(row, "withdraw", "");
      return;
    }
    setArmed(true);
    window.setTimeout(() => setArmed((current) => (current ? false : current)), 4000);
  }
  function act(action: RequestAction) {
    // A refused action keeps the typed note so it can be corrected and
    // retried instead of retyped.
    void onAction(row, action, note).then((done) => { if (done) setNote(""); });
  }
  return <div className="request-row">
    <div className="request-chips">
      <span className="status-chip clean">{requestStatusLabel(row.status)}</span>
      {row.needs_human && <span className="status-chip attention-danger">needs human</span>}
      {row.max_rounds > 0 && <code className="request-round" title={`Round ${row.round} of ${row.max_rounds}`}>{row.round}/{row.max_rounds}</code>}
      <span className="comment-badge" title={`Requested by ${row.requester}`}>{row.requester}</span>
      {row.reviewers.length > 0 && <span className="comment-badge" title={`Named reviewers: ${row.reviewers.join(", ")}`}>{row.reviewers.length === 1 ? row.reviewers[0] : `${row.reviewers.length} reviewers`}</span>}
      {row.note && <button className="request-note-toggle" type="button" aria-expanded={noteOpen} aria-controls={`request-note-detail-${row.id}`} title={row.note} onClick={() => setNoteOpen((open) => !open)}><ChevronRight size={12} className="chev" /><span className="request-note-display">{row.note}</span></button>}
    </div>
    {row.note && noteOpen && <div className="request-note-detail" id={`request-note-detail-${row.id}`}><CommentBody text={row.note} /></div>}
    {actionable && <div className="request-actions">
      <input className="request-note-input" type="text" aria-label={`Optional note for the ${requestStatusLabel(row.status)} request`} placeholder="Optional note" maxLength={REQUEST_NOTE_LIMIT} value={note} onChange={(event) => setNote(event.currentTarget.value)} onKeyDown={(event) => { if ((event.ctrlKey || event.metaKey) && !event.shiftKey && !event.altKey && (event.key === "k" || event.key === "b")) event.stopPropagation(); }} />
      {canVerdict(row.status) && <button className="request-action" type="button" onClick={() => act("approve")}>Approve</button>}
      {canVerdict(row.status) && <button className="request-action" type="button" onClick={() => act("request_changes")}>Request changes</button>}
      {canReRequest(row) && <button className="request-action" type="button" disabled={!headSha} title={headSha ? "Re-request with the displayed head" : "The displayed head is unavailable"} onClick={() => act("re_request")}>Re-request</button>}
      {canWithdraw(row.status) && <button className={`request-action ${withdrawArmed ? "danger" : ""}`} type="button" onClick={withdraw}>{withdrawArmed ? "Confirm withdraw" : "Withdraw"}</button>}
    </div>}
  </div>;
}

function RequestForm({ identityKey, headSha, onCreated }: { identityKey: ReviewKey; headSha: string; onCreated: (row: ReviewRequestRow) => void }) {
  const [note, setNote] = useState("");
  const [lenses, setLenses] = useState<RequestLens[]>([]);
  const [reviewers, setReviewers] = useState<string[]>([]);
  const [maxRounds, setMaxRounds] = useState<number>(REQUEST_ROUNDS.default);
  const [tokens, setTokens] = useState<AgentToken[]>([]);
  const [attempted, setAttempted] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    let cancelled = false;
    listAgentTokens().then((next) => { if (!cancelled) setTokens(next); }).catch(() => { /* open pickup stays available */ });
    return () => { cancelled = true; };
  }, []);
  const errors = validateRequestForm({ note, lenses, max_rounds: maxRounds });
  const blocked = errors.note || errors.lenses || errors.max_rounds || (!headSha ? "The displayed head is unavailable, so a request cannot be recorded." : "");
  const shownError = attempted ? blocked : "";
  function toggleLens(lens: RequestLens) { setLenses((current) => current.includes(lens) ? current.filter((item) => item !== lens) : [...current, lens]); }
  function toggleReviewer(name: string) { setReviewers((current) => current.includes(name) ? current.filter((item) => item !== name) : [...current, name]); }
  async function submit() {
    setAttempted(true);
    if (blocked || submitting) return;
    setSubmitting(true);
    setError("");
    try {
      onCreated(await invoke<ReviewRequestRow>("create_review_request", { repoPath: identityKey.repoPath, baseSha: identityKey.baseSha, targetKey: identityKey.targetKey, targetKind: identityKey.targetKind, note: note.trim(), lenses, reviewers, maxRounds, headSha }));
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setSubmitting(false);
    }
  }
  return <form className="request-form" onSubmit={(event) => { event.preventDefault(); void submit(); }}>
    <div className="request-form-head"><strong>Request review</strong><span>{headSha ? <>The displayed head <code title={headSha}>{shortToken(headSha)}</code> is recorded, exactly as an agent records its own.</> : "The displayed head is unavailable, so a request cannot be recorded."}</span></div>
    <textarea className="request-form-note" aria-label="Request note" placeholder="Optional: what should reviewers focus on?" value={note} maxLength={REQUEST_NOTE_LIMIT} onChange={(event) => setNote(event.currentTarget.value)} onKeyDown={(event) => { if ((event.ctrlKey || event.metaKey) && !event.shiftKey && !event.altKey && (event.key === "k" || event.key === "b")) event.stopPropagation(); }} />
    {attempted && errors.note && <p className="request-form-error">{errors.note}</p>}
    <div className="request-form-field" role="group" aria-label="Review lenses">{REQUEST_LENS_OPTIONS.map((lens) => <label key={lens} className="request-check"><input type="checkbox" checked={lenses.includes(lens)} onChange={() => toggleLens(lens)} />{lens}</label>)}</div>
    <div className="request-form-field" role="group" aria-label="Named reviewers">
      {tokens.map((token) => <label key={token.id} className="request-check"><input type="checkbox" checked={reviewers.includes(token.name)} onChange={() => toggleReviewer(token.name)} />{token.name}</label>)}
      <span className="request-form-hint">{reviewers.length === 0 ? "No reviewers named; any agent can pick it up." : "Named reviewers must claim the request."}</span>
    </div>
    <div className="request-form-field">
      <span className="request-form-label">Round budget</span>
      <div className="scope-toggle request-rounds" role="group" aria-label="Round budget">{[REQUEST_ROUNDS.min, REQUEST_ROUNDS.default, REQUEST_ROUNDS.max].map((rounds) => <button key={rounds} type="button" className={maxRounds === rounds ? "active" : ""} aria-pressed={maxRounds === rounds} onClick={() => setMaxRounds(rounds)}>{rounds}</button>)}</div>
    </div>
    {shownError && <p className="request-form-error">{shownError}</p>}
    {error && <p className="request-form-error" role="status">{error}</p>}
    <div className="request-form-actions">
      <button className="request-action primary" type="submit" disabled={submitting}>{submitting ? "Sending..." : "Send request"}</button>
    </div>
  </form>;
}

export function ReviewView({ repoPath, repoName, liveWorktree, worktrees, target, refs, base, scope, reversed, index, loading, selectedFile, patch, patchError, patchLoading, fileView, diffPrefs, diffToggles, comments, content, contentLoading, contentError, imageSrc, imageError, imageLoading, onEnsureContent, canBack, canForward, onHistoryBack, onHistoryForward, onBack, onBaseChange, onTargetChange, onPreset, onReverse, onFileView, onFile, panes, onPaneVisibility, branchAction, focusedCommentId, reviewRefreshTick, commentsWide, onCommentsWide, osOpenRepoKey, osOpenBlocked, onOsOpenFailure, resolveSource, readOnly }: { repoPath: string; repoName: string; liveWorktree?: Worktree; worktrees?: Worktree[]; target: ReviewTarget; refs: RefInventory; base: string; scope: ReviewScope; reversed: boolean; index: ReviewIndex | null; loading: boolean; selectedFile: ChangedFile | null; patch: FilePatch | null; patchError: string; patchLoading: boolean; fileView: ChangedFilesView; diffPrefs: DiffPreferences; diffToggles: React.ReactNode; comments: CommentsApi; content: FileContent | null; contentLoading: boolean; contentError: string; imageSrc: string | null; imageError: string; imageLoading: boolean; onEnsureContent: () => void; canBack: boolean; canForward: boolean; onHistoryBack: () => void; onHistoryForward: () => void; onBack: () => void; onBaseChange: (value: string) => void; onTargetChange: (target: ReviewTarget) => void; onPreset: (preset: WorktreeReviewPreset) => void; onReverse: () => void; onFileView: (view: ChangedFilesView) => void; onFile: (file: ChangedFile) => void; panes: { files: boolean; comments: boolean }; onPaneVisibility: (next: { files: boolean; comments: boolean }) => void; branchAction?: { ref: string; busy: boolean; run: () => void } | null; focusedCommentId: number | null; reviewRefreshTick: number; commentsWide: boolean; onCommentsWide: (wide: boolean) => void; osOpenRepoKey?: string | null; osOpenBlocked?: boolean; onOsOpenFailure?: () => void; resolveSource: SourceResolver; readOnly: boolean }) {
  const targetName = target.kind === "worktree" ? target.worktree.branch : target.kind === "commit" ? target.sha : target.name;
  const allRefs = [...refs.heads, ...refs.remotes, ...refs.tags];
  const targetRefs = allRefs.filter((ref) => ref !== liveWorktree?.branch);
  const targetOptions = liveWorktree ? [liveWorktree.branch, ...targetRefs] : allRefs;
  const files = index?.files ?? [];
  const commitPreset = target.kind === "commit" ? target.parents[0] ?? "empty-tree" : "";
  const branchPreset = refs.default_base ?? "";
  const workingBase = target.kind === "worktree" ? workingChangesBase(target.worktree) : "";
  const workingChangesActive = base === workingBase && scope === "all" && !reversed;
  const summary = useCommitSummary(repoPath, target.kind === "commit" ? target.sha : null, resolveSource);
  const commitTitle = target.kind === "commit" ? summary?.subject ?? shortToken(target.sha) : shortToken(targetName);
  const [compareOpen, setCompareOpen] = useState(false);
  const compareRef = useRef<HTMLDivElement | null>(null);
  // A stream card's anchor or a focused portal thread opens the anchored
  // file in the patch and lands on the row; the pane consumes the jump
  // once that row renders.
  const { anchorJump, openCommentAnchor } = useAnchorJump(reversed, selectedFile, files, onFile, comments);
  const diskWorktree = readOnly ? null : reviewFileRoot(target, selectedFile, worktrees, repoPath);
  // A thread opened from the portal scrolls into view in the comments
  // stream and highlights once; the comments layer loads asynchronously,
  // so the retry rides the loaded-thread count. An anchored thread also
  // opens its file and lands the diff on the anchored row.
  const focusConsumedRef = useRef<number | null>(null);
  useEffect(() => {
    if (focusedCommentId === null || focusConsumedRef.current === focusedCommentId) return;
    const card = document.querySelector(`.comment-thread[data-comment-id="${focusedCommentId}"]`);
    if (!card) return;
    focusConsumedRef.current = focusedCommentId;
    card.scrollIntoView({ block: "center" });
    card.classList.add("comment-thread-focus");
    const timer = window.setTimeout(() => card.classList.remove("comment-thread-focus"), 2000);
    const thread = comments.threads.find((item) => item.comment.id === focusedCommentId);
    if (thread) openCommentAnchor(thread.comment);
    return () => window.clearTimeout(timer);
  }, [focusedCommentId, comments.threads.length]);
  useEffect(() => {
    if (!compareOpen) return;
    function onOutside(event: MouseEvent) {
      if (compareRef.current && event.target instanceof Node && !compareRef.current.contains(event.target)) setCompareOpen(false);
    }
    function onKey(event: KeyboardEvent) {
      // A picker's own handler closes its list first; Escape only dismisses
      // the popover when it originated outside that inner layer.
      if (compareRef.current && event.target instanceof Node && compareRef.current.contains(event.target)) return;
      if (event.key === "Escape") setCompareOpen(false);
    }
    document.addEventListener("mousedown", onOutside);
    document.addEventListener("keydown", onKey);
    return () => { document.removeEventListener("mousedown", onOutside); document.removeEventListener("keydown", onKey); };
  }, [compareOpen]);
  return <section className="review-view" aria-label="Code review">
    <header className="review-header">
      <div className="review-bar">
        <div className="review-nav"><button className="icon-button" type="button" aria-label="Navigate back" title="Back" disabled={!canBack} onClick={onHistoryBack}><ArrowLeft size={15} /></button><button className="icon-button" type="button" aria-label="Navigate forward" title="Forward" disabled={!canForward} onClick={onHistoryForward}><ArrowRight size={15} /></button></div>
        <button className="crumb-link" type="button" onClick={onBack} title="Open the project overview">{repoName}</button>
        <span className="crumb-sep">/</span>
        <span className="crumb-leaf" title={targetName}>{shortToken(targetName)}</span>
        <div className="compare-wrap" ref={compareRef}>
          <button className="range-chip" type="button" aria-haspopup="true" aria-expanded={compareOpen} title="Change what you are comparing" onClick={() => setCompareOpen((open) => !open)}><ArrowLeftRight size={12} /><code>{shortToken(reversed ? targetName : base)}...{shortToken(reversed ? base : targetName)}</code><ChevronDown size={11} /></button>
          {compareOpen && <div className="compare-pop" role="group" aria-label="Compare setup">
            <RefPicker id="review-target" label="Compare" repoPath={repoPath} refs={targetOptions} value={targetName} resolve={resolveSource} onChange={(value) => onTargetChange(liveWorktree && value === liveWorktree.branch ? { kind: "worktree", worktree: liveWorktree } : { kind: "ref", name: value })} onCommitPick={(detail) => onTargetChange({ kind: "commit", sha: detail.sha, parents: detail.parents, defaultBaseAncestor: false })} />
            <div className="compare-row">
              <RefPicker id="review-base" label="Base" repoPath={repoPath} refs={allRefs} value={base} resolve={resolveSource} exclude={target.kind === "worktree" ? [] : [targetName]} onChange={onBaseChange} />
              <button className="swap-button" type="button" aria-label="Swap review direction" title="Swap review direction" onClick={onReverse}>↔</button>
            </div>
          </div>}
        </div>
        {target.kind === "commit" ? <div className="scope-toggle" role="group" aria-label="Commit review preset"><button className={!target.defaultBaseAncestor && branchPreset && base === branchPreset && branchPreset !== commitPreset ? "active" : ""} type="button" disabled={target.defaultBaseAncestor || !branchPreset} onClick={() => onBaseChange(branchPreset)}>Branch so far</button><button className={base === commitPreset ? "active" : ""} type="button" onClick={() => onBaseChange(commitPreset)}>This commit</button></div> : target.kind === "worktree" ? <div className="scope-toggle" role="group" aria-label="Review scope"><button className={workingChangesActive ? "active" : ""} type="button" title="Only the uncommitted working-tree changes" onClick={() => onPreset("working")}>Working changes</button><button className={scope === "all" && !workingChangesActive ? "active" : ""} type="button" title="Everything against the base, uncommitted included" onClick={() => onPreset("all")}>All changes</button><button className={scope === "committed" && base !== workingBase ? "active" : ""} type="button" onClick={() => onPreset("committed")}>Committed only</button></div> : <span className="scope-fixed" aria-label="Review scope">Committed only</span>}
        <span className="bar-grow" />
        <span className="review-counts"><code>{index?.error ? "Review index unavailable" : index ? `${files.length} files, +${index.additions} -${index.deletions}` : "Loading review index..."}</code></span>
        {comments.key && !readOnly && <button className="secondary-button" type="button" aria-label="Comment on review" title="Comment on review" onClick={() => comments.openComposer("review")}><MessageSquare size={13} /> Review</button>}
      </div>
      {comments.key && !readOnly && <ReviewRequestBar identityKey={comments.key} headSha={index && !index.error ? index.target_sha : ""} />}
      <CommitRow key={targetName} title={commitTitle}>
        {target.kind === "commit" ? <>
          {summary?.body && <p className="commit-body">{summary.body}</p>}
          {(summary || (index && !index.error)) && <div className="sha-row">
            {summary && <span className="sha">{summary.author} · {summary.date}</span>}
            {index && !index.error && <>
              <span className="sha"><code>base {shortToken(index.base_sha)}</code><CopyButton ghost value={index.base_sha} label="Copy base commit hash" /></span>
              <span className="sha"><code>target {shortToken(index.target_sha)}</code><CopyButton ghost value={index.target_sha} label="Copy target commit hash" /></span>
            </>}
          </div>}
        </> : target.kind === "worktree" ? <div className="sha-row">
          <span className="sha"><code title={target.worktree.path}>{target.worktree.path}</code><CopyButton ghost value={target.worktree.path} label="Copy worktree path" /></span>
          <span className="sha"><code>HEAD {shortToken(target.worktree.head)}</code><CopyButton ghost value={target.worktree.head} label={`Copy commit hash ${shortToken(target.worktree.head)}`} /></span>
        </div> : <div className="sha-row">
          <span className="sha"><code title={repoPath}>{repoPath}</code><CopyButton ghost value={repoPath} label="Copy project path" /></span>
        </div>}
      </CommitRow>
      <div className="review-subbar"><span className="bar-grow" />{diffToggles}</div>
    </header>
    {!base && !loading ? <Empty icon={<GitBranch size={24} />} title="Choose a base branch to review" detail="This review has no default base." action={<RefPicker id="prompt-review-base" label="Choose base" repoPath={repoPath} refs={allRefs} value={base} resolve={resolveSource} exclude={target.kind === "worktree" ? [] : [targetName]} onChange={onBaseChange} />} /> : <div className={reviewBodyClass(panes, commentsWide)}>
      {panes.files ? <FileIndexPane base={base} index={index} loading={loading} selectedFile={selectedFile} view={fileView} onView={onFileView} onFile={onFile} onCollapse={() => onPaneVisibility({ files: false, comments: panes.comments })} branchAction={branchAction} /> : <PaneRail side="left" label="Changed files" onOpen={() => onPaneVisibility({ files: true, comments: panes.comments })} />}
      <PatchPane selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} diffPrefs={diffPrefs} reversed={reversed} comments={comments} onDiskWorktree={diskWorktree} content={content} contentLoading={contentLoading} contentError={contentError} imageSrc={imageSrc} imageError={imageError} imageLoading={imageLoading} onEnsureContent={onEnsureContent} anchorJump={anchorJump} osOpenRepoKey={osOpenRepoKey} osOpenBlocked={osOpenBlocked} onOsOpenFailure={onOsOpenFailure} />
      {panes.comments ? <CommentStream comments={comments} reversed={reversed} strip={<ReviewsStrip reviewKey={comments.key} refreshTick={reviewRefreshTick} resolve={resolveSource} />} onCollapse={() => onPaneVisibility({ files: panes.files, comments: false })} wide={commentsWide} onToggleWide={() => onCommentsWide(!commentsWide)} onOpenAnchor={openCommentAnchor} /> : <PaneRail side="right" label="Comments" onOpen={() => onPaneVisibility({ files: panes.files, comments: true })} />}
    </div>}
  </section>;
}

// A collapsed pane's slim reopen rail; it occupies the pane's grid column.
export function PaneRail({ side, label, onOpen }: { side: "left" | "right"; label: string; onOpen: () => void }) {
  return <aside className={`pane-rail ${side === "left" ? "" : "right"}`} aria-label={label}>
    <button className="icon-button" type="button" aria-label={`Show ${label}`} title={`Show ${label}${side === "left" ? " (Ctrl B)" : ""}`} onClick={onOpen}>{side === "left" ? <PanelLeftOpen size={14} /> : <PanelRightOpen size={14} />}</button>
  </aside>;
}

export function reviewBodyClass(panes: { files: boolean; comments: boolean }, commentsWide = false) {
  return ["review-body", panes.files ? "" : "files-collapsed", panes.comments ? "" : "comments-collapsed", panes.comments && commentsWide ? "comments-wide" : ""].filter(Boolean).join(" ");
}

const FILE_VIEW_OPTIONS: { value: ChangedFilesView; label: string; title: string }[] = [
  { value: "tree", label: "Tree", title: "Nested directory tree" },
  { value: "list", label: "List", title: "Compact single-line list" },
  { value: "details", label: "Details", title: "Name with full path on a second line" },
];

export function FileIndexPane({ base, index, loading, selectedFile, view, onView, onFile, onCollapse, branchAction }: { base: string; index: ReviewIndex | null; loading: boolean; selectedFile: ChangedFile | null; view: ChangedFilesView; onView: (view: ChangedFilesView) => void; onFile: (file: ChangedFile) => void; onCollapse: () => void; branchAction?: { ref: string; busy: boolean; run: () => void } | null }) {
  const files = index?.files ?? [];
  const [query, setQuery] = useState("");
  // Expand/collapse is deliberately local: the tree opens fully on every
  // visit and only the view choice itself is remembered. null = all open.
  const [openDirs, setOpenDirs] = useState<Set<string> | null>(null);
  useEffect(() => { setQuery(""); setOpenDirs(null); }, [index]);
  const shown = useMemo(() => filterChangedFiles(files, query), [files, query]);
  // A filter always expands the tree so matches are never hidden; until it
  // clears, directory toggles stay inert because they would otherwise edit
  // collapse state the user cannot see.
  const filtering = Boolean(query.trim());
  const open = filtering ? null : openDirs;
  const rows = useMemo(() => view === "tree" ? flattenFileTree(buildFileTree(shown), open) : shown.map((file) => ({ kind: "file" as const, file, depth: 0 })), [view, shown, open]);
  function toggleDir(path: string) {
    if (filtering) return;
    setOpenDirs((current) => {
      const next = new Set(current ?? allTreeDirPaths(buildFileTree(shown)));
      if (next.has(path)) next.delete(path); else next.add(path);
      return next;
    });
  }
  // Arrow keys walk the rendered file rows in order, skipping directory
  // rows; selection follows focus.
  function moveSelection(event: ReactKeyboardEvent<HTMLButtonElement>, delta: number) {
    const buttons = Array.from(event.currentTarget.closest(".file-list")?.querySelectorAll<HTMLButtonElement>("button[data-file-path]") ?? []);
    const at = buttons.findIndex((button) => button.dataset.filePath === event.currentTarget.dataset.filePath);
    const next = buttons[at + delta];
    const path = next?.dataset.filePath;
    if (!path) return;
    event.preventDefault();
    const file = files.find((item) => item.path === path);
    if (file) onFile(file);
    next.focus();
  }
  return <aside className="file-index" aria-label="Changed files">
    <div className="pane-heading"><strong>Changed files</strong><span className="pane-heading-actions"><span>{query.trim() ? `${shown.length} / ${files.length}` : files.length}</span><button className="icon-button" type="button" aria-label="Hide Changed files" title="Hide Changed files (Ctrl B)" onClick={onCollapse}><PanelLeftClose size={12} /></button></span></div>
    {loading ? <div className="index-skeleton">{Array.from({ length: 7 }, (_, i) => <i key={i} />)}</div>
      : index?.error ? <Empty icon={<CircleDot size={20} />} title="Review unavailable" detail={index.error} action={branchAction && index.error_code === "partial_clone_content" ? <button className="secondary-button" type="button" disabled={branchAction.busy} onClick={branchAction.run}><RefreshCw size={13} />{branchAction.busy ? "Fetching content..." : "Fetch branch content"}</button> : undefined} />
      : files.length === 0 ? <Empty icon={<CircleDot size={20} />} title={`No changes vs ${shortToken(base)}`} detail="Try a different base branch." />
      : <>
        <div className="file-tools">
          <div className="file-filter"><Search size={12} /><input type="text" aria-label="Filter changed files" placeholder="Filter files" value={query} onChange={(event) => setQuery(event.currentTarget.value)} /></div>
          <div className="file-view-toggle" role="group" aria-label="Changed files view">{FILE_VIEW_OPTIONS.map((option) => <button key={option.value} type="button" className={view === option.value ? "active" : ""} aria-pressed={view === option.value} title={option.title} onClick={() => onView(option.value)}>{option.label}</button>)}</div>
          {view === "tree" && <>
            <button className="icon-button" type="button" aria-label="Expand all directories" title="Expand all" disabled={filtering} onClick={() => setOpenDirs(new Set(allTreeDirPaths(buildFileTree(shown))))}><ChevronsUpDown size={13} /></button>
            <button className="icon-button" type="button" aria-label="Collapse all directories" title="Collapse all" disabled={filtering} onClick={() => setOpenDirs(new Set())}><ChevronsDownUp size={13} /></button>
          </>}
        </div>
        <div className="file-list" role={view === "tree" ? "tree" : "listbox"} aria-label="Changed files">
          {rows.map((row) => row.kind === "dir"
            ? <button key={row.path} type="button" role="treeitem" aria-expanded={open === null || open.has(row.path)} aria-disabled={filtering || undefined} className="file-dir-row" title={row.path} style={{ paddingLeft: 6 + row.depth * 14 }} onClick={() => toggleDir(row.path)}>
              <ChevronRight size={11} className="dir-chevron" aria-hidden="true" />
              <span className="dir-name">{row.label}</span>
              <span className="dir-count">{row.count}</span>
            </button>
            : <FileRow key={row.file.path} file={row.file} view={view} depth={row.depth} selected={selectedFile?.path === row.file.path} onFile={onFile} onArrow={moveSelection} />)}
          {rows.length === 0 && <div className="file-list-empty">No files matching <code>{query.trim()}</code></div>}
        </div>
      </>}
  </aside>;
}

function FileRow({ file, view, depth, selected, onFile, onArrow }: { file: ChangedFile; view: ChangedFilesView; depth: number; selected: boolean; onFile: (file: ChangedFile) => void; onArrow: (event: ReactKeyboardEvent<HTMLButtonElement>, delta: number) => void }) {
  const [dir, name] = splitFilePath(file.path);
  return <button type="button" role={view === "tree" ? "treeitem" : "option"} aria-selected={selected} data-file-path={file.path} className={`file-row ${view}-row status-${file.status.toLowerCase()} ${selected ? "selected" : ""}`} style={view === "tree" ? { paddingLeft: 8 + depth * 14 } : undefined} title={file.path}
    onClick={() => onFile(file)}
    onKeyDown={(event) => { if (event.key === "ArrowDown") onArrow(event, 1); else if (event.key === "ArrowUp") onArrow(event, -1); }}>
    {view === "details"
      ? <>
        <span className="details-top"><b>{file.status}</b><span className="file-name">{name}</span></span>
        <span className="file-dir-sub">{dir || "(root)"}</span>
      </>
      : <>
        <b>{file.status}</b>
        {view === "list"
          ? <span className="file-row-line">{dir && <span className="file-dir-part">{dir}</span>}<span className="file-name">{name}</span></span>
          : <span className="file-name">{name}</span>}
      </>}
  </button>;
}

export function PatchPane({ selectedFile, patch, patchError, patchLoading, diffPrefs, reversed, comments, onDiskWorktree, content, contentLoading, contentError, imageSrc, imageError, imageLoading, onEnsureContent, anchorJump, osOpenRepoKey, osOpenBlocked, onOsOpenFailure }: { selectedFile: ChangedFile | null; patch: FilePatch | null; patchError: string; patchLoading: boolean; diffPrefs: DiffPreferences; reversed: boolean; comments: CommentsApi; onDiskWorktree?: string | null; content: FileContent | null; contentLoading: boolean; contentError: string; imageSrc: string | null; imageError: string; imageLoading: boolean; onEnsureContent: () => void; anchorJump?: AnchorJump | null; osOpenRepoKey?: string | null; osOpenBlocked?: boolean; onOsOpenFailure?: () => void }) {
  const [openError, setOpenError] = useState("");
  useEffect(() => setOpenError(""), [selectedFile?.path]);
  // osOpenRepoKey rides only for remote projects: their open is a
  // copy-to-temp fetch, and a typed failure hides the action for that
  // project instead of leaving a button that cannot succeed.
  const openOnDisk = (reveal: boolean) => {
    if (!onDiskWorktree || !selectedFile) return;
    invoke("open_review_file", { worktreePath: onDiskWorktree, path: selectedFile.path, reveal, ...(osOpenRepoKey ? { repoPath: osOpenRepoKey } : {}) }).then(() => setOpenError("")).catch((error) => { setOpenError(errorMessage(error)); if (osOpenRepoKey) onOsOpenFailure?.(); });
  };
  // The parse is memoized so DiffLine objects keep their identity across
  // preference-driven re-renders; the token map is keyed by that identity.
  const hunks = useMemo(() => patch && !patch.binary ? parseHunks(patch.text) : [], [patch]);
  // Row highlights are a reading aid: clicks, shift-clicks, and gutter
  // drags only select. The composer opens solely on explicit intent, via
  // the line-number chip or the chip after a text selection.
  const [selection, setSelection] = useState<PaneSelection | null>(null);
  // A DOM text selection inside the patch (part of a line or across lines)
  // offers itself as a comment target without disturbing row anchors.
  const [textSelection, setTextSelection] = useState<TextSelectionRange | null>(null);
  // Clicking a line number arms the same chip over the picked range.
  const [gutterChip, setGutterChip] = useState<PaneSelection | null>(null);
  const [composeTarget, setComposeTarget] = useState<CommentSelection | null>(null);
  const paneRef = useRef<HTMLElement | null>(null);
  const gutterDragRef = useRef<{ side: DisplaySide } | null>(null);
  const suppressRowClickRef = useRef(false);
  // "diff" shows the patch; "file" swaps the pane's body to the full file on
  // the patch's new side. Both live in the same pane, so switching never
  // leaves the review, the file list, or the comment stream.
  const [patchView, setPatchView] = useState<"diff" | "file">("diff");
  // Expanded gaps persist per file; content arrives asynchronously, so a gap
  // stays on its expand control until the lines are actually in hand.
  const [expandedGaps, setExpandedGaps] = useState<Set<string>>(new Set());
  const contentLines = useMemo(() => content && !content.binary ? splitFileLines(content.text) : null, [content]);
  const gaps = useMemo(() => patchGaps(hunks.map((hunk) => hunk.header), contentLines?.length ?? null), [hunks, contentLines]);
  const split = diffPrefs.layout === "split";
  const whitespace = diffPrefs.whitespaceVisible;
  const commentsActive = comments.key !== null;
  // Read-only layers (server-backed reviews this phase) keep their cards
  // visible but offer no new anchors, chips, or composers.
  const canComment = commentsActive && !comments.readOnly;
  const fileMode = patchView === "file";
  const expandedHunks = useMemo(() => hunksWithExpandedGaps(hunks, gaps, expandedGaps, contentLines), [hunks, gaps, expandedGaps, contentLines]);
  // One continuous row stream serves both modes: patch rows, or the whole
  // new-side file with the patch's changes marked on it.
  const rows = useMemo(() => fileMode ? buildFileRows(contentLines ?? []) : buildPatchRows(expandedHunks, gaps, expandedGaps, contentLines, split), [fileMode, expandedHunks, gaps, expandedGaps, contentLines, split]);
  // Lines the patch adds, and the surviving lines that lost a deletion,
  // mark the full-file view so changes stay visible there.
  const addedNewLines = useMemo(() => {
    const added = new Set<number>();
    for (const hunk of hunks) for (const line of hunk.lines) if (line.newLine !== null && line.text.startsWith("+")) added.add(line.newLine);
    return added;
  }, [hunks]);
  const deletionLines = useMemo(() => new Set(deletionTicks(hunks)), [hunks]);
  // Change regions drive prev/next jumps and the change map strip in both
  // modes; region rows locate them in the row stream.
  const regions = useMemo(() => changeRegions(hunks.map((hunk) => hunk.header)), [hunks]);
  const regionRows = useMemo(() => {
    // New-side line numbers locate regions in the stream; split rows carry
    // theirs on the new half.
    const lineToRow = new Map<number, number>();
    rows.forEach((row, index) => {
      const number = row.kind === "line" ? row.line.newLine : row.kind === "split" ? row.next?.newLine ?? null : null;
      if (number !== null && !lineToRow.has(number)) lineToRow.set(number, index);
    });
    return regions.map((region) => {
      const exact = lineToRow.get(region.start);
      if (exact !== undefined) return exact;
      for (let index = 0; index < rows.length; index += 1) {
        const row = rows[index];
        const number = row.kind === "line" ? row.line.newLine : row.kind === "split" ? row.next?.newLine ?? null : null;
        if (number !== null && number >= region.start) return index;
      }
      return null;
    });
  }, [regions, rows]);

  // The whole stream renders in native flow: every row stays in the DOM,
  // the browser owns scrolling, and nothing custom runs in the scroll
  // path.
  const scrollRef = useRef<HTMLDivElement | null>(null);

  // Change navigation: prev/next step between change regions, the strip
  // maps them proportionally, and a flash marks where a jump landed.
  const [flashLine, setFlashLine] = useState<number | null>(null);
  const flashTimer = useRef(0);
  useEffect(() => () => clearTimeout(flashTimer.current), []);
  const scrollToRegion = (index: number) => {
    const el = scrollRef.current;
    const rowIndex = regionRows[index];
    if (!el || rowIndex === null || rowIndex === undefined) return;
    // The region's first row lands exactly at the viewport top so the
    // counter, the stepping, and the movement always agree.
    const rowEl = el.querySelector(`[data-index="${rowIndex}"]`);
    if (rowEl instanceof HTMLElement) el.scrollTop = rowEl.offsetTop - STREAM_TOP_PADDING;
    setFlashLine(regions[index].start);
    clearTimeout(flashTimer.current);
    flashTimer.current = window.setTimeout(() => setFlashLine(null), 900);
  };

  // An anchor jump lands once its row renders: until then it stays pending
  // across the async patch load, and an unreachable line (outdated anchor,
  // unexpanded gap) just leaves it waiting. A jump is a diff-view concept,
  // so it leaves the full-file view. Consumption is once per nonce, and
  // the flash rides a ref-held timer so the pending reset cannot cancel
  // the row's un-highlight.
  const [pendingAnchor, setPendingAnchor] = useState<AnchorJump | null>(null);
  const consumedAnchorRef = useRef(0);
  const anchorFlashTimer = useRef(0);
  useEffect(() => () => clearTimeout(anchorFlashTimer.current), []);
  useEffect(() => {
    if (!anchorJump || consumedAnchorRef.current === anchorJump.nonce) return;
    consumedAnchorRef.current = anchorJump.nonce;
    setPendingAnchor(anchorJump);
    if (fileMode) setPatchView("diff");
  }, [anchorJump, fileMode]);
  useEffect(() => {
    if (!pendingAnchor || selectedFile?.path !== pendingAnchor.filePath) return;
    const el = scrollRef.current;
    if (!el) return;
    const row = el.querySelector(`.diff-line[data-side="${pendingAnchor.side}"][data-line="${pendingAnchor.line}"]`);
    if (!(row instanceof HTMLElement)) return;
    const streamRow = row.closest(".stream-row");
    el.scrollTop = (streamRow instanceof HTMLElement ? streamRow : row).offsetTop - STREAM_TOP_PADDING;
    row.classList.add("anchor-flash");
    clearTimeout(anchorFlashTimer.current);
    anchorFlashTimer.current = window.setTimeout(() => row.classList.remove("anchor-flash"), 1600);
    setPendingAnchor(null);
  }, [pendingAnchor, rows, selectedFile?.path]);

  function expandGap(gap: PatchGap) {
    onEnsureContent();
    setExpandedGaps((current) => { const next = new Set(current); next.add(gap.id); return next; });
  }
  useEffect(() => { setSelection(null); setTextSelection(null); setGutterChip(null); setComposeTarget(null); setPatchView("diff"); setExpandedGaps(new Set()); if (scrollRef.current) scrollRef.current.scrollTop = 0; }, [selectedFile?.path]);
  useEffect(() => {
    function read() {
      const next = textSelectionRange(paneRef.current);
      setTextSelection(next);
      if (next) setGutterChip(null);
    }
    document.addEventListener("selectionchange", read);
    return () => document.removeEventListener("selectionchange", read);
  }, []);
  // A gutter drag ends wherever the pointer lifts; the click it would leave
  // behind on the row is swallowed exactly once.
  useEffect(() => {
    function endDrag() {
      if (gutterDragRef.current === null) return;
      gutterDragRef.current = null;
      suppressRowClickRef.current = true;
    }
    window.addEventListener("mouseup", endDrag);
    return () => window.removeEventListener("mouseup", endDrag);
  }, []);
  const normalized = selection ? { displaySide: selection.displaySide, start: Math.min(selection.anchor, selection.focus), end: Math.max(selection.anchor, selection.focus) } : null;
  const cardsFor: ReturnType<typeof inlineCards> = selectedFile && diffPrefs.inlineCommentsVisible ? inlineCards(comments, selectedFile.path, reversed) : new Map();
  const patchLines = hunks.flatMap((hunk) => hunk.lines);
  function selectRow(side: DisplaySide, number: number, extend: boolean) {
    setSelection((current) => extend && current && current.displaySide === side ? { displaySide: side, anchor: current.anchor, focus: number } : { displaySide: side, anchor: number, focus: number });
  }
  function beginGutterDrag(side: DisplaySide, number: number) {
    suppressRowClickRef.current = false;
    gutterDragRef.current = { side };
    setSelection({ displaySide: side, anchor: number, focus: number });
    setGutterChip({ displaySide: side, anchor: number, focus: number });
  }
  function extendDrag(side: DisplaySide, number: number) {
    if (!gutterDragRef.current || gutterDragRef.current.side !== side) return;
    setSelection((current) => current && current.displaySide === side ? { ...current, focus: number } : { displaySide: side, anchor: number, focus: number });
    setGutterChip((current) => current && current.displaySide === side ? { ...current, focus: number } : { displaySide: side, anchor: number, focus: number });
  }
  function rowClick(side: DisplaySide, number: number, extend: boolean) {
    if (suppressRowClickRef.current) { suppressRowClickRef.current = false; return; }
    // A live text selection means the user is copying code, not picking rows.
    if (textSelection) return;
    // Clicking code is a reading gesture: it dismisses a gutter chip rather
    // than moving it.
    setGutterChip(null);
    selectRow(side, number, extend);
  }
  function startCompose(displaySide: DisplaySide, start: number, end: number, excerpt?: string) {
    setSelection({ displaySide, anchor: start, focus: end });
    setGutterChip(null);
    setTextSelection(null);
    setComposeTarget({ displaySide, start, end, excerpt });
  }
  // The chip floats under its end row without displacing the diff. A text
  // selection wins over a gutter pick; both compose with one click.
  const chipFor = (side: DisplaySide, number: number | null): React.ReactNode => {
    if (number === null) return null;
    let start: number; let end: number; let excerpt: string | undefined; let title: string;
    if (textSelection && textSelection.displaySide === side && number === textSelection.end) {
      ({ start, end } = textSelection);
      excerpt = textSelection.text;
      title = "Comment on the selected text";
    } else if (gutterChip && gutterChip.displaySide === side && number === Math.max(gutterChip.anchor, gutterChip.focus)) {
      start = Math.min(gutterChip.anchor, gutterChip.focus);
      end = Math.max(gutterChip.anchor, gutterChip.focus);
      title = start === end ? `Comment on line ${start}` : `Comment on lines ${start}-${end}`;
    } else return null;
    return <button className="selection-comment-chip" type="button" aria-label={title} title={title} onMouseDown={(event) => event.preventDefault()} onClick={(event) => { event.stopPropagation(); startCompose(side, start, end, excerpt); }}>Comment</button>;
  };
  const inlineAfter = (side: DisplaySide, number: number | null) => {
    if (number === null) return null;
    const cards = cardsFor.get(`${side}:${number}`) ?? [];
    const composerHere = composeTarget !== null && composeTarget.displaySide === side && number === composeTarget.end;
    return <>{cards.map(({ thread }) => <div className="inline-comment" key={thread.comment.id}><CommentThreadView thread={thread} status={comments.statuses[thread.comment.id] ?? null} comments={comments} reversed={reversed} /></div>)}
      {composerHere && selectedFile && <InlineCommentComposer selection={composeTarget} filePath={selectedFile.path} lines={patchLines} reversed={reversed} comments={comments} onDone={() => setComposeTarget(null)} />}</>;
  };
  const selectedClass = (side: DisplaySide, number: number | null) => {
    if (number === null) return "";
    if (normalized && normalized.displaySide === side && number >= normalized.start && number <= normalized.end) return " comment-selected";
    if (textSelection && textSelection.displaySide === side && number >= textSelection.start && number <= textSelection.end) return " comment-selected";
    return "";
  };
  // Highlighting is progressive: lines paint as plain text immediately, and
  // token spans swap in once their hunk has been tokenized in the worker.
  // Hunks tokenize whole, in order, off the UI thread; token spans swap in
  // as results land and can never block rendering or input.
  const lang = diffPrefs.syntaxVisible && patch && !patch.binary && selectedFile ? languageForPath(selectedFile.path) : null;
  const [tokenMap, setTokenMap] = useState<Map<DiffLine, TokenLine> | null>(null);
  const tokenizedHunksRef = useRef(new Set<string>());
  useEffect(() => {
    tokenizedHunksRef.current = new Set();
    setTokenMap(null);
  }, [patch, lang]);
  useEffect(() => {
    if (fileMode || !lang || !patch) return;
    let cancelled = false;
    void (async () => {
      for (let hunkIndex = 0; hunkIndex < expandedHunks.length; hunkIndex += 1) {
        const lines = expandedHunks[hunkIndex]?.lines;
        if (!lines || lines.length === 0) continue;
        // Expanding a gap changes its host hunk's length; tokenize it again.
        const key = `${hunkIndex}:${lines.length}`;
        if (tokenizedHunksRef.current.has(key)) continue;
        const tokens = await tokenizeHunk(lines, lang, () => cancelled);
        if (cancelled) return;
        // Mark only once the tokens landed: a cancelled round trip must be
        // retried by a later pass, not remembered as done.
        tokenizedHunksRef.current.add(key);
        if (!tokens) continue;
        setTokenMap((current) => {
          const next = new Map(current ?? []);
          const sources = hunkSideSources(lines);
          sources.old.forEach((line, index) => next.set(line, tokens.old[index] ?? []));
          sources.new.forEach((line, index) => next.set(line, tokens.new[index] ?? []));
          return next;
        });
      }
    })();
    return () => { cancelled = true; };
  }, [fileMode, lang, patch, expandedHunks]);
  const tokensOf = (line: DiffLine) => tokenMap?.get(line);
  // The file view tokenizes in bounded chunks; a chunk's rows swap from
  // plain text to token spans once its worker round trip lands.
  const [fileTokenChunks, setFileTokenChunks] = useState<Map<number, TokenLine[]>>(new Map());
  const requestedChunksRef = useRef(new Set<number>());
  useEffect(() => {
    requestedChunksRef.current = new Set();
    setFileTokenChunks(new Map());
  }, [contentLines, lang]);
  useEffect(() => {
    if (!fileMode || !lang || contentLines === null) return;
    let cancelled = false;
    void (async () => {
      for (let start = 0; start < contentLines.length; start += FILE_TOKEN_CHUNK_LINES) {
        const chunk = start / FILE_TOKEN_CHUNK_LINES;
        if (requestedChunksRef.current.has(chunk)) continue;
        const slice = contentLines.slice(start, start + FILE_TOKEN_CHUNK_LINES);
        const tokens = await tokenizeHunk(slice.map((text) => ({ text: ` ${text}` })), lang, () => cancelled);
        if (cancelled) return;
        requestedChunksRef.current.add(chunk);
        if (tokens) setFileTokenChunks((current) => new Map(current).set(chunk, tokens.new));
      }
    })();
    return () => { cancelled = true; };
  }, [fileMode, lang, contentLines]);
  const fileTokenOf = (lineNumber: number) => fileTokenChunks.get(Math.floor((lineNumber - 1) / FILE_TOKEN_CHUNK_LINES))?.[(lineNumber - 1) % FILE_TOKEN_CHUNK_LINES];
  const renderRow = (row: RowSpec, index: number): React.ReactNode => {
    if (row.kind === "header") return <div key={`h${index}`} className="hunk-header" data-index={index}>{row.header}</div>;
    if (row.kind === "gap") {
      const pending = expandedGaps.has(row.gap.id) && contentLines === null && contentLoading;
      const error = contentLines === null && contentError !== "";
      return <div key={`g${index}`} className={`diff-gap${row.slim ? " slim" : ""}`} data-index={index}><ExpandGapRow gap={row.gap} slim={row.slim} pending={pending} error={error} onExpand={() => expandGap(row.gap)} /></div>;
    }
    if (row.kind === "split") {
      return <div key={`s${index}`} className="stream-row split" data-index={index}>
        {row.old ? <DiffHalf placement="left" side="LEFT" gutter={row.old.oldLine} text={row.old.text} whitespace={whitespace} tokens={tokensOf(row.old)} commentsActive={canComment} commentable={!row.old.expanded} selected={selectedClass("LEFT", row.old.oldLine)} chip={canComment ? chipFor("LEFT", row.old.oldLine) : null} onRowClick={rowClick} onGutterDown={beginGutterDrag} onEnter={extendDrag} /> : <div className="diff-line half half-left" />}
        {inlineAfter("LEFT", row.old?.oldLine ?? null)}
        {row.next ? <DiffHalf placement="right" side="RIGHT" gutter={row.next.newLine} text={row.next.text} whitespace={whitespace} tokens={tokensOf(row.next)} commentsActive={canComment} commentable={!row.next.expanded} selected={selectedClass("RIGHT", row.next.newLine)} chip={canComment ? chipFor("RIGHT", row.next.newLine) : null} onRowClick={rowClick} onGutterDown={beginGutterDrag} onEnter={extendDrag} /> : <div className="diff-line half half-right" />}
        {inlineAfter("RIGHT", row.next?.newLine ?? null)}
      </div>;
    }
    const line = row.line;
    const fileTokens = fileMode && line.newLine !== null ? fileTokenOf(line.newLine) : undefined;
    const flash = line.newLine !== null && flashLine === line.newLine;
    const anchor = canComment && !fileMode ? selectableRow(line) : null;
    const target = anchor !== null && !line.expanded ? anchor : null;
    const lineClasses = ["diff-line",
      line.text.startsWith("+") ? "addition" : "",
      line.text.startsWith("-") ? "deletion" : "",
      fileMode && line.newLine !== null && addedNewLines.has(line.newLine) ? "addition" : "",
      fileMode && line.newLine !== null && deletionLines.has(line.newLine) ? "deletion-tick" : "",
      flash ? "jump-flash" : "",
      target ? " commentable" : "",
      anchor ? selectedClass(anchor.side, anchor.number) : ""].join(" ");
    return <div key={`l${index}`} className="stream-row" data-index={index}>
      <div className={lineClasses} data-side={target?.side} data-line={target?.number} onClick={target ? (event) => rowClick(target.side, target.number, event.shiftKey) : undefined} onMouseEnter={target ? () => extendDrag(target.side, target.number) : undefined}>
        {/* The file view shows one gutter; a second span would auto-flow the
            code into the grid's next row, under the number. */}
        {fileMode ? <span className="line-number">{line.newLine ?? ""}</span> : <>
          <span className="line-number" onMouseDown={target ? (event) => { event.preventDefault(); beginGutterDrag(target.side, target.number); } : undefined}>{line.oldLine ?? ""}</span>
          <span className="line-number" onMouseDown={target ? (event) => { event.preventDefault(); beginGutterDrag(target.side, target.number); } : undefined}>{line.newLine ?? ""}</span>
        </>}
        <code>{diffLineContent(line.text, whitespace, fileTokens ?? tokensOf(line))}</code>
        {target && chipFor(target.side, target.number)}
      </div>
      {anchor && inlineAfter(anchor.side, anchor.number)}
    </div>;
  };
  // Renderable image assets preview in File view even though their patch is
  // binary; the diff view keeps its binary notice.
  const imageBody = fileMode && selectedFile && imageMimeForPath(selectedFile.path) !== null ? imageLoading ? <div className="patch-skeleton" aria-label="Loading image"><i /><i /><i /><i /></div> : imageError ? <Empty icon={<FileDiff size={24} />} title="Image unavailable" detail={imageError} /> : !imageSrc ? <Empty icon={<CircleDot size={24} />} title="No file on this side" detail="The image does not exist on this side of the diff." /> : <div className="image-preview-body"><img className="image-preview" src={imageSrc} alt={selectedFile.path} /></div> : null;
  // Freeze guard for pathological files, not a feature: past the cap the
  // pane declines and points at the open/reveal actions instead.
  const capBody = rows.length > MAX_RENDERED_ROWS ? <Empty icon={<FileWarning size={24} />} title="File too large to render" detail={`This file has about ${rows.length.toLocaleString()} lines, past what WorktreeView renders so the app stays fast. Open it externally instead.`} action={onDiskWorktree && !osOpenBlocked ? <div className="cap-actions"><button className="secondary-button" type="button" onClick={() => openOnDisk(false)}><ExternalLink size={13} />Open in default app</button><button className="secondary-button" type="button" onClick={() => openOnDisk(true)}><FolderOpen size={13} />Reveal in file explorer</button></div> : undefined} /> : null;
  const streamPane = <div ref={scrollRef} className="patch-scroll"><div className={`hunk-list ${fileMode ? "file-view" : ""} ${split ? "split-layout" : ""} ${diffPrefs.lineWrap ? "wrap-lines" : ""}`}>{rows.map((row, index) => renderRow(row, index))}</div></div>;
  return <section ref={paneRef} className="patch-pane" aria-label="File patch">{selectedFile && <div className="patch-heading"><code title={selectedFile.path}>{selectedFile.path}</code><span className="patch-heading-meta"><span>{selectedFile.status}</span><div className="patch-view-toggle" role="group" aria-label="Patch or full file view"><button type="button" className={patchView === "diff" ? "active" : ""} aria-pressed={patchView === "diff"} title="Diff view" onClick={() => setPatchView("diff")}>Diff</button><button type="button" className={patchView === "file" ? "active" : ""} aria-pressed={patchView === "file"} title="Full file view" onClick={() => { setPatchView("file"); onEnsureContent(); }}>File</button></div>{regions.length > 0 && <ChangeNav scrollRef={scrollRef} regions={regions} regionRows={regionRows} onStep={scrollToRegion} streamKey={`${selectedFile?.path ?? ""}:${patchView}:${rows.length}:${contentLines?.length ?? 0}`} />}{onDiskWorktree && !osOpenBlocked && <><button className="icon-button" type="button" aria-label="Open file" title="Open file" onClick={() => openOnDisk(false)}><ExternalLink size={13} /></button><button className="icon-button" type="button" aria-label="Reveal in file explorer" title="Reveal in file explorer" onClick={() => openOnDisk(true)}><FolderOpen size={13} /></button></>}{canComment && <button className="icon-button" type="button" aria-label="Comment on file" title="Comment on file" onClick={() => comments.openFileComposer(selectedFile.path)}><MessageSquare size={13} /></button>}</span></div>}{openError && <p className="patch-open-error" role="status">{openError}</p>}{comments.composer?.kind === "file" && selectedFile && comments.composer.filePath === selectedFile.path && <div className="comment-composer-panel"><p className="eyebrow">Comment on {selectedFile.path}</p><DraftComposer placeholder={`Comment on ${selectedFile.path}`} submitLabel="Comment" onSubmit={({ body, severity }) => { void comments.create({ body, severity, file_path: selectedFile.path, side: null, start_line: null, end_line: null, lines: [] }).then(comments.closeComposer); }} onCancel={comments.closeComposer} /></div>}<div className="patch-body">{patchLoading ? <div className="patch-skeleton" aria-label="Loading patch"><i /><i /><i /><i /></div> : patchError ? <Empty icon={<FileDiff size={24} />} title="Patch not rendered" detail={patchError} /> : !selectedFile ? <Empty icon={<FileDiff size={24} />} title="Pick a file to review" detail="Choose one from Changed files and its diff opens here." /> : imageBody !== null ? imageBody : patch?.binary ? <Empty icon={<FileDiff size={24} />} title="Binary file changed" detail={selectedFile.path} /> : patch?.text === "" ? <Empty icon={<CircleDot size={24} />} title="No changes in this file" detail="The selected file has no renderable patch." /> : fileMode ? contentLoading ? <div className="patch-skeleton" aria-label="Loading file"><i /><i /><i /><i /></div> : contentError ? <Empty icon={<FileDiff size={24} />} title="File content unavailable" detail={contentError} /> : !content || content.binary ? <Empty icon={<FileDiff size={24} />} title="Binary file" detail="The full file view is unavailable for binary content." /> : rows.length === 0 ? <Empty icon={<CircleDot size={24} />} title="No file on this side" detail="The file does not exist on this side of the diff." /> : capBody ?? streamPane : hunks.length === 0 ? <pre className="patch-metadata"><code>{patch?.text}</code></pre> : capBody ?? streamPane}{streamReady() && fileMode && regions.length > 0 && <ChangeStrip scrollRef={scrollRef} regions={regions} regionRows={regionRows} onJump={scrollToRegion} streamKey={`${selectedFile?.path ?? ""}:${patchView}:${rows.length}`} />}</div></section>;

  // The stream renders only when a file, patch, or file content is actually
  // present; every other body state above replaces it wholesale.
  function streamReady() {
    return Boolean(selectedFile) && !patchLoading && !patchError && !patch?.binary && patch?.text !== "" && (fileMode ? !contentLoading && !contentError && content !== null && !content.binary && rows.length > 0 : hunks.length > 0);
  }
}

// One hunk gap's inline expand control, joined into the stream. Small gaps
// collapse to a slim one-line row; while lines are loading it shows
// progress, and a failed content read retries through the same click.
function ExpandGapRow({ gap, slim, pending, error, onExpand }: { gap: PatchGap; slim: boolean; pending: boolean; error: boolean; onExpand: () => void }) {
  const label = `Expand ${gap.lines} hidden line${gap.lines === 1 ? "" : "s"}`;
  return <button className={`expand-gap${slim ? " slim" : ""}`} type="button" disabled={pending} aria-label={label} title={error ? "File content is unavailable" : label} onClick={onExpand}>{pending ? "Loading hidden lines…" : error ? "Hidden lines unavailable, click to retry" : <><UnfoldVertical size={slim ? 10 : 12} />{label}</>}</button>;
}

// The pane heading's change stepper. It derives the current region from the
// live scroll position, so the counter, the enabled states, and the
// movement can never disagree.
function ChangeNav({ scrollRef, regions, regionRows, onStep, streamKey }: { scrollRef: React.RefObject<HTMLDivElement | null>; regions: ChangeRegion[]; regionRows: (number | null)[]; onStep: (index: number) => void; streamKey: string }) {
  const [current, setCurrent] = useState(-1);
  const frame = useRef(0);
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    // Region tops only move when layout changes, so they are measured up
    // front and the scroll path compares plain numbers.
    let marks: Array<{ index: number; top: number }> = [];
    let layoutStale = true;
    const measure = () => {
      marks = [];
      for (let i = 0; i < regionRows.length; i += 1) {
        const row = regionRows[i];
        if (row === null || row === undefined) continue;
        const element = el.querySelector(`[data-index="${row}"]`);
        if (element instanceof HTMLElement) marks.push({ index: i, top: element.offsetTop - STREAM_TOP_PADDING });
      }
    };
    const update = () => {
      frame.current = 0;
      if (layoutStale) { layoutStale = false; measure(); }
      let index = -1;
      for (const mark of marks) { if (mark.top <= el.scrollTop) index = mark.index; else break; }
      // At max scroll the last region is on screen even when its start row
      // sits above the clamp point, so the counter must reach it.
      if (marks.length > 0 && el.scrollTop >= el.scrollHeight - el.clientHeight - 1) index = marks[marks.length - 1].index;
      setCurrent(index);
    };
    const schedule = () => { if (!frame.current) frame.current = requestAnimationFrame(update); };
    const markLayout = () => { layoutStale = true; schedule(); };
    update();
    el.addEventListener("scroll", schedule, { passive: true });
    // A resize rewraps rows (wrap mode) and shifts offsets without any
    // scroll event, so re-derive from live layout then too. Observing the
    // element covers app-internal resizes (pane collapse), not just window
    // edges; the content element covers reflows that leave the pane's own
    // size alone (wrap toggle, zoom).
    window.addEventListener("resize", markLayout);
    let observer: ResizeObserver | null = null;
    if (typeof ResizeObserver !== "undefined") {
      observer = new ResizeObserver(markLayout);
      observer.observe(el);
      if (el.firstElementChild) observer.observe(el.firstElementChild);
    }
    return () => {
      el.removeEventListener("scroll", schedule);
      window.removeEventListener("resize", markLayout);
      observer?.disconnect();
      cancelAnimationFrame(frame.current);
    };
  }, [regionRows, scrollRef, streamKey]);
  const step = (direction: 1 | -1) => {
    const index = current + direction;
    if (index >= 0 && index < regions.length) onStep(index);
  };
  return <div className="change-nav">
    <button type="button" className="icon-button" aria-label="Previous change" title="Previous change" disabled={current < 1} onClick={() => step(-1)}><ChevronUp size={12} /></button>
    <span className="change-count" title="Change under the viewport">{Math.min(Math.max(current + 1, 1), regions.length)} / {regions.length}</span>
    <button type="button" className="icon-button" aria-label="Next change" title="Next change" disabled={current >= regions.length - 1} onClick={() => step(1)}><ChevronDown size={12} /></button>
  </div>;
}

// A thin fixed rail mapping where the patch's changes sit in the stream;
// one click jumps to a change. Positions come from the real DOM, so they
// are exact without a height model.
// The rail hugs the native scrollbar's left edge (the JS right offset keeps
// the scrollbar itself clickable) and shows in the File view only. It is a
// minimap: every tick sits at its change's fraction of the whole stream and
// never moves on scroll; only the band tracks the viewport.
function ChangeStrip({ scrollRef, regions, regionRows, onJump, streamKey }: { scrollRef: React.RefObject<HTMLDivElement | null>; regions: ChangeRegion[]; regionRows: (number | null)[]; onJump: (index: number) => void; streamKey: string }) {
  const railRef = useRef<HTMLDivElement | null>(null);
  const bandRef = useRef<HTMLDivElement | null>(null);
  const frame = useRef(0);
  const [ticks, setTicks] = useState<Array<{ key: string; frac: number; added: boolean; title: string; index: number; line: number }>>([]);
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    // The band is all that moves on scroll: two style writes, nothing else.
    const updateBand = () => {
      const band = bandRef.current;
      if (!band || el.scrollHeight <= 0) return;
      band.style.top = `${(el.scrollTop / el.scrollHeight) * 100}%`;
      band.style.height = `${Math.min(100, (el.clientHeight / el.scrollHeight) * 100)}%`;
    };
    // Ticks and the scrollbar-hugging offset depend on layout alone, so
    // they re-measure on layout changes, never in the scroll path.
    let layoutStale = true;
    const update = () => {
      frame.current = 0;
      const rail = railRef.current;
      updateBand();
      if (!rail || !layoutStale) return;
      layoutStale = false;
      // Hug the native scrollbar's left edge (offsetWidth - clientWidth is
      // the scrollbar's occupied width; scrollbar-gutter keeps it stable).
      rail.style.right = `${el.offsetWidth - el.clientWidth}px`;
      const extent = Math.max(1, el.scrollHeight - STREAM_TOP_PADDING * 2);
      setTicks(regions.flatMap((region, index) => {
        const row = regionRows[index];
        if (row === null || row === undefined) return [];
        const rowEl = el.querySelector(`[data-index="${row}"]`);
        if (!(rowEl instanceof HTMLElement)) return [];
        const frac = Math.min(1, Math.max(0, (rowEl.offsetTop - STREAM_TOP_PADDING) / extent));
        return [{ key: `${region.start}:${region.end}`, frac, added: region.added, title: `Change at line ${region.start}`, index, line: region.start }];
      }));
    };
    const schedule = () => { if (!frame.current) frame.current = requestAnimationFrame(update); };
    const markLayout = () => { layoutStale = true; schedule(); };
    update();
    el.addEventListener("scroll", updateBand, { passive: true });
    window.addEventListener("resize", markLayout);
    // The scroll container's own box misses reflows that move rows (wrap
    // toggle, zoom), so observe the content element too.
    let observer: ResizeObserver | null = null;
    if (typeof ResizeObserver !== "undefined") {
      observer = new ResizeObserver(markLayout);
      observer.observe(el);
      if (el.firstElementChild) observer.observe(el.firstElementChild);
    }
    return () => {
      el.removeEventListener("scroll", updateBand);
      window.removeEventListener("resize", markLayout);
      observer?.disconnect();
      cancelAnimationFrame(frame.current);
    };
  }, [regions, regionRows, scrollRef, streamKey]);
  return <div ref={railRef} className="change-strip" role="group" aria-label="Change map">
    <div ref={bandRef} className="strip-view" />
    {ticks.map((tick) => <button key={tick.key} type="button" className={`strip-tick ${tick.added ? "added" : "deleted"}`} style={{ top: `${tick.frac * 100}%` }} title={tick.title} aria-label={`Jump to change at line ${tick.line}`} onClick={() => onJump(tick.index)} />)}
  </div>;
}

function DiffHalf({ placement, side, gutter, text, whitespace, tokens, commentsActive, commentable = true, selected, chip, onRowClick, onGutterDown, onEnter }: { placement: "left" | "right"; side: DisplaySide; gutter: number | null; text: string; whitespace: boolean; tokens?: TokenLine; commentsActive: boolean; commentable?: boolean; selected: string; chip: React.ReactNode; onRowClick: (side: DisplaySide, number: number, extend: boolean) => void; onGutterDown: (side: DisplaySide, number: number) => void; onEnter: (side: DisplaySide, number: number) => void }) {
  // Expanded context rows keep their comment cards visible but accept no new
  // anchors: the comment layer validates against patch lines, which do not
  // include expanded rows.
  const target = commentsActive && gutter !== null && commentable;
  return <div className={`diff-line half half-${placement} ${text.startsWith("+") ? "addition" : text.startsWith("-") ? "deletion" : ""}${target ? " commentable" : ""}${selected}`} data-side={target ? side : undefined} data-line={target ? gutter : undefined} onClick={target ? (event) => onRowClick(side, gutter, event.shiftKey) : undefined} onMouseEnter={target ? () => onEnter(side, gutter) : undefined}><span className="line-number" onMouseDown={target ? (event) => { event.preventDefault(); onGutterDown(side, gutter); } : undefined}>{gutter ?? ""}</span><code>{diffLineContent(text, whitespace, tokens)}</code>{chip}</div>;
}

export function DiffToggles({ settings, onChange }: { settings: Settings; onChange: (next: Settings) => void }) {
  return <div className="diff-toggles" role="group" aria-label="Diff display options">
    <button className={`icon-button ${settings.syntax_visible ? "active" : ""}`} type="button" aria-pressed={settings.syntax_visible} aria-label="Syntax highlighting" title="Syntax highlighting" onClick={() => onChange({ ...settings, syntax_visible: !settings.syntax_visible })}><Code size={12} /></button>
    <button className={`icon-button ${settings.diff_layout === "split" ? "active" : ""}`} type="button" aria-pressed={settings.diff_layout === "split"} aria-label="Split diff layout" title="Split diff layout" onClick={() => onChange({ ...settings, diff_layout: settings.diff_layout === "split" ? "unified" : "split" })}><Columns2 size={12} /></button>
    <button className={`icon-button ${settings.whitespace_visible ? "active" : ""}`} type="button" aria-pressed={settings.whitespace_visible} aria-label="Visible whitespace" title="Visible whitespace" onClick={() => onChange({ ...settings, whitespace_visible: !settings.whitespace_visible })}><Space size={12} /></button>
    <button className={`icon-button ${settings.line_wrap ? "active" : ""}`} type="button" aria-pressed={settings.line_wrap} aria-label="Wrap lines" title="Wrap lines" onClick={() => onChange({ ...settings, line_wrap: !settings.line_wrap })}><WrapText size={12} /></button>
    <button className={`icon-button ${settings.inline_comments_visible ? "active" : ""}`} type="button" aria-pressed={settings.inline_comments_visible} aria-label="Inline comments" title="Inline comments in the diff" onClick={() => onChange({ ...settings, inline_comments_visible: !settings.inline_comments_visible })}><MessageSquare size={12} /></button>
  </div>;
}

