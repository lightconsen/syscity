//! An exclusive lock on a name, held across processes.
//!
//! The desktop has one pointer and one focus, and those are not properties of
//! a process: the gateway, the CLI, and a delegated child agent can all be
//! asked to act on the same screen at the same time. An in-process mutex — the
//! first layer of [`target_lock`](crate::tools::target_lock) — cannot see any
//! of the other processes, so it serialises only what it happens to be running.
//!
//! This is the second layer: a file, locked with the operating system's own
//! advisory lock (`flock` on Unix, `LockFileEx` on Windows, both through
//! `std::fs::File`). Two processes that derive the same path contend for the
//! same lock, and every process that ever drove a display derives it the same
//! way, so gateway + CLI + child exclude each other.
//!
//! The semantics are deliberately fail-closed. The existing use of `flock` in
//! [`crate::security::allowlist`] warns and writes anyway, which is right for
//! "two writers, last one wins" and wrong here: proceeding without the lock is
//! exactly the interleaving the lock exists to prevent. A timeout is reported
//! as an error, and the caller refuses the action.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Why a lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Something else holds it, in this process or another one.
    #[error("timed out after {waited_ms} ms waiting for {path}")]
    TimedOut {
        /// The lock file that is held.
        path: PathBuf,
        /// How long we waited for it.
        waited_ms: u64,
    },

    /// The lock file or its directory could not be prepared.
    #[error("could not open lock file {path}: {source}")]
    Io {
        /// The lock file we failed to open.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
}

/// How long to keep trying before reporting the lock as held elsewhere.
///
/// Long enough for a short action in another process to finish, short enough
/// that the model waiting on this is not stuck: a caller that has held the
/// desktop for longer than this is doing something the user should know about.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait between attempts.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// An exclusive lock on a file, released when dropped.
///
/// The lock is advisory and belongs to the open file description, so it is
/// released by closing the file — which is what dropping this does, including
/// when the process panics. If the process is killed outright the kernel
/// releases it too, so a crashed writer cannot leave a display locked.
#[derive(Debug)]
pub struct FileLock {
    /// Held open for exactly as long as the lock is.
    _file: File,
    path: PathBuf,
}

impl FileLock {
    /// Take the lock at `path`, retrying until `timeout` has passed.
    ///
    /// The parent directory is created when missing, so a caller does not
    /// have to know how the lock root is laid out.
    pub fn acquire(path: impl AsRef<Path>, timeout: Duration) -> Result<Self, LockError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| LockError::Io { path: path.clone(), source })?;
        }
        // Not truncated: another process may be holding this very file, and
        // truncating it does not release the lock but does rewrite the
        // contents (which exist only to give the lock a name).
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LockError::Io { path: path.clone(), source })?;

        let start = std::time::Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file, path }),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(source)) => {
                    return Err(LockError::Io { path, source });
                }
            }
            if start.elapsed() >= timeout {
                return Err(LockError::TimedOut {
                    path,
                    waited_ms: start.elapsed().as_millis() as u64,
                });
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Take the lock, or report it as unavailable without waiting.
    pub fn try_acquire(path: impl AsRef<Path>) -> Result<Self, LockError> {
        Self::acquire(path, Duration::ZERO)
    }

    /// The file this lock is held on.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Closing releases it either way; unlocking first makes that explicit
        // and means the release does not depend on `File`'s drop order.
        let _ = self._file.unlock();
    }
}

/// The lock root for this process.
///
/// Under the profile's data directory, so two profiles — two `SYSCITY_HOME`
/// roots — do not share locks, which is correct: they may be driving two
/// different users' displays. Within one profile every process resolves the
/// same root, which is the whole point.
pub fn lock_root() -> PathBuf {
    crate::dirs::paths().data_dir().join("locks")
}

