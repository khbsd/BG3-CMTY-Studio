//! Pass 2: Create tables and insert data into SQLite.

use crate::models::{LsxNode, LsxNodeAttribute, LsxResource};
use crate::parsers::loca as loca_parser;
use crate::parsers::{lsf as lsf_parser, lsfx as lsfx_parser, lsx as lsx_parser};
use crate::reference_db::cross_db::{self, ALIAS_BASE};
use crate::reference_db::discovery::is_stats_loca_column;
use crate::reference_db::schema::*;
use crate::reference_db::types::{self, SqlValue};
use crate::reference_db::{
    adaptive_pipeline_params, sanitize_id, should_use_in_memory, BuildOptions, FileEntry, FileType,
    PhaseTimes, SKIP_REGIONS,
};
use rusqlite::{params, Connection, Transaction};
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::LazyLock;
use std::time::Instant;

/// Sentinel values that should be treated as NULL for FK columns.
const NULL_UUID: &str = "00000000-0000-0000-0000-000000000000";
const NULL_TRANSLATED_STRING: &str = "ls::TranslatedStringRepository::s_HandleUnknown";

static ROW_ERROR_LOG_LIMIT: LazyLock<Option<usize>> = LazyLock::new(|| {
    std::env::var("BG3_REFDB_LOG_ROW_ERRORS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|limit| *limit > 0)
});
static ROW_ERROR_LOGGED: AtomicUsize = AtomicUsize::new(0);

/// Coerce a value to NULL if it's empty or a known sentinel for FK columns.
fn coerce_fk_null(value: &str) -> Option<&str> {
    if value.is_empty()
        || value == NULL_UUID
        || value == NULL_TRANSLATED_STRING
        || value == "EMPTY"
        || value == "NO_LEVELTEMPLATE_TRANSFER"
    {
        None
    } else {
        Some(value)
    }
}

fn maybe_log_row_error(
    file_path: &str,
    table_name: &str,
    error: &str,
    attrs: &[(String, String, String)],
) {
    let Some(limit) = *ROW_ERROR_LOG_LIMIT else {
        return;
    };

    let idx = ROW_ERROR_LOGGED.fetch_add(1, Ordering::Relaxed);
    if idx >= limit {
        return;
    }

    let sample_attrs = attrs
        .iter()
        .take(6)
        .map(|(name, _, value)| {
            let clipped = if value.len() > 80 {
                format!("{}...", &value[..80])
            } else {
                value.clone()
            };
            format!("{name}={clipped}")
        })
        .collect::<Vec<_>>()
        .join(", ");

    eprintln!(
        "  ROW ERROR [{} / {}] {} :: {} :: {}{}",
        idx + 1,
        limit,
        file_path,
        table_name,
        error,
        if sample_attrs.is_empty() {
            String::new()
        } else {
            format!(" :: attrs [{sample_attrs}]")
        }
    );
}

/// Result of the build phase.
pub struct BuildResult {
    pub total_rows: usize,
    pub total_tables: usize,
    pub fk_constraints: usize,
    pub file_errors: usize,
    pub row_errors: usize,
    pub phase_times: PhaseTimes,
}

// ---------------------------------------------------------------------------
// Schema DB creation (dev tool) & loading (app runtime)
// ---------------------------------------------------------------------------

/// Create an empty schema database with all DDL applied and the schema
/// serialized as a MessagePack blob.  Used by the `generate_schema` dev tool.
pub fn create_schema_db(db_path: &Path, schema: &DiscoveredSchema) -> Result<(), String> {
    let conn = Connection::open(db_path).map_err(|e| format!("Cannot open DB: {e}"))?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA page_size = 8192;",
    )
    .map_err(|e| format!("Pragma error: {e}"))?;

    // Wrap all DDL + metadata in a single transaction to avoid per-statement
    // fsyncs.  Without this, ~5500 individual commits each trigger an fsync
    // which can take 400+ seconds on spinning disks.
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("Begin transaction: {e}"))?;

    // Meta tables
    create_meta_tables(&tx)?;

    // Schema storage table
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS _embedded_schema (
            key   TEXT PRIMARY KEY,
            value BLOB NOT NULL
         ) WITHOUT ROWID;",
    )
    .map_err(|e| format!("Create _embedded_schema: {e}"))?;

    // Data tables
    let mut fk_count = 0;
    for ts in schema.tables.values() {
        fk_count += create_data_table(&tx, ts, schema)?;
    }

    // Junction tables
    for jt in &schema.junction_tables {
        let ddl = format!(
            "CREATE TABLE IF NOT EXISTS \"{}\" (\n\
             \x20   parent_id {} NOT NULL REFERENCES \"{}\"(\"{}\"),\n\
             \x20   child_id {} NOT NULL REFERENCES \"{}\"(\"{}\"),\n\
             \x20   PRIMARY KEY (parent_id, child_id)\n\
             ) WITHOUT ROWID",
            jt.table_name,
            jt.parent_pk_type,
            jt.parent_table,
            jt.parent_pk_column,
            jt.child_pk_type,
            jt.child_table,
            jt.child_pk_column,
        );
        tx.execute_batch(&ddl)
            .map_err(|e| format!("Create junction '{}': {}", jt.table_name, e))?;
    }

    // Store schema blob
    save_schema_to_db(&tx, schema)?;

    // Populate column types (static metadata — store at schema-creation time)
    populate_column_types(&tx, schema)?;

    tx.commit().map_err(|e| format!("Commit schema DDL: {e}"))?;

    eprintln!(
        "  Schema DB created: {} tables, {} junctions, {} FK constraints",
        schema.tables.len(),
        schema.junction_tables.len(),
        fk_count,
    );

    // VACUUM the empty DB (tiny — very fast)
    conn.execute_batch("PRAGMA journal_mode = DELETE; VACUUM;")
        .map_err(|e| format!("Vacuum schema DB: {e}"))?;

    conn.close()
        .map_err(|(_conn, e)| format!("Close error: {e}"))?;
    Ok(())
}

/// Serialize and store the discovered schema in the `_embedded_schema` table.
fn save_schema_to_db(conn: &Connection, schema: &DiscoveredSchema) -> Result<(), String> {
    let blob = rmp_serde::to_vec(schema).map_err(|e| format!("Serialize schema: {e}"))?;

    conn.execute(
        "INSERT OR REPLACE INTO _embedded_schema (key, value) VALUES ('schema', ?1)",
        params![blob],
    )
    .map_err(|e| format!("Store schema blob: {e}"))?;

    eprintln!("  Embedded schema blob: {} bytes", blob.len());
    Ok(())
}

/// Load the discovered schema from the `_embedded_schema` table.
pub fn load_schema_from_db(db_path: &Path) -> Result<DiscoveredSchema, String> {
    let conn = Connection::open(db_path).map_err(|e| format!("Cannot open DB: {e}"))?;

    let blob: Vec<u8> = conn
        .query_row(
            "SELECT value FROM _embedded_schema WHERE key = 'schema'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("Load schema blob: {e}"))?;

    let schema: DiscoveredSchema =
        rmp_serde::from_slice(&blob).map_err(|e| format!("Deserialize schema: {e}"))?;

    eprintln!(
        "  Loaded embedded schema: {} tables, {} junctions",
        schema.tables.len(),
        schema.junction_tables.len()
    );

    conn.close()
        .map_err(|(_conn, e)| format!("Close error: {e}"))?;
    Ok(schema)
}

/// Create a mods schema database with composite PKs.
///
/// Unlike the base/honor databases (which use `INSERT OR IGNORE` for priority
/// ordering), the mods database stores multiple mods' data simultaneously.
/// Each row is keyed by `(original_pk, _SourceID)` so the same entity from
/// different mods coexists.  FK constraints are omitted — mod rows reference
/// vanilla data that lives in `ref_base.sqlite`, not in this database.
pub fn create_mods_schema_db(db_path: &Path, schema: &DiscoveredSchema) -> Result<(), String> {
    let conn = Connection::open(db_path).map_err(|e| format!("Cannot open DB: {e}"))?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA page_size = 8192;",
    )
    .map_err(|e| format!("Pragma error: {e}"))?;

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("Begin transaction: {e}"))?;

    // Meta tables (same as base/honor)
    create_meta_tables(&tx)?;

    // Embedded schema storage
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS _embedded_schema (
            key   TEXT PRIMARY KEY,
            value BLOB NOT NULL
         ) WITHOUT ROWID;",
    )
    .map_err(|e| format!("Create _embedded_schema: {e}"))?;

    // Data tables with composite PKs — no FK constraints
    for ts in schema.tables.values() {
        create_mods_data_table(&tx, ts)?;
    }

    // Junction tables — no FK constraints, add _SourceID
    for jt in &schema.junction_tables {
        let ddl = format!(
            "CREATE TABLE IF NOT EXISTS \"{}\" (\n\
             \x20   parent_id {} NOT NULL,\n\
             \x20   child_id {} NOT NULL,\n\
             \x20   \"_SourceID\" INTEGER NOT NULL,\n\
             \x20   PRIMARY KEY (parent_id, child_id, \"_SourceID\")\n\
             ) WITHOUT ROWID",
            jt.table_name, jt.parent_pk_type, jt.child_pk_type,
        );
        tx.execute_batch(&ddl)
            .map_err(|e| format!("Create junction '{}': {}", jt.table_name, e))?;
    }

    // Store schema blob (shared format)
    save_schema_to_db(&tx, schema)?;
    populate_column_types(&tx, schema)?;

    tx.commit()
        .map_err(|e| format!("Commit mods schema DDL: {e}"))?;

    eprintln!(
        "  Mods schema DB created: {} tables, {} junctions (composite PKs, no FKs)",
        schema.tables.len(),
        schema.junction_tables.len(),
    );

    conn.execute_batch("PRAGMA journal_mode = DELETE; VACUUM;")
        .map_err(|e| format!("Vacuum mods schema DB: {e}"))?;

    conn.close()
        .map_err(|(_conn, e)| format!("Close error: {e}"))?;
    Ok(())
}

/// Create a data table for the mods DB (composite PK, no FK constraints).
fn create_mods_data_table(conn: &Connection, ts: &TableSchema) -> Result<(), String> {
    let mut ddl = String::new();
    ddl.push_str(&format!(
        "CREATE TABLE IF NOT EXISTS \"{}\" (\n",
        ts.table_name
    ));

    match &ts.pk_strategy {
        PkStrategy::Rowid => {
            // Rowid tables keep INTEGER PRIMARY KEY; add _SourceID as regular column
            ddl.push_str("    \"_row_id\" INTEGER PRIMARY KEY");
        }
        pk => {
            // Named-PK tables get composite (pk, _SourceID) primary key
            ddl.push_str(&format!(
                "    \"{}\" {} NOT NULL",
                pk.pk_column(),
                pk.pk_sql_type()
            ));
        }
    }

    // _SourceID column
    ddl.push_str(",\n    \"_SourceID\" INTEGER NOT NULL");

    // _file_id column
    if ts.has_file_id {
        ddl.push_str(",\n    _file_id INTEGER NOT NULL");
    }

    // Data columns
    for col in &ts.columns {
        ddl.push_str(&format!(",\n    \"{}\" {}", col.name, col.sqlite_type));
    }

    // Composite PK for non-Rowid tables
    if ts.pk_strategy != PkStrategy::Rowid {
        ddl.push_str(&format!(
            ",\n    PRIMARY KEY (\"{}\", \"_SourceID\")",
            ts.pk_strategy.pk_column()
        ));
    }

    // No FK constraints — mods reference vanilla data in ref_base.sqlite
    ddl.push_str("\n)");

    conn.execute_batch(&ddl)
        .map_err(|e| format!("Create mods table '{}': {}\nDDL: {}", ts.table_name, e, ddl))?;

    Ok(())
}

/// Populate a pre-built schema database with data.
///
/// Opens an existing DB that already has DDL applied and an embedded schema
/// blob, then inserts data from files.  Skips discovery and DDL creation.
///
/// Optimized path: in-memory DB + pre-computed insert plans + parallel file
/// parsing + single transaction.  The entire build happens in RAM; the final
/// DB is written to disk in one shot via SQLite's backup API.
///
/// Low-RAM fallback: if available RAM < 4 GB (or `options.force_disk == Some(true)`),
/// inserts happen directly on the on-disk file using WAL mode + periodic commits.
/// Slower but uses ~100 MB instead of ~1 GB.
pub fn populate_db(
    db_path: &Path,
    files: &[FileEntry],
    _unpacked_path: &Path,
    options: &BuildOptions,
) -> Result<BuildResult, String> {
    let schema = load_schema_from_db(db_path)?;
    let use_mem = should_use_in_memory(options);

    if use_mem {
        populate_db_in_memory(db_path, files, &schema, options)
    } else {
        populate_db_on_disk(db_path, files, &schema, options)
    }
}

/// Query the current maximum file_id in `_source_files` so subsequent inserts
/// start from a non-overlapping range.  Returns 0 when the table is empty.
fn existing_max_file_id(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COALESCE(MAX(file_id), 0) FROM _source_files",
        [],
        |row| row.get(0),
    )
    .unwrap_or(0)
}

/// In-memory build path: load schema into memory, insert everything in RAM,
/// write to disk in one shot via backup API.
fn populate_db_in_memory(
    db_path: &Path,
    files: &[FileEntry],
    schema: &DiscoveredSchema,
    options: &BuildOptions,
) -> Result<BuildResult, String> {
    let mut mem_conn =
        Connection::open_in_memory().map_err(|e| format!("Open in-memory DB: {e}"))?;
    {
        let file_conn =
            Connection::open(db_path).map_err(|e| format!("Open schema DB for backup: {e}"))?;
        let backup = rusqlite::backup::Backup::new(&file_conn, &mut mem_conn)
            .map_err(|e| format!("Backup schema to memory: {e}"))?;
        backup
            .run_to_completion(i32::MAX, std::time::Duration::ZERO, None)
            .map_err(|e| format!("Backup to memory failed: {e}"))?;
    }
    eprintln!("  Schema loaded into memory");

    mem_conn
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             PRAGMA journal_mode = OFF;
             PRAGMA cache_size = -512000;
             PRAGMA temp_store = MEMORY;
             PRAGMA locking_mode = EXCLUSIVE;",
        )
        .map_err(|e| format!("Pragma error: {e}"))?;

    mem_conn.set_prepared_statement_cache_capacity(3000);

    let mut phase_times = PhaseTimes::default();
    let fk_count = count_fk_constraints(&mem_conn)?;
    let id_offset = existing_max_file_id(&mem_conn);

    // --- Parallel partition: split Effect files from the rest ---
    let (counts, row_counts) = {
        let (effect_files, main_files): (Vec<&FileEntry>, Vec<&FileEntry>) =
            files.iter().partition(|f| is_effect_file(f));

        let n_parts = num_effect_partitions(effect_files.len());
        if n_parts >= 1 {
            // Split effect files into N contiguous chunks (preserving priority order).
            let chunk_size = effect_files.len().div_ceil(n_parts);
            let effect_file_groups: Vec<Vec<FileEntry>> = effect_files
                .chunks(chunk_size)
                .map(|chunk| chunk.iter().map(|f| (*f).clone()).collect())
                .collect();
            let effect_db_paths: Vec<Option<PathBuf>> = vec![None; effect_file_groups.len()];
            let effect_conns: Vec<Connection> = (0..effect_file_groups.len())
                .map(|_| prepare_partition_conn(db_path, 256_000))
                .collect::<Result<_, _>>()?;
            run_parallel_insert(
                &mut mem_conn,
                effect_conns,
                effect_db_paths,
                effect_file_groups,
                &main_files,
                schema,
                &mut phase_times,
                id_offset,
            )?
        } else {
            // Few/no effect files — single pipeline is fine
            run_pipeline_insert(
                &mem_conn,
                files,
                schema,
                &mut phase_times,
                id_offset,
                None,
                None,
            )?
        }
    };

    // Post-processing
    let t_post = Instant::now();
    populate_table_meta(&mem_conn, schema, &row_counts)?;
    build_indexes(&mem_conn, schema)?;
    let fallback_base_db_path = resolve_base_fallback_db_path(db_path, options);
    backfill_orphaned_loca(&mem_conn, schema, fallback_base_db_path.as_deref())?;
    phase_times.post_process = t_post.elapsed().as_secs_f64();
    eprintln!("  Phase: post_process   {:.1}s", phase_times.post_process);

    // Write to disk — use VACUUM INTO when vacuuming is requested,
    // which produces a compacted file in a single pass (instead of
    // backup → open → VACUUM which writes the full DB twice).
    let t_write = Instant::now();
    if options.vacuum {
        // VACUUM INTO writes a fresh, compacted database directly from memory.
        let _ = std::fs::remove_file(db_path);
        let db_str = db_path.to_string_lossy();
        for suffix in &["-wal", "-shm"] {
            let p = format!("{db_str}{suffix}");
            let _ = std::fs::remove_file(&p);
        }

        let escaped = db_path.to_string_lossy().replace('\'', "''");
        mem_conn
            .execute_batch(&format!("VACUUM INTO '{escaped}'"))
            .map_err(|e| format!("VACUUM INTO failed: {e}"))?;

        let write_secs = t_write.elapsed().as_secs_f64();
        phase_times.write_to_disk = write_secs;
        // vacuum is included in write_to_disk — no separate vacuum phase
        eprintln!("  Phase: write+vacuum   {write_secs:.1}s  (VACUUM INTO)");
    } else {
        // No vacuum requested — use backup API for a straight write.
        {
            let _ = std::fs::remove_file(db_path);
            let db_str = db_path.to_string_lossy();
            for suffix in &["-wal", "-shm"] {
                let p = format!("{db_str}{suffix}");
                let _ = std::fs::remove_file(&p);
            }

            let mut out_conn =
                Connection::open(db_path).map_err(|e| format!("Open output DB: {e}"))?;
            let backup = rusqlite::backup::Backup::new(&mem_conn, &mut out_conn)
                .map_err(|e| format!("Backup to disk: {e}"))?;
            backup
                .run_to_completion(i32::MAX, std::time::Duration::ZERO, None)
                .map_err(|e| format!("Write to disk failed: {e}"))?;
        }
        let write_secs = t_write.elapsed().as_secs_f64();
        phase_times.write_to_disk = write_secs;
        eprintln!("  Phase: write_to_disk  {write_secs:.1}s");
    }

    Ok(BuildResult {
        total_rows: counts.total_rows,
        total_tables: schema.tables.len() + schema.junction_tables.len(),
        fk_constraints: fk_count,
        file_errors: counts.file_errors,
        row_errors: counts.row_errors,
        phase_times,
    })
}

