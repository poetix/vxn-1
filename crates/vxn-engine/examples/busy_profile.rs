//! Profiling harness: a deliberately "busy" patch for samply/perf.
//!
//! Dual key mode → each layer reads its own params: Upper runs hard **sync**,
//! Lower runs through-zero **phase mod**, both at 4× oversample with FX, high
//! resonance, noise, and the fixed mod routes (Env→cutoff, LFO→pitch/PWM) all
//! doing work. 16 voices held at sustain. Run under a sampler:
//!
//!   cargo build --release --example busy_profile -p vxn-engine
//!   samply record ./target/release/examples/busy_profile

use vxn_engine::{
    GlobalParam, KeyMode, Layer, PatchParam, Synth, global_clap_id, patch_clap_id,
};

const SR: f32 = 48_000.0;
const FRAMES: usize = 512;
const ITERS: usize = 30_000;

fn main() {
    let iters = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(ITERS);
    let mut s = Synth::new(SR);
    s.set_key_mode(KeyMode::Dual); // each layer reads its own params

    let g = |p: GlobalParam, v: f32| (global_clap_id(p), v);
    for (id, v) in [
        g(GlobalParam::Oversample, 2.0), // 4×
        g(GlobalParam::ChorusOn, 1.0),
        g(GlobalParam::DelayOn, 1.0),
    ] {
        s.set_param(id, v);
    }

    // Common per-layer patch: high resonance, noise, both osc levels up, the
    // three fixed mod routes wired so they all evaluate every block.
    for layer in [Layer::Upper, Layer::Lower] {
        let pp = |p: PatchParam| patch_clap_id(layer, p);
        for (id, v) in [
            (pp(PatchParam::Resonance), 0.9),
            (pp(PatchParam::NoiseLevel), 0.2),
            (pp(PatchParam::Osc1Level), 0.8),
            (pp(PatchParam::Osc2Level), 0.8),
            (pp(PatchParam::Osc2Coarse), 7.0), // detuned slave for sync/PM motion
            (pp(PatchParam::CutoffEnvSrc), 1.0), // Env 1 → cutoff
            (pp(PatchParam::CutoffEnvDepth), 24.0),
            (pp(PatchParam::PitchLfoSrc), 1.0), // LFO 1 → pitch
            (pp(PatchParam::PitchLfoDepth), 3.0),
            (pp(PatchParam::PwmLfoSrc), 2.0), // LFO 2 → PWM
            (pp(PatchParam::PwmLfoDepth), 0.3),
        ] {
            s.set_param(id, v);
        }
    }
    // Upper → hard sync, Lower → phase mod.
    s.set_param(patch_clap_id(Layer::Upper, PatchParam::CrossModType), 1.0);
    s.set_param(patch_clap_id(Layer::Lower, PatchParam::CrossModType), 2.0);
    s.set_param(patch_clap_id(Layer::Lower, PatchParam::CrossModAmount), 0.5);

    for n in 48..64u8 {
        s.note_on(n, 1.0);
    }

    let mut l = vec![0.0; FRAMES];
    let mut r = vec![0.0; FRAMES];
    // Warm past the attack into steady state.
    for _ in 0..40 {
        s.process(&mut l, &mut r);
    }

    let mut acc = 0.0f32;
    for _ in 0..iters {
        s.process(&mut l, &mut r);
        acc += l[0]; // defeat dead-code elimination
    }
    std::hint::black_box(acc);
}
