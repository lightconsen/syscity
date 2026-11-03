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
//! - **Snapshot-path id** otherwise: `s<snapshot:016x>:<child path>`, e.g.
//!   `s1a2b3c4d0000001f:0.2.1` — the 2nd window's 3rd child's 2nd child of
//!   snapshot `1a2b3c4d0000001f`. The snapshot is what makes it honest: the id
//!   names an element *in the tree the caller was shown*, and resolving an id
//!   from a snapshot that is no longer cached fails with [`ResolveError::Stale`]
//!   instead of silently clicking whatever now happens to sit at that path.
//!
//!   The high half of that number is a per-process salt, not decoration. The
//!   counter restarts at 1 in every process, so a bare counter would let an id
//!   minted by an earlier gateway run (or by a different agent process on the
//!   same machine) name a live snapshot in this one and resolve against the
//!   wrong tree. The salt is drawn once per process from `RandomState`, so an
//!   id from another lifetime names a snapshot this process never had —
//!   reported as stale, which is true and actionable, instead of colliding
//!   with an unrelated tree.
//!
//! The cache holds the last few trees, because a perceive→act sequence can
//! interleave more than one read (the verification path re-reads the tree).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::computer::types::{AxAction, ClickTarget, Point, Tri, UiElement};

/// Monotonic snapshot counter, process-wide.
static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(1);

/// Per-process salt, in the high half of every snapshot id.
///
/// The counter alone restarts at 1 in each process, so `s00000003:0.1` minted
/// by a previous gateway run (or by a sibling CLI process) is
/// indistinguishable from one minted here and would resolve against an
/// unrelated tree. `RandomState` is seeded once per process from the OS;
/// hashing a constant gives the same salt for every snapshot in this process.
fn process_salt() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u8(0);
    hasher.finish() & 0xffff_ffff
}

/// The salt every snapshot id this process mints carries.
pub fn snapshot_salt() -> u64 {
    use std::sync::OnceLock;
    static SALT: OnceLock<u64> = OnceLock::new();
    *SALT.get_or_init(process_salt)
}

/// Allocate a fresh snapshot id: this process's salt and a fresh counter.
pub fn next_snapshot_id() -> u64 {
    (snapshot_salt() << 32) | NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed)
}

/// Build the snapshot-path form of an element id.
pub fn element_path_id(snapshot: u64, path: &[usize]) -> String {
    let mut id = format!("s{snapshot:016x}:");
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
    let (snapshot, path) = id.strip_prefix('s')?.split_once(':')?;
    let snapshot = u64::from_str_radix(snapshot, 16).ok()?;
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
    /// Center of the element, when its geometry was read.
    pub center: Option<Point>,
    /// The element's own rectangle, when the platform published one.
    ///
    /// A hit test cannot compare rectangles across calls (it reports the
    /// element the system would route an event to, from its own read), but
    /// the bounds are the one geometric fact the caller may want to show.
    pub bounds: Option<crate::computer::types::Rect>,
    /// Child-index path from the window, for platforms that can address an
    /// element structurally (macOS `UI element N of ...`).
    pub path: Vec<usize>,
    /// What the element says it can do, as the platform reported it.
    pub actions: Option<Vec<String>>,
    /// Whether the element accepts input, as the platform reported it.
    ///
    /// Only a positive "disabled" blocks an action: a read that did not say
    /// must not turn into a pre-check that rejects a perfectly clickable
    /// control.
    pub enabled: Tri,
}

impl ResolvedElement {
    fn of(element: &UiElement, path: Vec<usize>) -> Self {
        Self {
            id: element.id.clone(),
            role: element.role.clone(),
            label: element.label.clone(),
            center: element.center(),
            bounds: element.bounds,
            path,
            actions: element.actions.clone(),
            enabled: element.enabled,
        }
    }

