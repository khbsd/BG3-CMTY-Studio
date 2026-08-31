//! Unpack-and-populate pipeline — extracts game paks, converts, and populates
//! reference SQLite databases in a single orchestrated flow.
//!
//! This replaces the old YAML-consolidation pipeline with direct SQLite ingestion.
//! After extraction, `.lsx`/`.lsf`/`.lsfx`/`.txt`/`.loca`/`.xml`
//! files are fed directly to
//! the reference DB builder (same crossbeam parallel-parse pipeline) and then
//! cleaned up.
//!
//! # Pak Sources
//!
//! | Pak File               | Content                                          |
//! |------------------------|--------------------------------------------------|
//! | Gustav.pak             | Public/{Gustav,GustavDev,Honour}                 |
//! | GustavX.pak            | Public/{GustavX,HonourX,PhotoMode}               |
//! | Shared.pak             | Public/{Shared,SharedDev}                        |
//! | Engine.pak             | Public/Engine                                    |
//! | Game.pak               | Public/Game                                      |
//! | Effects.pak            | Public/{Shared,SharedDev} (.lsfx effects)        |
//! | Materials.pak          | Public/{Game,GustavX,Shared,SharedDev}            |
//! | DiceSet*.pak           | Public/DiceSet_NN (CustomDice)                   |
//! | English.pak            | Localization/English (loca files)                |
//!
//! # Honour Data
//!
//! Honour and HonourX are NOT in separate paks — they live inside Gustav.pak
//! and GustavX.pak under `Public/Honour/` and `Public/HonourX/` respectively.
//! The extraction regex is expanded to include these paths, and files are routed
//! to `ref_honor.sqlite` by `target_db_for_module()`.

use crate::pak::{EntrySelector, PakEntryFilter, PakReader};
use crate::reference_db::builder;
use crate::reference_db::{
    extract_mod_name, module_priority, reference_db_file_type_for_path, target_db_for_module,
    BuildOptions, BuildSummary, FileEntry, FileType, TargetDb, SKIP_REGIONS,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(test)]
use std::collections::HashSet;
#[cfg(test)]
use std::io;

// ---------------------------------------------------------------------------
// Pak file manifest
// ---------------------------------------------------------------------------

/// Core vanilla paks that contain LSX data (Public/ trees with game data).
const DATA_PAKS: &[&str] = &[
    "Gustav",
    "GustavX",
    "Shared",
    "Engine",
    "Effects",
    "Game",
    "Materials",
];

/// Glob prefix for DiceSet paks (DiceSet01.pak, DiceSet02.pak, etc.).
const DICESET_PAK_PREFIX: &str = "DiceSet";

/// Hotfix pak identification.
const HOTFIX_PAK_PREFIX: &str = "Patch";
const HOTFIX_PAK_MARKER: &str = "_HotFix";

/// Localization paks (relative to game data directory).
/// English.pak lives under Localization/English/.
const LOCA_PAK_SUBPATH: &str = "Localization/English/english.pak";
const LOCA_PAK_ALT: &str = "Localization/english.pak";

// ---------------------------------------------------------------------------
// All Public roots to extract (superset of VANILLA_PUBLIC_ROOTS in divine.rs)
// This adds Honour, HonourX, Engine, Game, PhotoMode, and DiceSet modules.
// ---------------------------------------------------------------------------

/// Every Public/<module> root we want from the paks.
const ALL_PUBLIC_ROOTS: &[&str] = &[
    // Main vanilla chain
    "Public/Shared",
    "Public/SharedDev",
    "Public/Gustav",
    "Public/GustavDev",
    "Public/GustavX",
    // Honour chain (inside Gustav.pak / GustavX.pak)
    "Public/Honour",
    "Public/HonourX",
    // Auxiliary modules
    "Public/Engine",
    "Public/Game",
    "Public/PhotoMode",
];

/// File extensions to extract from paks.
/// Includes `.lsf` and `.lsfx` plus final data formats.
/// `.lsfx` is the binary format for Effect data (found in Effects.pak, GustavX.pak, etc.).
const EXTRACT_EXTS: &[&str] = &["lsf", "lsfx", "lsx", "txt", "xml", "loca"];
// ---------------------------------------------------------------------------
// Progress reporting
// ---------------------------------------------------------------------------

/// Progress event sent during pipeline execution.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PipelineProgress {
    /// Current phase label (e.g. "Extracting Gustav.pak", "Populating ref_base").
    pub phase: String,
    /// Optional detail message.
    pub detail: String,
    /// Completion percentage (0–100), or -1 for indeterminate.
    pub percent: i32,
    /// Elapsed seconds since pipeline start.
    pub elapsed_secs: f64,
}

/// Summary returned when the pipeline completes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PipelineSummary {
    /// Total elapsed time for the entire pipeline.
    pub elapsed_secs: f64,
    /// Number of paks extracted.
    pub paks_extracted: usize,
    /// Total files extracted across all paks.
    pub files_extracted: usize,
    /// Number of compatibility binary files converted to .lsx.
    pub lsf_converted: usize,
    /// Number of localization files prepared for DB ingestion.
    pub loca_converted: usize,
    /// Build summary for ref_base.sqlite (or None if not populated).
    pub base_summary: Option<BuildSummary>,
    /// Build summary for ref_honor.sqlite (or None if not populated).
    pub honor_summary: Option<BuildSummary>,
    /// Non-fatal errors encountered during the pipeline.
    pub errors: Vec<String>,
    /// Diagnostic trace messages.
    pub diagnostics: Vec<String>,
}

/// Configuration for a pipeline run.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Path to divine.exe.
    pub divine_path: String,
    /// Path to the game's Data directory (contains .pak files).
    pub game_data_path: String,
    /// Directory where paks are extracted (temporary working area).
    pub work_dir: PathBuf,
    /// Path to ref_base.sqlite (must exist with DDL applied).
    pub base_db_path: PathBuf,
    /// Path to ref_honor.sqlite (must exist with DDL applied).
    pub honor_db_path: PathBuf,
    /// Whether to populate the honor DB (false = base-only).
    pub populate_honor: bool,
    /// Build options (vacuum, force_disk).
    pub build_options: BuildOptions,
    /// Whether to clean up intermediate files after DB population.
    pub cleanup: bool,
}

// ---------------------------------------------------------------------------
// Pipeline orchestrator
// ---------------------------------------------------------------------------

