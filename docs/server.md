# Running the headless server

`worktreeview-server` is the same Rust backend as the desktop app,
booted without a window: one binary, one SQLite store, and the
directory of repositories under review. It serves the unchanged agent
face (the MCP face at `POST /mcp` and the raw JSON-RPC face at
`POST /`) plus the push-event stream at `GET /events`. Systemd and
Docker are documented here, not packaged: deployment is a binary and a
home directory.

## Install shape

- The `worktreeview-server` binary: download the release tarball
  (`worktreeview-server-<version>-<target>.tar.gz`, with a `.sha256`
  beside it for `sha256sum -c`) from the release page, or build from
  the same crate as the desktop app (`cargo build --release
  --manifest-path src-tauri/Cargo.toml --bins` produces it beside the
  desktop binary). Targets: `linux-amd64`, `macos-arm64`, `macos-x64`,
  `windows-x64`.
- A server home directory, holding the store and the endpoint config
  file. The default is `~/.worktreeview-server/` (`~` is the user
  profile); relocate it with `--home <dir>` or
  `WORKTREEVIEW_SERVER_HOME`.
- The repositories under review, at paths that are stable for agents:
  a submission, comment, or refresh names the repository by its
  server-local path, and review computation runs on the server host
  under the same boundaries as the desktop (read-only Git CLI, fetch as
  the one write).

## Starting

```
worktreeview-server serve [--home <dir>] [--bind <addr:port>]
```

`--bind` defaults to `127.0.0.1:9890`, one port above the desktop's
9888, so a desktop app and a server coexist on one host. A bind failure
is fatal for the server: the reason lands on stderr and the process
exits nonzero, with no config file and no default token left behind for
a dead endpoint. On a successful bind the server provisions the default
agent token and writes `config.json` `{port, token}` into the server
home; the file is rewritten at every start, and agents discover the
server exactly as [connect-an-agent.md](connect-an-agent.md) describes,
reading that file. A stale `config.json` after an unclean stop is
survivable: the next start overwrites it, and clients re-read on
refusal.

## Agent face and events

The JSON-RPC methods, the MCP face, the error-code matrix, and the
token semantics are byte-for-byte the desktop listener's; the protocol
is specified in [agent-submissions.md](agent-submissions.md). The same
bearer tokens also authenticate `GET /events`, a server-sent-event
stream of the push events the desktop webview receives:
`submission-received`, `project-refreshed`, `comment-changed`, and
`review-request-changed`, each as

```
event: submission-received
data: {"repo_path": "...", ...}
```

Idle streams carry a `: keepalive` comment, and a subscriber that falls
too far behind is cut. The client fallback is reconnect with
exponential backoff; no polling re-implementation is built, and a
stream that cannot be established surfaces an error instead of
degrading silently. Two clients are built in: the desktop app consumes
this stream for its server-backed projects (see
[user-guide.md](user-guide.md#work-against-a-server)), and coding
agents discover the endpoint as
[connect-an-agent.md](connect-an-agent.md) describes.

The desktop app's webview is the one browser-engine caller, and its
fetches all carry an `Authorization` header, so the endpoint answers
CORS preflights for the bundled webview origins (`tauri://localhost`,
`http://tauri.localhost`, `https://tauri.localhost`) on the faces the
webview consumes (`/api/*` and `/events`), reflecting the matched
origin. Every other caller keeps the header-free responses a
non-browser client always sees, and a preflight never bypasses the
bearer gate: the token remains the authentication boundary on every
face.

## Accounts and tokens

The first admin is bootstrapped on the host with
`worktreeview-server create-admin <name>`: it prints the member's first
bearer token once, and that token is what a desktop app enters under
Settings > Servers to connect. Admins create further members and mint
agent tokens from the connected desktop app's management section; the
token semantics (hash-only storage, print-once secrets, the protected
last admin) are the same on both faces.

## Running under systemd

```
# /etc/systemd/system/worktreeview-server.service
[Unit]
Description=WorktreeView server
After=network.target

[Service]
User=worktreeview
ExecStart=/usr/local/bin/worktreeview-server serve --bind 127.0.0.1:9890
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

The home defaults to the service user's profile
(`/home/worktreeview/.worktreeview-server/`); pass `--home` to place it
elsewhere, such as a dedicated data volume.

## Running in Docker

No image is packaged; the example runs the operator-built binary in a
container:

```
docker run -d --name worktreeview-server \
  -p 127.0.0.1:9890:9890 \
  -v wv-server-home:/home/wv/.worktreeview-server \
  -v /srv/repos:/srv/repos \
  worktreeview-server:latest \
  serve --home /home/wv/.worktreeview-server --bind 0.0.0.0:9890
```

Inside the container the listener binds all interfaces so the port
mapping reaches it; publishing with `-p 127.0.0.1:9890:9890` keeps the
host-side exposure on loopback.

## Beyond loopback

The default bind is loopback because the bearer token, not the
network, is the authentication boundary (see
[safety-model.md](safety-model.md)). To serve other hosts, terminate
TLS at a reverse proxy and forward to the loopback listener, so the
bearer tokens never cross the network in clear text:

```
server {
    listen 443 ssl;
    server_name reviews.example.com;

    location / {
        proxy_pass http://127.0.0.1:9890;
        proxy_http_version 1.1;
        proxy_buffering off;   # /events streams; buffering delays it
        proxy_read_timeout 1h; # keep idle event streams alive
    }
}
```
