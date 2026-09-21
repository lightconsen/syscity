//! Windows AppContainer + Job write fence (kernel-enforced).
//!
//! This is the Windows counterpart of the macOS Seatbelt runner and the
//! Linux Landlock runner: it confines a child process so all file-write
//! operations outside the configured workspace / allowed paths are denied,
//! regardless of what the command does.
//!
//! # Fence direction (inverted vs. Unix)
//!
//! On macOS/Linux the fence is *additive*: the kernel allows writes by default
//! and we deny everything outside the workspace. Windows AppContainer is the
//! opposite — an AppContainer token starts out denying writes nearly
//! everywhere (only `%TEMP%\Low` and NUL are writable), so the fence must
//! *grant* workspace writes instead:
//!
//! 1. An [`AppContainerProfile`] produces a token whose process runs at
//!    **Low integrity** (S-1-16-4096) with a restricted SID set — the
//!    Chromium sandbox model.
//! 2. A **mandatory Low label** is placed on the workspace directory. A Low
//!    process can only write to objects at ≤ its own integrity level, so the
//!    default Medium-labelled workspace would reject the child even with a
//!    permissive DACL. Labelling it Low makes equal-integrity writes legal.
//! 3. A **DACL ACE** grants the AppContainer SID
//!    `FILE_GENERIC_READ|WRITE|EXECUTE|DELETE|FILE_DELETE_CHILD` (inherited).
//!    Integrity alone is not enough — AppContainer SIDs are not in arbitrary
//!    directories' DACLs by default.
//! 4. Network is opt-in: an AppContainer token without capabilities has **no
//!    network access at all**, so the token is given the three well-known
//!    internet/private-network capability SIDs (otherwise every networked tool
//!    would break).
//!
//! The DACL grant and the Low label are applied in a *single*
//! `SetNamedSecurityInfoW` call so the workspace never passes through an
//! intermediate all-denied (or unwritable) state.
//!
//! # Process lifecycle (Job object)
//!
//! A fenced child is created via raw `CreateProcessW` + `STARTUPINFOEXW`
//! (tokio/std `Command` cannot express an AppContainer attribute list) and
//! immediately assigned to a Job object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The Job gives us whole-tree
//! termination: `kill()` calls `TerminateJobObject` (all descendants die, not
//! just the direct child), and dropping the child closes the Job handle,
//! which the kernel translates into "terminate whatever is left" — no orphaned
//! sandboxed processes can leak.
//!
//! # Write surface (accepted limits)
//!
//! The fenced child may write to: the workspace, the `allowed_paths`,
//! `%TEMP%\Low` (AppContainer default), and NUL. `%TEMP%`, `%LOCALAPPDATA%`
//! and everything else are *not* granted — commands that need extra write
//! space must declare it via `allowed_paths`. This is an honest fence: the
//! workspace is the child's scratch area, and files the *parent* (Medium
//! integrity) creates in it are read-able by the child only at/under Low.
//!
//! # Verification status
//!
//! This module is compiled only for `x86_64-pc-windows-gnu` (zig cross-check)
//! — there is no Windows machine in this project, so the runtime paths are
//! compile-verified and the behavior tests are availability-gated.
//!
//! FFI surface: hand-written against `windows-sys` (no rappct dependency),
//! matching the module-level `#![allow(unsafe_code)]` precedent set by
//! `src/rag/sqlite_vec_store.rs`. All API calls return `Result<_, WinFenceError>`
//! and fail closed.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::ptr::{null, null_mut};
use std::sync::LazyLock;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use windows_sys::core::{PCWSTR, PWSTR};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, GENERIC_READ,
    GENERIC_WRITE, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    AddMandatoryAce, CopySid, GetLengthSid, InitializeAcl, ACL, ACL_REVISION,
    CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION,
    OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES,
    SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_MANDATORY_POLICY_NO_WRITE_UP,
};
#[cfg(test)]
use windows_sys::Win32::Security::{GetAce, ACE_HEADER};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use crate::tools::process_runner::{ProcessChild, ProcessRequest};

/// AppContainer profile name. Package-legal: letters/dots/hyphens only
/// (asserted in a unit test).
const PROFILE_NAME: &str = "Syscity.Fence";
const PROFILE_DISPLAY_NAME: &str = "Syscity Workspace Fence";
const PROFILE_DESCRIPTION: &str = "AppContainer write fence for workspace-only file access";

/// Well-known capability SIDs. An AppContainer token with no capabilities has
/// zero network access; these three are the internet / private-network
/// "network client" set used by Chromium's sandbox.
const INTERNET_CLIENT_SID: &str = "S-1-15-2-1";
const INTERNET_CLIENT_SERVER_SID: &str = "S-1-15-2-2";
const PRIVATE_NETWORK_CLIENT_SERVER_SID: &str = "S-1-15-2-3";

