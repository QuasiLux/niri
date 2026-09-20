# Honest, measured-truth catalog

This is the QuasiLux **honest, measured-truth catalog** of compositor `app_id`s
and how inject should treat them. It is **not** a claim that Linux computer-use
works.

Canonical source of truth: [`src/inject/app_catalog.rs`](../src/inject/app_catalog.rs)
(`APP_CATALOG` + `app_id_routes()`). This document is the human table; the Rust
static is what production code reads.

## Table

| app_id | toolkit | capture | semantic | raw inject | HUD skip? |
| --- | --- | --- | --- | --- | --- |
| `zen` | Gecko | WindowCast | AtSpiChrome | yes | no |
| `gnome-calculator` | Gtk3 | WindowCast | AtSpiStrong | yes | no |
| `inkscape` | Gtk3 | WindowCast | AtSpiModerate | yes | no |
| `Alacritty` | Winit | WindowCast | Weak | yes | no |
| `hermes-desktop` | Electron | Hud | Hud | no | yes |
| `org.gnome.Nautilus` | Gtk4 | WindowCast | AtSpiModerate | yes | no |
| `electron-generic` | Electron | BackgroundUnavailable | None | no | no |

Unknown `app_id`s are not in this table. Lookup returns `AppRoutes::unknown()`:
capture `BackgroundUnavailable`, semantic `None`, raw inject ineligible, HUD
skip false, toolkit unset.

## Chrome / Chromium

Chrome and Chromium are **intentionally not listed**. A later brief may add
rows; they are out of scope for Row 12. `app_id_routes("chrome")`,
`app_id_routes("chromium")`, and `app_id_routes("google-chrome")` all return
`AppRoutes::unknown()`.

## HUD skip-list

The `hermes-desktop` HUD skip-list entry here is **documentation**. Production
code skip-list stays whatever Row 5 + Row 11b set:
`grant_confirm::is_blocked_app_id` still returns true for `hermes-desktop` and
any `niri-*` app_id, and false for `zen` / `gnome-calculator` / `Alacritty`.
The catalog `hud_skip` field is a parallel truth source consulted after that
existing filter; it does not weaken it.

## What the columns mean

- **toolkit** — best-effort family (Gecko, Gtk3/4, Electron, Winit, …).
- **capture** — how the window is observed (`WindowCast`, `BackgroundUnavailable`, `Hud`).
- **semantic** — AT-SPI discovery strength. `AtSpiChrome` means chrome
  (buttons/fields) is visible to AT-SPI but canvas/media is not; raw inject is
  still required for canvas. Row 13 will read this field. This card does not
  implement AT-SPI dispatch.
- **raw inject** — whether the inject plane may target the window with pointer /
  key / scroll / drag once granted.
- **HUD skip?** — whether the toplevel must not appear on the grant-confirm
  surface.

## Not a computer-use claim

Presence in this catalog means a measurement was recorded, not that Linux
computer-use is complete, reliable, or shipping. Electron generics stay
`background_unavailable`. HUD (`hermes-desktop`) stays skip-listed.
