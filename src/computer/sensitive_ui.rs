//! Sensitive UI detection — detect password fields, payment pages, and
//! destructive confirmation dialogs via accessibility tree analysis.
//!
//! When sensitive UI is detected the agent should pause and request human
//! approval before proceeding.
//!
//! # Usage
//!
//! ```rust
//! use syscity::computer::sensitive_ui::SensitiveUiDetector;
//! use syscity::computer::types::{Rect, UiElement};
//!
//! let detector = SensitiveUiDetector::new();
//! let ui_tree = UiElement {
//!     id: "".to_string(),
//!     role: "window".to_string(),
//!     label: None,
//!     value: None,
//!     bounds: Rect {
//!         x: 0,
//!         y: 0,
//!         width: 100,
//!         height: 100,
//!     },
//!     enabled: true,
//!     focused: false,
//!     children: vec![],
//! };
//! let findings = detector.scan_tree(&ui_tree);
//! if !findings.is_empty() {
//!     println!("⚠️ Sensitive UI detected: {:?}", findings);
//! }
//! ```

use serde::{Deserialize, Serialize};

use crate::computer::types::UiElement;

/// Category of sensitive UI detected.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensitiveUiType {
    /// Password / passphrase input field.
    PasswordField,
    /// Credit card number / CVV / expiry input.
    PaymentField,
    /// Payment or checkout page.
    PaymentPage,
    /// Destructive action confirmation (delete, format, remove).
    DestructiveConfirmation,
    /// Dialog requesting elevated privileges.
    PrivilegeEscalation,
}

impl SensitiveUiType {
    /// Stable, model-facing name (matches the serde representation).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PasswordField => "password_field",
            Self::PaymentField => "payment_field",
            Self::PaymentPage => "payment_page",
            Self::DestructiveConfirmation => "destructive_confirmation",
            Self::PrivilegeEscalation => "privilege_escalation",
        }
    }
}

/// A single detection result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensitiveUiFinding {
    pub category: SensitiveUiType,
    pub description: String,
    /// Element id if available.
    pub element_id: String,
    /// Human-readable path to the element in the tree.
    pub path: String,
}

/// Detects sensitive UI elements in an accessibility tree.
#[derive(Debug, Default, Clone)]
pub struct SensitiveUiDetector;

impl SensitiveUiDetector {
    pub fn new() -> Self {
        Self
    }

    /// Scan a UI element tree and return all sensitive findings.
    pub fn scan_tree(&self, root: &UiElement) -> Vec<SensitiveUiFinding> {
        let mut findings = Vec::new();
        self.walk(root, &mut findings, &root.role);
        findings
    }

    /// Returns true if any sensitive UI was detected.
    pub fn has_sensitive_ui(&self, root: &UiElement) -> bool {
        !self.scan_tree(root).is_empty()
    }

    fn walk(&self, node: &UiElement, findings: &mut Vec<SensitiveUiFinding>, parent_role: &str) {
        let label_lower = node.label.as_deref().unwrap_or("").to_lowercase();
        let role_lower = node.role.to_lowercase();
        let value_lower = node.value.as_deref().unwrap_or("").to_lowercase();

        // ---- Password field detection ----
        if self.is_password_field(&role_lower, &label_lower) {
            findings.push(SensitiveUiFinding {
                category: SensitiveUiType::PasswordField,
                description: format!(
                    "Password input: {}",
                    node.label.as_deref().unwrap_or("(unlabeled)")
                ),
                element_id: node.id.clone(),
                path: format!("{}/{}", parent_role, role_lower),
            });
        }

        // ---- Payment field detection ----
        if self.is_payment_field(&role_lower, &label_lower) {
            findings.push(SensitiveUiFinding {
                category: SensitiveUiType::PaymentField,
                description: format!(
                    "Payment input: {}",
                    node.label.as_deref().unwrap_or("(unlabeled)")
                ),
                element_id: node.id.clone(),
                path: format!("{}/{}", parent_role, role_lower),
            });
        }

        // ---- Payment page detection ----
        if self.is_payment_page(&role_lower, &label_lower, &value_lower) {
            findings.push(SensitiveUiFinding {
                category: SensitiveUiType::PaymentPage,
                description: format!(
                    "Payment page detected: {}",
                    node.label.as_deref().unwrap_or("(unlabeled)")
                ),
                element_id: node.id.clone(),
                path: role_lower.clone(),
            });
        }

        // ---- Destructive confirmation detection ----
        if self.is_destructive_confirmation(&role_lower, &label_lower, &value_lower) {
            findings.push(SensitiveUiFinding {
                category: SensitiveUiType::DestructiveConfirmation,
                description: format!(
                    "Destructive action confirmation: {}",
                    node.label.as_deref().unwrap_or("(unlabeled)")
                ),
                element_id: node.id.clone(),
                path: role_lower.clone(),
            });
        }

        // ---- Privilege escalation dialog ----
        if self.is_privilege_escalation(&role_lower, &label_lower, &value_lower) {
            findings.push(SensitiveUiFinding {
                category: SensitiveUiType::PrivilegeEscalation,
                description: format!(
                    "Privilege escalation dialog: {}",
                    node.label.as_deref().unwrap_or("(unlabeled)")
                ),
                element_id: node.id.clone(),
                path: role_lower.clone(),
            });
        }

        for child in &node.children {
            self.walk(child, findings, &role_lower);
        }
    }

