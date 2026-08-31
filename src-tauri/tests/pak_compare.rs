/// Dump pak file entry list with sizes and compression info
/// Run with: cargo test --test pak_compare -- --nocapture
use std::path::Path;

fn dump_pak(label: &str, path: &Path) -> Vec<(String, u64, u64, String)> {
    let reader = bg3_cmty_studio_lib::pak::PakReader::open(path).unwrap();
    let manifest = reader.manifest();
    eprintln!("=== {label} ===");
    eprintln!("Version: {:?}", manifest.version());
    eprintln!("Flags: {:?}", manifest.package_flags());
    eprintln!(
        "Total size: {} bytes",
        std::fs::metadata(path).unwrap().len()
    );

    let mut entries: Vec<(String, u64, u64, String)> = Vec::new();
    let mut total_compressed = 0u64;
    let mut total_uncompressed = 0u64;
    let mut deleted = 0usize;

    for entry in reader.entries() {
        if entry.is_deleted() {
            deleted += 1;
            continue;
        }
        let path_str = entry.path.as_str().to_string();
        let compression = format!("{:?}", entry.compression);
        total_compressed += entry.size_on_disk;
        total_uncompressed += entry.uncompressed_size;
        entries.push((
            path_str,
            entry.size_on_disk,
            entry.uncompressed_size,
            compression,
        ));
    }

    eprintln!("Active entries: {}", entries.len());
    eprintln!("Deleted entries: {deleted}");
    eprintln!("Total compressed: {total_compressed} bytes");
    eprintln!("Total uncompressed: {total_uncompressed} bytes");
    eprintln!(
        "Compression ratio: {:.2}%",
        (total_compressed as f64 / total_uncompressed as f64) * 100.0
    );

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

#[test]
fn compare_paks() {
    let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let cs_pak = workspace.join("CS_CommunityLibrary.pak");
    let mmt_pak = workspace.join("MMT_CommunityLibrary.pak");

    if !cs_pak.exists() || !mmt_pak.exists() {
        eprintln!("One or both pak files not found, skipping");
        return;
    }

    let cs_entries = dump_pak("CS PAK", &cs_pak);
    eprintln!();
    let mmt_entries = dump_pak("MMT PAK", &mmt_pak);

    // Build maps
    let cs_map: std::collections::HashMap<&str, &(String, u64, u64, String)> =
        cs_entries.iter().map(|e| (e.0.as_str(), e)).collect();
    let mmt_map: std::collections::HashMap<&str, &(String, u64, u64, String)> =
        mmt_entries.iter().map(|e| (e.0.as_str(), e)).collect();

    eprintln!("\n=== ENTRY-BY-ENTRY COMPARISON ===");

    let mut all_paths: Vec<&str> = cs_map.keys().chain(mmt_map.keys()).copied().collect();
    all_paths.sort();
    all_paths.dedup();

    let mut cs_only = Vec::new();
    let mut mmt_only = Vec::new();
    let mut size_diffs = Vec::new();
    let mut identical = 0usize;
    let mut total_cs_compressed = 0i64;
    let mut total_mmt_compressed = 0i64;

    for path in &all_paths {
        match (cs_map.get(path), mmt_map.get(path)) {
            (Some(cs), None) => cs_only.push((path, cs.1)),
            (None, Some(mmt)) => mmt_only.push((path, mmt.1)),
            (Some(cs), Some(mmt)) => {
                total_cs_compressed += cs.1 as i64;
                total_mmt_compressed += mmt.1 as i64;
                if cs.1 != mmt.1 || cs.2 != mmt.2 {
                    size_diffs.push((path, cs.1 as i64, mmt.1 as i64, cs.2, mmt.2));
                } else {
                    identical += 1;
                }
            }
            _ => {}
        }
    }

    eprintln!("\nIdentical (same compressed size): {identical}");
    eprintln!("Size different: {}", size_diffs.len());
    eprintln!("CS-only: {}", cs_only.len());
    eprintln!("MMT-only: {}", mmt_only.len());

    if !size_diffs.is_empty() {
        eprintln!("\n--- SIZE DIFFERENCES (compressed bytes in pak) ---");
        let mut total_delta = 0i64;
        for (path, cs_sz, mmt_sz, cs_uncomp, mmt_uncomp) in &size_diffs {
            let delta = cs_sz - mmt_sz;
            total_delta += delta;
            eprintln!("  {delta:>8} bytes delta  CS={cs_sz:>8}/{cs_uncomp:>8}  MMT={mmt_sz:>8}/{mmt_uncomp:>8}  {path}");
        }
        eprintln!("  TOTAL compressed delta (common files): {total_delta} bytes");
    }

    if !cs_only.is_empty() {
        eprintln!("\n--- CS-ONLY ENTRIES ---");
        let mut total = 0u64;
        for (path, sz) in &cs_only {
            total += sz;
            eprintln!("  {sz:>8} bytes  {path}");
        }
        eprintln!("  Total: {total} bytes");
    }

    if !mmt_only.is_empty() {
        eprintln!("\n--- MMT-ONLY ENTRIES ---");
        let mut total = 0u64;
        for (path, sz) in &mmt_only {
            total += sz;
            eprintln!("  {sz:>8} bytes  {path}");
        }
        eprintln!("  Total: {total} bytes");
    }

    eprintln!("\n--- COMPRESSION COMPARISON (common files only) ---");
    eprintln!("CS total compressed: {total_cs_compressed} bytes");
    eprintln!("MMT total compressed: {total_mmt_compressed} bytes");
    eprintln!(
        "Delta: {} bytes",
        total_cs_compressed - total_mmt_compressed
    );

    // Group by extension
    let mut ext_groups: std::collections::HashMap<String, (i64, i64, i64, i64, usize)> =
        std::collections::HashMap::new();
    for (path, cs_sz, mmt_sz, cs_uncomp, mmt_uncomp) in &size_diffs {
        let ext = path.rsplit('.').next().unwrap_or("(none)").to_lowercase();
        let entry = ext_groups.entry(ext).or_insert((0, 0, 0, 0, 0));
        entry.0 += cs_sz;
        entry.1 += mmt_sz;
        entry.2 += *cs_uncomp as i64;
        entry.3 += *mmt_uncomp as i64;
        entry.4 += 1;
    }

    let mut ext_sorted: Vec<_> = ext_groups.into_iter().collect();
    ext_sorted.sort_by_key(|(_, v)| v.0 - v.1);

    eprintln!("\n=== COMPRESSION DELTA BY FILE EXTENSION ===");
    eprintln!(
        "{:>8} {:>10} {:>10} {:>10} {:>10} {:>10}  EXT",
        "COUNT", "CS_COMP", "MMT_COMP", "COMP_DELTA", "CS_UNCOMP", "MMT_UNCOMP"
    );
    eprintln!("{}", "-".repeat(80));
    for (ext, (cs_comp, mmt_comp, cs_uncomp, mmt_uncomp, count)) in &ext_sorted {
        eprintln!(
            "{:>8} {:>10} {:>10} {:>10} {:>10} {:>10}  .{}",
            count,
            cs_comp,
            mmt_comp,
            cs_comp - mmt_comp,
            cs_uncomp,
            mmt_uncomp,
            ext
        );
    }

    // Also show CS compression=None vs Lz4 counts
    eprintln!("\n=== COMPRESSION FALLBACK ANALYSIS ===");
    let mut none_count = 0usize;
    let mut lz4_count = 0usize;
    let mut none_bytes = 0u64;
    let mut lz4_bytes = 0u64;
    for (path, cs_sz, mmt_sz, cs_uncomp, mmt_uncomp) in &size_diffs {
        if *cs_uncomp == 0 {
            none_count += 1;
            none_bytes += *cs_sz as u64;
            eprintln!("  CS compression=None (fallback): {path} (CS={cs_sz}/{cs_uncomp}, MMT={mmt_sz}/{mmt_uncomp})");
        } else {
            lz4_count += 1;
            lz4_bytes += *cs_sz as u64;
        }
    }
    eprintln!("\nCS compression=None (fallback): {none_count} entries, {none_bytes} bytes on disk");
    eprintln!("CS compression=Lz4: {lz4_count} entries, {lz4_bytes} bytes on disk");

    // Top 10 biggest compression improvers (CS much smaller than MMT)
    let mut sorted_diffs = size_diffs.clone();
    sorted_diffs.sort_by_key(|(_, cs, mmt, _, _)| cs - mmt);

    eprintln!("\n=== TOP 15 BIGGEST CS COMPRESSION WINS (CS smaller) ===");
    for (path, cs_sz, mmt_sz, _cs_uncomp, _mmt_uncomp) in sorted_diffs.iter().take(15) {
        let delta = cs_sz - mmt_sz;
        eprintln!(
            "  {:>8} bytes saved  CS={}/{} MMT={}/{}  {}",
            -delta, cs_sz, _cs_uncomp, mmt_sz, _mmt_uncomp, path
        );
    }

    eprintln!("\n=== TOP 15 BIGGEST MMT COMPRESSION WINS (MMT smaller) ===");
    for (path, cs_sz, mmt_sz, cs_uncomp, mmt_uncomp) in sorted_diffs.iter().rev().take(15) {
        let delta = cs_sz - mmt_sz;
        eprintln!(
            "  {delta:>8} bytes worse  CS={cs_sz}/{cs_uncomp} MMT={mmt_sz}/{mmt_uncomp}  {path}"
        );
    }
}
