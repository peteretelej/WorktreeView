import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ArrowLeft } from "lucide-react";
import { ZOOM_LEVELS, snapZoom } from "./zoom.ts";

export type Theme = "system" | "light" | "dark";
export type DiffLayout = "unified" | "split";
export type Settings = { theme: Theme; diff_layout: DiffLayout; whitespace_visible: boolean; line_wrap: boolean; syntax_visible: boolean; zoom: number; mcp_enabled: boolean; mcp_listen_address: string; mcp_port: number };

export const defaultSettings: Settings = { theme: "system", diff_layout: "unified", whitespace_visible: false, line_wrap: false, syntax_visible: true, zoom: 1, mcp_enabled: true, mcp_listen_address: "127.0.0.1", mcp_port: 9888 };

export function getSettings() { return invoke<Settings>("get_settings"); }
export function persistSettings(settings: Settings) { return invoke<Settings>("set_settings", { settings }); }

// Mirrors the Rust agent token rows; the secret exists only in the create
// response and is shown once.
export type AgentToken = { id: number; name: string; is_default: boolean; created_at: number; last_used_at: number | null; revoked_at: number | null };
export type CreatedAgentToken = { token: AgentToken; secret: string };
export type McpStatus = { enabled: boolean; running: boolean; address: string; port: number; error: string | null };

export function listAgentTokens() { return invoke<AgentToken[]>("list_agent_tokens"); }
export function createAgentToken(name: string) { return invoke<CreatedAgentToken>("create_agent_token", { name }); }
export function deleteAgentToken(id: number) { return invoke<void>("delete_agent_token", { id }); }
export function getMcpStatus() { return invoke<McpStatus>("get_mcp_status"); }

const DARK_MEDIA_QUERY = "(prefers-color-scheme: dark)";

// Resolves the persisted tri-state to the effective palette: light and
// dark force their palette; system follows the OS color scheme.
export function resolveTheme(theme: Theme): "light" | "dark" {
  if (theme !== "system") return theme;
  return window.matchMedia(DARK_MEDIA_QUERY).matches ? "dark" : "light";
}

// Applies the resolved palette to the document root; called before the
// gated first content render and again whenever the preference or the
// OS scheme changes.
export function applyTheme(theme: Theme) {
  document.documentElement.dataset.theme = resolveTheme(theme);
}

const THEME_OPTIONS: { value: Theme; label: string }[] = [
  { value: "system", label: "System" },
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
];
const DIFF_LAYOUT_OPTIONS: { value: DiffLayout; label: string }[] = [
  { value: "unified", label: "Unified" },
  { value: "split", label: "Split" },
];

const zoomLabel = (level: number) => `${Math.round(level * 100)}%`;