/// Run the full unpack-and-populate pipeline.
///
/// Phases:
///   1. Extract all data paks (filtered) into work_dir
///   2. Extract localization paks
///   3. Collect all extracted files into FileEntry list
///   4. Populate ref_base.sqlite with Base-target files
///   5. Populate ref_honor.sqlite with Honor-target files (if requested)
///   6. Cleanup intermediate files (if requested)
///
/// `on_progress` is called at each phase boundary. It receives a clone of the
/// progress event. The callback is invoked on the pipeline thread (blocking).
pub fn run_vanilla_pipeline<F>(
    config: &PipelineConfig,
    on_progress: F,
) -> Result<PipelineSummary, String>
where
    F: Fn(PipelineProgress) + Send + Sync,
{
    let pipeline_start = Instant::now();
    let mut diagnostics: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut paks_extracted = 0usize;
    let mut files_extracted = 0usize;
    let lsf_converted = 0usize;

    // Validate inputs
    let data_dir = resolve_data_dir(&config.game_data_path, &mut diagnostics)?;
    if !config.base_db_path.is_file() {
        return Err(format!(
            "Base schema DB not found: {}. Run schema generator first.",
            config.base_db_path.display()
        ));
    }
    if config.populate_honor && !config.honor_db_path.is_file() {
        return Err(format!(
            "Honor schema DB not found: {}. Run schema generator first.",
            config.honor_db_path.display()
        ));
    }

    // Build the extraction regex (all public roots, all subfolders + Stats)
    let data_filter = build_pipeline_filter()?;
    if let Some(pattern) = data_filter.regex_pattern() {
        diagnostics.push(format!("Extraction regex: {pattern}"));
    }

    // Collect all .pak files in data_dir for diagnostics and DiceSet/hotfix detection
    let all_pak_files = list_pak_files(&data_dir)?;
    diagnostics.push(format!(
        "Found {} .pak files in {}",
        all_pak_files.len(),
        data_dir.display()
    ));

    let data_pak_names: Vec<String> = DATA_PAKS.iter().map(|name| (*name).to_string()).collect();
    let diceset_paks: Vec<String> = all_pak_files
        .iter()
        .filter(|n| n.starts_with(DICESET_PAK_PREFIX) && n.ends_with(".pak"))
        .cloned()
        .collect();
    let hotfix_paks: Vec<String> = all_pak_files
        .iter()
        .filter(|n| n.starts_with(HOTFIX_PAK_PREFIX) && n.contains(HOTFIX_PAK_MARKER))
        .cloned()
        .collect();

    let mut all_files = Vec::new();

    // -----------------------------------------------------------------------
    // Phase 1: Load data files from paks
    // -----------------------------------------------------------------------
    let phase_start = Instant::now();
    let total_paks_to_extract = count_paks_to_extract(&data_dir, &all_pak_files);
    let mut processed_data_paks = 0usize;

    for pak_name in &data_pak_names {
        processed_data_paks += 1;
        emit_progress(
            &on_progress,
            &pipeline_start,
            format!("Extracting {pak_name}.pak"),
            format!("{processed_data_paks}/{total_paks_to_extract}"),
            progress_pct(processed_data_paks - 1, total_paks_to_extract),
        );

        match load_data_pak(&data_dir, pak_name, &data_filter) {
            Ok((files, count, diag)) => {
                paks_extracted += 1;
                files_extracted += count;
                all_files.extend(files);
                diagnostics.extend(diag);
            }
            Err(e) => {
                diagnostics.push(format!("FAIL {pak_name}: {e}"));
                errors.push(format!("{pak_name}.pak: {e}"));
            }
        }
    }

    for ds_pak in &diceset_paks {
        processed_data_paks += 1;
        emit_progress(
            &on_progress,
            &pipeline_start,
            format!("Extracting {ds_pak}"),
            format!("{processed_data_paks}/{total_paks_to_extract}"),
            progress_pct(processed_data_paks - 1, total_paks_to_extract),
        );
        let ds_name = ds_pak.trim_end_matches(".pak");
        match load_data_pak(&data_dir, ds_name, &data_filter) {
            Ok((files, count, diag)) => {
                paks_extracted += 1;
                files_extracted += count;
                all_files.extend(files);
                diagnostics.extend(diag);
            }
            Err(e) => {
                diagnostics.push(format!("FAIL {ds_pak}: {e}"));
            }
        }
    }

    for hf_pak in &hotfix_paks {
        processed_data_paks += 1;
        emit_progress(
            &on_progress,
            &pipeline_start,
            format!("Extracting {hf_pak}"),
            format!("{processed_data_paks}/{total_paks_to_extract}"),
            progress_pct(processed_data_paks - 1, total_paks_to_extract),
        );
        let hf_name = hf_pak.trim_end_matches(".pak");
        match load_data_pak(&data_dir, hf_name, &data_filter) {
            Ok((files, count, diag)) => {
                paks_extracted += 1;
                files_extracted += count;
                all_files.extend(files);
                diagnostics.extend(diag);
            }
            Err(e) => {
                diagnostics.push(format!("FAIL {hf_pak}: {e}"));
                errors.push(format!("{hf_pak}: {e}"));
            }
        }
    }

    diagnostics.push(format!(
        "Phase 1 (extract): {:.1}s, {} paks, {} files (streamed)",
        phase_start.elapsed().as_secs_f64(),
        paks_extracted,
        files_extracted,
    ));

    // -----------------------------------------------------------------------
    // Phase 2: Extract localization
    // -----------------------------------------------------------------------
    emit_progress(
        &on_progress,
        &pipeline_start,
        "Extracting localization".into(),
        String::new(),
        -1,
    );
    let phase_start = Instant::now();

    let (loca_files, loca_converted) = load_loca(
        &data_dir,
        &mut paks_extracted,
        &mut files_extracted,
        &mut diagnostics,
        &mut errors,
    )?;
    all_files.extend(loca_files);

    // VoiceMeta not extracted — no schema tables for VoiceMetaData region.
    // Extracting it would produce ~2,300 row errors with 0 usable rows.

    diagnostics.push(format!(
        "Phase 2 (localization): {:.1}s, {} loca files",
        phase_start.elapsed().as_secs_f64(),
        loca_converted
    ));

    // -----------------------------------------------------------------------
    // Phase 3: Collect all extracted files into FileEntry list
    // -----------------------------------------------------------------------
    emit_progress(
        &on_progress,
        &pipeline_start,
        "Collecting files for DB ingestion".into(),
        String::new(),
        -1,
    );
    let phase_start = Instant::now();

    all_files.sort_by(|a, b| b.priority.cmp(&a.priority));
    let base_files: Vec<&FileEntry> = all_files
        .iter()
        .filter(|f| f.target_db == TargetDb::Base)
        .collect();
    let honor_files: Vec<&FileEntry> = all_files
        .iter()
        .filter(|f| f.target_db == TargetDb::Honor)
        .collect();

    diagnostics.push(format!(
        "Phase 3 (collect): {:.1}s, {} total ({} base, {} honor)",
        phase_start.elapsed().as_secs_f64(),
        all_files.len(),
        base_files.len(),
        honor_files.len()
    ));

    // -----------------------------------------------------------------------
    // Phase 4: Populate ref_base.sqlite
    // -----------------------------------------------------------------------
    emit_progress(
        &on_progress,
        &pipeline_start,
        "Populating ref_base.sqlite".into(),
        format!("{} files", base_files.len()),
        60,
    );
    let phase_start = Instant::now();

    let base_file_vec: Vec<FileEntry> = base_files.into_iter().cloned().collect();
    let base_summary = builder::populate_db(
        &config.base_db_path,
        &base_file_vec,
        &config.work_dir,
        &config.build_options,
    )
    .map_err(|e| format!("Populate ref_base: {e}"))?;

    let base_db_size = fs::metadata(&config.base_db_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let base_summary = BuildSummary {
        db_path: config.base_db_path.display().to_string(),
        total_files: base_file_vec.len(),
        total_rows: base_summary.total_rows,
        total_tables: base_summary.total_tables,
        fk_constraints: base_summary.fk_constraints,
        file_errors: base_summary.file_errors,
        row_errors: base_summary.row_errors,
        elapsed_secs: phase_start.elapsed().as_secs_f64(),
        db_size_mb: base_db_size as f64 / (1024.0 * 1024.0),
        phase_times: base_summary.phase_times,
    };

    diagnostics.push(format!(
        "Phase 4 (ref_base): {:.1}s, {} rows, {:.1} MB",
        base_summary.elapsed_secs, base_summary.total_rows, base_summary.db_size_mb
    ));

    // -----------------------------------------------------------------------
    // Phase 5: Populate ref_honor.sqlite (optional)
    // -----------------------------------------------------------------------
    let honor_summary = if config.populate_honor && !honor_files.is_empty() {
        emit_progress(
            &on_progress,
            &pipeline_start,
            "Populating ref_honor.sqlite".into(),
            format!("{} files", honor_files.len()),
            85,
        );
        let phase_start = Instant::now();

        let honor_file_vec: Vec<FileEntry> = honor_files.into_iter().cloned().collect();
        let mut honor_build_options = config.build_options.clone();
        honor_build_options.fallback_base_db_path = Some(config.base_db_path.clone());
        let honor_result = builder::populate_db(
            &config.honor_db_path,
            &honor_file_vec,
            &config.work_dir,
            &honor_build_options,
        )
        .map_err(|e| format!("Populate ref_honor: {e}"))?;

        let honor_db_size = fs::metadata(&config.honor_db_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let summary = BuildSummary {
            db_path: config.honor_db_path.display().to_string(),
            total_files: honor_file_vec.len(),
            total_rows: honor_result.total_rows,
            total_tables: honor_result.total_tables,
            fk_constraints: honor_result.fk_constraints,
            file_errors: honor_result.file_errors,
            row_errors: honor_result.row_errors,
            elapsed_secs: phase_start.elapsed().as_secs_f64(),
            db_size_mb: honor_db_size as f64 / (1024.0 * 1024.0),
            phase_times: honor_result.phase_times,
        };

        diagnostics.push(format!(
            "Phase 5 (ref_honor): {:.1}s, {} rows, {:.1} MB",
            summary.elapsed_secs, summary.total_rows, summary.db_size_mb
        ));
        Some(summary)
    } else {
        diagnostics.push("Phase 5 (ref_honor): skipped".into());
        None
    };

    emit_progress(
        &on_progress,
        &pipeline_start,
        "Pipeline complete".into(),
        String::new(),
        100,
    );

    Ok(PipelineSummary {
        elapsed_secs: pipeline_start.elapsed().as_secs_f64(),
        paks_extracted,
        files_extracted,
        lsf_converted,
        loca_converted,
        base_summary: Some(base_summary),
        honor_summary,
        errors,
        diagnostics,
    })
}

// ---------------------------------------------------------------------------
// Pak-based file collection (no DB population)
// ---------------------------------------------------------------------------

/// Collect all reference-DB–relevant files from `.pak` archives in a game Data
/// directory.  This is the pak-based counterpart of [`super::collect_files`]
/// (which walks loose files on disk).
///
/// Loads the same set of paks as `run_vanilla_pipeline` (core data paks,
/// DiceSet paks, and English localization), streams each entry through the
/// standard extraction filter, and returns the in-memory `FileEntry` list
/// ready for `discover_schema` or `populate_db`.
///
/// Returns `(files, diagnostics)`.
pub fn collect_files_from_paks(data_dir: &Path) -> Result<(Vec<FileEntry>, Vec<String>), String> {
    let mut diagnostics: Vec<String> = Vec::new();
    let mut all_files: Vec<FileEntry> = Vec::new();

    let data_dir = resolve_data_dir(data_dir.to_str().unwrap_or_default(), &mut diagnostics)?;

    let data_filter = build_pipeline_filter()?;
    let all_pak_files = list_pak_files(&data_dir)?;
    diagnostics.push(format!(
        "Found {} .pak files in {}",
        all_pak_files.len(),
        data_dir.display()
    ));

    let data_pak_names: Vec<String> = DATA_PAKS.iter().map(|name| (*name).to_string()).collect();
    let diceset_paks: Vec<String> = all_pak_files
        .iter()
        .filter(|n| n.starts_with(DICESET_PAK_PREFIX) && n.ends_with(".pak"))
        .cloned()
        .collect();

    // --- Core data paks ---
    for pak_name in &data_pak_names {
        match load_data_pak(&data_dir, pak_name, &data_filter) {
            Ok((files, _count, diag)) => {
                all_files.extend(files);
                diagnostics.extend(diag);
            }
            Err(e) => return Err(format!("{pak_name}.pak: {e}")),
        }
    }

    // --- DiceSet paks ---
    for ds_pak in &diceset_paks {
        let ds_name = ds_pak.trim_end_matches(".pak");
        match load_data_pak(&data_dir, ds_name, &data_filter) {
            Ok((files, _count, diag)) => {
                all_files.extend(files);
                diagnostics.extend(diag);
            }
            Err(e) => {
                diagnostics.push(format!("SKIP {ds_pak}: {e}"));
            }
        }
    }

    // --- Hotfix paks ---
    let hotfix_paks: Vec<String> = all_pak_files
        .iter()
        .filter(|n| n.starts_with(HOTFIX_PAK_PREFIX) && n.contains(HOTFIX_PAK_MARKER))
        .cloned()
        .collect();
    for hf_pak in &hotfix_paks {
        let hf_name = hf_pak.trim_end_matches(".pak");
        match load_data_pak(&data_dir, hf_name, &data_filter) {
            Ok((files, _count, diag)) => {
                all_files.extend(files);
                diagnostics.extend(diag);
            }
            Err(e) => {
                diagnostics.push(format!("SKIP {hf_pak}: {e}"));
            }
        }
    }

    // --- English localization ---
    {
        let mut paks_extracted = 0usize;
        let mut files_extracted = 0usize;
        let mut errors: Vec<String> = Vec::new();
        let (loca_files, _count) = load_loca(
            &data_dir,
            &mut paks_extracted,
            &mut files_extracted,
            &mut diagnostics,
            &mut errors,
        )?;
        all_files.extend(loca_files);
        if !errors.is_empty() {
            return Err(errors.join("; "));
        }
    }

    diagnostics.push(format!("Total: {} files from paks", all_files.len()));
    Ok((all_files, diagnostics))
}

// ---------------------------------------------------------------------------
// Populate-only vanilla pipeline (schema DBs must already exist)
// ---------------------------------------------------------------------------

/// Populate pre-built schema databases from game `.pak` files — no Divine.exe,
/// no intermediate files, no YAML.
///
/// This is the runtime path: the schema DBs (with DDL applied) are shipped with
/// the application.  This function streams pak entries through native parsers
/// directly into SQLite.
///
/// `game_data_path` — directory containing `.pak` files (auto-resolves `Data/`
///                     subdirectory if needed).
/// `base_db_path`   — path to `ref_base.sqlite` (must exist with DDL applied).
/// `honor_db_path`  — path to `ref_honor.sqlite` (must exist with DDL applied).
///                     Pass `None` to skip honour population.
/// `options`        — build options (vacuum, etc.).
/// `on_progress`    — optional callback invoked at phase boundaries.
pub fn populate_vanilla_dbs<F>(
    game_data_path: &str,
    base_db_path: &Path,
    honor_db_path: Option<&Path>,
    options: &BuildOptions,
    on_progress: F,
) -> Result<PipelineSummary, String>
where
    F: Fn(PipelineProgress),
{
    let pipeline_start = Instant::now();
    let mut diagnostics: Vec<String> = Vec::new();
    let errors: Vec<String> = Vec::new();

    // Validate inputs
    if !base_db_path.is_file() {
        return Err(format!(
            "Base schema DB not found: {}. The application may need to be reinstalled.",
            base_db_path.display()
        ));
    }
    if let Some(hp) = honor_db_path {
        if !hp.is_file() {
            return Err(format!(
                "Honor schema DB not found: {}. The application may need to be reinstalled.",
                hp.display()
            ));
        }
    }

    // Phase 1: Stream files from paks
    emit_progress(
        &on_progress,
        &pipeline_start,
        "Streaming game data".into(),
        "Reading .pak files…".into(),
        -1,
    );
    let phase_start = Instant::now();
    let (mut all_files, pak_diag) = collect_files_from_paks(Path::new(game_data_path))?;
    let files_extracted = all_files.len();
    diagnostics.extend(pak_diag);
    diagnostics.push(format!(
        "Phase 1a (stream paks): {:.1}s, {} files",
        phase_start.elapsed().as_secs_f64(),
        files_extracted,
    ));

    // Phase 1b: Collect Editor files (AllSpark + .lsefx) from disk
    emit_progress(
        &on_progress,
        &pipeline_start,
        "Collecting editor files".into(),
        "".into(),
        20,
    );
    let phase_start = Instant::now();
    let data_dir = Path::new(game_data_path);
    match super::collect_editor_files(data_dir) {
        Ok(editor_files) => {
            let count = editor_files.len();
            all_files.extend(editor_files);
            diagnostics.push(format!(
                "Phase 1b (Editor files): {:.1}s, {} files",
                phase_start.elapsed().as_secs_f64(),
                count,
            ));
        }
        Err(e) => {
            diagnostics.push(format!("WARN: Editor file scan failed: {e}"));
        }
    }

    // Sort by priority descending (highest wins on PK conflict via INSERT OR IGNORE)
    all_files.sort_by(|a, b| b.priority.cmp(&a.priority));

    // Partition into base / honor
    let base_files: Vec<FileEntry> = all_files
        .iter()
        .filter(|f| f.target_db == TargetDb::Base)
        .cloned()
        .collect();
    let honor_files: Vec<FileEntry> = all_files
        .iter()
        .filter(|f| f.target_db == TargetDb::Honor)
        .cloned()
        .collect();

    diagnostics.push(format!(
        "Partitioned: {} base, {} honor",
        base_files.len(),
        honor_files.len()
    ));

    // Count unique source modules (proxy for pak count)
    let paks_extracted = {
        let mut sources: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for f in &all_files {
            sources.insert(&f.mod_name);
        }
        sources.len()
    };

    // Phase 2: Populate ref_base
    emit_progress(
        &on_progress,
        &pipeline_start,
        "Populating ref_base".into(),
        format!("{} files", base_files.len()),
        30,
    );
    let phase_start = Instant::now();
    let base_result = builder::populate_db(
        base_db_path,
        &base_files,
        Path::new(game_data_path),
        options,
    )
    .map_err(|e| format!("Populate ref_base: {e}"))?;

    let base_db_size = fs::metadata(base_db_path).map(|m| m.len()).unwrap_or(0);
    let base_summary = BuildSummary {
        db_path: base_db_path.display().to_string(),
        total_files: base_files.len(),
        total_rows: base_result.total_rows,
        total_tables: base_result.total_tables,
        fk_constraints: base_result.fk_constraints,
        file_errors: base_result.file_errors,
        row_errors: base_result.row_errors,
        elapsed_secs: phase_start.elapsed().as_secs_f64(),
        db_size_mb: base_db_size as f64 / (1024.0 * 1024.0),
        phase_times: base_result.phase_times,
    };
    diagnostics.push(format!(
        "Phase 2 (ref_base): {:.1}s, {} rows, {:.1} MB",
        base_summary.elapsed_secs, base_summary.total_rows, base_summary.db_size_mb
    ));

    // Phase 3: Populate ref_honor (optional)
    emit_progress(
        &on_progress,
        &pipeline_start,
        "Populating ref_honor".into(),
        format!("{} files", honor_files.len()),
        70,
    );
    let honor_summary = match honor_db_path {
        Some(hp) if !honor_files.is_empty() => {
            let phase_start = Instant::now();
            let mut honor_options = options.clone();
            honor_options.fallback_base_db_path = Some(base_db_path.to_path_buf());

            let honor_result =
                builder::populate_db(hp, &honor_files, Path::new(game_data_path), &honor_options)
                    .map_err(|e| format!("Populate ref_honor: {e}"))?;

            let honor_db_size = fs::metadata(hp).map(|m| m.len()).unwrap_or(0);
            let summary = BuildSummary {
                db_path: hp.display().to_string(),
                total_files: honor_files.len(),
                total_rows: honor_result.total_rows,
                total_tables: honor_result.total_tables,
                fk_constraints: honor_result.fk_constraints,
                file_errors: honor_result.file_errors,
                row_errors: honor_result.row_errors,
                elapsed_secs: phase_start.elapsed().as_secs_f64(),
                db_size_mb: honor_db_size as f64 / (1024.0 * 1024.0),
                phase_times: honor_result.phase_times,
            };
            diagnostics.push(format!(
                "Phase 3 (ref_honor): {:.1}s, {} rows, {:.1} MB",
                summary.elapsed_secs, summary.total_rows, summary.db_size_mb
            ));
            Some(summary)
        }
        _ => {
            diagnostics.push("Phase 3 (ref_honor): skipped".into());
            None
        }
    };

    if !errors.is_empty() {
        diagnostics.push(format!("{} non-fatal errors", errors.len()));
    }

    Ok(PipelineSummary {
        elapsed_secs: pipeline_start.elapsed().as_secs_f64(),
        paks_extracted,
        files_extracted,
        lsf_converted: 0,
        loca_converted: all_files
            .iter()
            .filter(|f| f.file_type == FileType::Loca)
            .count(),
        base_summary: Some(base_summary),
        honor_summary,
        errors,
        diagnostics,
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Resolve the game data directory, auto-detecting Data/ subdirectory.
fn resolve_data_dir(
    game_data_path: &str,
    diagnostics: &mut Vec<String>,
) -> Result<PathBuf, String> {
    let mut data_dir = PathBuf::from(game_data_path);
    if !data_dir.exists() {
        return Err(format!(
            "Game data directory does not exist: {game_data_path}"
        ));
    }

    // Auto-resolve: if user pointed at game root instead of Data/
    let data_subfolder = data_dir.join("Data");
    if data_subfolder.is_dir() {
        let has_paks_here = has_pak_files(&data_dir);
        let has_paks_in_data = has_pak_files(&data_subfolder);
        if !has_paks_here && has_paks_in_data {
            diagnostics.push(format!(
                "Auto-resolved: using {}/Data (no .pak in root)",
                data_dir.display()
            ));
            data_dir = data_subfolder;
        }
    }

    diagnostics.push(format!("Game data dir: {}", data_dir.display()));
    Ok(data_dir)
}

/// Check if a directory contains any .pak files.
fn has_pak_files(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .any(|e| e.path().extension().is_some_and(|ext| ext == "pak"))
        })
        .unwrap_or(false)
}

/// List all .pak filenames in a directory (non-recursive).
fn list_pak_files(dir: &Path) -> Result<Vec<String>, String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("Cannot read data dir: {e}"))?;
    let mut paks: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "pak"))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    paks.sort();
    Ok(paks)
}

