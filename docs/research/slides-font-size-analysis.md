# Analysis: Font-Size Honoring in Canvas-HTML → PPTX Pipeline

**Date**: 2026-08-25
**Scope**: Research only — no code changes
**Files analyzed**: `src/office/slides.rs`, ppt-rs 0.2.25 source (local cache)

---

## 1. Problem Statement

`src/office/slides.rs` converts a canvas-HTML document (div.slide, 1280×720 px, absolutely
positioned children with inline CSS including `font-size`) into `.pptx` via ppt-rs 0.2.25.
The canvas contract specifies exact `font-size` values in px, but **ppt-rs ignores them** —
all shape text uses auto-fit sizing, where PowerPoint/calculated font size shrinks to fill
the bounding box.

### Root cause — exact code path

1. **`slides.rs:253-268`** — text elements call `Shape::new(...).with_text(text)`.
   The `Shape.text` field is `Option<String>` — a bare string with no formatting metadata.
   There is no `with_formatted_text()` or `with_runs()` method on Shape.

2. **`ppt-rs shapes_xml.rs:19`** — `generate_shape_xml` calls
   `generate_text_xml_with_autofit(&shape.text, shape.width, shape.height, fill_color)`.
   The function signature takes only `&Option<String>` — no font size parameter exists.

3. **`ppt-rs shapes_xml.rs:338-424`** — `generate_text_xml_with_autofit`:
   - Calls `calculate_font_size(text, width, height)` (line 369) which derives a font size
     from shape geometry and text length — **completely ignoring any user-specified value**.
   - Emits `<a:normAutofit/>` (line 390) inside `<a:bodyPr>`, which tells PowerPoint to
     further shrink text at runtime to fit the box.
   - Writes `sz="CALCULATED"` on `<a:rPr>` (line 396) — but `normAutofit` overrides it.

4. **Dead code path** — ppt-rs has a rich text model (`TextFormat` with `font_size: Option<u32>`,
   `FormattedText`, `Run` with `.size(points)`, `Paragraph`) that fully supports font sizing
   and generates correct `sz=` XML. But these types are **never used** by the Shape→XML
   pipeline. They're exported from `generator::text` but `shapes_xml.rs` doesn't import them.
   Confirmed: `grep -rn "Run\|FormattedText\|TextFormat" shapes_xml.rs shapes.rs` returns
   zero hits.

### The generated XML (from code analysis)

For a shape with text "季度回顾" in a 1120×100 px box, the generated XML is:

```xml
<p:sp>
<p:nvSpPr>
<p:cNvPr id="10" name="Shape 10"/>
<p:cNvSpPr/>
<p:nvPr/>
</p:nvSpPr>
<p:spPr>
<a:xfrm>
<a:off x="762000" y="571500"/>
<a:ext cx="10668000" cy="952500"/>
</a:xfrm>
<a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
</p:spPr>
<p:txBody>
<a:bodyPr wrap="square" rtlCol="0" anchor="ctr" lIns="91440" tIns="45720" rIns="91440" bIns="45720">
<a:normAutofit/>                          <!-- ← THE PROBLEM -->
</a:bodyPr>
<a:lstStyle/>
<a:p>
<a:pPr algn="ctr" marL="0" marR="0" indent="0"/>
<a:r>
<a:rPr lang="en-US" sz="3500" dirty="0">  <!-- ← CALCULATED, NOT HONORED -->
<a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:rPr>
<a:t>季度回顾</a:t>
</a:r>
</a:p>
</p:txBody>
</p:sp>
```

The `sz="3500"` (35pt) is computed from geometry. `<a:normAutofit/>` tells PowerPoint to
treat it as a starting point and shrink further if needed.

---

## 2. Option Analysis

### Option 1: ppt-rs Native Path

**Question**: Is there ANY public API path that carries font size to a positioned shape?

**Answer: NO** (in 0.2.25).

