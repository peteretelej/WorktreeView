import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { ArrowLeft, ChevronDown, ChevronRight, ChevronsLeft, CircleDot, Command, FileDiff, FolderGit2, GitBranch, HardDrive, Inbox, Search, Settings, X } from "lucide-react";
import "./App.css";

type Worktree = { path: string; branch: string; head: string };
type Repo = { path: string; name: string; worktrees: Worktree[] };
type CommandError = { code: string; message: string };
type SearchResult = { repo: Repo; worktree?: Worktree };
type BranchInventory = { branches: string[]; default_base: string | null };
type ChangedFile = { path: string; status: string; untracked: boolean };
type ReviewIndex = { files: ChangedFile[]; additions: number; deletions: number; error?: string };
type FilePatch = { binary: boolean; text: string };
type ReviewScope = "all" | "committed";
type ReviewIdentity = { worktreePath: string; base: string; scope: ReviewScope; reversed: boolean };
type RenderedHunk = { header: string; lines: string[]; lineOffset: number };

const WORKTREE_PAGE_SIZE = 100;
const SEARCH_PAGE_SIZE = 50;
const HUNKS_PAGE_SIZE = 50;
const BRANCH_PAGE_SIZE = 50;
const PATCH_LINE_PAGE_SIZE = 500;

function errorMessage(error: unknown) {
  if (typeof error === "object" && error !== null && "code" in error) {
    const commandError = error as CommandError;
    if (commandError.code === "not_git_repository" || commandError.code === "invalid_path") return commandError.message;
    if (commandError.code === "persistence") return "Repository storage is unavailable.";
    if (commandError.code === "git_timeout") return "Git took too long to respond.";
    if (commandError.code === "git_output_too_large") return "Git returned too much worktree data.";
    if (commandError.code === "git_output_malformed") return "Git returned malformed worktree data.";
    if (commandError.code === "git_filter_unsupported") return "This review cannot run because Git conversion filters are configured.";
    if (commandError.code === "git_execution") return "Git could not inspect this repository.";
  }
  if (typeof error === "object" && error !== null && "message" in error) return String(error.message);
  return "The repository operation failed.";
}

function shortToken(token: string) { return /^[0-9a-f]{40}$/i.test(token) ? token.slice(0, 7) : token; }
function parseHunks(text: string) {
  const lines = text.split("\n");
  const hunks: string[][] = [];
  let current: string[] | null = null;
  for (const line of lines) {
    if (line.startsWith("@@")) { current = [line]; hunks.push(current); }
    else if (current) current.push(line);
  }
  return hunks;
}

function paginateHunks(hunks: string[][]) {
  const pages: RenderedHunk[][] = [];
  let page: RenderedHunk[] = [];
  let lineCount = 0;
  const finishPage = () => {
    if (page.length > 0) pages.push(page);
    page = [];
    lineCount = 0;
  };
  for (const hunk of hunks) {
    const lines = hunk.slice(1);
    if (lines.length === 0) {
      if (page.length === HUNKS_PAGE_SIZE) finishPage();
      page.push({ header: hunk[0], lines: [], lineOffset: 0 });
      continue;
    }
    for (let offset = 0; offset < lines.length;) {
      if (page.length === HUNKS_PAGE_SIZE || lineCount === PATCH_LINE_PAGE_SIZE) finishPage();
      const count = Math.min(lines.length - offset, PATCH_LINE_PAGE_SIZE - lineCount);
      page.push({ header: hunk[0], lines: lines.slice(offset, offset + count), lineOffset: offset });
      offset += count;
      lineCount += count;
    }
  }
  finishPage();
  return pages;
}

function sameReview(left: ReviewIdentity | null, right: ReviewIdentity) {
  return left?.worktreePath === right.worktreePath
    && left.base === right.base
    && left.scope === right.scope
    && left.reversed === right.reversed;
}

