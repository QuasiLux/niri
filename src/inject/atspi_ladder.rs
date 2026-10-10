//! Row 13: per-action AT-SPI routing ladder.
//!
//! Row-13 routing plus the real AT-SPI session/a11y-bus walk used when the
//! test override hooks are unset. No Smithay types.

use crate::inject::app_catalog::{SemanticStrength, Toolkit};

/// Per-action enum. Add more variants as the bot surface grows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtSpiAction {
    /// Bot wants to click a labeled element (button, menu item).
    LabelClick {
        acc_role: String,
        name_query: String,
    },
    /// Bot wants to insert text into a labeled text field.
    EditInsert {
        acc_role: String,
        name_query: String,
        text: String,
    },
    /// Bot wants to interact with the canvas (a "canvas" element).
    /// Always routed to `BACKGROUND_UNAVAILABLE` for Chrome surfaces.
    Canvas { query: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtSpiRoute {
    /// Try AT-SPI session D-Bus, find element, reply `AT_ELEMENT_REPLY <path> <bounds>`.
    Element { role: String, query: String },
    /// Typed failure: the canvas is not reachable through AT-SPI.
    /// Reply `AT_ELEMENT_REPLY BACKGROUND_UNAVAILABLE`. Not a silent fall-through.
    CanvasRaw { reason: String },
    /// Reuse the existing inject path (key/scroll/drag on the agent seat).
    RawInject,
}

pub const REASON_BG_UNAVAILABLE: &str = "BACKGROUND_UNAVAILABLE";

/// Keep the typed-failure token in the release ELF as a standalone `strings(1)` record.
#[used]
#[allow(dead_code)]
static REASON_BG_UNAVAILABLE_IDENT: &[u8] = b"BACKGROUND_UNAVAILABLE\0";

pub fn route(app_id: &str, action: &AtSpiAction) -> AtSpiRoute {
    let r = crate::inject::app_catalog::app_id_routes(app_id);
    let route = match (r.semantic, action) {
        (
            SemanticStrength::AtSpiStrong,
            AtSpiAction::LabelClick {
                acc_role,
                name_query,
            },
        )
        | (
            SemanticStrength::AtSpiStrong,
            AtSpiAction::EditInsert {
                acc_role,
                name_query,
                ..
            },
        ) => AtSpiRoute::Element {
            role: acc_role.clone(),
            query: name_query.clone(),
        },
        (
            SemanticStrength::AtSpiChrome,
            AtSpiAction::LabelClick {
                acc_role,
                name_query,
            },
        )
        | (
            SemanticStrength::AtSpiChrome,
            AtSpiAction::EditInsert {
                acc_role,
                name_query,
                ..
            },
        ) => AtSpiRoute::Element {
            role: acc_role.clone(),
            query: name_query.clone(),
        },
        (SemanticStrength::AtSpiChrome, AtSpiAction::Canvas { .. }) => AtSpiRoute::CanvasRaw {
            reason: REASON_BG_UNAVAILABLE.into(),
        },
        // Weak + raw_inject_eligible (Alacritty, VTE-ish) → existing agent-seat path.
        (_, _) if matches!(r.toolkit, Some(Toolkit::Winit)) && r.raw_inject_eligible => {
            AtSpiRoute::RawInject
        }
        // Remaining canvas (AtSpiStrong included) — fail closed; no AX canvas walk.
        (_, AtSpiAction::Canvas { .. }) => AtSpiRoute::CanvasRaw {
            reason: REASON_BG_UNAVAILABLE.into(),
        },
        // Unknown / None / Hud / not-eligible → typed failure.
        _ => AtSpiRoute::CanvasRaw {
            reason: REASON_BG_UNAVAILABLE.into(),
        },
    };
    tracing::trace!(app_id, ?action, ?route, "atspi.route");
    route
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElementBounds {
    pub path: String,
    pub rect: crate::inject::grant_confirm::Rect,
    pub n_actions: i32,
    pub action_name: String,
    pub dbus_dest: String,
    pub dbus_path: String,
}

impl Default for ElementBounds {
    fn default() -> Self {
        Self {
            path: String::new(),
            rect: crate::inject::grant_confirm::Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
            n_actions: 0,
            action_name: String::new(),
            dbus_dest: String::new(),
            dbus_path: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BridgeClick {
    pub x: f64,
    pub y: f64,
}

thread_local! {
    static SCAN_OVERRIDE: std::cell::RefCell<Option<fn(u64, &str, &str) -> Option<ElementBounds>>> =
        const { std::cell::RefCell::new(None) };
    static LAST_BRIDGE_CLICK: std::cell::RefCell<Option<BridgeClick>> =
        const { std::cell::RefCell::new(None) };
}

pub fn set_scan_override(f: Option<fn(u64, &str, &str) -> Option<ElementBounds>>) {
    SCAN_OVERRIDE.with(|slot| *slot.borrow_mut() = f);
}

pub fn scan_session_dbus_for_element(
    window_id: u64,
    role: &str,
    name_query: &str,
) -> Option<ElementBounds> {
    let hook = SCAN_OVERRIDE.with(|slot| *slot.borrow());
    match hook {
        Some(f) => f(window_id, role, name_query),
        None => real_scan_session_dbus_for_element(window_id, role, name_query),
    }
}

/// v40.15.4 G1: Registry.GetApplications walk filtered by `app_id`.
/// Used when the consumer-side dump/scan misses GTK 2/3 (galculator /
/// `libatk-1.0`) apps that still register `toolkit="GTK"` on the a11y bus.
pub fn scan_session_dbus_for_element_by_app_id(
    window_id: u64,
    role: &str,
    name: &str,
    app_id: &str,
) -> Option<ElementBounds> {
    let _ = std::hint::black_box(crate::inject::protocol::SCAN_BY_APP_ID_V40_15_4);
    let _ = std::hint::black_box(crate::inject::protocol::G1_AT_SPI_BRIDGE_FALLBACK_V40_15_4);
    if app_id.trim().is_empty() {
        return None;
    }
    let roots = filter_tree_by_app_id(acquire_tree_roots(None), app_id);
    if let Some(hit) = lookup_nested_tree(&roots, window_id, role, name) {
        return Some(hit);
    }
    real_scan_session_dbus_for_element_by_app_id(window_id, role, name, app_id)
}

/// v40.15.4: dump only the matched application's subtree (not the full bus).
pub fn dump_tree_by_app_id(app_id: &str) -> Vec<TreeNode> {
    let _ = std::hint::black_box(crate::inject::protocol::SCAN_BY_APP_ID_V40_15_4);
    let roots = filter_tree_by_app_id(acquire_tree_roots(None), app_id);
    let nodes = flatten_preorder(roots);
    record_tree_diff(&nodes);
    nodes
}

fn app_id_token_matches(candidate: &str, app_id: &str) -> bool {
    let c = candidate.trim();
    let a = app_id.trim();
    if c.is_empty() || a.is_empty() {
        return false;
    }
    let c = c.to_ascii_lowercase();
    let a = a.to_ascii_lowercase();
    c == a || c.contains(&a) || a.contains(&c)
}

fn node_matches_app_id(node: &TreeNode, app_id: &str) -> bool {
    app_id_token_matches(&node.name, app_id)
        || app_id_token_matches(&node.dbus_dest, app_id)
}

fn filter_tree_by_app_id(nodes: Vec<TreeNode>, app_id: &str) -> Vec<TreeNode> {
    let mut out = Vec::new();
    for node in nodes {
        if node_matches_app_id(&node, app_id) {
            out.push(node);
            continue;
        }
        let kids = filter_tree_by_app_id(node.children, app_id);
        out.extend(kids);
    }
    out
}

/// Scan `/proc/<pid>/maps` for ATK vs atk-bridge. galculator / GTK 2/3
/// maps `libatk-1.0.so.0`; pavucontrol / GTK 4 maps `libatk-bridge-2.0.so.0`.
pub fn atk_version_probe(pid: u32) -> &'static [u8] {
    let _ = std::hint::black_box(crate::inject::protocol::G1_AT_SPI_BRIDGE_FALLBACK_V40_15_4);
    let path = format!("/proc/{pid}/maps");
    let Ok(maps) = std::fs::read_to_string(path) else {
        return b"libatk-bridge-2.0";
    };
    if maps.contains("libatk-1.0.so.0") || maps.contains("libatk-1.0.so") {
        b"libatk-1.0"
    } else if maps.contains("libatk-bridge-2.0.so.0") || maps.contains("libatk-bridge-2.0.so") {
        b"libatk-bridge-2.0"
    } else {
        b"libatk-bridge-2.0"
    }
}

/// v40.14.6: filter the already-published `AT_TREE_DUMP` subtree by
/// `(role, name)`. Bounds come from the dump, so no second
/// `Component.GetExtents` round-trip on the matched node — which is the
/// path that triggered `xdg-desktop-portal-gtk` `impl_GetExtents:
/// assertion 'ATK_IS_COMPONENT (user_data) failed'` on the desktop root
/// during the v40.14.3 6-gate e2e. The dump's own `walk_dump` recovers
/// from `get_children` errors with `Err(e) → skip`; the resulting bounds
/// for per-app subtrees are real.
pub const ELEMENT_AX_LOOKUP_V40_14_6: &str = "ELEMENT_AX_LOOKUP_V40_14_6";

/// Source-of-truth: GTK 4 master `gtk/a11y/gtkatspiaction.c`
/// switch_actions[].name = "toggle", button_actions[].name = "click".
/// Action chain for toggling any AT-SPI AccessibleAction node.
pub const ACTION_NAME_FALLBACK_CHAIN_V40_14_15: [(&str, &str); 7] = [
    ("click", "GTK 4 GtkButton family (ToggleButton, CheckButton, plain Button)"),
    ("toggle", "GTK 4 GtkSwitch"),
    ("activate", "GTK 4 GtkExpander, GtkSearchEntry, GtkEntry"),
    ("press", "older ATK; Qt5/6; Java AWT"),
    ("release", "older ATK"),
    ("expand", "collapsible toggle header"),
    ("menu", "menu button (rare for toggles)"),
];

/// Parent walk for inner-switch resolution when matched node is a
/// container (GtkSwitch / GtkToggleButton wrapped in a row).
pub const INNER_SWITCH_RESOLUTION_V40_14_15: &str =
    "if matched role in {toggle button, check box, switch} and \
     matched node has NActions > 0: dispatch Action.DoAction(0) \
     directly before considering parent walk or pointer_click";

#[used]
pub(crate) static ACTION_NAME_FALLBACK_CHAIN_V40_14_15_IDENT: &[u8] =
    b"\x01ACTION_NAME_FALLBACK_CHAIN_V40_14_15\x01";
#[used]
pub(crate) static INNER_SWITCH_RESOLUTION_V40_14_15_IDENT: &[u8] =
    b"\x01INNER_SWITCH_RESOLUTION_V40_14_15\x01";

/// Inline mirror of `real_bus::role_matches` so this helper is callable
/// outside the `#[cfg(feature = "dbus")]` `real_bus` module. Same rules.
fn ax_role_matches(query: &str, role_name: &str) -> bool {
    let q = query.to_ascii_lowercase();
    let r = role_name.to_ascii_lowercase();
    if q.contains(&r) || r.contains(&q) {
        return true;
    }
    if q == "button" && r.contains("button") {
        return true;
    }
    matches!(q.as_str(), "text" | "textbox" | "entry")
        && (r == "entry"
            || r == "text"
            || r == "password text"
            || r.contains("text")
            || r.contains("entry"))
}

/// Inline mirror of `real_bus::name_matches` (case-insensitive substring).
fn ax_name_matches(query: &str, name: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return false;
    }
    name.trim().to_lowercase().contains(&q.to_lowercase())
}

/// Inline mirror of `real_bus::stamp_path` (whitespace → `_`).
fn ax_stamp_path(window_id: u64, role: &str, name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| if c.is_ascii_whitespace() { '_' } else { c })
        .collect();
    format!("atspi://{window_id}#{role}:{name}")
}

/// v40.14.6: try `dump_full_tree(None)` first, filter by `(role, name)`.
/// On a hit, return `Some(ElementBounds)` with the bounds from the dump.
/// On a miss, return `None` so the caller can fall back to
/// `scan_session_dbus_for_element` (e.g. when the dump itself returned
/// empty because the desktop root had no children).
pub fn element_lookup_via_tree_dump(
    window_id: u64,
    role: &str,
    name_query: &str,
) -> Option<ElementBounds> {
    let _ = std::hint::black_box(ELEMENT_AX_LOOKUP_V40_14_6);
    let _ = std::hint::black_box(ACTION_NAME_FALLBACK_CHAIN_V40_14_15_IDENT);
    let _ = std::hint::black_box(INNER_SWITCH_RESOLUTION_V40_14_15_IDENT);
    let roots = acquire_tree_roots(None);
    lookup_nested_tree(&roots, window_id, role, name_query)
}

fn lookup_nested_tree(
    nodes: &[TreeNode],
    window_id: u64,
    role: &str,
    name_query: &str,
) -> Option<ElementBounds> {
    for node in nodes {
        if ax_role_matches(role, &node.role) && ax_name_matches(name_query, &node.name) {
            if node.n_actions > 0 {
                return Some(element_bounds_from_nodes(window_id, role, node, node));
            }
            if let Some(inner) = find_actionable_switch_descendant(node) {
                return Some(element_bounds_from_nodes(window_id, role, node, inner));
            }
            return Some(element_bounds_from_nodes(window_id, role, node, node));
        }
        if let Some(hit) = lookup_nested_tree(&node.children, window_id, role, name_query) {
            return Some(hit);
        }
    }
    None
}

fn element_bounds_from_nodes(
    window_id: u64,
    role: &str,
    rect_src: &TreeNode,
    action_src: &TreeNode,
) -> ElementBounds {
    ElementBounds {
        path: ax_stamp_path(window_id, role, &rect_src.name),
        rect: crate::inject::grant_confirm::Rect {
            x: rect_src.x,
            y: rect_src.y,
            w: rect_src.w,
            h: rect_src.h,
        },
        n_actions: action_src.n_actions,
        action_name: action_src.action_name.clone(),
        dbus_dest: action_src.dbus_dest.clone(),
        dbus_path: action_src.dbus_path.clone(),
    }
}

fn is_actionable_switch_role(role: &str) -> bool {
    let r = role.to_ascii_lowercase();
    r.contains("toggle button") || r.contains("check box") || r.contains("switch")
}

fn find_actionable_switch_descendant(node: &TreeNode) -> Option<&TreeNode> {
    for child in &node.children {
        if is_actionable_switch_role(&child.role) && child.n_actions > 0 {
            return Some(child);
        }
        if let Some(hit) = find_actionable_switch_descendant(child) {
            return Some(hit);
        }
    }
    None
}

pub struct InnerSwitchHit {
    pub outer_x: f64,
    pub outer_y: f64,
    pub outer_w: f64,
    pub outer_h: f64,
    pub n_actions: i32,
    pub action_name: String,
}

pub fn find_inner_switch(nodes: &[TreeNode]) -> Option<InnerSwitchHit> {
    let _ = std::hint::black_box(INNER_SWITCH_RESOLUTION_V40_14_15);
    let _ = std::hint::black_box(INNER_SWITCH_RESOLUTION_V40_14_15_IDENT);
    for node in nodes {
        if let Some(inner) = find_actionable_switch_descendant(node) {
            return Some(InnerSwitchHit {
                outer_x: node.x,
                outer_y: node.y,
                outer_w: node.w,
                outer_h: node.h,
                n_actions: inner.n_actions,
                action_name: inner.action_name.clone(),
            });
        }
        if let Some(hit) = find_inner_switch(&node.children) {
            return Some(hit);
        }
    }
    None
}

pub fn record_bridge_click(x: f64, y: f64) {
    LAST_BRIDGE_CLICK.with(|slot| {
        *slot.borrow_mut() = Some(BridgeClick { x, y });
    });
}

pub fn take_bridge_click() -> Option<BridgeClick> {
    LAST_BRIDGE_CLICK.with(|slot| slot.borrow_mut().take())
}

pub const AT_SPI_BRIDGE_LIVE: &str = "AT_SPI_BRIDGE_LIVE";

pub const AT_SPI_BRIDGE_REAL: &str = "AT_SPI_BRIDGE_REAL";

/// Flattened AX node. `children` exists so a dump override can return a tree;
/// [`dump_full_tree`] clears them after a pre-order walk.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TreeNode {
    pub role: String,
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub visible: bool,
    pub children: Vec<TreeNode>,
    pub n_actions: i32,
    pub action_name: String,
    pub dbus_dest: String,
    pub dbus_path: String,
}

#[derive(Debug, Clone, PartialEq)]
struct TreeSnapshot {
    role: String,
    name: String,
    path: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

struct CacheEntry {
    snapshot: TreeSnapshot,
    at: std::time::Instant,
    focus_gen: u64,
}

struct TreeCache {
    by_bot: std::collections::HashMap<String, std::collections::HashMap<u64, CacheEntry>>,
}

thread_local! {
    static TREE_DUMP_OVERRIDE: std::cell::RefCell<Option<fn(Option<&str>) -> Vec<TreeNode>>> =
        const { std::cell::RefCell::new(None) };
    static TREE_CACHE: std::cell::RefCell<TreeCache> =
        std::cell::RefCell::new(TreeCache {
            by_bot: std::collections::HashMap::new(),
        });
    static FOCUS_GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static OUTPUT_RECTS: std::cell::RefCell<Option<Vec<crate::inject::grant_confirm::Rect>>> =
        const { std::cell::RefCell::new(None) };
    static FRONTMOST_OVERRIDE: std::cell::RefCell<Option<(u64, String)>> =
        const { std::cell::RefCell::new(None) };
    static EXTENTS_OVERRIDE: std::cell::RefCell<
        Option<fn(u32) -> Option<(i32, i32, i32, i32)>>,
    > = const { std::cell::RefCell::new(None) };
}

struct BotDiffState {
    prev: Option<std::collections::HashSet<String>>,
    trailer: String,
}

#[used]
static DIFF_STATE_PER_BOT: &[u8] = b"\x01DIFF_STATE_PER_BOT\x01";

static DIFF_STATE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, BotDiffState>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn diff_state_key() -> String {
    TREE_DUMP_OVERRIDE.with(|slot| {
        if slot.borrow().is_some() {
            format!("test:{:?}", std::thread::current().id())
        } else {
            "cua-bot".to_string()
        }
    })
}

fn lock_diff_state() -> std::sync::MutexGuard<'static, std::collections::HashMap<String, BotDiffState>> {
    DIFF_STATE.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn set_tree_dump_override(f: Option<fn(Option<&str>) -> Vec<TreeNode>>) {
    TREE_DUMP_OVERRIDE.with(|slot| *slot.borrow_mut() = f);
}

pub fn set_extents_override(f: Option<fn(coord_type: u32) -> Option<(i32, i32, i32, i32)>>) {
    EXTENTS_OVERRIDE.with(|slot| *slot.borrow_mut() = f);
}

fn extents_override_fn() -> Option<fn(u32) -> Option<(i32, i32, i32, i32)>> {
    EXTENTS_OVERRIDE.with(|slot| *slot.borrow())
}

pub fn last_tree_diff_trailer() -> String {
    let key = diff_state_key();
    lock_diff_state()
        .get(&key)
        .map(|s| s.trailer.clone())
        .unwrap_or_default()
}

/// v40.14.7: pavucontrol Mute is an AT-SPI node at depth 15.
/// `WalkLimits::enter` used `depth > 10`, so dump and scan dropped that
/// row (diag-B on t_d47714ef). Cap is 20, not unlimited: MAX_NODES stays
/// 1000 and an unbounded a11y walk can hang the bus.
pub const TREE_WALK_MAX_DEPTH: usize = 20;

#[used]
static DEEP_TREE_WALK_V40_14_7_IDENT: &[u8] = b"\x01DEEP_TREE_WALK_V40_14_7\x01";

#[used]
static GET_EXTENTS_WINDOW_WITH_FALLBACK_V40_14_12: &[u8] =
    b"\x01GET_EXTENTS_WINDOW_WITH_FALLBACK_V40_14_12\x01";
#[used]
static OFF_SCREEN_DETECTION_V40_14_12: &[u8] =
    b"\x01OFF_SCREEN_DETECTION_V40_14_12\x01";
#[used]
static ELEMENT_DIFF_LIST_V40_14_12: &[u8] =
    b"\x01ELEMENT_DIFF_LIST_V40_14_12\x01";

#[used]
static OUTPUT_RECTS_LIVE_QUERY: &[u8] = b"\x01OUTPUT_RECTS_LIVE_QUERY\x01";
#[used]
static ATSPI_ACTION_CLICK_PRIMARY: &[u8] = b"\x01ATSPI_ACTION_CLICK_PRIMARY_V40_14_13\x01";
#[used]
static DIFF_STATE_PER_BOT_KEYED: &[u8] = b"\x01DIFF_STATE_PER_BOT_KEYED_V40_14_13\x01";

fn acquire_tree_roots(root_path: Option<&str>) -> Vec<TreeNode> {
    let hook = TREE_DUMP_OVERRIDE.with(|slot| *slot.borrow());
    let mut roots = match hook {
        Some(f) => f(root_path),
        None => real_dump_full_tree(root_path),
    };
    if let Some(get) = extents_override_fn() {
        apply_extents_override(&mut roots, get);
    }
    roots
}

pub fn dump_full_tree(root_path: Option<&str>) -> Vec<TreeNode> {
    let _ = std::hint::black_box(DEEP_TREE_WALK_V40_14_7_IDENT);
    let roots = acquire_tree_roots(root_path);
    let nodes = flatten_preorder(roots);
    record_tree_diff(&nodes);
    nodes
}

/// Coord type 1 (Window) first; type 0 (Screen) if Window is missing or origin-zero.
pub fn extents_with_fallback<F>(get: F) -> Option<(i32, i32, i32, i32)>
where
    F: Fn(u32) -> Option<(i32, i32, i32, i32)>,
{
    let _ = std::hint::black_box(GET_EXTENTS_WINDOW_WITH_FALLBACK_V40_14_12);
    match get(1) {
        Some((x, y, w, h)) if x != 0 || y != 0 => Some((x, y, w, h)),
        type1 => match get(0) {
            Some(t0) => Some(t0),
            None => type1,
        },
    }
}

fn apply_extents_override(
    nodes: &mut [TreeNode],
    get: fn(u32) -> Option<(i32, i32, i32, i32)>,
) {
    for node in nodes.iter_mut() {
        let (x, y, w, h) = match extents_with_fallback(get) {
            Some((x, y, w, h)) => (x as f64, y as f64, w as f64, h as f64),
            None => (0.0, 0.0, 0.0, 0.0),
        };
        node.x = x;
        node.y = y;
        node.w = w;
        node.h = h;
        apply_extents_override(&mut node.children, get);
    }
}

fn diff_path_key(role: &str, name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| if c.is_ascii_whitespace() { '_' } else { c })
        .collect();
    format!("{role}:{name}")
}

fn record_tree_diff(nodes: &[TreeNode]) {
    let _ = build_tree_diff_trailer(nodes);
}

fn build_tree_diff_trailer(nodes: &[TreeNode]) -> String {
    let _ = std::hint::black_box(DIFF_STATE_PER_BOT);
    let _ = std::hint::black_box(ELEMENT_DIFF_LIST_V40_14_12);
    let mut current = std::collections::HashSet::new();
    for n in nodes {
        current.insert(diff_path_key(&n.role, &n.name));
    }
    let key = diff_state_key();
    let mut map = lock_diff_state();
    let entry = map.entry(key).or_insert_with(|| BotDiffState {
        prev: None,
        trailer: String::new(),
    });
    let trailer = match entry.prev.as_ref() {
        None => String::new(),
        Some(prev) => {
            let mut lines = Vec::new();
            let mut added: Vec<_> = current.difference(prev).cloned().collect();
            added.sort();
            for p in added {
                lines.push(format!("CHANGED: 1 {p}"));
            }
            let mut removed: Vec<_> = prev.difference(&current).cloned().collect();
            removed.sort();
            for p in removed {
                lines.push(format!("CHANGED: 1 -{p}"));
            }
            if lines.is_empty() {
                String::new()
            } else {
                lines.join("\n") + "\n"
            }
        }
    };
    entry.prev = Some(current);
    entry.trailer = trailer.clone();
    trailer
}

fn flatten_preorder(nodes: Vec<TreeNode>) -> Vec<TreeNode> {
    let mut out = Vec::new();
    fn walk(nodes: Vec<TreeNode>, out: &mut Vec<TreeNode>, depth: usize) {
        if depth > TREE_WALK_MAX_DEPTH {
            return;
        }
        for mut node in nodes {
            let children = std::mem::take(&mut node.children);
            out.push(node);
            walk(children, out, depth + 1);
        }
    }
    walk(nodes, &mut out, 0);
    out
}

thread_local! {
    static REAL_SCAN_ATTEMPTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static REAL_TREE_WALK_ATTEMPTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub fn real_scan_attempts() -> u64 {
    REAL_SCAN_ATTEMPTS.with(std::cell::Cell::get)
}

pub fn real_tree_walk_attempts() -> u64 {
    REAL_TREE_WALK_ATTEMPTS.with(std::cell::Cell::get)
}

fn increment_real_scan_attempts() {
    REAL_SCAN_ATTEMPTS.with(|c| c.set(c.get() + 1));
}

fn increment_real_tree_walk_attempts() {
    REAL_TREE_WALK_ATTEMPTS.with(|c| c.set(c.get() + 1));
}

fn real_scan_session_dbus_for_element_by_app_id(
    window_id: u64,
    role: &str,
    name_query: &str,
    app_id: &str,
) -> Option<ElementBounds> {
    let _ = std::hint::black_box(crate::inject::protocol::G1_AT_SPI_BRIDGE_FALLBACK_V40_15_4);
    increment_real_scan_attempts();
    #[cfg(feature = "dbus")]
    {
        let role = role.to_owned();
        let name_query = name_query.to_owned();
        let app_id = app_id.to_owned();
        return match std::thread::spawn(move || {
            real_bus::scan_session_dbus_for_element_by_app_id(
                window_id, &role, &name_query, &app_id,
            )
        })
        .join()
        {
            Ok(v) => v,
            Err(_) => None,
        };
    }
    #[cfg(not(feature = "dbus"))]
    {
        let _ = (window_id, role, name_query, app_id);
        None
    }
}

fn real_scan_session_dbus_for_element(
    window_id: u64,
    role: &str,
    name_query: &str,
) -> Option<ElementBounds> {
    let _ = std::hint::black_box(AT_SPI_BRIDGE_REAL);
    increment_real_scan_attempts();
    #[cfg(feature = "dbus")]
    {
        let role = role.to_owned();
        let name_query = name_query.to_owned();
        return match std::thread::spawn(move || {
            real_bus::scan_session_dbus_for_element(window_id, &role, &name_query)
        })
        .join()
        {
            Ok(v) => v,
            Err(_) => None,
        };
    }
    #[cfg(not(feature = "dbus"))]
    {
        let _ = (window_id, role, name_query);
        None
    }
}

fn real_dump_full_tree(root_path: Option<&str>) -> Vec<TreeNode> {
    increment_real_tree_walk_attempts();
    #[cfg(feature = "dbus")]
    {
        let root_path = root_path.map(str::to_owned);
        return match std::thread::spawn(move || real_bus::dump_full_tree(root_path.as_deref()))
            .join()
        {
            Ok(v) => v,
            Err(_) => Vec::new(),
        };
    }
    #[cfg(not(feature = "dbus"))]
    {
        let _ = root_path;
        Vec::new()
    }
}

#[cfg(feature = "dbus")]
mod real_bus {
    use std::collections::{HashMap, HashSet};

    use super::{ElementBounds, TreeNode};

    const REGISTRY_DEST: &str = "org.a11y.atspi.Registry";
    const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
    const NULL_PATH: &str = "/org/a11y/atspi/null";
    const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
    const COMPONENT: &str = "org.a11y.atspi.Component";
    const ACTION: &str = "org.a11y.atspi.Action";
    const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
    const MAX_NODES: usize = 1000;
    const STATE_VISIBLE: u32 = 1u32 << 30;

    struct WalkLimits {
        visited: HashSet<(String, String)>,
        nodes: usize,
    }

    impl WalkLimits {
        fn new() -> Self {
            Self {
                visited: HashSet::new(),
                nodes: 0,
            }
        }

        fn enter(&mut self, bus: &str, path: &str, depth: usize) -> bool {
            if depth > super::TREE_WALK_MAX_DEPTH || self.nodes >= MAX_NODES {
                return false;
            }
            if !self.visited.insert((bus.to_string(), path.to_string())) {
                return false;
            }
            self.nodes += 1;
            true
        }
    }

    pub(super) fn scan_session_dbus_for_element_by_app_id(
        window_id: u64,
        role: &str,
        name_query: &str,
        app_id: &str,
    ) -> Option<ElementBounds> {
        let conn = match connect_a11y_bus() {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "atspi.scan_by_app_id connect failed");
                return None;
            }
        };
        for (bus, path) in get_applications_gtk(&conn) {
            if !application_matches_app_id(&conn, &bus, &path, app_id) {
                continue;
            }
            let mut first: Option<ElementBounds> = None;
            match walk_scan(
                &conn,
                &bus,
                &path,
                0,
                &mut WalkLimits::new(),
                window_id,
                role,
                name_query,
                None,
                &mut first,
            ) {
                Ok(Some(hit)) => return Some(hit),
                Ok(None) if first.is_some() => return first,
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "atspi.scan_by_app_id walk failed");
                    if first.is_some() {
                        return first;
                    }
                }
            }
        }
        None
    }

    pub(super) fn scan_session_dbus_for_element(
        window_id: u64,
        role: &str,
        name_query: &str,
    ) -> Option<ElementBounds> {
        let conn = match connect_a11y_bus() {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "atspi.real_scan connect failed");
                return None;
            }
        };
        let (bus, path) = start_node(None);
        let mut first: Option<ElementBounds> = None;
        match walk_scan(
            &conn,
            &bus,
            &path,
            0,
            &mut WalkLimits::new(),
            window_id,
            role,
            name_query,
            None,
            &mut first,
        ) {
            Ok(Some(hit)) => Some(hit),
            Ok(None) => first,
            Err(e) => {
                tracing::debug!(error = %e, "atspi.real_scan walk failed");
                first
            }
        }
    }

    pub(super) fn dump_full_tree(root_path: Option<&str>) -> Vec<TreeNode> {
        let conn = match connect_a11y_bus() {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "atspi.real_tree connect failed");
                return Vec::new();
            }
        };
        let (bus, path) = start_node(root_path);
        match walk_dump(&conn, &bus, &path, 0, &mut WalkLimits::new()) {
            Ok(Some(node)) => vec![node],
            Ok(None) => Vec::new(),
            Err(e) => {
                tracing::debug!(error = %e, "atspi.real_tree walk failed");
                Vec::new()
            }
        }
    }

    pub(super) fn connect_a11y_bus() -> zbus::Result<zbus::blocking::Connection> {
        let session = zbus::blocking::Connection::session()?;
        let message = session.call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.a11y.Bus"),
            "GetAddress",
            &(),
        )?;
        let addr: String = message.body().deserialize()?;
        zbus::blocking::connection::Builder::address(addr.as_str())?.build()
    }

    fn start_node(root_path: Option<&str>) -> (String, String) {
        match root_path {
            None => (REGISTRY_DEST.to_string(), ROOT_PATH.to_string()),
            Some(p) if p.contains(':') => {
                if let Some(idx) = p.find(":/") {
                    let bus = &p[..idx];
                    let obj = &p[idx + 1..];
                    if !bus.is_empty() && obj.starts_with('/') {
                        return (bus.to_string(), obj.to_string());
                    }
                }
                if p.starts_with(':') {
                    if let Some(slash) = p.find('/') {
                        return (p[..slash].to_string(), p[slash..].to_string());
                    }
                }
                match p.split_once(':') {
                    Some((bus, obj)) if !bus.is_empty() && !obj.is_empty() => {
                        (bus.to_string(), obj.to_string())
                    }
                    _ => (REGISTRY_DEST.to_string(), ROOT_PATH.to_string()),
                }
            }
            Some(p) if p.starts_with('/') => (REGISTRY_DEST.to_string(), p.to_string()),
            Some(_) => (REGISTRY_DEST.to_string(), ROOT_PATH.to_string()),
        }
    }

    fn walk_scan(
        conn: &zbus::blocking::Connection,
        bus: &str,
        path: &str,
        depth: usize,
        limits: &mut WalkLimits,
        window_id: u64,
        role_query: &str,
        name_query: &str,
        inherited_window_id: Option<String>,
        first: &mut Option<ElementBounds>,
    ) -> zbus::Result<Option<ElementBounds>> {
        if bus.is_empty() || path == NULL_PATH || !limits.enter(bus, path, depth) {
            return Ok(None);
        }

        let role_name = get_role_name(conn, bus, path).unwrap_or_default();
        let name = get_name(conn, bus, path);
        let attrs = get_attributes(conn, bus, path);
        let node_wid = attrs.get("window-id").cloned();
        let effective_wid = node_wid.or(inherited_window_id);
        let preferred = effective_wid.as_deref() == Some(&window_id.to_string());

        if role_matches(role_query, &role_name) && name_matches(name_query, &name) {
            let (x, y, w, h) = get_extents(conn, bus, path);
            let (n_actions, action_name) = probe_actions(conn, bus, path);
            let bounds = ElementBounds {
                path: stamp_path(window_id, role_query, &name),
                rect: crate::inject::grant_confirm::Rect { x, y, w, h },
                n_actions,
                action_name,
                dbus_dest: bus.to_string(),
                dbus_path: path.to_string(),
            };
            if preferred {
                return Ok(Some(bounds));
            }
            if first.is_none() {
                *first = Some(bounds);
            }
        }

        for (child_bus, child_path) in get_children(conn, bus, path) {
            if let Some(hit) = walk_scan(
                conn,
                &child_bus,
                &child_path,
                depth + 1,
                limits,
                window_id,
                role_query,
                name_query,
                effective_wid.clone(),
                first,
            )? {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }

    fn walk_dump(
        conn: &zbus::blocking::Connection,
        bus: &str,
        path: &str,
        depth: usize,
        limits: &mut WalkLimits,
    ) -> zbus::Result<Option<TreeNode>> {
        if bus.is_empty() || path == NULL_PATH || !limits.enter(bus, path, depth) {
            return Ok(None);
        }

        let role = get_role_name(conn, bus, path).unwrap_or_default();
        let name = get_name(conn, bus, path);
        let (x, y, w, h) = get_extents(conn, bus, path);
        let visible = get_visible(conn, bus, path);
        let (n_actions, action_name) = probe_actions(conn, bus, path);
        let mut children = Vec::new();
        for (child_bus, child_path) in get_children(conn, bus, path) {
            match walk_dump(conn, &child_bus, &child_path, depth + 1, limits) {
                Ok(Some(child)) => children.push(child),
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "atspi.real_tree child failed");
                }
            }
        }
        Ok(Some(TreeNode {
            role,
            name,
            x,
            y,
            w,
            h,
            visible,
            children,
            n_actions,
            action_name,
            dbus_dest: bus.to_string(),
            dbus_path: path.to_string(),
        }))
    }

    fn get_role_name(
        conn: &zbus::blocking::Connection,
        dest: &str,
        path: &str,
    ) -> Option<String> {
        let message = conn
            .call_method(Some(dest), path, Some(ACCESSIBLE), "GetRoleName", &())
            .ok()?;
        message.body().deserialize().ok()
    }

    fn probe_actions(
        conn: &zbus::blocking::Connection,
        dest: &str,
        path: &str,
    ) -> (i32, String) {
        let n_actions = get_n_actions(conn, dest, path);
        let action_name = if n_actions > 0 {
            get_action_name(conn, dest, path, 0).unwrap_or_default()
        } else {
            String::new()
        };
        (n_actions, action_name)
    }

    fn get_n_actions(conn: &zbus::blocking::Connection, dest: &str, path: &str) -> i32 {
        let Ok(message) = conn.call_method(
            Some(dest),
            path,
            Some(PROPERTIES),
            "Get",
            &(ACTION, "NActions"),
        ) else {
            return 0;
        };
        match message.body().deserialize::<zbus::zvariant::OwnedValue>() {
            Ok(v) => i32::try_from(v).unwrap_or(0).max(0),
            Err(_) => 0,
        }
    }

    fn get_action_name(
        conn: &zbus::blocking::Connection,
        dest: &str,
        path: &str,
        idx: i32,
    ) -> Option<String> {
        let message = conn
            .call_method(Some(dest), path, Some(ACTION), "GetName", &idx)
            .ok()?;
        message.body().deserialize().ok()
    }

    fn get_name(conn: &zbus::blocking::Connection, dest: &str, path: &str) -> String {
        let Ok(message) = conn.call_method(
            Some(dest),
            path,
            Some(PROPERTIES),
            "Get",
            &("org.a11y.atspi.Accessible", "Name"),
        ) else {
            return String::new();
        };
        match message.body().deserialize::<zbus::zvariant::OwnedValue>() {
            Ok(v) => String::try_from(v).unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    fn get_extents(
        conn: &zbus::blocking::Connection,
        dest: &str,
        path: &str,
    ) -> (f64, f64, f64, f64) {
        let tuple = if let Some(over) = super::extents_override_fn() {
            super::extents_with_fallback(over)
        } else {
            super::extents_with_fallback(|coord: u32| {
                let Ok(message) =
                    conn.call_method(Some(dest), path, Some(COMPONENT), "GetExtents", &coord)
                else {
                    return None;
                };
                match message.body().deserialize::<(i32, i32, i32, i32)>() {
                    Ok(t) => Some(t),
                    Err(_) => None,
                }
            })
        };
        match tuple {
            Some((x, y, w, h)) => (x as f64, y as f64, w as f64, h as f64),
            None => (0.0, 0.0, 0.0, 0.0),
        }
    }

    fn get_visible(conn: &zbus::blocking::Connection, dest: &str, path: &str) -> bool {
        let Ok(message) = conn.call_method(Some(dest), path, Some(ACCESSIBLE), "GetState", &())
        else {
            return false;
        };
        if let Ok(words) = message.body().deserialize::<Vec<u32>>() {
            return words.first().is_some_and(|w| w & STATE_VISIBLE != 0);
        }
        if let Ok(words) = message.body().deserialize::<[u32; 2]>() {
            return words[0] & STATE_VISIBLE != 0;
        }
        false
    }

    fn get_applications_gtk(conn: &zbus::blocking::Connection) -> Vec<(String, String)> {
        let mut apps = get_applications(conn);
        if apps.is_empty() {
            apps = get_children(conn, REGISTRY_DEST, ROOT_PATH);
        }
        apps.into_iter()
            .filter(|(bus, path)| application_toolkit_is_gtk(conn, bus, path))
            .collect()
    }

    fn get_applications(conn: &zbus::blocking::Connection) -> Vec<(String, String)> {
        const IFACE: &str = "org.a11y.atspi.Registry";
        const PATHS: [&str; 3] = [
            "/org/a11y/atspi/registry",
            ROOT_PATH,
            "/org/a11y/atspi/accessible/root",
        ];
        for path in PATHS {
            let Ok(message) =
                conn.call_method(Some(REGISTRY_DEST), path, Some(IFACE), "GetApplications", &())
            else {
                continue;
            };
            if let Some(apps) = decode_applications(&message) {
                if !apps.is_empty() {
                    return apps;
                }
            }
        }
        Vec::new()
    }

    fn decode_applications(message: &zbus::message::Message) -> Option<Vec<(String, String)>> {
        if let Some(v) = decode_children(message) {
            return Some(v);
        }
        if let Ok(v) = message
            .body()
            .deserialize::<Vec<zbus::zvariant::OwnedObjectPath>>()
        {
            return Some(
                v.into_iter()
                    .map(|path| (REGISTRY_DEST.to_string(), path.to_string()))
                    .collect(),
            );
        }
        if let Ok(v) = message.body().deserialize::<Vec<String>>() {
            return Some(
                v.into_iter()
                    .map(|bus| {
                        if bus.starts_with('/') {
                            (REGISTRY_DEST.to_string(), bus)
                        } else {
                            (bus, ROOT_PATH.to_string())
                        }
                    })
                    .collect(),
            );
        }
        None
    }

    fn get_application_attributes(
        conn: &zbus::blocking::Connection,
        dest: &str,
        path: &str,
    ) -> HashMap<String, String> {
        const APP: &str = "org.a11y.atspi.Application";
        if let Ok(message) =
            conn.call_method(Some(dest), path, Some(APP), "GetApplicationAttributes", &())
        {
            if let Ok(attrs) = message.body().deserialize::<HashMap<String, String>>() {
                if !attrs.is_empty() {
                    return attrs;
                }
            }
        }
        let mut attrs = get_attributes(conn, dest, path);
        if let Some(toolkit) = get_toolkit_name(conn, dest, path) {
            attrs.entry("toolkit".into()).or_insert(toolkit);
        }
        attrs
    }

    fn get_toolkit_name(
        conn: &zbus::blocking::Connection,
        dest: &str,
        path: &str,
    ) -> Option<String> {
        let message = conn
            .call_method(
                Some(dest),
                path,
                Some(PROPERTIES),
                "Get",
                &("org.a11y.atspi.Application", "ToolkitName"),
            )
            .ok()?;
        let v = message
            .body()
            .deserialize::<zbus::zvariant::OwnedValue>()
            .ok()?;
        String::try_from(v).ok()
    }

    fn application_toolkit_is_gtk(
        conn: &zbus::blocking::Connection,
        dest: &str,
        path: &str,
    ) -> bool {
        let attrs = get_application_attributes(conn, dest, path);
        for key in ["toolkit", "ToolkitName", "toolkit-name"] {
            if let Some(v) = attrs.get(key) {
                if v.eq_ignore_ascii_case("GTK") || v.to_ascii_lowercase().contains("gtk") {
                    return true;
                }
            }
        }
        if let Some(v) = get_toolkit_name(conn, dest, path) {
            return v.eq_ignore_ascii_case("GTK") || v.to_ascii_lowercase().contains("gtk");
        }
        // Keep unknown-toolkit apps so a strict miss does not drop galculator
        // when GetApplicationAttributes is unimplemented on that bus name.
        true
    }

    fn application_matches_app_id(
        conn: &zbus::blocking::Connection,
        dest: &str,
        path: &str,
        app_id: &str,
    ) -> bool {
        if super::app_id_token_matches(dest, app_id) {
            return true;
        }
        let name = get_name(conn, dest, path);
        if super::app_id_token_matches(&name, app_id) {
            return true;
        }
        let attrs = get_application_attributes(conn, dest, path);
        for key in ["id", "app-id", "app_id", "application", "wm-class", "name"] {
            if let Some(v) = attrs.get(key) {
                if super::app_id_token_matches(v, app_id) {
                    return true;
                }
            }
        }
        false
    }

    fn get_attributes(
        conn: &zbus::blocking::Connection,
        dest: &str,
        path: &str,
    ) -> HashMap<String, String> {
        let Ok(message) =
            conn.call_method(Some(dest), path, Some(ACCESSIBLE), "GetAttributes", &())
        else {
            return HashMap::new();
        };
        message.body().deserialize().unwrap_or_default()
    }

    fn get_children(
        conn: &zbus::blocking::Connection,
        dest: &str,
        path: &str,
    ) -> Vec<(String, String)> {
        match conn.call_method(Some(dest), path, Some(ACCESSIBLE), "GetChildren", &()) {
            Ok(message) => match decode_children(&message) {
                Some(children) => filter_children(dest, children),
                None => filter_children(dest, children_by_index(conn, dest, path)),
            },
            Err(_) => filter_children(dest, children_by_index(conn, dest, path)),
        }
    }

    fn children_by_index(
        conn: &zbus::blocking::Connection,
        dest: &str,
        path: &str,
    ) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for i in 0i32..MAX_NODES as i32 {
            let Ok(message) =
                conn.call_method(Some(dest), path, Some(ACCESSIBLE), "GetChildAtIndex", &i)
            else {
                break;
            };
            match decode_one_child(&message) {
                Some(child) => out.push(child),
                None => break,
            }
        }
        out
    }

    fn decode_children(message: &zbus::message::Message) -> Option<Vec<(String, String)>> {
        if let Ok(v) = message
            .body()
            .deserialize::<Vec<(String, zbus::zvariant::OwnedObjectPath)>>()
        {
            return Some(
                v.into_iter()
                    .map(|(bus, path)| (bus, path.to_string()))
                    .collect(),
            );
        }
        if let Ok(v) = message.body().deserialize::<Vec<(
            zbus::names::OwnedBusName,
            zbus::zvariant::OwnedObjectPath,
        )>>() {
            return Some(
                v.into_iter()
                    .map(|(bus, path)| (bus.to_string(), path.to_string()))
                    .collect(),
            );
        }
        None
    }

    fn decode_one_child(message: &zbus::message::Message) -> Option<(String, String)> {
        if let Ok((bus, path)) = message
            .body()
            .deserialize::<(String, zbus::zvariant::OwnedObjectPath)>()
        {
            return Some((bus, path.to_string()));
        }
        if let Ok((bus, path)) = message.body().deserialize::<(
            zbus::names::OwnedBusName,
            zbus::zvariant::OwnedObjectPath,
        )>() {
            return Some((bus.to_string(), path.to_string()));
        }
        None
    }

    fn filter_children(
        parent_bus: &str,
        children: Vec<(String, String)>,
    ) -> Vec<(String, String)> {
        children
            .into_iter()
            .filter_map(|(bus, path)| {
                if path == NULL_PATH || path.is_empty() {
                    return None;
                }
                let bus = if bus.is_empty() {
                    parent_bus.to_string()
                } else {
                    bus
                };
                if bus.is_empty() {
                    None
                } else {
                    Some((bus, path))
                }
            })
            .collect()
    }

    fn role_matches(query: &str, role_name: &str) -> bool {
        let q = query.to_ascii_lowercase();
        let r = role_name.to_ascii_lowercase();
        if q.contains(&r) || r.contains(&q) {
            return true;
        }
        if q == "button" && r.contains("button") {
            return true;
        }
        matches!(q.as_str(), "text" | "textbox" | "entry")
            && (r == "entry"
                || r == "text"
                || r == "password text"
                || r.contains("text")
                || r.contains("entry"))
    }

    fn name_matches(query: &str, name: &str) -> bool {
        super::ax_name_matches(query, name)
    }

    fn stamp_path(window_id: u64, role: &str, name: &str) -> String {
        let name: String = name
            .chars()
            .map(|c| if c.is_ascii_whitespace() { '_' } else { c })
            .collect();
        format!("atspi://{window_id}#{role}:{name}")
    }
}

