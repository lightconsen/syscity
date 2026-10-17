//! Rendering authored HTML to PDF, through the same browser the preview uses.
//!
//! This is why the export goes through the browser rather than a PDF library:
//! the document is arbitrary HTML an agent wrote, so what it looks like depends
//! on a full CSS engine. A partial renderer would not fail — it would produce a
//! plausible-looking PDF with a broken layout, which is the worse outcome.

/// How long a render may take before it is abandoned.
///
/// A page that never finishes loading would otherwise hold an export request —
/// and a browser tab — open indefinitely.
#[cfg(feature = "browser")]
const RENDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Render `html` to PDF bytes in a fresh tab of `pool`.
///
/// The tab is closed whether or not the render succeeds: a failed export must
/// not leak a page into a pool shared with the agent's own browsing.
#[cfg(feature = "browser")]
pub async fn html_to_pdf(
    pool: &crate::browser::BrowserPool,
    profile: &str,
    html: &str,
) -> crate::Result<Vec<u8>> {
    use chromiumoxide::cdp::browser_protocol::page::PrintToPdfParams;

    let instance = pool.get_or_create(profile).await?;
    let handle = instance.new_page("about:blank").await?;

    let rendered = async {
        handle.page.set_content(html).await.map_err(|e| {
            crate::error::SyscityError::ExternalService {
                source: format!("PDF export could not load the document: {}", e),
                cause: None,
            }
        })?;

        handle
            .page
            .pdf(PrintToPdfParams::default())
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: format!("PDF export could not render the document: {}", e),
                cause: None,
            })
    };

    let result = tokio::time::timeout(RENDER_TIMEOUT, rendered).await;

    // Best-effort: the tab is being discarded either way, and a close failure
    // says nothing about the document.
    if let Err(e) = instance.close_page(&handle.target_id).await {
        tracing::debug!("PDF export: could not close its tab: {}", e);
    }

    match result {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(crate::error::SyscityError::ExternalService {
            source: format!("PDF export did not finish within {}s", RENDER_TIMEOUT.as_secs()),
            cause: None,
        }),
    }
}
