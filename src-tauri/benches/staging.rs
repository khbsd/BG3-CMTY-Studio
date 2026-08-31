//! Performance benchmarks for staging DB and export pipeline.
//!
//! Run with: `cd src-tauri && cargo bench`
//!
//! Performance targets:
//!   - Single row UPSERT (direct DB, not IPC): < 5 ms
//!   - Batch 100 toggles:                      < 50 ms
//!   - projectStore hydration (500 entries):    < 100 ms
//!   - Export plan (all handlers, 1000 entries): < 200 ms
//!   - Full save (50 files, 5 handlers):        < 2 s
//!   - Staging DB size (500 entries):           < 5 MB
//!   - Undo journal (1000 operations):          < 1 MB

use criterion::{criterion_group, criterion_main, Criterion};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::PathBuf;

use bg3_cmty_studio_lib::export::{build_export_plan, default_handlers, ExportContext};
use bg3_cmty_studio_lib::reference_db::schema::*;
use bg3_cmty_studio_lib::reference_db::staging;

// ---------------------------------------------------------------------------
// Helpers — build a minimal embedded schema and staging DB in-memory
// ---------------------------------------------------------------------------

/// Column definitions for a typical LSX data table (~10 columns).
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

/// Build a minimal `DiscoveredSchema` with `n_tables` LSX tables.
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

/// Create an in-memory staging DB with `n_tables` data tables and the required
/// meta infrastructure (embedded schema, meta tables, undo journal).
fn create_bench_staging_db(n_tables: usize) -> Connection {
    let schema = build_test_schema(n_tables);
    let conn = Connection::open_in_memory().unwrap();

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA page_size = 8192;
         PRAGMA foreign_keys = OFF;",
    )
    .unwrap();

    // Meta tables
    conn.execute_batch(
        "CREATE TABLE _sources (
            id   INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE
        );
        CREATE TABLE _source_files (
            file_id   INTEGER PRIMARY KEY AUTOINCREMENT,
            path      TEXT NOT NULL UNIQUE,
            file_type TEXT NOT NULL,
            mod_name  TEXT,
            region_id TEXT,
            row_count INTEGER DEFAULT 0,
            file_size INTEGER
        );
        CREATE TABLE _column_types (
            table_name  TEXT NOT NULL,
            column_name TEXT NOT NULL,
            bg3_type    TEXT,
            sqlite_type TEXT NOT NULL,
            PRIMARY KEY (table_name, column_name)
        ) WITHOUT ROWID;
        CREATE TABLE _table_meta (
            table_name    TEXT PRIMARY KEY,
            source_type   TEXT NOT NULL,
            region_id     TEXT,
            node_id       TEXT,
            parent_tables TEXT,
            row_count     INTEGER DEFAULT 0
        ) WITHOUT ROWID;
        CREATE TABLE _staging_authoring (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            _is_new INTEGER NOT NULL DEFAULT 0,
            _is_modified INTEGER NOT NULL DEFAULT 0,
            _is_deleted INTEGER NOT NULL DEFAULT 0,
            _original_hash TEXT
        ) WITHOUT ROWID;
        CREATE TABLE _embedded_schema (
            key   TEXT PRIMARY KEY,
            value BLOB NOT NULL
        ) WITHOUT ROWID;",
    )
    .unwrap();

    // Embed the schema blob
    let blob = rmp_serde::to_vec(&schema).unwrap();
    conn.execute(
        "INSERT INTO _embedded_schema (key, value) VALUES ('schema', ?1)",
        rusqlite::params![blob],
    )
    .unwrap();

    // Create data tables matching the schema
    for ts in schema.tables.values() {
        let pk_col = ts.pk_strategy.pk_column();
        let pk_type = ts.pk_strategy.pk_sql_type();

        let mut ddl = format!(
            "CREATE TABLE \"{}\" (\n  \"{}\" {} PRIMARY KEY",
            ts.table_name, pk_col, pk_type
        );
        for col in &ts.columns {
            ddl.push_str(&format!(",\n  \"{}\" {}", col.name, col.sqlite_type));
        }
        ddl.push_str(
            ",\n  \"_is_modified\" INTEGER NOT NULL DEFAULT 0\
             ,\n  \"_is_new\" INTEGER NOT NULL DEFAULT 0\
             ,\n  \"_is_deleted\" INTEGER NOT NULL DEFAULT 0\
             )",
        );
        conn.execute_batch(&ddl).unwrap();

        // Register in _table_meta
        conn.execute(
            "INSERT INTO _table_meta (table_name, source_type, region_id, node_id) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                ts.table_name,
                ts.source_type,
                ts.region_id,
                ts.node_id,
            ],
        )
        .unwrap();
    }

    conn
}