/// Count total paks we'll try to extract (for progress percentage).
fn count_paks_to_extract(data_dir: &Path, all_pak_files: &[String]) -> usize {
    let mut count = 0;
    // Named data paks
    for pak_name in DATA_PAKS {
        let p = data_dir.join(format!("{pak_name}.pak"));
        let alt = data_dir.join(format!("{pak_name}_0.pak"));
        if p.exists() || alt.exists() {
            count += 1;
        }
    }
    // DiceSet paks
    count += all_pak_files
        .iter()
        .filter(|n| n.starts_with(DICESET_PAK_PREFIX) && n.ends_with(".pak"))
        .count();
    // Hotfix paks
    count += all_pak_files
        .iter()
        .filter(|n| n.starts_with(HOTFIX_PAK_PREFIX) && n.contains(HOTFIX_PAK_MARKER))
        .count();
    count
}

fn load_data_pak(
    data_dir: &Path,
    pak_name: &str,
    data_filter: &PakEntryFilter,
) -> Result<(Vec<FileEntry>, usize, Vec<String>), String> {
    load_data_pak_streamed(data_dir, pak_name, data_filter)
}

fn load_data_pak_streamed(
    data_dir: &Path,
    pak_name: &str,
    data_filter: &PakEntryFilter,
) -> Result<(Vec<FileEntry>, usize, Vec<String>), String> {
    let mut diag = Vec::new();
    let pak_path = data_dir.join(format!("{pak_name}.pak"));
    let alt_path = data_dir.join(format!("{pak_name}_0.pak"));

    let chosen = if pak_path.exists() {
        pak_path
    } else if alt_path.exists() {
        alt_path
    } else {
        return Err(format!(
            "{}.pak not found (tried {} and {})",
            pak_name,
            pak_path.display(),
            alt_path.display()
        ));
    };

    let pak_size_mb =
        fs::metadata(&chosen).map(|m| m.len()).unwrap_or(0) as f64 / (1024.0 * 1024.0);
    tracing::info!(
        "[pipeline] Streaming {} ({:.0} MB)",
        chosen.display(),
        pak_size_mb
    );

    let load_start = Instant::now();
    let files = collect_data_entries_native(&chosen, pak_name, data_filter)?;
    let load_secs = load_start.elapsed().as_secs_f64();

    let msg = format!(
        "  {} files loaded from {} in {:.1}s",
        files.len(),
        pak_name,
        load_secs
    );
    tracing::info!("[pipeline] {}", msg);
    diag.push(msg);
    let count = files.len();
    Ok((files, count, diag))
}

