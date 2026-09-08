import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { Bold, Code, Eye, Italic, Link2, List, ListOrdered, PenLine, Quote, Strikethrough } from "lucide-react";
import { CommentBody } from "./markdown.tsx";
import { insertLink, toggleLinePrefix, wrapSelection, type Edit, type TextState } from "./markdownEdit.ts";

// The comment editor: one textarea with a markdown toolbar and keyboard
// shortcuts, a preview tab rendering through the shared sanitized
// renderer, and autosizing height. Formatting actions run on the textarea's
// live selection and restore it after the controlled re-render.
export function MarkdownComposer({ value, onChange, placeholder, onSubmit, onCancel, footer, autoFocus = true }: {
  value: string;
  onChange(value: string): void;
  placeholder: string;
  onSubmit(): void;
  onCancel(): void;
  footer?: ReactNode;
  autoFocus?: boolean;
}) {
  const ref = useRef<HTMLTextAreaElement>(null);
  const pendingSelection = useRef<[number, number] | null>(null);
  const [tab, setTab] = useState<"write" | "preview">("write");

  useLayoutEffect(() => {
    const area = ref.current;
    if (!area) return;
    if (pendingSelection.current) {
      const [start, end] = pendingSelection.current;
      pendingSelection.current = null;
      area.focus();
      area.setSelectionRange(start, end);
    }
    area.style.height = "0px";
    area.style.height = `${Math.min(360, Math.max(84, area.scrollHeight + 1))}px`;
  }, [value, tab]);

  function apply(edit: Edit) {
    pendingSelection.current = [edit.start, edit.end];
    onChange(edit.value);
    setTab("write");
  }
  const state = (): TextState => ({ value, selectionStart: ref.current?.selectionStart ?? value.length, selectionEnd: ref.current?.selectionEnd ?? value.length });
  function onKeyDown(event: React.KeyboardEvent<HTMLTextAreaElement>) {
    if ((event.ctrlKey || event.metaKey) && !event.altKey) {
      const key = event.key.toLowerCase();
      const shortcuts: Record<string, Edit> = {
        b: wrapSelection(state(), "**", "**"),
        i: wrapSelection(state(), "_", "_"),
        e: wrapSelection(state(), "`", "`"),
        k: insertLink(state()),
      };
      if (!event.shiftKey && shortcuts[key]) {
        event.preventDefault();
        // Stop the chord from reaching global handlers (Ctrl+K is the
        // command palette); the editor owns it while composing.
        event.stopPropagation();
        apply(shortcuts[key]);
        return;
      }
      if (key === "enter") {
        event.preventDefault();
        onSubmit();
        return;
      }
    }
    if (event.key === "Escape") {
      event.stopPropagation();
      onCancel();
    }
  }
  function tool(label: string, shortcut: string, icon: ReactNode, action: () => void) {
    return <button type="button" className="composer-tool" aria-label={`${label} (${shortcut})`} title={`${label} (${shortcut})`} onMouseDown={(event) => event.preventDefault()} onClick={action}>{icon}</button>;
  }
  return <div className="composer">
    <div className="composer-bar">
      <div className="composer-tools">
        {tool("Bold", "Ctrl+B", <Bold size={13} />, () => apply(wrapSelection(state(), "**", "**")))}
        {tool("Italic", "Ctrl+I", <Italic size={13} />, () => apply(wrapSelection(state(), "_", "_")))}
        {tool("Strikethrough", "", <Strikethrough size={13} />, () => apply(wrapSelection(state(), "~~", "~~")))}
        {tool("Code", "Ctrl+E", <Code size={13} />, () => apply(wrapSelection(state(), "`", "`")))}
        {tool("Link", "Ctrl+K", <Link2 size={13} />, () => apply(insertLink(state())))}
        <span className="composer-tool-gap" />
        {tool("Quote", "", <Quote size={13} />, () => apply(toggleLinePrefix(state(), "> ")))}
        {tool("Bulleted list", "", <List size={13} />, () => apply(toggleLinePrefix(state(), "- ")))}
        {tool("Numbered list", "", <ListOrdered size={13} />, () => apply(toggleLinePrefix(state(), (index) => `${index + 1}. `)))}
      </div>
      <div className="composer-tabs" role="group" aria-label="Editor mode">
        <button type="button" className={tab === "write" ? "active" : ""} aria-pressed={tab === "write"} onClick={() => { setTab("write"); requestAnimationFrame(() => ref.current?.focus()); }}><PenLine size={12} /> Write</button>
        <button type="button" className={tab === "preview" ? "active" : ""} aria-pressed={tab === "preview"} onClick={() => setTab("preview")}><Eye size={12} /> Preview</button>
      </div>
    </div>
    {tab === "write"
      ? <textarea ref={ref} aria-label={placeholder} placeholder={placeholder} value={value} autoFocus={autoFocus} onKeyDown={onKeyDown} onChange={(event) => onChange(event.currentTarget.value)} />
      : value.trim() === ""
        ? <p className="composer-preview-empty">Nothing to preview yet.</p>
        : <div className="composer-preview" ><CommentBody text={value} /></div>}
    {footer}
  </div>;
}
