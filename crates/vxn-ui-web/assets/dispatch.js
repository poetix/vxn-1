// ─── Init + dispatch ───────────────────────────────────────────────────────

// Lowest-id name lookup — for a per-patch param this is the Upper-layer id
// (id < patchCount); for a global it's the global id directly. Layer
// rebinding (0045) translates Upper → Lower with `+patchCount`.
function paramIdByName(name) {
  for (const k in window.vxn.params) {
    if (window.vxn.params[k].name === name) return parseInt(k, 10);
  }
  return null;
}

// Per-layer name → id lookup. Globals (id ≥ 2·patchCount) are layer-
// independent and pass through unchanged. Per-patch ids translate from
// Upper to Lower by adding `patchCount` (the slot offset that
// `vxn_app::patch_clap_id` bakes in: lower_id = upper_id + PATCH_COUNT).
function paramIdByNameAtLayer(name, layer) {
  const upper = paramIdByName(name);
  if (upper == null) return null;
  const pc = window.vxn.patchCount;
  if (upper >= 2 * pc) return upper;
  return layer === 'lower' ? upper + pc : upper;
}

// Look up a variant's plain index on an enum param at the current
// layer. Returns -1 if either the param or the variant name is
// unknown — callers treat that as "rule does not apply".
function variantIdx(paramName, variantName, layer) {
  const id = paramIdByNameAtLayer(paramName, layer);
  if (id == null) return -1;
  const variants = window.vxn.params[id].variants || [];
  return variants.indexOf(variantName);
}

function isLayeredEl(el) {
  return el.closest('[data-layered]') != null;
}

// Per-tick mutable state the dispatcher owns. Grouped here so the module
// reads as "init builds the model; dispatch reads + mutates it" rather
// than a dozen free-floating globals.
const model = {
  // ParamChanged routing: id → [updater closures]. Composite cells
  // (detune-legato) register secondary watchers on related ids; dispatch
  // fans each echo out to every updater on the id.
  controls: new Map(),
  // Last (plain, norm, display) seen per id. Sync-partner refresh /
  // dim refresh / layer rebind reseed from here.
  lastParam: new Map(),
  // sync_partner pairings: rateId ↔ syncId for LFO1 / LFO2 / Delay,
  // resolved per layer in rebindAllForLayer.
  syncOfRate: new Map(),
  rateOfSync: new Map(),
  // Active edit layer ('upper' | 'lower'). EditLayerChanged mutates.
  currentLayer: 'upper',
  // Dim-rule specs collected from HTML attributes + builtins (0066).
  dimRuleSpecs: [],
  // Resolved rules for the current layer: { watchId, predicate, target }.
  dimRules: [],
  // Per-cell binding info captured at init; layered entries rebuild
  // against new layer ids on EditLayerChanged, static entries don't.
  cells: [],
};

function addCtl(id, ctl) {
  let arr = model.controls.get(id);
  if (!arr) model.controls.set(id, arr = []);
  arr.push(ctl);
}

// Pair sync-able rate/time faders with their sync-toggle partners (E004 /
// 0015). Mirrors `vxn_ui_vizia::sync_partner`: LFO 1 rate ↔ LFO 1 sync
// (per-patch), LFO 2 rate ↔ LFO 2 sync (global), Delay Time ↔ Delay Sync
// (global, 0045). Resolved per current layer.
function locateSyncPartners(layer) {
  model.syncOfRate.clear();
  model.rateOfSync.clear();
  const pairs = [
    ['lfo_rate',   'lfo_sync'],
    ['lfo2_rate',  'lfo2_sync'],
    ['delay_time', 'delay_sync'],
  ];
  for (const [rateName, syncName] of pairs) {
    const r = paramIdByNameAtLayer(rateName, layer);
    const s = paramIdByNameAtLayer(syncName, layer);
    if (r == null || s == null) continue;
    model.syncOfRate.set(r, s);
    model.rateOfSync.set(s, r);
  }
}

// ─── Generic dim rules (0044) ──────────────────────────────────────────────
//
// Per-cell HTML markers register a dim rule resolved at bind time:
//   `data-dim-when-src-off="srcName"` — dim self when the named source
//     selector reads `Off` (the depth fader paired with a source
//     buttongroup: Pitch/PWM Mod, Cross Mod's osc2 Mod). Source
//     selectors themselves stay bright; only their paired fader dims so
//     a routed-Off path is still readable + clickable.
//   `data-dim-unless-fm="typeName"` — dim self unless the named type
//     selector reads the `FM` variant. Cross Mod's Amount fader only
//     drives PM (labelled FM, ADR 0004 §3), so it greys out for Off and
//     Sync, matching `vxn_ui_vizia::xmod_pair`.
//
// One pass collects the HTML-attribute specs from the DOM into
// `model.dimRuleSpecs`; resolution to current-layer CLAP ids happens on
// every (re)bind into `model.dimRules` so a layer flip rebuilds them
// without touching the markup.

