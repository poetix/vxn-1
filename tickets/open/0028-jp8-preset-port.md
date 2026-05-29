---
id: "0028"
title: Jupiter-8 factory preset port
priority: low
created: 2026-05-28
epic: E007
---

## Summary

The fun payload of E007: author a curated set of **Jupiter-8-flavoured** factory
presets (~16–24 patches across categories, plus a few Dual/Split performances)
in the 0024 TOML format, and record the JP-8 → VXN1 mapping and its divergences.
This is **honest archetypes, not a ROM clone** — we have no original JP-8 patch
data, and the platforms differ (see the mapping table). Framed as "Jupiter
character", not "the factory bank". Builds on the format (0024) and bank infra
(0025); auditioned via the browser (0027). Decisions:
[ADR 0005](../../adrs/0005-vxn1-presets.md) §Consequences.

## Acceptance criteria

- [x] `presets/factory/Jovial Presets/` (18 patches) + `Jovial Performances/`
  (4 performances) populated; each with `tags = ["jp8", ...]` and a `meta.comment`
  noting its hardware inspiration **and** the main divergence. *(Folder names
  diverge from the `jp8/` placeholder: the embed walker uses the directory as the
  browser **category**, and patches/performances are split into two folders since
  the 0027 browser doesn't otherwise distinguish kind. See "Ported set" below.)*
- [x] Coverage across archetypes: **Brass** ×4, **Strings/Ensemble** ×3,
  **Pad/Sweep** ×2, **Sync Lead** ×3, **Bass** (incl. a Twin fat-fifths bass) ×3,
  **Bell/Pluck/FM** ×2, **Poly Keys/Clav** ×2, plus a **Split** performance
  (bass / lead) and a **Dual** layered stack (brass over strings).
- [x] All pass 0025's CI round-trip (parse + zero warnings) —
  `cargo test -p vxn-engine factory`.
- [x] The mapping/divergence table below is reproduced; the per-preset
  translation is recorded in "Ported set" below and inline in each file's
  `meta.comment`.
- [ ] Sanity-listened in a host: each preset plays and is recognisably its
  archetype across a couple of octaves; unison/Twin patches don't clip (level
  compensation is in the engine). **Outstanding — manual audition required**
  ([[ask-before-screen-capture]]).

## JP-8 → VXN1 mapping

