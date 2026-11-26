//! `SQLite` database operations for `dirmove` stored in the unified `cli-tools.db`.
//!
//! Stores directory names that have been used or seen in `directory_names`,
//! importing existing rows from a legacy `dirmove.db` file on first migration.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cli_tools_db::sqlite;
use rusqlite::{Connection, OptionalExtension, params};

use cli_tools_dirmove::utils;

/// Schema migration component name for `dirmove`.
const COMPONENT_NAME: &str = "dirmove_names";
/// Current schema version for `directory_names`.
const SCHEMA_VERSION: i64 = 1;
/// Legacy database filename imported on first migration into `cli-tools.db`.
const LEGACY_DIRMOVE_DB: &str = "dirmove.db";

/// Database wrapper for directory name operations.
pub struct Database {
    connection: Connection,
}

/// A directory name entry from the database.
#[derive(Debug, Clone)]
pub struct DirectoryEntry {
    /// Normalized name for lookups (lowercase, no spaces).
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only the tests check the stored normalized form")
    )]
    pub normalized_name: String,
    /// Display name with original casing and spacing.
    pub display_name: String,
}

impl Database {
    /// Open or create the unified `cli-tools.db` database at the default path.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or initialized.
    pub fn open_default() -> Result<Self> {
        let path = sqlite::database_path()?;
        Self::open_at(&path)
    }

    /// Open or create the database at a specific path, importing from legacy `dirmove.db` if applicable.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or initialized.
    pub fn open_at(path: &Path) -> Result<Self> {
        let mut connection = sqlite::open_at(path)?;
        let legacy = if path.file_name().and_then(|name| name.to_str()) == Some(sqlite::DATABASE_FILENAME) {
            sqlite::legacy_database_path(path, LEGACY_DIRMOVE_DB)
        } else {
            None
        };
        ensure_schema(&mut connection, legacy.as_deref())?;
        Ok(Self { connection })
    }

    /// Open the database at the default path read-only if it already exists, without creating or mutating anything.
    ///
    /// # Errors
    /// Returns an error if an existing database cannot be opened.
    pub fn open_existing() -> Result<Option<Self>> {
        let path = sqlite::database_path()?;
        Self::open_read_only(&path)
    }

