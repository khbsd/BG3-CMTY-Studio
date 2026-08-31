use crate::commands::diff::{diff_section, diff_stats};
use crate::models::*;
use crate::reference_db::queries;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Cached parsed entries for a mod, keyed by mod path.
pub struct ParsedModEntries {
    pub lsx_entries: HashMap<Section, Vec<LsxEntry>>,
    pub stats_entries: Vec<StatsEntry>,
    pub source_files: HashMap<Section, String>,
}

/// Global cache of parsed mod entries. Uses Arc for O(1) cloning when reading.
static MOD_ENTRY_CACHE: std::sync::LazyLock<Mutex<HashMap<String, Arc<ParsedModEntries>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Clear the mod entry cache.
pub fn clear_cache() {
    if let Ok(mut cache) = MOD_ENTRY_CACHE.lock() {
        cache.clear();
    }
}

/// Get stat entry names/types from the cached mod data (populated by scan_mod).
pub fn get_mod_stat_entries(mod_path: &str) -> Vec<crate::StatEntryInfo> {
    let cache = match MOD_ENTRY_CACHE.lock() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let Some(entries) = cache.get(mod_path) else {
        return Vec::new();
    };
    let mut results: Vec<crate::StatEntryInfo> = entries
        .stats_entries
        .iter()
        .map(|e| crate::StatEntryInfo {
            name: e.name.clone(),
            entry_type: e.entry_type.clone(),
        })
        .collect();
    results.sort_by(|a, b| a.name.cmp(&b.name));
    results
}

pub fn get_scanned_stat_entry_fields(
    entry_name: &str,
    entry_type: &str,
) -> Option<HashMap<String, String>> {
    let cache = MOD_ENTRY_CACHE.lock().ok()?;
    let mut mod_paths: Vec<&String> = cache.keys().collect();
    mod_paths.sort();

    for mod_path in mod_paths {
        let Some(entries) = cache.get(mod_path) else {
            continue;
        };
        let Some(entry) = entries
            .stats_entries
            .iter()
            .find(|entry| entry.name == entry_name && entry.entry_type == entry_type)
        else {
            continue;
        };
        return Some(entry.data.clone());
    }

    None
}

/// Store parsed entries for a mod in the global cache.
/// Keeps all scanned mods so they can be used as comparison targets for re-diff.
pub fn cache_mod_entries(
    mod_path: &str,
    lsx_entries: HashMap<Section, Vec<LsxEntry>>,
    stats_entries: Vec<StatsEntry>,
    source_files: HashMap<Section, String>,
) -> Result<(), String> {
    let mut cache = MOD_ENTRY_CACHE
        .lock()
        .map_err(|e| format!("Cache lock poisoned: {e}"))?;
    cache.insert(
        mod_path.to_string(),
        Arc::new(ParsedModEntries {
            lsx_entries,
            stats_entries,
            source_files,
        }),
    );
    Ok(())
}

