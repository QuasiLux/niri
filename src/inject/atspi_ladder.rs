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