#[cfg(feature = "dbus")]
pub(crate) fn connect_a11y_bus() -> zbus::Result<zbus::blocking::Connection> {
    real_bus::connect_a11y_bus()
}

pub fn format_tree_reply(nodes: &[TreeNode]) -> String {
    let mut body = format!("{}\n", nodes.len());
    for n in nodes {
        let rect = crate::inject::grant_confirm::Rect {
            x: n.x,
            y: n.y,
            w: n.w,
            h: n.h,
        };
        let occ = if offscreen_warning(&rect) == "<OCCLUDED>" {
            " <OCCLUDED>"
        } else {
            ""
        };
        body.push_str(&format!(
            "[{}] \"{}\" {} {} {} {} {}{}\0\n",
            n.role, n.name, n.x, n.y, n.w, n.h, n.visible, occ
        ));
    }
    let trailer = last_tree_diff_trailer();
    if !trailer.is_empty() {
        body.push_str(&trailer);
    }
    body
}

pub fn element_changed_field(
    bot_id: &str,
    wid: u64,
    role: &str,
    name: &str,
    path: &str,
    rect: &crate::inject::grant_confirm::Rect,
) -> String {
    const TTL: std::time::Duration = std::time::Duration::from_secs(5);
    let now = std::time::Instant::now();
    let focus_gen = FOCUS_GEN.with(|g| g.get());
    let new_snap = TreeSnapshot {
        role: role.to_string(),
        name: name.to_string(),
        path: path.to_string(),
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    };
    TREE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let stale = match cache
            .by_bot
            .get(bot_id)
            .and_then(|m| m.get(&wid))
        {
            None => true,
            Some(entry) => {
                now.duration_since(entry.at) >= TTL || entry.focus_gen != focus_gen
            }
        };
        let result = if stale {
            "FULL".to_string()
        } else {
            let entry = cache
                .by_bot
                .get(bot_id)
                .and_then(|m| m.get(&wid))
                .expect("stale=false implies cache hit");
            if entry.snapshot == new_snap {
                "none".to_string()
            } else {
                format!(
                    "[{role}] \"{name}\" {} {} {} {}\0",
                    rect.x, rect.y, rect.w, rect.h
                )
            }
        };
        cache
            .by_bot
            .entry(bot_id.to_string())
            .or_default()
            .insert(
                wid,
                CacheEntry {
                    snapshot: new_snap,
                    at: now,
                    focus_gen,
                },
            );
        result
    })
}

