//! Canvas/A2UI - Dynamic UI Generation System for Syscity
//!
//! Provides A2UI (Agent-to-UI) capabilities for generating
//! dynamic user interfaces through WebSocket updates. Supports forms, buttons,
//! progress indicators, and real-time content streaming.
// INVARIANTS-NONE: canvases are process-local UI surfaces with no persisted state to hold invariants over.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::Message;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{debug, warn};
use uuid::Uuid;

/// Unique identifier for a UI session
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanvasId(pub String);

impl CanvasId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }
}

impl Default for CanvasId {
    fn default() -> Self {
        Self::new()
    }
}

/// A2UI Component types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum CanvasComponent {
    /// Container for other components
    Container {
        id: String,
        children: Vec<CanvasComponent>,
        layout: Option<ContainerLayout>,
    },
    /// Text display
    Text {
        id: String,
        content: String,
        style: Option<TextStyle>,
    },
    /// Markdown content
    Markdown { id: String, content: String },
    /// Input field
    Input {
        id: String,
        label: Option<String>,
        placeholder: Option<String>,
        value: Option<String>,
        input_type: Option<String>,
        required: Option<bool>,
    },
    /// Textarea for multi-line input
    Textarea {
        id: String,
        label: Option<String>,
        placeholder: Option<String>,
        value: Option<String>,
        rows: Option<u32>,
    },
    /// Button
    Button {
        id: String,
        label: String,
        variant: Option<String>,
        disabled: Option<bool>,
    },
    /// Select dropdown
    Select {
        id: String,
        label: Option<String>,
        options: Vec<SelectOption>,
        value: Option<String>,
    },
    /// Checkbox
    Checkbox {
        id: String,
        label: String,
        checked: Option<bool>,
    },
    /// Radio button group
    RadioGroup {
        id: String,
        label: Option<String>,
        options: Vec<SelectOption>,
        value: Option<String>,
    },
    /// Progress bar
    Progress {
        id: String,
        value: f64,
        max: Option<f64>,
        label: Option<String>,
    },
    /// Spinner/loading indicator
    Spinner { id: String, label: Option<String> },
    /// Image display
    Image {
        id: String,
        src: String,
        alt: Option<String>,
    },
    /// Code block with syntax highlighting
    Code {
        id: String,
        content: String,
        language: Option<String>,
    },
    /// Table display
    Table {
        id: String,
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// Divider line
    Divider { id: String },
    /// Alert/notification
    Alert {
        id: String,
        level: String,
        message: String,
    },
}

impl CanvasComponent {
    /// Return the unique identifier of this component.
    pub fn id(&self) -> &str {
        match self {
            CanvasComponent::Container { id, .. } => id,
            CanvasComponent::Text { id, .. } => id,
            CanvasComponent::Markdown { id, .. } => id,
            CanvasComponent::Input { id, .. } => id,
            CanvasComponent::Textarea { id, .. } => id,
            CanvasComponent::Button { id, .. } => id,
            CanvasComponent::Select { id, .. } => id,
            CanvasComponent::Checkbox { id, .. } => id,
            CanvasComponent::RadioGroup { id, .. } => id,
            CanvasComponent::Progress { id, .. } => id,
            CanvasComponent::Spinner { id, .. } => id,
            CanvasComponent::Image { id, .. } => id,
            CanvasComponent::Code { id, .. } => id,
            CanvasComponent::Table { id, .. } => id,
            CanvasComponent::Divider { id, .. } => id,
            CanvasComponent::Alert { id, .. } => id,
        }
    }

    /// Recursively find and replace a component by ID.
    /// Returns true if the component was found and replaced.
    pub fn update_by_id(&mut self, target_id: &str, new_component: CanvasComponent) -> bool {
        if self.id() == target_id {
            *self = new_component;
            return true;
        }
        if let CanvasComponent::Container { children, .. } = self {
            // Check direct children first (zero clones for shallow updates)
            if let Some(pos) = children.iter().position(|c| c.id() == target_id) {
                children[pos] = new_component;
                return true;
            }
            // Recurse — at most one clone per depth level
            for child in children.iter_mut() {
                if child.update_by_id(target_id, new_component.clone()) {
                    return true;
                }
            }
        }
        false
    }

    /// Recursively find a Container by parent_id and append a child.
    /// Returns true if the parent was found.
    pub fn append_child(&mut self, parent_id: &str, child: CanvasComponent) -> bool {
        if let CanvasComponent::Container { id, children, .. } = self {
            if id == parent_id {
                children.push(child);
                return true;
            }
            // Check direct children first (zero clones for shallow appends)
            if let Some(pos) = children
                .iter()
                .position(|c| matches!(c, CanvasComponent::Container { id, .. } if id == parent_id))
            {
                if let CanvasComponent::Container { children: target_children, .. } =
                    &mut children[pos]
                {
                    target_children.push(child);
                    return true;
                }
            }
            // Recurse — at most one clone per depth level
            for c in children.iter_mut() {
                if c.append_child(parent_id, child.clone()) {
                    return true;
                }
            }
        }
        false
    }

