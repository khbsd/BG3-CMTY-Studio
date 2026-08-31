//! Reference DB builder — creates a SQLite database from BG3 UnpackedData.
//!
//! Architecture: Two-pass build
//!   Pass 1 (discovery): Walk all files, parse XML/stats/loca, collect schema
//!                        (column names, types, PK strategy, parent-child links).
//!   Pass 2 (build):     Create all tables with proper PKs and real FOREIGN KEY
//!                        constraints, then walk files again and INSERT data.
//!
//! Primary keys:
//!   - Tables with UUID column  → UUID TEXT PRIMARY KEY
//!   - Tables with MapKey       → MapKey TEXT PRIMARY KEY
//!   - Tables with ID (banks)   → ID TEXT PRIMARY KEY
//!   - Stats tables             → _entry_name TEXT PRIMARY KEY
//!   - loca__english            → contentuid TEXT PRIMARY KEY
//!   - Child/junction tables    → INTEGER PRIMARY KEY (rowid)

pub mod builder;
pub mod cross_db;
pub mod discovery;
pub mod fk_patterns;
pub mod pipeline;
pub mod queries;
pub mod schema;
pub mod staging;
pub mod types;

use crate::models::EffectFileKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Summary returned after a successful build.
#[derive(Debug, serde::Serialize, Clone)]
pub struct BuildSummary {
    pub db_path: String,
    pub total_files: usize,
    pub total_rows: usize,
    pub total_tables: usize,
    pub fk_constraints: usize,
    pub file_errors: usize,
    pub row_errors: usize,
    pub elapsed_secs: f64,
    pub db_size_mb: f64,
    /// Per-phase timing breakdown (seconds).
    pub phase_times: PhaseTimes,
}

/// Timing for each phase of the build.
#[derive(Debug, serde::Serialize, Clone, Default)]
pub struct PhaseTimes {
    pub collect_files: f64,
    pub discovery: f64,
    pub ddl_creation: f64,
    pub data_insert: f64,
    pub merge: f64,
    pub post_process: f64,
    pub write_to_disk: f64,
    pub vacuum: f64,
}

/// Which reference DB a file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetDb {
    /// Vanilla: ref_base.sqlite  (Shared → SharedDev → Gustav → GustavDev → GustavX)
    Base,
    /// Honour mode: ref_honor.sqlite  (Honour → HonourX)
    Honor,
}

/// Options controlling the build/populate process.
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// Whether to VACUUM the database after populating.
    /// Reduces DB size by ~10-20% but adds ~14s to the build.
    pub vacuum: bool,
    /// Force on-disk build instead of in-memory.
    /// When `None`, auto-detects based on available RAM (threshold: 4 GB).
    /// Set to `Some(true)` to force on-disk (slower but uses ~100 MB instead of ~1 GB).
    pub force_disk: Option<bool>,
    /// Optional path to `ref_base.sqlite` for cross-DB honor lookups.
    ///
    /// When populating `ref_honor.sqlite`, this allows shared loca/stats/static
    /// references to resolve against base before honor adds any synthetic
    /// placeholder rows. If omitted, honor populate may infer a sibling
    /// `*base*.sqlite` path from the output DB name.
    pub fallback_base_db_path: Option<PathBuf>,
}

