//! A log file for `gitsync watch --log-file`, used by the background service
//! where there is no terminal (and, on Windows and macOS, no system journal
//! gitsync writes to). When it grows past a limit it is renamed to
//! `<name>.old` and a new one is started, so it never fills the disk.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Size at which the log starts over (the previous one is kept as `.old`).
const MAX_BYTES: u64 = 5 * 1024 * 1024;

pub struct LogFile {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl LogFile {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut log = Self {
            path: path.to_path_buf(),
            file: None,
            size: 0,
        };
        log.reopen()
            .with_context(|| format!("opening log file {}", path.display()))?;
        Ok(log)
    }

    fn reopen(&mut self) -> io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Closed first: Windows cannot rename a file that is open.
        self.file = None;
        let mut old = self.path.clone().into_os_string();
        old.push(".old");
        fs::rename(&self.path, old)?;
        self.reopen()
    }
}

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > MAX_BYTES {
            // If rotating fails, keep appending to the current file.
            if self.rotate().is_err() && self.file.is_none() {
                self.reopen()?;
            }
        }
        let file = self.file.as_mut().ok_or(io::ErrorKind::NotConnected)?;
        let written = file.write(buf)?;
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_over_when_full_keeping_the_previous_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("watch.log");
        let mut log = LogFile::open(&path).unwrap();
        let line = vec![b'x'; 1024 * 1024];
        for _ in 0..6 {
            log.write_all(&line).unwrap();
        }
        log.flush().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 1024 * 1024);
        let old = dir.path().join("logs").join("watch.log.old");
        assert_eq!(fs::metadata(old).unwrap().len(), 5 * 1024 * 1024);
    }
}