    /// Recursively find the parent of target_id and remove it.
    /// Returns true if the component was found and removed.
    pub fn remove_by_id(&mut self, target_id: &str) -> bool {
        if let CanvasComponent::Container { children, .. } = self {
            if let Some(pos) = children.iter().position(|c| c.id() == target_id) {
                children.remove(pos);
                return true;
            }
            for child in children.iter_mut() {
                if child.remove_by_id(target_id) {
                    return true;
                }
            }
        }
        false
    }
}

/// Container layout options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerLayout {
    Vertical,
    Horizontal,
    Grid { columns: u32 },
}

/// Text styling options
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextStyle {
    pub size: Option<String>,
    pub weight: Option<String>,
    pub color: Option<String>,
}

/// Select option
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

/// User interaction event from Canvas
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum CanvasEvent {
    /// Button clicked
    ButtonClick { component_id: String },
    /// Input value changed
    InputChange { component_id: String, value: String },
    /// Select option changed
    SelectChange { component_id: String, value: String },
    /// Checkbox toggled
    CheckboxChange { component_id: String, checked: bool },
    /// Radio selection changed
    RadioChange { component_id: String, value: String },
    /// Form submitted
    FormSubmit {
        component_id: String,
        values: HashMap<String, Value>,
    },
    /// Canvas closed by user
    Close,
}

/// Canvas update message (sent to clients)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum CanvasUpdate {
    /// Initialize/replace entire canvas
    Init {
        canvas_id: String,
        root: CanvasComponent,
    },
    /// Update specific component
    Update {
        component_id: String,
        component: CanvasComponent,
    },
    /// Remove component
    Remove { component_id: String },
    /// Append child to container
    Append {
        parent_id: String,
        component: CanvasComponent,
    },
    /// Show alert/notification
    Notify { level: String, message: String },
    /// Close canvas
    Close,
}

/// Canvas session state
pub struct CanvasSession {
    pub id: CanvasId,
    pub root: RwLock<CanvasComponent>,
    pub event_tx: mpsc::Sender<CanvasEvent>,
    pub update_tx: broadcast::Sender<CanvasUpdate>,
}

impl std::fmt::Debug for CanvasSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CanvasSession")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl CanvasSession {
    pub fn new(event_tx: mpsc::Sender<CanvasEvent>) -> Self {
        let id = CanvasId::new();
        let (update_tx, _) = broadcast::channel(1024);

        Self {
            id: id.clone(),
            root: RwLock::new(CanvasComponent::Container {
                id: "root".to_string(),
                children: vec![],
                layout: Some(ContainerLayout::Vertical),
            }),
            event_tx,
            update_tx,
        }
    }

    /// Initialize canvas with root component
    pub async fn init(&self, root: CanvasComponent) {
        let mut guard = self.root.write().await;
        *guard = root.clone();

        if self
            .update_tx
            .send(CanvasUpdate::Init {
                canvas_id: self.id.0.clone(),
                root,
            })
            .is_err()
        {
            warn!("Canvas {}: init send failed (no receivers)", self.id.0);
        }
    }

    /// Update a specific component
    pub async fn update(&self, component_id: String, component: CanvasComponent) {
        let mut guard = self.root.write().await;
        let found = guard.update_by_id(&component_id, component.clone());
        drop(guard);

        if !found {
            warn!("Canvas {}: update target '{}' not found in tree", self.id.0, component_id);
        }

        if self
            .update_tx
            .send(CanvasUpdate::Update { component_id, component })
            .is_err()
        {
            warn!("Canvas {}: update send failed (no receivers)", self.id.0);
        }
    }

    /// Append component to container
    pub async fn append(&self, parent_id: String, component: CanvasComponent) {
        let mut guard = self.root.write().await;
        let found = guard.append_child(&parent_id, component.clone());
        drop(guard);

        if !found {
            warn!("Canvas {}: append parent '{}' not found in tree", self.id.0, parent_id);
        }

        if self
            .update_tx
            .send(CanvasUpdate::Append { parent_id, component })
            .is_err()
        {
            warn!("Canvas {}: append send failed (no receivers)", self.id.0);
        }
    }

    /// Remove a component from the tree
    pub async fn remove(&self, component_id: String) {
        let mut guard = self.root.write().await;
        let found = guard.remove_by_id(&component_id);
        drop(guard);

        if !found {
            warn!("Canvas {}: remove target '{}' not found in tree", self.id.0, component_id);
        }

        if self
            .update_tx
            .send(CanvasUpdate::Remove { component_id })
            .is_err()
        {
            warn!("Canvas {}: remove send failed (no receivers)", self.id.0);
        }
    }

    /// Show notification
    pub async fn notify(&self, level: String, message: String) {
        if self
            .update_tx
            .send(CanvasUpdate::Notify { level, message })
            .is_err()
        {
            warn!("Canvas {}: notify send failed (no receivers)", self.id.0);
        }
    }

    /// Close canvas
    pub async fn close(&self) {
        if self.update_tx.send(CanvasUpdate::Close).is_err() {
            warn!("Canvas {}: close send failed (no receivers)", self.id.0);
        }
    }
}

/// Canvas manager handles multiple UI sessions
pub struct CanvasManager {
    sessions: RwLock<HashMap<CanvasId, Arc<CanvasSession>>>,
    /// Maps external session IDs (e.g. conversation IDs) to canvas sessions.
    session_map: RwLock<HashMap<String, CanvasId>>,
    /// Event receivers for sessions created via `get_or_create_for_session`.
    /// Keyed by external session ID.
    event_rxs: RwLock<HashMap<String, mpsc::Receiver<CanvasEvent>>>,
}

