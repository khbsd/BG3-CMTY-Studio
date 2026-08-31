//! Pass 1: Schema discovery — walk all files and collect table/column metadata.

use crate::models::{LsxNode, LsxNodeAttribute, LsxResource};
use crate::parsers::{lsf as lsf_parser, lsfx as lsfx_parser, lsx as lsx_parser};
use crate::reference_db::fk_patterns::{self, FkType};
use crate::reference_db::schema::*;
use crate::reference_db::types;
use crate::reference_db::{sanitize_id, FileEntry, FileType, SKIP_REGIONS};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::Path;
use std::sync::LazyLock;

static RE_NEW_ENTRY: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?i)^new\s+entry\s+""#).unwrap());

/// Raw column info collected per table during discovery.
struct RawTableInfo {
    source_type: String,
    region_id: Option<String>,
    node_id: Option<String>,
    /// col_name → bg3_type
    columns: HashMap<String, String>,
    /// Parent tables discovered
    parent_tables: HashSet<String>,
    /// Does this table have an _parent_key?
    has_parent: bool,
    /// Is this a root node of its region?
    is_root: bool,
}

impl RawTableInfo {
    fn new(source_type: &str) -> Self {
        Self {
            source_type: source_type.to_string(),
            region_id: None,
            node_id: None,
            columns: HashMap::new(),
            parent_tables: HashSet::new(),
            has_parent: false,
            is_root: false,
        }
    }
}

/// Per-file discovery result — produced in parallel, merged sequentially.
struct FileDiscovery {
    tables: HashMap<String, RawTableInfo>,
    region_node_ids: HashMap<String, HashSet<String>>,
}

/// Merge a per-file discovery result into the global accumulators.
fn merge_discovery(
    global_tables: &mut HashMap<String, RawTableInfo>,
    global_regions: &mut HashMap<String, HashSet<String>>,
    local: FileDiscovery,
) {
    for (name, info) in local.tables {
        let entry = global_tables
            .entry(name)
            .or_insert_with(|| RawTableInfo::new(&info.source_type));
        for (col, ty) in info.columns {
            entry.columns.entry(col).or_insert(ty);
        }
        for pt in info.parent_tables {
            entry.parent_tables.insert(pt);
        }
        if info.has_parent {
            entry.has_parent = true;
        }
        if info.is_root {
            entry.is_root = true;
        }
        if entry.region_id.is_none() {
            entry.region_id = info.region_id;
        }
        if entry.node_id.is_none() {
            entry.node_id = info.node_id;
        }
    }
    for (region, nodes) in local.region_node_ids {
        global_regions.entry(region).or_default().extend(nodes);
    }
}

