//! Diagnostic test: analyze row errors and FK mismatches in the built DB.

use std::collections::HashMap;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
#[ignore]
fn diagnose_row_errors() {
    let ws = workspace_root();
    let unpacked = ws.join("UnpackedData");

    // Run discovery only to analyze which tables would cause row errors
    let files = collect_files(&unpacked);
    println!("Collected {} files", files.len());

    let schema = bg3_cmty_studio_lib::reference_db::discovery::discover_schema(&files, &unpacked)
        .expect("discovery failed");

    // Count tables by PK strategy
    let mut pk_counts: HashMap<String, usize> = HashMap::new();
    let mut has_parent_and_uuid: Vec<String> = Vec::new();
    for (name, ts) in &schema.tables {
        *pk_counts
            .entry(format!("{:?}", ts.pk_strategy))
            .or_default() += 1;
        if !ts.parent_tables.is_empty()
            && ts.pk_strategy != bg3_cmty_studio_lib::reference_db::schema::PkStrategy::Rowid
        {
            has_parent_and_uuid.push(name.clone());
        }
    }

    println!("\n=== PK Strategy Distribution ===");
    let mut sorted: Vec<_> = pk_counts.iter().collect();
    sorted.sort_by_key(|(_k, v)| std::cmp::Reverse(*v));
    for (strategy, count) in sorted {
        println!("  {strategy:20} {count}");
    }

    println!(
        "\n=== Child tables with non-Rowid PK ({}) ===",
        has_parent_and_uuid.len()
    );
    has_parent_and_uuid.sort();
    for name in &has_parent_and_uuid {
        let ts = schema.tables.get(name).unwrap();
        println!(
            "  {} (PK: {:?}, parents: {:?})",
            name, ts.pk_strategy, ts.parent_tables
        );
    }
}

