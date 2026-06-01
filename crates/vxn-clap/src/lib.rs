//! VXN1 CLAP plugin shell (clack).
//!
//! Wires the framework-agnostic [`Synth`] engine to CLAP: a stereo output port,
//! a CLAP note input, the full parameter set, state save/restore, and the
//! HTML webview editor via the `gui` extension. Parameters bridge the
//! engine, the host and the UI through `vxn_engine::SharedParams`;
//! [`local::LocalParams`] diffs that store to echo UI edits to the host
//! without echoing host automation back (see its module docs).

mod gui;
mod local;

use clack_extensions::gui::PluginGui;
use clack_extensions::state::{PluginState, PluginStateImpl};
use clack_extensions::timer::{HostTimer, PluginTimer, PluginTimerImpl, TimerId};
use clack_extensions::{audio_ports::*, note_ports::*, params::*};
use clack_plugin::events::Match;
use clack_plugin::events::event_types::TransportFlags;
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};
use local::LocalParams;
use std::ffi::CStr;
use std::fmt::Write as _;
use std::io::{Read, Write as _IoWrite};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::Receiver;
use vxn_app::{
    Controller, CorpusHandle, HostEvent, ParamId, ParamModel, ViewEvent,
};
use vxn_engine::{
    EnginePresetStore, ParamDesc, ParamKind, SharedParams, Synth, TOTAL_PARAMS, desc_for_clap_id,
    module_for_clap_id,
};

/// Locks a poisoned mutex by extracting the inner value instead of unwrapping.
/// Plugin code runs with `panic = unwind`, so a panic during `tick` could
/// poison the controller's outer mutex; we don't want every subsequent flush
/// to fail. The data is still valid (the panic happened mid-write at worst).
fn lock_mut<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Top-level plugin marker type.
pub struct VxnPlugin;

impl Plugin for VxnPlugin {
    type AudioProcessor<'a> = VxnAudioProcessor<'a>;
    type Shared<'a> = VxnShared;
    type MainThread<'a> = VxnMainThread<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _shared: Option<&VxnShared>) {
        builder
            .register::<PluginAudioPorts>()
            .register::<PluginNotePorts>()
            .register::<PluginParams>()
            .register::<PluginState>()
            .register::<PluginGui>()
            .register::<PluginTimer>();
    }
}

impl DefaultPluginFactory for VxnPlugin {
    fn get_descriptor() -> PluginDescriptor {
        use clack_plugin::plugin::features::*;
        PluginDescriptor::new("labs.vulpus.vxn1", "VXN1").with_features([
            SYNTHESIZER,
            INSTRUMENT,
            STEREO,
        ])
    }

    fn new_shared(_host: HostSharedHandle) -> Result<VxnShared, PluginError> {
        Ok(VxnShared {
            params: Arc::new(SharedParams::new()),
        })
    }

    fn new_main_thread<'a>(
        host: HostMainThreadHandle<'a>,
        shared: &'a VxnShared,
    ) -> Result<VxnMainThread<'a>, PluginError> {
        let (controller, view_rx, corpus) =
            Controller::new(shared.params.clone(), Box::new(EnginePresetStore::new()));
        let controller = Arc::new(Mutex::new(controller));
        let view_rx = Arc::new(Mutex::new(view_rx));
        Ok(VxnMainThread {
            shared,
            controller,
            view_rx,
            corpus,
            gui: None,
            host,
            timer: None,
            last_seen: vec![f32::NAN; TOTAL_PARAMS],
        })
    }
}

/// Data shared between the main and audio threads. The parameter store lives
/// behind an `Arc` so the editor (created on the main thread) can hold a clone.
pub struct VxnShared {
    params: Arc<SharedParams>,
}

impl PluginShared<'_> for VxnShared {}

