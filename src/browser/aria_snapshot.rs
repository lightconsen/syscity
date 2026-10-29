//! ARIA Snapshot — LLM-friendly accessible tree extraction
//!
//! Generates a compact, ref-marked text representation of a web page's
//! interactive elements suitable for LLM consumption.
//!
//! Output format:
//! ```text
//! [1] heading "Example Domain"
//! [2] paragraph "This domain is for use in illustrative examples..."
//! [3] link "More information..."
//! [4] button "Accept Cookies"
//! ```

use serde::{Deserialize, Serialize};

/// A single line in the ARIA snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AriaNodeLine {
    /// Reference ID for interaction (e.g. 1, 2, 3...)
    pub ref_id: usize,
    /// ARIA role (button, link, textbox, etc.)
    pub role: String,
    /// Accessible name / visible text
    pub name: String,
    /// Current value (for inputs, checkboxes, etc.)
    pub value: Option<String>,
    /// Indentation level for nesting
    pub indent: usize,
}

/// Complete ARIA snapshot of a page
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AriaSnapshot {
    /// Page URL
    pub url: String,
    /// Page title
    pub title: String,
    /// Snapshot lines
    pub lines: Vec<AriaNodeLine>,
    /// Whether the snapshot was truncated
    pub truncated: bool,
    /// Total character count
    pub total_chars: usize,
}

impl AriaSnapshot {
    /// Render the snapshot as a text string for the LLM
    pub fn to_text(&self) -> String {
        let mut lines = vec![
            format!("URL: {}", self.url),
            format!("Title: {}", self.title),
            String::new(),
        ];

        for line in &self.lines {
            let indent = "  ".repeat(line.indent);
            let value_part = line
                .value
                .as_ref()
                .map(|v| format!(" value=\"{}\"", v))
                .unwrap_or_default();
            lines.push(format!(
                "{}[{}] {} \"{}\"{}",
                indent, line.ref_id, line.role, line.name, value_part
            ));
        }

        if self.truncated {
            lines.push(String::new());
            lines.push("... (snapshot truncated)".to_string());
        }

        lines.join("\n")
    }

    /// Get the number of interactive elements
    pub fn interactive_count(&self) -> usize {
        self.lines.len()
    }
}