/// On-disk build path: insert directly into the file with WAL mode.
/// Uses ~100 MB instead of ~1 GB, but is slower due to disk I/O.
fn populate_db_on_disk(
    db_path: &Path,
    files: &[FileEntry],
    schema: &DiscoveredSchema,
    options: &BuildOptions,
) -> Result<BuildResult, String> {
    let mut phase_times = PhaseTimes::default();

    // --- Decide whether to run in parallel ---
    let (effect_refs, main_refs): (Vec<&FileEntry>, Vec<&FileEntry>) =
        files.iter().partition(|f| is_effect_file(f));
    let n_parts = num_effect_partitions(effect_refs.len());
    let use_parallel = n_parts >= 1;

    // For parallel on-disk, create effect temp DBs BEFORE opening the
    // main connection to avoid copying WAL artifacts.
    let effect_tmp_paths: Vec<PathBuf> = if use_parallel {
        let chunk_size = effect_refs.len().div_ceil(n_parts);
        (0..effect_refs.chunks(chunk_size).len())
            .map(|i| {
                let p = std::env::temp_dir().join(format!(
                    "bg3_effect_partition_{}_{}.sqlite",
                    std::process::id(),
                    i,
                ));
                std::fs::copy(db_path, &p)
                    .map_err(|e| format!("Copy schema for effect partition {i}: {e}"))?;
                Ok(p)
            })
            .collect::<Result<_, String>>()?
    } else {
        Vec::new()
    };

    let mut conn =
        Connection::open(db_path).map_err(|e| format!("Open DB for on-disk populate: {e}"))?;

    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -128000;
         PRAGMA temp_store = MEMORY;
         PRAGMA mmap_size = 536870912;
         PRAGMA locking_mode = EXCLUSIVE;",
    )
    .map_err(|e| format!("Pragma error: {e}"))?;

    conn.set_prepared_statement_cache_capacity(3000);

    let fk_count = count_fk_constraints(&conn)?;
    let id_offset = existing_max_file_id(&conn);

    let (counts, row_counts) = if use_parallel {
        let chunk_size = effect_refs.len().div_ceil(n_parts);
        let effect_file_groups: Vec<Vec<FileEntry>> = effect_refs
            .chunks(chunk_size)
            .map(|chunk| chunk.iter().map(|f| (*f).clone()).collect())
            .collect();

        let effect_conns: Vec<Connection> = effect_tmp_paths
            .iter()
            .map(|p| {
                let c = Connection::open(p).map_err(|e| format!("Open effect partition: {e}"))?;
                c.execute_batch(
                    "PRAGMA foreign_keys = OFF;
                     PRAGMA journal_mode = OFF;
                     PRAGMA synchronous = OFF;
                     PRAGMA cache_size = -128000;
                     PRAGMA temp_store = MEMORY;
                     PRAGMA locking_mode = EXCLUSIVE;",
                )
                .map_err(|e| format!("Effect partition pragma: {e}"))?;
                c.set_prepared_statement_cache_capacity(3000);
                Ok(c)
            })
            .collect::<Result<_, String>>()?;

        let effect_db_paths: Vec<Option<PathBuf>> =
            effect_tmp_paths.into_iter().map(Some).collect();

        run_parallel_insert(
            &mut conn,
            effect_conns,
            effect_db_paths,
            effect_file_groups,
            &main_refs,
            schema,
            &mut phase_times,
            id_offset,
        )?
    } else {
        run_pipeline_insert(
            &conn,
            files,
            schema,
            &mut phase_times,
            id_offset,
            None,
            None,
        )?
    };

    // Post-processing
    let t_post = Instant::now();
    populate_table_meta(&conn, schema, &row_counts)?;
    build_indexes(&conn, schema)?;
    let fallback_base_db_path = resolve_base_fallback_db_path(db_path, options);
    backfill_orphaned_loca(&conn, schema, fallback_base_db_path.as_deref())?;
    phase_times.post_process = t_post.elapsed().as_secs_f64();
    eprintln!("  Phase: post_process   {:.1}s", phase_times.post_process);

    if options.vacuum {
        let t_vac = Instant::now();
        conn.execute_batch("PRAGMA journal_mode = DELETE; VACUUM;")
            .map_err(|e| format!("Vacuum error: {e}"))?;
        phase_times.vacuum = t_vac.elapsed().as_secs_f64();
        eprintln!("  Phase: vacuum         {:.1}s", phase_times.vacuum);
    }

    conn.close()
        .map_err(|(_conn, e)| format!("Close error: {e}"))?;

    // Clean up any stale WAL/SHM files so subsequent read-only ATTACH works.
    let db_str = db_path.to_string_lossy();
    for suffix in &["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{db_str}{suffix}"));
    }

    Ok(BuildResult {
        total_rows: counts.total_rows,
        total_tables: schema.tables.len() + schema.junction_tables.len(),
        fk_constraints: fk_count,
        file_errors: counts.file_errors,
        row_errors: counts.row_errors,
        phase_times,
    })
}

// ---------------------------------------------------------------------------
// Parallel table-partitioned insert
// ---------------------------------------------------------------------------

/// Returns `true` if this file contributes to the "Effect" region.
/// Effect data comes exclusively from `.lsfx` binary files and their
/// `.lsfx.lsx` converted counterparts.
fn is_effect_file(file: &FileEntry) -> bool {
    let ext = file.extension();
    ext.as_deref() == Some("lsfx")
        || ext.as_deref() == Some("lsefx")
        || file.rel_path.ends_with(".lsfx.lsx")
}

/// Prepare an in-memory connection for use as a partition: load the schema,
/// apply performance PRAGMAs, and set up the prepared statement cache.
fn prepare_partition_conn(schema_db_path: &Path, cache_size_kb: i64) -> Result<Connection, String> {
    let mut conn =
        Connection::open_in_memory().map_err(|e| format!("Open partition in-memory DB: {e}"))?;
    {
        let file_conn = Connection::open(schema_db_path)
            .map_err(|e| format!("Open schema for partition backup: {e}"))?;
        let backup = rusqlite::backup::Backup::new(&file_conn, &mut conn)
            .map_err(|e| format!("Backup schema to partition: {e}"))?;
        backup
            .run_to_completion(i32::MAX, std::time::Duration::ZERO, None)
            .map_err(|e| format!("Partition schema backup failed: {e}"))?;
    }
    conn.execute_batch(&format!(
        "PRAGMA foreign_keys = OFF;
         PRAGMA journal_mode = OFF;
         PRAGMA cache_size = -{cache_size_kb};
         PRAGMA temp_store = MEMORY;
         PRAGMA locking_mode = EXCLUSIVE;",
    ))
    .map_err(|e| format!("Partition pragma error: {e}"))?;
    conn.set_prepared_statement_cache_capacity(3000);
    Ok(conn)
}

/// Pre-populate the `_sources` table identically in all connections so that
/// every partition assigns the same integer IDs to module names.
fn pre_populate_sources(conns: &[&Connection], all_files: &[&FileEntry]) -> Result<(), String> {
    let mut names: Vec<String> = all_files
        .iter()
        .map(|f| f.mod_name.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    names.sort();

    for conn in conns {
        for (i, name) in names.iter().enumerate() {
            let id = (i + 1) as i64;
            conn.execute(
                "INSERT OR IGNORE INTO _sources (id, name) VALUES (?1, ?2)",
                params![id, name],
            )
            .map_err(|e| format!("Pre-populate _sources: {e}"))?;
        }
    }

    Ok(())
}

/// Decide how many sub-partitions to split effect files into.
/// Benchmarks show that a single effect partition is optimal: multiple
/// sub-partitions cause CPU contention on ≤8-core machines and introduce
/// cross-partition dedup issues for ROWID-based child tables.
fn num_effect_partitions(effect_file_count: usize) -> usize {
    if effect_file_count < 200 {
        return 0; // too few — single pipeline is fine
    }
    1
}

/// Run N+1 insert pipelines in parallel: N for Effect sub-partitions, 1 for
/// everything else.  Each partition operates on its own connection.
/// After all complete, the Effect partitions are merged back into main
/// via ATTACH + INSERT OR IGNORE SELECT, in chunk order (preserving priority).
///
/// `effect_conns`: one connection per effect sub-partition.
/// `effect_db_paths`: for each sub-partition, `Some(path)` if on-disk,
///                    `None` if in-memory.
/// `effect_file_groups`: effect files split into contiguous chunks (preserving
///                       the original priority sort order).
#[allow(clippy::too_many_arguments)]
fn run_parallel_insert(
    main_conn: &mut Connection,
    effect_conns: Vec<Connection>,
    effect_db_paths: Vec<Option<PathBuf>>,
    effect_file_groups: Vec<Vec<FileEntry>>,
    main_files: &[&FileEntry],
    schema: &DiscoveredSchema,
    phase_times: &mut PhaseTimes,
    id_offset: i64,
) -> Result<(PopulateCounts, HashMap<String, i64>), String> {
    let num_effect = effect_conns.len();
    let total_effect_files: usize = effect_file_groups.iter().map(|g| g.len()).sum::<usize>();
    let total_partitions = num_effect + 1;
    eprintln!(
        "  Parallel insert: {} effect files ({} sub-partitions), {} main files",
        total_effect_files,
        num_effect,
        main_files.len()
    );

    // Pre-populate _sources identically in ALL connections.
    {
        let all_file_refs: Vec<&FileEntry> = effect_file_groups
            .iter()
            .flat_map(|g| g.iter())
            .chain(main_files.iter().copied())
            .collect();
        let mut conn_refs: Vec<&Connection> = vec![&*main_conn];
        for c in &effect_conns {
            conn_refs.push(c);
        }
        pre_populate_sources(&conn_refs, &all_file_refs)?;
    }

    // Load source caches for each partition.
    let main_sources = SourceIdCache::new_from_existing(main_conn)?;
    let effect_sources: Vec<SourceIdCache> = effect_conns
        .iter()
        .map(SourceIdCache::new_from_existing)
        .collect::<Result<_, _>>()?;

    // File-id ranges: sub-partition 0 gets 1..N0, sub-partition 1 gets N0+1..N0+N1, etc.
    // Main gets IDs after all effect sub-partitions.
    // `id_offset` ensures we don't collide with existing rows in the DB.
    let mut effect_id_bases: Vec<i64> = Vec::with_capacity(num_effect);
    let mut cumulative = id_offset;
    for group in &effect_file_groups {
        effect_id_bases.push(cumulative);
        cumulative += group.len() as i64;
    }
    let main_id_base: i64 = id_offset + total_effect_files as i64;

    // Divide parse threads among partitions.
    let total_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    let parse_threads = (total_threads / total_partitions).max(1);

    // Owned main files for the parallel scope.
    let main_owned: Vec<FileEntry> = main_files.iter().map(|f| (*f).clone()).collect();

    // Bundle effect data for move into threads.
    let effect_bundles: Vec<_> = effect_conns
        .into_iter()
        .zip(effect_file_groups)
        .zip(effect_sources)
        .zip(effect_id_bases)
        .map(|(((conn, files), sources), base)| (conn, files, sources, base))
        .collect();

    // Run all pipelines in parallel.
    let (main_result, effect_results) = std::thread::scope(|s| {
        let schema_ref = schema;

        // Spawn one thread per effect sub-partition.
        let effect_handles: Vec<_> = effect_bundles
            .into_iter()
            .map(|(conn, files, sources, base)| {
                s.spawn(move || {
                    let mut pt = PhaseTimes::default();
                    let r = run_pipeline_insert(
                        &conn,
                        &files,
                        schema_ref,
                        &mut pt,
                        base,
                        Some(parse_threads),
                        Some(sources),
                    );
                    (r, pt, conn)
                })
            })
            .collect();

        // Main runs on this thread.
        let mut main_pt = PhaseTimes::default();
        let main_r = run_pipeline_insert(
            main_conn,
            &main_owned,
            schema,
            &mut main_pt,
            main_id_base,
            Some(parse_threads),
            Some(main_sources),
        );

        // Join all effect threads (preserving order for priority-correct merge).
        let effect_results: Result<Vec<_>, String> = effect_handles
            .into_iter()
            .map(|h| {
                h.join()
                    .map_err(|_| "Effect insert thread panicked".to_string())
            })
            .collect();

        Ok::<_, String>(((main_r, main_pt), effect_results?))
    })?;

    let ((main_r, main_pt), effect_results) = (main_result, effect_results);
    let (main_counts, main_row_counts) = main_r?;

    // Report timing.
    let max_effect_time = effect_results
        .iter()
        .map(|(_, pt, _)| pt.data_insert)
        .fold(0.0f64, f64::max);
    phase_times.data_insert = main_pt.data_insert.max(max_effect_time);
    eprintln!(
        "  Parallel insert done: main {:.1}s, effect max {:.1}s (wall = {:.1}s)",
        main_pt.data_insert, max_effect_time, phase_times.data_insert
    );

    // --- Merge all effect sub-partitions into main (in order) ---
    let t_merge = Instant::now();
    let mut total_counts = main_counts;
    let mut row_counts = main_row_counts;
    let tmp_merge_path =
        std::env::temp_dir().join(format!("bg3_effect_merge_{}.sqlite", std::process::id()));

    for (i, ((r, _pt, conn), db_path_opt)) in effect_results
        .into_iter()
        .zip(effect_db_paths.into_iter())
        .enumerate()
    {
        let (effect_counts, effect_row_counts) = r?;

        let merge_path = if let Some(path) = db_path_opt {
            // On-disk: drop conn to release locks, then merge from file.
            drop(conn);
            path
        } else {
            // In-memory: backup to temp file for ATTACH bridge (reuse path).
            {
                let mut out = Connection::open(&tmp_merge_path)
                    .map_err(|e| format!("Open merge temp file: {e}"))?;
                let backup = rusqlite::backup::Backup::new(&conn, &mut out)
                    .map_err(|e| format!("Backup effect sub-{i} to temp: {e}"))?;
                backup
                    .run_to_completion(i32::MAX, std::time::Duration::ZERO, None)
                    .map_err(|e| format!("Write effect sub-{i} temp: {e}"))?;
            }
            tmp_merge_path.clone()
        };
        merge_partition_from_file(main_conn, &merge_path, schema, &effect_row_counts)?;

        total_counts.total_rows += effect_counts.total_rows;
        total_counts.file_errors += effect_counts.file_errors;
        total_counts.row_errors += effect_counts.row_errors;
        for (table, count) in effect_row_counts {
            *row_counts.entry(table).or_insert(0) += count;
        }
    }

    let merge_secs = t_merge.elapsed().as_secs_f64();
    phase_times.merge = merge_secs;
    eprintln!("  Phase: merge_effect   {merge_secs:.1}s");

    Ok((total_counts, row_counts))
}

/// Merge all data from an Effect partition's DB file into `dest_conn`.
/// ATTACHes the file, copies rows via INSERT OR IGNORE SELECT, then
/// DETACHes and deletes the source file.
///
/// Only tables that received rows (per `effect_row_counts`) are merged;
/// the remaining ~1700 empty tables are skipped entirely.
fn merge_partition_from_file(
    dest_conn: &Connection,
    source_path: &Path,
    schema: &DiscoveredSchema,
    effect_row_counts: &HashMap<String, i64>,
) -> Result<(), String> {
    // ATTACH and copy data.
    let escaped = source_path.to_string_lossy().replace('\'', "''");
    dest_conn
        .execute_batch(&format!("ATTACH DATABASE '{escaped}' AS effect_db"))
        .map_err(|e| format!("Attach effect temp: {e}"))?;

    // Copy _source_files (non-overlapping file_ids).
    dest_conn
        .execute_batch(
            "INSERT INTO main._source_files
                (file_id, path, file_type, mod_name, region_id, row_count, file_size)
             SELECT file_id, path, file_type, mod_name, region_id, row_count, file_size
             FROM effect_db._source_files",
        )
        .map_err(|e| format!("Merge _source_files: {e}"))?;

    // Build set of tables that actually received rows in this partition.
    let populated: HashSet<&str> = effect_row_counts
        .iter()
        .filter(|(_, &count)| count > 0)
        .map(|(name, _)| name.as_str())
        .collect();

    let mut merged_tables = 0usize;
    let mut merged_junctions = 0usize;

    // Copy only populated data tables (skip the ~1700 empty ones).
    for table_name in schema.tables.keys() {
        if !populated.contains(table_name.as_str()) {
            continue;
        }
        dest_conn
            .execute(
                &format!(
                    "INSERT OR IGNORE INTO main.\"{table_name}\" SELECT * FROM effect_db.\"{table_name}\""
                ),
                [],
            )
            .map_err(|e| format!("Merge table '{table_name}': {e}"))?;
        merged_tables += 1;
    }

    // Copy junction tables only if their parent table was populated.
    for jt in &schema.junction_tables {
        if !populated.contains(jt.parent_table.as_str()) {
            continue;
        }
        dest_conn
            .execute(
                &format!(
                    "INSERT OR IGNORE INTO main.\"{}\" SELECT * FROM effect_db.\"{}\"",
                    jt.table_name, jt.table_name
                ),
                [],
            )
            .map_err(|e| format!("Merge junction '{}': {}", jt.table_name, e))?;
        merged_junctions += 1;
    }

    dest_conn
        .execute_batch("DETACH DATABASE effect_db")
        .map_err(|e| format!("Detach effect temp: {e}"))?;

    // Clean up temp file.
    let _ = std::fs::remove_file(source_path);

    eprintln!(
        "    Merged {}/{} data tables, {}/{} junctions",
        merged_tables,
        schema.tables.len(),
        merged_junctions,
        schema.junction_tables.len()
    );

    Ok(())
}

/// Shared pipeline: parse files in parallel, insert sequentially.
/// Works with both in-memory and on-disk connections.
///
/// `file_id_base`: offset added to each file's index to produce globally-unique
/// file_ids.  For a single-pipeline build, pass 0 so file_ids are 1..N.
/// For parallel partitions, the second partition passes N_first so its
/// file_ids start at N_first+1.
///
/// `max_parse_threads`: if `Some(n)`, cap parse threads at `n` instead of the
/// default (available_parallelism capped at 8).  Used for parallel partitions
/// to avoid over-subscribing cores.
///
/// `pre_sources`: if `Some(cache)`, use a pre-populated SourceIdCache instead
/// of building one on the fly.  Used to ensure identical `_sources` ID
/// mappings across parallel partitions.
fn run_pipeline_insert(
    conn: &Connection,
    files: &[FileEntry],
    schema: &DiscoveredSchema,
    phase_times: &mut PhaseTimes,
    file_id_base: i64,
    max_parse_threads: Option<usize>,
    pre_sources: Option<SourceIdCache>,
) -> Result<(PopulateCounts, HashMap<String, i64>), String> {
    let mut plans = build_insert_plans(schema);
    let junction_plans = build_junction_plans(schema);

    let default_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    let num_threads = match max_parse_threads {
        Some(n) => n.min(default_threads),
        None => default_threads,
    };
    eprintln!("  Parallel parse threads: {num_threads}");

    let t_insert = Instant::now();
    let mut counts = PopulateCounts::default();
    let mut source_ids = pre_sources.unwrap_or_else(SourceIdCache::new);
    let mut row_counts: HashMap<String, i64> =
        schema.tables.keys().map(|k| (k.clone(), 0i64)).collect();
    let mut pak_rows: HashMap<String, usize> = HashMap::new();
    let mut pak_files: HashMap<String, usize> = HashMap::new();

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("Transaction error: {e}"))?;

    let (parse_chunk_size, channel_capacity) = adaptive_pipeline_params();
    {
        let (sender, receiver) =
            crossbeam_channel::bounded::<Vec<(ParsedFile, usize)>>(channel_capacity);
        let schema_ref = schema;
        let files_ref = files;

        std::thread::scope(|s| {
            let parse_handle = s.spawn(move || {
                for (chunk_idx, file_chunk) in files_ref.chunks(parse_chunk_size).enumerate() {
                    let parsed: Vec<(ParsedFile, usize)> = {
                        let per_thread = file_chunk.len().div_ceil(num_threads);
                        std::thread::scope(|s2| {
                            let handles: Vec<_> = file_chunk
                                .chunks(per_thread)
                                .map(|thread_files| {
                                    s2.spawn(move || {
                                        thread_files
                                            .iter()
                                            .map(|file| parse_file(file, schema_ref))
                                            .collect::<Vec<_>>()
                                    })
                                })
                                .collect();
                            handles
                                .into_iter()
                                .flat_map(|h| h.join().unwrap())
                                .enumerate()
                                .map(|(i, pf)| (pf, chunk_idx * parse_chunk_size + i))
                                .collect()
                        })
                    };
                    if sender.send(parsed).is_err() {
                        break;
                    }
                }
            });

            for batch in &receiver {
                for (mut parsed_file, file_idx) in batch {
                    let file = &files_ref[file_idx];
                    let file_id = file_id_base + file_idx as i64 + 1;
                    match insert_parsed_file(
                        &tx,
                        &mut parsed_file,
                        file,
                        file_id,
                        &mut plans,
                        &junction_plans,
                        schema_ref,
                        &mut source_ids,
                        &mut row_counts,
                    ) {
                        Ok((rows, errors)) => {
                            counts.total_rows += rows;
                            counts.row_errors += errors;
                            *pak_rows
                                .entry(parsed_file.mod_name.clone())
                                .or_insert(0usize) += rows;
                            *pak_files
                                .entry(parsed_file.mod_name.clone())
                                .or_insert(0usize) += 1;
                        }
                        Err(e) => {
                            eprintln!("  FILE ERROR: {}: {}", file.rel_path, e);
                            counts.file_errors += 1;
                        }
                    }
                }
            }

            parse_handle.join().unwrap();
        });
    }

    tx.commit().map_err(|e| format!("Commit error: {e}"))?;

    phase_times.data_insert = t_insert.elapsed().as_secs_f64();
    eprintln!(
        "  Phase: data_insert    {:.1}s  ({} rows, {} files)",
        phase_times.data_insert,
        counts.total_rows,
        files.len()
    );

    // Per-pak breakdown
    if !pak_rows.is_empty() {
        let mut paks: Vec<_> = pak_rows.iter().collect();
        paks.sort_by(|a, b| b.1.cmp(a.1));
        for (pak, rows) in &paks {
            let files = pak_files.get(*pak).copied().unwrap_or(0);
            let avg = if files > 0 { *rows / files } else { 0 };
            eprintln!("    {pak:<20} {rows:>10} rows  {files:>6} files  ({avg} rows/file)");
        }
    }

    Ok((counts, row_counts))
}

