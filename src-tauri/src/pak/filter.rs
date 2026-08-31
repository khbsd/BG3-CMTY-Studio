use regex::Regex;

use crate::pak::entry::PakEntry;
use crate::pak::path::PakPath;

pub trait EntrySelector {
    fn matches(&self, entry: &PakEntry) -> bool;
}

#[derive(Debug, Clone, Default)]
pub struct PakEntryFilter {
    extensions: Vec<String>,
    prefixes: Vec<String>,
    regex: Option<Regex>,
    regex_pattern: Option<String>,
}

impl PakEntryFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_extension(mut self, ext: impl Into<String>) -> Self {
        self.extensions.push(ext.into().to_ascii_lowercase());
        self
    }

    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefixes.push(prefix.into());
        self
    }

    pub fn with_regex(mut self, regex: Regex) -> Self {
        self.regex = Some(regex);
        self.regex_pattern = None;
        self
    }

    pub fn with_regex_pattern(mut self, pattern: impl Into<String>) -> Result<Self, regex::Error> {
        let pattern = pattern.into();
        self.regex = Some(Regex::new(&pattern)?);
        self.regex_pattern = Some(pattern);
        Ok(self)
    }

    pub fn regex_pattern(&self) -> Option<&str> {
        self.regex_pattern.as_deref()
    }

    pub fn matches_path(&self, path: &str) -> bool {
        let Ok(path) = PakPath::parse(path) else {
            return false;
        };

        if !self.extensions.is_empty() {
            let Some(ext) = path.extension() else {
                return false;
            };
            let ext = ext.to_ascii_lowercase();
            if !self.extensions.iter().any(|allowed| allowed == &ext) {
                return false;
            }
        }

        if !self.prefixes.is_empty() && !self.prefixes.iter().any(|prefix| path.starts_with(prefix))
        {
            return false;
        }

        if let Some(regex) = &self.regex {
            return regex.is_match(path.as_str());
        }

        true
    }

    pub fn reference_db_data(public_roots: &[&str], extensions: &[&str]) -> Result<Self, String> {
        let pattern = build_reference_db_data_regex(public_roots, extensions);
        Self::new()
            .with_regex_pattern(pattern)
            .map_err(|error| format!("Invalid reference DB pak filter regex: {error}"))
    }
}

impl EntrySelector for PakEntryFilter {
    fn matches(&self, entry: &PakEntry) -> bool {
        self.matches_path(entry.path.as_str())
    }
}

pub fn build_reference_db_data_regex(public_roots: &[&str], extensions: &[&str]) -> String {
    let roots_alt = public_roots
        .iter()
        .map(|root| regex::escape(root))
        .collect::<Vec<_>>()
        .join("|");
    let ext_filter = extensions.join("|");
    let ext_suffix = format!("\\.({ext_filter})$");
    let open_pattern = format!("({roots_alt})/.+{ext_suffix}");
    let diceset_pattern = format!("Public/DiceSet_[^/]+/.+{ext_suffix}");
    let meta_pattern = "Mods/[^/]+/meta\\.(lsx|lsf)".to_string();

    format!("({open_pattern}|{diceset_pattern}|{meta_pattern})")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOTS: &[&str] = &[
        "Public/Shared",
        "Public/SharedDev",
        "Public/Gustav",
        "Public/GustavDev",
        "Public/GustavX",
        "Public/Honour",
        "Public/HonourX",
        "Public/Engine",
        "Public/Game",
        "Public/PhotoMode",
    ];
    const EXTS: &[&str] = &["lsf", "lsfx", "lsx", "txt", "xml", "loca"];

    #[test]
    fn reference_db_data_filter_matches_expected_paths() {
        let filter = PakEntryFilter::reference_db_data(ROOTS, EXTS).unwrap();

        assert!(filter.matches_path("Public/Honour/Stats/Generated/Data/Passive.txt"));
        assert!(filter.matches_path("Public/HonourX/Stats/Generated/Data/Spell_Projectile.txt"));
        assert!(filter.matches_path("Public/Shared/Progressions/Progressions.lsf"));
        assert!(filter.matches_path("Public/Gustav/AI/something.lsf"));
        assert!(filter.matches_path("Public/Game/Hints/hints.lsf"));
        assert!(filter.matches_path("Public/Shared/Assets/Effects/Effects_Banks/VFX_Cast.lsfx"));
        assert!(filter.matches_path("Public/DiceSet_01/CustomDice/DiceSet01.lsf"));
        assert!(filter.matches_path("Mods/Foo/meta.lsx"));
        assert!(filter.matches_path("Mods/Foo/meta.lsf"));
    }

    #[test]
    fn reference_db_data_filter_rejects_unwanted_paths() {
        let filter = PakEntryFilter::reference_db_data(ROOTS, EXTS).unwrap();

        assert!(!filter.matches_path("Generated/Shared/something.lsf"));
        assert!(!filter.matches_path("Public/Gustav/Progressions/texture.dds"));
        assert!(!filter.matches_path("Public/Gustav/Progressions/model.GR2"));
        assert!(!filter.matches_path("Public/Shared/Assets/something.bnk"));
        assert!(!filter.matches_path("Mods/Foo/not-meta.lsx"));
        assert!(!filter.matches_path("Mods/Foo/meta.xml"));
    }

    #[test]
    fn reference_db_data_filter_exposes_regex_pattern() {
        let filter = PakEntryFilter::reference_db_data(ROOTS, EXTS).unwrap();
        let pattern = filter.regex_pattern().unwrap();

        assert!(pattern.contains("Public/Honour"));
        assert!(pattern.contains("Public/DiceSet_[^/]+"));
        assert!(pattern.contains("Mods/[^/]+/meta"));
    }
}