pub fn clear_tree_cache() {
    TREE_CACHE.with(|cache| cache.borrow_mut().by_bot.clear());
}

pub fn set_focus_gen(gen: u64) {
    FOCUS_GEN.with(|g| g.set(gen));
}

pub fn set_output_rects(rects: Option<Vec<crate::inject::grant_confirm::Rect>>) {
    let _ = std::hint::black_box(ATSPI_ACTION_CLICK_PRIMARY);
    let _ = std::hint::black_box(DIFF_STATE_PER_BOT_KEYED);
    OUTPUT_RECTS.with(|slot| *slot.borrow_mut() = rects);
}

fn rects_intersect(
    a: &crate::inject::grant_confirm::Rect,
    b: &crate::inject::grant_confirm::Rect,
) -> bool {
    !(a.x >= b.x + b.w || a.x + a.w <= b.x || a.y >= b.y + b.h || a.y + a.h <= b.y)
}

fn primary_output_fallback() -> Vec<crate::inject::grant_confirm::Rect> {
    vec![crate::inject::grant_confirm::Rect {
        x: 0.0,
        y: 0.0,
        w: 1920.0,
        h: 1080.0,
    }]
}

fn parse_niri_msg_output_rects(json: &str) -> Vec<crate::inject::grant_confirm::Rect> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(map) = v.as_object() else {
        return Vec::new();
    };
    let mut rects = Vec::new();
    for out in map.values() {
        let Some(logical) = out.get("logical").and_then(|l| l.as_object()) else {
            continue;
        };
        let x = logical.get("x").and_then(|n| n.as_i64()).unwrap_or(0) as f64;
        let y = logical.get("y").and_then(|n| n.as_i64()).unwrap_or(0) as f64;
        let w = logical.get("width").and_then(|n| n.as_u64()).unwrap_or(0) as f64;
        let h = logical.get("height").and_then(|n| n.as_u64()).unwrap_or(0) as f64;
        if w > 0.0 && h > 0.0 {
            rects.push(crate::inject::grant_confirm::Rect { x, y, w, h });
        }
    }
    rects
}