| JP-8 feature | VXN1 target | Notes / divergence |
| --- | --- | --- |
| VCO-1 waves (saw / pulse / square) | `osc1_wave` Saw/Pulse | "square" = Pulse at `osc1_pw = 0.5` |
| VCO-2 waves (saw / pulse / tri / sine) | `osc2_wave` | VXN1 osc2 adds Sine/Tri natively — good fit |
| VCO-2 = **noise** | `noise_level` (+ `noise_color`) | noise is a **mixer source** in VXN1, not an osc-2 wave |
| VCO range / octave switches | `osc{1,2}_octave`, `osc{1,2}_coarse` (±7 st) | coarse can't span a full octave; use octave for ±12 |
| VCO-2 **Low-Freq** mode | (no direct map) | VXN1 has dedicated LFOs; emulate slow movement with LFO1/2 instead. Documented loss. |
| **XMOD** (VCO-1 freq ← VCO-2) | `cross_mod_type = "FM"` + `cross_mod_amount` | exp2/semitone FM; aliases for non-sine carriers (by design, [[vxn1-crossmod-pm-aliasing-by-design]]) — lean on `oversample` |
| **VCO SYNC** | `cross_mod_type = "Sync"` | band-limited (E006/0020). **Mutually exclusive** with FM in VXN1 (JP-8 had separate switches) — pick the dominant one per patch |
| Source mixer (VCO-1 / VCO-2) | `osc1_level` / `osc2_level` | ring (`ring_level`) is a VXN1 extra, use sparingly |
| **HPF** 4-step (0/1/2/3) | `hpf_cutoff` (Hz) | continuous in VXN1; map steps → ~`20 / 120 / 360 / 1000` Hz (20 ≈ off). Tune by ear. |
| VCF cutoff / reso | `cutoff` / `resonance` | OTA ladder is the same IR3109 family — strong fidelity |
| VCF -12 / -24 dB | `filter_slope` 12 dB / 24 dB | direct |
| VCF env amount + **ENV polarity** | `cutoff_env_depth` (bipolar) | negative depth = inverted env (replaces the polarity switch) |
| VCF key-follow | `filter_key_track` (on/off) | VXN1 is 1 oct/oct over C4, on/off only (JP-8 had a pot) — coarser |
| VCF ← LFO | `cutoff_lfo1_depth` / `cutoff_lfo2_depth` | choose LFO1 (per-voice) or LFO2 (global) per intent |
| VCA ← ENV-2 / gate | hardwired Env2; `amp_env_bypass` for gate | direct |
| ENV-1 / ENV-2 (ADSR) | `env1_*` / `env2_*` | Env1 = filter env, Env2 = amp env (VXN1 convention) |
| ENV shape | `env{1,2}_shape` Lin/Exp | JP-8 is roughly exponential — prefer Exp for amp |
| LFO (sine/saw/square/random) + rate | `lfo_shape` / `lfo_rate` (LFO1) and/or `lfo2_*` | VXN1 shapes are a superset (Tri, Saw±, S&H) |
| LFO **per-section** | LFO is **per-voice** (LFO1) or **global** (LFO2) | per-note vibrato → LFO1; patch-wide sweep → LFO2. Modern divergence ([[vxn1-feature-roadmap]]) |
| **Bender** lever (→ VCO / VCF depth) | `pitch_wheel_depth`; mod-wheel panel | JP-8 had no mod wheel; route expressive depth via `pitch_wheel_depth` and the `mod_wheel_*` panel |
| **Unison / Solo** | `assign_mode` Unison / Solo / Twin; `unison_detune` | VXN1 = 16 voices vs JP-8's 8; Twin is a VXN1 extra |
| Whole / Dual / Split | `key_mode` + `split_point` (Performance only) | direct (ADR 0003) |
| Aftertouch / **velocity** | leave `vel_cutoff_depth = 0` | JP-8 hardware had no velocity — keep authentic ([[vxn1-status]]) |
| (no onboard chorus) | `chorus_on = false` by default | chorus is Juno, not JP-8 — off for authenticity; a tasteful variant may enable it, noted in `comment` |
| Analog drift / VCO instability | (none) | no global drift model; `unison_detune` + phase decorrelation approximate thickness only. Documented loss. |

## Notes

- **Authoring stance:** build by ear from documented JP-8 synthesis recipes and
  the archetype, not from any claimed original patch data. Name presets
  descriptively (e.g. "Jupiter Brass", "Sync Lead", "Octave Bass") — avoid
  implying a specific ROM slot.
- **Sparse files:** only write params that deviate from default; lean on the
  format's default-fill so the JP-8 set auto-adopts engine default improvements.
- **Mutually-exclusive osc interaction** is the sharpest divergence: a JP-8 patch
  using sync *and* cross-mod must collapse to one `cross_mod_type` — choose the
  audibly dominant behaviour and note it in the patch comment.
