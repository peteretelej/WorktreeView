export type DiffLine = { text: string; oldLine: number | null; newLine: number | null };
export type SplitRow = { old: DiffLine | null; new: DiffLine | null };

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
