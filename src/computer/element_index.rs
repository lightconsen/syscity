//! Snapshot-bound element addressing.
//!
//! `ClickTarget::ElementId` used to be unusable in practice: every parser
//! filled `UiElement::id` with `String::new()`, and resolution searched only
//! the top-level roots (`tree.iter().find(..)`), so a target nested inside a
//! window — which is most of them — could never match.
//!
//! This module gives every element an id that a caller can hand back:
//!
//! - **Platform id** when the accessibility API exposes one
//!   (`AXIdentifier` on macOS, `AutomationId` on Windows). Stable across
//!   snapshots; resolved by a recursive search.
//! - **Snapshot-path id** otherwise: `s<snapshot:08x>:<child path>`, e.g.
//!   `s0000001f:0.2.1` — the 2nd window's 3rd child's 2nd child. The snapshot
//!   is what makes it honest: the id names an element *in the tree the caller
//!   was shown*, and resolving an id from a snapshot that is no longer cached
//!   fails with [`ResolveError::Stale`] instead of silently clicking whatever
//!   now happens to sit at that path.
//!
//! The cache holds the last few trees, because a perceive→act sequence can
//! interleave more than one read (the verification path re-reads the tree).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::computer::types::{ClickTarget, Point, UiElement};

/// Monotonic snapshot counter, process-wide. Only needs to be unique per
/// process: ids are never persisted.
static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(1);

/// Allocate a fresh snapshot id.
pub fn next_snapshot_id() -> u64 {
    NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed)
}

/// Build the snapshot-path form of an element id.
pub fn element_path_id(snapshot: u64, path: &[usize]) -> String {
    let mut id = format!("s{snapshot:08x}:");
    for (i, index) in path.iter().enumerate() {
        if i > 0 {
            id.push('.');
        }
        id.push_str(&index.to_string());
    }
    id
}

/// Parse a snapshot-path id back into `(snapshot, path)`.
///
/// Returns `None` for platform-generated ids, which carry no snapshot.
pub fn parse_element_id(id: &str) -> Option<(u64, Vec<usize>)> {
    let (snapshot_hex, path) = id.strip_prefix('s')?.split_once(':')?;
    let snapshot = u64::from_str_radix(snapshot_hex, 16).ok()?;
    if path.is_empty() {
        return None;
    }
    let mut indices = Vec::new();
    for part in path.split('.') {
        indices.push(part.parse::<usize>().ok()?);
    }
    Some((snapshot, indices))
}

/// Fill in an id for every element that does not have a platform one.
pub fn assign_ids(roots: &mut [UiElement], snapshot: u64) {
    let mut path = Vec::new();
    for (i, root) in roots.iter_mut().enumerate() {
        path.push(i);
        assign_one(root, snapshot, &mut path);
        path.pop();
    }
}

fn assign_one(element: &mut UiElement, snapshot: u64, path: &mut Vec<usize>) {
    if element.id.is_empty() {
        element.id = element_path_id(snapshot, path);
    }
    for (i, child) in element.children.iter_mut().enumerate() {
        path.push(i);
        assign_one(child, snapshot, path);
        path.pop();
    }
}

/// Follow a child-index path through the tree.
pub fn find_by_path<'a>(roots: &'a [UiElement], path: &[usize]) -> Option<&'a UiElement> {
    let mut current = roots.get(*path.first()?)?;
    for index in &path[1..] {
        current = current.children.get(*index)?;
    }
    Some(current)
}

/// An element resolved from a snapshot, with everything an action rung needs:
/// where it is, and how to address it again inside its application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedElement {
    pub id: String,
    pub role: String,
    pub label: Option<String>,
    pub center: Point,
    /// Child-index path from the window, for platforms that can address an
    /// element structurally (macOS `UI element N of ...`).
    pub path: Vec<usize>,
}

impl ResolvedElement {
    fn of(element: &UiElement, path: Vec<usize>) -> Self {
        Self {
            id: element.id.clone(),
            role: element.role.clone(),
            label: element.label.clone(),
            center: element.center(),
            path,
        }
    }

