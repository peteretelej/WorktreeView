import { useEffect, useState } from "react";
import {
  AlertTriangle,
  Check,
  ChevronDown,
  ChevronRight,
  ChevronsLeft,
  CircleDot,
  Clock3,
  Command,
  FileDiff,
  FolderGit2,
  GitBranch,
  HardDrive,
  Inbox,
  Search,
  Settings,
  X,
} from "lucide-react";
import "./App.css";

type ReviewState = "Needs review" | "Changed" | "In progress" | "Reviewed";

type Worktree = {
  id: string;
  branch: string;
  path: string;
  summary: string;
  state: ReviewState;
  files: number;
  additions: number;
  deletions: number;
  activity: string;
};

type Project = {
  id: string;
  owner: string;
  name: string;
  path: string;
  worktrees: Worktree[];
};

const projects: Project[] = [
  {
    id: "tree",
    owner: "peteretelej",
    name: "tree",
    path: "~/code/peteretelej/tree",
    worktrees: [
      {
        id: "highlight-pattern",
        branch: "feature/highlight-pattern",
        path: "~/code/peteretelej/tree/wt/highlight-pattern",
        summary: "Highlight entries that match a pattern",
        state: "Needs review",
        files: 2,
        additions: 91,
        deletions: 28,
        activity: "12m ago",
      },
      {
        id: "gitignore",
        branch: "feat/gitignore",
        path: "~/code/peteretelej/tree/wt/gitignore",
        summary: "Respect .gitignore during traversal",
        state: "Changed",
        files: 9,
        additions: 1036,
        deletions: 4,
        activity: "35m ago",
      },
      {
        id: "matchdirs-fix",
        branch: "fix/matchdirs-children",
        path: "~/code/peteretelej/tree/wt/matchdirs-fix",
        summary: "Keep matching children when filtering directories",
        state: "Reviewed",
        files: 3,
        additions: 538,
        deletions: 264,
        activity: "45m ago",
      },
    ],
  },
  {
    id: "gitcompare",
    owner: "peteretelej",
    name: "gitcompare",
    path: "~/code/peteretelej/gitcompare",
    worktrees: [
      {
        id: "main",
        branch: "main",
        path: "~/code/peteretelej/gitcompare",
        summary: "Bootstrap Tauri desktop shell",
        state: "In progress",
        files: 12,
        additions: 684,
        deletions: 0,
        activity: "now",
      },
    ],
  },
  {
    id: "largefile",
    owner: "peteretelej",
    name: "largefile",
    path: "~/code/peteretelej/largefile",
    worktrees: [],
  },
];

const stateClass: Record<ReviewState, string> = {
  "Needs review": "danger",
  Changed: "warn",
  "In progress": "info",
  Reviewed: "ok",
};

