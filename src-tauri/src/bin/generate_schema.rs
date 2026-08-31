//! Dev tool: generate pre-built schema databases from BG3 game paks.
//!
//! Streams pak files via the native Rust reader, runs full schema discovery
//! (using rayon for parallelism), creates empty SQLite databases with all DDL
//! applied and the discovered schema stored as a MessagePack blob, then copies
//! for each DB target.
//!
//! Usage:
//!   cargo run --release --bin generate_schema [-- [<game_data_dir>] [<output_dir>]]
//!
//! Defaults:
//!   game_data_dir: BG3_GAME_DATA from .env or environment variable
//!   output_dir:    src-tauri/resources/gamedbs/ (or resources/gamedbs/ if CWD is src-tauri/)

use bg3_cmty_studio_lib::reference_db;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let game_data_dir = if args.len() > 1 {
        let p = PathBuf::from(&args[1]);
        if !p.is_dir() {
            eprintln!("ERROR: Game Data directory not found: {}", p.display());
            std::process::exit(1);
        }
        p
    } else {
        resolve_game_data_dir().unwrap_or_else(|| {
            eprintln!(
                "ERROR: BG3_GAME_DATA not set. Set it in .env or as an environment variable,"
            );
            eprintln!("       or pass the game Data directory as the first argument.");
            eprintln!("Usage: generate_schema [<game_data_dir>] [<output_dir>]");
            std::process::exit(1);
        })
    };

    let output_dir = if args.len() > 2 {
        PathBuf::from(&args[2]).join("gamedbs/")
    } else {
        auto_detect_output().join("gamedbs/")
    };

    std::fs::create_dir_all(&output_dir).unwrap_or_else(|e| {
        eprintln!(
            "ERROR: Cannot create output dir {}: {}",
            output_dir.display(),
            e
        );
        std::process::exit(1);
    });

    eprintln!("=== Generate Schema DBs ===");
    eprintln!("  Game Data:  {}", game_data_dir.display());
    eprintln!("  Output dir: {}", output_dir.display());

    let start = Instant::now();

    // Phase 1: Stream files from game paks
    eprintln!("\n--- Collecting files from paks ---");
    let t0 = Instant::now();
    let (mut files, pak_diags) = reference_db::pipeline::collect_files_from_paks(&game_data_dir)
        .unwrap_or_else(|e| {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        });
    eprintln!(
        "  {} files from paks in {:.1}s",
        files.len(),
        t0.elapsed().as_secs_f64()
    );
    for d in &pak_diags {
        eprintln!("    {d}");
    }

    // Phase 1b: Collect Editor files (AllSpark XCD/XMD + .lsefx) from disk
    {
        let t_ed = Instant::now();
        match reference_db::collect_editor_files(&game_data_dir) {
            Ok(editor_files) => {
                eprintln!(
                    "  {} Editor files (AllSpark + .lsefx) in {:.1}s",
                    editor_files.len(),
                    t_ed.elapsed().as_secs_f64()
                );
                files.extend(editor_files);
            }
            Err(e) => eprintln!("  WARN: Editor file scan failed: {e}"),
        }
    }

    // Phase 2: Schema discovery
    eprintln!("\n--- Discovering schema ---");
    let t1 = Instant::now();
    let schema =
        reference_db::discovery::discover_schema(&files, &game_data_dir).unwrap_or_else(|e| {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        });
    eprintln!(
        "  {} tables, {} junctions discovered in {:.1}s",
        schema.tables.len(),
        schema.junction_tables.len(),
        t1.elapsed().as_secs_f64(),
    );

    // Phase 3: Create ref_base.sqlite (schema DB for the Base/vanilla target)
    let ref_base_path = output_dir.join("ref_base.sqlite");
    eprintln!("\n--- Creating {} ---", ref_base_path.display());

    // Remove existing files
    for path in [&ref_base_path] {
        if path.exists() {
            std::fs::remove_file(path).unwrap_or_else(|e| {
                eprintln!("ERROR: Cannot remove {}: {}", path.display(), e);
                std::process::exit(1);
            });
        }
        for sfx in ["-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{}", path.display(), sfx));
            let _ = std::fs::remove_file(&p);
        }
    }

    reference_db::builder::create_schema_db(&ref_base_path, &schema).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        std::process::exit(1);
    });

    let ref_size = std::fs::metadata(&ref_base_path)
        .map(|m| m.len())
        .unwrap_or(0);
    eprintln!(
        "  ref_base.sqlite: {} bytes ({:.1} KB)",
        ref_size,
        ref_size as f64 / 1024.0
    );

    // Phase 4: Copy to ref_honor.sqlite (same schema, different data target)
    let ref_honor_path = output_dir.join("ref_honor.sqlite");
    eprintln!("\n--- Copying to {} ---", ref_honor_path.display());

    if ref_honor_path.exists() {
        let _ = std::fs::remove_file(&ref_honor_path);
    }
    for sfx in ["-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{}", ref_honor_path.display(), sfx));
        let _ = std::fs::remove_file(&p);
    }
    std::fs::copy(&ref_base_path, &ref_honor_path).unwrap_or_else(|e| {
        eprintln!("ERROR: Cannot copy to ref_honor: {e}");
        std::process::exit(1);
    });
    eprintln!("  ref_honor.sqlite: copied");

    // Phase 5: Create ref_mods.sqlite (composite PKs, no FK constraints)
    let ref_mods_path = output_dir.join("ref_mods.sqlite");
    eprintln!("\n--- Creating {} ---", ref_mods_path.display());

    if ref_mods_path.exists() {
        let _ = std::fs::remove_file(&ref_mods_path);
    }
    for sfx in ["-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{}", ref_mods_path.display(), sfx));
        let _ = std::fs::remove_file(&p);
    }

    reference_db::builder::create_mods_schema_db(&ref_mods_path, &schema).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        std::process::exit(1);
    });

    let mods_size = std::fs::metadata(&ref_mods_path)
        .map(|m| m.len())
        .unwrap_or(0);
    eprintln!(
        "  ref_mods.sqlite: {} bytes ({:.1} KB)",
        mods_size,
        mods_size as f64 / 1024.0
    );

    // Phase 6: Create staging.sqlite (reads schema from ref_base, no FKs, with tracking columns)
    let staging_path = output_dir.join("staging.sqlite");
    eprintln!("\n--- Creating {} ---", staging_path.display());

    let staging_summary = reference_db::staging::create_staging_db(&ref_base_path, &staging_path)
        .unwrap_or_else(|e| {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        });

    eprintln!(
        "  staging.sqlite: {:.1} KB ({} tables, {:.2}s)",
        staging_summary.db_size_mb * 1024.0,
        staging_summary.total_tables,
        staging_summary.elapsed_secs,
    );

    let total_secs = start.elapsed().as_secs_f64();
    eprintln!("\n=== Done in {total_secs:.1}s ===");
    eprintln!("  ref_base.sqlite:  {}", ref_base_path.display());
    eprintln!("  ref_honor.sqlite: {}", ref_honor_path.display());
    eprintln!("  ref_mods.sqlite:  {}", ref_mods_path.display());
    eprintln!("  staging.sqlite:   {}", staging_path.display());
}