/// Count FK constraints in an existing database by inspecting sqlite_master.
fn count_fk_constraints(conn: &Connection) -> Result<usize, String> {
    let mut count = 0usize;
    let table_names: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .map_err(|e| format!("List tables: {e}"))?;
        let names = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| format!("Query tables: {e}"))?
            .filter_map(|r| r.ok())
            .collect();
        names
    };
    for table_name in &table_names {
        let sql = format!("PRAGMA foreign_key_list(\"{table_name}\")");
        let mut stmt = conn.prepare(&sql).map_err(|e| format!("FK list: {e}"))?;
        let n: usize = stmt
            .query_map([], |_| Ok(()))
            .map_err(|e| format!("FK query: {e}"))?
            .count();
        count += n;
    }
    Ok(count)
}

/// Build the SQLite database given discovered schema and files.
pub fn build_db(
    db_path: &Path,
    files: &[FileEntry],
    unpacked_path: &Path,
    schema: &DiscoveredSchema,
) -> Result<BuildResult, String> {
    let conn = Connection::open(db_path).map_err(|e| format!("Cannot open DB: {e}"))?;

    // Performance pragmas
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = OFF;
         PRAGMA cache_size = -64000;
         PRAGMA temp_store = MEMORY;
         PRAGMA page_size = 8192;
         PRAGMA foreign_keys = OFF;
         PRAGMA mmap_size = 1073741824;",
    )
    .map_err(|e| format!("Pragma error: {e}"))?;

    let mut phase_times = PhaseTimes::default();

    // Create meta tables
    create_meta_tables(&conn)?;

    // Create all data tables with proper PKs and FKs
    let t_ddl = Instant::now();
    let mut fk_count = 0;
    for ts in schema.tables.values() {
        fk_count += create_data_table(&conn, ts, schema)?;
    }

    // Create junction tables for parent→child relationships
    for jt in &schema.junction_tables {
        let ddl = format!(
            "CREATE TABLE IF NOT EXISTS \"{}\" (\n\
             \x20   parent_id {} NOT NULL REFERENCES \"{}\"(\"{}\"),\n\
             \x20   child_id {} NOT NULL REFERENCES \"{}\"(\"{}\"),\n\
             \x20   PRIMARY KEY (parent_id, child_id)\n\
             ) WITHOUT ROWID",
            jt.table_name,
            jt.parent_pk_type,
            jt.parent_table,
            jt.parent_pk_column,
            jt.child_pk_type,
            jt.child_table,
            jt.child_pk_column,
        );
        conn.execute_batch(&ddl)
            .map_err(|e| format!("Create junction '{}': {}\nDDL: {}", jt.table_name, e, ddl))?;
        fk_count += 2; // parent FK + child FK
    }
    phase_times.ddl_creation = t_ddl.elapsed().as_secs_f64();
    eprintln!(
        "  Phase: ddl_creation   {:.1}s  ({} tables, {} junctions)",
        phase_times.ddl_creation,
        schema.tables.len(),
        schema.junction_tables.len()
    );

    // Insert data
    let t_insert = Instant::now();
    let mut total_rows = 0usize;
    let mut file_errors = 0usize;
    let mut row_errors = 0usize;
    let batch_size = 2000;

    for chunk in files.chunks(batch_size) {
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Transaction error: {e}"))?;

        for file in chunk {
            match process_file(&tx, file, unpacked_path, schema) {
                Ok(result) => {
                    total_rows += result.rows;
                    row_errors += result.errors;
                }
                Err(e) => {
                    eprintln!("  FILE ERROR: {}: {}", file.rel_path, e);
                    file_errors += 1;
                }
            }
        }

        tx.commit().map_err(|e| format!("Commit error: {e}"))?;
    }
    phase_times.data_insert = t_insert.elapsed().as_secs_f64();
    eprintln!(
        "  Phase: data_insert    {:.1}s  ({} rows, {} files)",
        phase_times.data_insert,
        total_rows,
        files.len()
    );

    // Post-processing: populate _table_meta, indexes, column types, loca backfill
    let t_post = Instant::now();
    populate_table_meta(&conn, schema, &HashMap::new())?;
    build_indexes(&conn, schema)?;
    populate_column_types(&conn, schema)?;
    let fallback_base_db_path = resolve_base_fallback_db_path(db_path, &BuildOptions::default());
    backfill_orphaned_loca(&conn, schema, fallback_base_db_path.as_deref())?;
    phase_times.post_process = t_post.elapsed().as_secs_f64();
    eprintln!("  Phase: post_process   {:.1}s", phase_times.post_process);

    // VACUUM
    let t_vac = Instant::now();
    conn.execute_batch(
        "PRAGMA journal_mode = DELETE;
         VACUUM;
         PRAGMA journal_mode = WAL;",
    )
    .map_err(|e| format!("Vacuum error: {e}"))?;
    phase_times.vacuum = t_vac.elapsed().as_secs_f64();
    eprintln!("  Phase: vacuum         {:.1}s", phase_times.vacuum);

    conn.close()
        .map_err(|(_conn, e)| format!("Close error: {e}"))?;

    Ok(BuildResult {
        total_rows,
        total_tables: schema.tables.len() + schema.junction_tables.len(),
        fk_constraints: fk_count,
        file_errors,
        row_errors,
        phase_times,
    })
}

// ---------------------------------------------------------------------------
// Meta tables
// ---------------------------------------------------------------------------

fn create_meta_tables(conn: &Connection) -> Result<(), String> {
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
        ) WITHOUT ROWID;",
    )
    .map_err(|e| format!("Meta table error: {e}"))
}

// ---------------------------------------------------------------------------
// Data table creation
// ---------------------------------------------------------------------------

/// Create a data table. Returns the number of FK constraints added.
fn create_data_table(
    conn: &Connection,
    ts: &TableSchema,
    schema: &DiscoveredSchema,
) -> Result<usize, String> {
    let mut ddl = String::new();
    ddl.push_str(&format!(
        "CREATE TABLE IF NOT EXISTS \"{}\" (\n",
        ts.table_name
    ));

    // PK column
    match &ts.pk_strategy {
        PkStrategy::Rowid => {
            // Explicit _row_id column — alias for SQLite rowid, enables FK references
            ddl.push_str("    \"_row_id\" INTEGER PRIMARY KEY");
        }
        pk => {
            ddl.push_str(&format!(
                "    \"{}\" {} PRIMARY KEY",
                pk.pk_column(),
                pk.pk_sql_type()
            ));
        }
    }

    let mut need_comma = true; // PK column is always present now

    // _file_id column
    if ts.has_file_id {
        if need_comma {
            ddl.push_str(",\n");
        }
        ddl.push_str("    _file_id INTEGER NOT NULL REFERENCES _source_files(file_id)");
        need_comma = true;
    }

    // _SourceID column — integer FK to _sources(id).
    // Only for non-Rowid tables (children inherit the parent's source).
    if ts.pk_strategy != PkStrategy::Rowid {
        if need_comma {
            ddl.push_str(",\n");
        }
        ddl.push_str("    \"_SourceID\" INTEGER REFERENCES _sources(id)");
        need_comma = true;
    }

    // Data columns
    for col in &ts.columns {
        if need_comma {
            ddl.push_str(",\n");
        }
        ddl.push_str(&format!("    \"{}\" {}", col.name, col.sqlite_type));
        need_comma = true;
    }

    // FK constraints (only for columns that reference tables that exist)
    let mut fk_count = 0;
    for fk in &ts.fk_constraints {
        // Verify target table exists in schema
        let target_ts = match schema.tables.get(&fk.target_table) {
            Some(t) => t,
            None => continue,
        };

        // SQLite FK constraints require the target column to be the PRIMARY KEY
        // (or have a UNIQUE index). Skip FKs that reference non-PK columns.
        if fk.target_column != target_ts.pk_strategy.pk_column() {
            continue;
        }

        // Don't emit FK for self-PK columns (UUID → itself)
        if fk.source_column == ts.pk_strategy.pk_column() && fk.target_table == ts.table_name {
            continue;
        }

        // Verify source column exists in the table (either as data column or PK)
        let col_exists = ts.columns.iter().any(|c| c.name == fk.source_column)
            || fk.source_column == ts.pk_strategy.pk_column();
        if !col_exists {
            continue;
        }

        if need_comma {
            ddl.push_str(",\n");
        }
        ddl.push_str(&format!(
            "    FOREIGN KEY(\"{}\") REFERENCES \"{}\"(\"{}\")",
            fk.source_column, fk.target_table, fk.target_column
        ));
        need_comma = true;
        fk_count += 1;
    }

    ddl.push_str("\n)");
    conn.execute_batch(&ddl).map_err(|e| {
        format!(
            "Create table '{}' error: {}\nDDL: {}",
            ts.table_name, e, ddl
        )
    })?;

    Ok(fk_count)
}

// ---------------------------------------------------------------------------
// File processing
// ---------------------------------------------------------------------------

struct FileResult {
    rows: usize,
    errors: usize,
    region_id: Option<String>,
}

fn process_file(
    tx: &Transaction,
    file: &FileEntry,
    _unpacked_path: &Path,
    schema: &DiscoveredSchema,
) -> Result<FileResult, String> {
    let mod_name = file.mod_name.clone();
    let file_size = file.file_size();

    tx.prepare_cached(
        "INSERT INTO _source_files (path, file_type, mod_name, region_id, row_count, file_size)
         VALUES (?1, ?2, ?3, NULL, 0, ?4)",
    )
    .and_then(|mut stmt| {
        stmt.execute(params![
            file.rel_path,
            file.file_type.as_str(),
            mod_name,
            file_size
        ])
    })
    .map_err(|e| format!("Insert source file: {e}"))?;

    let file_id = tx.last_insert_rowid();

    let result = match file.file_type {
        FileType::Lsx => process_lsx(tx, file, file_id, schema)?,
        FileType::Stats => process_stats(tx, file, file_id, schema)?,
        FileType::Loca => process_loca(tx, file, file_id, schema)?,
        FileType::AllSpark => process_allspark(tx, file, file_id, schema)?,
        FileType::Effect => process_effect(tx, file, file_id, schema)?,
    };

    // Update source file with region_id and row_count
    tx.prepare_cached("UPDATE _source_files SET region_id = ?1, row_count = ?2 WHERE file_id = ?3")
        .and_then(|mut stmt| stmt.execute(params![result.region_id, result.rows as i64, file_id]))
        .map_err(|e| format!("Update source file: {e}"))?;

    Ok(result)
}

// ---------------------------------------------------------------------------
// LSX processing
// ---------------------------------------------------------------------------

fn process_lsx(
    tx: &Transaction,
    file: &FileEntry,
    file_id: i64,
    schema: &DiscoveredSchema,
) -> Result<FileResult, String> {
    let resource = parse_lsx_resource_file(file)?;
    let mut total_rows = 0usize;
    let mut total_errors = 0usize;
    let mut last_region_id: Option<String> = None;

    for region in &resource.regions {
        if SKIP_REGIONS.contains(&region.id.as_str()) {
            continue;
        }

        last_region_id = Some(region.id.clone());
        for node in &region.nodes {
            let (_, rows, errors) =
                insert_resource_node_direct(tx, node, &region.id, file_id, schema);
            total_rows += rows;
            total_errors += errors;
        }
    }

    Ok(FileResult {
        rows: total_rows,
        errors: total_errors,
        region_id: last_region_id,
    })
}

/// Find the junction table for a given (parent_table, child_table) pair.
fn find_junction_table<'a>(
    schema: &'a DiscoveredSchema,
    parent_table: &str,
    child_table: &str,
) -> Option<&'a str> {
    schema
        .junction_lookup
        .get(parent_table)
        .and_then(|children| children.get(child_table))
        .map(|s| s.as_str())
}