/// Estimate available physical memory in bytes.
/// Returns `None` if detection fails.
fn available_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        use std::mem;
        #[repr(C)]
        #[allow(non_snake_case, clippy::upper_case_acronyms)]
        struct MEMORYSTATUSEX {
            dwLength: u32,
            dwMemoryLoad: u32,
            ullTotalPhys: u64,
            ullAvailPhys: u64,
            ullTotalPageFile: u64,
            ullAvailPageFile: u64,
            ullTotalVirtual: u64,
            ullAvailVirtual: u64,
            ullAvailExtendedVirtual: u64,
        }

        extern "system" {
            fn GlobalMemoryStatusEx(lpBuffer: *mut MEMORYSTATUSEX) -> i32;
        }

        unsafe {
            let mut status: MEMORYSTATUSEX = mem::zeroed();
            status.dwLength = mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut status) != 0 {
                return Some(status.ullAvailPhys);
            }
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|info| {
                info.lines()
                    .find(|l| l.starts_with("MemAvailable:"))
                    .and_then(|l| {
                        l.split_whitespace()
                            .nth(1)?
                            .parse::<u64>()
                            .ok()
                            .map(|kb| kb * 1024)
                    })
            })
    }
    #[cfg(target_os = "macos")]
    {
        // sysctl hw.memsize gives total; rough approximation with vm_statistics
        // For simplicity, return None and let the fallback use total/2
        None
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Compute pipeline parallelism parameters based on available RAM.
/// Returns (parse_chunk_size, channel_capacity).
pub(crate) fn adaptive_pipeline_params() -> (usize, usize) {
    match available_memory_bytes() {
        Some(avail) => {
            let gb = avail / (1024 * 1024 * 1024);
            let (chunk, cap) = if gb >= 32 {
                (1500, 8)
            } else if gb >= 16 {
                (1000, 6)
            } else if gb >= 8 {
                (750, 4)
            } else {
                (500, 2)
            };
            eprintln!(
                "  Adaptive pipeline: chunk_size={}, channel_cap={} ({:.1} GB available)",
                chunk,
                cap,
                avail as f64 / (1024.0 * 1024.0 * 1024.0)
            );
            (chunk, cap)
        }
        None => {
            eprintln!("  Adaptive pipeline: defaults (RAM detection unavailable)");
            (500, 2)
        }
    }
}

/// Decide whether to use in-memory build.
/// Requires at least 4 GB of available RAM.
pub fn should_use_in_memory(options: &BuildOptions) -> bool {
    if let Some(force) = options.force_disk {
        return !force;
    }
    const MIN_RAM_BYTES: u64 = 4 * 1024 * 1024 * 1024; // 4 GB
    match available_memory_bytes() {
        Some(avail) => {
            let use_mem = avail >= MIN_RAM_BYTES;
            eprintln!(
                "  Available RAM: {:.1} GB → {}",
                avail as f64 / (1024.0 * 1024.0 * 1024.0),
                if use_mem {
                    "in-memory build"
                } else {
                    "on-disk build (low RAM)"
                }
            );
            use_mem
        }
        None => {
            eprintln!("  RAM detection unavailable — defaulting to in-memory build");
            true
        }
    }
}

/// Top-level directories under UnpackedData to scan.
const TARGET_DIRS: &[&str] = &[
    "Shared/Public",
    "Gustav/Public",
    "GustavX/Public",
    "English",
    "Effects/Public",
    "Materials/Public",
    "Engine/Public",
    "Game/Public",
];

/// Regions to skip — timeline data is massive and not needed for modding.
pub const SKIP_REGIONS: &[&str] = &[
    "TimelineContent",
    "TLScene",
    "TimelineBank",
    "TimelineSceneBank",
    "TimelineVTPrefetch",
];

/// Build the reference database from UnpackedData.
///
/// `unpacked_path` — path to the UnpackedData directory
/// `db_path`       — output SQLite file path
pub fn build_reference_db(unpacked_path: &Path, db_path: &Path) -> Result<BuildSummary, String> {
    let start = Instant::now();

    // Collect all files
    let t0 = Instant::now();
    let files = collect_files(unpacked_path)?;
    let total_files = files.len();
    let collect_secs = t0.elapsed().as_secs_f64();
    eprintln!("  Phase: collect_files  {collect_secs:.1}s  ({total_files} files)");

    // Remove existing DB
    if db_path.exists() {
        std::fs::remove_file(db_path)
            .or_else(|_| {
                let bak = db_path.with_extension("sqlite.bak");
                let _ = std::fs::remove_file(&bak);
                std::fs::rename(db_path, &bak)
            })
            .map_err(|e| format!("Cannot remove existing DB: {e}"))?;
    }
    for sfx in &["-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{}", db_path.display(), sfx));
        let _ = std::fs::remove_file(&p);
    }

    // Pass 1: Schema discovery
    let t1 = Instant::now();
    let discovered = discovery::discover_schema(&files, unpacked_path)?;
    let discovery_secs = t1.elapsed().as_secs_f64();
    eprintln!(
        "  Phase: discovery      {:.1}s  ({} tables)",
        discovery_secs,
        discovered.tables.len()
    );

    // Pass 2: Build DB
    let result = builder::build_db(db_path, &files, unpacked_path, &discovered)?;

    let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);

    let mut phase_times = result.phase_times;
    phase_times.collect_files = collect_secs;
    phase_times.discovery = discovery_secs;

    Ok(BuildSummary {
        db_path: db_path.display().to_string(),
        total_files,
        total_rows: result.total_rows,
        total_tables: result.total_tables,
        fk_constraints: result.fk_constraints,
        file_errors: result.file_errors,
        row_errors: result.row_errors,
        elapsed_secs: start.elapsed().as_secs_f64(),
        db_size_mb: db_size_bytes as f64 / (1024.0 * 1024.0),
        phase_times,
    })
}