/// Main-thread state. The [`Controller`] is the single arbiter of non-audio
/// model mutation (ADR 0007) — host param events arriving on the main thread,
/// state save/restore, and (after 0037) UI edits all funnel through it. The
/// audio-thread still keeps its own [`LocalParams`] mirror for engine latency;
/// see `VxnAudioProcessor`.
pub struct VxnMainThread<'a> {
    shared: &'a VxnShared,
    /// Wrapped in `Arc<Mutex<...>>` so the timer drain and the host's `flush`
    /// paths share one controller without either reaching across thread
    /// boundaries. Both sites are main-thread, so there is no real contention.
    controller: Arc<Mutex<Controller<SharedParams>>>,
    /// View-bound events the controller emits. The timer drain consumes
    /// these; when the GUI is closed they stay queued and the bounded
    /// channel drops on full (controller emits via `try_send`).
    view_rx: Arc<Mutex<Receiver<ViewEvent>>>,
    /// Shared snapshot of the preset corpus the controller publishes for the
    /// editor's browser. Refreshed by the controller after every disk op.
    corpus: CorpusHandle,
    /// The live editor window, while the GUI is open.
    gui: Option<vxn_ui_web::EditorHandle>,
    /// Host main-thread handle. `on_timer` uses it to call
    /// `HostTimer::register_timer` / `unregister_timer`.
    host: HostMainThreadHandle<'a>,
    /// Editor's main-thread timer (id + the host's timer extension), driving
    /// the view-event drain + controller tick. `None` when the GUI is closed
    /// or the host doesn't support `timer-support`.
    timer: Option<(HostTimer, TimerId)>,
    /// Last param values seen by the diff pump. Audio-thread automation
    /// writes [`SharedParams`] directly without round-tripping the
    /// controller, so the editor would otherwise never see it. On each tick
    /// we diff the current values against this vector and push a
    /// `ParamChanged` for any drift. Seeded all-`NaN` so the first tick
    /// after open broadcasts the whole table to populate the page.
    last_seen: Vec<f32>,
}

impl<'a> PluginMainThread<'a, VxnShared> for VxnMainThread<'a> {}

impl<'a> VxnMainThread<'a> {
    /// Drain the controller's view-event queue and forward each event into
    /// the live WebView. Called from the timer tick; safe to call when there
    /// is no GUI (just no-ops).
    fn drain_view_events(&mut self) {
        let Some(handle) = self.gui.as_ref() else {
            return;
        };
        let rx = lock_mut(&self.view_rx);
        while let Ok(ev) = rx.try_recv() {
            handle.push_view_event(ev);
        }
    }

    /// Diff the shared param store against `last_seen` and push a
    /// `ParamChanged` for any drift. This is the path that catches
    /// audio-thread automation: `process()` writes `SharedParams` directly
    /// (via `LocalParams::publish`) without routing through the controller,
    /// so the controller's view-event queue stays empty for those changes.
    fn push_param_diffs(&mut self) {
        let Some(handle) = self.gui.as_ref() else {
            return;
        };
        let model = &*self.shared.params;
        let n = ParamModel::total(model).min(self.last_seen.len());
        // Sync flips refresh their rate partner's display label even though
        // the rate's value didn't change. Collect those first, then emit
        // after the main pass.
        let mut force_rate_refresh: Vec<usize> = Vec::new();
        for i in 0..n {
            let id = ParamId::new(i);
            let plain = ParamModel::get(model, id);
            // NaN-aware: NaN never equals itself, so the seeded all-NaN
            // vector forces a full broadcast on the first tick after open.
            if plain == self.last_seen[i] {
                continue;
            }
            self.last_seen[i] = plain;
            let norm = ParamModel::get_normalized(model, id);
            let display = sync_aware_display(&self.shared.params, i, plain);
            handle.push_view_event(ViewEvent::ParamChanged {
                id,
                plain,
                norm,
                display,
            });
            if let Some(rate_id) = vxn_app::sync::rate_partner_clap_id(i) {
                force_rate_refresh.push(rate_id);
            }
        }
        for rate_id in force_rate_refresh {
            let id = ParamId::new(rate_id);
            let plain = ParamModel::get(model, id);
            let norm = ParamModel::get_normalized(model, id);
            let display = sync_aware_display(&self.shared.params, rate_id, plain);
            handle.push_view_event(ViewEvent::ParamChanged {
                id,
                plain,
                norm,
                display,
            });
        }
    }
}

impl<'a> PluginTimerImpl for VxnMainThread<'a> {
    fn on_timer(&mut self, _id: TimerId) {
        // Pull UI-posted intents into the model first so the ViewEvents they
        // generate land in `view_rx` before we drain it — saves a tick of
        // round-trip latency on a knob drag.
        lock_mut(&self.controller).tick();
        self.drain_view_events();
        // Then catch any audio-thread automation the controller never saw.
        // The two pushes can echo the same param twice in a tick (controller
        // emit + diff push); the WebView dedupes ParamChanged by id inside
        // `flush_view_events`, so the overlap costs nothing on the wire.
        self.push_param_diffs();
        // 0046: one `evaluate_script` per tick. Both pushes above only
        // buffered into the EditorHandle; this is the single bridge call.
        if let Some(handle) = self.gui.as_ref() {
            handle.flush_view_events();
        }
    }
}

