import { pairHunkLines, type DiffLine, type HunkLike, type PatchGap } from "./diff.ts";

// Presentation stream for the patch pane: one flat row list per file
// (hunk headers, patch or file lines, expand controls), plus the height
// bookkeeping that lets a scroll window mount only visible rows. Both
// patch and full-file views render from this module.

// Gaps at or under this many hidden lines collapse to a slim one-line
// control; taller gaps keep the regular control.
export const SLIM_GAP_MAX_LINES = 3;

export type HeaderRow = { kind: "header"; header: string; hunkIndex: number };
export type LineRow = { kind: "line"; line: DiffLine; hunkIndex: number };
export type SplitRow = { kind: "split"; old: DiffLine | null; next: DiffLine | null; hunkIndex: number };
export type GapRow = { kind: "gap"; gap: PatchGap; slim: boolean };
export type RowSpec = HeaderRow | LineRow | SplitRow | GapRow;

// Flattens a patch into one continuous row stream. `expandedHunks` is the
// patch after hunksWithExpandedGaps, so expanded gap lines already sit in
// their host hunks; unexpanded gaps render inline (a leading gap ahead of
// the first header, between/trailing gaps after their host hunk).
export function buildPatchRows(expandedHunks: HunkLike[], gaps: PatchGap[], expanded: ReadonlySet<string>, contentLines: string[] | null, split: boolean): RowSpec[] {
  const rows: RowSpec[] = [];
  expandedHunks.forEach((hunk, index) => {
    const leading = index === 0 ? gaps.find((gap) => gap.before) : undefined;
    if (leading && (!expanded.has(leading.id) || contentLines === null)) rows.push({ kind: "gap", gap: leading, slim: leading.lines <= SLIM_GAP_MAX_LINES });
    rows.push({ kind: "header", header: hunk.header, hunkIndex: index });
    if (split) {
      for (const pair of pairHunkLines(hunk.lines)) rows.push({ kind: "split", old: pair.old, next: pair.new, hunkIndex: index });
    } else {
      for (const line of hunk.lines) rows.push({ kind: "line", line, hunkIndex: index });
    }
    for (const gap of gaps) {
      if (gap.before || gap.hostHunk !== index) continue;
      if (expanded.has(gap.id) && contentLines !== null) continue;
      rows.push({ kind: "gap", gap, slim: gap.lines <= SLIM_GAP_MAX_LINES });
    }
  });
  return rows;
}

// The full-file view as a stream of new-side line rows, so both pane modes
// share the windowed renderer. Lines carry a context prefix so token
// lookup rides the same path as patch lines.
export function buildFileRows(contentLines: string[]): RowSpec[] {
  return contentLines.map((text, index) => ({ kind: "line" as const, line: { text: ` ${text}`, oldLine: null, newLine: index + 1 }, hunkIndex: 0 }));
}

// Heights of the rows mounted in a window are measured after paint;
// unmounted rows ride the default estimate. Fixed-height rows (the
// default wrap-off case) never measure, so their model is exact from the
// first frame. Offsets come from a prefix-sum array rebuilt lazily: rows
// mutate rarely (measure, reset, resize) and offsets are queried on every
// scroll frame.
export type HeightModel = {
  count: number;
  /** Records a measured height; returns true when it changed the model. */
  measure(index: number, height: number): boolean;
  reset(): void;
  offset(index: number): number;
  total(): number;
  indexAt(offset: number): number;
};

export function createHeightModel(count: number, defaultHeight: number): HeightModel {
  const measured = new Map<number, number>();
  let prefix = buildPrefix();
  let dirty = false;

  function buildPrefix(): Float64Array {
    const sums = new Float64Array(count + 1);
    for (let index = 0; index < count; index += 1) sums[index + 1] = sums[index] + (measured.get(index) ?? defaultHeight);
    return sums;
  }
  function ensurePrefix() {
    if (dirty) {
      prefix = buildPrefix();
      dirty = false;
    }
  }

  return {
    count,
    measure(index, height) {
      const rounded = Math.max(1, Math.round(height * 100) / 100);
      const previous = measured.get(index);
      if (previous !== undefined && Math.abs(previous - rounded) < 0.5) return false;
      measured.set(index, rounded);
      dirty = true;
      return true;
    },
    reset() {
      measured.clear();
      dirty = true;
    },
    offset(index) {
      ensurePrefix();
      return prefix[Math.max(0, Math.min(index, count))];
    },
    total() {
      ensurePrefix();
      return prefix[count];
    },
    indexAt(offset) {
      ensurePrefix();
      let low = 0;
      let high = count;
      while (low + 1 < high) {
        const mid = (low + high) >> 1;
        if (prefix[mid] <= offset) low = mid;
        else high = mid;
      }
      return low;
    },
  };
}

// The mounted row range for a scroll position, padded by whole rows of
// overscan on both sides.
export type RowWindow = { start: number; end: number };

export function computeWindow(model: HeightModel, scrollTop: number, viewportHeight: number, overscan: number): RowWindow {
  if (model.count === 0) return { start: 0, end: 0 };
  const top = Math.max(0, scrollTop - overscan);
  const bottom = scrollTop + viewportHeight + overscan;
  return { start: model.indexAt(top), end: Math.min(model.count, model.indexAt(bottom) + 1) };
}

// Where to scroll so `index` sits `lead` pixels below the viewport top.
export function scrollTopForRow(model: HeightModel, index: number, lead: number): number {
  return Math.max(0, model.offset(Math.max(0, Math.min(index, model.count - 1))) - lead);
}