/// Mandatory integrity label of an AppContainer token / Low process. Applied
/// to the workspace so the Low child can write to it (equal-integrity write).
const LOW_MANDATORY_LABEL_SID: &str = "S-1-16-4096";

/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)` — `CreateAppContainerProfile`
/// returns this when the profile is already registered.
const HRESULT_ALREADY_EXISTS: i32 = 0x8007_00B7u32 as i32;

/// `SE_GROUP_ENABLED` (0x4): marks a SID in a token group/capability as
/// enabled. Only exported under `Win32_System_SystemServices`, which we do
/// not enable.
const SE_GROUP_ENABLED: u32 = 0x4;

/// Failure modes for the AppContainer fence. Every API error is fail-closed:
/// the caller refuses to run the child unfenced.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WinFenceError {
    #[error("{op} failed (win32 error {code})")]
    Os { op: &'static str, code: u32 },
    #[error("app container profile: {0}")]
    Profile(String),
    #[error("acl grant for '{path}' failed: {reason}")]
    Acl { path: String, reason: String },
    #[error("stdio setup failed: {0}")]
    Stdio(String),
    #[error("fenced spawn failed: {0}")]
    Spawn(String),
}

impl WinFenceError {
    fn acl(path: &Path, reason: impl Into<String>) -> Self {
        WinFenceError::Acl {
            path: path.display().to_string(),
            reason: reason.into(),
        }
    }
}

/// Capture the last Win32 error for a named operation.
fn last_error(op: &'static str) -> WinFenceError {
    WinFenceError::Os {
        op,
        code: unsafe { GetLastError() },
    }
}

/// An owned copy of a SID (variable-length), held as raw bytes so the pointer
/// handed to the kernel stays valid. `Send + Sync` (plain heap bytes).
#[derive(Clone)]
pub(crate) struct AppContainerSid {
    bytes: Vec<u8>,
}

impl AppContainerSid {
    fn as_ptr(&self) -> PSID {
        self.bytes.as_ptr() as *const c_void as PSID
    }
}

/// The AppContainer profile, cached process-wide after first use.
///
/// `create_or_open` is idempotent: a profile registered by an earlier run is
/// reopened via `DeriveAppContainerSidFromAppContainerName` rather than
/// failing. AppContainer profiles persist across reboots, so this matters.
pub(crate) struct AppContainerProfile {
    sid: AppContainerSid,
}

impl AppContainerProfile {
    /// Get (creating if needed) the fence's AppContainer profile and its SID.
    pub(crate) fn create_or_open() -> Result<AppContainerProfile, WinFenceError> {
        static SID: LazyLock<Result<AppContainerSid, String>> =
            LazyLock::new(create_profile_sid_cached);
        match &*SID {
            Ok(sid) => Ok(AppContainerProfile { sid: sid.clone() }),
            Err(msg) => Err(WinFenceError::Profile(msg.clone())),
        }
    }

    /// The profile's AppContainer SID.
    pub(crate) fn sid(&self) -> &AppContainerSid {
        &self.sid
    }
}

fn create_profile_sid_cached() -> Result<AppContainerSid, String> {
    let name = to_wide_nul(PROFILE_NAME);
    let display = to_wide_nul(PROFILE_DISPLAY_NAME);
    let description = to_wide_nul(PROFILE_DESCRIPTION);
    unsafe {
        let mut sid: PSID = null_mut();
        let hr = CreateAppContainerProfile(
            name.as_ptr(),
            display.as_ptr(),
            description.as_ptr(),
            null(),
            0,
            &mut sid,
        );
        let sid = if hr == HRESULT_ALREADY_EXISTS {
            // Profile already registered (e.g. from a previous run) — derive
            // its SID instead of failing.
            let mut derived: PSID = null_mut();
            let hr = DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut derived);
            if hr < 0 {
                return Err(format!(
                    "DeriveAppContainerSidFromAppContainerName failed: 0x{:08X}",
                    hr as u32
                ));
            }
            derived
        } else if hr < 0 {
            return Err(format!("CreateAppContainerProfile failed: 0x{:08X}", hr as u32));
        } else {
            sid
        };
        let len = GetLengthSid(sid);
        let mut bytes = vec![0u8; len as usize];
        let ok = CopySid(len, bytes.as_mut_ptr() as PSID, sid);
        LocalFree(sid as HLOCAL);
        if ok == 0 {
            return Err("CopySid failed".to_string());
        }
        Ok(AppContainerSid { bytes })
    }
}

/// Owned capability SIDs + the `SID_AND_ATTRIBUTES` array pointing at them.
///
/// The two must live together: `SECURITY_CAPABILITIES` hands the kernel a
/// pointer into `attrs`, which points into the SID byte buffers. Keeping both
/// in one struct (with `attrs` built from the live SIDs) avoids dangling
/// pointers.
struct Capabilities {
    _sids: Vec<AppContainerSid>,
    attrs: Vec<SID_AND_ATTRIBUTES>,
}

/// No capabilities at all: the token gets zero network access.
///
/// The AppContainer model is deny-by-default for the network — a token with no
/// capability SIDs cannot open a socket — so `[security] fence_network` is
/// expressed by simply not handing over the network capability set.
fn no_capabilities() -> Capabilities {
    Capabilities {
        _sids: Vec::new(),
        attrs: Vec::new(),
    }
}

/// Build the network capability set for the AppContainer token.
fn network_capabilities() -> Result<Capabilities, WinFenceError> {
    let mut sids = Vec::with_capacity(3);
    for s in [
        INTERNET_CLIENT_SID,
        INTERNET_CLIENT_SERVER_SID,
        PRIVATE_NETWORK_CLIENT_SERVER_SID,
    ] {
        sids.push(string_to_sid(s)?);
    }
    let attrs = sids
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.as_ptr(),
            Attributes: SE_GROUP_ENABLED,
        })
        .collect();
    Ok(Capabilities { _sids: sids, attrs })
}

/// Parse a SID string (`S-1-15-2-1`) into an owned [`AppContainerSid`].
fn string_to_sid(s: &str) -> Result<AppContainerSid, WinFenceError> {
    let wide = to_wide_nul(s);
    unsafe {
        let mut sid: PSID = null_mut();
        if ConvertStringSidToSidW(wide.as_ptr(), &mut sid) == 0 {
            return Err(WinFenceError::Spawn(format!(
                "ConvertStringSidToSidW('{s}') failed: {}",
                GetLastError()
            )));
        }
        let len = GetLengthSid(sid);
        let mut bytes = vec![0u8; len as usize];
        let ok = CopySid(len, bytes.as_mut_ptr() as PSID, sid);
        LocalFree(sid as HLOCAL);
        if ok == 0 {
            return Err(WinFenceError::Spawn(format!(
                "CopySid for '{s}' failed: {}",
                GetLastError()
            )));
        }
        Ok(AppContainerSid { bytes })
    }
}

/// Whether the AppContainer fence is usable on this system. Probes profile
/// creation (Win8+); shares the process-wide cache with the runner.
pub(crate) fn available() -> bool {
    AppContainerProfile::create_or_open().is_ok()
}

/// Grant an AppContainer SID write access to `path` (a directory) and label it
/// Low integrity — the two halves of the *reversed* Windows fence.
///
/// Both security updates are applied in one `SetNamedSecurityInfoW` call so the
/// path never sits in an intermediate state (all-denied, or writable-but-not-
/// yet-low). Any failure leaves the path's previous security untouched and
/// returns an error (fail-closed).
pub(crate) fn grant_write_acl(path: &Path, sid: &AppContainerSid) -> Result<(), WinFenceError> {
    let wide = to_wide_nul(&path.to_string_lossy());
    unsafe {
        // --- 1. Read the object's current DACL (and the descriptor that owns
        // ---    it; the DACL pointer is only valid while the SD is alive).
        let mut pp_dacl: *mut ACL = null_mut();
        let mut pp_sd: PSECURITY_DESCRIPTOR = null_mut();
        let status = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut pp_dacl,
            null_mut(),
            &mut pp_sd,
        );
        if status != ERROR_SUCCESS {
            return Err(WinFenceError::acl(path, format!("GetNamedSecurityInfoW: {status}")));
        }

        // --- 2. Build a merged ACL: current DACL + one grant ACE for the
        // ---    AppContainer SID. The ACE inherits to children so files the
        // ---    child creates in the workspace stay writable.
        let old_dacl: *const ACL = if pp_dacl.is_null() { null() } else { pp_dacl };
        let trustee = TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.as_ptr() as PWSTR,
        };
        let explicit = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_GENERIC_READ
                | FILE_GENERIC_WRITE
                | FILE_GENERIC_EXECUTE
                | DELETE
                | FILE_DELETE_CHILD,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: trustee,
        };
        let mut new_dacl: *mut ACL = null_mut();
        let status = SetEntriesInAclW(1, &explicit, old_dacl, &mut new_dacl);
        if status != ERROR_SUCCESS {
            let reason = format!("SetEntriesInAclW: {status}");
            LocalFree(pp_sd as HLOCAL);
            return Err(WinFenceError::acl(path, reason));
        }

        // --- 3. Build the mandatory Low integrity label ACE. Placed on the
        // ---    workspace's system ACL with container+object inheritance so
        // ---    children inherit Low.
        let low_wide = to_wide_nul(LOW_MANDATORY_LABEL_SID);
        let mut low_sid: PSID = null_mut();
        if ConvertStringSidToSidW(low_wide.as_ptr(), &mut low_sid) == 0 {
            let reason = format!("ConvertStringSidToSidW(Low): {}", GetLastError());
            LocalFree(new_dacl as HLOCAL);
            LocalFree(pp_sd as HLOCAL);
            return Err(WinFenceError::acl(path, reason));
        }
        let mut label_acl = vec![0u8; 256];
        let p_label_acl = label_acl.as_mut_ptr() as *mut ACL;
        let acl_ok = InitializeAcl(p_label_acl, label_acl.len() as u32, ACL_REVISION) != 0
            && AddMandatoryAce(
                p_label_acl,
                ACL_REVISION,
                CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
                TOKEN_MANDATORY_POLICY_NO_WRITE_UP,
                low_sid,
            ) != 0;
        LocalFree(low_sid as HLOCAL);
        if !acl_ok {
            let reason = format!("InitializeAcl/AddMandatoryAce: {}", GetLastError());
            LocalFree(new_dacl as HLOCAL);
            LocalFree(pp_sd as HLOCAL);
            return Err(WinFenceError::acl(path, reason));
        }

        // --- 4. Apply DACL + mandatory label atomically (one call).
        let status = SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_dacl,
            p_label_acl,
        );
        LocalFree(new_dacl as HLOCAL);
        LocalFree(pp_sd as HLOCAL);
        if status != ERROR_SUCCESS {
            return Err(WinFenceError::acl(path, format!("SetNamedSecurityInfoW: {status}")));
        }
        Ok(())
    }
}

/// A Windows Job object configured with `KILL_ON_JOB_CLOSE`.
///
/// - `kill()`/`terminate()` → `TerminateJobObject`: whole-tree termination.
/// - Dropping the last job handle closes the object, and the kernel terminates
///   any processes still in it — the no-orphan backstop.
struct Job {
    handle: OwnedHandle,
}

impl Job {
    fn new() -> Result<Job, WinFenceError> {
        unsafe {
            let h = CreateJobObjectW(null(), null());
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                return Err(last_error("CreateJobObjectW"));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                let _ = CloseHandle(h);
                return Err(last_error("SetInformationJobObject"));
            }
            Ok(Job {
                handle: OwnedHandle::from_raw_handle(h),
            })
        }
    }

    /// Assign a process to the job. Must happen before the child can run
    /// (the caller creates it suspended) so no grandchild escapes the fence.
    fn assign(&self, process: HANDLE) -> Result<(), WinFenceError> {
        unsafe {
            if AssignProcessToJobObject(self.handle.as_raw_handle(), process) == 0 {
                return Err(last_error("AssignProcessToJobObject"));
            }
            Ok(())
        }
    }
}

/// The result of a fenced spawn: the process/job handles and the parent-side
/// pipe ends, ready to be turned into an [`AppContainerChild`].
pub(crate) struct Launched {
    pub(crate) pid: u32,
    process: OwnedHandle,
    job: OwnedHandle,
    stdout: Option<Box<dyn AsyncRead + Unpin + Send>>,
    stderr: Option<Box<dyn AsyncRead + Unpin + Send>>,
    stdin: Option<Box<dyn AsyncWrite + Unpin + Send>>,
}

impl Launched {
    pub(crate) fn into_child(self) -> AppContainerChild {
        AppContainerChild {
            pid: self.pid,
            process: self.process,
            job: self.job,
            stdout: self.stdout,
            stderr: self.stderr,
            stdin: self.stdin,
        }
    }
}

/// A live AppContainer-fenced child, exposed through the [`ProcessChild`]
/// trait. `kill()` terminates the whole job tree; dropping the child closes
/// the job handle, which kills whatever is left (`KILL_ON_JOB_CLOSE`).
pub(crate) struct AppContainerChild {
    pid: u32,
    process: OwnedHandle,
    job: OwnedHandle,
    stdout: Option<Box<dyn AsyncRead + Unpin + Send>>,
    stderr: Option<Box<dyn AsyncRead + Unpin + Send>>,
    stdin: Option<Box<dyn AsyncWrite + Unpin + Send>>,
}

impl AppContainerChild {
    /// Take the child's stdin pipe (write end), for feeding `req.stdin`.
    pub(crate) fn take_stdin(&mut self) -> Option<Box<dyn AsyncWrite + Unpin + Send>> {
        self.stdin.take()
    }
}

#[async_trait]
impl ProcessChild for AppContainerChild {
    fn id(&self) -> Option<u32> {
        Some(self.pid)
    }

    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Unpin + Send>> {
        self.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Unpin + Send>> {
        self.stderr.take()
    }

    async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let handle = self
            .process
            .try_clone()
            .map_err(|e| std::io::Error::other(format!("cannot clone process handle: {e}")))?;
        // `WaitForSingleObject` blocks the thread; run it off the async reactor.
        tokio::task::spawn_blocking(move || unsafe {
            let r = WaitForSingleObject(handle.as_raw_handle(), INFINITE);
            if r != WAIT_OBJECT_0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut code: u32 = 0;
            if GetExitCodeProcess(handle.as_raw_handle(), &mut code) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(ExitStatus::from_raw(code))
        })
        .await
        .map_err(|e| std::io::Error::other(format!("fenced wait task failed: {e}")))?
    }

    async fn kill(&mut self) -> std::io::Result<()> {
        unsafe {
            // Terminate the whole job tree (direct child + any descendants).
            if TerminateJobObject(self.job.as_raw_handle(), 1) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
    }
}

/// Raw pipe ends for one fenced spawn. Child-side handles are inherited by the
/// child (via `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`); parent-side ends are kept
/// by the caller as tokio streams.
struct PipeSet {
    /// Child's stdin read end, or NUL when no stdin is requested.
    stdin_child: HANDLE,
    /// Parent's stdin write end (kept only when feeding input).
    stdin_parent: Option<HANDLE>,
    /// Parent's stdout read end.
    stdout_parent: Option<HANDLE>,
    /// Child's stdout write end, or NUL.
    stdout_child: HANDLE,
    /// Parent's stderr read end.
    stderr_parent: Option<HANDLE>,
    /// Child's stderr write end, or NUL.
    stderr_child: HANDLE,
    /// Shared NUL device handle used for every non-piped stream.
    nul: HANDLE,
    /// All handle closures are explicit; this only guards against a forgotten
    /// close on an early-return path.
    closed: bool,
}

/// Deduplicate handle values regardless of order. Raw pointers are not `Ord`,
/// so compare (and close) via the numeric value; zero (null) is skipped.
fn dedup_handles(handles: &[HANDLE]) -> Vec<HANDLE> {
    let mut vals: Vec<usize> = handles.iter().map(|h| *h as usize).collect();
    vals.sort_unstable();
    vals.dedup();
    vals.into_iter()
        .filter(|v| *v != 0)
        .map(|v| v as HANDLE)
        .collect()
}

impl PipeSet {
    fn child_handles(&self) -> [HANDLE; 3] {
        [self.stdin_child, self.stdout_child, self.stderr_child]
    }

    /// Close the parent's copies of the child-side handles (the child holds
    /// its own inherited copies after spawn). Handles are deduplicated first:
    /// every non-piped stream aliases the shared NUL handle, so a naive
    /// per-field close would double-close it.
    fn close_child_side(&mut self) {
        let to_close = dedup_handles(&[
            self.stdin_child,
            self.stdout_child,
            self.stderr_child,
            self.nul,
        ]);
        for h in to_close {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
        self.stdin_child = null_mut();
        self.stdout_child = null_mut();
        self.stderr_child = null_mut();
    }

    /// Take the parent-side ends for conversion into tokio streams.
    fn take_parent_ends(&mut self) -> (Option<HANDLE>, Option<HANDLE>, Option<HANDLE>) {
        let out = (self.stdout_parent.take(), self.stderr_parent.take(), self.stdin_parent.take());
        self.closed = true;
        out
    }
}

impl Drop for PipeSet {
    fn drop(&mut self) {
        if !self.closed {
            self.close_child_side();
            unsafe {
                for h in self
                    .stdout_parent
                    .iter()
                    .chain(self.stderr_parent.iter())
                    .chain(self.stdin_parent.iter())
                {
                    let _ = CloseHandle(*h);
                }
            }
        }
    }
}

fn create_pipe() -> Result<(HANDLE, HANDLE), WinFenceError> {
    unsafe {
        let mut read: HANDLE = null_mut();
        let mut write: HANDLE = null_mut();
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            // Non-inheritable at creation; the inheritable subset is chosen
            // explicitly via PROC_THREAD_ATTRIBUTE_HANDLE_LIST.
            bInheritHandle: 0,
        };
        if CreatePipe(&mut read, &mut write, &sa, 0) == 0 {
            return Err(WinFenceError::Stdio(format!("CreatePipe: {}", GetLastError())));
        }
        Ok((read, write))
    }
}

/// Open the NUL device (the `\\.\NUL` equivalent of `/dev/null`).
fn open_nul() -> Result<HANDLE, WinFenceError> {
    let name = to_wide_nul("NUL");
    unsafe {
        let h = CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return Err(WinFenceError::Stdio(format!("CreateFileW(NUL): {}", GetLastError())));
        }
        Ok(h)
    }
}

fn to_wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Build a `CREATE_UNICODE_ENVIRONMENT` block: `KEY=VALUE\0` entries followed
/// by a final `\0`. When `env_clear` is set only `req.env` is included,
/// otherwise the current process environment with `req.env` overlaid.
fn build_env_block(req: &ProcessRequest) -> Vec<u16> {
    let mut vars: Vec<(String, String)> = if req.env_clear {
        Vec::new()
    } else {
        std::env::vars().collect()
    };
    for (k, v) in &req.env {
        if let Some(slot) = vars.iter_mut().find(|(ek, _)| ek == k) {
            slot.1 = v.clone();
        } else {
            vars.push((k.clone(), v.clone()));
        }
    }
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(k.encode_utf16());
        block.push(b'=' as u16);
        block.extend(v.encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

/// Quote an argv into a `CreateProcessW` command line. Each argument follows
/// the "Parsing C Command-Line Arguments" rules: arguments containing
/// whitespace or quotes are wrapped in double quotes, inner quotes are escaped
/// as `\"`, and trailing backslashes are doubled before a closing quote.
fn quote_argv(argv: &[String]) -> String {
    let mut out = String::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&quote_arg(arg));
    }
    out
}

