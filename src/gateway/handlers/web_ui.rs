use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{Html, IntoResponse},
};

/// The embedded SPA index.html with the version placeholder filled in.
fn embedded_index_html() -> String {
    match crate::embed::get_asset_string("index.html") {
        Some(html) => html,
        None => {
            "<h1>Syscity Chat UI</h1><p>Build not found. Run: cd web and pnpm build</p>".to_string()
        }
    }
    .replace("{VERSION}", crate::VERSION)
}

/// HTML handler for the web chat UI
///
/// Serves the built React app from embedded assets (or filesystem fallback).
pub async fn web_terminal_html_handler() -> Html<String> {
    Html(embedded_index_html())
}

/// HTML handler for the cloud OAuth return URL (`/cloud/login/callback`).
///
/// Same SPA as `/`, but with relative asset URLs rewritten to absolute: the
/// vite build uses `base: './'`, so `./assets/*` only resolves at the site
/// root — at this nested path the script/style tags would 404 and the SPA
/// (which reads `#token=`) would never mount.
pub async fn cloud_login_callback_html_handler() -> Html<String> {
    Html(
        embedded_index_html()
            .replace("href=\"./", "href=\"/")
            .replace("src=\"./", "src=\"/"),
    )
}

/// Favicon handler — serves the syscity PNG favicon
pub async fn favicon_handler() -> impl IntoResponse {
    if let Some((data, mime)) = crate::embed::get_asset("syscity.png") {
        return ([(header::CONTENT_TYPE, mime)], data).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

/// Asset handler — serves JS/CSS/fonts from embedded assets (or filesystem
/// fallback).
pub async fn asset_handler(Path(path): Path<String>) -> impl IntoResponse {
    // Try embedded assets first (handles both direct keys and "assets/" prefix).
    if let Some((data, mime)) = crate::embed::get_asset(&path) {
        return ([(header::CONTENT_TYPE, mime)], data).into_response();
    }

    StatusCode::NOT_FOUND.into_response()
}

/// Service worker handler — serves the Vite PWA service worker registration
/// script.
pub async fn register_sw_handler() -> impl IntoResponse {
    let js = crate::embed::get_asset_string("registerSW.js").unwrap_or_else(|| {
        "if('serviceWorker' in \
         navigator){window.addEventListener('load',()=>{navigator.serviceWorker.register('.\
         /sw.js',{scope:'./'})})}"
            .to_string()
    });
    ([(header::CONTENT_TYPE, "application/javascript")], js)
}

/// Service worker script handler — serves the vite-plugin-pwa generated
/// `sw.js` (workbox runtime inlined, so no sibling chunk import is needed).
/// A dedicated handler without a `Path` extractor: the route is a literal
/// path, and extracting `Path` from it makes axum reject every request
/// with a 500.
pub async fn sw_js_handler() -> impl IntoResponse {
    if let Some((data, _)) = crate::embed::get_asset("sw.js") {
        return ([(header::CONTENT_TYPE, "application/javascript")], data).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

/// Web app manifest handler — serves the Vite PWA manifest.
pub async fn manifest_handler() -> impl IntoResponse {
    let manifest = crate::embed::get_asset_string("manifest.webmanifest").unwrap_or_default();
    ([(header::CONTENT_TYPE, "application/manifest+json")], manifest)
}

/// Logo handler for /syscity.png — static route with no path params.
pub async fn syscity_png_handler() -> impl IntoResponse {
    let path = "syscity.png";
    if let Some((data, mime)) = crate::embed::get_asset(path) {
        return ([(header::CONTENT_TYPE, mime)], data).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;

    async fn body(resp: axum::response::Response) -> (StatusCode, Vec<u8>) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec();
        (status, bytes)
    }

    #[tokio::test]
    async fn web_terminal_serves_html() {
        let (status, bytes) = body(web_terminal_html_handler().await.into_response()).await;
        assert_eq!(status, StatusCode::OK);
        let html = String::from_utf8_lossy(&bytes);
        assert!(
            html.contains("<html") || html.contains("Syscity Chat UI"),
            "serves html: {:.80}",
            html
        );
    }

    #[tokio::test]
    async fn cloud_login_callback_rewrites_relative_assets() {
        let (status, bytes) = body(cloud_login_callback_html_handler().await.into_response()).await;
        assert_eq!(status, StatusCode::OK);
        let html = String::from_utf8_lossy(&bytes);
        // Relative asset refs (vite `base: './'`) must become absolute so the
        // SPA mounts at this nested path.
        assert!(!html.contains("href=\"./"), "relative href left: {:.200}", html);
        assert!(!html.contains("src=\"./"), "relative src left: {:.200}", html);
    }

    #[tokio::test]
    async fn favicon_served_with_png_mime() {
        let resp = favicon_handler().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(header::CONTENT_TYPE).unwrap(), "image/png");
        let (_, bytes) = body(resp).await;
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn asset_missing_returns_404() {
        let (status, _) = body(
            asset_handler(Path("nope-xyz.bin".into()))
                .await
                .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn asset_found_returns_content() {
        let resp = asset_handler(Path("favicon-32.png".into()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let (_, bytes) = body(resp).await;
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn register_sw_returns_javascript() {
        let resp = register_sw_handler().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(header::CONTENT_TYPE).unwrap(), "application/javascript");
        let (_, bytes) = body(resp).await;
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn sw_js_returns_javascript() {
        let resp = sw_js_handler().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(header::CONTENT_TYPE).unwrap(), "application/javascript");
        let (_, bytes) = body(resp).await;
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn manifest_returns_json() {
        let resp = manifest_handler().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(header::CONTENT_TYPE).unwrap(), "application/manifest+json");
        let (_, bytes) = body(resp).await;
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn syscity_png_served() {
        let resp = syscity_png_handler().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(header::CONTENT_TYPE).unwrap(), "image/png");
    }
}
