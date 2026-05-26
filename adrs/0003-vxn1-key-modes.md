# ADR 0003 — VXN1 key modes & voice assignment

- **Status:** Accepted
- **Date:** 2026-05-24
- **Scope:** How VXN1 implements the Jupiter-8 Whole / Dual / Split key modes,
  and the per-layer voice-assignment model (poly / unison / mono, with a hook
  for future arpeggiation) that rides on the same machinery. This is the ADR
  that ADR 0002 §10 deferred.

## Context

ADR 0002 listed key modes as the largest roadmap item and flagged that it needs
its own ADR before an epic, because two questions decide whether it is a
parameter-table reshuffle or a genuine engine restructure:

1. **Parameter doubling vs CLAP automation** — Dual/Split need two independent
   patches, but every automatable parameter needs a fixed CLAP id, so a
   "current-layer" multiplexer would break DAW automation.
2. **Poly-kernel global-param hoisting** — `vxn-dsp`'s poly kernels take a
   *single* `wave` / `noise_color` and hoist the match outside the lane loop
   (that hoist is what vectorises across the 16 channels). Two layers with
   different oscillator waveforms cannot share one `process()` call.

The resolving insight (worked out in discussion): stop treating key mode as an
engine special-case. Model it as **event-routing policy over two layers that
are always present**. The mode decides how the inbound MIDI stream fans out and
which parameter block each layer reads — not how the DSP graph is wired.

The Jupiter-8 itself confirms the partition: its LFO is **per-section**, not
global. The Rate section shows two LED indicators that "flash independently to
indicate the LFO speed of the Lower and Upper sections" in Dual/Split, and
"flash together" in Whole (manual p.19). Portamento (UPPER-ONLY/OFF/ON) and the
per-target Bender switches are likewise per-section. So on the JP-8 a "patch" is
a complete voice spec *including its LFO and modulation*, and Dual/Split run two
of them. We adopt that: **a layer is a complete patch.**

## Terminology

- **Layer** — Upper or Lower; a complete patch/timbre. Two always exist.
- **Channel** — one polyphony slot (a DSP voice). 8 per layer, 16 total.

(Earlier notes overloaded "voice"; layer/channel are used precisely from here.)

## Decision

### 1. Two layers, always instantiated

The engine always holds **two layers** (Upper, Lower), each a full patch: its
own oscillators, filter, envelopes, **LFO, and modulation matrix**. There is no
separate "one-patch" code path — Whole mode is expressed as a routing/parameter
choice over the same two layers (§3).

### 2. Channels & polyphony — static 8 per layer

Each layer owns **8 channels** (16 total). Allocation is per layer; there is no
cross-layer channel pool. Consequently:

- **Whole** = 16-voice polyphony (both layers active, round-robinned — §3).
- **Dual / Split** = 8-voice polyphony per layer.

This is the faithful analogue of the JP-8 (8 total → 4+4 in Dual/Split); we
double it. The asymmetry (16 in Whole, 8 per layer otherwise) is intended.

### 3. Mode = (event routing, parameter-source map)

A key mode is fully described by two policies. No DSP reconfiguration.

| Mode  | MIDI events                | Layer A reads | Layer B reads | Poly |
|-------|----------------------------|---------------|---------------|------|
| Whole | round-robin across A/B     | A             | **A**         | 16   |
| Dual  | both layers receive all    | A             | B             | 8+8  |
| Split | partitioned at split point | A             | B             | 8/8  |

**Whole reads one block.** In Whole, *both* layer-engines read **layer A's**
parameter block; layer B's block lies dormant. Round-robinning 16 channels
across two engines that read the same params gives uniform 16-voice mono-timbral
behaviour with **no parameter mirroring** and no risk of the two halves drifting
into different timbres. (The alternative — echoing each automated write to both
blocks — was rejected: the host sends one fixed id, so the echo would have to be
engine-side and fights the automation model.)

**Seed-on-entry.** Entering Dual or Split from Whole **copies layer A → layer
B** once, then the two diverge as edited. This matches the JP-8 ("the patch
previously used for Whole is automatically assigned to Upper," then you choose
Lower) and avoids a stale-Lower timbre surprise.

### 4. Per-layer MIDI processor — the assignment model

Each layer owns a **MIDI-event processor** sitting between the (already routed)
event stream and channel allocation. It implements the assign mode:

- **Poly** — first-free / oldest-steal across the layer's 8 channels (today's
  allocator, scoped to 8).
- **Unison / mono** — one logical note broadcast across all 8 channels with
  per-channel detune drift for thickness (the JP-8 Unison/Solo idea).
- **(future) Arp** — a stream transform that turns held notes into a sequence,
  played into the layer's channels.

Because assignment is a **stream transform before allocation**, unison,
portamento and (later) arpeggiation are per-layer features of this processor,
not engine surgery.

**Roadmap consequence:** the "Unison" and "Portamento" items from ADR 0002 are
**absorbed into this work** rather than shipped as independent epics — they are
the per-layer assignment model, and building them separately would build the
assignment layer twice. Arpeggiation (explicitly out of scope per ADR 0002) is
left a documented hook in the processor, not implemented.

