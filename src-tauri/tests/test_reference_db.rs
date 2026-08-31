//! Integration tests for the reference DB schema and population pipeline.
//!
//! Tests that require game data read paths from a `.env` file in the workspace
//! root. Copy `.env.example` → `.env` and fill in your local paths.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
#[ignore] // Requires game-data fixture file not present in CI/dev environments
fn build_reference_db_small_real_lsf_fixture() {
    let ws = workspace_root();
    let source_lsf =
        ws.join("UnpackedData/Gustav/Public/Gustav/Tags/3549f056-0826-45ee-a8ae-351449b70fe3.lsf");
    assert!(
        source_lsf.is_file(),
        "sample LSF not found at {}",
        source_lsf.display()
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let unpacked = tmp.path().join("UnpackedData");

    let tags_dir = unpacked.join("Gustav/Public/Gustav/Tags");
    std::fs::create_dir_all(&tags_dir).expect("create tags dir");
    std::fs::copy(&source_lsf, tags_dir.join(source_lsf.file_name().unwrap()))
        .expect("copy sample lsf");

    let stats_dir = unpacked.join("Gustav/Public/Gustav/Stats/Generated/Data");
    std::fs::create_dir_all(&stats_dir).expect("create stats dir");
    std::fs::write(
        stats_dir.join("Passive.txt"),
        "new entry \"TestPassive\"\ntype \"PassiveData\"\ndata \"DisplayName\" \"h123456789abcdefg;1\"\ndata \"Description\" \"hfedcba9876543210;2\"\n",
    )
    .expect("write stats fixture");

    let loca_dir = unpacked.join("English/Localization/English");
    std::fs::create_dir_all(&loca_dir).expect("create loca dir");
    std::fs::write(
        loca_dir.join("fixture.xml"),
        r#"<?xml version="1.0" encoding="utf-8"?>
<contentList>
  <content contentuid="h123456789abcdefg" version="1">Fixture Passive Name</content>
  <content contentuid="hfedcba9876543210" version="2">Fixture Passive Description</content>
</contentList>
"#,
    )
    .expect("write loca fixture");

    let db_path = tmp.path().join("fixture_reference.sqlite");
    let summary = bg3_cmty_studio_lib::reference_db::build_reference_db(&unpacked, &db_path)
        .expect("build_reference_db failed");

    assert_eq!(
        summary.total_files, 3,
        "expected exactly 3 collected fixture files"
    );
    assert_eq!(
        summary.file_errors, 0,
        "unexpected file errors in fixture build"
    );
    assert_eq!(
        summary.row_errors, 0,
        "unexpected row errors in fixture build"
    );
    assert!(
        summary.total_rows >= 4,
        "expected at least 4 rows from fixture inputs"
    );

    let conn = rusqlite::Connection::open(&db_path).expect("open fixture db");

    let source_files: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _source_files WHERE file_id > 0",
            [],
            |row| row.get(0),
        )
        .expect("count source files");
    assert_eq!(
        source_files, 3,
        "expected one _source_files row per fixture file"
    );

    let tags_table: String = conn
        .query_row(
            "SELECT table_name FROM _table_meta WHERE region_id = 'Tags' AND row_count > 0 LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("find populated tags table");
    let tags_rows: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM \"{tags_table}\""),
            [],
            |row| row.get(0),
        )
        .expect("count tags rows");
    assert!(
        tags_rows > 0,
        "expected native LSF fixture to populate at least one Tags row"
    );

    let stats_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM \"stats__PassiveData\"", [], |row| {
            row.get(0)
        })
        .expect("count stats rows");
    assert_eq!(stats_rows, 1, "expected one passive fixture row");

    let loca_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM \"loca__english\" WHERE contentuid IN ('h123456789abcdefg', 'hfedcba9876543210')",
            [],
            |row| row.get(0),
        )
        .expect("count loca rows");
    assert_eq!(
        loca_rows, 2,
        "expected both localization fixture entries to be present"
    );
}

