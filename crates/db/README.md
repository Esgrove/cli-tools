# cli-tools-db

Shared `SQLite` database layer for `cli-tools`.

Manages the unified `cli-tools.db` database
(`WAL` journal mode, busy timeout, per-component versioned migrations, and one-time legacy database import)
and the cross-tool `active_locks` lease store with background heartbeats and same-host PID liveness checks.