// Built-in dim specs that don't fit the HTML-attribute model (targets are
// named params resolved at bind time, not DOM elements picked up by a
// querySelectorAll). Each entry fans out into N `DIM_RULES` entries that
// share one `watchId` and `predicate`.
//   - `free-run`: LFO 1's delay/fade dim when Free toggles on (0042).
//   - `filter-notch`: Slope strip dims when Filter Mode = Notch (0043).
const BUILTIN_DIM_SPECS = [
  {
    kind: 'free-run',
    watch: 'lfo1_free_run',
    buildPredicate: () => (plain) => plain >= 0.5,
    targets: ['lfo1_delay_time', 'lfo1_fade'],
  },
  {
    kind: 'filter-notch',
    watch: 'filter_mode',
    buildPredicate: (layer) => {
      const notchIdx = variantIdx('filter_mode', 'Notch', layer);
      return (plain) => notchIdx >= 0 && Math.round(plain) === notchIdx;
    },
    targets: ['filter_slope'],
  },
];

function collectDimRuleSpecs() {
  model.dimRuleSpecs.length = 0;
  document.querySelectorAll('[data-dim-when-src-off]').forEach((el) => {
    model.dimRuleSpecs.push({
      kind: 'src-off',
      watchName: el.dataset.dimWhenSrcOff,
      target: el,
    });
  });
  document.querySelectorAll('[data-dim-unless-fm]').forEach((el) => {
    model.dimRuleSpecs.push({
      kind: 'unless-fm',
      watchName: el.dataset.dimUnlessFm,
      target: el,
    });
  });
}

function rebuildDimRules(layer) {
  model.dimRules.length = 0;
  for (const spec of model.dimRuleSpecs) {
    const watchId = paramIdByNameAtLayer(spec.watchName, layer);
    if (watchId == null) continue;
    let predicate;
    if (spec.kind === 'src-off') {
      predicate = (plain) => Math.round(plain) === 0;
    } else if (spec.kind === 'unless-fm') {
      const fmIdx = variantIdx(spec.watchName, 'FM', layer);
      predicate = (plain) => fmIdx < 0 || Math.round(plain) !== fmIdx;
    } else {
      continue;
    }
    model.dimRules.push({ watchId, predicate, target: spec.target });
  }
  for (const spec of BUILTIN_DIM_SPECS) {
    const watchId = paramIdByNameAtLayer(spec.watch, layer);
    if (watchId == null) continue;
    const predicate = spec.buildPredicate(layer);
    for (const name of spec.targets) {
      const target = document.querySelector(`[data-param="${name}"]`);
      if (target) model.dimRules.push({ watchId, predicate, target });
    }
  }
}

function applyDimRulesFor(id, plain) {
  for (const r of model.dimRules) {
    if (r.watchId !== id) continue;
    r.target.classList.toggle('dimmed', r.predicate(plain));
  }
}

// Re-apply every dim rule from cached last-known values. Called after a
// layer rebind so the new layer's bindings reflect the correct dim state
// before any fresh ParamChanged echoes arrive.
function refreshAllDimRules() {
  for (const r of model.dimRules) {
    const last = model.lastParam.get(r.watchId);
    if (!last) continue;
    r.target.classList.toggle('dimmed', r.predicate(last.plain));
  }
}

// Returns the `displayOverride` callback for `id` if it's a rate fader
// whose sync partner is currently on. The fader's `update` runs this
// before settling on a popup label.
function rateDisplayOverride(id) {
  const syncId = model.syncOfRate.get(id);
  if (syncId == null) return null;
  return (plain, norm, display) => {
    const last = model.lastParam.get(syncId);
    if (last && last.plain >= 0.5) return subdivisionLabel(norm);
    return null;
  };
}

