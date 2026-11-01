//! Mobile runners: Android routes through `/system/bin/sh` and bundled
//! native binaries; iOS has no `fork`/`exec` at all.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use super::{
    CommandOutput, ProcessChild, ProcessError, ProcessRequest, ProcessRunner, StdProcessRunner,
};

/// Android: the only executable entry points are `/system/bin/sh` (which
/// resolves the toybox applets by its built-in PATH) and bundled native
/// binaries shipped in `jniLibs` and extracted to `nativeLibraryDir`.
/// SELinux blocks `exec` from the app-private `filesDir` for targetSdk 29+,
/// so everything else is rejected (docs/mobile-migration.md §3.1).
#[cfg(target_os = "android")]
#[derive(Debug, Clone)]
pub struct AndroidShellRunner {
    native_library_dir: Option<PathBuf>,
    whitelist: Arc<HashSet<&'static str>>,
    inner: StdProcessRunner,
}

#[cfg(target_os = "android")]
const TOYBOX_APPLETS: &[&str] = &[
    "sh",
    "/bin/sh",
    "/system/bin/sh",
    "ls",
    "cat",
    "echo",
    "printf",
    "pwd",
    "cp",
    "mv",
    "rm",
    "rmdir",
    "mkdir",
    "touch",
    "chmod",
    "chown",
    "grep",
    "sed",
    "awk",
    "wc",
    "head",
    "tail",
    "sort",
    "uniq",
    "find",
    "xxd",
    "base64",
    "date",
    "seq",
    "tr",
    "cut",
    "paste",
    "dirname",
    "basename",
    "stat",
    "df",
    "du",
    "ps",
    "sleep",
    "test",
    "which",
];

#[cfg(target_os = "android")]
impl AndroidShellRunner {
    /// Build the Android runner. `nativeLibraryDir` is read from
    /// `SYSCITY_NATIVE_LIB_DIR` (set by `MainActivity.kt` next to
    /// `SYSCITY_HOME`); bundled binaries are exec'd from there.
    pub fn from_env() -> Self {
        Self {
            native_library_dir: std::env::var("SYSCITY_NATIVE_LIB_DIR")
                .ok()
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
            whitelist: Arc::new(TOYBOX_APPLETS.iter().copied().collect()),
            inner: StdProcessRunner,
        }
    }

    /// Rewrite the request argv to an executable form permitted on Android.
    fn resolve_argv(&self, req: &ProcessRequest) -> Result<Vec<String>, ProcessError> {
        let program = req.argv.first().ok_or(ProcessError::EmptyArgv)?;

        // Bundled native binary (exec from nativeLibraryDir is the only
        // allowed exec path for same-UID binaries on targetSdk 29+).
        if let Some(dir) = &self.native_library_dir {
            // APK packaging keeps only `*.so` in jniLibs, so the bundled adb
            // ships as `libadb.so` (an ELF executable under a library name).
            // Both spellings are tried: a tree that still has the older `adb`
            // keeps working.
            //
            // The caller names the program `adb` while the file is `libadb.so`,
            // so the exact name has to be tried too — not just the prefixed
            // form of whatever it asked for, which would look for `libadb.so.so`.
            let bundled = [
                dir.join(program),
                dir.join(format!("lib{program}.so")),
                dir.join(format!("{program}.so")),
            ]
            .into_iter()
            .find(|candidate| candidate.exists());
            if let Some(bundled) = bundled {
                let mut argv = vec![bundled.to_string_lossy().into_owned()];
                argv.extend(req.argv[1..].iter().cloned());
                return Ok(argv);
            }
        }

        // The shell itself, or a whitelisted toybox applet routed through it
        // so the applet resolves via sh's built-in PATH.
        if self.whitelist.contains(program.as_str()) {
            return Ok(req.argv.clone());
        }

        Err(ProcessError::Unsupported)
    }
}

