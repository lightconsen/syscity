//! The same screen, read as data rather than as tap targets.

use super::*;

/// The most characters an extract may return.
///
/// A long chat list can hold far more text than a turn can usefully carry, and
/// a tool result that fills the context crowds out the task itself. Over the
/// ceiling the answer is cut at a line boundary and says so, which is a fact
/// the caller can act on — reading the rest with a scroll — rather than a
/// silent shortening.
pub const EXTRACT_MAX_CHARS: usize = 50_000;

/// One row of a list, as several elements that share a horizontal band.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExtractRow {
    /// 1-based, top to bottom.
    pub index: usize,
    /// Centre y, the band the row occupies.
    pub y: i32,
    /// Every readable label in the row, left to right.
    pub texts: Vec<String>,
    /// Resource ids of the elements that carried them, deduplicated and in the
    /// same order as `texts`.
    pub resource_ids: Vec<String>,
}

/// One field of a form.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FormField {
    /// 1-based, top to bottom.
    pub index: usize,
    /// What the field is called, from the label beside it — or, when the screen
    /// has none, from the field's own hint / resource id.
    pub label: String,
    /// Text can be typed into it.
    pub editable: bool,
    /// What kind of value it expects, inferred from the label.
    pub kind: &'static str,
    /// What it holds now. Omitted for a password field: its contents are never
    /// part of a read, whatever the model means to do with them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// The centre, for a tap that focuses it.
    pub center: (i32, i32),
    pub resource_id: Option<String>,
}

/// How rows are grouped: two elements share a row when their vertical extents
/// overlap by at least half of the shorter one.
///
/// The centre alone is too strict on Android, where a tall label and a short
/// icon in the same row can have centres a hundred pixels apart; the extents
/// say what a person sees, which is that the two sit on one line.
fn same_row(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    let overlap = a.3.min(b.3) - a.1.max(b.1);
    let shorter = (a.3 - a.1).min(b.3 - b.1).max(1);
    overlap * 2 >= shorter
}

/// Group the readable elements of a tree into rows, top to bottom.
pub fn extract_rows(elements: &[UiElement]) -> Vec<ExtractRow> {
    let mut named: Vec<&UiElement> = elements
        .iter()
        .filter(|element| !element.state.password)
        .filter(|element| element_label(element).is_some())
        .collect();
    named.sort_by_key(|element| (element.bounds.1, element.bounds.0));

    let mut rows: Vec<(i32, Vec<&UiElement>)> = Vec::new();
    for element in named {
        match rows.iter_mut().find(|(_, members)| {
            members
                .last()
                .is_some_and(|last| same_row(last.bounds, element.bounds))
        }) {
            Some((_, members)) => members.push(element),
            None => rows.push((element.bounds.1, vec![element])),
        }
    }

    rows.iter()
        .enumerate()
        .map(|(index, (_, members))| {
            let texts: Vec<String> = members
                .iter()
                .filter_map(|element| element_label(element).map(str::to_string))
                .collect();
            let mut resource_ids: Vec<String> = Vec::new();
            for id in members
                .iter()
                .filter_map(|element| element.resource_id.clone())
            {
                if !resource_ids.contains(&id) {
                    resource_ids.push(id);
                }
            }
            let y = members
                .iter()
                .map(|element| element.center.1)
                .min()
                .unwrap_or_default();
            ExtractRow {
                index: index + 1,
                y,
                texts,
                resource_ids,
            }
        })
        .collect()
}

/// What kind of value a field asks for, from the words naming it.
///
/// Deliberately small: a hint that guesses wrong is worse than "text", because
/// the caller then writes the wrong shape into the field.
pub fn infer_field_kind(label: &str) -> &'static str {
    let lowered = label.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lowered.contains(needle));
    if has(&["密码", "password", "passcode"]) {
        "password"
    } else if has(&["手机", "电话", "号码", "phone", "mobile"]) {
        "phone"
    } else if has(&["邮箱", "邮件", "email", "e-mail"]) {
        "email"
    } else if has(&["验证码", "code", "captcha"]) {
        "code"
    } else if has(&["搜索", "查找", "search", "find"]) {
        "search"
    } else if has(&[
        "数量", "金额", "价格", "number", "amount", "quantity", "price",
    ]) {
        "number"
    } else {
        "text"
    }
}

