import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { ArrowLeft, ChevronDown, ChevronRight, ChevronsLeft, CircleDot, Command, FileDiff, FolderGit2, GitBranch, HardDrive, History, Inbox, Search, Settings, X } from "lucide-react";
import "./App.css";

type Worktree = { path: string; branch: string; head: string };
type Repo = { path: string; name: string; worktrees: Worktree[]; pinned_at: number | null };
type CommandError = { code: string; message: string };
type SearchResult = { repo: Repo; worktree?: Worktree };
type RefInventory = { heads: string[]; remotes: string[]; tags: string[]; default_base: string | null };
type ChangedFile = { path: string; status: string; untracked: boolean };
type ReviewIndex = { files: ChangedFile[]; additions: number; deletions: number; base_sha: string; target_sha: string; error?: string };
type FilePatch = { binary: boolean; text: string };
type ReviewScope = "all" | "committed";
type ReviewTarget = { kind: "worktree"; worktree: Worktree } | { kind: "ref"; name: string } | { kind: "commit"; sha: string; parents: string[]; defaultBaseAncestor: boolean };
type ReviewIdentity = { repoPath: string; base: string; target: ReviewTarget; scope: ReviewScope; reversed: boolean };
type CommitInfo = { sha: string; subject: string; author: string; date: string; refs: string[]; parents: string[]; default_base_ancestor: boolean };
type CommitPage = { commits: CommitInfo[]; has_more: boolean };
type HistoryEntry = { repoPath: string; startPointLabel: string; worktreePath?: string; startRef?: string };
type HistoryState = HistoryEntry & { commits: CommitInfo[]; hasMore: boolean; loading: boolean; error: string; selected: CommitInfo | null };
type DiffLine = { text: string; oldLine: number | null; newLine: number | null };
type ParsedHunk = { header: string; lines: DiffLine[] };
type RenderedHunk = { header: string; lines: DiffLine[] };

const WORKTREE_PAGE_SIZE = 100;
const SEARCH_PAGE_SIZE = 50;
const HUNKS_PAGE_SIZE = 50;
const BRANCH_PAGE_SIZE = 50;
const PATCH_LINE_PAGE_SIZE = 500;
const COMMIT_PAGE_SIZE = 100;

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
    if (commandError.code === "scope_requires_worktree") return "All changes scope requires the worktree's checked-out state.";
    if (commandError.code === "unresolvable_ref") return commandError.message;
  }
  if (typeof error === "object" && error !== null && "message" in error) return String(error.message);
  return "The repository operation failed.";
}

function shortToken(token: string) {
  if (token.startsWith("refs/heads/")) return token.slice("refs/heads/".length);
  if (token.startsWith("refs/remotes/")) return token.slice("refs/remotes/".length);
  if (token.startsWith("refs/tags/")) return token.slice("refs/tags/".length);
  return /^[0-9a-f]{40}$/i.test(token) ? token.slice(0, 7) : token;
}
function commitTargetOf(commit: CommitInfo): ReviewTarget { return { kind: "commit", sha: commit.sha, parents: commit.parents, defaultBaseAncestor: commit.default_base_ancestor }; }
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

function paginateHunks(hunks: ParsedHunk[]) {
  const pages: RenderedHunk[][] = [];
  let page: RenderedHunk[] = [];
  let lineCount = 0;
  const finishPage = () => {
    if (page.length > 0) pages.push(page);
    page = [];
    lineCount = 0;
  };
  for (const hunk of hunks) {
    const lines = hunk.lines;
    if (lines.length === 0) {
      if (page.length === HUNKS_PAGE_SIZE) finishPage();
      page.push({ header: hunk.header, lines: [] });
      continue;
    }
    for (let offset = 0; offset < lines.length;) {
      if (page.length === HUNKS_PAGE_SIZE || lineCount === PATCH_LINE_PAGE_SIZE) finishPage();
      const count = Math.min(lines.length - offset, PATCH_LINE_PAGE_SIZE - lineCount);
       page.push({ header: hunk.header, lines: lines.slice(offset, offset + count) });
      offset += count;
      lineCount += count;
    }
  }
  finishPage();
  return pages;
}

function sameReview(left: ReviewIdentity | null, right: ReviewIdentity) {
  return left?.repoPath === right.repoPath
    && left.base === right.base
    && left.target.kind === right.target.kind
    && (left.target.kind === "ref" ? right.target.kind === "ref" && left.target.name === right.target.name : left.target.kind === "commit" ? right.target.kind === "commit" && left.target.sha === right.target.sha : right.target.kind === "worktree" && left.target.worktree.path === right.target.worktree.path)
    && left.scope === right.scope
    && left.reversed === right.reversed;
}

