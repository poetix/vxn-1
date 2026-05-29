//! Per-thread parameter mirror used to bridge the engine, the host and the UI.
//!
//! Following clack's gain-gui pattern, the plugin never writes the shared store
//! directly from host input events. Instead each processing thread keeps a
//! local mirror:
//!
//! 1. [`fetch_ui_changes`](LocalParams::fetch_ui_changes) pulls UI-originated
//!    writes out of the shared store (flagging them for echo to the host).
//! 2. [`apply_input`](LocalParams::apply_input) folds host automation events
//!    into the mirror (and reports them so the engine can be updated).
//! 3. [`publish`](LocalParams::publish) writes the mirror back to the shared
//!    store so the host (`get_value`) and the UI observe host-side changes.
//! 4. [`emit`](LocalParams::emit) sends UI edits back to the host, each wrapped
//!    in a CLAP gesture begin/end so automation recording and undo coalesce.
//!
//! Because the shared store only ever changes via the UI or via `publish`, the
//! `fetch_ui_changes` diff sees *only* UI edits — host automation is never
//! echoed back to the host (no feedback loop).

use clack_plugin::events::Pckn;
use clack_plugin::events::event_types::{
    ParamGestureBeginEvent, ParamGestureEndEvent, ParamValueEvent,
};
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::prelude::*;
use clack_plugin::utils::Cookie;
use vxn_engine::{ParamValues, SharedParams, TOTAL_PARAMS};

pub struct LocalParams {
    /// Working values (plain units), the authoritative set for this thread.
    values: [f32; TOTAL_PARAMS],
    /// Last-seen UI gesture state per param (to detect begin/end transitions).
    gesture: [bool; TOTAL_PARAMS],
    /// Params changed by the UI since the last [`emit`](Self::emit).
    ui_changed: [bool; TOTAL_PARAMS],
    /// Params changed by **host automation** since the last
    /// [`publish`](Self::publish). Only these are written back to the shared
    /// store, so a concurrent UI bulk write (a preset load) landing between this
    /// block's [`fetch_ui_changes`](Self::fetch_ui_changes) and `publish` is
    /// never clobbered by re-publishing the stale mirror.
    host_changed: [bool; TOTAL_PARAMS],
}

impl LocalParams {
    pub fn new(shared: &SharedParams) -> Self {
        Self {
            values: std::array::from_fn(|i| shared.get(i)),
            gesture: [false; TOTAL_PARAMS],
            ui_changed: [false; TOTAL_PARAMS],
            host_changed: [false; TOTAL_PARAMS],
        }
    }

    /// Pull UI-originated writes from `shared` into the mirror, flagging them
    /// for echo to the host. Returns whether anything changed.
    pub fn fetch_ui_changes(&mut self, shared: &SharedParams) -> bool {
        let mut any = false;
        for i in 0..TOTAL_PARAMS {
            let sv = shared.get(i);
            if sv != self.values[i] {
                self.values[i] = sv;
                self.ui_changed[i] = true;
                any = true;
            }
        }
        any
    }

    /// Fold a host param-value input event into the mirror. Returns the
    /// `(index, value)` so the caller can forward it to the engine. Not flagged
    /// as a UI change, so it is never echoed back to the host.
    pub fn apply_input(&mut self, event: &UnknownEvent) -> Option<(usize, f32)> {
        if let Some(CoreEventSpace::ParamValue(e)) = event.as_core_event() {
            if let Some(pid) = e.param_id() {
                let i = pid.get() as usize;
                if i < TOTAL_PARAMS {
                    let v = e.value() as f32;
                    self.values[i] = v;
                    self.host_changed[i] = true;
                    return Some((i, v));
                }
            }
        }
        None
    }

    /// Copy the working values into the engine's parameter table.
    pub fn write_to(&self, params: &mut ParamValues) {
        for (i, &v) in self.values.iter().enumerate() {
            params.set_by_clap_id(i, v);
        }
    }

