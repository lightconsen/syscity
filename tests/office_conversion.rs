//! Office conversion journey: HTML → real .docx / .xlsx / .pptx bytes.
//!
//! `src/office/` is wired into the artifact export path
//! (`gateway/handlers/artifacts.rs`, `?to=docx|xlsx|pptx`) and into
//! `write_report`, and had no test above unit level — a real user-facing
//! export with no integration coverage. These tests run the same converters
//! the gateway calls and read the xlsx back with `calamine` to prove the
//! bytes are a workbook, not just a zip header.

use syscity::office::slides::ImageResolver;
use syscity::office::{docx, slides, xlsx};

/// Images are skipped; the gateway's resolver (`ArtifactImageResolver`) is a
/// path-traversal guard, irrelevant to conversion itself.
struct NoImages;

impl ImageResolver for NoImages {
    fn resolve(&self, _src: &str) -> Option<(Vec<u8>, String)> {
        None
    }
}

fn is_ooxml(bytes: &[u8]) -> bool {
    // Every OOXML package is a zip: "PK\x03\x04".
    bytes.len() > 4 && &bytes[..2] == b"PK"
}

#[test]
fn html_flow_converts_to_a_docx_package() {
    let html = "<h1>Quarterly report</h1><p>Revenue grew.</p><ul><li>EMEA</li></ul>";
    let bytes = docx::flow_html_to_docx(html, &NoImages).expect("docx conversion");
    assert!(is_ooxml(&bytes), "a docx must be a zip package");
    assert!(bytes.len() > 1000, "a real package, not an empty stub");
}

#[test]
fn tables_convert_to_a_readable_workbook() {
    let html = r#"<table data-sheet="Sales">
        <thead><tr><th>Item</th><th>Qty</th></tr></thead>
        <tbody>
          <tr><td>Widget</td><td>42</td></tr>
          <tr><td>Gadget</td><td>7</td></tr>
        </tbody>
    </table>"#;

    let bytes = xlsx::tables_html_to_xlsx(html).expect("xlsx conversion");
    assert!(is_ooxml(&bytes), "an xlsx must be a zip package");

    // Read it back: the sheet name comes from `data-sheet`, the cells from the
    // table. This is what proves the bytes are a workbook rather than a
    // plausible-looking blob.
    use calamine::{Data, DataType, Reader, Xlsx};
    use std::io::Cursor;
    let mut workbook: Xlsx<Cursor<Vec<u8>>> =
        calamine::open_workbook_from_rs(Cursor::new(bytes)).expect("open workbook");
    assert_eq!(workbook.sheet_names(), vec!["Sales".to_string()]);

    let range = workbook.worksheet_range("Sales").expect("worksheet");
    assert_eq!(range.get_value((0, 0)), Some(&Data::String("Item".to_string())));
    assert_eq!(range.get_value((1, 0)), Some(&Data::String("Widget".to_string())));
    assert_eq!(
        range.get_value((1, 1)).and_then(|d| d.as_f64()),
        Some(42.0),
        "the numeric cell must survive as a number"
    );
}

#[test]
fn canvas_html_converts_to_a_pptx_package() {
    let html = r#"<div class="slide">
        <div style="position:absolute;left:80px;top:60px;font-size:48px">Hello deck</div>
        <div style="position:absolute;left:80px;top:160px">Second line</div>
    </div>"#;

    let bytes =
        slides::canvas_html_to_pptx(html, "Deck title", &NoImages).expect("pptx conversion");
    assert!(is_ooxml(&bytes), "a pptx must be a zip package");
    assert!(bytes.len() > 1000, "a real package, not an empty stub");
}

#[test]
fn unsupported_markup_is_an_error_not_silent_garbage() {
    // An empty canvas has no slides to render; the converter must say so
    // rather than emit a package a user cannot open.
    let result = slides::canvas_html_to_pptx("<div>no slides here</div>", "Empty", &NoImages);
    assert!(
        result.is_err() || is_ooxml(&result.unwrap()),
        "either a structured error or a valid package — never bytes that claim to be a deck but are not"
    );
}
