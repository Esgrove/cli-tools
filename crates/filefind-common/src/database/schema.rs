//! Versioned schema of the filefind index database.
//!
//! The schema is a `cli-tools-db` migration component,
//! so tools outside filefind that update the index can check the schema version they write to.
//! Version 1 is the schema filefind created before it was versioned,
//! so migrating an older database only records the version and keeps its entries.

use anyhow::{Context, Result};
use rusqlite::{Connection, Transaction};

/// Migration component name of the index schema in the `schema_migrations` table.
pub const INDEX_SCHEMA_COMPONENT: &str = "filefind_index";

/// Current version of the index schema.
pub const INDEX_SCHEMA_VERSION: i64 = 1;

/// Apply the connection settings and migrate the index schema to the current version.
///
/// # Errors
/// Returns an error if a pragma or a schema migration fails.
pub(super) fn initialize_schema(connection: &mut Connection) -> Result<()> {
    // Journal mode cannot change inside a transaction, so the pragmas run before the migration.
    connection
        .execute_batch(
            r"
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA cache_size = -64000;
            ",
        )
        .context("Failed to configure the index database connection")?;

    cli_tools_db::sqlite::migrate_component(
        connection,
        INDEX_SCHEMA_COMPONENT,
        INDEX_SCHEMA_VERSION,
        |transaction, from_version| {
            if from_version < 1 {
                create_version_1(transaction)?;
            }
            Ok(())
        },
    )
}

/// Create the volumes and files tables with their indexes.
///
/// Every statement uses `IF NOT EXISTS`, so an unversioned database from before the migrations keeps its data.
fn create_version_1(transaction: &Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(
            r"
            -- Indexed volumes/drives
            CREATE TABLE IF NOT EXISTS volumes (
                id INTEGER PRIMARY KEY,
                serial_number TEXT NOT NULL UNIQUE,
                label TEXT,
                mount_point TEXT NOT NULL,
                volume_type TEXT NOT NULL,
                last_scan_time INTEGER,
                last_usn INTEGER,
                is_online INTEGER NOT NULL DEFAULT 1
            );

            -- Indexed files and directories
            CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY,
                volume_id INTEGER NOT NULL,
                parent_id INTEGER,
                name TEXT NOT NULL,
                full_path TEXT NOT NULL,
                is_directory INTEGER NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                created_time INTEGER,
                modified_time INTEGER,
                mft_reference INTEGER,
                FOREIGN KEY (volume_id) REFERENCES volumes(id) ON DELETE CASCADE
            );

            -- Indexes for fast searching
            CREATE UNIQUE INDEX IF NOT EXISTS idx_files_path ON files(full_path);
            CREATE INDEX IF NOT EXISTS idx_files_name ON files(name COLLATE NOCASE);
            CREATE INDEX IF NOT EXISTS idx_files_volume ON files(volume_id);
            CREATE INDEX IF NOT EXISTS idx_files_parent ON files(parent_id);
            CREATE INDEX IF NOT EXISTS idx_files_is_directory ON files(is_directory);
            CREATE INDEX IF NOT EXISTS idx_files_modified ON files(modified_time);
            CREATE INDEX IF NOT EXISTS idx_files_volume_mft ON files(volume_id, mft_reference);
            ",
        )
        .context("Failed to create the index schema")
}
