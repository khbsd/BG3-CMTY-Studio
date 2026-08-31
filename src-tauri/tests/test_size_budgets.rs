//! Size-budget tests for staging DB and undo journal.
//!
//! These are integration tests (not benchmarks) that verify storage size
//! constraints are met.

use rusqlite::Connection;
use std::collections::HashMap;

use bg3_cmty_studio_lib::reference_db::schema::*;
use bg3_cmty_studio_lib::reference_db::staging;

// ---------------------------------------------------------------------------
// Helpers (duplicated from benches/staging.rs — kept simple, no shared crate)
// ---------------------------------------------------------------------------

fn test_columns() -> Vec<ColumnDef> {
    vec![
        ColumnDef {
            name: "Name".into(),
            bg3_type: "FixedString".into(),
            sqlite_type: "TEXT".into(),
        },
        ColumnDef {
            name: "DisplayName".into(),
            bg3_type: "TranslatedString".into(),
            sqlite_type: "TEXT".into(),
        },
        ColumnDef {
            name: "Description".into(),
            bg3_type: "TranslatedString".into(),
            sqlite_type: "TEXT".into(),
        },
        ColumnDef {
            name: "ParentGuid".into(),
            bg3_type: "guid".into(),
            sqlite_type: "TEXT".into(),
        },
        ColumnDef {
            name: "SpellCastingAbility".into(),
            bg3_type: "FixedString".into(),
            sqlite_type: "TEXT".into(),
        },
        ColumnDef {
            name: "ProgressionTableUUID".into(),
            bg3_type: "guid".into(),
            sqlite_type: "TEXT".into(),
        },
        ColumnDef {
            name: "Level".into(),
            bg3_type: "int32".into(),
            sqlite_type: "INTEGER".into(),
        },
        ColumnDef {
            name: "Flag".into(),
            bg3_type: "int32".into(),
            sqlite_type: "INTEGER".into(),
        },
        ColumnDef {
            name: "SoundInitEvent".into(),
            bg3_type: "FixedString".into(),
            sqlite_type: "TEXT".into(),
        },
        ColumnDef {
            name: "PassivesAdded".into(),
            bg3_type: "FixedString".into(),
            sqlite_type: "TEXT".into(),
        },
    ]
}

fn build_test_schema(n_tables: usize) -> DiscoveredSchema {
    let mut tables = HashMap::new();
    for i in 0..n_tables {
        let name = format!("lsx__ClassDescriptions__{i}");
        tables.insert(
            name.clone(),
            TableSchema {
                table_name: name.clone(),
                pk_strategy: PkStrategy::Uuid,
                source_type: "lsx".into(),
                region_id: Some("ClassDescriptions".into()),
                node_id: Some("ClassDescription".into()),
                columns: test_columns(),
                fk_constraints: Vec::new(),
                has_file_id: false,
                parent_tables: Default::default(),
            },
        );
    }
    DiscoveredSchema {
        tables,
        renames: HashMap::new(),
        region_node_ids: HashMap::new(),
        junction_tables: Vec::new(),
        junction_lookup: HashMap::new(),
    }
}

fn test_uuid(i: usize) -> String {
    format!(
        "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
        i,
        i & 0xFFFF,
        i & 0xFFF,
        i & 0xFFF,
        i
    )
}

fn test_row_columns(uuid: &str) -> HashMap<String, String> {
    let mut cols = HashMap::new();
    cols.insert("UUID".into(), uuid.into());
    cols.insert("Name".into(), format!("TestClass_{uuid}"));
    cols.insert("DisplayName".into(), "h00000000g0000".into());
    cols.insert("Description".into(), "h11111111g1111".into());
    cols.insert(
        "ParentGuid".into(),
        "00000000-0000-0000-0000-000000000000".into(),
    );
    cols.insert("SpellCastingAbility".into(), "Intelligence".into());
    cols.insert(
        "ProgressionTableUUID".into(),
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
    );
    cols.insert("Level".into(), "1".into());
    cols.insert("Flag".into(), "0".into());
    cols.insert("SoundInitEvent".into(), "".into());
    cols.insert("PassivesAdded".into(), "SomePassive;AnotherPassive".into());
    cols
}