#[test]
#[ignore]
fn diagnose_fk_mismatches() {
    let ws = workspace_root();
    let db_path = ws.join("reference_rust.sqlite");
    if !db_path.is_file() {
        println!("reference_rust.sqlite not found — run build_reference_db_full first");
        return;
    }

    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    // Find all FK definitions
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };

    println!("=== FK Definitions Referencing Missing Columns ===");
    let mut bad_fks = 0;
    for table in &tables {
        let fk_list: Vec<(i64, String, String, String, String)> = {
            let mut stmt = conn
                .prepare(&format!("PRAGMA foreign_key_list(\"{table}\")"))
                .unwrap();
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(2)?, // target table
                    row.get::<_, String>(3)?, // from column
                    row.get::<_, String>(4)?, // to column
                    row.get::<_, String>(7)?, // match
                ))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
        };

        for (id, target_table, from_col, to_col, _) in &fk_list {
            // Check if target table exists
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
                    [target_table],
                    |r| r.get(0),
                )
                .unwrap_or(false);

            if !exists {
                println!("  {table} FK#{id}: {table}.{from_col} -> {target_table}.{to_col} — TARGET TABLE MISSING");
                bad_fks += 1;
                continue;
            }

            // Check if target column exists
            let col_exists: bool = {
                let mut cols_stmt = conn
                    .prepare(&format!("PRAGMA table_info(\"{target_table}\")"))
                    .unwrap();
                let cols: Vec<String> = cols_stmt
                    .query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect();
                cols.iter().any(|c| c == to_col)
            };

            if !col_exists {
                println!("  {table} FK#{id}: {table}.{from_col} -> {target_table}.{to_col} — TARGET COLUMN MISSING");
                bad_fks += 1;
            }
        }
    }
    println!("\nTotal bad FKs: {bad_fks}");

    // Check DDL of key problem tables
    println!("\n=== DDL of Key Tables ===");
    for check_table in &[
        "lsx__Templates__GameObjects",
        "lsx__Progressions__Progression",
    ] {
        let ddl: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                [check_table],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| "NOT FOUND".to_string());
        println!("\n--- {check_table} ---\n{ddl}");

        // Show FK list
        let mut stmt = conn
            .prepare(&format!("PRAGMA foreign_key_list(\"{check_table}\")"))
            .unwrap();
        let fks: Vec<String> = stmt
            .query_map([], |row| {
                Ok(format!(
                    "  FK#{}: {} -> {}.{}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        for fk in &fks {
            println!("{fk}");
        }
    }

    // Detailed per-table FK check with violation breakdown
    println!("\n=== Per-Table FK Violations (detailed) ===");
    let mut total_violations = 0;
    let mut tables_with_violations = 0;

    // Collect all violations with details: (source_table, rowid, target_table, fk_index)
    struct ViolationGroup {
        source_table: String,
        target_table: String,
        from_col: String,
        to_col: String,
        count: usize,
        sample_values: Vec<String>,
    }
    let mut groups: Vec<ViolationGroup> = Vec::new();

    for table in &tables {
        // Get FK definitions for this table
        let fk_list: Vec<(i64, String, String, String)> = {
            let mut stmt = conn
                .prepare(&format!("PRAGMA foreign_key_list(\"{table}\")"))
                .unwrap();
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,    // id
                    row.get::<_, String>(2)?, // target table
                    row.get::<_, String>(3)?, // from column
                    row.get::<_, String>(4)?, // to column
                ))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
        };

        if fk_list.is_empty() {
            continue;
        }

        // Run FK check
        match conn.prepare(&format!("PRAGMA foreign_key_check(\"{table}\")")) {
            Ok(mut stmt) => {
                // foreign_key_check returns: table, rowid, parent_table, fkid
                let violations: Vec<(i64, String, i64)> = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, i64>(1)?,    // rowid
                            row.get::<_, String>(2)?, // parent (target) table
                            row.get::<_, i64>(3)?,    // fkid
                        ))
                    })
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect();

                if violations.is_empty() {
                    continue;
                }

                tables_with_violations += 1;
                total_violations += violations.len();

                // Group violations by FK index
                let mut by_fk: HashMap<i64, Vec<i64>> = HashMap::new();
                for (rowid, _, fkid) in &violations {
                    by_fk.entry(*fkid).or_default().push(*rowid);
                }

                for (fkid, rowids) in &by_fk {
                    let (_, target_table, from_col, to_col) =
                        match fk_list.iter().find(|(id, ..)| id == fkid) {
                            Some(fk) => fk,
                            None => continue,
                        };

                    // Sample up to 5 orphaned values
                    let sample_rowids: Vec<i64> = rowids.iter().take(5).copied().collect();
                    let mut sample_values = Vec::new();
                    for rid in &sample_rowids {
                        let val: Option<String> = conn
                            .query_row(
                                &format!("SELECT \"{from_col}\" FROM \"{table}\" WHERE rowid = ?1"),
                                [rid],
                                |r| r.get(0),
                            )
                            .ok();
                        if let Some(v) = val {
                            sample_values.push(v);
                        }
                    }

                    groups.push(ViolationGroup {
                        source_table: table.clone(),
                        target_table: target_table.clone(),
                        from_col: from_col.clone(),
                        to_col: to_col.clone(),
                        count: rowids.len(),
                        sample_values,
                    });
                }
            }
            Err(e) => {
                println!("  {table}: FK CHECK ERROR — {e}");
            }
        }
    }

    // Sort by violation count descending
    groups.sort_by(|a, b| b.count.cmp(&a.count));

    println!("\n=== FK Violation Groups (sorted by count) ===");
    for g in &groups {
        println!(
            "\n  {}.{} -> {}.{} — {} violations",
            g.source_table, g.from_col, g.target_table, g.to_col, g.count
        );
        for (i, val) in g.sample_values.iter().enumerate() {
            println!("    sample[{i}]: {val}");
        }
    }

    // Summary by target table
    println!("\n=== Violations by Target Table ===");
    let mut by_target: HashMap<String, usize> = HashMap::new();
    for g in &groups {
        *by_target.entry(g.target_table.clone()).or_default() += g.count;
    }
    let mut target_sorted: Vec<_> = by_target.iter().collect();
    target_sorted.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    for (target, count) in &target_sorted {
        println!("  {target:60} {count}");
    }

    // Summary by source→target relationship type (lsx→lsx, lsx→stats, stats→lsx, stats→stats, loca→*)
    println!("\n=== Violations by Relationship Type ===");
    let mut by_type: HashMap<String, usize> = HashMap::new();
    for g in &groups {
        let src_type = if g.source_table.starts_with("lsx__") {
            "lsx"
        } else if g.source_table.starts_with("stats__") {
            "stats"
        } else if g.source_table.starts_with("loca__") {
            "loca"
        } else {
            "other"
        };
        let tgt_type = if g.target_table.starts_with("lsx__") {
            "lsx"
        } else if g.target_table.starts_with("stats__") {
            "stats"
        } else if g.target_table.starts_with("loca__") {
            "loca"
        } else {
            "other"
        };
        let key = format!("{src_type} -> {tgt_type}");
        *by_type.entry(key).or_default() += g.count;
    }
    let mut type_sorted: Vec<_> = by_type.iter().collect();
    type_sorted.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    for (rtype, count) in &type_sorted {
        println!("  {rtype:20} {count}");
    }

    println!("\n=== TOTALS ===");
    println!(
        "Total data violations: {} across {} tables in {} FK groups",
        total_violations,
        tables_with_violations,
        groups.len()
    );
}