/// Discover the schema from all files (Pass 1).
pub fn discover_schema(
    files: &[FileEntry],
    _unpacked_path: &Path,
) -> Result<DiscoveredSchema, String> {
    let mut raw_tables: HashMap<String, RawTableInfo> = HashMap::new();
    let mut region_node_ids: HashMap<String, HashSet<String>> = HashMap::new();

    // Parallel discovery: each file produces its own result, merged sequentially.
    // Loca, AllSpark, Effect, and stats-metadata have trivial fixed schemas —
    // discover once up front.
    discover_loca(&mut raw_tables);
    discover_allspark(&mut raw_tables);
    discover_effect(&mut raw_tables);
    discover_equipment(&mut raw_tables);
    discover_valuelists(&mut raw_tables);
    discover_modifiers(&mut raw_tables);

    let per_file_results: Vec<FileDiscovery> = files
        .par_iter()
        .filter_map(|file| match file.file_type {
            FileType::Lsx => Some(discover_lsx(file)),
            FileType::Stats => Some(discover_stats(file)),
            FileType::Loca | FileType::AllSpark | FileType::Effect => None, // fixed schemas
        })
        .collect();

    for result in per_file_results {
        merge_discovery(&mut raw_tables, &mut region_node_ids, result);
    }

    // Determine consolidation: single-node regions get shortened names
    let mut renames: HashMap<String, String> = HashMap::new();
    for (region_id, node_ids) in &region_node_ids {
        if node_ids.len() == 1 {
            let node_id = node_ids.iter().next().unwrap();
            let old_name = format!("lsx__{}__{}", sanitize_id(region_id), sanitize_id(node_id));
            let new_name = format!("lsx__{}", sanitize_id(region_id));
            if old_name != new_name && raw_tables.contains_key(&old_name) {
                // Rename the raw table
                if let Some(mut info) = raw_tables.remove(&old_name) {
                    info.region_id = Some(region_id.clone());
                    info.node_id = Some(node_id.clone());
                    raw_tables.insert(new_name.clone(), info);
                    renames.insert(old_name, new_name);
                }
            }
        }
    }

    // Apply renames to parent_tables references
    for info in raw_tables.values_mut() {
        let new_parents: HashSet<String> = info
            .parent_tables
            .iter()
            .map(|p| renames.get(p).cloned().unwrap_or_else(|| p.clone()))
            .collect();
        info.parent_tables = new_parents;
    }

    // Collect root table names for PK strategy decisions
    let root_tables: HashSet<&str> = raw_tables
        .iter()
        .filter(|(_, info)| info.is_root)
        .map(|(name, _)| name.as_str())
        .collect();

    // Build final TableSchema for each table
    let mut tables: HashMap<String, TableSchema> = HashMap::new();

    for (table_name, info) in &raw_tables {
        let parent_is_root = info
            .parent_tables
            .iter()
            .any(|p| root_tables.contains(p.as_str()));
        let pk_strategy =
            determine_pk_strategy(table_name, &info.columns, info.has_parent, parent_is_root);

        let mut data_columns: Vec<ColumnDef> = Vec::new();
        let pk_col = pk_strategy.pk_column();

        for (col_name, bg3_type) in &info.columns {
            // Skip PK column from data columns — it'll be in CREATE TABLE PK clause
            if col_name == pk_col && pk_strategy != PkStrategy::Rowid {
                continue;
            }
            data_columns.push(ColumnDef {
                name: col_name.clone(),
                bg3_type: bg3_type.clone(),
                sqlite_type: types::sqlite_type(bg3_type).to_string(),
            });
        }

        // Sort columns for deterministic DDL
        data_columns.sort_by(|a, b| a.name.cmp(&b.name));

        // Deduplicate case-insensitive collisions (SQLite column names are case-insensitive).
        // e.g. "Automated" vs "automated" — keep the first (capitalized) variant.
        {
            let mut seen = HashSet::new();
            data_columns.retain(|col| seen.insert(col.name.to_ascii_lowercase()));
        }

        // Compute FK constraints
        let fk_constraints = compute_fk_constraints(
            table_name,
            &info.columns,
            info.node_id.as_deref(),
            &raw_tables,
            &renames,
        );

        tables.insert(
            table_name.clone(),
            TableSchema {
                table_name: table_name.clone(),
                pk_strategy,
                source_type: info.source_type.clone(),
                region_id: info.region_id.clone(),
                node_id: info.node_id.clone(),
                columns: data_columns,
                fk_constraints,
                has_file_id: true,
                parent_tables: info.parent_tables.clone(),
            },
        );
    }

    // Generate junction tables for every observed parent→child relationship.
    // Each (parent_table, child_table) pair gets its own junction table with
    // proper FK constraints to both sides.
    let mut junction_tables: Vec<JunctionDef> = Vec::new();
    for (child_name, child_schema) in &tables {
        if child_schema.parent_tables.is_empty() {
            continue;
        }
        for parent_name in &child_schema.parent_tables {
            if let Some(parent_schema) = tables.get(parent_name) {
                let child_node = child_schema
                    .node_id
                    .as_deref()
                    .unwrap_or(child_name.as_str());
                let jct_name = format!("{}__to__{}", parent_name, sanitize_id(child_node),);
                junction_tables.push(JunctionDef {
                    table_name: jct_name,
                    parent_table: parent_name.clone(),
                    parent_pk_column: parent_schema.pk_strategy.pk_column().to_string(),
                    parent_pk_type: parent_schema.pk_strategy.pk_sql_type().to_string(),
                    child_table: child_name.clone(),
                    child_pk_column: child_schema.pk_strategy.pk_column().to_string(),
                    child_pk_type: child_schema.pk_strategy.pk_sql_type().to_string(),
                });
            }
        }
    }
    junction_tables.sort_by(|a, b| a.table_name.cmp(&b.table_name));

    let mut junction_lookup: HashMap<String, HashMap<String, String>> = HashMap::new();
    for jt in &junction_tables {
        junction_lookup
            .entry(jt.parent_table.clone())
            .or_default()
            .insert(jt.child_table.clone(), jt.table_name.clone());
    }

    Ok(DiscoveredSchema {
        tables,
        renames,
        region_node_ids,
        junction_tables,
        junction_lookup,
    })
}

/// Determine the PK strategy for a table based on its discovered columns.
///
/// `has_parent` / `parent_is_root` allow us to detect deep-hierarchy children
/// (e.g. Templates__Item, Templates__Skill) whose UUID column is unreliable.
/// For these, we fall back to Rowid.  MapKey and BankId remain valid even for
/// deep children because they are explicit key attributes.
fn determine_pk_strategy(
    table_name: &str,
    columns: &HashMap<String, String>,
    has_parent: bool,
    parent_is_root: bool,
) -> PkStrategy {
    if table_name == "loca__english" {
        return PkStrategy::ContentUid;
    }
    if table_name.starts_with("stats__") {
        return PkStrategy::EntryName;
    }
    // MapKey takes priority over UUID — GameObjects (and similar) have both
    // but UUID is always blank; MapKey is the real identifier.
    if columns.contains_key("MapKey") {
        return PkStrategy::MapKey;
    }

    // UUID is only a reliable PK for direct children of region root nodes
    // (e.g. Backgrounds__Background whose parent is Backgrounds__Backgrounds).
    // Deep-hierarchy children like Item, Skill, TargetConditions often lack
    // UUID or have empty strings.  Downgrade to Rowid for those.
    let is_deep_child = has_parent && !parent_is_root;
    if columns.contains_key("UUID") && !is_deep_child {
        return PkStrategy::Uuid;
    }

    if columns.contains_key("ID") {
        // Bank-style resources have ID as PK
        return PkStrategy::BankId;
    }
    PkStrategy::Rowid
}

