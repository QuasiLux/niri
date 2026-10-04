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
    let hook = TREE_DUMP_OVERRIDE.with(|slot| *slot.borrow());
    let roots = match hook {
        Some(f) => f(root_path),
        None => real_dump_full_tree(root_path),
    };
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
    const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
    const MAX_DEPTH: usize = 10;
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
            if depth > MAX_DEPTH || self.nodes >= MAX_NODES {
                return false;
            }
            if !self.visited.insert((bus.to_string(), path.to_string())) {
                return false;
            }
            self.nodes += 1;
            true
        }
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

    fn connect_a11y_bus() -> zbus::Result<zbus::blocking::Connection> {
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
            let bounds = ElementBounds {
                path: stamp_path(window_id, role_query, &name),
                rect: crate::inject::grant_confirm::Rect { x, y, w, h },
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
        let Ok(message) =
            conn.call_method(Some(dest), path, Some(COMPONENT), "GetExtents", &0u32)
        else {
            return (0.0, 0.0, 0.0, 0.0);
        };
        match message.body().deserialize::<(i32, i32, i32, i32)>() {
            Ok((x, y, w, h)) => (x as f64, y as f64, w as f64, h as f64),
            Err(_) => (0.0, 0.0, 0.0, 0.0),
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
        query.trim() == name.trim()
    }

    fn stamp_path(window_id: u64, role: &str, name: &str) -> String {
        let name: String = name
            .chars()
            .map(|c| if c.is_ascii_whitespace() { '_' } else { c })
            .collect();
        format!("atspi://{window_id}#{role}:{name}")
    }
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