    /// Open an in-memory database for testing.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let mut connection = sqlite::open_in_memory()?;
        ensure_schema(&mut connection, None)?;
        Ok(Self { connection })
    }

    /// Add or update a directory name in the database.
    pub fn upsert(&self, display_name: &str) -> Result<()> {
        let normalized_name = utils::normalize_name(display_name);

        self.connection
            .execute(
                r"
                INSERT INTO directory_names (normalized_name, display_name)
                VALUES (?1, ?2)
                ON CONFLICT(normalized_name) DO UPDATE SET display_name = excluded.display_name
                ",
                params![normalized_name, display_name],
            )
            .context("Failed to upsert directory name")?;

        Ok(())
    }

    /// Upsert multiple directory names in a single `IMMEDIATE` transaction, keeping display casing current.
    ///
    /// # Errors
    /// Returns an error if the transaction cannot be started or committed.
    pub fn batch_upsert(&mut self, display_names: &[String]) -> Result<usize> {
        if display_names.is_empty() {
            return Ok(0);
        }
        let tx = sqlite::immediate_transaction(&mut self.connection)?;
        let mut count = 0;
        for display_name in display_names {
            let normalized_name = utils::normalize_name(display_name);
            tx.execute(
                r"
                INSERT INTO directory_names (normalized_name, display_name)
                VALUES (?1, ?2)
                ON CONFLICT(normalized_name) DO UPDATE SET display_name = excluded.display_name
                ",
                params![normalized_name, display_name],
            )
            .context("Failed to upsert directory name")?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }

    /// Add a directory name only if it does not already exist.
    #[cfg(test)]
    pub fn insert_if_new(&self, display_name: &str) -> Result<bool> {
        let normalized_name = utils::normalize_name(display_name);

        let rows_affected = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO directory_names (normalized_name, display_name) VALUES (?1, ?2)",
                params![normalized_name, display_name],
            )
            .context("Failed to insert directory name")?;

        Ok(rows_affected > 0)
    }

    /// Remove a directory name by exact or normalized name.
    ///
    /// # Errors
    /// Returns an error if the delete query fails.
    pub fn remove(&self, name: &str) -> Result<bool> {
        let normalized_name = utils::normalize_name(name);
        let rows = self
            .connection
            .execute(
                "DELETE FROM directory_names WHERE normalized_name = ?1 OR display_name = ?2",
                params![normalized_name, name],
            )
            .context("Failed to remove directory name")?;
        Ok(rows > 0)
    }

    /// Clear all stored directory names.
    ///
    /// # Errors
    /// Returns an error if the delete query fails.
    pub fn clear(&self) -> Result<usize> {
        let rows = self
            .connection
            .execute("DELETE FROM directory_names", [])
            .context("Failed to clear directory names")?;
        Ok(rows)
    }

    /// Get all directory entries sorted by display name.
    pub fn get_all_entries(&self) -> Result<Vec<DirectoryEntry>> {
        if !sqlite::table_exists(&self.connection, "main", "directory_names")? {
            return Ok(Vec::new());
        }
        let mut statement = self
            .connection
            .prepare("SELECT normalized_name, display_name FROM directory_names ORDER BY display_name")
            .context("Failed to prepare query")?;

        let entries = statement
            .query_map([], |row| {
                Ok(DirectoryEntry {
                    normalized_name: row.get(0)?,
                    display_name: row.get(1)?,
                })
            })
            .context("Failed to execute query")?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to collect results")?;

        Ok(entries)
    }

    /// Get the stored display name for a directory name when its normalized form matches exactly.
    #[allow(clippy::doc_markdown)]
    pub fn exact_match_display_name(&self, directory_name: &str) -> Option<String> {
        if !sqlite::table_exists(&self.connection, "main", "directory_names").ok()? {
            return None;
        }
        self.connection
            .query_row(
                "SELECT display_name FROM directory_names WHERE normalized_name = ?1",
                params![utils::normalize_name(directory_name)],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Print where the database is and every stored name to standard error.
    pub fn print_debug_info(&self) {
        eprintln!("Database: {}", database_path().display());
        match self.get_all_entries() {
            Ok(entries) if entries.is_empty() => eprintln!("Database: empty"),
            Ok(entries) => {
                eprintln!("Database entries ({}):", entries.len());
                for entry in entries {
                    eprintln!("  {entry}");
                }
            }
            Err(error) => eprintln!("Failed to get database entries: {error}"),
        }
    }

    /// Get the count of directory names in the database.
    #[cfg(test)]
    pub fn count(&self) -> Result<u64> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM directory_names", [], |row| row.get(0))
            .context("Failed to count entries")?;

        #[allow(clippy::cast_sign_loss)]
        Ok(count as u64)
    }

    /// Open an existing database in read-only mode without creating files or running DDL.
    fn open_read_only(path: &Path) -> Result<Option<Self>> {
        let Some(connection) = sqlite::open_read_only_at(path)? else {
            return Ok(None);
        };
        Ok(Some(Self { connection }))
    }
}

impl std::fmt::Display for DirectoryEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name)
    }
}

/// Ensure the `directory_names` table exists and import legacy `dirmove.db` rows once.
fn ensure_schema(connection: &mut Connection, legacy_dirmove_db: Option<&Path>) -> Result<()> {
    let was_uninitialized = sqlite::current_component_version(connection, COMPONENT_NAME)? == 0;
    sqlite::migrate_component(connection, COMPONENT_NAME, SCHEMA_VERSION, |tx, current_version| {
        if current_version < 1 {
            tx.execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS directory_names (
                    id INTEGER PRIMARY KEY,
                    normalized_name TEXT NOT NULL UNIQUE,
                    display_name TEXT NOT NULL
                );
                DROP INDEX IF EXISTS idx_normalized_name;
                ",
            )
            .context("Failed to create directory_names table")?;
        }
        Ok(())
    })?;

    if was_uninitialized && let Some(legacy_path) = legacy_dirmove_db {
        sqlite::import_from_legacy_database(connection, legacy_path, |tx| {
            if sqlite::table_exists(tx, "legacy", "directory_names")? {
                tx.execute(
                    r"
                    INSERT OR IGNORE INTO main.directory_names (normalized_name, display_name)
                    SELECT normalized_name, display_name FROM legacy.directory_names
                    ",
                    [],
                )
                .context("Failed to import legacy directory_names rows")?;
            }
            Ok(())
        })?;
    }

    Ok(())
}

/// Print the number of directory names in the default database and every stored name.
///
/// A missing database is reported and not created.
///
/// # Errors
/// Returns an error if the database cannot be opened or its entries cannot be read.
pub fn print_entries() -> Result<()> {
    let Some(database) = Database::open_existing()? else {
        println!("No database at {}", database_path().display());
        return Ok(());
    };
    let entries = database.get_all_entries()?;
    println!("Database entries: {}", entries.len());
    for entry in entries {
        println!("  {}", entry.display_name);
    }
    Ok(())
}