impl std::fmt::Debug for CanvasManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CanvasManager")
            .field("session_count", &self.sessions.try_read().ok().map(|s| s.len()))
            .finish_non_exhaustive()
    }
}

impl CanvasManager {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            session_map: RwLock::new(HashMap::new()),
            event_rxs: RwLock::new(HashMap::new()),
        }
    }

    /// Create new canvas session
    pub async fn create_session(&self, event_tx: mpsc::Sender<CanvasEvent>) -> Arc<CanvasSession> {
        let session = Arc::new(CanvasSession::new(event_tx));
        let mut sessions = self.sessions.write().await;
        sessions.insert(session.id.clone(), session.clone());
        session
    }

    /// Get or create a canvas session tied to an external session_id.
    ///
    /// Used by the outbound pipeline to render UI for a specific conversation.
    /// The event receiver is stored in the manager; call
    /// [`take_event_rx`](Self::take_event_rx) to retrieve it for event
    /// processing.
    pub async fn get_or_create_for_session(&self, session_id: &str) -> Arc<CanvasSession> {
        // Use a single write lock for atomic check-and-insert to prevent TOCTOU race
        let mut map = self.session_map.write().await;
        let mut sessions = self.sessions.write().await;

        if let Some(canvas_id) = map.get(session_id) {
            if let Some(session) = sessions.get(canvas_id) {
                return session.clone();
            }
        }

        let (event_tx, event_rx) = mpsc::channel(256);
        let session = Arc::new(CanvasSession::new(event_tx));
        let canvas_id = session.id.clone();
        map.insert(session_id.to_string(), canvas_id.clone());
        sessions.insert(canvas_id.clone(), session.clone());
        drop(sessions);
        drop(map);

        // Store the event receiver so events are not silently dropped
        let mut rxs = self.event_rxs.write().await;
        rxs.insert(session_id.to_string(), event_rx);

        session
    }

    /// Take the event receiver for a session created via
    /// [`get_or_create_for_session`].
    ///
    /// Returns `None` if no receiver is registered for this session (e.g. if
    /// the session was created via [`create_session`](Self::create_session)
    /// instead, or if the receiver was already taken).
    pub async fn take_event_rx(&self, session_id: &str) -> Option<mpsc::Receiver<CanvasEvent>> {
        self.event_rxs.write().await.remove(session_id)
    }

    /// Apply a [`CanvasUpdate`] to the session associated with `session_id`.
    pub async fn apply_update(&self, session_id: &str, update: CanvasUpdate) {
        let session = self.get_or_create_for_session(session_id).await;
        match update {
            CanvasUpdate::Init { root, .. } => session.init(root).await,
            CanvasUpdate::Update { component_id, component } => {
                session.update(component_id, component).await;
            }
            CanvasUpdate::Remove { component_id } => {
                session.remove(component_id).await;
            }
            CanvasUpdate::Append { parent_id, component } => {
                session.append(parent_id, component).await;
            }
            CanvasUpdate::Notify { level, message } => {
                session.notify(level, message).await;
            }
            CanvasUpdate::Close => session.close().await,
        }
    }

    /// Get session by ID
    pub async fn get_session(&self, id: &CanvasId) -> Option<Arc<CanvasSession>> {
        let sessions = self.sessions.read().await;
        sessions.get(id).cloned()
    }

    /// Remove session, cleaning up all three maps.
    ///
    /// Locks are acquired and released sequentially (never held simultaneously)
    /// in `session_map → sessions → event_rxs` order to match
    /// [`get_or_create_for_session`] and prevent AB-BA deadlock.
    pub async fn remove_session(&self, id: &CanvasId) {
        // 1. Collect external session IDs from session_map and remove them
        let mut map = self.session_map.write().await;
        let external_ids: Vec<String> = map
            .iter()
            .filter(|(_, v)| *v == id)
            .map(|(k, _)| k.clone())
            .collect();
        for key in &external_ids {
            map.remove(key);
        }
        drop(map);

        // 2. Remove session from the primary map
        let mut sessions = self.sessions.write().await;
        sessions.remove(id);
        drop(sessions);

        // 3. Cleanup the event receivers
        let mut rxs = self.event_rxs.write().await;
        for key in &external_ids {
            rxs.remove(key);
        }
    }

    /// List active sessions
    pub async fn list_sessions(&self) -> Vec<CanvasId> {
        let sessions = self.sessions.read().await;
        sessions.keys().cloned().collect()
    }
}

impl Default for CanvasManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Canvas protocol handler for WebSocket
pub struct CanvasWebSocketHandler {
    canvas_id: CanvasId,
    event_tx: mpsc::Sender<CanvasEvent>,
    update_rx: broadcast::Receiver<CanvasUpdate>,
}

impl CanvasWebSocketHandler {
    pub fn new(
        canvas_id: CanvasId,
        event_tx: mpsc::Sender<CanvasEvent>,
        update_rx: broadcast::Receiver<CanvasUpdate>,
    ) -> Self {
        Self { canvas_id, event_tx, update_rx }
    }