/// Populate a pre-built schema database with data from UnpackedData.
///
/// The database at `db_path` must already exist with DDL applied and an
/// embedded schema blob (created by `generate_schema` dev tool or
/// `builder::create_schema_db`).
///
/// Only files whose `target_db` matches `target` are inserted.
/// Files are already sorted by priority (highest first) so INSERT OR IGNORE
/// keeps the highest-priority module's data on PK conflicts.
///
/// This skips discovery and DDL creation entirely — only file collection,
/// data insertion, post-processing, and optional VACUUM are performed.
pub fn populate_reference_db(
    unpacked_path: &Path,
    db_path: &Path,
    target: TargetDb,
    options: &BuildOptions,
) -> Result<BuildSummary, String> {
    let start = Instant::now();

    // Collect all files (sorted by priority, highest first)
    let t0 = Instant::now();
    let all_files = collect_files(unpacked_path)?;
    // Filter to only files targeting this DB
    let files: Vec<FileEntry> = all_files
        .into_iter()
        .filter(|f| f.target_db == target)
        .collect();
    let total_files = files.len();
    let collect_secs = t0.elapsed().as_secs_f64();
    eprintln!("  Phase: collect_files  {collect_secs:.1}s  ({total_files} files for {target:?})");

    // Populate data (schema is loaded from the DB internally)
    let result = builder::populate_db(db_path, &files, unpacked_path, options)?;

    let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);

    let mut phase_times = result.phase_times;
    phase_times.collect_files = collect_secs;
    // discovery = 0.0, ddl_creation = 0.0 (skipped)

    Ok(BuildSummary {
        db_path: db_path.display().to_string(),
        total_files,
        total_rows: result.total_rows,
        total_tables: result.total_tables,
        fk_constraints: result.fk_constraints,
        file_errors: result.file_errors,
        row_errors: result.row_errors,
        elapsed_secs: start.elapsed().as_secs_f64(),
        db_size_mb: db_size_bytes as f64 / (1024.0 * 1024.0),
        phase_times,
    })
}

/// Ingest a single mod into the mods reference database.
///
/// `mod_path`  — root directory of the unpacked mod (contains Public/, Mods/, etc.)
/// `mod_name`  — display name of the mod (used as `_SourceID` in `_sources`)
/// `db_path`   — path to `ref_mods.sqlite` (must already exist with DDL applied)
/// `options`   — build options (vacuum, etc.)
///
/// Unlike the base/honor populate path, this uses composite PKs so multiple
/// mods can coexist.  Each call appends one mod's data; call repeatedly for
/// multiple mods.
pub fn populate_mods_db(
    mod_path: &Path,
    mod_name: &str,
    db_path: &Path,
    options: &BuildOptions,
) -> Result<BuildSummary, String> {
    let start = Instant::now();

    let t0 = Instant::now();
    let files = collect_mod_files(mod_path, mod_name)?;
    let total_files = files.len();
    let collect_secs = t0.elapsed().as_secs_f64();
    eprintln!(
        "  Phase: collect_mod    {collect_secs:.1}s  ({total_files} files for mod '{mod_name}')"
    );

    let result = builder::populate_db(db_path, &files, mod_path, options)?;

    let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);

    let mut phase_times = result.phase_times;
    phase_times.collect_files = collect_secs;

    Ok(BuildSummary {
        db_path: db_path.display().to_string(),
        total_files,
        total_rows: result.total_rows,
        total_tables: result.total_tables,
        fk_constraints: result.fk_constraints,
        file_errors: result.file_errors,
        row_errors: result.row_errors,
        elapsed_secs: start.elapsed().as_secs_f64(),
        db_size_mb: db_size_bytes as f64 / (1024.0 * 1024.0),
        phase_times,
    })
}

