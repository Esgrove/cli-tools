//! Shared `SQLite` connection setup, per-component migrations, and legacy DB import.
//!
//! All database-backed tools in `cli-tools` share a single `cli-tools.db` file in the platform local data directory
//! (overridable via `CLI_TOOLS_DB` for testing).
//! Pragmas (`busy_timeout`, `WAL`, `synchronous = NORMAL`, `foreign_keys = ON`) are configured
//! before any schema statements, and each crate versions its own tables independently through `schema_migrations`
//! so already-migrated opens take no write lock.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params};

/// Unified database filename stored inside the platform local data directory.
pub const DATABASE_FILENAME: &str = "cli-tools.db";

/// Environment variable that overrides the unified database path.
pub const DATABASE_PATH_ENV: &str = "CLI_TOOLS_DB";

/// Default busy timeout applied to every `SQLite` connection.
const BUSY_TIMEOUT_SECS: u64 = 30;

/// Resolve the path to the unified `cli-tools.db` file.
///
/// Honors `CLI_TOOLS_DB` when set to a non-empty path.
/// Otherwise uses `data_local_dir()/cli-tools/cli-tools.db`.
///
/// # Errors
/// Returns an error if the local data directory cannot be determined.
pub fn database_path() -> Result<PathBuf> {
    if let Some(override_path) = env::var_os(DATABASE_PATH_ENV)
        && !override_path.is_empty()
    {
        return Ok(PathBuf::from(override_path));
    }
    crate::data_file_path(DATABASE_FILENAME)
}

/// Return a sibling legacy database path (such as `vconvert.db` or `dirmove.db`)
/// when it exists alongside `db_path` and is a distinct file.
#[must_use]
pub fn legacy_database_path(db_path: &Path, legacy_filename: &str) -> Option<PathBuf> {
    let candidate = db_path.parent()?.join(legacy_filename);
    if candidate != db_path && candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// Open (or create) the default unified `cli-tools.db` connection configured for concurrent WAL access.
///
/// # Errors
/// Returns an error if the database path cannot be resolved, created, or configured.
pub fn open_default() -> Result<Connection> {
    let path = database_path()?;
    open_at(&path)
}

/// Open (or create) a `SQLite` database at `path` with WAL mode and busy timeout configured before DDL.
///
/// # Errors
/// Returns an error if the parent directory cannot be created or the connection cannot be opened.
pub fn open_at(path: &Path) -> Result<Connection> {
    ensure_parent_directory(path)?;
    let connection = Connection::open(path).with_context(|| format!("Failed to open database: {}", path.display()))?;
    configure_connection(&connection)?;
    Ok(connection)
}

/// Open the default unified database in read-only mode, returning `Ok(None)` if it does not exist yet.
///
/// Read-only commands use this so they never create database files or acquire write locks.
///
/// # Errors
/// Returns an error if the path cannot be resolved or an existing database cannot be opened.
pub fn open_read_only_default() -> Result<Option<Connection>> {
    let path = database_path()?;
    open_read_only_at(&path)
}

/// Open a `SQLite` database at `path` in read-only mode, returning `Ok(None)` if `path` does not exist.
///
/// # Errors
/// Returns an error if an existing database cannot be opened or configured.
pub fn open_read_only_at(path: &Path) -> Result<Option<Connection>> {
    if !path.is_file() {
        return Ok(None);
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(path, flags)
        .with_context(|| format!("Failed to open database read-only: {}", path.display()))?;
    connection
        .busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECS))
        .context("Failed to set SQLite busy timeout")?;
    connection
        .execute_batch("PRAGMA query_only = ON; PRAGMA cache_size = -8000;")
        .context("Failed to configure read-only SQLite pragmas")?;
    Ok(Some(connection))
}

/// Open an in-memory `SQLite` database configured identically to on-disk connections, for testing.
///
/// # Errors
/// Returns an error if the in-memory database cannot be created.
pub fn open_in_memory() -> Result<Connection> {
    let connection = Connection::open_in_memory().context("Failed to open in-memory database")?;
    connection
        .busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECS))
        .context("Failed to set SQLite busy timeout")?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .context("Failed to configure in-memory SQLite pragmas")?;
    Ok(connection)
}