fn quote_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '"'))
    {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat('\\').take(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    out.extend(std::iter::repeat('\\').take(backslashes * 2));
    out.push('"');
    out
}

/// Spawn `req` inside the AppContainer profile, returning the live child and
/// its pipe ends. The child is created suspended, assigned to a
/// `KILL_ON_JOB_CLOSE` Job, then resumed — so it can never run outside the
/// fence or escape tree-termination.
pub(crate) fn launch_fenced(
    profile: &AppContainerProfile,
    req: &ProcessRequest,
    capture_stdio: bool,
) -> Result<Launched, WinFenceError> {
    let cmdline = quote_argv(&req.argv);
    let mut cmdline_wide = to_wide_nul(&cmdline);
    let cwd_wide = req.cwd.as_ref().map(|p| to_wide_nul(&p.to_string_lossy()));
    let cwd_ptr: PCWSTR = match &cwd_wide {
        Some(w) => w.as_ptr(),
        None => null(),
    };
    let env_block = build_env_block(req);

    // --- stdio ---
    let mut pipes = PipeSet {
        stdin_child: null_mut(),
        stdin_parent: None,
        stdout_parent: None,
        stdout_child: null_mut(),
        stderr_parent: None,
        stderr_child: null_mut(),
        nul: open_nul()?,
        closed: false,
    };
    if req.stdin.is_some() {
        let (read, write) = create_pipe()?;
        pipes.stdin_child = read;
        pipes.stdin_parent = Some(write);
    } else {
        pipes.stdin_child = pipes.nul;
    }
    if capture_stdio {
        let (read, write) = create_pipe()?;
        pipes.stdout_parent = Some(read);
        pipes.stdout_child = write;
        let (read, write) = create_pipe()?;
        pipes.stderr_parent = Some(read);
        pipes.stderr_child = write;
    } else {
        pipes.stdout_child = pipes.nul;
        pipes.stderr_child = pipes.nul;
    }

    let job = Job::new()?;

    unsafe {
        // --- proc-thread attribute list (two-phase init) ---
        // Two attributes: the AppContainer security capabilities and the
        // explicit inheritable-handle whitelist.
        let mut attr_size: usize = 0;
        let init = InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut attr_size);
        if init == 0 && GetLastError() != ERROR_INSUFFICIENT_BUFFER {
            return Err(last_error("InitializeProcThreadAttributeList (sizing)"));
        }
        let mut attr_buf = vec![0u64; attr_size.div_ceil(8)];
        let attr_list = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(attr_list, 2, 0, &mut attr_size) == 0 {
            return Err(last_error("InitializeProcThreadAttributeList"));
        }

        let finish_attr = |rc: Result<Launched, WinFenceError>| {
            DeleteProcThreadAttributeList(attr_list);
            rc
        };

        // --- attribute 1: AppContainer security capabilities ---
        // Build the capability set before touching the attribute list; on
        // failure it must still be deleted, so route through `finish_attr`.
        // `fence_network` turns the set into the empty one, which is what
        // removes network access from the token.
        let deny_network = req.fence.as_ref().is_some_and(|f| f.deny_network);
        let mut caps = if deny_network {
            no_capabilities()
        } else {
            match network_capabilities() {
                Ok(caps) => caps,
                Err(e) => return finish_attr(Err(e)),
            }
        };
        let sid = profile.sid();
        let mut sec_caps = SECURITY_CAPABILITIES {
            AppContainerSid: sid.as_ptr(),
            Capabilities: caps.attrs.as_mut_ptr(),
            CapabilityCount: caps.attrs.len() as u32,
            Reserved: 0,
        };
        let ok = UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            &mut sec_caps as *mut SECURITY_CAPABILITIES as *const c_void,
            size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null(),
        );
        if ok == 0 {
            return finish_attr(Err(last_error(
                "UpdateProcThreadAttribute(SECURITY_CAPABILITIES)",
            )));
        }

        // --- attribute 2: inheritable handle whitelist ---
        // (dedup handles the shared NUL aliasing; the list is always
        // non-empty because every stream points at pipe-or-NUL)
        let mut inherit = dedup_handles(&pipes.child_handles());
        let ok = UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            inherit.as_mut_ptr() as *const c_void,
            inherit.len() * size_of::<HANDLE>(),
            null_mut(),
            null(),
        );
        if ok == 0 {
            return finish_attr(Err(last_error("UpdateProcThreadAttribute(HANDLE_LIST)")));
        }

        // --- STARTUPINFOEXW ---
        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = pipes.stdin_child;
        si.StartupInfo.hStdOutput = pipes.stdout_child;
        si.StartupInfo.hStdError = pipes.stderr_child;
        si.lpAttributeList = attr_list;

        // --- CreateProcessW (suspended so the job assignment can't lose the
        // --- race with a fast-exiting or forking child) ---
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let flags = CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT;
        let ok = CreateProcessW(
            null(),
            cmdline_wide.as_mut_ptr(),
            null(),
            null(),
            1, // bInheritHandles: only the HANDLE_LIST subset is actually inherited
            flags,
            env_block.as_ptr() as *const c_void,
            cwd_ptr,
            &si as *const STARTUPINFOEXW as *const STARTUPINFOW,
            &mut pi,
        );
        if ok == 0 {
            let err = last_error("CreateProcessW");
            DeleteProcThreadAttributeList(attr_list);
            return Err(err);
        }
        DeleteProcThreadAttributeList(attr_list);

        // Assign to the job while suspended, then start it running.
        if let Err(e) = job.assign(pi.hProcess) {
            let _ = TerminateProcess(pi.hProcess, 1);
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(pi.hThread);
            return Err(e);
        }
        let _ = ResumeThread(pi.hThread);
        let _ = CloseHandle(pi.hThread);

        // Parent closes its copies of the child-side handles; the child keeps
        // its inherited ones.
        pipes.close_child_side();
        let (stdout_parent, stderr_parent, stdin_parent) = pipes.take_parent_ends();

        let stdout = stdout_parent.map(|h| {
            let f = std::fs::File::from_raw_handle(h);
            Box::new(tokio::fs::File::from_std(f)) as Box<dyn AsyncRead + Unpin + Send>
        });
        let stderr = stderr_parent.map(|h| {
            let f = std::fs::File::from_raw_handle(h);
            Box::new(tokio::fs::File::from_std(f)) as Box<dyn AsyncRead + Unpin + Send>
        });
        let stdin = stdin_parent.map(|h| {
            let f = std::fs::File::from_raw_handle(h);
            Box::new(tokio::fs::File::from_std(f)) as Box<dyn AsyncWrite + Unpin + Send>
        });

        Ok(Launched {
            pid: pi.dwProcessId,
            process: OwnedHandle::from_raw_handle(pi.hProcess),
            job: job.handle,
            stdout,
            stderr,
            stdin,
        })
    }
}

