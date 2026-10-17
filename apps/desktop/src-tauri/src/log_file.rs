//! A size-capped, rotating log file.
//!
//! The shell can run for months in the background, so an append-only log would grow without
//! bound. `base.log` is renamed to `base.log.1` (and `.1` to `.2`, ...) once it reaches
//! `max_bytes`; at most `keep` rotated files survive, bounding the directory to roughly
//! `(keep + 1) * max_bytes`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: File,
    written: u64,
}

impl RotatingFile {
    pub fn open(path: PathBuf, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        let mut this = Self { path, max_bytes, keep, file, written };
        if this.written >= this.max_bytes {
            this.rotate()?;
        }
        Ok(this)
    }

    fn numbered(&self, n: usize) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Oldest first, so nothing is overwritten before it has moved on.
        let _ = fs::remove_file(self.numbered(self.keep));
        for n in (1..self.keep).rev() {
            let from = self.numbered(n);
            if from.exists() {
                let _ = fs::rename(&from, self.numbered(n + 1));
            }
        }
        if self.keep == 0 {
            let _ = fs::remove_file(&self.path);
        } else {
            let _ = fs::rename(&self.path, self.numbered(1));
        }
        self.file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.written = 0;
        Ok(())
    }

}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written >= self.max_bytes {
            // A failed rotation must not lose the line or stop logging: keep appending.
            let _ = self.rotate();
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_at_the_cap_and_keeps_only_the_newest_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let mut log = RotatingFile::open(path.clone(), 100, 2).unwrap();
        for i in 0..10 {
            log.write_all(format!("{i:0>60}\n").as_bytes()).unwrap();
        }
        let names: Vec<_> = {
            let mut v: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        assert_eq!(names, ["app.log", "app.log.1", "app.log.2"]);
        let total: u64 = names
            .iter()
            .map(|n| fs::metadata(dir.path().join(n)).unwrap().len())
            .sum();
        assert!(total <= 3 * (100 + 61), "bounded: {total}");
        assert!(fs::read_to_string(&path).unwrap().contains("000009"), "newest line is in app.log");
    }

    #[test]
    fn an_oversized_existing_file_is_rotated_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        fs::write(&path, vec![b'x'; 500]).unwrap();
        let _log = RotatingFile::open(path.clone(), 100, 1).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(fs::metadata(dir.path().join("app.log.1")).unwrap().len(), 500);
    }
}
