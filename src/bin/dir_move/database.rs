//! `SQLite` database operations for `dir_move`.
//!
//! Stores directory names that have been used or seen, to use as primary candidates
//! for group names when creating new directories.

use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

/// Default database filename.
const DATABASE_FILENAME: &str = "dirmove.db";

/// Database wrapper for directory name operations.
pub struct Database {
    connection: Connection,
}

/// A directory name entry from the database.
#[derive(Debug, Clone)]
pub struct DirectoryEntry {
    /// Normalized name for lookups (lowercase, no spaces).
    pub normalized_name: String,
    /// Display name with original casing and spacing.
    pub display_name: String,
}

impl Database {
    /// Print debug information about the database contents.
    pub fn print_debug_info(&self) {
        eprintln!("Database: {}", Self::path().display());
        match self.get_all_entries() {
            Ok(entries) => {
                if entries.is_empty() {
                    eprintln!("Database: empty");
                } else {
                    eprintln!("Database entries ({}):", entries.len());
                    for entry in entries {
                        eprintln!("  {entry}");
                    }
                }
            }
            Err(e) => {
                eprintln!("Failed to get database entries: {e}");
            }
        }
    }

    /// Get the database path for display purposes.
    pub fn path() -> PathBuf {
        Self::database_path()
    }

    /// Open or create the database at the default path.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or initialized.
    pub fn open_default() -> Result<Self> {
        let path = Self::database_path();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create database directory: {}", parent.display()))?;
        }

        let connection =
            Connection::open(&path).with_context(|| format!("Failed to open database: {}", path.display()))?;

        // Set busy timeout for concurrent access (5 seconds)
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .context("Failed to set busy timeout")?;

        let database = Self { connection };
        database.initialize()?;

        Ok(database)
    }

    /// Add a directory name to the database.
    /// If a name with the same normalized form already exists, updates the display name.
    pub fn upsert(&self, display_name: &str) -> Result<()> {
        let normalized_name = normalize_name(display_name);

        self.connection
            .execute(
                r"
                INSERT INTO directory_names (normalized_name, display_name)
                VALUES (?1, ?2)
                ON CONFLICT(normalized_name) DO UPDATE SET display_name = ?2
                ",
                params![normalized_name, display_name],
            )
            .context("Failed to upsert directory name")?;

        Ok(())
    }

    /// Add a directory name only if it doesn't already exist.
    /// Returns true if inserted, false if already existed.
    pub fn insert_if_new(&self, display_name: &str) -> Result<bool> {
        let normalized_name = normalize_name(display_name);

        let rows_affected = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO directory_names (normalized_name, display_name) VALUES (?1, ?2)",
                params![normalized_name, display_name],
            )
            .context("Failed to insert directory name")?;

        Ok(rows_affected > 0)
    }

    /// Get all directory entries sorted by display name.
    pub fn get_all_entries(&self) -> Result<Vec<DirectoryEntry>> {
        let mut stmt = self
            .connection
            .prepare("SELECT normalized_name, display_name FROM directory_names ORDER BY display_name")
            .context("Failed to prepare query")?;

        let entries = stmt
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

    /// Get the count of directory names in the database.
    #[allow(unused)]
    pub fn count(&self) -> Result<u64> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM directory_names", [], |row| row.get(0))
            .context("Failed to count entries")?;

        #[allow(clippy::cast_sign_loss)]
        Ok(count as u64)
    }

    /// Get the database path.
    ///
    /// Uses the platform-specific local data directory:
    /// - Windows: `%LOCALAPPDATA%\cli-tools\dirmove.db`
    /// - macOS: `~/Library/Application Support/cli-tools/dirmove.db`
    /// - Linux: `~/.local/share/cli-tools/dirmove.db`
    fn database_path() -> PathBuf {
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("cli-tools");
        data_dir.join(DATABASE_FILENAME)
    }

    /// Open an in-memory database for testing.
    #[cfg(test)]
    fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory().context("Failed to open in-memory database")?;

        let database = Self { connection };
        database.initialize()?;

        Ok(database)
    }

    /// Initialize the database schema.
    fn initialize(&self) -> Result<()> {
        self.connection
            .execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS directory_names (
                    id INTEGER PRIMARY KEY,
                    normalized_name TEXT NOT NULL UNIQUE,
                    display_name TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_normalized_name ON directory_names(normalized_name);

                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                PRAGMA cache_size = -2000;
                ",
            )
            .context("Failed to initialize database schema")?;

        Ok(())
    }
}