function App() {
  const [activeProjectId, setActiveProjectId] = useState(projects[0].id);
  const [selectedWorktreeId, setSelectedWorktreeId] = useState(
    projects[0].worktrees[0]?.id ?? "",
  );
  const [collapsed, setCollapsed] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [query, setQuery] = useState("");

  const activeProject =
    projects.find((project) => project.id === activeProjectId) ?? projects[0];
  const selectedWorktree = activeProject.worktrees.find(
    (worktree) => worktree.id === selectedWorktreeId,
  );

  useEffect(() => {
    function handleKeyboard(event: KeyboardEvent) {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen((open) => !open);
      }
      if (event.key === "Escape") setPaletteOpen(false);
    }

    window.addEventListener("keydown", handleKeyboard);
    return () => window.removeEventListener("keydown", handleKeyboard);
  }, []);

  function selectProject(project: Project) {
    setActiveProjectId(project.id);
    setSelectedWorktreeId(project.worktrees[0]?.id ?? "");
  }

  const matchingProjects = projects.filter((project) => {
    const search = query.trim().toLowerCase();
    return (
      !search ||
      `${project.owner}/${project.name}`.toLowerCase().includes(search) ||
      project.worktrees.some((worktree) =>
        `${worktree.branch} ${worktree.summary}`.toLowerCase().includes(search),
      )
    );
  });

  const attentionCount = projects.reduce(
    (total, project) =>
      total +
      project.worktrees.filter(
        (worktree) => worktree.state === "Needs review" || worktree.state === "Changed",
      ).length,
    0,
  );

  return (
    <div className={`app-shell ${collapsed ? "nav-collapsed" : ""}`}>
      <aside className="sidebar">
        <div className="brand-row">
          <span className="brand-mark">gc</span>
          <span className="brand-name">GitCompare</span>
          <button
            className="icon-button collapse-button"
            type="button"
            aria-label={collapsed ? "Expand project navigator" : "Collapse project navigator"}
            onClick={() => setCollapsed((value) => !value)}
          >
            {collapsed ? <ChevronRight size={16} /> : <ChevronsLeft size={16} />}
          </button>
        </div>

        <div className="nav-tabs" aria-label="Workspace views">
          <button className="nav-tab active" type="button">
            <FolderGit2 size={15} />
            <span>Projects</span>
          </button>
          <button className="nav-tab" type="button">
            <Inbox size={15} />
            <span>Attention</span>
            <span className="nav-count">{attentionCount}</span>
          </button>
        </div>

        <button className="search-trigger" type="button" onClick={() => setPaletteOpen(true)}>
          <Search size={14} />
          <span>Find projects and refs</span>
          <kbd>Ctrl K</kbd>
        </button>

        <nav className="project-list" aria-label="Projects">
          <div className="nav-section-label">Pinned</div>
          {projects.map((project) => {
            const projectAttention = project.worktrees.filter(
              (worktree) =>
                worktree.state === "Needs review" || worktree.state === "Changed",
            ).length;
            const active = project.id === activeProject.id;

            return (
              <button
                className={`project-row ${active ? "active" : ""}`}
                key={project.id}
                type="button"
                title={`${project.owner}/${project.name}`}
                onClick={() => selectProject(project)}
              >
                {active ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                <span className="project-avatar">{project.name.slice(0, 2)}</span>
                <span className="project-copy">
                  <strong>{project.name}</strong>
                  <small>{project.owner}</small>
                </span>
                {projectAttention > 0 && <span className="attention-dot">{projectAttention}</span>}
              </button>
            );
          })}
        </nav>

        <div className="sidebar-footer">
          <HardDrive size={13} />
          <span>read-only / local</span>
        </div>
      </aside>

      <section className="workspace">
        <header className="topbar">
          <div className="breadcrumb">
            <span>{activeProject.owner}</span>
            <span>/</span>
            <strong>{activeProject.name}</strong>
          </div>
          <div className="topbar-actions">
            <span className="safety-label"><Check size={13} /> Read-only</span>
            <button className="search-button" type="button" onClick={() => setPaletteOpen(true)}>
              <Command size={14} /> <span>Search</span> <kbd>Ctrl K</kbd>
            </button>
            <button className="icon-button" type="button" aria-label="Open settings">
              <Settings size={16} />
            </button>
          </div>
        </header>

        <main className="content">
          <section className="inbox-pane" aria-labelledby="inbox-heading">
            <div className="section-heading">
              <div>
                <p className="eyebrow">Review inbox</p>
                <h1 id="inbox-heading">Worktrees</h1>
              </div>
              <span className="path-label">{activeProject.path}</span>
            </div>

            <div className="table-header" aria-hidden="true">
              <span>Branch</span>
              <span>State</span>
              <span>Changes</span>
              <span>Activity</span>
            </div>

            <div className="worktree-list">
              {activeProject.worktrees.length === 0 ? (
                <div className="empty-state">
                  <CircleDot size={24} />
                  <strong>No linked worktrees</strong>
                  <span>This repository has no reviewable worktrees yet.</span>
                </div>
              ) : (
                activeProject.worktrees.map((worktree) => (
                  <button
                    className={`worktree-row ${
                      worktree.id === selectedWorktreeId ? "selected" : ""
                    }`}
                    key={worktree.id}
                    type="button"
                    onClick={() => setSelectedWorktreeId(worktree.id)}
                  >
                    <span className="branch-cell">
                      <span className="branch-title">
                        <GitBranch size={14} />
                        <strong>{worktree.branch}</strong>
                      </span>
                      <small>{worktree.summary}</small>
                    </span>
                    <span className={`state-pill ${stateClass[worktree.state]}`}>
                      {worktree.state === "Needs review" && <AlertTriangle size={12} />}
                      {worktree.state}
                    </span>
                    <span className="change-cell">
                      <strong>{worktree.files}</strong> files
                      <small>
                        <span className="additions">+{worktree.additions}</span>
                        <span className="deletions">-{worktree.deletions}</span>
                      </small>
                    </span>
                    <span className="activity-cell">
                      <Clock3 size={13} /> {worktree.activity}
                    </span>
                  </button>
                ))
              )}
            </div>
          </section>

          <aside className="detail-pane" aria-label="Selected worktree details">
            {selectedWorktree ? (
              <>
                <div className="detail-heading">
                  <div className="detail-icon"><FileDiff size={18} /></div>
                  <div>
                    <p className="eyebrow">Selected worktree</p>
                    <h2>{selectedWorktree.branch}</h2>
                  </div>
                </div>
                <p className="detail-summary">{selectedWorktree.summary}</p>
                <dl className="detail-list">
                  <div><dt>Review state</dt><dd>{selectedWorktree.state}</dd></div>
                  <div><dt>Files changed</dt><dd>{selectedWorktree.files}</dd></div>
                  <div><dt>Insertions</dt><dd className="additions">+{selectedWorktree.additions}</dd></div>
                  <div><dt>Deletions</dt><dd className="deletions">-{selectedWorktree.deletions}</dd></div>
                </dl>
                <div className="detail-path">
                  <span>Worktree path</span>
                  <code>{selectedWorktree.path}</code>
                </div>
                <button className="primary-button" type="button">
                  Open review <ChevronRight size={15} />
                </button>
                <p className="bootstrap-note">
                  This bootstrap uses local fixture data. Git inspection and review persistence
                  will enter through typed Tauri commands.
                </p>
              </>
            ) : (
              <div className="empty-state detail-empty">
                <FileDiff size={24} />
                <strong>Select a worktree</strong>
                <span>Review details will appear here.</span>
              </div>
            )}
          </aside>
        </main>
      </section>

      {paletteOpen && (
        <div className="palette-backdrop" role="presentation" onMouseDown={() => setPaletteOpen(false)}>
          <div
            className="palette"
            role="dialog"
            aria-modal="true"
            aria-label="Find projects and refs"
            onMouseDown={(event) => event.stopPropagation()}
          >
            <div className="palette-input-row">
              <Search size={17} />
              <input
                autoFocus
                value={query}
                onChange={(event) => setQuery(event.currentTarget.value)}
                placeholder="Find projects, branches, and worktrees"
              />
              <button type="button" aria-label="Close search" onClick={() => setPaletteOpen(false)}>
                <X size={16} />
              </button>
            </div>
            <div className="palette-results">
              <div className="nav-section-label">Projects</div>
              {matchingProjects.map((project) => (
                <button
                  key={project.id}
                  type="button"
                  onClick={() => {
                    selectProject(project);
                    setPaletteOpen(false);
                    setQuery("");
                  }}
                >
                  <FolderGit2 size={16} />
                  <span><strong>{project.name}</strong><small>{project.owner}</small></span>
                  <kbd>Enter</kbd>
                </button>
              ))}
              {matchingProjects.length === 0 && <p>No projects match "{query}".</p>}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

export default App;