fn parse_lsx_resource_file(file: &FileEntry) -> Result<LsxResource, String> {
    match file.extension().as_deref() {
        Some("lsf") => {
            if let Some(bytes) = file.in_memory_bytes() {
                lsf_parser::parse_lsf(Cursor::new(bytes))
            } else {
                lsf_parser::parse_lsf_file(&file.abs_path)
            }
        }
        Some("lsfx") => {
            if let Some(bytes) = file.in_memory_bytes() {
                lsfx_parser::parse_lsfx(Cursor::new(bytes))
            } else {
                lsfx_parser::parse_lsfx_file(&file.abs_path)
            }
        }
        _ => {
            let content = read_text_content(file)?;
            if content.trim().is_empty() {
                return Ok(LsxResource {
                    regions: Vec::new(),
                });
            }
            lsx_parser::parse_lsx_resource(&content)
        }
    }
}

fn insert_resource_node_direct(
    tx: &Transaction,
    node: &LsxNode,
    region_id: &str,
    file_id: i64,
    schema: &DiscoveredSchema,
) -> (Vec<(String, String)>, usize, usize) {
    let parsed = build_parsed_node(region_id, node, schema);
    insert_resource_parsed_node_direct(tx, &parsed, file_id, schema)
}

fn insert_resource_parsed_node_direct(
    tx: &Transaction,
    node: &ParsedNode,
    file_id: i64,
    schema: &DiscoveredSchema,
) -> (Vec<(String, String)>, usize, usize) {
    let mut total_rows = 0;
    let mut total_errors = 0;

    if !node.has_data {
        let mut child_pks = Vec::new();
        for child in &node.children {
            let (regs, rows, errors) =
                insert_resource_parsed_node_direct(tx, child, file_id, schema);
            child_pks.extend(regs);
            total_rows += rows;
            total_errors += errors;
        }
        return (child_pks, total_rows, total_errors);
    }

    match insert_lsx_row(tx, &node.table_name, file_id, &node.attrs, schema) {
        Ok(pk_value) => {
            total_rows += 1;

            let mut child_pks = Vec::new();
            for child in &node.children {
                let (regs, rows, errors) =
                    insert_resource_parsed_node_direct(tx, child, file_id, schema);
                child_pks.extend(regs);
                total_rows += rows;
                total_errors += errors;
            }

            if let Some(ref parent_pk) = pk_value {
                for (child_table, child_pk) in &child_pks {
                    if let Some(jn) = find_junction_table(schema, &node.table_name, child_table) {
                        let jct_sql = format!(
                            "INSERT OR IGNORE INTO \"{jn}\" (parent_id, child_id) VALUES (?1, ?2)"
                        );
                        let _ = tx
                            .prepare_cached(&jct_sql)
                            .and_then(|mut stmt| stmt.execute(params![parent_pk, child_pk]));
                    }
                }
            }

            let regs = pk_value
                .map(|pk| vec![(node.table_name.clone(), pk)])
                .unwrap_or_default();
            (regs, total_rows, total_errors)
        }
        Err(_) => (Vec::new(), total_rows, total_errors + 1),
    }
}

fn build_parsed_node(region_id: &str, node: &LsxNode, schema: &DiscoveredSchema) -> ParsedNode {
    let raw_name = format!("lsx__{}__{}", sanitize_id(region_id), sanitize_id(&node.id));
    let table_name = schema
        .resolve_table(&raw_name)
        .unwrap_or(raw_name.as_str())
        .to_string();

    let mut attrs = Vec::new();
    for attr in &node.attributes {
        collect_resource_attribute(attr, &mut attrs);
    }

    let has_data = !attrs.is_empty();

    // Swap PK attribute to index 0 so insert_row_planned's .find() hits
    // immediately (O(1) instead of O(n)) and the data-column loop's
    // `continue` triggers on the first iteration.
    if has_data {
        if let Some(ts) = schema.tables.get(&table_name) {
            if ts.pk_strategy != PkStrategy::Rowid {
                let pk_lower = ts.pk_strategy.pk_column().to_ascii_lowercase();
                if let Some(pk_pos) = attrs.iter().position(|(name, _, _)| *name == pk_lower) {
                    if pk_pos != 0 {
                        attrs.swap(0, pk_pos);
                    }
                }
            }
        }
    }

    let children = node
        .children
        .iter()
        .map(|child| build_parsed_node(region_id, child, schema))
        .collect();

    ParsedNode {
        table_name,
        attrs,
        has_data,
        children,
    }
}

fn collect_resource_attribute(attr: &LsxNodeAttribute, attrs: &mut Vec<(String, String, String)>) {
    if attr.id.is_empty() || attr.id == "_OriginalFileVersion_" {
        return;
    }

    let attr_id_lower = attr.id.to_ascii_lowercase();
    let should_store_value = attr.handle.is_none()
        || attr.attr_type != "TranslatedString"
        || attr.value != attr.handle.as_deref().unwrap_or("");

    if should_store_value {
        attrs.push((
            attr_id_lower.clone(),
            attr.attr_type.clone(),
            attr.value.clone(),
        ));
    }

    if let Some(handle) = &attr.handle {
        let col_name = if attr.attr_type == "TranslatedString" {
            attr_id_lower.clone()
        } else {
            format!("{attr_id_lower}_handle")
        };
        let ts_type = if attr.attr_type.is_empty() {
            "TranslatedString".to_string()
        } else {
            attr.attr_type.clone()
        };
        attrs.push((col_name, ts_type, handle.clone()));
    }

    if attr.attr_type == "TranslatedString" {
        if let Some(version) = attr.version {
            attrs.push((
                format!("{attr_id_lower}_version"),
                "int32".to_string(),
                version.to_string(),
            ));
        }
    }
}

/// Insert a single LSX row. Returns the PK value (UUID/MapKey/ID) if available.
fn insert_lsx_row(
    tx: &Transaction,
    table_name: &str,
    file_id: i64,
    attrs: &[(String, String, String)],
    schema: &DiscoveredSchema,
) -> Result<Option<String>, String> {
    let ts = match schema.tables.get(table_name) {
        Some(ts) => ts,
        None => return Err(format!("Unknown table: {table_name}")),
    };

    // Build column list and values
    let mut col_names: Vec<String> = Vec::new();
    let mut values: Vec<SqlValue> = Vec::new();
    let mut pk_value: Option<String> = None;

    // _file_id
    if ts.has_file_id {
        col_names.push("_file_id".to_string());
        values.push(SqlValue::Integer(file_id));
    }

    // Build set of FK source columns for sentinel/empty → NULL coercion
    let fk_cols: HashSet<&str> = ts
        .fk_constraints
        .iter()
        .map(|fk| fk.source_column.as_str())
        .collect();

    // PK column (for non-rowid tables)
    // Rowid tables get their PK auto-assigned by SQLite.
    let pk_col = ts.pk_strategy.pk_column();
    if ts.pk_strategy != PkStrategy::Rowid {
        // Find PK in attrs (case-insensitive to match BG3 data inconsistencies)
        if let Some((_, _, val)) = attrs
            .iter()
            .find(|(name, _, _)| name.eq_ignore_ascii_case(pk_col))
        {
            col_names.push(pk_col.to_string());
            values.push(SqlValue::Text(val.clone()));
            pk_value = Some(val.clone());
        } else {
            // PK attribute missing — generate a synthetic key so the row isn't lost.
            // This happens for nodes that share a table with PK-bearing siblings
            // but lack the PK attribute themselves.
            use std::sync::atomic::{AtomicU64, Ordering};
            static SYNTH_COUNTER: AtomicU64 = AtomicU64::new(1);
            let synth = format!("_row_id_{}", SYNTH_COUNTER.fetch_add(1, Ordering::Relaxed));
            col_names.push(pk_col.to_string());
            values.push(SqlValue::Text(synth.clone()));
            pk_value = Some(synth);
        }
    }

    // Data columns
    for (col_name, bg3_type, value) in attrs {
        // Skip PK (already added)
        if col_name.eq_ignore_ascii_case(pk_col) && ts.pk_strategy != PkStrategy::Rowid {
            continue;
        }
        // Find matching column in schema (case-insensitive — BG3 data has
        // inconsistent casing like "automated" vs "Automated")
        if let Some(schema_col) = ts
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(col_name))
        {
            // For FK columns, coerce empty strings and sentinel values to NULL
            if fk_cols.contains(schema_col.name.as_str()) || bg3_type == "TranslatedString" {
                match coerce_fk_null(value) {
                    Some(v) => {
                        col_names.push(schema_col.name.clone());
                        values.push(types::coerce_value(v, bg3_type));
                    }
                    None => {
                        col_names.push(schema_col.name.clone());
                        values.push(SqlValue::Null);
                    }
                }
            } else {
                col_names.push(schema_col.name.clone());
                values.push(types::coerce_value(value, bg3_type));
            }
        }
    }

    // Build SQL
    let col_list: String = col_names
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders: String = (1..=values.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql =
        format!("INSERT OR IGNORE INTO \"{table_name}\" ({col_list}) VALUES ({placeholders})");

    let params: Vec<&dyn rusqlite::types::ToSql> = values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let rows_affected = tx
        .prepare_cached(&sql)
        .and_then(|mut stmt| stmt.execute(params.as_slice()))
        .map_err(|e| format!("Insert into '{table_name}': {e}"))?;

    // For Rowid tables, capture the auto-assigned rowid as the PK value.
    if ts.pk_strategy == PkStrategy::Rowid && rows_affected > 0 {
        pk_value = Some(tx.last_insert_rowid().to_string());
    }

    Ok(pk_value)
}

// ---------------------------------------------------------------------------
// Stats processing
// ---------------------------------------------------------------------------

fn process_stats(
    tx: &Transaction,
    file: &FileEntry,
    file_id: i64,
    schema: &DiscoveredSchema,
) -> Result<FileResult, String> {
    let content = read_text_content(file)?;

    let mut entry_name: Option<String> = None;
    let mut entry_type: Option<String> = None;
    let mut entry_using: Option<String> = None;
    let mut entry_data: HashMap<String, String> = HashMap::new();
    let mut total_rows = 0usize;
    let mut has_entries = false;
    let mut last_region_id: Option<String> = None;

    let flush = |tx: &Transaction,
                 name: &Option<String>,
                 etype: &Option<String>,
                 eusing: &Option<String>,
                 data: &HashMap<String, String>,
                 file_id: i64,
                 schema: &DiscoveredSchema|
     -> Result<bool, String> {
        let name = match name {
            Some(n) => n,
            None => return Ok(false),
        };
        let etype = match etype {
            Some(t) => t,
            None => return Ok(false),
        };

        let table_name = format!("stats__{}", sanitize_id(etype));
        if !schema.tables.contains_key(&table_name) {
            return Ok(false);
        }

        let mut col_names = vec![
            "_entry_name".to_string(),
            "_file_id".to_string(),
            "_type".to_string(),
            "_using".to_string(),
        ];
        let mut values: Vec<SqlValue> = vec![
            SqlValue::Text(name.clone()),
            SqlValue::Integer(file_id),
            SqlValue::Text(etype.clone()),
            match eusing.as_deref().and_then(coerce_fk_null) {
                Some(u) => SqlValue::Text(u.to_string()),
                None => SqlValue::Null,
            },
        ];

        // Build set of FK source columns for sentinel/empty → NULL coercion
        let stats_fk_cols: HashSet<&str> = schema
            .tables
            .get(&table_name)
            .map(|ts| {
                ts.fk_constraints
                    .iter()
                    .map(|fk| fk.source_column.as_str())
                    .collect()
            })
            .unwrap_or_default();

        for (k, v) in data {
            // Stats loca FK columns contain "contentuid;version" —
            // split into bare contentuid (for FK) and version (companion column).
            if is_stats_loca_column(k) {
                if let Some((cuid, ver)) = v.split_once(';') {
                    col_names.push(k.clone());
                    values.push(match coerce_fk_null(cuid) {
                        Some(c) => SqlValue::Text(c.to_string()),
                        None => SqlValue::Null,
                    });
                    col_names.push(format!("{k}_version"));
                    values.push(match ver.parse::<i64>() {
                        Ok(n) => SqlValue::Integer(n),
                        Err(_) => SqlValue::Text(ver.to_string()),
                    });
                } else {
                    // No semicolon — store as-is with FK coercion
                    col_names.push(k.clone());
                    values.push(match coerce_fk_null(v) {
                        Some(s) => SqlValue::Text(s.to_string()),
                        None => SqlValue::Null,
                    });
                }
            } else if stats_fk_cols.contains(k.as_str()) {
                // Non-loca FK column — coerce empty/sentinel to NULL
                col_names.push(k.clone());
                values.push(match coerce_fk_null(v) {
                    Some(s) => SqlValue::Text(s.to_string()),
                    None => SqlValue::Null,
                });
            } else {
                col_names.push(k.clone());
                values.push(SqlValue::Text(v.clone()));
            }
        }

        let col_list: String = col_names
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let placeholders: String = (1..=values.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql =
            format!("INSERT OR IGNORE INTO \"{table_name}\" ({col_list}) VALUES ({placeholders})");

        let params: Vec<&dyn rusqlite::types::ToSql> = values
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        tx.prepare_cached(&sql)
            .and_then(|mut stmt| stmt.execute(params.as_slice()))
            .map_err(|e| format!("Insert stats: {e}"))?;

        Ok(true)
    };

    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }

        if let Some(rest) = strip_prefix_ci(t, "new entry \"") {
            if let Some(end) = rest.find('"') {
                // Flush previous entry
                if flush(
                    tx,
                    &entry_name,
                    &entry_type,
                    &entry_using,
                    &entry_data,
                    file_id,
                    schema,
                )? {
                    total_rows += 1;
                }
                entry_name = Some(rest[..end].to_string());
                entry_type = None;
                entry_using = None;
                entry_data.clear();
                has_entries = true;
            }
        } else if let Some(rest) = strip_prefix_ci(t, "type \"") {
            if let Some(end) = rest.find('"') {
                entry_type = Some(rest[..end].to_string());
                last_region_id = entry_type.clone();
            }
        } else if let Some(rest) = strip_prefix_ci(t, "using \"") {
            if let Some(end) = rest.find('"') {
                entry_using = Some(rest[..end].to_string());
            }
        } else if let Some(rest) = strip_prefix_ci(t, "data \"") {
            // Parse: data "field" "value"
            if let Some(end) = rest.find('"') {
                let field = &rest[..end];
                let remainder = &rest[end + 1..];
                // Find the value part: skip whitespace and opening quote
                if let Some(vstart) = remainder.find('"') {
                    let val_part = &remainder[vstart + 1..];
                    // Find the closing quote (last quote in the line)
                    let value = if let Some(vend) = val_part.rfind('"') {
                        &val_part[..vend]
                    } else {
                        val_part.trim()
                    };
                    entry_data.insert(field.to_string(), value.to_string());
                }
            }
        }
    }

    // Flush last entry
    if flush(
        tx,
        &entry_name,
        &entry_type,
        &entry_using,
        &entry_data,
        file_id,
        schema,
    )? {
        total_rows += 1;
    }

    // Raw text fallback
    if !has_entries && !content.trim().is_empty() && schema.tables.contains_key("txt__raw") {
        tx.prepare_cached("INSERT INTO \"txt__raw\" (_file_id, content) VALUES (?1, ?2)")
            .and_then(|mut stmt| stmt.execute(params![file_id, content]))
            .map_err(|e| format!("Insert raw txt: {e}"))?;
        total_rows += 1;
    }

    Ok(FileResult {
        rows: total_rows,
        errors: 0,
        region_id: last_region_id,
    })
}

/// Case-insensitive prefix strip.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Loca processing
// ---------------------------------------------------------------------------

fn process_loca(
    tx: &Transaction,
    file: &FileEntry,
    file_id: i64,
    schema: &DiscoveredSchema,
) -> Result<FileResult, String> {
    if !schema.tables.contains_key("loca__english") {
        return Ok(FileResult {
            rows: 0,
            errors: 0,
            region_id: None,
        });
    }

    let entries = parse_loca_entries(file)?;
    if entries.is_empty() {
        return Ok(FileResult {
            rows: 0,
            errors: 0,
            region_id: None,
        });
    }

    let mut total_rows = 0usize;

    for entry in entries {
        tx.prepare_cached(
            "INSERT OR IGNORE INTO \"loca__english\" (contentuid, _file_id, version, text) VALUES (?1, ?2, ?3, ?4)",
        )
        .and_then(|mut stmt| stmt.execute(params![entry.contentuid, file_id, entry.version, entry.text]))
        .map_err(|e| format!("Insert loca: {e}"))?;
        total_rows += 1;
    }

    Ok(FileResult {
        rows: total_rows,
        errors: 0,
        region_id: None,
    })
}

fn decode_xml_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
}

fn parse_loca_entries(file: &FileEntry) -> Result<Vec<ParsedLocaEntry>, String> {
    match file.extension() {
        Some(ext) if ext == "loca" => parse_loca_binary(file).map(|entries| {
            entries
                .into_iter()
                .map(|entry| ParsedLocaEntry {
                    contentuid: entry.key,
                    version: i64::from(entry.version),
                    text: Some(entry.text),
                })
                .collect()
        }),
        _ => parse_loca_xml_entries(file),
    }
}

fn parse_loca_binary(file: &FileEntry) -> Result<Vec<loca_parser::LocaEntry>, String> {
    if let Some(bytes) = file.in_memory_bytes() {
        loca_parser::parse_loca(Cursor::new(bytes))
    } else {
        loca_parser::parse_loca_file(&file.abs_path)
    }
}