/// Create a file-backed staging DB with embedded schema and data tables.
fn create_file_backed_staging_db(db_path: &std::path::Path, n_tables: usize) -> Connection {
    let schema = build_test_schema(n_tables);
    let conn = Connection::open(db_path).unwrap();

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA page_size = 8192;
         PRAGMA foreign_keys = OFF;",
    )
    .unwrap();

    conn.execute_batch(
        "CREATE TABLE _sources (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
         CREATE TABLE _source_files (
             file_id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL UNIQUE,
             file_type TEXT NOT NULL, mod_name TEXT, region_id TEXT,
             row_count INTEGER DEFAULT 0, file_size INTEGER
         );
         CREATE TABLE _column_types (
             table_name TEXT NOT NULL, column_name TEXT NOT NULL,
             bg3_type TEXT, sqlite_type TEXT NOT NULL,
             PRIMARY KEY(table_name, column_name)
         ) WITHOUT ROWID;
         CREATE TABLE _table_meta (
             table_name TEXT PRIMARY KEY, source_type TEXT NOT NULL,
             region_id TEXT, node_id TEXT, parent_tables TEXT,
             row_count INTEGER DEFAULT 0
         ) WITHOUT ROWID;
         CREATE TABLE _staging_authoring (
             key TEXT PRIMARY KEY, value TEXT NOT NULL,
             _is_new INTEGER NOT NULL DEFAULT 0,
             _is_modified INTEGER NOT NULL DEFAULT 0,
             _is_deleted INTEGER NOT NULL DEFAULT 0,
             _original_hash TEXT
         ) WITHOUT ROWID;
         CREATE TABLE _embedded_schema (
             key TEXT PRIMARY KEY, value BLOB NOT NULL
         ) WITHOUT ROWID;",
    )
    .unwrap();

    let blob = rmp_serde::to_vec(&schema).unwrap();
    conn.execute(
        "INSERT INTO _embedded_schema (key, value) VALUES ('schema', ?1)",
        rusqlite::params![blob],
    )
    .unwrap();

    for ts in schema.tables.values() {
        let pk_col = ts.pk_strategy.pk_column();
        let pk_type = ts.pk_strategy.pk_sql_type();
        let mut ddl = format!(
            "CREATE TABLE \"{}\" (\"{}\" {} PRIMARY KEY",
            ts.table_name, pk_col, pk_type
        );
        for col in &ts.columns {
            ddl.push_str(&format!(", \"{}\" {}", col.name, col.sqlite_type));
        }
        ddl.push_str(
            ", \"_is_modified\" INTEGER NOT NULL DEFAULT 0\
             , \"_is_new\" INTEGER NOT NULL DEFAULT 0\
             , \"_is_deleted\" INTEGER NOT NULL DEFAULT 0)",
        );
        conn.execute_batch(&ddl).unwrap();

        conn.execute(
            "INSERT INTO _table_meta (table_name, source_type, region_id, node_id) VALUES (?1,?2,?3,?4)",
            rusqlite::params![ts.table_name, ts.source_type, ts.region_id, ts.node_id],
        )
        .unwrap();
    }

    conn
}