/// Add a directory name to the default database.
///
/// # Errors
/// Returns an error if the database cannot be opened or updated.
pub fn add_entry(name: &str) -> Result<()> {
    let database = Database::open_default()?;
    database.upsert(name)?;
    println!("Saved '{name}' to database");
    Ok(())
}

/// Remove a directory name from the default database.
///
/// # Errors
/// Returns an error if the database cannot be opened or updated.
pub fn remove_entry(name: &str) -> Result<()> {
    let database = Database::open_default()?;
    if database.remove(name)? {
        println!("Removed '{name}' from database");
    } else {
        println!("No matching entry found for '{name}'");
    }
    Ok(())
}

/// Clear all directory names from the default database.
///
/// # Errors
/// Returns an error if the database cannot be opened or cleared.
pub fn clear_entries() -> Result<()> {
    let database = Database::open_default()?;
    let removed = database.clear()?;
    println!("Cleared {removed} entries from database");
    Ok(())
}

/// Resolve the unified database path for display.
fn database_path() -> PathBuf {
    sqlite::database_path().unwrap_or_else(|_| PathBuf::from(sqlite::DATABASE_FILENAME))
}

#[cfg(test)]
mod test_database {
    use super::*;

    #[test]
    fn open_in_memory() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.count().unwrap(), 0);
    }

    #[test]
    fn upsert_and_batch_upsert_update_display_casing() {
        let mut db = Database::open_in_memory().unwrap();

        db.upsert("my show").unwrap();
        db.batch_upsert(&["My Show".to_string(), "Another Show".to_string()])
            .unwrap();

        assert_eq!(db.count().unwrap(), 2);
        assert_eq!(db.exact_match_display_name("myshow"), Some("My Show".to_string()));
    }

    #[test]
    fn remove_and_clear_manage_entries() {
        let db = Database::open_in_memory().unwrap();
        db.upsert("Alpha").unwrap();
        db.upsert("Beta").unwrap();

        assert!(db.remove("alpha").unwrap());
        assert!(!db.remove("missing").unwrap());
        assert_eq!(db.count().unwrap(), 1);

        assert_eq!(db.clear().unwrap(), 1);
        assert_eq!(db.count().unwrap(), 0);
    }

    #[test]
    fn insert_if_new_preserves_original() {
        let db = Database::open_in_memory().unwrap();

        assert!(db.insert_if_new("New Name").unwrap());
        assert!(!db.insert_if_new("new name").unwrap());
        assert!(!db.insert_if_new("NewName").unwrap());

        let entries = db.get_all_entries().unwrap();
        assert_eq!(entries[0].display_name, "New Name");
        assert_eq!(entries[0].normalized_name, "newname");
    }

    #[test]
    fn migrates_legacy_dirmove_db_into_cli_tools_db() {
        let temp_dir = tempfile::tempdir().unwrap();
        let legacy_path = temp_dir.path().join("dirmove.db");
        let unified_path = temp_dir.path().join("cli-tools.db");

        let legacy = sqlite::open_at(&legacy_path).unwrap();
        legacy
            .execute_batch(
                r"
                CREATE TABLE directory_names (
                    id INTEGER PRIMARY KEY,
                    normalized_name TEXT NOT NULL UNIQUE,
                    display_name TEXT NOT NULL
                );
                INSERT INTO directory_names (normalized_name, display_name) VALUES ('janedoe', 'Jane Doe');
                ",
            )
            .unwrap();
        drop(legacy);

        let db = Database::open_at(&unified_path).unwrap();
        assert_eq!(db.exact_match_display_name("Jane.Doe"), Some("Jane Doe".to_string()));
        assert!(legacy_path.is_file());
    }

    #[test]
    fn open_read_only_reads_existing_entries_without_writing() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("cli-tools.db");
        {
            let writable = Database::open_at(&db_path).expect("create db");
            writable.upsert("The Best Show").expect("upsert");
        }

        let mut permissions = std::fs::metadata(&db_path).expect("meta").permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&db_path, permissions).expect("set readonly");

        let read_only = Database::open_read_only(&db_path)
            .expect("open read-only")
            .expect("existing db");
        let entries = read_only.get_all_entries().expect("read entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].display_name, "The Best Show");
        assert!(read_only.upsert("Blocked Write").is_err());

        let mut restore = std::fs::metadata(&db_path).expect("meta").permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        restore.set_readonly(false);
        let _ = std::fs::set_permissions(&db_path, restore);
    }
}
