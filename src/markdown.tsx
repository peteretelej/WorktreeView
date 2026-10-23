import { createContext, memo, useContext, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import Markdown, { type Components, type Options } from "react-markdown";
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

// The components map must keep a stable identity: react-markdown hands these
// functions to React as element types, so rebuilding the map per render would
// remount every `pre` and `code` (diagrams included) on each parent render.
// Values that vary per call site (the jump-chip handler) arrive by context.
const MARKDOWN_COMPONENTS: Components = {
  pre({ children }) {
    const child = Array.isArray(children) ? children[0] : children;
    const className = (child as { props?: { className?: string } })?.props?.className ?? "";
    if (className.includes("language-mermaid")) {
      const code = String((child as { props?: { children?: unknown } })?.props?.children ?? "");
      return <MermaidBlock code={code.replace(/\n$/, "")} />;
    }
    return <pre>{children}</pre>;
  },
  code: MarkdownCode,
};

const CodeRefContext = createContext<((ref: CodeRef, raw: string) => void) | null>(null);

function MarkdownCode({ className, children }: { className?: string; children?: ReactNode }) {
  const onCodeRef = useContext(CodeRefContext);
  const raw = String(children);
  if (!className && onCodeRef) {
    const ref = parseCodeRef(raw);
    if (ref) {
      return <button type="button" className="code-ref" title={`Jump to ${ref.path}:${ref.start}`} onClick={() => onCodeRef(ref, raw)}>{raw}</button>;
    }
  }
  return <code className={className}>{children}</code>;
}

// Diagram palettes are baked into each cached SVG, so a theme flip must
// reach MermaidBlock even where memoization skips unrelated re-renders;
// applyTheme pings this store after the root attribute lands.
type DiagramTheme = "dark" | "default";

function readDiagramTheme(): DiagramTheme {
  return document.documentElement.dataset.theme === "dark" ? "dark" : "default";
}
let diagramTheme = readDiagramTheme();
const diagramThemeListeners = new Set<() => void>();

export function diagramThemeChanged() {
  const next = readDiagramTheme();
  if (next === diagramTheme) return;
  diagramTheme = next;
  diagramThemeListeners.forEach((listener) => listener());
}

function useDiagramTheme(): DiagramTheme {
  return useSyncExternalStore((listener) => {
    diagramThemeListeners.add(listener);
    return () => diagramThemeListeners.delete(listener);
  }, () => diagramTheme);
}

function MermaidBlock({ code }: { code: string }) {
  const [svg, setSvg] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const theme = useDiagramTheme();
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
    // A fresh id per run keeps a superseded render from colliding with the
    // live one inside mermaid's global registry.
    const figureId = `mermaid-${Math.random().toString(36).slice(2)}`;
    setSvg(null);
    setFailed(false);
    void (async () => {
      try {
        // Strict mode strips HTML labels and event handlers from agent
        // content; the SVG that comes back is mermaid's own output.
        const mermaid = (await import("mermaid")).default;
        mermaid.initialize({ startOnLoad: false, securityLevel: "strict", theme });
        const { svg: rendered } = await mermaid.render(figureId, code);
        // Only the live run caches: a render superseded by a theme flip can
        // come back painted under the other theme's global config, and the
        // cache would replay that mixed render for the rest of the session.
        if (!cancelled) mermaidCache.set(`${theme}:${code}`, rendered);
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

const REMARK_PLUGINS: Options["remarkPlugins"] = [remarkGfm];
const REHYPE_PLUGINS: Options["rehypePlugins"] = [[rehypeSanitize, markdownSchema]];

function CommentBodyImpl({ text, onCodeRef }: { text: string; onCodeRef?: (ref: CodeRef, raw: string) => void }) {
  // Read through a ref so the memo below can compare text alone; the
  // wrapper identity stays stable across renders.
  const handlerRef = useRef(onCodeRef);
  handlerRef.current = onCodeRef;
  const hasHandler = onCodeRef !== undefined;
  const handler = useMemo(() => (hasHandler
    ? (ref: CodeRef, raw: string) => handlerRef.current?.(ref, raw)
    : null), [hasHandler]);
  return <div className="comment-body"><CodeRefContext.Provider value={handler}><Markdown
    remarkPlugins={REMARK_PLUGINS}
    rehypePlugins={REHYPE_PLUGINS}
    components={MARKDOWN_COMPONENTS}
  >{text}</Markdown></CodeRefContext.Provider></div>;
}

// Comment bodies re-render on every unrelated app state update; equal text
// must skip the markdown re-parse and everything under it.
export const CommentBody = memo(CommentBodyImpl, (a, b) => a.text === b.text && (a.onCodeRef !== undefined) === (b.onCodeRef !== undefined));