function App() {
  const [repos, setRepos] = useState<Repo[]>([]); const [activeRepoPath, setActiveRepoPath] = useState(""); const [selectedWorktreePath, setSelectedWorktreePath] = useState("");
  const [loading, setLoading] = useState(true); const [opening, setOpening] = useState(false); const [loadError, setLoadError] = useState(""); const [operationError, setOperationError] = useState(""); const [repoErrors, setRepoErrors] = useState<Record<string, string>>({}); const [hydratingRepos, setHydratingRepos] = useState<Record<string, number>>({});
  const [collapsed, setCollapsed] = useState(false); const [paletteOpen, setPaletteOpen] = useState(false); const [query, setQuery] = useState(""); const [worktreePage, setWorktreePage] = useState(0); const [searchPage, setSearchPage] = useState(0);
  const [reviewTarget, setReviewTarget] = useState<ReviewTarget | null>(null); const [refs, setRefs] = useState<RefInventory>({ heads: [], remotes: [], tags: [], default_base: null }); const [base, setBase] = useState(""); const [scope, setScope] = useState<ReviewScope>("all"); const [reversed, setReversed] = useState(false); const [reviewIndex, setReviewIndex] = useState<ReviewIndex | null>(null); const [reviewLoading, setReviewLoading] = useState(false); const [selectedFile, setSelectedFile] = useState<ChangedFile | null>(null); const [patch, setPatch] = useState<FilePatch | null>(null); const [patchError, setPatchError] = useState(""); const [patchLoading, setPatchLoading] = useState(false); const [filePage, setFilePage] = useState(0); const [hunkPage, setHunkPage] = useState(0);
  const [expandedRepos, setExpandedRepos] = useState<Record<string, boolean>>({}); const [repoRefs, setRepoRefs] = useState<Record<string, string[]>>({}); const [repoPages, setRepoPages] = useState<Record<string, { worktrees: number; branches: number }>>({});
  const [history, setHistory] = useState<HistoryState | null>(null); const [historyRefs, setHistoryRefs] = useState<RefInventory>({ heads: [], remotes: [], tags: [], default_base: null }); const [openedFromHistory, setOpenedFromHistory] = useState(false);
  const paletteRef = useRef<HTMLDialogElement>(null); const paletteOpenerRef = useRef<HTMLElement | null>(null);
  const indexGenerationRef = useRef(0); const patchGenerationRef = useRef(0); const reviewIdentityRef = useRef<ReviewIdentity | null>(null); const patchIdentityRef = useRef(""); const reviewRepoPathRef = useRef(""); const historyGenerationRef = useRef(0);
  const activeRepo = repos.find((repo) => repo.path === activeRepoPath); const hydrating = Object.keys(hydratingRepos).length > 0; const activeRepoHydrating = activeRepo ? Boolean(hydratingRepos[activeRepo.path]) : false;
  const historySelected = history?.selected ?? null;

  useEffect(() => { let mounted = true; async function load() { try { const loaded = await invoke<Repo[]>("list_repos"); if (!mounted) return; setRepos(loaded); setActiveRepoPath((current) => loaded.some((repo) => repo.path === current) ? current : loaded[0]?.path ?? ""); setHydratingRepos(Object.fromEntries(loaded.map((repo) => [repo.path, 1]))); setLoading(false); for (let start = 0; start < loaded.length; start += 4) await Promise.all(loaded.slice(start, start + 4).map(async (repo) => { try { const worktrees = await invoke<Worktree[]>("list_worktrees", { path: repo.path }); if (mounted) setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, worktrees } : item)); } catch (error) { if (mounted) setRepoErrors((current) => ({ ...current, [repo.path]: errorMessage(error) })); } finally { if (mounted) setHydratingRepos((current) => { const count = current[repo.path] ?? 0; if (count > 1) return { ...current, [repo.path]: count - 1 }; const next = { ...current }; delete next[repo.path]; return next; }); } })); } catch (error) { if (mounted) { setLoadError(errorMessage(error)); setLoading(false); } } } void load(); return () => { mounted = false; }; }, []);
  useEffect(() => { if (!activeRepo) { setSelectedWorktreePath(""); return; } setSelectedWorktreePath((current) => activeRepo.worktrees.some((worktree) => worktree.path === current) ? current : activeRepo.worktrees[0]?.path ?? ""); }, [activeRepo]);
  useEffect(() => { setWorktreePage(0); }, [activeRepoPath]);
  useEffect(() => { function handleKeyboard(event: KeyboardEvent) { if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") { event.preventDefault(); paletteRef.current?.open ? closePalette() : openPalette(); } } window.addEventListener("keydown", handleKeyboard); return () => window.removeEventListener("keydown", handleKeyboard); }, []);
  useEffect(() => { const dialog = paletteRef.current; if (paletteOpen && dialog && !dialog.open) dialog.showModal(); }, [paletteOpen]);
  useEffect(() => { if (reviewTarget) return; ++indexGenerationRef.current; ++patchGenerationRef.current; reviewIdentityRef.current = null; patchIdentityRef.current = ""; reviewRepoPathRef.current = ""; }, [reviewTarget]);
  useEffect(() => { if (reviewTarget || !historySelected) return; void fetchIndex(commitTargetOf(historySelected), historySelected.parents[0] ?? "empty-tree", "committed", false, history?.repoPath ?? ""); }, [reviewTarget, historySelected]);

  function openPalette(opener?: HTMLElement | null) { paletteOpenerRef.current = opener ?? (document.activeElement instanceof HTMLElement ? document.activeElement : null); setPaletteOpen(true); }
  function closePalette() { if (paletteRef.current?.open) paletteRef.current.close(); else handlePaletteClosed(); }
  function handlePaletteClosed() { setPaletteOpen(false); setQuery(""); setSearchPage(0); paletteOpenerRef.current?.focus(); paletteOpenerRef.current = null; }
  function handlePaletteKeyDown(event: ReactKeyboardEvent<HTMLDialogElement>) { if (event.key !== "Tab") return; const focusable = Array.from(event.currentTarget.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])')); const first = focusable[0], last = focusable[focusable.length - 1]; if (!first || !last) return; if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); } else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); } }
  async function openRepository() { if (opening) return; let selected: string | null; try { selected = import.meta.env.MODE === "e2e" && import.meta.env.VITE_E2E_PICKER_PATH === "/tmp/worktreeview-e2e-selection" ? "/tmp/worktreeview-e2e-selection" : await open({ directory: true, multiple: false }); } catch (error) { setOperationError(errorMessage(error)); return; } if (!selected || Array.isArray(selected)) return; setOpening(true); setOperationError(""); try { const repo = await invoke<Repo>("open_repo", { path: selected }); closeReviewSurfaces(); setRepos((current) => [repo, ...current.filter((item) => item.path !== repo.path)]); setActiveRepoPath(repo.path); setSelectedWorktreePath(""); setRepoErrors((current) => { const next = { ...current }; delete next[repo.path]; return next; }); setHydratingRepos((current) => ({ ...current, [repo.path]: (current[repo.path] ?? 0) + 1 })); try { const worktrees = await invoke<Worktree[]>("list_worktrees", { path: repo.path }); setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, worktrees } : item)); } catch (error) { setRepoErrors((current) => ({ ...current, [repo.path]: errorMessage(error) })); } finally { setHydratingRepos((current) => { const count = current[repo.path] ?? 0; if (count > 1) return { ...current, [repo.path]: count - 1 }; const next = { ...current }; delete next[repo.path]; return next; }); } } catch (error) { setOperationError(errorMessage(error)); } finally { setOpening(false); } }

  async function openReview(target: ReviewTarget, repoPath = activeRepoPath, baseOverride?: string) {
    const nextScope = target.kind === "worktree" ? scope : "committed";
    const identity = { repoPath, base: "", target, scope: nextScope, reversed };
    const generation = ++indexGenerationRef.current;
    ++patchGenerationRef.current;
    reviewIdentityRef.current = identity;
    reviewRepoPathRef.current = repoPath;
    patchIdentityRef.current = "";
    setReviewTarget(target); setRefs({ heads: [], remotes: [], tags: [], default_base: null }); setBase(""); setScope(nextScope); setReviewIndex(null); setSelectedFile(null); setPatch(null); setPatchError(""); setOperationError(""); setReviewLoading(true); setPatchLoading(false);
    try {
      const inventory = await invoke<RefInventory>("list_refs", { path: repoPath, worktreeBranch: target.kind === "worktree" ? target.worktree.branch : null });
      if (generation !== indexGenerationRef.current || !sameReview(reviewIdentityRef.current, identity)) return;
      setRefs(inventory);
      let nextBase = "";
      if (target.kind === "worktree") {
        const checkedOut = target.worktree.branch;
        const remoteBases = inventory.remotes.filter((ref) => ref.endsWith(`/${checkedOut.replace(/^refs\/heads\//, "")}`));
        nextBase = checkedOut.startsWith("refs/heads/") && remoteBases.length === 1 ? remoteBases[0] : inventory.default_base ?? "";
      } else if (target.kind === "ref") {
        nextBase = inventory.default_base === target.name ? "" : inventory.default_base ?? "";
      } else {
        nextBase = baseOverride ?? (inventory.default_base && !target.defaultBaseAncestor ? inventory.default_base : target.parents[0] ?? "empty-tree");
      }
      if (!nextBase) return;
      setBase(nextBase);
      await fetchIndex(target, nextBase, nextScope, reversed, repoPath);
    } catch (error) {
      if (generation === indexGenerationRef.current && sameReview(reviewIdentityRef.current, identity)) {
        setReviewIndex({ files: [], additions: 0, deletions: 0, base_sha: "", target_sha: "", error: errorMessage(error) });
      }
    } finally {
      if (generation === indexGenerationRef.current && sameReview(reviewIdentityRef.current, identity)) setReviewLoading(false);
    }
  }
  async function toggleRepo(repo: Repo) {
    const expanded = !expandedRepos[repo.path];
    setExpandedRepos((current) => ({ ...current, [repo.path]: expanded }));
    if (expanded && repoRefs[repo.path] === undefined) {
      try {
        const inventory = await invoke<RefInventory>("list_refs", { path: repo.path, worktreeBranch: null });
        setRepoRefs((current) => ({ ...current, [repo.path]: inventory.heads.filter((head) => !repo.worktrees.some((worktree) => worktree.branch === head)) }));
      } catch (error) { setRepoErrors((current) => ({ ...current, [repo.path]: errorMessage(error) })); }
    }
  }
  async function togglePin(repo: Repo) {
    try {
      const pinned_at = await invoke<number | null>("set_repo_pinned", { path: repo.path, pinned: repo.pinned_at === null });
      setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, pinned_at } : item));
    } catch (error) { setOperationError(errorMessage(error)); }
  }
  async function fetchIndex(target: ReviewTarget, nextBase: string, nextScope: ReviewScope, nextReversed: boolean, nextRepoPath: string) {
    const identity = { repoPath: nextRepoPath, base: nextBase, target, scope: nextScope, reversed: nextReversed };
    const generation = ++indexGenerationRef.current;
    ++patchGenerationRef.current;
    reviewIdentityRef.current = identity;
    patchIdentityRef.current = "";
    setReviewLoading(true); setReviewIndex(null); setSelectedFile(null); setPatch(null); setPatchError(""); setPatchLoading(false); setFilePage(0);
    try {
      const index = await invoke<ReviewIndex>("list_review_changes", { path: target.kind === "worktree" ? target.worktree.path : nextRepoPath, base: nextBase, headRef: target.kind === "ref" ? target.name : target.kind === "commit" ? target.sha : null, committedOnly: nextScope === "committed", reversed: nextReversed });
      if (generation === indexGenerationRef.current && sameReview(reviewIdentityRef.current, identity)) setReviewIndex(index);
    } catch (error) {
      if (generation === indexGenerationRef.current && sameReview(reviewIdentityRef.current, identity)) {
        setReviewIndex({ files: [], additions: 0, deletions: 0, base_sha: "", target_sha: "", error: errorMessage(error) });
      }
    } finally {
      if (generation === indexGenerationRef.current && sameReview(reviewIdentityRef.current, identity)) setReviewLoading(false);
    }
  }
  function changeReviewSetting(nextBase: string, nextScope = scope, nextReversed = reversed) { if (!reviewTarget || !nextBase) return; setBase(nextBase); setScope(nextScope); setReversed(nextReversed); void fetchIndex(reviewTarget, nextBase, nextScope, nextReversed, reviewRepoPathRef.current); }
  async function selectFile(file: ChangedFile, nextTarget: ReviewTarget, nextBase: string, nextScope: ReviewScope, nextRepoPath: string = reviewRepoPathRef.current, nextReversed: boolean = reversed) {
    if (!nextBase) return;
    const identity = { repoPath: nextRepoPath, base: nextBase, target: nextTarget, scope: nextScope, reversed: nextReversed };
    const patchIdentity = `${nextRepoPath}\0${nextTarget.kind === "ref" ? nextTarget.name : nextTarget.kind === "commit" ? nextTarget.sha : nextTarget.worktree.path}\0${nextBase}\0${nextScope}\0${nextReversed}\0${file.path}\0${file.untracked}`;
    const generation = ++patchGenerationRef.current;
    patchIdentityRef.current = patchIdentity;
    setSelectedFile(file); setPatch(null); setPatchError(""); setPatchLoading(true); setHunkPage(0);
    try {
      const nextPatch = await invoke<FilePatch>("read_review_patch", { path: nextTarget.kind === "worktree" ? nextTarget.worktree.path : nextRepoPath, base: nextBase, headRef: nextTarget.kind === "ref" ? nextTarget.name : nextTarget.kind === "commit" ? nextTarget.sha : null, committedOnly: nextScope === "committed", reversed: nextReversed, file: file.path, untracked: file.untracked });
      if (generation === patchGenerationRef.current && patchIdentityRef.current === patchIdentity && sameReview(reviewIdentityRef.current, identity)) setPatch(nextPatch);
    } catch (error) {
      if (generation !== patchGenerationRef.current || patchIdentityRef.current !== patchIdentity || !sameReview(reviewIdentityRef.current, identity)) return;
      const code = typeof error === "object" && error !== null && "code" in error ? String((error as CommandError).code) : "";
      setPatchError(code === "git_output_too_large" ? "This file's patch exceeds the 4 MiB output bound and was not rendered." : errorMessage(error));
    } finally {
      if (generation === patchGenerationRef.current && patchIdentityRef.current === patchIdentity && sameReview(reviewIdentityRef.current, identity)) setPatchLoading(false);
    }
  }

  function closeHistory() { ++historyGenerationRef.current; setHistory(null); setHistoryRefs({ heads: [], remotes: [], tags: [], default_base: null }); }
  function closeReviewSurfaces() { setReviewTarget(null); setOpenedFromHistory(false); closeHistory(); ++indexGenerationRef.current; ++patchGenerationRef.current; reviewIdentityRef.current = null; patchIdentityRef.current = ""; reviewRepoPathRef.current = ""; setReviewIndex(null); setSelectedFile(null); setPatch(null); setPatchError(""); setPatchLoading(false); setReviewLoading(false); setFilePage(0); setHunkPage(0); }
  function handleReviewBack() { if (openedFromHistory) { setReviewTarget(null); setOpenedFromHistory(false); } else closeReviewSurfaces(); }
  function selectHistoryCommit(commit: CommitInfo) { setHistory((current) => current ? { ...current, selected: commit } : current); }
  async function loadHistoryPage(entry: HistoryEntry, generation: number, skip: number, against: string | null, append: boolean) {
    try {
      const page = await invoke<CommitPage>("list_commits", { path: entry.worktreePath ?? entry.repoPath, startRef: entry.startRef ?? null, against, skip, limit: COMMIT_PAGE_SIZE });
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
  async function openHistoryEntry(entry: HistoryEntry) {
    setReviewTarget(null); setOpenedFromHistory(false);
    const generation = ++historyGenerationRef.current;
    setHistory({ ...entry, commits: [], hasMore: false, loading: true, error: "", selected: null });
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

  const search = query.trim().toLowerCase(); const matchingResults: SearchResult[] = search ? repos.flatMap((repo) => `${repo.name} ${repo.path}`.toLowerCase().includes(search) ? [{ repo }] : repo.worktrees.filter((worktree) => `${worktree.branch} ${worktree.path} ${worktree.head}`.toLowerCase().includes(search)).map((worktree) => ({ repo, worktree }))) : repos.map((repo) => ({ repo }));
  const pinnedRepos = repos.filter((repo) => repo.pinned_at !== null).sort((left, right) => (right.pinned_at ?? 0) - (left.pinned_at ?? 0)); const recentRepos = repos.filter((repo) => repo.pinned_at === null); const sidebarRows = (repo: Repo) => { const page = repoPages[repo.path] ?? { worktrees: 0, branches: 0 }; const branches = repoRefs[repo.path] ?? []; const worktreePageCount = Math.max(1, Math.ceil(repo.worktrees.length / BRANCH_PAGE_SIZE)); const branchPageCount = Math.max(1, Math.ceil(branches.length / BRANCH_PAGE_SIZE)); const visibleWorktrees = repo.worktrees.slice(page.worktrees * BRANCH_PAGE_SIZE, (page.worktrees + 1) * BRANCH_PAGE_SIZE); const visibleBranches = branches.slice(page.branches * BRANCH_PAGE_SIZE, (page.branches + 1) * BRANCH_PAGE_SIZE); return <>{visibleWorktrees.map((worktree) => <div className="project-row-wrap" key={worktree.path}><button className="sidebar-worktree-row" type="button" onClick={() => void openReview({ kind: "worktree", worktree }, repo.path)}><GitBranch size={12} /><span>{shortToken(worktree.branch)}</span></button><button className="history-button" type="button" aria-label={`Show commit history for ${shortToken(worktree.branch)}`} title="Commit history" onClick={() => void openHistoryEntry({ repoPath: repo.path, startPointLabel: shortToken(worktree.branch), worktreePath: worktree.path })}><History size={12} /></button></div>)}{visibleBranches.map((branch) => <div className="project-row-wrap" key={branch}><button className="sidebar-branch-row" type="button" onClick={() => void openReview({ kind: "ref", name: branch }, repo.path)}><GitBranch size={12} /><span>{shortToken(branch)}</span></button><button className="history-button" type="button" aria-label={`Show commit history for ${shortToken(branch)}`} title="Commit history" onClick={() => void openHistoryEntry({ repoPath: repo.path, startPointLabel: shortToken(branch), startRef: branch })}><History size={12} /></button></div>)}{worktreePageCount > 1 && <Pager label={`${repo.name} worktree pages`} page={page.worktrees} pages={worktreePageCount} total={repo.worktrees.length} size={BRANCH_PAGE_SIZE} onPage={(next) => setRepoPages((current) => ({ ...current, [repo.path]: { ...page, worktrees: next } }))} />}{branchPageCount > 1 && <Pager label={`${repo.name} branch pages`} page={page.branches} pages={branchPageCount} total={branches.length} size={BRANCH_PAGE_SIZE} onPage={(next) => setRepoPages((current) => ({ ...current, [repo.path]: { ...page, branches: next } }))} />}</>; };
  const statusMessage = loadError || operationError || (loading ? "Loading repositories..." : hydrating ? "Loading worktrees..." : ""); const worktreePageCount = Math.max(1, Math.ceil((activeRepo?.worktrees.length ?? 0) / WORKTREE_PAGE_SIZE)); const visibleWorktreePage = Math.min(worktreePage, worktreePageCount - 1); const visibleWorktrees = activeRepo?.worktrees.slice(visibleWorktreePage * WORKTREE_PAGE_SIZE, (visibleWorktreePage + 1) * WORKTREE_PAGE_SIZE) ?? []; const searchPageCount = Math.max(1, Math.ceil(matchingResults.length / SEARCH_PAGE_SIZE)); const visibleSearchPage = Math.min(searchPage, searchPageCount - 1); const visibleSearchResults = matchingResults.slice(visibleSearchPage * SEARCH_PAGE_SIZE, (visibleSearchPage + 1) * SEARCH_PAGE_SIZE);

  return <div className={`app-shell ${collapsed ? "nav-collapsed" : ""}`} aria-busy={loading || opening || hydrating}><aside className="sidebar"><div className="brand-row"><span className="brand-mark">wv</span><span className="brand-name">WorktreeView</span><button className="icon-button collapse-button" type="button" aria-label={collapsed ? "Expand project navigator" : "Collapse project navigator"} onClick={() => setCollapsed((value) => !value)}>{collapsed ? <ChevronRight size={16} /> : <ChevronsLeft size={16} />}</button></div><div className="nav-tabs" aria-label="Workspace views"><button className="nav-tab active" type="button" aria-label="Projects" aria-current="page"><FolderGit2 size={15} /><span>Projects</span></button><button className="nav-tab" type="button" aria-label="Attention, unavailable" disabled><Inbox size={15} /><span>Attention</span></button></div><button className="search-trigger" type="button" aria-label="Find repositories and worktrees" onClick={(event) => openPalette(event.currentTarget)}><Search size={14} /><span>Find repositories and worktrees</span><kbd>Ctrl K</kbd></button><nav className="project-list" aria-label="Repositories"><div className="nav-section-label">Pinned</div>{pinnedRepos.length === 0 && <div className="sidebar-empty">No pinned repositories</div>}{pinnedRepos.map((repo) => <RepoNavGroup key={repo.path} repo={repo} active={repo.path === activeRepoPath} expanded={Boolean(expandedRepos[repo.path])} onToggle={() => { setActiveRepoPath(repo.path); closeReviewSurfaces(); void toggleRepo(repo); }} onPin={() => void togglePin(repo)} rows={sidebarRows(repo)} />)}<div className="nav-section-label">Recent</div>{recentRepos.length === 0 && <div className="sidebar-empty">No recent repositories</div>}{recentRepos.map((repo) => <RepoNavGroup key={repo.path} repo={repo} active={repo.path === activeRepoPath} expanded={Boolean(expandedRepos[repo.path])} onToggle={() => { setActiveRepoPath(repo.path); closeReviewSurfaces(); void toggleRepo(repo); }} onPin={() => void togglePin(repo)} rows={sidebarRows(repo)} />)}</nav>{history && <section className="commits-bar" aria-label="Commit history"><div className="commits-bar-heading"><strong>{history.startPointLabel}</strong><button className="icon-button" type="button" aria-label="Close commit history" onClick={closeReviewSurfaces}><X size={12} /></button></div>{history.error ? <div className="sidebar-empty" role="status">{history.error}</div> : history.commits.length === 0 ? <div className="sidebar-empty">{history.loading ? "Loading commits..." : "No commits"}</div> : <><div className="commit-list">{history.commits.map((commit) => <button key={commit.sha} type="button" className={`commit-row ${history.selected?.sha === commit.sha ? "selected" : ""}`} onClick={() => selectHistoryCommit(commit)}><span className="commit-subject">{commit.subject}</span><span className="commit-meta"><code>{shortToken(commit.sha)}</code><span>{commit.author}</span><span>{commit.date.slice(0, 10)}</span>{commit.refs.map((ref) => <span key={ref} className="commit-ref">{shortToken(ref)}</span>)}</span></button>)}</div>{history.hasMore && <button type="button" className="commits-load-more" disabled={history.loading} onClick={() => void loadMoreHistory()}>{history.loading ? "Loading..." : "Load more"}</button>}</>}</section>}<button className="open-repository-button" type="button" aria-label="Open repository" title="Open repository" onClick={() => void openRepository()} disabled={opening}><FolderGit2 size={14} /><span>Open repository...</span></button><div className="sidebar-footer"><HardDrive size={13} /><span>read-only / local</span></div></aside><section className="workspace"><p className="visually-hidden" id="unavailable-features">Attention and Settings are unavailable.</p><div className="live-error" role="status" aria-live="polite">{statusMessage}</div><header className="topbar"><div className="breadcrumb"><span>Repositories</span><span>/</span><strong>{activeRepo?.name ?? "No repository"}</strong>{history && <><span>/</span><strong>{history.startPointLabel}</strong></>}{reviewTarget && <><span>/</span><strong>{shortToken(reviewTarget.kind === "worktree" ? reviewTarget.worktree.branch : reviewTarget.kind === "commit" ? reviewTarget.sha : reviewTarget.name)}</strong></>}</div><div className="topbar-actions"><span className="safety-label">Read-only</span><button className="search-button" type="button" aria-label="Find repositories and worktrees" onClick={(event) => openPalette(event.currentTarget)}><Command size={14} /> <span>Search</span> <kbd>Ctrl K</kbd></button><button className="icon-button" type="button" aria-label="Open settings, unavailable" aria-describedby="unavailable-features" disabled><Settings size={16} /></button></div></header><main className="content">{reviewTarget ? <ReviewView repoPath={reviewRepoPathRef.current || activeRepoPath} liveWorktree={activeRepo?.worktrees.find((worktree) => worktree.path === selectedWorktreePath)} target={reviewTarget} refs={refs} base={base} scope={scope} reversed={reversed} index={reviewIndex} loading={reviewLoading} selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} filePage={filePage} hunkPage={hunkPage} onBack={handleReviewBack} onTargetChange={(target) => { setOpenedFromHistory(false); void openReview(target, reviewRepoPathRef.current || activeRepoPath); }} onBaseChange={(value) => changeReviewSetting(value)} onScopeChange={(value) => changeReviewSetting(base, value)} onReverse={() => changeReviewSetting(base, scope, !reversed)} onFilePage={setFilePage} onHunkPage={setHunkPage} onFile={(file) => { if (reviewTarget) void selectFile(file, reviewTarget, base, scope); }} /> : history?.selected ? <HistoryView history={history} historyRefs={historyRefs} index={reviewIndex} loading={reviewLoading} selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} filePage={filePage} hunkPage={hunkPage} onBack={closeReviewSurfaces} onBasePick={(value) => { const selected = history.selected; if (selected) { setOpenedFromHistory(true); void openReview(commitTargetOf(selected), history.repoPath, value); } }} onFilePage={setFilePage} onHunkPage={setHunkPage} onFile={(file) => { const selected = history.selected; if (selected) void selectFile(file, commitTargetOf(selected), selected.parents[0] ?? "empty-tree", "committed", history.repoPath, false); }} /> : <section className="inbox-pane" aria-labelledby="inbox-heading" aria-busy={loading || activeRepoHydrating}>{operationError && <div className="operation-error">{operationError}</div>}<div className="section-heading"><div><p className="eyebrow">Repository</p><h1 id="inbox-heading">Worktrees</h1></div><span className="path-label">{activeRepo?.path}</span></div>{loading ? <Empty icon={<CircleDot size={24} />} title="Loading repositories..." detail="Reading saved repositories." /> : loadError ? <Empty icon={<CircleDot size={24} />} title="Repositories could not be loaded" detail={loadError} /> : !activeRepo ? <Empty icon={<FolderGit2 size={24} />} title="No repositories" detail="Open a local Git folder to begin." action={<button className="secondary-button" type="button" onClick={() => void openRepository()} disabled={opening}>Open repository...</button>} /> : activeRepoHydrating ? <Empty icon={<CircleDot size={24} />} title="Loading worktrees..." detail="Reading live worktree identity." /> : repoErrors[activeRepo.path] ? <Empty icon={<CircleDot size={24} />} title="Worktrees unavailable" detail={repoErrors[activeRepo.path]} /> : activeRepo.worktrees.length === 0 ? <Empty icon={<CircleDot size={24} />} title="No worktrees" detail="This repository has no linked worktrees." /> : <><div className="table-header" aria-hidden="true"><span>Branch</span><span>Path</span><span>HEAD</span></div><div className="worktree-list">{visibleWorktrees.map((worktree) => <button className={`worktree-row ${worktree.path === selectedWorktreePath ? "selected" : ""}`} key={worktree.path} type="button" aria-pressed={worktree.path === selectedWorktreePath} onClick={() => { setSelectedWorktreePath(worktree.path); void openReview({ kind: "worktree", worktree }); }}><span className="branch-cell"><span className="branch-title"><GitBranch size={14} /><strong>{shortToken(worktree.branch)}</strong></span><small>{worktree.path}</small></span><span className="worktree-path">{worktree.path}</span><code className="head-cell">{worktree.head}</code></button>)}</div>{worktreePageCount > 1 && <Pager label="Worktree pages" page={visibleWorktreePage} pages={worktreePageCount} total={activeRepo.worktrees.length} size={WORKTREE_PAGE_SIZE} onPage={setWorktreePage} />}</>}</section>}</main></section>{paletteOpen && <dialog className="palette-backdrop" ref={paletteRef} aria-label="Find repositories and worktrees" onClose={handlePaletteClosed} onKeyDown={handlePaletteKeyDown} onMouseDown={(event) => { if (event.target === event.currentTarget) closePalette(); }}><div className="palette" onMouseDown={(event) => event.stopPropagation()}><div className="palette-input-row"><Search size={17} /><input autoFocus value={query} onChange={(event) => { setQuery(event.currentTarget.value); setSearchPage(0); }} placeholder="Find repositories, branches, and worktrees" /><button type="button" aria-label="Close search" onClick={closePalette}><X size={16} /></button></div><div className="palette-results"><div className="nav-section-label">Repositories and worktrees</div>{visibleSearchResults.map((result) => <button key={`${result.repo.path}:${result.worktree?.path ?? "repo"}`} type="button" onClick={() => { setActiveRepoPath(result.repo.path); if (!result.worktree) closeReviewSurfaces(); else { if (result.repo.path !== activeRepoPath) closeHistory(); setSelectedWorktreePath(result.worktree.path); void openReview({ kind: "worktree", worktree: result.worktree }, result.repo.path); } closePalette(); }}>{result.worktree ? <GitBranch size={16} /> : <FolderGit2 size={16} />}<span><strong>{result.worktree ? shortToken(result.worktree.branch) : result.repo.name}</strong><small>{result.worktree?.path ?? result.repo.path}</small></span><kbd>Enter</kbd></button>)}{matchingResults.length === 0 && <p>No repositories or worktrees match "{query}".</p>}{searchPageCount > 1 && <Pager label="Search result pages" page={visibleSearchPage} pages={searchPageCount} total={matchingResults.length} size={SEARCH_PAGE_SIZE} onPage={setSearchPage} />}</div></div></dialog>}</div>;
}

