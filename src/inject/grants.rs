//! In-memory per-bot × per-window inject BIND grants.
//!
//! Compositor restart drops the map by construction (no persistence). Overlay
//! or HUD click is not approval — Row 11 owns the confirmation surface.
//!
//! architecture-delta (v40.19 — Gap G fresh-window grant auto-grant):
//! - what changed: [`GrantMap`] now holds two sidecars — `interactive`
//!   (Row-11 persistent grants from manual user clicks; value `()` for
//!   backward-compat with `state.grants` test mirrors) and `auto`
//!   (`GrantEntry { kind: Auto, app_id }` rows pre-created on FIRST
//!   `wl_compositor.new_surface` event for any bot already granted the
//!   parent `app_id`). A third sidecar `interactive_app_id` indexes each
//!   interactive grant by `wid → app_id` so `on_new_window(wid, app_id)`
//!   can find which bots have already been granted the parent toplevel
//!   for the new toplevel's app_id. The first manual click promotes
//!   `Auto → Interactive`; the bot keeps BIND rights.
//! - why: pre-v40.19 freshly spawned windows stay `NOT_GRANTED` while
//!   `qlux_grant_confirm` is Exclusive and invisible; cua-bot cannot
//!   BIND a fresh pavucontrol/alacritty/galculator even after its parent
//!   (quickshell) was granted. v40.19 pre-grants the fresh window to the
//!   originally-granted bot without a manual click; Row-11 cross-bot
//!   BIND defense is preserved (other bots still require the manual
//!   click).
//! - supersedes: Row-11 `apply_grant` 2-arg signature is preserved as a
//!   backward-compat shim (Interactive entry, no app_id recorded; the
//!   bot was granted but no auto-promotion chain is triggered). New
//!   callers (the seat-side click handler with `app_id` from the
//!   bar row) use [`GrantMap::apply_grant_with_app`]. Existing
//!   `state.grants.lock().unwrap().contains_key(...)` test mirrors stay
//!   green because the `interactive` map still uses `()` values.
//! - next cards must: the M2 install+e2e follow-up (default, scratch)
//!   verifies the live path: cua-bot BINDs a freshly spawned pavucontrol
//!   without manual click, then another bot (e.g. `sleep-bot`) requires
//!   the manual click for the same fresh window.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Per-row grant kind. The same `(bot_id, window_id)` key can hold at
/// most one entry, but the entry may transition `Auto → Interactive` on
/// the first manual user click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantKind {
    /// Persistent grant from a manual user click on the bar. BIND OK for
    /// the granted bot (and any bot, per the Row-11 cross-bot sharing
    /// defense at the policy layer).
    Interactive,
    /// Pre-created on the FIRST `wl_compositor.new_surface` event for a
    /// toplevel whose `app_id` already has a parent grant for the same
    /// bot. BIND OK by that bot without a manual click. Promotes to
    /// `Interactive` on the first manual click on the bar.
    Auto,
}

/// Per-row grant payload. Stores `kind` (Auto vs Interactive) and the
/// toplevel `app_id` so `on_new_window` can find which bots have already
/// been granted the parent toplevel.
#[derive(Debug, Clone)]
pub struct GrantEntry {
    pub kind: GrantKind,
    pub app_id: String,
}

impl GrantEntry {
    fn auto(app_id: &str) -> Self {
        Self {
            kind: GrantKind::Auto,
            app_id: app_id.to_string(),
        }
    }
}

/// Shared `(bot_id, window_id)` grant set. Cheap to clone (`Arc`).
///
/// Three internal sidecars:
/// - `interactive` (`HashMap<(String, u64), ()>`): persistent Row-11 grants
///   from manual clicks. The value `()` is preserved so test mirrors that
///   pass `Arc::new(Mutex::new(HashMap::new()))` to `DispatchState::new`
///   continue to type-infer without a turbofish annotation. Backed by
///   `state.grants` on `DispatchState`.
/// - `auto` (`HashMap<(String, u64), GrantEntry>`): pre-created Auto
///   grants keyed by `(bot_id, wid)`. Each row carries the toplevel
///   `app_id` so `on_new_window` walks it directly to find bots already
///   holding an Auto grant for the same app_id (e.g. quickshell). The
///   first manual click promotes Auto to Interactive (the entry is
///   removed from `auto` and inserted into `interactive`).
/// - `interactive_app_id` (`HashMap<u64, String>`): records `app_id` per
///   `wid` for every interactive grant. Populated by
///   [`GrantMap::apply_grant_with_app`]. Used by
///   [`GrantMap::on_new_window`] to find which bots have been granted
///   the parent toplevel whose `app_id` matches the new window's
///   `app_id`.
#[derive(Clone, Default)]
pub struct GrantMap {
    pub interactive: Arc<Mutex<HashMap<(String, u64), ()>>>,
    pub auto: Arc<Mutex<HashMap<(String, u64), GrantEntry>>>,
    pub interactive_app_id: Arc<Mutex<HashMap<u64, String>>>,
}