fn parse_loca_xml_entries(file: &FileEntry) -> Result<Vec<ParsedLocaEntry>, String> {
    let content = read_text_content(file)?;

    if content.trim().is_empty()
        || content.contains("<contentList />")
        || content.contains("<contentList/>")
    {
        return Ok(Vec::new());
    }

    use std::sync::LazyLock;
    static LOCA_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r#"<content\s+contentuid="([^"]*)"\s+version="(\d+)"(?:\s*/>|>([\s\S]*?)</content>)"#,
        )
        .unwrap()
    });

    Ok(LOCA_RE
        .captures_iter(&content)
        .map(|caps| {
            let uid = caps
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let version = caps
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let text = caps.get(3).map(|m| decode_xml_entities(m.as_str()));
            ParsedLocaEntry {
                contentuid: uid,
                version,
                text,
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// AllSpark ingestion (XCD / XMD)
// ---------------------------------------------------------------------------

fn process_allspark(
    tx: &Transaction,
    file: &FileEntry,
    file_id: i64,
    schema: &DiscoveredSchema,
) -> Result<FileResult, String> {
    let content = read_text_content(file)?;
    if content.trim().is_empty() {
        return Ok(FileResult {
            rows: 0,
            errors: 0,
            region_id: None,
        });
    }

    let mut registry = crate::allspark::AllSparkRegistry::default();
    let ext = file.extension().unwrap_or_default();

    match ext.as_str() {
        "xcd" => registry.parse_xcd(&content)?,
        "xmd" => registry.parse_xmd(&content)?,
        _ => {
            return Ok(FileResult {
                rows: 0,
                errors: 0,
                region_id: None,
            })
        }
    }

    let mut total_rows = 0usize;

    // --- Components ---
    if schema.tables.contains_key("allspark__components") {
        for (name, comp) in &registry.components {
            tx.prepare_cached(
                "INSERT OR IGNORE INTO \"allspark__components\" (_file_id, name, tooltip, color) VALUES (?1, ?2, ?3, ?4)",
            )
            .and_then(|mut stmt| stmt.execute(params![file_id, name, comp.tooltip, comp.color]))
            .map_err(|e| format!("Insert allspark component: {e}"))?;
            total_rows += 1;
        }
    }

    // --- Properties ---
    if schema.tables.contains_key("allspark__properties") {
        for (comp_name, comp) in &registry.components {
            for (guid, prop) in &comp.properties {
                tx.prepare_cached(
                    "INSERT OR IGNORE INTO \"allspark__properties\" \
                     (_file_id, guid, component_name, name, type_name, specializable, tooltip, default_value) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .and_then(|mut stmt| stmt.execute(params![
                    file_id, guid, comp_name, prop.name, prop.type_name,
                    prop.specializable as i32, prop.tooltip, prop.default_value,
                ]))
                .map_err(|e| format!("Insert allspark property: {e}"))?;
                total_rows += 1;
            }
        }
    }

    // --- Property groups ---
    if schema.tables.contains_key("allspark__property_groups") {
        for (comp_name, comp) in &registry.components {
            for (idx, group) in comp.property_groups.iter().enumerate() {
                tx.prepare_cached(
                    "INSERT OR IGNORE INTO \"allspark__property_groups\" \
                     (_file_id, guid, component_name, name, collapsed, sort_order) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![
                        file_id,
                        group.guid,
                        comp_name,
                        group.name,
                        group.collapsed,
                        idx as i64,
                    ])
                })
                .map_err(|e| format!("Insert allspark property group: {e}"))?;
                total_rows += 1;
            }
        }
    }

    // --- Property group refs ---
    if schema.tables.contains_key("allspark__property_group_refs") {
        for (comp_name, comp) in &registry.components {
            for group in &comp.property_groups {
                for (idx, prop_guid) in group.property_refs.iter().enumerate() {
                    tx.prepare_cached(
                        "INSERT OR IGNORE INTO \"allspark__property_group_refs\" \
                         (_file_id, group_guid, component_name, property_guid, sort_order) \
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                    )
                    .and_then(|mut stmt| {
                        stmt.execute(params![
                            file_id, group.guid, comp_name, prop_guid, idx as i64,
                        ])
                    })
                    .map_err(|e| format!("Insert allspark property group ref: {e}"))?;
                    total_rows += 1;
                }
            }
        }
    }

    // --- Modules ---
    if schema.tables.contains_key("allspark__modules") {
        for module in registry.modules.values() {
            tx.prepare_cached(
                "INSERT OR IGNORE INTO \"allspark__modules\" (_file_id, guid, name) VALUES (?1, ?2, ?3)",
            )
            .and_then(|mut stmt| stmt.execute(params![file_id, module.guid, module.name]))
            .map_err(|e| format!("Insert allspark module: {e}"))?;
            total_rows += 1;
        }
    }

    // --- Module properties ---
    if schema.tables.contains_key("allspark__module_properties") {
        for module in registry.modules.values() {
            for (guid, prop_name) in &module.properties {
                tx.prepare_cached(
                    "INSERT OR IGNORE INTO \"allspark__module_properties\" \
                     (_file_id, module_guid, property_guid, property_name) \
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .and_then(|mut stmt| stmt.execute(params![file_id, module.guid, guid, prop_name]))
                .map_err(|e| format!("Insert allspark module property: {e}"))?;
                total_rows += 1;
            }
        }
    }

    Ok(FileResult {
        rows: total_rows,
        errors: 0,
        region_id: None,
    })
}

// ---------------------------------------------------------------------------
// Effect ingestion (.lsefx)
// ---------------------------------------------------------------------------

fn process_effect(
    tx: &Transaction,
    file: &FileEntry,
    file_id: i64,
    schema: &DiscoveredSchema,
) -> Result<FileResult, String> {
    let content = read_text_content(file)?;
    if content.trim().is_empty() {
        return Ok(FileResult {
            rows: 0,
            errors: 0,
            region_id: None,
        });
    }

    let effect = crate::parsers::lsefx::read_lsefx(&content)?;
    let source_file = &file.rel_path;
    let effect_id = &effect.id;
    let mut total_rows = 0usize;

    // --- Effect header ---
    if schema.tables.contains_key("effect__effects") {
        tx.prepare_cached(
            "INSERT OR IGNORE INTO \"effect__effects\" \
             (_file_id, id, version, effect_version, phases_xml, colors_xml, source_file) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .and_then(|mut stmt| {
            stmt.execute(params![
                file_id,
                effect_id,
                effect.version,
                effect.effect_version,
                effect.phases_xml,
                effect.colors_xml,
                source_file,
            ])
        })
        .map_err(|e| format!("Insert effect: {e}"))?;
        total_rows += 1;
    }

    // --- Components, Properties, Datums ---
    let has_components = schema.tables.contains_key("effect__components");
    let has_properties = schema.tables.contains_key("effect__properties");
    let has_datums = schema.tables.contains_key("effect__datums");

    for tg in &effect.track_groups {
        for track in &tg.tracks {
            for comp in &track.components {
                if has_components {
                    tx.prepare_cached(
                        "INSERT INTO \"effect__components\" \
                         (_file_id, instance_name, class_name, start, \"end\", track_name, track_group_name, effect_id, source_file) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    )
                    .and_then(|mut stmt| stmt.execute(params![
                        file_id, comp.instance_name, comp.class_name,
                        comp.start, comp.end, track.name, tg.name,
                        effect_id, source_file,
                    ]))
                    .map_err(|e| format!("Insert effect component: {e}"))?;
                    total_rows += 1;
                }

                for prop in &comp.properties {
                    if has_properties {
                        tx.prepare_cached(
                            "INSERT INTO \"effect__properties\" \
                             (_file_id, property_guid, component_instance, effect_id, source_file) \
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                        )
                        .and_then(|mut stmt| {
                            stmt.execute(params![
                                file_id,
                                prop.guid,
                                comp.instance_name,
                                effect_id,
                                source_file,
                            ])
                        })
                        .map_err(|e| format!("Insert effect property: {e}"))?;
                        total_rows += 1;
                    }

                    if has_datums {
                        for datum in &prop.data {
                            let value = datum.value.as_deref().unwrap_or("");
                            tx.prepare_cached(
                                "INSERT INTO \"effect__datums\" \
                                 (_file_id, property_guid, component_instance, platform, lod, value, effect_id, source_file) \
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                            )
                            .and_then(|mut stmt| stmt.execute(params![
                                file_id, prop.guid, comp.instance_name,
                                datum.platform, datum.lod, value,
                                effect_id, source_file,
                            ]))
                            .map_err(|e| format!("Insert effect datum: {e}"))?;
                            total_rows += 1;
                        }
                    }
                }
            }
        }
    }

    Ok(FileResult {
        rows: total_rows,
        errors: 0,
        region_id: None,
    })
}

// ---------------------------------------------------------------------------
// Post-processing
// ---------------------------------------------------------------------------

fn populate_table_meta(
    conn: &Connection,
    schema: &DiscoveredSchema,
    row_counts: &HashMap<String, i64>,
) -> Result<(), String> {
    let use_tracked_counts = !row_counts.is_empty();
    let mut stmt = conn
        .prepare(
            "INSERT OR REPLACE INTO _table_meta
             (table_name, source_type, region_id, node_id, parent_tables, row_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .map_err(|e| format!("Prepare meta: {e}"))?;

    for (table_name, ts) in &schema.tables {
        // Use pre-tracked row counts when available, otherwise COUNT(*)
        let row_count = if use_tracked_counts {
            row_counts.get(table_name).copied().unwrap_or(0)
        } else {
            conn.query_row(
                &format!("SELECT COUNT(*) FROM \"{table_name}\""),
                [],
                |row| row.get(0),
            )
            .unwrap_or(0)
        };

        let parent_str = if ts.parent_tables.is_empty() {
            None
        } else {
            let mut parents: Vec<&str> = ts.parent_tables.iter().map(|s| s.as_str()).collect();
            parents.sort();
            Some(parents.join(","))
        };

        stmt.execute(params![
            table_name,
            ts.source_type,
            ts.region_id,
            ts.node_id,
            parent_str,
            row_count,
        ])
        .map_err(|e| format!("Insert meta for '{table_name}': {e}"))?;
    }

    // Junction table metadata — still uses COUNT(*) since junction rows
    // are inserted via separate paths and tracking would add complexity
    for jt in &schema.junction_tables {
        let row_count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM \"{}\"", jt.table_name),
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let parent_str = Some(format!("{},{}", jt.parent_table, jt.child_table));

        stmt.execute(params![
            jt.table_name,
            "junction",
            Option::<String>::None,
            Option::<String>::None,
            parent_str,
            row_count,
        ])
        .map_err(|e| format!("Insert meta for junction '{}': {}", jt.table_name, e))?;
    }

    Ok(())
}

fn build_indexes(conn: &Connection, schema: &DiscoveredSchema) -> Result<(), String> {
    for (table_name, ts) in &schema.tables {
        // _file_id index
        if ts.has_file_id {
            conn.execute_batch(&format!(
                "CREATE INDEX IF NOT EXISTS \"idx_{table_name}__fid\" ON \"{table_name}\"(\"_file_id\")"
            ))
            .map_err(|e| format!("Index error: {e}"))?;
        }

        // _parent_key index — removed: parent-child relationships are now
        // tracked via junction tables.

        // Stats entry name (it's the PK so already indexed, but let's be explicit)
        if table_name.starts_with("stats__") && ts.columns.iter().any(|c| c.name == "_entry_name") {
            conn.execute_batch(&format!(
                    "CREATE INDEX IF NOT EXISTS \"idx_{table_name}__ent\" ON \"{table_name}\"(\"_entry_name\")"
                ))
                .map_err(|e| format!("Index error: {e}"))?;
        }

        // Loca contentuid (it's the PK so already indexed)
        // No additional indexes needed for PKs
    }
    Ok(())
}

fn populate_column_types(conn: &Connection, schema: &DiscoveredSchema) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "INSERT OR IGNORE INTO _column_types (table_name, column_name, bg3_type, sqlite_type)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .map_err(|e| format!("Prepare column types: {e}"))?;

    for (table_name, ts) in &schema.tables {
        for col in &ts.columns {
            stmt.execute(params![table_name, col.name, col.bg3_type, col.sqlite_type])
                .map_err(|e| format!("Insert column type: {e}"))?;
        }
    }

    Ok(())
}

/// Insert placeholder rows into `loca__english` for any FK-referenced handles
/// that don't exist, so consumers get a clear "Invalid or Unknown" message
/// instead of a broken FK.
fn backfill_orphaned_loca(
    conn: &Connection,
    schema: &DiscoveredSchema,
    fallback_base_db_path: Option<&Path>,
) -> Result<(), String> {
    const LOCA_TABLE: &str = "loca__english";
    const PLACEHOLDER_TEXT: &str = "Invalid or Unknown Localization Handle Provided";
    const SYNTHETIC_FILE_ID: i64 = -1;

    if !schema.tables.contains_key(LOCA_TABLE) {
        return Ok(());
    }

    // Insert a synthetic _source_files entry so placeholder rows satisfy the
    // loca__english._file_id → _source_files(file_id) FK.
    conn.execute(
        "INSERT OR IGNORE INTO _source_files (file_id, path, file_type, mod_name, region_id, row_count, file_size)
         VALUES (?1, 'synthetic:loca_placeholder', 'synthetic', NULL, NULL, 0, 0)",
        params![SYNTHETIC_FILE_ID],
    )
    .map_err(|e| format!("Insert synthetic source file: {e}"))?;

    // Collect all (table, column) pairs that have an FK → loca__english
    let mut loca_fk_cols: Vec<(String, String)> = Vec::new();
    for (table_name, ts) in &schema.tables {
        for fk in &ts.fk_constraints {
            if fk.target_table == LOCA_TABLE && fk.target_column == "contentuid" {
                // Verify the source column actually exists
                let col_exists = ts.columns.iter().any(|c| c.name == fk.source_column)
                    || fk.source_column == ts.pk_strategy.pk_column();
                if col_exists {
                    loca_fk_cols.push((table_name.clone(), fk.source_column.clone()));
                }
            }
        }
    }

    if loca_fk_cols.is_empty() {
        return Ok(());
    }

    let attached_here = attach_base_fallback_if_needed(conn, fallback_base_db_path)?;
    let base_attached = attached_here
        || cross_db::list_attached(conn)?
            .iter()
            .any(|(alias, _)| alias == ALIAS_BASE);
    let base_has_loca = if base_attached {
        has_table_in_schema(conn, ALIAS_BASE, LOCA_TABLE)?
    } else {
        false
    };

    // Build a UNION query to find all orphaned loca handles
    let unions: Vec<String> = loca_fk_cols
        .iter()
        .map(|(tbl, col)| {
            format!(
                "SELECT DISTINCT \"{col}\" AS handle FROM \"{tbl}\" WHERE \"{col}\" IS NOT NULL AND \"{col}\" != ''",
            )
        })
        .collect();

    let mut existing_queries = vec![format!("SELECT contentuid FROM main.\"{}\"", LOCA_TABLE)];
    if base_has_loca {
        existing_queries.push(format!(
            "SELECT contentuid FROM \"{ALIAS_BASE}\".\"{LOCA_TABLE}\""
        ));
    }

    let sql = format!(
        "WITH refs AS ({unions}),
              existing AS ({existing})
         INSERT OR IGNORE INTO \"{loca}\" (contentuid, _file_id, version, text)
         SELECT handle, -1, 0, ?1
         FROM refs
         WHERE NOT EXISTS (
             SELECT 1 FROM existing WHERE existing.contentuid = refs.handle
         )",
        loca = LOCA_TABLE,
        unions = unions.join(" UNION "),
        existing = existing_queries.join(" UNION "),
    );

    let inserted: usize = conn
        .execute(&sql, params![PLACEHOLDER_TEXT])
        .map_err(|e| format!("Backfill orphaned loca: {e}"))?;

    if attached_here {
        cross_db::detach(conn, ALIAS_BASE)?;
    }

    if inserted > 0 {
        eprintln!("  Backfilled {inserted} orphaned loca handles with placeholder text");
        // Update the synthetic source file row count
        conn.execute(
            "UPDATE _source_files SET row_count = ?1 WHERE file_id = ?2",
            params![inserted as i64, SYNTHETIC_FILE_ID],
        )
        .map_err(|e| format!("Update synthetic source file: {e}"))?;
    }

    Ok(())
}

fn resolve_base_fallback_db_path(
    db_path: &Path,
    options: &BuildOptions,
) -> Option<std::path::PathBuf> {
    if let Some(path) = options
        .fallback_base_db_path
        .as_ref()
        .filter(|p| p.is_file())
    {
        return Some(path.clone());
    }

    let file_name = db_path.file_name()?.to_str()?;
    let candidate_names = [
        file_name.replacen("honor", "base", 1),
        file_name.replacen("Honor", "Base", 1),
        String::from("ref_base.sqlite"),
    ];

    candidate_names
        .into_iter()
        .filter(|candidate| candidate != file_name)
        .map(|candidate| db_path.with_file_name(candidate))
        .find(|candidate| candidate.is_file())
}

fn attach_base_fallback_if_needed(
    conn: &Connection,
    fallback_base_db_path: Option<&Path>,
) -> Result<bool, String> {
    let Some(path) = fallback_base_db_path else {
        return Ok(false);
    };

    let already_attached = cross_db::list_attached(conn)?
        .iter()
        .any(|(alias, _)| alias == ALIAS_BASE);
    if already_attached {
        return Ok(false);
    }

    cross_db::attach_readonly(conn, path, ALIAS_BASE)?;
    Ok(true)
}

fn has_table_in_schema(
    conn: &Connection,
    schema_alias: &str,
    table_name: &str,
) -> Result<bool, String> {
    let sql = format!(
        "SELECT COUNT(*) FROM \"{schema_alias}\".sqlite_master WHERE type='table' AND name=?1"
    );
    let count: i64 = conn
        .query_row(&sql, params![table_name], |row| row.get(0))
        .map_err(|e| format!("Check table {schema_alias}.{table_name}: {e}"))?;
    Ok(count > 0)
}

// ===========================================================================
// Optimized populate infrastructure
// ===========================================================================
// Pre-computed insert plans eliminate per-row SQL construction.
// Parallel file parsing overlaps CPU (XML parsing) with I/O (disk + DB).

// ---------------------------------------------------------------------------
// Source ID cache — maps module name → integer PK in _sources table
// ---------------------------------------------------------------------------

/// Lazily resolves module names to integer IDs in the `_sources` meta table.
/// Only ~6 unique module names exist, so this is a tiny in-memory HashMap
/// that eliminates 13M+ per-row string allocations for `_SourceID`.
struct SourceIdCache {
    cache: HashMap<String, i64>,
    next_id: i64,
}

impl SourceIdCache {
    fn new() -> Self {
        Self {
            cache: HashMap::with_capacity(8),
            next_id: 1,
        }
    }

    /// Load existing `_sources` rows from the connection and seed the cache.
    /// Used when `_sources` is pre-populated (e.g. for parallel insert partitions).
    fn new_from_existing(conn: &Connection) -> Result<Self, String> {
        let mut cache = HashMap::with_capacity(8);
        let mut max_id = 0i64;
        let mut stmt = conn
            .prepare("SELECT id, name FROM _sources")
            .map_err(|e| format!("Load _sources: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| format!("Query _sources: {e}"))?;
        for row in rows {
            let (id, name) = row.map_err(|e| format!("Read _sources row: {e}"))?;
            if id > max_id {
                max_id = id;
            }
            cache.insert(name, id);
        }
        Ok(Self {
            cache,
            next_id: max_id + 1,
        })
    }

    /// Get-or-insert the integer ID for `mod_name`.
    fn resolve(&mut self, tx: &Transaction, mod_name: &str) -> Result<i64, String> {
        if let Some(&id) = self.cache.get(mod_name) {
            return Ok(id);
        }
        let id = self.next_id;
        tx.prepare_cached("INSERT OR IGNORE INTO _sources (id, name) VALUES (?1, ?2)")
            .and_then(|mut stmt| stmt.execute(params![id, mod_name]))
            .map_err(|e| format!("Insert _sources: {e}"))?;
        self.cache.insert(mod_name.to_string(), id);
        self.next_id += 1;
        Ok(id)
    }
}

// ---------------------------------------------------------------------------
// Pre-computed insert plans
// ---------------------------------------------------------------------------

/// Pre-computed INSERT statement and column mapping for a single table.
/// Created once before data insertion, reused for every row.
struct TableInsertPlan {
    /// `INSERT OR IGNORE INTO "table" (col1, col2, ...) VALUES (?1, ?2, ...)`
    sql: String,
    /// Lowercase column name → 0-based parameter index.
    col_map: HashMap<String, usize>,
    /// 0-based index of the `_file_id` parameter.
    file_id_idx: usize,
    /// 0-based index of the `_SourceID` parameter (None for Rowid tables).
    source_id_idx: Option<usize>,
    /// 0-based index of the PK parameter (None for Rowid tables).
    pk_idx: Option<usize>,
    /// Pre-computed lowercase PK column name.
    pk_col_lower: String,
    /// PK strategy for this table.
    pk_strategy: PkStrategy,
    /// Lowercase names of FK source columns (for NULL coercion).
    fk_cols: HashSet<String>,
    /// Reusable row buffer — reset to NULLs before each row.
    row_buf: Vec<SqlValue>,
}

/// Pre-built junction table INSERT SQL, indexed by junction table name.
struct JunctionPlan {
    sql: String,
}

/// Build insert plans for every table in the schema.
fn build_insert_plans(schema: &DiscoveredSchema) -> HashMap<String, TableInsertPlan> {
    let mut plans = HashMap::with_capacity(schema.tables.len());

    for (table_name, ts) in &schema.tables {
        let mut col_names: Vec<String> = Vec::new();
        let mut col_map: HashMap<String, usize> = HashMap::new();
        let mut idx = 0usize;

        // PK column first (non-Rowid tables)
        let pk_idx = if ts.pk_strategy != PkStrategy::Rowid {
            let pk_col = ts.pk_strategy.pk_column();
            col_names.push(pk_col.to_string());
            col_map.insert(pk_col.to_ascii_lowercase(), idx);
            let pi = idx;
            idx += 1;
            Some(pi)
        } else {
            None
        };

        // _file_id
        let file_id_idx = idx;
        if ts.has_file_id {
            col_names.push("_file_id".to_string());
            idx += 1;
        }

        // _SourceID (non-Rowid tables only)
        let source_id_idx = if ts.pk_strategy != PkStrategy::Rowid {
            col_names.push("_SourceID".to_string());
            let si = idx;
            idx += 1;
            Some(si)
        } else {
            None
        };

        // Data columns (sorted, same order as schema)
        for col in &ts.columns {
            // Skip PK column if it already appears as PK
            if pk_idx.is_some() && col.name.eq_ignore_ascii_case(ts.pk_strategy.pk_column()) {
                continue;
            }
            col_map.insert(col.name.to_ascii_lowercase(), idx);
            col_names.push(col.name.clone());
            idx += 1;
        }

        let col_list: String = col_names
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let placeholders: String = (1..=idx)
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql =
            format!("INSERT OR IGNORE INTO \"{table_name}\" ({col_list}) VALUES ({placeholders})");

        let fk_cols: HashSet<String> = ts
            .fk_constraints
            .iter()
            .map(|fk| fk.source_column.to_ascii_lowercase())
            .collect();

        let row_buf = vec![SqlValue::Null; idx];

        plans.insert(
            table_name.clone(),
            TableInsertPlan {
                sql,
                col_map,
                file_id_idx,
                source_id_idx,
                pk_idx,
                pk_col_lower: ts.pk_strategy.pk_column().to_ascii_lowercase(),
                pk_strategy: ts.pk_strategy.clone(),
                fk_cols,
                row_buf,
            },
        );
    }

    plans
}

/// Build pre-computed junction INSERT SQL for all junction tables.
fn build_junction_plans(schema: &DiscoveredSchema) -> HashMap<String, JunctionPlan> {
    let mut plans = HashMap::with_capacity(schema.junction_tables.len());
    for jt in &schema.junction_tables {
        let sql = format!(
            "INSERT OR IGNORE INTO \"{}\" (parent_id, child_id) VALUES (?1, ?2)",
            jt.table_name,
        );
        plans.insert(jt.table_name.clone(), JunctionPlan { sql });
    }
    plans
}

// ---------------------------------------------------------------------------
// Parsed file structures (produced by parallel parse, consumed by inserter)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PopulateCounts {
    total_rows: usize,
    row_errors: usize,
    file_errors: usize,
}

/// Parsed content of a single file — all data needed for insertion.
struct ParsedFile {
    file_size: i64,
    mod_name: String,
    content: ParsedContent,
}

enum ParsedContent {
    Lsx {
        last_region_id: Option<String>,
        root_nodes: Vec<ParsedNode>,
    },
    Stats {
        entries: Vec<ParsedStatsEntry>,
        last_type: Option<String>,
        /// Raw text for files with no `new entry` statements.
        raw_content: Option<String>,
    },
    Loca {
        entries: Vec<ParsedLocaEntry>,
    },
    Equipment {
        entries: Vec<ParsedEquipmentEntry>,
    },
    ValueLists {
        entries: Vec<ParsedValueListRow>,
    },
    Modifiers {
        entries: Vec<ParsedModifierRow>,
    },
    Empty,
}

/// A single LSX node with its attributes and children (tree structure).
struct ParsedNode {
    table_name: String,
    attrs: Vec<(String, String, String)>, // (col_name, bg3_type, value)
    has_data: bool,
    children: Vec<ParsedNode>,
}

struct ParsedStatsEntry {
    table_name: String,
    entry_name: String,
    entry_type: String,
    entry_using: Option<String>,
    data: Vec<(String, String)>, // (field_name, value)
}

struct ParsedLocaEntry {
    contentuid: String,
    version: i64,
    text: Option<String>,
}

struct ParsedEquipmentEntry {
    name: String,
}

struct ParsedValueListRow {
    list_key: String,
    value_name: String,
    value_index: i64,
}

struct ParsedModifierRow {
    type_name: String,
    field_name: String,
    field_type: String,
}

// ---------------------------------------------------------------------------
// Parallel file parsing
// ---------------------------------------------------------------------------

/// Parse a file into a ParsedFile without any DB access.
/// Safe to call from multiple threads concurrently.
fn parse_file(file: &FileEntry, schema: &DiscoveredSchema) -> ParsedFile {
    let file_size = file.file_size();
    let mod_name = file.mod_name.clone();

    let content = match file.file_type {
        FileType::Lsx => parse_lsx_file(file, schema),
        FileType::Stats => parse_stats_file(file, schema),
        FileType::Loca => parse_loca_file(file),
        // AllSpark + Effect are processed directly in process_file (not pre-parsed)
        FileType::AllSpark | FileType::Effect => ParsedContent::Empty,
    };

    ParsedFile {
        file_size,
        mod_name,
        content,
    }
}

fn parse_lsx_file(file: &FileEntry, schema: &DiscoveredSchema) -> ParsedContent {
    let resource = match parse_lsx_resource_file(file) {
        Ok(resource) => resource,
        Err(_) => return ParsedContent::Empty,
    };
    parsed_content_from_resource(&resource, schema)
}

fn parsed_content_from_resource(
    resource: &LsxResource,
    schema: &DiscoveredSchema,
) -> ParsedContent {
    let mut last_region_id = None;
    let mut root_nodes = Vec::new();

    for region in &resource.regions {
        if SKIP_REGIONS.contains(&region.id.as_str()) {
            continue;
        }

        last_region_id = Some(region.id.clone());
        root_nodes.extend(
            region
                .nodes
                .iter()
                .map(|node| build_parsed_node(&region.id, node, schema)),
        );
    }

    if root_nodes.is_empty() {
        ParsedContent::Empty
    } else {
        ParsedContent::Lsx {
            last_region_id,
            root_nodes,
        }
    }
}

fn parse_stats_file(file: &FileEntry, schema: &DiscoveredSchema) -> ParsedContent {
    let content = match read_text_content(file) {
        Ok(c) => c,
        Err(_) => return ParsedContent::Empty,
    };
    if content.trim().is_empty() {
        return ParsedContent::Empty;
    }

    // Detect format by first significant line
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }
        if strip_prefix_ci(t, "new equipment \"").is_some() {
            return parse_equipment_content(&content, schema);
        }
        if strip_prefix_ci(t, "valuelist \"").is_some() {
            return parse_valuelist_content(&content, schema);
        }
        if strip_prefix_ci(t, "modifier type \"").is_some() {
            return parse_modifier_content(&content, schema);
        }
        if strip_prefix_ci(t, "new entry \"").is_some() {
            break; // Standard stats format
        }
        // Non-standard: key "..." or new <keyword> "..." (not entry/equipment)
        if t.starts_with("key ")
            || t.starts_with("key\t")
            || (t.starts_with("new ") && t.contains('"'))
        {
            let file_stem = file
                .abs_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Unknown");
            return parse_nonstandard_stats_content(&content, file_stem, schema);
        }
        break;
    }

    parse_stats_entries_content(&content, schema)
}

/// Parse `new entry` blocks (the original stats format).
fn parse_stats_entries_content(content: &str, schema: &DiscoveredSchema) -> ParsedContent {
    let mut entries: Vec<ParsedStatsEntry> = Vec::new();
    let mut entry_name: Option<String> = None;
    let mut entry_type: Option<String> = None;
    let mut entry_using: Option<String> = None;
    let mut entry_data: Vec<(String, String)> = Vec::new();
    let mut has_entries = false;
    let mut last_type: Option<String> = None;

    let flush = |entries: &mut Vec<ParsedStatsEntry>,
                 name: &Option<String>,
                 etype: &Option<String>,
                 eusing: &Option<String>,
                 data: &[(String, String)],
                 schema: &DiscoveredSchema| {
        if let (Some(name), Some(etype)) = (name, etype) {
            let table_name = format!("stats__{}", sanitize_id(etype));
            if schema.tables.contains_key(&table_name) {
                entries.push(ParsedStatsEntry {
                    table_name,
                    entry_name: name.clone(),
                    entry_type: etype.clone(),
                    entry_using: eusing.clone(),
                    data: data.to_vec(),
                });
            }
        }
    };

    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }

        if let Some(rest) = strip_prefix_ci(t, "new entry \"") {
            if let Some(end) = rest.find('"') {
                flush(
                    &mut entries,
                    &entry_name,
                    &entry_type,
                    &entry_using,
                    &entry_data,
                    schema,
                );
                entry_name = Some(rest[..end].to_string());
                entry_type = None;
                entry_using = None;
                entry_data.clear();
                has_entries = true;
            }
        } else if let Some(rest) = strip_prefix_ci(t, "type \"") {
            if let Some(end) = rest.find('"') {
                entry_type = Some(rest[..end].to_string());
                last_type = entry_type.clone();
            }
        } else if let Some(rest) = strip_prefix_ci(t, "using \"") {
            if let Some(end) = rest.find('"') {
                entry_using = Some(rest[..end].to_string());
            }
        } else if let Some(rest) = strip_prefix_ci(t, "data \"") {
            if let Some(end) = rest.find('"') {
                let field = &rest[..end];
                let remainder = &rest[end + 1..];
                if let Some(vstart) = remainder.find('"') {
                    let val_part = &remainder[vstart + 1..];
                    let value = if let Some(vend) = val_part.rfind('"') {
                        &val_part[..vend]
                    } else {
                        val_part.trim()
                    };
                    entry_data.push((field.to_string(), value.to_string()));
                }
            }
        }
    }

    // Flush last entry
    flush(
        &mut entries,
        &entry_name,
        &entry_type,
        &entry_using,
        &entry_data,
        schema,
    );

    ParsedContent::Stats {
        entries,
        last_type,
        raw_content: if !has_entries && !content.trim().is_empty() {
            Some(content.to_string())
        } else {
            None
        },
    }
}