/// Collect processable files from an unpacked mod directory.
///
/// Unlike `collect_files` (which scans specific UnpackedData subdirs),
/// this walks the entire mod directory for .lsx/.lsf/.txt/.loca/.xml files.
/// All files are assigned the given `mod_name` and `TargetDb::Base` target
/// (the target is irrelevant for the mods DB — all files go in).
fn collect_mod_files(mod_path: &Path, mod_name: &str) -> Result<Vec<FileEntry>, String> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(mod_path)
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
            .strip_prefix(mod_path)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        // Skip timeline data
        if SKIP_REGIONS.iter().any(|r| rel_path.contains(r)) {
            continue;
        }

        files.push(FileEntry::from_disk(
            path.to_path_buf(),
            rel_path,
            file_type,
            mod_name.to_string(),
            TargetDb::Base, // irrelevant for mods DB
            50,             // single mod — no priority conflicts
        ));
    }
    Ok(files)
}

/// Collect reference-DB–relevant files from a single mod `.pak` archive.
///
/// Streams every entry through `reference_db_file_type_for_path`, reads
/// matching entries into memory, and returns the `FileEntry` list ready for
/// `builder::populate_db`.
///
/// Also returns `has_public_folder` — `true` when any entry path starts with
/// `Public/`.
pub fn collect_mod_files_from_pak(
    pak_path: &Path,
    mod_name: &str,
) -> Result<(Vec<FileEntry>, bool), String> {
    use crate::pak::PakReader;

    let reader = PakReader::open(pak_path).map_err(|e| format!("Failed to open pak: {e}"))?;

    let mut files = Vec::new();
    let mut has_public = false;

    for entry in reader.entries() {
        if entry.is_deleted() {
            continue;
        }

        let package_path = entry.path.as_str();

        // Detect Public/ folder presence
        if !has_public && package_path.starts_with("Public/") {
            has_public = true;
        }

        let rel_path = package_path.replace('\\', "/");

        // Skip timeline data
        if SKIP_REGIONS.iter().any(|r| rel_path.contains(r)) {
            continue;
        }

        let file_type = match reference_db_file_type_for_path(Path::new(package_path)) {
            Some(ft) => ft,
            None => continue,
        };

        let byte_limit = usize::try_from(entry.effective_size())
            .map_err(|_| format!("Pak entry too large: {package_path}"))?;
        let mut source = reader.open_entry(entry).map_err(|e| e.to_string())?;
        let bytes = source
            .read_to_end_with_limit(byte_limit)
            .map_err(|e| e.to_string())?;

        files.push(FileEntry::from_bytes(
            rel_path,
            file_type,
            mod_name.to_string(),
            TargetDb::Base, // irrelevant for mods DB
            50,             // single mod — no priority conflicts
            bytes,
        ));
    }

    Ok((files, has_public))
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub abs_path: PathBuf,
    pub rel_path: String,
    pub file_type: FileType,
    pub bytes: Option<Arc<[u8]>>,
    /// Which module this file belongs to (e.g. "Shared", "Gustav", "Honour").
    pub mod_name: String,
    /// Which reference DB this file should be inserted into.
    pub target_db: TargetDb,
    /// Module priority (higher = wins on PK conflict).  Files are processed
    /// highest-priority-first so that INSERT OR IGNORE keeps the winner.
    pub priority: u8,
}

impl FileEntry {
    pub fn from_disk(
        abs_path: PathBuf,
        rel_path: String,
        file_type: FileType,
        mod_name: String,
        target_db: TargetDb,
        priority: u8,
    ) -> Self {
        Self {
            abs_path,
            rel_path,
            file_type,
            bytes: None,
            mod_name,
            target_db,
            priority,
        }
    }