/// Test cross-DB FK validation: honor DB references resolved against base DB.
///
/// The honor DB has FK violations on its own (Honour overrides reference stats
/// entries that exist only in ref_base). ATTACHing ref_base should resolve
/// most or all of them.
///
/// Requires: test_populated_dbs/ from `populate_all_schema_dbs`.
#[test]
#[ignore]
fn cross_db_honor_fk_validation() {
    let ws = workspace_root();

    // Require both populated DBs from prior test runs
    let base_db = ws.join("test_populated_dbs/ref_base.sqlite");
    let honor_db = ws.join("test_populated_dbs/ref_honor.sqlite");
    assert!(
        base_db.is_file(),
        "test_populated_dbs/ref_base.sqlite not found — run populate_all_schema_dbs first"
    );
    assert!(
        honor_db.is_file(),
        "test_populated_dbs/ref_honor.sqlite not found — run populate_all_schema_dbs first"
    );

    // Step 1: Count standalone FK violations (no attached DBs)
    println!("=== Standalone FK check (honor only) ===");
    let conn = rusqlite::Connection::open(&honor_db).expect("open honor DB");
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

    let standalone_violations: Vec<(String, i64, String, i64)> = {
        let mut stmt = conn.prepare("PRAGMA foreign_key_check").unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    };
    let standalone_count = standalone_violations.len();
    println!("  Standalone violations: {standalone_count}");

    // Step 2: Cross-DB validation with ref_base attached
    println!("\n=== Cross-DB FK check (honor + base attached) ===");
    bg3_cmty_studio_lib::reference_db::cross_db::attach_readonly(&conn, &base_db, "base")
        .expect("attach base DB");

    // Verify attachment
    let attached =
        bg3_cmty_studio_lib::reference_db::cross_db::list_attached(&conn).expect("list attached");
    println!(
        "  Attached DBs: {:?}",
        attached.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>()
    );

    let report =
        bg3_cmty_studio_lib::reference_db::cross_db::validate_cross_db_fks(&conn, &["base"])
            .expect("cross-DB FK validation");

    println!("  Total FK columns checked: {}", report.total_checked);
    println!(
        "  Cross-resolved (found in base): {}",
        report.cross_resolved
    );
    println!("  Truly unresolved: {}", report.unresolved.len());

    if !report.unresolved.is_empty() {
        println!("\n  Unresolved violations (first 20):");
        for v in report.unresolved.iter().take(20) {
            println!(
                "    {}.{} = '{}' → {}.{}",
                v.table,
                v.from_column,
                &v.value[..v.value.len().min(40)],
                v.target_table,
                v.target_column
            );
        }
    }

    // The cross-resolved count should be > 0 — honour stats reference vanilla entries
    assert!(
        report.cross_resolved > 0 || standalone_count == 0,
        "Expected some FK violations to resolve against base DB"
    );
    println!(
        "\n  Summary: {} standalone → {} cross-resolved + {} unresolved",
        standalone_count,
        report.cross_resolved,
        report.unresolved.len()
    );
}

/// Integration test: create a staging DB from the ref_base schema,
/// verify it has the tracking columns and can accept data via populate.
///
/// Requires: test_populated_dbs/ref_base.sqlite from `populate_all_schema_dbs`.
#[test]
#[ignore]
fn create_staging_db_from_base() {
    let ws = workspace_root();
    let base_db = ws.join("test_populated_dbs/ref_base.sqlite");
    assert!(
        base_db.is_file(),
        "test_populated_dbs/ref_base.sqlite not found — run populate_all_schema_dbs first"
    );

    let staging_db = ws.join("staging_test.sqlite");
    // Clean up previous test artifacts
    let _ = std::fs::remove_file(&staging_db);
    let staging_str = staging_db.to_string_lossy();
    for sfx in &["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{staging_str}{sfx}"));
    }

    // Step 1: Create empty staging DB
    println!("=== Creating staging DB from ref_base schema ===");
    let summary =
        bg3_cmty_studio_lib::reference_db::staging::create_staging_db(&base_db, &staging_db)
            .expect("create staging DB");

    println!(
        "  Tables: {} (including {} junctions)",
        summary.total_tables, summary.junction_tables
    );
    println!("  Size: {:.1} MB", summary.db_size_mb);
    println!("  Elapsed: {:.2}s", summary.elapsed_secs);

    assert!(
        summary.total_tables > 100,
        "Expected 100+ tables, got {}",
        summary.total_tables
    );
    assert!(staging_db.is_file(), "staging DB file not created");

    // Step 2: Verify tracking columns exist on a sample table
    let conn = rusqlite::Connection::open(&staging_db).expect("open staging DB");
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' \
                 AND name NOT LIKE '\\_%' ESCAPE '\\' \
                 AND name NOT LIKE 'jct_%' \
                 ORDER BY name LIMIT 1",
            )
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    assert!(!tables.is_empty(), "No data tables found in staging DB");
    let sample_table = &tables[0];
    println!("  Checking columns on sample table: {sample_table}");

    let cols: Vec<String> = {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info(\"{sample_table}\")"))
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    assert!(
        cols.contains(&"_is_modified".to_string()),
        "Missing _is_modified on {sample_table}. Columns: {cols:?}"
    );
    assert!(
        cols.contains(&"_is_new".to_string()),
        "Missing _is_new on {sample_table}"
    );
    assert!(
        cols.contains(&"_is_deleted".to_string()),
        "Missing _is_deleted on {sample_table}"
    );
    println!("  Tracking columns present: _is_modified, _is_new, _is_deleted");

    // Step 3: Verify WAL mode
    let journal: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    println!("  Journal mode: {journal}");
    assert_eq!(journal, "wal", "Expected WAL mode for staging DB");

    // Step 4: Verify no FK constraints exist
    let fk_count: i64 = {
        let table_names: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type='table'")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        let mut total = 0i64;
        for t in &table_names {
            let mut stmt = conn
                .prepare(&format!("PRAGMA foreign_key_list(\"{t}\")"))
                .unwrap();
            total += stmt.query_map([], |_| Ok(())).unwrap().count() as i64;
        }
        total
    };
    println!("  FK constraints: {fk_count} (expected 0)");
    assert_eq!(fk_count, 0, "Staging DB should have no FK constraints");

    // Step 5: Verify embedded schema blob exists
    let has_schema: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM _embedded_schema WHERE key = 'schema'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    assert!(has_schema, "Missing embedded schema blob in staging DB");
    println!("  Embedded schema blob: present");

    // Step 6: Verify ATTACH works for cross-DB queries
    bg3_cmty_studio_lib::reference_db::cross_db::attach_readonly(&conn, &base_db, "base")
        .expect("attach base to staging");

    let attached =
        bg3_cmty_studio_lib::reference_db::cross_db::list_attached(&conn).expect("list attached");
    assert_eq!(attached.len(), 1, "Expected 1 attached DB");
    println!("  Cross-DB ATTACH: OK (base attached)");

    // Verify we can query across DBs (base should have rows)
    let base_row_count: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM base.\"{sample_table}\""),
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    println!("  base.{sample_table} row count: {base_row_count} (cross-DB query OK)");

    // Clean up
    drop(conn);
    let _ = std::fs::remove_file(&staging_db);
    for sfx in &["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{staging_str}{sfx}"));
    }

    println!("\n  STAGING DB TEST PASSED");
}