### 5. LFO & modulation matrix are per-layer

Following the JP-8 (§Context) and the "layer = full patch" rule, each layer has
its own LFO and its own 5×4 (later 6×4) modulation matrix. VXN1's current single
global LFO becomes per-layer. This keeps the partition clean: **everything in a
patch duplicates; only truly global state is shared (§6, §7).**

> **Amended by E005 (per-voice & global LFO split).** The two LFOs became
> asymmetric rather than uniformly per-layer:
>
> - **LFO 1 is per-voice** (E005 / 0018): each note runs its own phase,
>   retriggered to the shape's zero crossing at note-on (or free-running), with a
>   per-voice delay→fade onset. Its rate/shape/sync stay per-patch.
> - **LFO 2 is a single instrument-wide global LFO** (E005 / 0019): one shared
>   phase across both layers and all voices; its rate/shape/sync are global
>   params, its matrix-routing depths stay per-patch.
>
> The modulation matrix stays per-layer (each layer routes both LFOs with its own
> depths). The global LFO 2 is shared instrument state, **not** part of a
> per-layer patch/preset.
>
> **Superseded by ADR 0004 / E006 0022 (fixed-panel modulation).** The generic
> 6×4 modulation matrix (`ModSource`/`ModDest`/`ModMatrix`) is removed and
> replaced with **fixed, labelled routes** carrying per-channel source selectors
> ({Off/LFO1/LFO2} + depth, {Off/Env1/Env2} + depth) for Pitch / PWM / Cutoff,
> plus a wide osc-2 pitch route, a mod-wheel panel, and a filter key-track toggle.
> The VCA is hardwired to Env2 (no Amp destination). The routes are still
> **per-layer** (each layer carries its own selectors/depths) and either LFO can
> feed any channel, so the per-layer/global split above is unchanged — only the
> matrix's generality is dropped. See ADR 0004 §4–§5.

### 6. Parameter model — two per-patch blocks + a small global block

We accept parameter doubling. The flat `ParamId` table splits into:

- A **per-patch block** (oscillators, noise, filter, envelopes, LFO, fixed
  modulation routes, PWM, etc.; the generic matrix was replaced by ADR 0004's
  fixed routes), instantiated **twice** — `Upper_*` and `Lower_*` — each fully and
  independently automatable with stable CLAP ids.
- A **global block** (master tune, master volume, FX — §7 — key mode, split
  point — §8) that exists once.

The editor shows **one faceplate** with an Upper/Lower toggle selecting which
per-patch block it edits; a UI gesture writes that block's fixed id, so the host
records the specific `Upper_*` / `Lower_*` parameter (no ambiguity). In Whole
mode the toggle is hidden and the editor edits layer A.

### 7. FX bus is global

Chorus and delay remain a single global bus, post-mix of both layers (as today).
This diverges from the strict "layer = patch" rule (and from a hypothetical
per-layer FX), but is the pragmatic v1 choice; the JP-8 had no onboard chorus to
be faithful to. Per-layer FX is deferred (§Consequences).

### 8. Split point is opaque saved state

The split point is **not** a CLAP parameter and is **not automatable**. It is
opaque state, persisted in plugin state (a MIDI note number, 0–127), set in the
UI. It is deliberately performance/setup state, not a sound parameter.

### 9. Performance controls — value global, response per-layer

Pitch bend and mod wheel (ADR 0002 / ticket 0006) are single physical/MIDI
controls, so their **values are global** and broadcast to both layers. Each
layer responds according to **its own** routing params (the per-patch mod-wheel
destination/depth, bend range). So in Split the two ends can react differently
to the same wheel because each carries its own routing — without per-end control
*assignment* (which the JP-8 offered and we defer). Bend range stays a global
±2 st for now.

### 10. DSP structure

Either two `VoiceBank`s of 8 channels, or one bank that processes 8-channel
**slices** with per-slice global params. Either preserves the kernel's
hoisted-global / vectorised lane loop *within a layer* (problem #2 dissolves: a
layer is homogeneous). `render` builds a `BlockCtx` **per layer** from that
layer's param block, renders each layer's 8 channels, and sums into the global
FX bus.

## Consequences

- **Parameter count roughly doubles** (the per-patch block ×2). Automation lists
  grow; ids stay stable and unambiguous. The append-at-end discipline still
  holds for *new* params within a block.
- **Engine restructure**, not a reshuffle: per-layer `BlockCtx`, per-layer LFO
  state, two render passes summed, and the per-layer MIDI processor + event
  router are new. `Synth` grows a mode + split-point + the routing logic.
- **Poly kernels** must run 8-channel groups (slice or two-bank). The vectorised
  inner loop is unchanged; only the grouping changes.
- **MIDI handling** in `vxn-clap` gains stream routing: round-robin (Whole),
  duplicate (Dual), or split-point partition (Split) before per-layer
  processing. NoteOn/NoteOff that cross a moving split point need defined
  behaviour (notes already sounding are unaffected until released).
