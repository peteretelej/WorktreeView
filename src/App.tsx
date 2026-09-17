import { Fragment, useEffect, useMemo, useRef, useState, useSyncExternalStore, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { ChevronsLeft, Clock, Download, FileCheck, FolderGit2, MessageSquare, ChevronDown, ChevronRight, CircleDot, Copy, CornerUpLeft, GitBranch, GitCommitHorizontal, HardDrive, Inbox, LoaderCircle, MessagesSquare, MoreVertical, Pin, PinOff, Search, Settings as SettingsIcon, Trash2, X } from "lucide-react";
import { createNavigationHistory, DEFAULT_PORTAL_FILTERS, sameReviewTarget, type AppLocation, type BranchInventory, type BranchSummary, type ChangedFile, type CommitInfo, type GoneSurface, type RecordedKey, type RefInventory, type ReviewIdentity, type ReviewScope, type ReviewTarget, type ReviewsStateFilter, type SurfaceListing, type ThreadsStateFilter, type ThreadsVoiceFilter, type Worktree } from "./navigation";
import { autoReviewBase, workingChangesBase, type WorktreeReviewPreset } from "./reviewPresets";
import { arrivalChangeLabel, arrivalProjectLabel, arrivalSentenceBody, olderArrivalsSuffix } from "./arrivals";
import { SettingsPage, applyTheme, defaultSettings, getSettings, persistSettings, type ChangedFilesView, type Settings } from "./settings";
import { DEFAULT_ZOOM, snapZoom, stepZoom, zoomShortcut } from "./zoom";
import { imageMimeForPath } from "./stream";
import { filterGoneSurfaces, goneSurfaceLabel, pinnedSurfaces, surfacePinIndex, surfaceRows, worktreeKey, type SurfaceRow } from "./surfaces";
import { ATTENTION_TABS, attentionAge, attentionPreview, attentionRows, attentionStatus, attentionTabCounts, groupedChangeLabel, isNarrowAttention, requestStatusLabel, reviewsBackLabel, rowsForAttentionTab, type AttentionCategory, type AttentionQueue, type AttentionRow, type RequestChange, } from "./requests.ts";
import { normalizeReviewsSearch, normalizeThreadsText, threadIdentityRef, type PortalReviewRow, type PortalSearchMatches, type ReviewIdentityRef } from "./portal.ts";
import { PortalActivityTab, PortalReviewsTab, PortalThreadDetail, PortalThreadsTab, useNow, usePaneWidth, type PortalActivityPayload, type PortalReviewsPayload, type PortalThreadPayload, type PortalThreadsPayload } from "./portal.tsx";
import { useReviewComments } from "./comments.tsx";
import { copyText } from "./clipboard";
import { BrandMark, CopyButton, Empty, Pager, TopBar, UpdatedStamp } from "./ui";
import { BRANCH_PAGE_SIZE, parseHunks, patchIdentityOf, sameReview, DiffToggles, ReviewView, type DiffPreferences, type FileContent, type FilePatch, type ReviewIndex } from "./review";
import { HistoryView, type CommitPage, type HistoryEntry, type HistoryState } from "./history";
import "./App.css";

import { authorInitials, changesLabel, compactAge, errorCodeOf, errorMessage, originSlug, relativeTime, shortToken, STALE_AFTER_SECONDS, type CommandError } from "./format";
type Repo = { path: string; name: string; worktrees: Worktree[]; pinned_at: number | null };
type SearchResult = { repo: Repo; worktree?: Worktree };
// One palette row: the in-memory repo/worktree search or a store-search
// match; sections render as labels between the rows of one paged list.
type PaletteRow = { section: "repos"; result: SearchResult } | { section: "comments"; match: PortalSearchMatches["comments"][number] } | { section: "requests"; match: PortalSearchMatches["requests"][number] } | { section: "commits"; match: PortalSearchMatches["commits"][number] };
const PALETTE_SECTION_LABELS: Record<PaletteRow["section"], string> = { repos: "Repositories and worktrees", comments: "Threads and comments", requests: "Requests", commits: "Commits" };
type WorktreeStatus = { path: string; changes: number | null };
type OverviewTab = "worktrees" | "branches" | "remote" | "archived";
// Mirrors the payload of the Rust `submission-received` event.
type SubmissionArrival = { repo_path: string; base_sha: string; target_key: string; target_kind: "worktree" | "head"; submission_id: number; agent_name: string };
// One arrival-cue entry: a delivered submission or an announce moment;
// both carry the review identity the cue's open action lands on.
type ArrivalEntry = { moment: "delivery" | "announce"; repo_path: string; base_sha: string; target_key: string; target_kind: "worktree" | "head"; actor: string };
// Mirrors the payload of the Rust `comment-changed` event.
type CommentChange = { repo_path: string; base_sha: string; target_key: string; target_kind: "worktree" | "head"; comment_id: number; action: "created" | "replied" | "resolved" | "unresolved" | "edited" | "deleted"; agent_name: string };
// Mirrors the payload of the Rust `project-refreshed` event.
type ProjectRefresh = { repo_path: string };

const WORKTREE_PAGE_SIZE = 100;
const SEARCH_PAGE_SIZE = 50;
const COMMIT_PAGE_SIZE = 100;
const PATCH_CACHE_LIMIT = 32;
// File content serves both context expansion and the full-file view; kept
// smaller than the patch cache since whole files outweigh patches.
const FILE_CONTENT_CACHE_LIMIT = 8;
// Jump targets land just below the stream's top padding.
// The file view tokenizes in bounded chunks so a huge file swaps tokens in
// progressively instead of waiting on one oversized worker round trip.
// Sidebar children stay scannable: pinned surfaces plus the most recently
// committed worktrees; everything else lives on the project overview.
const OVERVIEW_TABS: Array<{ id: OverviewTab; label: string; filter: string; pager: string; pageSize: number }> = [
  { id: "worktrees", label: "Worktrees", filter: "Filter worktrees", pager: "Worktree pages", pageSize: WORKTREE_PAGE_SIZE },
  { id: "branches", label: "Branches", filter: "Filter branches", pager: "Branch pages", pageSize: BRANCH_PAGE_SIZE },
  { id: "remote", label: "Remote", filter: "Filter remote branches", pager: "Remote branch pages", pageSize: BRANCH_PAGE_SIZE },
  { id: "archived", label: "Archived", filter: "Filter archived", pager: "Archived pages", pageSize: BRANCH_PAGE_SIZE },
];

// Cadence of the overview's quiet local re-read; focus reloads immediately.
const OVERVIEW_RELOAD_INTERVAL_MS = 20000;

// Zoom is applied to the whole webview; a failure (browser preview, refused
// call) keeps the current scale, so the call is best-effort.
async function applyZoom(zoom: number) {
  try { await getCurrentWebview().setZoom(zoom); } catch { /* keep the current zoom */ }
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
  if ((location.kind === "inbox" || location.kind === "portal") && activeRepo) {
    const worktree = activeRepo.worktrees.find((item) => item.path === selectedWorktreePath) ?? activeRepo.worktrees[0];
    return worktree ? { repoPath: activeRepo.path, startPointLabel: shortToken(worktree.branch), worktreePath: worktree.path } : null;
  }
  return null;
}
function historyKeyOf(entry: { repoPath: string; startPointLabel: string; worktreePath?: string; startRef?: string } | null) { return entry ? `${entry.repoPath}\0${entry.startPointLabel}\0${entry.worktreePath ?? ""}\0${entry.startRef ?? ""}` : ""; }

function App() {
  const [repos, setRepos] = useState<Repo[]>([]); const [activeRepoPath, setActiveRepoPath] = useState(""); const [selectedWorktreePath, setSelectedWorktreePath] = useState("");
  const [loading, setLoading] = useState(true); const [opening, setOpening] = useState(false); const [loadError, setLoadError] = useState(""); const [operationError, setOperationError] = useState(""); const [repoErrors, setRepoErrors] = useState<Record<string, string>>({}); const [hydratingRepos, setHydratingRepos] = useState<Record<string, number>>({});
  const [collapsed, setCollapsed] = useState(false); const [paletteOpen, setPaletteOpen] = useState(false); const [query, setQuery] = useState(""); const [searchPage, setSearchPage] = useState(0);
  const [refs, setRefs] = useState<RefInventory>({ heads: [], remotes: [], tags: [], default_base: null }); const [reviewIndex, setReviewIndex] = useState<ReviewIndex | null>(null); const [reviewLoading, setReviewLoading] = useState(false); const [patch, setPatch] = useState<FilePatch | null>(null); const [patchError, setPatchError] = useState(""); const [patchLoading, setPatchLoading] = useState(false); const [fetchingBranchObjects, setFetchingBranchObjects] = useState(false);
  const [fileContent, setFileContent] = useState<FileContent | null>(null); const [fileContentError, setFileContentError] = useState(""); const [fileContentLoading, setFileContentLoading] = useState(false); const [fileImageSrc, setFileImageSrc] = useState<string | null>(null); const [fileImageError, setFileImageError] = useState(""); const [fileImageLoading, setFileImageLoading] = useState(false);
  const fileImageSrcRef = useRef<string | null>(null);
  function setFileImage(next: string | null) {
    // Blob URLs are hand-managed: revoke the one being replaced or cleared.
    if (fileImageSrcRef.current) URL.revokeObjectURL(fileImageSrcRef.current);
    fileImageSrcRef.current = next;
    setFileImageSrc(next);
  }
  const isImageFile = (file: ChangedFile) => imageMimeForPath(file.path) !== null;
  const [expandedRepos, setExpandedRepos] = useState<Record<string, boolean>>({}); const [surfaces, setSurfaces] = useState<Record<string, SurfaceListing>>({}); const [overviewTab, setOverviewTab] = useState<OverviewTab>("worktrees"); const [overviewQuery, setOverviewQuery] = useState(""); const [overviewPage, setOverviewPage] = useState(0); const [fetchingRepos, setFetchingRepos] = useState<Record<string, boolean>>({});
  const [history, setHistory] = useState<HistoryState | null>(null); const [historyRefs, setHistoryRefs] = useState<RefInventory>({ heads: [], remotes: [], tags: [], default_base: null }); const [worktreeStatuses, setWorktreeStatuses] = useState<Record<string, WorktreeStatus[] | null>>({}); const [statusNonce, setStatusNonce] = useState(0);
  const [inventories, setInventories] = useState<Record<string, BranchInventory | null>>({}); const [menuOpen, setMenuOpen] = useState(false); const [removeTarget, setRemoveTarget] = useState<Repo | null>(null); const [removing, setRemoving] = useState(false);
  const [settings, setSettings] = useState<Settings>(defaultSettings); const [settingsSaveError, setSettingsSaveError] = useState("");
  // The arrival cue holds deliveries and announces until opened or
  // dismissed, so it also records what happened while the window was
  // covered.
  const [arrivals, setArrivals] = useState<ArrivalEntry[]>([]);
  // Bumped when a submission arrives for the review identity the user is
  // reading: the open review's comment stream and submissions strip reload
  // from it without navigating away and back.
  const [reviewRefreshTick, setReviewRefreshTick] = useState(0);
  // When each project's local overview data was last re-read, in epoch
  // milliseconds: the header's freshness stamp reads from this.
  const [refreshedAt, setRefreshedAt] = useState<Record<string, number>>({});
  const [attention, setAttention] = useState<AttentionQueue | null>(null);
  const [portalReviews, setPortalReviews] = useState<PortalReviewsPayload | null>(null);
  const [portalThreads, setPortalThreads] = useState<PortalThreadsPayload | null>(null);
  const [portalActivity, setPortalActivity] = useState<PortalActivityPayload | null>(null);
  const [markingSeen, setMarkingSeen] = useState(false);
  const [portalThread, setPortalThread] = useState<PortalThreadPayload>({ detail: null, loading: false, error: "" });
  const [portalSearch, setPortalSearch] = useState<PortalSearchMatches | null>(null);
  const [threadsNonce, setThreadsNonce] = useState(0);
  const paletteRef = useRef<HTMLDialogElement>(null); const paletteOpenerRef = useRef<HTMLElement | null>(null);
  const menuAnchorRef = useRef<HTMLDivElement | null>(null); const confirmRef = useRef<HTMLDialogElement>(null);
  const indexGenerationRef = useRef(0); const patchGenerationRef = useRef(0); const reviewIdentityRef = useRef<ReviewIdentity | null>(null); const patchIdentityRef = useRef(""); const patchCacheRef = useRef(new Map<string, FilePatch>()); const contentCacheRef = useRef(new Map<string, FileContent>()); const contentGenerationRef = useRef(0); const contentIdentityRef = useRef(""); const contentInFlightRef = useRef(false); const historyGenerationRef = useRef(0); const lastBaseRef = useRef<{ repoPath: string; base: string } | null>(null); const refsIdentityRef = useRef<ReviewIdentity | null>(null); const navigateRef = useRef<(delta: number) => void>(() => undefined); const zoomActionsRef = useRef<{ step: (direction: 1 | -1) => void; reset: () => void }>({ step: () => undefined, reset: () => undefined }); const zoomLiveRef = useRef(DEFAULT_ZOOM);
  const paneToggleRef = useRef<() => void>(() => undefined);
  const imageGenerationRef = useRef(0);
  const imageIdentityRef = useRef("");
  const imageInFlightRef = useRef(false);
  const [nav] = useState(createNavigationHistory);
  const location = useSyncExternalStore(nav.subscribe, nav.current);
  const reviewLocation = location.kind === "review" ? location : null;
  const historyLocation = location.kind === "commit-history" ? location : null;
  const portalLocation = location.kind === "portal" ? location : null;
  const threadLocation = location.kind === "thread" ? location : null;
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
  const activeRepo = repos.find((repo) => repo.path === activeRepoPath); const hydrating = Object.keys(hydratingRepos).length > 0; const activeRepoHydrating = activeRepo ? Boolean(hydratingRepos[activeRepo.path]) : false;
  // Display names for cross-project rows; a project removed from the app
  // falls back to its path.
  const repoNames = useMemo(() => new Map(repos.map((repo) => [repo.path, repo.name])), [repos]);
  // The review page reads repo-scoped props from the reviewed project, not
  // the active one: a row opened for an unregistered project must not
  // inherit another repo's worktrees.
  const reviewRepo = reviewLocation ? repos.find((repo) => repo.path === reviewLocation.identity.repoPath) : null;
  const historyAnchor = historyAnchorOf(location, activeRepo, selectedWorktreePath); const historyAnchorKey = historyKeyOf(historyAnchor);
  // The comment layer attaches to whichever review identity is rendered:
  // the review surface's identity, or the commit history quick-look's.
  const commentIdentity: ReviewIdentity | null = reviewLocation?.identity ?? (historyLocation?.selectedCommit ? { repoPath: historyLocation.repoPath, base: historyLocation.selectedCommit.parents[0] ?? "empty-tree", target: commitTargetOf(historyLocation.selectedCommit), scope: "committed", reversed: false } : null);
  const commentPatchLines = useMemo(() => (patch && !patch.binary ? parseHunks(patch.text).flatMap((hunk) => hunk.lines) : []), [patch]);
  const commentFile = selectedFile && patch && !patch.binary ? { path: selectedFile.path, lines: commentPatchLines } : null;
  const comments = useReviewComments(commentIdentity, reviewIndex, commentFile, reversed);
  // The event listeners read the live comment layer and open repo path
  // through refs, so they subscribe once and never hold stale closures.
  const commentsRef = useRef(comments); const activeRepoPathRef = useRef(activeRepoPath); const reposRef = useRef(repos); const settingsRef = useRef(settings);

  useEffect(() => { let mounted = true; async function load() { try { const loaded = await invoke<Repo[]>("list_repos"); if (!mounted) return; setRepos(loaded); setActiveRepoPath((current) => loaded.some((repo) => repo.path === current) ? current : loaded[0]?.path ?? ""); setHydratingRepos(Object.fromEntries(loaded.map((repo) => [repo.path, 1]))); let nextSettings = defaultSettings; try { nextSettings = await getSettings(); } catch (error) { if (mounted) setOperationError(errorMessage(error)); } if (!mounted) return; nextSettings.zoom = snapZoom(nextSettings.zoom); applyTheme(nextSettings.theme); setSettings(nextSettings); setLoading(false); for (let start = 0; start < loaded.length; start += 4) await Promise.all(loaded.slice(start, start + 4).map(async (repo) => { try { const worktrees = await invoke<Worktree[]>("list_worktrees", { path: repo.path }); if (mounted) setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, worktrees } : item)); let listing: SurfaceListing = { gone: [], pinned: [] }; try { listing = await invoke<SurfaceListing>("list_surfaces", { path: repo.path }); } catch { listing = { gone: [], pinned: [] }; } if (mounted) setSurfaces((current) => ({ ...current, [repo.path]: listing })); } catch (error) { if (mounted) setRepoErrors((current) => ({ ...current, [repo.path]: errorMessage(error) })); } finally { if (mounted) setHydratingRepos((current) => { const count = current[repo.path] ?? 0; if (count > 1) return { ...current, [repo.path]: count - 1 }; const next = { ...current }; delete next[repo.path]; return next; }); } })); } catch (error) { if (mounted) { setLoadError(errorMessage(error)); setLoading(false); } } } void load(); return () => { mounted = false; }; }, []);
  useEffect(() => { if (!activeRepo) { setSelectedWorktreePath(""); return; } setSelectedWorktreePath((current) => activeRepo.worktrees.some((worktree) => worktree.path === current) ? current : activeRepo.worktrees[0]?.path ?? ""); }, [activeRepo]);
  // The overview's change badges come from one bounded status probe per
  // worktree; they refresh when the repo activates, its worktree count
  // changes, the project reloads (focus, the quiet interval, or an agent
  // ping), or a fetch asks for a fresh pass. A successful probe also
  // stamps the freshness: the stamp marks the last local read, never the
  // last fetch.
  useEffect(() => {
    const repoPath = activeRepoPath;
    const worktreeCount = activeRepo?.worktrees.length ?? 0;
    if (!repoPath || worktreeCount === 0) return;
    let cancelled = false;
    void (async () => {
      try {
        const statuses = await invoke<WorktreeStatus[]>("list_worktree_status", { path: repoPath });
        if (!cancelled) {
          setWorktreeStatuses((current) => ({ ...current, [repoPath]: statuses }));
          setRefreshedAt((current) => ({ ...current, [repoPath]: Date.now() }));
        }
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
  // Agents edit worktrees and commit while the window is away or idle, so
  // the open project re-reads its local state on focus and on a quiet
  // interval. That pass is read-only Git; the network fetch stays a
  // deliberate action and never runs on this path.
  const relistRef = useRef(relistProject);
  useEffect(() => { relistRef.current = relistProject; });
  useEffect(() => {
    const repoPath = activeRepoPath;
    if (!repoPath) return;
    let inFlight = false;
    async function reloadIfVisible() {
      if (document.hidden || inFlight) return;
      inFlight = true;
      try { await relistRef.current(repoPath); } finally { inFlight = false; }
    }
    const reloadSoon = () => { void reloadIfVisible(); };
    window.addEventListener("focus", reloadSoon);
    document.addEventListener("visibilitychange", reloadSoon);
    const timer = window.setInterval(() => void reloadIfVisible(), OVERVIEW_RELOAD_INTERVAL_MS);
    return () => {
      window.removeEventListener("focus", reloadSoon);
      document.removeEventListener("visibilitychange", reloadSoon);
      window.clearInterval(timer);
    };
  }, [activeRepoPath]);
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
  useEffect(() => { function handleKeyboard(event: KeyboardEvent) { if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") { event.preventDefault(); paletteRef.current?.open ? closePalette() : openPalette(); return; } if ((event.ctrlKey || event.metaKey) && !event.altKey) { const shortcut = zoomShortcut(event.key); if (shortcut) { event.preventDefault(); if (shortcut === "reset") zoomActionsRef.current.reset(); else zoomActionsRef.current.step(shortcut === "in" ? 1 : -1); return; } } if ((event.ctrlKey || event.metaKey) && !event.shiftKey && !event.altKey && event.key.toLowerCase() === "b") { if (!paletteRef.current?.open) { event.preventDefault(); paneToggleRef.current(); } return; } if (!event.altKey || (event.key !== "ArrowLeft" && event.key !== "ArrowRight") || paletteRef.current?.open) return; event.preventDefault(); navigateRef.current(event.key === "ArrowLeft" ? -1 : 1); } window.addEventListener("keydown", handleKeyboard); return () => window.removeEventListener("keydown", handleKeyboard); }, []);
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
      if (disposed) return;
      const arrival = event.payload;
      void arrive({ moment: "delivery", repo_path: arrival.repo_path, base_sha: arrival.base_sha, target_key: arrival.target_key, target_kind: arrival.target_kind, actor: arrival.agent_name });
      // A submission moves request statuses, so the queue follows.
      void refreshAttention();
      // A delivery for the review the user is reading refreshes it live:
      // the tick drives the open review's stream and submissions strip.
      const key = commentsRef.current.key;
      if (key && key.repoPath === arrival.repo_path && key.baseSha === arrival.base_sha && key.targetKey === arrival.target_key && key.targetKind === arrival.target_kind) {
        setReviewRefreshTick((tick) => tick + 1);
      }
    });
    return () => { disposed = true; void subscription.then((unsubscribe) => unsubscribe()); };
  }, []);
  // Every request mutation (agent tool or human command) fires the same
  // event, so the queue and its tab count track without polling. A failed
  // refresh keeps the last payload; the store-backed queue is stale, not
  // gone. The announce moment is the one narrated mutation that also
  // surfaces as an arrival: claims, verdicts, and dedup refreshes carry
  // other event kinds or null and produce no notification.
  useEffect(() => {
    let disposed = false;
    const subscription = listen<RequestChange>("review-request-changed", (event) => {
      if (disposed) return;
      void refreshAttention().then((queue) => {
        const change = event.payload;
        if (disposed || change.event !== "review_announced") return;
        // The announcing agent is the queue row's requester: announce
        // writes the caller's token as the requester on both of its
        // notified paths (fresh row and the caller's own refreshable row).
        const actor = attentionRows(queue).find((row) => row.request_id === change.request_id)?.requester ?? "An agent";
        void arrive({ moment: "announce", repo_path: change.repo_path, base_sha: change.base_sha, target_key: change.target_key, target_kind: change.target_kind, actor });
      });
    });
    return () => { disposed = true; void subscription.then((unsubscribe) => unsubscribe()); };
  }, []);
  useEffect(() => { void refreshAttention(); }, []);
  // The Reviews tab's listing loads while the portal is open (not only on
  // its own tab) so the shared tab strip's count stays live: the search
  // needle narrows server-side (debounced while typing), and a refreshed
  // attention payload re-runs it so request events keep the tab live.
  // A failed refresh keeps the last payload; the tab is stale, not gone.
  useEffect(() => {
    if (!portalLocation) return;
    const search = normalizeReviewsSearch(portalLocation.filters.reviewsSearch);
    let cancelled = false;
    const timer = setTimeout(() => {
      invoke<PortalReviewRow[]>("list_portal_reviews", { repoPath: null, stateFilter: null, search: search || null })
        .then((rows) => { if (!cancelled) setPortalReviews({ rows, loading: false, error: "" }); })
        .catch((error) => { if (!cancelled) setPortalReviews((current) => ({ rows: current?.rows ?? [], loading: false, error: errorMessage(error) })); });
    }, search ? 250 : 0);
    return () => { cancelled = true; clearTimeout(timer); };
  }, [portalLocation?.tab, portalLocation?.filters.reviewsSearch, attention]);
  // The Threads tab's listing (and the detail view's other-threads column)
  // loads while the portal is open so the tab strip's count stays live: the
  // text needle narrows server-side; the discrete filters stay client-side
  // over the same payload so chip counts keep their meaning. A refreshed
  // attention payload re-runs it so comment-driven activity keeps the tab
  // live; failures keep the last payload.
  useEffect(() => {
    if (!portalLocation && !threadLocation) return;
    const text = normalizeThreadsText(portalLocation?.filters.threadsText ?? "");
    let cancelled = false;
    const timer = setTimeout(() => {
      invoke<PortalThreadsPayload["groups"]>("list_portal_threads", { repoPath: null, stateFilter: "all", voice: "all", text: text || null })
        .then((groups) => { if (!cancelled) setPortalThreads({ groups, loading: false, error: "" }); })
        .catch((error) => { if (!cancelled) setPortalThreads((current) => ({ groups: current?.groups ?? [], loading: false, error: errorMessage(error) })); });
    }, text ? 250 : 0);
    return () => { cancelled = true; clearTimeout(timer); };
  }, [portalLocation?.tab, portalLocation?.filters.threadsText, threadLocation?.commentId, attention, threadsNonce]);
  // The thread detail follows the thread location: one store read per open
  // thread; mutations refetch through the nonce below.
  useEffect(() => {
    if (!threadLocation) return;
    const commentId = threadLocation.commentId;
    let cancelled = false;
    setPortalThread((current) => ({ detail: current.detail?.root_comment_id === commentId ? current.detail : null, loading: true, error: "" }));
    invoke<PortalThreadPayload["detail"]>("get_portal_thread", { rootCommentId: commentId })
      .then((detail) => { if (!cancelled) setPortalThread({ detail, loading: false, error: "" }); })
      .catch((error) => { if (!cancelled) setPortalThread({ detail: null, loading: false, error: errorMessage(error) }); });
    return () => { cancelled = true; };
  }, [threadLocation?.commentId, threadsNonce]);
  // The Activity tab's feed page carries the seen watermark from the same
  // read, so the divider is frozen as of the load; it moves only when Mark
  // all seen advances the watermark below. The page loads while the portal
  // is open so the tab strip's count stays live; request-driven refreshes
  // re-run the fetch the same way the Reviews and Threads tabs do; failures
  // keep the last page.
  useEffect(() => {
    if (!portalLocation) return;
    let cancelled = false;
    invoke<{ events: PortalActivityPayload["events"]; seen_id: number }>("list_portal_activity", { repoPath: null, limit: null })
      .then((page) => { if (!cancelled) setPortalActivity({ events: page.events, seen_id: page.seen_id, loading: false, error: "" }); })
      .catch((error) => { if (!cancelled) setPortalActivity((current) => ({ events: current?.events ?? [], seen_id: current?.seen_id ?? 0, loading: false, error: errorMessage(error) })); });
    return () => { cancelled = true; };
  }, [portalLocation?.tab, attention]);
  // The palette's cross-store search rides a short debounce; an empty
  // needle drops the extra sections entirely.
  useEffect(() => {
    const needle = query.trim();
    if (!needle) {
      setPortalSearch(null);
      return;
    }
    let cancelled = false;
    const timer = setTimeout(() => {
      invoke<PortalSearchMatches>("search_portal", { needle })
        .then((matches) => { if (!cancelled) setPortalSearch(matches); })
        .catch(() => { if (!cancelled) setPortalSearch({ comments: [], requests: [], commits: [] }); });
    }, 250);
    return () => { cancelled = true; clearTimeout(timer); };
  }, [query]);
  useEffect(() => { commentsRef.current = comments; activeRepoPathRef.current = activeRepoPath; reposRef.current = repos; settingsRef.current = settings; paneToggleRef.current = () => {
    // Ctrl+B means the surface's pane: the files list in a review, the
    // project navigator on the overview; the settings overlay has none.
    const kind = nav.current().kind;
    if (kind === "review" || kind === "commit-history") void updateSettings({ ...settings, files_pane_visible: !settings.files_pane_visible });
    else if (kind === "inbox") setCollapsed((value) => !value);
  }; });
  // Agent comment changes arrive through the same endpoint; when one names
  // the loaded review, the comment layer refetches so the stream and inline
  // threads update without a reload. Changes to other reviews are ignored:
  // a review's data is fetched fresh when it opens.
  useEffect(() => {
    let disposed = false;
    const subscription = listen<CommentChange>("comment-changed", (event) => {
      if (disposed) return;
      const key = commentsRef.current.key;
      const change = event.payload;
      if (!key || key.repoPath !== change.repo_path || key.baseSha !== change.base_sha || key.targetKey !== change.target_key || key.targetKind !== change.target_kind) return;
      void commentsRef.current.refresh();
    });
    return () => { disposed = true; void subscription.then((unsubscribe) => unsubscribe()); };
  }, []);
  // A live arrival for the open review re-reads its comment stream through
  // the comment layer's refresh; the submissions strip follows the same
  // tick passed down to the review surface.
  useEffect(() => {
    if (reviewRefreshTick === 0) return;
    void commentsRef.current.refresh();
  }, [reviewRefreshTick]);
  // A completed refresh ping re-lists the open repository's surfaces, so an
  // agent's push-then-ping becomes visible without a manual refresh. The
  // ping already ran the fetch, so this is only the local re-reads; the
  // user is never navigated anywhere.
  useEffect(() => {
    let disposed = false;
    const subscription = listen<ProjectRefresh>("project-refreshed", (event) => {
      if (disposed) return;
      if (event.payload.repo_path === activeRepoPathRef.current) {
        void relistProject(event.payload.repo_path);
        return;
      }
      // An agent's add_repo announces through the same event: pull the new
      // repository into the sidebar without disturbing the current view.
      if (reposRef.current.some((repo) => repo.path === event.payload.repo_path)) return;
      void (async () => {
        try {
          const repo = await invoke<Repo>("open_repo", { path: event.payload.repo_path });
          setRepos((current) => current.some((item) => item.path === repo.path) ? current : [repo, ...current]);
          const worktrees = await invoke<Worktree[]>("list_worktrees", { path: repo.path });
          setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, worktrees } : item));
        } catch { /* best-effort follow: the store row exists, so the repo
        // appears on the next app start even if this re-read fails */ }
      })();
    });
    return () => { disposed = true; void subscription.then((unsubscribe) => unsubscribe()); };
  }, []);
  useEffect(() => {
    if (reviewTarget || !historyLocation) return;
    const selected = historyLocation.selectedCommit;
    if (!selected) return;
    const quickBase = selected.parents[0] ?? "empty-tree";
    const identity: ReviewIdentity = { repoPath: historyLocation.repoPath, base: quickBase, target: commitTargetOf(selected), scope: "committed", reversed: false, originRef: historyLocation.startRef ?? undefined };
    if (sameReview(reviewIdentityRef.current, identity) && (reviewIndex !== null || reviewLoading)) return;
    void fetchIndex(identity.target, quickBase, "committed", false, historyLocation.repoPath, identity.originRef);
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
    const identity: ReviewIdentity = { repoPath: historyLocation.repoPath, base: selected.parents[0] ?? "empty-tree", target: commitTargetOf(selected), scope: "committed", reversed: false, originRef: historyLocation.startRef ?? undefined };
    if (patchIdentityRef.current === patchIdentityOf(identity, file)) return;
    void selectFile(file, identity);
  }, [historyLocation, reviewLoading, reviewIndex]);
  useEffect(() => {
    if (!reviewLocation || !reviewLocation.identity.base) return;
    const identity = reviewLocation.identity;
    if (sameReview(reviewIdentityRef.current, identity) && (reviewIndex !== null || reviewLoading)) return;
    void fetchIndex(identity.target, identity.base, identity.scope, identity.reversed, identity.repoPath, identity.originRef);
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

  async function openReview(target: ReviewTarget, repoPath = activeRepoPath, preset?: { base?: string; scope?: ReviewScope; reversed?: boolean; recordedKey?: RecordedKey }, originRef?: string, focusedCommentId: number | null = null) {
    const nextScope = preset?.scope ?? (target.kind === "worktree" ? scope : "committed");
    const nextReversed = preset?.reversed ?? reversed;
    const nextOriginRef = originRef ?? (target.kind === "ref" && target.name.startsWith("refs/remotes/") ? target.name : undefined);
    // A preset base rides the pushed location immediately: when list_refs
    // cannot resolve (moved project, gone surface), the review still
    // carries the recorded base its stored conversation keys by.
    const identity: ReviewIdentity = { repoPath, base: preset?.base ?? "", target, scope: nextScope, reversed: nextReversed, originRef: nextOriginRef, recordedKey: preset?.recordedKey };
    const generation = ++indexGenerationRef.current;
    ++patchGenerationRef.current;
    reviewIdentityRef.current = identity;
    patchIdentityRef.current = "";
    patchCacheRef.current.clear();
    contentIdentityRef.current = "";
    contentCacheRef.current.clear();
    const entry: AppLocation = { kind: "review", identity, selectedFile: null, focusedCommentId };
    const current = nav.current();
    if (current.kind === "review" && current.identity.repoPath === repoPath && sameReviewTarget(current.identity.target, target)) nav.replace(entry); else nav.push(entry);
    setRefs({ heads: [], remotes: [], tags: [], default_base: null }); setReviewIndex(null); setPatch(null); setPatchError(""); setOperationError(""); setReviewLoading(true); setPatchLoading(false); setFileContent(null); setFileContentError(""); setFileContentLoading(false); setFileImage(null); setFileImageError(""); setFileImageLoading(false);
    try {
      const inventory = await invoke<RefInventory>("list_refs", { path: repoPath, worktreeBranch: target.kind === "worktree" ? target.worktree.branch : null, targetRef: target.kind === "ref" ? target.name : null });
      if (generation !== indexGenerationRef.current || !reviewLoadsMatch(identity)) return;
      setRefs(inventory);
      refsIdentityRef.current = identity;
      const nextBase = preset?.base ?? autoReviewBase(target, inventory);
      if (!nextBase) return;
      nav.replace({ kind: "review", identity: { ...identity, base: nextBase }, selectedFile: null, focusedCommentId });
      await fetchIndex(target, nextBase, nextScope, nextReversed, repoPath, nextOriginRef);
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
  async function fetchIndex(target: ReviewTarget, nextBase: string, nextScope: ReviewScope, nextReversed: boolean, nextRepoPath: string, nextOriginRef?: string) {
    const identity: ReviewIdentity = { repoPath: nextRepoPath, base: nextBase, target, scope: nextScope, reversed: nextReversed, originRef: nextOriginRef };
    const generation = ++indexGenerationRef.current;
    ++patchGenerationRef.current;
    reviewIdentityRef.current = identity;
    patchIdentityRef.current = "";
    patchCacheRef.current.clear();
    contentIdentityRef.current = "";
    contentCacheRef.current.clear();
    setReviewLoading(true); setReviewIndex(null); setPatch(null); setPatchError(""); setPatchLoading(false); setFileContent(null); setFileContentError(""); setFileContentLoading(false); setFileImage(null); setFileImageError(""); setFileImageLoading(false);
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
  // The partial-clone recovery: one branch's objects are fetched with the
  // partial-clone filter suspended, then the review reloads from the state
  // the fetch left behind. The fetch is user-initiated and never runs as
  // part of review computation.
  async function fetchBranchObjects(branchAction: { ref: string; identity: ReviewIdentity }) {
    setFetchingBranchObjects(true);
    try {
      await invoke("fetch_review_objects", { path: branchAction.identity.repoPath, targetRef: branchAction.ref });
      const identity = branchAction.identity;
      await fetchIndex(identity.target, identity.base, identity.scope, identity.reversed, identity.repoPath, identity.originRef);
    } catch (error) {
      setOperationError(errorMessage(error));
    } finally {
      setFetchingBranchObjects(false);
    }
  }
  function changeReviewSetting(nextBase: string, nextScope = scope, nextReversed = reversed) {
    const current = nav.current();
    if (current.kind !== "review" || !nextBase) return;
    const identity: ReviewIdentity = { ...current.identity, base: nextBase, scope: nextScope, reversed: nextReversed };
    const entry: AppLocation = { kind: "review", identity, selectedFile: null, focusedCommentId: current.focusedCommentId };
    if (current.identity.base !== nextBase) nav.push(entry); else nav.replace(entry);
    void fetchIndex(identity.target, nextBase, nextScope, nextReversed, identity.repoPath, identity.originRef);
  }
  async function selectFile(file: ChangedFile, identity: ReviewIdentity) {
    if (!identity.base) return;
    const patchIdentity = patchIdentityOf(identity, file);
    const generation = ++patchGenerationRef.current;
    patchIdentityRef.current = patchIdentity;
    // File content is keyed to the selected file: switching files drops the
    // pane's loaded content and any in-flight read so gap expansion and the
    // file view never splice another file's lines. The per-file cache keeps
    // serving on the way back.
    ++contentGenerationRef.current;
    contentIdentityRef.current = "";
    contentInFlightRef.current = false;
    setFileContent(null); setFileContentError(""); setFileContentLoading(false); setFileImage(null); setFileImageError(""); setFileImageLoading(false);
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
      setPatchError(code === "git_output_too_large" ? "This file's patch exceeds the 16 MiB output bound and was not rendered." : errorMessage(error));
    } finally {
      if (generation === patchGenerationRef.current && patchIdentityRef.current === patchIdentity && sameReview(reviewIdentityRef.current, identity)) setPatchLoading(false);
    }
  }

  // Fetches the selected file's new-side content for context expansion and
  // the full-file view. Cached like patches, fetched only on first use, and
  // retried after a failure on the next request.
  async function ensureFileContent(file: ChangedFile, identity: ReviewIdentity) {
    if (!identity.base) return;
    if (isImageFile(file)) { void ensureFileImage(file, identity); return; }
    const contentIdentity = patchIdentityOf(identity, file);
    const cached = contentCacheRef.current.get(contentIdentity);
    if (cached) {
      contentCacheRef.current.delete(contentIdentity);
      contentCacheRef.current.set(contentIdentity, cached);
      setFileContent(cached); setFileContentError(""); setFileContentLoading(false);
      return;
    }
    if (contentIdentityRef.current === contentIdentity && contentInFlightRef.current) return;
    const generation = ++contentGenerationRef.current;
    contentIdentityRef.current = contentIdentity;
    contentInFlightRef.current = true;
    setFileContentLoading(true); setFileContentError("");
    try {
      const content = await invoke<FileContent>("read_review_file", { path: identity.target.kind === "worktree" ? identity.target.worktree.path : identity.repoPath, base: identity.base, headRef: identity.target.kind === "ref" ? identity.target.name : identity.target.kind === "commit" ? identity.target.sha : null, committedOnly: identity.scope === "committed", reversed: identity.reversed, file: file.path, untracked: file.untracked });
      if (generation === contentGenerationRef.current && contentIdentityRef.current === contentIdentity && sameReview(reviewIdentityRef.current, identity)) {
        contentCacheRef.current.set(contentIdentity, content);
        if (contentCacheRef.current.size > FILE_CONTENT_CACHE_LIMIT) {
          const oldest = contentCacheRef.current.keys().next().value;
          if (oldest !== undefined) contentCacheRef.current.delete(oldest);
        }
        setFileContent(content);
      }
    } catch (error) {
      if (generation !== contentGenerationRef.current || contentIdentityRef.current !== contentIdentity || !sameReview(reviewIdentityRef.current, identity)) return;
      setFileContentError(errorMessage(error));
    } finally {
      contentInFlightRef.current = false;
      if (generation === contentGenerationRef.current && contentIdentityRef.current === contentIdentity && sameReview(reviewIdentityRef.current, identity)) setFileContentLoading(false);
    }
  }

  async function ensureFileImage(file: ChangedFile, identity: ReviewIdentity) {
    const imageIdentity = patchIdentityOf(identity, file);
    if (imageIdentityRef.current === imageIdentity && imageInFlightRef.current) return;
    const generation = ++imageGenerationRef.current;
    imageIdentityRef.current = imageIdentity;
    imageInFlightRef.current = true;
    setFileImageLoading(true); setFileImageError("");
    try {
      const bytes = await invoke<ArrayBuffer>("read_review_file_bytes", { path: identity.target.kind === "worktree" ? identity.target.worktree.path : identity.repoPath, base: identity.base, headRef: identity.target.kind === "ref" ? identity.target.name : identity.target.kind === "commit" ? identity.target.sha : null, committedOnly: identity.scope === "committed", reversed: identity.reversed, file: file.path, untracked: file.untracked });
      if (generation !== imageGenerationRef.current || imageIdentityRef.current !== imageIdentity || !sameReview(reviewIdentityRef.current, identity)) return;
      // Empty bytes mean the file does not exist on this side (deleted
      // images), not a renderable asset.
      if (bytes.byteLength === 0) { setFileImage(null); return; }
      const mime = imageMimeForPath(file.path) ?? "application/octet-stream";
      setFileImage(URL.createObjectURL(new Blob([bytes], { type: mime })));
    } catch (error) {
      if (generation !== imageGenerationRef.current || imageIdentityRef.current !== imageIdentity || !sameReview(reviewIdentityRef.current, identity)) return;
      setFileImageError(errorMessage(error));
    } finally {
      imageInFlightRef.current = false;
      if (generation === imageGenerationRef.current && imageIdentityRef.current === imageIdentity && sameReview(reviewIdentityRef.current, identity)) setFileImageLoading(false);
    }
  }

  // A review degrades to "content no longer available" only when its own
  // surface is recorded gone, the index failure means the objects behind
  // it are gone, and nothing is stored on the identity: a stored
  // conversation keeps the review page up in its degraded form.
  function goneSurfaceMatching(repoPath: string, target: ReviewTarget): GoneSurface | null {
    const gone = surfaces[repoPath]?.gone ?? [];
    if (target.kind === "commit") return gone.find((surface) => surface.head_sha === target.sha) ?? null;
    if (target.kind === "ref") return gone.find((surface) => surface.kind === "branch" && surface.identity_key === target.name) ?? null;
    return null;
  }

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
  // The local half of a refresh: re-read the worktree list, surfaces, and
  // status counts. The refresh action runs it after its fetch; the
  // project-refreshed listener runs it alone because the agent's ping
  // already performed the fetch.
  async function relistProject(repoPath: string) {
    try {
      const worktrees = await invoke<Worktree[]>("list_worktrees", { path: repoPath });
      setRepos((current) => current.map((item) => item.path === repoPath ? { ...item, worktrees } : item));
      // Success clears a prior failure: the unattended reloads must self-heal.
      setRepoErrors((current) => { if (!(repoPath in current)) return current; const next = { ...current }; delete next[repoPath]; return next; });
    } catch (error) { setRepoErrors((current) => ({ ...current, [repoPath]: errorMessage(error) })); }
    void refreshSurfaces(repoPath);
    setStatusNonce((nonce) => nonce + 1);
  }
  async function refreshProject() {
    const repo = activeRepo;
    if (!repo) return;
    await fetchProjectRemotes(repo.path);
    await relistProject(repo.path);
  }
  async function refreshCommits(entry: HistoryEntry) {
    await fetchProjectRemotes(entry.repoPath);
    await loadHistorySurface(entry);
  }
  function openSettings() { nav.push({ kind: "settings" }); }
  // The attention queue reads one store-backed payload per refresh: no Git
  // runs on this path, and failures leave the previous payload in place.
  // The payload also returns to callers that need the fresh rows (the
  // announce arrival reads the actor from them).
  async function refreshAttention(): Promise<AttentionQueue | null> {
    try {
      const queue = await invoke<AttentionQueue>("list_attention");
      setAttention(queue);
      return queue;
    } catch {
      // keep the last payload
      return null;
    }
  }
  // Mark all seen advances the stored watermark to the current event
  // cursor: the Activity divider clears and the Recent comments category
  // drains on the queue refresh that follows.
  async function markActivitySeen() {
    setMarkingSeen(true);
    try {
      const seenId = await invoke<number>("mark_activity_seen");
      setPortalActivity((current) => current ? { ...current, seen_id: seenId } : current);
      await refreshAttention();
    } catch (error) {
      setOperationError(errorMessage(error));
    } finally {
      setMarkingSeen(false);
    }
  }
  function goInbox() { nav.push({ kind: "inbox" }); }
  // Opening a queue or portal row lands on the review it points at: the
  // live worktree review when the surface still exists, otherwise the
  // recorded head as a commit review against the request's base. The
  // project does not have to be registered: the row carries the stored
  // refs, so a moved or removed project still opens and degrades to its
  // stored conversation where Git cannot resolve. Shared by the inbox and
  // the Reviews and Threads tab rows.
  function openReviewIdentity(row: ReviewIdentityRef, focusedCommentId: number | null = null) {
    const repo = repos.find((item) => item.path === row.repo_path);
    if (repo) setActiveRepoPath(repo.path);
    if (row.target_kind === "worktree") {
      const worktree = repo?.worktrees.find((item) => worktreeKey(item.path) === worktreeKey(row.target_key));
      if (worktree) {
        setSelectedWorktreePath(worktree.path);
        // The stored base pins the session the row's requests and
        // comments live on; the auto base would open a different,
        // conversation-less review.
        void openReview({ kind: "worktree", worktree }, row.repo_path, { base: row.base_sha }, undefined, focusedCommentId);
        return;
      }
    }
    // A worktree row without a live worktree falls back to its recorded
    // head; with none recorded there is nothing reviewable to open. The
    // recordedKey keeps the comment session on the row's stored identity,
    // so its conversation follows the stand-in target.
    const head = row.head_sha ?? (row.target_kind === "head" ? row.target_key : null);
    if (head) void openReview({ kind: "commit", sha: head, parents: [], defaultBaseAncestor: false }, row.repo_path, { base: row.base_sha, recordedKey: { targetKey: row.target_key, targetKind: row.target_kind } }, undefined, focusedCommentId);
  }
  // A thread row's open: the review location carries the focused comment
  // id so the comments pane scrolls to the thread.
  function openThreadInReview(row: ReviewIdentityRef, focusedCommentId: number) {
    openReviewIdentity(row, focusedCommentId);
  }
  function openPortalThread(commentId: number) {
    nav.push({ kind: "thread", commentId });
  }

  function openAttentionRow(row: AttentionRow) {
    openReviewIdentity(row);
  }
  // One landing for every notification-worthy moment (deliveries and
  // announces), and the single place the focus rule and the master toggle
  // are evaluated: the cue always records the entry so it also records
  // what happened while the window was covered, and an unfocused window
  // additionally gets an OS toast. A denied permission or failed send
  // never loses the arrival; the cue already holds it.
  async function arrive(entry: ArrivalEntry) {
    if (!settingsRef.current.notifications_enabled) return;
    setArrivals((current) => [...current, entry]);
    if (document.hasFocus()) return;
    try {
      let granted = await isPermissionGranted();
      if (!granted) granted = (await requestPermission()) === "granted";
      if (!granted) return;
      const project = arrivalProjectLabel(reposRef.current, entry.repo_path);
      const change = arrivalChangeLabel(entry.target_kind, entry.target_key);
      sendNotification({ title: entry.actor, body: `${entry.actor} ${arrivalSentenceBody(entry.moment, change, project)}` });
    } catch {
      // The cue holds the arrival; a failed toast send is non-fatal.
    }
  }
  // Activating an arrival opens the review the submission targeted: the
  // worktree row when it is loaded, otherwise the commit review re-derived
  // from the recorded identity (the base override pins the recorded base).
  function openArrival(entry: ArrivalEntry) {
    const repo = repos.find((item) => item.path === entry.repo_path);
    if (!repo) return;
    setActiveRepoPath(repo.path);
    if (entry.target_kind === "head") {
      void openReview({ kind: "commit", sha: entry.target_key, parents: [], defaultBaseAncestor: false }, entry.repo_path, { base: entry.base_sha });
      return;
    }
    const worktree = repo.worktrees.find((item) => item.path === entry.target_key);
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
  function changeFileView(changed_files_view: ChangedFilesView) { void updateSettings({ ...settings, changed_files_view }); }
  function changePaneVisibility(next: { files: boolean; comments: boolean }) { void updateSettings({ ...settings, files_pane_visible: next.files, comments_pane_visible: next.comments }); }
  function changeCommentsWide(comments_pane_wide: boolean) { void updateSettings({ ...settings, comments_pane_wide }); }
  function navigate(delta: number) { if (delta < 0) nav.back(); else nav.forward(); }
  function handleReviewBack() { const previous = nav.peekBack(); if (previous?.kind === "commit-history" || previous?.kind === "portal" || previous?.kind === "thread") nav.back(); else goInbox(); }
  function selectHistoryCommit(commit: CommitInfo) { const current = nav.current(); if (current.kind !== "commit-history") return; nav.replace({ ...current, selectedCommit: commit, selectedFile: null }); }
  // A history row's corner button pins the review base to that commit: the
  // open review re-bases in place, and a history quick-look escalates to the
  // full review of its selected commit against the picked fork point.
  function pickCommitBase(commit: CommitInfo) {
    const current = nav.current();
    if (current.kind === "review") changeReviewSetting(commit.sha);
    else if (current.kind === "commit-history" && current.selectedCommit) void openReview(commitTargetOf(current.selectedCommit), current.repoPath, { base: commit.sha }, current.startRef ?? undefined);
  }
  function openHistoryEntry(entry: HistoryEntry) { nav.push({ kind: "commit-history", repoPath: entry.repoPath, startPointLabel: entry.startPointLabel, startRef: entry.startRef ?? null, worktreePath: entry.worktreePath ?? null, selectedCommit: null, selectedFile: null }); }
  async function loadHistorySurface(entry: HistoryEntry) {
    const generation = ++historyGenerationRef.current;
    setHistory({ ...entry, commits: [], hasMore: false, loading: true, error: "" });
    setHistoryRefs({ heads: [], remotes: [], tags: [], default_base: null });
    let against: string | null = null;
    try {
      const inventory = await invoke<RefInventory>("list_refs", { path: entry.repoPath, worktreeBranch: null, targetRef: entry.startRef ?? null });
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

  const diffPrefs: DiffPreferences = { layout: settings.diff_layout, whitespaceVisible: settings.whitespace_visible, lineWrap: settings.line_wrap, syntaxVisible: settings.syntax_visible, inlineCommentsVisible: settings.inline_comments_visible };
  const diffToggles = <DiffToggles settings={settings} onChange={(next) => { void updateSettings(next); }} />;
  const goneReview = reviewLocation && reviewIndex !== null && reviewIndex.error && (reviewIndex.error_code === "unresolvable_ref" || reviewIndex.error_code === "git_execution") ? goneSurfaceMatching(reviewLocation.identity.repoPath, reviewLocation.identity.target) : null;
  // The partial-clone recovery offer: only when the live review failed for
  // missing promisor content and the review knows which remote branch it
  // came from.
  const activeReviewIdentity: ReviewIdentity | null = reviewLocation?.identity.base ? reviewLocation.identity : historyLocation?.selectedCommit ? { repoPath: historyLocation.repoPath, base: historyLocation.selectedCommit.parents[0] ?? "empty-tree", target: commitTargetOf(historyLocation.selectedCommit), scope: "committed", reversed: false, originRef: historyLocation.startRef ?? undefined } : null;
  let branchFetchAction: { ref: string; busy: boolean; run: () => void } | null = null;
  if (activeReviewIdentity?.originRef?.startsWith("refs/remotes/") && reviewIndex?.error && reviewIndex.error_code === "partial_clone_content") {
    const identity: ReviewIdentity = activeReviewIdentity;
    const ref: string = activeReviewIdentity.originRef;
    branchFetchAction = { ref, busy: fetchingBranchObjects, run: () => { void fetchBranchObjects({ ref, identity }); } };
  }
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
  const pinnedWorktrees = new Set(listing.pinned.filter((pin) => pin.kind === "worktree").map((pin) => worktreeKey(pin.identity_key)));
  // Children stay quiet: pinned surfaces plus the repository's current
  // checkout; every other branch and worktree lives on the project overview.
  const currentWorktree = repo.path === activeRepoPath
    ? repo.worktrees.find((worktree) => worktree.path === selectedWorktreePath)
    : undefined;
  return <>{pinned.map(surfaceRow)}{currentWorktree && !pinnedWorktrees.has(worktreeKey(currentWorktree.path)) && surfaceRow({ kind: "worktree", identityKey: currentWorktree.path, label: shortToken(currentWorktree.branch), startRef: null, worktreePath: currentWorktree.path, pinnedAt: null, gone: false })}</>;
};
  const statusMessage = loadError || (loading ? "Loading repositories..." : hydrating ? "Loading worktrees..." : ""); const paletteRows: PaletteRow[] = [...matchingResults.map((result): PaletteRow => ({ section: "repos", result })), ...(portalSearch?.comments.map((match): PaletteRow => ({ section: "comments", match })) ?? []), ...(portalSearch?.requests.map((match): PaletteRow => ({ section: "requests", match })) ?? []), ...(portalSearch?.commits.map((match): PaletteRow => ({ section: "commits", match })) ?? [])]; const palettePageCount = Math.max(1, Math.ceil(paletteRows.length / SEARCH_PAGE_SIZE)); const visiblePalettePage = Math.min(searchPage, palettePageCount - 1); const visiblePaletteRows = paletteRows.slice(visiblePalettePage * SEARCH_PAGE_SIZE, (visiblePalettePage + 1) * SEARCH_PAGE_SIZE);
  // The cue's sentence names the newest entry's moment; older entries only
  // count into the stacking suffix.
  const latestArrival = arrivals[arrivals.length - 1];
  const statusByPath: Record<string, number | null> = {};
  for (const status of worktreeStatuses[activeRepoPath] ?? []) statusByPath[status.path] = status.changes;
  const activeInventory = inventories[activeRepoPath] ?? null;
  const branchByRef = new Map<string, BranchSummary>((activeInventory?.branches ?? []).map((branch) => [branch.ref_name, branch]));
  // The overview's filter row scopes one tab at a time: worktrees, local
  // branches, remote branches, or archived surfaces, all client-side over
  // the already-bounded inventory passes. Tab counts carry the inventory
  // sizes; per-row chips carry each worktree's status, so no summary cards.
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
  // The default branch contains itself, and its remote-tracking twin reads
  // as merged whenever local main is current; both chips would be pure
  // noise, and the main badge already marks the default row.
  const mergedChip = (merged: boolean, refName: string | undefined) => {
    if (!merged || refName === activeInventory?.default_branch) return null;
    const defaultName = shortToken(activeInventory?.default_branch ?? "");
    const remoteName = refName?.startsWith("refs/remotes/") ? refName.slice("refs/remotes/".length).split("/").slice(1).join("/") : null;
    if (remoteName && remoteName === defaultName) return null;
    return <span className="status-chip merged" title={`Every commit is already in ${defaultName}`}>Merged</span>;
  };
  const inventoryBranchRow = (branch: BranchSummary) => {
    const pinned = branchPinIndex.get(`branch:${branch.ref_name}`) !== undefined;
    const checkedOut = worktreeBranchRefs.has(branch.ref_name);
    const syncBase = branch.upstream ?? activeInventory?.default_branch ?? null;
    const ahead = branch.ahead ?? 0;
    const behind = branch.behind ?? 0;
    const openBranchReview = () => void openReview({ kind: "ref", name: branch.ref_name }, activeRepoPath);
    return <div className="project-row-wrap" key={branch.ref_name}><div className="worktree-row" role="button" tabIndex={0} title={branch.ref_name} onClick={openBranchReview} onKeyDown={(event) => { if (event.target !== event.currentTarget) return; if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openBranchReview(); } }}><div className="worktree-cell"><div className="branch-title"><GitBranch size={14} /><strong>{shortToken(branch.ref_name)}</strong>{branch.ref_name === activeInventory?.default_branch && <span className="main-badge">main</span>}{checkedOut && <span className="in-worktree-badge">worktree</span>}</div><div className="worktree-path"><span className="path-text">{branch.ref_name}</span></div></div><div className="status-cell">{mergedChip(branch.merged, branch.ref_name)}{syncChip(syncBase, ahead, behind, (base) => openReview({ kind: "ref", name: branch.ref_name }, activeRepoPath, { base, scope: "committed", reversed: false }))}</div><div className="last-commit-cell"><span className="row-commit-subject">{branch.subject}</span><span className="row-commit-age">{branch.author} · {relativeTime(branch.commit_date)}</span></div></div><button className="pin-button surface-pin" type="button" aria-label={`${pinned ? "Unpin" : "Pin"} branch ${shortToken(branch.ref_name)}`} title={`${pinned ? "Unpin" : "Pin"} branch`} onClick={(event) => { event.stopPropagation(); void toggleSurfacePin(activeRepoPath, "branch", branch.ref_name, !pinned); }}>{pinned ? <PinOff size={12} /> : <Pin size={12} />}</button></div>;
  };
  const archivedRow = (surface: GoneSurface) => {
    const label = goneSurfaceLabel(surface);
    const openArchivedHistory = () => void openHistoryEntry({ repoPath: activeRepoPath, startPointLabel: label, startRef: surface.head_sha });
    return <div className="project-row-wrap" key={`gone:${surface.kind}:${surface.identity_key}`}><div className="worktree-row" role="button" tabIndex={0} title={surface.identity_key} onClick={openArchivedHistory} onKeyDown={(event) => { if (event.target !== event.currentTarget) return; if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openArchivedHistory(); } }}><div className="worktree-cell"><div className="branch-title"><GitBranch size={14} /><strong>{label}</strong>{surface.kind === "worktree" && <span className="in-worktree-badge">worktree</span>}<span className="gone-badge">gone</span></div><div className="worktree-path"><span className="path-text">{surface.detail}</span></div></div><div className="status-cell" /><div className="last-commit-cell"><span className="row-commit-age">{surface.head_sha && <span className="commit-hash"><code title={surface.head_sha}>{shortToken(surface.head_sha)}</code><CopyButton ghost value={surface.head_sha} label={`Copy commit hash ${shortToken(surface.head_sha)}`} /></span>}{surface.last_seen_at ? ` · last seen ${relativeTime(surface.last_seen_at)}` : ""}</span></div></div><button className="pin-button surface-pin" type="button" aria-label={`Pin ${surface.kind} ${label}`} title={`Pin ${surface.kind}`} onClick={(event) => { event.stopPropagation(); void toggleSurfacePin(activeRepoPath, surface.kind, surface.identity_key, true); }}><Pin size={12} /></button></div>;
  };
  const activeCommitSha = historyLocation?.selectedCommit?.sha ?? (reviewTarget !== null && reviewTarget.kind === "commit" ? reviewTarget.sha : null);
  // The row-level "use as base" button only means something when a review
  // can receive the base: an open review, or a history quick-look with a
  // selected commit to escalate.
  const canPickCommitBase = reviewLocation !== null || (historyLocation?.selectedCommit ?? null) !== null;
  // The bar is a permanent sidebar fixture: visible whenever a loaded
  // history matches the active ref, held across commit reviews so picking
  // commits keeps the ref's list anchored.
  const showCommitsBar = history !== null && (historyAnchor !== null ? historyAnchorKey === historyKeyOf(history) : activeCommitSha !== null && reviewLocation !== null && history.repoPath === reviewLocation.identity.repoPath);
  // The queue's tab badge counts every queued row across categories.
  const attentionCount = attentionRows(attention).length;
  // The portal tab strip's counts: each tab's payload total, null until
  // that payload's first load lands.
  const portalThreadsCount = portalThreads ? portalThreads.groups.reduce((total, group) => total + group.threads.length, 0) : null;
  const portalReviewsCount = portalReviews?.rows.length ?? null;
  const portalActivityCount = portalActivity?.events.length ?? null;
  // The top bar addresses non-review surfaces; review and history keep
  // their own headers with the history controls.
  const topBarLabel = portalLocation ? `pulse/${portalLocation.tab}` : threadLocation ? "pulse/thread" : location.kind === "inbox" ? activeRepo ? `projects/${activeRepo.name}` : "projects" : settingsLocation ? "settings" : null;

  return <div className={`app-shell ${collapsed ? "nav-collapsed" : ""}`} aria-busy={loading || opening || hydrating}>
<aside className="sidebar">
<div className="brand-row">
<BrandMark />
<span className="brand-name">WorktreeView</span>
</div>
<button className="open-repository-button" type="button" aria-label="Open repository" title={collapsed ? undefined : "Open repository"} data-tip="Open repository..." onClick={() => void openRepository()} disabled={opening}>
<FolderGit2 size={14} />
<span>Open repository...</span>
</button>
<div className="nav-tabs" aria-label="Workspace views">
<button className={`nav-tab ${portalLocation ? "" : "active"}`} type="button" aria-label="Projects" aria-current={portalLocation ? undefined : "page"} data-tip="Projects" onClick={goInbox}>
<FolderGit2 size={15} />
<span>Projects</span>
</button>
<button className={`nav-tab ${portalLocation ? "active" : ""}`} type="button" aria-label={`Pulse, ${attentionCount} ${attentionCount === 1 ? "item" : "items"}`} aria-current={portalLocation ? "page" : undefined} onClick={() => nav.push({ kind: "portal", tab: "inbox", filters: DEFAULT_PORTAL_FILTERS })}>
<Inbox size={15} />
<span>Pulse</span>{attentionCount > 0 && <span className="nav-tab-count" aria-hidden="true">{attentionCount > 99 ? "99+" : attentionCount}</span>}</button>
</div>
<button className="search-trigger" type="button" aria-label="Find repositories and worktrees" data-tip="Find repositories and worktrees (Ctrl K)" onClick={(event) => openPalette(event.currentTarget)}>
<Search size={14} />
<span>Find repositories and worktrees</span>
<kbd>Ctrl K</kbd>
</button>
<nav className="project-list" aria-label="Repositories">
<div className="nav-section-label">Pinned</div>{pinnedRepos.length === 0 && <div className="sidebar-empty">No pinned repositories</div>}{pinnedRepos.map((repo) => <RepoNavGroup key={repo.path} repo={repo} active={repo.path === activeRepoPath} expanded={Boolean(expandedRepos[repo.path])} collapsed={collapsed} onToggle={() => { activateRepo(repo); void toggleRepo(repo); }} onPin={() => void togglePin(repo)} rows={sidebarRows(repo)} />)}<div className="nav-section-label">Recent</div>{recentRepos.length === 0 && <div className="sidebar-empty">No recent repositories</div>}{recentRepos.map((repo) => <RepoNavGroup key={repo.path} repo={repo} active={repo.path === activeRepoPath} expanded={Boolean(expandedRepos[repo.path])} collapsed={collapsed} onToggle={() => { activateRepo(repo); void toggleRepo(repo); }} onPin={() => void togglePin(repo)} rows={sidebarRows(repo)} />)}</nav>{showCommitsBar && history && <section className="commits-bar" aria-label="Commit history">
<div className="commits-bar-heading">
<strong>{history.startPointLabel}</strong>
<span className="commits-bar-actions">
<button className={`icon-button ${fetchingRepos[history.repoPath] ? "spinning" : ""}`} type="button" aria-label="Fetch remote updates" title="Fetch from remotes, then reload history" disabled={Boolean(fetchingRepos[history.repoPath])} onClick={() => { if (history) void refreshCommits({ repoPath: history.repoPath, startPointLabel: history.startPointLabel, worktreePath: history.worktreePath ?? undefined, startRef: history.startRef ?? undefined }); }}>
{fetchingRepos[history.repoPath] ? <LoaderCircle size={12} /> : <Download size={12} />}
</button>
</span>
</div>{history.error ? <div className="sidebar-empty" role="status">{history.error}</div> : history.commits.length === 0 ? <div className="sidebar-empty">{history.loading ? "Loading commits..." : "No commits"}</div> : <>
<div className="commit-list">{history.commits.map((commit) => { const openCommit = () => { if (historyLocation) selectHistoryCommit(commit); else void openReview(commitTargetOf(commit), history.repoPath, { base: commit.parents[0] ?? "empty-tree" }, history.startRef); }; return <div key={commit.sha} role="button" tabIndex={0} className={`commit-row ${activeCommitSha === commit.sha ? "selected" : ""}`} onClick={openCommit} onKeyDown={(event) => { if (event.target !== event.currentTarget) return; if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openCommit(); } }}>
<span className="commit-subject" title={commit.subject}>{commit.subject}</span>
<span className="commit-meta">
<code title={commit.sha}>{shortToken(commit.sha)}</code>
<CopyButton ghost value={commit.sha} label={`Copy commit hash ${shortToken(commit.sha)}`} />{canPickCommitBase && <button className="ghost-action" type="button" aria-label={`Use ${shortToken(commit.sha)} as review base`} title="Use as review base: review everything after this commit" onClick={(event) => { event.stopPropagation(); pickCommitBase(commit); }}>
<CornerUpLeft size={12} />
</button>}{commit.refs.length === 0 ? <>
<span title={commit.author}>{authorInitials(commit.author)}</span>
<span>{compactAge(commit.date)}</span>
</> : commit.refs.slice(0, 2).map((ref) => <span key={ref} className="commit-ref" title={ref}>{shortToken(ref)}</span>)}</span>
</div>; })}</div>{history.hasMore && <button type="button" className="commits-load-more" disabled={history.loading} onClick={() => void loadMoreHistory()}>{history.loading ? "Loading..." : "Load more"}</button>}</>}</section>}<div className="sidebar-footer">
<button className="icon-button footer-button" type="button" aria-label="Open settings" title={collapsed ? undefined : "Settings"} data-tip="Settings" onClick={openSettings}>
<SettingsIcon size={15} />
</button>
<button className="icon-button footer-button" type="button" aria-label={collapsed ? "Expand project navigator" : "Collapse project navigator"} title={collapsed ? undefined : "Collapse project navigator"} data-tip={collapsed ? "Expand project navigator (Ctrl B)" : "Collapse project navigator (Ctrl B)"} onClick={() => setCollapsed((value) => !value)}>{collapsed ? <ChevronRight size={15} /> : <ChevronsLeft size={15} />}</button>
<span className="footer-note">
<HardDrive size={13} />read-only / local</span>
</div>
</aside>
<section className="workspace">
<div className="live-error" role="status" aria-live="polite">{statusMessage}</div>{operationError && <div className="operation-error" role="status">{operationError}</div>}{topBarLabel && <TopBar label={topBarLabel} canBack={nav.canBack()} canForward={nav.canForward()} onBack={() => nav.back()} onForward={() => nav.forward()} onSearch={openPalette} />}<main className="content">{portalLocation ?
      <div className="portal-view">
        <header className="portal-head">
          <h1>Pulse</h1>
          <p className="portal-sub">Everything moving across your projects: what needs you, what is being said, what was decided, what happened.</p>
        </header>
        <div className="overview-tabs portal-tabs" role="tablist" aria-label="Pulse tabs">
          <button role="tab" type="button" aria-selected={portalLocation.tab === "inbox"} className={`overview-tab ${portalLocation.tab === "inbox" ? "active" : ""}`} onClick={() => nav.push({ ...portalLocation, tab: "inbox" })}><Inbox size={14} /><span>Inbox</span><span className="tab-count">{attentionCount}</span></button>
          <button role="tab" type="button" aria-selected={portalLocation.tab === "threads"} className={`overview-tab ${portalLocation.tab === "threads" ? "active" : ""}`} onClick={() => nav.push({ ...portalLocation, tab: "threads" })}><MessageSquare size={14} /><span>Threads</span>{portalThreadsCount !== null && <span className="tab-count">{portalThreadsCount}</span>}</button>
          <button role="tab" type="button" aria-selected={portalLocation.tab === "reviews"} className={`overview-tab ${portalLocation.tab === "reviews" ? "active" : ""}`} onClick={() => nav.push({ ...portalLocation, tab: "reviews" })}><FileCheck size={14} /><span>Reviews</span>{portalReviewsCount !== null && <span className="tab-count">{portalReviewsCount}</span>}</button>
          <button role="tab" type="button" aria-selected={portalLocation.tab === "activity"} className={`overview-tab ${portalLocation.tab === "activity" ? "active" : ""}`} onClick={() => nav.push({ ...portalLocation, tab: "activity" })}><Clock size={14} /><span>Activity</span>{portalActivityCount !== null && <span className="tab-count">{portalActivityCount}</span>}</button>
        </div>
        {portalLocation.tab === "reviews" ?
      <PortalReviewsTab payload={portalReviews} repoNames={repoNames} reviewsState={portalLocation.filters.reviewsState} reviewsProject={portalLocation.filters.reviewsProject} reviewsSearch={portalLocation.filters.reviewsSearch} onState={(state: ReviewsStateFilter) => nav.push({ ...portalLocation, filters: { ...portalLocation.filters, reviewsState: state } })} onProject={(project) => nav.push({ ...portalLocation, filters: { ...portalLocation.filters, reviewsProject: project } })} onSearch={(search) => nav.replace({ ...portalLocation, filters: { ...portalLocation.filters, reviewsSearch: search } })} onOpenRow={openReviewIdentity} />
      : portalLocation.tab === "threads" ? <PortalThreadsTab payload={portalThreads} repoNames={repoNames} threadsState={portalLocation.filters.threadsState} threadsVoice={portalLocation.filters.threadsVoice} threadsProject={portalLocation.filters.threadsProject} threadsText={portalLocation.filters.threadsText} onState={(state: ThreadsStateFilter) => nav.push({ ...portalLocation, filters: { ...portalLocation.filters, threadsState: state } })} onVoice={(voice: ThreadsVoiceFilter) => nav.push({ ...portalLocation, filters: { ...portalLocation.filters, threadsVoice: voice } })} onProject={(project) => nav.push({ ...portalLocation, filters: { ...portalLocation.filters, threadsProject: project } })} onText={(text) => nav.replace({ ...portalLocation, filters: { ...portalLocation.filters, threadsText: text } })} onOpenThread={(thread) => openThreadInReview(threadIdentityRef(thread), thread.root_comment_id)} />
      : portalLocation.tab === "activity" ? <PortalActivityTab payload={portalActivity} repoNames={repoNames} activityProject={portalLocation.filters.activityProject} onProject={(project) => nav.push({ ...portalLocation, filters: { ...portalLocation.filters, activityProject: project } })} onMarkSeen={() => void markActivitySeen()} marking={markingSeen} />
      : <AttentionQueueView queue={attention} endpointEnabled={settings.mcp_enabled} tab={portalLocation.filters.category} onTab={(category) => nav.push({ ...portalLocation, filters: { ...portalLocation.filters, category } })} onOpenRow={openAttentionRow} />}
      </div>
      : threadLocation ?
      <PortalThreadDetail payload={portalThread} groups={portalThreads?.groups ?? []} repoNames={repoNames} onOpenThread={openPortalThread} onOpenReview={openThreadInReview} onChanged={() => setThreadsNonce((nonce) => nonce + 1)} />
      : reviewLocation ?
      goneReview && !comments.key ?
      <Empty icon={<CircleDot size={24} />} title="Content no longer available" detail="This surface's content is no longer available in the repository." /> : <ReviewView repoPath={reviewLocation.identity.repoPath} repoName={reviewRepo?.name ?? reviewLocation.identity.repoPath} liveWorktree={reviewRepo?.worktrees.find((worktree) => worktree.path === selectedWorktreePath)} worktrees={reviewRepo?.worktrees} target={reviewLocation.identity.target} refs={refs} base={displayBase} scope={scope} reversed={reversed} index={reviewIndex} loading={reviewLoading} selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} fileView={settings.changed_files_view} diffPrefs={diffPrefs} diffToggles={diffToggles} comments={comments} content={fileContent} contentLoading={fileContentLoading} contentError={fileContentError} imageSrc={fileImageSrc} imageError={fileImageError} imageLoading={fileImageLoading} onEnsureContent={() => { if (selectedFile) void ensureFileContent(selectedFile, reviewLocation.identity); }} canBack={nav.canBack()} canForward={nav.canForward()} onHistoryBack={() => nav.back()} onHistoryForward={() => nav.forward()} onBack={handleReviewBack} onTargetChange={(target) => { void openReview(target, reviewLocation.identity.repoPath); }} onBaseChange={(value) => changeReviewSetting(value)} onPreset={applyReviewPreset} onReverse={() => changeReviewSetting(base, scope, !reversed)} onFileView={changeFileView} panes={{ files: settings.files_pane_visible, comments: settings.comments_pane_visible }} onPaneVisibility={changePaneVisibility}focusedCommentId={reviewLocation.focusedCommentId}reviewRefreshTick={reviewRefreshTick}commentsWide={settings.comments_pane_wide}onCommentsWide={changeCommentsWide}onFile={(file) => { nav.replace({ ...reviewLocation, selectedFile: file }); void selectFile(file, reviewLocation.identity); }} branchAction={branchFetchAction} />
      : historyLocation ?
      <HistoryView history={history ?? { repoPath: historyLocation.repoPath, startPointLabel: historyLocation.startPointLabel, worktreePath: historyLocation.worktreePath ?? undefined, startRef: historyLocation.startRef ?? undefined, commits: [], hasMore: false, loading: true, error: "" }} historyRefs={historyRefs} index={reviewIndex} loading={reviewLoading} selectedCommit={historyLocation.selectedCommit} selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} fileView={settings.changed_files_view} diffPrefs={diffPrefs} diffToggles={diffToggles} comments={comments} content={fileContent} contentLoading={fileContentLoading} contentError={fileContentError} imageSrc={fileImageSrc} imageError={fileImageError} imageLoading={fileImageLoading} onEnsureContent={() => { const selected = historyLocation.selectedCommit; if (selectedFile && selected) void ensureFileContent(selectedFile, { repoPath: historyLocation.repoPath, base: selected.parents[0] ?? "empty-tree", target: commitTargetOf(selected), scope: "committed", reversed: false }); }} onBack={goInbox} onBasePick={(value) => { const selected = historyLocation.selectedCommit; if (selected) void openReview(commitTargetOf(selected), historyLocation.repoPath, { base: value }, historyLocation.startRef ?? undefined); }} onFileView={changeFileView} panes={{ files: settings.files_pane_visible, comments: settings.comments_pane_visible }} onPaneVisibility={changePaneVisibility}commentsWide={settings.comments_pane_wide}onCommentsWide={changeCommentsWide}onFile={(file) => { const selected = historyLocation.selectedCommit; if (selected) { nav.replace({ ...historyLocation, selectedFile: file }); void selectFile(file, { repoPath: historyLocation.repoPath, base: selected.parents[0] ?? "empty-tree", target: commitTargetOf(selected), scope: "committed", reversed: false, originRef: historyLocation.startRef ?? undefined }); } }} branchAction={branchFetchAction} />
      : <section className="inbox-pane" aria-labelledby="inbox-heading" aria-busy={loading || activeRepoHydrating}>
<div className="section-heading">
<div className="project-heading">
<h1 id="inbox-heading">{activeRepo?.name ?? "Worktrees"}</h1>{activeRepo && !activeRepoHydrating && <div className="project-meta">
<span className="meta-chip" title={activeRepo.path}>
<span className="meta-chip-label">{activeRepo.path}</span>
<CopyButton value={activeRepo.path} label="Copy project path" />
</span>{activeInventory?.origin_url && <span className="meta-chip" title={activeInventory.origin_url}>
<span className="meta-chip-label">{originSlug(activeInventory.origin_url)}</span>
<CopyButton value={activeInventory.origin_url} label="Copy clone URL" />
</span>}{activeInventory?.default_branch && <span className="meta-chip" title="Default branch">
<span className="meta-chip-label">default {shortToken(activeInventory.default_branch)}</span>
<CopyButton value={shortToken(activeInventory.default_branch)} label="Copy branch name" />
</span>}</div>}</div>{activeRepo && <div className="heading-actions" ref={menuAnchorRef}>
<span className="sync-cluster"><UpdatedStamp at={refreshedAt[activeRepo.path]} /><button className={`icon-button ${fetchingRepos[activeRepo.path] ? "spinning" : ""}`} type="button" aria-label="Fetch remote updates" title="Fetch from remotes, then re-read local state" disabled={Boolean(fetchingRepos[activeRepo.path])} onClick={() => void refreshProject()}>
{fetchingRepos[activeRepo.path] ? <LoaderCircle size={15} /> : <Download size={15} />}
</button></span>
<button className={`icon-button ${menuOpen ? "open" : ""}`} type="button" aria-label="Project actions" title="Project actions" aria-haspopup="menu" aria-expanded={menuOpen} onClick={() => setMenuOpen((open) => !open)}>
<MoreVertical size={15} />
</button>{menuOpen && <div className="project-menu" role="menu" aria-label="Project actions">
<button className="menu-item" type="button" role="menuitem" onClick={() => { setMenuOpen(false); void togglePin(activeRepo); }}>{activeRepo.pinned_at === null ? <Pin size={13} /> : <PinOff size={13} />}{activeRepo.pinned_at === null ? "Pin project" : "Unpin project"}</button>
<button className="menu-item" type="button" role="menuitem" onClick={() => { void copyText(activeRepo.path); setMenuOpen(false); }}>
<Copy size={13} />Copy path</button>
<div className="menu-separator" />
<button className="menu-item danger" type="button" role="menuitem" onClick={() => { setMenuOpen(false); setRemoveTarget(activeRepo); }}>
<Trash2 size={13} />Remove from WorktreeView</button>
<p className="menu-note">Removes the project from this app only. Your repository, worktrees, and history on disk are never touched.</p>
</div>}</div>}</div>{loading ? <Empty icon={<CircleDot size={24} />} title="Loading repositories..." detail="Reading saved repositories." /> : loadError ? <Empty icon={<CircleDot size={24} />} title="Repositories could not be loaded" detail={loadError} /> : !activeRepo ? <Empty icon={<FolderGit2 size={24} />} title="No repositories" detail="Open a local Git folder to begin." action={<button className="secondary-button" type="button" onClick={() => void openRepository()} disabled={opening}>Open repository...</button>} /> : activeRepoHydrating ? <Empty icon={<CircleDot size={24} />} title="Loading worktrees..." detail="Reading live worktree identity." /> : repoErrors[activeRepo.path] ? <Empty icon={<CircleDot size={24} />} title="Worktrees unavailable" detail={repoErrors[activeRepo.path]} /> : activeRepo.worktrees.length === 0 ? <Empty icon={<CircleDot size={24} />} title="No worktrees" detail="This repository has no linked worktrees." /> : <>
<div className="overview-filter">
<div className="overview-tabs" role="tablist" aria-label="Project inventory">{OVERVIEW_TABS.map((tab) => <button key={tab.id} role="tab" type="button" aria-selected={overviewTab === tab.id} className={`overview-tab ${overviewTab === tab.id ? "active" : ""}`} onClick={() => { setOverviewTab(tab.id); setOverviewPage(0); }}>{tab.label}<span className="tab-count">{overviewTabCount(tab.id)}</span>
</button>)}</div>
<div className="overview-filter-input">
<Search size={12} />
<input type="text" aria-label={activeTab.filter} placeholder={activeTab.filter} value={overviewQuery} onChange={(event) => { setOverviewQuery(event.currentTarget.value); setOverviewPage(0); }} />
</div>
</div>{overviewTab === "worktrees" && <>
<div className="table-header" aria-hidden="true">
<span>Worktree</span>
<span>Status</span>
<span>Last commit</span>
</div>
<div className="worktree-list">{overviewSlice(filteredWorktrees).map((worktree) => {
          const changes = statusByPath[worktree.path];
          const selected = worktree.path === selectedWorktreePath;
          const detached = !worktree.branch.startsWith("refs/heads/");
          const summary = detached ? undefined : branchByRef.get(worktree.branch);
          const stale = summary !== undefined && Date.now() / 1000 - summary.commit_date > STALE_AFTER_SECONDS;
          const syncBase = summary?.upstream ?? activeInventory?.default_branch ?? null;
          const ahead = summary?.ahead ?? 0;
          const behind = summary?.behind ?? 0;
          const pinned = branchPinIndex.get(`worktree:${worktreeKey(worktree.path)}`) !== undefined;
          const openDefaultReview = () => { setSelectedWorktreePath(worktree.path); void openReview({ kind: "worktree", worktree }); };
          return <div key={worktree.path} className="project-row-wrap"><div className={`worktree-row ${selected ? "selected" : ""}`} role="button" tabIndex={0} title={worktree.path} aria-pressed={selected} onClick={openDefaultReview} onKeyDown={(event) => { if (event.target !== event.currentTarget) return; if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openDefaultReview(); } }}><div className="worktree-cell"><div className="branch-title"><GitBranch size={14} /><strong>{shortToken(worktree.branch)}</strong>{worktree.branch === activeInventory?.default_branch && <span className="main-badge">main</span>}{detached && <span className="detached-badge">detached</span>}{stale && <span className="stale-badge">stale</span>}</div><div className="worktree-path"><span className="path-text">{worktree.path}</span><CopyButton ghost value={worktree.path} label="Copy worktree path" /></div></div><div className="status-cell">{mergedChip(summary?.merged ?? false, worktree.branch)}{changes !== undefined && changes !== null && (changes ? <button className="status-chip dirty" type="button" title="Review working changes: the uncommitted diff against HEAD" onClick={(event) => { event.stopPropagation(); setSelectedWorktreePath(worktree.path); void openReview({ kind: "worktree", worktree }, activeRepoPath, { base: workingChangesBase(worktree), scope: "all", reversed: false }); }}>{changesLabel(changes)}</button> : <span className="status-chip clean">Clean</span>)}{syncChip(syncBase, ahead, behind, (base) => { setSelectedWorktreePath(worktree.path); void openReview({ kind: "worktree", worktree }, activeRepoPath, { base, scope: "committed", reversed: false }); })}</div><div className="last-commit-cell">{summary ? <><span className="row-commit-subject">{summary.subject}</span><span className="row-commit-age">{summary.author} · {relativeTime(summary.commit_date)}</span></> : <span className="row-commit-age">{detached && <span className="commit-hash"><span title={worktree.head}>HEAD {shortToken(worktree.head)}</span><CopyButton ghost value={worktree.head} label={`Copy commit hash ${shortToken(worktree.head)}`} /></span>}</span>}</div></div><button className="pin-button surface-pin" type="button" aria-label={`${pinned ? "Unpin" : "Pin"} worktree ${shortToken(worktree.branch)}`} title={`${pinned ? "Unpin" : "Pin"} worktree`} onClick={(event) => { event.stopPropagation(); void toggleSurfacePin(activeRepoPath, "worktree", worktree.path, !pinned); }}>{pinned ? <PinOff size={12} /> : <Pin size={12} />}</button></div>;
        })}</div>{filteredWorktrees.length === 0 && <div className="filter-empty">{overviewNeedle ? `No worktrees match "${overviewQuery}"` : "No worktrees"}</div>}</>}{overviewTab !== "worktrees" && <><div className="table-header" aria-hidden="true">{overviewTab === "archived" ? <><span>Archived surface</span><span>Detail</span><span>Last seen</span></> : <><span>Branch</span><span>Sync</span><span>Last commit</span></>}</div><div className="worktree-list">{overviewTab === "archived" ? <>{overviewSlice(filteredGone).map((surface) => archivedRow(surface))}{filteredGone.length === 0 && <div className="filter-empty">{overviewNeedle ? `No archived surfaces match "${overviewQuery}"` : "No archived surfaces"}</div>}</> : <>{overviewSlice(overviewTab === "branches" ? filteredBranches : filteredRemoteBranches).map((branch) => inventoryBranchRow(branch))}{(overviewTab === "branches" ? filteredBranches : filteredRemoteBranches).length === 0 && <div className="filter-empty">{overviewNeedle ? (overviewTab === "remote" ? `No remote branches match "${overviewQuery}"` : `No branches match "${overviewQuery}"`) : inventoryUnavailable ? "Branch inventory unavailable" : overviewTab === "remote" && activeInventory ? <>No remote branches yet; this tab lists what a fetch saw. <button className="link-button" type="button" disabled={Boolean(fetchingRepos[activeRepoPath])} onClick={() => void refreshProject()}>Fetch remote</button></> : activeInventory ? "No branches" : "Loading branch inventory..."}</div>}</>}</div></>}{overviewPageCount > 1 && <Pager label={activeTab.pager} page={visibleOverviewPage} pages={overviewPageCount} total={overviewTotal} size={activeTab.pageSize} onPage={setOverviewPage} />}</>}</section>}</main>{settingsLocation && <SettingsPage settings={settings} saveError={settingsSaveError} onBack={() => nav.back()} onChange={(next) => void updateSettings(next)} />}</section>{latestArrival && <div className="arrival-cue" role="status" aria-label="Review arrivals"><button className="arrival-open" type="button" onClick={openOldestArrival}><Inbox size={13} /><span><strong>{latestArrival.actor}</strong> {arrivalSentenceBody(latestArrival.moment, arrivalChangeLabel(latestArrival.target_kind, latestArrival.target_key), arrivalProjectLabel(repos, latestArrival.repo_path))}{olderArrivalsSuffix(arrivals.length - 1)}</span></button><button className="arrival-dismiss" type="button" aria-label="Dismiss review arrivals" title="Dismiss" onClick={dismissArrivals}><X size={12} /></button></div>}{paletteOpen && <dialog className="palette-backdrop" ref={paletteRef} aria-label="Find repositories, worktrees, and conversations" onClose={handlePaletteClosed} onKeyDown={handlePaletteKeyDown} onMouseDown={(event) => { if (event.target === event.currentTarget) closePalette(); }}><div className="palette" onMouseDown={(event) => event.stopPropagation()}><div className="palette-input-row"><Search size={17} /><input autoFocus value={query} onChange={(event) => { setQuery(event.currentTarget.value); setSearchPage(0); }} placeholder="Find repositories, worktrees, and conversations" /><button type="button" aria-label="Close search" onClick={closePalette}><X size={16} /></button></div><div className="palette-results">{visiblePaletteRows.map((row, index) => {
          const header = index === 0 || visiblePaletteRows[index - 1].section !== row.section;
          const sectionLabel = header && <div className="nav-section-label">{PALETTE_SECTION_LABELS[row.section]}</div>;
          if (row.section === "repos") { const result = row.result; return <Fragment key={`repos:${result.repo.path}:${result.worktree?.path ?? "repo"}`}>{sectionLabel}<button type="button" title={result.worktree?.path ?? result.repo.path} onClick={() => { activateRepo(result.repo); if (!result.worktree) { closePalette(); } else { setSelectedWorktreePath(result.worktree.path); void openReview({ kind: "worktree", worktree: result.worktree }, result.repo.path); closePalette(); } }}>{result.worktree ? <GitBranch size={16} /> : <FolderGit2 size={16} />}<span><strong>{result.worktree ? shortToken(result.worktree.branch) : result.repo.name}</strong>{result.worktree && <small>{result.repo.name}</small>}</span><kbd>Enter</kbd></button></Fragment>; }
          if (row.section === "comments") { const match = row.match; return <Fragment key={`comments:${match.comment_id}`}>{sectionLabel}<button type="button" title={match.excerpt} onClick={() => { closePalette(); openPortalThread(match.root_comment_id); }}><MessageSquare size={16} /><span><strong>{match.excerpt}</strong><small>{match.change_label} · {repoNames.get(match.repo_path) ?? match.repo_path}</small></span></button></Fragment>; }
          if (row.section === "requests") { const match = row.match; return <Fragment key={`requests:${match.repo_path}:${match.base_sha}:${match.target_key}`}>{sectionLabel}<button type="button" title={match.note || match.change_label} onClick={() => { closePalette(); openReviewIdentity(match); }}><MessagesSquare size={16} /><span><strong>{match.change_label}</strong><small>{requestStatusLabel(match.status)} · {repoNames.get(match.repo_path) ?? match.repo_path}</small></span></button></Fragment>; }
          const match = row.match; return <Fragment key={`commits:${match.repo_path}:${match.sha}`}>{sectionLabel}<button type="button" title={match.subject} onClick={() => { closePalette(); void openReview({ kind: "commit", sha: match.sha, parents: match.parents, defaultBaseAncestor: false }, match.repo_path); }}><GitCommitHorizontal size={16} /><span><strong>{match.subject}</strong><small><code>{shortToken(match.sha)}</code> · {repoNames.get(match.repo_path) ?? match.repo_path}</small></span></button></Fragment>;
        })}{paletteRows.length === 0 && <p>No repositories, worktrees, or conversations match "{query}".</p>}{palettePageCount > 1 && <Pager label="Search result pages" page={visiblePalettePage} pages={palettePageCount} total={paletteRows.length} size={SEARCH_PAGE_SIZE} onPage={setSearchPage} />}</div></div></dialog>}{removeTarget && <dialog className="palette-backdrop" ref={confirmRef} aria-label="Remove project" onClose={() => setRemoveTarget(null)} onMouseDown={(event) => { if (event.target === event.currentTarget) setRemoveTarget(null); }}><div className="confirm-dialog" onMouseDown={(event) => event.stopPropagation()}><h2>Remove {removeTarget.name}?</h2><p>Removes the project from WorktreeView only. Your repository, worktrees, and history on disk are never touched.</p><div className="confirm-actions"><button className="secondary-button" type="button" onClick={() => setRemoveTarget(null)}>Cancel</button><button className="danger-button" type="button" disabled={removing} onClick={() => void removeProject(removeTarget)}>{removing ? "Removing..." : "Remove project"}</button></div></div></dialog>}</div>;
}

