# ADR 0007 — Separable UI / engine / controller (MVC, reusable abstractions)

- **Status:** Accepted
- **Date:** 2026-05-30
- **Scope:** Cross-cutting. Refactor the plugin's data flow into an explicit
  MVC layout: **Model** = shared parameter store + non-automatable state,
  **Controller** = single arbiter of intent (UI edits, host automation, state
  loads, preset operations), **View** = pluggable editor backend. Extracts the
  controller + event model into a new `vxn-app` crate, so a future VXN-2 can
  reuse the architecture with a different engine and a different UI.

## Context

Two pressures converged.

**Vizia is hitting its ceiling.** The shipped editor has accumulated a
list of toolkit-imposed bugs that the codebase works around but cannot
fix at root: clicks dropped on tiny cursor drift
([[vxn1-vizia-no-click-slop]]), continuous DAW automation re-laying out the
window every frame and synthesising `MouseMove` events that strip dragging
controls of input ([[vxn1-vizia-automation-relayout-input-stomp]]),
absolute-positioned overlays eating clicks across their full stretch
([[vxn1-vizia-absolute-stretch-overlay]]). A WebView prototype (now deleted)
confirmed the input-handling bugs disappear under a browser engine; only the
host's reserved keys (Space etc.) remain — a hosting limit, not a toolkit
one.

**The codebase mixes responsibilities.** UI control callbacks today write
directly to `SharedParams`, raise gestures inline, parse host events at the
audio-processor boundary, and own preset-load logic across both the engine
crate and the editor. There is no single place to ask "what happened next"
when a parameter changes. That is fine for one synth and one UI; it does
not scale to a second engine sharing the same shell, or to a second UI
sharing the same engine.

VXN-2 is on the roadmap (ADR 0002): a different oscillator topology,
different routing, but the same plugin shell, preset format, host
integration and (likely) a similar control surface idiom. The reuse target
is real, not speculative.

This ADR records the architecture that lets us migrate the editor without
re-deriving the data flow each time, and that gives VXN-2 a head start.

## Decision

### 1. Three roles, with sharp boundaries

- **Model** — the live parameter and shell state. Owned by
  `SharedParams` (parameter atomics, ADR 0001 §6) plus the non-automatable
  shared state (key mode, split point, ADR 0003 §3). Audio-thread visible
  via atomics; main-thread visible via the same struct. **No business
  logic lives here.** The model only stores values; it does not decide
  what to do when one changes.

- **Controller** — a main-thread struct that **mediates every non-audio
  mutation of the model**. Inputs:

  - UI intent: "edit param", "begin/end gesture", "load preset",
    "rename preset", "set key mode", "switch edit layer", etc.
  - Host events: parameter automation, state save/restore, transport.
  - Lifecycle ticks: idle polls (controller drains pending work).

  The controller decides whether each intent translates to a model write,
  a side-effecting IO call (load/save TOML), a host echo (gesture
  brackets + param events), and/or a view update for the editor to
  re-render.

  **The audio thread never goes through the controller.** It continues to
  read atomics directly from `SharedParams`. This preserves the existing
  real-time path (ADR 0001 §6) untouched.

- **View** — a pluggable editor backend. The current Vizia faceplate is
  one; the next WebView editor will be another. The view receives a
  stream of `ViewEvent`s from the controller (loaded a preset → repaint
  every control; host moved a knob → repaint that one), emits `UiEvent`s
  to the controller (user dragged a fader), and owns nothing else.

### 2. Crate split: introduce `vxn-app`

The current layout is `vxn-dsp` (audio primitives) → `vxn-engine` (synth +
model + presets) → `vxn-ui` (Vizia editor) → `vxn-clap` (host shell).

Insert a new crate **`vxn-app`** between the engine and the editor:

