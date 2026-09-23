//! Linux: build a private filesystem view around a fenced command.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::Path;

use super::{NamespacePosture, WriteFence};

/// fcntl.h: apply `mount_setattr` to the whole mount subtree. Not yet in
/// libc; a local constant keeps the raw call honest, same practice as the
/// local `SECCOMP_SET_MODE_FILTER`. Needs kernel ≥ 5.12.
const AT_RECURSIVE: libc::c_uint = 0x8000;

/// Why the view could not be built, and how bad that is.
enum BuildError {
    /// Nothing (or a fully torn-down view) is left mounted; the shared
    /// host view is intact and the run can safely continue without it.
    Failed(io::Error),
    /// A half-built view could not be torn down — the process must not
    /// exec into it, whatever the posture.
    Mangled(io::Error),
}

/// Entry point from the runner's `pre_exec` hook. See the module docs for
/// the posture semantics.
pub(super) fn setup(posture: NamespacePosture, fence: &WriteFence) -> io::Result<()> {
    if posture == NamespacePosture::Off {
        return Ok(());
    }
    match build_view(fence) {
        Ok(()) => Ok(()),
        Err(BuildError::Mangled(e)) => Err(e),
        Err(BuildError::Failed(e)) => {
            if posture == NamespacePosture::Require {
                return Err(e);
            }
            tracing::warn!(
                error = %e,
                "namespace view unavailable; continuing with Landlock + seccomp only"
            );
            Ok(())
        }
    }
}

/// A tree to re-bind read-write once the root is read-only, held open so
/// the read-only flip and the `/tmp` cover cannot hide it.
struct Rebind {
    target: &'static Path,
    /// O_PATH fd taken before any mount changed. The bind source is
    /// `/proc/self/fd/<fd>`, which `/proc` — itself re-bound RW below —
    /// serves.
    fd: RawFd,
}

/// A working tree (workspace root, allowed path, cwd) to re-bind onto its
/// own absolute path once `/tmp` is covered.
struct WorkingRebind {
    target: std::path::PathBuf,
    fd: RawFd,
}

fn build_view(fence: &WriteFence) -> Result<(), BuildError> {
    // Open everything that must survive the read-only flip or the /tmp
    // cover *before* any mount changes.
    let mut working: Vec<WorkingRebind> = Vec::new();
    push_unique(&mut working, fence.workspace_root.clone()).map_err(BuildError::Failed)?;
    for allowed in &fence.allowed_paths {
        push_unique(&mut working, allowed.clone()).map_err(BuildError::Failed)?;
    }
    if let Ok(cwd) = std::env::current_dir() {
        push_unique(&mut working, cwd).map_err(BuildError::Failed)?;
    }
    let ipc: Vec<Rebind> = ["/dev", "/run", "/proc"]
        .iter()
        .map(|t| open_o_path(Path::new(t)).map(|fd| Rebind { target: Path::new(t), fd }))
        .collect::<io::Result<_>>()
        .map_err(BuildError::Failed)?;

    // ②③ The namespaces themselves. Both fail before anything is mounted,
    // so they are clean degradations (the common one: unprivileged user
    // namespaces switched off).
    user_namespace().map_err(BuildError::Failed)?;
    mount_namespace().map_err(BuildError::Failed)?;

    // Past this point we are inside our own mount namespace and a failure
    // must not leave a half-built view behind to exec into.
    let mut stacked = false;
    let outcome = (|| -> io::Result<()> {
        // ④ Read-only root. Kernel ≥ 5.12 for the recursive setattr; on
        // older kernels this degrades to the plain copy and Landlock
        // carries the write fence alone.
        match root_read_only() {
            Ok(()) => stacked = true,
            Err(e) => tracing::warn!(
                error = %e,
                "read-only root unavailable; relying on Landlock for outside writes"
            ),
        }
        // ⑤ IPC trees back on top, read-write.
        for h in &ipc {
            bind_back(h.target, h.fd)
                .map_err(|e| io::Error::other(format!("re-bind {}: {e}", h.target.display())))?;
        }
        // ⑥ Private /tmp, then the working trees back onto their own
        // paths — a workspace under /tmp survives the cover.
        private_tmp().map_err(|e| io::Error::other(format!("private /tmp: {e}")))?;
        for w in &working {
            bind_back(&w.target, w.fd)
                .map_err(|e| io::Error::other(format!("re-bind {}: {e}", w.target.display())))?;
        }
        Ok(())
    })();
    match outcome {
        Ok(()) => Ok(()),
        // Nothing was stacked at "/": the shared view is untouched.
        Err(e) if !stacked => Err(BuildError::Failed(e)),
        Err(e) => match umount_slash() {
            Ok(()) => Err(BuildError::Failed(e)),
            Err(rb) => {
                tracing::warn!(error = %rb, "namespace view rollback failed");
                Err(BuildError::Mangled(e))
            }
        },
    }
}

/// Hold one O_PATH reference per distinct tree; a path already held is
/// kept (binding the same tree twice is pointless, and two fds to one
/// tree are wasteful).
fn push_unique(list: &mut Vec<WorkingRebind>, target: std::path::PathBuf) -> io::Result<()> {
    if target == Path::new("/") {
        // A workspace/cwd of `/` would re-bind the whole tree read-write
        // on top of the read-only root, undoing it. Skip the re-bind —
        // Landlock still fences writes, and `/` is not a workspace.
        tracing::warn!("workspace or cwd is `/`; skipping its view re-bind");
        return Ok(());
    }
    if list.iter().any(|w| w.target == target) {
        return Ok(());
    }
    let fd = open_o_path(&target)?;
    list.push(WorkingRebind { target, fd });
    Ok(())
}