/// Parse non-standard stats file formats: CSV entries, key-value pairs, groups.
///
/// Handles:
/// - `new <keyword> "name","v1","v2",...` (BloodTypes, ItemColor)
/// - `key "name","value"` (XPData)
/// - `new <keyword> "name"` + `add <sub> ...` (ItemProgressionNames/Visuals)
fn parse_nonstandard_stats_content(
    content: &str,
    file_stem: &str,
    schema: &DiscoveredSchema,
) -> ParsedContent {
    use crate::reference_db::discovery::parse_csv_values;

    let table_name = format!("stats__{}", sanitize_id(file_stem));
    if !schema.tables.contains_key(&table_name) {
        return ParsedContent::Stats {
            entries: vec![],
            last_type: Some(file_stem.to_string()),
            raw_content: Some(content.to_string()),
        };
    }

    let mut entries: Vec<ParsedStatsEntry> = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_data: Vec<(String, String)> = Vec::new();

    let flush = |entries: &mut Vec<ParsedStatsEntry>,
                 name: &Option<String>,
                 data: &[(String, String)],
                 table_name: &str,
                 file_stem: &str| {
        if let Some(name) = name {
            entries.push(ParsedStatsEntry {
                table_name: table_name.to_string(),
                entry_name: name.clone(),
                entry_type: file_stem.to_string(),
                entry_using: None,
                data: data.to_vec(),
            });
        }
    };

    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") {
            // Empty line flushes the current entry (same as standard format)
            if current_name.is_some() {
                flush(
                    &mut entries,
                    &current_name,
                    &current_data,
                    &table_name,
                    file_stem,
                );
                current_name = None;
                current_data.clear();
            }
            continue;
        }

        // key "Name","Value" format (XPData)
        if t.starts_with("key ") || t.starts_with("key\t") {
            // Flush any pending entry
            flush(
                &mut entries,
                &current_name,
                &current_data,
                &table_name,
                file_stem,
            );
            current_name = None;
            current_data.clear();

            if let Some(q) = t.find('"') {
                let values = parse_csv_values(&t[q..]);
                if let Some(name) = values.first() {
                    let mut data = Vec::new();
                    if let Some(val) = values.get(1) {
                        data.push(("Value".to_string(), val.clone()));
                    }
                    entries.push(ParsedStatsEntry {
                        table_name: table_name.clone(),
                        entry_name: name.clone(),
                        entry_type: file_stem.to_string(),
                        entry_using: None,
                        data,
                    });
                }
            }
            continue;
        }

        // new <keyword> "name","v1","v2",...
        if t.starts_with("new ") {
            // Flush previous entry
            flush(
                &mut entries,
                &current_name,
                &current_data,
                &table_name,
                file_stem,
            );
            current_data.clear();

            if let Some(q) = t.find('"') {
                let values = parse_csv_values(&t[q..]);
                current_name = values.first().cloned();
                for (i, val) in values.iter().enumerate().skip(1) {
                    current_data.push((format!("Param{i}"), val.clone()));
                }
            } else {
                current_name = None;
            }
            continue;
        }

        // add <keyword> <values> (sub-entries for groups)
        if let Some(rest) = t.strip_prefix("add ") {
            if let Some(kw_end) = rest.find(|c: char| c.is_whitespace() || c == '"') {
                let keyword = &rest[..kw_end];
                let remainder = rest[kw_end..].trim();
                let values = parse_csv_values(remainder);
                let val_str = values.join(";");

                let col = capitalize_first(keyword);

                // Append to existing value if multiple add lines for same keyword
                if let Some(existing) = current_data.iter_mut().find(|(k, _)| *k == col) {
                    existing.1.push('|');
                    existing.1.push_str(&val_str);
                } else {
                    current_data.push((col, val_str));
                }
            }
        }
    }

    // Flush last entry
    flush(
        &mut entries,
        &current_name,
        &current_data,
        &table_name,
        file_stem,
    );

    ParsedContent::Stats {
        entries,
        last_type: Some(file_stem.to_string()),
        raw_content: None,
    }
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let mut out = c.to_uppercase().collect::<String>();
            out.push_str(chars.as_str());
            out
        }
    }
}