/// The label that belongs to a field, found on its own row or just above it.
fn field_label(field: &UiElement, elements: &[UiElement]) -> Option<String> {
    let mut best: Option<(&UiElement, i32)> = None;
    for other in elements {
        if std::ptr::eq(other, field) || element_label(other).is_none() {
            continue;
        }
        let distance = if same_row(field.bounds, other.bounds) {
            // On the same row, the label is the one to its left.
            if other.bounds.2 <= field.bounds.0 {
                field.bounds.0 - other.bounds.2
            } else if other.bounds.0 >= field.bounds.2 {
                // A control to the right is not this field's label.
                continue;
            } else {
                0
            }
        } else if other.bounds.3 <= field.bounds.1 {
            // Above it: how far above, but only a line or two.
            let gap = field.bounds.1 - other.bounds.3;
            if gap > 120 {
                continue;
            }
            gap + 1_000
        } else {
            continue;
        };
        if best.is_none_or(|(_, current)| distance < current) {
            best = Some((other, distance));
        }
    }
    best.and_then(|(element, _)| element_label(element))
        .map(str::to_string)
}

/// The fields of a form, top to bottom.
pub fn extract_fields(elements: &[UiElement]) -> Vec<FormField> {
    let mut fields: Vec<&UiElement> = elements
        .iter()
        .filter(|element| element.state.editable || element.state.password)
        .collect();
    fields.sort_by_key(|element| (element.bounds.1, element.bounds.0));
    fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let label = field_label(field, elements)
                .or_else(|| {
                    field
                        .resource_id
                        .as_deref()
                        .and_then(|id| id.rsplit('/').next())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| format!("field {}", index + 1));
            FormField {
                index: index + 1,
                // The node's own flag is authoritative: a password box is a
                // password box whatever its label says (and its label is often
                // "请输入" on every field).
                kind: if field.state.password {
                    "password"
                } else {
                    infer_field_kind(&label)
                },
                label,
                editable: field.state.editable,
                current: if field.state.password {
                    None
                } else {
                    field.text.clone().filter(|text| !text.is_empty())
                },
                center: field.center,
                resource_id: field.resource_id.clone(),
            }
        })
        .collect()
}

/// The tree names a list rather than a form.
fn looks_like_a_list(elements: &[UiElement]) -> bool {
    elements.iter().any(|element| {
        element.state.scrollable
            || element.class.contains("RecyclerView")
            || element.class.contains("ListView")
            || element.class.contains("LazyColumn")
    })
}

/// Read a screen as text, as rows, or as form fields.
#[derive(Debug)]
pub struct AdbExtractTool {
    device: Option<String>,
}

impl Default for AdbExtractTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbExtractTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbExtractTool {
    fn name(&self) -> &str {
        "android_extract"
    }