/// Extract an ARIA snapshot from a browser page
#[cfg(feature = "browser")]
pub async fn aria_snapshot(
    page: &chromiumoxide::Page,
    max_chars: usize,
) -> crate::Result<AriaSnapshot> {
    // First, get page metadata
    let url = page.url().await.ok().flatten().unwrap_or_default();
    let title = page.get_title().await.ok().flatten().unwrap_or_default();

    // JavaScript to extract accessible/interactive elements
    // We assign data-syscity-ref attributes for later interaction
    let script = r#"
() => {
    // Interactive roles and selectors
    const interactiveSelectors = [
        'a[href]', 'button', 'input', 'select', 'textarea',
        '[role="button"]', '[role="link"]', '[role="textbox"]',
        '[role="checkbox"]', '[role="radio"]', '[role="tab"]',
        '[role="menuitem"]', '[role="option"]', '[role="searchbox"]',
        'label', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6',
        '[role="heading"]'
    ];

    const seen = new Set();
    const results = [];
    let refId = 0;

    function getAccessibleName(el) {
        // aria-label
        const ariaLabel = el.getAttribute('aria-label');
        if (ariaLabel) return ariaLabel.trim();

        // aria-labelledby
        const labelledBy = el.getAttribute('aria-labelledby');
        if (labelledBy) {
            const labels = labelledBy.split(/\s+/)
                .map(id => document.getElementById(id))
                .filter(Boolean)
                .map(el => el.textContent.trim())
                .join(' ');
            if (labels) return labels;
        }

        // Associated label element
        if (el.id) {
            const label = document.querySelector(`label[for="${el.id}"]`);
            if (label) return label.textContent.trim();
        }

        // Parent label
        const parentLabel = el.closest('label');
        if (parentLabel) {
            const text = parentLabel.textContent.trim();
            if (text) return text;
        }

        // Placeholder
        const placeholder = el.getAttribute('placeholder');
        if (placeholder) return placeholder.trim();

        // Text content
        const text = el.textContent.trim();
        if (text) return text;

        // Value for inputs
        if (el.value && typeof el.value === 'string') return el.value.trim();

        // Alt for images
        const alt = el.getAttribute('alt');
        if (alt) return alt.trim();

        // Title
        const title = el.getAttribute('title');
        if (title) return title.trim();

        return '';
    }

    function getRole(el) {
        const explicit = el.getAttribute('role');
        if (explicit) return explicit;

        const tag = el.tagName.toLowerCase();
        const type = (el.getAttribute('type') || 'text').toLowerCase();

        // An input's `type` attribute is not its role: `type="text"` is a
        // textbox and `type="submit"` is a button. Reporting the raw attribute
        // left the snapshot saying "text" and "submit", which nothing that
        // matches by role — the page rung included — could act on.
        const inputRoles = {
            'text': 'textbox',
            'email': 'textbox',
            'tel': 'textbox',
            'url': 'textbox',
            'password': 'textbox',
            'search': 'searchbox',
            'number': 'spinbutton',
            'range': 'slider',
            'checkbox': 'checkbox',
            'radio': 'radio',
            'button': 'button',
            'submit': 'button',
            'reset': 'button',
            'image': 'button',
            'file': 'button',
        };

        const roleMap = {
            'a': 'link',
            'button': 'button',
            'input': inputRoles[type] || 'textbox',
            'select': 'combobox',
            'textarea': 'textbox',
            'h1': 'heading',
            'h2': 'heading',
            'h3': 'heading',
            'h4': 'heading',
            'h5': 'heading',
            'h6': 'heading',
            'label': 'label',
        };

        return roleMap[tag] || tag;
    }

    function getValue(el) {
        if (el.tagName === 'INPUT') {
            const type = el.getAttribute('type');
            if (type === 'checkbox' || type === 'radio') {
                return el.checked ? 'checked' : 'unchecked';
            }
            return el.value || null;
        }
        if (el.tagName === 'SELECT') {
            const selected = el.querySelector('option:checked');
            return selected ? selected.textContent.trim() : null;
        }
        if (el.tagName === 'TEXTAREA') {
            return el.value || null;
        }
        return null;
    }

    function isInteractive(el) {
        const role = getRole(el);
        const interactive = ['button', 'link', 'textbox', 'checkbox', 'radio',
            'combobox', 'searchbox', 'menuitem', 'tab', 'option',
            'spinbutton', 'slider'];
        return interactive.includes(role);
    }

    function getDepth(el) {
        let depth = 0;
        let parent = el.parentElement;
        while (parent && parent.tagName !== 'BODY') {
            depth++;
            parent = parent.parentElement;
        }
        return depth;
    }

    // Collect all candidate elements
    const allElements = document.querySelectorAll(interactiveSelectors.join(', '));

    for (const el of allElements) {
        // Skip invisible elements
        const style = window.getComputedStyle(el);
        if (style.display === 'none' || style.visibility === 'hidden') continue;

        const rect = el.getBoundingClientRect();
        if (rect.width === 0 && rect.height === 0) continue;

        const name = getAccessibleName(el);
        if (!name && !isInteractive(el)) continue;

        const role = getRole(el);

        // Assign ref ID for interactive elements
        let id = null;
        if (isInteractive(el)) {
            refId++;
            id = refId;
            el.setAttribute('data-syscity-ref', String(refId));
        }

        results.push({
            refId: id,
            role: role,
            name: name || '(unnamed)',
            value: getValue(el),
            tag: el.tagName.toLowerCase(),
            indent: getDepth(el)
        });
    }

    return {
        refCount: refId,
        elements: results
    };
}
"#;

    let result =
        page.evaluate(script)
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: "Failed to extract ARIA snapshot".to_string(),
                cause: Some(Box::new(e)),
            })?;

    let data = result.value().cloned().unwrap_or_default();

    let elements = data
        .get("elements")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut lines = Vec::new();
    let mut char_count = url.len() + title.len() + 20;
    let mut truncated = false;

    for el in elements {
        let ref_id = el.get("refId").and_then(|v| v.as_u64()).map(|v| v as usize);
        let role = el
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let name = el
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let value = el
            .get("value")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let indent = el
            .get("indent")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(0);

        let line = AriaNodeLine {
            ref_id: ref_id.unwrap_or(0),
            role,
            name,
            value,
            indent,
        };

        let line_text = format!("[{}] {} \"{}\"", line.ref_id, line.role, line.name);
        let line_len = line_text.len() + 1;

        if char_count + line_len > max_chars && !truncated {
            truncated = true;
            break;
        }

        char_count += line_len;
        lines.push(line);
    }

    Ok(AriaSnapshot {
        url,
        title,
        lines,
        truncated,
        total_chars: char_count,
    })
}