impl GrantMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap an existing raw `interactive` HashMap (tests; the
    /// `state.grants` raw map). The `auto` and `interactive_app_id`
    /// sidecars start empty.
    pub fn from_arc(interactive: Arc<Mutex<HashMap<(String, u64), ()>>>) -> Self {
        Self {
            interactive,
            auto: Arc::new(Mutex::new(HashMap::new())),
            interactive_app_id: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Backward-compat 2-arg insert: persistent `Interactive` grant for
    /// `(bot_id, window_id)`. Promotes any existing `Auto` entry to
    /// `Interactive` (the Auto entry is removed). Does NOT touch
    /// `interactive_app_id` (caller does not know the app_id).
    pub fn apply_grant(&self, bot_id: impl Into<String>, window_id: u64) {
        let bot_id = bot_id.into();
        self.interactive
            .lock()
            .unwrap()
            .insert((bot_id.clone(), window_id), ());
        self.auto.lock().unwrap().remove(&(bot_id, window_id));
    }

    /// 3-arg insert with `app_id` (v40.19 — the seat-side click handler
    /// uses this so `interactive_app_id` is recorded for `on_new_window`).
    /// Returns the previous kind (`Auto` if a pre-grant existed, else
    /// `Interactive`). The new row is always `Interactive`.
    pub fn apply_grant_with_app(
        &self,
        bot_id: &str,
        window_id: u64,
        app_id: &str,
    ) -> GrantKind {
        let k = (bot_id.to_string(), window_id);
        let was_auto = self.auto.lock().unwrap().remove(&k).is_some();
        self.interactive.lock().unwrap().insert(k, ());
        // Record the app_id for `on_new_window`. If another bot already
        // claimed this `wid` for a different `app_id`, keep the first
        // claim (the parent is the first grant made, and re-using a
        // window id is unusual at this layer).
        self.interactive_app_id
            .lock()
            .unwrap()
            .entry(window_id)
            .or_insert_with(|| app_id.to_string());
        if was_auto {
            GrantKind::Auto
        } else {
            GrantKind::Interactive
        }
    }

    /// Pre-create an `Auto` grant for `(bot_id, window_id)`. Returns
    /// `true` when the entry was newly inserted; `false` when an
    /// `Interactive` grant already exists (the manual click path will
    /// promote any Auto later — no need to overwrite) or when an `Auto`
    /// entry for this key already exists (idempotent).
    pub fn apply_auto_grant(&self, bot_id: &str, window_id: u64, app_id: &str) -> bool {
        let k = (bot_id.to_string(), window_id);
        // If an Interactive grant already exists, do nothing.
        if self.interactive.lock().unwrap().contains_key(&k) {
            return false;
        }
        let mut m = self.auto.lock().unwrap();
        if m.contains_key(&k) {
            return false;
        }
        m.insert(k, GrantEntry::auto(app_id));
        true
    }

    /// True iff `(bot_id, window_id)` holds any grant — `Interactive` or
    /// `Auto`. The BIND-time check.
    pub fn has_grant(&self, bot_id: &str, window_id: u64) -> bool {
        let k = (bot_id.to_string(), window_id);
        self.interactive.lock().unwrap().contains_key(&k)
            || self.auto.lock().unwrap().contains_key(&k)
    }

    /// True iff an `Auto` grant exists for `(bot_id, window_id)`. Used by
    /// tests to verify the pre-grant path and by the promote-on-click
    /// flow.
    pub fn is_auto_grant(&self, bot_id: &str, window_id: u64) -> bool {
        self.auto
            .lock()
            .unwrap()
            .contains_key(&(bot_id.to_string(), window_id))
    }

    /// v40.19 — called from the FIRST `wl_compositor.new_surface` event
    /// for a freshly mapped toplevel. Walks the existing
    /// `interactive_app_id` for any `(parent_wid → app_id)` rows whose
    /// `app_id` matches `app_id`; for each such parent, walks
    /// `interactive` to find every bot already granted that parent and
    /// pre-creates an `Auto` grant for `(bot, wid)`. Returns the number
    /// of newly inserted `Auto` rows.
    ///
    /// Idempotent across multiple calls for the same `(wid, app_id)`
    /// (e.g. focus events that re-fire new_surface — `apply_auto_grant`
    /// returns `false` for existing keys).
    pub fn on_new_window(&self, wid: u64, app_id: &str) -> usize {
        // Step 1 — find parent wid(s) whose recorded app_id matches.
        let parent_wids: Vec<u64> = self
            .interactive_app_id
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(parent_wid, recorded_app_id)| {
                if recorded_app_id == app_id {
                    Some(*parent_wid)
                } else {
                    None
                }
            })
            .collect();

        // Step 2 — find bots already granted any of those parent wid(s).
        let mut bots: Vec<String> = Vec::new();
        {
            let interactive = self.interactive.lock().unwrap();
            for (key, _) in interactive.iter() {
                let (bot_id, parent_wid) = key;
                if parent_wids.contains(parent_wid) {
                    bots.push(bot_id.clone());
                }
            }
        }
        // Also include bots holding an Auto grant for the same app_id
        // (e.g. an earlier sibling toplevel that was pre-granted). The
        // first Auto grant for that bot's app_id sets the linkage; a
        // later sibling just propagates the pre-grant forward.
        {
            let auto = self.auto.lock().unwrap();
            for (key, entry) in auto.iter() {
                let (bot_id, _existing_wid) = key;
                if entry.app_id == app_id && !bots.contains(bot_id) {
                    bots.push(bot_id.clone());
                }
            }
        }
        bots.sort();
        bots.dedup();

        // Step 3 — pre-create Auto entries.
        let mut count = 0usize;
        for bot in bots {
            if self.apply_auto_grant(&bot, wid, app_id) {
                count += 1;
            }
        }
        count
    }

    /// Backward-compat raw-Arc accessor for `interactive`. Used by
    /// `dispatch_inject_connection` to share the live `state.niri.grant_map`
    /// with the worker thread (matches `state.grants`).
    pub fn arc(&self) -> Arc<Mutex<HashMap<(String, u64), ()>>> {
        Arc::clone(&self.interactive)
    }

    #[allow(dead_code)]
    pub fn clear_grants(&self) {
        self.interactive.lock().unwrap().clear();
        self.auto.lock().unwrap().clear();
        self.interactive_app_id.lock().unwrap().clear();
    }
}