    fn description(&self) -> &str {
        "Read the current screen as structured data instead of a list of elements. \
         `mode: text` returns the readable text in reading order; `list` returns one entry per \
         row of a list, with all the text on that row together; `form` returns one entry per \
         input field, with the label beside it and what kind of value it expects. Use this when \
         the answer is 'what does this screen say' rather than 'where do I tap'."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Extract the screen as text, rows or form fields",
            serde_json::json!({
                "mode": {
                    "type": "string",
                    "enum": ["text", "list", "form"],
                    "description": "text: readable lines in order. list: one entry per row, requiring a scrolling list. form: one entry per input field."
                },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let mode = args.get("mode").and_then(Value::as_str).unwrap_or("text");
        if !matches!(mode, "text" | "list" | "form") {
            return Ok(ToolExecutionResult::error(format!(
                "`mode` must be text, list or form, not {mode:?}"
            )));
        }
        let dump = match dump_ui(&device, target).await {
            Ok(dump) => dump,
            Err(failure) => {
                return Ok(ToolExecutionResult::error(failure.message)
                    .with_data(serde_json::json!({ "code": failure.code })))
            }
        };
        if dump.elements.is_empty() {
            return Ok(ToolExecutionResult::success(
                "This screen has no elements to extract — read the screenshot with android_observe \
                 (it falls back to OCR) instead.",
            )
            .with_data(serde_json::json!({ "mode": mode, "count": 0 })));
        }

        let serial = resolved_serial(&device).await;
        match mode {
            "list" => {
                if !looks_like_a_list(&dump.elements) {
                    return Ok(ToolExecutionResult::success(
                        "This screen is not a scrolling list — no scrollable node, RecyclerView or \
                         ListView is present. Use mode 'text' for reading order, 'form' for input \
                         fields, or android_query to find a specific element.",
                    )
                    .with_data(serde_json::json!({
                        "mode": mode,
                        "items": [],
                        "device": serial,
                    })));
                }
                let rows = extract_rows(&dump.elements);
                let (items, truncated) = cap_rows(&rows, EXTRACT_MAX_CHARS);
                let text = list_text(&items, truncated);
                Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
                    "mode": mode,
                    "count": items.len(),
                    "items": items,
                    "truncated": truncated,
                    "source": dump.source,
                    "device": serial,
                })))
            }
            "form" => {
                let fields = extract_fields(&dump.elements);
                if fields.is_empty() {
                    return Ok(ToolExecutionResult::success(
                        "This screen has no input fields — nothing to fill in here.",
                    )
                    .with_data(serde_json::json!({
                        "mode": mode,
                        "fields": [],
                        "device": serial,
                    })));
                }
                let mut text = format!("{} field(s):", fields.len());
                for field in &fields {
                    text.push_str(&format!("\n{}. {} [{}]", field.index, field.label, field.kind));
                    if let Some(current) = &field.current {
                        text.push_str(&format!(" = {current:?}"));
                    } else if field.kind == "password" {
                        text.push_str(" (password — contents not read)");
                    } else {
                        text.push_str(" (empty)");
                    }
                    text.push_str(&format!(" center=({},{})", field.center.0, field.center.1));
                }
                let (fields, truncated) = cap_fields(fields, EXTRACT_MAX_CHARS);
                Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
                    "mode": mode,
                    "count": fields.len(),
                    "fields": fields,
                    "truncated": truncated,
                    "source": dump.source,
                    "device": serial,
                })))
            }
            _ => {
                let lines: Vec<String> = {
                    let rows = extract_rows(&dump.elements);
                    let mut seen = std::collections::HashSet::new();
                    rows.into_iter()
                        .flat_map(|row| row.texts)
                        .filter(|line| seen.insert(line.clone()))
                        .collect()
                };
                let (lines, truncated) = cap_lines(lines, EXTRACT_MAX_CHARS);
                let mut text = format!("{} line(s):", lines.len());
                for line in &lines {
                    text.push('\n');
                    text.push_str(line);
                }
                if truncated {
                    text.push_str(
                        "\n\n[truncated — the screen holds more text than one extract carries; \
                         scroll and extract again for the rest]",
                    );
                }
                Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
                    "mode": mode,
                    "count": lines.len(),
                    "lines": lines,
                    "truncated": truncated,
                    "source": dump.source,
                    "device": serial,
                })))
            }
        }
    }
}

/// How many characters a value costs in the result, for the budget.
fn cost(text: &str) -> usize {
    text.chars().count() + 1
}

/// Cut a list of lines at the character budget.
fn cap_lines(lines: Vec<String>, budget: usize) -> (Vec<String>, bool) {
    let mut kept = Vec::new();
    let mut used = 0;
    for line in lines {
        let this = cost(&line);
        if used + this > budget {
            return (kept, true);
        }
        used += this;
        kept.push(line);
    }
    (kept, false)
}

/// Cut rows at the character budget.
fn cap_rows(rows: &[ExtractRow], budget: usize) -> (Vec<ExtractRow>, bool) {
    let mut kept = Vec::new();
    let mut used = 0;
    for row in rows {
        let this: usize = row.texts.iter().map(|text| cost(text)).sum();
        if used + this > budget {
            return (kept, true);
        }
        used += this;
        kept.push(row.clone());
    }
    (kept, false)
}