function RepoNavGroup({ repo, active, expanded, onToggle, onPin, rows }: { repo: Repo; active: boolean; expanded: boolean; onToggle: () => void; onPin: () => void; rows: React.ReactNode }) {
  return <div className="project-group"><div className="project-row-wrap"><button className={`project-row ${active ? "active" : ""}`} type="button" title={repo.path} aria-current={active ? "true" : undefined} onClick={onToggle}>{expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}<span className="project-avatar">{repo.name.slice(0, 2)}</span><span className="project-copy"><strong>{repo.name}</strong><small>{repo.path}</small></span></button><button className="pin-button" type="button" aria-label={repo.pinned_at === null ? "Pin repository" : "Unpin repository"} onClick={onPin}>{repo.pinned_at === null ? "+" : "x"}</button></div>{expanded && <div className="sidebar-children">{rows}</div>}</div>;
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

function ReviewView({ repoPath, liveWorktree, target, refs, base, scope, reversed, index, loading, selectedFile, patch, patchError, patchLoading, filePage, hunkPage, onBack, onBaseChange, onTargetChange, onScopeChange, onReverse, onFilePage, onHunkPage, onFile }: { repoPath: string; liveWorktree?: Worktree; target: ReviewTarget; refs: RefInventory; base: string; scope: ReviewScope; reversed: boolean; index: ReviewIndex | null; loading: boolean; selectedFile: ChangedFile | null; patch: FilePatch | null; patchError: string; patchLoading: boolean; filePage: number; hunkPage: number; onBack: () => void; onBaseChange: (value: string) => void; onTargetChange: (target: ReviewTarget) => void; onScopeChange: (value: ReviewScope) => void; onReverse: () => void; onFilePage: (page: number) => void; onHunkPage: (page: number) => void; onFile: (file: ChangedFile) => void }) {
  const targetName = target.kind === "worktree" ? target.worktree.branch : target.kind === "commit" ? target.sha : target.name;
  const allRefs = [...refs.heads, ...refs.remotes, ...refs.tags];
  const targetRefs = allRefs.filter((ref) => ref !== liveWorktree?.branch);
  const targetOptions = liveWorktree ? [liveWorktree.branch, ...targetRefs] : allRefs;
  const files = index?.files ?? [];
  const commitPreset = target.kind === "commit" ? target.parents[0] ?? "empty-tree" : "";
  const branchPreset = refs.default_base ?? "";
  return <section className="review-view" aria-label="Code review">
    <header className="review-header">
       <div className="review-heading"><button className="back-button" type="button" onClick={onBack}><ArrowLeft size={15} /> Worktrees</button><p className="eyebrow">Review</p><h1>{shortToken(targetName)}</h1><div className="review-meta"><code>{target.kind === "worktree" ? target.worktree.path : repoPath}</code><code>{target.kind === "ref" ? target.name : target.kind === "commit" ? target.sha : `HEAD ${target.worktree.head}`}</code></div></div>
       <div className="review-controls"><RefPicker id="review-target" label="Review target" refs={targetOptions} value={targetName} onChange={(value) => onTargetChange(liveWorktree && value === liveWorktree.branch ? { kind: "worktree", worktree: liveWorktree } : { kind: "ref", name: value })} /><RefPicker id="review-base" label="Review base" refs={allRefs} value={base} exclude={target.kind === "worktree" ? [] : [targetName]} onChange={onBaseChange} /><button className="swap-button" type="button" aria-label="Swap review direction" title="Swap review direction" onClick={onReverse}>↔ <code>{reversed ? shortToken(targetName) : shortToken(base)}...{reversed ? shortToken(base) : shortToken(targetName)}</code></button>{target.kind === "commit" ? <div className="scope-toggle" role="group" aria-label="Commit review preset"><button className={!target.defaultBaseAncestor && branchPreset && base === branchPreset && branchPreset !== commitPreset ? "active" : ""} type="button" disabled={target.defaultBaseAncestor || !branchPreset} onClick={() => onBaseChange(branchPreset)}>Branch so far</button><button className={base === commitPreset ? "active" : ""} type="button" onClick={() => onBaseChange(commitPreset)}>This commit</button></div> : target.kind === "worktree" ? <div className="scope-toggle" role="group" aria-label="Review scope"><button className={scope === "all" ? "active" : ""} type="button" onClick={() => onScopeChange("all")}>All changes</button><button className={scope === "committed" ? "active" : ""} type="button" onClick={() => onScopeChange("committed")}>Committed only</button></div> : <span className="scope-fixed" aria-label="Review scope">Committed only</span>}</div>
       <div className="review-counts"><code>{index?.error ? "Review index unavailable" : index ? `${files.length} files, +${index.additions} -${index.deletions}` : "Loading review index..."}</code></div>{index && !index.error && <div className="review-identity"><code>base {index.base_sha} / target {index.target_sha}</code></div>}
    </header>
     {!base && !loading ? <Empty icon={<GitBranch size={24} />} title="Choose a base branch to review" detail="This review has no default base." action={<RefPicker id="prompt-review-base" label="Choose review base" refs={allRefs} value={base} exclude={target.kind === "worktree" ? [] : [targetName]} onChange={onBaseChange} />} /> : <div className="review-body">
      <FileIndexPane base={base} index={index} loading={loading} selectedFile={selectedFile} filePage={filePage} onFilePage={onFilePage} onFile={onFile} />
      <PatchPane selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} hunkPage={hunkPage} onHunkPage={onHunkPage} />
    </div>}
  </section>;
}