function bindCell(entry, layer) {
  const { el, kind, name } = entry;
  const id = paramIdByNameAtLayer(name, layer);
  if (id == null) return null;
  const desc = window.vxn.params[id];
  let ctl = null;
  switch (kind) {
    case 'fader': {
      const opts = { displayOverride: rateDisplayOverride(id) };
      ctl = makeFader(el, id, desc, opts);
      break;
    }
    case 'wave':          ctl = makeWave(el, id, desc); break;
    case 'switch':        ctl = makeSwitch(el, id, desc); break;
    case 'buttongroup':   ctl = makeButtonGroup(el, id, desc); break;
    case 'dropdown':      ctl = makeDropdown(el, id, desc); break;
    case 'header-switch': ctl = makeHeaderSwitch(el, id, desc); break;
    case 'detune-legato': {
      const legatoId = paramIdByNameAtLayer(entry.extras.legatoName, layer);
      const modeId   = paramIdByNameAtLayer(entry.extras.modeName, layer);
      if (legatoId == null || modeId == null) return null;
      const composite = makeDetuneLegato(
        el,
        { detune: id, legato: legatoId, mode: modeId },
        {
          detune: desc,
          legato: window.vxn.params[legatoId],
          mode:   window.vxn.params[modeId],
        },
        entry.extras.modeName,
        layer,
      );
      // Fan composite updaters through model.controls by id. Mode is also bound
      // by the AssignMode buttongroup cell — `addCtl` keeps both updaters
      // alive on the same id so the buttongroup repaints and the detune-
      // legato visuals (top-override, Legato dim, Twin clamp) follow the
      // same echo.
      addCtl(id,       { update: (p, n, d) => composite.detuneUpdate(p, n, d) });
      addCtl(legatoId, { update: (p) => composite.legatoUpdate(p) });
      addCtl(modeId,   { update: (p) => composite.modeUpdate(p) });
      return { ids: [id, legatoId, modeId] };
    }
    default:
      console.warn('vxn: unknown control type', kind);
      return null;
  }
  addCtl(id, ctl);

  // Double-click resets the param to its descriptor default — mirrors
  // the vizia editor's `.on_double_click` (bracketed by a gesture so
  // the host records one edit). Wired on the cell root so it covers
  // every primitive uniformly; the intermediate single-click value
  // changes still fire and the reset lands last. (Header-switch is a
  // bool toggle — double-clicking it would just toggle twice; skip.)
  if (kind !== 'header-switch') {
    el.addEventListener('dblclick', (ev) => {
      ev.preventDefault();
      window.vxn.send.discrete(id, desc.default);
    });
  }
  return { ids: [id] };
}

function rebindAllForLayer(layer) {
  // Drop every prior binding — closures held the old ids; the only safe
  // way to retarget is to start fresh. `model.controls` is the routing
  // table for ParamChanged dispatch, so emptying it before re-bind
  // avoids stale updates landing on the old (now-orphaned) primitives.
  model.controls.clear();
  for (const entry of model.cells) {
    if (entry.layered) {
      // Reset the cell so a re-init clears whatever the previous primitive
      // dropped onto el (innerHTML / inline styles / classes specific to
      // its kind). Static cells aren't rebuilt on layer flips so they
      // skip the reset — bindCell's primitive factory still runs and
      // clobbers `el.innerHTML` if it had any.
      entry.el.innerHTML = '';
      entry.el.removeAttribute('style');
      entry.el.classList.remove(
        'ctl-buttongroup', 'ctl-dropdown', 'ctl-detune', 'dimmed',
      );
    }
    bindCell(entry, layer);
  }
  // The "watched id" of every quirk rule moves with the layer; re-resolve.
  locateSyncPartners(layer);
  rebuildDimRules(layer);
  // Reseed the visual dim state from cached last-known values so a layer
  // rebind reflects the new layer's state before any echo arrives.
  refreshAllDimRules();
  // Feed cached values into freshly-rebound controls (the new ids are
  // already in model.lastParam from the editor-ready broadcast).
  for (const [id, ctls] of model.controls) {
    const last = model.lastParam.get(id);
    if (!last) continue;
    for (const c of ctls) c.update(last.plain, last.norm, last.display);
  }
}

