//! VXN1 CLAP plugin shell (clack).
//!
//! Wires the framework-agnostic [`Synth`] engine to CLAP: a stereo output port,
//! a CLAP note input, the full parameter set, and state save/restore. The UI is
//! a separate crate added later via the `gui` extension.

mod shared;

use clack_extensions::state::{PluginState, PluginStateImpl};
use clack_extensions::{audio_ports::*, note_ports::*, params::*};
use clack_plugin::events::Match;
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};
use shared::SharedParams;
use std::ffi::CStr;
use std::fmt::Write as _;
use std::io::{Read, Write as _};
use vxn_engine::{PARAMS, ParamId, ParamKind, Synth};

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
            .register::<PluginState>();
    }
}

impl DefaultPluginFactory for VxnPlugin {
    fn get_descriptor() -> PluginDescriptor {
        use clack_plugin::plugin::features::*;
        PluginDescriptor::new("labs.vulpus.vxn1", "VXN1")
            .with_features([SYNTHESIZER, INSTRUMENT, STEREO])
    }

    fn new_shared(_host: HostSharedHandle) -> Result<VxnShared, PluginError> {
        Ok(VxnShared { params: SharedParams::new() })
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        shared: &'a VxnShared,
    ) -> Result<VxnMainThread<'a>, PluginError> {
        Ok(VxnMainThread { shared })
    }
}

/// Data shared between the main and audio threads.
pub struct VxnShared {
    params: SharedParams,
}

impl PluginShared<'_> for VxnShared {}

/// Main-thread state (parameter queries, state save/restore).
pub struct VxnMainThread<'a> {
    shared: &'a VxnShared,
}

impl<'a> PluginMainThread<'a, VxnShared> for VxnMainThread<'a> {}

/// Audio-thread processor: owns the synth engine and render scratch.
pub struct VxnAudioProcessor<'a> {
    synth: Synth,
    shared: &'a VxnShared,
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
        vxn_engine::enable_flush_to_zero();
        let max = audio_config.max_frames_count as usize;
        Ok(Self {
            synth: Synth::new(audio_config.sample_rate as f32),
            shared,
            scratch_l: vec![0.0; max],
            scratch_r: vec![0.0; max],
        })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        // Pick up any parameter changes made on the main thread / UI.
        self.shared.params.snapshot_into(self.synth.params_mut());

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
        let shared = &self.shared.params;
        let l = &mut self.scratch_l[..frames];
        let r = &mut self.scratch_r[..frames];

        for event_batch in events.input.batch() {
            for event in event_batch.events() {
                apply_event(synth, shared, event);
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

        Ok(ProcessStatus::Continue)
    }

    fn reset(&mut self) {
        self.synth.reset();
    }
}

impl VxnAudioProcessor<'_> {}

/// Apply a single input event to the engine. Free function so the caller can
/// keep the synth and scratch buffers borrowed disjointly.
fn apply_event(synth: &mut Synth, shared: &SharedParams, event: &UnknownEvent) {
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
        Some(CoreEventSpace::ParamValue(e)) => {
            if let Some(pid) = e.param_id() {
                let idx = pid.get() as usize;
                let value = e.value() as f32;
                shared.set(idx, value);
                synth.set_param(idx, value);
            }
        }
        _ => {}
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

fn format_value(id: ParamId, value: f64, writer: &mut ParamDisplayWriter) -> std::fmt::Result {
    let desc = id.desc();
    match desc.kind {
        ParamKind::Enum { variants } => {
            let i = (value.round() as usize).min(variants.len().saturating_sub(1));
            write!(writer, "{}", variants[i])
        }
        ParamKind::Bool => write!(writer, "{}", if value >= 0.5 { "On" } else { "Off" }),
        ParamKind::Int { unit } => write!(writer, "{} {}", value.round() as i64, unit),
        ParamKind::Float { unit, .. } => {
            if unit.is_empty() {
                write!(writer, "{value:.3}")
            } else {
                write!(writer, "{value:.2} {unit}")
            }
        }
    }
}

impl PluginMainThreadParams for VxnMainThread<'_> {
    fn count(&mut self) -> u32 {
        ParamId::COUNT as u32
    }

    fn get_info(&mut self, param_index: u32, info: &mut ParamInfoWriter) {
        let Some(id) = ParamId::from_index(param_index as usize) else {
            return;
        };
        let desc = id.desc();
        let stepped = !matches!(desc.kind, ParamKind::Float { .. });
        let mut flags = ParamInfoFlags::IS_AUTOMATABLE;
        if stepped {
            flags |= ParamInfoFlags::IS_STEPPED;
        }
        info.set(&ParamInfo {
            id: ClapId::new(id.index() as u32),
            flags,
            cookie: Default::default(),
            name: desc.label.as_bytes(),
            module: b"",
            min_value: desc.min as f64,
            max_value: desc.max as f64,
            default_value: desc.default as f64,
        });
    }

    fn get_value(&mut self, param_id: ClapId) -> Option<f64> {
        let idx = param_id.get() as usize;
        if idx < ParamId::COUNT {
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
        match ParamId::from_index(param_id.get() as usize) {
            Some(id) => format_value(id, value, writer),
            None => Err(std::fmt::Error),
        }
    }

    fn text_to_value(&mut self, _param_id: ClapId, text: &CStr) -> Option<f64> {
        let s = text.to_str().ok()?;
        // Take the leading numeric token (ignore any unit suffix).
        let num: String = s.trim().chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
        num.parse::<f64>().ok()
    }

    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        for event in input {
            if let Some(CoreEventSpace::ParamValue(e)) = event.as_core_event() {
                if let Some(pid) = e.param_id() {
                    self.shared.params.set(pid.get() as usize, e.value() as f32);
                }
            }
        }
    }
}

impl PluginAudioProcessorParams for VxnAudioProcessor<'_> {
    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        let synth = &mut self.synth;
        let shared = &self.shared.params;
        for event in input {
            apply_event(synth, shared, event);
        }
    }
}

// ── State save / restore ──────────────────────────────────────────────────────

impl PluginStateImpl for VxnMainThread<'_> {
    fn save(&mut self, output: &mut OutputStream) -> Result<(), PluginError> {
        for i in 0..ParamId::COUNT {
            output.write_all(&self.shared.params.get(i).to_le_bytes())?;
        }
        Ok(())
    }

    fn load(&mut self, input: &mut InputStream) -> Result<(), PluginError> {
        for i in 0..ParamId::COUNT {
            let mut buf = [0u8; 4];
            input.read_exact(&mut buf)?;
            self.shared.params.set(i, f32::from_le_bytes(buf));
        }
        Ok(())
    }
}

clack_export_entry!(SinglePluginEntry<VxnPlugin>);

// Keep the param table referenced so the linker never drops it in a thin-LTO
// cdylib build (defensive; also a compile-time check the import is used).
#[used]
static _PARAM_COUNT: usize = PARAMS.len();
