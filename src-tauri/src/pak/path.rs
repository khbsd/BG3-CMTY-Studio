use crate::pak::error::{PakError, PakResult};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PakPath(String);

impl PakPath {
    pub fn parse(raw: &str) -> PakResult<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(PakError::invalid_path("path cannot be empty"));
        }

        if trimmed.starts_with('/') || trimmed.starts_with('\\') {
            return Err(PakError::invalid_path(format!(
                "absolute pak paths are not allowed: {raw}"
            )));
        }

        if has_windows_drive_prefix(trimmed) {
            return Err(PakError::invalid_path(format!(
                "drive-qualified pak paths are not allowed: {raw}"
            )));
        }

        let normalized = trimmed.replace('\\', "/");
        if normalized.chars().any(char::is_control) {
            return Err(PakError::invalid_path(format!(
                "control characters are not allowed in pak paths: {raw}"
            )));
        }

        let mut segments = Vec::new();
        for segment in normalized.split('/') {
            if segment.is_empty() {
                continue;
            }
            if segment == "." || segment == ".." {
                return Err(PakError::invalid_path(format!(
                    "path traversal segment is not allowed: {raw}"
                )));
            }
            segments.push(segment);
        }

        if segments.is_empty() {
            return Err(PakError::invalid_path(format!(
                "path has no usable segments: {raw}"
            )));
        }

        Ok(Self(segments.join("/")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    pub fn parent(&self) -> Option<&str> {
        self.0.rsplit_once('/').map(|(parent, _)| parent)
    }

    pub fn extension(&self) -> Option<&str> {
        self.file_name()
            .rsplit_once('.')
            .and_then(|(_, ext)| (!ext.is_empty()).then_some(ext))
    }

    pub fn starts_with(&self, prefix: &str) -> bool {
        let Ok(prefix) = Self::parse(prefix) else {
            return false;
        };
        self.starts_with_path(&prefix)
    }

    pub fn starts_with_path(&self, prefix: &PakPath) -> bool {
        self.0 == prefix.0
            || self
                .0
                .strip_prefix(prefix.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    }

    pub fn segments(&self) -> std::str::Split<'_, char> {
        self.0.split('/')
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for PakPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for PakPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for PakPath {
    type Err = PakError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for PakPath {
    type Error = PakError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

fn has_windows_drive_prefix(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_separators_and_redundant_delimiters() {
        let path = PakPath::parse(r" Public\\Shared//Assets///foo.lsx ").unwrap();

        assert_eq!(path.as_str(), "Public/Shared/Assets/foo.lsx");
        assert_eq!(path.file_name(), "foo.lsx");
        assert_eq!(path.parent(), Some("Public/Shared/Assets"));
        assert_eq!(path.extension(), Some("lsx"));
    }

    #[test]
    fn rejects_empty_absolute_and_traversal_paths() {
        assert!(PakPath::parse("   ").is_err());
        assert!(PakPath::parse("/Public/Shared").is_err());
        assert!(PakPath::parse(r"\Public\Shared").is_err());
        assert!(PakPath::parse("Public/../Shared").is_err());
        assert!(PakPath::parse("./Public/Shared").is_err());
    }

    #[test]
    fn rejects_drive_qualified_and_control_character_paths() {
        assert!(PakPath::parse(r"C:\bg3\foo.lsx").is_err());
        assert!(PakPath::parse("Public/Shared\nfoo.lsx").is_err());
    }

    #[test]
    fn prefix_matching_is_segment_aware() {
        let path = PakPath::parse("Public/Gustav/Assets/foo.lsx").unwrap();

        assert!(path.starts_with("Public"));
        assert!(path.starts_with("Public/Gustav"));
        assert!(path.starts_with("Public/Gustav/"));
        assert!(!path.starts_with("Public/Gus"));
        assert!(!path.starts_with("Public/GustavDev"));
    }

    #[test]
    fn from_str_and_segments_match_normalized_path() {
        let path: PakPath = "Mods/Gustav/meta.lsx".parse().unwrap();

        assert_eq!(
            path.segments().collect::<Vec<_>>(),
            vec!["Mods", "Gustav", "meta.lsx"]
        );
        assert_eq!(path.to_string(), "Mods/Gustav/meta.lsx");
    }
}
