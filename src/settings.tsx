import { useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ArrowLeft } from "lucide-react";

export type Theme = "system" | "light" | "dark";
export type DiffLayout = "unified" | "split";
export type Settings = { theme: Theme; diff_layout: DiffLayout; whitespace_visible: boolean; line_wrap: boolean };

export const defaultSettings: Settings = { theme: "system", diff_layout: "unified", whitespace_visible: false, line_wrap: false };

export function getSettings() { return invoke<Settings>("get_settings"); }
export function persistSettings(settings: Settings) { return invoke<Settings>("set_settings", { settings }); }

const THEME_OPTIONS: { value: Theme; label: string }[] = [
  { value: "system", label: "System" },
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
];
const DIFF_LAYOUT_OPTIONS: { value: DiffLayout; label: string }[] = [
  { value: "unified", label: "Unified" },
  { value: "split", label: "Split" },
];

export function SettingsPage({ settings, saveError, onBack, onChange }: { settings: Settings; saveError: string; onBack: () => void; onChange: (next: Settings) => void }) {
  useEffect(() => {
    function handleKeyDown(event: KeyboardEvent) {
      if (event.key !== "Escape" || event.defaultPrevented) return;
      if (document.querySelector("dialog[open]")) return;
      onBack();
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [onBack]);
  return <section className="settings-overlay" role="region" aria-label="Settings">
    <aside className="settings-nav">
      <button className="back-button" type="button" onClick={onBack}><ArrowLeft size={15} /> Back</button>
      <p className="eyebrow">Settings</p>
      <nav className="settings-nav-sections" aria-label="Settings sections">
        <button className="settings-nav-link" type="button" onClick={() => document.getElementById("settings-appearance")?.scrollIntoView({ block: "start" })}>Appearance</button>
        <button className="settings-nav-link" type="button" onClick={() => document.getElementById("settings-diff")?.scrollIntoView({ block: "start" })}>Diff</button>
      </nav>
    </aside>
    <div className="settings-content">
      <section id="settings-appearance" className="settings-section" aria-labelledby="settings-appearance-heading">
        <h2 id="settings-appearance-heading">Appearance</h2>
        <div className="settings-row">
          <div className="settings-row-copy"><strong>Theme</strong><span>Interface color scheme; the app stays dark until theming is applied.</span></div>
          <div className="scope-toggle" role="group" aria-label="Theme">{THEME_OPTIONS.map((option) => <button key={option.value} type="button" className={settings.theme === option.value ? "active" : ""} aria-pressed={settings.theme === option.value} onClick={() => onChange({ ...settings, theme: option.value })}>{option.label}</button>)}</div>
        </div>
      </section>
      <section id="settings-diff" className="settings-section" aria-labelledby="settings-diff-heading">
        <h2 id="settings-diff-heading">Diff</h2>
        <div className="settings-row">
          <div className="settings-row-copy"><strong>Diff layout</strong><span>How changed files are rendered in reviews.</span></div>
          <div className="scope-toggle" role="group" aria-label="Diff layout">{DIFF_LAYOUT_OPTIONS.map((option) => <button key={option.value} type="button" className={settings.diff_layout === option.value ? "active" : ""} aria-pressed={settings.diff_layout === option.value} onClick={() => onChange({ ...settings, diff_layout: option.value })}>{option.label}</button>)}</div>
        </div>
        <label className="settings-row settings-toggle">
          <span className="settings-row-copy"><strong>Whitespace visibility</strong><span>Show whitespace changes in rendered patches.</span></span>
          <input type="checkbox" checked={settings.whitespace_visible} onChange={(event) => onChange({ ...settings, whitespace_visible: event.currentTarget.checked })} />
        </label>
        <label className="settings-row settings-toggle">
          <span className="settings-row-copy"><strong>Line wrap</strong><span>Wrap long lines instead of scrolling horizontally.</span></span>
          <input type="checkbox" checked={settings.line_wrap} onChange={(event) => onChange({ ...settings, line_wrap: event.currentTarget.checked })} />
        </label>
      </section>
      {saveError && <div className="settings-inline-error" role="status" aria-live="polite">{saveError}</div>}
    </div>
  </section>;
}