    /// Whether the rung ladder may try an accessibility action on this
    /// element.
    ///
    /// Two questions, deliberately separate. An empty path means the platform
    /// has no structural address, so the action cannot be aimed at all — that
    /// is `Absent`. When there is a path, the element's own declared actions
    /// decide: a table row that publishes no actions is `Absent` too, and a
    /// platform that did not publish a list leaves it `Unknown`.
    pub fn supports_ax_action(&self, action: &AxAction) -> Tri {
        if self.path.is_empty() {
            return Tri::Absent;
        }
        let Some(actions) = self.actions.as_ref() else {
            return Tri::Unknown;
        };
        let wanted = action.declared_names();
        Tri::Known(actions.iter().any(|declared| {
            wanted
                .iter()
                .any(|name| declared.eq_ignore_ascii_case(name))
        }))
    }
}

/// Where an action should be aimed, and what the caller may do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionTarget {
    /// The point to aim at, when the target has one.
    ///
    /// `None` for an element whose geometry was not readable: the action can
    /// still be aimed at the element by accessibility, but nothing may be
    /// clicked at a point the platform never gave.
    pub point: Option<Point>,
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
            point: Some(Point::new(x, y)),
            element: None,
            app: None,
            snapshot: None,
        }
    }

    /// The coordinates of this target, or why there are none.
    pub fn coordinates(&self) -> crate::computer::Result<Point> {
        self.point.ok_or_else(|| {
            crate::computer::ComputerError::ElementNotFound(
                "the element has no readable geometry, so it cannot be clicked — target it by \
                 element id (the accessibility rung) or by screen coordinates"
                    .to_string(),
            )
        })
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
    ///
    /// `reason` separates the two ways a snapshot goes missing, because they
    /// call for different responses: an evicted tree means the caller held a
    /// real id for too long (act sooner), while an id this process never had
    /// means it came from somewhere else (another process, an older run).
    Stale {
        /// The snapshot the id names.
        snapshot: u64,
        /// Why it is not here.
        reason: StaleReason,
    },
    /// Nothing has been observed yet.
    NoSnapshot,
    /// More than one element matched, and picking one would be a guess.
    ///
    /// A label is not an identity: two "Save" buttons are two different
    /// buttons, and clicking the first is a coin flip the caller never asked
    /// for. The candidates travel with the error so the model can choose.
    Ambiguous {
        /// What was asked for, e.g. `label~Save`.
        query: String,
        /// How many elements matched.
        count: usize,
        /// The first few matches, so a caller can disambiguate.
        candidates: Vec<Candidate>,
    },
    /// Nothing matched, and the tree that was searched was not complete.
    ///
    /// "Not found" is only a fact about a tree that was read in full. A depth
    /// cut, a line cap or a child list the platform truncated means the
    /// element may exist in the part nobody looked at, and reporting that as
    /// `NotFound` turns a refusal into a false statement. The two need
    /// different next steps: re-aim, or re-read deeper.
    Incomplete {
        /// What was asked for, e.g. `label~Save`.
        query: String,
        /// How many elements the search did see.
        searched: usize,
    },
}

/// Why a snapshot id no longer resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// The cache held this snapshot and dropped it to make room for newer
    /// reads. The tree the caller saw really existed here.
    Evicted,
    /// This process never stored this snapshot — the id was minted elsewhere
    /// (another process, an earlier run) or predates the cache.
    NeverStored,
}

impl StaleReason {
    /// The sentence a caller needs to know what to do differently.
    fn advice(self) -> &'static str {
        match self {
            // The tree is real; the caller was slow. Say so plainly: the next
            // read will look like the one that just scrolled away.
            Self::Evicted => {
                "it was evicted to make room for newer reads; act sooner after reading"
            }
            // Nothing here ever held that tree, so "re-read" is not just a
            // remedy for lateness.
            Self::NeverStored => {
                "it was minted by another process or an earlier run; the UI must be read again here"
            }
        }
    }
}

/// One of several elements that matched a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub role: String,
    pub label: Option<String>,
    pub bounds: Option<crate::computer::Rect>,
}

impl Candidate {
    fn of(element: &UiElement) -> Self {
        Self {
            id: element.id.clone(),
            role: element.role.clone(),
            label: element.label.clone(),
            bounds: element.bounds,
        }
    }
}

