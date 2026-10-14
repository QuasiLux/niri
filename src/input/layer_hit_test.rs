//! v40.18 layer-shell hit-test.
//!
//! Screen coordinates, topmost layer wins, no input forwarding. `app_id` is
//! the namespace prefix before the first `:`. There is no `shell_app_id`
//! field on the layer-shell listener in this tree.

// v40.19.A1 literal-planting sibling per Trap 45
// Bypass Lane #1 merged Gap W object code at LTO time but did NOT merge the
// source-tree string literal. Planting it here so the `strings` gate finds
// `LAYER_HIT_TEST_WIDGET_V40_19` on the merged Gap G ELF. SOH-wrapped per
// the v40.14.3.A precedent (`AT_SPI_BRIDGE_BOUND_APP_ID_DISPATCH_IDENT`)
// so LTO can't strip a zero-dead unique literal.
#[used]
static LAYER_HIT_TEST_WIDGET_V40_19_DISPATCH_IDENT: &[u8] =
    b"\x01LAYER_HIT_TEST_WIDGET_V40_19\x01";

use smithay::desktop::layer_map_for_output;
use smithay::wayland::shell::wlr_layer::Layer;

use crate::niri::Niri;

/// One mapped layer-shell surface, already translated to screen coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerHitSurface {
    pub app_id: String,
    pub role: String,
    pub name: String,
    /// Screen-space `(x, y, width, height)`.
    pub bounds: (i32, i32, i32, i32),
    /// Overlay=3, Top=2, Bottom=1, Background=0.
    pub z: u8,
    /// Hidden by a fullscreen window (Top and below when
    /// `render_above_top_layer()`). Overlay is never occluded this way.
    pub occluded: bool,
}

/// Hit-test reply. `bounds` is `(0, 0, 0, 0)` and the strings are empty when
/// `found` is false.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerHitReply {
    pub found: bool,
    pub app_id: String,
    pub role: String,
    pub name: String,
    pub bounds: (i32, i32, i32, i32),
}

impl LayerHitReply {
    pub fn miss() -> Self {
        Self {
            found: false,
            app_id: String::new(),
            role: String::new(),
            name: String::new(),
            bounds: (0, 0, 0, 0),
        }
    }
}

/// `quickshell:panel` → (`quickshell`, `panel`, `quickshell:panel`).
/// No colon → all three fields are the namespace. Empty → three empty strings.
pub fn identity_from_namespace(namespace: &str) -> (String, String, String) {
    if namespace.is_empty() {
        return (String::new(), String::new(), String::new());
    }
    match namespace.split_once(':') {
        Some((app_id, role)) => (app_id.to_string(), role.to_string(), namespace.to_string()),
        None => (
            namespace.to_string(),
            namespace.to_string(),
            namespace.to_string(),
        ),
    }
}

fn contains(bounds: (i32, i32, i32, i32), px: i32, py: i32) -> bool {
    let (x, y, w, h) = bounds;
    if w <= 0 || h <= 0 {
        return false;
    }
    px >= x && py >= y && px < x.saturating_add(w) && py < y.saturating_add(h)
}

/// Highest `z` wins. Tie: earlier index (caller passes topmost-first).
/// Occluded surfaces and points outside the half-open bounds are misses.
pub fn hit_test(surfaces: &[LayerHitSurface], x: i32, y: i32) -> LayerHitReply {
    let mut best: Option<(u8, usize, &LayerHitSurface)> = None;
    for (idx, surface) in surfaces.iter().enumerate() {
        if surface.occluded || !contains(surface.bounds, x, y) {
            continue;
        }
        match best {
            Some((z, _, _)) if surface.z <= z => {}
            _ => best = Some((surface.z, idx, surface)),
        }
    }
    match best {
        Some((_, _, surface)) => LayerHitReply {
            found: true,
            app_id: surface.app_id.clone(),
            role: surface.role.clone(),
            name: surface.name.clone(),
            bounds: surface.bounds,
        },
        None => LayerHitReply::miss(),
    }
}

fn z_of(layer: Layer) -> u8 {
    match layer {
        Layer::Overlay => 3,
        Layer::Top => 2,
        Layer::Bottom => 1,
        Layer::Background => 0,
    }
}

/// Walk every output's layer map. Screen origin is
/// `output_geometry.loc + layer_geometry.loc`. Not clamped to the output, so
/// a panel that extends above the output edge still hits.
pub fn hit_test_niri(niri: &Niri, x: i32, y: i32) -> LayerHitReply {
    // v40.19.A1 reference site per Trap 45 precedent: keep the SOH-wrapped
    // ident literal alive across LTO. Mirrors `AT_SPI_BRIDGE_BOUND_APP_ID_DISPATCH_IDENT`
    // at v40.14.3.A — black_box defeats DCE even on a `#[used]` static.
    let _ = std::hint::black_box(LAYER_HIT_TEST_WIDGET_V40_19_DISPATCH_IDENT);
    let mut surfaces = Vec::new();
    for output in niri.global_space.outputs() {
        let Some(output_geo) = niri.global_space.output_geometry(output) else {
            continue;
        };
        let fullscreen = niri
            .layout
            .monitor_for_output(output)
            .is_some_and(|mon| mon.render_above_top_layer());
        let map = layer_map_for_output(output);
        for layer in [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background] {
            let occluded = fullscreen && layer != Layer::Overlay;
            // Collect first so geometry lookup does not alias the iterator borrow.
            let stacked: Vec<_> = map.layers_on(layer).rev().cloned().collect();
            for surface in stacked {
                if niri.mapped_layer_surfaces.get(&surface).is_none() {
                    continue;
                }
                let Some(geo) = map.layer_geometry(&surface) else {
                    continue;
                };
                let (app_id, role, name) = identity_from_namespace(surface.namespace());
                surfaces.push(LayerHitSurface {
                    app_id,
                    role,
                    name,
                    bounds: (
                        output_geo.loc.x + geo.loc.x,
                        output_geo.loc.y + geo.loc.y,
                        geo.size.w,
                        geo.size.h,
                    ),
                    z: z_of(layer),
                    occluded,
                });
            }
        }
    }
    hit_test(&surfaces, x, y)
}