    /// Whether the rung ladder may try an accessibility action on this
    /// element. Rows of a table or a window's own frame usually have none.
    pub fn supports_ax_action(&self) -> bool {
        !self.path.is_empty()
    }
}

/// Where an action should be aimed, and what the caller may do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionTarget {
    pub x: i32,
    pub y: i32,
    /// Present when the target was an element rather than a bare coordinate.
    pub element: Option<ResolvedElement>,
    /// Application the snapshot came from, when known (needed for AX actions).
    pub app: Option<String>,
    /// Snapshot the element came from, when it was snapshot-addressed.
    pub snapshot: Option<u64>,
}

impl ActionTarget {
    pub fn point(x: i32, y: i32) -> Self {
        Self {
            x,
            y,
            element: None,
            app: None,
            snapshot: None,
        }
    }
}

/// Why a target could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No element matched.
    NotFound(String),
    /// The id names an element in a snapshot that is no longer cached. The
    /// caller must re-read the tree; resolving anyway would aim at whatever
    /// now occupies that path.
    Stale(u64),
    /// Nothing has been observed yet.
    NoSnapshot,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(t) => write!(f, "element not found: {t}"),
            Self::Stale(snapshot) => write!(
                f,
                "element id belongs to snapshot {snapshot:08x}, which is no longer cached; \
                 re-read the UI tree and use a fresh id"
            ),
            Self::NoSnapshot => write!(f, "no UI tree has been read yet"),
        }
    }
}

/// One observed UI tree.
#[derive(Debug, Clone)]
pub struct UiSnapshot {
    pub id: u64,
    /// Application the tree was read from, when the platform reports it.
    pub app: Option<String>,
    pub roots: Vec<UiElement>,
}

impl UiSnapshot {
    /// Resolve a click target against this snapshot.
    pub fn resolve(&self, target: &ClickTarget) -> Result<ActionTarget, ResolveError> {
        match target {
            ClickTarget::Coordinate(p) => Ok(ActionTarget::point(p.x, p.y)),
            ClickTarget::ElementId(id) => self
                .resolve_id(id)
                .ok_or_else(|| ResolveError::NotFound(id.clone())),
            ClickTarget::ElementLabel(label) => self
                .find(|el| {
                    el.label
                        .as_ref()
                        .map(|l| l.contains(label.as_str()))
                        .unwrap_or(false)
                })
                .ok_or_else(|| ResolveError::NotFound(format!("label~{label}"))),
            ClickTarget::ElementRoleLabel { role, label } => self
                .find(|el| {
                    el.role == *role
                        && el
                            .label
                            .as_ref()
                            .map(|l| l.contains(label.as_str()))
                            .unwrap_or(false)
                })
                .ok_or_else(|| ResolveError::NotFound(format!("{role}~{label}"))),
        }
    }

    /// Resolve an element id, either snapshot-path or platform.
    pub fn resolve_id(&self, id: &str) -> Option<ActionTarget> {
        match parse_element_id(id) {
            Some((snapshot, path)) => {
                if snapshot != self.id {
                    return None;
                }
                let element = find_by_path(&self.roots, &path)?;
                Some(self.target(element, path))
            }
            None => {
                // Platform id: stable across snapshots, so search the whole
                // tree — this is the recursive lookup the old code lacked.
                let (element, path) = find_path(&self.roots, &|el| el.id == id)?;
                Some(self.target(element, path))
            }
        }
    }

    /// Depth-first search for the first element matching `predicate`.
    pub fn find<F>(&self, predicate: F) -> Option<ActionTarget>
    where
        F: Fn(&UiElement) -> bool,
    {
        let (element, path) = find_path(&self.roots, &predicate)?;
        Some(self.target(element, path))
    }

    fn target(&self, element: &UiElement, path: Vec<usize>) -> ActionTarget {
        let resolved = ResolvedElement::of(element, path);
        ActionTarget {
            x: resolved.center.x,
            y: resolved.center.y,
            element: Some(resolved),
            app: self.app.clone(),
            snapshot: Some(self.id),
        }
    }

    /// Every element in the tree, depth-first.
    pub fn iter_elements(&self) -> Vec<&UiElement> {
        let mut out = Vec::new();
        fn walk<'a>(node: &'a UiElement, out: &mut Vec<&'a UiElement>) {
            out.push(node);
            for child in &node.children {
                walk(child, out);
            }
        }
        for root in &self.roots {
            walk(root, &mut out);
        }
        out
    }
}