/// How many candidates an `Ambiguous` error carries.
const MAX_CANDIDATES: usize = 8;

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(t) => write!(f, "element not found: {t}"),
            Self::Stale { snapshot, reason } => write!(
                f,
                "element id belongs to snapshot s{snapshot:016x}, which is no longer cached — \
                 {}; re-read the UI tree and use a fresh id",
                reason.advice()
            ),
            Self::NoSnapshot => write!(f, "no UI tree has been read yet"),
            Self::Incomplete { query, searched } => write!(
                f,
                "no element matched {query}, but the UI tree is incomplete ({searched} elements \
                 read): the element may be below a depth cut — re-read the tree with a deeper \
                 `max_depth`, or target what is visible"
            ),
            Self::Ambiguous { query, count, candidates } => {
                let listed: Vec<String> = candidates
                    .iter()
                    .map(|c| match c.label.as_deref() {
                        Some(label) => format!("{} {:?} [id={}]", c.role, label, c.id),
                        None => format!("{} [id={}]", c.role, c.id),
                    })
                    .collect();
                write!(
                    f,
                    "{count} elements match {query}; refusing to pick one — use an element id: \
                     {}",
                    listed.join("; ")
                )
            }
        }
    }
}

impl ResolveError {
    /// The computer-layer error this resolution failure means.
    ///
    /// One mapping for every adapter: three platforms and the remote adapter
    /// each used to spell out the same five arms, so a message fixed in one
    /// place stayed wrong in the others — and the snapshot id in the stale
    /// message was formatted for the old 8-digit ids long after ids grew a
    /// salt.
    pub fn into_computer_error(self) -> crate::computer::ComputerError {
        use crate::computer::ComputerError as E;
        match self {
            // A stale id is its own class: re-reading the tree, not re-aiming,
            // is what fixes it.
            Self::Stale { .. } => E::StaleElement(self.to_string()),
            Self::Ambiguous { .. } => E::AmbiguousTarget(self.to_string()),
            // A partial tree cannot say "not there". The message carries the
            // next step, which is a deeper read rather than a different aim.
            Self::Incomplete { .. } => E::TreeIncomplete(self.to_string()),
            Self::NotFound(_) | Self::NoSnapshot => E::ElementNotFound(self.to_string()),
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
    ///
    /// Label queries are answered strictly: an exact match wins, a substring
    /// match is only used when nothing matches exactly, and more than one
    /// match at the winning tier is an error rather than the first element in
    /// depth-first order. Picking one of two identical buttons is not a
    /// resolution, it is a guess with the caller's name on it.
    pub fn resolve(&self, target: &ClickTarget) -> Result<ActionTarget, ResolveError> {
        match target {
            ClickTarget::Coordinate(p) => Ok(ActionTarget::point(p.x, p.y)),
            ClickTarget::ElementId(id) => self.resolve_id(id).ok_or_else(|| {
                // A snapshot-path id names a slot in *this* tree, so the tree
                // not holding it is a wrong id — completeness cannot change
                // that. A platform id is a search, and a search of a partial
                // tree is exactly the case `Incomplete` exists for.
                if parse_element_id(id).is_some() {
                    ResolveError::NotFound(id.clone())
                } else {
                    self.not_found(id.clone())
                }
            }),
            ClickTarget::ElementLabel(label) => {
                let matches = self.collect(|el| el.label.as_deref() == Some(label.as_str()));
                let query = format!("label~{label}");
                self.one_or_ambiguous(query, matches, |el| {
                    el.label
                        .as_ref()
                        .map(|l| l.contains(label.as_str()))
                        .unwrap_or(false)
                })
            }
            ClickTarget::ElementRoleLabel { role, label } => {
                let matches = self
                    .collect(|el| el.role == *role && el.label.as_deref() == Some(label.as_str()));
                let query = format!("{role}~{label}");
                self.one_or_ambiguous(query, matches, |el| {
                    el.role == *role
                        && el
                            .label
                            .as_ref()
                            .map(|l| l.contains(label.as_str()))
                            .unwrap_or(false)
                })
            }
        }
    }

    /// Resolve the exact matches, or fall back to `contains` — but only when
    /// the exact tier is empty.
    ///
    /// The fallback is what makes a label query usable at all ("Save changes"
    /// for a button labelled "Save changes…"), and it is deliberately not a
    /// silent preference: whichever tier answered is the tier that is
    /// checked for ambiguity.
    fn one_or_ambiguous<F>(
        &self,
        query: String,
        exact: Vec<(&UiElement, Vec<usize>)>,
        contains: F,
    ) -> Result<ActionTarget, ResolveError>
    where
        F: Fn(&UiElement) -> bool,
    {
        let matches = if exact.is_empty() {
            let fuzzy = self.collect(contains);
            if fuzzy.is_empty() {
                return Err(self.not_found(query));
            }
            fuzzy
        } else {
            exact
        };
        if matches.len() > 1 {
            return Err(ResolveError::Ambiguous {
                query,
                count: matches.len(),
                candidates: matches
                    .iter()
                    .take(MAX_CANDIDATES)
                    .map(|(el, _)| Candidate::of(el))
                    .collect(),
            });
        }
        // `matches` is non-empty on both paths above: the exact tier only
        // runs with a match, and the fallback returns `NotFound` when empty.
        let Some((element, path)) = matches.into_iter().next() else {
            return Err(self.not_found(query));
        };
        Ok(self.target(element, path))
    }

    /// The right "nothing matched" for this tree.
    ///
    /// `NotFound` asserts the element is not there, which is only true of a
    /// tree that was read in full. A partial tree gets `Incomplete` instead,
    /// so the caller re-reads deeper rather than re-aiming at nothing.
    fn not_found(&self, query: String) -> ResolveError {
        if self.is_complete() {
            ResolveError::NotFound(query)
        } else {
            ResolveError::Incomplete {
                query,
                searched: self.iter_elements().len(),
            }
        }
    }

    /// Every element matching `predicate`, with its index path.
    fn collect<F>(&self, predicate: F) -> Vec<(&UiElement, Vec<usize>)>
    where
        F: Fn(&UiElement) -> bool,
    {
        let mut out = Vec::new();
        fn walk<'a, F>(
            node: &'a UiElement,
            predicate: &F,
            path: &mut Vec<usize>,
            out: &mut Vec<(&'a UiElement, Vec<usize>)>,
        ) where
            F: Fn(&UiElement) -> bool,
        {
            if predicate(node) {
                out.push((node, path.clone()));
            }
            for (i, child) in node.children.iter().enumerate() {
                path.push(i);
                walk(child, predicate, path, out);
                path.pop();
            }
        }
        for (i, root) in self.roots.iter().enumerate() {
            let mut path = vec![i];
            walk(root, &predicate, &mut path, &mut out);
        }
        out
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

    /// Whether this tree was read in full — every node's children list is
    /// known to be the whole list.
    pub fn is_complete(&self) -> bool {
        UiElement::tree_is_complete(&self.roots)
    }

    fn target(&self, element: &UiElement, path: Vec<usize>) -> ActionTarget {
        let resolved = ResolvedElement::of(element, path);
        ActionTarget {
            point: resolved.center,
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
    /// Snapshots this cache used to hold and dropped, most recent last.
    ///
    /// Bounded by `capacity`, which is enough to tell "we had it and made
    /// room" from "we never had it" — the only distinction a caller can act
    /// on — without keeping every tree that ever passed through.
    retired: tokio::sync::RwLock<VecDeque<u64>>,
}

impl SnapshotCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            slots: tokio::sync::RwLock::new(VecDeque::new()),
            retired: tokio::sync::RwLock::new(VecDeque::new()),
        }
    }

    /// Assign a snapshot id and per-element ids, then remember the tree.
    pub async fn store(&self, mut roots: Vec<UiElement>, app: Option<String>) -> UiSnapshot {
        let id = next_snapshot_id();
        assign_ids(&mut roots, id);
        let snapshot = UiSnapshot { id, app, roots };
        let mut slots = self.slots.write().await;
        let mut retired = self.retired.write().await;
        slots.push_back(snapshot.clone());
        while slots.len() > self.capacity {
            if let Some(dropped) = slots.pop_front() {
                // Remember that this one was ours, so a late id gets "act
                // sooner" rather than "that tree was never here".
                retired.push_back(dropped.id);
            }
            while retired.len() > self.capacity {
                retired.pop_front();
            }
        }
        snapshot
    }

    /// Whether this cache ever held `snapshot`.
    async fn stale_reason(&self, snapshot: u64) -> StaleReason {
        if self.retired.read().await.contains(&snapshot) {
            StaleReason::Evicted
        } else {
            StaleReason::NeverStored
        }
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
                let tree = match self.get(snapshot).await {
                    Some(tree) => tree,
                    None => {
                        return Err(ResolveError::Stale {
                            snapshot,
                            reason: self.stale_reason(snapshot).await,
                        });
                    }
                };
                // A path id the tree cannot hold is a wrong id, not a search
                // that came up empty — completeness says nothing about it.
                return tree
                    .resolve_id(id)
                    .ok_or(ResolveError::NotFound(id.clone()));
            }
            let tree = self.latest().await.ok_or(ResolveError::NoSnapshot)?;
            let query = id.clone();
            return tree.resolve_id(id).ok_or_else(|| tree.not_found(query));
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
            bounds: Some(Rect::new(x, y, 20, 10)),
            enabled: Tri::Known(true),
            focused: Tri::Known(false),
            actions: None,
            children: vec![],
            children_count: None,
            truncated: false,
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
        assert_eq!(roots[0].id, element_path_id(7, &[0]));
        assert_eq!(roots[0].children[0].id, element_path_id(7, &[0, 0]));
        assert_eq!(roots[0].children[1].id, element_path_id(7, &[0, 1]));
        assert_eq!(roots[0].children[1].children[0].id, element_path_id(7, &[0, 1, 0]));
        // The path half is what a human reads off the outline.
        assert!(roots[0].children[1].id.ends_with(":0.1"), "{}", roots[0].children[1].id);
    }

    /// Each resolution failure keeps its own class and its own next step.
    #[test]
    fn resolution_failures_map_to_their_own_error_classes() {
        use crate::computer::ComputerError;
        let stale = ResolveError::Stale {
            snapshot: 0x1234_5678_0000_0005,
            reason: StaleReason::NeverStored,
        }
        .into_computer_error();
        match &stale {
            ComputerError::StaleElement(message) => {
                // The full id, salt included: an id the caller can match
                // against what it was shown.
                assert!(message.contains("s1234567800000005"), "{message}");
                assert!(message.contains("re-read the UI tree"), "{message}");
            }
            other => panic!("expected StaleElement, got {other:?}"),
        }
        assert!(matches!(
            ResolveError::NoSnapshot.into_computer_error(),
            ComputerError::ElementNotFound(_)
        ));
        assert!(matches!(
            ResolveError::NotFound("x".into()).into_computer_error(),
            ComputerError::ElementNotFound(_)
        ));
        let ambiguous = ResolveError::Ambiguous {
            query: "label~Save".into(),
            count: 2,
            candidates: vec![],
        }
        .into_computer_error();
        assert!(matches!(ambiguous, ComputerError::AmbiguousTarget(_)));
        let incomplete = ResolveError::Incomplete {
            query: "label~Save".into(),
            searched: 3,
        }
        .into_computer_error();
        assert!(matches!(incomplete, ComputerError::TreeIncomplete(_)));
        // The classes are what a caller branches on, so they must differ.
        use crate::error::ComputerErrorCode;
        let codes: Vec<&str> = [
            ResolveError::Stale {
                snapshot: 1,
                reason: StaleReason::Evicted,
            }
            .into_computer_error(),
            ResolveError::NotFound("x".into()).into_computer_error(),
            ResolveError::Ambiguous {
                query: "q".into(),
                count: 2,
                candidates: vec![],
            }
            .into_computer_error(),
            ResolveError::Incomplete { query: "q".into(), searched: 1 }.into_computer_error(),
        ]
        .iter()
        .map(|e| ComputerErrorCode::of(e).0.as_str())
        .collect();
        assert_eq!(
            codes,
            vec![
                "desktop.stale_ref",
                "desktop.element_not_found",
                "desktop.ambiguous_target",
                "desktop.tree_incomplete",
            ]
        );
    }

    /// The counter restarts in every process; the salt is what stops an id
    /// minted elsewhere from naming a live snapshot here.
    #[tokio::test]
    async fn an_id_from_another_process_does_not_resolve_against_this_one() {
        let cache = SnapshotCache::new(4);
        let mine = cache.store(tree(), None).await;
        let id = element_path_id(mine.id, &[0, 1]);
        assert_eq!(parse_element_id(&id), Some((mine.id, vec![0, 1])));
        assert!(cache
            .resolve(&ClickTarget::ElementId(id.clone()))
            .await
            .is_ok());

        // Same counter, different salt: the shape another process mints.
        let counter = mine.id & 0xffff_ffff;
        let elsewhere = (counter ^ 0x1234_5678_0000_0000) | counter;
        assert_ne!(elsewhere, mine.id);
        let foreign = element_path_id(elsewhere, &[0, 1]);
        assert_eq!(parse_element_id(&foreign), Some((elsewhere, vec![0, 1])));
        assert_eq!(
            cache.resolve(&ClickTarget::ElementId(foreign)).await.err(),
            Some(ResolveError::Stale {
                snapshot: elsewhere,
                reason: StaleReason::NeverStored,
            })
        );
    }

    /// Ids minted in separate calls carry the same salt and differ only in
    /// the counter, so the path half stays readable.
    #[tokio::test]
    async fn ids_from_one_process_are_readable_and_distinct() {
        let cache = SnapshotCache::new(4);
        let first = cache.store(tree(), None).await;
        let second = cache.store(tree(), None).await;
        assert_eq!(first.id >> 32, snapshot_salt());
        assert_eq!(first.id >> 32, second.id >> 32);
        assert_ne!(first.id & 0xffff_ffff, second.id & 0xffff_ffff);
        let id = element_path_id(first.id, &[0, 1]);
        assert!(id.ends_with(":0.1"), "{id}");
    }

    #[test]
    fn platform_ids_are_preserved() {
        let mut roots = tree();
        roots[0].children[1].id = "btnSave".to_string();
        assign_ids(&mut roots, 3);
        assert_eq!(roots[0].children[1].id, "btnSave");
        assert_eq!(roots[0].children[0].id, element_path_id(3, &[0, 0]));
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
        assert_eq!(target.point, Some(Point::new(110, 55)));
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
        assert_eq!(
            err,
            ResolveError::Stale {
                snapshot: first.id,
                reason: StaleReason::Evicted
            }
        );
        // The message says which kind of staleness this is: the caller was
        // slow, not wrong about the tree.
        assert!(err.to_string().contains("evicted"), "{err}");
    }

    #[tokio::test]
    async fn coordinates_bypass_the_tree() {
        let cache = SnapshotCache::new(1);
        let target = cache
            .resolve(&ClickTarget::Coordinate(Point::new(5, 6)))
            .await
            .unwrap();
        assert_eq!(target.point, Some(Point::new(5, 6)));
        assert!(target.element.is_none());
    }

    #[test]
    fn two_identical_labels_are_ambiguous_not_a_guess() {
        let mut roots = tree();
        // A second "Save" button in the same window.
        roots[0]
            .children
            .push(element("button", Some("Save"), 300, 50));
        assign_ids(&mut roots, 9);
        let snapshot = UiSnapshot { id: 9, app: None, roots };

        let err = snapshot
            .resolve(&ClickTarget::ElementLabel("Save".to_string()))
            .expect_err("two matching buttons must not resolve to the first");
        match err {
            ResolveError::Ambiguous { count, candidates, .. } => {
                assert_eq!(count, 2);
                assert_eq!(candidates.len(), 2);
                assert!(candidates
                    .iter()
                    .all(|c| c.label.as_deref() == Some("Save")));
                // The candidates carry ids, so the model can pick one.
                assert!(candidates.iter().all(|c| !c.id.is_empty()));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn the_ambiguous_error_names_the_candidates() {
        let mut roots = tree();
        roots[0]
            .children
            .push(element("button", Some("Save"), 300, 50));
        assign_ids(&mut roots, 9);
        let snapshot = UiSnapshot { id: 9, app: None, roots };
        let err = snapshot
            .resolve(&ClickTarget::ElementLabel("Save".to_string()))
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("2 elements match"), "{text}");
        assert!(text.contains("use an element id"), "{text}");
        assert!(text.contains(&element_path_id(9, &[0, 1])), "{text}");
    }

    #[test]
    fn an_exact_label_wins_over_a_substring_match() {
        let mut roots = tree();
        // "Save" is a substring of "Save As…"; the exact button must win.
        roots[0]
            .children
            .push(element("button", Some("Save As…"), 300, 50));
        assign_ids(&mut roots, 9);
        let snapshot = UiSnapshot { id: 9, app: None, roots };

        let target = snapshot
            .resolve(&ClickTarget::ElementLabel("Save".to_string()))
            .expect("the exact match must resolve");
        assert_eq!(target.element.unwrap().label.as_deref(), Some("Save"));
    }

    #[test]
    fn a_substring_still_resolves_when_nothing_matches_exactly() {
        let mut roots = tree();
        roots[0]
            .children
            .push(element("button", Some("Save As…"), 300, 50));
        assign_ids(&mut roots, 9);
        let snapshot = UiSnapshot { id: 9, app: None, roots };

        let target = snapshot
            .resolve(&ClickTarget::ElementLabel("Save As".to_string()))
            .expect("a unique substring must resolve");
        assert_eq!(target.element.unwrap().label.as_deref(), Some("Save As…"));
    }

    #[test]
    fn two_substring_matches_are_ambiguous_too() {
        let mut roots = vec![element("window", Some("Files"), 0, 0)];
        roots[0]
            .children
            .push(element("button", Some("Open Recent"), 10, 10));
        roots[0]
            .children
            .push(element("button", Some("Open Other"), 10, 30));
        assign_ids(&mut roots, 5);
        let snapshot = UiSnapshot { id: 5, app: None, roots };

        let err = snapshot
            .resolve(&ClickTarget::ElementLabel("Open".to_string()))
            .unwrap_err();
        assert!(matches!(err, ResolveError::Ambiguous { count: 2, .. }), "{err:?}");
    }

    #[test]
    fn role_and_label_disambiguate_each_other() {
        let mut roots = vec![element("window", Some("Files"), 0, 0)];
        roots[0]
            .children
            .push(element("button", Some("Open"), 10, 10));
        roots[0]
            .children
            .push(element("menuitem", Some("Open"), 10, 30));
        assign_ids(&mut roots, 5);
        let snapshot = UiSnapshot { id: 5, app: None, roots };

        let target = snapshot
            .resolve(&ClickTarget::ElementRoleLabel {
                role: "menuitem".to_string(),
                label: "Open".to_string(),
            })
            .expect("the role narrows the match to one");
        assert_eq!(target.element.unwrap().role, "menuitem");
    }

    #[test]
    fn an_element_with_no_geometry_has_no_point() {
        let mut roots = tree();
        roots[0].children[1].bounds = None;
        assign_ids(&mut roots, 9);
        let snapshot = UiSnapshot { id: 9, app: None, roots };

        let target = snapshot
            .resolve(&ClickTarget::ElementId("s00000009:0.1".to_string()))
            .unwrap();
        assert_eq!(target.point, None);
        assert!(target.coordinates().is_err(), "a point-less target must not be clickable");
        // It can still be acted on by accessibility.
        assert!(target.element.is_some());
    }

    #[test]
    fn the_ax_gate_reads_the_elements_declared_actions() {
        let mut roots = tree();
        roots[0].children[1].actions = Some(vec!["AXPress".to_string()]);
        assign_ids(&mut roots, 9);
        let snapshot = UiSnapshot { id: 9, app: None, roots };
        let target = snapshot
            .resolve(&ClickTarget::ElementId("s00000009:0.1".to_string()))
            .unwrap();
        let element = target.element.unwrap();
        assert_eq!(element.supports_ax_action(&AxAction::Press), Tri::Known(true));
        assert_eq!(
            element.supports_ax_action(&AxAction::SetValue { value: "x".to_string() }),
            Tri::Known(false)
        );
    }

    #[test]
    fn an_element_that_declares_no_actions_skips_the_ax_rung() {
        let mut roots = tree();
        roots[0].children[1].actions = Some(vec![]);
        assign_ids(&mut roots, 9);
        let snapshot = UiSnapshot { id: 9, app: None, roots };
        let element = snapshot
            .resolve(&ClickTarget::ElementId("s00000009:0.1".to_string()))
            .unwrap()
            .element
            .unwrap();
        assert_eq!(element.supports_ax_action(&AxAction::Press), Tri::Known(false));
    }

    #[test]
    fn an_element_that_did_not_publish_actions_leaves_the_rung_open() {
        let mut roots = tree();
        roots[0].children[1].actions = None;
        assign_ids(&mut roots, 9);
        let snapshot = UiSnapshot { id: 9, app: None, roots };
        let element = snapshot
            .resolve(&ClickTarget::ElementId("s00000009:0.1".to_string()))
            .unwrap()
            .element
            .unwrap();
        assert_eq!(element.supports_ax_action(&AxAction::Press), Tri::Unknown);
    }

    #[test]
    fn an_unaddressable_element_is_absent_from_the_ax_rung() {
        let mut roots = tree();
        roots[0].children[1].id = "platform-id".to_string();
        assign_ids(&mut roots, 9);
        // Resolving a platform id still yields a path in this tree; the gate
        // turns on the *empty* path, which no parser has produced yet, so the
        // element with actions but no path must be represented directly.
        let element = ResolvedElement::of(&roots[0].children[1], vec![]);
        assert_eq!(element.supports_ax_action(&AxAction::Press), Tri::Absent);
    }

    /// "Not found" is a claim about the tree, and a partial tree cannot make
    /// it: the element may be in the part the read never reached.
    #[test]
    fn a_missing_element_in_a_cut_tree_is_incomplete_not_not_found() {
        let mut roots = tree();
        assign_ids(&mut roots, 9);
        let snapshot = UiSnapshot { id: 9, app: None, roots };
        assert!(snapshot.is_complete());
        assert!(matches!(
            snapshot.resolve(&ClickTarget::ElementLabel("Quit".to_string())),
            Err(ResolveError::NotFound(_))
        ));

        // Now the same tree, with the depth limit visibly having bitten.
        let mut roots = tree();
        roots[0].children_count = Some(9);
        assign_ids(&mut roots, 10);
        let snapshot = UiSnapshot { id: 10, app: None, roots };
        assert!(!snapshot.is_complete());
        match snapshot.resolve(&ClickTarget::ElementLabel("Quit".to_string())) {
            Err(ResolveError::Incomplete { query, searched }) => {
                assert_eq!(query, "label~Quit");
                assert!(searched > 0, "the error says how much was searched");
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    /// The message has to name the next step, or it is just `NotFound` with
    /// extra words.
    #[test]
    fn the_incomplete_error_says_to_read_deeper() {
        let mut roots = tree();
        roots[0].truncated = true;
        let snapshot = UiSnapshot { id: 11, app: None, roots };
        let text = snapshot
            .resolve(&ClickTarget::ElementRoleLabel {
                role: "button".to_string(),
                label: "Quit".to_string(),
            })
            .unwrap_err()
            .to_string();
        assert!(text.contains("incomplete"), "{text}");
        assert!(text.contains("max_depth"), "{text}");
        assert!(!text.contains("element not found"), "{text}");
    }

    /// A tree that reports every child it has is complete, so a miss stays a
    /// miss — the new state must not turn every lookup into "maybe".
    #[test]
    fn a_count_that_matches_the_children_keeps_not_found() {
        let mut roots = tree();
        roots[0].children_count = Some(roots[0].children.len() as u32);
        assign_ids(&mut roots, 12);
        let snapshot = UiSnapshot { id: 12, app: None, roots };
        assert!(snapshot.is_complete());
        assert!(matches!(
            snapshot.resolve(&ClickTarget::ElementLabel("Quit".to_string())),
            Err(ResolveError::NotFound(_))
        ));
    }

    /// A snapshot-path id that the tree does not hold is a wrong id, not a
    /// failed search: completeness cannot say anything about it.
    #[test]
    fn an_unresolvable_path_id_stays_not_found_in_a_cut_tree() {
        let mut roots = tree();
        roots[0].truncated = true;
        assign_ids(&mut roots, 13);
        let snapshot = UiSnapshot { id: 13, app: None, roots };
        assert!(matches!(
            snapshot.resolve(&ClickTarget::ElementId("s00000013:0.9".to_string())),
            Err(ResolveError::NotFound(_))
        ));
    }
}