fn auto_detect_output() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();

    // If CWD has src-tauri/, output to src-tauri/resources/
    let candidate = cwd.join("src-tauri").join("resources");
    if cwd.join("src-tauri").is_dir() {
        return candidate;
    }

    // If CWD IS src-tauri/, output to resources/
    if cwd.join("Cargo.toml").is_file() && cwd.join("src").is_dir() {
        return cwd.join("resources");
    }

    // Fallback
    cwd.join("resources")
}

/// Resolve the game Data directory from BG3_GAME_DATA env var.
/// Returns None if not set or not a valid directory (schema generation still works
/// without it — the AllSpark/effect tables will have fixed schemas but no file-driven
/// discovery).
fn resolve_game_data_dir() -> Option<PathBuf> {
    // Try .env file first (dotenv style)
    let cwd = std::env::current_dir().unwrap_or_default();
    for candidate in [cwd.join(".env"), cwd.join("../.env")] {
        if candidate.is_file() {
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                for line in content.lines() {
                    let line = line.trim();
                    if line.starts_with('#') || line.is_empty() {
                        continue;
                    }
                    if let Some(value) = line.strip_prefix("BG3_GAME_DATA=") {
                        let value = value.trim().trim_matches('\'').trim_matches('"');
                        let path = PathBuf::from(value);
                        if path.is_dir() {
                            return Some(path);
                        }
                    }
                }
            }
        }
    }

    // Fall back to environment variable
    if let Ok(value) = std::env::var("BG3_GAME_DATA") {
        let path = PathBuf::from(&value);
        if path.is_dir() {
            return Some(path);
        }
    }

    None
}