```text
vxn-dsp ─┐
         ├─→ vxn-engine ──→ vxn-app ──→ {vxn-ui-vizia, vxn-ui-web}
         │     (model)        (ctrl)       (views, pluggable)
         │                       │
         └─────────────────────→ vxn-clap (shell; depends on app + chosen view)
```

`vxn-app` owns:

- The `ParamModel` and `ParamDescriptor` traits — the surface the
  controller programs against, decoupled from `SharedParams`'s concrete
  type so VXN-2 can plug in its own param store.
- The `EditorBackend` trait — a pluggable view's `open` / `close` /
  `push_view_event` surface, so the clack shell hosts whichever editor
  is compiled in without `cfg!`-spaghetti at the shell layer.
- The `Controller` struct — generic over `M: ParamModel`, holds the
  channels, drains intents, emits view events, manages preset IO,
  arbitrates gestures.
- The `UiEvent` / `HostEvent` / `ViewEvent` enums.

`vxn-engine` stays audio-pure: `SharedParams` lives there, and it
**implements** `ParamModel` (the trait lives in `vxn-app`, but the impl
can live in the engine — orphan rules allow it because the engine
depends on app for the trait). `SharedParams` knows nothing about the
controller.

`vxn-ui` becomes `vxn-ui-vizia` (rename) and adds an
`impl EditorBackend for ViziaEditor`. The future `vxn-ui-web` does the
same. `vxn-clap` depends on whichever editor crate the build selects via
a cargo feature, talking only through `EditorBackend`.

### 3. Event flow: channels + structured enums

UI and controller communicate over **bounded mpsc channels** carrying
structured enums:

```rust
// vxn-app, sketch
pub enum UiEvent {
    SetParam      { id: ParamId, plain: f32 },
    SetParamNorm  { id: ParamId, norm: f32 },
    BeginGesture  { id: ParamId },
    EndGesture    { id: ParamId },
    LoadPreset    { source: PresetSource },
    SavePreset    { name: String, folder: Option<String> },
    RenamePreset  { path: PathBuf, new_name: String },
    DeletePreset  { path: PathBuf },
    SetKeyMode    { mode: KeyMode },
    SetSplitPoint { note: u8 },
    SetEditLayer  { layer: Layer },
    // … (one variant per UI intent)
}

pub enum HostEvent {
    ParamAutomation { id: ParamId, plain: f32 },
    StateLoaded     { snapshot: PluginState },
    Tempo           { bpm: f32 },
}

pub enum ViewEvent {
    ParamChanged   { id: ParamId, plain: f32, norm: f32, display: String },
    PresetLoaded   { meta: Meta, source: PresetSource, warnings: Vec<String> },
    PresetCorpusChanged,    // re-read the user dir
    KeyModeChanged { mode: KeyMode },
    Status         { line: String },
}
```

- UI → controller: `mpsc::Sender<UiEvent>` (UI side) →
  `mpsc::Receiver<UiEvent>` (controller side). UI handlers do not touch
  `SharedParams`; they post intents.
- Controller → UI: `mpsc::Sender<ViewEvent>` → drained by the editor on
  idle / animation tick.
- Host → controller: the clack shell extracts events from clack's CLAP
  `Events` stream into `HostEvent`s and posts them.

The controller's `tick(&mut self)` (called on idle) drains both inbound
queues, applies their effects to the model, runs IO (preset load/save),
and posts `ViewEvent`s.

**Why channels, not direct trait calls.** Three reasons:

- The same controller drives a Vizia view (callback-driven) and a
  WebView (IPC-message-driven) without either knowing about the other.
- A headless test harness becomes a single `Controller<M>` + `Vec`s of
  events — no UI toolkit pulled in. Preset round-trip, gesture
  accounting, automation echo become unit-testable.
- A future asynchronous IO step (e.g. preset thumbnail rendering) drops
  into the controller's tick loop without restructuring its callers.

