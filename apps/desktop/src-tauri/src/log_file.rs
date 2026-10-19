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
    /// Whether this file holds the exclusive lock. The lock belongs to the handle, and the
    /// rotation opens a new one, so it is taken again on the new file.
    locked: bool,
}

impl RotatingFile {
    pub fn open(path: PathBuf, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Self::from_file(file, path, max_bytes, keep)
    }

    /// Open the log and try to take an exclusive lock on it.
    ///
    /// Two shells (the second launch is the single-instance plugin's handover) would otherwise
    /// interleave into one file, and the rotation renames it — which fails on Windows while the
    /// other process holds it open. The lock is the cheapest way to keep one file whole: the
    /// process that cannot take it writes a file of its own.
    ///
    /// The boolean is `false` when another process holds the lock.
    pub fn open_locked(path: PathBuf, max_bytes: u64, keep: usize) -> io::Result<(Self, bool)> {
        // `read(true)` is not decoration. On Windows the lock is a `LockFileEx` byte-range lock,
        // and it refuses a handle that was not opened with the `GENERIC_READ` or `GENERIC_WRITE`
        // access right: `append(true)` alone opens the file with `FILE_APPEND_DATA` and nothing
        // else, and the lock then fails with `ERROR_ACCESS_DENIED` — "Access is denied" — rather
        // than with the "somebody else holds it" the caller is asking about.
        let file = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(&path)?;
        let locked = fs4::fs_std::FileExt::try_lock_exclusive(&file)?;
        let mut this = Self::from_file(file, path, max_bytes, keep)?;
        this.locked = locked;
        Ok((this, locked))
    }

    fn from_file(file: File, path: PathBuf, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let written = file.metadata()?.len();
        let mut this = Self {
            path,
            max_bytes,
            keep,
            file,
            written,
            locked: false,
        };
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
        self.file = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(&self.path)?;
        // The lock belonged to the handle that was just renamed away, so the new file takes it
        // again: a file nobody holds is a file another process may take.
        if self.locked {
            self.locked = fs4::fs_std::FileExt::try_lock_exclusive(&self.file).unwrap_or(false);
        }
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
        assert!(
            fs::read_to_string(&path).unwrap().contains("000009"),
            "newest line is in app.log"
        );
    }

    #[test]
    fn an_oversized_existing_file_is_rotated_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        fs::write(&path, vec![b'x'; 500]).unwrap();
        let _log = RotatingFile::open(path.clone(), 100, 1).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(
            fs::metadata(dir.path().join("app.log.1")).unwrap().len(),
            500
        );
    }

    /// The env var that turns a test run into the "holder" half of the test below.
    const HOLDER_ENV: &str = "LOCALPLAY_LOG_LOCK_HOLDER";
    const HOLDER_LOG_ENV: &str = "LOCALPLAY_LOG_LOCK_LOG";
    const HOLDER_READY_ENV: &str = "LOCALPLAY_LOG_LOCK_READY";

    /// The child half of [`a_second_holder_of_the_log_is_told_so`].
    ///
    /// A byte-range lock is scoped to a *process*, so the holder has to be a real second
    /// process. Whether a second handle inside one process can take a conflicting lock is a
    /// different answer on every platform, and the case that matters is two shells.
    #[test]
    fn the_lock_holder_process() {
        if std::env::var(HOLDER_ENV).is_err() {
            return;
        }
        let path = PathBuf::from(std::env::var(HOLDER_LOG_ENV).unwrap());
        let (_log, owns) = RotatingFile::open_locked(path, 1024, 1).unwrap();
        assert!(owns, "the holder takes the lock");
        fs::write(std::env::var(HOLDER_READY_ENV).unwrap(), b"ready").unwrap();
        // Hold the lock until the parent has tried for it.
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    /// One process writes the shared log; the other one is told so.
    ///
    /// Two shells would otherwise interleave their lines into one file, and the rotation renames
    /// it — which fails on Windows while the other process holds it open. The lock is what keeps
    /// the file whole: the process that cannot take it writes a file of its own.
    #[test]
    fn a_second_holder_of_the_log_is_told_so() {
        if std::env::var(HOLDER_ENV).is_ok() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let ready = dir.path().join("holder-ready");

        let mut holder = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "log_file::tests::the_lock_holder_process",
                "--nocapture",
            ])
            .env(HOLDER_ENV, "1")
            .env(HOLDER_LOG_ENV, &path)
            .env(HOLDER_READY_ENV, &ready)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the holder process starts");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !ready.exists() {
            if let Ok(Some(status)) = holder.try_wait() {
                panic!("the holder process exited before it took the lock: {status}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the holder process never took the lock"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let (_second, owns) = RotatingFile::open_locked(path, 1024, 1).unwrap();
        let _ = holder.kill();
        let _ = holder.wait();

        assert!(!owns, "and the second one is refused it");
    }
}