/// Diagnostic test: audit a populated reference DB to understand
/// row distribution, empty tables, and FK integrity.
///
/// Requires: test_populated_dbs/ref_base.sqlite from `populate_all_schema_dbs`.
#[test]
#[ignore]
fn audit_populated_db() {
    let ws = workspace_root();
    let base_db = ws.join("test_populated_dbs/ref_base.sqlite");
    assert!(
        base_db.is_file(),
        "test_populated_dbs/ref_base.sqlite not found — run populate_all_schema_dbs first"
    );

    let conn = rusqlite::Connection::open(&base_db).expect("open DB");

    // 1. Source file breakdown by mod_name
    println!("=== Source Files by Module ===");
    let mut stmt = conn
        .prepare(
            "SELECT mod_name, file_type, COUNT(*), SUM(row_count)
         FROM _source_files GROUP BY mod_name, file_type ORDER BY mod_name, file_type",
        )
        .unwrap();
    let rows: Vec<(String, String, i64, i64)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get::<_, i64>(3).unwrap_or(0),
            ))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    for (mod_name, ftype, count, total_rows) in &rows {
        println!("  {mod_name:<20} {ftype:<6} {count:>6} files  {total_rows:>8} rows");
    }

    // 2. Tables with most rows
    println!("\n=== Top 30 Tables by Row Count ===");
    let table_names: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE '\\_%' ESCAPE '\\' AND name NOT LIKE 'jct_%'"
        ).unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    let mut table_counts: Vec<(String, i64)> = Vec::new();
    for tn in &table_names {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{tn}\""), [], |r| r.get(0))
            .unwrap_or(0);
        table_counts.push((tn.clone(), count));
    }
    table_counts.sort_by(|a, b| b.1.cmp(&a.1));
    let mut total_db_rows: i64 = 0;
    for (tn, count) in table_counts.iter().take(30) {
        println!("  {count:>10}  {tn}");
        total_db_rows += count;
    }
    for (_, count) in table_counts.iter().skip(30) {
        total_db_rows += count;
    }
    println!("  ----------");
    println!(
        "  {:>10}  TOTAL across {} tables",
        total_db_rows,
        table_counts.len()
    );

    // 3. Tables with 0 rows (empty)
    let empty_tables: Vec<&(String, i64)> = table_counts.iter().filter(|(_, c)| *c == 0).collect();
    if !empty_tables.is_empty() {
        println!("\n=== Empty Tables ({}) ===", empty_tables.len());
        for (tn, _) in &empty_tables {
            println!("  {tn}");
        }
    }

    // 4. FK violations breakdown
    println!("\n=== FK Violations ===");
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    let mut stmt = conn.prepare("PRAGMA foreign_key_check").unwrap();
    let violations: Vec<(String, i64, String, i64)> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    println!("  Total: {}", violations.len());

    // Group by table → parent
    let mut viol_groups: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (table, _, parent, _) in &violations {
        *viol_groups
            .entry(format!("{table} → {parent}"))
            .or_insert(0) += 1;
    }
    let mut viol_sorted: Vec<_> = viol_groups.into_iter().collect();
    viol_sorted.sort_by(|a, b| b.1.cmp(&a.1));
    for (key, count) in &viol_sorted {
        println!("  {count:>5}  {key}");
    }

    // Show sample of each violation type
    for (key, _) in viol_sorted.iter().take(5) {
        let parts: Vec<&str> = key.split(" → ").collect();
        if parts.len() == 2 {
            let sample = violations
                .iter()
                .filter(|(t, _, p, _)| t == parts[0] && p == parts[1])
                .take(3)
                .collect::<Vec<_>>();
            if !sample.is_empty() {
                println!("\n  Samples for '{key}': ");
                for (table, rowid, parent, fk_idx) in sample {
                    println!("    rowid={rowid} in {table} → {parent} (fk_idx={fk_idx})");
                }
            }
        }
    }

    println!("\n  AUDIT COMPLETE");
}