    /// Handle incoming WebSocket message
    pub async fn handle_message(&self, msg: Message) -> Option<CanvasEvent> {
        match msg {
            Message::Text(text) => {
                debug!("Canvas {} received message: {}", self.canvas_id.0, text);

                match serde_json::from_str::<CanvasEvent>(&text) {
                    Ok(event) => {
                        if self.event_tx.send(event.clone()).await.is_err() {
                            warn!(
                                "Canvas {}: event send failed (receiver dropped)",
                                self.canvas_id.0
                            );
                        }
                        Some(event)
                    }
                    Err(e) => {
                        warn!("Failed to parse canvas event: {}", e);
                        None
                    }
                }
            }
            Message::Close(_) => {
                debug!("Canvas {} received close frame", self.canvas_id.0);
                let event = CanvasEvent::Close;
                if self.event_tx.send(event.clone()).await.is_err() {
                    warn!(
                        "Canvas {}: close event send failed (receiver dropped)",
                        self.canvas_id.0
                    );
                }
                Some(event)
            }
            _ => None,
        }
    }

    /// Get next update to send to client
    pub async fn next_update(&mut self) -> Option<CanvasUpdate> {
        loop {
            match self.update_rx.recv().await {
                Ok(update) => return Some(update),
                Err(RecvError::Lagged(n)) => {
                    warn!("Canvas {} broadcast lagged by {} messages", self.canvas_id.0, n);
                    continue;
                }
                Err(RecvError::Closed) => return None,
            }
        }
    }

    /// Get canvas ID
    pub fn canvas_id(&self) -> &CanvasId {
        &self.canvas_id
    }

    /// Close the canvas session and remove it from the manager.
    ///
    /// Should be called when the WebSocket connection is torn down so the
    /// session and its associated resources are cleaned up.
    pub async fn close_session(&self, manager: &CanvasManager) {
        manager.remove_session(&self.canvas_id).await;
    }
}

/// Helper functions for creating common UI patterns
pub mod helpers {
    use super::*;

    /// Create a simple form with inputs and submit button
    pub fn create_form(id: impl Into<String>, inputs: Vec<(String, String)>) -> CanvasComponent {
        let id = id.into();
        let mut children = vec![];

        for (input_id, label) in inputs {
            children.push(CanvasComponent::Input {
                id: format!("{}_{}", id, input_id),
                label: Some(label),
                placeholder: None,
                value: None,
                input_type: Some("text".to_string()),
                required: Some(true),
            });
        }

        children.push(CanvasComponent::Button {
            id: format!("{}_submit", id),
            label: "Submit".to_string(),
            variant: Some("primary".to_string()),
            disabled: Some(false),
        });

        CanvasComponent::Container {
            id,
            children,
            layout: Some(ContainerLayout::Vertical),
        }
    }

    /// Create a progress indicator
    pub fn create_progress(
        id: impl Into<String>,
        value: f64,
        label: Option<String>,
    ) -> CanvasComponent {
        CanvasComponent::Progress {
            id: id.into(),
            value,
            max: Some(100.0),
            label,
        }
    }

    /// Create an alert
    pub fn create_alert(
        id: impl Into<String>,
        level: impl Into<String>,
        message: impl Into<String>,
    ) -> CanvasComponent {
        CanvasComponent::Alert {
            id: id.into(),
            level: level.into(),
            message: message.into(),
        }
    }

    /// Create a button group
    pub fn create_button_group(id: impl Into<String>, labels: Vec<String>) -> CanvasComponent {
        let id = id.into();
        let children = labels
            .into_iter()
            .enumerate()
            .map(|(i, label)| CanvasComponent::Button {
                id: format!("{}_btn_{}", id, i),
                label,
                variant: Some("secondary".to_string()),
                disabled: Some(false),
            })
            .collect();

        CanvasComponent::Container {
            id,
            children,
            layout: Some(ContainerLayout::Horizontal),
        }
    }

