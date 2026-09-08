export type DiffLine = { text: string; oldLine: number | null; newLine: number | null; expanded?: boolean };
export type SplitRow = { old: DiffLine | null; new: DiffLine | null };
export type HunkLike = { header: string; lines: DiffLine[] };

// Pairs a hunk's unified lines into side-by-side rows for split rendering.
// Context lines appear on both sides; deletion/addition runs zip row-wise and
// the side without a line stays blank. A `\` metadata line becomes its own row
// on the side it follows (both sides when it follows a context line).
export function pairHunkLines(lines: DiffLine[]): SplitRow[] {
  const rows: SplitRow[] = [];
  let afterContext = false;
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    if (line.text.startsWith(" ")) {
      rows.push({ old: line, new: line });
      afterContext = true;
      index += 1;
      continue;
    }
    const deletions: DiffLine[] = [];
    const additions: DiffLine[] = [];
    let oldMeta: DiffLine | null = null;
    let newMeta: DiffLine | null = null;
    while (index < lines.length && lines[index].text.startsWith("-")) { deletions.push(lines[index]); index += 1; }
    if (index < lines.length && lines[index].text.startsWith("\\")) { oldMeta = lines[index]; index += 1; }
    while (index < lines.length && lines[index].text.startsWith("+")) { additions.push(lines[index]); index += 1; }
    if (index < lines.length && lines[index].text.startsWith("\\")) { newMeta = lines[index]; index += 1; }
    const count = Math.max(deletions.length, additions.length);
    for (let offset = 0; offset < count; offset += 1) rows.push({ old: deletions[offset] ?? null, new: additions[offset] ?? null });
    if (oldMeta) rows.push({ old: oldMeta, new: deletions.length === 0 && afterContext ? oldMeta : null });
    if (newMeta) rows.push({ old: null, new: newMeta });
    afterContext = false;
  }
  return rows;
}

export type HunkSpan = { oldStart: number; oldCount: number; newStart: number; newCount: number };

export function parseHunkHeader(header: string): HunkSpan | null {
  const match = header.match(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/);
  if (!match) return null;
  return {
    oldStart: Number(match[1]),
    oldCount: match[2] === undefined ? 1 : Number(match[2]),
    newStart: Number(match[3]),
    newCount: match[4] === undefined ? 1 : Number(match[4]),
  };
}

// A run of lines a patch hides before, between, or after its hunks. Their
// text is identical on both sides (git only hides unchanged lines), so a
// fetched copy of the new-side file renders every gap. `hostHunk` is the
// hunk expanded lines merge into; `before` puts them ahead of its content.
export type PatchGap = {
  id: "leading" | `between:${number}` | "trailing";
  hostHunk: number;
  before: boolean;
  oldStart: number;
  newStart: number;
  lines: number;
};

// A count-0 side names the line before its change (git's convention), so
// unchanged content on that side resumes one past the printed number.
function resumedLine(start: number, count: number): number {
  return count === 0 ? start + 1 : start + count;
}

// totalNewLines (the fetched file's line count) only unlocks the trailing
// gap; hunk headers alone cannot know where the file ends. Malformed headers
// disable expansion rather than misnumbering lines.
export function patchGaps(headers: string[], totalNewLines: number | null): PatchGap[] {
  const spans = headers.map(parseHunkHeader);
  if (spans.length === 0 || spans.some((span) => span === null)) return [];
  const gaps: PatchGap[] = [];
  const first = spans[0]!;
  if (first.newStart > 1) {
    gaps.push({
      id: "leading",
      hostHunk: 0,
      before: true,
      oldStart: Math.max(1, first.oldStart - first.newStart + 1),
      newStart: 1,
      lines: first.newStart - 1,
    });
  }
  for (let index = 1; index < spans.length; index += 1) {
    const previous = spans[index - 1]!;
    const next = spans[index]!;
    const newStart = resumedLine(previous.newStart, previous.newCount);
    const oldStart = resumedLine(previous.oldStart, previous.oldCount);
    const lines = Math.min(next.newStart - newStart, next.oldStart - oldStart);
    if (lines > 0) {
      gaps.push({ id: `between:${index - 1}`, hostHunk: index - 1, before: false, oldStart, newStart, lines });
    }
  }
  if (totalNewLines !== null) {
    const last = spans[spans.length - 1]!;
    const newEnd = last.newCount === 0 ? last.newStart : last.newStart + last.newCount - 1;
    const lines = totalNewLines - newEnd;
    if (lines > 0) {
      gaps.push({ id: "trailing", hostHunk: spans.length - 1, before: false, oldStart: resumedLine(last.oldStart, last.oldCount), newStart: newEnd + 1, lines });
    }
  }
  return gaps;
}

// Splices fetched gap lines into their host hunks as context rows, so the
// rest of the pipeline (pagination, split pairing, tokenization, comment
// anchoring) sees one coherent line stream. Gaps without content stay out.
export function hunksWithExpandedGaps(
  hunks: HunkLike[],
  gaps: PatchGap[],
  expanded: ReadonlySet<string>,
  contentLines: string[] | null,
): HunkLike[] {
  if (gaps.length === 0 || expanded.size === 0 || contentLines === null) return hunks;
  const result = hunks.map((hunk) => ({ header: hunk.header, lines: [...hunk.lines] }));
  for (const gap of gaps) {
    if (!expanded.has(gap.id)) continue;
    const host = result[gap.hostHunk];
    if (!host) continue;
    const lines: DiffLine[] = [];
    for (let offset = 0; offset < gap.lines; offset += 1) {
      const text = contentLines[gap.newStart - 1 + offset];
      if (text === undefined) break;
      // Expanded rows carry no anchor content in the comment layer (which
      // only sees patch lines), so they stay unmarkable via this flag.
      lines.push({ text: ` ${text}`, oldLine: gap.oldStart + offset, newLine: gap.newStart + offset, expanded: true });
    }
    host.lines = gap.before ? [...lines, ...host.lines] : [...host.lines, ...lines];
  }
  return result;
}

// Splits file content into display lines, dropping the phantom entry a
// trailing newline produces while keeping a genuinely empty final line.
export function splitFileLines(text: string): string[] {
  if (text === "") return [];
  const lines = text.split("\n");
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  return lines;
}