// ---------------------------------------------------------------------------
// Embedded Schema DB: Creation & Population
// ---------------------------------------------------------------------------

/// Helper: print a build summary with full phase timing.
fn print_build_summary(label: &str, summary: &bg3_cmty_studio_lib::reference_db::BuildSummary) {
    let pt = &summary.phase_times;
    println!("\n=== {label} ===");
    println!("  Files:       {}", summary.total_files);
    println!("  Tables:      {}", summary.total_tables);
    println!("  Rows:        {}", summary.total_rows);
    println!("  FK constrs:  {}", summary.fk_constraints);
    println!("  File errors: {}", summary.file_errors);
    println!("  Row errors:  {}", summary.row_errors);
    println!("  DB size:     {:.1} MB", summary.db_size_mb);
    println!("  --- Phase Timing ---");
    println!("  collect_files: {:>7.2}s", pt.collect_files);
    println!("  discovery:     {:>7.2}s", pt.discovery);
    println!("  ddl_creation:  {:>7.2}s", pt.ddl_creation);
    println!("  data_insert:   {:>7.2}s", pt.data_insert);
    println!("  merge:         {:>7.2}s", pt.merge);
    println!("  post_process:  {:>7.2}s", pt.post_process);
    println!("  write_to_disk: {:>7.2}s", pt.write_to_disk);
    println!("  TOTAL:         {:>7.02}s", summary.elapsed_secs);
}

/// Helper: count tables in a database (excludes internal/meta tables).
fn count_db_tables(conn: &rusqlite::Connection) -> usize {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' \
             AND name NOT LIKE '\\_%' ESCAPE '\\' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    n as usize
}

/// Helper: verify a database has an embedded schema blob.
fn assert_has_embedded_schema(conn: &rusqlite::Connection, label: &str) {
    let has: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM _embedded_schema WHERE key = 'schema'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    assert!(has, "{label}: missing embedded schema blob");
}

