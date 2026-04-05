use std::path::PathBuf;
use crate::scan::ScanState;

pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub file_size: u64,
    pub is_parent: bool,
}

impl DirEntry {
    pub fn current_size(&self, scan: &ScanState) -> u64 {
        if self.is_dir {
            scan.get_size(&self.path).unwrap_or(0)
        } else {
            self.file_size
        }
    }

    pub fn is_scanning(&self, scan: &ScanState) -> bool {
        if !self.is_dir {
            return false;
        }
        scan.is_scanning() && !scan.is_completed(&self.path)
    }
}
