use std::path::PathBuf;

#[derive(Clone)]
pub struct MountPoint {
    pub path: PathBuf,
    pub label: String,
}

/// List available drives/mount points for the current platform.
pub fn list_mounts() -> Vec<MountPoint> {
    #[cfg(target_os = "windows")]
    {
        list_mounts_windows()
    }
    #[cfg(not(target_os = "windows"))]
    {
        list_mounts_unix()
    }
}

#[cfg(target_os = "windows")]
fn list_mounts_windows() -> Vec<MountPoint> {
    let mut mounts = Vec::new();
    for letter in b'A'..=b'Z' {
        let drive = format!("{}:\\", letter as char);
        let path = PathBuf::from(&drive);
        if path.exists() {
            mounts.push(MountPoint {
                path,
                label: drive,
            });
        }
    }
    mounts
}

#[cfg(not(target_os = "windows"))]
fn list_mounts_unix() -> Vec<MountPoint> {
    let mut mounts = Vec::new();

    mounts.push(MountPoint {
        path: PathBuf::from("/"),
        label: "/".to_string(),
    });

    let mount_files = ["/proc/mounts", "/etc/mtab"];
    for mf in &mount_files {
        if let Ok(content) = fs::read_to_string(mf) {
            for line in content.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let mount_path = parts[1];
                    if mount_path == "/"
                        || mount_path.starts_with("/proc")
                        || mount_path.starts_with("/sys")
                        || mount_path.starts_with("/dev")
                        || mount_path.starts_with("/run")
                        || mount_path.starts_with("/snap")
                    {
                        continue;
                    }
                    let path = PathBuf::from(mount_path);
                    if path.exists() {
                        mounts.push(MountPoint {
                            path,
                            label: format!("{} ({})", mount_path, parts[0]),
                        });
                    }
                }
            }
            break;
        }
    }

    if let Ok(read_dir) = fs::read_dir("/Volumes") {
        for entry in read_dir.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !mounts.iter().any(|m| m.path == path) {
                mounts.push(MountPoint {
                    path,
                    label: format!("/Volumes/{name}"),
                });
            }
        }
    }

    mounts
}