function FileIndexPane({ base, index, loading, selectedFile, filePage, onFilePage, onFile }: { base: string; index: ReviewIndex | null; loading: boolean; selectedFile: ChangedFile | null; filePage: number; onFilePage: (page: number) => void; onFile: (file: ChangedFile) => void }) {
  const files = index?.files ?? [];
  const pages = Math.max(1, Math.ceil(files.length / WORKTREE_PAGE_SIZE));
  const page = Math.min(filePage, pages - 1);
  const visibleFiles = files.slice(page * WORKTREE_PAGE_SIZE, (page + 1) * WORKTREE_PAGE_SIZE);
  return <aside className="file-index" aria-label="Changed files"><div className="pane-heading"><strong>Changed files</strong><span>{files.length}</span></div>{loading ? <div className="index-skeleton">{Array.from({ length: 7 }, (_, i) => <i key={i} />)}</div> : index?.error ? <Empty icon={<CircleDot size={20} />} title="Review unavailable" detail={index.error} /> : files.length === 0 ? <Empty icon={<CircleDot size={20} />} title={`No changes vs ${shortToken(base)}`} detail="Try a different base branch." /> : <><div className="file-list" role="listbox" aria-label="Changed files">{visibleFiles.map((file) => <button key={file.path} type="button" role="option" aria-selected={selectedFile?.path === file.path} className={`file-row status-${file.status.toLowerCase()} ${selectedFile?.path === file.path ? "selected" : ""}`} onClick={() => onFile(file)} onKeyDown={(event) => { const current = visibleFiles.findIndex((item) => item.path === file.path); const next = event.key === "ArrowDown" ? current + 1 : event.key === "ArrowUp" ? current - 1 : -1; if (next >= 0 && next < visibleFiles.length) { event.preventDefault(); onFile(visibleFiles[next]); (event.currentTarget.parentElement?.children[next] as HTMLElement)?.focus(); } }}><b>{file.status}</b><span>{file.path}</span></button>)}</div>{pages > 1 && <Pager label="Changed file pages" page={page} pages={pages} total={files.length} size={WORKTREE_PAGE_SIZE} onPage={onFilePage} />}</>}</aside>;
}

