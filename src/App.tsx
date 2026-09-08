import { useEffect, useMemo, useRef, useState, useSyncExternalStore, Fragment, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { ArrowLeft, ArrowRight, Check, ChevronDown, ChevronRight, ChevronsLeft, CircleDot, Code, Columns2, Command, Copy, ExternalLink, FileDiff, FolderGit2, FolderOpen, GitBranch, HardDrive, Inbox, ListTree, MessageSquare, MoreVertical, Pin, PinOff, RefreshCw, Search, Settings as SettingsIcon, Space, Trash2, WrapText, X } from "lucide-react";
import { createNavigationHistory, sameReviewTarget, type AppLocation, type BranchInventory, type BranchSummary, type ChangedFile, type CommitInfo, type GoneSurface, type RefInventory, type ReviewIdentity, type ReviewScope, type ReviewTarget, type SurfaceListing, type Worktree } from "./navigation";
import { autoReviewBase, workingChangesBase, type WorktreeReviewPreset } from "./reviewPresets";
import { SettingsPage, applyTheme, defaultSettings, getSettings, persistSettings, type DiffLayout, type Settings } from "./settings";
import { DEFAULT_ZOOM, snapZoom, stepZoom, zoomShortcut } from "./zoom";
import { pairHunkLines, type DiffLine } from "./diff";
import { hunkSideSources, languageForPath, splitWhitespace, tokenizeHunk, type HighlightToken, type TokenLine } from "./highlight";
import { filterGoneSurfaces, goneSurfaceLabel, pinnedSurfaces, recentWorktrees, reviewFileWorktree, surfacePinIndex, surfaceRows, worktreeKey, type SurfaceRow } from "./surfaces";
import { CommentStream, CommentThreadView, DraftComposer, InlineCommentComposer, inlineCards, selectableRow, useReviewComments, type CommentsApi } from "./comments.tsx";
import { ReviewsStrip } from "./canvas.tsx";
import { copyText } from "./clipboard";
import type { CommentSelection, DisplaySide } from "./comments";
import "./App.css";

type Repo = { path: string; name: string; worktrees: Worktree[]; pinned_at: number | null };
type CommandError = { code: string; message: string };
type SearchResult = { repo: Repo; worktree?: Worktree };
type ReviewIndex = { files: ChangedFile[]; additions: number; deletions: number; base_sha: string; target_sha: string; error?: string; error_code?: string };
type FilePatch = { binary: boolean; text: string };
type CommitPage = { commits: CommitInfo[]; has_more: boolean };
type HistoryEntry = { repoPath: string; startPointLabel: string; worktreePath?: string; startRef?: string };
type HistoryState = HistoryEntry & { commits: CommitInfo[]; hasMore: boolean; loading: boolean; error: string };
type WorktreeStatus = { path: string; changes: number | null };
type ParsedHunk = { header: string; lines: DiffLine[] };
type RenderedHunk = { header: string; lines: DiffLine[]; hunkIndex: number };
type DiffPreferences = { layout: DiffLayout; whitespaceVisible: boolean; lineWrap: boolean; syntaxVisible: boolean };
type OverviewTab = "worktrees" | "branches" | "remote" | "archived";
// Mirrors the payload of the Rust `submission-received` event.
type SubmissionArrival = { repo_path: string; base_sha: string; target_key: string; target_kind: "worktree" | "head"; submission_id: number; agent_name: string };

const WORKTREE_PAGE_SIZE = 100;
const SEARCH_PAGE_SIZE = 50;
const HUNKS_PAGE_SIZE = 50;
const BRANCH_PAGE_SIZE = 50;
const PATCH_LINE_PAGE_SIZE = 500;
const COMMIT_PAGE_SIZE = 100;
const PATCH_CACHE_LIMIT = 32;
// Sidebar children stay scannable: pinned surfaces plus the most recently
// committed worktrees; everything else lives on the project overview.
const SIDEBAR_WORKTREE_LIMIT = 6;
const OVERVIEW_TABS: Array<{ id: OverviewTab; label: string; filter: string; pager: string; pageSize: number }> = [
  { id: "worktrees", label: "Worktrees", filter: "Filter worktrees", pager: "Worktree pages", pageSize: WORKTREE_PAGE_SIZE },
  { id: "branches", label: "Branches", filter: "Filter branches", pager: "Branch pages", pageSize: BRANCH_PAGE_SIZE },
  { id: "remote", label: "Remote", filter: "Filter remote branches", pager: "Remote branch pages", pageSize: BRANCH_PAGE_SIZE },
  { id: "archived", label: "Archived", filter: "Filter archived", pager: "Archived pages", pageSize: BRANCH_PAGE_SIZE },
];

// Zoom is applied to the whole webview; a failure (browser preview, refused
// call) keeps the current scale, so the call is best-effort.
async function applyZoom(zoom: number) {
  try { await getCurrentWebview().setZoom(zoom); } catch { /* keep the current zoom */ }
}

function errorMessage(error: unknown) {
  if (typeof error === "object" && error !== null && "code" in error) {
    const commandError = error as CommandError;
    if (commandError.code === "not_git_repository" || commandError.code === "invalid_path") return commandError.message;
    if (commandError.code === "persistence") return "Repository storage is unavailable.";
    if (commandError.code === "git_timeout") return "Git took too long to respond.";
    if (commandError.code === "git_output_too_large") return "Git returned too much worktree data.";
    if (commandError.code === "git_output_malformed") return "Git returned malformed worktree data.";
    if (commandError.code === "git_filter_unsupported") return "This review cannot run because Git conversion filters apply to files in this review.";
    if (commandError.code === "git_execution") return "Git could not inspect this repository.";
    if (commandError.code === "content_unavailable") return "This surface's content is no longer available in the repository.";
    if (commandError.code === "scope_requires_worktree") return "All changes scope requires the worktree's checked-out state.";
    if (commandError.code === "unresolvable_ref") return commandError.message;
    if (commandError.code === "invalid_submission") return commandError.message;
    if (commandError.code === "unknown_review_target") return commandError.message;
  }
  if (typeof error === "object" && error !== null && "message" in error) return String(error.message);
  return "The repository operation failed.";
}

function errorCodeOf(error: unknown) {
  return typeof error === "object" && error !== null && "code" in error ? String((error as CommandError).code) : "";
}

// Mirrors the Rust u32::MAX sentinel for status counts that exceeded the
// output bound.
const STATUS_COUNT_OVERFLOW = 4294967295;

function changesLabel(changes: number | null | undefined) {
  if (changes === null || changes === undefined) return "";
  if (changes === 0) return "Clean";
  if (changes === STATUS_COUNT_OVERFLOW) return "9999+ changed";
  return `${changes} changed`;
}

function shortToken(token: string) {
  if (token.startsWith("refs/heads/")) return token.slice("refs/heads/".length);
  if (token.startsWith("refs/remotes/")) return token.slice("refs/remotes/".length);
  if (token.startsWith("refs/tags/")) return token.slice("refs/tags/".length);
  return /^[0-9a-f]{40}$/i.test(token) ? token.slice(0, 7) : token;
}

// Branches untouched for a week read as stale; agent worktrees often are.
const STALE_AFTER_SECONDS = 7 * 24 * 60 * 60;

function relativeTime(unixSeconds: number) {
  const seconds = Math.floor(Date.now() / 1000) - unixSeconds;
  if (seconds < 45) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d ago`;
  return new Date(unixSeconds * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

// host + path display form of a remote URL; copying always uses the full URL.
function originSlug(url: string) {
  return url.replace(/\.git$/i, "").replace(/^(https?|ssh|git):\/\//i, "").replace(/^[^/@]+@/, "").replace(":", "/");
}

function CopyButton({ value, label, ghost = false }: { value: string; label: string; ghost?: boolean }) {
  const [copied, setCopied] = useState(false);
  return <button className={`copy-button ${ghost ? "ghost" : ""} ${copied ? "copied" : ""}`} type="button" aria-label={label} title={copied ? "Copied" : label} onClick={async (event) => { event.stopPropagation(); if (await copyText(value)) { setCopied(true); setTimeout(() => setCopied(false), 1200); } }}>{copied ? <Check size={12} /> : <Copy size={12} />}</button>;
}
function commitTargetOf(commit: CommitInfo): ReviewTarget { return { kind: "commit", sha: commit.sha, parents: commit.parents, defaultBaseAncestor: commit.default_base_ancestor }; }
// The commits bar follows the active ref: the history surface's start point,
// the reviewed worktree or branch, or the selected inbox worktree. A commit
// review holds whatever ref the commit was reached from, so the bar keeps
// showing that ref's history instead of narrowing to the commit's ancestors.
function historyAnchorOf(location: AppLocation, activeRepo: Repo | undefined, selectedWorktreePath: string): HistoryEntry | null {
  if (location.kind === "commit-history") return { repoPath: location.repoPath, startPointLabel: location.startPointLabel, worktreePath: location.worktreePath ?? undefined, startRef: location.startRef ?? undefined };
  if (location.kind === "review") {
    const target = location.identity.target;
    if (target.kind === "worktree") return { repoPath: location.identity.repoPath, startPointLabel: shortToken(target.worktree.branch), worktreePath: target.worktree.path };
    if (target.kind === "ref") return { repoPath: location.identity.repoPath, startPointLabel: shortToken(target.name), startRef: target.name };
    return null;
  }
  if (location.kind === "inbox" && activeRepo) {
    const worktree = activeRepo.worktrees.find((item) => item.path === selectedWorktreePath) ?? activeRepo.worktrees[0];
    return worktree ? { repoPath: activeRepo.path, startPointLabel: shortToken(worktree.branch), worktreePath: worktree.path } : null;
  }
  return null;
}
function historyKeyOf(entry: { repoPath: string; startPointLabel: string; worktreePath?: string; startRef?: string } | null) { return entry ? `${entry.repoPath}\0${entry.startPointLabel}\0${entry.worktreePath ?? ""}\0${entry.startRef ?? ""}` : ""; }
function parseHunks(text: string) {
  const lines = text.split("\n");
  const hunks: ParsedHunk[] = [];
  let current: ParsedHunk | null = null;
  let oldLine = 0;
  let newLine = 0;
  for (const line of lines) {
    const header = line.match(/^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
    if (header) {
      current = { header: line, lines: [] };
      hunks.push(current);
      oldLine = Number(header[1]);
      newLine = Number(header[2]);
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

function paginateHunks(hunks: ParsedHunk[]) {  const pages: RenderedHunk[][] = [];
  let page: RenderedHunk[] = [];
  let lineCount = 0;
  const finishPage = () => {
    if (page.length > 0) pages.push(page);
    page = [];
    lineCount = 0;
  };
  for (const [hunkIndex, hunk] of hunks.entries()) {
    const lines = hunk.lines;
    if (lines.length === 0) {
      if (page.length === HUNKS_PAGE_SIZE) finishPage();
      page.push({ header: hunk.header, lines: [], hunkIndex });
      continue;
    }
    for (let offset = 0; offset < lines.length;) {
      if (page.length === HUNKS_PAGE_SIZE || lineCount === PATCH_LINE_PAGE_SIZE) finishPage();
      const count = Math.min(lines.length - offset, PATCH_LINE_PAGE_SIZE - lineCount);
       page.push({ header: hunk.header, lines: lines.slice(offset, offset + count), hunkIndex });
      offset += count;
      lineCount += count;
    }
  }
  finishPage();
  return pages;
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

function sameReview(left: ReviewIdentity | null, right: ReviewIdentity) {
  return left !== null
    && left.repoPath === right.repoPath
    && left.base === right.base
    && sameReviewTarget(left.target, right.target)
    && left.scope === right.scope
    && left.reversed === right.reversed;
}

function patchIdentityOf(identity: ReviewIdentity, file: ChangedFile) {
  const target = identity.target;
  return `${identity.repoPath}\0${target.kind === "ref" ? target.name : target.kind === "commit" ? target.sha : target.worktree.path}\0${identity.base}\0${identity.scope}\0${identity.reversed}\0${file.path}\0${file.untracked}`;
}

function App() {
  const [repos, setRepos] = useState<Repo[]>([]); const [activeRepoPath, setActiveRepoPath] = useState(""); const [selectedWorktreePath, setSelectedWorktreePath] = useState("");
  const [loading, setLoading] = useState(true); const [opening, setOpening] = useState(false); const [loadError, setLoadError] = useState(""); const [operationError, setOperationError] = useState(""); const [repoErrors, setRepoErrors] = useState<Record<string, string>>({}); const [hydratingRepos, setHydratingRepos] = useState<Record<string, number>>({});
  const [collapsed, setCollapsed] = useState(false); const [paletteOpen, setPaletteOpen] = useState(false); const [query, setQuery] = useState(""); const [searchPage, setSearchPage] = useState(0);
  const [refs, setRefs] = useState<RefInventory>({ heads: [], remotes: [], tags: [], default_base: null }); const [reviewIndex, setReviewIndex] = useState<ReviewIndex | null>(null); const [reviewLoading, setReviewLoading] = useState(false); const [patch, setPatch] = useState<FilePatch | null>(null); const [patchError, setPatchError] = useState(""); const [patchLoading, setPatchLoading] = useState(false);
  const [expandedRepos, setExpandedRepos] = useState<Record<string, boolean>>({}); const [surfaces, setSurfaces] = useState<Record<string, SurfaceListing>>({}); const [overviewTab, setOverviewTab] = useState<OverviewTab>("worktrees"); const [overviewQuery, setOverviewQuery] = useState(""); const [overviewPage, setOverviewPage] = useState(0); const [fetchingRepos, setFetchingRepos] = useState<Record<string, boolean>>({});
  const [history, setHistory] = useState<HistoryState | null>(null); const [historyRefs, setHistoryRefs] = useState<RefInventory>({ heads: [], remotes: [], tags: [], default_base: null }); const [worktreeStatuses, setWorktreeStatuses] = useState<Record<string, WorktreeStatus[] | null>>({}); const [statusNonce, setStatusNonce] = useState(0);
  const [inventories, setInventories] = useState<Record<string, BranchInventory | null>>({}); const [menuOpen, setMenuOpen] = useState(false); const [removeTarget, setRemoveTarget] = useState<Repo | null>(null); const [removing, setRemoving] = useState(false);
  const [settings, setSettings] = useState<Settings>(defaultSettings); const [settingsSaveError, setSettingsSaveError] = useState("");
  const [arrivals, setArrivals] = useState<SubmissionArrival[]>([]);
  const paletteRef = useRef<HTMLDialogElement>(null); const paletteOpenerRef = useRef<HTMLElement | null>(null);
  const menuAnchorRef = useRef<HTMLDivElement | null>(null); const confirmRef = useRef<HTMLDialogElement>(null);
  const indexGenerationRef = useRef(0); const patchGenerationRef = useRef(0); const reviewIdentityRef = useRef<ReviewIdentity | null>(null); const patchIdentityRef = useRef(""); const patchCacheRef = useRef(new Map<string, FilePatch>()); const historyGenerationRef = useRef(0); const lastBaseRef = useRef<{ repoPath: string; base: string } | null>(null); const refsIdentityRef = useRef<ReviewIdentity | null>(null); const navigateRef = useRef<(delta: number) => void>(() => undefined); const zoomActionsRef = useRef<{ step: (direction: 1 | -1) => void; reset: () => void }>({ step: () => undefined, reset: () => undefined }); const zoomLiveRef = useRef(DEFAULT_ZOOM);
  const [nav] = useState(createNavigationHistory);
  const location = useSyncExternalStore(nav.subscribe, nav.current);
  const reviewLocation = location.kind === "review" ? location : null;
  const historyLocation = location.kind === "commit-history" ? location : null;
  const settingsLocation = location.kind === "settings" ? location : null;
  const reviewTarget = reviewLocation?.identity.target ?? null;
  const base = reviewLocation?.identity.base ?? "";
  const scope = reviewLocation?.identity.scope ?? "all";
  const reversed = reviewLocation?.identity.reversed ?? false;
  // Opening or retargeting a review pushes its location before list_refs
  // resolves the base; holding the review's previous base keeps the base
  // picker and swap control from flashing "No base selected" in between.
  const displayBase = base || (reviewLoading && lastBaseRef.current?.repoPath === reviewLocation?.identity.repoPath ? lastBaseRef.current?.base ?? "" : "");
  const selectedFile = reviewLocation?.selectedFile ?? historyLocation?.selectedFile ?? null;
  const filePage = reviewLocation?.filePage ?? historyLocation?.filePage ?? 0;
  const hunkPage = reviewLocation?.hunkPage ?? historyLocation?.hunkPage ?? 0;
  const activeRepo = repos.find((repo) => repo.path === activeRepoPath); const hydrating = Object.keys(hydratingRepos).length > 0; const activeRepoHydrating = activeRepo ? Boolean(hydratingRepos[activeRepo.path]) : false;
  const historyAnchor = historyAnchorOf(location, activeRepo, selectedWorktreePath); const historyAnchorKey = historyKeyOf(historyAnchor);
  // The comment layer attaches to whichever review identity is rendered:
  // the review surface's identity, or the commit history quick-look's.
  const commentIdentity: ReviewIdentity | null = reviewLocation?.identity ?? (historyLocation?.selectedCommit ? { repoPath: historyLocation.repoPath, base: historyLocation.selectedCommit.parents[0] ?? "empty-tree", target: commitTargetOf(historyLocation.selectedCommit), scope: "committed", reversed: false } : null);
  const commentPatchLines = useMemo(() => (patch && !patch.binary ? parseHunks(patch.text).flatMap((hunk) => hunk.lines) : []), [patch]);
  const commentFile = selectedFile && patch && !patch.binary ? { path: selectedFile.path, lines: commentPatchLines } : null;
  const comments = useReviewComments(commentIdentity, reviewIndex, commentFile, reversed);

  useEffect(() => { let mounted = true; async function load() { try { const loaded = await invoke<Repo[]>("list_repos"); if (!mounted) return; setRepos(loaded); setActiveRepoPath((current) => loaded.some((repo) => repo.path === current) ? current : loaded[0]?.path ?? ""); setHydratingRepos(Object.fromEntries(loaded.map((repo) => [repo.path, 1]))); let nextSettings = defaultSettings; try { nextSettings = await getSettings(); } catch (error) { if (mounted) setOperationError(errorMessage(error)); } if (!mounted) return; nextSettings.zoom = snapZoom(nextSettings.zoom); applyTheme(nextSettings.theme); setSettings(nextSettings); setLoading(false); for (let start = 0; start < loaded.length; start += 4) await Promise.all(loaded.slice(start, start + 4).map(async (repo) => { try { const worktrees = await invoke<Worktree[]>("list_worktrees", { path: repo.path }); if (mounted) setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, worktrees } : item)); let listing: SurfaceListing = { gone: [], pinned: [] }; try { listing = await invoke<SurfaceListing>("list_surfaces", { path: repo.path }); } catch { listing = { gone: [], pinned: [] }; } if (mounted) setSurfaces((current) => ({ ...current, [repo.path]: listing })); } catch (error) { if (mounted) setRepoErrors((current) => ({ ...current, [repo.path]: errorMessage(error) })); } finally { if (mounted) setHydratingRepos((current) => { const count = current[repo.path] ?? 0; if (count > 1) return { ...current, [repo.path]: count - 1 }; const next = { ...current }; delete next[repo.path]; return next; }); } })); } catch (error) { if (mounted) { setLoadError(errorMessage(error)); setLoading(false); } } } void load(); return () => { mounted = false; }; }, []);
  useEffect(() => { if (!activeRepo) { setSelectedWorktreePath(""); return; } setSelectedWorktreePath((current) => activeRepo.worktrees.some((worktree) => worktree.path === current) ? current : activeRepo.worktrees[0]?.path ?? ""); }, [activeRepo]);
  // The overview's change badges come from one bounded status probe per
  // worktree; they refresh when the repo activates, its worktree count
  // changes, or the overview's refresh action asks for a fresh pass.
  useEffect(() => {
    const repoPath = activeRepoPath;
    const worktreeCount = activeRepo?.worktrees.length ?? 0;
    if (!repoPath || worktreeCount === 0) return;
    let cancelled = false;
    void (async () => {
      try {
        const statuses = await invoke<WorktreeStatus[]>("list_worktree_status", { path: repoPath });
        if (!cancelled) setWorktreeStatuses((current) => ({ ...current, [repoPath]: statuses }));
      } catch {
        if (!cancelled) setWorktreeStatuses((current) => ({ ...current, [repoPath]: null }));
      }
    })();
    return () => { cancelled = true; };
  }, [activeRepoPath, activeRepo?.worktrees.length, statusNonce]);
  // The overview's history and sync columns come from one for-each-ref pass
  // per repo; they share the status probe's refresh triggers.
  useEffect(() => {
    const repoPath = activeRepoPath;
    if (!repoPath) return;
    let cancelled = false;
    void (async () => {
      try {
        const fetched = await invoke<BranchInventory>("get_branch_inventory", { path: repoPath });
        if (!cancelled) setInventories((current) => ({ ...current, [repoPath]: fetched }));
      } catch {
        if (!cancelled) setInventories((current) => ({ ...current, [repoPath]: null }));
      }
    })();
    return () => { cancelled = true; };
  }, [activeRepoPath, statusNonce]);
  useEffect(() => { setOverviewTab("worktrees"); setOverviewQuery(""); setOverviewPage(0); }, [activeRepoPath]);
  useEffect(() => {
    if (!menuOpen) return;
    function handleDismiss(event: MouseEvent) {
      if (menuAnchorRef.current && event.target instanceof Node && menuAnchorRef.current.contains(event.target)) return;
      setMenuOpen(false);
    }
    function handleKeyDown(event: KeyboardEvent) { if (event.key === "Escape") setMenuOpen(false); }
    document.addEventListener("mousedown", handleDismiss);
    document.addEventListener("keydown", handleKeyDown);
    return () => { document.removeEventListener("mousedown", handleDismiss); document.removeEventListener("keydown", handleKeyDown); };
  }, [menuOpen]);
  useEffect(() => { const dialog = confirmRef.current; if (removeTarget && dialog && !dialog.open) dialog.showModal(); }, [removeTarget]);
  useEffect(() => { navigateRef.current = navigate; zoomActionsRef.current = { step: (direction) => void changeZoom(stepZoom(zoomLiveRef.current, direction)), reset: () => void changeZoom(DEFAULT_ZOOM) }; });
  // Applies the zoom whenever the persisted preference changes and keeps
  // the live level in sync; startup application happens when the loaded
  // settings replace the defaults.
  useEffect(() => { zoomLiveRef.current = settings.zoom; void applyZoom(settings.zoom); }, [settings.zoom]);
  // Keeps the root palette on the persisted preference and, while the
  // preference is system, tracks live OS scheme changes. Forced themes
  // detach from the OS entirely.
  useEffect(() => {
    applyTheme(settings.theme);
    if (settings.theme !== "system") return;
    const query = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = () => applyTheme("system");
    query.addEventListener("change", onChange);
    return () => query.removeEventListener("change", onChange);
  }, [settings.theme]);
  useEffect(() => { function handleKeyboard(event: KeyboardEvent) { if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") { event.preventDefault(); paletteRef.current?.open ? closePalette() : openPalette(); return; } if ((event.ctrlKey || event.metaKey) && !event.altKey) { const shortcut = zoomShortcut(event.key); if (shortcut) { event.preventDefault(); if (shortcut === "reset") zoomActionsRef.current.reset(); else zoomActionsRef.current.step(shortcut === "in" ? 1 : -1); return; } } if (!event.altKey || (event.key !== "ArrowLeft" && event.key !== "ArrowRight") || paletteRef.current?.open) return; event.preventDefault(); navigateRef.current(event.key === "ArrowLeft" ? -1 : 1); } window.addEventListener("keydown", handleKeyboard); return () => window.removeEventListener("keydown", handleKeyboard); }, []);
  // Ctrl+wheel steps zoom like a browser; trackpad pinch delivers the same
  // ctrl+wheel events. The accumulator turns smooth deltas into discrete
  // steps and resets after a pause.
  useEffect(() => { let accumulated = 0; let idleAt = 0; function handleWheel(event: WheelEvent) { if (!event.ctrlKey || event.defaultPrevented) return; event.preventDefault(); const now = Date.now(); if (now - idleAt > 400) accumulated = 0; idleAt = now; accumulated += event.deltaY; if (Math.abs(accumulated) >= 100) { zoomActionsRef.current.step(accumulated < 0 ? 1 : -1); accumulated = 0; } } window.addEventListener("wheel", handleWheel, { passive: false }); return () => window.removeEventListener("wheel", handleWheel); }, []);
  useEffect(() => { const dialog = paletteRef.current; if (paletteOpen && dialog && !dialog.open) dialog.showModal(); }, [paletteOpen]);
  // Agent submissions arrive through the loopback endpoint in Rust, which
  // announces each accepted delivery to the webview as an event.
  useEffect(() => {
    let disposed = false;
    const subscription = listen<SubmissionArrival>("submission-received", (event) => {
      if (!disposed) setArrivals((current) => [...current, event.payload]);
    });
    return () => { disposed = true; void subscription.then((unsubscribe) => unsubscribe()); };
  }, []);
  useEffect(() => {
    if (reviewTarget || !historyLocation) return;
    const selected = historyLocation.selectedCommit;
    if (!selected) return;
    const quickBase = selected.parents[0] ?? "empty-tree";
    const identity: ReviewIdentity = { repoPath: historyLocation.repoPath, base: quickBase, target: commitTargetOf(selected), scope: "committed", reversed: false };
    if (sameReview(reviewIdentityRef.current, identity) && (reviewIndex !== null || reviewLoading)) return;
    void fetchIndex(identity.target, quickBase, "committed", false, historyLocation.repoPath);
  }, [reviewTarget, historyLocation, reviewIndex, reviewLoading]);
  useEffect(() => {
    if (!reviewLocation || !reviewLocation.selectedFile) return;
    if (reviewLoading || reviewIndex === null) return;
    const identity = reviewLocation.identity;
    const file = reviewLocation.selectedFile;
    if (patchIdentityRef.current === patchIdentityOf(identity, file)) return;
    void selectFile(file, identity);
  }, [reviewLocation, reviewLoading, reviewIndex]);
  useEffect(() => {
    if (!historyLocation || !historyLocation.selectedCommit || !historyLocation.selectedFile) return;
    if (reviewLoading || reviewIndex === null) return;
    const selected = historyLocation.selectedCommit;
    const file = historyLocation.selectedFile;
    const identity: ReviewIdentity = { repoPath: historyLocation.repoPath, base: selected.parents[0] ?? "empty-tree", target: commitTargetOf(selected), scope: "committed", reversed: false };
    if (patchIdentityRef.current === patchIdentityOf(identity, file)) return;
    void selectFile(file, identity);
  }, [historyLocation, reviewLoading, reviewIndex]);
  useEffect(() => {
    if (!reviewLocation || !reviewLocation.identity.base) return;
    const identity = reviewLocation.identity;
    if (sameReview(reviewIdentityRef.current, identity) && (reviewIndex !== null || reviewLoading)) return;
    void fetchIndex(identity.target, identity.base, identity.scope, identity.reversed, identity.repoPath);
  }, [reviewLocation, reviewIndex, reviewLoading]);
  useEffect(() => {
    if (reviewLocation && reviewLocation.identity.base) lastBaseRef.current = { repoPath: reviewLocation.identity.repoPath, base: reviewLocation.identity.base };
  }, [reviewLocation]);
  useEffect(() => {
    if (!historyAnchor) return;
    if (history && history.repoPath === historyAnchor.repoPath && history.startPointLabel === historyAnchor.startPointLabel && (history.worktreePath ?? null) === (historyAnchor.worktreePath ?? null) && (history.startRef ?? null) === (historyAnchor.startRef ?? null)) return;
    void loadHistorySurface(historyAnchor);
  }, [historyAnchorKey, history]);

  function openPalette(opener?: HTMLElement | null) { paletteOpenerRef.current = opener ?? (document.activeElement instanceof HTMLElement ? document.activeElement : null); setPaletteOpen(true); }
  function closePalette() { if (paletteRef.current?.open) paletteRef.current.close(); else handlePaletteClosed(); }
  function handlePaletteClosed() { setPaletteOpen(false); setQuery(""); setSearchPage(0); paletteOpenerRef.current?.focus(); paletteOpenerRef.current = null; }
  function handlePaletteKeyDown(event: ReactKeyboardEvent<HTMLDialogElement>) { if (event.key !== "Tab") return; const focusable = Array.from(event.currentTarget.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])')); const first = focusable[0], last = focusable[focusable.length - 1]; if (!first || !last) return; if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); } else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); } }
  async function openRepository() { if (opening) return; let selected: string | null; try { selected = import.meta.env.MODE === "e2e" && import.meta.env.VITE_E2E_PICKER_PATH === "/tmp/worktreeview-e2e-selection" ? "/tmp/worktreeview-e2e-selection" : await open({ directory: true, multiple: false }); } catch (error) { setOperationError(errorMessage(error)); return; } if (!selected || Array.isArray(selected)) return; setOpening(true); setOperationError(""); try { const repo = await invoke<Repo>("open_repo", { path: selected }); goInbox(); setRepos((current) => [repo, ...current.filter((item) => item.path !== repo.path)]); setActiveRepoPath(repo.path); setSelectedWorktreePath(""); setRepoErrors((current) => { const next = { ...current }; delete next[repo.path]; return next; }); setHydratingRepos((current) => ({ ...current, [repo.path]: (current[repo.path] ?? 0) + 1 })); try { const worktrees = await invoke<Worktree[]>("list_worktrees", { path: repo.path }); setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, worktrees } : item)); } catch (error) { setRepoErrors((current) => ({ ...current, [repo.path]: errorMessage(error) })); } finally { setHydratingRepos((current) => { const count = current[repo.path] ?? 0; if (count > 1) return { ...current, [repo.path]: count - 1 }; const next = { ...current }; delete next[repo.path]; return next; }); } } catch (error) { setOperationError(errorMessage(error)); } finally { setOpening(false); } }

  function reviewLoadsMatch(identity: ReviewIdentity) {
    const live = nav.current();
    if (live.kind === "review") return sameReview(live.identity, identity);
    if (live.kind === "commit-history") {
      const selected = live.selectedCommit;
      return selected !== null && sameReview({ repoPath: live.repoPath, base: selected.parents[0] ?? "empty-tree", target: commitTargetOf(selected), scope: "committed", reversed: false }, identity);
    }
    return false;
  }

  async function openReview(target: ReviewTarget, repoPath = activeRepoPath, preset?: { base?: string; scope?: ReviewScope; reversed?: boolean }) {
    const nextScope = preset?.scope ?? (target.kind === "worktree" ? scope : "committed");
    const nextReversed = preset?.reversed ?? reversed;
    const identity: ReviewIdentity = { repoPath, base: "", target, scope: nextScope, reversed: nextReversed };
    const generation = ++indexGenerationRef.current;
    ++patchGenerationRef.current;
    reviewIdentityRef.current = identity;
    patchIdentityRef.current = "";
    patchCacheRef.current.clear();
    const entry: AppLocation = { kind: "review", identity, selectedFile: null, filePage: 0, hunkPage: 0 };
    const current = nav.current();
    if (current.kind === "review" && current.identity.repoPath === repoPath && sameReviewTarget(current.identity.target, target)) nav.replace(entry); else nav.push(entry);
    setRefs({ heads: [], remotes: [], tags: [], default_base: null }); setReviewIndex(null); setPatch(null); setPatchError(""); setOperationError(""); setReviewLoading(true); setPatchLoading(false);
    try {
      const inventory = await invoke<RefInventory>("list_refs", { path: repoPath, worktreeBranch: target.kind === "worktree" ? target.worktree.branch : null });
      if (generation !== indexGenerationRef.current || !reviewLoadsMatch(identity)) return;
      setRefs(inventory);
      refsIdentityRef.current = identity;
      const nextBase = preset?.base ?? autoReviewBase(target, inventory);
      if (!nextBase) return;
      nav.replace({ kind: "review", identity: { ...identity, base: nextBase }, selectedFile: null, filePage: 0, hunkPage: 0 });
      await fetchIndex(target, nextBase, nextScope, nextReversed, repoPath);
    } catch (error) {
      if (generation === indexGenerationRef.current && reviewLoadsMatch(identity)) {
        setReviewIndex({ files: [], additions: 0, deletions: 0, base_sha: "", target_sha: "", error: errorMessage(error), error_code: errorCodeOf(error) });
      }
    } finally {
      // Clear the loading flag for the newest generation regardless of the
      // live location; only the result above is position-sensitive. A
      // position-gated reset would wedge a review restored via history on
      // its loading skeleton forever, with no in-surface recovery.
      if (generation === indexGenerationRef.current) setReviewLoading(false);
    }
  }
  async function toggleRepo(repo: Repo) {
    const expanded = !expandedRepos[repo.path];
    setExpandedRepos((current) => ({ ...current, [repo.path]: expanded }));
    // Expanding needs the branch inventory to rank and cap the worktree
    // children; a failed inventory (null) retries on the next expansion and
    // meanwhile leaves list-order children in place.
    if (!expanded || inventories[repo.path]) return;
    try {
      const inventory = await invoke<BranchInventory>("get_branch_inventory", { path: repo.path });
      setInventories((current) => ({ ...current, [repo.path]: inventory }));
    } catch {
      setInventories((current) => ({ ...current, [repo.path]: null }));
    }
  }
  async function togglePin(repo: Repo) {
    try {
      const pinned_at = await invoke<number | null>("set_repo_pinned", { path: repo.path, pinned: repo.pinned_at === null });
      setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, pinned_at } : item));
    } catch (error) { setOperationError(errorMessage(error)); }
  }
  // Registry-only removal: the store cascade clears this repo's cached pages
  // and retrospected surfaces; the repository on disk is never touched.
  async function removeProject(repo: Repo) {
    setRemoving(true);
    try {
      await invoke("remove_repo", { path: repo.path });
      const remaining = repos.filter((item) => item.path !== repo.path);
      setRepos(remaining);
      setSurfaces((current) => { const next = { ...current }; delete next[repo.path]; return next; });
      setWorktreeStatuses((current) => { const next = { ...current }; delete next[repo.path]; return next; });
      setInventories((current) => { const next = { ...current }; delete next[repo.path]; return next; });
      setRepoErrors((current) => { const next = { ...current }; delete next[repo.path]; return next; });
      setExpandedRepos((current) => { const next = { ...current }; delete next[repo.path]; return next; });
      if (activeRepoPath === repo.path) {
        setActiveRepoPath(remaining[0]?.path ?? "");
        setSelectedWorktreePath("");
      }
      setOperationError("");
      setRemoveTarget(null);
    } catch (error) {
      setOperationError(errorMessage(error));
    } finally {
      setRemoving(false);
    }
  }
  async function refreshSurfaces(repoPath: string) {
    try {
      const listing = await invoke<SurfaceListing>("list_surfaces", { path: repoPath });
      setSurfaces((current) => ({ ...current, [repoPath]: listing }));
    } catch (error) { setOperationError(errorMessage(error)); }
  }
  async function toggleSurfacePin(repoPath: string, kind: SurfaceRow["kind"], identityKey: string, pinned: boolean) {
    try {
      await invoke("set_surface_pinned", { path: repoPath, kind, identityKey, pinned });
      await refreshSurfaces(repoPath);
    } catch (error) { setOperationError(errorMessage(error)); }
  }
  async function fetchIndex(target: ReviewTarget, nextBase: string, nextScope: ReviewScope, nextReversed: boolean, nextRepoPath: string) {
    const identity: ReviewIdentity = { repoPath: nextRepoPath, base: nextBase, target, scope: nextScope, reversed: nextReversed };
    const generation = ++indexGenerationRef.current;
    ++patchGenerationRef.current;
    reviewIdentityRef.current = identity;
    patchIdentityRef.current = "";
    patchCacheRef.current.clear();
    setReviewLoading(true); setReviewIndex(null); setPatch(null); setPatchError(""); setPatchLoading(false);
    try {
      const index = await invoke<ReviewIndex>("list_review_changes", { path: target.kind === "worktree" ? target.worktree.path : nextRepoPath, repoPath: nextRepoPath, base: nextBase, headRef: target.kind === "ref" ? target.name : target.kind === "commit" ? target.sha : null, committedOnly: nextScope === "committed", reversed: nextReversed });
      if (generation === indexGenerationRef.current && reviewLoadsMatch(identity)) setReviewIndex(index);
    } catch (error) {
      if (generation === indexGenerationRef.current && reviewLoadsMatch(identity)) {
        setReviewIndex({ files: [], additions: 0, deletions: 0, base_sha: "", target_sha: "", error: errorMessage(error), error_code: errorCodeOf(error) });
      }
    } finally {
      // Same as openReview: the reset is generation-scoped so a load
      // orphaned by navigation cannot leave the restored review stuck on
      // its loading skeleton; returning via history re-runs it.
      if (generation === indexGenerationRef.current) setReviewLoading(false);
    }
  }
  function changeReviewSetting(nextBase: string, nextScope = scope, nextReversed = reversed) {
    const current = nav.current();
    if (current.kind !== "review" || !nextBase) return;
    const identity: ReviewIdentity = { ...current.identity, base: nextBase, scope: nextScope, reversed: nextReversed };
    const entry: AppLocation = { kind: "review", identity, selectedFile: null, filePage: 0, hunkPage: 0 };
    if (current.identity.base !== nextBase) nav.push(entry); else nav.replace(entry);
    void fetchIndex(identity.target, nextBase, nextScope, nextReversed, identity.repoPath);
  }
  async function selectFile(file: ChangedFile, identity: ReviewIdentity) {
    if (!identity.base) return;
    const patchIdentity = patchIdentityOf(identity, file);
    const generation = ++patchGenerationRef.current;
    patchIdentityRef.current = patchIdentity;
    // Commit patches are immutable and worktree patches share the review
    // index's snapshot freshness, so a cached patch renders without
    // respawning Git. Reinserting keeps recency order for eviction.
    const cached = patchCacheRef.current.get(patchIdentity);
    if (cached) {
      patchCacheRef.current.delete(patchIdentity);
      patchCacheRef.current.set(patchIdentity, cached);
      setPatch(cached); setPatchError(""); setPatchLoading(false);
      return;
    }
    setPatch(null); setPatchError(""); setPatchLoading(true);
    try {
      const nextPatch = await invoke<FilePatch>("read_review_patch", { path: identity.target.kind === "worktree" ? identity.target.worktree.path : identity.repoPath, base: identity.base, headRef: identity.target.kind === "ref" ? identity.target.name : identity.target.kind === "commit" ? identity.target.sha : null, committedOnly: identity.scope === "committed", reversed: identity.reversed, file: file.path, untracked: file.untracked });
      if (generation === patchGenerationRef.current && patchIdentityRef.current === patchIdentity && sameReview(reviewIdentityRef.current, identity)) {
        patchCacheRef.current.set(patchIdentity, nextPatch);
        if (patchCacheRef.current.size > PATCH_CACHE_LIMIT) {
          const oldest = patchCacheRef.current.keys().next().value;
          if (oldest !== undefined) patchCacheRef.current.delete(oldest);
        }
        setPatch(nextPatch);
      }
    } catch (error) {
      if (generation !== patchGenerationRef.current || patchIdentityRef.current !== patchIdentity || !sameReview(reviewIdentityRef.current, identity)) return;
      const code = typeof error === "object" && error !== null && "code" in error ? String((error as CommandError).code) : "";
      setPatchError(code === "git_output_too_large" ? "This file's patch exceeds the 4 MiB output bound and was not rendered." : errorMessage(error));
    } finally {
      if (generation === patchGenerationRef.current && patchIdentityRef.current === patchIdentity && sameReview(reviewIdentityRef.current, identity)) setPatchLoading(false);
    }
  }

  // A review degrades to "content no longer available" only when its own
  // surface is recorded gone and the index failure means the objects behind
  // it are gone; commit targets match by head SHA, ref targets by full ref.
  function goneSurfaceMatching(repoPath: string, target: ReviewTarget): GoneSurface | null {
    const gone = surfaces[repoPath]?.gone ?? [];
    if (target.kind === "commit") return gone.find((surface) => surface.head_sha === target.sha) ?? null;
    if (target.kind === "ref") return gone.find((surface) => surface.kind === "branch" && surface.identity_key === target.name) ?? null;
    return null;
  }

  function goInbox() { nav.push({ kind: "inbox" }); }
  // The inbox pane is the project overview: activating a repository lands there.
  function activateRepo(repo: Repo) { setActiveRepoPath(repo.path); goInbox(); }
  // Worktree review presets pick a view, not just a scope: working changes
  // pin the base to the checked-out branch tip, the other two restore the
  // review's default base when the working preset replaced it and keep a
  // manually picked base otherwise. The default base is only recomputed when
  // the held ref inventory belongs to this review; back and forward restores
  // do not refetch it, so a stale inventory from another review must not
  // contribute a base.
  function applyReviewPreset(preset: WorktreeReviewPreset) {
    const current = nav.current();
    if (current.kind !== "review") return;
    const identity = current.identity;
    const target = identity.target;
    if (target.kind !== "worktree") return;
    const workingBase = workingChangesBase(target.worktree);
    if (preset === "working") { changeReviewSetting(workingBase, "all", false); return; }
    const base = identity.base;
    const restorable = base === workingBase && sameReview(refsIdentityRef.current, identity);
    const nextBase = restorable ? autoReviewBase(target, refs) || base : base;
    changeReviewSetting(nextBase, preset === "all" ? "all" : "committed", identity.reversed);
  }
  // The refresh action's network step: fetch updates remote-tracking refs
  // before the local re-reads so inventory and history reflect upstream. A
  // failed fetch (offline, rejected credentials) never blocks the refresh;
  // the error surfaces and the local state still reloads.
  async function fetchProjectRemotes(repoPath: string) {
    setFetchingRepos((current) => ({ ...current, [repoPath]: true }));
    try {
      await invoke("fetch_project", { path: repoPath });
    } catch (error) {
      setOperationError(`Fetch failed: ${errorMessage(error)}`);
    } finally {
      setFetchingRepos((current) => { const next = { ...current }; delete next[repoPath]; return next; });
    }
  }
  async function refreshProject() {
    const repo = activeRepo;
    if (!repo) return;
    await fetchProjectRemotes(repo.path);
    try {
      const worktrees = await invoke<Worktree[]>("list_worktrees", { path: repo.path });
      setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, worktrees } : item));
    } catch (error) { setRepoErrors((current) => ({ ...current, [repo.path]: errorMessage(error) })); }
    void refreshSurfaces(repo.path);
    setStatusNonce((nonce) => nonce + 1);
  }
  async function refreshCommits(entry: HistoryEntry) {
    await fetchProjectRemotes(entry.repoPath);
    await loadHistorySurface(entry);
  }
  function openSettings() { nav.push({ kind: "settings" }); }
  // Activating an arrival opens the review the submission targeted: the
  // worktree row when it is loaded, otherwise the commit review re-derived
  // from the recorded identity (the base override pins the recorded base).
  function openArrival(arrival: SubmissionArrival) {
    const repo = repos.find((item) => item.path === arrival.repo_path);
    if (!repo) return;
    setActiveRepoPath(repo.path);
    if (arrival.target_kind === "head") {
      void openReview({ kind: "commit", sha: arrival.target_key, parents: [], defaultBaseAncestor: false }, arrival.repo_path, { base: arrival.base_sha });
      return;
    }
    const worktree = repo.worktrees.find((item) => item.path === arrival.target_key);
    if (worktree) {
      setSelectedWorktreePath(worktree.path);
      void openReview({ kind: "worktree", worktree }, repo.path);
    }
  }
  function openOldestArrival() {
    const arrival = arrivals[0];
    if (!arrival) return;
    openArrival(arrival);
    setArrivals((current) => current.slice(1));
  }
  function dismissArrivals() { setArrivals([]); }
  async function changeZoom(zoom: number) {
    const current = zoomLiveRef.current;
    const target = snapZoom(zoom);
    if (target === current) return;
    // The live level advances immediately so rapid steps chain instead of
    // reading stale state; on a failed save the level and the webview both
    // roll back so shortcuts keep working.
    zoomLiveRef.current = target;
    try { await applyZoom(target); setSettings(await persistSettings({ ...settings, zoom: target })); } catch { zoomLiveRef.current = current; void applyZoom(current); }
  }
  async function updateSettings(next: Settings) { setSettingsSaveError(""); applyTheme(next.theme); try { setSettings(await persistSettings(next)); } catch (error) { applyTheme(settings.theme); setSettingsSaveError(errorMessage(error)); } }
  function navigate(delta: number) { if (delta < 0) nav.back(); else nav.forward(); }
  function handleReviewBack() { const previous = nav.peekBack(); if (previous?.kind === "commit-history") nav.back(); else goInbox(); }
  function selectHistoryCommit(commit: CommitInfo) { const current = nav.current(); if (current.kind !== "commit-history") return; nav.replace({ ...current, selectedCommit: commit, selectedFile: null, filePage: 0, hunkPage: 0 }); }
  function openHistoryEntry(entry: HistoryEntry) { nav.push({ kind: "commit-history", repoPath: entry.repoPath, startPointLabel: entry.startPointLabel, startRef: entry.startRef ?? null, worktreePath: entry.worktreePath ?? null, selectedCommit: null, selectedFile: null, filePage: 0, hunkPage: 0 }); }
  async function loadHistorySurface(entry: HistoryEntry) {
    const generation = ++historyGenerationRef.current;
    setHistory({ ...entry, commits: [], hasMore: false, loading: true, error: "" });
    setHistoryRefs({ heads: [], remotes: [], tags: [], default_base: null });
    let against: string | null = null;
    try {
      const inventory = await invoke<RefInventory>("list_refs", { path: entry.repoPath, worktreeBranch: null });
      if (generation !== historyGenerationRef.current) return;
      setHistoryRefs(inventory);
      against = inventory.default_base;
    } catch (error) {
      if (generation !== historyGenerationRef.current) return;
      setHistory((current) => current && current.repoPath === entry.repoPath && current.startPointLabel === entry.startPointLabel ? { ...current, loading: false, error: errorMessage(error) } : current);
      return;
    }
    await loadHistoryPage(entry, generation, 0, against, false);
  }
  async function loadHistoryPage(entry: HistoryEntry, generation: number, skip: number, against: string | null, append: boolean) {
    try {
      const page = await invoke<CommitPage>("list_commits", { path: entry.worktreePath ?? entry.repoPath, repoPath: entry.repoPath, startRef: entry.startRef ?? null, against, skip, limit: COMMIT_PAGE_SIZE });
      if (generation !== historyGenerationRef.current) return;
      setHistory((current) => current && current.repoPath === entry.repoPath && current.startPointLabel === entry.startPointLabel ? { ...current, commits: append ? [...current.commits, ...page.commits] : page.commits, hasMore: page.has_more, loading: false, error: "" } : current);
    } catch (error) {
      if (generation !== historyGenerationRef.current) return;
      setHistory((current) => current && current.repoPath === entry.repoPath && current.startPointLabel === entry.startPointLabel ? { ...current, loading: false, error: errorMessage(error) } : current);
    }
  }
  async function loadMoreHistory() {
    const current = history;
    if (!current || current.loading) return;
    const generation = ++historyGenerationRef.current;
    setHistory((inFlight) => inFlight ? { ...inFlight, loading: true } : inFlight);
    await loadHistoryPage(current, generation, current.commits.length, historyRefs.default_base, true);
  }

  const diffPrefs: DiffPreferences = { layout: settings.diff_layout, whitespaceVisible: settings.whitespace_visible, lineWrap: settings.line_wrap, syntaxVisible: settings.syntax_visible };
  const diffToggles = <DiffToggles settings={settings} onChange={(next) => { void updateSettings(next); }} />;
  const goneReview = reviewLocation && reviewIndex !== null && reviewIndex.error && (reviewIndex.error_code === "unresolvable_ref" || reviewIndex.error_code === "git_execution") ? goneSurfaceMatching(reviewLocation.identity.repoPath, reviewLocation.identity.target) : null;
  const search = query.trim().toLowerCase(); const matchingResults: SearchResult[] = search ? repos.flatMap((repo) => `${repo.name} ${repo.path}`.toLowerCase().includes(search) ? [{ repo }] : repo.worktrees.filter((worktree) => `${worktree.branch} ${worktree.path} ${worktree.head}`.toLowerCase().includes(search)).map((worktree) => ({ repo, worktree }))) : repos.map((repo) => ({ repo }));
  const pinnedRepos = repos.filter((repo) => repo.pinned_at !== null).sort((left, right) => (right.pinned_at ?? 0) - (left.pinned_at ?? 0)); const recentRepos = repos.filter((repo) => repo.pinned_at === null); const sidebarRows = (repo: Repo) => {
  const listing = surfaces[repo.path] ?? { gone: [], pinned: [] };
  // Pinned branches ride the inventory's branch lists (local plus remote,
  // minus branches already rendered as worktree rows) so a pin on a
  // still-existing branch keeps its sidebar row; unpinned branch rows are
  // dropped by the cap.
  const checkedOut = new Set(repo.worktrees.map((worktree) => worktree.branch));
  const inventoryBranches = [...(inventories[repo.path]?.branches ?? []), ...(inventories[repo.path]?.remote_branches ?? [])].map((branch) => branch.ref_name).filter((ref) => !checkedOut.has(ref));
  const pinned = pinnedSurfaces(surfaceRows(repo.worktrees, inventoryBranches, listing.gone, listing.pinned));
  const surfaceRow = (row: SurfaceRow) => {
    const worktree = row.worktreePath ? repo.worktrees.find((item) => item.path === row.worktreePath) : undefined;
    const action = row.pinnedAt === null ? "Pin" : "Unpin";
    const main = row.gone
      ? <button className="sidebar-branch-row" type="button" aria-label={`Show commit history for gone surface ${row.label}`} onClick={() => { setActiveRepoPath(repo.path); void openHistoryEntry({ repoPath: repo.path, startPointLabel: row.label, startRef: row.startRef ?? undefined }); }}><GitBranch size={12} /><span>{row.label}</span>{row.kind === "worktree" && <span className="in-worktree-badge">worktree</span>}<span className="gone-badge">gone</span></button>
      : worktree
        ? <button className="sidebar-worktree-row" type="button" onClick={() => { setActiveRepoPath(repo.path); if (worktree) { setSelectedWorktreePath(worktree.path); void openReview({ kind: "worktree", worktree }, repo.path); } }}><GitBranch size={12} /><span>{row.label}</span></button>
        : <button className="sidebar-branch-row" type="button" onClick={() => { setActiveRepoPath(repo.path); void openReview({ kind: "ref", name: row.identityKey }, repo.path); }}><GitBranch size={12} /><span>{row.label}</span></button>;
    return <div className="project-row-wrap" key={`${row.kind}:${row.identityKey}`}>{main}<button className="pin-button surface-pin" type="button" aria-label={`${action} ${row.kind} ${row.label}`} title={`${action} ${row.kind}`} onClick={() => void toggleSurfacePin(repo.path, row.kind, row.identityKey, row.pinnedAt === null)}>{row.pinnedAt === null ? <Pin size={12} /> : <PinOff size={12} />}</button></div>;
  };
  // Children stay capped: pinned surfaces first, then the most recently
  // committed worktrees. Unpinned branches and archived surfaces live on
  // the project overview instead of growing the sidebar without bound.
  const dateByBranch = new Map((inventories[repo.path]?.branches ?? []).map((branch) => [branch.ref_name, branch.commit_date]));
  const pinnedWorktrees = new Set(listing.pinned.filter((pin) => pin.kind === "worktree").map((pin) => worktreeKey(pin.identity_key)));
  const activeWorktrees = recentWorktrees(repo.worktrees, dateByBranch, pinnedWorktrees, SIDEBAR_WORKTREE_LIMIT);
  return <>{pinned.map(surfaceRow)}{activeWorktrees.map((worktree) => surfaceRow({ kind: "worktree", identityKey: worktree.path, label: shortToken(worktree.branch), startRef: null, worktreePath: worktree.path, pinnedAt: null, gone: false }))}<button className="sidebar-link-row" type="button" onClick={() => activateRepo(repo)}><ListTree size={12} /><span>All worktrees &amp; branches</span></button></>;
};
  const statusMessage = loadError || (loading ? "Loading repositories..." : hydrating ? "Loading worktrees..." : ""); const searchPageCount = Math.max(1, Math.ceil(matchingResults.length / SEARCH_PAGE_SIZE)); const visibleSearchPage = Math.min(searchPage, searchPageCount - 1); const visibleSearchResults = matchingResults.slice(visibleSearchPage * SEARCH_PAGE_SIZE, (visibleSearchPage + 1) * SEARCH_PAGE_SIZE);
  const statusByPath: Record<string, number | null> = {};
  for (const status of worktreeStatuses[activeRepoPath] ?? []) statusByPath[status.path] = status.changes;
  const activeInventory = inventories[activeRepoPath] ?? null;
  const branchByRef = new Map<string, BranchSummary>((activeInventory?.branches ?? []).map((branch) => [branch.ref_name, branch]));
  const statusesForActiveRepo = worktreeStatuses[activeRepoPath] ?? [];
  const uncommittedOverflow = statusesForActiveRepo.some((status) => status.changes === STATUS_COUNT_OVERFLOW);
  const uncommittedTotal = statusesForActiveRepo.reduce((sum, status) => status.changes !== null && status.changes !== STATUS_COUNT_OVERFLOW ? sum + status.changes : sum, 0);
  const aheadWorktrees = (activeRepo?.worktrees ?? []).filter((worktree) => (branchByRef.get(worktree.branch)?.ahead ?? 0) > 0).length;
  const overviewStats: Array<{ value: React.ReactNode; label: string }> = activeRepo ? [
    { value: activeRepo.worktrees.length, label: "Worktrees" },
    { value: <span className={uncommittedTotal > 0 || uncommittedOverflow ? "warn-count" : ""}>{uncommittedOverflow ? "9999+" : uncommittedTotal}</span>, label: "Uncommitted" },
    { value: aheadWorktrees, label: "Ahead of base" },
    { value: activeInventory ? activeInventory.branches.length : "…", label: "Local branches" },
    { value: activeInventory ? activeInventory.remote_branch_count : "…", label: "Remote branches" },
  ] : [];
  // The overview's filter row scopes one tab at a time: worktrees, local
  // branches, remote branches, or archived surfaces, all client-side over
  // the already-bounded inventory passes.
  const overviewNeedle = overviewQuery.trim().toLowerCase();
  const matchesOverview = (haystack: string) => haystack.toLowerCase().includes(overviewNeedle);
  const filteredWorktrees = activeRepo?.worktrees.filter((worktree) => !overviewNeedle || matchesOverview(`${shortToken(worktree.branch)} ${worktree.branch} ${worktree.path}`)) ?? [];
  const worktreeBranchRefs = new Set(activeRepo?.worktrees.map((worktree) => worktree.branch) ?? []);
  const sortedLocalBranches = [...(activeInventory?.branches ?? [])].sort((left, right) => right.commit_date - left.commit_date);
  const filteredBranches = overviewNeedle ? sortedLocalBranches.filter((branch) => matchesOverview(`${shortToken(branch.ref_name)} ${branch.subject} ${branch.author}`)) : sortedLocalBranches;
  const sortedRemoteBranches = [...(activeInventory?.remote_branches ?? [])].sort((left, right) => right.commit_date - left.commit_date);
  const filteredRemoteBranches = overviewNeedle ? sortedRemoteBranches.filter((branch) => matchesOverview(`${shortToken(branch.ref_name)} ${branch.subject} ${branch.author}`)) : sortedRemoteBranches;
  const unpinnedGone = (surfaces[activeRepoPath]?.gone ?? []).filter((surface) => surface.pinned_at === null);
  const filteredGone = filterGoneSurfaces(unpinnedGone, overviewQuery);
  const activeTab = OVERVIEW_TABS.find((tab) => tab.id === overviewTab) ?? OVERVIEW_TABS[0];
  // Tab counts stay unfiltered so they keep meaning the inventory's size
  // while the input scopes the rows.
  const overviewTabCount = (tab: OverviewTab) => tab === "worktrees" ? String(activeRepo?.worktrees.length ?? 0) : tab === "branches" ? (activeInventory ? String(sortedLocalBranches.length) : "…") : tab === "remote" ? (activeInventory ? String(sortedRemoteBranches.length) : "…") : String(unpinnedGone.length);
  const overviewTotal = overviewTab === "worktrees" ? filteredWorktrees.length : overviewTab === "branches" ? filteredBranches.length : overviewTab === "remote" ? filteredRemoteBranches.length : filteredGone.length;
  const overviewPageCount = Math.max(1, Math.ceil(overviewTotal / activeTab.pageSize));
  const visibleOverviewPage = Math.min(overviewPage, overviewPageCount - 1);
  const overviewSlice = <T,>(items: T[]) => items.slice(visibleOverviewPage * activeTab.pageSize, (visibleOverviewPage + 1) * activeTab.pageSize);
  const inventoryUnavailable = inventories[activeRepoPath] === null;
  const goneListing = surfaces[activeRepoPath] ?? { gone: [], pinned: [] };
  const branchPinIndex = surfacePinIndex(goneListing.pinned);
  // Branch rows reuse the worktree row grid; the chip reviews the branch's
  // committed divergence the same way the worktree table's sync chip does.
  const syncChip = (syncBase: string | null, ahead: number, behind: number, onOpen: (base: string) => void) => syncBase && (ahead > 0 || behind > 0) ? <button className="status-chip sync" type="button" title={`Review committed changes vs ${shortToken(syncBase)}`} onClick={(event) => { event.stopPropagation(); onOpen(syncBase); }}>{ahead > 0 ? `↑${ahead}` : ""}{ahead > 0 && behind > 0 ? " " : ""}{behind > 0 ? `↓${behind}` : ""}</button> : null;
  const inventoryBranchRow = (branch: BranchSummary) => {
    const pinned = branchPinIndex.get(`branch:${branch.ref_name}`) !== undefined;
    const checkedOut = worktreeBranchRefs.has(branch.ref_name);
    const syncBase = branch.upstream ?? activeInventory?.default_branch ?? null;
    const ahead = branch.ahead ?? 0;
    const behind = branch.behind ?? 0;
    const openBranchReview = () => void openReview({ kind: "ref", name: branch.ref_name }, activeRepoPath);
    return <div className="project-row-wrap" key={branch.ref_name}><div className="worktree-row" role="button" tabIndex={0} title={branch.ref_name} onClick={openBranchReview} onKeyDown={(event) => { if (event.target !== event.currentTarget) return; if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openBranchReview(); } }}><div className="worktree-cell"><div className="branch-title"><GitBranch size={14} /><strong>{shortToken(branch.ref_name)}</strong>{branch.ref_name === activeInventory?.default_branch && <span className="main-badge">main</span>}{checkedOut && <span className="in-worktree-badge">worktree</span>}</div><div className="worktree-path"><span className="path-text">{branch.ref_name}</span></div></div><div className="status-cell">{syncChip(syncBase, ahead, behind, (base) => openReview({ kind: "ref", name: branch.ref_name }, activeRepoPath, { base, scope: "committed", reversed: false }))}</div><div className="last-commit-cell"><span className="row-commit-subject">{branch.subject}</span><span className="row-commit-age">{branch.author} · {relativeTime(branch.commit_date)}</span></div></div><button className="pin-button surface-pin" type="button" aria-label={`${pinned ? "Unpin" : "Pin"} branch ${shortToken(branch.ref_name)}`} title={`${pinned ? "Unpin" : "Pin"} branch`} onClick={(event) => { event.stopPropagation(); void toggleSurfacePin(activeRepoPath, "branch", branch.ref_name, !pinned); }}>{pinned ? <PinOff size={12} /> : <Pin size={12} />}</button></div>;
  };
  const archivedRow = (surface: GoneSurface) => {
    const label = goneSurfaceLabel(surface);
    const openArchivedHistory = () => void openHistoryEntry({ repoPath: activeRepoPath, startPointLabel: label, startRef: surface.head_sha });
    return <div className="project-row-wrap" key={`gone:${surface.kind}:${surface.identity_key}`}><div className="worktree-row" role="button" tabIndex={0} title={surface.identity_key} onClick={openArchivedHistory} onKeyDown={(event) => { if (event.target !== event.currentTarget) return; if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openArchivedHistory(); } }}><div className="worktree-cell"><div className="branch-title"><GitBranch size={14} /><strong>{label}</strong>{surface.kind === "worktree" && <span className="in-worktree-badge">worktree</span>}<span className="gone-badge">gone</span></div><div className="worktree-path"><span className="path-text">{surface.detail}</span></div></div><div className="status-cell" /><div className="last-commit-cell"><span className="row-commit-age">{surface.head_sha ? <code>{shortToken(surface.head_sha)}</code> : ""}{surface.last_seen_at ? ` · last seen ${relativeTime(surface.last_seen_at)}` : ""}</span></div></div><button className="pin-button surface-pin" type="button" aria-label={`Pin ${surface.kind} ${label}`} title={`Pin ${surface.kind}`} onClick={(event) => { event.stopPropagation(); void toggleSurfacePin(activeRepoPath, surface.kind, surface.identity_key, true); }}><Pin size={12} /></button></div>;
  };
  const breadcrumbRepo = repos.find((repo) => repo.path === (reviewLocation?.identity.repoPath ?? historyLocation?.repoPath ?? activeRepoPath)) ?? activeRepo;
  const activeCommitSha = historyLocation?.selectedCommit?.sha ?? (reviewTarget !== null && reviewTarget.kind === "commit" ? reviewTarget.sha : null);
  // The bar is a permanent sidebar fixture: visible whenever a loaded
  // history matches the active ref, held across commit reviews so picking
  // commits keeps the ref's list anchored.
  const showCommitsBar = history !== null && (historyAnchor !== null ? historyAnchorKey === historyKeyOf(history) : activeCommitSha !== null && reviewLocation !== null && history.repoPath === reviewLocation.identity.repoPath);

  return <div className={`app-shell ${collapsed ? "nav-collapsed" : ""}`} aria-busy={loading || opening || hydrating}><aside className="sidebar"><div className="brand-row"><span className="brand-mark">wv</span><span className="brand-name">WorktreeView</span></div><button className="open-repository-button" type="button" aria-label="Open repository" title="Open repository" onClick={() => void openRepository()} disabled={opening}><FolderGit2 size={14} /><span>Open repository...</span></button><div className="nav-tabs" aria-label="Workspace views"><button className="nav-tab active" type="button" aria-label="Projects" aria-current="page"><FolderGit2 size={15} /><span>Projects</span></button><button className="nav-tab" type="button" aria-label="Attention, unavailable" aria-describedby="unavailable-features" disabled><Inbox size={15} /><span>Attention</span></button></div><button className="search-trigger" type="button" aria-label="Find repositories and worktrees" onClick={(event) => openPalette(event.currentTarget)}><Search size={14} /><span>Find repositories and worktrees</span><kbd>Ctrl K</kbd></button><nav className="project-list" aria-label="Repositories"><div className="nav-section-label">Pinned</div>{pinnedRepos.length === 0 && <div className="sidebar-empty">No pinned repositories</div>}{pinnedRepos.map((repo) => <RepoNavGroup key={repo.path} repo={repo} active={repo.path === activeRepoPath} expanded={Boolean(expandedRepos[repo.path])} onToggle={() => { activateRepo(repo); void toggleRepo(repo); }} onPin={() => void togglePin(repo)} rows={sidebarRows(repo)} />)}<div className="nav-section-label">Recent</div>{recentRepos.length === 0 && <div className="sidebar-empty">No recent repositories</div>}{recentRepos.map((repo) => <RepoNavGroup key={repo.path} repo={repo} active={repo.path === activeRepoPath} expanded={Boolean(expandedRepos[repo.path])} onToggle={() => { activateRepo(repo); void toggleRepo(repo); }} onPin={() => void togglePin(repo)} rows={sidebarRows(repo)} />)}</nav>{showCommitsBar && history && <section className="commits-bar" aria-label="Commit history"><div className="commits-bar-heading"><strong>{history.startPointLabel}</strong><span className="commits-bar-actions"><button className={`icon-button ${fetchingRepos[history.repoPath] ? "spinning" : ""}`} type="button" aria-label="Refresh commit history" title="Fetch and refresh" disabled={Boolean(fetchingRepos[history.repoPath])} onClick={() => { if (history) void refreshCommits({ repoPath: history.repoPath, startPointLabel: history.startPointLabel, worktreePath: history.worktreePath ?? undefined, startRef: history.startRef ?? undefined }); }}><RefreshCw size={12} /></button></span></div>{history.error ? <div className="sidebar-empty" role="status">{history.error}</div> : history.commits.length === 0 ? <div className="sidebar-empty">{history.loading ? "Loading commits..." : "No commits"}</div> : <><div className="commit-list">{history.commits.map((commit) => <button key={commit.sha} type="button" className={`commit-row ${activeCommitSha === commit.sha ? "selected" : ""}`} onClick={() => { if (historyLocation) selectHistoryCommit(commit); else void openReview(commitTargetOf(commit), history.repoPath, { base: commit.parents[0] ?? "empty-tree" }); }}><span className="commit-subject">{commit.subject}</span><span className="commit-meta"><code>{shortToken(commit.sha)}</code><span>{commit.author}</span><span>{commit.date.slice(0, 10)}</span>{commit.refs.map((ref) => <span key={ref} className="commit-ref">{shortToken(ref)}</span>)}</span></button>)}</div>{history.hasMore && <button type="button" className="commits-load-more" disabled={history.loading} onClick={() => void loadMoreHistory()}>{history.loading ? "Loading..." : "Load more"}</button>}</>}</section>}<div className="sidebar-footer"><button className="icon-button footer-button" type="button" aria-label="Open settings" title="Settings" onClick={openSettings}><SettingsIcon size={15} /></button><button className="icon-button footer-button" type="button" aria-label={collapsed ? "Expand project navigator" : "Collapse project navigator"} title={collapsed ? "Expand project navigator" : "Collapse project navigator"} onClick={() => setCollapsed((value) => !value)}>{collapsed ? <ChevronRight size={15} /> : <ChevronsLeft size={15} />}</button><span className="footer-note"><HardDrive size={13} />read-only / local</span></div></aside><section className="workspace"><p className="visually-hidden" id="unavailable-features">Attention is unavailable.</p><div className="live-error" role="status" aria-live="polite">{statusMessage}</div>{operationError && <div className="operation-error" role="status">{operationError}</div>}<header className="topbar"><div className="breadcrumb"><span>Repositories</span><span>/</span><strong>{breadcrumbRepo?.name ?? "No repository"}</strong>{historyLocation && <><span>/</span><strong>{historyLocation.startPointLabel}</strong></>}{reviewTarget && <><span>/</span><strong>{shortToken(reviewTarget.kind === "worktree" ? reviewTarget.worktree.branch : reviewTarget.kind === "commit" ? reviewTarget.sha : reviewTarget.name)}</strong></>}</div><div className="topbar-actions"><span className="safety-label">Read-only</span><button className="search-button" type="button" aria-label="Find repositories and worktrees" onClick={(event) => openPalette(event.currentTarget)}><Command size={14} /> <span>Search</span> <kbd>Ctrl K</kbd></button></div></header><main className="content">{reviewLocation ? goneReview ? <Empty icon={<CircleDot size={24} />} title="Content no longer available" detail="This surface's content is no longer available in the repository." /> : <ReviewView repoPath={reviewLocation.identity.repoPath} liveWorktree={activeRepo?.worktrees.find((worktree) => worktree.path === selectedWorktreePath)} target={reviewLocation.identity.target} refs={refs} base={displayBase} scope={scope} reversed={reversed} index={reviewIndex} loading={reviewLoading} selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} filePage={filePage} hunkPage={hunkPage} diffPrefs={diffPrefs} diffToggles={diffToggles} comments={comments} canBack={nav.canBack()} canForward={nav.canForward()} onHistoryBack={() => nav.back()} onHistoryForward={() => nav.forward()} onBack={handleReviewBack} onTargetChange={(target) => { void openReview(target, reviewLocation.identity.repoPath); }} onBaseChange={(value) => changeReviewSetting(value)} onPreset={applyReviewPreset} onReverse={() => changeReviewSetting(base, scope, !reversed)} onFilePage={(page) => nav.replace({ ...reviewLocation, filePage: page })} onHunkPage={(page) => nav.replace({ ...reviewLocation, hunkPage: page })} onFile={(file) => { nav.replace({ ...reviewLocation, selectedFile: file, hunkPage: 0 }); void selectFile(file, reviewLocation.identity); }} /> : historyLocation ? <HistoryView history={history ?? { repoPath: historyLocation.repoPath, startPointLabel: historyLocation.startPointLabel, worktreePath: historyLocation.worktreePath ?? undefined, startRef: historyLocation.startRef ?? undefined, commits: [], hasMore: false, loading: true, error: "" }} historyRefs={historyRefs} index={reviewIndex} loading={reviewLoading} selectedCommit={historyLocation.selectedCommit} selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} filePage={filePage} hunkPage={hunkPage} diffPrefs={diffPrefs} diffToggles={diffToggles} comments={comments} onBack={goInbox} onBasePick={(value) => { const selected = historyLocation.selectedCommit; if (selected) void openReview(commitTargetOf(selected), historyLocation.repoPath, { base: value }); }} onFilePage={(page) => nav.replace({ ...historyLocation, filePage: page })} onHunkPage={(page) => nav.replace({ ...historyLocation, hunkPage: page })} onFile={(file) => { const selected = historyLocation.selectedCommit; if (selected) { nav.replace({ ...historyLocation, selectedFile: file, hunkPage: 0 }); void selectFile(file, { repoPath: historyLocation.repoPath, base: selected.parents[0] ?? "empty-tree", target: commitTargetOf(selected), scope: "committed", reversed: false }); } }} /> : <section className="inbox-pane" aria-labelledby="inbox-heading" aria-busy={loading || activeRepoHydrating}><div className="section-heading"><div className="project-heading"><h1 id="inbox-heading">{activeRepo?.name ?? "Worktrees"}</h1>{activeRepo && !activeRepoHydrating && <div className="project-meta"><span className="meta-chip" title={activeRepo.path}><span className="meta-chip-label">{activeRepo.path}</span><CopyButton value={activeRepo.path} label="Copy project path" /></span>{activeInventory?.origin_url && <span className="meta-chip" title={activeInventory.origin_url}><span className="meta-chip-label">{originSlug(activeInventory.origin_url)}</span><CopyButton value={activeInventory.origin_url} label="Copy clone URL" /></span>}{activeInventory?.default_branch && <span className="meta-chip" title="Default branch"><span className="meta-chip-label">default {shortToken(activeInventory.default_branch)}</span><CopyButton value={shortToken(activeInventory.default_branch)} label="Copy branch name" /></span>}</div>}</div>{activeRepo && <div className="heading-actions" ref={menuAnchorRef}><button className={`icon-button ${fetchingRepos[activeRepo.path] ? "spinning" : ""}`} type="button" aria-label="Refresh project" title="Fetch and refresh" disabled={Boolean(fetchingRepos[activeRepo.path])} onClick={() => void refreshProject()}><RefreshCw size={15} /></button><button className={`icon-button ${menuOpen ? "open" : ""}`} type="button" aria-label="Project actions" title="Project actions" aria-haspopup="menu" aria-expanded={menuOpen} onClick={() => setMenuOpen((open) => !open)}><MoreVertical size={15} /></button>{menuOpen && <div className="project-menu" role="menu" aria-label="Project actions"><button className="menu-item" type="button" role="menuitem" onClick={() => { setMenuOpen(false); void togglePin(activeRepo); }}>{activeRepo.pinned_at === null ? <Pin size={13} /> : <PinOff size={13} />}{activeRepo.pinned_at === null ? "Pin project" : "Unpin project"}</button><button className="menu-item" type="button" role="menuitem" onClick={() => { void copyText(activeRepo.path); setMenuOpen(false); }}><Copy size={13} />Copy path</button><div className="menu-separator" /><button className="menu-item danger" type="button" role="menuitem" onClick={() => { setMenuOpen(false); setRemoveTarget(activeRepo); }}><Trash2 size={13} />Remove from WorktreeView</button><p className="menu-note">Removes the project from this app only. Your repository, worktrees, and history on disk are never touched.</p></div>}</div>}</div>{loading ? <Empty icon={<CircleDot size={24} />} title="Loading repositories..." detail="Reading saved repositories." /> : loadError ? <Empty icon={<CircleDot size={24} />} title="Repositories could not be loaded" detail={loadError} /> : !activeRepo ? <Empty icon={<FolderGit2 size={24} />} title="No repositories" detail="Open a local Git folder to begin." action={<button className="secondary-button" type="button" onClick={() => void openRepository()} disabled={opening}>Open repository...</button>} /> : activeRepoHydrating ? <Empty icon={<CircleDot size={24} />} title="Loading worktrees..." detail="Reading live worktree identity." /> : repoErrors[activeRepo.path] ? <Empty icon={<CircleDot size={24} />} title="Worktrees unavailable" detail={repoErrors[activeRepo.path]} /> : activeRepo.worktrees.length === 0 ? <Empty icon={<CircleDot size={24} />} title="No worktrees" detail="This repository has no linked worktrees." /> : <><div className="stats-strip">{overviewStats.map((stat) => <div className="stat" key={stat.label}><div className="stat-value">{stat.value}</div><div className="stat-label">{stat.label}</div></div>)}</div><div className="overview-filter"><div className="overview-tabs" role="tablist" aria-label="Project inventory">{OVERVIEW_TABS.map((tab) => <button key={tab.id} role="tab" type="button" aria-selected={overviewTab === tab.id} className={`overview-tab ${overviewTab === tab.id ? "active" : ""}`} onClick={() => { setOverviewTab(tab.id); setOverviewPage(0); }}>{tab.label}<span className="tab-count">{overviewTabCount(tab.id)}</span></button>)}</div><div className="overview-filter-input"><Search size={12} /><input type="text" aria-label={activeTab.filter} placeholder={activeTab.filter} value={overviewQuery} onChange={(event) => { setOverviewQuery(event.currentTarget.value); setOverviewPage(0); }} /></div></div>{overviewTab === "worktrees" && <><div className="table-header" aria-hidden="true"><span>Worktree</span><span>Status</span><span>Last commit</span></div><div className="worktree-list">{overviewSlice(filteredWorktrees).map((worktree) => {
          const changes = statusByPath[worktree.path];
          const selected = worktree.path === selectedWorktreePath;
          const detached = !worktree.branch.startsWith("refs/heads/");
          const summary = detached ? undefined : branchByRef.get(worktree.branch);
          const stale = summary !== undefined && Date.now() / 1000 - summary.commit_date > STALE_AFTER_SECONDS;
          const syncBase = summary?.upstream ?? activeInventory?.default_branch ?? null;
          const ahead = summary?.ahead ?? 0;
          const behind = summary?.behind ?? 0;
          const openDefaultReview = () => { setSelectedWorktreePath(worktree.path); void openReview({ kind: "worktree", worktree }); };
          return <div key={worktree.path} className={`worktree-row ${selected ? "selected" : ""}`} role="button" tabIndex={0} title={worktree.path} aria-pressed={selected} onClick={openDefaultReview} onKeyDown={(event) => { if (event.target !== event.currentTarget) return; if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openDefaultReview(); } }}><div className="worktree-cell"><div className="branch-title"><GitBranch size={14} /><strong>{shortToken(worktree.branch)}</strong>{worktree.branch === activeInventory?.default_branch && <span className="main-badge">main</span>}{detached && <span className="detached-badge">detached</span>}{stale && <span className="stale-badge">stale</span>}</div><div className="worktree-path"><span className="path-text">{worktree.path}</span><CopyButton ghost value={worktree.path} label="Copy worktree path" /></div></div><div className="status-cell">{changes !== undefined && changes !== null && (changes ? <button className="status-chip dirty" type="button" title="Review working changes: the uncommitted diff against HEAD" onClick={(event) => { event.stopPropagation(); setSelectedWorktreePath(worktree.path); void openReview({ kind: "worktree", worktree }, activeRepoPath, { base: workingChangesBase(worktree), scope: "all", reversed: false }); }}>{changesLabel(changes)}</button> : <span className="status-chip clean">Clean</span>)}{syncChip(syncBase, ahead, behind, (base) => { setSelectedWorktreePath(worktree.path); void openReview({ kind: "worktree", worktree }, activeRepoPath, { base, scope: "committed", reversed: false }); })}</div><div className="last-commit-cell">{summary ? <><span className="row-commit-subject">{summary.subject}</span><span className="row-commit-age">{summary.author} · {relativeTime(summary.commit_date)}</span></> : <span className="row-commit-age">{detached ? `HEAD ${shortToken(worktree.head)}` : ""}</span>}</div></div>;
        })}</div>{filteredWorktrees.length === 0 && <div className="filter-empty">{overviewNeedle ? `No worktrees match "${overviewQuery}"` : "No worktrees"}</div>}</>}{overviewTab !== "worktrees" && <><div className="table-header" aria-hidden="true">{overviewTab === "archived" ? <><span>Archived surface</span><span>Detail</span><span>Last seen</span></> : <><span>Branch</span><span>Sync</span><span>Last commit</span></>}</div><div className="worktree-list">{overviewTab === "archived" ? <>{overviewSlice(filteredGone).map((surface) => archivedRow(surface))}{filteredGone.length === 0 && <div className="filter-empty">{overviewNeedle ? `No archived surfaces match "${overviewQuery}"` : "No archived surfaces"}</div>}</> : <>{overviewSlice(overviewTab === "branches" ? filteredBranches : filteredRemoteBranches).map((branch) => inventoryBranchRow(branch))}{(overviewTab === "branches" ? filteredBranches : filteredRemoteBranches).length === 0 && <div className="filter-empty">{overviewNeedle ? `No branches match "${overviewQuery}"` : inventoryUnavailable ? "Branch inventory unavailable" : activeInventory ? "No branches" : "Loading branch inventory..."}</div>}</>}</div></>}{overviewPageCount > 1 && <Pager label={activeTab.pager} page={visibleOverviewPage} pages={overviewPageCount} total={overviewTotal} size={activeTab.pageSize} onPage={setOverviewPage} />}</>}</section>}</main></section>{arrivals.length > 0 && <div className="arrival-cue" role="status" aria-label="Review arrivals"><button className="arrival-open" type="button" onClick={openOldestArrival}><Inbox size={13} /><span><strong>{arrivals[arrivals.length - 1].agent_name}</strong> delivered a review{arrivals.length > 1 ? ` (+${arrivals.length - 1} new)` : ""}</span></button><button className="arrival-dismiss" type="button" aria-label="Dismiss review arrivals" title="Dismiss" onClick={dismissArrivals}><X size={12} /></button></div>}{settingsLocation && <SettingsPage settings={settings} saveError={settingsSaveError} onBack={() => nav.back()} onChange={(next) => void updateSettings(next)} />}{paletteOpen && <dialog className="palette-backdrop" ref={paletteRef} aria-label="Find repositories and worktrees" onClose={handlePaletteClosed} onKeyDown={handlePaletteKeyDown} onMouseDown={(event) => { if (event.target === event.currentTarget) closePalette(); }}><div className="palette" onMouseDown={(event) => event.stopPropagation()}><div className="palette-input-row"><Search size={17} /><input autoFocus value={query} onChange={(event) => { setQuery(event.currentTarget.value); setSearchPage(0); }} placeholder="Find repositories, branches, and worktrees" /><button type="button" aria-label="Close search" onClick={closePalette}><X size={16} /></button></div><div className="palette-results"><div className="nav-section-label">Repositories and worktrees</div>{visibleSearchResults.map((result) => <button key={`${result.repo.path}:${result.worktree?.path ?? "repo"}`} type="button" title={result.worktree?.path ?? result.repo.path} onClick={() => { activateRepo(result.repo); if (!result.worktree) { closePalette(); } else { setSelectedWorktreePath(result.worktree.path); void openReview({ kind: "worktree", worktree: result.worktree }, result.repo.path); closePalette(); } }}>{result.worktree ? <GitBranch size={16} /> : <FolderGit2 size={16} />}<span><strong>{result.worktree ? shortToken(result.worktree.branch) : result.repo.name}</strong>{result.worktree && <small>{result.repo.name}</small>}</span><kbd>Enter</kbd></button>)}{matchingResults.length === 0 && <p>No repositories or worktrees match "{query}".</p>}{searchPageCount > 1 && <Pager label="Search result pages" page={visibleSearchPage} pages={searchPageCount} total={matchingResults.length} size={SEARCH_PAGE_SIZE} onPage={setSearchPage} />}</div></div></dialog>}{removeTarget && <dialog className="palette-backdrop" ref={confirmRef} aria-label="Remove project" onClose={() => setRemoveTarget(null)} onMouseDown={(event) => { if (event.target === event.currentTarget) setRemoveTarget(null); }}><div className="confirm-dialog" onMouseDown={(event) => event.stopPropagation()}><h2>Remove {removeTarget.name}?</h2><p>Removes the project from WorktreeView only. Your repository, worktrees, and history on disk are never touched.</p><div className="confirm-actions"><button className="secondary-button" type="button" onClick={() => setRemoveTarget(null)}>Cancel</button><button className="danger-button" type="button" disabled={removing} onClick={() => void removeProject(removeTarget)}>{removing ? "Removing..." : "Remove project"}</button></div></div></dialog>}</div>;
}

function RepoNavGroup({ repo, active, expanded, onToggle, onPin, rows }: { repo: Repo; active: boolean; expanded: boolean; onToggle: () => void; onPin: () => void; rows: React.ReactNode }) {
  return <div className="project-group"><div className="project-row-wrap"><button className={`project-row ${active ? "active" : ""}`} type="button" title={repo.path} aria-current={active ? "true" : undefined} onClick={onToggle}>{expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}<span className="project-avatar">{repo.name.slice(0, 2)}</span><span className="project-copy"><strong>{repo.name}</strong></span></button><button className="pin-button" type="button" aria-label={repo.pinned_at === null ? "Pin repository" : "Unpin repository"} title={repo.pinned_at === null ? "Pin repository" : "Unpin repository"} onClick={onPin}>{repo.pinned_at === null ? <Pin size={12} /> : <PinOff size={12} />}</button></div>{expanded && <div className="sidebar-children">{rows}</div>}</div>;
}

function Empty({ icon, title, detail, action }: { icon: React.ReactNode; title: string; detail: string; action?: React.ReactNode }) { return <div className="empty-state">{icon}<strong>{title}</strong><span>{detail}</span>{action}</div>; }
function Pager({ label, page, pages, total, size, pageLabel = false, onPage }: { label: string; page: number; pages: number; total: number; size: number; pageLabel?: boolean; onPage: (page: number) => void }) { return <div className="page-controls" role="group" aria-label={label}><button type="button" disabled={page === 0} onClick={() => onPage(Math.max(0, page - 1))}>Previous</button><span>{pageLabel ? `Page ${page + 1} of ${pages}` : `${page * size + 1}-${Math.min((page + 1) * size, total)} of ${total}`}</span><button type="button" disabled={page === pages - 1} onClick={() => onPage(Math.min(pages - 1, page + 1))}>Next</button></div>; }

function RefPicker({ id, label, refs, value, onChange, exclude = [] }: { id: string; label: string; refs: string[]; value: string; onChange: (value: string) => void; exclude?: string[] }) {
  const [query, setQuery] = useState("");
  const [open, setOpen] = useState(false);
  const [page, setPage] = useState(0);
  const search = query.trim().toLowerCase();
  const options = refs.filter((ref) => !exclude.includes(ref));
  const matches = search ? options.filter((ref) => ref.toLowerCase().includes(search)) : options;
  const pages = Math.max(1, Math.ceil(matches.length / BRANCH_PAGE_SIZE));
  const visiblePage = Math.min(page, pages - 1);
  const visibleRefs = matches.slice(visiblePage * BRANCH_PAGE_SIZE, (visiblePage + 1) * BRANCH_PAGE_SIZE);
  useEffect(() => { setQuery(""); setPage(0); }, [value]);
  return <div className="branch-picker"><label htmlFor={id}>{label}</label><code className="branch-selection">{value ? shortToken(value) : "No base selected"}</code><input id={id} role="combobox" aria-controls={`${id}-options`} aria-expanded={open} aria-autocomplete="list" value={query} placeholder="Search refs" onFocus={() => setOpen(true)} onChange={(event) => { setQuery(event.currentTarget.value); setPage(0); setOpen(true); }} onKeyDown={(event) => { if (event.key === "Escape") setOpen(false); }} />{open && <div className="branch-options" id={`${id}-options`} role="listbox" aria-label={`${label} branches`}>{visibleRefs.map((ref) => <button key={ref} type="button" role="option" aria-selected={ref === value} onClick={() => { onChange(ref); setOpen(false); }}>{shortToken(ref)}</button>)}{matches.length === 0 && <span>No matching refs</span>}{pages > 1 && <Pager label={`${label} branch pages`} page={visiblePage} pages={pages} total={matches.length} size={BRANCH_PAGE_SIZE} onPage={setPage} />}</div>}</div>;
}

function ReviewView({ repoPath, liveWorktree, target, refs, base, scope, reversed, index, loading, selectedFile, patch, patchError, patchLoading, filePage, hunkPage, diffPrefs, diffToggles, comments, canBack, canForward, onHistoryBack, onHistoryForward, onBack, onBaseChange, onTargetChange, onPreset, onReverse, onFilePage, onHunkPage, onFile }: { repoPath: string; liveWorktree?: Worktree; target: ReviewTarget; refs: RefInventory; base: string; scope: ReviewScope; reversed: boolean; index: ReviewIndex | null; loading: boolean; selectedFile: ChangedFile | null; patch: FilePatch | null; patchError: string; patchLoading: boolean; filePage: number; hunkPage: number; diffPrefs: DiffPreferences; diffToggles: React.ReactNode; comments: CommentsApi; canBack: boolean; canForward: boolean; onHistoryBack: () => void; onHistoryForward: () => void; onBack: () => void; onBaseChange: (value: string) => void; onTargetChange: (target: ReviewTarget) => void; onPreset: (preset: WorktreeReviewPreset) => void; onReverse: () => void; onFilePage: (page: number) => void; onHunkPage: (page: number) => void; onFile: (file: ChangedFile) => void }) {
  const targetName = target.kind === "worktree" ? target.worktree.branch : target.kind === "commit" ? target.sha : target.name;
  const allRefs = [...refs.heads, ...refs.remotes, ...refs.tags];
  const targetRefs = allRefs.filter((ref) => ref !== liveWorktree?.branch);
  const targetOptions = liveWorktree ? [liveWorktree.branch, ...targetRefs] : allRefs;
  const files = index?.files ?? [];
  const commitPreset = target.kind === "commit" ? target.parents[0] ?? "empty-tree" : "";
  const branchPreset = refs.default_base ?? "";
  const workingBase = target.kind === "worktree" ? workingChangesBase(target.worktree) : "";
  const workingChangesActive = base === workingBase && scope === "all" && !reversed;
  return <section className="review-view" aria-label="Code review">
    <header className="review-header">
       <div className="review-heading"><div className="review-nav"><button className="icon-button" type="button" aria-label="Navigate back" title="Back" disabled={!canBack} onClick={onHistoryBack}><ArrowLeft size={15} /></button><button className="icon-button" type="button" aria-label="Navigate forward" title="Forward" disabled={!canForward} onClick={onHistoryForward}><ArrowRight size={15} /></button></div><button className="back-button" type="button" onClick={onBack}><ArrowLeft size={15} /> Overview</button><p className="eyebrow">Review</p><h1>{shortToken(targetName)}</h1><div className="review-meta"><code>{target.kind === "worktree" ? target.worktree.path : repoPath}</code><code>{target.kind === "ref" ? target.name : target.kind === "commit" ? shortToken(target.sha) : `HEAD ${shortToken(target.worktree.head)}`}</code></div></div>
       <div className="review-controls"><RefPicker id="review-target" label="Compare" refs={targetOptions} value={targetName} onChange={(value) => onTargetChange(liveWorktree && value === liveWorktree.branch ? { kind: "worktree", worktree: liveWorktree } : { kind: "ref", name: value })} /><RefPicker id="review-base" label="Base" refs={allRefs} value={base} exclude={target.kind === "worktree" ? [] : [targetName]} onChange={onBaseChange} /><button className="swap-button" type="button" aria-label="Swap review direction" title="Swap review direction" onClick={onReverse}>↔ <code>{reversed ? shortToken(targetName) : shortToken(base)}...{reversed ? shortToken(base) : shortToken(targetName)}</code></button>{comments.key && <button className="secondary-button" type="button" aria-label="Comment on review" title="Comment on review" onClick={() => comments.openComposer("review")}><MessageSquare size={13} /> Review</button>}{target.kind === "commit" ? <div className="scope-toggle" role="group" aria-label="Commit review preset"><button className={!target.defaultBaseAncestor && branchPreset && base === branchPreset && branchPreset !== commitPreset ? "active" : ""} type="button" disabled={target.defaultBaseAncestor || !branchPreset} onClick={() => onBaseChange(branchPreset)}>Branch so far</button><button className={base === commitPreset ? "active" : ""} type="button" onClick={() => onBaseChange(commitPreset)}>This commit</button></div> : target.kind === "worktree" ? <div className="scope-toggle" role="group" aria-label="Review scope"><button className={workingChangesActive ? "active" : ""} type="button" title="Only the uncommitted working-tree changes" onClick={() => onPreset("working")}>Working changes</button><button className={scope === "all" && !workingChangesActive ? "active" : ""} type="button" title="Everything against the base, uncommitted included" onClick={() => onPreset("all")}>All changes</button><button className={scope === "committed" && base !== workingBase ? "active" : ""} type="button" onClick={() => onPreset("committed")}>Committed only</button></div> : <span className="scope-fixed" aria-label="Review scope">Committed only</span>}</div>
       <div className="review-status"><div className="review-counts"><code>{index?.error ? "Review index unavailable" : index ? `${files.length} files, +${index.additions} -${index.deletions}` : "Loading review index..."}</code></div>{index && !index.error && <div className="review-identity"><code>base {shortToken(index.base_sha)} / target {shortToken(index.target_sha)}</code></div>}{diffToggles}</div>
    </header>
    {!base && !loading ? <Empty icon={<GitBranch size={24} />} title="Choose a base branch to review" detail="This review has no default base." action={<RefPicker id="prompt-review-base" label="Choose base" refs={allRefs} value={base} exclude={target.kind === "worktree" ? [] : [targetName]} onChange={onBaseChange} />} /> : <div className="review-body comments-visible">
      <FileIndexPane base={base} index={index} loading={loading} selectedFile={selectedFile} filePage={filePage} onFilePage={onFilePage} onFile={onFile} />
      <PatchPane selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} hunkPage={hunkPage} diffPrefs={diffPrefs} reversed={reversed} comments={comments} onHunkPage={onHunkPage} onDiskWorktree={reviewFileWorktree(target, selectedFile, reversed)} />
      <CommentStream comments={comments} reversed={reversed} strip={<ReviewsStrip reviewKey={comments.key} />} />
    </div>}
  </section>;
}

function FileIndexPane({ base, index, loading, selectedFile, filePage, onFilePage, onFile }: { base: string; index: ReviewIndex | null; loading: boolean; selectedFile: ChangedFile | null; filePage: number; onFilePage: (page: number) => void; onFile: (file: ChangedFile) => void }) {
  const files = index?.files ?? [];
  const pages = Math.max(1, Math.ceil(files.length / WORKTREE_PAGE_SIZE));
  const page = Math.min(filePage, pages - 1);
  const visibleFiles = files.slice(page * WORKTREE_PAGE_SIZE, (page + 1) * WORKTREE_PAGE_SIZE);
  return <aside className="file-index" aria-label="Changed files"><div className="pane-heading"><strong>Changed files</strong><span>{files.length}</span></div>{loading ? <div className="index-skeleton">{Array.from({ length: 7 }, (_, i) => <i key={i} />)}</div> : index?.error ? <Empty icon={<CircleDot size={20} />} title="Review unavailable" detail={index.error} /> : files.length === 0 ? <Empty icon={<CircleDot size={20} />} title={`No changes vs ${shortToken(base)}`} detail="Try a different base branch." /> : <><div className="file-list" role="listbox" aria-label="Changed files">{visibleFiles.map((file) => <button key={file.path} type="button" role="option" aria-selected={selectedFile?.path === file.path} className={`file-row status-${file.status.toLowerCase()} ${selectedFile?.path === file.path ? "selected" : ""}`} title={file.path} onClick={() => onFile(file)} onKeyDown={(event) => { const current = visibleFiles.findIndex((item) => item.path === file.path); const next = event.key === "ArrowDown" ? current + 1 : event.key === "ArrowUp" ? current - 1 : -1; if (next >= 0 && next < visibleFiles.length) { event.preventDefault(); onFile(visibleFiles[next]); (event.currentTarget.parentElement?.children[next] as HTMLElement)?.focus(); } }}><b>{file.status}</b><span>{file.path}</span></button>)}</div>{pages > 1 && <Pager label="Changed file pages" page={page} pages={pages} total={files.length} size={WORKTREE_PAGE_SIZE} onPage={onFilePage} />}</>}</aside>;
}

function PatchPane({ selectedFile, patch, patchError, patchLoading, hunkPage, diffPrefs, reversed, comments, onHunkPage, onDiskWorktree }: { selectedFile: ChangedFile | null; patch: FilePatch | null; patchError: string; patchLoading: boolean; hunkPage: number; diffPrefs: DiffPreferences; reversed: boolean; comments: CommentsApi; onHunkPage: (page: number) => void; onDiskWorktree?: string | null }) {
  const [openError, setOpenError] = useState("");
  useEffect(() => setOpenError(""), [selectedFile?.path]);
  const openOnDisk = (reveal: boolean) => {
    if (!onDiskWorktree || !selectedFile) return;
    invoke("open_review_file", { worktreePath: onDiskWorktree, path: selectedFile.path, reveal }).then(() => setOpenError("")).catch((error) => setOpenError(errorMessage(error)));
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
  const patchPages = paginateHunks(hunks);
  const patchPage = Math.min(hunkPage, Math.max(0, patchPages.length - 1));
  const visibleHunks = patchPages[patchPage] ?? [];
  const split = diffPrefs.layout === "split";
  const whitespace = diffPrefs.whitespaceVisible;
  const commentsActive = comments.key !== null;
  useEffect(() => { setSelection(null); setTextSelection(null); setGutterChip(null); setComposeTarget(null); }, [selectedFile?.path]);
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
  const cardsFor: ReturnType<typeof inlineCards> = selectedFile ? inlineCards(comments, selectedFile.path, reversed) : new Map();
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
  // token spans swap in once their whole hunk has been tokenized in the
  // worker (one hunk spans page slices coherently). Tokenization runs off
  // the UI thread, so it can never block rendering or input. A null map
  // (unsupported language, failed tokenization) costs nothing.
  const lang = diffPrefs.syntaxVisible && patch && !patch.binary && selectedFile ? languageForPath(selectedFile.path) : null;
  const [tokenMap, setTokenMap] = useState<Map<DiffLine, TokenLine> | null>(null);
  useEffect(() => {
    if (!lang) {
      setTokenMap(null);
      return;
    }
    let cancelled = false;
    void (async () => {
      const map = new Map<DiffLine, TokenLine>();
      const tokenized = new Set<number>();
      for (const slice of visibleHunks) {
        if (tokenized.has(slice.hunkIndex)) continue;
        tokenized.add(slice.hunkIndex);
        const hunk = hunks[slice.hunkIndex];
        const tokens = await tokenizeHunk(hunk.lines, lang, () => cancelled);
        if (cancelled) return;
        if (!tokens) continue;
        const sources = hunkSideSources(hunk.lines);
        sources.old.forEach((line, index) => map.set(line, tokens.old[index] ?? []));
        sources.new.forEach((line, index) => map.set(line, tokens.new[index] ?? []));
      }
      if (!cancelled) setTokenMap(map);
    })();
    return () => { cancelled = true; };
  }, [patch, patchPage, lang]);
  const tokensOf = (line: DiffLine) => tokenMap?.get(line);
  return <section ref={paneRef} className="patch-pane" aria-label="File patch">{selectedFile && <div className="patch-heading"><code title={selectedFile.path}>{selectedFile.path}</code><span className="patch-heading-meta"><span>{selectedFile.status}</span>{onDiskWorktree && <><button className="icon-button" type="button" aria-label="Open file" title="Open file" onClick={() => openOnDisk(false)}><ExternalLink size={13} /></button><button className="icon-button" type="button" aria-label="Reveal in file explorer" title="Reveal in file explorer" onClick={() => openOnDisk(true)}><FolderOpen size={13} /></button></>}{comments.key && <button className="icon-button" type="button" aria-label="Comment on file" title="Comment on file" onClick={() => comments.openFileComposer(selectedFile.path)}><MessageSquare size={13} /></button>}</span></div>}{openError && <p className="patch-open-error" role="status">{openError}</p>}{comments.composer?.kind === "file" && selectedFile && comments.composer.filePath === selectedFile.path && <div className="comment-composer-panel"><p className="eyebrow">Comment on {selectedFile.path}</p><DraftComposer placeholder={`Comment on ${selectedFile.path}`} submitLabel="Comment" onSubmit={({ body, severity }) => { void comments.create({ body, severity, file_path: selectedFile.path, side: null, start_line: null, end_line: null, lines: [] }).then(comments.closeComposer); }} onCancel={comments.closeComposer} /></div>}{patchLoading ? <div className="patch-skeleton" aria-label="Loading patch"><i /><i /><i /><i /></div> : patchError ? <Empty icon={<FileDiff size={24} />} title="Patch not rendered" detail={patchError} /> : !selectedFile ? <Empty icon={<FileDiff size={24} />} title="Select a changed file" detail="The patch is rendered one file at a time." /> : patch?.binary ? <Empty icon={<FileDiff size={24} />} title="Binary file changed" detail={selectedFile.path} /> : patch?.text === "" ? <Empty icon={<CircleDot size={24} />} title="No changes in this file" detail="The selected file has no renderable patch." /> : patchPages.length === 0 ? <pre className="patch-metadata"><code>{patch?.text}</code></pre> : <><div className={`hunk-list ${split ? "split-layout" : ""} ${diffPrefs.lineWrap ? "wrap-lines" : ""}`}>{visibleHunks.map((hunk, hunkIndex) => <div className="hunk" key={`${hunk.header}-${hunkIndex}`}><div className="hunk-header">{hunk.header}</div>{split ? <div className="split-rows">{pairHunkLines(hunk.lines).map((row, rowIndex) => <Fragment key={`${row.old?.oldLine}-${row.new?.newLine}-${rowIndex}`}>{row.old ? <DiffHalf side="LEFT" gutter={row.old.oldLine} text={row.old.text} whitespace={whitespace} tokens={tokensOf(row.old)} commentsActive={commentsActive} selected={selectedClass("LEFT", row.old.oldLine)} chip={commentsActive ? chipFor("LEFT", row.old.oldLine) : null} onRowClick={rowClick} onGutterDown={beginGutterDrag} onEnter={extendDrag} /> : <div className="diff-line half" />}{inlineAfter("LEFT", row.old?.oldLine ?? null)}{row.new ? <DiffHalf side="RIGHT" gutter={row.new.newLine} text={row.new.text} whitespace={whitespace} tokens={tokensOf(row.new)} commentsActive={commentsActive} selected={selectedClass("RIGHT", row.new.newLine)} chip={commentsActive ? chipFor("RIGHT", row.new.newLine) : null} onRowClick={rowClick} onGutterDown={beginGutterDrag} onEnter={extendDrag} /> : <div className="diff-line half" />}{inlineAfter("RIGHT", row.new?.newLine ?? null)}</Fragment>)}</div> : hunk.lines.map((line, lineIndex) => { const target = commentsActive ? selectableRow(line) : null; return <Fragment key={`${line.oldLine}-${line.newLine}-${lineIndex}`}><div className={`diff-line ${line.text.startsWith("+") ? "addition" : line.text.startsWith("-") ? "deletion" : ""}${target ? " commentable" : ""}${target ? selectedClass(target.side, target.number) : ""}`} data-side={target?.side} data-line={target?.number} onClick={target ? (event) => rowClick(target.side, target.number, event.shiftKey) : undefined} onMouseEnter={target ? () => extendDrag(target.side, target.number) : undefined}><span className="line-number" onMouseDown={target ? (event) => { event.preventDefault(); beginGutterDrag(target.side, target.number); } : undefined}>{line.oldLine ?? ""}</span><span className="line-number" onMouseDown={target ? (event) => { event.preventDefault(); beginGutterDrag(target.side, target.number); } : undefined}>{line.newLine ?? ""}</span><code>{diffLineContent(line.text, whitespace, tokensOf(line))}</code>{target && chipFor(target.side, target.number)}</div>{target && inlineAfter(target.side, target.number)}</Fragment>; })}</div>)}</div>{patchPages.length > 1 && <Pager label="Patch pages" page={patchPage} pages={patchPages.length} total={patchPages.length} size={1} pageLabel onPage={onHunkPage} />}</>}</section>;
}

function DiffHalf({ side, gutter, text, whitespace, tokens, commentsActive, selected, chip, onRowClick, onGutterDown, onEnter }: { side: DisplaySide; gutter: number | null; text: string; whitespace: boolean; tokens?: TokenLine; commentsActive: boolean; selected: string; chip: React.ReactNode; onRowClick: (side: DisplaySide, number: number, extend: boolean) => void; onGutterDown: (side: DisplaySide, number: number) => void; onEnter: (side: DisplaySide, number: number) => void }) {
  const target = commentsActive && gutter !== null;
  return <div className={`diff-line half ${text.startsWith("+") ? "addition" : text.startsWith("-") ? "deletion" : ""}${target ? " commentable" : ""}${selected}`} data-side={target ? side : undefined} data-line={target ? gutter : undefined} onClick={target ? (event) => onRowClick(side, gutter, event.shiftKey) : undefined} onMouseEnter={target ? () => onEnter(side, gutter) : undefined}><span className="line-number" onMouseDown={target ? (event) => { event.preventDefault(); onGutterDown(side, gutter); } : undefined}>{gutter ?? ""}</span><code>{diffLineContent(text, whitespace, tokens)}</code>{chip}</div>;
}

function DiffToggles({ settings, onChange }: { settings: Settings; onChange: (next: Settings) => void }) {
  return <div className="diff-toggles" role="group" aria-label="Diff display options">
    <button className={`icon-button ${settings.syntax_visible ? "active" : ""}`} type="button" aria-pressed={settings.syntax_visible} aria-label="Syntax highlighting" title="Syntax highlighting" onClick={() => onChange({ ...settings, syntax_visible: !settings.syntax_visible })}><Code size={12} /></button>
    <button className={`icon-button ${settings.diff_layout === "split" ? "active" : ""}`} type="button" aria-pressed={settings.diff_layout === "split"} aria-label="Split diff layout" title="Split diff layout" onClick={() => onChange({ ...settings, diff_layout: settings.diff_layout === "split" ? "unified" : "split" })}><Columns2 size={12} /></button>
    <button className={`icon-button ${settings.whitespace_visible ? "active" : ""}`} type="button" aria-pressed={settings.whitespace_visible} aria-label="Visible whitespace" title="Visible whitespace" onClick={() => onChange({ ...settings, whitespace_visible: !settings.whitespace_visible })}><Space size={12} /></button>
    <button className={`icon-button ${settings.line_wrap ? "active" : ""}`} type="button" aria-pressed={settings.line_wrap} aria-label="Wrap lines" title="Wrap lines" onClick={() => onChange({ ...settings, line_wrap: !settings.line_wrap })}><WrapText size={12} /></button>
  </div>;
}


function HistoryView({ history, historyRefs, index, loading, selectedCommit, selectedFile, patch, patchError, patchLoading, filePage, hunkPage, diffPrefs, diffToggles, comments, onBack, onBasePick, onFilePage, onHunkPage, onFile }: { history: HistoryState; historyRefs: RefInventory; index: ReviewIndex | null; loading: boolean; selectedCommit: CommitInfo | null; selectedFile: ChangedFile | null; patch: FilePatch | null; patchError: string; patchLoading: boolean; filePage: number; hunkPage: number; diffPrefs: DiffPreferences; diffToggles: React.ReactNode; comments: CommentsApi; onBack: () => void; onBasePick: (base: string) => void; onFilePage: (page: number) => void; onHunkPage: (page: number) => void; onFile: (file: ChangedFile) => void }) {
  const selected = selectedCommit;
  const allRefs = [...historyRefs.heads, ...historyRefs.remotes, ...historyRefs.tags];
  if (!selected) {
    return <section className="review-view" aria-label="Commit history">
      <header className="review-header">
        <div className="review-heading"><button className="back-button" type="button" onClick={onBack}><ArrowLeft size={15} /> Overview</button><p className="eyebrow">History</p><h1>{history.startPointLabel}</h1><div className="review-meta"><code>Select a commit in the sidebar to inspect its own changes.</code></div></div>
      </header>
    </section>;
  }
  const quickBase = selected.parents[0] ?? "empty-tree";
  const escalationDefault = historyRefs.default_base && !selected.default_base_ancestor ? historyRefs.default_base : quickBase;
  return <section className="review-view" aria-label="Commit history">
    <header className="review-header">
      <div className="review-heading"><button className="back-button" type="button" onClick={onBack}><ArrowLeft size={15} /> Worktrees</button><p className="eyebrow">History / {history.startPointLabel}</p><h1>{shortToken(selected.sha)}</h1><p className="history-subject">{selected.subject}</p><div className="review-meta"><span>{selected.author}</span><span>{selected.date.slice(0, 10)}</span>{selected.refs.map((ref) => <span key={ref} className="commit-ref">{shortToken(ref)}</span>)}</div></div>
      <div className="review-controls"><RefPicker id="history-base" label="Base" refs={allRefs} value={escalationDefault} exclude={[selected.sha]} onChange={onBasePick} />{comments.key && <button className="secondary-button" type="button" aria-label="Comment on review" title="Comment on review" onClick={() => comments.openComposer("review")}><MessageSquare size={13} /> Review</button>}</div>
      <div className="review-status"><div className="review-counts"><code>Quick look: this commit's own changes vs {shortToken(quickBase)}; pick a base to open the full review.</code></div>{diffToggles}</div>
    </header>
    <div className="review-body comments-visible">
      <FileIndexPane base={quickBase} index={index} loading={loading} selectedFile={selectedFile} filePage={filePage} onFilePage={onFilePage} onFile={onFile} />
      <PatchPane selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} hunkPage={hunkPage} diffPrefs={diffPrefs} reversed={false} comments={comments} onHunkPage={onHunkPage} />
      <CommentStream comments={comments} />
    </div>
  </section>;
}

export default App;
