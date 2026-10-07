import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { listen } from "@tauri-apps/api/event";
import { ArrowLeft } from "lucide-react";
import { ZOOM_LEVELS, snapZoom } from "./zoom.ts";
import { connectionLabel, call, probeConnection, type RemoteSource, type ServerConnectionRow } from "./remote.ts";
import { type SkillTarget } from "./skillTargets.ts";

export type Theme = "system" | "light" | "dark";
export type DiffLayout = "unified" | "split";
export type ChangedFilesView = "tree" | "list" | "details";
export type Settings = { theme: Theme; diff_layout: DiffLayout; whitespace_visible: boolean; line_wrap: boolean; syntax_visible: boolean; inline_comments_visible: boolean; files_pane_visible: boolean; comments_pane_visible: boolean; comments_pane_wide: boolean; changed_files_view: ChangedFilesView; zoom: number; mcp_enabled: boolean; notifications_enabled: boolean; mcp_listen_address: string; mcp_port: number; activity_seen_id: number; skill_prompt_key: string };

// Mirrors the server's identity rows; the secret exists only in the
// create response and is shown once.
export type ServerUser = { id: number; name: string; is_admin: boolean; created_at: number };
export type CreatedServerUser = { user: ServerUser; secret: string };

export const defaultSettings: Settings = { theme: "system", diff_layout: "unified", whitespace_visible: false, line_wrap: false, syntax_visible: true, inline_comments_visible: true, files_pane_visible: true, comments_pane_visible: true, comments_pane_wide: false, changed_files_view: "tree", zoom: 1, mcp_enabled: true, notifications_enabled: true, mcp_listen_address: "127.0.0.1", mcp_port: 9888, activity_seen_id: 0, skill_prompt_key: "" };

export function getSettings() { return invoke<Settings>("get_settings"); }
export function persistSettings(settings: Settings) { return invoke<Settings>("set_settings", { settings }); }

// Mirrors the Rust agent token rows; the secret exists only in the create
// response and is shown once.
export type AgentToken = { id: number; name: string; is_default: boolean; created_at: number; last_used_at: number | null; revoked_at: number | null };
export type CreatedAgentToken = { token: AgentToken; secret: string };
export type McpStatus = { enabled: boolean; running: boolean; address: string; port: number; error: string | null; config_path: string; skill_dir: string };

export function listAgentTokens() { return invoke<AgentToken[]>("list_agent_tokens"); }
export function createAgentToken(name: string) { return invoke<CreatedAgentToken>("create_agent_token", { name }); }
export function deleteAgentToken(id: number) { return invoke<void>("delete_agent_token", { id }); }
export function getMcpStatus() { return invoke<McpStatus>("get_mcp_status"); }
export function restartMcp() { return invoke<McpStatus>("restart_mcp"); }
// Start-at-login is OS registration, not a stored setting: nothing
// registers at boot, so off is the default on every platform.
export function getStartAtLogin() { return invoke<boolean>("get_autostart_enabled"); }
export function setStartAtLogin(enabled: boolean) { return invoke<boolean>("set_autostart_enabled", { enabled }); }

// The updater flow is event-driven: Rust owns the phase machine and the
// webview mirrors `update-status` events plus this one-shot read.
export type UpdateStatus = { phase: "idle" | "checking" | "up_to_date" | "managed_by_store" | "available" | "downloading" | "ready" | "failed"; version: string | null; notes: string | null; received: number | null; total: number | null; message: string | null };
export function getUpdateStatus() { return invoke<UpdateStatus>("get_update_status"); }
export function checkForUpdates() { return invoke<void>("check_for_updates"); }
export function installUpdate() { return invoke<void>("install_update"); }
export function restartApp() { return invoke<void>("restart_app"); }
export function listSkillTargets() { return invoke<SkillTarget[]>("list_skill_targets"); }
export function installSkillTarget(path: string) { return invoke<SkillTarget>("install_skill_target", { path }); }
// The persisted write for a dismissed install prompt; the key rides the
// Settings read but only this command writes it (the activity_seen_id pattern).
export function setSkillPrompt(key: string) { return invoke<void>("set_skill_prompt", { key }); }