/// Audio-thread processor: owns the synth engine, a local parameter mirror and
/// render scratch.
pub struct VxnAudioProcessor<'a> {
    synth: Synth,
    shared: &'a VxnShared,
    local: LocalParams,
    scratch_l: Vec<f32>,
    scratch_r: Vec<f32>,
}

impl<'a> PluginAudioProcessor<'a, VxnShared, VxnMainThread<'a>> for VxnAudioProcessor<'a> {
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &mut VxnMainThread,
        shared: &'a VxnShared,
        audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        let max = audio_config.max_frames_count as usize;
        Ok(Self {
            synth: Synth::new(audio_config.sample_rate as f32),
            local: LocalParams::new(&shared.params),
            shared,
            scratch_l: vec![0.0; max],
            scratch_r: vec![0.0; max],
        })
    }

    fn process(
        &mut self,
        process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        // Flush denormals to zero for this block, restoring the host's FP mode
        // on return. Set per-process (not once in `activate`) because the FP
        // control word is thread-local and the host may run `process` on a
        // different thread; the engine's filter/delay feedback paths rely on it.
        let _ftz = vxn_engine::ScopedFlushToZero::new();

        // Fold UI edits made since the last process into the local mirror, then
        // drive the engine from the working values (UI + last host state).
        self.local.fetch_ui_changes(&self.shared.params);
        self.local.write_to(self.synth.params_mut());

        // Key mode + split point are non-automatable shared state (ADR 0003
        // §3/§8): push them to the engine so note routing and per-layer param
        // sourcing follow the current mode. Seed-on-entry happened in the store.
        self.synth.set_key_mode(self.shared.params.key_mode());
        self.synth.set_split_point(self.shared.params.split_point());

        // Host transport → engine tempo for LFO host-sync (E004 / 0015). Use the
        // BPM only when the transport actually carries a tempo; otherwise the
        // engine keeps its sane default (never NaN).
        if let Some(t) = process.transport {
            if t.flags.contains(TransportFlags::HAS_TEMPO) {
                self.synth.set_tempo(t.tempo as f32);
            }
        }

        let mut output_port = audio
            .output_port(0)
            .ok_or(PluginError::Message("No output port"))?;
        let mut out = output_port
            .channels()?
            .into_f32()
            .ok_or(PluginError::Message("Expected f32 output"))?;

        let frames = (out.frames_count() as usize).min(self.scratch_l.len());
        let nch = out.channel_count() as usize;

        // Disjoint field borrows so event handling and rendering can coexist.
        let synth = &mut self.synth;
        let local = &mut self.local;
        let l = &mut self.scratch_l[..frames];
        let r = &mut self.scratch_r[..frames];

        for event_batch in events.input.batch() {
            for event in event_batch.events() {
                match event.as_core_event() {
                    Some(CoreEventSpace::NoteOn(e)) => {
                        if let Match::Specific(key) = e.key() {
                            synth.note_on(key as u8, e.velocity() as f32);
                        }
                    }
                    Some(CoreEventSpace::NoteOff(e)) => {
                        if let Match::Specific(key) = e.key() {
                            synth.note_off(key as u8);
                        }
                    }
                    Some(CoreEventSpace::ParamValue(_)) => {
                        // Host automation: fold into the mirror and the engine.
                        if let Some((idx, value)) = local.apply_input(event) {
                            synth.set_param(idx, value);
                        }
                    }
                    Some(CoreEventSpace::Midi(e)) => {
                        // Raw MIDI 1.0: pitch bend (0xE0) → pitch; CC1 (mod wheel)
                        // → routable destination. Channel nibble ignored (global).
                        let [status, d1, d2] = e.data();
                        match status & 0xF0 {
                            0xE0 => {
                                // 14-bit bend, centre 8192 → normalised [-1, 1].
                                let raw = ((d2 as u16) << 7) | d1 as u16;
                                synth.set_pitch_bend((raw as f32 - 8192.0) / 8192.0);
                            }
                            0xB0 if d1 == 1 => {
                                // Deadzone the bottom LSB: a hardware wheel
                                // rarely rests clean at 0 and 1 LSB is already
                                // a large step on a wide pitch route (±48 st →
                                // 0.76 st/LSB), so jitter at rest reads as a
                                // wandering pitch. Floor low values to 0.
                                let wheel = if d2 <= 1 { 0.0 } else { d2 as f32 / 127.0 };
                                synth.set_mod_wheel(wheel);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            let (sb, eb) = event_batch.sample_bounds();
            let start = match sb {
                std::ops::Bound::Included(n) => n,
                std::ops::Bound::Excluded(n) => n + 1,
                std::ops::Bound::Unbounded => 0,
            }
            .min(frames);
            let end = match eb {
                std::ops::Bound::Included(n) => n + 1,
                std::ops::Bound::Excluded(n) => n,
                std::ops::Bound::Unbounded => frames,
            }
            .min(frames);
            if start < end {
                synth.process(&mut l[start..end], &mut r[start..end]);
            }
        }

        // Copy the stereo scratch into the host's channels.
        if let Some(ch) = out.channel_mut(0) {
            let n = ch.len().min(frames);
            ch[..n].copy_from_slice(&self.scratch_l[..n]);
        }
        if nch >= 2 {
            if let Some(ch) = out.channel_mut(1) {
                let n = ch.len().min(frames);
                ch[..n].copy_from_slice(&self.scratch_r[..n]);
            }
        }

        // Fold host automation into the shared store (so the UI/host observe it)
        // and echo UI edits back to the host as gesture-bracketed param events.
        self.local.publish(&self.shared.params);
        self.local
            .emit(&self.shared.params, events.output, frames as u32);

        Ok(ProcessStatus::Continue)
    }

    fn reset(&mut self) {
        self.synth.reset();
    }
}

// ── Audio / Note ports ──────────────────────────────────────────────────────

impl PluginAudioPortsImpl for VxnMainThread<'_> {
    fn count(&mut self, is_input: bool) -> u32 {
        if is_input { 0 } else { 1 }
    }

    fn get(&mut self, index: u32, is_input: bool, writer: &mut AudioPortInfoWriter) {
        if !is_input && index == 0 {
            writer.set(&AudioPortInfo {
                id: ClapId::new(1),
                name: b"main",
                channel_count: 2,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::STEREO),
                in_place_pair: None,
            });
        }
    }
}

impl PluginNotePortsImpl for VxnMainThread<'_> {
    fn count(&mut self, is_input: bool) -> u32 {
        if is_input { 1 } else { 0 }
    }

    fn get(&mut self, index: u32, is_input: bool, writer: &mut NotePortInfoWriter) {
        if is_input && index == 0 {
            writer.set(&NotePortInfo {
                id: ClapId::new(1),
                name: b"main",
                preferred_dialect: Some(NoteDialect::Clap),
                supported_dialects: NoteDialects::CLAP | NoteDialects::MIDI,
            });
        }
    }
}

// ── Parameters ────────────────────────────────────────────────────────────────

fn format_value(desc: &ParamDesc, value: f64, writer: &mut ParamDisplayWriter) -> std::fmt::Result {
    // Shared with the editor's value readouts so host and UI render identically.
    write!(writer, "{}", desc.display(value as f32))
}

/// Sync-aware display string for a CLAP param. When `id` is an LFO/Delay
/// rate/time whose sync partner reads on, returns the matching subdivision
/// label; otherwise the normal unit-formatted display. Shared by the host
/// `value_to_text` path and the editor `ParamChanged` broadcast so both
/// readouts agree.
fn sync_aware_display(params: &SharedParams, clap_id: usize, value: f32) -> String {
    let Some(desc) = desc_for_clap_id(clap_id) else {
        return String::new();
    };
    if let Some(sync_id) = vxn_app::sync::sync_partner_clap_id(clap_id) {
        if params.get(sync_id) >= 0.5 {
            return vxn_app::sync::synced_label_for(desc, value).to_string();
        }
    }
    desc.display(value)
}

impl PluginMainThreadParams for VxnMainThread<'_> {
    fn count(&mut self) -> u32 {
        TOTAL_PARAMS as u32
    }

    fn get_info(&mut self, param_index: u32, info: &mut ParamInfoWriter) {
        let idx = param_index as usize;
        let Some(desc) = desc_for_clap_id(idx) else {
            return;
        };
        let stepped = !matches!(desc.kind, ParamKind::Float { .. });
        let mut flags = ParamInfoFlags::IS_AUTOMATABLE;
        if stepped {
            flags |= ParamInfoFlags::IS_STEPPED;
        }
        info.set(&ParamInfo {
            id: ClapId::new(idx as u32),
            flags,
            cookie: Default::default(),
            name: desc.label.as_bytes(),
            // Group the automation list by layer (Upper/Lower/Global).
            module: module_for_clap_id(idx).as_bytes(),
            min_value: desc.min as f64,
            max_value: desc.max as f64,
            default_value: desc.default as f64,
        });
    }

    fn get_value(&mut self, param_id: ClapId) -> Option<f64> {
        let idx = param_id.get() as usize;
        if idx < TOTAL_PARAMS {
            Some(self.shared.params.get(idx) as f64)
        } else {
            None
        }
    }

    fn value_to_text(
        &mut self,
        param_id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> std::fmt::Result {
        let id = param_id.get() as usize;
        let Some(desc) = desc_for_clap_id(id) else {
            return Err(std::fmt::Error);
        };
        // Synced rate/time params display their subdivision label (E004 /
        // 0015), so the host's value readouts match the editor's popup.
        if let Some(sync_id) = vxn_app::sync::sync_partner_clap_id(id) {
            if self.shared.params.get(sync_id) >= 0.5 {
                return write!(
                    writer,
                    "{}",
                    vxn_app::sync::synced_label_for(desc, value as f32)
                );
            }
        }
        format_value(desc, value, writer)
    }

    fn text_to_value(&mut self, _param_id: ClapId, text: &CStr) -> Option<f64> {
        let s = text.to_str().ok()?;
        // Take the leading numeric token (ignore any unit suffix).
        let num: String = s
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        num.parse::<f64>().ok()
    }

    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        // Inactive-plugin / main-thread param flush. Host param events become
        // `HostEvent::ParamAutomation`; the controller folds them into
        // `SharedParams` on tick (writing the same atomic the old `LocalParams`
        // mirror used to publish to). The editor's idle drain consumes the
        // emitted ViewEvents — we don't drop them here.
        let host_tx = lock_mut(&self.controller).host_sender();
        for event in input {
            if let Some(CoreEventSpace::ParamValue(e)) = event.as_core_event() {
                if let Some(pid) = e.param_id() {
                    let id = ParamId::new(pid.get() as usize);
                    let plain = e.value() as f32;
                    let _ = host_tx.try_send(HostEvent::ParamAutomation { id, plain });
                }
            }
        }
        lock_mut(&self.controller).tick();
    }
}