/// Generate a UUID-like string for test data.
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

/// Build a column map for a single upsert with ~10 columns.
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

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// Benchmark: single row UPSERT into staging table.
/// Target: < 5 ms
fn bench_single_upsert(c: &mut Criterion) {
    let conn = create_bench_staging_db(1);
    // Ensure undo journal exists so we benchmark with it active
    staging::ensure_undo_journal_table(&conn).unwrap();
    let table = "lsx__ClassDescriptions__0";
    let mut i = 0usize;

    c.bench_function("single_upsert", |b| {
        b.iter(|| {
            let uuid = test_uuid(i);
            let cols = test_row_columns(&uuid);
            staging::staging_upsert_row(&conn, table, &cols, true).unwrap();
            i += 1;
        });
    });
}

/// Benchmark: batch 100 mark-deleted toggles in a single transaction.
/// Target: < 50 ms
fn bench_batch_100_toggles(c: &mut Criterion) {
    let conn = create_bench_staging_db(1);
    staging::ensure_undo_journal_table(&conn).unwrap();

    // Pre-populate 100 rows (not _is_new, so mark_deleted does soft delete)
    let table = "lsx__ClassDescriptions__0";
    {
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..100 {
            let uuid = test_uuid(i);
            let cols = test_row_columns(&uuid);
            staging::staging_upsert_row(&tx, table, &cols, false).unwrap();
        }
        tx.commit().unwrap();
    }

    let pks: Vec<String> = (0..100).map(test_uuid).collect();

    c.bench_function("batch_100_toggles", |b| {
        b.iter(|| {
            // Build batch operations: mark all 100 as deleted
            let ops: Vec<staging::StagingOperation> = pks
                .iter()
                .map(|pk| staging::StagingOperation::MarkDeleted {
                    table: table.to_string(),
                    pk: pk.clone(),
                })
                .collect();
            staging::staging_batch_write(&conn, &ops).unwrap();

            // Un-delete them so the next iteration can re-toggle
            let undelete_ops: Vec<staging::StagingOperation> = pks
                .iter()
                .map(|pk| staging::StagingOperation::UnmarkDeleted {
                    table: table.to_string(),
                    pk: pk.clone(),
                })
                .collect();
            staging::staging_batch_write(&conn, &undelete_ops).unwrap();
        });
    });
}

/// Benchmark: list sections + query 500 entries (hydration simulation).
/// Target: < 100 ms
fn bench_hydration_500(c: &mut Criterion) {
    // Create 5 tables × 100 entries each = 500 entries
    let conn = create_bench_staging_db(5);
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

    c.bench_function("hydration_500_entries", |b| {
        b.iter(|| {
            let sections = staging::staging_list_sections(&conn).unwrap();
            for section in &sections {
                staging::staging_query_section(&conn, &section.table_name, false).unwrap();
            }
        });
    });
}

/// Benchmark: build an export plan from 1000 entries across lsx tables.
/// Target: < 200 ms
fn bench_export_plan_1000(c: &mut Criterion) {
    // ExportContext takes ownership of the Connection, so we recreate per
    // iteration via iter_batched. The setup cost is excluded from timing.
    c.bench_function("export_plan_1000_entries", |b| {
        b.iter_batched(
            || {
                // Setup: create a populated DB each time (ExportContext consumes conn)
                let conn = create_bench_staging_db(10);
                for t in 0..10 {
                    let table = format!("lsx__ClassDescriptions__{t}");
                    let tx = conn.unchecked_transaction().unwrap();
                    for i in 0..100 {
                        let uuid = test_uuid(t * 1000 + i);
                        let cols = test_row_columns(&uuid);
                        staging::staging_upsert_row(&tx, &table, &cols, true).unwrap();
                    }
                    tx.commit().unwrap();
                }

                let ctx = ExportContext {
                    staging_conn: conn,
                    ref_base_path: PathBuf::from("__nonexistent_ref_base.sqlite"),
                    mod_path: PathBuf::from("__nonexistent_mod"),
                    mod_name: "BenchMod".to_string(),
                    mod_folder: "BenchModFolder".to_string(),
                };
                let handlers = default_handlers();
                (ctx, handlers)
            },
            |(ctx, handlers)| {
                build_export_plan(&ctx, &handlers).unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

// ---------------------------------------------------------------------------
// Criterion groups
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_single_upsert,
    bench_batch_100_toggles,
    bench_hydration_500,
    bench_export_plan_1000,
);
criterion_main!(benches);
