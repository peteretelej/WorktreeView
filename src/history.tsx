import { MessageSquare } from "lucide-react";
import { CommentStream, type CommentsApi } from "./comments.tsx";
import { useCommitSummary, CommitRow, reviewBodyClass, FileIndexPane, PatchPane, PaneRail, RefPicker, useAnchorJump, type DiffPreferences, type FileContent, type FilePatch, type ReviewIndex } from "./review";
import { compactAge, shortAuthor, shortToken } from "./format";
import type { ChangedFile, CommitInfo, RefInventory } from "./navigation";
import type { ChangedFilesView } from "./settings";

export type CommitPage = { commits: CommitInfo[]; has_more: boolean };
export type HistoryEntry = { repoPath: string; worktreePath?: string; startRef?: string; startPointLabel: string };
export type HistoryState = HistoryEntry & { commits: CommitInfo[]; hasMore: boolean; loading: boolean; error: string };


// A pasted commit hash inserts, never searches: the picker keeps listing refs
// only, and a hex-shaped query adds one direct row that resolves to the
// commit's full SHA.
export function HistoryView({ history, historyRefs, index, loading, selectedCommit, selectedFile, patch, patchError, patchLoading, fileView, diffPrefs, diffToggles, comments, content, contentLoading, contentError, imageSrc, imageError, imageLoading, onEnsureContent, onBack, onBasePick, onFileView, onFile, panes, onPaneVisibility, branchAction, commentsWide, onCommentsWide }: { history: HistoryState; historyRefs: RefInventory; index: ReviewIndex | null; loading: boolean; selectedCommit: CommitInfo | null; selectedFile: ChangedFile | null; patch: FilePatch | null; patchError: string; patchLoading: boolean; fileView: ChangedFilesView; diffPrefs: DiffPreferences; diffToggles: React.ReactNode; comments: CommentsApi; content: FileContent | null; contentLoading: boolean; contentError: string; imageSrc: string | null; imageError: string; imageLoading: boolean; onEnsureContent: () => void; onBack: () => void; onBasePick: (base: string) => void; onFileView: (view: ChangedFilesView) => void; onFile: (file: ChangedFile) => void; panes: { files: boolean; comments: boolean }; onPaneVisibility: (next: { files: boolean; comments: boolean }) => void; branchAction?: { ref: string; busy: boolean; run: () => void } | null; commentsWide: boolean; onCommentsWide: (wide: boolean) => void }) {
  const selected = selectedCommit;
  const allRefs = [...historyRefs.heads, ...historyRefs.remotes, ...historyRefs.tags];
  const summary = useCommitSummary(history.repoPath, selected?.sha ?? null);
  // The quick look's diff is never reversed; stream anchors jump like the
  // review surface's.
  const files = index?.files ?? [];
  const { anchorJump, openCommentAnchor } = useAnchorJump(false, selectedFile, files, onFile, comments);
  if (!selected) {
    return <section className="review-view" aria-label="Commit history">
      <header className="review-header">
        <div className="review-bar"><button className="crumb-link" type="button" onClick={onBack} title="Open the project overview">Overview</button><span className="crumb-sep">/</span><span className="crumb-leaf">{history.startPointLabel}</span><span className="bar-grow" /><span className="review-counts"><code>Select a commit in the sidebar to inspect its own changes.</code></span></div>
      </header>
    </section>;
  }
  const quickBase = selected.parents[0] ?? "empty-tree";
  const escalationDefault = historyRefs.default_base && !selected.default_base_ancestor ? historyRefs.default_base : quickBase;
  return <section className="review-view" aria-label="Commit history">
    <header className="review-header">
      <div className="review-bar"><button className="crumb-link" type="button" onClick={onBack} title="Open the project overview">Overview</button><span className="crumb-sep">/</span><span className="crumb-leaf" title={history.startPointLabel}>{history.startPointLabel}</span><span className="bar-grow" />{comments.key && <button className="secondary-button" type="button" aria-label="Comment on review" title="Comment on review" onClick={() => comments.openComposer("review")}><MessageSquare size={13} /> Review</button>}</div>
      <CommitRow key={selected.sha} title={summary?.subject ?? selected.subject}>
        <div className="sha-row"><span title={selected.author}>{shortAuthor(selected.author)}</span><span>{compactAge(selected.date)}</span>{selected.refs.map((ref) => <code key={ref} title={ref}>{shortToken(ref)}</code>)}</div>
        {summary?.body && <p className="commit-body">{summary.body}</p>}
      </CommitRow>
      <div className="review-subbar"><span className="review-counts"><code>Quick look: this commit's own changes vs {shortToken(quickBase)}; pick a base to open the full review.</code></span><span className="bar-grow" /><RefPicker id="history-base" label="Base" repoPath={history.repoPath} refs={allRefs} value={escalationDefault} exclude={[selected.sha]} onChange={onBasePick} />{diffToggles}</div>
    </header>
    <div className={reviewBodyClass(panes, commentsWide)}>
      {panes.files ? <FileIndexPane base={quickBase} index={index} loading={loading} selectedFile={selectedFile} view={fileView} onView={onFileView} onFile={onFile} onCollapse={() => onPaneVisibility({ files: false, comments: panes.comments })} branchAction={branchAction} /> : <PaneRail side="left" label="Changed files" onOpen={() => onPaneVisibility({ files: true, comments: panes.comments })} />}
      <PatchPane selectedFile={selectedFile} patch={patch} patchError={patchError} patchLoading={patchLoading} diffPrefs={diffPrefs} reversed={false} comments={comments} onDiskWorktree={history.worktreePath ?? history.repoPath} content={content} contentLoading={contentLoading} contentError={contentError} imageSrc={imageSrc} imageError={imageError} imageLoading={imageLoading} onEnsureContent={onEnsureContent} anchorJump={anchorJump} />
      {panes.comments ? <CommentStream comments={comments} onCollapse={() => onPaneVisibility({ files: panes.files, comments: false })} wide={commentsWide} onToggleWide={() => onCommentsWide(!commentsWide)} onOpenAnchor={openCommentAnchor} /> : <PaneRail side="right" label="Comments" onOpen={() => onPaneVisibility({ files: panes.files, comments: true })} />}
    </div>
  </section>;
}