| Check | Result |
|-------|--------|
| `Shape` struct fields | `text: Option<String>` — no format field |
| `Shape::with_text()` | Takes `&str` only |
| `Shape` builder methods | `with_fill`, `with_line`, `with_text`, `with_rotation`, `with_hyperlink`, `with_id` — no `with_format`, `with_text_format`, `with_runs` |
| `TextFormat.font_size` | Exists (`Option<u32>` in points), generates correct `sz=` XML |
| `Run.size()` | Exists, generates `sz="2400"` for 24pt |
| `Paragraph` / `TextFrame` | Exist with full formatting support |
| Are these wired into Shape? | **NO** — zero imports of `Run`/`TextFormat` in `shapes_xml.rs` or `shapes.rs` |
| `SlideContent.title_size` / `content_size` | Only apply to layout-based title/content, not to shape text |
| Autofit-disable flag | None exists anywhere in the codebase |

**Newer versions**: 0.2.25 (2026-08-19) is the latest on crates.io. No newer version exists.
The repo at `github.com/yingkitw/ppt-rs` is active but no font-size-on-shape API has been
added. The `TextFormat`/`Run`/`Paragraph` types appear to be groundwork for a future integration
but are currently orphaned from the Shape path.

**Verdict**: ❌ Not viable. The types exist but are dead code in the shape pipeline. No
autofit-disable flag. Would require forking ppt-rs or submitting a PR and waiting.

---

### Option 2: Post-Generation XML Patch ⭐ RECOMMENDED

**Concept**: After `canvas_html_to_pptx` generates the PPTX bytes (via ppt-rs), reopen the
zip archive and patch the slide XML to:
1. Replace `<a:normAutofit/>` with `<a:noAutofit/>` (disable autofit)
2. Set `sz="NNNN"` on each `<a:rPr>` to the CSS `font-size` value in hundredths of a point

**This is the same approach already used for the 16:9 slide size patch**
(`patch_slide_size_16x9` at `slides.rs:308-349`), so the pattern is established.

#### Feasibility: Shape-to-Element Mapping

The critical question: how do we map parsed `CanvasElement`s (with their CSS font-size)
to the generated `<p:sp>` shapes in the slide XML?

**Answer: Deterministic 1:1 mapping by position order.**

From `slide_xml/content.rs:24-28`:
```rust
for (i, shape) in content.shapes.iter().enumerate() {
    let shape_id = shape.id.unwrap_or((i + 10) as u32);
    xml.push_str(&generate_shape_xml(shape, shape_id));
}
```

- Shapes are rendered **in insertion order** (the same order as `spec.elements`).
- Shape IDs start at 10 (or custom via `shape.id`).
- The `cNvPr` element has `id` and `name` attributes for identification.
- Background rects are inserted first (as the first shape), shifting element indices by 1.