/// Describe the DACL at `path` (debug/test aid): the count of ACEs and whether
/// a mandatory Low label ACE is present.
#[cfg(test)]
fn label_acl_has_low_label(path: &Path) -> bool {
    let wide = to_wide_nul(&path.to_string_lossy());
    unsafe {
        let mut pp_sacl: *mut ACL = null_mut();
        let mut pp_sd: PSECURITY_DESCRIPTOR = null_mut();
        let status = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut pp_sacl,
            &mut pp_sd,
        );
        if status != ERROR_SUCCESS || pp_sacl.is_null() {
            return false;
        }
        let count = (*pp_sacl).AceCount;
        let mut found = false;
        for i in 0..count {
            let mut ace: *mut c_void = null_mut();
            if GetAce(pp_sacl, i as u32, &mut ace) == 0 {
                continue;
            }
            let header = &*(ace as *const ACE_HEADER);
            if header.AceType == SYSTEM_MANDATORY_LABEL_ACE_TYPE {
                found = true;
                break;
            }
        }
        LocalFree(pp_sd as HLOCAL);
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn argv(parts: &[&str]) -> ProcessRequest {
        ProcessRequest {
            argv: parts.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn quote_arg_plain_is_unchanged() {
        assert_eq!(quote_arg("echo"), "echo");
        assert_eq!(quote_arg("C:\\Windows\\System32\\cmd.exe"), "C:\\Windows\\System32\\cmd.exe");
    }

    #[test]
    fn quote_arg_wraps_whitespace() {
        assert_eq!(quote_arg("echo hi"), "\"echo hi\"");
        assert_eq!(quote_arg("a\tb"), "\"a\tb\"");
    }

    #[test]
    fn quote_arg_escapes_quotes_and_trailing_backslashes() {
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_arg("a\\"), "\"a\\\\\"");
        assert_eq!(quote_arg("a\\\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn quote_argv_joins_with_single_spaces() {
        let v = vec![
            "C:\\cmd.exe".to_string(),
            "/c".to_string(),
            "echo hi".to_string(),
        ];
        assert_eq!(quote_argv(&v), "C:\\cmd.exe /c \"echo hi\"");
    }

    #[test]
    fn profile_name_is_package_legal() {
        let re = regex::Regex::new(r"^[A-Za-z0-9.-]+$").unwrap();
        assert!(re.is_match(PROFILE_NAME), "profile name must be package-legal");
        assert!(!PROFILE_NAME.contains(' '));
        assert!(!PROFILE_NAME.contains('_'));
    }

    #[test]
    fn network_capability_sids_are_the_well_known_values() {
        assert_eq!(INTERNET_CLIENT_SID, "S-1-15-2-1");
        assert_eq!(INTERNET_CLIENT_SERVER_SID, "S-1-15-2-2");
        assert_eq!(PRIVATE_NETWORK_CLIENT_SERVER_SID, "S-1-15-2-3");
    }

    #[test]
    fn attribute_constants_match_win32_headers() {
        assert_eq!(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, 0x0002_0009);
        assert_eq!(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, 0x0002_0002);
    }

    #[test]
    fn env_block_uses_double_nul_terminator() {
        let req = ProcessRequest {
            env_clear: true,
            env: HashMap::from([("A".to_string(), "1".to_string())]),
            ..Default::default()
        };
        let block = build_env_block(&req);
        assert_eq!(block, "A=1\0\0".encode_utf16().collect::<Vec<u16>>());
    }

    #[test]
    fn env_block_does_not_clear_when_env_clear_is_false() {
        let req = argv(&["cmd.exe"]);
        let block = build_env_block(&req);
        // Current process env is present (PATH at least) and double-nul ended.
        assert!(block.len() > 2);
        assert_eq!(block[block.len() - 1], 0);
        assert_eq!(block[block.len() - 2], 0);
    }

    #[test]
    fn string_to_sid_roundtrips_well_known_sid() {
        let sid = string_to_sid("S-1-15-2-1").expect("parse S-1-15-2-1");
        assert!(sid.bytes.len() >= 8);
    }

    #[test]
    fn fence_requires_app_container_support_to_run() {
        // On supported Windows this is true; on unsupported it is false. The
        // important invariant is that available() never panics and that a
        // False result is what the runner uses to fail closed.
        let _ = available();
    }

    #[test]
    fn quote_argv_covers_real_tool_invocations() {
        // Command lines the tools actually build: paths with spaces are
        // quoted, and each argument stays present as a token.
        let cases = [
            vec!["C:\\Program Files\\git\\bin\\bash.exe".to_string()],
            vec![
                "cmd.exe".to_string(),
                "/c".to_string(),
                "echo \"hi there\"".to_string(),
            ],
            vec!["node".to_string(), "script with space.js".to_string()],
            vec![
                "powershell".to_string(),
                "-Command".to_string(),
                "Write-Host 'a\\b'".to_string(),
            ],
        ];
        for argv in cases {
            let line = quote_argv(&argv);
            assert!(!line.is_empty());
            // The first token must survive verbatim (quoted when it has spaces).
            assert!(line.starts_with(&argv[0]) || line.starts_with(&format!("\"{}\"", argv[0])));
        }
    }
}