/// Act on an element by ref_id within a page
#[cfg(feature = "browser")]
pub async fn act_by_ref(
    page: &chromiumoxide::Page,
    ref_id: usize,
    action: ActKind,
) -> crate::Result<String> {
    if ref_id == 0 {
        return Err(crate::error::SyscityError::Validation(
            "ref_id must be a positive integer".to_string(),
        ));
    }
    let selector = format!("[data-syscity-ref=\"{}\"]", ref_id);

    match action {
        ActKind::Click => {
            let script = format!(
                r#"() => {{
                    const el = document.querySelector('{}');
                    if (!el) return {{ error: "Element not found" }};
                    el.click();
                    return {{ success: true, action: "click" }};
                }}"#,
                selector
            );
            let result = page.evaluate(script.as_str()).await.map_err(|e| {
                crate::error::SyscityError::ExternalService {
                    source: "Click failed".to_string(),
                    cause: Some(Box::new(e)),
                }
            })?;
            let value = result.value().cloned().unwrap_or_default();
            if value.get("error").is_some() {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Element with ref {} not found",
                    ref_id
                )));
            }
            Ok(format!("Clicked element [ref={}]", ref_id))
        }

        ActKind::Type { text } => {
            let escaped = escape_js_string(&text);
            let script = format!(
                r#"() => {{
                    const el = document.querySelector('{}');
                    if (!el) return {{ error: "Element not found" }};
                    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
                        el.focus();
                        el.value = '{}';
                        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                        return {{ success: true, action: "type" }};
                    }}
                    return {{ error: "Element is not an input" }};
                }}"#,
                selector, escaped,
            );
            let result = page.evaluate(script.as_str()).await.map_err(|e| {
                crate::error::SyscityError::ExternalService {
                    source: "Type failed".to_string(),
                    cause: Some(Box::new(e)),
                }
            })?;
            let value = result.value().cloned().unwrap_or_default();
            if value.get("error").is_some() {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Element with ref {} not found or not an input",
                    ref_id
                )));
            }
            Ok(format!("Typed text into element [ref={}]", ref_id))
        }

        ActKind::Hover => {
            let script = format!(
                r#"() => {{
                    const el = document.querySelector('{}');
                    if (!el) return {{ error: "Element not found" }};
                    const ev = new MouseEvent('mouseover', {{ bubbles: true }});
                    el.dispatchEvent(ev);
                    return {{ success: true, action: "hover" }};
                }}"#,
                selector
            );
            let result = page.evaluate(script.as_str()).await.map_err(|e| {
                crate::error::SyscityError::ExternalService {
                    source: "Hover failed".to_string(),
                    cause: Some(Box::new(e)),
                }
            })?;
            let value = result.value().cloned().unwrap_or_default();
            if value.get("error").is_some() {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Element with ref {} not found",
                    ref_id
                )));
            }
            Ok(format!("Hovered over element [ref={}]", ref_id))
        }

        ActKind::Fill { text } => {
            let escaped = escape_js_string(&text);
            let script = format!(
                r#"() => {{
                    const el = document.querySelector('{}');
                    if (!el) return {{ error: "Element not found" }};
                    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
                        el.focus();
                        el.select();
                        el.value = '{}';
                        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                        return {{ success: true, action: "fill" }};
                    }}
                    return {{ error: "Element is not an input" }};
                }}"#,
                selector, escaped,
            );
            let result = page.evaluate(script.as_str()).await.map_err(|e| {
                crate::error::SyscityError::ExternalService {
                    source: "Fill failed".to_string(),
                    cause: Some(Box::new(e)),
                }
            })?;
            let value = result.value().cloned().unwrap_or_default();
            if value.get("error").is_some() {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Element with ref {} not found or not an input",
                    ref_id
                )));
            }
            Ok(format!("Filled element [ref={}]", ref_id))
        }
    }
}

