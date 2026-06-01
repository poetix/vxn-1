//! Engine-side parameter storage. The typed param ids, descriptor table and
//! CLAP-id lookup live in [`vxn_app::params`] — see that module for the model
//! surface. This file owns only the concrete `f32` storage backing those ids:
//! per-patch values × 2 layers + the global block, with dsp-typed read helpers
//! so the audio code can resolve enum-valued params back to their `vxn_dsp`
//! types without a second match table.

use crate::reverb_macro::ReverbType;
use vxn_dsp::{AdsrShape, FilterMode, FilterSlope, LfoShape, NoiseColor, Waveform};

// Re-export the param model surface so engine call sites keep their paths.
pub use vxn_app::{
    AssignMode, CrossModType, DEFAULT_SPLIT_POINT, EnvSel, GLOBAL_COUNT, GLOBAL_PARAMS,
    GlobalParam, KeyMode, Layer, LfoSel, PATCH_COUNT, PATCH_PARAMS, ParamDesc, ParamKind, ParamRef,
    PatchParam, REVERB_TYPE_LABELS, TOTAL_PARAMS, Taper, desc_for_clap_id, global_clap_id,
    module_for_clap_id, param_ref, patch_clap_id,
};

#[inline]
fn enum_index(value: f32, max: usize) -> usize {
    (value.round() as usize).min(max)
}

/// One layer's worth of per-patch values (plain units). A **self-contained,
/// serializable unit** (ADR 0003 §6): a future single-patch preset loads
/// straight into one of these.
#[derive(Clone, Debug)]
pub struct PatchValues {
    v: [f32; PatchParam::COUNT],
}

impl Default for PatchValues {
    fn default() -> Self {
        let mut v = [0.0; PatchParam::COUNT];
        for (idx, d) in PATCH_PARAMS.iter().enumerate() {
            v[idx] = d.default;
        }
        Self { v }
    }
}

impl PatchValues {
    #[inline]
    pub fn get(&self, p: PatchParam) -> f32 {
        self.v[p.index()]
    }

    #[inline]
    pub fn get_index(&self, index: usize) -> f32 {
        self.v[index]
    }

    #[inline]
    pub fn set(&mut self, p: PatchParam, value: f32) {
        self.v[p.index()] = p.desc().clamp(value);
    }

    #[inline]
    pub fn set_index(&mut self, index: usize, value: f32) {
        if let Some(p) = PatchParam::from_index(index) {
            self.set(p, value);
        }
    }

    #[inline]
    pub fn bool(&self, p: PatchParam) -> bool {
        self.get(p) >= 0.5
    }

    pub fn osc_wave(&self, p: PatchParam) -> Waveform {
        Waveform::ALL[enum_index(self.get(p), Waveform::ALL.len() - 1)]
    }

    pub fn filter_mode(&self) -> FilterMode {
        FilterMode::ALL[enum_index(self.get(PatchParam::FilterMode), FilterMode::COUNT - 1)]
    }

    pub fn filter_slope(&self) -> FilterSlope {
        if enum_index(self.get(PatchParam::FilterSlope), 1) == 0 {
            FilterSlope::Pole2
        } else {
            FilterSlope::Pole4
        }
    }

    pub fn noise_color(&self) -> NoiseColor {
        NoiseColor::ALL[enum_index(self.get(PatchParam::NoiseColor), NoiseColor::ALL.len() - 1)]
    }

    pub fn lfo_shape(&self) -> LfoShape {
        LfoShape::ALL[enum_index(self.get(PatchParam::LfoShape), LfoShape::ALL.len() - 1)]
    }

    pub fn assign_mode(&self) -> AssignMode {
        AssignMode::from_index(enum_index(
            self.get(PatchParam::AssignMode),
            AssignMode::COUNT - 1,
        ))
    }

    pub fn legato(&self) -> bool {
        self.bool(PatchParam::Legato)
    }

    pub fn lfo_sel(&self, p: PatchParam) -> LfoSel {
        LfoSel::from_index(enum_index(self.get(p), LfoSel::COUNT - 1))
    }

    pub fn env_sel(&self, p: PatchParam) -> EnvSel {
        EnvSel::from_index(enum_index(self.get(p), EnvSel::COUNT - 1))
    }

    pub fn cross_mod_type(&self) -> CrossModType {
        CrossModType::from_index(enum_index(
            self.get(PatchParam::CrossModType),
            CrossModType::COUNT - 1,
        ))
    }

    pub fn env1_shape(&self) -> AdsrShape {
        self.adsr_shape(PatchParam::Env1Shape)
    }

    pub fn env2_shape(&self) -> AdsrShape {
        self.adsr_shape(PatchParam::Env2Shape)
    }

    fn adsr_shape(&self, p: PatchParam) -> AdsrShape {
        if enum_index(self.get(p), 1) == 0 {
            AdsrShape::Linear
        } else {
            AdsrShape::Exponential
        }
    }
}

/// The global value block (master, FX, oversample).
#[derive(Clone, Debug)]
pub struct GlobalValues {
    v: [f32; GlobalParam::COUNT],
}

impl Default for GlobalValues {
    fn default() -> Self {
        let mut v = [0.0; GlobalParam::COUNT];
        for (idx, d) in GLOBAL_PARAMS.iter().enumerate() {
            v[idx] = d.default;
        }
        Self { v }
    }
}