/// Compute FK constraints for a table.
fn compute_fk_constraints(
    table_name: &str,
    columns: &HashMap<String, String>,
    node_id: Option<&str>,
    all_tables: &HashMap<String, RawTableInfo>,
    renames: &HashMap<String, String>,
) -> Vec<FkConstraint> {
    let mut fks = Vec::new();

    // Helper to resolve a table name (check exists after renames)
    let resolve = |name: &str| -> Option<String> {
        let n = renames
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string());
        if all_tables.contains_key(&n) {
            return Some(n);
        }
        // Try consolidated form
        let parts: Vec<&str> = name.split("__").collect();
        if parts.len() >= 3 {
            let consolidated = format!("{}__{}", parts[0], parts[1]);
            let consolidated = renames.get(&consolidated).cloned().unwrap_or(consolidated);
            if all_tables.contains_key(&consolidated) {
                return Some(consolidated);
            }
        }
        None
    };

    // Check table-specific FKs first (highest priority)
    for col_name in columns.keys() {
        if let Some((tgt_tbl, tgt_col)) = fk_patterns::table_specific_fk(table_name, col_name) {
            if let Some(resolved) = resolve(tgt_tbl) {
                fks.push(FkConstraint {
                    source_column: col_name.clone(),
                    target_table: resolved,
                    target_column: tgt_col.to_string(),
                });
                continue;
            }
        }
    }

    // Already-handled columns from table-specific
    let specific_cols: HashSet<String> = fks.iter().map(|f| f.source_column.clone()).collect();

    // Object column FKs (node_id-based)
    if columns.contains_key("Object")
        && columns.get("Object").map(|t| t.as_str()) == Some("guid")
        && !specific_cols.contains("Object")
    {
        if let Some(nid) = node_id {
            if let Some((tgt_tbl, tgt_col)) = fk_patterns::object_fk_for_node(nid) {
                if let Some(resolved) = resolve(tgt_tbl) {
                    fks.push(FkConstraint {
                        source_column: "Object".to_string(),
                        target_table: resolved,
                        target_column: tgt_col.to_string(),
                    });
                }
            }
        }
    }

    // Column-name pattern FKs
    for (col_name, bg3_type) in columns {
        if specific_cols.contains(col_name) {
            continue;
        }
        if fks.iter().any(|f| f.source_column == *col_name) {
            continue;
        }
        // Only apply to guid/FixedString columns
        if bg3_type != "guid" && bg3_type != "FixedString" {
            continue;
        }

        if let Some((tgt_tbl, tgt_col, fk_type)) = fk_patterns::match_fk_pattern(col_name, bg3_type)
        {
            match fk_type {
                FkType::SelfPk | FkType::UuidUnresolved | FkType::TemplateUnresolved => {
                    // No FK constraint emitted
                }
                FkType::UuidSelf => {
                    // Self-referencing FK
                    if let Some(tgt_col) = tgt_col {
                        fks.push(FkConstraint {
                            source_column: col_name.clone(),
                            target_table: table_name.to_string(),
                            target_column: tgt_col.to_string(),
                        });
                    }
                }
                FkType::Uuid | FkType::Name => {
                    if let Some(tgt_tbl) = tgt_tbl {
                        if let Some(resolved) = resolve(tgt_tbl) {
                            if let Some(tgt_col) = tgt_col {
                                fks.push(FkConstraint {
                                    source_column: col_name.clone(),
                                    target_table: resolved,
                                    target_column: tgt_col.to_string(),
                                });
                            }
                        }
                    }
                }
                FkType::ContentUid | FkType::ContentUidVersioned => {
                    // These are loca contentuid FKs — handled separately
                }
            }
        }
    }

    // TranslatedString columns → loca FK
    for (col_name, bg3_type) in columns {
        if bg3_type == "TranslatedString" && resolve("loca__english").is_some() {
            fks.push(FkConstraint {
                source_column: col_name.clone(),
                target_table: "loca__english".to_string(),
                target_column: "contentuid".to_string(),
            });
        }
    }

    // Stats _using → self inheritance
    if table_name.starts_with("stats__") && columns.contains_key("_using") {
        fks.push(FkConstraint {
            source_column: "_using".to_string(),
            target_table: table_name.to_string(),
            target_column: "_entry_name".to_string(),
        });
    }

    // Stats contentuid-versioned columns → loca
    if table_name.starts_with("stats__") {
        for col_name in columns.keys() {
            if is_stats_loca_column(col_name)
                && resolve("loca__english").is_some()
                && !fks.iter().any(|f| f.source_column == *col_name)
            {
                fks.push(FkConstraint {
                    source_column: col_name.clone(),
                    target_table: "loca__english".to_string(),
                    target_column: "contentuid".to_string(),
                });
            }
        }
    }

    fks
}