/// Extract a single data pak into work_dir/<pak_name>/.
/// Only used by integration tests for native-vs-Divine comparison.
#[cfg(test)]
#[allow(dead_code)]
fn extract_data_pak(
    data_dir: &Path,
    pak_name: &str,
    work_dir: &Path,
    data_filter: &PakEntryFilter,
) -> Result<(usize, Vec<String>), String> {
    let mut diag = Vec::new();
    let pak_path = data_dir.join(format!("{pak_name}.pak"));
    let alt_path = data_dir.join(format!("{pak_name}_0.pak"));

    let chosen = if pak_path.exists() {
        pak_path
    } else if alt_path.exists() {
        alt_path
    } else {
        return Err(format!(
            "{}.pak not found (tried {} and {})",
            pak_name,
            pak_path.display(),
            alt_path.display()
        ));
    };

    let dest = work_dir.join(pak_name);
    let pak_size_mb =
        fs::metadata(&chosen).map(|m| m.len()).unwrap_or(0) as f64 / (1024.0 * 1024.0);
    tracing::info!(
        "[pipeline] Extracting {} ({:.0} MB) → {}",
        chosen.display(),
        pak_size_mb,
        dest.display()
    );

    let extract_start = Instant::now();
    let count = extract_data_entries_native(&chosen, &dest, data_filter)?;
    let extract_secs = extract_start.elapsed().as_secs_f64();

    let msg = format!("  {count} files extracted from {pak_name} in {extract_secs:.1}s");
    tracing::info!("[pipeline] {}", msg);
    diag.push(msg);
    Ok((count, diag))
}

fn collect_data_entries_native(
    pak_path: &Path,
    pak_name: &str,
    data_filter: &PakEntryFilter,
) -> Result<Vec<FileEntry>, String> {
    let reader = PakReader::open(pak_path).map_err(|e| e.to_string())?;
    let mut files = Vec::new();

    for entry in reader.entries() {
        if entry.is_deleted() {
            continue;
        }

        let package_path = entry.path.as_str();
        if !data_filter.matches(entry) {
            continue;
        }

        let rel_path = format!("{}/{}", pak_name, package_path.replace('\\', "/"));
        if SKIP_REGIONS
            .iter()
            .any(|segment| rel_path.contains(segment))
        {
            continue;
        }
        if SKIP_PATH_SEGMENTS
            .iter()
            .any(|segment| rel_path.contains(segment))
        {
            continue;
        }

        let file_type = match reference_db_file_type_for_path(Path::new(package_path)) {
            Some(file_type) => file_type,
            None => continue,
        };

        let mod_name = extract_mod_name(&rel_path);
        let target_db = target_db_for_module(&mod_name);
        let priority = module_priority(&mod_name);
        let byte_limit = usize::try_from(entry.effective_size())
            .map_err(|_| format!("Pak entry too large to materialize: {package_path}"))?;
        let mut source = reader.open_entry(entry).map_err(|e| e.to_string())?;
        let bytes = source
            .read_to_end_with_limit(byte_limit)
            .map_err(|e| e.to_string())?;

        files.push(FileEntry::from_bytes(
            rel_path, file_type, mod_name, target_db, priority, bytes,
        ));
    }

    Ok(files)
}