    pub fn from_bytes(
        rel_path: String,
        file_type: FileType,
        mod_name: String,
        target_db: TargetDb,
        priority: u8,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            abs_path: PathBuf::from(&rel_path),
            rel_path,
            file_type,
            bytes: Some(Arc::from(bytes.into_boxed_slice())),
            mod_name,
            target_db,
            priority,
        }
    }

    pub fn extension(&self) -> Option<String> {
        Path::new(&self.rel_path)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .or_else(|| {
                self.abs_path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.to_ascii_lowercase())
            })
    }

    pub fn file_size(&self) -> i64 {
        if let Some(bytes) = &self.bytes {
            bytes.len() as i64
        } else {
            std::fs::metadata(&self.abs_path)
                .map(|metadata| metadata.len() as i64)
                .unwrap_or(0)
        }
    }

    pub fn in_memory_bytes(&self) -> Option<&[u8]> {
        self.bytes.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Lsx,
    Stats,
    Loca,
    /// AllSpark definition files (ComponentDefinition.xcd, ModuleDefinition.xmd)
    AllSpark,
    /// Toolkit effect source files (.lsefx)
    Effect,
}

impl FileType {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileType::Lsx => "lsx",
            FileType::Stats => "stats",
            FileType::Loca => "loca",
            FileType::AllSpark => "allspark",
            FileType::Effect => "effect",
        }
    }
}

/// Classify a file path for direct reference DB ingestion.
///
/// Runtime `.lsfx` binaries are ingested natively. Converted `.lsfx.lsx`
/// artifacts remain a fallback for inspection workflows, but are skipped when a
/// sibling runtime `.lsfx` file already exists to avoid double-ingesting the
/// same effect. Toolkit `.lsefx` sources are ingested as structured effect data.
/// AllSpark definition files (`.xcd`, `.xmd`) provide component/module metadata.
pub(crate) fn reference_db_file_type_for_path(path: &Path) -> Option<FileType> {
    match EffectFileKind::from_path(path) {
        Some(EffectFileKind::RuntimeLsfxConvertedLsx) if has_runtime_lsfx_sibling(path) => None,
        Some(EffectFileKind::RuntimeLsfxBinary | EffectFileKind::RuntimeLsfxConvertedLsx) => {
            Some(FileType::Lsx)
        }
        Some(EffectFileKind::ToolkitLsefxSource) => Some(FileType::Effect),
        None => match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("lsx" | "lsf") => Some(FileType::Lsx),
            Some("txt") => Some(FileType::Stats),
            Some("loca" | "xml") => Some(FileType::Loca),
            Some("xcd" | "xmd") => Some(FileType::AllSpark),
            _ => None,
        },
    }
}

fn has_runtime_lsfx_sibling(path: &Path) -> bool {
    let file_name = match path.file_name().and_then(|name| name.to_str()) {
        Some(file_name) => file_name,
        None => return false,
    };

    if !file_name.to_ascii_lowercase().ends_with(".lsfx.lsx") {
        return false;
    }

    let sibling_name = &file_name[..file_name.len() - ".lsx".len()];
    path.parent()
        .map(|parent| parent.join(sibling_name).is_file())
        .unwrap_or(false)
}

/// Collect all processable files from UnpackedData.
pub fn collect_files(unpacked_path: &Path) -> Result<Vec<FileEntry>, String> {
    let mut files = Vec::new();
    for abs_dir in collect_scan_roots(unpacked_path) {
        if !abs_dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&abs_dir)
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
                .strip_prefix(unpacked_path)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");

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
    }

    // Sort highest priority first so INSERT OR IGNORE keeps the winner
    files.sort_by(|a, b| b.priority.cmp(&a.priority));

    Ok(files)
}

fn collect_scan_roots(unpacked_path: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = TARGET_DIRS
        .iter()
        .map(|dir| unpacked_path.join(dir))
        .collect();

    if let Ok(entries) = std::fs::read_dir(unpacked_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let mods_dir = path.join("Mods");
            if mods_dir.is_dir() {
                roots.push(mods_dir);
            }
        }
    }

    roots.sort();
    roots.dedup();
    roots
}