/// Check whether a stats column name is a loca content-uid FK column.
/// These store `contentuid;version` and need splitting at insert time.
pub fn is_stats_loca_column(col_name: &str) -> bool {
    let lower = col_name.to_ascii_lowercase();
    // Exclude companion _version columns generated by splitting
    if lower.ends_with("_version") {
        return false;
    }
    (lower.contains("displayname")
        || lower.contains("description")
        || lower.contains("extradescription")
        || lower.contains("loredescription")
        || lower.contains("shortdescription")
        || lower.contains("tooltipupcastdescription"))
        && !lower.contains("params")
}

// ---------------------------------------------------------------------------
// LSX discovery
// ---------------------------------------------------------------------------

/// Discover schema from an LSX/LSF file using the shared resource model.
fn discover_lsx(file: &FileEntry) -> FileDiscovery {
    let mut raw_tables: HashMap<String, RawTableInfo> = HashMap::new();
    let mut region_node_ids: HashMap<String, HashSet<String>> = HashMap::new();

    let resource = match parse_lsx_resource_file(file) {
        Ok(resource) => resource,
        Err(_) => {
            return FileDiscovery {
                tables: raw_tables,
                region_node_ids,
            }
        }
    };
    if resource.regions.is_empty() {
        return FileDiscovery {
            tables: raw_tables,
            region_node_ids,
        };
    }

    for region in &resource.regions {
        if SKIP_REGIONS.contains(&region.id.as_str()) {
            continue;
        }

        for node in &region.nodes {
            discover_resource_node(
                &mut raw_tables,
                &mut region_node_ids,
                &region.id,
                node,
                None,
                true,
            );
        }
    }

    FileDiscovery {
        tables: raw_tables,
        region_node_ids,
    }
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
            let content = if let Some(bytes) = file.in_memory_bytes() {
                String::from_utf8(bytes.to_vec()).map_err(|e| format!("UTF-8 decode error: {e}"))?
            } else {
                std::fs::read_to_string(&file.abs_path).map_err(|e| format!("Read error: {e}"))?
            };
            if content.trim().is_empty() {
                return Ok(LsxResource {
                    regions: Vec::new(),
                });
            }
            lsx_parser::parse_lsx_resource(&content)
        }
    }
}

fn discover_resource_node(
    raw_tables: &mut HashMap<String, RawTableInfo>,
    region_node_ids: &mut HashMap<String, HashSet<String>>,
    region_id: &str,
    node: &LsxNode,
    parent_table: Option<String>,
    is_root: bool,
) {
    let table_name = format!("lsx__{}__{}", sanitize_id(region_id), sanitize_id(&node.id));

    region_node_ids
        .entry(region_id.to_string())
        .or_default()
        .insert(node.id.clone());

    let info = raw_tables.entry(table_name.clone()).or_insert_with(|| {
        let mut info = RawTableInfo::new("lsx");
        info.region_id = Some(region_id.to_string());
        info.node_id = Some(node.id.clone());
        info
    });

    if is_root {
        info.is_root = true;
    }
    if let Some(parent) = &parent_table {
        info.parent_tables.insert(parent.clone());
        info.has_parent = true;
    }

    for attr in &node.attributes {
        collect_resource_columns(info, attr);
    }

    for child in &node.children {
        discover_resource_node(
            raw_tables,
            region_node_ids,
            region_id,
            child,
            Some(table_name.clone()),
            false,
        );
    }
}

fn collect_resource_columns(info: &mut RawTableInfo, attr: &LsxNodeAttribute) {
    if attr.id.is_empty() || attr.id == "_OriginalFileVersion_" {
        return;
    }

    let should_store_value = attr.handle.is_none()
        || attr.attr_type != "TranslatedString"
        || attr.value != attr.handle.as_deref().unwrap_or("");
    if should_store_value {
        info.columns
            .entry(attr.id.clone())
            .or_insert_with(|| attr.attr_type.clone());
    }

    if attr.handle.is_some() {
        let col_name = if attr.attr_type == "TranslatedString" {
            attr.id.clone()
        } else {
            format!("{}_handle", attr.id)
        };
        info.columns.entry(col_name).or_insert_with(|| {
            if attr.attr_type.is_empty() {
                "TranslatedString".to_string()
            } else {
                attr.attr_type.clone()
            }
        });
    }

    if attr.attr_type == "TranslatedString" && attr.version.is_some() {
        info.columns
            .entry(format!("{}_version", attr.id))
            .or_insert_with(|| "int32".to_string());
    }
}
// Stats discovery
// ---------------------------------------------------------------------------

