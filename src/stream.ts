import { pairHunkLines, type DiffLine, type HunkLike, type PatchGap } from "./diff.ts";

// Presentation stream for the patch pane: one flat row list per file
// (hunk headers, patch or file lines, expand controls), plus the height
// bookkeeping that lets a scroll window mount only visible rows. Both
// patch and full-file views render from this module.

// Gaps at or under this many hidden lines collapse to a slim one-line
// control; taller gaps keep the regular control.
export const SLIM_GAP_MAX_LINES = 3;

// Past this many rows the pane declines to render and shows a notice with
// open-externally actions: a bounded freeze guard for pathological files,
// not a memory optimization.
export const MAX_RENDERED_ROWS = 50_000;

const IMAGE_MIME_BY_EXTENSION: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  bmp: "image/bmp",
  ico: "image/x-icon",
  avif: "image/avif",
  svg: "image/svg+xml",
};

// The MIME type for renderable image assets, or null when the path is not
// one; used to preview images the diff cannot show.
export function imageMimeForPath(path: string): string | null {
  const name = path.split(/[\/]/).pop() ?? "";
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return null;
  return IMAGE_MIME_BY_EXTENSION[name.slice(dot + 1).toLowerCase()] ?? null;
}

export type HeaderRow = { kind: "header"; header: string; hunkIndex: number };
export type LineRow = { kind: "line"; line: DiffLine; hunkIndex: number };
export type SplitRow = { kind: "split"; old: DiffLine | null; next: DiffLine | null; hunkIndex: number };
export type GapRow = { kind: "gap"; gap: PatchGap; slim: boolean };
export type RowSpec = HeaderRow | LineRow | SplitRow | GapRow;

// Flattens a patch into one continuous row stream. `expandedHunks` is the
// patch after hunksWithExpandedGaps, so expanded gap lines already sit in
// their host hunks; unexpanded gaps render inline (a leading gap ahead of
// the first header, between/trailing gaps after their host hunk). A hunk
// header only marks a seam: once the gap above a hunk is expanded the
// content joins seamlessly and the header would read as a leftover gap.
export function buildPatchRows(expandedHunks: HunkLike[], gaps: PatchGap[], expanded: ReadonlySet<string>, contentLines: string[] | null, split: boolean): RowSpec[] {
  const rows: RowSpec[] = [];
  expandedHunks.forEach((hunk, index) => {
    const leading = index === 0 ? gaps.find((gap) => gap.before) : undefined;
    if (leading && (!expanded.has(leading.id) || contentLines === null)) rows.push({ kind: "gap", gap: leading, slim: leading.lines <= SLIM_GAP_MAX_LINES });
    const seamAbove = index === 0 ? leading : gaps.find((gap) => !gap.before && gap.hostHunk === index - 1);
    const joinedAbove = seamAbove !== undefined && expanded.has(seamAbove.id) && contentLines !== null;
    if (!joinedAbove) rows.push({ kind: "header", header: hunk.header, hunkIndex: index });
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