/// The lock file a target key maps to.
///
/// The key is used as-is when it is a single safe path segment, and otherwise
/// hex-encoded, so a target id that came from a device serial or a package
/// name can never climb out of the lock directory or collide with another
/// key's file. The encoding is injective, so two keys never share a file by
/// accident.
pub fn lock_path(key: &str) -> PathBuf {
    let safe = !key.is_empty()
        && key.len() <= 100
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    let name = if safe {
        key.to_string()
    } else {
        // Hex, not lossy replacement: `a/b` and `a_b` must not become the same
        // lock, and length is bounded because the input is bounded.
        key.bytes().map(|b| format!("{b:02x}")).collect()
    };
    lock_root().join(format!("{name}.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One holder at a time for one path, even across separately opened files.
    ///
    /// Separate `File`s in one process is the same kernel-level conflict two
    /// processes have — `flock` is per open file description, not per process
    /// — so this is the cross-process case with a shorter setup.
    #[test]
    fn one_path_is_held_exclusively() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("target.lock");

        let held = FileLock::acquire(&path, Duration::from_millis(50)).expect("first");
        let second = FileLock::acquire(&path, Duration::from_millis(50));
        assert!(
            matches!(second, Err(LockError::TimedOut { .. })),
            "a second holder got in: {second:?}"
        );
        drop(held);
    }

    /// Different paths are different locks.
    #[test]
    fn different_paths_do_not_block_each_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _one =
            FileLock::acquire(dir.path().join("a.lock"), Duration::from_millis(0)).expect("a");
        let other = FileLock::acquire(dir.path().join("b.lock"), Duration::from_millis(0));
        assert!(other.is_ok(), "one target waited on another: {other:?}");
    }

    /// Dropping the lock releases it, so the next writer gets in immediately.
    #[test]
    fn dropping_the_lock_releases_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("target.lock");
        drop(FileLock::acquire(&path, Duration::from_millis(0)).expect("first"));
        let again = FileLock::acquire(&path, Duration::from_millis(0));
        assert!(again.is_ok(), "the lock outlived its guard: {again:?}");
    }

    /// A waiting caller gets the lock as soon as the holder finishes, rather
    /// than failing just because the first attempt was refused.
    #[test]
    fn a_waiter_takes_over_when_the_holder_finishes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("target.lock");
        let held = FileLock::acquire(&path, Duration::from_millis(0)).expect("first");

        let released = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            drop(held);
        });
        let second = FileLock::acquire(&path, Duration::from_secs(2));
        released.join().expect("releaser");
        assert!(second.is_ok(), "the waiter never got the lock: {second:?}");
    }

    /// The parent directory is made for the caller, so a fresh profile works.
    #[test]
    fn a_missing_directory_is_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("deeper").join("x.lock");
        let lock = FileLock::acquire(&path, Duration::from_millis(0)).expect("acquire");
        assert!(lock.path().exists());
    }

    /// The timeout is a real bound, not a suggestion.
    #[test]
    fn a_held_lock_reports_how_long_it_waited() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("target.lock");
        let _held = FileLock::acquire(&path, Duration::from_millis(0)).expect("first");
        let started = std::time::Instant::now();
        match FileLock::acquire(&path, Duration::from_millis(60)) {
            Err(LockError::TimedOut { waited_ms, path: reported }) => {
                assert_eq!(reported, path);
                assert!(waited_ms >= 60, "gave up early after {waited_ms} ms");
                assert!(started.elapsed() >= Duration::from_millis(60));
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    /// A key that is a safe segment names its file directly.
    #[test]
    fn a_plain_key_names_its_own_file() {
        assert_eq!(lock_path("desktop-input"), lock_root().join("desktop-input.lock"));
    }

    /// A key borrowed from a device serial or a package name still lands in
    /// the lock directory, and keeps its identity there.
    #[test]
    fn a_device_key_stays_in_the_lock_directory() {
        let key = "android-device:emulator-5554";
        let path = lock_path(key);
        assert_eq!(path.parent(), Some(lock_root().as_path()));
        assert_ne!(path, lock_path("android-device-emulator-5554"));
        assert_eq!(path, lock_path(key), "the same key must find the same file");
    }

    /// Two keys that differ only in what a lossy encoder would replace must
    /// not share a file.
    #[test]
    fn different_unsafe_keys_do_not_collide() {
        assert_ne!(lock_path("a/b"), lock_path("a_b"));
        // And an unsafe key does not escape the lock directory.
        let escaped = lock_path("../../etc/passwd");
        assert_eq!(escaped.parent(), Some(lock_root().as_path()));
    }

    /// The same key in two calls is the same file, which is what lets two
    /// processes find each other.
    #[test]
    fn the_same_key_maps_to_the_same_file() {
        assert_eq!(lock_path("desktop-input"), lock_path("desktop-input"));
    }

    /// The variable that turns this test binary into a lock-holding child.
    const CHILD_ENV: &str = "SYSCITY_FILE_LOCK_HOLD";

    /// The child half of [`a_lock_is_held_by_another_process`].
    ///
    /// Re-executing the test binary is the only way to get a genuinely
    /// separate process without building a helper crate; the harness runs
    /// this test alone, in a process that inherited `CHILD_ENV`, so its job is
    /// to take the lock and stay alive until killed.
    #[test]
    fn child_process_holds_the_lock() {
        let Some(path) = std::env::var_os(CHILD_ENV) else {
            // The ordinary test run: nothing to hold.
            return;
        };
        use std::io::Write;
        let _held = FileLock::acquire(PathBuf::from(path), Duration::from_secs(10))
            .expect("child acquires the lock");
        // `--nocapture` is set on the child, so this reaches the parent's pipe
        // without waiting for the (never reached) end of the test.
        println!("HELD");
        let _ = std::io::stdout().flush();
        // Block until the parent kills this process; whichever way it ends,
        // the kernel releases the lock.
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1];
        while matches!(std::io::Read::read(&mut stdin, &mut buf), Ok(1)) {}
    }

    /// Another process holding the lock is a real conflict, and a process
    /// that dies releases it: the lock must not outlive the writer.
    #[test]
    fn a_lock_is_held_by_another_process() {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cross.lock");
        let exe = std::env::current_exe().expect("the test binary path");

        let mut child = std::process::Command::new(&exe)
            .args([
                "--exact",
                "tools::file_lock::tests::child_process_holds_the_lock",
                "--nocapture",
            ])
            .env(CHILD_ENV, &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The harness chatter would interleave with the handshake.
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the holding child");

        let mut line = String::new();
        let mut reader = BufReader::new(child.stdout.as_mut().expect("child stdout"));
        // The harness prints its own banner first; wait for the handshake.
        let mut saw_held = false;
        for _ in 0..20 {
            line.clear();
            if reader
                .read_line(&mut line)
                .expect("handshake from the child")
                == 0
            {
                break;
            }
            if line.contains("HELD") {
                saw_held = true;
                break;
            }
        }
        assert!(saw_held, "the child never took the lock: {line:?}");

        let contended = FileLock::acquire(&path, Duration::from_millis(50));
        assert!(
            matches!(contended, Err(LockError::TimedOut { .. })),
            "a lock held by another process was acquired: {contended:?}"
        );

        // The kernel, not the child, is responsible for release on death.
        child.kill().expect("kill the holding child");
        child.wait().expect("reap the holding child");
        let after = FileLock::acquire(&path, Duration::from_secs(5));
        assert!(after.is_ok(), "the lock outlived the process: {after:?}");
    }

    /// A contended target, counted: the peak number of simultaneous holders of
    /// one path is one.
    #[test]
    fn threads_sharing_a_lock_never_overlap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shared.lock");
        let concurrent = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..3 {
                        let _held = FileLock::acquire(&path, Duration::from_secs(5)).expect("lock");
                        let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(5));
                        concurrent.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            }
        });

        assert_eq!(peak.load(Ordering::SeqCst), 1, "two holders overlapped");
    }
}