function App() {
  const [repos, setRepos] = useState<Repo[]>([]); const [activeRepoPath, setActiveRepoPath] = useState(""); const [selectedWorktreePath, setSelectedWorktreePath] = useState("");
  const [loading, setLoading] = useState(true); const [opening, setOpening] = useState(false); const [loadError, setLoadError] = useState(""); const [operationError, setOperationError] = useState(""); const [repoErrors, setRepoErrors] = useState<Record<string, string>>({}); const [hydratingRepos, setHydratingRepos] = useState<Record<string, number>>({});
  const [collapsed, setCollapsed] = useState(false); const [paletteOpen, setPaletteOpen] = useState(false); const [query, setQuery] = useState(""); const [worktreePage, setWorktreePage] = useState(0); const [searchPage, setSearchPage] = useState(0);
  const [reviewWorktree, setReviewWorktree] = useState<Worktree | null>(null); const [branches, setBranches] = useState<string[]>([]); const [base, setBase] = useState(""); const [scope, setScope] = useState<ReviewScope>("all"); const [reversed, setReversed] = useState(false); const [reviewIndex, setReviewIndex] = useState<ReviewIndex | null>(null); const [reviewLoading, setReviewLoading] = useState(false); const [selectedFile, setSelectedFile] = useState<ChangedFile | null>(null); const [patch, setPatch] = useState<FilePatch | null>(null); const [patchError, setPatchError] = useState(""); const [patchLoading, setPatchLoading] = useState(false); const [filePage, setFilePage] = useState(0); const [hunkPage, setHunkPage] = useState(0);
  const paletteRef = useRef<HTMLDialogElement>(null); const paletteOpenerRef = useRef<HTMLElement | null>(null);
  const indexGenerationRef = useRef(0); const patchGenerationRef = useRef(0); const reviewIdentityRef = useRef<ReviewIdentity | null>(null); const patchIdentityRef = useRef("");
  const activeRepo = repos.find((repo) => repo.path === activeRepoPath); const hydrating = Object.keys(hydratingRepos).length > 0; const activeRepoHydrating = activeRepo ? Boolean(hydratingRepos[activeRepo.path]) : false;

  useEffect(() => { let mounted = true; async function load() { try { const loaded = await invoke<Repo[]>("list_repos"); if (!mounted) return; setRepos(loaded); setActiveRepoPath((current) => loaded.some((repo) => repo.path === current) ? current : loaded[0]?.path ?? ""); setHydratingRepos(Object.fromEntries(loaded.map((repo) => [repo.path, 1]))); setLoading(false); for (let start = 0; start < loaded.length; start += 4) await Promise.all(loaded.slice(start, start + 4).map(async (repo) => { try { const worktrees = await invoke<Worktree[]>("list_worktrees", { path: repo.path }); if (mounted) setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, worktrees } : item)); } catch (error) { if (mounted) setRepoErrors((current) => ({ ...current, [repo.path]: errorMessage(error) })); } finally { if (mounted) setHydratingRepos((current) => { const count = current[repo.path] ?? 0; if (count > 1) return { ...current, [repo.path]: count - 1 }; const next = { ...current }; delete next[repo.path]; return next; }); } })); } catch (error) { if (mounted) { setLoadError(errorMessage(error)); setLoading(false); } } } void load(); return () => { mounted = false; }; }, []);
  useEffect(() => { if (!activeRepo) { setSelectedWorktreePath(""); return; } setSelectedWorktreePath((current) => activeRepo.worktrees.some((worktree) => worktree.path === current) ? current : activeRepo.worktrees[0]?.path ?? ""); }, [activeRepo]);
  useEffect(() => { setWorktreePage(0); }, [activeRepoPath]);
  useEffect(() => { function handleKeyboard(event: KeyboardEvent) { if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") { event.preventDefault(); paletteRef.current?.open ? closePalette() : openPalette(); } } window.addEventListener("keydown", handleKeyboard); return () => window.removeEventListener("keydown", handleKeyboard); }, []);
  useEffect(() => { const dialog = paletteRef.current; if (paletteOpen && dialog && !dialog.open) dialog.showModal(); }, [paletteOpen]);
  useEffect(() => { if (reviewWorktree) return; ++indexGenerationRef.current; ++patchGenerationRef.current; reviewIdentityRef.current = null; patchIdentityRef.current = ""; }, [reviewWorktree]);

  function openPalette(opener?: HTMLElement | null) { paletteOpenerRef.current = opener ?? (document.activeElement instanceof HTMLElement ? document.activeElement : null); setPaletteOpen(true); }
  function closePalette() { if (paletteRef.current?.open) paletteRef.current.close(); else handlePaletteClosed(); }
  function handlePaletteClosed() { setPaletteOpen(false); setQuery(""); setSearchPage(0); paletteOpenerRef.current?.focus(); paletteOpenerRef.current = null; }
  function handlePaletteKeyDown(event: ReactKeyboardEvent<HTMLDialogElement>) { if (event.key !== "Tab") return; const focusable = Array.from(event.currentTarget.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])')); const first = focusable[0], last = focusable[focusable.length - 1]; if (!first || !last) return; if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); } else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); } }
  async function openRepository() { if (opening) return; let selected: string | null; try { selected = import.meta.env.MODE === "e2e" && import.meta.env.VITE_E2E_PICKER_PATH === "/tmp/worktreeview-e2e-selection" ? "/tmp/worktreeview-e2e-selection" : await open({ directory: true, multiple: false }); } catch (error) { setOperationError(errorMessage(error)); return; } if (!selected || Array.isArray(selected)) return; setOpening(true); setOperationError(""); try { const repo = await invoke<Repo>("open_repo", { path: selected }); setRepos((current) => [repo, ...current.filter((item) => item.path !== repo.path)]); setActiveRepoPath(repo.path); setSelectedWorktreePath(""); setRepoErrors((current) => { const next = { ...current }; delete next[repo.path]; return next; }); setHydratingRepos((current) => ({ ...current, [repo.path]: (current[repo.path] ?? 0) + 1 })); try { const worktrees = await invoke<Worktree[]>("list_worktrees", { path: repo.path }); setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, worktrees } : item)); } catch (error) { setRepoErrors((current) => ({ ...current, [repo.path]: errorMessage(error) })); } finally { setHydratingRepos((current) => { const count = current[repo.path] ?? 0; if (count > 1) return { ...current, [repo.path]: count - 1 }; const next = { ...current }; delete next[repo.path]; return next; }); } } catch (error) { setOperationError(errorMessage(error)); } finally { setOpening(false); } }

  async function openReview(worktree: Worktree) {
    const identity = { worktreePath: worktree.path, base: "", scope, reversed };
    const generation = ++indexGenerationRef.current;
    ++patchGenerationRef.current;
    reviewIdentityRef.current = identity;
    patchIdentityRef.current = "";
    setReviewWorktree(worktree); setBranches([]); setBase(""); setReviewIndex(null); setSelectedFile(null); setPatch(null); setPatchError(""); setOperationError(""); setReviewLoading(true); setPatchLoading(false);
    try {
      const inventory = await invoke<BranchInventory>("list_branches", { path: worktree.path, worktreeBranch: worktree.branch });
      if (generation !== indexGenerationRef.current || !sameReview(reviewIdentityRef.current, identity)) return;
      setBranches(inventory.branches);
      if (!inventory.default_base) return;
      setBase(inventory.default_base);
      await fetchIndex(worktree, inventory.default_base, scope, reversed);
    } catch (error) {
      if (generation === indexGenerationRef.current && sameReview(reviewIdentityRef.current, identity)) {
        setReviewIndex({ files: [], additions: 0, deletions: 0, error: errorMessage(error) });
      }
    } finally {
      if (generation === indexGenerationRef.current && sameReview(reviewIdentityRef.current, identity)) setReviewLoading(false);
    }
  }
  async function fetchIndex(worktree: Worktree, nextBase: string, nextScope: ReviewScope, nextReversed: boolean) {
    const identity = { worktreePath: worktree.path, base: nextBase, scope: nextScope, reversed: nextReversed };
    const generation = ++indexGenerationRef.current;
    ++patchGenerationRef.current;
    reviewIdentityRef.current = identity;
    patchIdentityRef.current = "";
    setReviewLoading(true); setReviewIndex(null); setSelectedFile(null); setPatch(null); setPatchError(""); setPatchLoading(false); setFilePage(0);
    try {
      const index = await invoke<ReviewIndex>("list_review_changes", { path: worktree.path, base: nextBase, committedOnly: nextScope === "committed", reversed: nextReversed });
      if (generation === indexGenerationRef.current && sameReview(reviewIdentityRef.current, identity)) setReviewIndex(index);
    } catch (error) {
      if (generation === indexGenerationRef.current && sameReview(reviewIdentityRef.current, identity)) {
        setReviewIndex({ files: [], additions: 0, deletions: 0, error: errorMessage(error) });
      }
    } finally {
      if (generation === indexGenerationRef.current && sameReview(reviewIdentityRef.current, identity)) setReviewLoading(false);
    }
  }
  function changeReviewSetting(nextBase: string, nextScope = scope, nextReversed = reversed) { if (!reviewWorktree || !nextBase) return; setBase(nextBase); setScope(nextScope); setReversed(nextReversed); void fetchIndex(reviewWorktree, nextBase, nextScope, nextReversed); }
  async function selectFile(file: ChangedFile) {
    if (!reviewWorktree || !base) return;
    const identity = { worktreePath: reviewWorktree.path, base, scope, reversed };
    const patchIdentity = `${reviewWorktree.path}\0${base}\0${scope}\0${reversed}\0${file.path}\0${file.untracked}`;
    const generation = ++patchGenerationRef.current;
    patchIdentityRef.current = patchIdentity;
    setSelectedFile(file); setPatch(null); setPatchError(""); setPatchLoading(true); setHunkPage(0);
    try {
      const nextPatch = await invoke<FilePatch>("read_review_patch", { path: reviewWorktree.path, base, committedOnly: scope === "committed", reversed, file: file.path, untracked: file.untracked });
      if (generation === patchGenerationRef.current && patchIdentityRef.current === patchIdentity && sameReview(reviewIdentityRef.current, identity)) setPatch(nextPatch);
    } catch (error) {
      if (generation !== patchGenerationRef.current || patchIdentityRef.current !== patchIdentity || !sameReview(reviewIdentityRef.current, identity)) return;
      const code = typeof error === "object" && error !== null && "code" in error ? String((error as CommandError).code) : "";
      setPatchError(code === "git_output_too_large" ? "This file's patch exceeds the 4 MiB output bound and was not rendered." : errorMessage(error));
    } finally {
      if (generation === patchGenerationRef.current && patchIdentityRef.current === patchIdentity && sameReview(reviewIdentityRef.current, identity)) setPatchLoading(false);
    }
  }

  const search = query.trim().toLowerCase(); const matchingResults: SearchResult[] = search ? repos.flatMap((repo) => `${repo.name} ${repo.path}`.toLowerCase().includes(search) ? [{ repo }] : repo.worktrees.filter((worktree) => `${worktree.branch} ${worktree.path} ${worktree.head}`.toLowerCase().includes(search)).map((worktree) => ({ repo, worktree }))) : repos.map((repo) => ({ repo }));
  const statusMessage = loadError || operationError || (loading ? "Loading repositories..." : hydrating ? "Loading worktrees..." : ""); const worktreePageCount = Math.max(1, Math.ceil((activeRepo?.worktrees.length ?? 0) / WORKTREE_PAGE_SIZE)); const visibleWorktreePage = Math.min(worktreePage, worktreePageCount - 1); const visibleWorktrees = activeRepo?.worktrees.slice(visibleWorktreePage * WORKTREE_PAGE_SIZE, (visibleWorktreePage + 1) * WORKTREE_PAGE_SIZE) ?? []; const searchPageCount = Math.max(1, Math.ceil(matchingResults.length / SEARCH_PAGE_SIZE)); const visibleSearchPage = Math.min(searchPage, searchPageCount - 1); const visibleSearchResults = matchingResults.slice(visibleSearchPage * SEARCH_PAGE_SIZE, (visibleSearchPage + 1) * SEARCH_PAGE_SIZE);

  return <div className={`app-shell ${collapsed ? "nav-collapsed" : ""}`} aria-busy={loading || opening || hydrating}><aside className="sidebar"><div className="brand-row"><span className="brand-mark">wv</span><span className="brand-name">WorktreeView</span><button className="icon-button collapse-button" type="button" aria-label={collapsed ? "Expand project navigator" : "Collapse project navigator"} onClick={() => setCollapsed((value) => !value)}>{collapsed ? <ChevronRight size={16} /> : <ChevronsLeft size={16} />}</button></div><div className="nav-tabs" aria-label="Workspace views"><button className="nav-tab active" type="button" aria-label="Projects" aria-current="page"><FolderGit2 size={15} /><span>Projects</span></button><button className="nav-tab" type="button" aria-label="Attention, unavailable" disabled><Inbox size={15} /><span>Attention</span></button></div><button className="search-trigger" type="button" aria-label="Find repositories and worktrees" onClick={(event) => openPalette(event.currentTarget)}><Search size={14} /><span>Find repositories and worktrees</span><kbd>Ctrl K</kbd></button><nav className="project-list" aria-label="Repositories"><div className="nav-section-label">Repositories</div>{repos.map((repo) => <button className={`project-row ${repo.path === activeRepoPath ? "active" : ""}`} key={repo.path} type="button" title={repo.path} aria-current={repo.path === activeRepoPath ? "true" : undefined} onClick={() => { setActiveRepoPath(repo.path); setReviewWorktree(null); }}>{repo.path === activeRepoPath ? <ChevronDown size={14} /> : <ChevronRight size={14} />}<span className="project-avatar">{repo.name.slice(0, 2)}</span><span className="project-copy"><strong>{repo.name}</strong><small>{repo.path}</small></span></button>)}</nav><button className="open-repository-button" type="button" aria-label="Open repository" title="Open repository" onClick={() => void openRepository()} disabled={opening}><FolderGit2 size={14} /><span>Open repository...</span></button><div className="sidebar-footer"><HardDrive size={13} /><span>read-only / local</span></div></aside><section className="workspace"><p className="visually-hidden" id="unavailable-features">Attention and Settings are unavailable.</p><div className="live-error" role="status" aria-live="polite">{statusMessage}</div><header className="topbar"><div className="breadcrumb"><span>Repositories</span><span>/</span><strong>{activeRepo?.name ?? "No repository"}</strong>{reviewWorktree && <><span>/</span><strong>{reviewWorktree.branch}</strong></>}</div><div className="topbar-actions"><span className="safety-label">Read-only</span><button className="search-button" type="button" aria-label="Find repositories and worktrees" onClick={(event) => openPalette(event.currentTarget)}><Command size={14} /> <span>Search</span> <kbd>Ctrl K</kbd></button><button className="icon-button" type="button" aria-label="Open settings, unavailable" aria-describedby="unavailable-features" disabled><Settings size={16} /></button></div></header><main className="content">{reviewWorktree ? <ReviewView worktree={reviewWorktree} branches={branches} base={base} scope={scope} reversed={reversed} index={reviewIndex} loading={reviewLoading} selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} filePage={filePage} hunkPage={hunkPage} onBack={() => setReviewWorktree(null)} onBaseChange={(value) => changeReviewSetting(value)} onScopeChange={(value) => changeReviewSetting(base, value)} onReverse={() => changeReviewSetting(base, scope, !reversed)} onFilePage={setFilePage} onHunkPage={setHunkPage} onFile={selectFile} /> : <section className="inbox-pane" aria-labelledby="inbox-heading" aria-busy={loading || activeRepoHydrating}>{operationError && <div className="operation-error">{operationError}</div>}<div className="section-heading"><div><p className="eyebrow">Repository</p><h1 id="inbox-heading">Worktrees</h1></div><span className="path-label">{activeRepo?.path}</span></div>{loading ? <Empty icon={<CircleDot size={24} />} title="Loading repositories..." detail="Reading saved repositories." /> : loadError ? <Empty icon={<CircleDot size={24} />} title="Repositories could not be loaded" detail={loadError} /> : !activeRepo ? <Empty icon={<FolderGit2 size={24} />} title="No repositories" detail="Open a local Git folder to begin." action={<button className="secondary-button" type="button" onClick={() => void openRepository()} disabled={opening}>Open repository...</button>} /> : activeRepoHydrating ? <Empty icon={<CircleDot size={24} />} title="Loading worktrees..." detail="Reading live worktree identity." /> : repoErrors[activeRepo.path] ? <Empty icon={<CircleDot size={24} />} title="Worktrees unavailable" detail={repoErrors[activeRepo.path]} /> : activeRepo.worktrees.length === 0 ? <Empty icon={<CircleDot size={24} />} title="No worktrees" detail="This repository has no linked worktrees." /> : <><div className="table-header" aria-hidden="true"><span>Branch</span><span>Path</span><span>HEAD</span></div><div className="worktree-list">{visibleWorktrees.map((worktree) => <button className={`worktree-row ${worktree.path === selectedWorktreePath ? "selected" : ""}`} key={worktree.path} type="button" aria-pressed={worktree.path === selectedWorktreePath} onClick={() => { setSelectedWorktreePath(worktree.path); void openReview(worktree); }}><span className="branch-cell"><span className="branch-title"><GitBranch size={14} /><strong>{worktree.branch}</strong></span><small>{worktree.path}</small></span><span className="worktree-path">{worktree.path}</span><code className="head-cell">{worktree.head}</code></button>)}</div>{worktreePageCount > 1 && <Pager label="Worktree pages" page={visibleWorktreePage} pages={worktreePageCount} total={activeRepo.worktrees.length} size={WORKTREE_PAGE_SIZE} onPage={setWorktreePage} />}</>}</section>}</main></section>{paletteOpen && <dialog className="palette-backdrop" ref={paletteRef} aria-label="Find repositories and worktrees" onClose={handlePaletteClosed} onKeyDown={handlePaletteKeyDown} onMouseDown={(event) => { if (event.target === event.currentTarget) closePalette(); }}><div className="palette" onMouseDown={(event) => event.stopPropagation()}><div className="palette-input-row"><Search size={17} /><input autoFocus value={query} onChange={(event) => { setQuery(event.currentTarget.value); setSearchPage(0); }} placeholder="Find repositories, branches, and worktrees" /><button type="button" aria-label="Close search" onClick={closePalette}><X size={16} /></button></div><div className="palette-results"><div className="nav-section-label">Repositories and worktrees</div>{visibleSearchResults.map((result) => <button key={`${result.repo.path}:${result.worktree?.path ?? "repo"}`} type="button" onClick={() => { setActiveRepoPath(result.repo.path); if (result.worktree) { setSelectedWorktreePath(result.worktree.path); void openReview(result.worktree); } closePalette(); }}>{result.worktree ? <GitBranch size={16} /> : <FolderGit2 size={16} />}<span><strong>{result.worktree?.branch ?? result.repo.name}</strong><small>{result.worktree?.path ?? result.repo.path}</small></span><kbd>Enter</kbd></button>)}{matchingResults.length === 0 && <p>No repositories or worktrees match "{query}".</p>}{searchPageCount > 1 && <Pager label="Search result pages" page={visibleSearchPage} pages={searchPageCount} total={matchingResults.length} size={SEARCH_PAGE_SIZE} onPage={setSearchPage} />}</div></div></dialog>}</div>;
}