/// Parse `new equipment "..."` blocks from Equipment.txt.
fn parse_equipment_content(content: &str, schema: &DiscoveredSchema) -> ParsedContent {
    if !schema.tables.contains_key("stats__equipment") {
        return ParsedContent::Empty;
    }

    let mut entries = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        if let Some(rest) = strip_prefix_ci(t, "new equipment \"") {
            if let Some(end) = rest.find('"') {
                let name = rest[..end].to_string();
                if !name.is_empty() {
                    entries.push(ParsedEquipmentEntry { name });
                }
            }
        }
    }

    if entries.is_empty() {
        ParsedContent::Empty
    } else {
        ParsedContent::Equipment { entries }
    }
}

/// Parse `valuelist "..." / value "..." : N` blocks from ValueLists.txt.
fn parse_valuelist_content(content: &str, schema: &DiscoveredSchema) -> ParsedContent {
    if !schema.tables.contains_key("valuelist_entries") {
        return ParsedContent::Empty;
    }

    let mut entries = Vec::new();
    let mut current_key: Option<String> = None;

    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }

        if let Some(rest) = strip_prefix_ci(t, "valuelist \"") {
            if let Some(end) = rest.find('"') {
                current_key = Some(rest[..end].to_string());
            }
        } else if let Some(rest) = strip_prefix_ci(t, "value \"") {
            if let Some(ref key) = current_key {
                // Format: value "Name" : Index
                if let Some(end) = rest.find('"') {
                    let value_name = rest[..end].to_string();
                    let remainder = &rest[end + 1..];
                    // Parse the index after " : "
                    let value_index = remainder
                        .find(':')
                        .and_then(|pos| remainder[pos + 1..].trim().parse::<i64>().ok())
                        .unwrap_or(0);
                    entries.push(ParsedValueListRow {
                        list_key: key.clone(),
                        value_name,
                        value_index,
                    });
                }
            }
        }
    }

    if entries.is_empty() {
        ParsedContent::Empty
    } else {
        ParsedContent::ValueLists { entries }
    }
}

/// Parse `modifier type "..." / modifier "field","type"` blocks from Modifiers.txt.
fn parse_modifier_content(content: &str, schema: &DiscoveredSchema) -> ParsedContent {
    if !schema.tables.contains_key("modifier_definitions") {
        return ParsedContent::Empty;
    }

    let mut entries = Vec::new();
    let mut current_type: Option<String> = None;

    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }

        if let Some(rest) = strip_prefix_ci(t, "modifier type \"") {
            if let Some(end) = rest.find('"') {
                current_type = Some(rest[..end].to_string());
            }
        } else if let Some(rest) = strip_prefix_ci(t, "modifier \"") {
            if let Some(ref type_name) = current_type {
                // Format: modifier "FieldName","FieldType"
                if let Some(end) = rest.find('"') {
                    let field_name = rest[..end].to_string();
                    let remainder = &rest[end + 1..];
                    // After closing quote, expect ,"FieldType"
                    if let Some(comma_pos) = remainder.find(",\"") {
                        let type_part = &remainder[comma_pos + 2..];
                        if let Some(type_end) = type_part.find('"') {
                            let field_type = type_part[..type_end].to_string();
                            entries.push(ParsedModifierRow {
                                type_name: type_name.clone(),
                                field_name,
                                field_type,
                            });
                        }
                    }
                }
            }
        }
    }

    if entries.is_empty() {
        ParsedContent::Empty
    } else {
        ParsedContent::Modifiers { entries }
    }
}

fn parse_loca_file(file: &FileEntry) -> ParsedContent {
    let entries = match parse_loca_entries(file) {
        Ok(entries) => entries,
        Err(_) => return ParsedContent::Empty,
    };

    if entries.is_empty() {
        ParsedContent::Empty
    } else {
        ParsedContent::Loca { entries }
    }
}

fn read_text_content(file: &FileEntry) -> Result<String, String> {
    if let Some(bytes) = file.in_memory_bytes() {
        String::from_utf8(bytes.to_vec()).map_err(|e| format!("UTF-8 decode error: {e}"))
    } else {
        std::fs::read_to_string(&file.abs_path).map_err(|e| format!("Read error: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Planned insertion (uses pre-computed plans, no dynamic SQL)
// ---------------------------------------------------------------------------

/// Insert all content from a parsed file.  Returns (rows_inserted, errors).
#[allow(clippy::too_many_arguments)]
fn insert_parsed_file(
    tx: &Transaction,
    parsed: &mut ParsedFile,
    file: &FileEntry,
    file_id: i64,
    plans: &mut HashMap<String, TableInsertPlan>,
    junction_plans: &HashMap<String, JunctionPlan>,
    schema: &DiscoveredSchema,
    source_ids: &mut SourceIdCache,
    row_counts: &mut HashMap<String, i64>,
) -> Result<(usize, usize), String> {
    // Resolve module name → integer ID (cached, ~6 unique values)
    let source_id = source_ids.resolve(tx, &parsed.mod_name)?;

    // Insert _source_files row with explicit file_id (region_id + row_count filled after insert)
    tx.prepare_cached(
        "INSERT INTO _source_files (file_id, path, file_type, mod_name, region_id, row_count, file_size)
         VALUES (?1, ?2, ?3, ?4, NULL, 0, ?5)",
    )
    .and_then(|mut stmt| stmt.execute(params![
        file_id, file.rel_path, file.file_type.as_str(), parsed.mod_name, parsed.file_size
    ]))
    .map_err(|e| format!("Insert source file: {e}"))?;

    let (rows, errors, region_id) = match &mut parsed.content {
        ParsedContent::Lsx {
            last_region_id,
            root_nodes,
        } => {
            let (rows, errors) = insert_parsed_lsx(
                tx,
                &file.rel_path,
                file_id,
                source_id,
                root_nodes,
                plans,
                junction_plans,
                schema,
                row_counts,
            );
            (rows, errors, last_region_id.clone())
        }
        ParsedContent::Stats {
            entries,
            last_type,
            raw_content,
        } => {
            let rows = insert_parsed_stats(
                tx,
                file_id,
                source_id,
                entries,
                raw_content.as_deref(),
                plans,
                schema,
                row_counts,
            )?;
            (rows, 0, last_type.clone())
        }
        ParsedContent::Loca { entries } => {
            let rows = insert_parsed_loca(tx, file_id, source_id, entries, row_counts)?;
            (rows, 0, None)
        }
        ParsedContent::Equipment { entries } => {
            let rows = insert_parsed_equipment(tx, file_id, source_id, entries, plans, row_counts)?;
            (rows, 0, None)
        }
        ParsedContent::ValueLists { entries } => {
            let rows = insert_parsed_valuelists(tx, file_id, source_id, entries, row_counts)?;
            (rows, 0, None)
        }
        ParsedContent::Modifiers { entries } => {
            let rows = insert_parsed_modifiers(tx, file_id, source_id, entries, row_counts)?;
            (rows, 0, None)
        }
        ParsedContent::Empty => (0, 0, None),
    };

    // Update _source_files with region_id and row_count
    tx.prepare_cached("UPDATE _source_files SET region_id = ?1, row_count = ?2 WHERE file_id = ?3")
        .and_then(|mut stmt| stmt.execute(params![region_id, rows as i64, file_id]))
        .map_err(|e| format!("Update source file: {e}"))?;

    Ok((rows, errors))
}

#[allow(clippy::too_many_arguments)]
fn insert_parsed_lsx(
    tx: &Transaction,
    file_path: &str,
    file_id: i64,
    source_id: i64,
    root_nodes: &mut [ParsedNode],
    plans: &mut HashMap<String, TableInsertPlan>,
    junction_plans: &HashMap<String, JunctionPlan>,
    schema: &DiscoveredSchema,
    row_counts: &mut HashMap<String, i64>,
) -> (usize, usize) {
    let mut total_rows = 0;
    let mut total_errors = 0;

    for node in root_nodes.iter_mut() {
        let (_, rows, errors) = insert_node_tree(
            tx,
            file_path,
            node,
            file_id,
            source_id,
            plans,
            junction_plans,
            schema,
            row_counts,
        );
        total_rows += rows;
        total_errors += errors;
    }

    (total_rows, total_errors)
}

/// Recursively insert a node tree.  Top-down: the parent is inserted first;
/// if the parent PK already exists (INSERT OR IGNORE returned 0 rows), the
/// entire subtree is skipped.  This ensures that higher-priority modules
/// (processed first) claim ownership of PK entities and their children.
///
/// Returns: (registrations_for_grandparent, rows_inserted, errors)
#[allow(clippy::too_many_arguments)]
fn insert_node_tree(
    tx: &Transaction,
    file_path: &str,
    node: &mut ParsedNode,
    file_id: i64,
    source_id: i64,
    plans: &mut HashMap<String, TableInsertPlan>,
    junction_plans: &HashMap<String, JunctionPlan>,
    schema: &DiscoveredSchema,
    row_counts: &mut HashMap<String, i64>,
) -> (Vec<(String, String)>, usize, usize) {
    let mut total_rows = 0;
    let mut total_errors = 0;

    if !node.has_data {
        // Container node: no row to insert, just recurse into children
        let mut child_pks: Vec<(String, String)> = Vec::new();
        for child in &mut node.children {
            let (regs, rows, errors) = insert_node_tree(
                tx,
                file_path,
                child,
                file_id,
                source_id,
                plans,
                junction_plans,
                schema,
                row_counts,
            );
            child_pks.extend(regs);
            total_rows += rows;
            total_errors += errors;
        }
        return (child_pks, total_rows, total_errors);
    }

    // Try to insert this node (parent) first
    match insert_row_planned(
        tx,
        &node.table_name,
        file_id,
        source_id,
        &mut node.attrs,
        plans,
    ) {
        Ok((Some(pk_val), was_inserted)) => {
            total_rows += 1;
            if let Some(c) = row_counts.get_mut(&node.table_name) {
                *c += 1;
            }

            if !was_inserted {
                // PK already exists from a higher-priority module — skip
                // the entire subtree to avoid orphaned children.
                return (
                    vec![(node.table_name.clone(), pk_val)],
                    total_rows,
                    total_errors,
                );
            }

            // Parent was freshly inserted — now insert children
            let mut child_pks: Vec<(String, String)> = Vec::new();
            for child in &mut node.children {
                let (regs, rows, errors) = insert_node_tree(
                    tx,
                    file_path,
                    child,
                    file_id,
                    source_id,
                    plans,
                    junction_plans,
                    schema,
                    row_counts,
                );
                child_pks.extend(regs);
                total_rows += rows;
                total_errors += errors;
            }

            // Insert junction rows linking parent → children (pre-cached SQL)
            for (child_table, child_pk) in &child_pks {
                if let Some(jn) = find_junction_table(schema, &node.table_name, child_table) {
                    if let Some(jp) = junction_plans.get(jn) {
                        let _ = tx
                            .prepare_cached(&jp.sql)
                            .and_then(|mut stmt| stmt.execute(params![pk_val, child_pk]));
                    }
                }
            }

            (
                vec![(node.table_name.clone(), pk_val)],
                total_rows,
                total_errors,
            )
        }
        Ok((None, _)) => {
            // Rowid table — always insert children regardless
            total_rows += 1;
            if let Some(c) = row_counts.get_mut(&node.table_name) {
                *c += 1;
            }
            let mut child_pks: Vec<(String, String)> = Vec::new();
            for child in &mut node.children {
                let (regs, rows, errors) = insert_node_tree(
                    tx,
                    file_path,
                    child,
                    file_id,
                    source_id,
                    plans,
                    junction_plans,
                    schema,
                    row_counts,
                );
                child_pks.extend(regs);
                total_rows += rows;
                total_errors += errors;
            }
            (vec![], total_rows, total_errors)
        }
        Err(err) => {
            maybe_log_row_error(file_path, &node.table_name, &err, &node.attrs);
            total_errors += 1;
            (vec![], total_rows, total_errors)
        }
    }
}

/// Insert a single row using a pre-computed plan with reusable row buffer.
/// Returns (pk_value, was_inserted) — `was_inserted` is false when INSERT OR
/// IGNORE skipped the row because the PK already exists.
fn insert_row_planned(
    tx: &Transaction,
    table_name: &str,
    file_id: i64,
    source_id: i64,
    attrs: &mut [(String, String, String)],
    plans: &mut HashMap<String, TableInsertPlan>,
) -> Result<(Option<String>, bool), String> {
    let plan = match plans.get_mut(table_name) {
        Some(p) => p,
        None => return Err(format!("No plan for table: {table_name}")),
    };

    // Reset reusable row buffer to all NULLs
    for v in plan.row_buf.iter_mut() {
        *v = SqlValue::Null;
    }
    let mut pk_value: Option<String> = None;

    // _file_id
    plan.row_buf[plan.file_id_idx] = SqlValue::Integer(file_id);

    // _SourceID (non-Rowid tables only) — integer FK to _sources(id)
    if let Some(si) = plan.source_id_idx {
        plan.row_buf[si] = SqlValue::Integer(source_id);
    }

    // PK column (non-Rowid tables)
    if let Some(pk_idx) = plan.pk_idx {
        if let Some((_, _, val)) = attrs
            .iter_mut()
            .find(|(name, _, _)| *name == plan.pk_col_lower)
        {
            let owned_val = std::mem::take(val);
            pk_value = Some(owned_val.clone());
            plan.row_buf[pk_idx] = SqlValue::Text(owned_val);
        } else {
            // Synthetic PK for nodes missing the PK attribute
            use std::sync::atomic::{AtomicU64, Ordering};
            static SYNTH_COUNTER: AtomicU64 = AtomicU64::new(1);
            let synth = format!("_row_id_{}", SYNTH_COUNTER.fetch_add(1, Ordering::Relaxed));
            plan.row_buf[pk_idx] = SqlValue::Text(synth.clone());
            pk_value = Some(synth);
        }
    }

    // Fill data columns via O(1) lookup — col_names are pre-lowercased
    // during the parallel parse phase, so no per-row lowercase needed here.
    for (col_name, bg3_type, value) in attrs.iter_mut() {
        // Skip PK (already set above)
        if plan.pk_idx.is_some() && *col_name == plan.pk_col_lower {
            continue;
        }
        if let Some(&idx) = plan.col_map.get(col_name.as_str()) {
            if plan.fk_cols.contains(col_name.as_str()) || bg3_type == "TranslatedString" {
                if coerce_fk_null(value).is_some() {
                    let owned_val = std::mem::take(value);
                    plan.row_buf[idx] = types::coerce_value_owned(owned_val, bg3_type);
                }
                // else: leave as NULL
            } else {
                let owned_val = std::mem::take(value);
                plan.row_buf[idx] = types::coerce_value_owned(owned_val, bg3_type);
            }
        }
    }

    // Execute via cached prepared statement — direct binding avoids
    // per-row Vec<&dyn ToSql> allocation.
    let mut stmt = tx
        .prepare_cached(&plan.sql)
        .map_err(|e| format!("Prepare '{table_name}': {e}"))?;
    for (i, v) in plan.row_buf.iter().enumerate() {
        v.bind_to(&mut stmt, i + 1)
            .map_err(|e| format!("Bind col {i} in '{table_name}': {e}"))?;
    }
    let rows_affected = stmt
        .raw_execute()
        .map_err(|e| format!("Insert into '{table_name}': {e}"))?;

    let was_inserted = rows_affected > 0;

    if plan.pk_strategy == PkStrategy::Rowid && was_inserted {
        pk_value = Some(tx.last_insert_rowid().to_string());
    }

    Ok((pk_value, was_inserted))
}

#[allow(clippy::too_many_arguments)]
fn insert_parsed_stats(
    tx: &Transaction,
    file_id: i64,
    source_id: i64,
    entries: &[ParsedStatsEntry],
    raw_content: Option<&str>,
    plans: &mut HashMap<String, TableInsertPlan>,
    schema: &DiscoveredSchema,
    row_counts: &mut HashMap<String, i64>,
) -> Result<usize, String> {
    let mut total_rows = 0;

    for entry in entries {
        let plan = match plans.get_mut(&entry.table_name) {
            Some(p) => p,
            None => continue,
        };

        // Reset reusable row buffer
        for v in plan.row_buf.iter_mut() {
            *v = SqlValue::Null;
        }

        // PK: _entry_name
        if let Some(pk_idx) = plan.pk_idx {
            plan.row_buf[pk_idx] = SqlValue::Text(entry.entry_name.clone());
        }

        // _file_id
        plan.row_buf[plan.file_id_idx] = SqlValue::Integer(file_id);

        // _SourceID — integer FK
        if let Some(si) = plan.source_id_idx {
            plan.row_buf[si] = SqlValue::Integer(source_id);
        }

        // _type
        if let Some(&idx) = plan.col_map.get("_type") {
            plan.row_buf[idx] = SqlValue::Text(entry.entry_type.clone());
        }

        // _using (with FK coercion)
        if let Some(&idx) = plan.col_map.get("_using") {
            if let Some(u) = entry.entry_using.as_deref().and_then(coerce_fk_null) {
                plan.row_buf[idx] = SqlValue::Text(u.to_string());
            }
        }

        // FK cols for this table
        let stats_fk_cols: HashSet<&str> = schema
            .tables
            .get(&entry.table_name)
            .map(|ts| {
                ts.fk_constraints
                    .iter()
                    .map(|fk| fk.source_column.as_str())
                    .collect()
            })
            .unwrap_or_default();

        // Data fields
        for (k, v) in &entry.data {
            let key_lower = k.to_ascii_lowercase();
            if is_stats_loca_column(k) {
                if let Some((cuid, ver)) = v.split_once(';') {
                    if let Some(&idx) = plan.col_map.get(&key_lower) {
                        if let Some(c) = coerce_fk_null(cuid) {
                            plan.row_buf[idx] = SqlValue::Text(c.to_string());
                        }
                    }
                    let ver_key = format!("{key_lower}_version");
                    if let Some(&idx) = plan.col_map.get(&ver_key) {
                        plan.row_buf[idx] = match ver.parse::<i64>() {
                            Ok(n) => SqlValue::Integer(n),
                            Err(_) => SqlValue::Text(ver.to_string()),
                        };
                    }
                } else if let Some(&idx) = plan.col_map.get(&key_lower) {
                    if let Some(s) = coerce_fk_null(v) {
                        plan.row_buf[idx] = SqlValue::Text(s.to_string());
                    }
                }
            } else if stats_fk_cols.contains(k.as_str()) {
                if let Some(&idx) = plan.col_map.get(&key_lower) {
                    if let Some(s) = coerce_fk_null(v) {
                        plan.row_buf[idx] = SqlValue::Text(s.to_string());
                    }
                }
            } else if let Some(&idx) = plan.col_map.get(&key_lower) {
                plan.row_buf[idx] = SqlValue::Text(v.clone());
            }
        }

        // Execute via cached prepared statement — direct binding avoids
        // per-row Vec<&dyn ToSql> allocation.
        let mut stmt = tx
            .prepare_cached(&plan.sql)
            .map_err(|e| format!("Prepare stats '{}': {}", entry.table_name, e))?;
        for (i, v) in plan.row_buf.iter().enumerate() {
            v.bind_to(&mut stmt, i + 1)
                .map_err(|e| format!("Bind stats col {} in '{}': {}", i, entry.table_name, e))?;
        }
        stmt.raw_execute()
            .map_err(|e| format!("Insert stats '{}': {}", entry.table_name, e))?;
        total_rows += 1;
        if let Some(c) = row_counts.get_mut(&entry.table_name) {
            *c += 1;
        }
    }

    // Raw text fallback
    if let Some(content) = raw_content {
        if schema.tables.contains_key("txt__raw") {
            tx.prepare_cached("INSERT INTO \"txt__raw\" (_file_id, content) VALUES (?1, ?2)")
                .and_then(|mut stmt| stmt.execute(params![file_id, content]))
                .map_err(|e| format!("Insert raw txt: {e}"))?;
            total_rows += 1;
            if let Some(c) = row_counts.get_mut("txt__raw") {
                *c += 1;
            }
        }
    }

    Ok(total_rows)
}

fn insert_parsed_loca(
    tx: &Transaction,
    file_id: i64,
    source_id: i64,
    entries: &[ParsedLocaEntry],
    row_counts: &mut HashMap<String, i64>,
) -> Result<usize, String> {
    if entries.is_empty() {
        return Ok(0);
    }

    // Batch loca rows into multi-row INSERT statements.
    // 5 params per row × 100 rows = 500 params — well under SQLite's 32766 limit.
    const COLS_PER_ROW: usize = 5;
    const BATCH_SIZE: usize = 100;

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|i| {
                let b = i * COLS_PER_ROW + 1;
                format!("(?{}, ?{}, ?{}, ?{}, ?{})", b, b + 1, b + 2, b + 3, b + 4)
            })
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT OR IGNORE INTO \"loca__english\" (contentuid, _file_id, \"_SourceID\", version, text) VALUES {placeholders}"
        );

        let mut stmt = tx
            .prepare_cached(&sql)
            .map_err(|e| format!("Prepare loca batch: {e}"))?;

        for (i, entry) in chunk.iter().enumerate() {
            let b = i * COLS_PER_ROW + 1;
            stmt.raw_bind_parameter(b, &entry.contentuid)
                .map_err(|e| format!("Bind loca: {e}"))?;
            stmt.raw_bind_parameter(b + 1, file_id)
                .map_err(|e| format!("Bind loca: {e}"))?;
            stmt.raw_bind_parameter(b + 2, source_id)
                .map_err(|e| format!("Bind loca: {e}"))?;
            stmt.raw_bind_parameter(b + 3, entry.version)
                .map_err(|e| format!("Bind loca: {e}"))?;
            stmt.raw_bind_parameter(b + 4, &entry.text as &dyn rusqlite::types::ToSql)
                .map_err(|e| format!("Bind loca: {e}"))?;
        }

        stmt.raw_execute()
            .map_err(|e| format!("Execute loca batch: {e}"))?;
    }

    let total_rows = entries.len();
    if let Some(c) = row_counts.get_mut("loca__english") {
        *c += total_rows as i64;
    }
    Ok(total_rows)
}

fn insert_parsed_equipment(
    tx: &Transaction,
    file_id: i64,
    source_id: i64,
    entries: &[ParsedEquipmentEntry],
    plans: &mut HashMap<String, TableInsertPlan>,
    row_counts: &mut HashMap<String, i64>,
) -> Result<usize, String> {
    if entries.is_empty() {
        return Ok(0);
    }

    let plan = match plans.get_mut("stats__equipment") {
        Some(p) => p,
        None => return Ok(0),
    };

    let mut total_rows = 0;
    for entry in entries {
        // Reset row buffer
        for v in plan.row_buf.iter_mut() {
            *v = SqlValue::Null;
        }

        // PK: _entry_name
        if let Some(pk_idx) = plan.pk_idx {
            plan.row_buf[pk_idx] = SqlValue::Text(entry.name.clone());
        }
        // _file_id
        plan.row_buf[plan.file_id_idx] = SqlValue::Integer(file_id);
        // _SourceID
        if let Some(si) = plan.source_id_idx {
            plan.row_buf[si] = SqlValue::Integer(source_id);
        }

        let mut stmt = tx
            .prepare_cached(&plan.sql)
            .map_err(|e| format!("Prepare equipment: {e}"))?;
        for (i, v) in plan.row_buf.iter().enumerate() {
            v.bind_to(&mut stmt, i + 1)
                .map_err(|e| format!("Bind equipment col {i}: {e}"))?;
        }
        stmt.raw_execute()
            .map_err(|e| format!("Insert equipment: {e}"))?;
        total_rows += 1;
    }
    if let Some(c) = row_counts.get_mut("stats__equipment") {
        *c += total_rows as i64;
    }
    Ok(total_rows)
}

fn insert_parsed_valuelists(
    tx: &Transaction,
    file_id: i64,
    source_id: i64,
    entries: &[ParsedValueListRow],
    row_counts: &mut HashMap<String, i64>,
) -> Result<usize, String> {
    if entries.is_empty() {
        return Ok(0);
    }

    // Batch insert: 5 params per row (list_key, value_name, value_index, _file_id, _SourceID)
    const COLS_PER_ROW: usize = 5;
    const BATCH_SIZE: usize = 100;

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|i| {
                let b = i * COLS_PER_ROW + 1;
                format!("(?{}, ?{}, ?{}, ?{}, ?{})", b, b + 1, b + 2, b + 3, b + 4)
            })
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT OR IGNORE INTO \"valuelist_entries\" (list_key, value_name, value_index, _file_id, \"_SourceID\") VALUES {placeholders}"
        );

        let mut stmt = tx
            .prepare_cached(&sql)
            .map_err(|e| format!("Prepare valuelist batch: {e}"))?;

        for (i, entry) in chunk.iter().enumerate() {
            let b = i * COLS_PER_ROW + 1;
            stmt.raw_bind_parameter(b, &entry.list_key)
                .map_err(|e| format!("Bind valuelist: {e}"))?;
            stmt.raw_bind_parameter(b + 1, &entry.value_name)
                .map_err(|e| format!("Bind valuelist: {e}"))?;
            stmt.raw_bind_parameter(b + 2, entry.value_index)
                .map_err(|e| format!("Bind valuelist: {e}"))?;
            stmt.raw_bind_parameter(b + 3, file_id)
                .map_err(|e| format!("Bind valuelist: {e}"))?;
            stmt.raw_bind_parameter(b + 4, source_id)
                .map_err(|e| format!("Bind valuelist: {e}"))?;
        }

        stmt.raw_execute()
            .map_err(|e| format!("Execute valuelist batch: {e}"))?;
    }

    let total_rows = entries.len();
    if let Some(c) = row_counts.get_mut("valuelist_entries") {
        *c += total_rows as i64;
    }
    Ok(total_rows)
}