/// Escape a string for safe interpolation into a single-quoted JS string.
fn escape_js_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Action kinds for ref-based interaction
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActKind {
    /// Click the element
    Click,
    /// Type text into the element (appends)
    Type { text: String },
    /// Hover over the element
    Hover,
    /// Fill the element with text (replaces)
    Fill { text: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aria_snapshot_to_text() {
        let snapshot = AriaSnapshot {
            url: "https://example.com".to_string(),
            title: "Example".to_string(),
            lines: vec![
                AriaNodeLine {
                    ref_id: 1,
                    role: "heading".to_string(),
                    name: "Example Domain".to_string(),
                    value: None,
                    indent: 0,
                },
                AriaNodeLine {
                    ref_id: 2,
                    role: "link".to_string(),
                    name: "More information...".to_string(),
                    value: None,
                    indent: 0,
                },
                AriaNodeLine {
                    ref_id: 3,
                    role: "button".to_string(),
                    name: "Accept".to_string(),
                    value: Some("checked".to_string()),
                    indent: 1,
                },
            ],
            truncated: false,
            total_chars: 100,
        };

        let text = snapshot.to_text();
        assert!(text.contains("URL: https://example.com"));
        assert!(text.contains("Title: Example"));
        assert!(text.contains("[1] heading \"Example Domain\""));
        assert!(text.contains("[2] link \"More information...\""));
        assert!(text.contains("  [3] button \"Accept\" value=\"checked\""));
        assert!(!text.contains("truncated"));
    }

    #[test]
    fn test_aria_snapshot_truncated() {
        let snapshot = AriaSnapshot {
            url: "https://example.com".to_string(),
            title: "Example".to_string(),
            lines: vec![],
            truncated: true,
            total_chars: 0,
        };

        let text = snapshot.to_text();
        assert!(text.contains("... (snapshot truncated)"));
    }

    #[test]
    fn test_aria_node_line_count() {
        let snapshot = AriaSnapshot {
            url: "https://example.com".to_string(),
            title: "Example".to_string(),
            lines: vec![
                AriaNodeLine {
                    ref_id: 1,
                    role: "button".to_string(),
                    name: "Click".to_string(),
                    value: None,
                    indent: 0,
                },
                AriaNodeLine {
                    ref_id: 2,
                    role: "link".to_string(),
                    name: "Go".to_string(),
                    value: None,
                    indent: 0,
                },
            ],
            truncated: false,
            total_chars: 50,
        };

        assert_eq!(snapshot.interactive_count(), 2);
    }

    #[test]
    fn test_act_kind_serde() {
        let click = ActKind::Click;
        let json = serde_json::to_string(&click).unwrap();
        assert!(json.contains("click"));

        let type_action = ActKind::Type { text: "hello".to_string() };
        let json = serde_json::to_string(&type_action).unwrap();
        assert!(json.contains("type"));
        assert!(json.contains("hello"));

        let fill = ActKind::Fill { text: "world".to_string() };
        let json = serde_json::to_string(&fill).unwrap();
        assert!(json.contains("fill"));

        let hover = ActKind::Hover;
        let json = serde_json::to_string(&hover).unwrap();
        assert!(json.contains("hover"));
    }
}