function PatchPane({ selectedFile, patch, patchError, patchLoading, hunkPage, onHunkPage }: { selectedFile: ChangedFile | null; patch: FilePatch | null; patchError: string; patchLoading: boolean; hunkPage: number; onHunkPage: (page: number) => void }) {
  const hunks = patch && !patch.binary ? parseHunks(patch.text) : [];
  const patchPages = paginateHunks(hunks);
  const patchPage = Math.min(hunkPage, Math.max(0, patchPages.length - 1));
  const visibleHunks = patchPages[patchPage] ?? [];
  return <section className="patch-pane" aria-label="File patch">{selectedFile && <div className="patch-heading"><code>{selectedFile.path}</code><span>{selectedFile.status}</span></div>}{patchLoading ? <div className="patch-skeleton" aria-label="Loading patch"><i /><i /><i /><i /></div> : patchError ? <Empty icon={<FileDiff size={24} />} title="Patch not rendered" detail={patchError} /> : !selectedFile ? <Empty icon={<FileDiff size={24} />} title="Select a changed file" detail="The patch is rendered one file at a time." /> : patch?.binary ? <Empty icon={<FileDiff size={24} />} title="Binary file changed" detail={selectedFile.path} /> : patch?.text === "" ? <Empty icon={<CircleDot size={24} />} title="No changes in this file" detail="The selected file has no renderable patch." /> : patchPages.length === 0 ? <pre className="patch-metadata"><code>{patch?.text}</code></pre> : <><div className="hunk-list">{visibleHunks.map((hunk, hunkIndex) => <div className="hunk" key={`${hunk.header}-${hunkIndex}`}><div className="hunk-header">{hunk.header}</div>{hunk.lines.map((line, lineIndex) => <div className={`diff-line ${line.text.startsWith("+") ? "addition" : line.text.startsWith("-") ? "deletion" : ""}`} key={`${line.oldLine}-${line.newLine}-${lineIndex}`}><span className="line-number">{line.oldLine ?? ""}</span><span className="line-number">{line.newLine ?? ""}</span><code>{line.text || " "}</code></div>)}</div>)}</div>{patchPages.length > 1 && <Pager label="Patch pages" page={patchPage} pages={patchPages.length} total={patchPages.length} size={1} pageLabel onPage={onHunkPage} />}</>}</section>;
}

