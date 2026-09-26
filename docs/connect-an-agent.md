# Connecting a coding agent

WorktreeView serves coding agents on one loopback listener with two
faces: the MCP face at `POST /mcp` (reads, comment collaboration,
refresh ping) and the raw JSON-RPC face at `POST /` (review
submissions). Both accept the same bearer token; the listener address
and port live in Settings' Agent API section, and nothing is reachable
from outside the machine unless the listen address is changed there.

## Discovery clients

Clients that support the config file need no configuration: after a
successful bind the app writes `config.json` into its app home,
carrying the current `{port, token}`. The token it names is the
built-in default, which renews at every app start and whenever the
endpoint is restarted from the Settings Agent API section, so discovery
clients keep working; the file is rewritten each time. The app home is
`~/.worktreeview/` on every OS (`~` is the user profile; launch with
`--home <dir>` or set `WORKTREEVIEW_DATA_DIR` to relocate it). Dev
builds use `~/.worktreeview-dev/` instead, so with default homes the two
channels never trade registrations or stores (pointing both at the same
override removes that separation).

## Manual configuration

1. In Settings' Agent API section, create a token named after the
   agent. The secret is shown once and stored only as a hash; named
   tokens survive restarts, give the agent its own identity, and can be
   deleted independently.
2. Point the client's MCP server entry at
   `http://127.0.0.1:<port>/mcp` with the header
   `Authorization: Bearer <secret>`. Clients that take a JSON server
   list accept an entry like:

   ```json
   {
     "mcpServers": {
       "worktreeview": {
         "url": "http://127.0.0.1:9888/mcp",
         "headers": { "Authorization": "Bearer <secret>" }
       }
     }
   }
   ```

   Comment writes accept an optional self-reported `author_model` label
   ("GPT Luna medium") shown next to the agent name, so one token can
   distinguish the models it runs. Clients that speak the stateless
   2026-07-28 MCP revision connect directly; older revisions connect
   through the stateless handshake answer (see the conformance notes in
   [agent-submissions.md](agent-submissions.md)).
3. For review submissions, deliver to `POST /` on the same listener
   with the same header, using the schema in
   [agent-submissions.md](agent-submissions.md).

Deleting a token in Settings refuses it immediately; comments it
authored remain visible as history and can no longer be mutated by any
agent. The protocol details for both faces, including error semantics
and size caps, are in [agent-submissions.md](agent-submissions.md).

## Agents on a remote project's host

Agents working in repositories added as remote projects need nothing
new: they post over MCP directly, exactly as agents on this machine do.
Point the client at the app's listener and give it a named token of its
own. One boundary moves, though: an agent on another host reaches the
listener over the network, so the listen address must be bound beyond
loopback for it to connect at all. Beyond loopback the per-agent bearer
token is the authentication boundary; anything holding a token reads
reviews and comments as that agent, so issue one token per agent and
delete the tokens of machines that leave.

## Agents on a WorktreeView server

The headless server ([server.md](server.md)) serves the same two faces
on its own listener, so an agent running on the server host connects
exactly as above, reading the server home's `config.json` instead of the
desktop app's: it carries the same `{port, token}`, is rewritten at
every server start, and lives under `~/.worktreeview-server/` by default
(relocated with `--home <dir>` or `WORKTREEVIEW_SERVER_HOME`). The
contract is unchanged and the address stays a localhost default: same
MCP and JSON-RPC faces, same tools and error semantics, same
`Authorization: Bearer` header. Manual configuration uses the server's
address with an agent token an admin minted for it (in the desktop
app's Servers management section, exposed to the server's admin routes);
the server's built-in default token works the same way the desktop's
does while it is live. Comments and submissions attribute to the
token's agent name exactly as they do on the desktop.

## The agent skill

The repo ships an installable skill that teaches agents all of the above
plus the day-to-day workflow (adding projects, refreshing, reading reviews,
posting and responding to comments, and the review-request fleet protocol:
requesting review of their own work and picking up other agents'
requests) so the setup steps become a checklist instead of prose. It
follows the Agent Skills standard:

```sh
npx skills add peteretelej/WorktreeView
```

The app also publishes its own version-matched copy of the skill folder
into the app home at `~/.worktreeview/skills/worktreeview/`, refreshed on
every launch, and the Settings Agent API section shows the path with a
copy button. Copying that folder into your agents' skills directory (for
example `~/.agents/skills/worktreeview/`) is the same as installing from
the repository, and it always matches the app version you run. The MCP
endpoint remains fully usable without the skill; it only removes the
guesswork.