function formatDate(unixMillis: number) {
  return new Date(unixMillis).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

function SettingSwitch({ checked, label, onChange }: { checked: boolean; label: string; onChange: (next: boolean) => void }) {
  return <button type="button" role="switch" aria-checked={checked} aria-label={label} className={`settings-switch${checked ? " on" : ""}`} onClick={() => onChange(!checked)}><span className="settings-switch-knob" /></button>;
}

function CopyButton({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  return <button className="settings-copy-button" type="button" onClick={() => {
    navigator.clipboard?.writeText(value).then(() => {
      setCopied(true);
      window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setCopied(false), 1500);
    }).catch(() => { /* clipboard unavailable; the text is selectable */ });
  }}>{copied ? "Copied" : label}</button>;
}

function StatusLine({ status }: { status: McpStatus | null }) {
  const [state, text] = status === null ? ["off", "Checking..."]
    : !status.enabled ? ["off", "The agent endpoint is off."]
    : status.running ? ["ok", `Running at http://${status.address}:${status.port}/`]
    : ["err", status.error ?? "Not running."];
  return <span className="settings-status"><span className={`settings-status-dot ${state}`} />{state === "ok" ? <code>{text}</code> : text}</span>;
}

// The Agent API page is the control surface for the agent endpoint:
// enable, address, and port apply at the next app start; the token list
// mints named tokens (secret revealed once, with a connection snippet
// while it is visible) and deletes them; the status line surfaces the
// live listener, including a non-fatal bind failure.
function AgentApiSection({ settings, onChange }: { settings: Settings; onChange: (next: Settings) => void }) {
  const [tokens, setTokens] = useState<AgentToken[]>([]);
  const [status, setStatus] = useState<McpStatus | null>(null);
  const [newName, setNewName] = useState("");
  const [revealed, setRevealed] = useState<CreatedAgentToken | null>(null);
  const [error, setError] = useState("");
  const [armed, setArmed] = useState<number | null>(null);

  useEffect(() => {
    let mounted = true;
    listAgentTokens().then((loaded) => { if (mounted) setTokens(loaded); }).catch((caught) => { if (mounted) setError(errorMessageOf(caught)); });
    getMcpStatus().then((loaded) => { if (mounted) setStatus(loaded); }).catch((caught) => { if (mounted) setError(errorMessageOf(caught)); });
    return () => { mounted = false; };
  }, []);

  async function mint() {
    setError("");
    try {
      const created = await createAgentToken(newName);
      setRevealed(created);
      setNewName("");
      setTokens(await listAgentTokens());
    } catch (caught) {
      setError(errorMessageOf(caught));
    }
  }

  async function remove(token: AgentToken) {
    setError("");
    try {
      await deleteAgentToken(token.id);
      setTokens(await listAgentTokens());
      if (revealed?.token.id === token.id) setRevealed(null);
    } catch (caught) {
      setError(errorMessageOf(caught));
    }
  }

  const liveAddress = `http://${status?.address ?? settings.mcp_listen_address}:${status?.port ?? settings.mcp_port}/`;
  return <>
    <section id="settings-mcp" className="settings-section" aria-labelledby="settings-mcp-heading">
      <h2 id="settings-mcp-heading">Agent API</h2>
      <div className="settings-row">
        <div className="settings-row-copy"><strong>Agent endpoint</strong><span>Let coding agents deliver reviews, comment, and ping for refresh.</span></div>
        <SettingSwitch checked={settings.mcp_enabled} label="Agent endpoint" onChange={(mcp_enabled) => onChange({ ...settings, mcp_enabled })} />
      </div>
      <div className="settings-row">
        <div className="settings-row-copy"><strong>Listen address</strong><span>Beyond 127.0.0.1 the token is the real auth boundary.</span></div>
        <input className="settings-select settings-input-address" type="text" spellCheck={false} aria-label="Listen address" value={settings.mcp_listen_address} onChange={(event) => onChange({ ...settings, mcp_listen_address: event.currentTarget.value })} />
      </div>
      <div className="settings-row">
        <div className="settings-row-copy"><strong>Port</strong><span>1-65535; a port already in use surfaces below without blocking startup.</span></div>
        <input className="settings-select settings-input-port" type="number" min={1} max={65535} step={1} aria-label="Listen port" value={String(settings.mcp_port)} onChange={(event) => { const port = Number(event.currentTarget.value); if (Number.isInteger(port) && port >= 1 && port <= 65535) onChange({ ...settings, mcp_port: port }); }} />
      </div>
      <div className="settings-row">
        <div className="settings-row-copy"><strong>Listener</strong><StatusLine status={status} /></div>
      </div>
    </section>
    <section id="settings-tokens" className="settings-section" aria-labelledby="settings-tokens-heading">
      <h2 id="settings-tokens-heading">Agent tokens</h2>
      <div className="settings-row settings-row-flush">
        <div className="settings-row-copy"><strong>Mint a token</strong><span>Name the agent, paste the secret into its config once. Named tokens survive restarts.</span></div>
        <span className="settings-row-actions">
          <input className="settings-select settings-input-name" type="text" spellCheck={false} aria-label="Token name" placeholder="codex" value={newName} onChange={(event) => setNewName(event.currentTarget.value)} onKeyDown={(event) => { if (event.key === "Enter" && newName.trim()) void mint(); }} />
          <button className="settings-select settings-button" type="button" disabled={!newName.trim()} onClick={() => void mint()}>Create token</button>
        </span>
      </div>
      {revealed && <div className="settings-reveal" role="status">
        <div className="settings-reveal-head"><strong>{revealed.token.name}</strong><CopyButton value={revealed.secret} label="Copy secret" /></div>
        <code>{revealed.secret}</code>
        <div className="settings-reveal-head"><code>Authorization: Bearer &lt;secret&gt; on POST {liveAddress}mcp</code><CopyButton value={`Authorization: Bearer ${revealed.secret}`} label="Copy header" /></div>
        <span>Shown once; it is stored only as a hash and cannot be displayed again.</span>
      </div>}
      {tokens.length > 0 && <div className="settings-token-list">
        {tokens.map((token) => <div className="settings-token" key={token.id}>
          <div className="settings-token-copy">
            <strong>{token.name}{token.is_default && <span className="settings-badge default">built-in</span>}</strong>
            <span>{token.is_default ? "Renewed at every app start; discovery clients re-read it automatically." : `Created ${formatDate(token.created_at)}${token.last_used_at ? ` · last used ${formatDate(token.last_used_at)}` : " · never used"}`}</span>
          </div>
          {!token.is_default && <button className={armed === token.id ? "settings-select settings-button settings-button-danger" : "settings-select settings-button"} type="button" onClick={() => {
            if (armed === token.id) {
              setArmed(null);
              void remove(token);
            } else {
              setArmed(token.id);
              window.setTimeout(() => setArmed((current) => (current === token.id ? null : current)), 4000);
            }
          }}>{armed === token.id ? "Confirm delete" : "Delete"}</button>}
        </div>)}
      </div>}
      {error && <div className="settings-inline-error" role="status" aria-live="polite">{error}</div>}
    </section>
  </>;
}

function errorMessageOf(error: unknown) {
  if (typeof error === "object" && error !== null && "message" in error) return String((error as { message: unknown }).message);
  return "The agent API settings could not be loaded.";
}

type SettingsPageId = "general" | "agents";

const SETTINGS_PAGES: { id: SettingsPageId; label: string }[] = [
  { id: "general", label: "General" },
  { id: "agents", label: "Agent API" },
];

export function SettingsPage({ settings, saveError, onBack, onChange }: { settings: Settings; saveError: string; onBack: () => void; onChange: (next: Settings) => void }) {
  const [page, setPage] = useState<SettingsPageId>("general");
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
      <nav className="settings-nav-sections" aria-label="Settings pages">
        {SETTINGS_PAGES.map((entry) => <button key={entry.id} className="settings-nav-link" type="button" aria-current={page === entry.id ? "page" : undefined} onClick={() => setPage(entry.id)}>{entry.label}</button>)}
      </nav>
    </aside>
    <div className="settings-content">
      {page === "general" && <>
        <section id="settings-appearance" className="settings-section" aria-labelledby="settings-appearance-heading">
          <h2 id="settings-appearance-heading">Appearance</h2>
          <div className="settings-row">
            <div className="settings-row-copy"><strong>Theme</strong><span>Interface color scheme; system follows the OS color scheme.</span></div>
            <div className="scope-toggle" role="group" aria-label="Theme">{THEME_OPTIONS.map((option) => <button key={option.value} type="button" className={settings.theme === option.value ? "active" : ""} aria-pressed={settings.theme === option.value} onClick={() => onChange({ ...settings, theme: option.value })}>{option.label}</button>)}</div>
          </div>
          <div className="settings-row">
            <div className="settings-row-copy"><strong>Zoom</strong><span>Interface scale; Ctrl +, Ctrl -, and Ctrl 0 also work.</span></div>
            <select className="settings-select" aria-label="Interface zoom" value={String(snapZoom(settings.zoom))} onChange={(event) => onChange({ ...settings, zoom: Number(event.currentTarget.value) })}>
              {ZOOM_LEVELS.map((level) => <option key={level} value={String(level)}>{zoomLabel(level)}</option>)}
            </select>
          </div>
        </section>
        <section id="settings-diff" className="settings-section" aria-labelledby="settings-diff-heading">
          <h2 id="settings-diff-heading">Diff</h2>
          <div className="settings-row">
            <div className="settings-row-copy"><strong>Diff layout</strong><span>How changed files are rendered in reviews.</span></div>
            <div className="scope-toggle" role="group" aria-label="Diff layout">{DIFF_LAYOUT_OPTIONS.map((option) => <button key={option.value} type="button" className={settings.diff_layout === option.value ? "active" : ""} aria-pressed={settings.diff_layout === option.value} onClick={() => onChange({ ...settings, diff_layout: option.value })}>{option.label}</button>)}</div>
          </div>
          <div className="settings-row">
            <div className="settings-row-copy"><strong>Syntax highlighting</strong><span>Colorize code tokens in rendered patches.</span></div>
            <SettingSwitch checked={settings.syntax_visible} label="Syntax highlighting" onChange={(syntax_visible) => onChange({ ...settings, syntax_visible })} />
          </div>
          <div className="settings-row">
            <div className="settings-row-copy"><strong>Whitespace visibility</strong><span>Show whitespace changes in rendered patches.</span></div>
            <SettingSwitch checked={settings.whitespace_visible} label="Whitespace visibility" onChange={(whitespace_visible) => onChange({ ...settings, whitespace_visible })} />
          </div>
          <div className="settings-row">
            <div className="settings-row-copy"><strong>Line wrap</strong><span>Wrap long lines instead of scrolling horizontally.</span></div>
            <SettingSwitch checked={settings.line_wrap} label="Line wrap" onChange={(line_wrap) => onChange({ ...settings, line_wrap })} />
          </div>
        </section>
      </>}
      {page === "agents" && <AgentApiSection settings={settings} onChange={onChange} />}
      {saveError && <div className="settings-inline-error" role="status" aria-live="polite">{saveError}</div>}
    </div>
  </section>;
}