impl GlobalValues {
    #[inline]
    pub fn get(&self, g: GlobalParam) -> f32 {
        self.v[g.index()]
    }

    #[inline]
    pub fn get_index(&self, index: usize) -> f32 {
        self.v[index]
    }

    #[inline]
    pub fn set(&mut self, g: GlobalParam, value: f32) {
        self.v[g.index()] = g.desc().clamp(value);
    }

    #[inline]
    pub fn set_index(&mut self, index: usize, value: f32) {
        if let Some(g) = GlobalParam::from_index(index) {
            self.set(g, value);
        }
    }

    #[inline]
    pub fn bool(&self, g: GlobalParam) -> bool {
        self.get(g) >= 0.5
    }

    pub fn oversample_factor(&self) -> usize {
        match enum_index(self.get(GlobalParam::Oversample), 3) {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => 8,
        }
    }

    pub fn lfo2_shape(&self) -> LfoShape {
        LfoShape::ALL[enum_index(self.get(GlobalParam::Lfo2Shape), LfoShape::ALL.len() - 1)]
    }

    pub fn reverb_type(&self) -> ReverbType {
        ReverbType::from_index(enum_index(
            self.get(GlobalParam::ReverbType),
            ReverbType::COUNT - 1,
        ))
    }
}

/// The complete engine-side value set: two per-patch layers plus the global
/// block. Addressed typed (per layer / global) by the engine, or by CLAP id at
/// the host/UI boundary via [`Self::get_by_clap_id`] / [`Self::set_by_clap_id`].
#[derive(Clone, Debug, Default)]
pub struct ParamValues {
    pub layers: [PatchValues; Layer::COUNT],
    pub global: GlobalValues,
}

impl ParamValues {
    #[inline]
    pub fn layer(&self, layer: Layer) -> &PatchValues {
        &self.layers[layer as usize]
    }

    #[inline]
    pub fn layer_mut(&mut self, layer: Layer) -> &mut PatchValues {
        &mut self.layers[layer as usize]
    }

    #[inline]
    pub fn global(&self) -> &GlobalValues {
        &self.global
    }

    #[inline]
    pub fn global_mut(&mut self) -> &mut GlobalValues {
        &mut self.global
    }

    #[inline]
    pub fn get_by_clap_id(&self, clap_id: usize) -> f32 {
        match param_ref(clap_id) {
            Some(ParamRef::Patch(layer, p)) => self.layer(layer).get(p),
            Some(ParamRef::Global(g)) => self.global.get(g),
            None => 0.0,
        }
    }

    #[inline]
    pub fn set_by_clap_id(&mut self, clap_id: usize, value: f32) {
        match param_ref(clap_id) {
            Some(ParamRef::Patch(layer, p)) => self.layer_mut(layer).set(p, value),
            Some(ParamRef::Global(g)) => self.global.set(g, value),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_in_range() {
        let p = ParamValues::default();
        for id in 0..TOTAL_PARAMS {
            let d = desc_for_clap_id(id).unwrap();
            let val = p.get_by_clap_id(id);
            assert!(val >= d.min && val <= d.max, "{} default OOR", d.name);
        }
    }

    #[test]
    fn default_patch_keeps_gentle_vibrato() {
        let p = PatchValues::default();
        assert_eq!(p.lfo_sel(PatchParam::PitchLfoSrc), LfoSel::Lfo1);
        assert_eq!(p.get(PatchParam::PitchLfoDepth), 0.05);
        assert_eq!(p.env_sel(PatchParam::PitchEnvSrc), EnvSel::Off);
    }

    #[test]
    fn route_selectors_roundtrip() {
        let mut p = PatchValues::default();
        p.set(PatchParam::PwmLfoSrc, 2.0);
        assert_eq!(p.lfo_sel(PatchParam::PwmLfoSrc), LfoSel::Lfo2);
        p.set(PatchParam::PwmEnvSrc, 1.0);
        assert_eq!(p.env_sel(PatchParam::PwmEnvSrc), EnvSel::Env1);
        for (idx, t) in [
            (0.0, CrossModType::Off),
            (1.0, CrossModType::Sync),
            (2.0, CrossModType::Pm),
        ] {
            p.set(PatchParam::CrossModType, idx);
            assert_eq!(p.cross_mod_type(), t);
        }
    }

    #[test]
    fn clap_id_roundtrip_through_values() {
        let mut pv = ParamValues::default();
        let up = patch_clap_id(Layer::Upper, PatchParam::Cutoff);
        let lo = patch_clap_id(Layer::Lower, PatchParam::Cutoff);
        pv.set_by_clap_id(up, 1000.0);
        pv.set_by_clap_id(lo, 2000.0);
        assert_eq!(pv.layer(Layer::Upper).get(PatchParam::Cutoff), 1000.0);
        assert_eq!(pv.layer(Layer::Lower).get(PatchParam::Cutoff), 2000.0);
        assert_eq!(pv.get_by_clap_id(up), 1000.0);
        let res = patch_clap_id(Layer::Upper, PatchParam::Resonance);
        pv.set_by_clap_id(res, 5.0);
        assert_eq!(pv.get_by_clap_id(res), 1.0);
    }

    #[test]
    fn key_mode_roundtrips() {
        for m in KeyMode::ALL {
            assert_eq!(KeyMode::from_u8(m as u8), m);
        }
        assert_eq!(KeyMode::default(), KeyMode::Whole);
    }
}