**Audio thread is unchanged.** It reads atomics from `SharedParams`. The
controller folds host param events into the model on the main thread; the
audio thread observes them on its next process block. The existing
`LocalParams` diff (vxn-clap's UI-edit echo path) collapses into the
controller — it becomes the controller's "publish what changed since last
process" step.

### 4. Trait surface (sketch)

```rust
// vxn-app
pub trait ParamModel: Send + Sync {
    fn total(&self) -> usize;
    fn get(&self, id: ParamId) -> f32;
    fn set(&self, id: ParamId, plain: f32);
    fn get_normalized(&self, id: ParamId) -> f32;
    fn set_normalized(&self, id: ParamId, norm: f32);
    fn gesture(&self, id: ParamId) -> bool;
    fn set_gesture(&self, id: ParamId, on: bool);
    fn descriptor(&self, id: ParamId) -> Option<&dyn ParamDescriptor>;
}

pub trait ParamDescriptor {
    fn label(&self) -> &str;
    fn min(&self) -> f32;
    fn max(&self) -> f32;
    fn default(&self) -> f32;
    fn to_fader(&self, plain: f32) -> f32;
    fn from_fader(&self, norm: f32) -> f32;
    fn display(&self, plain: f32) -> String;
}

pub trait EditorBackend: 'static {
    type Handle;
    fn open(parent: ParentWindow, ctx: ControllerHandle) -> Self::Handle;
    fn close(handle: &mut Self::Handle);
    /// `push_view_event` runs on the controller's thread; the backend is
    /// responsible for forwarding to its render context.
    fn push_view_event(handle: &Self::Handle, event: ViewEvent);
}
```

`ParamId` is a newtype over the existing CLAP-id `usize` — the engine's
`param_ref` / `desc_for_clap_id` continue to resolve it.

### 5. Audio-thread integrity

The audio thread continues to read `SharedParams` atomics directly; the
controller never intervenes on the audio path. The existing
`fetch_ui_changes` / `publish` / `emit` cycle in `vxn-clap`'s
`AudioProcessor` is preserved; only its main-thread counterpart
(`flush`, gesture bookkeeping) moves into the controller. The local
mirror that lets UI edits avoid being echoed back to the UI as host
automation stays — it is structurally the same problem.

### 6. View has no other state

The view's only persistent state is its own widget tree (handles,
positions, focus). All data — current preset name, browser corpus, the
"current is dirty" flag, even the edit-target layer — flows in via
`ViewEvent`. This is the invariant that makes swapping Vizia for
WebView a focused job and not a re-derivation.

The current Vizia editor mostly already obeys this for parameter
values (via the `PollAutomation` idle re-read). It does not for preset
state and the browser corpus — those are read straight from
`vxn-engine::preset_io` in the editor. Both move into the controller
during migration.

### 7. Migration phasing

This ADR captures the destination architecture. Migration is
**phased**, not big-bang.

- **Phase A (E009): Introduce the controller**, keep Vizia.
  - Land `vxn-app` with `ParamModel` + traits + `Controller` skeleton +
    event enums.
  - `SharedParams: ParamModel`.
  - Route the existing Vizia editor's writes through `UiEvent`s; route
    host events through `HostEvent`s; the controller does today's work
    (write model, emit gesture brackets, publish). Vizia's `on_idle`
    drains `ViewEvent`s and reseeds signals.
  - No new features. Behaviour-preserving refactor. Verify with the
    existing preset and host integration tests.

- **Phase B (E010): WebView synth control panel.**
  - Add `vxn-ui-web` crate with `wry`-backed WKWebView. HTML faceplate
    reaches parity with the Vizia synth controls (rows, panels, mod
    routes, voice, master). Positions taken from
    `target/vxn-layout.jsonl` (panel-level bounds dumped by the
    `layout-probe` feature) plus the in-source cell constants
    (`FADER_H`, `COL_H`, `DIAL`).
  - Reuse the **same** controller; only the `EditorBackend` impl is
    new.
  - Behind a cargo feature; flip the default once parity reached.