function init() {
  // Categorize every mount point by descriptor name + kind, layer-
  // agnostic. The actual id resolution + primitive instantiation happens
  // in `rebindAllForLayer`, which is also what a layer flip calls.
  document.querySelectorAll('[data-control]').forEach((el) => {
    const name = el.dataset.param;
    if (!name) return;
    const kind = el.dataset.control;
    const entry = { el, kind, name, layered: isLayeredEl(el) };
    if (kind === 'detune-legato') {
      entry.extras = {
        legatoName: el.dataset.legatoParam,
        modeName: el.dataset.modeParam,
      };
    }
    model.cells.push(entry);
  });
  collectDimRuleSpecs();
  rebindAllForLayer(model.currentLayer);

  // Dispatch one ViewEvent from Rust. ParamChanged routes by id (with the
  // partner-rate / free-run / filter-mode / generic-dim side effects pulled
  // in from 0042–0044). EditLayerChanged triggers a full layered-cell
  // rebind (0045). Status flashes the lower-right pill (0046). KeyModeChanged
  // / PresetLoaded / PresetCorpusChanged are still pre-wiring — log when
  // verbose tracing is on so the contract is visible without spamming the
  // console during automation.
  const dispatch = function (ev) {
    if (ev.kind === 'param_changed') {
      // Cache last-seen value so the sync-flip / dim-refresh / layer-
      // rebind reseed paths can reapply without waiting for the next echo.
      model.lastParam.set(ev.id, { plain: ev.plain, norm: ev.norm, display: ev.display });
      const ctls = model.controls.get(ev.id);
      if (ctls) for (const c of ctls) c.update(ev.plain, ev.norm, ev.display);
      // If this is an LFO/Delay sync toggle, the partnered rate/time fader
      // display label needs to flip Hz/s ↔ subdivision. Re-update the
      // partner with its last-seen value — the fader's displayOverride
      // will recompute.
      const rateId = model.rateOfSync.get(ev.id);
      if (rateId != null) {
        const last = model.lastParam.get(rateId);
        const rateCtls = model.controls.get(rateId);
        if (last && rateCtls) {
          for (const c of rateCtls) c.update(last.plain, last.norm, last.display);
        }
      }
      // Unified dim rules: source-Off / Cross Mod Type ≠ FM (0044) plus
      // the built-in Free-run (0042) and Filter Mode = Notch (0043).
      applyDimRulesFor(ev.id, ev.plain);
      return;
    }
    if (ev.kind === 'edit_layer_changed') {
      const layer = ev.layer === 'lower' ? 'lower' : 'upper';
      // The Keys panel's Upper/Lower toggle always follows — it owns
      // its own active-row paint regardless of whether the layer
      // actually flipped (cheap idempotent setter).
      keysPanel.setLayer(layer);
      if (layer === model.currentLayer) return;
      model.currentLayer = layer;
      rebindAllForLayer(layer);
      return;
    }
    if (ev.kind === 'key_mode_changed') {
      keysPanel.setMode(ev.mode);
      return;
    }
    if (ev.kind === 'split_point_changed') {
      keysPanel.setSplit(ev.note);
      return;
    }
    if (ev.kind === 'status') {
      statusPill.flash(ev.line);
      return;
    }
    if (ev.kind === 'text_input_result') {
      // Fire-once: drop the entry before invoking so a re-entrant
      // promptText() from inside the callback can't see a stale id.
      const cb = _textInputCallbacks.get(ev.id);
      if (cb) {
        _textInputCallbacks.delete(ev.id);
        try { cb(ev.value == null ? null : ev.value); }
        catch (e) { console.warn('promptText callback threw', e); }
      }
      return;
    }
    if (ev.kind === 'preset_loaded') {
      // 0049: preset bar name binds here. Warnings (if any) flash
      // through the status chip — they belong with the load result,
      // not in the corner.
      presetBar.setName(ev.name);
      // 0050: feed the browser panel's "currently loaded" highlight
      // from the same event. `source` is null on host state-load
      // (no on-disk anchor) — the panel just clears the highlight.
      browserPanel.setCurrentSource(ev.source || null);
      if (Array.isArray(ev.warnings) && ev.warnings.length) {
        statusPill.flash(ev.warnings.join('; '));
      }
      return;
    }
    // 0050: corpus snapshot arrives via __vxn.applyPresetCorpus
    // (separate Rust→JS channel), not through this batch. The
    // PresetCorpusChanged ViewEvent is the trigger for that push, so
    // by the time we get here the corpus is already rendered.
    // 0052: a non-null `follow` means a Move/Rename produced a new
    // path — jump the panel to its new folder and scroll it into view.
    if (ev.kind === 'preset_corpus_changed') {
      if (ev.follow) browserPanel.followPath(ev.follow);
      return;
    }
    // key_mode_changed lands here too. Uncomment for verbose
    // tracing during development:
    // console.log('vxn:view', ev);
  };
  // Batched bridge entry — Rust calls this once per controller tick.
  const applyViewEvents = function (arr) {
    for (const ev of arr) dispatch(ev);
  };
  // Replay any events buffered between bootstrap and init.
  for (const ev of _earlyViewEvents) dispatch(ev);
  _earlyViewEvents.length = 0;
  window.__vxn.applyViewEvents = applyViewEvents;
  window.vxn.onViewEvent = dispatch;

  // Tell the controller we're ready — it re-broadcasts every param + key
  // mode so any first-tick `push_param_diffs` that ran before
  // `window.vxn` even existed (real race against wry's HTML load) gets
  // re-sent into a now-wired dispatcher. Without this, sliders that
  // never received their seed `ParamChanged` show an empty hover popup
  // until the user wiggles them.
  window.vxn.send.ready();
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', init);
} else {
  init();
}