#[cfg(test)]
fn extract_data_entries_native(
    pak_path: &Path,
    dest_dir: &Path,
    data_filter: &PakEntryFilter,
) -> Result<usize, String> {
    let reader = PakReader::open(pak_path).map_err(|e| e.to_string())?;
    let mut extracted = 0usize;
    let mut created_dirs: HashSet<PathBuf> = HashSet::new();

    for entry in reader.entries() {
        if entry.is_deleted() {
            continue;
        }

        let package_path = entry.path.as_str();
        if !data_filter.matches(entry) {
            continue;
        }

        let output_path = package_output_path(dest_dir, package_path);
        if let Some(parent) = output_path.parent() {
            if created_dirs.insert(parent.to_path_buf()) {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Create data parent dir {}: {}", parent.display(), e))?;
            }
        }

        let mut source = reader.open_entry(entry).map_err(|e| e.to_string())?;
        let mut destination = fs::File::create(&output_path)
            .map_err(|e| format!("Create data file {}: {}", output_path.display(), e))?;
        io::copy(&mut source, &mut destination)
            .map_err(|e| format!("Write data file {}: {}", output_path.display(), e))?;
        extracted += 1;
    }

    Ok(extracted)
}

/// Extract localization paks (English .loca).
/// Only used by integration tests.
#[cfg(test)]
#[allow(dead_code)]
fn extract_loca(
    data_dir: &Path,
    loca_dir: &Path,
    paks_extracted: &mut usize,
    files_extracted: &mut usize,
    diagnostics: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> Result<usize, String> {
    // Find English loca pak
    let candidates = [data_dir.join(LOCA_PAK_SUBPATH), data_dir.join(LOCA_PAK_ALT)];

    let mut loca_pak = None;
    for candidate in &candidates {
        if candidate.exists() {
            loca_pak = Some(candidate.clone());
            diagnostics.push(format!("Found English loca pak: {}", candidate.display()));
            break;
        }
    }

    let loca_pak = match loca_pak {
        Some(p) => p,
        None => {
            diagnostics.push("No English localization pak found".into());
            return Ok(0);
        }
    };

    // Extract only .loca files (skip any .bnk or asset files)
    fs::create_dir_all(loca_dir).map_err(|e| format!("Create loca dir: {e}"))?;

    let pak_size_mb =
        fs::metadata(&loca_pak).map(|m| m.len()).unwrap_or(0) as f64 / (1024.0 * 1024.0);
    tracing::info!(
        "[pipeline] Extracting {} ({:.0} MB) → {}",
        loca_pak.display(),
        pak_size_mb,
        loca_dir.display()
    );
    let extract_start = Instant::now();

    match extract_loca_entries_native(&loca_pak, loca_dir) {
        Ok(count) => {
            let secs = extract_start.elapsed().as_secs_f64();
            tracing::info!(
                "[pipeline]   {} .loca files extracted in {:.1}s",
                count,
                secs
            );
            *paks_extracted += 1;
            *files_extracted += count;
            diagnostics.push(format!("Loca pak: {count} files extracted in {secs:.1}s"));
        }
        Err(e) => {
            errors.push(format!("Loca extraction: {e}"));
            return Ok(0);
        }
    }

    let extracted_files = count_files_recursive(loca_dir);
    diagnostics.push(format!(
        "Localization files ready for ingestion: {extracted_files}"
    ));
    Ok(extracted_files)
}

fn load_loca(
    data_dir: &Path,
    paks_extracted: &mut usize,
    files_extracted: &mut usize,
    diagnostics: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> Result<(Vec<FileEntry>, usize), String> {
    load_loca_streamed(
        data_dir,
        paks_extracted,
        files_extracted,
        diagnostics,
        errors,
    )
}

fn load_loca_streamed(
    data_dir: &Path,
    paks_extracted: &mut usize,
    files_extracted: &mut usize,
    diagnostics: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> Result<(Vec<FileEntry>, usize), String> {
    let candidates = [data_dir.join(LOCA_PAK_SUBPATH), data_dir.join(LOCA_PAK_ALT)];

    let mut loca_pak = None;
    for candidate in &candidates {
        if candidate.exists() {
            loca_pak = Some(candidate.clone());
            diagnostics.push(format!("Found English loca pak: {}", candidate.display()));
            break;
        }
    }

    let Some(loca_pak) = loca_pak else {
        diagnostics.push("No English localization pak found".into());
        return Ok((Vec::new(), 0));
    };

    let pak_size_mb =
        fs::metadata(&loca_pak).map(|m| m.len()).unwrap_or(0) as f64 / (1024.0 * 1024.0);
    tracing::info!(
        "[pipeline] Streaming {} ({:.0} MB)",
        loca_pak.display(),
        pak_size_mb
    );
    let load_start = Instant::now();

    match collect_loca_entries_native(&loca_pak) {
        Ok(files) => {
            let secs = load_start.elapsed().as_secs_f64();
            tracing::info!(
                "[pipeline]   {} .loca files loaded in {:.1}s",
                files.len(),
                secs
            );
            *paks_extracted += 1;
            *files_extracted += files.len();
            diagnostics.push(format!(
                "Loca pak: {} files loaded in {:.1}s",
                files.len(),
                secs
            ));
            let count = files.len();
            Ok((files, count))
        }
        Err(e) => {
            errors.push(format!("Loca extraction: {e}"));
            Ok((Vec::new(), 0))
        }
    }
}

#[cfg(test)]
fn extract_loca_entries_native(pak_path: &Path, loca_dir: &Path) -> Result<usize, String> {
    let reader = PakReader::open(pak_path).map_err(|e| e.to_string())?;
    let mut extracted = 0usize;
    let mut created_dirs: HashSet<PathBuf> = HashSet::new();

    for entry in reader.entries() {
        if entry.is_deleted() {
            continue;
        }

        let path = entry.path.as_str();
        let is_loca = entry
            .path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("loca"));
        if !is_loca {
            continue;
        }

        let output_path = localization_output_path(loca_dir, path);
        if let Some(parent) = output_path.parent() {
            if created_dirs.insert(parent.to_path_buf()) {
                fs::create_dir_all(parent).map_err(|e| {
                    format!("Create localization parent dir {}: {}", parent.display(), e)
                })?;
            }
        }

        let mut source = reader.open_entry(entry).map_err(|e| e.to_string())?;
        let mut destination = fs::File::create(&output_path)
            .map_err(|e| format!("Create localization file {}: {}", output_path.display(), e))?;
        io::copy(&mut source, &mut destination)
            .map_err(|e| format!("Write localization file {}: {}", output_path.display(), e))?;
        extracted += 1;
    }

    Ok(extracted)
}

fn collect_loca_entries_native(pak_path: &Path) -> Result<Vec<FileEntry>, String> {
    let reader = PakReader::open(pak_path).map_err(|e| e.to_string())?;
    let mut files = Vec::new();

    for entry in reader.entries() {
        if entry.is_deleted() {
            continue;
        }

        let package_path = entry.path.as_str();
        let is_loca = entry
            .path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("loca"));
        if !is_loca {
            continue;
        }

        let rel_path = localization_rel_path(package_path);
        let byte_limit = usize::try_from(entry.effective_size())
            .map_err(|_| format!("Pak entry too large to materialize: {package_path}"))?;
        let mut source = reader.open_entry(entry).map_err(|e| e.to_string())?;
        let bytes = source
            .read_to_end_with_limit(byte_limit)
            .map_err(|e| e.to_string())?;

        files.push(FileEntry::from_bytes(
            rel_path,
            FileType::Loca,
            "English".to_string(),
            TargetDb::Base,
            module_priority("English"),
            bytes,
        ));
    }

    Ok(files)
}

fn localization_rel_path(package_path: &str) -> String {
    let normalized = package_path.replace('\\', "/");
    let relative = normalized
        .strip_prefix("Localization/")
        .unwrap_or(&normalized);
    format!("Localization/{relative}")
}

#[cfg(test)]
fn localization_output_path(loca_dir: &Path, package_path: &str) -> PathBuf {
    let normalized = package_path.replace('\\', "/");
    let relative = normalized
        .strip_prefix("Localization/")
        .unwrap_or(&normalized);

    let mut output = loca_dir.to_path_buf();
    for segment in relative.split('/').filter(|segment| !segment.is_empty()) {
        output.push(segment);
    }
    output
}

#[cfg(test)]
fn package_output_path(dest_dir: &Path, package_path: &str) -> PathBuf {
    let normalized = package_path.replace('\\', "/");

    let mut output = dest_dir.to_path_buf();
    for segment in normalized.split('/').filter(|segment| !segment.is_empty()) {
        output.push(segment);
    }
    output
}

fn build_pipeline_filter() -> Result<PakEntryFilter, String> {
    PakEntryFilter::reference_db_data(ALL_PUBLIC_ROOTS, EXTRACT_EXTS)
}

/// Path segments that indicate timeline/cinematic data — skip at collection
/// time to avoid ingesting tens of thousands of massive files that the builder
/// would also skip at the region level (see `SKIP_REGIONS`).
const SKIP_PATH_SEGMENTS: &[&str] = &["/Timeline/", "/TimelineTemplates/"];

/// Collect all extracted files from work_dir into a FileEntry list.
///
/// This walks all pak subdirectories under work_dir looking for ingestible data
/// files and constructs FileEntry records using the same module-detection logic
/// as `reference_db::collect_files()`.
#[cfg(test)]
fn collect_extracted_files(work_dir: &Path) -> Result<Vec<FileEntry>, String> {
    let mut files = Vec::new();

    for entry in walkdir::WalkDir::new(work_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let file_type = match reference_db_file_type_for_path(path) {
            Some(file_type) => file_type,
            _ => continue,
        };

        let rel_path = path
            .strip_prefix(work_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        // Skip timeline data (massive, not needed for modding)
        if SKIP_REGIONS.iter().any(|r| rel_path.contains(r)) {
            continue;
        }
        // Skip timeline directory trees entirely (path-level guard)
        if SKIP_PATH_SEGMENTS.iter().any(|seg| rel_path.contains(seg)) {
            continue;
        }

        // The extracted path is: work_dir/{PakName}/Public/{Module}/...
        // extract_mod_name looks for Public/<module> in the rel_path
        let mod_name = extract_mod_name(&rel_path);
        let target_db = target_db_for_module(&mod_name);
        let priority = module_priority(&mod_name);

        files.push(FileEntry::from_disk(
            path.to_path_buf(),
            rel_path,
            file_type,
            mod_name,
            target_db,
            priority,
        ));
    }

    // Sort highest priority first (INSERT OR IGNORE keeps the winner)
    files.sort_by(|a, b| b.priority.cmp(&a.priority));
    Ok(files)
}

/// Count files recursively in a directory.
#[cfg(test)]
fn count_files_recursive(dir: &Path) -> usize {
    if !dir.exists() {
        return 0;
    }
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .count()
}

/// Clean up all intermediate files in work_dir (everything).
#[cfg(test)]
fn cleanup_work_dir(work_dir: &Path) -> Result<usize, String> {
    if !work_dir.exists() {
        return Ok(0);
    }

    let mut removed = 0usize;

    // Remove data files (.lsx, .lsf, .lsfx, .loca, .xml, .txt)
    for entry in walkdir::WalkDir::new(work_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if matches!(ext, "lsx" | "lsf" | "lsfx" | "loca" | "xml" | "txt")
            && fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }

    // Clean up empty directories (bottom-up)
    let mut dirs: Vec<PathBuf> = walkdir::WalkDir::new(work_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .map(|e| e.into_path())
        .collect();
    dirs.sort_by_key(|b| std::cmp::Reverse(b.components().count()));
    for d in &dirs {
        if d.as_path() == work_dir {
            continue;
        }
        if fs::read_dir(d).is_ok_and(|mut e| e.next().is_none()) {
            let _ = fs::remove_dir(d);
        }
    }

    Ok(removed)
}

/// Emit a progress event.
fn emit_progress<F: Fn(PipelineProgress)>(
    on_progress: &F,
    start: &Instant,
    phase: String,
    detail: String,
    percent: i32,
) {
    tracing::info!("[pipeline] {} {}", phase, detail);
    on_progress(PipelineProgress {
        phase,
        detail,
        percent,
        elapsed_secs: start.elapsed().as_secs_f64(),
    });
}

/// Calculate progress percentage for a step within a phase.
fn progress_pct(current: usize, total: usize) -> i32 {
    if total == 0 {
        return -1;
    }
    // Extraction is 0-50%, localization 50-60%, collect 60%,
    // base populate 60-85%, honor 85-95%, cleanup 95-100%
    let extraction_pct = (current as f64 / total as f64 * 50.0) as i32;
    extraction_pct.min(50)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reference_db::FileType;

    #[test]
    fn test_pipeline_filter_matches_honour() {
        let filter = build_pipeline_filter().unwrap();

        // Honour paths (inside Gustav.pak) must match
        assert!(
            filter.matches_path("Public/Honour/Stats/Generated/Data/Passive.txt"),
            "Honour Stats should match"
        );
        assert!(
            filter.matches_path("Public/Honour/RootTemplates/SomeTemplate.lsf"),
            "Honour RootTemplates should match"
        );

        // HonourX paths (inside GustavX.pak) must match
        assert!(
            filter.matches_path("Public/HonourX/Stats/Generated/Data/Spell_Projectile.txt"),
            "HonourX Stats should match"
        );

        // Regular vanilla paths must match
        assert!(
            filter.matches_path("Public/Shared/Progressions/Progressions.lsf"),
            "Shared Progressions should match"
        );
        assert!(
            filter.matches_path("Public/Gustav/Races/Races.lsf"),
            "Gustav Races should match"
        );
        assert!(
            filter.matches_path("Public/GustavX/Feats/FeatDescriptions.lsf"),
            "GustavX FeatDescriptions should match"
        );

        // Stats paths must match
        assert!(
            filter.matches_path("Public/Gustav/Stats/Generated/Data/Armor.txt"),
            "Gustav Stats should match"
        );
        assert!(
            filter.matches_path("Public/SharedDev/Stats/Generated/Structure/ValueLists.txt"),
            "SharedDev Stats Structure should match"
        );

        // DiceSet paths must match
        assert!(
            filter.matches_path("Public/DiceSet_01/CustomDice/DiceSet01.lsf"),
            "DiceSet CustomDice should match"
        );

        // Engine paths must match
        assert!(
            filter.matches_path("Public/Engine/Content/UIBase.lsf"),
            "Engine Content should match"
        );

        // .lsx files should also match (already converted)
        assert!(
            filter.matches_path("Public/Shared/Progressions/Progressions.lsx"),
            "Already-converted .lsx should match"
        );

        // Subfolders NOT in VANILLA_SUBFOLDERS must now match
        // (open regex — no subfolder whitelist)
        assert!(
            filter.matches_path("Public/Gustav/AI/something.lsf"),
            "AI subfolder should match (previously missed)"
        );
        assert!(
            filter.matches_path("Public/GustavDev/ApprovalRatings/data.lsf"),
            "ApprovalRatings should match (previously missed)"
        );
        assert!(
            filter.matches_path("Public/Gustav/Gossips/gossips.lsf"),
            "Gossips should match (previously missed)"
        );
        assert!(
            filter.matches_path("Public/SharedDev/TadpolePowers/powers.lsf"),
            "TadpolePowers should match (previously missed)"
        );
        assert!(
            filter.matches_path("Public/SharedDev/CrowdCharacters/crowd.lsf"),
            "CrowdCharacters should match (previously missed)"
        );
        assert!(
            filter.matches_path("Public/Shared/ErrorDescriptions/errors.lsf"),
            "ErrorDescriptions should match (previously missed)"
        );
        assert!(
            filter.matches_path("Public/Game/Hints/hints.lsf"),
            "Hints should match (previously missed)"
        );
        assert!(
            filter.matches_path("Public/PhotoMode/PhotoMode/data.lsf"),
            "PhotoMode/PhotoMode should match (previously missed)"
        );

        // Timeline paths DO match regex (filtered at collection time, not extraction)
        assert!(
            filter.matches_path("Public/Gustav/Timeline/scene.lsf"),
            "Timeline extraction OK (filtered at collection)"
        );

        // Paths that should NOT match
        assert!(
            !filter.matches_path("Generated/Shared/something.lsf"),
            "Non-Public paths should not match"
        );
        assert!(
            !filter.matches_path("Public/Gustav/Progressions/texture.dds"),
            "Asset extensions (.dds) should not match"
        );
        assert!(
            !filter.matches_path("Public/Gustav/Progressions/model.GR2"),
            "Asset extensions (.GR2) should not match"
        );
        assert!(
            !filter.matches_path("Public/Shared/Progressions/image.png"),
            "Asset extensions (.png) should not match"
        );
        assert!(
            !filter.matches_path("Public/Shared/Assets/something.bnk"),
            ".bnk soundbanks should not match"
        );

        // .lsfx Effect files must match (binary Effects format, converted via --input-format lsf)
        assert!(
            filter.matches_path("Public/Shared/Assets/Effects/Effects_Banks/VFX_Cast.lsfx"),
            "Effects .lsfx in Shared should match"
        );
        assert!(
            filter.matches_path("Public/SharedDev/Assets/Effects/VFX_Status.lsfx"),
            "Effects .lsfx in SharedDev should match"
        );
        assert!(
            filter.matches_path("Public/GustavX/Assets/Effects/VFX_Camp.lsfx"),
            "Effects .lsfx in GustavX should match"
        );
    }

    #[test]
    fn test_pipeline_filter_exposes_all_public_roots() {
        let filter = build_pipeline_filter().unwrap();
        let regex_str = filter.regex_pattern().unwrap();
        // Every root in ALL_PUBLIC_ROOTS should appear in the regex
        for root in ALL_PUBLIC_ROOTS {
            let escaped = regex::escape(root);
            assert!(
                regex_str.contains(&escaped),
                "Regex should contain root: {root}"
            );
        }
    }

    #[test]
    fn test_collect_extracted_files_routes_honour() {
        // Create a temp dir mimicking extracted pak layout
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();

        // Create Gustav/Public/Honour/Stats/Generated/Data/Passive.txt
        let honour_stats = work.join("Gustav/Public/Honour/Stats/Generated/Data");
        fs::create_dir_all(&honour_stats).unwrap();
        fs::write(
            honour_stats.join("Passive.txt"),
            "new entry \"Test\"\ntype \"PassiveData\"\n",
        )
        .unwrap();

        // Create Gustav/Public/Gustav/Progressions/Progressions.lsf
        let gustav_prog = work.join("Gustav/Public/Gustav/Progressions");
        fs::create_dir_all(&gustav_prog).unwrap();
        fs::write(gustav_prog.join("Progressions.lsf"), b"binary").unwrap();

        // Create Localization/english.loca
        let loca = work.join("Localization/English");
        fs::create_dir_all(&loca).unwrap();
        fs::write(loca.join("english.loca"), b"LOCA\0\0\0\0\x0C\0\0\0").unwrap();

        let files = collect_extracted_files(work).unwrap();

        // Find the honour file and verify routing
        let honour_file = files.iter().find(|f| f.mod_name == "Honour");
        assert!(honour_file.is_some(), "Should find Honour file");
        let hf = honour_file.unwrap();
        assert_eq!(
            hf.target_db,
            TargetDb::Honor,
            "Honour should route to Honor DB"
        );
        assert_eq!(hf.file_type, FileType::Stats);

        // Find the Gustav file
        let gustav_file = files.iter().find(|f| f.mod_name == "Gustav");
        assert!(gustav_file.is_some(), "Should find Gustav file");
        let gf = gustav_file.unwrap();
        assert_eq!(
            gf.target_db,
            TargetDb::Base,
            "Gustav should route to Base DB"
        );
        assert_eq!(gf.file_type, FileType::Lsx);

        // Find the loca file
        let loca_file = files.iter().find(|f| f.file_type == FileType::Loca);
        assert!(loca_file.is_some(), "Should find loca file");
    }

    #[test]
    fn test_localization_output_path_strips_duplicate_localization_prefix() {
        let root = Path::new("work/Localization");

        assert_eq!(
            localization_output_path(root, "Localization/English/english.loca"),
            PathBuf::from("work/Localization/English/english.loca")
        );
        assert_eq!(
            localization_output_path(root, "English/english.loca"),
            PathBuf::from("work/Localization/English/english.loca")
        );
    }

    #[test]
    fn test_package_output_path_preserves_package_segments() {
        let root = Path::new("work/Gustav");

        assert_eq!(
            package_output_path(root, "Public/Gustav/Progressions/Progressions.lsx"),
            PathBuf::from("work/Gustav/Public/Gustav/Progressions/Progressions.lsx")
        );
        assert_eq!(
            package_output_path(root, "Public\\Gustav\\Stats\\Generated\\Data\\Passive.txt"),
            PathBuf::from("work/Gustav/Public/Gustav/Stats/Generated/Data/Passive.txt")
        );
    }

    #[test]
    #[ignore = "requires workspace-root English.pak copied into the repository"]
    fn test_extract_loca_entries_native_real_workspace_pak() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let pak_path = root.join("English.pak");
        assert!(pak_path.exists(), "missing {}", pak_path.display());

        let tmp = tempfile::tempdir().unwrap();
        let loca_dir = tmp.path().join("Localization");
        let count = extract_loca_entries_native(&pak_path, &loca_dir).unwrap();

        assert!(count > 0, "expected at least one extracted .loca file");
        let extracted_files = count_files_recursive(&loca_dir);
        assert_eq!(count, extracted_files);
    }

    #[test]
    #[ignore = "requires workspace-root Gustav.pak copied into the repository"]
    fn test_extract_data_entries_native_real_workspace_gustav_pak() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let pak_path = root.join("Gustav.pak");
        assert!(pak_path.exists(), "missing {}", pak_path.display());

        let tmp = tempfile::tempdir().unwrap();
        let dest_dir = tmp.path().join("Gustav");
        let filter = build_pipeline_filter().unwrap();
        let count = extract_data_entries_native(&pak_path, &dest_dir, &filter).unwrap();

        assert!(count > 0, "expected extracted data files from Gustav.pak");
        assert!(dest_dir.join("Public/Gustav").exists() || dest_dir.join("Public/Honour").exists());

        let files = collect_extracted_files(tmp.path()).unwrap();
        assert!(
            !files.is_empty(),
            "expected collected file entries after extraction"
        );
        assert!(files
            .iter()
            .any(|file| file.file_type == FileType::Stats || file.file_type == FileType::Lsx));
    }

    #[test]
    fn test_collect_extracted_files_skips_timeline() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();

        // Create a timeline file that should be skipped
        let timeline = work.join("Gustav/Public/Gustav/TimelineContent");
        fs::create_dir_all(&timeline).unwrap();
        fs::write(timeline.join("scene.lsx"), "<xml/>").unwrap();

        // Create a regular file that should be kept
        let regular = work.join("Gustav/Public/Gustav/Progressions");
        fs::create_dir_all(&regular).unwrap();
        fs::write(regular.join("Progressions.lsx"), "<xml/>").unwrap();

        let files = collect_extracted_files(work).unwrap();

        assert_eq!(files.len(), 1, "Only non-timeline file should be collected");
        assert!(
            files[0].rel_path.contains("Progressions"),
            "Should keep Progressions"
        );
    }

    #[test]
    fn test_collect_skips_timeline_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();

        // Timeline directory files should be skipped by SKIP_PATH_SEGMENTS
        let timeline = work.join("Gustav/Public/Gustav/Timeline/Scenes");
        fs::create_dir_all(&timeline).unwrap();
        fs::write(timeline.join("act1_scene.lsx"), "<xml/>").unwrap();

        let templates = work.join("Gustav/Public/GustavDev/TimelineTemplates");
        fs::create_dir_all(&templates).unwrap();
        fs::write(templates.join("template.lsx"), "<xml/>").unwrap();

        // Non-timeline file should be kept
        let regular = work.join("Gustav/Public/Gustav/AI");
        fs::create_dir_all(&regular).unwrap();
        fs::write(regular.join("ai_data.lsx"), "<xml/>").unwrap();

        let files = collect_extracted_files(work).unwrap();
        assert_eq!(files.len(), 1, "Only non-timeline file should be collected");
        assert!(files[0].rel_path.contains("AI"), "Should keep AI data");
    }

    #[test]
    fn test_collect_priority_ordering() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();

        // Create files from different modules
        for (module, _) in &[("Shared", 10), ("Gustav", 30), ("GustavX", 50)] {
            let dir = work.join(format!("{module}/Public/{module}/Progressions"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("Progressions.lsx"), "<xml/>").unwrap();
        }

        let files = collect_extracted_files(work).unwrap();
        assert_eq!(files.len(), 3);

        // Files should be sorted highest priority first
        assert_eq!(files[0].mod_name, "GustavX", "Highest priority first");
        assert_eq!(files[1].mod_name, "Gustav", "Second highest");
        assert_eq!(files[2].mod_name, "Shared", "Lowest priority last");
    }

    #[test]
    fn test_cleanup_work_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();

        // Create various file types
        let dir = work.join("Gustav/Public/Gustav/Progressions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("file.lsx"), "data").unwrap();
        fs::write(dir.join("file.lsf"), "data").unwrap();
        fs::write(dir.join("file.lsfx"), "data").unwrap();
        fs::write(dir.join("file.txt"), "data").unwrap();
        fs::write(dir.join("file.png"), "data").unwrap(); // Should NOT be removed

        let removed = cleanup_work_dir(work).unwrap();
        assert_eq!(
            removed, 4,
            "Should remove .lsx, .lsf, .lsfx, .txt but not .png"
        );

        // .png should still exist
        assert!(dir.join("file.png").exists(), ".png should survive cleanup");
    }

    #[test]
    fn test_collect_extracted_files_includes_both_lsefx_and_converted_lsfx_lsx() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();

        let runtime_dir = work.join("Effects/Public/Shared/Assets/Effects/Effects_Banks");
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(runtime_dir.join("keep.lsfx.lsx"), "<save />").unwrap();

        let toolkit_dir =
            work.join("Effects/Public/Shared/Content/Assets/Effects/Effects/Actions/[PAK]_Cast");
        fs::create_dir_all(&toolkit_dir).unwrap();
        fs::write(toolkit_dir.join("effect.lsefx"), "toolkit source").unwrap();

        let files = collect_extracted_files(work).unwrap();
        assert_eq!(
            files.len(),
            2,
            "expected both .lsefx and .lsfx.lsx to be collected"
        );
        assert!(files
            .iter()
            .any(|f| f.file_type == FileType::Lsx && f.rel_path.ends_with("keep.lsfx.lsx")));
        assert!(files
            .iter()
            .any(|f| f.file_type == FileType::Effect && f.rel_path.ends_with("effect.lsefx")));
    }

    #[test]
    fn test_collect_extracted_files_prefers_runtime_lsfx_over_converted_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();

        let runtime_dir = work.join("Effects/Public/Shared/Assets/Effects/Effects_Banks");
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(runtime_dir.join("keep.lsfx"), b"LSOF").unwrap();
        fs::write(runtime_dir.join("keep.lsfx.lsx"), "<save />").unwrap();

        let files = collect_extracted_files(work).unwrap();
        assert_eq!(
            files.len(),
            1,
            "expected converted lsfx.lsx sibling to be skipped"
        );
        assert!(files[0].rel_path.ends_with("keep.lsfx"));
    }

    #[test]
    #[ignore = "requires unpacked BG3 game data in UnpackedData/"]
    fn test_pipeline_shaped_lsfx_fixture_builds_reference_db() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let source_lsfx = root.join(
            "UnpackedData/Effects/Public/SharedDev/Assets/Effects/Effects_Banks/Status/VFX_Status_Apostle_ChillTouch_BodyFX_Apply_01.lsfx"
        );
        assert!(source_lsfx.is_file(), "missing {}", source_lsfx.display());

        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();

        let effects_dir = work.join("Effects/Public/SharedDev/Assets/Effects/Effects_Banks/Status");
        fs::create_dir_all(&effects_dir).unwrap();
        fs::copy(
            &source_lsfx,
            effects_dir.join(source_lsfx.file_name().unwrap()),
        )
        .unwrap();

        let files = collect_extracted_files(work).unwrap();
        assert_eq!(files.len(), 1, "expected one native .lsfx fixture file");
        assert!(files[0].rel_path.ends_with(".lsfx"));

        let schema = crate::reference_db::discovery::discover_schema(&files, work)
            .expect("discover_schema failed");
        let db_path = tmp.path().join("pipeline_lsfx_fixture.sqlite");
        crate::reference_db::builder::create_schema_db(&db_path, &schema)
            .expect("create_schema_db failed");

        let summary = crate::reference_db::builder::populate_db(
            &db_path,
            &files,
            work,
            &crate::reference_db::BuildOptions::default(),
        )
        .expect("populate_db failed");

        assert_eq!(summary.file_errors, 0, "unexpected file errors");
        assert_eq!(summary.row_errors, 0, "unexpected row errors");
        assert!(
            summary.total_rows > 0,
            "expected native .lsfx fixture rows to be inserted"
        );

        let conn = rusqlite::Connection::open(&db_path).expect("open lsfx fixture db");
        let source_files: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _source_files WHERE file_id > 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            source_files, 1,
            "expected one source row for the .lsfx fixture"
        );

        let populated_tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _table_meta WHERE row_count > 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            populated_tables > 0,
            "expected at least one populated table from the .lsfx fixture"
        );
    }

    #[test]
    #[ignore = "requires unpacked BG3 game data in UnpackedData/"]
    fn test_pipeline_shaped_fixture_builds_reference_db() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let source_lsf = root.join(
            "UnpackedData/Gustav/Public/Gustav/Tags/3549f056-0826-45ee-a8ae-351449b70fe3.lsf",
        );
        assert!(source_lsf.is_file(), "missing {}", source_lsf.display());

        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();

        let tags_dir = work.join("Gustav/Public/Gustav/Tags");
        fs::create_dir_all(&tags_dir).unwrap();
        fs::copy(&source_lsf, tags_dir.join(source_lsf.file_name().unwrap())).unwrap();

        let stats_dir = work.join("Gustav/Public/Gustav/Stats/Generated/Data");
        fs::create_dir_all(&stats_dir).unwrap();
        fs::write(
            stats_dir.join("Passive.txt"),
            "new entry \"PipelineFixturePassive\"\ntype \"PassiveData\"\ndata \"DisplayName\" \"hpipelinefixturename;1\"\n",
        )
        .unwrap();

        let loca_dir = work.join("Localization/English");
        fs::create_dir_all(&loca_dir).unwrap();
        fs::write(
            loca_dir.join("fixture.xml"),
            r#"<?xml version="1.0" encoding="utf-8"?>
<contentList>
  <content contentuid="hpipelinefixturename" version="1">Pipeline Fixture Passive</content>
</contentList>
"#,
        )
        .unwrap();

        let files = collect_extracted_files(work).unwrap();
        assert_eq!(files.len(), 3, "expected .lsf + .txt + .xml fixture files");

        let schema = crate::reference_db::discovery::discover_schema(&files, work)
            .expect("discover_schema failed");
        let db_path = tmp.path().join("pipeline_fixture.sqlite");
        crate::reference_db::builder::create_schema_db(&db_path, &schema)
            .expect("create_schema_db failed");

        let summary = crate::reference_db::builder::populate_db(
            &db_path,
            &files,
            work,
            &crate::reference_db::BuildOptions::default(),
        )
        .expect("populate_db failed");

        assert_eq!(summary.file_errors, 0, "unexpected file errors");
        assert_eq!(summary.row_errors, 0, "unexpected row errors");
        assert!(
            summary.total_rows >= 3,
            "expected fixture rows to be inserted"
        );

        let conn = rusqlite::Connection::open(&db_path).expect("open fixture db");
        let source_files: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _source_files WHERE file_id > 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            source_files, 3,
            "expected one source row per collected file"
        );

        let tags_table: String = conn
            .query_row(
                "SELECT table_name FROM _table_meta WHERE region_id = 'Tags' AND row_count > 0 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("find tags table");
        let tags_rows: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM \"{tags_table}\""),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(tags_rows > 0, "expected pipeline-shaped .lsf fixture rows");

        let passive_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM \"stats__PassiveData\" WHERE _entry_name = 'PipelineFixturePassive'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(passive_rows, 1, "expected fixture passive row");

        let loca_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM \"loca__english\" WHERE contentuid = 'hpipelinefixturename' AND text = 'Pipeline Fixture Passive'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(loca_rows, 1, "expected fixture localization row");
    }

    #[test]
    fn pipeline_progress_serializes_correctly() {
        let progress = PipelineProgress {
            phase: "Extracting Gustav.pak".to_string(),
            detail: "1/5".to_string(),
            percent: 20,
            elapsed_secs: 3.125,
        };

        let json = serde_json::to_string(&progress).unwrap();
        assert!(json.contains("\"phase\":\"Extracting Gustav.pak\""));
        assert!(json.contains("\"percent\":20"));
        assert!(json.contains("\"detail\":\"1/5\""));

        // Verify it round-trips through serde
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["phase"], "Extracting Gustav.pak");
        assert_eq!(parsed["percent"], 20);
    }

    #[test]
    fn pipeline_progress_indeterminate() {
        let progress = PipelineProgress {
            phase: "Collecting files".to_string(),
            detail: String::new(),
            percent: -1,
            elapsed_secs: 0.0,
        };

        let json = serde_json::to_string(&progress).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["percent"], -1);
    }

    #[test]
    fn pipeline_summary_success_case() {
        let summary = PipelineSummary {
            elapsed_secs: 120.5,
            paks_extracted: 8,
            files_extracted: 5000,
            lsf_converted: 3000,
            loca_converted: 200,
            base_summary: None,
            honor_summary: None,
            errors: vec![],
            diagnostics: vec!["All good".to_string()],
        };

        let json = serde_json::to_string(&summary).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["paks_extracted"], 8);
        assert_eq!(parsed["files_extracted"], 5000);
        assert!(parsed["errors"].as_array().unwrap().is_empty());
        assert_eq!(parsed["base_summary"], serde_json::Value::Null);
    }

    #[test]
    fn pipeline_summary_failure_case() {
        let summary = PipelineSummary {
            elapsed_secs: 5.0,
            paks_extracted: 1,
            files_extracted: 0,
            lsf_converted: 0,
            loca_converted: 0,
            base_summary: None,
            honor_summary: None,
            errors: vec![
                "Gustav.pak: file not found".to_string(),
                "Shared.pak: corrupted header".to_string(),
            ],
            diagnostics: vec![],
        };

        let json = serde_json::to_string(&summary).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["errors"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["paks_extracted"], 1);
    }

    #[test]
    fn progress_pct_calculates_correctly() {
        assert_eq!(progress_pct(0, 10), 0);
        assert_eq!(progress_pct(5, 10), 25);
        assert_eq!(progress_pct(10, 10), 50);
        // Edge case: 0 total
        assert_eq!(progress_pct(0, 0), -1);
    }
}
