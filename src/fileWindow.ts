import type { ChangedFilesView } from "./settings";

// These constants pair with heights pinned in App.css (.file-row and
// .file-dir-row share the one-line height; .file-row.details-row pins the
// two-line height) and must change together: the spacer and row offsets
// are computed from them. One-line 27px is today's .file-row height
// (11px mono line + 7px padding per side); tree-view directory rows
// normalize up to it from 23px, the one accepted visual change.
export const OVERSCAN_ROWS = 8;
export const ONE_LINE_ROW_HEIGHT = 27;
export const DETAILS_ROW_HEIGHT = 37;

export function rowHeightFor(view: ChangedFilesView): number {
  return view === "details" ? DETAILS_ROW_HEIGHT : ONE_LINE_ROW_HEIGHT;
}

export function visibleRange(totalRows: number, scrollTop: number, viewportHeight: number, rowHeight: number, overscan = OVERSCAN_ROWS): { start: number; end: number } {
  const start = Math.max(0, Math.floor(scrollTop / rowHeight) - overscan);
  const visible = viewportHeight > 0 ? Math.ceil(viewportHeight / rowHeight) : 0;
  const end = Math.min(totalRows, start + visible + 2 * overscan);
  return { start, end };
}

export function totalHeight(totalRows: number, rowHeight: number): number {
  return totalRows * rowHeight;
}

export function scrollTopForRow(index: number, scrollTop: number, viewportHeight: number, rowHeight: number): number {
  const top = index * rowHeight;
  const bottom = top + rowHeight;
  if (top >= scrollTop && bottom <= scrollTop + viewportHeight) return scrollTop;
  if (top < scrollTop) return Math.max(0, top);
  return Math.max(0, bottom - viewportHeight);
}
