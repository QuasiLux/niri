//! v40.21 widget tree under a layer-shell hit.
//!
//! Pure. No dbus. The inject socket converts AT-SPI nodes into [`WidgetNode`]
//! before calling [`widget_tree_from_hit`].

use crate::input::layer_hit_test::{hit_test, LayerHitReply, LayerHitSurface};

/// cua-driver `LAYER_WIDGET_TREE_MAX_WIDGETS`.
pub const MAX_WIDGETS: usize = 32;
/// cua-driver `LAYER_WIDGET_TREE_MAX_DEPTH`. Root depth is 1.
pub const MAX_DEPTH: usize = 4;

/// One widget under a layer hit. `bounds` is `(x, y, width, height)`.
/// `parent_index` is the index in the `widgets` vec, or `None` for a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerWidget {
    pub name: String,
    pub role: String,
    pub bounds: (i32, i32, i32, i32),
    pub parent_index: Option<usize>,
}

/// Structured layer-widget-tree reply. `layer` is the v40.18 hit `name`
/// (full namespace), not the role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerWidgetTreeReply {
    pub found: bool,
    pub app_id: String,
    pub layer: String,
    pub widgets: Vec<LayerWidget>,
}

impl LayerWidgetTreeReply {
    pub fn miss() -> Self {
        Self {
            found: false,
            app_id: String::new(),
            layer: String::new(),
            widgets: Vec::new(),
        }
    }
}

/// Accessible node already translated to screen pixels. Children stay intact
/// so `parent_index` can be rebuilt. Do not import `crate::inject` here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidgetNode {
    pub name: String,
    pub role: String,
    pub bounds: (i32, i32, i32, i32),
    pub children: Vec<WidgetNode>,
}

fn token_is_single(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(char::is_whitespace)
}

fn sanitize_role(role: &str) -> String {
    role.chars()
        .map(|c| if c == '[' || c == ']' { '_' } else { c })
        .collect()
}

fn sanitize_name(name: &str) -> String {
    name.chars().map(|c| if c == '"' { '_' } else { c }).collect()
}

fn contains(bounds: (i32, i32, i32, i32), px: i32, py: i32) -> bool {
    let (x, y, w, h) = bounds;
    if w <= 0 || h <= 0 {
        return false;
    }
    px >= x && py >= y && px < x.saturating_add(w) && py < y.saturating_add(h)
}

fn synthetic(hit: &LayerHitReply) -> LayerWidget {
    LayerWidget {
        name: sanitize_name(&hit.name),
        role: sanitize_role(&hit.role),
        bounds: hit.bounds,
        parent_index: None,
    }
}

/// Hit-test `surfaces` (v40.18 rules), then the widget walk.
pub fn widget_tree_from_surfaces(
    surfaces: &[LayerHitSurface],
    roots: &[WidgetNode],
    x: i32,
    y: i32,
) -> LayerWidgetTreeReply {
    widget_tree_from_hit(&hit_test(surfaces, x, y), roots, x, y)
}

/// `layer` is `hit.name` (full namespace). Empty roots, or no node containing
/// `(x, y)`, yield one synthetic root from the layer hit. A miss stays a miss.
pub fn widget_tree_from_hit(
    hit: &LayerHitReply,
    roots: &[WidgetNode],
    x: i32,
    y: i32,
) -> LayerWidgetTreeReply {
    if !hit.found || !token_is_single(&hit.app_id) || !token_is_single(&hit.name) {
        return LayerWidgetTreeReply::miss();
    }
    let widgets = widgets_at_point(roots, x, y);
    let widgets = if widgets.is_empty() {
        vec![synthetic(hit)]
    } else {
        widgets
    };
    LayerWidgetTreeReply {
        found: true,
        app_id: hit.app_id.clone(),
        layer: hit.name.clone(),
        widgets,
    }
}

fn widgets_at_point(roots: &[WidgetNode], x: i32, y: i32) -> Vec<LayerWidget> {
    let mut path = Vec::new();
    let mut best: Option<Vec<&WidgetNode>> = None;
    find_deepest(roots, x, y, 1, &mut path, &mut best);
    let Some(path) = best else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, node) in path.iter().enumerate() {
        if out.len() >= MAX_WIDGETS {
            break;
        }
        out.push(LayerWidget {
            name: sanitize_name(&node.name),
            role: sanitize_role(&node.role),
            bounds: node.bounds,
            parent_index: if i == 0 { None } else { Some(i - 1) },
        });
    }
    if let Some(hit) = path.last() {
        let hit_index = out.len() - 1;
        let hit_depth = path.len();
        emit_descendants(&hit.children, hit_index, hit_depth + 1, &mut out);
    }
    out
}

fn find_deepest<'a>(
    nodes: &'a [WidgetNode],
    x: i32,
    y: i32,
    depth: usize,
    path: &mut Vec<&'a WidgetNode>,
    best: &mut Option<Vec<&'a WidgetNode>>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for node in nodes {
        path.push(node);
        if contains(node.bounds, x, y) {
            let replace = match best {
                None => true,
                Some(prev) => path.len() > prev.len(),
            };
            if replace {
                *best = Some(path.clone());
            }
        }
        find_deepest(&node.children, x, y, depth + 1, path, best);
        path.pop();
    }
}

fn emit_descendants(
    nodes: &[WidgetNode],
    parent_index: usize,
    depth: usize,
    out: &mut Vec<LayerWidget>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for node in nodes {
        if out.len() >= MAX_WIDGETS {
            return;
        }
        let my_index = out.len();
        out.push(LayerWidget {
            name: sanitize_name(&node.name),
            role: sanitize_role(&node.role),
            bounds: node.bounds,
            parent_index: Some(parent_index),
        });
        emit_descendants(&node.children, my_index, depth + 1, out);
    }
}
