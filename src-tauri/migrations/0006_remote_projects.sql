-- Remote projects key on the same path string local projects do: the
-- canonical identity (user@host:path, or ssh://host:port/path for a
-- non-default port) already carries host, user, port, and path, and every
-- repo-keyed table references repos(path), so an additive marker column
-- needs no cross-table carry. The marker lets each layer classify remote
-- vs local without re-parsing the identity; remote paths are POSIX and are
-- never canonicalized locally. Existing rows are untouched.
ALTER TABLE repos ADD COLUMN remote INTEGER NOT NULL DEFAULT 0;
