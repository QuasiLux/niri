//! QuasiLux honest app catalog (Row 12).
//!
//! Measured-truth table. Each row records `app_id -> { toolkit, capture, semantic,
//! raw_inject_eligible, hud_skip }`. Used by [`crate::inject::grant_confirm::is_blocked_app_id`]
//! for the HUD skip-list and by future Row 13 for per-toolkit routing.

/// Capture strategy for an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureStrategy {
    WindowCast,
    BackgroundUnavailable,
    Hud,
}

/// Semantic-discovery strength via AT-SPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticStrength {
    AtSpiStrong,
    AtSpiModerate,
    AtSpiChrome,
    Weak,
    None,
    Hud,
}

/// Toolkit family (best-effort).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toolkit {
    Gecko,
    Gtk3,
    Gtk4,
    Qt5,
    Electron,
    Winit,
}

/// Marker type for `Niri::app_catalog`. The catalog is static; the field
/// documents the dependency and lets Row 13 grow a live override later.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppCatalogMarker;

/// One catalog row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppCatalogEntry {
    pub app_id: &'static str,
    pub toolkit: Toolkit,
    pub capture: CaptureStrategy,
    pub semantic: SemanticStrength,
    pub raw_inject_eligible: bool,
    pub hud_skip: bool,
}

/// Routing answers returned by [`InjectHarness::app_id_routes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppRoutes {
    pub toolkit: Option<Toolkit>,
    pub capture: CaptureStrategy,
    pub semantic: SemanticStrength,
    pub raw_inject_eligible: bool,
    pub hud_skip: bool,
}

impl AppRoutes {
    pub const fn unknown() -> Self {
        Self {
            toolkit: None,
            capture: CaptureStrategy::BackgroundUnavailable,
            semantic: SemanticStrength::None,
            raw_inject_eligible: false,
            hud_skip: false,
        }
    }
}

/// Static, measured-truth catalog. 7 rows, no Chrome/Chromium.
pub static APP_CATALOG: &[AppCatalogEntry] = &[
    AppCatalogEntry {
        app_id: "zen",
        toolkit: Toolkit::Gecko,
        capture: CaptureStrategy::WindowCast,
        semantic: SemanticStrength::AtSpiChrome,
        raw_inject_eligible: true,
        hud_skip: false,
    },
    AppCatalogEntry {
        app_id: "gnome-calculator",
        toolkit: Toolkit::Gtk3,
        capture: CaptureStrategy::WindowCast,
        semantic: SemanticStrength::AtSpiStrong,
        raw_inject_eligible: true,
        hud_skip: false,
    },
    AppCatalogEntry {
        app_id: "inkscape",
        toolkit: Toolkit::Gtk3,
        capture: CaptureStrategy::WindowCast,
        semantic: SemanticStrength::AtSpiModerate,
        raw_inject_eligible: true,
        hud_skip: false,
    },
    AppCatalogEntry {
        app_id: "Alacritty",
        toolkit: Toolkit::Winit,
        capture: CaptureStrategy::WindowCast,
        semantic: SemanticStrength::Weak,
        raw_inject_eligible: true,
        hud_skip: false,
    },
    AppCatalogEntry {
        app_id: "hermes-desktop",
        toolkit: Toolkit::Electron,
        capture: CaptureStrategy::Hud,
        semantic: SemanticStrength::Hud,
        raw_inject_eligible: false,
        hud_skip: true,
    },
    AppCatalogEntry {
        app_id: "org.gnome.Nautilus",
        toolkit: Toolkit::Gtk4,
        capture: CaptureStrategy::WindowCast,
        semantic: SemanticStrength::AtSpiModerate,
        raw_inject_eligible: true,
        hud_skip: false,
    },
    AppCatalogEntry {
        app_id: "electron-generic",
        toolkit: Toolkit::Electron,
        capture: CaptureStrategy::BackgroundUnavailable,
        semantic: SemanticStrength::None,
        raw_inject_eligible: false,
        hud_skip: false,
    },
];

/// Catalog lookup. Unknown app_ids yield `AppRoutes::unknown()`.
pub fn app_id_routes(app_id: &str) -> AppRoutes {
    for entry in APP_CATALOG {
        if entry.app_id == app_id {
            return AppRoutes {
                toolkit: Some(entry.toolkit),
                capture: entry.capture,
                semantic: entry.semantic,
                raw_inject_eligible: entry.raw_inject_eligible,
                hud_skip: entry.hud_skip,
            };
        }
    }
    AppRoutes::unknown()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zen_is_gecko_atspi_chrome() {
        let r = app_id_routes("zen");
        assert_eq!(r.toolkit, Some(Toolkit::Gecko));
        assert_eq!(r.semantic, SemanticStrength::AtSpiChrome);
        assert!(r.raw_inject_eligible);
        assert!(!r.hud_skip);
        assert_eq!(r.capture, CaptureStrategy::WindowCast);
    }

    #[test]
    fn gnome_calculator_is_gtk3_atspi_strong() {
        let r = app_id_routes("gnome-calculator");
        assert_eq!(r.toolkit, Some(Toolkit::Gtk3));
        assert_eq!(r.semantic, SemanticStrength::AtSpiStrong);
        assert!(r.raw_inject_eligible);
        assert!(!r.hud_skip);
    }

    #[test]
    fn alacritty_is_winit_raw_inject_required() {
        let r = app_id_routes("Alacritty");
        assert_eq!(r.toolkit, Some(Toolkit::Winit));
        assert_eq!(r.semantic, SemanticStrength::Weak);
        assert!(r.raw_inject_eligible);
        assert!(!r.hud_skip);
    }

    #[test]
    fn hermes_desktop_is_hud_skip_true() {
        let r = app_id_routes("hermes-desktop");
        assert!(r.hud_skip);
        assert_eq!(r.toolkit, Some(Toolkit::Electron));
        assert_eq!(r.semantic, SemanticStrength::Hud);
        assert_eq!(r.capture, CaptureStrategy::Hud);
        assert!(!r.raw_inject_eligible);
    }

    #[test]
    fn unknown_app_id_routes_to_unknown() {
        let r = app_id_routes("not-in-catalog-xyz");
        assert_eq!(r, AppRoutes::unknown());
        assert_eq!(r.capture, CaptureStrategy::BackgroundUnavailable);
        assert_eq!(r.semantic, SemanticStrength::None);
        assert!(!r.raw_inject_eligible);
        assert!(!r.hud_skip);
        assert_eq!(r.toolkit, None);
    }

    #[test]
    fn chrome_and_chromium_are_not_in_catalog() {
        assert_eq!(app_id_routes("chrome"), AppRoutes::unknown());
        assert_eq!(app_id_routes("chromium"), AppRoutes::unknown());
        assert_eq!(app_id_routes("google-chrome"), AppRoutes::unknown());
    }

    #[test]
    fn catalog_has_at_least_seven_rows() {
        assert!(APP_CATALOG.len() >= 7);
    }
}
