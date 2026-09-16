//! Embedded web frontend assets
//!
//! Uses rust-embed to compile the built React app (`dist/`) into the binary
//! when the `embedded-assets` feature is enabled. Without the feature, assets
//! are read from the filesystem at runtime, which avoids recompiling the Rust
//! binary on every frontend change during development.

// INVARIANTS-NONE: compiled-in web assets; nothing to check
#[cfg(feature = "embedded-assets")]
use rust_embed::Embed;

#[cfg(feature = "embedded-assets")]
#[derive(Embed)]
#[folder = "dist/"]
pub struct WebAssets;

/// Look up an asset by its request path.
///
/// Vite places hashed JS/CSS under `dist/assets/`, so the embedded key
/// is either the direct path or `"assets/{path}"`.  This helper tries both
/// and returns `(bytes, mime_type)` on success.
///
/// When `embedded-assets` is enabled, assets are served from the compiled-in
/// bundle. Otherwise they are read from the `dist/` directory at runtime.
pub fn get_asset(path: &str) -> Option<(Vec<u8>, &'static str)> {
    let keys = [path.to_string(), format!("assets/{}", path)];

    #[cfg(feature = "embedded-assets")]
    {
        for key in &keys {
            if let Some(file) = WebAssets::get(key) {
                let mime = guess_mime(key);
                return Some((file.data.to_vec(), mime));
            }
        }
    }

    // Filesystem fallback for development or when embedding is disabled.
    // Vite outputs the built app to ../dist, so look there rather than the cwd.
    for key in &keys {
        let dist_key = format!("dist/{}", key);
        if let Ok(data) = std::fs::read(&dist_key) {
            let mime = guess_mime(&dist_key);
            return Some((data, mime));
        }
        if let Ok(data) = std::fs::read(key) {
            let mime = guess_mime(key);
            return Some((data, mime));
        }
    }

    None
}

/// Read an asset as a UTF-8 string.
///
/// Tries embedded assets first when the feature is enabled, then falls back to
/// the filesystem.
pub fn get_asset_string(path: &str) -> Option<String> {
    #[cfg(feature = "embedded-assets")]
    {
        if let Some(file) = WebAssets::get(path) {
            return Some(String::from_utf8_lossy(file.data.as_ref()).to_string());
        }
    }

    std::fs::read_to_string(format!("dist/{}", path))
        .ok()
        .or_else(|| std::fs::read_to_string(path).ok())
}

/// Guess MIME type from file extension for embedded assets.
pub fn guess_mime(path: &str) -> &'static str {
    if path.ends_with(".js") || path.ends_with(".mjs") {
        "application/javascript"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".woff") {
        "font/woff"
    } else if path.ends_with(".ttf") {
        "font/ttf"
    } else if path.ends_with(".html") {
        "text/html"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else {
        "application/octet-stream"
    }
}