    /// Create a code display with copy button
    pub fn create_code_block(
        id: impl Into<String>,
        content: impl Into<String>,
        language: Option<String>,
    ) -> CanvasComponent {
        let id = id.into();
        CanvasComponent::Container {
            id: id.clone(),
            children: vec![
                CanvasComponent::Code {
                    id: format!("{}_code", id),
                    content: content.into(),
                    language,
                },
                CanvasComponent::Button {
                    id: format!("{}_copy", id),
                    label: "Copy".to_string(),
                    variant: Some("ghost".to_string()),
                    disabled: Some(false),
                },
            ],
            layout: Some(ContainerLayout::Vertical),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canvas_id_generation() {
        let id1 = CanvasId::new();
        let id2 = CanvasId::new();
        assert_ne!(id1.0, id2.0);
    }

    #[test]
    fn test_component_serialization() {
        let component = CanvasComponent::Text {
            id: "test".to_string(),
            content: "Hello".to_string(),
            style: None,
        };

        let json = serde_json::to_string(&component).unwrap();
        assert!(json.contains("text"));
        assert!(json.contains("Hello"));
    }

    #[test]
    fn test_canvas_update_serialization() {
        let update = CanvasUpdate::Notify {
            level: "info".to_string(),
            message: "Test".to_string(),
        };

        let json = serde_json::to_string(&update).unwrap();
        assert!(json.contains("notify"));
        assert!(json.contains("info"));
    }

    #[test]
    fn test_helper_create_form() {
        let form = helpers::create_form(
            "my_form",
            vec![
                ("name".to_string(), "Name".to_string()),
                ("email".to_string(), "Email".to_string()),
            ],
        );

        match form {
            CanvasComponent::Container { children, .. } => {
                assert_eq!(children.len(), 3); // 2 inputs + 1 button
            }
            _ => panic!("Expected container"),
        }
    }

    #[test]
    fn test_button_serialization() {
        let btn = CanvasComponent::Button {
            id: "btn1".to_string(),
            label: "Click me".to_string(),
            variant: Some("primary".to_string()),
            disabled: Some(false),
        };
        let json = serde_json::to_string(&btn).unwrap();
        assert!(json.contains("button"));
        assert!(json.contains("Click me"));
        assert!(json.contains("primary"));
    }

    #[test]
    fn test_input_serialization() {
        let input = CanvasComponent::Input {
            id: "input1".to_string(),
            label: Some("Username".to_string()),
            placeholder: Some("Enter username".to_string()),
            value: None,
            input_type: Some("text".to_string()),
            required: Some(true),
        };
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains("input"));
        assert!(json.contains("Username"));
    }

    #[test]
    fn test_progress_serialization() {
        let progress = CanvasComponent::Progress {
            id: "prog1".to_string(),
            value: 42.0,
            max: Some(100.0),
            label: Some("Loading".to_string()),
        };
        let json = serde_json::to_string(&progress).unwrap();
        assert!(json.contains("progress"));
        assert!(json.contains("42"));
    }

    #[test]
    fn test_table_serialization() {
        let table = CanvasComponent::Table {
            id: "tbl1".to_string(),
            headers: vec!["Name".to_string(), "Value".to_string()],
            rows: vec![vec!["A".to_string(), "1".to_string()]],
        };
        let json = serde_json::to_string(&table).unwrap();
        assert!(json.contains("table"));
        assert!(json.contains("Name"));
        assert!(json.contains("Value"));
    }

    #[test]
    fn test_alert_serialization() {
        let alert = CanvasComponent::Alert {
            id: "alert1".to_string(),
            level: "error".to_string(),
            message: "Something went wrong".to_string(),
        };
        let json = serde_json::to_string(&alert).unwrap();
        assert!(json.contains("alert"));
        assert!(json.contains("error"));
    }

    #[test]
    fn test_select_option_serialization() {
        let opt = SelectOption {
            value: "opt1".to_string(),
            label: "Option 1".to_string(),
        };
        let json = serde_json::to_string(&opt).unwrap();
        assert!(json.contains("opt1"));
        assert!(json.contains("Option 1"));
    }

    #[test]
    fn test_canvas_event_serialization() {
        let event = CanvasEvent::ButtonClick {
            component_id: "btn1".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("button_click"));
        assert!(json.contains("btn1"));
    }

    #[test]
    fn test_canvas_event_form_submit() {
        let mut values = HashMap::new();
        values.insert("name".to_string(), serde_json::json!("test"));
        let event = CanvasEvent::FormSubmit {
            component_id: "form1".to_string(),
            values,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("form_submit"));
        assert!(json.contains("form1"));
    }

    #[tokio::test]
    async fn test_canvas_manager_create_and_get_session() {
        let manager = CanvasManager::new();
        let (tx, _rx) = mpsc::channel(10);

        let session = manager.create_session(tx).await;
        assert!(!session.id.0.is_empty());

        let retrieved = manager.get_session(&session.id).await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().id.0, session.id.0);
    }

