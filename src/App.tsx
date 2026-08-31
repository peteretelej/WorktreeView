import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { ChevronDown, ChevronRight, ChevronsLeft, CircleDot, Command, FileDiff, FolderGit2, GitBranch, HardDrive, Inbox, Search, Settings, X } from "lucide-react";
import "./App.css";

type Worktree = { path: string; branch: string; head: string };
type Repo = { path: string; name: string; worktrees: Worktree[] };
type CommandError = { code: string; message: string };
type SearchResult = { repo: Repo; worktree?: Worktree };

const WORKTREE_PAGE_SIZE = 100;
const SEARCH_PAGE_SIZE = 50;

function errorMessage(error: unknown) {
  if (typeof error === "object" && error !== null && "code" in error) {
    const commandError = error as CommandError;
    if (commandError.code === "not_git_repository" || commandError.code === "invalid_path") return commandError.message;
    if (commandError.code === "persistence") return "Repository storage is unavailable.";
    if (commandError.code === "git_timeout") return "Git took too long to respond.";
    if (commandError.code === "git_output_too_large") return "Git returned too much worktree data.";
    if (commandError.code === "git_output_malformed") return "Git returned malformed worktree data.";
    if (commandError.code === "git_execution") return "Git could not inspect this repository.";
  }
  if (typeof error === "object" && error !== null && "message" in error) return String(error.message);
  return "The repository operation failed.";
}

