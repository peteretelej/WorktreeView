// Layout arithmetic for the review body's three columns. The patch pane
// owns the middle and must stay readable, so side-pane widths (persisted
// settings or live drags) yield once the window cannot fit them both.
export const MIN_PANE_WIDTH = 240;
export const MAX_PANE_WIDTH = 2000;
export const PATCH_FLOOR_PX = 380;
export const COLLAPSED_RAIL_PX = 44;
export const HANDLE_TRACK_PX = 5;

// Returns the display widths for the files and comments panes. Unmeasured
// bodies (width 0) pass through so the first paint uses the stored widths.
export function capPaneWidths(bodyWidth: number, files: number, comments: number, filesOpen: boolean, commentsOpen: boolean): [number, number] {
  if (bodyWidth <= 0) return [files, comments];
  const reserved = (filesOpen ? HANDLE_TRACK_PX : COLLAPSED_RAIL_PX) + (commentsOpen ? HANDLE_TRACK_PX : COLLAPSED_RAIL_PX) + PATCH_FLOOR_PX;
  const budget = Math.max(0, bodyWidth - reserved);
  const cap = (width: number) => Math.max(MIN_PANE_WIDTH, Math.min(width, budget));
  if (!filesOpen || !commentsOpen) return filesOpen ? [cap(files), comments] : [files, cap(comments)];
  const total = files + comments;
  if (total <= budget) return [files, comments];
  const scale = budget / total;
  return [cap(files * scale), cap(comments * scale)];
}
