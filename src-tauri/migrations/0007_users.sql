CREATE TABLE users (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL UNIQUE CHECK (
    LENGTH(TRIM(name)) > 0 AND name = TRIM(name) AND LENGTH(name) <= 200
  ),
  is_admin INTEGER NOT NULL CHECK (is_admin IN (0, 1)),
  created_at INTEGER NOT NULL
);

CREATE TABLE user_tokens (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  token_hash TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER
);
