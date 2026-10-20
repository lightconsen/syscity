use std::process::Command;

fn main() {
    // Embed the git commit hash for version display at runtime
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_HASH={}", git_hash);

    // Named cfg aliases for platform families, so `#[cfg]` gates read
    // `desktop_os` / `mobile_os` instead of repeating
    // `any(target_os = "linux", target_os = "macos", target_os = "windows")`
    // at every site (and drifting out of sync).
    println!("cargo::rustc-check-cfg=cfg(desktop_os)");
    println!("cargo::rustc-check-cfg=cfg(mobile_os)");
    match std::env::var("CARGO_CFG_TARGET_OS")
        .unwrap_or_default()
        .as_str()
    {
        "linux" | "macos" | "windows" => println!("cargo:rustc-cfg=desktop_os"),
        "android" | "ios" => println!("cargo:rustc-cfg=mobile_os"),
        _ => {}
    }

    // ort (the `vision` feature) links a prebuilt libonnxruntime.a whose
    // Objective-C++ CoreML objects call clang's `___isPlatformVersionAtLeast`
    // helper for their `@available` checks. That symbol lives in clang's
    // compiler-rt (`libclang_rt.osx.a`), not in libSystem, and rustc links
    // with `-nodefaultlibs` so the driver never adds it. Debug builds get away
    // with it because those archive members are only pulled in when something
    // references them; the release profile's `lto = true` walks the whole
    // archive, and the link then fails on the undefined symbol.
    //
    // Linking compiler-rt explicitly is the fix. `rustc-link-search` /
    // `rustc-link-lib` rather than `rustc-link-arg`, so the requirement also
    // reaches downstream binaries (the Tauri desktop app links this crate as
    // an rlib).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var_os("CARGO_FEATURE_VISION").is_some()
    {
        match Command::new("clang")
            .args(["--print-resource-dir"])
            .output()
        {
            Ok(out) if out.status.success() => {
                let resource_dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let lib_dir = format!("{resource_dir}/lib/darwin");
                println!("cargo:rustc-link-search=native={lib_dir}");
                println!("cargo:rustc-link-lib=clang_rt.osx");
            }
            // No clang means no C dependency can be built either; let the
            // link report the missing symbol rather than failing here.
            _ => println!("cargo:warning=could not locate clang's resource dir"),
        }
    }
}