    /// Publish **host-automation** changes to `shared` so the host and UI observe
    /// host-side movement. Only params flagged by [`apply_input`](Self::apply_input)
    /// this block are written (then cleared) — re-publishing the whole mirror
    /// would race a concurrent UI bulk write (a preset load) and silently revert
    /// it. UI-originated writes already live in `shared`; the audio thread only
    /// reads them (via `fetch_ui_changes`), never writes them back.
    pub fn publish(&mut self, shared: &SharedParams) {
        for i in 0..TOTAL_PARAMS {
            if self.host_changed[i] {
                shared.set(i, self.values[i]);
                self.host_changed[i] = false;
            }
        }
    }

    /// Emit UI-originated changes to the host, each bracketed by a gesture
    /// begin/end. `end_time` is the sample offset for the closing gesture.
    pub fn emit(&mut self, shared: &SharedParams, out: &mut OutputEvents, end_time: u32) {
        for i in 0..TOTAL_PARAMS {
            let prev = self.gesture[i];
            let cur = shared.gesture(i);
            self.gesture[i] = cur;
            let changed = self.ui_changed[i];
            self.ui_changed[i] = false;

            if !changed && cur == prev {
                continue;
            }
            // A held gesture brackets a burst of values; a bare value change
            // (no sustained gesture) is wrapped in its own begin/end ("Both").
            let bare = changed && !cur && !prev;
            let begin = (cur && !prev) || bare;
            let end = (!cur && prev) || bare;
            let id = ClapId::new(i as u32);
            if begin {
                let _ = out.try_push(ParamGestureBeginEvent::new(0, id));
            }
            if changed {
                let _ = out.try_push(ParamValueEvent::new(
                    0,
                    id,
                    Pckn::match_all(),
                    self.values[i] as f64,
                    Cookie::empty(),
                ));
            }
            if end {
                let _ = out.try_push(ParamGestureEndEvent::new(end_time, id));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vxn_engine::{Layer, PatchParam, ParamValues, patch_clap_id};

    /// A UI bulk write (preset load) that lands in the window between this block's
    /// `fetch_ui_changes` and `publish` must survive: `publish` only writes host
    /// automation, so it can't revert the UI's value. The next `fetch_ui_changes`
    /// then folds it into the mirror. (Regression: a blanket re-publish silently
    /// dropped the load, leaving the old patch in place — 0027.)
    #[test]
    fn publish_does_not_clobber_concurrent_ui_writes() {
        let shared = SharedParams::new();
        let mut local = LocalParams::new(&shared);
        let cutoff = patch_clap_id(Layer::Upper, PatchParam::Cutoff);

        // No host automation this block; the UI writes a new value to the shared
        // store after the mirror was built (the preset-load race window).
        let before = shared.get(cutoff);
        let loaded = before + 1234.0;
        shared.set(cutoff, loaded);

        // publish must leave the UI's value untouched (nothing host-changed).
        local.publish(&shared);
        assert_eq!(shared.get(cutoff), loaded, "publish reverted a UI write");

        // The next fetch folds the UI value into the mirror / engine.
        assert!(local.fetch_ui_changes(&shared));
        let mut params = ParamValues::default();
        local.write_to(&mut params);
        assert_eq!(params.get_by_clap_id(cutoff), loaded);
    }

    /// host automation still reaches the shared store via publish, so the UI/host
    /// observe it. (`apply_input` flags the param; we exercise the publish side
    /// by flagging through the same field a host event would set.)
    #[test]
    fn publish_writes_host_changes_once_then_clears() {
        let shared = SharedParams::new();
        let mut local = LocalParams::new(&shared);
        let cutoff = patch_clap_id(Layer::Upper, PatchParam::Cutoff);

        // Simulate a host-automation fold: mirror updated + flagged.
        local.values[cutoff] = 777.0;
        local.host_changed[cutoff] = true;
        local.publish(&shared);
        assert_eq!(shared.get(cutoff), 777.0);

        // A second publish with no new host change is a no-op (flag cleared), so a
        // later UI write to the same param is not overwritten.
        shared.set(cutoff, 888.0);
        local.publish(&shared);
        assert_eq!(shared.get(cutoff), 888.0);
    }
}