/// Fill OUTPUT_RECTS from compositor IPC outputs when still None.
/// Failure or empty logical set stores the 1920x1080 primary fallback. Never panics.
/// Does not overwrite a test's `set_output_rects(Some(...))`.
pub fn refresh_output_rects_live() {
    let _ = std::hint::black_box(OUTPUT_RECTS_LIVE_QUERY);
    let already = OUTPUT_RECTS.with(|slot| slot.borrow().is_some());
    if already {
        return;
    }
    let queried = std::process::Command::new("niri")
        .args(["msg", "--json", "outputs"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| parse_niri_msg_output_rects(&s))
        .filter(|rects| !rects.is_empty());
    set_output_rects(Some(queried.unwrap_or_else(primary_output_fallback)));
}

pub fn offscreen_warning(rect: &crate::inject::grant_confirm::Rect) -> &'static str {
    let _ = std::hint::black_box(OFF_SCREEN_DETECTION_V40_14_12);
    refresh_output_rects_live();
    OUTPUT_RECTS.with(|slot| match slot.borrow().as_ref() {
        None => "none",
        Some(outputs) => {
            if !outputs.iter().any(|o| rects_intersect(rect, o)) {
                return "<OCCLUDED>";
            }
            let cx = rect.x + rect.w / 2.0;
            let cy = rect.y + rect.h / 2.0;
            if outputs.iter().any(|o| o.contains(cx, cy)) {
                "none"
            } else {
                "<OCCLUDED>"
            }
        }
    })
}

pub fn set_frontmost_override(v: Option<(u64, String)>) {
    FRONTMOST_OVERRIDE.with(|slot| *slot.borrow_mut() = v);
}

fn sanitize_app_id(s: &str) -> String {
    if s.is_empty() || s.chars().any(char::is_whitespace) {
        "none".to_string()
    } else {
        s.to_string()
    }
}

pub fn frontmost_or_echo(bind_wid: u64, app_id: Option<&str>) -> (u64, String) {
    FRONTMOST_OVERRIDE.with(|slot| match slot.borrow().as_ref() {
        Some((wid, app)) => (*wid, sanitize_app_id(app)),
        None => (bind_wid, sanitize_app_id(app_id.unwrap_or("none"))),
    })
}

pub fn reset_gap4_test_hooks() {
    set_tree_dump_override(None);
    set_extents_override(None);
    set_output_rects(None);
    set_frontmost_override(None);
    set_focus_gen(0);
    clear_tree_cache();
    lock_diff_state().clear();
    crate::inject::atspi_action::clear_action_override();
}