/// Start an `IMMEDIATE` write transaction so writers serialize cleanly via `busy_timeout`
/// instead of failing with `SQLITE_BUSY` on deferred read-to-write upgrades.
///
/// # Errors
/// Returns an error if the transaction cannot be started.
pub fn immediate_transaction(connection: &mut Connection) -> Result<Transaction<'_>> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("Failed to begin IMMEDIATE SQLite transaction")
}

/// Run a per-component schema migration when `component` is below `target_version`.
///
/// When `component` is already at or above `target_version`, no write lock is taken.
/// Otherwise an `IMMEDIATE` transaction is acquired, the version is re-checked, `apply` runs,
/// and the updated version is persisted atomically in `schema_migrations`.
///
/// # Errors
/// Returns an error if inspecting `schema_migrations` or executing the migration fails.
pub fn migrate_component<F>(connection: &mut Connection, component: &str, target_version: i64, apply: F) -> Result<()>
where
    F: FnOnce(&Transaction<'_>, i64) -> Result<()>,
{
    if current_component_version(connection, component)? >= target_version {
        return Ok(());
    }

    let transaction = immediate_transaction(connection)?;
    transaction
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS schema_migrations (
                component TEXT PRIMARY KEY,
                version INTEGER NOT NULL
            );
            ",
        )
        .context("Failed to create schema_migrations table")?;

    let current = transaction
        .query_row(
            "SELECT version FROM schema_migrations WHERE component = ?1",
            params![component],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .context("Failed to read schema_migrations version")?
        .unwrap_or(0);

    if current >= target_version {
        transaction.commit().context("Failed to close migration transaction")?;
        return Ok(());
    }

    apply(&transaction, current)?;

    transaction
        .execute(
            r"
            INSERT INTO schema_migrations (component, version)
            VALUES (?1, ?2)
            ON CONFLICT(component) DO UPDATE SET version = excluded.version
            ",
            params![component, target_version],
        )
        .with_context(|| format!("Failed to record schema version for {component}"))?;

    transaction
        .commit()
        .with_context(|| format!("Failed to commit schema migration for {component}"))?;
    Ok(())
}

/// Return the recorded schema version for `component`, or `0` if not yet initialized.
///
/// # Errors
/// Returns an error if querying fails for a reason other than `schema_migrations` not existing yet.
pub fn current_component_version(connection: &Connection, component: &str) -> Result<i64> {
    if !table_exists(connection, "main", "schema_migrations")? {
        return Ok(0);
    }
    let version = connection
        .query_row(
            "SELECT version FROM schema_migrations WHERE component = ?1",
            params![component],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .with_context(|| format!("Failed to query schema_migrations for {component}"))?;
    Ok(version.unwrap_or(0))
}

/// Return whether `table_name` exists in `schema` (for example `"main"` or `"legacy"`).
///
/// # Errors
/// Returns an error if querying `sqlite_master` fails.
pub fn table_exists(connection: &Connection, schema: &str, table_name: &str) -> Result<bool> {
    let query = format!("SELECT 1 FROM {schema}.sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1");
    let found = connection
        .query_row(&query, params![table_name], |_| Ok(true))
        .optional()
        .with_context(|| format!("Failed to check whether table {schema}.{table_name} exists"))?;
    Ok(found.unwrap_or(false))
}

/// Return whether `column_name` exists on `table_name` in `schema`.
///
/// # Errors
/// Returns an error if `PRAGMA table_info` fails.
pub fn column_exists(connection: &Connection, schema: &str, table_name: &str, column_name: &str) -> Result<bool> {
    let mut statement = connection
        .prepare(&format!("PRAGMA {schema}.table_info({table_name})"))
        .with_context(|| format!("Failed to inspect columns of {schema}.{table_name}"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column_name {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Add `column_name` to `table_name` inside `transaction` if it is not already present.
///
/// # Errors
/// Returns an error if inspecting the table or executing `ALTER TABLE` fails.
pub fn add_column_if_missing(
    transaction: &Transaction<'_>,
    table_name: &str,
    column_name: &str,
    column_definition: &str,
) -> Result<()> {
    if column_exists(transaction, "main", table_name, column_name)? {
        return Ok(());
    }
    match transaction.execute(
        &format!("ALTER TABLE {table_name} ADD COLUMN {column_name} {column_definition}"),
        [],
    ) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(_, Some(ref message))) if message.contains("duplicate column name") => {
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("Failed to add {column_name} column to {table_name}")),
    }
}

/// Attach a legacy database file as `legacy`, execute `import` inside an `IMMEDIATE` transaction,
/// and always detach `legacy` before returning.
///
/// Legacy database files are left untouched on disk as read-only backups.
///
/// # Errors
/// Returns an error if attaching, importing, or committing fails.
pub fn import_from_legacy_database<F>(connection: &mut Connection, legacy_path: &Path, import: F) -> Result<()>
where
    F: FnOnce(&Transaction<'_>) -> Result<()>,
{
    let Some(legacy_str) = legacy_path.to_str() else {
        return Ok(());
    };
    connection
        .execute("ATTACH DATABASE ?1 AS legacy", params![legacy_str])
        .with_context(|| format!("Failed to attach legacy database {}", legacy_path.display()))?;

    let result = (|| -> Result<()> {
        let transaction = immediate_transaction(connection)?;
        import(&transaction)?;
        transaction.commit().context("Failed to commit legacy import")?;
        Ok(())
    })();

    let detach_result = connection.execute("DETACH DATABASE legacy", []);
    result?;
    detach_result.context("Failed to detach legacy database")?;
    Ok(())
}

/// Configure busy timeout, WAL journal mode, synchronous mode, and cache size on an open connection.
fn configure_connection(connection: &Connection) -> Result<()> {
    connection
        .busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECS))
        .context("Failed to set SQLite busy timeout")?;
    connection
        .execute_batch(
            r"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            PRAGMA cache_size = -8000;
            ",
        )
        .context("Failed to configure SQLite pragmas")?;
    Ok(())
}

/// Create parent directories for `path` when it has a non-empty parent.
fn ensure_parent_directory(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create database directory: {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod test_sqlite_core {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn opens_with_wal_mode_and_runs_component_migration_once() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cli-tools.db");
        let mut conn = open_at(&db_path).unwrap();

        let journal_mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

        let mut runs = 0;
        migrate_component(&mut conn, "sample", 1, |tx, from| {
            assert_eq!(from, 0);
            runs += 1;
            tx.execute_batch("CREATE TABLE sample_items (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(runs, 1);

        migrate_component(&mut conn, "sample", 1, |_tx, _from| {
            runs += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(runs, 1);
        assert_eq!(current_component_version(&conn, "sample").unwrap(), 1);
    }

    #[test]
    fn open_read_only_returns_none_for_missing_file_and_blocks_writes_for_existing() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("missing.db");
        assert!(open_read_only_at(&db_path).unwrap().is_none());
        assert!(!db_path.exists());

        let conn = open_at(&db_path).unwrap();
        drop(conn);

        let ro = open_read_only_at(&db_path).unwrap().expect("existing db should open");
        assert!(ro.execute("CREATE TABLE nope (id INTEGER)", []).is_err());
    }

    #[test]
    fn imports_rows_from_legacy_database_leaving_legacy_file_intact() {
        let dir = TempDir::new().unwrap();
        let legacy_path = dir.path().join("legacy.db");
        let new_path = dir.path().join("cli-tools.db");

        let legacy = open_at(&legacy_path).unwrap();
        legacy
            .execute_batch("CREATE TABLE items (name TEXT UNIQUE); INSERT INTO items VALUES ('alpha'), ('beta');")
            .unwrap();
        drop(legacy);

        let mut conn = open_at(&new_path).unwrap();
        conn.execute_batch("CREATE TABLE items (name TEXT UNIQUE);").unwrap();
        import_from_legacy_database(&mut conn, &legacy_path, |tx| {
            if table_exists(tx, "legacy", "items")? {
                tx.execute(
                    "INSERT OR IGNORE INTO main.items (name) SELECT name FROM legacy.items",
                    [],
                )?;
            }
            Ok(())
        })
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
        assert!(legacy_path.is_file());
    }
}