fn discover_stats(file: &FileEntry) -> FileDiscovery {
    let mut raw_tables: HashMap<String, RawTableInfo> = HashMap::new();

    let content = if let Some(bytes) = file.in_memory_bytes() {
        match String::from_utf8(bytes.to_vec()) {
            Ok(c) => c,
            Err(_) => {
                return FileDiscovery {
                    tables: raw_tables,
                    region_node_ids: HashMap::new(),
                }
            }
        }
    } else {
        match std::fs::read_to_string(&file.abs_path) {
            Ok(c) => c,
            Err(_) => {
                return FileDiscovery {
                    tables: raw_tables,
                    region_node_ids: HashMap::new(),
                }
            }
        }
    };

    let mut current_type: Option<String> = None;
    let mut has_entries = false;

    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }

        if let Some(_caps) = RE_NEW_ENTRY.find(t) {
            has_entries = true;
        } else if let Some(rest) = t.strip_prefix("type \"") {
            if let Some(end) = rest.find('"') {
                current_type = Some(rest[..end].to_string());
            }
        } else if let Some(rest) = t.strip_prefix("data \"") {
            if let Some(end) = rest.find('"') {
                let field = &rest[..end];
                if let Some(ref stype) = current_type {
                    let table_name = format!("stats__{}", sanitize_id(stype));
                    let info = raw_tables.entry(table_name).or_insert_with(|| {
                        let mut i = RawTableInfo::new("stats");
                        i.columns
                            .insert("_entry_name".to_string(), "FixedString".to_string());
                        i.columns
                            .insert("_type".to_string(), "FixedString".to_string());
                        i.columns
                            .insert("_using".to_string(), "FixedString".to_string());
                        i
                    });
                    info.columns
                        .entry(field.to_string())
                        .or_insert_with(|| "FixedString".to_string());
                    // Stats loca FK columns store "contentuid;version" —
                    // add a companion version column.
                    if is_stats_loca_column(field) {
                        info.columns
                            .entry(format!("{field}_version"))
                            .or_insert_with(|| "int32".to_string());
                    }
                }
            }
        }
    }

    // Raw text fallback — only when the standard format found nothing AND
    // the non-standard format also couldn't discover a schema.
    if !has_entries && !content.trim().is_empty() {
        // Try non-standard stat formats (CSV entries, key-value, groups)
        let file_stem = file
            .abs_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if !file_stem.is_empty() {
            discover_nonstandard_stats(&content, file_stem, &mut raw_tables);
        }

        // If non-standard discovery didn't create any table, fall back to raw text
        if raw_tables.is_empty() {
            raw_tables.entry("txt__raw".to_string()).or_insert_with(|| {
                let mut i = RawTableInfo::new("txt");
                i.columns.insert("content".to_string(), "TEXT".to_string());
                i
            });
        }
    }

    FileDiscovery {
        tables: raw_tables,
        region_node_ids: HashMap::new(),
    }
}

/// Parse comma-separated values (quoted and unquoted) from a CSV-style line.
pub fn parse_csv_values(input: &str) -> Vec<String> {
    let mut values = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'"' {
            i += 1; // skip opening quote
            let start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            values.push(input[start..i].to_string());
            if i < bytes.len() {
                i += 1;
            } // skip closing quote
              // Skip comma/whitespace
            while i < bytes.len() && (bytes[i] == b',' || bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
        } else if bytes[i] == b',' {
            // Empty value between commas
            values.push(String::new());
            i += 1;
        } else {
            // Unquoted value
            let start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            values.push(input[start..i].trim().to_string());
            if i < bytes.len() {
                i += 1;
            } // skip comma
        }
    }
    values
}