function Empty({ icon, title, detail, action }: { icon: React.ReactNode; title: string; detail: string; action?: React.ReactNode }) { return <div className="empty-state">{icon}<strong>{title}</strong><span>{detail}</span>{action}</div>; }
function Pager({ label, page, pages, total, size, pageLabel = false, onPage }: { label: string; page: number; pages: number; total: number; size: number; pageLabel?: boolean; onPage: (page: number) => void }) { return <div className="page-controls" role="group" aria-label={label}><button type="button" disabled={page === 0} onClick={() => onPage(Math.max(0, page - 1))}>Previous</button><span>{pageLabel ? `Page ${page + 1} of ${pages}` : `${page * size + 1}-${Math.min((page + 1) * size, total)} of ${total}`}</span><button type="button" disabled={page === pages - 1} onClick={() => onPage(Math.min(pages - 1, page + 1))}>Next</button></div>; }

function BranchPicker({ id, label, branches, base, onChange }: { id: string; label: string; branches: string[]; base: string; onChange: (value: string) => void }) {
  const [query, setQuery] = useState("");
  const [open, setOpen] = useState(false);
  const [page, setPage] = useState(0);
  const search = query.trim().toLowerCase();
  const matches = search ? branches.filter((branch) => branch.toLowerCase().includes(search)) : branches;
  const pages = Math.max(1, Math.ceil(matches.length / BRANCH_PAGE_SIZE));
  const visiblePage = Math.min(page, pages - 1);
  const visibleBranches = matches.slice(visiblePage * BRANCH_PAGE_SIZE, (visiblePage + 1) * BRANCH_PAGE_SIZE);
  useEffect(() => { setQuery(""); setPage(0); }, [base]);
  return <div className="branch-picker"><label htmlFor={id}>{label}</label><code className="branch-selection">{base ? shortToken(base) : "No base selected"}</code><input id={id} role="combobox" aria-controls={`${id}-options`} aria-expanded={open} aria-autocomplete="list" value={query} placeholder="Search branches" onFocus={() => setOpen(true)} onChange={(event) => { setQuery(event.currentTarget.value); setPage(0); setOpen(true); }} onKeyDown={(event) => { if (event.key === "Escape") setOpen(false); }} />{open && <div className="branch-options" id={`${id}-options`} role="listbox" aria-label={`${label} branches`}>{visibleBranches.map((branch) => <button key={branch} type="button" role="option" aria-selected={branch === base} onClick={() => { onChange(branch); setOpen(false); }}>{branch}</button>)}{matches.length === 0 && <span>No matching branches</span>}{pages > 1 && <Pager label={`${label} branch pages`} page={visiblePage} pages={pages} total={matches.length} size={BRANCH_PAGE_SIZE} onPage={setPage} />}</div>}</div>;
}