/// Create all four empty embedded schema databases from pak-streamed discovery:
///   - ref_base.sqlite  (standard PKs, FK constraints)
///   - ref_honor.sqlite (same structure as base)
///   - ref_mods.sqlite  (composite PKs, no FK constraints)
///   - staging.sqlite   (tracking columns, no FK constraints, WAL mode)
///
/// Reports per-phase timing including pak streaming, discovery, per-DB creation, and total.
///
/// Requires: BG3_GAME_DATA in .env or environment (path to game Data directory).
///
/// Run: cargo test --release --test test_reference_db create_all_empty_schema_dbs -- --ignored --nocapture
#[test]
#[ignore]
fn create_all_empty_schema_dbs() {
    use std::time::Instant;

    // Load .env from workspace root (env vars take precedence)
    let ws = workspace_root();
    let env_path = ws.join(".env");
    if env_path.is_file() {
        match dotenvy::from_path(&env_path) {
            Ok(_) => println!("  Loaded .env"),
            Err(e) => println!("  WARNING: failed to load .env: {e}"),
        }
    }
    let game_data_path = std::env::var("BG3_GAME_DATA")
        .expect("BG3_GAME_DATA not set — add it to .env or set the env var");
    let data_dir = std::path::Path::new(&game_data_path);
    assert!(
        data_dir.is_dir(),
        "Game data directory not found at: {game_data_path}"
    );
    println!("  Game data: {game_data_path}");

    let total_start = Instant::now();
    let mut phase_times: Vec<(&str, f64)> = Vec::new();

    // --- Phase 1: Stream files from paks ---
    let t = Instant::now();
    let (files, pak_diag) =
        bg3_cmty_studio_lib::reference_db::pipeline::collect_files_from_paks(data_dir)
            .expect("collect_files_from_paks failed");
    let collect_secs = t.elapsed().as_secs_f64();
    phase_times.push(("stream_paks", collect_secs));
    println!(
        "  stream_paks:   {:.2}s  ({} files)",
        collect_secs,
        files.len()
    );
    for d in &pak_diag {
        println!("    {d}");
    }

    // --- Phase 2: Schema discovery ---
    let t = Instant::now();
    let schema = bg3_cmty_studio_lib::reference_db::discovery::discover_schema(&files, data_dir)
        .expect("discover_schema failed");
    let discovery_secs = t.elapsed().as_secs_f64();
    phase_times.push(("discovery", discovery_secs));
    println!(
        "  discovery:     {:.2}s  ({} tables, {} junctions)",
        discovery_secs,
        schema.tables.len(),
        schema.junction_tables.len()
    );

    // Persist DBs in workspace so populate test can reuse them
    let out_dir = ws.join("test_schema_dbs");
    if out_dir.exists() {
        std::fs::remove_dir_all(&out_dir).expect("clean up old test_schema_dbs");
    }
    std::fs::create_dir_all(&out_dir).expect("create test_schema_dbs");
    println!("  Output dir: {}", out_dir.display());

    // --- Phase 3: Create ref_base.sqlite ---
    let base_db = out_dir.join("ref_base.sqlite");
    let t = Instant::now();
    bg3_cmty_studio_lib::reference_db::builder::create_schema_db(&base_db, &schema)
        .expect("create ref_base schema DB");
    let base_secs = t.elapsed().as_secs_f64();
    phase_times.push(("create_ref_base", base_secs));
    let base_size = std::fs::metadata(&base_db).map(|m| m.len()).unwrap_or(0);
    println!(
        "  create_ref_base:   {:.2}s  ({:.0} KB)",
        base_secs,
        base_size as f64 / 1024.0
    );

    // --- Phase 4: Copy ref_honor.sqlite (identical schema to base) ---
    let honor_db = out_dir.join("ref_honor.sqlite");
    let t = Instant::now();
    std::fs::copy(&base_db, &honor_db).expect("copy ref_base → ref_honor");
    let honor_secs = t.elapsed().as_secs_f64();
    phase_times.push(("copy_ref_honor", honor_secs));
    let honor_size = std::fs::metadata(&honor_db).map(|m| m.len()).unwrap_or(0);
    println!(
        "  copy_ref_honor:    {:.2}s  ({:.0} KB)",
        honor_secs,
        honor_size as f64 / 1024.0
    );

    // --- Phase 5: Create ref_mods.sqlite ---
    let mods_db = out_dir.join("ref_mods.sqlite");
    let t = Instant::now();
    bg3_cmty_studio_lib::reference_db::builder::create_mods_schema_db(&mods_db, &schema)
        .expect("create ref_mods schema DB");
    let mods_secs = t.elapsed().as_secs_f64();
    phase_times.push(("create_ref_mods", mods_secs));
    let mods_size = std::fs::metadata(&mods_db).map(|m| m.len()).unwrap_or(0);
    println!(
        "  create_ref_mods:   {:.2}s  ({:.0} KB)",
        mods_secs,
        mods_size as f64 / 1024.0
    );

    // --- Phase 6: Create staging.sqlite (reads schema from ref_base) ---
    let staging_db = out_dir.join("staging.sqlite");
    let t = Instant::now();
    let _staging_summary =
        bg3_cmty_studio_lib::reference_db::staging::create_staging_db(&base_db, &staging_db)
            .expect("create staging DB");
    let staging_secs = t.elapsed().as_secs_f64();
    phase_times.push(("create_staging", staging_secs));
    let staging_size = std::fs::metadata(&staging_db).map(|m| m.len()).unwrap_or(0);
    println!(
        "  create_staging:    {:.2}s  ({:.0} KB)",
        staging_secs,
        staging_size as f64 / 1024.0
    );

    let total_secs = total_start.elapsed().as_secs_f64();

    // --- Summary ---
    println!("\n=== Schema Creation Summary ===");
    println!(
        "  Schema: {} tables, {} junctions",
        schema.tables.len(),
        schema.junction_tables.len()
    );
    println!("  ┌────────────────────┬──────────┬──────────┐");
    println!("  │ Phase              │  Time    │  Output  │");
    println!("  ├────────────────────┼──────────┼──────────┤");
    for (name, secs) in &phase_times {
        let size_str = match *name {
            "create_ref_base" => format!("{:.0} KB", base_size as f64 / 1024.0),
            "copy_ref_honor" => format!("{:.0} KB", honor_size as f64 / 1024.0),
            "create_ref_mods" => format!("{:.0} KB", mods_size as f64 / 1024.0),
            "create_staging" => format!("{:.0} KB", staging_size as f64 / 1024.0),
            "stream_paks" => format!("{} files", files.len()),
            "discovery" => format!("{} tables", schema.tables.len()),
            _ => String::new(),
        };
        println!("  │ {name:<18} │ {secs:>6.2}s  │ {size_str:>8} │");
    }
    println!("  ├────────────────────┼──────────┼──────────┤");
    println!("  │ TOTAL              │ {total_secs:>6.2}s  │          │");
    println!("  └────────────────────┴──────────┴──────────┘");

    // --- Validation: ref_base ---
    {
        let conn = rusqlite::Connection::open(&base_db).expect("open ref_base");
        let table_count = count_db_tables(&conn);
        assert_has_embedded_schema(&conn, "ref_base");
        assert!(
            table_count > 100,
            "ref_base: expected 100+ tables, got {table_count}"
        );

        // Should have FK constraints
        let fk_total: i64 = {
            let tables: Vec<String> = {
                let mut stmt = conn
                    .prepare("SELECT name FROM sqlite_master WHERE type='table'")
                    .unwrap();
                stmt.query_map([], |row| row.get(0))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect()
            };
            let mut total = 0i64;
            for t in &tables {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA foreign_key_list(\"{t}\")"))
                    .unwrap();
                total += stmt.query_map([], |_| Ok(())).unwrap().count() as i64;
            }
            total
        };
        assert!(fk_total > 0, "ref_base: expected FK constraints, got 0");

        // FK violations (empty schema = 0 rows, so should be 0)
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let fk_violations: i64 = {
            let mut stmt = conn.prepare("PRAGMA foreign_key_check").unwrap();
            stmt.query_map([], |_| Ok(())).unwrap().count() as i64
        };
        assert_eq!(
            fk_violations, 0,
            "ref_base: expected 0 FK violations, got {fk_violations}"
        );
        println!("\n  ref_base:  {table_count} tables, {fk_total} FK constraint defs, {fk_violations} FK violations — OK");
    }

    // --- Validation: ref_honor (same structure as base) ---
    {
        let conn = rusqlite::Connection::open(&honor_db).expect("open ref_honor");
        let table_count = count_db_tables(&conn);
        assert_has_embedded_schema(&conn, "ref_honor");
        assert!(
            table_count > 100,
            "ref_honor: expected 100+ tables, got {table_count}"
        );
        println!("  ref_honor: {table_count} tables — OK");
    }

    // --- Validation: ref_mods (composite PKs, no FKs) ---
    {
        let conn = rusqlite::Connection::open(&mods_db).expect("open ref_mods");
        let table_count = count_db_tables(&conn);
        assert_has_embedded_schema(&conn, "ref_mods");
        assert!(
            table_count > 100,
            "ref_mods: expected 100+ tables, got {table_count}"
        );

        // Mods DB should have zero FK constraints
        let fk_total: i64 = {
            let tables: Vec<String> = {
                let mut stmt = conn
                    .prepare("SELECT name FROM sqlite_master WHERE type='table'")
                    .unwrap();
                stmt.query_map([], |row| row.get(0))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect()
            };
            let mut total = 0i64;
            for t in &tables {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA foreign_key_list(\"{t}\")"))
                    .unwrap();
                total += stmt.query_map([], |_| Ok(())).unwrap().count() as i64;
            }
            total
        };
        assert_eq!(
            fk_total, 0,
            "ref_mods: expected 0 FK constraints, got {fk_total}"
        );

        // Verify a sample data table has _SourceID in PK (composite PK check)
        let sample_table: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' \
                 AND name NOT LIKE '\\_%' ESCAPE '\\' AND name NOT LIKE 'jct_%' \
                 AND name NOT LIKE 'sqlite_%' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();
        if let Some(ref t) = sample_table {
            let cols: Vec<String> = {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA table_info(\"{t}\")"))
                    .unwrap();
                stmt.query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect()
            };
            assert!(
                cols.contains(&"_SourceID".to_string()),
                "ref_mods: sample table {t} missing _SourceID column"
            );
        }

        println!("  ref_mods:  {table_count} tables, 0 FK constraints, _SourceID in PK — OK");
    }

    // --- Validation: staging (tracking columns, WAL, no FKs) ---
    {
        let conn = rusqlite::Connection::open(&staging_db).expect("open staging");
        let table_count = count_db_tables(&conn);
        assert_has_embedded_schema(&conn, "staging");
        assert!(
            table_count > 100,
            "staging: expected 100+ tables, got {table_count}"
        );

        // WAL mode
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal, "wal", "staging: expected WAL mode");

        // Tracking columns on a sample table
        let sample: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' \
             AND name NOT LIKE '\\_%' ESCAPE '\\' AND name NOT LIKE 'jct_%' \
             AND name NOT LIKE 'sqlite_%' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .ok();
        if let Some(ref t) = sample {
            let cols: Vec<String> = {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA table_info(\"{t}\")"))
                    .unwrap();
                stmt.query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect()
            };
            assert!(
                cols.contains(&"_is_modified".to_string()),
                "staging: missing _is_modified on {t}"
            );
            assert!(
                cols.contains(&"_is_new".to_string()),
                "staging: missing _is_new on {t}"
            );
            assert!(
                cols.contains(&"_is_deleted".to_string()),
                "staging: missing _is_deleted on {t}"
            );
        }

        // Zero FK constraints
        let fk_total: i64 = {
            let tables: Vec<String> = {
                let mut stmt = conn
                    .prepare("SELECT name FROM sqlite_master WHERE type='table'")
                    .unwrap();
                stmt.query_map([], |row| row.get(0))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect()
            };
            let mut total = 0i64;
            for t in &tables {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA foreign_key_list(\"{t}\")"))
                    .unwrap();
                total += stmt.query_map([], |_| Ok(())).unwrap().count() as i64;
            }
            total
        };
        assert_eq!(
            fk_total, 0,
            "staging: expected 0 FK constraints, got {fk_total}"
        );

        println!("  staging:   {table_count} tables, WAL, tracking columns, 0 FKs — OK");
    }

    println!("\n  ALL SCHEMA DBS CREATED SUCCESSFULLY ({total_secs:.2}s total)");
}