#[test]
#[ignore]
fn investigate_non_loca_violations() {
    let ws = workspace_root();
    let db_path = ws.join("reference_rust.sqlite");
    if !db_path.is_file() {
        println!("reference_rust.sqlite not found");
        return;
    }

    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    // Helper: run a query and print results
    let run_query = |label: &str, sql: &str| {
        println!("\n=== {label} ===");
        match conn.prepare(sql) {
            Ok(mut stmt) => {
                let col_count = stmt.column_count();
                let col_names: Vec<String> = (0..col_count)
                    .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
                    .collect();
                println!("  {}", col_names.join(" | "));
                println!(
                    "  {}",
                    col_names
                        .iter()
                        .map(|_| "---")
                        .collect::<Vec<_>>()
                        .join(" | ")
                );
                let mut rows = stmt.query([]).unwrap();
                let mut count = 0;
                while let Some(row) = rows.next().unwrap() {
                    let vals: Vec<String> = (0..col_count)
                        .map(|i| {
                            row.get::<_, String>(i)
                                .unwrap_or_else(|_| "NULL".to_string())
                        })
                        .collect();
                    println!("  {}", vals.join(" | "));
                    count += 1;
                }
                println!("  ({count} rows)");
            }
            Err(e) => println!("  ERROR: {e}"),
        }
    };

    // =========================================================================
    // 1. VoiceTableUUID — is it referencing Voice.UUID or Voice.TableUUID?
    // =========================================================================
    println!("\n================================================================================");
    println!("INVESTIGATION 1: VoiceTableUUID");
    println!("================================================================================");

    // Show Voice table columns
    run_query(
        "Voice table DDL",
        "SELECT sql FROM sqlite_master WHERE name='lsx__Voices__Voice'",
    );

    // Show sample Voice rows
    run_query(
        "Sample Voice rows (UUID, TableUUID, SpeakerUUID)",
        "SELECT UUID, TableUUID, SpeakerUUID FROM lsx__Voices__Voice LIMIT 10",
    );

    // Count distinct TableUUID values vs UUID values
    run_query(
        "Voice UUID vs TableUUID cardinality",
        "SELECT 
           COUNT(DISTINCT UUID) as distinct_uuids,
           COUNT(DISTINCT TableUUID) as distinct_table_uuids,
           COUNT(*) as total_rows
         FROM lsx__Voices__Voice",
    );

    // Check: do the violating VoiceTableUUID values match any Voice.TableUUID?
    run_query("CompanionPresets violating VoiceTableUUIDs — do they match Voice.TableUUID?",
        "SELECT cp.VoiceTableUUID, 
                (SELECT COUNT(*) FROM lsx__Voices__Voice v WHERE v.UUID = cp.VoiceTableUUID) as matches_uuid,
                (SELECT COUNT(*) FROM lsx__Voices__Voice v WHERE v.TableUUID = cp.VoiceTableUUID) as matches_tableuuid
         FROM lsx__CompanionPresets__CompanionPreset cp
         WHERE cp.VoiceTableUUID NOT IN (SELECT UUID FROM lsx__Voices__Voice)
         GROUP BY cp.VoiceTableUUID
         LIMIT 20");

    // Same for Origins
    run_query("Origins violating VoiceTableUUIDs — do they match Voice.TableUUID?",
        "SELECT o.VoiceTableUUID,
                (SELECT COUNT(*) FROM lsx__Voices__Voice v WHERE v.UUID = o.VoiceTableUUID) as matches_uuid,
                (SELECT COUNT(*) FROM lsx__Voices__Voice v WHERE v.TableUUID = o.VoiceTableUUID) as matches_tableuuid
         FROM lsx__Origins__Origin o
         WHERE o.VoiceTableUUID NOT IN (SELECT UUID FROM lsx__Voices__Voice)
         GROUP BY o.VoiceTableUUID
         LIMIT 20");

    // =========================================================================
    // 2. CharacterVisualBank MaterialOverrides parent orphans (6382)
    // =========================================================================
    println!("\n================================================================================");
    println!("INVESTIGATION 2: CharacterVisualBank MaterialOverrides._parent_key");
    println!("================================================================================");

    run_query(
        "MaterialOverrides parent table DDL",
        "SELECT sql FROM sqlite_master WHERE name='lsx__CharacterVisualBank__Materials'",
    );

    run_query(
        "MaterialOverrides sample orphaned parent keys",
        "SELECT mo._parent_key, COUNT(*) as cnt
         FROM lsx__CharacterVisualBank__MaterialOverrides mo
         WHERE mo._parent_key NOT IN (SELECT MapKey FROM lsx__CharacterVisualBank__Materials)
         GROUP BY mo._parent_key
         ORDER BY cnt DESC
         LIMIT 15",
    );

    // Check if orphaned parent keys exist in CharacterVisualBank__Resource instead
    run_query("Do orphaned MaterialOverrides._parent_key values exist in Resource.ID?",
        "SELECT COUNT(*) as orphan_count,
                SUM(CASE WHEN mo._parent_key IN (SELECT ID FROM lsx__CharacterVisualBank__Resource) THEN 1 ELSE 0 END) as in_resource,
                SUM(CASE WHEN mo._parent_key IN (SELECT MapKey FROM lsx__CharacterVisualBank__Materials) THEN 1 ELSE 0 END) as in_materials
         FROM lsx__CharacterVisualBank__MaterialOverrides mo
         WHERE mo._parent_key NOT IN (SELECT MapKey FROM lsx__CharacterVisualBank__Materials)");

    // What tables exist in CharacterVisualBank?
    run_query("CharacterVisualBank tables",
        "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE '%CharacterVisualBank%' ORDER BY name");

    // =========================================================================
    // 3. VisualTemplate / PhysicsTemplate → VisualBank/PhysicsBank
    // =========================================================================
    println!("\n================================================================================");
    println!("INVESTIGATION 3: VisualTemplate & PhysicsTemplate");
    println!("================================================================================");

    run_query(
        "Sample orphaned VisualTemplate values",
        "SELECT VisualTemplate, COUNT(*) as cnt
         FROM lsx__Templates__GameObjects
         WHERE VisualTemplate IS NOT NULL
           AND VisualTemplate NOT IN (SELECT ID FROM lsx__VisualBank__Resource)
         GROUP BY VisualTemplate
         ORDER BY cnt DESC
         LIMIT 10",
    );

    // Check if VisualBank Resource table has these as Name instead of ID
    run_query("Do orphaned VisualTemplates exist as VisualBank Name?",
        "SELECT COUNT(*) as orphan_count,
                SUM(CASE WHEN go.VisualTemplate IN (SELECT Name FROM lsx__VisualBank__Resource) THEN 1 ELSE 0 END) as matches_name
         FROM lsx__Templates__GameObjects go
         WHERE go.VisualTemplate IS NOT NULL
           AND go.VisualTemplate NOT IN (SELECT ID FROM lsx__VisualBank__Resource)");

    run_query(
        "Sample orphaned PhysicsTemplate values",
        "SELECT PhysicsTemplate, COUNT(*) as cnt
         FROM lsx__Templates__GameObjects
         WHERE PhysicsTemplate IS NOT NULL
           AND PhysicsTemplate NOT IN (SELECT ID FROM lsx__PhysicsBank__Resource)
         GROUP BY PhysicsTemplate
         ORDER BY cnt DESC
         LIMIT 10",
    );

    // =========================================================================
    // 4. Templates__String._parent_key → Templates__Object.MapKey (110)
    // =========================================================================
    println!("\n================================================================================");
    println!("INVESTIGATION 4: Templates__String._parent_key orphans");
    println!("================================================================================");

    run_query(
        "Templates__String parent table",
        "SELECT sql FROM sqlite_master WHERE name='lsx__Templates__String'",
    );

    run_query(
        "Templates__Object table columns",
        "SELECT sql FROM sqlite_master WHERE name='lsx__Templates__Object'",
    );

    run_query(
        "Sample orphaned Templates__String._parent_key values",
        "SELECT s._parent_key, COUNT(*) as cnt
         FROM lsx__Templates__String s
         WHERE s._parent_key NOT IN (SELECT MapKey FROM lsx__Templates__Object)
         GROUP BY s._parent_key
         ORDER BY cnt DESC
         LIMIT 10",
    );

    // What other Templates tables have MapKey?
    run_query(
        "Templates tables with MapKey column",
        "SELECT m.name FROM sqlite_master m
         WHERE m.type='table' AND m.name LIKE 'lsx__Templates%'
         AND EXISTS (SELECT 1 FROM pragma_table_info(m.name) WHERE name='MapKey')
         ORDER BY m.name",
    );

    // =========================================================================
    // 5. Tags UUID references
    // =========================================================================
    println!("\n================================================================================");
    println!("INVESTIGATION 5: Tags UUID references");
    println!("================================================================================");

    run_query(
        "Tags table row count and sample",
        "SELECT COUNT(*) as total FROM lsx__Tags__Tags",
    );

    run_query(
        "FaceExpressions TagsFilter orphaned .Object values",
        "SELECT Object, COUNT(*) as cnt
         FROM lsx__FaceExpressions__TagsFilter
         WHERE Object NOT IN (SELECT UUID FROM lsx__Tags__Tags)
         GROUP BY Object
         ORDER BY cnt DESC
         LIMIT 10",
    );

    run_query(
        "Templates__Tag orphaned .Object values",
        "SELECT Object, COUNT(*) as cnt
         FROM lsx__Templates__Tag
         WHERE Object NOT IN (SELECT UUID FROM lsx__Tags__Tags)
         GROUP BY Object
         ORDER BY cnt DESC
         LIMIT 10",
    );

    // =========================================================================
    // 6. AnimationSetPriorities → ClassDescriptions (42)
    // =========================================================================
    println!("\n================================================================================");
    println!("INVESTIGATION 6: AnimationSetPriorities → ClassDescriptions");
    println!("================================================================================");

    run_query(
        "Sample orphaned AnimationSetPriorities .Object values",
        "SELECT Object, COUNT(*) as cnt
         FROM lsx__AnimationSetPriorities__AddidionalObjects
         WHERE Object NOT IN (SELECT UUID FROM lsx__ClassDescriptions__ClassDescription)
         GROUP BY Object
         ORDER BY cnt DESC
         LIMIT 10",
    );

    // =========================================================================
    // 7. Origins.GlobalTemplate → GameObjects.MapKey (25)
    // =========================================================================
    println!("\n================================================================================");
    println!("INVESTIGATION 7: Origins.GlobalTemplate");
    println!("================================================================================");

    run_query(
        "Sample orphaned Origins.GlobalTemplate values",
        "SELECT GlobalTemplate, COUNT(*) as cnt
         FROM lsx__Origins__Origin
         WHERE GlobalTemplate IS NOT NULL
           AND GlobalTemplate NOT IN (SELECT MapKey FROM lsx__Templates__GameObjects)
         GROUP BY GlobalTemplate
         ORDER BY cnt DESC
         LIMIT 10",
    );

    // =========================================================================
    // 8. CharacterCreationMaterialOverrides → MaterialPresetBank (22+22)
    // =========================================================================
    println!("\n================================================================================");
    println!("INVESTIGATION 8: CharacterCreationMaterialOverrides");
    println!("================================================================================");

    run_query(
        "Sample orphaned ActiveMaterialPresetUUID values",
        "SELECT ActiveMaterialPresetUUID, COUNT(*) as cnt
         FROM lsx__CharacterCreationMaterialOverrides__CharacterCreationMaterialOverride
         WHERE ActiveMaterialPresetUUID NOT IN (SELECT ID FROM lsx__MaterialPresetBank__Resource)
         GROUP BY ActiveMaterialPresetUUID
         ORDER BY cnt DESC
         LIMIT 10",
    );

    run_query("MaterialPresetBank tables",
        "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE '%MaterialPreset%' ORDER BY name");

    // =========================================================================
    // 9. MultiEffectInfos EffectInfo parent orphans (18)
    // =========================================================================
    println!("\n================================================================================");
    println!("INVESTIGATION 9: MultiEffectInfos EffectInfo._parent_key");
    println!("================================================================================");

    run_query(
        "MultiEffectInfos table DDL",
        "SELECT sql FROM sqlite_master WHERE name='lsx__MultiEffectInfos__MultiEffectInfos'",
    );

    run_query(
        "Sample orphaned EffectInfo parent keys",
        "SELECT e._parent_key, COUNT(*) as cnt
         FROM lsx__MultiEffectInfos__EffectInfo e
         WHERE e._parent_key NOT IN (SELECT UUID FROM lsx__MultiEffectInfos__MultiEffectInfos)
         GROUP BY e._parent_key
         ORDER BY cnt DESC
         LIMIT 10",
    );

    // What files contain these orphaned parent UUIDs?
    run_query(
        "Source files for orphaned EffectInfo rows",
        "SELECT f.rel_path, COUNT(*) as cnt
         FROM lsx__MultiEffectInfos__EffectInfo e
         JOIN _source_files f ON f.file_id = e._file_id
         WHERE e._parent_key NOT IN (SELECT UUID FROM lsx__MultiEffectInfos__MultiEffectInfos)
         GROUP BY f.rel_path
         ORDER BY cnt DESC
         LIMIT 10",
    );

    // =========================================================================
    // 10. Stats small groups
    // =========================================================================
    println!("\n================================================================================");
    println!("INVESTIGATION 10: Stats violations");
    println!("================================================================================");

    run_query(
        "stats__Object orphaned RootTemplate values",
        "SELECT RootTemplate, COUNT(*) as cnt
         FROM stats__Object
         WHERE RootTemplate IS NOT NULL
           AND RootTemplate NOT IN (SELECT MapKey FROM lsx__Templates__GameObjects)
         GROUP BY RootTemplate
         ORDER BY cnt DESC
         LIMIT 10",
    );

    run_query(
        "stats__SpellData orphaned SpellContainerID values",
        "SELECT SpellContainerID, COUNT(*) as cnt
         FROM stats__SpellData
         WHERE SpellContainerID IS NOT NULL
           AND SpellContainerID NOT IN (SELECT _entry_name FROM stats__SpellData)
         GROUP BY SpellContainerID
         LIMIT 10",
    );

    run_query(
        "stats__SpellData orphaned CastEffect values",
        "SELECT CastEffect, COUNT(*) as cnt
         FROM stats__SpellData
         WHERE CastEffect IS NOT NULL
           AND CastEffect NOT IN (SELECT UUID FROM lsx__MultiEffectInfos__MultiEffectInfos)
         GROUP BY CastEffect
         LIMIT 10",
    );

    println!("\n=== INVESTIGATION COMPLETE ===");
}

#[test]
#[ignore]
fn voice_deep_dive() {
    let ws = workspace_root();
    let db_path = ws.join("reference_rust.sqlite");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    // 1. All distinct VoiceTableUUID from Origins — match against Voice.UUID and Voice.TableUUID
    println!("=== Origins: all VoiceTableUUID values ===");
    let mut stmt = conn.prepare(
        "SELECT DISTINCT o.VoiceTableUUID FROM lsx__Origins__Origin o WHERE o.VoiceTableUUID IS NOT NULL"
    ).unwrap();
    let uuids: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    for uuid in &uuids {
        let in_uuid: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM lsx__Voices__Voice WHERE UUID = ?1",
                [uuid],
                |r| r.get(0),
            )
            .unwrap();
        let in_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM lsx__Voices__Voice WHERE TableUUID = ?1",
                [uuid],
                |r| r.get(0),
            )
            .unwrap();
        let status = if in_uuid > 0 {
            "MATCH-UUID"
        } else if in_table > 0 {
            "MATCH-TableUUID"
        } else {
            "ORPHAN"
        };
        println!("  {uuid} | Voice.UUID:{in_uuid} | Voice.TableUUID:{in_table} | {status}");
    }

    // 2. All distinct VoiceTableUUID from CompanionPresets
    println!("\n=== CompanionPresets: all VoiceTableUUID values ===");
    let mut stmt = conn.prepare(
        "SELECT DISTINCT cp.VoiceTableUUID FROM lsx__CompanionPresets__CompanionPreset cp WHERE cp.VoiceTableUUID IS NOT NULL"
    ).unwrap();
    let uuids: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    for uuid in &uuids {
        let in_uuid: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM lsx__Voices__Voice WHERE UUID = ?1",
                [uuid],
                |r| r.get(0),
            )
            .unwrap();
        let in_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM lsx__Voices__Voice WHERE TableUUID = ?1",
                [uuid],
                |r| r.get(0),
            )
            .unwrap();
        let status = if in_uuid > 0 {
            "MATCH-UUID"
        } else if in_table > 0 {
            "MATCH-TableUUID"
        } else {
            "ORPHAN"
        };
        println!("  {uuid} | Voice.UUID:{in_uuid} | Voice.TableUUID:{in_table} | {status}");
    }

    // 3. MaterialOverrides orphan analysis
    println!("\n=== MaterialOverrides orphan analysis ===");
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lsx__CharacterVisualBank__MaterialOverrides",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let orphaned: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lsx__CharacterVisualBank__MaterialOverrides mo
         WHERE mo._parent_key IS NOT NULL
           AND mo._parent_key NOT IN (SELECT MapKey FROM lsx__CharacterVisualBank__Materials)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let null_parent: i64 = conn.query_row(
        "SELECT COUNT(*) FROM lsx__CharacterVisualBank__MaterialOverrides mo WHERE mo._parent_key IS NULL",
        [], |r| r.get(0)
    ).unwrap();
    println!("  Total rows: {total}");
    println!("  Orphaned _parent_key: {orphaned}");
    println!("  NULL _parent_key: {null_parent}");

    // Are orphaned parent keys actually in CharacterVisualBank__Resource.ID?
    let in_resource: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lsx__CharacterVisualBank__MaterialOverrides mo
         WHERE mo._parent_key IS NOT NULL
           AND mo._parent_key NOT IN (SELECT MapKey FROM lsx__CharacterVisualBank__Materials)
           AND mo._parent_key IN (SELECT ID FROM lsx__CharacterVisualBank__Resource)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    println!("  Orphans matching Resource.ID: {in_resource}");

    // Check the XML hierarchy: what parent node do MaterialOverrides sit under?
    // The Materials table has MapKey PK, so parent_key should be a Materials.MapKey
    // But if Materials is a child of Resource, and MaterialOverrides is also a child of Resource...
    // Check parent info
    println!("\n=== CharacterVisualBank parent chains ===");
    let tables = vec![
        "lsx__CharacterVisualBank__MaterialOverrides",
        "lsx__CharacterVisualBank__Materials",
        "lsx__CharacterVisualBank__Resource",
        "lsx__CharacterVisualBank__ColorPreset",
    ];
    for t in &tables {
        let ddl: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name = ?1", [t], |r| {
                r.get(0)
            })
            .unwrap_or_else(|_| "NOT FOUND".into());
        // Extract just the first few lines
        let first_lines: String = ddl.lines().take(5).collect::<Vec<_>>().join("\n");
        println!("  {t} => {first_lines}");
    }

    // 4. Source files for orphaned EffectInfo rows
    println!("\n=== _source_files DDL ===");
    let ddl: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='_source_files'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    println!("{ddl}");

    println!("\n=== Source files for orphaned EffectInfo rows ===");
    // Get the columns of _source_files first
    let mut stmt = conn.prepare("PRAGMA table_info(_source_files)").unwrap();
    let cols: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(1)?, r.get(2)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    for (name, typ) in &cols {
        println!("  col: {name} ({typ})");
    }

    // 5. Verify: do VisualTemplate/PhysicsTemplate orphan values exist in other bank tables?
    println!("\n=== VisualBank tables ===");
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE '%VisualBank%' ORDER BY name"
    ).unwrap();
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    for n in &names {
        println!("  {n}");
    }

    println!("\n=== PhysicsBank tables ===");
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE '%PhysicsBank%' OR name LIKE '%Physics_Resource%' ORDER BY name"
    ).unwrap();
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    for n in &names {
        println!("  {n}");
    }

    // 6. Tags: check source files for orphaned tag UUIDs
    println!("\n=== Tags total count ===");
    let cnt: i64 = conn
        .query_row("SELECT COUNT(*) FROM lsx__Tags__Tags", [], |r| r.get(0))
        .unwrap();
    println!("  {cnt} tags in DB");

    // Check unique orphan values from FaceExpressions + Templates__Tag
    println!("\n=== Unique orphan tag UUIDs ===");
    let mut stmt = conn.prepare(
        "SELECT DISTINCT Object FROM (
           SELECT Object FROM lsx__FaceExpressions__TagsFilter WHERE Object NOT IN (SELECT UUID FROM lsx__Tags__Tags)
           UNION ALL
           SELECT Object FROM lsx__Templates__Tag WHERE Object NOT IN (SELECT UUID FROM lsx__Tags__Tags)
         ) ORDER BY Object"
    ).unwrap();
    let vals: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    println!("  {} distinct orphan values:", vals.len());
    for v in &vals {
        println!("    {v}");
    }

    // 7. VoiceMeta: check if those orphan voice UUIDs match VoiceMeta file names
    println!("\n=== Check orphan Voice UUIDs against VoiceMeta filenames ===");
    let voicemeta_dir = ws.join("UnpackedData/VoiceMeta/Localization/English/Soundbanks");
    if voicemeta_dir.is_dir() {
        let mut vm_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for entry in std::fs::read_dir(&voicemeta_dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".lsx") {
                // Convert hex-without-dashes to GUID format
                if stem.len() == 32 {
                    let guid = format!(
                        "{}-{}-{}-{}-{}",
                        &stem[0..8],
                        &stem[8..12],
                        &stem[12..16],
                        &stem[16..20],
                        &stem[20..32]
                    );
                    vm_ids.insert(guid);
                }
                vm_ids.insert(stem.to_string());
            }
        }
        println!("  VoiceMeta has {} soundbank files", vm_ids.len());

        // Check orphan Origin VoiceTableUUIDs against VoiceMeta
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT o.VoiceTableUUID FROM lsx__Origins__Origin o
             WHERE o.VoiceTableUUID IS NOT NULL
               AND o.VoiceTableUUID NOT IN (SELECT UUID FROM lsx__Voices__Voice)",
            )
            .unwrap();
        let orphans: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let mut in_vm = 0;
        let mut not_in_vm = 0;
        for o in &orphans {
            let normalized = o.replace('-', "");
            if vm_ids.contains(o) || vm_ids.contains(&normalized) {
                in_vm += 1;
                println!("  FOUND in VoiceMeta: {o}");
            } else {
                not_in_vm += 1;
            }
        }
        println!(
            "  Origin orphans in VoiceMeta: {} / {} total orphans",
            in_vm,
            orphans.len()
        );
        println!("  Origin orphans NOT in VoiceMeta: {not_in_vm}");

        // Same for CompanionPresets
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT cp.VoiceTableUUID FROM lsx__CompanionPresets__CompanionPreset cp
             WHERE cp.VoiceTableUUID IS NOT NULL
               AND cp.VoiceTableUUID NOT IN (SELECT UUID FROM lsx__Voices__Voice)",
            )
            .unwrap();
        let orphans: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let mut in_vm = 0;
        for o in &orphans {
            let normalized = o.replace('-', "");
            if vm_ids.contains(o) || vm_ids.contains(&normalized) {
                in_vm += 1;
                println!("  CP FOUND in VoiceMeta: {o}");
            }
        }
        println!(
            "  CompanionPresets orphans in VoiceMeta: {} / {} total orphans",
            in_vm,
            orphans.len()
        );
    } else {
        println!("  VoiceMeta dir not found at {voicemeta_dir:?}");
    }
}

