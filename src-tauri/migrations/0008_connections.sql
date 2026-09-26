CREATE TABLE server_connections (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  url TEXT NOT NULL,
  label TEXT,
  -- The user's bearer token, stored as sent: the local store's trust
  -- boundary already covers every other secret-adjacent row it holds.
  token TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE server_projects (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  connection_id INTEGER NOT NULL REFERENCES server_connections(id) ON DELETE CASCADE,
  -- The project's path on the server host, never resolved locally.
  repo_path TEXT NOT NULL,
  pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
  created_at INTEGER NOT NULL,
  UNIQUE (connection_id, repo_path)
);