/// Collect AllSpark definition files (XCD/XMD) and toolkit effect files (.lsefx)
/// from the game's Editor directory, which is a sibling of the Data directory
/// containing paks.
///
/// Layout relative to `data_dir`:
///   - `Editor/Config/AllSpark/*.xcd` / `*.xmd`
///   - `Editor/Mods/*/Assets/Effects/**/*.lsefx`
pub fn collect_editor_files(data_dir: &Path) -> Result<Vec<FileEntry>, String> {
    let editor_dir = data_dir.join("Editor");
    if !editor_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();

    // AllSpark config files
    let allspark_dir = editor_dir.join("Config").join("AllSpark");
    if allspark_dir.is_dir() {
        for entry in walkdir::WalkDir::new(&allspark_dir)
            .follow_links(false)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let file_type = match reference_db_file_type_for_path(path) {
                Some(ft) => ft,
                None => continue,
            };
            let rel_path = path
                .strip_prefix(data_dir)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            files.push(FileEntry::from_disk(
                path.to_path_buf(),
                rel_path,
                file_type,
                "AllSpark".to_string(),
                TargetDb::Base,
                10, // low priority — definition data
            ));
        }
    }

    // Toolkit effect files (.lsefx) from Editor/Mods/*/
    let editor_mods = editor_dir.join("Mods");
    if editor_mods.is_dir() {
        for entry in walkdir::WalkDir::new(&editor_mods)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let file_type = match reference_db_file_type_for_path(path) {
                Some(ft) => ft,
                None => continue,
            };
            let rel_path = path
                .strip_prefix(data_dir)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let mod_name = extract_editor_mod_name(&rel_path);
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
    }

    Ok(files)
}

/// Extract the module name from an Editor/Mods/... relative path.
/// e.g. "Editor/Mods/Shared/Assets/Effects/foo.lsefx" → "Shared"
fn extract_editor_mod_name(rel_path: &str) -> String {
    let parts: Vec<&str> = rel_path.split('/').collect();
    // Editor / Mods / <ModName> / ...
    if parts.len() >= 3 && parts[0] == "Editor" && parts[1] == "Mods" {
        return parts[2].to_string();
    }
    "Unknown".to_string()
}

/// Extract mod name from a relative path.
pub fn extract_mod_name(rel_path: &str) -> String {
    let parts: Vec<&str> = rel_path.split('/').collect();
    if let Some(pub_idx) = parts.iter().position(|&p| p == "Public") {
        if parts.len() > pub_idx + 1 {
            return parts[pub_idx + 1].to_string();
        }
    }
    if parts.first() == Some(&"English") {
        return "English".to_string();
    }
    parts.first().unwrap_or(&"Unknown").to_string()
}

/// Sanitize an identifier for use as a SQL table/column name.
pub fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Module priority for override ordering.  Higher value = higher priority.
/// When two files define the same PK, the higher-priority module wins.
///
/// Vanilla chain: Shared(10) → SharedDev(20) → Gustav(30) → GustavDev(40) → GustavX(50)
/// Honour chain:  Honour(10) → HonourX(20)
/// Auxiliary modules get low base priority (won't conflict with the main chain).
pub fn module_priority(mod_name: &str) -> u8 {
    match mod_name {
        // Main vanilla override chain
        "Shared" => 10,
        "SharedDev" => 20,
        "Gustav" => 30,
        "GustavDev" => 40,
        "GustavX" => 50,
        // Honour chain
        "Honour" => 10,
        "HonourX" => 20,
        // Auxiliary modules (no conflicts expected with main chain)
        "Engine" => 5,
        "Game" => 5,
        "PhotoMode" => 5,
        "English" => 5,
        // Unknown modules get lowest priority
        _ => 1,
    }
}

/// Determine which reference DB a module's files belong to.
pub fn target_db_for_module(mod_name: &str) -> TargetDb {
    match mod_name {
        "Honour" | "HonourX" => TargetDb::Honor,
        _ => TargetDb::Base,
    }
}