    #[tokio::test]
    async fn test_canvas_manager_list_sessions() {
        let manager = CanvasManager::new();
        let (tx1, _rx1) = mpsc::channel(10);
        let (tx2, _rx2) = mpsc::channel(10);

        let s1 = manager.create_session(tx1).await;
        let s2 = manager.create_session(tx2).await;

        let list = manager.list_sessions().await;
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|id| id.0 == s1.id.0));
        assert!(list.iter().any(|id| id.0 == s2.id.0));
    }

    #[tokio::test]
    async fn test_canvas_manager_remove_session() {
        let manager = CanvasManager::new();
        let (tx, _rx) = mpsc::channel(10);

        let session = manager.create_session(tx).await;
        manager.remove_session(&session.id).await;

        let retrieved = manager.get_session(&session.id).await;
        assert!(retrieved.is_none());

        let list = manager.list_sessions().await;
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_canvas_manager_get_or_create_for_session() {
        let manager = CanvasManager::new();

        let session1 = manager.get_or_create_for_session("conv-123").await;
        let session2 = manager.get_or_create_for_session("conv-123").await;

        assert_eq!(session1.id.0, session2.id.0);

        let session3 = manager.get_or_create_for_session("conv-456").await;
        assert_ne!(session1.id.0, session3.id.0);
    }

    #[tokio::test]
    async fn test_canvas_session_init_sends_update() {
        let (tx, _rx) = mpsc::channel(10);
        let session = CanvasSession::new(tx);

        // Subscribe before sending
        let mut rx = session.update_tx.subscribe();

        let root = CanvasComponent::Text {
            id: "root".to_string(),
            content: "Hello".to_string(),
            style: None,
        };

        session.init(root).await;

        let update = rx.try_recv();
        assert!(update.is_ok());
        match update.unwrap() {
            CanvasUpdate::Init { canvas_id, .. } => {
                assert_eq!(canvas_id, session.id.0);
            }
            _ => panic!("Expected Init update"),
        }
    }

    #[tokio::test]
    async fn test_canvas_session_notify_sends_update() {
        let (tx, _rx) = mpsc::channel(10);
        let session = CanvasSession::new(tx);

        // Subscribe before sending
        let mut rx = session.update_tx.subscribe();

        session
            .notify("info".to_string(), "Test message".to_string())
            .await;

        let update = rx.try_recv();
        assert!(update.is_ok());
        match update.unwrap() {
            CanvasUpdate::Notify { level, message } => {
                assert_eq!(level, "info");
                assert_eq!(message, "Test message");
            }
            _ => panic!("Expected Notify update"),
        }
    }

    #[tokio::test]
    async fn test_canvas_session_close_sends_update() {
        let (tx, _rx) = mpsc::channel(10);
        let session = CanvasSession::new(tx);

        // Subscribe before sending
        let mut rx = session.update_tx.subscribe();

        session.close().await;

        let update = rx.try_recv();
        assert!(update.is_ok());
        assert!(matches!(update.unwrap(), CanvasUpdate::Close));
    }

    #[tokio::test]
    async fn test_canvas_websocket_handle_message() {
        let (event_tx, mut event_rx) = mpsc::channel(10);
        let (_update_tx, update_rx) = broadcast::channel(10);
        let canvas_id = CanvasId::new();

        let handler = CanvasWebSocketHandler::new(canvas_id, event_tx, update_rx);

        let msg = Message::Text(r#"{"event":"button_click","component_id":"btn1"}"#.into());
        let result = handler.handle_message(msg).await;

        assert!(result.is_some());
        assert!(matches!(result.unwrap(), CanvasEvent::ButtonClick { .. }));

        // Verify event was forwarded
        let forwarded = event_rx.try_recv();
        assert!(forwarded.is_ok());
    }

    #[tokio::test]
    async fn test_canvas_websocket_handle_binary_returns_none() {
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (_update_tx, update_rx) = broadcast::channel(10);
        let canvas_id = CanvasId::new();

        let handler = CanvasWebSocketHandler::new(canvas_id, event_tx, update_rx);

        let msg = Message::Binary(vec![1, 2, 3].into());
        let result = handler.handle_message(msg).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_canvas_websocket_next_update() {
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (update_tx, update_rx) = broadcast::channel(10);
        let canvas_id = CanvasId::new();

        let mut handler = CanvasWebSocketHandler::new(canvas_id, event_tx, update_rx);

        let _ = update_tx.send(CanvasUpdate::Close);

        let update = handler.next_update().await;
        assert!(update.is_some());
        assert!(matches!(update.unwrap(), CanvasUpdate::Close));
    }

    #[test]
    fn test_helper_create_progress() {
        let progress = helpers::create_progress("prog1", 75.0, Some("Loading...".to_string()));
        match progress {
            CanvasComponent::Progress { id, value, max, label } => {
                assert_eq!(id, "prog1");
                assert_eq!(value, 75.0);
                assert_eq!(max, Some(100.0));
                assert_eq!(label, Some("Loading...".to_string()));
            }
            _ => panic!("Expected progress"),
        }
    }

    #[test]
    fn test_helper_create_alert() {
        let alert = helpers::create_alert("alert1", "warning", "Be careful");
        match alert {
            CanvasComponent::Alert { id, level, message } => {
                assert_eq!(id, "alert1");
                assert_eq!(level, "warning");
                assert_eq!(message, "Be careful");
            }
            _ => panic!("Expected alert"),
        }
    }

    #[test]
    fn test_helper_create_button_group() {
        let group = helpers::create_button_group("bg1", vec!["A".to_string(), "B".to_string()]);
        match group {
            CanvasComponent::Container { children, layout, .. } => {
                assert_eq!(children.len(), 2);
                assert!(matches!(layout, Some(ContainerLayout::Horizontal)));
            }
            _ => panic!("Expected container"),
        }
    }

    #[test]
    fn test_helper_create_code_block() {
        let block = helpers::create_code_block("code1", "fn main() {}", Some("rust".to_string()));
        match block {
            CanvasComponent::Container { children, .. } => {
                assert_eq!(children.len(), 2); // code + copy button
            }
            _ => panic!("Expected container"),
        }
    }

    #[test]
    fn test_container_layout_serialization() {
        let layout = ContainerLayout::Grid { columns: 3 };
        let json = serde_json::to_string(&layout).unwrap();
        assert!(json.contains("grid"));
    }

    #[test]
    fn test_text_style_default() {
        let style = TextStyle {
            size: None,
            weight: None,
            color: None,
        };
        assert!(style.size.is_none());
        assert!(style.weight.is_none());
        assert!(style.color.is_none());
    }

    // -----------------------------------------------------------------------
    // Component tree operation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_component_update_by_id_direct_child() {
        let mut container = CanvasComponent::Container {
            id: "root".to_string(),
            children: vec![
                CanvasComponent::Text {
                    id: "txt1".to_string(),
                    content: "old".to_string(),
                    style: None,
                },
                CanvasComponent::Button {
                    id: "btn1".to_string(),
                    label: "Click".to_string(),
                    variant: None,
                    disabled: None,
                },
            ],
            layout: None,
        };

        let updated = CanvasComponent::Text {
            id: "txt1".to_string(),
            content: "new".to_string(),
            style: None,
        };

        assert!(container.update_by_id("txt1", updated));
        match &container {
            CanvasComponent::Container { children, .. } => {
                assert_eq!(children.len(), 2);
                match &children[0] {
                    CanvasComponent::Text { content, .. } => assert_eq!(content, "new"),
                    _ => panic!("Expected Text"),
                }
            }
            _ => panic!("Expected Container"),
        }
    }

    #[test]
    fn test_component_update_by_id_nested() {
        let mut tree = CanvasComponent::Container {
            id: "root".to_string(),
            children: vec![CanvasComponent::Container {
                id: "inner".to_string(),
                children: vec![CanvasComponent::Text {
                    id: "target".to_string(),
                    content: "old".to_string(),
                    style: None,
                }],
                layout: None,
            }],
            layout: None,
        };

        let updated = CanvasComponent::Text {
            id: "target".to_string(),
            content: "updated".to_string(),
            style: None,
        };

        assert!(tree.update_by_id("target", updated));
    }

    #[test]
    fn test_component_update_by_id_not_found() {
        let mut container = CanvasComponent::Container {
            id: "root".to_string(),
            children: vec![CanvasComponent::Text {
                id: "txt1".to_string(),
                content: "hello".to_string(),
                style: None,
            }],
            layout: None,
        };

        let updated = CanvasComponent::Text {
            id: "nonexistent".to_string(),
            content: "should not appear".to_string(),
            style: None,
        };

        assert!(!container.update_by_id("missing", updated));
    }

    #[test]
    fn test_component_append_child() {
        let mut container = CanvasComponent::Container {
            id: "root".to_string(),
            children: vec![],
            layout: None,
        };

        let child = CanvasComponent::Text {
            id: "new_child".to_string(),
            content: "appended".to_string(),
            style: None,
        };

        assert!(container.append_child("root", child));
        match &container {
            CanvasComponent::Container { children, .. } => {
                assert_eq!(children.len(), 1);
            }
            _ => panic!("Expected Container"),
        }
    }

    #[test]
    fn test_component_append_child_parent_not_found() {
        let mut container = CanvasComponent::Container {
            id: "root".to_string(),
            children: vec![],
            layout: None,
        };

        let child = CanvasComponent::Text {
            id: "orphan".to_string(),
            content: "never added".to_string(),
            style: None,
        };

        assert!(!container.append_child("nonexistent", child));
    }

    #[test]
    fn test_component_remove_by_id() {
        let mut container = CanvasComponent::Container {
            id: "root".to_string(),
            children: vec![
                CanvasComponent::Text {
                    id: "txt1".to_string(),
                    content: "remove me".to_string(),
                    style: None,
                },
                CanvasComponent::Divider { id: "div1".to_string() },
            ],
            layout: None,
        };

        assert!(container.remove_by_id("txt1"));
        match &container {
            CanvasComponent::Container { children, .. } => {
                assert_eq!(children.len(), 1);
                assert_eq!(children[0].id(), "div1");
            }
            _ => panic!("Expected Container"),
        }
    }

    #[test]
    fn test_component_remove_nonexistent() {
        let mut container = CanvasComponent::Container {
            id: "root".to_string(),
            children: vec![],
            layout: None,
        };

        assert!(!container.remove_by_id("ghost"));
    }

    #[test]
    fn test_component_id_on_non_container() {
        let text = CanvasComponent::Text {
            id: "my_text".to_string(),
            content: "hello".to_string(),
            style: None,
        };
        assert_eq!(text.id(), "my_text");

        let divider = CanvasComponent::Divider { id: "sep".to_string() };
        assert_eq!(divider.id(), "sep");
    }

    // -----------------------------------------------------------------------
    // CanvasManager extended tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_take_event_rx() {
        let manager = CanvasManager::new();

        let _session = manager.get_or_create_for_session("test-session").await;
        let rx = manager.take_event_rx("test-session").await;
        assert!(rx.is_some());

        // Second take should return None (already taken)
        let rx2 = manager.take_event_rx("test-session").await;
        assert!(rx2.is_none());
    }

    #[tokio::test]
    async fn test_take_event_rx_receives_events() {
        let manager = CanvasManager::new();

        let session = manager.get_or_create_for_session("test-session").await;
        let mut rx = manager.take_event_rx("test-session").await.unwrap();

        // Simulate an event coming through the WebSocket handler
        let event = CanvasEvent::ButtonClick {
            component_id: "btn1".to_string(),
        };
        session.event_tx.send(event.clone()).await.unwrap();

        let received = rx.recv().await;
        assert!(received.is_some());
        assert!(matches!(received.unwrap(), CanvasEvent::ButtonClick { .. }));
    }

    #[tokio::test]
    async fn test_canvas_manager_remove_session_cleans_all_maps() {
        let manager = CanvasManager::new();

        let session = manager.get_or_create_for_session("cleanup-test").await;
        let canvas_id = session.id.clone();
        let _rx = manager.take_event_rx("cleanup-test").await;

        manager.remove_session(&canvas_id).await;

        // Session should be gone
        assert!(manager.get_session(&canvas_id).await.is_none());
        assert!(manager.list_sessions().await.is_empty());

        // get_or_create_for_session should create a fresh session
        let new_session = manager.get_or_create_for_session("cleanup-test").await;
        assert_ne!(new_session.id, canvas_id);
    }

    #[tokio::test]
    async fn test_handle_message_close() {
        let (event_tx, mut event_rx) = mpsc::channel(10);
        let (_update_tx, update_rx) = broadcast::channel(10);
        let canvas_id = CanvasId::new();

        let handler = CanvasWebSocketHandler::new(canvas_id, event_tx, update_rx);

        let result = handler.handle_message(Message::Close(None)).await;
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), CanvasEvent::Close));

        let forwarded = event_rx.try_recv();
        assert!(forwarded.is_ok());
        assert!(matches!(forwarded.unwrap(), CanvasEvent::Close));
    }

    #[tokio::test]
    async fn test_canvas_id_default_generates_valid_id() {
        let id = CanvasId::default();
        assert!(!id.0.is_empty());
        // Should be a valid UUID format
        assert_eq!(id.0.len(), 36);
    }

    // -----------------------------------------------------------------------
    // apply_update dispatch tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_apply_update_init_dispatch() {
        let manager = CanvasManager::new();

        let root = CanvasComponent::Text {
            id: "greeting".to_string(),
            content: "hello".to_string(),
            style: None,
        };

        manager
            .apply_update(
                "session-1",
                CanvasUpdate::Init {
                    canvas_id: "ignored".to_string(),
                    root,
                },
            )
            .await;

        // Session should have been created and root set
        let session = manager.get_or_create_for_session("session-1").await;
        let guard = session.root.read().await;
        assert_eq!(guard.id(), "greeting");
    }

    #[tokio::test]
    async fn test_apply_update_notify_dispatch() {
        let manager = CanvasManager::new();
        let session = manager.get_or_create_for_session("session-1").await;
        let mut rx = session.update_tx.subscribe();

        manager
            .apply_update(
                "session-1",
                CanvasUpdate::Notify {
                    level: "warn".to_string(),
                    message: "test notification".to_string(),
                },
            )
            .await;

        let update = rx.try_recv().unwrap();
        assert!(matches!(update, CanvasUpdate::Notify { level, message }
            if level == "warn" && message == "test notification"));
    }

    #[tokio::test]
    async fn test_apply_update_close_dispatch() {
        let manager = CanvasManager::new();
        let session = manager.get_or_create_for_session("session-1").await;
        let mut rx = session.update_tx.subscribe();

        manager.apply_update("session-1", CanvasUpdate::Close).await;

        let update = rx.try_recv().unwrap();
        assert!(matches!(update, CanvasUpdate::Close));
    }

    #[tokio::test]
    async fn test_apply_update_update_dispatch() {
        let manager = CanvasManager::new();
        let session = manager.get_or_create_for_session("session-1").await;
        let mut rx = session.update_tx.subscribe();

        let component = CanvasComponent::Text {
            id: "title".to_string(),
            content: "updated".to_string(),
            style: None,
        };

        manager
            .apply_update(
                "session-1",
                CanvasUpdate::Update {
                    component_id: "title".to_string(),
                    component,
                },
            )
            .await;

        let update = rx.try_recv().unwrap();
        assert!(matches!(update, CanvasUpdate::Update { .. }));
    }

    #[tokio::test]
    async fn test_apply_update_remove_dispatch() {
        let manager = CanvasManager::new();
        let session = manager.get_or_create_for_session("session-1").await;
        let mut rx = session.update_tx.subscribe();

        manager
            .apply_update(
                "session-1",
                CanvasUpdate::Remove {
                    component_id: "old-comp".to_string(),
                },
            )
            .await;

        let update = rx.try_recv().unwrap();
        assert!(matches!(update, CanvasUpdate::Remove { .. }));
    }

    #[tokio::test]
    async fn test_apply_update_append_dispatch() {
        let manager = CanvasManager::new();
        let session = manager.get_or_create_for_session("session-1").await;
        let mut rx = session.update_tx.subscribe();

        manager
            .apply_update(
                "session-1",
                CanvasUpdate::Append {
                    parent_id: "root".to_string(),
                    component: CanvasComponent::Text {
                        id: "child".to_string(),
                        content: "appended".to_string(),
                        style: None,
                    },
                },
            )
            .await;

        let update = rx.try_recv().unwrap();
        assert!(matches!(update, CanvasUpdate::Append { .. }));
    }

    // -----------------------------------------------------------------------
    // WebSocket handler cleanup tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_handler_close_session_removes_from_manager() {
        let manager = CanvasManager::new();
        let session = manager.get_or_create_for_session("cleanup-test").await;
        let canvas_id = session.id.clone();

        let (event_tx, _event_rx) = mpsc::channel(10);
        let (_update_tx, update_rx) = broadcast::channel(10);

        // Create a new handler that references the same canvas_id
        let handler = CanvasWebSocketHandler::new(canvas_id, event_tx, update_rx);

        assert!(manager.get_session(handler.canvas_id()).await.is_some());

        handler.close_session(&manager).await;

        assert!(manager.get_session(handler.canvas_id()).await.is_none());
    }
}
