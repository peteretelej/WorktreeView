# Connecting a coding agent

WorktreeView serves coding agents on one loopback listener with two
faces: the MCP face at `POST /mcp` (reads, comment collaboration,
refresh ping) and the raw JSON-RPC face at `POST /` (review
submissions). Both accept the same bearer token; the listener address
and port live in Settings' Agent API section, and nothing is reachable
from outside the machine unless the listen address is changed there.

## Discovery clients

Clients that support the discovery file need no configuration: after a
successful bind the app writes `agent-endpoint.json` into its app data
directory (dev builds use `agent-endpoint-dev.json`), carrying the
current `{port, token}`. The token it names is the built-in default,
which renews at every app start, so discovery clients keep working
across restarts; the file is rewritten each boot.

## Manual configuration

1. In Settings' Agent API section, create a token named after the
   agent. The secret is shown once and stored only as a hash; named
   tokens survive restarts, give the agent its own identity, and can be
   deleted independently.
2. Point the client's MCP server entry at
   `http://127.0.0.1:<port>/mcp` with the header
   `Authorization: Bearer <secret>`. Clients that speak the stateless
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
