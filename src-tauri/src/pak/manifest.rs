use std::collections::BTreeMap;

use crate::pak::entry::PakEntry;
use crate::pak::format::{PakPackageFlags, PakVersion};
use crate::pak::path::PakPath;

#[derive(Debug, Clone)]
pub struct PakManifest {
    version: PakVersion,
    package_flags: PakPackageFlags,
    entries: Vec<PakEntry>,
    index_by_path: BTreeMap<PakPath, usize>,
}

impl PakManifest {
    pub fn empty(version: PakVersion) -> Self {
        Self::new(version, PakPackageFlags::empty(), Vec::new())
    }

    pub fn new(
        version: PakVersion,
        package_flags: PakPackageFlags,
        entries: Vec<PakEntry>,
    ) -> Self {
        let mut index_by_path = BTreeMap::new();
        for (index, entry) in entries.iter().enumerate() {
            index_by_path.insert(entry.path.clone(), index);
        }

        Self {
            version,
            package_flags,
            entries,
            index_by_path,
        }
    }

    pub fn version(&self) -> PakVersion {
        self.version
    }

    pub fn package_flags(&self) -> PakPackageFlags {
        self.package_flags
    }

    pub fn entries(&self) -> &[PakEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, path: &PakPath) -> Option<&PakEntry> {
        self.index_by_path
            .get(path)
            .and_then(|index| self.entries.get(*index))
    }

    pub fn contains(&self, path: &PakPath) -> bool {
        self.index_by_path.contains_key(path)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, PakEntry> {
        self.entries.iter()
    }
}