function RepoNavGroup({ repo, active, expanded, collapsed, onToggle, onPin, rows }: { repo: Repo; active: boolean; expanded: boolean; collapsed: boolean; onToggle: () => void; onPin: () => void; rows: React.ReactNode }) {
  return <div className="project-group"><div className="project-row-wrap"><button className={`project-row ${active ? "active" : ""}`} type="button" title={collapsed ? undefined : repo.path} data-tip={repo.name} aria-current={active ? "true" : undefined} onClick={onToggle}>{expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}<span className="project-avatar">{repo.name.slice(0, 2)}</span><span className="project-copy"><strong>{repo.name}</strong></span></button><button className="pin-button" type="button" aria-label={repo.pinned_at === null ? "Pin repository" : "Unpin repository"} title={repo.pinned_at === null ? "Pin repository" : "Unpin repository"} onClick={onPin}>{repo.pinned_at === null ? <Pin size={12} /> : <PinOff size={12} />}</button></div>{expanded && <div className="sidebar-children">{rows}</div>}</div>;
}


// The Attention queue: the sidebar's second projection over every project,
// store-backed and read-only. Category membership arrives in the payload;
// this view only counts, filters, sorts, and labels.
function AttentionQueueView({ queue, endpointEnabled, tab, onTab, onOpenRow }: { queue: AttentionQueue | null; endpointEnabled: boolean; tab: AttentionCategory; onTab: (tab: AttentionCategory) => void; onOpenRow: (row: AttentionRow) => void }) {
  const rows = attentionRows(queue);
  const counts = attentionTabCounts(rows);
  const repoNames = new Map((queue?.repos ?? []).map((group) => [group.repo_path, group.repo_name]));
  const [paneRef, paneWidth] = usePaneWidth();
  const narrow = paneWidth > 0 && isNarrowAttention(paneWidth);
  const visible = rowsForAttentionTab(rows, tab);
  const activeTab = ATTENTION_TABS.find((item) => item.id === tab) ?? ATTENTION_TABS[0];
  const now = useNow(10000);
  return <section className="inbox-pane attention-pane" ref={paneRef} aria-label="Inbox">
    {!endpointEnabled && <p className="attention-endpoint-note">The agent endpoint is off, so agents cannot reach this queue. Reviews already delivered still appear.</p>}
    <div className="overview-tabs" role="tablist" aria-label="Inbox categories">{ATTENTION_TABS.map((item) => <button key={item.id} role="tab" type="button" aria-selected={tab === item.id} className={`overview-tab ${tab === item.id ? "active" : ""}`} onClick={() => onTab(item.id)}>{item.label}<span className="tab-count">{counts[item.id]}</span></button>)}</div>
    {rows.length === 0 ? <Empty icon={<Inbox size={24} />} title="Nothing needs attention" detail="Review requests and findings land here as agents work." /> : <>
      <div className={`table-header attention-head ${narrow ? "attention-narrow" : ""}`} aria-hidden="true"><span>Project</span><span>Change</span><span>Requester</span><span>Status</span><span>Round</span><span>Findings</span>{!narrow && <span className="attention-age">Age</span>}</div>
      <div className={`attention-list ${narrow ? "attention-narrow" : ""}`}>{visible.map((row) => {
        const status = attentionStatus(row);
        const openRow = () => onOpenRow(row);
        const blocking = row.unresolved_p0 + row.unresolved_p1;
        return <div key={`${row.repo_path}:${row.base_sha}:${row.target_key}:${row.request_id ?? "surface"}`} className="attention-row" role="button" tabIndex={0} onClick={openRow} onKeyDown={(event) => { if (event.target !== event.currentTarget) return; if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openRow(); } }}>
          <div className="attention-project"><strong title={row.repo_path}>{repoNames.get(row.repo_path) ?? row.repo_path}</strong></div>
          <div className="attention-change"><strong>{groupedChangeLabel(row, rows)}</strong>{row.stale && <span className="comment-badge comment-state-badge" title="The surface's head moved past the reviewed head">stale</span>}<span className="attention-preview">{attentionPreview(row, now)}</span><span className="attention-meta">{reviewsBackLabel(row) && <span title="Reviews delivered by the named reviewers">{reviewsBackLabel(row)}</span>}{row.target_kind === "head" ? <code title={row.target_key}>{shortToken(row.target_key)}</code> : <span className="path-text" title={row.target_key}>{row.target_key}</span>}{narrow && <span className="attention-age">{attentionAge(row.age_basis, now)}</span>}</span></div>
          <div className="attention-requester">{row.requester && <span className="comment-badge" title={`Requested by ${row.requester}`}>{row.requester}</span>}</div>
          <div className="attention-status">{status && <span className={`status-chip ${status.tone === "needs-human" ? "attention-danger" : "clean"}`}>{status.label}</span>}</div>
          <div className="attention-round">{row.max_rounds > 0 && <code title={`Round ${row.round} of ${row.max_rounds}`}>{row.round}/{row.max_rounds}</code>}</div>
          <div className="attention-findings">{narrow
            ? blocking > 0 && <span className="comment-badge severity-P1" aria-label={`${blocking} blocking findings`}>{blocking}</span>
            : <>{row.unresolved_p0 > 0 && <span className="comment-badge severity-P0" aria-label={`${row.unresolved_p0} P0 findings`}>{row.unresolved_p0} P0</span>}{row.unresolved_p1 > 0 && <span className="comment-badge severity-P1" aria-label={`${row.unresolved_p1} P1 findings`}>{row.unresolved_p1} P1</span>}</>}</div>
          {!narrow && <div className="attention-age">{attentionAge(row.age_basis, now)}</div>}
        </div>;
      })}</div>
      {visible.length === 0 && <div className="filter-empty">{activeTab.empty}</div>}
    </>}
  </section>;
}
export default App;
