//! Thread-safe parameter store shared between the main thread (host queries,
//! state load, future UI) and the audio thread.
//!
//! Values are plain `f32` stored as bits in `AtomicU32`. The audio thread takes
//! a snapshot into the engine's [`ParamValues`] each `process` call; param
//! events arriving during processing update both the store and the engine.

use std::sync::atomic::{AtomicU32, Ordering};
use vxn_engine::{PARAMS, ParamId, ParamValues};

pub struct SharedParams {
    values: [AtomicU32; ParamId::COUNT],
}

impl Default for SharedParams {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedParams {
    pub fn new() -> Self {
        Self { values: std::array::from_fn(|i| AtomicU32::new(PARAMS[i].default.to_bits())) }
    }

    #[inline]
    pub fn get(&self, index: usize) -> f32 {
        f32::from_bits(self.values[index].load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set(&self, index: usize, value: f32) {
        if index < ParamId::COUNT {
            let clamped = PARAMS[index].clamp(value);
            self.values[index].store(clamped.to_bits(), Ordering::Relaxed);
        }
    }

    /// Copy the whole store into an engine [`ParamValues`].
    pub fn snapshot_into(&self, params: &mut ParamValues) {
        for i in 0..ParamId::COUNT {
            params.set_index(i, self.get(i));
        }
    }
}