#[test]
#[ignore]
fn colorpreset_parent_analysis() {
    let ws = workspace_root();
    let db_path = ws.join("reference_rust.sqlite");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    // 1. DDLs — how are these tables defined?
    for t in &[
        "lsx__CharacterVisualBank__ColorPreset",
        "lsx__CharacterVisualBank__Materials",
        "lsx__CharacterVisualBank__Resource",
    ] {
        let ddl: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name = ?1", [t], |r| {
                r.get(0)
            })
            .unwrap_or_else(|_| "NOT FOUND".into());
        println!(
            "\n--- {} ---\n{}",
            t,
            ddl.lines().take(8).collect::<Vec<_>>().join("\n")
        );
    }

    // 2. ColorPreset row counts and parent key analysis
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lsx__CharacterVisualBank__ColorPreset",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let null_parent: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lsx__CharacterVisualBank__ColorPreset WHERE _parent_key IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let in_resource: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lsx__CharacterVisualBank__ColorPreset cp
         WHERE cp._parent_key IN (SELECT ID FROM lsx__CharacterVisualBank__Resource)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let in_materials: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lsx__CharacterVisualBank__ColorPreset cp
         WHERE cp._parent_key IN (SELECT MapKey FROM lsx__CharacterVisualBank__Materials)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let orphan: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lsx__CharacterVisualBank__ColorPreset cp
         WHERE cp._parent_key IS NOT NULL
           AND cp._parent_key NOT IN (SELECT ID FROM lsx__CharacterVisualBank__Resource)
           AND cp._parent_key NOT IN (SELECT MapKey FROM lsx__CharacterVisualBank__Materials)",
            [],
            |r| r.get(0),
        )
        .unwrap();

    println!("\n=== ColorPreset _parent_key Analysis ===");
    println!("  Total rows:              {total}");
    println!("  NULL _parent_key:        {null_parent}");
    println!("  Matches Resource.ID:     {in_resource}");
    println!("  Matches Materials.MapKey:{in_materials}");
    println!("  True orphans:            {orphan}");
    println!(
        "  (Violations would be {} if FK → Resource.ID)",
        total - null_parent - in_resource
    );
    println!(
        "  (Violations would be {} if FK → Materials.MapKey)",
        total - null_parent - in_materials
    );

    // 3. Same analysis for MaterialOverrides
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lsx__CharacterVisualBank__MaterialOverrides",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let null_parent: i64 = conn.query_row(
        "SELECT COUNT(*) FROM lsx__CharacterVisualBank__MaterialOverrides WHERE _parent_key IS NULL",
        [], |r| r.get(0)
    ).unwrap();
    let in_resource: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lsx__CharacterVisualBank__MaterialOverrides mo
         WHERE mo._parent_key IN (SELECT ID FROM lsx__CharacterVisualBank__Resource)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let in_materials: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lsx__CharacterVisualBank__MaterialOverrides mo
         WHERE mo._parent_key IN (SELECT MapKey FROM lsx__CharacterVisualBank__Materials)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let orphan: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lsx__CharacterVisualBank__MaterialOverrides mo
         WHERE mo._parent_key IS NOT NULL
           AND mo._parent_key NOT IN (SELECT ID FROM lsx__CharacterVisualBank__Resource)
           AND mo._parent_key NOT IN (SELECT MapKey FROM lsx__CharacterVisualBank__Materials)",
            [],
            |r| r.get(0),
        )
        .unwrap();

    println!("\n=== MaterialOverrides _parent_key Analysis ===");
    println!("  Total rows:              {total}");
    println!("  NULL _parent_key:        {null_parent}");
    println!("  Matches Resource.ID:     {in_resource}");
    println!("  Matches Materials.MapKey:{in_materials}");
    println!("  True orphans:            {orphan}");
    println!(
        "  (Violations would be {} if FK → Resource.ID)",
        total - null_parent - in_resource
    );
    println!(
        "  (Violations would be {} if FK → Materials.MapKey)",
        total - null_parent - in_materials
    );

    // 4. FK definition check: what does the DB currently say?
    for t in &[
        "lsx__CharacterVisualBank__ColorPreset",
        "lsx__CharacterVisualBank__MaterialOverrides",
    ] {
        println!("\n=== FK list for {t} ===");
        let mut stmt = conn
            .prepare(&format!("PRAGMA foreign_key_list(\"{t}\")"))
            .unwrap();
        let fks: Vec<String> = stmt
            .query_map([], |row| {
                Ok(format!(
                    "  FK#{}: {} -> {}.{}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        for fk in &fks {
            println!("{fk}");
        }
    }
}

#[test]
#[ignore]
fn colorpreset_discovery_debug() {
    let ws = workspace_root();
    let unpacked = ws.join("UnpackedData");
    let files = collect_files(&unpacked);

    let schema = bg3_cmty_studio_lib::reference_db::discovery::discover_schema(&files, &unpacked)
        .expect("discovery failed");

    let targets = [
        "lsx__CharacterVisualBank__ColorPreset",
        "lsx__CharacterVisualBank__MaterialOverrides",
        "lsx__CharacterVisualBank__Materials",
        "lsx__CharacterVisualBank__Resource",
    ];
    for name in &targets {
        if let Some(ts) = schema.tables.get(*name) {
            println!("\n=== {name} ===");
            println!("  PK strategy:    {:?}", ts.pk_strategy);
            println!("  parent_tables:  {:?}", ts.parent_tables);
            println!("  parent_tables:   {:?}", ts.parent_tables);
        } else {
            println!("\n=== {name} === NOT FOUND");
        }
    }
}

fn collect_files(
    unpacked_path: &std::path::Path,
) -> Vec<bg3_cmty_studio_lib::reference_db::FileEntry> {
    use bg3_cmty_studio_lib::reference_db::{FileEntry, FileType, TargetDb};

    let target_dirs = &[
        "Shared/Public",
        "Gustav/Public",
        "GustavX/Public",
        "English",
    ];
    let file_exts = &[".lsx", ".txt", ".xml"];
    let mut files = Vec::new();

    for dir in target_dirs {
        let abs_dir = unpacked_path.join(dir);
        if !abs_dir.is_dir() {
            continue;
        }
        let mod_name = dir.split('/').next().unwrap_or("Unknown").to_string();
        for entry in walkdir::WalkDir::new(&abs_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{}", e.to_ascii_lowercase()));
            let ext = match ext {
                Some(e) if file_exts.contains(&e.as_str()) => e,
                _ => continue,
            };
            let rel_path = path
                .strip_prefix(unpacked_path)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let file_type = match ext.as_str() {
                ".xml" => FileType::Loca,
                ".txt" => FileType::Stats,
                _ => FileType::Lsx,
            };
            files.push(FileEntry::from_disk(
                path.to_path_buf(),
                rel_path,
                file_type,
                mod_name.clone(),
                TargetDb::Base,
                10,
            ));
        }
    }
    files
}
