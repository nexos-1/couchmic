//! Log file with size-based rotation. glass-mic appends to one file; once it grows beyond the
//! limit it is moved to `<name>.1` (replacing an older one) and a new file is started, so at most
//! two files of bounded size remain. The check runs on every write, not only at startup, so a
//! long run cannot fill the disk.

use std::fs::{File, OpenOptions};
use std::io::Write;
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

fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Append-only log file that rotates itself when it grows beyond `max_bytes`. If rotation is
/// impossible (the file is held open by another program without delete sharing), it retries at
/// most once a minute and stops writing at twice the limit, so the disk never fills up.
pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    file: Option<File>,
    written: u64,
    last_attempt: Option<std::time::Instant>,
}

const RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

impl RotatingFile {
    pub fn open(path: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        // A failed rotation must not cost the whole log: keep appending and retry later.
        let _ = rotate_if_larger(&path, max_bytes);
        let file = open_append(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            max_bytes,
            file: Some(file),
            written,
            last_attempt: None,
        })
    }

    fn rotate(&mut self) {
        self.last_attempt = Some(std::time::Instant::now());
        // Close first: renaming a file that is still open is not reliable on every setup.
        self.file = None;
        let _ = rotate_if_larger(&self.path, 0);
        self.file = open_append(&self.path).ok();
        // If the rename failed this is still the old, large file.
        self.written = self
            .file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(0);
        // Only a failed rotation (the old file is still there) waits before the next attempt.
        if self.written == 0 {
            self.last_attempt = None;
        }
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let over = self.written + buf.len() as u64 > self.max_bytes && self.written > 0;
        let may_retry = self.last_attempt.is_none_or(|t| t.elapsed() >= RETRY_EVERY);
        if over && may_retry {
            self.rotate();
        }
        // Hard cap: never let one file grow beyond twice the limit.
        if self.written + buf.len() as u64 > self.max_bytes.saturating_mul(2) {
            return Ok(buf.len());
        }
        let Some(f) = self.file.as_mut() else {
            // Reopening failed (disk full, file locked): drop the line instead of failing the
            // program. Logging must never take glass-mic down.
            return Ok(buf.len());
        };
        let n = f.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
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

    #[test]
    fn rotates_while_running_and_stays_bounded() {
        let d = temp_dir("runtime");
        let log = d.join("glass-mic.log");
        let mut f = RotatingFile::open(log.clone(), 100).unwrap();
        for _ in 0..50 {
            f.write_all(b"0123456789012345678\n").unwrap(); // 20 bytes per line
        }
        f.flush().unwrap();
        let cur = std::fs::metadata(&log).unwrap().len();
        let old = std::fs::metadata(d.join("glass-mic.log.1")).unwrap().len();
        assert!(cur <= 100, "current file bounded: {cur}");
        assert!(old <= 100, "rotated file bounded: {old}");
        assert!(!d.join("glass-mic.log.2").exists());
        drop(f);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn hard_cap_when_rotation_is_impossible() {
        let d = temp_dir("cap");
        let log = d.join("glass-mic.log");
        let mut f = RotatingFile::open(log.clone(), 100).unwrap();
        // Simulate a failed rotation: pretend one was just attempted, so no retry happens now.
        f.last_attempt = Some(std::time::Instant::now());
        for _ in 0..100 {
            f.write_all(b"0123456789012345678\n").unwrap();
        }
        f.flush().unwrap();
        let cur = std::fs::metadata(&log).unwrap().len();
        assert!(cur <= 200, "never beyond twice the limit: {cur}");
        drop(f);
        let _ = std::fs::remove_dir_all(&d);
    }
}
