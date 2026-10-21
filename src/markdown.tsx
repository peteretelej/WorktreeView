import { useLayoutEffect, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSanitize from "rehype-sanitize";
import { markdownSchema } from "./markdown-schema.ts";
import { parseCodeRef, type CodeRef } from "./comments.ts";

// Shared sanitized markdown renderer for comment bodies; GFM extensions
// (tables, strikethrough, task lists) match what agents routinely emit, and
// the schema is the audited default, so raw HTML and event handlers never
// reach the DOM.
//
// Two rich-value extensions layer on top of the plain render:
// - fenced ```mermaid blocks render as diagrams through the lazy-loaded
//   mermaid engine in strict mode (no HTML labels, no click handlers);
// - inline `path:line`, `path:start-end`, and `path:line@sha` spans that
//   look like file references become jump chips when a handler is given.

export type { CodeRef };

// Rendered diagrams are cached per theme+code: the canvas surface re-renders
// documents on unrelated state updates, and re-running the async mermaid
// render would flash the placeholder and churn the SVG each time.
const mermaidCache = new Map<string, string>();

function MermaidBlock({ code }: { code: string }) {
  const [svg, setSvg] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const figureId = useRef(`mermaid-${Math.random().toString(36).slice(2)}`);
  const theme = document.documentElement.dataset.theme === "dark" ? "dark" : "default";
  // useLayoutEffect so a cached diagram is applied before the browser
  // paints: a remount would otherwise flash the placeholder for a frame.
  useLayoutEffect(() => {
    const cached = mermaidCache.get(`${theme}:${code}`);
    if (cached) {
      setSvg(cached);
      setFailed(false);
      return;
    }
    let cancelled = false;
    setSvg(null);
    setFailed(false);
    void (async () => {
      try {
        // Strict mode strips HTML labels and event handlers from agent
        // content; the SVG that comes back is mermaid's own output.
        const mermaid = (await import("mermaid")).default;
        mermaid.initialize({ startOnLoad: false, securityLevel: "strict", theme });
        const { svg: rendered } = await mermaid.render(figureId.current, code);
        mermaidCache.set(`${theme}:${code}`, rendered);
        if (!cancelled) setSvg(rendered);
      } catch {
        if (!cancelled) setFailed(true);
      }
    })();
    return () => { cancelled = true; };
  }, [code, theme]);
  if (failed) return <pre className="mermaid-fallback"><code>{code}</code></pre>;
  return svg === null
    ? <div className="mermaid-loading">Rendering diagram…</div>
    : <div className="mermaid-figure" dangerouslySetInnerHTML={{ __html: svg }} />;
}

export function CommentBody({ text, onCodeRef }: { text: string; onCodeRef?: (ref: CodeRef, raw: string) => void }) {
  return <div className="comment-body"><Markdown
    remarkPlugins={[remarkGfm]}
    rehypePlugins={[[rehypeSanitize, markdownSchema]]}
    components={{
      pre({ children }) {
        const child = Array.isArray(children) ? children[0] : children;
        const className = (child as { props?: { className?: string } })?.props?.className ?? "";
        if (className.includes("language-mermaid")) {
          const code = String((child as { props?: { children?: unknown } })?.props?.children ?? "");
          return <MermaidBlock code={code.replace(/\n$/, "")} />;
        }
        return <pre>{children}</pre>;
      },
      code({ className, children }) {
        const raw = String(children);
        if (!className && onCodeRef) {
          const ref = parseCodeRef(raw);
          if (ref) {
            return <button type="button" className="code-ref" title={`Jump to ${ref.path}:${ref.start}`} onClick={() => onCodeRef(ref, raw)}>{raw}</button>;
          }
        }
        return <code className={className}>{children}</code>;
      },
    }}
  >{text}</Markdown></div>;
}