/// Discover schema for non-standard stats formats.
///
/// Handles three format families:
/// - **CSV entries**: `new <keyword> "name","val1","val2",...` (BloodTypes, ItemColor)
/// - **Key-value**: `key "name","value"` (XPData)
/// - **Groups**: `new <keyword> "name"` + `add <sub> ...` (ItemProgressionNames/Visuals)
fn discover_nonstandard_stats(
    content: &str,
    file_stem: &str,
    raw_tables: &mut HashMap<String, RawTableInfo>,
) {
    let table_name = format!("stats__{}", sanitize_id(file_stem));
    let mut has_key_format = false;
    let mut has_new_keyword = false;
    let mut max_csv_params = 0usize;
    let mut add_keywords: HashSet<String> = HashSet::new();

    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }

        if (t.starts_with("key ") || t.starts_with("key\t")) && t.contains('"') {
            has_key_format = true;
        } else if t.starts_with("new ")
            && !t.to_ascii_lowercase().starts_with("new entry ")
            && !t.to_ascii_lowercase().starts_with("new equipment ")
        {
            has_new_keyword = true;
            // Extract CSV values after the keyword to count params
            if let Some(q) = t.find('"') {
                let csv_part = &t[q..];
                let params = parse_csv_values(csv_part);
                if params.len() > max_csv_params {
                    max_csv_params = params.len();
                }
            }
        } else if let Some(rest) = t.strip_prefix("add ") {
            // Track sub-entry keywords for group formats
            if let Some(kw) = rest
                .split(|c: char| c.is_whitespace() || c == '"')
                .find(|s| !s.is_empty())
            {
                add_keywords.insert(kw.to_string());
            }
        }
    }

    if !has_key_format && !has_new_keyword {
        return;
    }

    let info = raw_tables.entry(table_name).or_insert_with(|| {
        let mut i = RawTableInfo::new("stats");
        i.columns
            .insert("_entry_name".to_string(), "FixedString".to_string());
        i.columns
            .insert("_type".to_string(), "FixedString".to_string());
        i.columns
            .insert("_using".to_string(), "FixedString".to_string());
        i
    });

    if has_key_format {
        info.columns
            .entry("Value".to_string())
            .or_insert_with(|| "FixedString".to_string());
    }

    if has_new_keyword {
        // Positional params (index 0 = name stored in _entry_name, 1..N = Param1..ParamN)
        for idx in 1..max_csv_params {
            info.columns
                .entry(format!("Param{idx}"))
                .or_insert_with(|| "FixedString".to_string());
        }

        // Group sub-entry keywords become columns (e.g. "name" → "Name", "levelgroup" → "Levelgroup")
        for kw in &add_keywords {
            let col = capitalize_first(kw);
            info.columns
                .entry(col)
                .or_insert_with(|| "FixedString".to_string());
        }
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

// ---------------------------------------------------------------------------
// Loca discovery (fixed schema)
// ---------------------------------------------------------------------------

fn discover_loca(raw_tables: &mut HashMap<String, RawTableInfo>) {
    raw_tables
        .entry("loca__english".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("loca");
            i.columns
                .insert("contentuid".to_string(), "FixedString".to_string());
            i.columns.insert("version".to_string(), "int32".to_string());
            i.columns.insert("text".to_string(), "LSString".to_string());
            i
        });
}

/// Fixed schema for AllSpark component/module definition files (XCD/XMD).
fn discover_allspark(raw_tables: &mut HashMap<String, RawTableInfo>) {
    // Components (one row per component type: BoundingSphere, ParticleSystem, etc.)
    raw_tables
        .entry("allspark__components".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("allspark");
            i.columns
                .insert("name".to_string(), "FixedString".to_string());
            i.columns
                .insert("tooltip".to_string(), "LSString".to_string());
            i.columns
                .insert("color".to_string(), "FixedString".to_string());
            i
        });

    // Properties (one row per property definition, global across components)
    raw_tables
        .entry("allspark__properties".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("allspark");
            i.columns
                .insert("guid".to_string(), "FixedString".to_string());
            i.columns
                .insert("component_name".to_string(), "FixedString".to_string());
            i.columns
                .insert("name".to_string(), "FixedString".to_string());
            i.columns
                .insert("type_name".to_string(), "FixedString".to_string());
            i.columns
                .insert("specializable".to_string(), "bool".to_string());
            i.columns
                .insert("tooltip".to_string(), "LSString".to_string());
            i.columns
                .insert("default_value".to_string(), "LSString".to_string());
            i
        });

    // Property groups (logical groupings of properties within a component)
    raw_tables
        .entry("allspark__property_groups".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("allspark");
            i.columns
                .insert("guid".to_string(), "FixedString".to_string());
            i.columns
                .insert("component_name".to_string(), "FixedString".to_string());
            i.columns
                .insert("name".to_string(), "FixedString".to_string());
            i.columns
                .insert("collapsed".to_string(), "FixedString".to_string());
            i.columns
                .insert("sort_order".to_string(), "int32".to_string());
            i
        });

    // Property group → property membership (ordered)
    raw_tables
        .entry("allspark__property_group_refs".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("allspark");
            i.columns
                .insert("group_guid".to_string(), "FixedString".to_string());
            i.columns
                .insert("component_name".to_string(), "FixedString".to_string());
            i.columns
                .insert("property_guid".to_string(), "FixedString".to_string());
            i.columns
                .insert("sort_order".to_string(), "int32".to_string());
            i
        });

    // Modules (one row per module type)
    raw_tables
        .entry("allspark__modules".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("allspark");
            i.columns
                .insert("guid".to_string(), "FixedString".to_string());
            i.columns
                .insert("name".to_string(), "FixedString".to_string());
            i
        });

    // Module → property membership
    raw_tables
        .entry("allspark__module_properties".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("allspark");
            i.columns
                .insert("module_guid".to_string(), "FixedString".to_string());
            i.columns
                .insert("property_guid".to_string(), "FixedString".to_string());
            i.columns
                .insert("property_name".to_string(), "FixedString".to_string());
            i
        });
}

