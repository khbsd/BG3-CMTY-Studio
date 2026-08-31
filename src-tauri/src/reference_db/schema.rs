//! Schema types for discovered table structures.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// How a table's primary key is determined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PkStrategy {
    /// UUID TEXT PRIMARY KEY
    Uuid,
    /// MapKey TEXT PRIMARY KEY
    MapKey,
    /// ID TEXT PRIMARY KEY (bank resources)
    BankId,
    /// _entry_name TEXT PRIMARY KEY (stats tables)
    EntryName,
    /// contentuid TEXT PRIMARY KEY (loca)
    ContentUid,
    /// _row_id INTEGER PRIMARY KEY — for child/junction tables with no natural key
    Rowid,
}

impl PkStrategy {
    /// The column name used as PK.
    pub fn pk_column(&self) -> &'static str {
        match self {
            PkStrategy::Uuid => "UUID",
            PkStrategy::MapKey => "MapKey",
            PkStrategy::BankId => "ID",
            PkStrategy::EntryName => "_entry_name",
            PkStrategy::ContentUid => "contentuid",
            PkStrategy::Rowid => "_row_id",
        }
    }

    /// The SQL type for the PK column.
    pub fn pk_sql_type(&self) -> &'static str {
        match self {
            PkStrategy::Rowid => "INTEGER",
            _ => "TEXT",
        }
    }
}

/// A column discovered during Pass 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub bg3_type: String,
    pub sqlite_type: String,
}

/// An FK constraint to emit in CREATE TABLE.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FkConstraint {
    pub source_column: String,
    pub target_table: String,
    pub target_column: String,
}

/// Complete schema for one table, determined after Pass 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchema {
    pub table_name: String,
    pub pk_strategy: PkStrategy,
    /// Source type: "lsx", "stats", "loca", "txt"
    pub source_type: String,
    /// Region ID (for LSX tables)
    pub region_id: Option<String>,
    /// Node ID (for LSX tables)
    pub node_id: Option<String>,
    /// All data columns (excludes the PK and internal columns like _file_id)
    pub columns: Vec<ColumnDef>,
    /// Foreign key constraints
    pub fk_constraints: Vec<FkConstraint>,
    /// Whether this table needs _file_id
    pub has_file_id: bool,
    /// Set of parent table names (for metadata / _table_meta)
    pub parent_tables: HashSet<String>,
}

/// A junction table linking a parent row to a child row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JunctionDef {
    pub table_name: String,
    pub parent_table: String,
    pub parent_pk_column: String,
    pub parent_pk_type: String,
    pub child_table: String,
    pub child_pk_column: String,
    pub child_pk_type: String,
}

/// The full discovered schema for all tables.
#[derive(Debug, Serialize, Deserialize)]
pub struct DiscoveredSchema {
    /// All table schemas, keyed by table name.
    pub tables: HashMap<String, TableSchema>,
    /// Mapping from pre-consolidation name → post-consolidation name.
    /// Only entries where the name actually changed are included.
    pub renames: HashMap<String, String>,
    /// region_id → Set<node_id> (used for _table_meta recovery)
    pub region_node_ids: HashMap<String, HashSet<String>>,
    /// Junction tables for parent→child relationships.
    pub junction_tables: Vec<JunctionDef>,
    /// Fast lookup: parent_table → child_table → junction table name.
    /// Nested map avoids heap-allocating key pairs on every lookup.
    pub junction_lookup: HashMap<String, HashMap<String, String>>,
}

impl DiscoveredSchema {
    /// Resolve a table name, applying consolidation renames.
    pub fn resolve_table<'a>(&'a self, name: &'a str) -> Option<&'a str> {
        if self.tables.contains_key(name) {
            return Some(name);
        }
        // Try consolidated form: lsx__Region__Node → lsx__Region
        if let Some(renamed) = self.renames.get(name) {
            if self.tables.contains_key(renamed) {
                return Some(renamed.as_str());
            }
        }
        // Try reverse: given lsx__Region, find it as a renamed target
        // (multi-node form → consolidated)
        let parts: Vec<&str> = name.split("__").collect();
        if parts.len() >= 3 {
            // lsx__Region__Node → lsx__Region
            let consolidated = format!("{}__{}", parts[0], parts[1]);
            if self.tables.contains_key(&consolidated) {
                return Some(self.tables.get_key_value(&consolidated)?.0.as_str());
            }
        }
        None
    }
}