impl PluginAudioProcessorParams for VxnAudioProcessor<'_> {
    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        for event in input {
            if let Some((idx, value)) = self.local.apply_input(event) {
                self.synth.set_param(idx, value);
            }
        }
        self.local.publish(&self.shared.params);
    }
}

// ── State save / restore ──────────────────────────────────────────────────────

impl PluginStateImpl for VxnMainThread<'_> {
    fn save(&mut self, output: &mut OutputStream) -> Result<(), PluginError> {
        // Snapshot via the `ParamModel` trait so the serialiser is whatever
        // the model defines — same canonical blob as before (the engine's
        // `PluginState` write) routed through the trait surface for symmetry
        // with `load`.
        let blob = ParamModel::snapshot_bytes(&*self.shared.params);
        output
            .write_all(&blob)
            .map_err(|_| PluginError::Message("state save failed"))
    }

    fn load(&mut self, input: &mut InputStream) -> Result<(), PluginError> {
        // Read the whole blob, hand it to the controller; the controller's
        // tick applies it through the model and emits the matching ViewEvents
        // (the editor's idle drain picks them up).
        let mut blob = Vec::new();
        input
            .read_to_end(&mut blob)
            .map_err(|_| PluginError::Message("state read failed"))?;
        let host_tx = lock_mut(&self.controller).host_sender();
        let _ = host_tx.try_send(HostEvent::StateLoaded { blob });
        lock_mut(&self.controller).tick();
        Ok(())
    }
}

clack_export_entry!(SinglePluginEntry<VxnPlugin>);

// Keep the param tables referenced so the linker never drops them in a thin-LTO
// cdylib build (defensive; also a compile-time check the import is used).
#[used]
static _PARAM_COUNT: usize = TOTAL_PARAMS;
