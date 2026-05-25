//! VXN1 CLAP plugin shell (clack).
//!
//! Wires the framework-agnostic [`Synth`] engine to CLAP: a stereo output port,
//! a CLAP note input, the full parameter set, state save/restore, and the
//! `vxn-ui` Vizia editor via the `gui` extension. Parameters bridge the engine,
//! the host and the UI through `vxn_engine::SharedParams`; [`local::LocalParams`]
//! diffs that store to echo UI edits to the host without echoing host
//! automation back (see its module docs).

mod gui;
mod local;

use clack_extensions::gui::PluginGui;
use clack_extensions::state::{PluginState, PluginStateImpl};
use clack_extensions::{audio_ports::*, note_ports::*, params::*};
use clack_plugin::events::Match;
use clack_plugin::events::event_types::TransportFlags;
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};
use local::LocalParams;
use std::ffi::CStr;
use std::fmt::Write as _;
use std::sync::Arc;
use vxn_engine::{
    ParamDesc, ParamKind, PluginState as VxnState, SharedParams, Synth, TOTAL_PARAMS,
    desc_for_clap_id, module_for_clap_id,
};

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
            .register::<PluginGui>();
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
        _host: HostMainThreadHandle<'a>,
        shared: &'a VxnShared,
    ) -> Result<VxnMainThread<'a>, PluginError> {
        Ok(VxnMainThread {
            shared,
            params: LocalParams::new(&shared.params),
            gui: None,
        })
    }
}

/// Data shared between the main and audio threads. The parameter store lives
/// behind an `Arc` so the editor (created on the main thread) can hold a clone.
pub struct VxnShared {
    params: Arc<SharedParams>,
}

impl PluginShared<'_> for VxnShared {}

/// Main-thread state (parameter queries, state save/restore). Holds a local
/// parameter mirror used when the host flushes params while the plugin is
/// inactive.
pub struct VxnMainThread<'a> {
    shared: &'a VxnShared,
    params: LocalParams,
    /// The live editor window, while the GUI is open.
    gui: Option<vxn_ui::EditorHandle>,
}

impl<'a> PluginMainThread<'a, VxnShared> for VxnMainThread<'a> {}

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
                                synth.set_mod_wheel(d2 as f32 / 127.0);
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
        match desc_for_clap_id(param_id.get() as usize) {
            Some(desc) => format_value(desc, value, writer),
            None => Err(std::fmt::Error),
        }
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
        // Inactive-plugin param flush (main thread): fold host changes into the
        // mirror and publish so `get_value`/the UI observe them.
        for event in input {
            self.params.apply_input(event);
        }
        self.params.publish(&self.shared.params);
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
        // One canonical serializer (both per-patch blocks + global + the
        // non-automatable shared state); reused by future preset management.
        self.shared
            .params
            .to_state()
            .write(output)
            .map_err(|_| PluginError::Message("state save failed"))
    }

    fn load(&mut self, input: &mut InputStream) -> Result<(), PluginError> {
        let state = VxnState::read(input).map_err(|_| PluginError::Message("state load failed"))?;
        self.shared.params.restore_from(&state);
        Ok(())
    }
}

clack_export_entry!(SinglePluginEntry<VxnPlugin>);

// Keep the param tables referenced so the linker never drops them in a thin-LTO
// cdylib build (defensive; also a compile-time check the import is used).
#[used]
static _PARAM_COUNT: usize = TOTAL_PARAMS;