/// Fixed schema for toolkit effect files (.lsefx).
fn discover_effect(raw_tables: &mut HashMap<String, RawTableInfo>) {
    // Top-level effect resource (one row per .lsefx file)
    raw_tables
        .entry("effect__effects".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("effect");
            i.columns
                .insert("id".to_string(), "FixedString".to_string());
            i.columns
                .insert("version".to_string(), "FixedString".to_string());
            i.columns
                .insert("effect_version".to_string(), "FixedString".to_string());
            i.columns
                .insert("phases_xml".to_string(), "LSString".to_string());
            i.columns
                .insert("colors_xml".to_string(), "LSString".to_string());
            i.columns
                .insert("source_file".to_string(), "FixedString".to_string());
            i
        });

    // Effect components (one row per component instance)
    raw_tables
        .entry("effect__components".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("effect");
            i.columns
                .insert("instance_name".to_string(), "FixedString".to_string());
            i.columns
                .insert("class_name".to_string(), "FixedString".to_string());
            i.columns
                .insert("start".to_string(), "FixedString".to_string());
            i.columns
                .insert("end".to_string(), "FixedString".to_string());
            i.columns
                .insert("track_name".to_string(), "FixedString".to_string());
            i.columns
                .insert("track_group_name".to_string(), "FixedString".to_string());
            i.columns
                .insert("effect_id".to_string(), "FixedString".to_string());
            i.columns
                .insert("source_file".to_string(), "FixedString".to_string());
            i
        });

    // Per-component properties (GUID-identified)
    raw_tables
        .entry("effect__properties".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("effect");
            i.columns
                .insert("property_guid".to_string(), "FixedString".to_string());
            i.columns
                .insert("component_instance".to_string(), "FixedString".to_string());
            i.columns
                .insert("effect_id".to_string(), "FixedString".to_string());
            i.columns
                .insert("source_file".to_string(), "FixedString".to_string());
            i
        });

    // Property data (platform/lod/value triples)
    raw_tables
        .entry("effect__datums".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("effect");
            i.columns
                .insert("property_guid".to_string(), "FixedString".to_string());
            i.columns
                .insert("component_instance".to_string(), "FixedString".to_string());
            i.columns
                .insert("platform".to_string(), "FixedString".to_string());
            i.columns
                .insert("lod".to_string(), "FixedString".to_string());
            i.columns
                .insert("value".to_string(), "LSString".to_string());
            i.columns
                .insert("effect_id".to_string(), "FixedString".to_string());
            i.columns
                .insert("source_file".to_string(), "FixedString".to_string());
            i
        });
}

// ---------------------------------------------------------------------------
// Equipment / ValueLists / Modifiers discovery (fixed schemas)
// ---------------------------------------------------------------------------

/// Fixed schema for `new equipment "..."` blocks in Equipment.txt.
fn discover_equipment(raw_tables: &mut HashMap<String, RawTableInfo>) {
    raw_tables
        .entry("stats__equipment".to_string())
        .or_insert_with(|| {
            // source_type "equipment" keeps it out of generic stats queries
            let mut i = RawTableInfo::new("equipment");
            // PK will be _entry_name (automatic for stats__ prefix).
            i.columns
                .insert("_entry_name".to_string(), "FixedString".to_string());
            i
        });
}

/// Fixed schema for `valuelist "..." / value "..." : N` blocks in ValueLists.txt.
/// Uses Rowid PK since each row is a single (list_key, value_name, value_index) triple.
fn discover_valuelists(raw_tables: &mut HashMap<String, RawTableInfo>) {
    raw_tables
        .entry("valuelist_entries".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("valuelist");
            i.columns
                .insert("list_key".to_string(), "FixedString".to_string());
            i.columns
                .insert("value_name".to_string(), "FixedString".to_string());
            i.columns
                .insert("value_index".to_string(), "int32".to_string());
            i
        });
}