    // ------------------------------------------------------------------
    // Heuristic rules
    // ------------------------------------------------------------------

    fn is_password_field(&self, role: &str, label: &str) -> bool {
        let role_signals = [
            "password",
            "secure_text",
            "password_field",
            "passwort",
            "pin",
        ];
        let label_signals = [
            "password",
            "passwort",
            "pin",
            "密钥",
            "密码",
            "口令",
            "passphrase",
            "secret key",
            "api key",
        ];

        role_signals.iter().any(|s| role.contains(s))
            || label_signals.iter().any(|s| label.contains(s))
    }

    fn is_payment_field(&self, _role: &str, label: &str) -> bool {
        let label_signals = [
            "card number",
            "credit card",
            "cvv",
            "cvc",
            "expiry",
            "expiration",
            "billing",
            "iban",
            "account number",
            "银行卡号",
            "信用卡",
            "验证码",
        ];

        label_signals.iter().any(|s| label.contains(s))
    }

    fn is_payment_page(&self, role: &str, label: &str, value: &str) -> bool {
        let combined = format!("{} {} {}", role, label, value);
        let signals = [
            "checkout",
            "payment",
            "billing",
            "place order",
            "confirm purchase",
            "结账",
            "支付",
            "付款",
        ];
        signals.iter().any(|s| combined.contains(s))
    }

    fn is_destructive_confirmation(&self, role: &str, label: &str, value: &str) -> bool {
        let combined = format!("{} {} {}", role, label, value);
        let signals = [
            "delete",
            "remove",
            "format",
            "erase",
            "destroy",
            "permanently",
            "确认删除",
            "确认移除",
            "格式化",
            "are you sure",
            "cannot be undone",
            "此操作不可恢复",
        ];
        let is_dialog = role.contains("dialog")
            || role.contains("alert")
            || role.contains("window") && combined.contains("confirm");

        is_dialog && signals.iter().any(|s| combined.contains(s))
    }

    fn is_privilege_escalation(&self, role: &str, label: &str, value: &str) -> bool {
        let combined = format!("{} {} {}", role, label, value);
        let signals = [
            "sudo",
            "administrator",
            "uac",
            "user account control",
            "elevation",
            "authenticate",
            "enter password to",
            "requires authentication",
            "需要管理员权限",
            "请输入密码",
        ];
        let is_dialog =
            role.contains("dialog") || role.contains("alert") || role.contains("prompt");

        is_dialog && signals.iter().any(|s| combined.contains(s))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(role: &str, label: Option<&str>) -> UiElement {
        UiElement {
            id: "test".to_string(),
            role: role.to_string(),
            label: label.map(String::from),
            value: None,
            bounds: crate::computer::types::Rect::new(0, 0, 100, 30),
            enabled: true,
            focused: false,
            children: vec![],
        }
    }

    #[test]
    fn test_detect_password_field() {
        let detector = SensitiveUiDetector::new();
        let tree = make_node("text_field", Some("Password"));
        let findings = detector.scan_tree(&tree);
        assert!(findings
            .iter()
            .any(|f| matches!(f.category, SensitiveUiType::PasswordField)));
    }

    #[test]
    fn test_detect_password_role() {
        let detector = SensitiveUiDetector::new();
        let tree = make_node("password_field", None);
        let findings = detector.scan_tree(&tree);
        assert!(findings
            .iter()
            .any(|f| matches!(f.category, SensitiveUiType::PasswordField)));
    }

    #[test]
    fn test_detect_payment_field() {
        let detector = SensitiveUiDetector::new();
        let tree = make_node("text_field", Some("Card Number"));
        let findings = detector.scan_tree(&tree);
        assert!(findings
            .iter()
            .any(|f| matches!(f.category, SensitiveUiType::PaymentField)));
    }

    #[test]
    fn test_detect_destructive_confirmation() {
        let detector = SensitiveUiDetector::new();
        let tree = UiElement {
            id: "dlg".to_string(),
            role: "dialog".to_string(),
            label: Some("Are you sure you want to delete?".to_string()),
            value: None,
            bounds: crate::computer::types::Rect::new(0, 0, 400, 200),
            enabled: true,
            focused: false,
            children: vec![],
        };
        let findings = detector.scan_tree(&tree);
        assert!(findings
            .iter()
            .any(|f| matches!(f.category, SensitiveUiType::DestructiveConfirmation)));
    }

    #[test]
    fn test_no_false_positive() {
        let detector = SensitiveUiDetector::new();
        let tree = make_node("button", Some("OK"));
        let findings = detector.scan_tree(&tree);
        assert!(findings.is_empty());
    }

    #[test]
    fn test_chinese_password() {
        let detector = SensitiveUiDetector::new();
        let tree = make_node("text_field", Some("输入密码"));
        let findings = detector.scan_tree(&tree);
        assert!(findings
            .iter()
            .any(|f| matches!(f.category, SensitiveUiType::PasswordField)));
    }
}
