// `tauri_utils::build::link_apple_library` is cfg(target_os = "macos"), so the
// Swift link helpers below only exist on a macOS build host (they're compiled
// for the host regardless of the cross-target).
#[cfg(target_os = "macos")]
use std::env;
#[cfg(target_os = "macos")]
use std::{
    fs, io,
    path::{Path, PathBuf},
};

fn main() {
    tauri_build::build();
    // Android: the cdylib's unwind tables (from cc-compiled C deps) reference
    // `__gxx_personality_v0`, which lives in libc++. Linking `-lc++_shared`
    // records it in DT_NEEDED so tauri-cli bundles libc++_shared.so into the
    // APK and the Android linker resolves the symbol at dlopen time.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-arg=-lc++_shared");
    }
    // iOS: compile the app's Swift plugin (mobile-migration §4.4/§4.6) into the
    // static library via swift-rs. This mirrors how tauri-plugin crates link
    // their `ios/` packages (tauri-plugin-2.x/src/build/mobile.rs): the Tauri
    // Swift framework is copied into `mobile-ios/.tauri/tauri-api` and the
    // plugin package's `Package.swift` resolves `Tauri` from that relative path.
    // `link_apple_library` is macOS-host-only, so the whole branch is gated.
    #[cfg(target_os = "macos")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        link_ios_swift();
    }
}

#[cfg(target_os = "macos")]
fn link_ios_swift() {
    let tauri_library_path = env::var("DEP_TAURI_IOS_LIBRARY_PATH")
        .expect("missing DEP_TAURI_IOS_LIBRARY_PATH; the `tauri` crate must be a dependency");
    let swift_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("mobile-ios");
    let tauri_dep = swift_dir.join(".tauri").join("tauri-api");
    fs::create_dir_all(&tauri_dep).expect("failed to create mobile-ios/.tauri");
    copy_dir_recursive(
        Path::new(&tauri_library_path),
        &tauri_dep,
        &[".build", "Package.resolved", "Tests"],
    )
    .expect("failed to copy the Tauri Swift framework into mobile-ios/.tauri/tauri-api");
    tauri_utils::build::link_apple_library("syscity-device", &swift_dir);
}

/// Recursively copy a directory tree, skipping any path segment that starts
/// with one of `ignore_prefixes` (same exclusion list as tauri-plugin).
#[cfg(target_os = "macos")]
fn copy_dir_recursive(source: &Path, target: &Path, ignore_prefixes: &[&str]) -> io::Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(source).unwrap();
        let rel_str = rel.to_string_lossy();
        if ignore_prefixes.iter().any(|p| rel_str.starts_with(p)) {
            continue;
        }
        let dest = target.join(rel);
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest, ignore_prefixes)?;
        } else {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}