function HistoryView({ history, historyRefs, index, loading, selectedFile, patch, patchError, patchLoading, filePage, hunkPage, onBack, onBasePick, onFilePage, onHunkPage, onFile }: { history: HistoryState; historyRefs: RefInventory; index: ReviewIndex | null; loading: boolean; selectedFile: ChangedFile | null; patch: FilePatch | null; patchError: string; patchLoading: boolean; filePage: number; hunkPage: number; onBack: () => void; onBasePick: (base: string) => void; onFilePage: (page: number) => void; onHunkPage: (page: number) => void; onFile: (file: ChangedFile) => void }) {
  const selected = history.selected;
  const allRefs = [...historyRefs.heads, ...historyRefs.remotes, ...historyRefs.tags];
  if (!selected) {
    return <section className="review-view" aria-label="Commit history">
      <header className="review-header">
        <div className="review-heading"><button className="back-button" type="button" onClick={onBack}><ArrowLeft size={15} /> Worktrees</button><p className="eyebrow">History</p><h1>{history.startPointLabel}</h1><div className="review-meta"><code>Select a commit in the sidebar to inspect its own changes.</code></div></div>
      </header>
    </section>;
  }
  const quickBase = selected.parents[0] ?? "empty-tree";
  const escalationDefault = historyRefs.default_base && !selected.default_base_ancestor ? historyRefs.default_base : quickBase;
  return <section className="review-view" aria-label="Commit history">
    <header className="review-header">
      <div className="review-heading"><button className="back-button" type="button" onClick={onBack}><ArrowLeft size={15} /> Worktrees</button><p className="eyebrow">History / {history.startPointLabel}</p><h1>{shortToken(selected.sha)}</h1><p className="history-subject">{selected.subject}</p><div className="review-meta"><span>{selected.author}</span><span>{selected.date.slice(0, 10)}</span>{selected.refs.map((ref) => <span key={ref} className="commit-ref">{shortToken(ref)}</span>)}</div></div>
      <div className="review-controls"><RefPicker id="history-base" label="Review base" refs={allRefs} value={escalationDefault} exclude={[selected.sha]} onChange={onBasePick} /></div>
      <div className="review-counts"><code>Quick look: this commit's own changes vs {shortToken(quickBase)}; pick a review base to open the full review.</code></div>
    </header>
    <div className="review-body">
      <FileIndexPane base={quickBase} index={index} loading={loading} selectedFile={selectedFile} filePage={filePage} onFilePage={onFilePage} onFile={onFile} />
      <PatchPane selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} hunkPage={hunkPage} onHunkPage={onHunkPage} />
    </div>
  </section>;
}

export default App;
