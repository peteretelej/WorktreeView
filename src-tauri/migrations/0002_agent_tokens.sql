CREATE TABLE agent_tokens (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  secret_hash TEXT NOT NULL UNIQUE,
  is_default INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at INTEGER
);
ALTER TABLE comments ADD COLUMN author_token_id INTEGER REFERENCES agent_tokens(id) ON DELETE SET NULL;