/// Cut form fields at the character budget.
fn cap_fields(fields: Vec<FormField>, budget: usize) -> (Vec<FormField>, bool) {
    let mut kept = Vec::new();
    let mut used = 0;
    for field in fields {
        let this = cost(&field.label) + field.current.as_deref().map(cost).unwrap_or(0);
        if used + this > budget {
            return (kept, true);
        }
        used += this;
        kept.push(field);
    }
    (kept, false)
}

/// The result text for a set of rows.
fn list_text(rows: &[ExtractRow], truncated: bool) -> String {
    if rows.is_empty() {
        return "The list is on screen but carries no readable rows.".to_string();
    }
    let mut text = format!("{} row(s):", rows.len());
    for row in rows {
        text.push_str(&format!("\n{}. {}", row.index, row.texts.join(" · ")));
    }
    if truncated {
        text.push_str(
            "\n\n[truncated — the list holds more rows than one extract carries; scroll and \
             extract again for the rest]",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;

    #[test]
    fn elements_that_share_a_line_become_one_row() {
        // A chat row is a name and a message side by side, drawn by two nodes.
        // The centres differ; the extents say they are one line.
        let mut name = element(1, "Alice", "android.widget.TextView", false, (20, 300, 200, 360));
        let mut body =
            element(2, "see you at six", "android.widget.TextView", false, (220, 300, 900, 360));
        name.state.depth = 4;
        body.state.depth = 5;
        let rows = extract_rows(&[name, body]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].texts, vec!["Alice", "see you at six"]);
    }
    #[test]
    fn rows_separated_vertically_stay_separate_and_are_numbered_top_down() {
        let first = element(1, "one", "TextView", false, (0, 100, 400, 160));
        let second = element(2, "two", "TextView", false, (0, 500, 400, 560));
        let rows = extract_rows(&[second, first]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].texts, vec!["one"]);
        assert_eq!(rows[1].index, 2);
    }
    #[test]
    fn a_password_value_is_never_part_of_a_row() {
        let mut secret = element(1, "hunter2", "android.widget.EditText", false, (0, 0, 400, 60));
        secret.state.editable = true;
        secret.state.password = true;
        assert!(extract_rows(&[secret]).is_empty());
    }
    #[test]
    fn a_field_takes_the_label_to_its_left() {
        let mut field = element(2, "", "android.widget.EditText", false, (300, 100, 1000, 160));
        field.state.editable = true;
        let label = element(1, "手机号", "android.widget.TextView", false, (20, 100, 280, 160));
        let fields = extract_fields(&[label, field]);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].label, "手机号");
        assert_eq!(fields[0].kind, "phone");
    }
    #[test]
    fn a_field_kind_is_inferred_from_its_label() {
        assert_eq!(infer_field_kind("邮箱地址"), "email");
        assert_eq!(infer_field_kind("Password"), "password");
        assert_eq!(infer_field_kind("验证码"), "code");
        assert_eq!(infer_field_kind("Search"), "search");
        assert_eq!(infer_field_kind("数量"), "number");
        assert_eq!(infer_field_kind("备注"), "text");
    }
    #[test]
    fn a_password_field_reports_no_current_value() {
        let mut field = element(1, "secret", "android.widget.EditText", false, (0, 0, 400, 60));
        field.state.editable = true;
        field.state.password = true;
        let fields = extract_fields(&[field]);
        assert_eq!(fields[0].kind, "password");
        assert!(fields[0].current.is_none());
    }
    #[test]
    fn the_extract_budget_cuts_at_a_line_and_says_so() {
        let lines = vec!["a".repeat(30), "b".repeat(30), "c".repeat(30)];
        // Each line costs its length plus the newline separator.
        let (kept, truncated) = cap_lines(lines, 62);
        assert_eq!(kept.len(), 2);
        assert!(truncated);
        // Exactly the budget is not a truncation.
        let (kept, truncated) = cap_lines(vec!["x".repeat(10)], 11);
        assert_eq!(kept.len(), 1);
        assert!(!truncated);
    }
    #[test]
    fn a_list_screen_is_recognised_by_its_container() {
        let list = scrollable(2, (0, 100, 1080, 2300));
        assert!(looks_like_a_list(&[list]));
        let plain = element(1, "设置", "TextView", true, (0, 0, 100, 40));
        assert!(!looks_like_a_list(&[plain]));
    }
}