/// Normalize a directory name for storage and comparison.
/// Converts to lowercase and removes spaces.
pub fn normalize_name(name: &str) -> String {
    name.to_lowercase().replace(' ', "")
}

impl std::fmt::Display for DirectoryEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.count().unwrap(), 0);
    }

    #[test]
    fn upsert_new_entry() {
        let db = Database::open_in_memory().unwrap();

        db.upsert("Test Directory").unwrap();

        assert_eq!(db.count().unwrap(), 1);

        let entries = db.get_all_entries().unwrap();
        assert_eq!(entries[0].display_name, "Test Directory");
        assert_eq!(entries[0].normalized_name, "testdirectory");
    }

    #[test]
    fn upsert_updates_display_name() {
        let db = Database::open_in_memory().unwrap();

        db.upsert("my show").unwrap();
        db.upsert("My Show").unwrap();
        db.upsert("MY SHOW").unwrap();

        // Only one entry exists
        assert_eq!(db.count().unwrap(), 1);

        // Display name is updated to latest
        let entries = db.get_all_entries().unwrap();
        assert_eq!(entries[0].display_name, "MY SHOW");
    }

    #[test]
    fn insert_if_new_preserves_original() {
        let db = Database::open_in_memory().unwrap();

        assert!(db.insert_if_new("New Name").unwrap());
        assert!(!db.insert_if_new("new name").unwrap());
        assert!(!db.insert_if_new("NEW NAME").unwrap());
        assert!(!db.insert_if_new("NewName").unwrap());

        // Display name is preserved from first insert
        let entries = db.get_all_entries().unwrap();
        assert_eq!(entries[0].display_name, "New Name");
    }

    #[test]
    fn get_all_entries_sorted() {
        let db = Database::open_in_memory().unwrap();

        db.upsert("Zebra").unwrap();
        db.upsert("Alpha").unwrap();
        db.upsert("Middle").unwrap();

        let entries = db.get_all_entries().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].display_name, "Alpha");
        assert_eq!(entries[1].display_name, "Middle");
        assert_eq!(entries[2].display_name, "Zebra");
    }

    #[test]
    fn normalized_name_removes_spaces() {
        let db = Database::open_in_memory().unwrap();

        db.upsert("Some Name Here").unwrap();

        let entries = db.get_all_entries().unwrap();
        assert_eq!(entries[0].normalized_name, "somenamehere");
        assert_eq!(entries[0].display_name, "Some Name Here");
    }

    #[test]
    fn different_spacing_same_entry() {
        let db = Database::open_in_memory().unwrap();

        db.upsert("MyShow").unwrap();
        db.upsert("My Show").unwrap();

        // Both normalize to "myshow", so only one entry
        assert_eq!(db.count().unwrap(), 1);
    }

    #[test]
    fn normalize_name_function() {
        assert_eq!(normalize_name("Hello World"), "helloworld");
        assert_eq!(normalize_name("UPPER CASE"), "uppercase");
        assert_eq!(normalize_name("NoSpaces"), "nospaces");
        assert_eq!(normalize_name("  extra  spaces  "), "extraspaces");
    }

    #[test]
    fn display_entry() {
        let entry = DirectoryEntry {
            normalized_name: "test".to_string(),
            display_name: "Test".to_string(),
        };
        assert_eq!(format!("{entry}"), "Test");
    }
}