function ReviewView({ worktree, branches, base, scope, reversed, index, loading, selectedFile, patch, patchError, patchLoading, filePage, hunkPage, onBack, onBaseChange, onScopeChange, onReverse, onFilePage, onHunkPage, onFile }: { worktree: Worktree; branches: string[]; base: string; scope: ReviewScope; reversed: boolean; index: ReviewIndex | null; loading: boolean; selectedFile: ChangedFile | null; patch: FilePatch | null; patchError: string; patchLoading: boolean; filePage: number; hunkPage: number; onBack: () => void; onBaseChange: (value: string) => void; onScopeChange: (value: ReviewScope) => void; onReverse: () => void; onFilePage: (page: number) => void; onHunkPage: (page: number) => void; onFile: (file: ChangedFile) => void }) {
  const files = index?.files ?? [];
  const pages = Math.max(1, Math.ceil(files.length / WORKTREE_PAGE_SIZE));
  const page = Math.min(filePage, pages - 1);
  const visibleFiles = files.slice(page * WORKTREE_PAGE_SIZE, (page + 1) * WORKTREE_PAGE_SIZE);
  const hunks = patch && !patch.binary ? parseHunks(patch.text) : [];
  const patchPages = paginateHunks(hunks);
  const patchPage = Math.min(hunkPage, Math.max(0, patchPages.length - 1));
  const visibleHunks = patchPages[patchPage] ?? [];
  return <section className="review-view" aria-label="Code review">
    <header className="review-header">
      <div className="review-heading"><button className="back-button" type="button" onClick={onBack}><ArrowLeft size={15} /> Worktrees</button><p className="eyebrow">Review</p><h1>{worktree.branch}</h1><div className="review-meta"><code>{worktree.path}</code><code>HEAD {worktree.head}</code></div></div>
      <div className="review-controls"><BranchPicker id="review-base" label="Review base" branches={branches} base={base} onChange={onBaseChange} /><button className="swap-button" type="button" aria-label="Swap review direction" title="Swap review direction" onClick={onReverse}>↔ <code>{reversed ? "HEAD" : shortToken(base)}...{reversed ? shortToken(base) : "HEAD"}</code></button><div className="scope-toggle" role="group" aria-label="Review scope"><button className={scope === "all" ? "active" : ""} type="button" onClick={() => onScopeChange("all")}>All changes</button><button className={scope === "committed" ? "active" : ""} type="button" onClick={() => onScopeChange("committed")}>Committed only</button></div></div>
      <div className="review-counts"><code>{index?.error ? "Review index unavailable" : index ? `${files.length} files, +${index.additions} -${index.deletions}` : "Loading review index..."}</code></div>
    </header>
    {!base && !loading ? <Empty icon={<GitBranch size={24} />} title="Choose a base branch to review" detail="This worktree has no merge-base default." action={<BranchPicker id="prompt-review-base" label="Choose review base" branches={branches} base={base} onChange={onBaseChange} />} /> : <div className="review-body">
      <aside className="file-index" aria-label="Changed files"><div className="pane-heading"><strong>Changed files</strong><span>{files.length}</span></div>{loading ? <div className="index-skeleton">{Array.from({ length: 7 }, (_, i) => <i key={i} />)}</div> : index?.error ? <Empty icon={<CircleDot size={20} />} title="Review unavailable" detail={index.error} /> : files.length === 0 ? <Empty icon={<CircleDot size={20} />} title={`No changes vs ${shortToken(base)}`} detail="Try a different base branch." /> : <><div className="file-list" role="listbox" aria-label="Changed files">{visibleFiles.map((file) => <button key={file.path} type="button" role="option" aria-selected={selectedFile?.path === file.path} className={`file-row status-${file.status.toLowerCase()} ${selectedFile?.path === file.path ? "selected" : ""}`} onClick={() => onFile(file)} onKeyDown={(event) => { const current = visibleFiles.findIndex((item) => item.path === file.path); if (event.key === "ArrowDown" && current < visibleFiles.length - 1) { event.preventDefault(); (event.currentTarget.nextElementSibling as HTMLElement)?.focus(); } if (event.key === "ArrowUp" && current > 0) { event.preventDefault(); (event.currentTarget.previousElementSibling as HTMLElement)?.focus(); } if (event.key === "Enter") onFile(file); }}><b>{file.status}</b><span>{file.path}</span></button>)}</div>{pages > 1 && <Pager label="Changed file pages" page={page} pages={pages} total={files.length} size={WORKTREE_PAGE_SIZE} onPage={onFilePage} />}</>}</aside>
      <section className="patch-pane" aria-label="File patch">{selectedFile && <div className="patch-heading"><code>{selectedFile.path}</code><span>{selectedFile.status}</span></div>}{patchLoading ? <div className="patch-skeleton" aria-label="Loading patch"><i /><i /><i /><i /></div> : patchError ? <Empty icon={<FileDiff size={24} />} title="Patch not rendered" detail={patchError} /> : !selectedFile ? <Empty icon={<FileDiff size={24} />} title="Select a changed file" detail="The patch is rendered one file at a time." /> : patch?.binary ? <Empty icon={<FileDiff size={24} />} title="Binary file changed" detail={selectedFile.path} /> : patch?.text === "" ? <Empty icon={<CircleDot size={24} />} title="No changes in this file" detail="The selected file has no renderable patch." /> : <><div className="hunk-list">{visibleHunks.map((hunk, hunkIndex) => <div className="hunk" key={`${hunk.header}-${hunk.lineOffset}-${hunkIndex}`}><div className="hunk-header">{hunk.header}</div>{hunk.lines.map((line, lineIndex) => <div className={`diff-line ${line.startsWith("+") && !line.startsWith("+++") ? "addition" : line.startsWith("-") && !line.startsWith("---") ? "deletion" : ""}`} key={`${hunk.lineOffset + lineIndex}-${line}`}><span className="line-number">{hunk.lineOffset + lineIndex + 1}</span><code>{line || " "}</code></div>)}</div>)}</div>{patchPages.length > 1 && <Pager label="Patch pages" page={patchPage} pages={patchPages.length} total={patchPages.length} size={1} pageLabel onPage={onHunkPage} />}</>}</section>
    </div>}
  </section>;
}

export default App;
