//! Office document generation — canvas-HTML → OOXML converters.
//!
//! The authoring model: the agent writes constrained HTML (the "canvas
//! contract"), the preview panel renders it directly (WYSIWYG), and these
//! converters turn it into Office files on download.
//!
// INVARIANTS-NONE: stateless document converters; own no runtime state.
//! Contracts per format:
//! - **Slides** (`slides.rs`): each `<div class="slide">` is a 1280×720 px
//!   canvas; children use absolute positioning in px. 1 px = 9525 EMU.
//! - **Docx** (`docx.rs`): flowing HTML subset (headings, paragraphs, lists,
//!   tables, inline emphasis) → docx-rs.
//! - **Xlsx** (`xlsx.rs`): table HTML subset (one `<table>` per sheet) →
//!   rust_xlsxwriter with value type inference.

pub mod docx;
pub mod slides;
pub mod xlsx;