fn insert_parsed_modifiers(
    tx: &Transaction,
    file_id: i64,
    source_id: i64,
    entries: &[ParsedModifierRow],
    row_counts: &mut HashMap<String, i64>,
) -> Result<usize, String> {
    if entries.is_empty() {
        return Ok(0);
    }

    // Batch insert: 5 params per row (type_name, field_name, field_type, _file_id, _SourceID)
    const COLS_PER_ROW: usize = 5;
    const BATCH_SIZE: usize = 100;

    for chunk in entries.chunks(BATCH_SIZE) {
        let placeholders: String = (0..chunk.len())
            .map(|i| {
                let b = i * COLS_PER_ROW + 1;
                format!("(?{}, ?{}, ?{}, ?{}, ?{})", b, b + 1, b + 2, b + 3, b + 4)
            })
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT OR IGNORE INTO \"modifier_definitions\" (type_name, field_name, field_type, _file_id, \"_SourceID\") VALUES {placeholders}"
        );

        let mut stmt = tx
            .prepare_cached(&sql)
            .map_err(|e| format!("Prepare modifier batch: {e}"))?;

        for (i, entry) in chunk.iter().enumerate() {
            let b = i * COLS_PER_ROW + 1;
            stmt.raw_bind_parameter(b, &entry.type_name)
                .map_err(|e| format!("Bind modifier: {e}"))?;
            stmt.raw_bind_parameter(b + 1, &entry.field_name)
                .map_err(|e| format!("Bind modifier: {e}"))?;
            stmt.raw_bind_parameter(b + 2, &entry.field_type)
                .map_err(|e| format!("Bind modifier: {e}"))?;
            stmt.raw_bind_parameter(b + 3, file_id)
                .map_err(|e| format!("Bind modifier: {e}"))?;
            stmt.raw_bind_parameter(b + 4, source_id)
                .map_err(|e| format!("Bind modifier: {e}"))?;
        }

        stmt.raw_execute()
            .map_err(|e| format!("Execute modifier batch: {e}"))?;
    }

    let total_rows = entries.len();
    if let Some(c) = row_counts.get_mut("modifier_definitions") {
        *c += total_rows as i64;
    }
    Ok(total_rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reference_db::TargetDb;
    use tempfile::tempdir;

    fn test_schema() -> DiscoveredSchema {
        let mut tables = HashMap::new();
        tables.insert(
            "loca__english".to_string(),
            TableSchema {
                table_name: "loca__english".to_string(),
                pk_strategy: PkStrategy::ContentUid,
                source_type: "loca".to_string(),
                region_id: None,
                node_id: None,
                columns: vec![
                    ColumnDef {
                        name: "version".to_string(),
                        bg3_type: "int32".to_string(),
                        sqlite_type: "INTEGER".to_string(),
                    },
                    ColumnDef {
                        name: "text".to_string(),
                        bg3_type: "LSString".to_string(),
                        sqlite_type: "TEXT".to_string(),
                    },
                ],
                fk_constraints: Vec::new(),
                has_file_id: true,
                parent_tables: HashSet::new(),
            },
        );
        tables.insert(
            "stats__PassiveData".to_string(),
            TableSchema {
                table_name: "stats__PassiveData".to_string(),
                pk_strategy: PkStrategy::EntryName,
                source_type: "stats".to_string(),
                region_id: None,
                node_id: None,
                columns: vec![ColumnDef {
                    name: "DisplayName".to_string(),
                    bg3_type: "TranslatedString".to_string(),
                    sqlite_type: "TEXT".to_string(),
                }],
                fk_constraints: vec![FkConstraint {
                    source_column: "DisplayName".to_string(),
                    target_table: "loca__english".to_string(),
                    target_column: "contentuid".to_string(),
                }],
                has_file_id: false,
                parent_tables: HashSet::new(),
            },
        );

        DiscoveredSchema {
            tables,
            renames: HashMap::new(),
            region_node_ids: HashMap::new(),
            junction_tables: Vec::new(),
            junction_lookup: HashMap::new(),
        }
    }

    fn create_test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE _source_files (
                file_id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                file_type TEXT NOT NULL,
                mod_name TEXT,
                region_id TEXT,
                row_count INTEGER DEFAULT 0,
                file_size INTEGER
            );
            CREATE TABLE loca__english (
                contentuid TEXT PRIMARY KEY,
                _file_id INTEGER NOT NULL,
                version INTEGER,
                text TEXT
            );
            CREATE TABLE \"stats__PassiveData\" (
                _entry_name TEXT PRIMARY KEY,
                DisplayName TEXT
            );",
        )
        .unwrap();
        conn
    }

    fn create_base_db(path: &Path, handles: &[&str]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE _source_files (
                file_id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                file_type TEXT NOT NULL,
                mod_name TEXT,
                region_id TEXT,
                row_count INTEGER DEFAULT 0,
                file_size INTEGER
            );
            CREATE TABLE loca__english (
                contentuid TEXT PRIMARY KEY,
                _file_id INTEGER NOT NULL,
                version INTEGER,
                text TEXT
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO _source_files (file_id, path, file_type, row_count) VALUES (1, 'base:loca', 'loca', 0)",
            [],
        )
        .unwrap();
        for handle in handles {
            conn.execute(
                "INSERT INTO loca__english (contentuid, _file_id, version, text) VALUES (?1, 1, 0, 'Base Text')",
                params![handle],
            )
            .unwrap();
        }
    }

    fn build_test_loca_bytes(key: &str, version: u16, text: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        let text_len = text.len() + 1;
        let header_size = 12u32;
        let entry_size = 70u32;
        bytes.extend_from_slice(&0x4143_4f4cu32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&(header_size + entry_size).to_le_bytes());

        let mut key_bytes = [0u8; 64];
        key_bytes[..key.len()].copy_from_slice(key.as_bytes());
        bytes.extend_from_slice(&key_bytes);
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes.extend_from_slice(&(text_len as u32).to_le_bytes());
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
        bytes
    }

    #[test]
    fn backfill_orphaned_loca_skips_handles_present_in_base_fallback() {
        let schema = test_schema();
        let conn = create_test_conn();
        conn.execute(
            "INSERT INTO \"stats__PassiveData\" (_entry_name, DisplayName) VALUES ('Passive_Test', 'hbase')",
            [],
        )
        .unwrap();

        let tmp = tempdir().unwrap();
        let base_db = tmp.path().join("ref_base.sqlite");
        create_base_db(&base_db, &["hbase"]);

        backfill_orphaned_loca(&conn, &schema, Some(base_db.as_path())).unwrap();

        let placeholder_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM loca__english WHERE contentuid = 'hbase'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(placeholder_count, 0);
    }

    #[test]
    fn backfill_orphaned_loca_inserts_placeholder_when_missing_everywhere() {
        let schema = test_schema();
        let conn = create_test_conn();
        conn.execute(
            "INSERT INTO \"stats__PassiveData\" (_entry_name, DisplayName) VALUES ('Passive_Test', 'hmissing')",
            [],
        )
        .unwrap();

        backfill_orphaned_loca(&conn, &schema, None).unwrap();

        let inserted: (String, String) = conn
            .query_row(
                "SELECT contentuid, text FROM loca__english WHERE contentuid = 'hmissing'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(inserted.0, "hmissing");
        assert_eq!(
            inserted.1,
            "Invalid or Unknown Localization Handle Provided"
        );
    }

    #[test]
    fn parse_stats_file_reads_in_memory_content() {
        let schema = test_schema();
        let file = FileEntry::from_bytes(
            "Shared/Public/Shared/Stats/Generated/Data/Passive.txt".to_string(),
            FileType::Stats,
            "Shared".to_string(),
            TargetDb::Base,
            10,
            b"new entry \"Passive_Test\"\n type \"PassiveData\"\n data \"DisplayName\" \"hpassive;1\"\n".to_vec(),
        );

        match parse_stats_file(&file, &schema) {
            ParsedContent::Stats { entries, .. } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].entry_name, "Passive_Test");
                assert_eq!(entries[0].table_name, "stats__PassiveData");
            }
            _ => panic!("expected parsed stats content"),
        }
    }

    #[test]
    fn parse_loca_file_reads_in_memory_binary_content() {
        let file = FileEntry::from_bytes(
            "Localization/English/Test.loca".to_string(),
            FileType::Loca,
            "English".to_string(),
            TargetDb::Base,
            5,
            build_test_loca_bytes("hstream", 7, "Hello"),
        );

        match parse_loca_file(&file) {
            ParsedContent::Loca { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].contentuid, "hstream");
                assert_eq!(entries[0].version, 7);
                assert_eq!(entries[0].text.as_deref(), Some("Hello"));
            }
            _ => panic!("expected parsed loca content"),
        }
    }
}