// The server-connection commands are purely local store operations; every
// server call itself runs from the webview through the remote seam (the
// save flow probes first, then persists).
export function listServerConnections() { return invoke<ServerConnectionRow[]>("list_server_connections"); }
export function saveServerConnection(id: number | null, url: string, label: string, token: string) {
  return invoke<ServerConnectionRow>("save_server_connection", { id, url, label: label.trim() || null, token });
}
export function deleteServerConnection(id: number) { return invoke<void>("delete_server_connection", { id }); }
export function addServerProject(connectionId: number, repoPath: string) {
  return invoke<ServerConnectionRow["projects"][number]>("add_server_project", { connectionId, repoPath });
}
export function removeServerProject(id: number) { return invoke<void>("remove_server_project", { id }); }
export function setServerProjectPinned(id: number, pinned: boolean) {
  return invoke<void>("set_server_project_pinned", { id, pinned });
}

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
// enable, address, and port are stored immediately and applied by the
// Restart action beside the listener status; the token list mints named
// tokens (secret revealed once, with a connection snippet while it is
// visible) and deletes them; the status line surfaces the live listener,
// including a non-fatal bind failure.
function AgentApiSection({ settings, onChange, targets, onInstall }: { settings: Settings; onChange: (next: Settings) => void; targets: SkillTarget[]; onInstall: (path: string) => Promise<SkillTarget> }) {
  const [tokens, setTokens] = useState<AgentToken[]>([]);
  const [status, setStatus] = useState<McpStatus | null>(null);
  const [startAtLogin, setStartAtLoginState] = useState<boolean | null>(null);
  const [installing, setInstalling] = useState("");
  const [newName, setNewName] = useState("");
  const [revealed, setRevealed] = useState<CreatedAgentToken | null>(null);
  const [error, setError] = useState("");
  const [armed, setArmed] = useState<number | null>(null);
  const [restarting, setRestarting] = useState(false);

  useEffect(() => {
    let mounted = true;
    listAgentTokens().then((loaded) => { if (mounted) setTokens(loaded); }).catch((caught) => { if (mounted) setError(errorMessageOf(caught)); });
    getMcpStatus().then((loaded) => { if (mounted) setStatus(loaded); }).catch((caught) => { if (mounted) setError(errorMessageOf(caught)); });
    getStartAtLogin().then((enabled) => { if (mounted) setStartAtLoginState(enabled); }).catch((caught) => { if (mounted) setError(errorMessageOf(caught)); });
    return () => { mounted = false; };
  }, []);

  // The targets list is owned by the app shell (the install prompt shares
  // it), so installing only reports the outcome; the refreshed row state
  // arrives through the shared prop.
  async function install(path: string) {
    setError("");
    setInstalling(path);
    try {
      await onInstall(path);
    } catch (caught) {
      setError(errorMessageOf(caught));
    } finally {
      setInstalling("");
    }
  }

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

  async function restart() {
    setError("");
    setRestarting(true);
    try {
      setStatus(await restartMcp());
      // The built-in default token renews on restart, so its row changes.
      setTokens(await listAgentTokens());
    } catch (caught) {
      setError(errorMessageOf(caught));
    } finally {
      setRestarting(false);
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

  async function toggleStartAtLogin(next: boolean) {
    setError("");
    try {
      setStartAtLoginState(await setStartAtLogin(next));
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
        <div className="settings-row-copy"><strong>Start at login</strong><span>Register the app to start when you log in so agents reach the endpoint after a reboot without opening it; off by default. Closing the window already keeps the app running in the tray.</span></div>
        <SettingSwitch checked={startAtLogin === true} label="Start at login" onChange={(next) => void toggleStartAtLogin(next)} />
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
        <button className="settings-select settings-button" type="button" disabled={restarting} onClick={() => void restart()}>{restarting ? "Restarting..." : "Restart"}</button>
      </div>
      {status && <div className="settings-row">
        <div className="settings-row-copy">
          <strong>Integration files</strong>
          <span>Discovery clients read the config file for the live endpoint; the app keeps a version-matched copy of the agent skill in its home, refreshed on every launch.</span>
          <span className="settings-paths">
            <code>{status.config_path}</code>
            <code>{status.skill_dir}</code>
          </span>
        </div>
        <span className="settings-row-actions">
          <CopyButton value={status.config_path} label="Copy config path" />
          <CopyButton value={status.skill_dir} label="Copy skill path" />
        </span>
      </div>}
      <div className="settings-row settings-row-flush">
        <div className="settings-row-copy"><strong>Skill installs</strong><span>Copy the bundled skill into your agents' skills folders so they follow the documented workflows; a release that changes the skill marks copies here for update. Updating overwrites the folder's skill files, nothing else.</span></div>
      </div>
      {targets.length > 0 ? <div className="settings-token-list">
        {targets.map((target) => <div className="settings-token" key={target.path}>
          <div className="settings-token-copy">
            <strong>{target.label}{target.status !== "up_to_date" && <span className={`settings-badge ${target.status === "differs" ? "warn" : "muted"}`} title={target.status === "differs" ? "The installed copy does not match the bundled skill." : undefined}>{target.status === "differs" ? "differs" : "not installed"}</span>}</strong>
            <span><code>{target.path}</code></span>
          </div>
          {target.status !== "up_to_date" && <button className="settings-select settings-button" type="button" disabled={installing !== ""} onClick={() => void install(target.path)}>{installing === target.path ? "Installing..." : target.status === "differs" ? "Update" : "Install"}</button>}
        </div>)}
      </div> : <p className="request-form-hint">No agent skills folders detected under your user folder. The home copy above always travels with the app, and manual installs follow docs/connect-an-agent.md.</p>}
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
            <span>{token.is_default ? "Renewed at every endpoint start; discovery clients re-read it automatically." : `Created ${formatDate(token.created_at)}${token.last_used_at ? ` · last used ${formatDate(token.last_used_at)}` : " · never used"}`}</span>
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

// The Servers page is the control surface for the desktop client's server
// connections: URL plus the account's token, stored in the local store.
// Saving probes the server first (one authenticated list_repos through the
// remote seam) so a wrong URL or token never lands as a row; deleting a
// connection cascades this app's references to its projects and never
// touches the server's own store. Under the list, one management section
// per connection drives the server's admin routes through the remote seam.
function ServerConnectionsSection({ onChanged }: { onChanged: () => void }) {
  const [connections, setConnections] = useState<ServerConnectionRow[]>([]);
  const [url, setUrl] = useState("");
  const [label, setLabel] = useState("");
  const [token, setToken] = useState("");
  const [editingId, setEditingId] = useState<number | null>(null);
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  const [armed, setArmed] = useState<number | null>(null);

  useEffect(() => {
    let mounted = true;
    listServerConnections().then((loaded) => { if (mounted) setConnections(loaded); }).catch((caught) => { if (mounted) setError(errorMessageOf(caught)); });
    return () => { mounted = false; };
  }, []);

  async function reload() {
    setConnections(await listServerConnections());
    onChanged();
  }

  function startEdit(connection: ServerConnectionRow) {
    setEditingId(connection.id);
    setUrl(connection.url);
    setLabel(connection.label ?? "");
    setToken(connection.token);
    setError("");
  }

  function resetForm() {
    setEditingId(null);
    setUrl("");
    setLabel("");
    setToken("");
    setError("");
  }

  async function save() {
    if (saving) return;
    setError("");
    setSaving(true);
    try {
      await probeConnection(url, token);
      await saveServerConnection(editingId, url, label, token);
      resetForm();
      await reload();
    } catch (caught) {
      setError(errorMessageOf(caught));
    } finally {
      setSaving(false);
    }
  }

  async function remove(connection: ServerConnectionRow) {
    setError("");
    try {
      await deleteServerConnection(connection.id);
      if (editingId === connection.id) resetForm();
      await reload();
    } catch (caught) {
      setError(errorMessageOf(caught));
    }
  }

  return <>
    <section id="settings-servers" className="settings-section" aria-labelledby="settings-servers-heading">
    <h2 id="settings-servers-heading">Servers</h2>
    <div className="settings-row settings-row-flush">
      <div className="settings-row-copy"><strong>{editingId === null ? "Connect to a server" : "Edit connection"}</strong><span>WorktreeView server URL and your user token. The token is stored in this app's local store and sent as a bearer header on every server call.</span></div>
      <span className="settings-row-actions">
        <input className="settings-select settings-input-name" type="text" spellCheck={false} aria-label="Server URL" placeholder="http://host:9887" value={url} onChange={(event) => setUrl(event.currentTarget.value)} />
        <input className="settings-select settings-input-name" type="text" spellCheck={false} aria-label="Label (optional)" placeholder="Label (optional)" value={label} onChange={(event) => setLabel(event.currentTarget.value)} />
        <input className="settings-select settings-input-name" type="password" aria-label="User token" placeholder="User token" value={token} onChange={(event) => setToken(event.currentTarget.value)} />
        <button className="settings-select settings-button" type="button" disabled={saving || !url.trim() || !token.trim()} onClick={() => void save()}>{saving ? "Checking..." : editingId === null ? "Connect" : "Save changes"}</button>
        {editingId !== null && <button className="settings-select settings-button" type="button" onClick={resetForm}>Cancel</button>}
      </span>
    </div>
    <p className="request-form-hint">Saving checks the server with one authenticated call; a wrong URL or token surfaces above without being stored.</p>
    {connections.length > 0 && <div className="settings-token-list">
      {connections.map((connection) => <div className="settings-token" key={connection.id}>
        <div className="settings-token-copy">
          <strong>{connectionLabel(connection)}</strong>
          <span><code>{connection.url}</code> · {connection.projects.length} {connection.projects.length === 1 ? "project" : "projects"}</span>
        </div>
        <span className="settings-row-actions">
          <button className="settings-select settings-button" type="button" onClick={() => startEdit(connection)}>Edit</button>
          <button className={armed === connection.id ? "settings-select settings-button settings-button-danger" : "settings-select settings-button"} type="button" onClick={() => {
            if (armed === connection.id) {
              setArmed(null);
              void remove(connection);
            } else {
              setArmed(connection.id);
              window.setTimeout(() => setArmed((current) => (current === connection.id ? null : current)), 4000);
            }
          }}>{armed === connection.id ? "Confirm delete" : "Delete"}</button>
        </span>
      </div>)}
    </div>}
    {connections.length > 0 && <p className="request-form-hint">Deleting removes the connection and this app's references to its projects. The server's own store is never touched.</p>}
    {error && <div className="settings-inline-error" role="status" aria-live="polite">{error}</div>}
    </section>
    {connections.map((connection) => <ServerManagementSection key={connection.id} connection={connection} />)}
  </>;
}

// One connection's server management: member accounts and agent tokens
// over the server's admin routes through the remote seam. The server
// enforces the admin flag and its refusals (non-admin access, the
// last-admin delete) surface in the section's error line; created secrets
// are shown once and stored only as a hash server-side.
function ServerManagementSection({ connection }: { connection: ServerConnectionRow }) {
  const source: RemoteSource = { kind: "server", connectionId: connection.id, url: connection.url, token: connection.token };
  const label = connectionLabel(connection);
  const endpointBase = `${connection.url.replace(/\/+$/, "")}`;
  const [users, setUsers] = useState<ServerUser[]>([]);
  const [tokens, setTokens] = useState<AgentToken[]>([]);
  const [revealedUser, setRevealedUser] = useState<CreatedServerUser | null>(null);
  const [revealedToken, setRevealedToken] = useState<CreatedAgentToken | null>(null);
  const [newUserName, setNewUserName] = useState("");
  const [newTokenName, setNewTokenName] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [armed, setArmed] = useState<string | null>(null);

  useEffect(() => {
    let mounted = true;
    void (async () => {
      try {
        const [loadedUsers, loadedTokens] = await Promise.all([
          call<ServerUser[]>("list_users", undefined, source),
          call<AgentToken[]>("list_agent_tokens", undefined, source),
        ]);
        if (!mounted) return;
        setUsers(loadedUsers);
        setTokens(loadedTokens);
      } catch (caught) {
        if (mounted) setError(errorMessageOf(caught));
      }
    })();
    return () => { mounted = false; };
  }, []);

  async function reload() {
    const [loadedUsers, loadedTokens] = await Promise.all([
      call<ServerUser[]>("list_users", undefined, source),
      call<AgentToken[]>("list_agent_tokens", undefined, source),
    ]);
    setUsers(loadedUsers);
    setTokens(loadedTokens);
  }

  async function run(action: () => Promise<void>) {
    setError("");
    setBusy(true);
    try {
      await action();
    } catch (caught) {
      setError(errorMessageOf(caught));
    } finally {
      setBusy(false);
    }
  }

  // The delete buttons arm with a confirming second click; refusals land
  // in the section's error line.
  function armThen(key: string, action: () => Promise<void>) {
    if (armed !== key) {
      setArmed(key);
      window.setTimeout(() => setArmed((current) => (current === key ? null : current)), 4000);
      return;
    }
    setArmed(null);
    void run(action);
  }

  async function createUser() {
    const created = await call<CreatedServerUser>("create_user", { name: newUserName.trim() }, source);
    setRevealedUser(created);
    setNewUserName("");
    await reload();
  }

  async function createToken() {
    const created = await call<CreatedAgentToken>("create_agent_token", { name: newTokenName.trim() }, source);
    setRevealedToken(created);
    setNewTokenName("");
    await reload();
  }

  return <section className="settings-section" aria-labelledby={`settings-server-${connection.id}-heading`}>
    <h2 id={`settings-server-${connection.id}-heading`}>Manage {label}</h2>
    <div className="settings-row settings-row-flush">
      <div className="settings-row-copy"><strong>Create a user</strong><span>Names the member and mints their first bearer token, shown once.</span></div>
      <span className="settings-row-actions">
        <input className="settings-select settings-input-name" type="text" spellCheck={false} aria-label={`User name on ${label}`} placeholder="name" value={newUserName} onChange={(event) => setNewUserName(event.currentTarget.value)} onKeyDown={(event) => { if (event.key === "Enter" && newUserName.trim()) void run(createUser); }} />
        <button className="settings-select settings-button" type="button" disabled={busy || !newUserName.trim()} onClick={() => void run(createUser)}>Create user</button>
      </span>
    </div>
    {revealedUser && <div className="settings-reveal" role="status">
      <div className="settings-reveal-head"><strong>{revealedUser.user.name}</strong><CopyButton value={revealedUser.secret} label="Copy token" /></div>
      <code>{revealedUser.secret}</code>
      <span>Shown once; the server stores only its hash. It is the user token that member enters under Servers.</span>
    </div>}
    {users.length > 0 && <div className="settings-token-list">
      {users.map((user) => <div className="settings-token" key={user.id}>
        <div className="settings-token-copy">
          <strong>{user.name}{user.is_admin && <span className="settings-badge default">admin</span>}</strong>
          <span>Created {formatDate(user.created_at)}</span>
        </div>
        <button className={armed === `user-${user.id}` ? "settings-select settings-button settings-button-danger" : "settings-select settings-button"} type="button" onClick={() => armThen(`user-${user.id}`, async () => {
          await call("delete_user", { id: user.id }, source);
          if (revealedUser?.user.id === user.id) setRevealedUser(null);
          await reload();
        })}>{armed === `user-${user.id}` ? "Confirm delete" : "Delete"}</button>
      </div>)}
    </div>}
    <div className="settings-row settings-row-flush">
      <div className="settings-row-copy"><strong>Mint an agent token</strong><span>Name the agent and paste the secret into its config on the server host, once.</span></div>
      <span className="settings-row-actions">
        <input className="settings-select settings-input-name" type="text" spellCheck={false} aria-label={`Agent token name on ${label}`} placeholder="codex" value={newTokenName} onChange={(event) => setNewTokenName(event.currentTarget.value)} onKeyDown={(event) => { if (event.key === "Enter" && newTokenName.trim()) void run(createToken); }} />
        <button className="settings-select settings-button" type="button" disabled={busy || !newTokenName.trim()} onClick={() => void run(createToken)}>Create token</button>
      </span>
    </div>
    {revealedToken && <div className="settings-reveal" role="status">
      <div className="settings-reveal-head"><strong>{revealedToken.token.name}</strong><CopyButton value={revealedToken.secret} label="Copy secret" /></div>
      <code>{revealedToken.secret}</code>
      <div className="settings-reveal-head"><code>Authorization: Bearer &lt;secret&gt; on POST {endpointBase}/mcp</code><CopyButton value={`Authorization: Bearer ${revealedToken.secret}`} label="Copy header" /></div>
      <span>Shown once; it is stored only as a hash and cannot be displayed again.</span>
    </div>}
    {tokens.length > 0 && <div className="settings-token-list">
      {tokens.map((token) => <div className="settings-token" key={token.id}>
        <div className="settings-token-copy">
          <strong>{token.name}{token.is_default && <span className="settings-badge default">built-in</span>}</strong>
          <span>{token.is_default ? "Renewed at every server start; discovery clients re-read it automatically." : `Created ${formatDate(token.created_at)}${token.last_used_at ? ` · last used ${formatDate(token.last_used_at)}` : " · never used"}`}</span>
        </div>
        {!token.is_default && <button className={armed === `token-${token.id}` ? "settings-select settings-button settings-button-danger" : "settings-select settings-button"} type="button" onClick={() => armThen(`token-${token.id}`, async () => {
          await call("delete_agent_token", { id: token.id }, source);
          if (revealedToken?.token.id === token.id) setRevealedToken(null);
          await reload();
        })}>{armed === `token-${token.id}` ? "Confirm delete" : "Delete"}</button>}
      </div>)}
    </div>}
    {error && <div className="settings-inline-error" role="status" aria-live="polite">{error}</div>}
  </section>;
}

// Static app facts plus the release version; the version is the same value
// the agent endpoint reports as its server version.
function AboutSection() {
  const [version, setVersion] = useState("");
  const [logError, setLogError] = useState("");
  const [update, setUpdate] = useState<UpdateStatus | null>(null);
  useEffect(() => {
    let mounted = true;
    getVersion().then((loaded) => { if (mounted) setVersion(loaded); }).catch(() => { /* the rest of the page still renders */ });
    getUpdateStatus().then((loaded) => { if (mounted) setUpdate(loaded); }).catch(() => { /* ditto */ });
    const unlisten = listen<UpdateStatus>("update-status", (event) => { if (mounted) setUpdate(event.payload); });
    return () => { mounted = false; void unlisten.then((off) => off()); };
  }, []);
  async function openLogs() {
    setLogError("");
    try {
      await invoke("open_log_dir");
    } catch (caught) {
      setLogError(errorMessageOf(caught));
    }
  }
  function runUpdateAction() {
    if (!update) return;
    setLogError("");
    const action = update.phase === "available" ? installUpdate() : update.phase === "ready" ? restartApp() : checkForUpdates();
    void action.catch((caught) => setLogError(errorMessageOf(caught)));
  }
  const phase = update?.phase ?? "idle";
  const updateText = phase === "checking" ? "Checking for updates..."
    : phase === "up_to_date" ? "You're up to date."
    : phase === "managed_by_store" ? "Updates are managed by the Microsoft Store."
    : phase === "available" ? `Version ${update?.version} is available.`
    : phase === "downloading" ? `Downloading update${update?.total ? ` (${Math.round((100 * (update.received ?? 0)) / update.total)}%)` : "..."}`
    : phase === "ready" ? `Version ${update?.version} is installed; restart to finish.`
    : phase === "failed" ? update?.message ?? "The update check failed."
    : "Check for a new version and install it from here.";
  const updateAction = phase === "available" ? "Download and install"
    : phase === "ready" ? "Restart to update"
    : phase === "checking" || phase === "downloading" ? "Working..."
    : "Check for updates";
  return <section id="settings-about" className="settings-section" aria-labelledby="settings-about-heading">
    <h2 id="settings-about-heading">About</h2>
    <div className="settings-row">
      <div className="settings-row-copy"><strong>WorktreeView</strong><span>Review-first desktop app for code review across Git worktrees, built for developers running coding agents in parallel.</span></div>
    </div>
    <div className="settings-row">
      <div className="settings-row-copy"><strong>Version</strong><span>Also reported to agents as the endpoint server version.</span></div>
      <code>{version || "unknown"}</code>
    </div>
    <div className="settings-row">
      <div className="settings-row-copy"><strong>Updates</strong><span>{updateText}</span></div>
      <button className="settings-select settings-button" type="button" disabled={phase === "checking" || phase === "downloading"} onClick={runUpdateAction}>{updateAction}</button>
    </div>
    <div className="settings-row">
      <div className="settings-row-copy"><strong>Logs</strong><span>Small rolling diagnostic files; attach them when reporting an issue. Secrets, notes, and review text are never written to them.</span></div>
      <button className="settings-select settings-button" type="button" onClick={() => void openLogs()}>Open folder</button>
    </div>
    {logError && <div className="settings-inline-error" role="status" aria-live="polite">{logError}</div>}
  </section>;
}

type SettingsPageId = "general" | "servers" | "agents" | "about";

const SETTINGS_PAGES: { id: SettingsPageId; label: string }[] = [
  { id: "general", label: "General" },
  { id: "servers", label: "Servers" },
  { id: "agents", label: "Agent API" },
  { id: "about", label: "About" },
];

export function SettingsPage({ settings, saveError, skillTargets, onInstallSkillTarget, onBack, onChange, onConnectionsChanged }: { settings: Settings; saveError: string; skillTargets: SkillTarget[]; onInstallSkillTarget: (path: string) => Promise<SkillTarget>; onBack: () => void; onChange: (next: Settings) => void; onConnectionsChanged: () => void }) {
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
        <section id="settings-notifications" className="settings-section" aria-labelledby="settings-notifications-heading">
          <h2 id="settings-notifications-heading">Notifications</h2>
          <div className="settings-row">
            <div className="settings-row-copy"><strong>Notifications</strong><span>Arrival cue and desktop toasts when agents deliver reviews or start reviewing.</span></div>
            <SettingSwitch checked={settings.notifications_enabled} label="Notifications" onChange={(notifications_enabled) => onChange({ ...settings, notifications_enabled })} />
          </div>
        </section>
      </>}
      {page === "servers" && <ServerConnectionsSection onChanged={onConnectionsChanged} />}
      {page === "agents" && <AgentApiSection settings={settings} onChange={onChange} targets={skillTargets} onInstall={onInstallSkillTarget} />}
      {page === "about" && <AboutSection />}
      {saveError && <div className="settings-inline-error" role="status" aria-live="polite">{saveError}</div>}
    </div>
  </section>;
}
