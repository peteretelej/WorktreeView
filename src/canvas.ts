// Mirrors of the serde types behind the submission IPC plus pure view
// helpers for the canvas. Findings are not mirrored here: they materialize
// as ordinary agent comments (see comments.ts) and never duplicate into
// the submission surface.
export type SubmissionSection = { kind: string; title: string; body: string };
export type Submission = {
  id: number;
  review_id: number;
  agent_name: string;
  agent_model: string;
  command_context: string | null;
  sections: SubmissionSection[];
  created_at: number;
};

export type SectionView =
  | { mode: "markdown"; label: string; fallback: boolean }
  | { mode: "html"; label: string; badge: string };

const NATIVE_LABELS: Record<string, string> = {
  brief: "Brief",
  walkthrough: "Walkthrough",
  notes: "Notes",
};

// The kind vocabulary is app-owned: native kinds get their label, unknown
// kinds fall back to the raw kind string rendered through the same
// sanitized markdown, and the client-supplied html kind maps to the
// sandboxed iframe descriptor with its client-content badge.
export function sectionView(section: SubmissionSection): SectionView {
  if (section.kind === "html") return { mode: "html", label: "html", badge: "client content" };
  const native = section.kind in NATIVE_LABELS;
  return {
    mode: "markdown",
    label: native ? NATIVE_LABELS[section.kind] : section.kind,
    fallback: !native,
  };
}

// One submission as markdown for pasting into another tool or agent chat:
// author context first, then every section under its label or title.
export function formatSubmissionForCopy(submission: Submission): string {
  const date = new Date(submission.created_at).toISOString().slice(0, 10);
  const head = `Review submitted by **${submission.agent_name}** (${submission.agent_model}) on ${date}`;
  const sections = submission.sections.map((section) => {
    const title = section.title || sectionView(section).label;
    return `## ${title}\n\n${section.body.trim()}`;
  });
  const context = submission.command_context ? `\nCommand context: ${submission.command_context}\n` : "";
  return `${head}\n${context}\n${sections.join("\n\n")}\n`;
}