fn create_inmemory_staging_db(n_tables: usize) -> Connection {
    let schema = build_test_schema(n_tables);
    let conn = Connection::open_in_memory().unwrap();

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA page_size = 8192;
         PRAGMA foreign_keys = OFF;",
    )
    .unwrap();

    conn.execute_batch(
        "CREATE TABLE _sources (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
         CREATE TABLE _source_files (
             file_id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL UNIQUE,
             file_type TEXT NOT NULL, mod_name TEXT, region_id TEXT,
             row_count INTEGER DEFAULT 0, file_size INTEGER
         );
         CREATE TABLE _column_types (
             table_name TEXT NOT NULL, column_name TEXT NOT NULL,
             bg3_type TEXT, sqlite_type TEXT NOT NULL,
             PRIMARY KEY(table_name, column_name)
         ) WITHOUT ROWID;
         CREATE TABLE _table_meta (
             table_name TEXT PRIMARY KEY, source_type TEXT NOT NULL,
             region_id TEXT, node_id TEXT, parent_tables TEXT,
             row_count INTEGER DEFAULT 0
         ) WITHOUT ROWID;
         CREATE TABLE _staging_authoring (
             key TEXT PRIMARY KEY, value TEXT NOT NULL,
             _is_new INTEGER NOT NULL DEFAULT 0,
             _is_modified INTEGER NOT NULL DEFAULT 0,
             _is_deleted INTEGER NOT NULL DEFAULT 0,
             _original_hash TEXT
         ) WITHOUT ROWID;
         CREATE TABLE _embedded_schema (
             key TEXT PRIMARY KEY, value BLOB NOT NULL
         ) WITHOUT ROWID;",
    )
    .unwrap();

    let blob = rmp_serde::to_vec(&schema).unwrap();
    conn.execute(
        "INSERT INTO _embedded_schema (key, value) VALUES ('schema', ?1)",
        rusqlite::params![blob],
    )
    .unwrap();

    for ts in schema.tables.values() {
        let pk_col = ts.pk_strategy.pk_column();
        let pk_type = ts.pk_strategy.pk_sql_type();
        let mut ddl = format!(
            "CREATE TABLE \"{}\" (\"{}\" {} PRIMARY KEY",
            ts.table_name, pk_col, pk_type
        );
        for col in &ts.columns {
            ddl.push_str(&format!(", \"{}\" {}", col.name, col.sqlite_type));
        }
        ddl.push_str(
            ", \"_is_modified\" INTEGER NOT NULL DEFAULT 0\
             , \"_is_new\" INTEGER NOT NULL DEFAULT 0\
             , \"_is_deleted\" INTEGER NOT NULL DEFAULT 0)",
        );
        conn.execute_batch(&ddl).unwrap();

        conn.execute(
            "INSERT INTO _table_meta (table_name, source_type, region_id, node_id) VALUES (?1,?2,?3,?4)",
            rusqlite::params![ts.table_name, ts.source_type, ts.region_id, ts.node_id],
        )
        .unwrap();
    }

    conn
}

// ---------------------------------------------------------------------------
// Size-budget tests
// ---------------------------------------------------------------------------

/// Verify staging DB size with 500 entries stays under 5 MB.
#[test]
fn test_db_size_500_entries() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("staging_bench.sqlite");
    let conn = create_file_backed_staging_db(&db_path, 5);

    // Insert 500 rows (100 per table × 5 tables)
    for t in 0..5 {
        let table = format!("lsx__ClassDescriptions__{t}");
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..100 {
            let uuid = test_uuid(t * 1000 + i);
            let cols = test_row_columns(&uuid);
            staging::staging_upsert_row(&tx, &table, &cols, true).unwrap();
        }
        tx.commit().unwrap();
    }

    drop(conn);

    // Check file sizes (main DB + WAL)
    let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let wal_path = db_path.with_extension("sqlite-wal");
    let wal_size = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    let total_bytes = db_size + wal_size;
    let total_mb = total_bytes as f64 / (1024.0 * 1024.0);

    eprintln!("DB size with 500 entries: {total_mb:.2} MB (db={db_size}, wal={wal_size})");
    assert!(
        total_mb < 5.0,
        "Staging DB with 500 entries is {total_mb:.2} MB — exceeds 5 MB budget"
    );
}

/// Verify undo journal stays under 1 MB after 1000 operations.
#[test]
fn test_undo_journal_size_1000_ops() {
    let conn = create_inmemory_staging_db(1);
    staging::ensure_undo_journal_table(&conn).unwrap();
    let table = "lsx__ClassDescriptions__0";

    // Insert 1000 rows (each records an undo journal entry)
    for i in 0..1000 {
        let uuid = test_uuid(i);
        let cols = test_row_columns(&uuid);
        staging::staging_upsert_row(&conn, table, &cols, true).unwrap();
    }

    // Query journal size: sum of all row content
    let journal_bytes: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(
                LENGTH(label) + LENGTH(table_name) + LENGTH(pk_value)
                + COALESCE(LENGTH(old_row), 0) + COALESCE(LENGTH(new_row), 0)
            ), 0) FROM _staging_undo_journal",
            [],
            |row| row.get(0),
        )
        .unwrap();

    let journal_mb = journal_bytes as f64 / (1024.0 * 1024.0);
    eprintln!("Undo journal after 1000 ops: {journal_mb:.3} MB ({journal_bytes} bytes)");
    assert!(
        journal_mb < 1.0,
        "Undo journal is {journal_mb:.3} MB — exceeds 1 MB budget"
    );
}