#[cfg(test)]
mod tests {
    use super::{collect_files, collect_mod_files, reference_db_file_type_for_path, FileType};
    use std::path::Path;

    #[test]
    fn reference_db_file_type_accepts_toolkit_lsefx_source() {
        let path = Path::new(
            r"Public/TestMod/Content/Assets/Effects/Effects/Actions/[PAK]_Cast/SpellCast_A.lsefx",
        );
        assert_eq!(
            reference_db_file_type_for_path(path),
            Some(FileType::Effect)
        );
    }

    #[test]
    fn reference_db_file_type_accepts_converted_runtime_lsfx_lsx() {
        let path = Path::new(r"Public/TestMod/Assets/Effects/Effects_Banks/SpellCast_A.lsfx.lsx");
        assert_eq!(reference_db_file_type_for_path(path), Some(FileType::Lsx));
    }

    #[test]
    fn reference_db_file_type_accepts_runtime_lsfx_binary() {
        let path = Path::new(r"Public/TestMod/Assets/Effects/Effects_Banks/SpellCast_A.lsfx");
        assert_eq!(reference_db_file_type_for_path(path), Some(FileType::Lsx));
    }

    #[test]
    fn collect_mod_files_includes_both_lsefx_and_converted_lsfx_lsx() {
        let tmp = tempfile::tempdir().unwrap();
        let mod_root = tmp.path();

        let runtime_dir = mod_root.join("Public/TestMod/Assets/Effects/Effects_Banks");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(runtime_dir.join("SpellCast_A.lsfx.lsx"), "<save />").unwrap();

        let toolkit_dir =
            mod_root.join("Public/TestMod/Content/Assets/Effects/Effects/Actions/[PAK]_Cast");
        std::fs::create_dir_all(&toolkit_dir).unwrap();
        std::fs::write(toolkit_dir.join("SpellCast_A.lsefx"), "toolkit source").unwrap();

        let files = collect_mod_files(mod_root, "TestMod").unwrap();
        assert_eq!(
            files.len(),
            2,
            "expected both .lsefx and .lsfx.lsx to be collected"
        );
        assert!(files
            .iter()
            .any(|f| f.file_type == FileType::Lsx && f.rel_path.ends_with("SpellCast_A.lsfx.lsx")));
        assert!(files
            .iter()
            .any(|f| f.file_type == FileType::Effect && f.rel_path.ends_with("SpellCast_A.lsefx")));
    }

    #[test]
    fn collect_mod_files_prefers_runtime_lsfx_over_converted_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let mod_root = tmp.path();

        let runtime_dir = mod_root.join("Public/TestMod/Assets/Effects/Effects_Banks");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(runtime_dir.join("SpellCast_A.lsfx"), b"LSOF").unwrap();
        std::fs::write(runtime_dir.join("SpellCast_A.lsfx.lsx"), "<save />").unwrap();

        let files = collect_mod_files(mod_root, "TestMod").unwrap();
        assert_eq!(
            files.len(),
            1,
            "expected converted lsfx.lsx sibling to be skipped"
        );
        assert!(files[0].rel_path.ends_with("SpellCast_A.lsfx"));
    }

    #[test]
    fn collect_files_includes_top_level_mods_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let unpacked = tmp.path();

        let public_dir = unpacked.join("Gustav/Public/Gustav/Flags");
        std::fs::create_dir_all(&public_dir).unwrap();
        std::fs::write(public_dir.join("flag.lsx"), "<save />").unwrap();

        let mods_dir = unpacked.join("Game/Mods/PhotoMode");
        std::fs::create_dir_all(&mods_dir).unwrap();
        std::fs::write(mods_dir.join("meta.lsx"), "<save />").unwrap();

        let files = collect_files(unpacked).unwrap();
        let rel_paths: Vec<&str> = files.iter().map(|file| file.rel_path.as_str()).collect();

        assert!(rel_paths
            .iter()
            .any(|path| path == &"Gustav/Public/Gustav/Flags/flag.lsx"));
        assert!(rel_paths
            .iter()
            .any(|path| path == &"Game/Mods/PhotoMode/meta.lsx"));
    }
}