function App() {
  const [repos, setRepos] = useState<Repo[]>([]);
  const [activeRepoPath, setActiveRepoPath] = useState("");
  const [selectedWorktreePath, setSelectedWorktreePath] = useState("");
  const [loading, setLoading] = useState(true);
  const [opening, setOpening] = useState(false);
  const [loadError, setLoadError] = useState("");
  const [operationError, setOperationError] = useState("");
  const [repoErrors, setRepoErrors] = useState<Record<string, string>>({});
  const [hydratingRepos, setHydratingRepos] = useState<Record<string, number>>({});
  const [collapsed, setCollapsed] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [worktreePage, setWorktreePage] = useState(0);
  const [searchPage, setSearchPage] = useState(0);
  const paletteRef = useRef<HTMLDialogElement>(null);
  const paletteOpenerRef = useRef<HTMLElement | null>(null);

  const activeRepo = repos.find((repo) => repo.path === activeRepoPath);
  const selectedWorktree = activeRepo?.worktrees.find((worktree) => worktree.path === selectedWorktreePath);

  useEffect(() => {
    let mounted = true;
    async function load() {
      try {
        const loaded = await invoke<Repo[]>("list_repos");
        if (!mounted) return;
        setRepos(loaded);
        setActiveRepoPath((current) => loaded.some((repo) => repo.path === current) ? current : loaded[0]?.path ?? "");
        setHydratingRepos(Object.fromEntries(loaded.map((repo) => [repo.path, 1])));
        setLoading(false);
        for (let start = 0; start < loaded.length; start += 4) {
          await Promise.all(loaded.slice(start, start + 4).map(async (repo) => {
            try {
              const worktrees = await invoke<Worktree[]>("list_worktrees", { path: repo.path });
              if (mounted) setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, worktrees } : item));
            } catch (error) {
              if (mounted) setRepoErrors((current) => ({ ...current, [repo.path]: errorMessage(error) }));
            } finally {
              if (mounted) setHydratingRepos((current) => {
                const count = current[repo.path] ?? 0;
                if (count > 1) return { ...current, [repo.path]: count - 1 };
                const next = { ...current };
                delete next[repo.path];
                return next;
              });
            }
          }));
        }
      } catch (error) {
        if (mounted) { setLoadError(errorMessage(error)); setLoading(false); }
      }
    }
    void load();
    return () => { mounted = false; };
  }, []);

  useEffect(() => {
    if (!activeRepo) { setSelectedWorktreePath(""); return; }
    setSelectedWorktreePath((current) => activeRepo.worktrees.some((worktree) => worktree.path === current) ? current : activeRepo.worktrees[0]?.path ?? "");
  }, [activeRepo]);

  useEffect(() => { setWorktreePage(0); }, [activeRepoPath]);

  useEffect(() => {
    function handleKeyboard(event: KeyboardEvent) { if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") { event.preventDefault(); paletteRef.current?.open ? closePalette() : openPalette(); } }
    window.addEventListener("keydown", handleKeyboard); return () => window.removeEventListener("keydown", handleKeyboard);
  }, []);

  useEffect(() => { const dialog = paletteRef.current; if (paletteOpen && dialog && !dialog.open) dialog.showModal(); }, [paletteOpen]);

  function openPalette(opener?: HTMLElement | null) { paletteOpenerRef.current = opener ?? (document.activeElement instanceof HTMLElement ? document.activeElement : null); setPaletteOpen(true); }
  function closePalette() { if (paletteRef.current?.open) paletteRef.current.close(); else handlePaletteClosed(); }
  function handlePaletteClosed() { setPaletteOpen(false); setQuery(""); setSearchPage(0); paletteOpenerRef.current?.focus(); paletteOpenerRef.current = null; }
  function handlePaletteKeyDown(event: ReactKeyboardEvent<HTMLDialogElement>) {
    if (event.key !== "Tab") return;
    const focusable = Array.from(event.currentTarget.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])'));
    const first = focusable[0], last = focusable[focusable.length - 1]; if (!first || !last) return;
    if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); } else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
  }

  async function openRepository() {
    if (opening) return;
    let selected: string | null;
    try {
      selected = import.meta.env.MODE === "e2e" && import.meta.env.VITE_E2E_PICKER_PATH === "/tmp/worktreeview-e2e-selection"
        ? "/tmp/worktreeview-e2e-selection"
        : await open({ directory: true, multiple: false });
    }
    catch (error) { setOperationError(errorMessage(error)); return; }
    if (!selected || Array.isArray(selected)) return;
    setOpening(true); setOperationError("");
    try {
      const repo = await invoke<Repo>("open_repo", { path: selected });
      setRepos((current) => [repo, ...current.filter((item) => item.path !== repo.path)]);
      setActiveRepoPath(repo.path); setSelectedWorktreePath("");
      setRepoErrors((current) => { const next = { ...current }; delete next[repo.path]; return next; });
      setHydratingRepos((current) => ({ ...current, [repo.path]: (current[repo.path] ?? 0) + 1 }));
      try {
        const worktrees = await invoke<Worktree[]>("list_worktrees", { path: repo.path });
        setRepos((current) => current.map((item) => item.path === repo.path ? { ...item, worktrees } : item));
      } catch (error) { setRepoErrors((current) => ({ ...current, [repo.path]: errorMessage(error) })); }
      finally {
        setHydratingRepos((current) => {
          const count = current[repo.path] ?? 0;
          if (count > 1) return { ...current, [repo.path]: count - 1 };
          const next = { ...current }; delete next[repo.path]; return next;
        });
      }
    } catch (error) { setOperationError(errorMessage(error)); }
    finally { setOpening(false); }
  }

  const search = query.trim().toLowerCase();
  const matchingResults: SearchResult[] = search ? repos.flatMap((repo) => {
    if (`${repo.name} ${repo.path}`.toLowerCase().includes(search)) return [{ repo }];
    return repo.worktrees.filter((worktree) => `${worktree.branch} ${worktree.path} ${worktree.head}`.toLowerCase().includes(search)).map((worktree) => ({ repo, worktree }));
  }) : repos.map((repo) => ({ repo }));
  const hydrating = Object.keys(hydratingRepos).length > 0;
  const activeRepoHydrating = activeRepo ? Boolean(hydratingRepos[activeRepo.path]) : false;
  const statusMessage = loadError || operationError || (loading ? "Loading repositories..." : hydrating ? "Loading worktrees..." : "");
  const worktreePageCount = Math.max(1, Math.ceil((activeRepo?.worktrees.length ?? 0) / WORKTREE_PAGE_SIZE));
  const visibleWorktreePage = Math.min(worktreePage, worktreePageCount - 1);
  const visibleWorktrees = activeRepo?.worktrees.slice(visibleWorktreePage * WORKTREE_PAGE_SIZE, (visibleWorktreePage + 1) * WORKTREE_PAGE_SIZE) ?? [];
  const searchPageCount = Math.max(1, Math.ceil(matchingResults.length / SEARCH_PAGE_SIZE));
  const visibleSearchPage = Math.min(searchPage, searchPageCount - 1);
  const visibleSearchResults = matchingResults.slice(visibleSearchPage * SEARCH_PAGE_SIZE, (visibleSearchPage + 1) * SEARCH_PAGE_SIZE);

  return <div className={`app-shell ${collapsed ? "nav-collapsed" : ""}`} aria-busy={loading || opening || hydrating}>
    <aside className="sidebar">
      <div className="brand-row"><span className="brand-mark">wv</span><span className="brand-name">WorktreeView</span><button className="icon-button collapse-button" type="button" aria-label={collapsed ? "Expand project navigator" : "Collapse project navigator"} onClick={() => setCollapsed((value) => !value)}>{collapsed ? <ChevronRight size={16} /> : <ChevronsLeft size={16} />}</button></div>
      <div className="nav-tabs" aria-label="Workspace views"><button className="nav-tab active" type="button" aria-label="Projects" aria-current="page"><FolderGit2 size={15} /><span>Projects</span></button><button className="nav-tab" type="button" aria-label="Attention, unavailable" disabled><Inbox size={15} /><span>Attention</span></button></div>
      <button className="search-trigger" type="button" aria-label="Find repositories and worktrees" onClick={(event) => openPalette(event.currentTarget)}><Search size={14} /><span>Find repositories and worktrees</span><kbd>Ctrl K</kbd></button>
      <nav className="project-list" aria-label="Repositories"><div className="nav-section-label">Repositories</div>{repos.map((repo) => <button className={`project-row ${repo.path === activeRepoPath ? "active" : ""}`} key={repo.path} type="button" title={repo.path} aria-current={repo.path === activeRepoPath ? "true" : undefined} onClick={() => setActiveRepoPath(repo.path)}>{repo.path === activeRepoPath ? <ChevronDown size={14} /> : <ChevronRight size={14} />}<span className="project-avatar">{repo.name.slice(0, 2)}</span><span className="project-copy"><strong>{repo.name}</strong><small>{repo.path}</small></span></button>)}</nav>
      <button className="open-repository-button" type="button" aria-label="Open repository" title="Open repository" onClick={() => void openRepository()} disabled={opening}><FolderGit2 size={14} /><span>Open repository...</span></button><div className="sidebar-footer"><HardDrive size={13} /><span>read-only / local</span></div>
    </aside>
    <section className="workspace"><p className="visually-hidden" id="unavailable-features">Attention, Settings, and Open review are unavailable.</p><div className="live-error" role="status" aria-live="polite">{statusMessage}</div>
      <header className="topbar"><div className="breadcrumb"><span>Repositories</span><span>/</span><strong>{activeRepo?.name ?? "No repository"}</strong></div><div className="topbar-actions"><span className="safety-label">Read-only</span><button className="search-button" type="button" aria-label="Find repositories and worktrees" onClick={(event) => openPalette(event.currentTarget)}><Command size={14} /> <span>Search</span> <kbd>Ctrl K</kbd></button><button className="icon-button" type="button" aria-label="Open settings, unavailable" aria-describedby="unavailable-features" disabled><Settings size={16} /></button></div></header>
       <main className="content"><section className="inbox-pane" aria-labelledby="inbox-heading" aria-busy={loading || activeRepoHydrating}>{operationError && <div className="operation-error">{operationError}</div>}<div className="section-heading"><div><p className="eyebrow">Repository</p><h1 id="inbox-heading">Worktrees</h1></div><span className="path-label">{activeRepo?.path}</span></div>
         {loading ? <div className="empty-state"><CircleDot size={24} /><strong>Loading repositories...</strong><span>Reading saved repositories.</span></div> : loadError ? <div className="empty-state"><CircleDot size={24} /><strong>Repositories could not be loaded</strong><span>{loadError}</span></div> : !activeRepo ? <div className="empty-state"><FolderGit2 size={24} /><strong>No repositories</strong><span>Open a local Git folder to begin.</span><button className="secondary-button" type="button" onClick={() => void openRepository()} disabled={opening}>Open repository...</button></div> : activeRepoHydrating ? <div className="empty-state"><CircleDot size={24} /><strong>Loading worktrees...</strong><span>Reading live worktree identity.</span></div> : repoErrors[activeRepo.path] ? <div className="empty-state"><CircleDot size={24} /><strong>Worktrees unavailable</strong><span>{repoErrors[activeRepo.path]}</span></div> : activeRepo.worktrees.length === 0 ? <div className="empty-state"><CircleDot size={24} /><strong>No worktrees</strong><span>This repository has no linked worktrees.</span></div> : <><div className="table-header" aria-hidden="true"><span>Branch</span><span>Path</span><span>HEAD</span></div><div className="worktree-list">{visibleWorktrees.map((worktree) => <button className={`worktree-row ${worktree.path === selectedWorktreePath ? "selected" : ""}`} key={worktree.path} type="button" aria-pressed={worktree.path === selectedWorktreePath} onClick={() => setSelectedWorktreePath(worktree.path)}><span className="branch-cell"><span className="branch-title"><GitBranch size={14} /><strong>{worktree.branch}</strong></span><small>{worktree.path}</small></span><span className="worktree-path">{worktree.path}</span><code className="head-cell">{worktree.head}</code></button>)}</div>{worktreePageCount > 1 && <div className="page-controls" role="group" aria-label="Worktree pages"><button type="button" disabled={visibleWorktreePage === 0} onClick={() => setWorktreePage((page) => Math.max(0, page - 1))}>Previous</button><span>{visibleWorktreePage * WORKTREE_PAGE_SIZE + 1}-{Math.min((visibleWorktreePage + 1) * WORKTREE_PAGE_SIZE, activeRepo.worktrees.length)} of {activeRepo.worktrees.length}</span><button type="button" disabled={visibleWorktreePage === worktreePageCount - 1} onClick={() => setWorktreePage((page) => Math.min(worktreePageCount - 1, page + 1))}>Next</button></div>}</>}
      </section><aside className="detail-pane" aria-label="Selected worktree details" aria-busy={activeRepoHydrating}>{selectedWorktree ? <><div className="detail-heading"><div className="detail-icon"><FileDiff size={18} /></div><div><p className="eyebrow">Selected worktree</p><h2>{selectedWorktree.branch}</h2></div></div><div className="detail-path"><span>Worktree path</span><code>{selectedWorktree.path}</code></div><div className="detail-path"><span>HEAD</span><code>{selectedWorktree.head}</code></div><button className="primary-button" type="button" aria-describedby="unavailable-features" disabled>Open review <ChevronRight size={15} /></button></> : activeRepoHydrating ? <div className="empty-state detail-empty"><CircleDot size={24} /><strong>Loading worktrees...</strong><span>Worktree identity will appear when ready.</span></div> : <div className="empty-state detail-empty"><FileDiff size={24} /><strong>Select a worktree</strong><span>Worktree identity will appear here.</span></div>}</aside></main>
    </section>
    {paletteOpen && <dialog className="palette-backdrop" ref={paletteRef} aria-label="Find repositories and worktrees" onClose={handlePaletteClosed} onKeyDown={handlePaletteKeyDown} onMouseDown={(event) => { if (event.target === event.currentTarget) closePalette(); }}><div className="palette" onMouseDown={(event) => event.stopPropagation()}><div className="palette-input-row"><Search size={17} /><input autoFocus value={query} onChange={(event) => { setQuery(event.currentTarget.value); setSearchPage(0); }} placeholder="Find repositories, branches, and worktrees" /><button type="button" aria-label="Close search" onClick={closePalette}><X size={16} /></button></div><div className="palette-results"><div className="nav-section-label">Repositories and worktrees</div>{visibleSearchResults.map((result) => <button key={`${result.repo.path}:${result.worktree?.path ?? "repo"}`} type="button" onClick={() => { setActiveRepoPath(result.repo.path); if (result.worktree) setSelectedWorktreePath(result.worktree.path); closePalette(); }}>{result.worktree ? <GitBranch size={16} /> : <FolderGit2 size={16} />}<span><strong>{result.worktree?.branch ?? result.repo.name}</strong><small>{result.worktree?.path ?? result.repo.path}</small></span><kbd>Enter</kbd></button>)}{matchingResults.length === 0 && <p>No repositories or worktrees match "{query}".</p>}{searchPageCount > 1 && <div className="page-controls" role="group" aria-label="Search result pages"><button type="button" disabled={visibleSearchPage === 0} onClick={() => setSearchPage((page) => Math.max(0, page - 1))}>Previous</button><span>{visibleSearchPage * SEARCH_PAGE_SIZE + 1}-{Math.min((visibleSearchPage + 1) * SEARCH_PAGE_SIZE, matchingResults.length)} of {matchingResults.length}</span><button type="button" disabled={visibleSearchPage === searchPageCount - 1} onClick={() => setSearchPage((page) => Math.min(searchPageCount - 1, page + 1))}>Next</button></div>}</div></div></dialog>}
  </div>;
}

export default App;