- **Phase C (E011): Plugin management redesign.**
  - Preset browser redesigned freely (the Vizia version never reached
    a shippable shape, ADR 0006 carries open ergonomic debt).
  - Floating NSWindow popup for text input (host kbd workaround).
  - Retire `vxn-ui-vizia`.

### 8. Reuse intent for VXN-2

VXN-2 will define its own `Synth`, its own `PatchParam` / `GlobalParam`
enums, its own param table. The reusable surface is **architectural**,
not visual.

**Carries over unchanged:**

- `vxn-app` — the controller, event enums, trait surface. The Model /
  Controller / View separation, the channel discipline, the audio-
  thread invariant.
- The CLAP shell integration in `vxn-clap` — host event extraction,
  state save/restore, GUI extension wiring against `EditorBackend`.
- The WebView **embedding plumbing** in `vxn-ui-web` — wry-based child
  WKWebView setup, parent-window scale handling, the IPC bridge
  transport (JS ↔ Rust message protocol), the floating popup for text
  input.
- The preset format + corpus IO in `vxn-engine::preset_io` (ADR 0005),
  including the controller's mediation of mutations.

**Does not carry over:** the faceplate itself. VXN-2 will likely want
control idioms VXN-1 has no place for — oscilloscopes, spectrum
displays, X/Y pads, modulation matrix grids, custom envelope editors,
waveform-shape editors driven by the engine's actual sample buffer.
These are not parameter widgets; they need engine data the present
`ParamModel` / `ParamChanged` surface does not carry. New ViewEvent
variants (waveform tap, scope buffer, meter readings) will be
defined as needed; they extend the protocol, they do not slot into the
existing one.

**The honest scope of "shared":**

- The architecture (Model / Controller / View, audio thread integrity,
  event channels, IO mediation).
- The transport (wry child WebView, IPC bridge, floating popup, host
  integration).
- The format (preset TOML + corpus rules).

**Not shared:** the faceplate's structure, its layout, its visual
language, or any assumption that "if you have a parameter you get a
fader for it". VXN-2's editor is a new HTML/CSS/JS application built
on the same scaffolding. What changes is the build cost relative to
greenfield — re-deriving the host integration, the parameter
plumbing, the preset IO and the embedding glue is the slow part, and
those parts are done.

## Consequences

- **Bigger crate count** — `vxn-app` plus `vxn-ui-web`. Worth it for the
  reuse story; one-time cost during phase A.
- **Indirection on the UI write path** — UI events traverse a channel
  before reaching the model. Latency: negligible (one main-thread hop,
  cycles not milliseconds). The audio thread is unaffected.
- **Headless testability of plugin shell logic** improves: gesture
  accounting, preset round-trip, automation echo, all unit-testable
  against a `Controller<MockModel>` without spinning up vizia or wry.
- **A second IO layer (preset corpus) now lives in the controller**, not
  the view. Browser changes (rename / move / delete) become controller
  intents → corpus reload → `PresetCorpusChanged` ViewEvent. The Vizia
  view's `reseed_browser` collapses to "redraw on
  PresetCorpusChanged".
- **vxn-clap shrinks** to: extract host events → controller; pump idle
  ticks → controller; expose `EditorBackend` to the host's GUI extension.
  The `local::LocalParams` diff stays as the audio→host echo path; it
  no longer carries UI-write logic.

## Open questions

- **Channel sizing.** mpsc bounded, but at what depth? UI burst on
  preset load is `TOTAL_PARAMS` ParamChanged events — well under 1000.
  Pick 1024 to start.
- **VXN-2 host shell shared or forked?** Likely shared `vxn-clap`
  parameterised by which engine + which app. Out of scope for this ADR;
  decide when VXN-2 starts.
- **Floating popup for text input.** Detail belongs in phase C; ADR
  notes it as a known requirement, not a decision.
