//! VXN1 controller crate (ADR 0007).
//!
//! Holds the single arbiter of non-audio model mutation — the [`Controller`] —
//! plus the trait surface a UI (`EditorBackend`) and a parameter store
//! ([`ParamModel`]) program against. Engine-agnostic: VXN-2 will plug in its
//! own `ParamModel` impl.
//!
//! Scaffold only; handlers fill in across tickets 0034–0038.

pub mod backend;
pub mod controller;
pub mod domain;
pub mod events;
pub mod model;
pub mod params;
pub mod preset;
pub mod sync;

pub use backend::EditorBackend;
pub use controller::{CHANNEL_CAPACITY, Controller, ControllerHandle, CorpusHandle, Tick};
pub use domain::{DEFAULT_SPLIT_POINT, KeyMode, Layer, PresetMeta, UNCATEGORIZED};
pub use events::{HostEvent, PresetSource, UiEvent, ViewEvent};
pub use model::{ParamId, ParamModel};
pub use params::{
    AssignMode, CrossModType, EnvSel, GLOBAL_PARAMS, GLOBAL_COUNT, GlobalParam, LfoSel,
    PATCH_COUNT, PATCH_PARAMS, ParamDesc, ParamKind, ParamRef, PatchParam, REVERB_TYPE_LABELS,
    TOTAL_PARAMS, Taper, desc_for_clap_id, global_clap_id, module_for_clap_id, param_ref,
    patch_clap_id,
};
pub use preset::{PresetCorpus, PresetLoad, PresetStore, UserFolderEntry, UserPresetEntry};