- **Mode transitions** are now cheap (routing/param-source changes, plus the
  one-shot seed-on-entry copy), but hanging-note handling on transition must be
  specified in the implementing epic.
- **CPU**: two render passes. Whole is still 16 channels total, so no worse than
  today; Dual/Split pay two 8-channel passes plus two LFOs/matrices.
- **Roadmap reorder**: Unison and Portamento fold into this epic's assignment
  model (§4); they leave the standalone backlog.

**Deferred (intentional):**

- Per-layer FX, and separate Upper/Lower audio outs / per-layer pan (JP-8 had
  split outs); v1 is one global stereo FX bus.
- Per-end *assignment* of performance controls (hold/portamento/bend to one end
  only); v1 broadcasts values and relies on per-layer routing params.
- A `BendRange` parameter (stays global ±2 st).
- Arpeggiation (hook only, per §4).

## References

- ADR 0001 — overall design (flat param table, poly kernels, single global LFO,
  CLAP automation/id contract).
- ADR 0002 — feature roadmap (§10 key modes deferred here; Unison/Portamento
  reassigned into §4 of this ADR).
- Roland Jupiter-8 Owner's Manual: per-section LFO (p.19), Key Modes /
  split-point behaviour (p.11), Assign Modes (p.12), Performance Controls
  (p.14).

## Amendment — 2026-05-26 (Solo/Twin assign modes; global-vs-per-layer scope)

Two clarifications from the E006 voice work. §3–§9 stand; §4's assign-mode set
is formalised and the global/per-layer split is made explicit.

### §4 — the assign-mode set, formalised

The assign mode is a per-layer enum with **four** values (§4 originally wrote
the set loosely as "poly / unison / mono"):

- **Poly** — first-free / nearest-free / oldest-steal across the layer's 8
  channels. One channel per note.
- **Unison** — one note stacked across all 8 channels with a per-channel detune
  spread and half-cycle start-phase decorrelation; level-compensated `1/√8`.
- **Solo** — monophonic: exactly one channel sounds per layer, last-note
  priority. A new note takes over the sounding channel, so portamento is legato.
- **Twin** — each note assigned to **two** channels with a pitch spread
  (±`UnisonDetune`) and a quarter-cycle phase decorrelation; level-compensated
  `1/√2`. A fat two-voice-per-note stack.

**Naming.** The two-channels-per-note mode is **Twin**, deliberately *not*
"Dual", to avoid colliding with the keyboard-level **`KeyMode::Dual`** (§3 —
note routed to *both layers*). The two axes are orthogonal and compose.

**Polyphony is not a separate setting — it falls out of the static 8-channel
pool (§2).** Twin consumes two channels per note; effective note-polyphony
against the engine's 16 channels total:

| Key mode        | Channels per note           | Poly / Unison / Solo | Twin          |
| --------------- | --------------------------- | -------------------- | ------------- |
| Whole           | 1 (round-robin both layers) | up to 16 notes       | 8 notes       |
| Split           | 1 (one layer, 8 ch)         | up to 8 notes/layer  | 4 notes/layer |
| Dual (layered)  | 2 (one per layer)           | up to 8 notes        | 4 notes       |

Oldest-`alloc_tick` stealing (§2/§4) absorbs overflow; these counts are
descriptive, not enforced caps.

**Structure.** The allocation *policy* (which channel(s) a note takes, plus the
per-channel detune/phase to stamp) is a **pure function over per-layer
bookkeeping** — channel `active` / `note` / glide source / `alloc_tick` — with no
oscillator, filter or sample-rate dependency, so it is unit-tested in isolation.
The MIDI processor applies the plan via the DSP trigger (§4's "stream transform
before allocation" seam is unchanged). Solo/Twin add allocation policies, not
engine surgery.

### Global vs per-layer — explicit scope (confirms §6/§9)

Because Solo/Twin (like all voice settings) are per-layer, the boundary is worth
stating outright:

- **Per-layer** — the `PatchParam` block, instantiated Upper + Lower: oscillators,
  mixer levels + noise type, filter, envelopes, **LFO 1** (per-voice:
  delay/fade/free-run), the fixed modulation routes, PWM, and the **voice/assign
  controls** — `AssignMode` (incl. Solo/Twin), `UnisonDetune`, portamento. In
  Split each layer runs its own assign mode independently.
- **Truly global** — the `GlobalParam` block, one instance: **master tune**,
  **master volume**, the **FX bus** (chorus, delay — §7), **oversample quality**,
  and **LFO 2** (shape / rate / sync — the single instrument-wide LFO; either LFO
  still reaches any channel via the per-channel `{Off/LFO1/LFO2}` selectors,
  ADR 0004 §4). Performance-control **values** (pitch bend, mod wheel) are global
  but their **response is per-layer** (§9).

So "global" is **not** just LFO 2 + FX: it also covers master tune/volume and
oversample. Everything else — all voice settings included — is per-layer.
