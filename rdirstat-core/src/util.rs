use std::fs::Metadata;
use std::path::PathBuf;

/// Size on disk for a file — what the filesystem actually allocated.
///
/// On Unix, this is `st_blocks * 512`, matching `du` (default), `stat -f %z`,
/// and what tools like KDirStat / QDirStat / WinDirStat show. It correctly
/// reflects sparse files (smaller than logical), filesystems with cluster
/// rounding (larger than logical), and APFS clones.
///
/// On other platforms we currently fall back to the logical file length
/// (`Metadata::len()`). On Windows the accurate value would come from
/// `GetCompressedFileSizeW`, which can be added later if needed.
pub fn allocated_size(metadata: &Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return metadata.blocks().saturating_mul(512);
    }
    #[cfg(not(unix))]
    {
        metadata.len()
    }
}

pub fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        path
    }
}

pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_unc_prefix_removes_prefix() {
        let path = PathBuf::from(r"\\?\C:\Users\test");
        assert_eq!(strip_unc_prefix(path), PathBuf::from(r"C:\Users\test"));
    }

    #[test]
    fn strip_unc_prefix_no_prefix() {
        let path = PathBuf::from(r"C:\Users\test");
        assert_eq!(strip_unc_prefix(path.clone()), path);
    }

    #[test]
    fn strip_unc_prefix_empty() {
        let path = PathBuf::from("");
        assert_eq!(strip_unc_prefix(path.clone()), path);
    }

    #[test]
    fn strip_unc_prefix_unix_path() {
        let path = PathBuf::from("/home/user");
        assert_eq!(strip_unc_prefix(path.clone()), path);
    }

    #[test]
    fn format_size_zero() {
        assert_eq!(format_size(0), "0 B");
    }

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(1), "1 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn format_size_kb() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1023), "1023.0 KB");
    }

    #[test]
    fn format_size_mb() {
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(1024 * 1024 * 5 + 1024 * 512), "5.5 MB");
    }

    #[test]
    fn format_size_gb() {
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(format_size(1024u64 * 1024 * 1024 * 100), "100.0 GB");
    }

    #[test]
    fn format_size_tb() {
        assert_eq!(format_size(1024u64 * 1024 * 1024 * 1024), "1.0 TB");
        assert_eq!(format_size(1024u64 * 1024 * 1024 * 1024 * 2), "2.0 TB");
    }
}
