# AT-SPI per-action ladder (Row 13)

`app_id` + `AtSpiAction` → one of three routes. Policy lives in
[`src/inject/atspi_ladder.rs`](../src/inject/atspi_ladder.rs); the catalog is
read-only Row 12. No AX-tree walk, no session-bus bind, no user-seat read.

Wire: `AT_ELEMENT_REQ <wid> LC|EI|CV …` → `AT_ELEMENT_REPLY <payload>`.
`Element` replies `BACKGROUND_UNAVAILABLE` until Row 14/15 walk AT-SPI.
`RawInject` reuses `DISPATCH OK` (Alacritty / Winit + eligible only).

| semantic / eligible | LabelClick | EditInsert | Canvas |
| --- | --- | --- | --- |
| AtSpiStrong (`gnome-calculator`) | Element | Element | BACKGROUND_UNAVAILABLE |
| AtSpiChrome (`zen`) | Element | Element | BACKGROUND_UNAVAILABLE |
| Weak + raw eligible (`Alacritty`) | RawInject | RawInject | RawInject |
| None / unknown | BACKGROUND_UNAVAILABLE | same | same |
| Hud (`hermes-desktop`) | BACKGROUND_UNAVAILABLE | same | same |

Unknown `app_id` and AtSpiStrong never take RawInject. Agent seat only.
