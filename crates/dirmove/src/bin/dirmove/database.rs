//! `SQLite` database operations for `dirmove`.
//!
//! Stores directory names that have been used or seen,
//! to use as primary candidates for group names when creating new directories.

use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use cli_tools_dirmove::utils;

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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only the tests check the stored normalized form")
    )]
    pub normalized_name: String,
    /// Display name with original casing and spacing.
    pub display_name: String,
}

impl Database {
    /// Open or create the database at the default path.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or initialized.
    pub fn open_default() -> Result<Self> {
        let path = database_path();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create database directory: {}", parent.display()))?;
        }

        Self::open(&path)
    }

    /// Open the database at the default path read-only if it already exists, without creating or mutating anything.
    ///
    /// # Errors
    /// Returns an error if an existing database cannot be opened.
    pub fn open_existing() -> Result<Option<Self>> {
        let path = database_path();
        if !path.exists() {
            return Ok(None);
        }
        Self::open_read_only(&path).map(Some)
    }

    /// Open an in-memory database for testing.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory().context("Failed to open in-memory database")?;

        let database = Self { connection };
        database.initialize()?;

        Ok(database)
    }

    /// Add a directory name to the database.
    /// If a name with the same normalized form already exists, updates the display name.
    pub fn upsert(&self, display_name: &str) -> Result<()> {
        let normalized_name = utils::normalize_name(display_name);

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

    /// Get all directory entries sorted by display name.
    pub fn get_all_entries(&self) -> Result<Vec<DirectoryEntry>> {
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

    /// Get the stored display name for a directory name, but only if it's an exact match.
    ///
    /// An "exact match" means the same letters (case-insensitive), but may differ in:
    /// - Whitespace (e.g., "JaneDoe" vs "Jane Doe")
    /// - Dots (e.g., "Jane.Doe" vs "Jane Doe")
    /// - Case (e.g., "JANE DOE" vs "Jane Doe")
    ///
    /// This maps a grouping name to the database's preferred formatting,
    /// only when they represent the same entity.
    #[allow(clippy::doc_markdown)]
    pub fn exact_match_display_name(&self, directory_name: &str) -> Option<String> {
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

    /// Open an existing database in read-only mode without running DDL or WAL pragmas.
    fn open_read_only(path: &std::path::Path) -> Result<Self> {
        let connection =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
                .with_context(|| format!("Failed to open database read-only: {}", path.display()))?;

        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .context("Failed to set busy timeout")?;

        Ok(Self { connection })
    }

    /// Open and initialize the database at `path`.
    fn open(path: &std::path::Path) -> Result<Self> {
        let connection =
            Connection::open(path).with_context(|| format!("Failed to open database: {}", path.display()))?;

        // Set busy timeout for concurrent access (5 seconds)
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .context("Failed to set busy timeout")?;

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

impl std::fmt::Display for DirectoryEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name)
    }
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

/// Get the database path.
///
/// Uses the platform-specific local data directory:
/// - Windows: `%LOCALAPPDATA%\cli-tools\dirmove.db`
/// - macOS: `~/Library/Application Support/cli-tools/dirmove.db`
/// - Linux: `~/.local/share/cli-tools/dirmove.db`
fn database_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cli-tools")
        .join(DATABASE_FILENAME)
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
    fn dotted_and_spaced_same_entry() {
        let db = Database::open_in_memory().unwrap();

        db.upsert("Jane Doe").unwrap();

        // All these variations should map to the same entry
        assert!(!db.insert_if_new("JaneDoe").unwrap());
        assert!(!db.insert_if_new("Jane.Doe").unwrap());
        assert!(!db.insert_if_new("JANE DOE").unwrap());
        assert!(!db.insert_if_new("jane.doe").unwrap());

        // Only one entry exists
        assert_eq!(db.count().unwrap(), 1);

        // Display name is preserved from first insert
        let entries = db.get_all_entries().unwrap();
        assert_eq!(entries[0].display_name, "Jane Doe");
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

#[cfg(test)]
mod test_exact_match_display_name {
    use super::*;

    /// Create an in-memory database containing the given entries.
    fn database_with_entries(entries: &[&str]) -> Database {
        let database = Database::open_in_memory().unwrap();
        for entry in entries {
            database.upsert(entry).unwrap();
        }
        database
    }

    #[test]
    fn exact_match_same_case() {
        let database = database_with_entries(&["Jane Doe"]);
        let suggestion = database.exact_match_display_name("Jane Doe");
        assert_eq!(suggestion, Some("Jane Doe".to_string()));
    }

    #[test]
    fn exact_match_different_case() {
        let database = database_with_entries(&["Jane Doe"]);
        let suggestion = database.exact_match_display_name("JANE DOE");
        assert_eq!(suggestion, Some("Jane Doe".to_string()));
    }

    #[test]
    fn exact_match_no_spaces() {
        let database = database_with_entries(&["Jane Doe"]);
        let suggestion = database.exact_match_display_name("JaneDoe");
        assert_eq!(suggestion, Some("Jane Doe".to_string()));
    }

    #[test]
    fn exact_match_with_dots() {
        let database = database_with_entries(&["Jane Doe"]);
        let suggestion = database.exact_match_display_name("Jane.Doe");
        assert_eq!(suggestion, Some("Jane Doe".to_string()));
    }

    #[test]
    fn exact_match_dots_and_different_case() {
        let database = database_with_entries(&["Jane Doe"]);
        let suggestion = database.exact_match_display_name("jane.doe");
        assert_eq!(suggestion, Some("Jane Doe".to_string()));
    }

    #[test]
    fn no_match_for_prefix_only() {
        // "Jane" should NOT match "Jane Doe" - they are different entities
        let database = database_with_entries(&["Jane Doe"]);
        let suggestion = database.exact_match_display_name("Jane");
        assert_eq!(suggestion, None);
    }

    #[test]
    fn no_match_for_longer_name() {
        // "Jane Doe Extra" should NOT match "Jane Doe"
        let database = database_with_entries(&["Jane Doe"]);
        let suggestion = database.exact_match_display_name("Jane Doe Extra");
        assert_eq!(suggestion, None);
    }

    #[test]
    fn no_match_for_partial_overlap() {
        // "Jan" should NOT match "Jane Doe"
        let database = database_with_entries(&["Jane Doe"]);
        let suggestion = database.exact_match_display_name("Jan");
        assert_eq!(suggestion, None);
    }

    #[test]
    fn no_match_for_completely_different() {
        let database = database_with_entries(&["Jane Doe"]);
        let suggestion = database.exact_match_display_name("Bob Wilson");
        assert_eq!(suggestion, None);
    }

    #[test]
    fn multiple_entries_exact_match() {
        let database = database_with_entries(&["Jane Doe", "John Smith", "Bob Wilson"]);

        assert_eq!(
            database.exact_match_display_name("JohnSmith"),
            Some("John Smith".to_string())
        );
        assert_eq!(
            database.exact_match_display_name("bob.wilson"),
            Some("Bob Wilson".to_string())
        );
        assert_eq!(database.exact_match_display_name("Alice"), None);
    }

    #[test]
    fn empty_database_returns_none() {
        let database = database_with_entries(&[]);
        let suggestion = database.exact_match_display_name("Anything");
        assert_eq!(suggestion, None);
    }

    #[test]
    fn preserves_database_formatting() {
        // Database has specific formatting that should be preserved
        let database = database_with_entries(&["The Best Show"]);

        // All these variations should return the database's formatting
        assert_eq!(
            database.exact_match_display_name("thebestshow"),
            Some("The Best Show".to_string())
        );
        assert_eq!(
            database.exact_match_display_name("The.Best.Show"),
            Some("The Best Show".to_string())
        );
        assert_eq!(
            database.exact_match_display_name("THE BEST SHOW"),
            Some("The Best Show".to_string())
        );
    }

    #[test]
    fn open_read_only_reads_existing_entries_without_writing() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("dirmove.db");
        {
            let writable = Database::open(&db_path).expect("create db");
            writable.upsert("The Best Show").expect("upsert");
        }

        let mut permissions = std::fs::metadata(&db_path).expect("meta").permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&db_path, permissions).expect("set readonly");

        let read_only = Database::open_read_only(&db_path).expect("open read-only");
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