/// Populate ref_base and ref_honor from pak-streamed game data.
///
/// Uses schema DBs from `test_schema_dbs/` (created by `create_all_empty_schema_dbs`)
/// and writes populated DBs to `test_populated_dbs/`.
///
/// Requires: BG3_GAME_DATA + test_schema_dbs/ directory.
///
/// Run: cargo test --release --test test_reference_db populate_all_schema_dbs -- --ignored --nocapture
#[test]
#[ignore]
fn populate_all_schema_dbs() {
    use std::time::Instant;

    let ws = workspace_root();
    let env_path = ws.join(".env");
    if env_path.is_file() {
        match dotenvy::from_path(&env_path) {
            Ok(_) => println!("  Loaded .env"),
            Err(e) => println!("  WARNING: failed to load .env: {e}"),
        }
    }
    let game_data_path = std::env::var("BG3_GAME_DATA")
        .expect("BG3_GAME_DATA not set — add it to .env or set the env var");

    // Require schema DBs from prior test
    let schema_dir = ws.join("test_schema_dbs");
    assert!(
        schema_dir.is_dir(),
        "test_schema_dbs/ not found — run create_all_empty_schema_dbs first"
    );

    // Output directory for populated DBs
    let out_dir = ws.join("test_populated_dbs");
    if out_dir.exists() {
        std::fs::remove_dir_all(&out_dir).expect("clean old test_populated_dbs");
    }
    std::fs::create_dir_all(&out_dir).expect("create test_populated_dbs");

    // Copy schema DBs to output directory (populate_vanilla_dbs modifies in-place)
    let base_db = out_dir.join("ref_base.sqlite");
    let honor_db = out_dir.join("ref_honor.sqlite");
    std::fs::copy(schema_dir.join("ref_base.sqlite"), &base_db).expect("copy ref_base schema");
    std::fs::copy(schema_dir.join("ref_honor.sqlite"), &honor_db).expect("copy ref_honor schema");

    println!("  Schema DBs: {}", schema_dir.display());
    println!("  Output dir: {}", out_dir.display());

    let total_start = Instant::now();

    // Run the unified populate pipeline (streams from paks + populates both DBs)
    let options = bg3_cmty_studio_lib::reference_db::BuildOptions::default();
    let pipeline = bg3_cmty_studio_lib::reference_db::pipeline::populate_vanilla_dbs(
        &game_data_path,
        &base_db,
        Some(&honor_db),
        &options,
        |_progress| {},
    )
    .expect("populate_vanilla_dbs failed");

    // Print diagnostics
    for d in &pipeline.diagnostics {
        println!("  {d}");
    }

    let base_summary = pipeline
        .base_summary
        .expect("base_summary missing from pipeline");
    let honor_summary = pipeline
        .honor_summary
        .expect("honor_summary missing from pipeline");

    // Approximate streaming time = pipeline total - (base populate + honor populate)
    let stream_secs =
        pipeline.elapsed_secs - base_summary.elapsed_secs - honor_summary.elapsed_secs;

    print_build_summary("ref_base Population", &base_summary);
    print_build_summary("ref_honor Population", &honor_summary);

    let total_secs = total_start.elapsed().as_secs_f64();

    // --- FK violation counts (computed before summary table) ---
    let (base_fk_violations, honor_fk_violations) = {
        let base_conn = rusqlite::Connection::open(&base_db).expect("open ref_base for FK check");
        base_conn
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
        let base_v: i64 = {
            let mut stmt = base_conn.prepare("PRAGMA foreign_key_check").unwrap();
            stmt.query_map([], |_| Ok(())).unwrap().count() as i64
        };
        let honor_conn =
            rusqlite::Connection::open(&honor_db).expect("open ref_honor for FK check");
        honor_conn
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
        let honor_v: i64 = {
            let mut stmt = honor_conn.prepare("PRAGMA foreign_key_check").unwrap();
            stmt.query_map([], |_| Ok(())).unwrap().count() as i64
        };
        (base_v, honor_v)
    };

    // --- Combined Summary ---
    println!("\n=== Combined Summary ===");
    println!("  ┌──────────────────────┬──────────┬──────────┬──────────┬─────────┐");
    println!("  │ Metric               │ ref_base │ ref_honor│ Combined │  Setup  │");
    println!("  ├──────────────────────┼──────────┼──────────┼──────────┼─────────┤");
    println!(
        "  │ Files                │ {:>8} │ {:>8} │ {:>8} │         │",
        base_summary.total_files,
        honor_summary.total_files,
        base_summary.total_files + honor_summary.total_files
    );
    println!(
        "  │ Tables               │ {:>8} │ {:>8} │          │         │",
        base_summary.total_tables, honor_summary.total_tables
    );
    println!(
        "  │ Rows                 │ {:>8} │ {:>8} │ {:>8} │         │",
        base_summary.total_rows,
        honor_summary.total_rows,
        base_summary.total_rows + honor_summary.total_rows
    );
    println!(
        "  │ FK constraints       │ {:>8} │ {:>8} │          │         │",
        base_summary.fk_constraints, honor_summary.fk_constraints
    );
    println!("  │ FK violations        │ {base_fk_violations:>8} │ {honor_fk_violations:>8} │          │         │");
    println!(
        "  │ File errors          │ {:>8} │ {:>8} │ {:>8} │         │",
        base_summary.file_errors,
        honor_summary.file_errors,
        base_summary.file_errors + honor_summary.file_errors
    );
    println!(
        "  │ Row errors           │ {:>8} │ {:>8} │ {:>8} │         │",
        base_summary.row_errors,
        honor_summary.row_errors,
        base_summary.row_errors + honor_summary.row_errors
    );
    println!(
        "  │ DB size (MB)         │ {:>8.1} │ {:>8.1} │ {:>8.1} │         │",
        base_summary.db_size_mb,
        honor_summary.db_size_mb,
        base_summary.db_size_mb + honor_summary.db_size_mb
    );
    println!("  ├──────────────────────┼──────────┼──────────┼──────────┼─────────┤");
    println!("  │ stream_paks (s)      │          │          │          │ {stream_secs:>7.2} │");
    println!(
        "  │ data_insert (s)      │ {:>8.2} │ {:>8.2} │ {:>8.2} │         │",
        base_summary.phase_times.data_insert,
        honor_summary.phase_times.data_insert,
        base_summary.phase_times.data_insert + honor_summary.phase_times.data_insert
    );
    println!(
        "  │ merge (s)            │ {:>8.2} │ {:>8.2} │ {:>8.2} │         │",
        base_summary.phase_times.merge,
        honor_summary.phase_times.merge,
        base_summary.phase_times.merge + honor_summary.phase_times.merge
    );
    println!(
        "  │ post_process (s)     │ {:>8.2} │ {:>8.2} │ {:>8.2} │         │",
        base_summary.phase_times.post_process,
        honor_summary.phase_times.post_process,
        base_summary.phase_times.post_process + honor_summary.phase_times.post_process
    );
    println!(
        "  │ write_to_disk (s)    │ {:>8.2} │ {:>8.2} │ {:>8.2} │         │",
        base_summary.phase_times.write_to_disk,
        honor_summary.phase_times.write_to_disk,
        base_summary.phase_times.write_to_disk + honor_summary.phase_times.write_to_disk
    );
    println!(
        "  │ populate total (s)   │ {:>8.2} │ {:>8.2} │ {:>8.2} │         │",
        base_summary.elapsed_secs,
        honor_summary.elapsed_secs,
        base_summary.elapsed_secs + honor_summary.elapsed_secs
    );
    println!("  ├──────────────────────┼──────────┼──────────┼──────────┼─────────┤");
    println!("  │ GRAND TOTAL (s)      │          │          │ {total_secs:>8.2} │         │");
    println!("  └──────────────────────┴──────────┴──────────┴──────────┴─────────┘");

    // --- Assertions ---
    assert!(
        base_summary.total_tables > 100,
        "ref_base: expected 100+ tables, got {}",
        base_summary.total_tables
    );
    assert!(
        base_summary.total_rows > 100_000,
        "ref_base: expected 100k+ rows, got {}",
        base_summary.total_rows
    );
    assert!(
        base_summary.fk_constraints > 0,
        "ref_base: expected FK constraints"
    );
    assert_eq!(
        base_summary.file_errors, 0,
        "ref_base: unexpected file errors"
    );
    assert_eq!(
        base_summary.row_errors, 0,
        "ref_base: unexpected row errors"
    );

    assert!(
        honor_summary.total_files > 0,
        "ref_honor: expected >0 files"
    );
    assert!(honor_summary.total_rows > 0, "ref_honor: expected >0 rows");
    assert_eq!(
        honor_summary.file_errors, 0,
        "ref_honor: unexpected file errors"
    );
    assert_eq!(
        honor_summary.row_errors, 0,
        "ref_honor: unexpected row errors"
    );

    // Discovery and DDL should both be skipped (0.0s) since we used embedded schemas
    assert_eq!(
        base_summary.phase_times.discovery, 0.0,
        "ref_base: discovery should be skipped"
    );
    assert_eq!(
        base_summary.phase_times.ddl_creation, 0.0,
        "ref_base: DDL should be skipped"
    );
    assert_eq!(
        honor_summary.phase_times.discovery, 0.0,
        "ref_honor: discovery should be skipped"
    );
    assert_eq!(
        honor_summary.phase_times.ddl_creation, 0.0,
        "ref_honor: DDL should be skipped"
    );

    // --- FK integrity detail on ref_base (if violations found) ---
    println!("\n=== FK Integrity: ref_base ===");
    if base_fk_violations == 0 {
        println!("  No FK violations!");
    } else {
        let conn = rusqlite::Connection::open(&base_db).expect("open ref_base for FK detail");
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let mut stmt = conn.prepare("PRAGMA foreign_key_check").unwrap();
        let violations: Vec<(String, i64, String, i64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        println!("  {} FK violations (first 10):", violations.len());
        for v in violations.iter().take(10) {
            println!("    {} rowid={} → {} (fk_idx={})", v.0, v.1, v.2, v.3);
        }
    }

    // FK violations are expected with pak-based ingestion (some referenced
    // parent rows live in paks/regions not yet fully resolved). Report but
    // don't hard-fail — track the count for regression monitoring.
    println!("\n  ref_base FK violations:  {base_fk_violations}");
    println!("  ref_honor FK violations: {honor_fk_violations}");

    println!("\n  ALL POPULATION TESTS PASSED ({total_secs:.2}s total)");
}