**Mapping strategy**:
1. Parse CSS `font-size` from each `CanvasElement` during `parse_element` (add to struct).
2. Track element index → font-size mapping per slide.
3. Account for the background rect (if present, it's shape 0 with no text — skip it).
4. In the patch phase, iterate `<p:sp>` elements in each slide XML and match by ordinal
   position (or `cNvPr` id) to apply the correct font size.

#### XML Transform

For each text-bearing shape, the patch needs to:

**Before** (generated by ppt-rs):
```xml
<a:bodyPr wrap="square" rtlCol="0" anchor="ctr" ...>
  <a:normAutofit/>
</a:bodyPr>
...
<a:rPr lang="en-US" sz="3500" dirty="0">
```

**After** (patched):
```xml
<a:bodyPr wrap="square" rtlCol="0" anchor="ctr" ...>
  <a:noAutofit/>
</a:bodyPr>
...
<a:rPr lang="en-US" sz="2400" dirty="0">
```

Where `sz="2400"` = 24px CSS font-size × 100 (hundredths of a point, and CSS px ≈ pt at
96 DPI which is close enough for presentation text).

#### Risks and Mitigations

| Risk | Severity | Mitigation |
|------|----------|------------|
| `normAutofit` + explicit `sz` interaction | Low | Replace with `<a:noAutofit/>` — explicit `sz` is then respected |
| PowerPoint "repair" on open | Low | `<a:noAutofit/>` is valid OOXML; LibreOffice and Google Slides also support it |
| Text overflow when CSS font-size is too large for box | Medium | Accept as-is: the canvas HTML preview shows the same overflow behavior. Could optionally keep `normAutofit` as fallback for oversized text |
| XML parsing fragility | Low | Use string replacement (same as existing `patch_slide_size_16x9`), not a full XML parser. The generated XML is deterministic |
| Bullet text (multi-line) | Low | Same transform applies; bullets use the same shape XML path |
| Non-text shapes (background rects, images) | None | Skip: images are `<p:pic>` not `<p:sp>`; background rects have empty `<p:txBody>` |

#### Implementation Sketch

```rust
// 1. Add font_size to CanvasElement
pub struct CanvasElement {
    // ... existing fields ...
    pub font_size_px: Option<f64>,  // CSS font-size in px
}

// 2. Parse font-size in parse_element()
let font_size_px = style_get(&style, "font-size")
    .and_then(|v| v.trim().trim_end_matches("px").trim().parse().ok());

// 3. Add patch function (called after patch_slide_size_16x9)
fn patch_font_sizes(
    pptx: &[u8],
    specs: &[SlideSpec],
) -> Result<Vec<u8>, String> {
    // For each slide:
    //   For each <p:sp> in slide XML (by ordinal):
    //     If the corresponding CanvasElement has font_size_px:
    //       - Replace <a:normAutofit/> with <a:noAutofit/>
    //       - Replace sz="CALCULATED" with sz="(font_size_px * 100) as u32"
}

// 4. Chain in canvas_html_to_pptx:
let pptx = create_pptx_with_content(title, slides)?;
let pptx = patch_slide_size_16x9(&pptx)?;
let pptx = patch_font_sizes(&pptx, &specs)?;
Ok(pptx)
```

#### Test Plan

1. **Unit test**: Add a canvas HTML element with `font-size:24px`, verify the patched
   slide XML contains `sz="2400"` and `<a:noAutofit/>`.
2. **Unit test**: Verify elements without `font-size` retain `<a:normAutofit/>` and
   the calculated `sz` value.
3. **Integration test**: Generate a PPTX, unzip `ppt/slides/slide1.xml`, assert:
   - `sz="2400"` appears for a 24px element
   - `<a:noAutofit/>` replaces `<a:normAutofit/>` for font-size-bearing shapes
   - Background rect shapes are untouched
4. **Visual test**: Open in PowerPoint/LibreOffice/Google Slides — verify text renders
   at the specified size without "repair" prompts.

#### Effort Estimate

**~2-3 hours** of implementation:
- Add `font_size_px` to `CanvasElement` (~10 lines)
- Parse `font-size` CSS (~5 lines)
- Write `patch_font_sizes` function (~60-80 lines, modeled on existing `patch_slide_size_16x9`)
- Tests (~40 lines)

**Verdict**: ✅ **RECOMMENDED** — lowest effort, lowest risk, follows established pattern,
preserves the canvas contract, doesn't add dependencies.

---

### Option 3: ooxmlsdk Backend Swap

**ooxmlsdk** (by KaiserY, v0.11.0, pure Rust, inspired by .NET Open XML SDK) provides
full control over OOXML elements including text body properties, run properties, and
font sizing.

| Aspect | Assessment |
|--------|------------|
| API shape | Low-level: build XML element trees programmatically. Full control over `<a:bodyPr>`, `<a:rPr sz=...>`, etc. |
| Font size control | Complete — set `sz` on run properties, choose `noAutofit`/`normAutofit`/`spAutoFit` |
| Dependency weight | Significant — ooxmlsdk pulls in the full OOXML schema types (PresentationML, DrawingML, etc.). Much heavier than ppt-rs |
| Maturity | Active development (v0.11.0, July 2025). But PPTX generation examples are scarce; the crate focuses more on reading/round-tripping |
| Migration effort | **High** — would need to rewrite the entire `canvas_html_to_pptx` function, the 16:9 patch (ooxmlsdk may not need it), image embedding, and all the boilerplate that ppt-rs handles (slide master, layout, theme, content types, relationships) |
| Risk | Medium — less battle-tested for PPTX generation; "opens with repair" risk is real if any required element is missing |

**Verdict**: ❌ Too much effort for the benefit. The canvas contract is tiny (positioned
text boxes, rects, images) and ooxmlsdk's value proposition is full-document manipulation,
not simple generation. Only justified if ppt-rs becomes a blocker for multiple features.

---

### Option 4: Hand-Rolled Minimal Writer

**Concept**: Write the OOXML XML directly, bypassing ppt-rs entirely. The canvas contract
is tiny: blank layout, positioned rectangles (solid fill), text boxes (with explicit font
size), and images.

#### OOXML Surface Required

| Component | Files in .pptx zip |
|-----------|-------------------|
| `[Content_Types].xml` | 1 file |
| `_rels/.rels` | 1 file |
| `ppt/presentation.xml` | Slide list + size |
| `ppt/_rels/presentation.xml.rels` | Relationships |
| `ppt/slides/slide{N}.xml` | Per-slide content |
| `ppt/slides/_rels/slide{N}.xml.rels` | Per-slide relationships (images) |
| `ppt/slideLayouts/slideLayout1.xml` | Blank layout |
| `ppt/slideLayouts/_rels/slideLayout1.xml.rels` | Layout → master rel |
| `ppt/slideMasters/slideMaster1.xml` | Master slide |
| `ppt/slideMasters/_rels/slideMaster1.xml.rels` | Master → layout + theme |
| `ppt/theme/theme1.xml` | Minimal theme |
| `docProps/app.xml`, `docProps/core.xml` | Metadata |

**~12-15 files** in the zip, most of which are static boilerplate.

#### Effort Estimate

**~2-3 days** of implementation:
- Write boilerplate XML templates (~200 lines)
- Implement zip assembly (~50 lines, already have the `zip` crate)
- Implement slide XML generation with proper text sizing (~150 lines)
- Handle image embedding with relationship IDs (~50 lines)
- Test across PowerPoint, LibreOffice, Google Slides (~1 day)

#### Risks

| Risk | Severity | Notes |
|------|----------|-------|
| "Opens with repair" in PowerPoint | **High** | Missing or malformed required elements (theme, master, layout) trigger repair dialogs. Getting these right requires extensive trial-and-error |
| LibreOffice/Google Slides compat | Medium | Both are more lenient than PowerPoint but have their own quirks |
| Maintenance burden | Medium | Every OOXML spec change or new PowerPoint version could break things |
| Image relationship management | Low | Well-documented pattern |

**Verdict**: ⚠️ Viable but overkill. Only justified if the team wants full OOXML control
for future features beyond font-size. The post-generation patch (Option 2) solves the
immediate problem at 1/10th the effort.

---

## 3. Recommendation

### Primary: Option 2 — Post-Generation XML Patch

**Why**: The existing codebase already patches the PPTX zip (slide size). Adding a font-size
patch follows the same pattern, requires ~2-3 hours, adds zero dependencies, and preserves
the canvas contract unchanged. The shape-to-element mapping is deterministic by insertion
order.

**Quick-win**: This can be shipped incrementally:
1. First pass: honor `font-size` on text elements only (most common case)
2. Second pass: extend to bullet text
3. Optional: keep `normAutofit` as fallback for elements without explicit `font-size`

### Concrete Implementation Steps

1. **Add `font_size_px: Option<f64>` to `CanvasElement`** — parse from CSS `font-size`.

2. **Write `patch_font_sizes(pptx: &[u8], specs: &[SlideSpec]) -> Result<Vec<u8>, String>`**:
   - Open the zip, iterate `ppt/slides/slide{N}.xml` entries.
   - For each slide, collect the font sizes from `specs[slide_idx].elements` (skipping
     non-text elements and accounting for the background rect offset).
   - Use a simple state machine to find `<p:sp>` boundaries and apply transforms:
     - Replace `<a:normAutofit/>` → `<a:noAutofit/>`
     - Replace `sz="NNNN"` on `<a:rPr>` with the CSS-derived value
   - Write back to a new zip.

3. **Chain in `canvas_html_to_pptx`** after `patch_slide_size_16x9`.

4. **Tests**:
   - Assert `sz="2400"` in slide XML for a `font-size:24px` element
   - Assert `<a:noAutofit/>` replaces `<a:normAutofit/>`
   - Assert elements without font-size retain auto-fit behavior
   - Round-trip: generated PPTX is a valid zip with expected structure

### Future (if needed)

- **ppt-rs upstream PR**: Wire `TextFormat`/`Run` into the Shape builder. This would
  eliminate the need for post-generation patches. File an issue on `yingkitw/ppt-rs`
  to gauge interest.
- **Hand-rolled writer** (Option 4): Only if the team needs multiple OOXML features
  that ppt-rs can't provide (e.g., custom fonts, gradients on text, animations).

---

## 4. Sources

### Local files analyzed
- `~/work/syscity/src/office/slides.rs` — the converter
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ppt-rs-0.2.25/src/generator/shapes_xml.rs` — autofit XML generation (the problem)
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ppt-rs-0.2.25/src/generator/shapes.rs` — Shape struct (no font-size field)
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ppt-rs-0.2.25/src/generator/text/format.rs` — TextFormat (has font_size, unused by Shape)
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ppt-rs-0.2.25/src/generator/text/run.rs` — Run (has .size(), unused by Shape)
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ppt-rs-0.2.25/src/generator/text/paragraph.rs` — Paragraph (unused by Shape)
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ppt-rs-0.2.25/src/generator/slide_content/content.rs` — SlideContent (shapes Vec, no format path)
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ppt-rs-0.2.25/src/generator/slide_xml/content.rs` — shape rendering order (deterministic)

### External references
- [crates.io: ppt-rs](https://crates.io/crates/ppt-rs) — 0.2.25 is latest (2026-08-19)
- [github.com/yingkitw/ppt-rs](https://github.com/yingkitw/ppt-rs) — active repo, no font-size-on-shape API
- [crates.io: ooxmlsdk](https://crates.io/crates/ooxmlsdk) — v0.11.0, pure-Rust OOXML SDK
- [github.com/KaiserY/ooxmlsdk](https://github.com/KaiserY/ooxmlsdk) — ooxmlsdk source
- [python-pptx: Auto-fit text to shape](https://python-pptx.readthedocs.io/en/latest/dev/analysis/txt-autofit-text.html) — definitive analysis of `normAutofit` vs `noAutofit` vs `spAutoFit`
- [c-rex.net: normAutofit](https://c-rex.net/samples/ooxml/e1/part4/OOXML_P4_DOCX_normAutofit_topic_ID0E1MOKB.html) — OOXML spec reference
- [Microsoft: NormalAutoFit Class](https://learn.microsoft.com/en-us/dotnet/api/documentformat.openxml.drawing.normalautofit?view=openxml-3.0.1) — .NET Open XML SDK docs
- [Stack Overflow: font size not being used with OpenXML](https://stackoverflow.com/questions/59413814/my-font-size-set-using-openxml-for-a-powerpoint-paragraph-is-not-being-used) — confirms normAutofit overrides explicit sz