/// Depth-first search returning the element and its index path.
fn find_path<'a, F>(roots: &'a [UiElement], predicate: &F) -> Option<(&'a UiElement, Vec<usize>)>
where
    F: Fn(&UiElement) -> bool,
{
    fn walk<'a, F>(
        node: &'a UiElement,
        predicate: &F,
        path: &mut Vec<usize>,
    ) -> Option<(&'a UiElement, Vec<usize>)>
    where
        F: Fn(&UiElement) -> bool,
    {
        if predicate(node) {
            return Some((node, path.clone()));
        }
        for (i, child) in node.children.iter().enumerate() {
            path.push(i);
            if let Some(found) = walk(child, predicate, path) {
                return Some(found);
            }
            path.pop();
        }
        None
    }

    let mut path = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        path.push(i);
        if let Some(found) = walk(root, predicate, &mut path) {
            return Some(found);
        }
        path.pop();
    }
    None
}

/// Bounded cache of recent UI trees, so an id handed to the model can be
/// resolved back to the exact tree it came from.
#[derive(Debug)]
pub struct SnapshotCache {
    capacity: usize,
    slots: tokio::sync::RwLock<VecDeque<UiSnapshot>>,
}

impl SnapshotCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            slots: tokio::sync::RwLock::new(VecDeque::new()),
        }
    }

    /// Assign a snapshot id and per-element ids, then remember the tree.
    pub async fn store(&self, mut roots: Vec<UiElement>, app: Option<String>) -> UiSnapshot {
        let id = next_snapshot_id();
        assign_ids(&mut roots, id);
        let snapshot = UiSnapshot { id, app, roots };
        let mut slots = self.slots.write().await;
        slots.push_back(snapshot.clone());
        while slots.len() > self.capacity {
            slots.pop_front();
        }
        snapshot
    }

    pub async fn get(&self, snapshot: u64) -> Option<UiSnapshot> {
        self.slots
            .read()
            .await
            .iter()
            .find(|s| s.id == snapshot)
            .cloned()
    }

    pub async fn latest(&self) -> Option<UiSnapshot> {
        self.slots.read().await.back().cloned()
    }

    /// Resolve a target against the snapshot it names.
    ///
    /// Snapshot-path ids resolve only against their own snapshot; label and
    /// platform-id targets use the most recent one.
    pub async fn resolve(&self, target: &ClickTarget) -> Result<ActionTarget, ResolveError> {
        if let ClickTarget::Coordinate(p) = target {
            return Ok(ActionTarget::point(p.x, p.y));
        }

        if let ClickTarget::ElementId(id) = target {
            if let Some((snapshot, _)) = parse_element_id(id) {
                let tree = self
                    .get(snapshot)
                    .await
                    .ok_or(ResolveError::Stale(snapshot))?;
                return tree
                    .resolve_id(id)
                    .ok_or(ResolveError::NotFound(id.clone()));
            }
            let tree = self.latest().await.ok_or(ResolveError::NoSnapshot)?;
            return tree
                .resolve_id(id)
                .ok_or_else(|| ResolveError::NotFound(id.clone()));
        }

        let tree = self.latest().await.ok_or(ResolveError::NoSnapshot)?;
        tree.resolve(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::types::Rect;

    fn element(role: &str, label: Option<&str>, x: i32, y: i32) -> UiElement {
        UiElement {
            id: String::new(),
            role: role.to_string(),
            label: label.map(String::from),
            value: None,
            bounds: Rect::new(x, y, 20, 10),
            enabled: true,
            focused: false,
            children: vec![],
        }
    }

    fn tree() -> Vec<UiElement> {
        let mut button = element("button", Some("Save"), 100, 50);
        button
            .children
            .push(element("static text", Some("hint"), 100, 50));
        let mut window = element("window", Some("Editor"), 0, 0);
        window
            .children
            .push(element("button", Some("Close"), 10, 10));
        window.children.push(button);
        vec![window]
    }

    #[test]
    fn every_element_gets_an_id() {
        let mut roots = tree();
        assign_ids(&mut roots, 7);
        assert_eq!(roots[0].id, "s00000007:0");
        assert_eq!(roots[0].children[0].id, "s00000007:0.0");
        assert_eq!(roots[0].children[1].id, "s00000007:0.1");
        assert_eq!(roots[0].children[1].children[0].id, "s00000007:0.1.0");
    }

    #[test]
    fn platform_ids_are_preserved() {
        let mut roots = tree();
        roots[0].children[1].id = "btnSave".to_string();
        assign_ids(&mut roots, 3);
        assert_eq!(roots[0].children[1].id, "btnSave");
        assert_eq!(roots[0].children[0].id, "s00000003:0.0");
    }

    #[test]
    fn nested_elements_resolve_by_id() {
        // The original defect: resolution searched only the roots, so a button
        // inside a window could never be found.
        let mut roots = tree();
        assign_ids(&mut roots, 9);
        let snapshot = UiSnapshot {
            id: 9,
            app: Some("Editor".to_string()),
            roots,
        };
        let target = snapshot
            .resolve(&ClickTarget::ElementId("s00000009:0.1".to_string()))
            .expect("nested element must resolve");
        assert_eq!(target.element.as_ref().unwrap().label.as_deref(), Some("Save"));
        assert_eq!((target.x, target.y), (110, 55));
        assert_eq!(target.element.unwrap().path, vec![0, 1]);
    }

    #[test]
    fn deep_grandchild_resolves() {
        let mut roots = tree();
        assign_ids(&mut roots, 9);
        let snapshot = UiSnapshot { id: 9, app: None, roots };
        let target = snapshot
            .resolve(&ClickTarget::ElementId("s00000009:0.1.0".to_string()))
            .unwrap();
        assert_eq!(target.element.unwrap().role, "static text");
    }

    #[test]
    fn labels_resolve_recursively() {
        let mut roots = tree();
        assign_ids(&mut roots, 9);
        let snapshot = UiSnapshot { id: 9, app: None, roots };
        let target = snapshot
            .resolve(&ClickTarget::ElementLabel("Save".to_string()))
            .unwrap();
        assert_eq!(target.element.unwrap().role, "button");
    }

    #[test]
    fn platform_id_resolves_recursively() {
        let mut roots = tree();
        roots[0].children[1].children[0].id = "ax-hint".to_string();
        assign_ids(&mut roots, 1);
        let snapshot = UiSnapshot { id: 1, app: None, roots };
        let target = snapshot
            .resolve(&ClickTarget::ElementId("ax-hint".to_string()))
            .unwrap();
        assert_eq!(target.element.unwrap().role, "static text");
    }

    #[test]
    fn stale_snapshot_is_an_error_not_a_guess() {
        let mut roots = tree();
        assign_ids(&mut roots, 4);
        let snapshot = UiSnapshot { id: 4, app: None, roots };
        // Id from a different snapshot must not resolve against this tree.
        assert!(snapshot.resolve_id("s00000005:0.1").is_none());
    }

    #[tokio::test]
    async fn cache_reports_stale_and_missing_snapshots() {
        let cache = SnapshotCache::new(2);
        let first = cache.store(tree(), None).await;
        let target = cache
            .resolve(&ClickTarget::ElementId(element_path_id(first.id, &[0, 1])))
            .await
            .unwrap();
        assert_eq!(target.element.unwrap().label.as_deref(), Some("Save"));

        // Evicted by newer snapshots: the id is stale, not silently remapped.
        cache.store(tree(), None).await;
        cache.store(tree(), None).await;
        let err = cache
            .resolve(&ClickTarget::ElementId(element_path_id(first.id, &[0, 1])))
            .await
            .unwrap_err();
        assert_eq!(err, ResolveError::Stale(first.id));
    }

    #[tokio::test]
    async fn coordinates_bypass_the_tree() {
        let cache = SnapshotCache::new(1);
        let target = cache
            .resolve(&ClickTarget::Coordinate(Point::new(5, 6)))
            .await
            .unwrap();
        assert_eq!((target.x, target.y), (5, 6));
        assert!(target.element.is_none());
    }
}
