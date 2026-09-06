CREATE TABLE retrospected_surfaces (
  repo_path TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('worktree','branch')),
  identity_key TEXT NOT NULL,
  label TEXT NOT NULL,
  detail TEXT NOT NULL,
  head_sha TEXT NOT NULL,
  last_seen_at INTEGER NOT NULL,
  PRIMARY KEY (repo_path, kind, identity_key)
);