/// Fixed schema for `modifier type "..." / modifier "field","type"` blocks in Modifiers.txt.
/// Uses Rowid PK since each row is a single (type_name, field_name, field_type) triple.
fn discover_modifiers(raw_tables: &mut HashMap<String, RawTableInfo>) {
    raw_tables
        .entry("modifier_definitions".to_string())
        .or_insert_with(|| {
            let mut i = RawTableInfo::new("modifier");
            i.columns
                .insert("type_name".to_string(), "FixedString".to_string());
            i.columns
                .insert("field_name".to_string(), "FixedString".to_string());
            i.columns
                .insert("field_type".to_string(), "FixedString".to_string());
            i
        });
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_loca_creates_expected_table() {
        let mut tables = HashMap::new();
        discover_loca(&mut tables);

        assert!(
            tables.contains_key("loca__english"),
            "loca__english table should exist"
        );
        let info = &tables["loca__english"];
        assert!(info.columns.contains_key("contentuid"));
        assert!(info.columns.contains_key("text"));
        assert!(info.columns.contains_key("version"));
    }

    #[test]
    fn discover_allspark_creates_expected_tables() {
        let mut tables = HashMap::new();
        discover_allspark(&mut tables);

        assert!(
            tables.contains_key("allspark__components"),
            "allspark__components table should exist"
        );
        assert!(
            tables.contains_key("allspark__properties"),
            "allspark__properties table should exist"
        );
        assert!(
            tables.contains_key("allspark__modules"),
            "allspark__modules table should exist"
        );
        let info = &tables["allspark__components"];
        assert!(info.columns.contains_key("name"));
        assert!(info.columns.contains_key("tooltip"));
    }

    #[test]
    fn discover_effect_creates_expected_tables() {
        let mut tables = HashMap::new();
        discover_effect(&mut tables);

        assert!(
            tables.contains_key("effect__effects"),
            "effect__effects table should exist"
        );
        assert!(
            tables.contains_key("effect__components"),
            "effect__components table should exist"
        );
        assert!(
            tables.contains_key("effect__properties"),
            "effect__properties table should exist"
        );
        assert!(
            tables.contains_key("effect__datums"),
            "effect__datums table should exist"
        );
        let info = &tables["effect__components"];
        assert!(info.columns.contains_key("instance_name"));
        assert!(info.columns.contains_key("class_name"));
    }

    #[test]
    fn discover_equipment_creates_expected_table() {
        let mut tables = HashMap::new();
        discover_equipment(&mut tables);

        assert!(
            tables.contains_key("stats__equipment"),
            "stats__equipment table should exist"
        );
        let info = &tables["stats__equipment"];
        assert!(info.columns.contains_key("_entry_name"));
        assert_eq!(info.source_type, "equipment");
    }

    #[test]
    fn discover_valuelists_creates_expected_table() {
        let mut tables = HashMap::new();
        discover_valuelists(&mut tables);

        assert!(
            tables.contains_key("valuelist_entries"),
            "valuelist_entries table should exist"
        );
        let info = &tables["valuelist_entries"];
        assert!(info.columns.contains_key("list_key"));
        assert!(info.columns.contains_key("value_name"));
        assert!(info.columns.contains_key("value_index"));
    }

    #[test]
    fn discover_modifiers_creates_expected_table() {
        let mut tables = HashMap::new();
        discover_modifiers(&mut tables);

        assert!(
            tables.contains_key("modifier_definitions"),
            "modifier_definitions table should exist"
        );
        let info = &tables["modifier_definitions"];
        assert!(info.columns.contains_key("type_name"));
        assert!(info.columns.contains_key("field_name"));
        assert!(info.columns.contains_key("field_type"));
    }

    #[test]
    fn is_stats_loca_column_recognizes_known_columns() {
        assert!(is_stats_loca_column("DisplayName"));
        assert!(is_stats_loca_column("Description"));
        assert!(is_stats_loca_column("ExtraDescription"));
        assert!(is_stats_loca_column("ShortDescription"));
        assert!(is_stats_loca_column("LoreDescription"));
        assert!(is_stats_loca_column("TooltipUpcastDescription"));
    }

    #[test]
    fn is_stats_loca_column_rejects_non_loca() {
        assert!(!is_stats_loca_column("UUID"));
        assert!(!is_stats_loca_column("Level"));
        assert!(!is_stats_loca_column("SpellType"));
        assert!(!is_stats_loca_column(""));
        // _version suffix columns are excluded
        assert!(!is_stats_loca_column("DisplayName_version"));
        // Params columns are excluded
        assert!(!is_stats_loca_column("DescriptionParams"));
    }

    #[test]
    fn merge_discovery_combines_columns() {
        let mut global_tables = HashMap::new();
        let mut global_regions = HashMap::new();

        let mut local = FileDiscovery {
            tables: HashMap::new(),
            region_node_ids: HashMap::new(),
        };
        let mut info = RawTableInfo::new("lsx");
        info.columns.insert("UUID".to_string(), "guid".to_string());
        info.columns
            .insert("Name".to_string(), "FixedString".to_string());
        local.tables.insert("test_table".to_string(), info);

        merge_discovery(&mut global_tables, &mut global_regions, local);

        assert!(global_tables.contains_key("test_table"));
        assert_eq!(global_tables["test_table"].columns.len(), 2);

        // Merge a second discovery that adds a new column
        let mut local2 = FileDiscovery {
            tables: HashMap::new(),
            region_node_ids: HashMap::new(),
        };
        let mut info2 = RawTableInfo::new("lsx");
        info2
            .columns
            .insert("Level".to_string(), "int32".to_string());
        local2.tables.insert("test_table".to_string(), info2);

        merge_discovery(&mut global_tables, &mut global_regions, local2);

        assert_eq!(global_tables["test_table"].columns.len(), 3);
        assert!(global_tables["test_table"].columns.contains_key("Level"));
    }
}