/// Re-diff a primary mod against a different comparison source.
/// If `compare_mod_path` is empty, diffs against vanilla.
///
/// Vanilla comparison data is loaded from the reference DB at `vanilla_db_path`.
pub fn rediff_mod(
    primary_mod_path: &str,
    compare_mod_path: &str,
    vanilla_db_path: &Path,
) -> Result<Vec<SectionResult>, String> {
    // Arc-clone data out of the Mutex lock to release it before computation (M2)
    let (primary, compare_mod) = {
        let cache = MOD_ENTRY_CACHE
            .lock()
            .map_err(|e| format!("Cache lock poisoned: {e}"))?;

        let primary = Arc::clone(
            cache
                .get(primary_mod_path)
                .ok_or_else(|| "Primary mod not found in entry cache".to_string())?,
        );

        let cmp = if !compare_mod_path.is_empty() {
            Some(Arc::clone(cache.get(compare_mod_path).ok_or_else(
                || "Comparison mod not found in entry cache".to_string(),
            )?))
        } else {
            None
        };

        (primary, cmp)
    }; // Lock dropped here

    // Load vanilla comparison data from DB
    let (vanilla_lsx_sections, vanilla_stats): (
        HashMap<Section, HashMap<String, LsxEntry>>,
        HashMap<String, StatsEntry>,
    ) = {
        let mut sections = HashMap::new();
        for section in Section::all_ordered() {
            if *section == Section::Meta || *section == Section::Spells {
                continue;
            }
            match queries::query_vanilla_lsx_for_scan(vanilla_db_path, section) {
                Ok(entries) if !entries.is_empty() => {
                    sections.insert(*section, entries);
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(section = ?section, error = %e, "Failed to load vanilla LSX from DB for rediff");
                }
            }
        }
        let stats = queries::query_vanilla_stats_for_scan(vanilla_db_path).unwrap_or_default();
        (sections, stats)
    };

    if let Some(ref compare) = compare_mod {
        // Diff against vanilla + comparison mod.
        // Baseline is vanilla data overlaid with comparison mod entries.
        let mut sections = Vec::new();
        for &section in Section::all_ordered() {
            if section == Section::Spells {
                // Merge vanilla stats with comparison stats (comparison overrides vanilla)
                let mut merged_stats = vanilla_stats.clone();
                for comp_entry in &compare.stats_entries {
                    merged_stats.insert(comp_entry.name.clone(), comp_entry.clone());
                }
                let diffs = diff_stats(&primary.stats_entries, &merged_stats);
                sections.push(SectionResult {
                    section,
                    entries: diffs,
                });
            } else {
                let mod_entries = primary
                    .lsx_entries
                    .get(&section)
                    .cloned()
                    .unwrap_or_default();
                // Start with vanilla entries as baseline
                let mut merged_map: HashMap<String, LsxEntry> = vanilla_lsx_sections
                    .get(&section)
                    .cloned()
                    .unwrap_or_default();
                // Overlay comparison mod entries on top of vanilla
                if let Some(comp_entries) = compare.lsx_entries.get(&section) {
                    for entry in comp_entries {
                        if !entry.uuid.is_empty() {
                            merged_map.insert(entry.uuid.clone(), entry.clone());
                        }
                    }
                }
                let source = primary
                    .source_files
                    .get(&section)
                    .cloned()
                    .unwrap_or_default();
                let diffs = diff_section(section, &mod_entries, &merged_map, &source);
                sections.push(SectionResult {
                    section,
                    entries: diffs,
                });
            }
        }
        Ok(sections)
    } else {
        // Diff against vanilla
        let mut sections = Vec::new();
        for &section in Section::all_ordered() {
            if section == Section::Spells {
                let diffs = diff_stats(&primary.stats_entries, &vanilla_stats);
                sections.push(SectionResult {
                    section,
                    entries: diffs,
                });
            } else {
                let mod_entries = primary
                    .lsx_entries
                    .get(&section)
                    .cloned()
                    .unwrap_or_default();
                let vanilla_entries = vanilla_lsx_sections
                    .get(&section)
                    .cloned()
                    .unwrap_or_default();
                let source = primary
                    .source_files
                    .get(&section)
                    .cloned()
                    .unwrap_or_default();
                let diffs = diff_section(section, &mod_entries, &vanilla_entries, &source);
                sections.push(SectionResult {
                    section,
                    entries: diffs,
                });
            }
        }
        Ok(sections)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a minimal StatsEntry.
    fn make_stats_entry(name: &str, entry_type: &str) -> StatsEntry {
        StatsEntry {
            name: name.to_string(),
            entry_type: entry_type.to_string(),
            parent: None,
            data: HashMap::new(),
        }
    }

    // NOTE: Tests use unique paths and never call clear_cache() because
    // MOD_ENTRY_CACHE is a global static shared across parallel tests.

    // ── cache_mod_entries + get_mod_stat_entries ──────────────────

    #[test]
    fn cache_mod_entries_stores_and_retrieves() {
        let path = "/test_rediff/cache_store_retrieve";
        let stats = vec![
            make_stats_entry("Fireball", "SpellData"),
            make_stats_entry("AcidSplash", "SpellData"),
        ];

        cache_mod_entries(path, HashMap::new(), stats, HashMap::new()).expect("cache insert");
        let result = get_mod_stat_entries(path);

        assert_eq!(result.len(), 2);
        // Results should be sorted by name
        assert_eq!(result[0].name, "AcidSplash");
        assert_eq!(result[1].name, "Fireball");
    }

    #[test]
    fn cache_mod_entries_overwrites_existing() {
        let path = "/test_rediff/cache_overwrite";
        let stats1 = vec![make_stats_entry("SpellA", "SpellData")];
        let stats2 = vec![
            make_stats_entry("SpellX", "SpellData"),
            make_stats_entry("SpellY", "SpellData"),
        ];

        cache_mod_entries(path, HashMap::new(), stats1, HashMap::new()).expect("first insert");
        cache_mod_entries(path, HashMap::new(), stats2, HashMap::new()).expect("overwrite");

        let result = get_mod_stat_entries(path);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "SpellX");
        assert_eq!(result[1].name, "SpellY");
    }

    #[test]
    fn get_mod_stat_entries_missing_path_returns_empty() {
        // A path never inserted returns empty without error
        let entries = get_mod_stat_entries("/test_rediff/nonexistent_path_xyz");
        assert!(entries.is_empty());
    }

    #[test]
    fn get_mod_stat_entries_populated_returns_sorted() {
        let path = "/test_rediff/stat_entries_sorted";
        let stats = vec![
            make_stats_entry("Zephyr", "PassiveData"),
            make_stats_entry("Alpha", "StatusData"),
            make_stats_entry("Mana", "SpellData"),
        ];

        cache_mod_entries(path, HashMap::new(), stats, HashMap::new()).expect("insert");
        let result = get_mod_stat_entries(path);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].name, "Alpha");
        assert_eq!(result[0].entry_type, "StatusData");
        assert_eq!(result[1].name, "Mana");
        assert_eq!(result[2].name, "Zephyr");
    }

    #[test]
    fn get_mod_stat_entries_preserves_entry_type() {
        let path = "/test_rediff/stat_entry_types";
        let stats = vec![make_stats_entry("TestSpell", "SpellData")];
        cache_mod_entries(path, HashMap::new(), stats, HashMap::new()).expect("insert");

        let result = get_mod_stat_entries(path);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].entry_type, "SpellData");
    }

    #[test]
    fn cache_mod_entries_stores_lsx_maps() {
        let path = "/test_rediff/lsx_maps";
        let mut lsx = HashMap::new();
        lsx.insert(
            Section::Races,
            vec![LsxEntry {
                uuid: "uuid-1".to_string(),
                node_id: "Race".to_string(),
                attributes: HashMap::new(),
                children: Vec::new(),
                commented: false,
                region_id: String::new(),
            }],
        );
        let mut sources = HashMap::new();
        sources.insert(Section::Races, "Races/test.lsx".to_string());

        cache_mod_entries(path, lsx, Vec::new(), sources).expect("insert");
        // Verify it's in cache by checking get_mod_stat_entries (stats are empty)
        let stat_result = get_mod_stat_entries(path);
        assert!(stat_result.is_empty(), "no stats stored");

        // Verify the LSX data is accessible via the cache lock
        let cache = MOD_ENTRY_CACHE.lock().expect("lock");
        let data = cache.get(path).expect("should be cached");
        assert_eq!(data.lsx_entries.get(&Section::Races).unwrap().len(), 1);
        assert_eq!(
            data.source_files.get(&Section::Races).unwrap(),
            "Races/test.lsx"
        );
    }

    // ── rediff_mod ──────────────────────────────────────────────

    #[test]
    fn rediff_mod_fails_when_primary_not_cached() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let fake_db = tmp.path().join("fake.sqlite");
        let result = rediff_mod("/test_rediff/never_cached_primary_xyz", "", &fake_db);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("Primary mod not found"),
            "error should mention primary mod not found"
        );
    }

    #[test]
    fn rediff_mod_fails_when_compare_not_cached() {
        let path = "/test_rediff/compare_primary_unique_abc";
        cache_mod_entries(path, HashMap::new(), Vec::new(), HashMap::new())
            .expect("insert primary");

        let tmp = tempfile::tempdir().expect("tempdir");
        let fake_db = tmp.path().join("fake.sqlite");
        let result = rediff_mod(path, "/test_rediff/never_cached_compare_xyz", &fake_db);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("Comparison mod not found"),
            "expected 'Comparison mod not found' but got: {err}"
        );
    }
}
