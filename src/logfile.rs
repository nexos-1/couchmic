//! Log file rotation. glass-mic appends to one file; at startup a file larger than the limit is
//! moved to `<name>.1` (replacing an older one), so at most two files of bounded size remain.

use std::path::{Path, PathBuf};

fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

/// Moves `path` to `<path>.1` if it is larger than `max_bytes`. Returns whether it rotated.
pub fn rotate_if_larger(path: &Path, max_bytes: u64) -> std::io::Result<bool> {
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if size <= max_bytes {
        return Ok(false);
    }
    let old = rotated_path(path);
    if old.exists() {
        std::fs::remove_file(&old)?;
    }
    std::fs::rename(path, &old)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("glass-mic-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rotates_only_when_too_large() {
        let d = temp_dir("rotate");
        let log = d.join("glass-mic.log");
        assert!(!rotate_if_larger(&log, 10).unwrap(), "missing file is fine");
        std::fs::write(&log, b"12345").unwrap();
        assert!(!rotate_if_larger(&log, 10).unwrap());
        std::fs::write(&log, b"0123456789abc").unwrap();
        std::fs::write(d.join("glass-mic.log.1"), b"older").unwrap();
        assert!(rotate_if_larger(&log, 10).unwrap());
        assert!(!log.exists());
        assert_eq!(
            std::fs::read(d.join("glass-mic.log.1")).unwrap(),
            b"0123456789abc",
            "older rotation is replaced"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