/// `unshare(CLONE_NEWUSER)` plus the single-identity map that gives us
/// capabilities in the new namespace (and nobody else anything).
fn user_namespace() -> io::Result<()> {
    // Read the identity BEFORE the unshare: inside the new (empty)
    // namespace `getuid()` reports the overflow uid (65534), and a map
    // written from that reading would be refused.
    // SAFETY: plain uid/gid reads; libc marks them unsafe (they can be
    // overridden), but they touch no memory.
    #[allow(unsafe_code)]
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    // SAFETY: `unshare` has no invariant beyond the flags; on failure it
    // leaves the caller untouched.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::unshare(libc::CLONE_NEWUSER) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    write_id_maps(uid, gid)
}

/// Map our own uid/gid 1:1 — an unprivileged process may map exactly its
/// own identity, which is all the view needs (files we own stay ours;
/// everyone else's stay inaccessible).
fn write_id_maps(uid: u32, gid: u32) -> io::Result<()> {
    // The kernel requires `setgroups` to be denied before an unprivileged
    // gid_map write; the file is absent on kernels old enough that the
    // restriction predates it.
    if let Err(e) = std::fs::write("/proc/self/setgroups", "deny") {
        if e.kind() != io::ErrorKind::NotFound {
            return Err(e);
        }
    }
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"))?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"))?;
    Ok(())
}

/// `unshare(CLONE_NEWNS)` plus private propagation, so nothing the view
/// mounts leaks back to the host's peer groups (or inherits their events).
fn mount_namespace() -> io::Result<()> {
    // SAFETY: as with the user namespace — flags only, self-contained.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::unshare(libc::CLONE_NEWNS) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // Cut shared propagation so nothing the view mounts leaks back to the
    // host's peer groups. Recursive first; some container runtimes refuse
    // the recursive form over their locked submounts (EINVAL), and the
    // plain form still makes *our* mounts private — anything mounted
    // under the private root inherits it.
    // SAFETY: mount with null src/fstype/data flips propagation only.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            (libc::MS_REC | libc::MS_PRIVATE) as libc::c_ulong,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        // SAFETY: as above.
        #[allow(unsafe_code)]
        let plain = unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_PRIVATE as libc::c_ulong,
                std::ptr::null(),
            )
        };
        if plain != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Bind `/` onto itself recursively and mark the copy read-only.
fn root_read_only() -> io::Result<()> {
    // SAFETY: bind of an existing path onto itself; kernel copies the
    // path strings during the call.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::mount(
            c"/".as_ptr(),
            c"/".as_ptr(),
            std::ptr::null(),
            (libc::MS_BIND | libc::MS_REC) as libc::c_ulong,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let attrs = libc::mount_attr {
        attr_set: libc::MOUNT_ATTR_RDONLY | libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    // SAFETY: `attr` outlives the call; the kernel copies it during the
    // syscall. Variadic args are widened to `c_long` explicitly — an int
    // left to C's default promotions leaves the register's upper half
    // undefined, and the kernel reads each syscall argument as 64 bits.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD as libc::c_long,
            c"/".as_ptr(),
            AT_RECURSIVE as libc::c_long,
            &attrs as *const libc::mount_attr,
            std::mem::size_of::<libc::mount_attr>(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Cover `/tmp` with a fresh tmpfs. `1777` keeps the sticky semantics
/// mktemp expects; no `noexec` — extracting a binary into /tmp is
/// ordinary tooling work.
fn private_tmp() -> io::Result<()> {
    // SAFETY: string constants and flags; the kernel copies the data blob.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::mount(
            c"tmpfs".as_ptr(),
            c"/tmp".as_ptr(),
            c"tmpfs".as_ptr(),
            (libc::MS_NOSUID | libc::MS_NODEV) as libc::c_ulong,
            c"mode=1777".as_ptr().cast(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Re-bind a pre-opened tree onto its own absolute path. Under the fresh
/// `/tmp` the path has to be re-created first; anywhere else the
/// read-only view preserved it — and creating it is impossible, which is
/// the point.
fn bind_back(target: &Path, fd: RawFd) -> io::Result<()> {
    if target.starts_with("/tmp") {
        std::fs::create_dir_all(target)?;
    }
    let src = CString::new(format!("/proc/self/fd/{fd}"))?;
    let dst = CString::new(target.as_os_str().as_bytes())?;
    // Recursive first (the tree's own submounts come along); container
    // runtimes lock their submounts and refuse the recursive form, and a
    // plain bind of the top mount still re-exposes what matters
    // (`/dev/null` lives on `/dev` itself). Submounts left behind stay on
    // the read-only layer — reads are unaffected and writes there were
    // Landlock-denied regardless.
    // SAFETY: both paths outlive the call; a plain bind, no data.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            dst.as_ptr(),
            std::ptr::null(),
            (libc::MS_BIND | libc::MS_REC) as libc::c_ulong,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        // SAFETY: as above.
        #[allow(unsafe_code)]
        let plain = unsafe {
            libc::mount(
                src.as_ptr(),
                dst.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND as libc::c_ulong,
                std::ptr::null(),
            )
        };
        if plain != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Detach whatever is stacked at `/` — with `MNT_DETACH` the read-only
/// copy and everything mounted on top of it go together, restoring the
/// untouched host-view copy the mount namespace started with. Harmless
/// (EINVAL) when nothing is stacked.
fn umount_slash() -> io::Result<()> {
    // SAFETY: a flag-only umount of a fixed path.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::umount2(c"/".as_ptr(), libc::MNT_DETACH) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_o_path(path: &Path) -> io::Result<RawFd> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: open of a caller-owned path; the fd is closed with the
    // process at exec (O_CLOEXEC) — nothing to leak or double-close.
    #[allow(unsafe_code)]
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}