#[cfg(target_os = "android")]
impl AndroidShellRunner {
    /// Point a bundled native binary at its sibling libraries.
    ///
    /// The bundled `adb` client (mobile-migration §4.5) is dynamically linked
    /// against `libprotobuf.so`, `libabsl_*.so`, … shipped alongside it in
    /// nativeLibraryDir; its DT_RUNPATH points at a Termux path that does not
    /// exist here. Bionic honors `LD_LIBRARY_PATH` for non-setuid app
    /// processes, so set it to nativeLibraryDir for the bundled-exec path.
    /// `sh`/toybox need nothing (they only use bionic) and are untouched.
    /// Where the bundled adb keeps its keys and settings.
    ///
    /// adb creates `$HOME/.android` on first use and **aborts** when it cannot
    /// (`Cannot mkdir '/data/data/com.termux/files/home/.android'`) — that is
    /// Termux's home baked into the binary, which does not exist here. The app
    /// sandbox is the only writable place it has.
    fn bundled_home() -> std::path::PathBuf {
        std::env::var("SYSCITY_HOME")
            .map(|home| std::path::PathBuf::from(home).join("data/adb"))
            .unwrap_or_else(|_| std::env::temp_dir().join("syscity-adb"))
    }

    fn apply_bundled_library_path(&self, eff: &mut ProcessRequest) {
        let Some(dir) = &self.native_library_dir else {
            return;
        };
        let Some(program) = eff.argv.first().map(String::as_str) else {
            return;
        };
        // Whether this program is one of the bundled ones — under either name,
        // because the file is `libadb.so` while the caller says `adb`.
        let bundled = dir.join(program).exists() || dir.join(format!("lib{program}.so")).exists();
        if bundled {
            let dir_str = dir.to_string_lossy().into_owned();
            eff.env
                .insert("LD_LIBRARY_PATH".to_string(), dir_str.clone());

            // `libz` and `libzstd` keep a versioned SONAME (`libz.so.1`) that
            // the executable asks for, but have to ship as plain `libz.so`:
            // Android's APK packaging only extracts names ending in `.so`.
            // Preloading them by path registers each under its SONAME, which is
            // what the dynamic linker then resolves `NEEDED libz.so.1` against.
            let preload: Vec<String> = ["libz.so", "libzstd.so"]
                .iter()
                .map(|name| dir.join(name))
                .filter(|path| path.exists())
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            if !preload.is_empty() {
                eff.env.insert("LD_PRELOAD".to_string(), preload.join(":"));
            }

            // adb's own config root. `HOME` for the older lookup, the
            // dedicated variable for current adb; both point at the same
            // sandbox directory, created by the caller before the spawn.
            let home = Self::bundled_home();
            let home_str = home.to_string_lossy().into_owned();
            eff.env.insert("HOME".to_string(), home_str.clone());
            eff.env.insert("ANDROID_USER_HOME".to_string(), home_str);
        }
    }
}

#[cfg(target_os = "android")]
#[async_trait]
impl ProcessRunner for AndroidShellRunner {
    async fn run(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        let mut eff = req.clone();
        eff.argv = self.resolve_argv(req)?;
        self.apply_bundled_library_path(&mut eff);
        let _ = tokio::fs::create_dir_all(Self::bundled_home()).await;
        // `run_collect` rather than `run`: it is the StdProcessRunner path that
        // honours `req.stdin`, and the display helper's launch carries a nonce
        // there. A timed-out run still surfaces as an error, as `run` promises.
        let out = self.inner.run_collect(&eff).await?;
        if out.timed_out {
            return Err(ProcessError::Timeout {
                duration: req.timeout.unwrap_or_default(),
            });
        }
        Ok(out)
    }

    async fn run_collect(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        let mut eff = req.clone();
        eff.argv = self.resolve_argv(req)?;
        self.apply_bundled_library_path(&mut eff);
        let _ = tokio::fs::create_dir_all(Self::bundled_home()).await;
        self.inner.run_collect(&eff).await
    }

    async fn spawn(&self, req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError> {
        let mut eff = req.clone();
        eff.argv = self.resolve_argv(req)?;
        self.apply_bundled_library_path(&mut eff);
        self.inner.spawn(&eff).await
    }
}

/// iOS: the sandbox forbids `fork`/`exec` for app code, so every process
/// call fails with [`ProcessError::Unsupported`] (docs/mobile-migration.md
/// §3.2).
#[cfg(target_os = "ios")]
#[derive(Debug, Clone, Copy, Default)]
pub struct IosProcessRunner;

#[cfg(target_os = "ios")]
#[async_trait]
impl ProcessRunner for IosProcessRunner {
    async fn run(&self, _req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        Err(ProcessError::Unsupported)
    }

    async fn spawn(&self, _req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError> {
        Err(ProcessError::Unsupported)
    }
}
