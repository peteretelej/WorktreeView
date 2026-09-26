import { useEffect, useRef, useState } from "react";
import { call, type SourceResolver } from "./remote.ts";
import { Check, ChevronDown, ChevronRight, Copy } from "lucide-react";
import { CommentBody } from "./markdown.tsx";
import { copyText } from "./clipboard.ts";
import type { ReviewKey } from "./comments.ts";
import { formatSubmissionForCopy, sectionView, type Submission, type SubmissionSection } from "./canvas.ts";

// Static client html rendered in a sandboxed iframe: the empty sandbox
// attribute denies scripts, forms, and same-origin (opaque origin), and the
// app CSP inherits into the srcdoc document, blocking external fetches.
function HtmlBlock({ body, title }: { body: string; title: string }) {
  return <div className="html-block">
    <span className="html-block-badge">client content</span>
    <iframe sandbox="" srcDoc={body} title={title} />
  </div>;
}

// One generic card for every section kind: native kinds render their label,
// unknown kinds the raw kind string, both through the shared sanitized
// markdown renderer; html sections bypass markdown entirely.
export function SectionCard({ section }: { section: SubmissionSection }) {
  const view = sectionView(section);
  const kindChip = view.mode === "html" ? view.badge : view.label;
  return <article className={`section-card${view.mode === "html" ? " section-card-html" : view.fallback ? " section-card-fallback" : ""}`}>
    <header className="section-card-head">
      <span className="section-kind">{kindChip}</span>
      {section.title && <strong className="section-title">{section.title}</strong>}
    </header>
    {view.mode === "html"
      ? <HtmlBlock body={section.body} title={section.title || "client content"} />
      : <CommentBody text={section.body} />}
  </article>;
}

function SubmissionCard({ submission }: { submission: Submission }) {
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  async function copy() {
    if (await copyText(formatSubmissionForCopy(submission))) {
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    }
  }
  return <article className="submission-card">
    <button className="submission-toggle" type="button" aria-expanded={open} onClick={() => setOpen(!open)}>
      {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
      <span className="submission-agent">{submission.agent_name}</span>
      <span className="submission-model">{submission.agent_model}</span>
      <span className="comment-time" title={new Date(submission.created_at).toLocaleString()}>{new Date(submission.created_at).toLocaleDateString()}</span>
    </button>
    <button className="comment-copy submission-copy" type="button" aria-label="Copy review as markdown" title="Copy review as markdown" onClick={() => void copy()}>{copied ? <Check size={12} /> : <Copy size={12} />}</button>
    {open && <div className="submission-sections">
      {submission.command_context && <p className="submission-context" title="Command context">{submission.command_context}</p>}
      {submission.sections.map((section, index) => <SectionCard key={index} section={section} />)}
    </div>}
  </article>;
}

// The review's submission list: one expandable card per agent submission.
// Finding comments render with severity badges in the comment stream; the
// author filter already separates agent authors. A changed refreshTick
// (a live arrival for this identity) reloads the listing in place. The
// listing reads from the review identity's source (local or server).
export function ReviewsStrip({ reviewKey, refreshTick, resolve = () => ({ kind: "local" }) }: { reviewKey: ReviewKey | null; refreshTick: number; resolve?: SourceResolver }) {
  const [submissions, setSubmissions] = useState<Submission[]>([]);
  const key = reviewKey ? `${reviewKey.repoPath}\0${reviewKey.baseSha}\0${reviewKey.targetKey}\0${reviewKey.targetKind}` : "";
  const resolveRef = useRef(resolve);
  useEffect(() => { resolveRef.current = resolve; });
  useEffect(() => {
    if (!reviewKey) {
      setSubmissions([]);
      return;
    }
    let cancelled = false;
    void call<Submission[]>("list_submissions", { repoPath: reviewKey.repoPath, baseSha: reviewKey.baseSha, targetKey: reviewKey.targetKey, targetKind: reviewKey.targetKind }, resolveRef.current(reviewKey.repoPath))
      .then((loaded) => { if (!cancelled) setSubmissions(loaded); })
      .catch(() => { if (!cancelled) setSubmissions([]); });
    return () => { cancelled = true; };
  }, [key, refreshTick]);
  if (!reviewKey || submissions.length === 0) return null;
  return <section className="reviews-strip" aria-label="Agent reviews">
    <div className="reviews-strip-heading"><strong>Reviews</strong><span>{submissions.length}</span></div>
    {submissions.map((submission) => <SubmissionCard key={submission.id} submission={submission} />)}
  </section>;
}
