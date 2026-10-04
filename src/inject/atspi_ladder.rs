//! Row 13: per-action AT-SPI routing ladder.
//!
//! Pure logic module — no Smithay, no D-Bus connection here. Reads the
//! Row-12 catalog via `app_id_routes`, returns one of three variants.

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
    SCAN_OVERRIDE.with(|slot| match *slot.borrow() {
        Some(f) => f(window_id, role, name_query),
        None => None,
    })
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



/// Flattened AX node. `children` exists so a dump override can return a tree;
/// [`dump_full_tree`] clears them after a pre-order walk.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeNode {
    pub role: String,
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub visible: bool,
    pub children: Vec<TreeNode>,
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
}

pub fn set_tree_dump_override(f: Option<fn(Option<&str>) -> Vec<TreeNode>>) {
    TREE_DUMP_OVERRIDE.with(|slot| *slot.borrow_mut() = f);
}

pub fn dump_full_tree(root_path: Option<&str>) -> Vec<TreeNode> {
    let roots = TREE_DUMP_OVERRIDE.with(|slot| match *slot.borrow() {
        Some(f) => f(root_path),
        None => Vec::new(),
    });
    flatten_preorder(roots)
}

fn flatten_preorder(nodes: Vec<TreeNode>) -> Vec<TreeNode> {
    let mut out = Vec::new();
    fn walk(nodes: Vec<TreeNode>, out: &mut Vec<TreeNode>) {
        for mut node in nodes {
            let children = std::mem::take(&mut node.children);
            out.push(node);
            walk(children, out);
        }
    }
    walk(nodes, &mut out);
    out
}

pub fn format_tree_reply(nodes: &[TreeNode]) -> String {
    let mut body = format!("{}\n", nodes.len());
    for n in nodes {
        body.push_str(&format!(
            "[{}] \"{}\" {} {} {} {} {}\0\n",
            n.role, n.name, n.x, n.y, n.w, n.h, n.visible
        ));
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
    OUTPUT_RECTS.with(|slot| *slot.borrow_mut() = rects);
}

fn rects_intersect(
    a: &crate::inject::grant_confirm::Rect,
    b: &crate::inject::grant_confirm::Rect,
) -> bool {
    !(a.x >= b.x + b.w || a.x + a.w <= b.x || a.y >= b.y + b.h || a.y + a.h <= b.y)
}

pub fn offscreen_warning(rect: &crate::inject::grant_confirm::Rect) -> &'static str {
    OUTPUT_RECTS.with(|slot| match slot.borrow().as_ref() {
        None => "none",
        Some(outputs) => {
            if !outputs.iter().any(|o| rects_intersect(rect, o)) {
                return "<OFFSCREEN>";
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
    set_output_rects(None);
    set_frontmost_override(None);
    set_focus_gen(0);
    clear_tree_cache();
}
