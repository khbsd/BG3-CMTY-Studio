use crate::pak::format::{PakCompression, PakEntryFlags};
use crate::pak::path::PakPath;

#[derive(Debug, Clone)]
pub struct PakEntry {
    pub path: PakPath,
    pub archive_part: u16,
    pub offset: u64,
    pub size_on_disk: u64,
    pub uncompressed_size: u64,
    pub compression: PakCompression,
    pub flags: PakEntryFlags,
}

impl PakEntry {
    pub fn is_deleted(&self) -> bool {
        self.flags.contains(PakEntryFlags::DELETION)
    }

    pub fn is_compressed(&self) -> bool {
        self.compression != PakCompression::None
    }

    pub fn effective_size(&self) -> u64 {
        if self.is_compressed() {
            self.uncompressed_size
        } else {
            self.size_on_disk
        }
    }
}