- **Oversample:** sync/FM patches alias by design on non-sine carriers; set a
  higher `oversample` on those presets (it's a global/per-performance param) and
  note it.
- Could grow into its own content epic if it expands past ~24 patches or wants a
  proper documented "JP-8 sound design" write-up — keep it one ticket for now.
- This is the deliverable to **listen to**, not just test. Ask before any GUI /
  screen capture ([[ask-before-screen-capture]]).

## Ported set

Curated from the documented 64-patch JP-8 factory bank (group.patch numbering,
e.g. `35` = group 3 / patch 5). Originals are **archetypal panel recipes**, not
ROM dumps. Names are whimsically suggestive of the originals (trademark-avoidance,
per the authoring stance). Full per-param translation lives in each file's `[patch]`
table and `meta.comment`.

**`Jovial Presets/`** (18 patches):

| Orig | VXN-1 name | Archetype | Defining translation |
| --- | --- | --- | --- |
| 35 Lo Brass | Bold as Brass | Brass | dual saw −1 oct, 24 dB, Env1→cutoff swell |
| 36 Hi Brass | Top Brass | Brass | brighter, faster Env1 bite, higher cutoff |
| 38 Synth Brass | Brass Tacks | Brass | pulse/saw, slow PWM on LFO 1 |
| 37 S-H Brass | Brass & Hold | Brass | `lfo_shape="S&H"` → `cutoff_lfo1_depth` |
| 33 Hi Strings | Highly Strung | Strings | Unison ×detune, slow swell, vibrato |
| 34 Mellow Strings | Mellow Drama | Strings | darker/lazier Unison pad |
| 56 Choir Voices | Vox Populi | Choir | twin pulse → `filter_mode="BP"` (formant sub) |
| 14 Sync Sweep | Clean Sweep | Pad | Sync + Env1 → wide osc2-pitch sweep |
| 66 Solar Winds | Solar Sails | Pad | pink noise + slow LFO 1 → cutoff |
| 16 Sync Lead | Out of Sync | Lead | Twin, Sync, Env1 sweep, pitch-wheel 7 st |
| 11 Neg Sync | Sync or Swim | Lead | Sync, **negative** osc2-pitch-env depth |
| 18 Duke Lead | Dukes Up | Lead | Solo legato glide, pitch-wheel 12 st |
| 87 Upright Bass | Downright Bass | Bass | saw+tri, fast filter pluck, key-track |
| 71 Fat Fifths | Phat Fifths | Bass | osc2 +7 st, **Twin** fat stack |
| 46 Organ Bass Pedals | Pedal Pusher | Bass | sine+sub-tri, full sustain |
| 12 Neg Pluck | Pluck of the Draw | Pluck | negative osc2-pitch chirp + bright bite |
| 21 Clav | Clavicle | Keys | narrow pulse, key-track, fast Env1 |
| 22 Harpsichord | Bach to Basics | Keys | twin narrow pulse, fast decay, no sustain |

**`Jovial Performances/`** (4 performances):

| Orig basis | VXN-1 name | Key mode | Notes |
| --- | --- | --- | --- |
| 23 Echo Piano | Echo Chamber | Whole | FM e-piano + global slapback delay (the echo a Patch can't carry); chorus off |
| 57 Tomita Chime | Isao Tinkle | Dual | Upper = static FM bell (sine+sine at +7 st, modulator out of mix); Lower = sine with inverted-Env-1 pitch ping. Splits body from transient so the FM ratio stays put |
| Brass + Strings | Strings Attached | Dual | Top Brass (Upper) over Highly Strung (Lower); master trimmed; chorus off |
| Bass / Lead split | Great Divide | Split @ C3 (48) | Sync lead (Upper) / Solo bass (Lower); ping-pong delay; `oversample="4x"` |

**Deliberate divergences in this set:**

- Folder = browser **category** (one level deep in the embed walker), so the bank
  ships as two groups ("Jovial Presets", "Jovial Performances") rather than the
  `jp8/` placeholder; archetype is carried in `tags`.
- **Oversample** is a global/performance param, so sync/FM **patches** can only
  *recommend* raising it (noted in their comments); only the performances bake it.
- JP-8 **negative/inverted envelopes** map to **negative** route depths
  (`osc2_pitch_env_depth`, `cutoff_env_depth`), replacing the polarity switch.
- No analog **VCO drift** model; thickness is unison/Twin detune + phase
  decorrelation only.
