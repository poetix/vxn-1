// E015 / 0079: `valuePop` is consumed by `attachValuePop` below. The
// splice loader drops this line for the inline wry `<script>` (the
// concat-time `valuePop` const from bridge.js is already in scope there);
// under Node ESM the binding resolves through bridge.js so the
// drag-and-popup tests can exercise the helper without re-mocking.
import { valuePop } from './bridge.js';

// ─── Preset bar wiring (0049 / 0050) ───────────────────────────────────────
//
// Prev/next post `step_preset` with a signed delta — the controller walks
// the combined Factory + User list and emits a fresh `preset_loaded` for
// us to pick up. The current name comes from the same `preset_loaded`
// dispatch branch in `init()`. Browse toggles the 0050 browser panel
// (open/close mirrored back via the panel's `onOpenChange` so the button's
// `.active` class stays in sync with click-outside / ESC dismissals).
// Save As opens the 0048 popup and on commit posts `save_preset` using
// the browser panel's currently-selected user folder as the destination
// (factory selections collapse to user root — there's no write target
// inside the factory bank).
export const presetBar = (() => {
  const nameEl   = document.getElementById('pbar-name');
  // E015 / 0077: under Node ESM `import` (no faceplate DOM, no concatenated
  // `browserPanel` global), bail out with a stub so pure-helper test
  // imports don't crash on `browserPanel.onOpenChange(...)` below.
  if (!nameEl) return { setName() {} };
  const prevEl   = document.getElementById('pbar-prev');
  const nextEl   = document.getElementById('pbar-next');
  const browseEl = document.getElementById('pbar-browse');
  const saveEl   = document.getElementById('pbar-save');
  let currentName = '';

  function setName(name) {
    currentName = name || '';
    if (nameEl) nameEl.textContent = currentName;
  }
  if (prevEl) {
    prevEl.addEventListener('click', () => window.vxn.send.stepPreset(-1));
  }
  if (nextEl) {
    nextEl.addEventListener('click', () => window.vxn.send.stepPreset(1));
  }
  if (browseEl) {
    browseEl.addEventListener('click', () => browserPanel.setOpen(!browserPanel.isOpen()));
  }
  browserPanel.onOpenChange((open) => {
    if (browseEl) browseEl.classList.toggle('active', open);
  });
  if (saveEl) {
    // Save As opens the combined name + folder modal. The name field
    // funnels through the existing native popup (`promptText`) for
    // spacebar-safe entry; the folder dropdown is mouse-driven (no kbd
    // capture concern). The modal anchors over the faceplate, so it
    // works whether or not the browser panel itself is open.
    saveEl.addEventListener('click', () => browserPanel.openSaveAs(currentName));
  }
  return { setName };
})();

// ─── Keys panel (0053) ────────────────────────────────────────────────────
//
// Mirrors `vxn_ui_vizia::keys_panel`: a 3-row mode selector (Whole / Dual /
// Split), a 2-row Upper/Lower edit toggle, a split-point slider over the
// C0..C7 MIDI window with a note-name readout, and a Reset button. The
// mode / edit / split widgets all write *non-automatable* shared state
// directly (ADR 0003 §3/§8) — no gestures, no host echo — and the
// controller broadcasts back via `KeyModeChanged` / `EditLayerChanged` /
// `SplitPointChanged` so the panel reseeds after a preset/state load.
//
// Visibility: the edit column hides in Whole and the split row hides
// outside Split, both via `visibility: hidden` so the Reset stays pinned
// to the same vertical position (matches the ticket's "keep the same
// shape" note about reset placement).
export const KEY_MODE_NAMES = ['WHOLE', 'DUAL', 'SPLIT'];
export const KEY_LAYERS = [
  { code: 'upper', label: 'UPPER' },
  { code: 'lower', label: 'LOWER' },
];
// Match `DEFAULT_SPLIT_POINT` in vxn-app/src/domain.rs — C4.
export const KEYS_DEFAULT_SPLIT = 60;
// Mirror `vxn_ui_vizia::SPLIT_MIN` / `SPLIT_MAX`: narrower than the full
// MIDI range so every semitone is easy to land on.
export const KEYS_SPLIT_MIN = 12;
export const KEYS_SPLIT_MAX = 96;
export function keysNoteName(n) {
  const NAMES = ['C','C#','D','D#','E','F','F#','G','G#','A','A#','B'];
  const octave = Math.floor(n / 12) - 1;
  return NAMES[((n % 12) + 12) % 12] + octave;
}
export const keysPanel = (() => {
  const bodyEl = document.querySelector('.panel[data-name="Keys"] .panel-body');
  if (!bodyEl) return { setMode() {}, setLayer() {}, setSplit() {} };

  // mode: 0 Whole, 1 Dual, 2 Split. layer: 'upper' | 'lower'. split: MIDI
  // note in [KEYS_SPLIT_MIN, KEYS_SPLIT_MAX]. Controller-side defaults
  // re-arrive on `EditorReady` so the cold-start seed gets overwritten;
  // these initials just keep the markup valid until the first echo lands.
  let mode = 0;
  let layer = 'upper';
  let split = KEYS_DEFAULT_SPLIT;

  bodyEl.innerHTML = `
    <div class="keys-top">
      <div class="keys-tg-list" id="keys-mode-list"></div>
      <div class="keys-tg-list" id="keys-edit-list"></div>
    </div>
    <div class="keys-split-row" id="keys-split-row">
      <input type="range" class="keys-split-slider" id="keys-split-slider"
             min="${KEYS_SPLIT_MIN}" max="${KEYS_SPLIT_MAX}" step="1" />
      <div class="keys-split-readout" id="keys-split-readout"></div>
    </div>
    <button type="button" class="keys-reset" id="keys-reset">RESET</button>
  `;
  const modeListEl   = bodyEl.querySelector('#keys-mode-list');
  const editListEl   = bodyEl.querySelector('#keys-edit-list');
  const splitRowEl   = bodyEl.querySelector('#keys-split-row');
  const splitSlider  = bodyEl.querySelector('#keys-split-slider');
  const splitReadout = bodyEl.querySelector('#keys-split-readout');
  const resetBtn     = bodyEl.querySelector('#keys-reset');

  function renderModeList() {
    modeListEl.innerHTML = '';
    KEY_MODE_NAMES.forEach((label, i) => {
      const row = tgRow(label);
      if (i === mode) row.classList.add('active');
      // pointerdown not click: matches the no-click-slop fix the vizia
      // toggles needed (a small wobble between down and up eats the
      // click). Browsers don't drop clicks the same way, but pointerdown
      // is still the more responsive surface.
      row.addEventListener('pointerdown', (ev) => {
        ev.preventDefault();
        if (i === mode) return;
        window.vxn.send.setKeyMode(i);
      });
      modeListEl.appendChild(row);
    });
  }
  function renderEditList() {
    editListEl.innerHTML = '';
    // Reserve the column slot in Whole so the layout (and therefore the
    // Reset button's Y position) doesn't shift when the mode flips.
    editListEl.style.visibility = mode === 0 ? 'hidden' : 'visible';
    KEY_LAYERS.forEach(({ code, label }) => {
      const row = tgRow(label);
      if (code === layer) row.classList.add('active');
      row.addEventListener('pointerdown', (ev) => {
        ev.preventDefault();
        if (code === layer) return;
        window.vxn.send.setEditLayer(code);
      });
      editListEl.appendChild(row);
    });
  }
  function renderSplit() {
    splitRowEl.style.visibility = mode === 2 ? 'visible' : 'hidden';
    splitSlider.value = String(split);
    splitReadout.textContent = keysNoteName(split);
  }

  splitSlider.addEventListener('input', () => {
    const note = Math.max(
      KEYS_SPLIT_MIN,
      Math.min(KEYS_SPLIT_MAX, Math.round(Number(splitSlider.value))),
    );
    // Optimistic local repaint of the readout; the echo from
    // `split_point_changed` will overwrite when it arrives.
    splitReadout.textContent = keysNoteName(note);
    window.vxn.send.setSplitPoint(note);
  });
  splitSlider.addEventListener('dblclick', (ev) => {
    ev.preventDefault();
    window.vxn.send.setSplitPoint(KEYS_DEFAULT_SPLIT);
  });
  resetBtn.addEventListener('pointerdown', (ev) => {
    ev.preventDefault();
    // In Whole the two layers share one patch — reset both. In Dual /
    // Split reset only the layer the edit toggle points at. Globals,
    // key mode and split point are setup state, left untouched.
    if (mode === 0) {
      window.vxn.send.resetLayer('upper');
      window.vxn.send.resetLayer('lower');
    } else {
      window.vxn.send.resetLayer(layer);
    }
  });

  renderModeList();
  renderEditList();
  renderSplit();

  return {
    setMode(m) {
      if (m === mode) return;
      mode = m;
      renderModeList();
      renderEditList();
      renderSplit();
    },
    setLayer(l) {
      if (l === layer) return;
      layer = l;
      renderEditList();
    },
    setSplit(n) {
      if (n === split) return;
      split = n;
      // Only the slider/readout change — no mode/layer visibility flip.
      splitSlider.value = String(split);
      splitReadout.textContent = keysNoteName(split);
    },
  };
})();

// ─── Waveform glyph polylines ──────────────────────────────────────────────
//
// In a [0, 1]² box (y down). Ported from `wave_points` in
// vxn-ui-vizia/src/lib.rs — coordinates only, no SVG-specific tweaks.
export const WAVE_GLYPHS = {
  'Sine': (() => {
    const pts = [];
    for (let k = 0; k <= 16; k++) {
      const t = k / 16;
      pts.push([t, 0.5 - 0.38 * Math.sin(t * Math.PI * 2)]);
    }
    return pts;
  })(),
  'Triangle': [[0, 0.85], [0.5, 0.15], [1, 0.85]],
  'Tri':      [[0, 0.85], [0.5, 0.15], [1, 0.85]],
  'Saw':      [[0, 0.85], [0.5, 0.15], [0.5, 0.85], [1, 0.15]],
  'Saw+':     [[0, 0.85], [0.5, 0.15], [0.5, 0.85], [1, 0.15]],
  'Saw-':     [[0, 0.15], [0.5, 0.85], [0.5, 0.15], [1, 0.85]],
  'Pulse':    [[0, 0.85], [0, 0.15], [0.5, 0.15], [0.5, 0.85], [1, 0.85]],
  'Square':   [[0, 0.85], [0, 0.15], [0.5, 0.15], [0.5, 0.85], [1, 0.85]],
  'S&H':      [[0, 0.6], [0.28, 0.6], [0.28, 0.2], [0.56, 0.2], [0.56, 0.8], [0.82, 0.8], [0.82, 0.45], [1, 0.45]],
};

export function glyphPath(label, w, h) {
  const pts = WAVE_GLYPHS[label];
  if (!pts) return null;
  return pts.map((p, i) =>
    (i === 0 ? 'M' : 'L') + (p[0] * w).toFixed(2) + ' ' + (p[1] * h).toFixed(2)
  ).join(' ');
}

// ─── Control primitives ────────────────────────────────────────────────────

// Dispatch state lives in `model` (declared in dispatch.js). `addCtl` is
// the only helper the primitive factories below need from it.

// One detent = one variant step. The drag sensitivity: pixels of vertical
// pointer travel per detent. ~30 feels close to hardware knobs.
export const PIXELS_PER_DETENT = 30;

// Smoothing transition on the wave-knob indicator. Long enough that
// automation moves don't strobe between detents; short enough that drag
// still feels responsive.
export const KNOB_INDICATOR_TRANSITION_MS = 120;

// Detune ceiling in Twin assign mode (cents). Twin's "useful" range is
// purely a view convention — the engine doesn't enforce it, so the
// editor that surfaces the mode is the one that has to clamp. Mirrors
// vxn_ui_vizia::TWIN_DETUNE_CT (retired in 0054 but the value is still
// load-bearing).
export const TWIN_TOP_CT = 20.0;

// Generalised pointer-drag protocol. Both fader-shaped controls (vertical
// linear norm) and the wave knob (vertical pixel-delta off a captured start
// state) share the same hover / down / capture / move / release lifecycle —
// they only differ in how pointer position maps to a value.
//
// `pointerToValue(ev, ctx)` — required. Runs on `pointerdown` (its return
//   value is the second arg to `onDown`) and `pointermove` (second arg to
//   `onMove`). `ctx` is whatever `downContext` returned for this drag.
// `downContext(ev)` — optional. Runs once on `pointerdown`, before
//   `pointerToValue`. Lets stateful drags (the wave knob) stash start
//   coordinates / start value cleanly instead of via closure-scoped lets.
//
// Callbacks fire in order:
//   onEnter(ev)             — hover begins (not during drag)
//   onDown(ev, value)       — pointer down, drag starts.
//   onMove(ev, value)       — drag-time move. Fires only while dragging.
//   onUp(ev)                — drag ends (pointerup or cancel).
//   onLeave()               — hover ends (not during drag).
// Returns { isDragging, isHovered } getters for callers whose
// ParamChanged echoes need to know whether to update the popup.
export function wireDrag(el, { pointerToValue, downContext }, { onEnter, onDown, onMove, onUp, onLeave }) {
  let dragging = false;
  let hovered = false;
  let ctx = null;
  el.addEventListener('pointerenter', (ev) => {
    if (dragging) return;
    hovered = true;
    if (onEnter) onEnter(ev);
  });
  el.addEventListener('pointerleave', () => {
    hovered = false;
    if (!dragging && onLeave) onLeave();
  });
  el.addEventListener('pointerdown', (ev) => {
    ev.preventDefault();
    dragging = true;
    ctx = downContext ? downContext(ev) : null;
    el.classList.add('dragging');
    el.setPointerCapture(ev.pointerId);
    if (onDown) onDown(ev, pointerToValue(ev, ctx));
  });
  el.addEventListener('pointermove', (ev) => {
    if (!dragging || !onMove) return;
    onMove(ev, pointerToValue(ev, ctx));
  });
  const end = (ev) => {
    if (!dragging) return;
    dragging = false;
    el.classList.remove('dragging');
    try { el.releasePointerCapture(ev.pointerId); } catch (e) {}
    if (onUp) onUp(ev);
    if (!hovered && onLeave) onLeave();
  };
  el.addEventListener('pointerup', end);
  el.addEventListener('pointercancel', end);
  return {
    isDragging: () => dragging,
    isHovered:  () => hovered,
  };
}

// Thin wrapper: the fader-shaped controls (Fader, DetuneLegato) all want
// the same vertical [0, 1] norm.
export function wireFaderDrag(fader, callbacks) {
  const pointerToValue = (ev) => {
    const r = fader.getBoundingClientRect();
    return Math.max(0, Math.min(1, 1 - (ev.clientY - r.top) / r.height));
  };
  return wireDrag(fader, { pointerToValue }, callbacks);
}

// Attaches the floating value popup's lifecycle to a control. `getLabel()`
// returns the current display string. The host control invokes the
// `markX` methods from its drag callbacks; `refresh()` runs on the
// ParamChanged echo. `host` is any object with `isHovered()` and
// `isDragging()` getters (the `wireFaderDrag` return value, or a shim
// over makeWave's local vars).
export function attachValuePop(host, getLabel) {
  return {
    markEntered(ev) {
      if (host.isDragging()) return;
      valuePop.show(getLabel(), ev.clientX, ev.clientY);
    },
    markLeft() {
      if (!host.isDragging()) valuePop.hide();
    },
    markGrabbed(ev) {
      valuePop.show(getLabel(), ev.clientX, ev.clientY);
    },
    markReleased() {
      if (!host.isHovered()) valuePop.hide();
    },
    refresh() {
      if (host.isHovered() || host.isDragging()) {
        valuePop.update(getLabel());
      }
    },
  };
}

// Paint a vertical fader's thumb at a [0, 1] norm. Norm 0 = bottom, 1 = top.
// Pins in pixel space against the live element height so the thumb's
// bounding box stays inside `.ctl-fader` exactly at both ends regardless of
// `--fader-h` / `--thumb-h` tweaks. Also sets `--fader-norm` for dependent
// CSS (track fill colour, etc).
export function paintFader(fader, thumb, norm) {
  const halfThumb = thumb.offsetHeight / 2;
  const travel = fader.clientHeight - thumb.offsetHeight;
  const n = Math.max(0, Math.min(1, norm));
  thumb.style.top = (halfThumb + (1 - n) * travel) + 'px';
  fader.style.setProperty('--fader-norm', n);
}

export function makeFader(el, id, desc, opts) {
  const noLabel = el.hasAttribute('data-no-label');
  const label = el.dataset.label || desc.label;
  const displayOverride = (opts && opts.displayOverride) || null;
  el.innerHTML = `
    ${noLabel ? '' : `<div class="ctl-label">${label.toUpperCase()}</div>`}
    <div class="ctl-fader">
      <div class="ctl-fader-track"></div>
      <div class="ctl-fader-thumb"></div>
    </div>
  `;
  const fader = el.querySelector('.ctl-fader');
  const thumb = el.querySelector('.ctl-fader-thumb');
  let lastDisplay = '';

  let drag;
  const pop = attachValuePop({
    isHovered:  () => drag.isHovered(),
    isDragging: () => drag.isDragging(),
  }, () => lastDisplay);
  drag = wireFaderDrag(fader, {
    onEnter: (ev) => pop.markEntered(ev),
    onLeave: () => pop.markLeft(),
    onDown: (ev, n) => {
      window.vxn.send.beginGesture(id);
      paintFader(fader, thumb, n);                        // local: no round-trip wait
      window.vxn.send.setParamNorm(id, n);
      pop.markGrabbed(ev);                                // re-anchor at the grab point
    },
    onMove: (_ev, n) => {
      paintFader(fader, thumb, n);                        // local feedback every frame
      window.vxn.send.setParamNorm(id, n);
    },
    onUp: () => {
      window.vxn.send.endGesture(id);
      pop.markReleased();
    },
  });

  return {
    update(plain, norm, display) {
      // ViewEvent echo — always position the thumb so DAW automation
      // moves it even mid-drag (engine value is authoritative). During a
      // drag the local pointermove `paintFader` and the round-trip echo
      // converge on the same value, so the thumb stays glued to the
      // cursor without flicker.
      paintFader(fader, thumb, norm);
      // Synced LFO rates swap the Hz readout for a subdivision label
      // (0042). The override is null for every other fader, so this
      // collapses to the plain path.
      let label = display;
      if (displayOverride) {
        const o = displayOverride(plain, norm, display);
        if (o != null) label = o;
      }
      lastDisplay = label;
      pop.refresh();
    },
  };
}

// Map a normalised fader position (linear `[0, 1]`) to the matching
// subdivision label. The LFO rate fader's `norm` is the linear range
// position (`get_normalized`, not the exp-tapered fader-position); since
// `vxn_app::sync::index_from_norm` only ever takes the slider's `0..1`,
// either convention agrees on the index — the table is just spread evenly
// across the travel.
export function subdivisionLabel(norm) {
  const t = window.vxn.subdivisions || [];
  if (t.length === 0) return '';
  const last = t.length - 1;
  const n = Math.max(0, Math.min(1, norm));
  return t[Math.max(0, Math.min(last, Math.round(n * last)))];
}

// ─── Rotary waveform knob ──────────────────────────────────────────────────
//
// Single SVG: knob face + rotating indicator + glyph labels spread around
// a 270° arc with the gap at the bottom (clamped knob, no wrap). Drag
// rotation = vertical pointer motion (up = CW, down = CCW), clamped at
// endpoints, snapped to the nearest detent. Click a glyph for direct
// selection.
//
// Variant angles are evenly distributed across ARC_START..ARC_END, so the
// 4-variant Osc knob still lands its glyphs at SW/NW/NE/SE (the corners
// of -135°…+135° "from up CW") while the 6-variant LFO shape fits without
// crowding the corners. Indicator angle is the same affine function of
// value, so the CSS transition always sweeps along the populated arc.
//
// **Future**: when intermediate / cross-fade waveforms ship, this becomes
// a continuous `[0, N)` knob with wrap-around. The angle math already
// works for fractional values; only the drag clamp + glyph-active logic
// need a `wrap: true` branch.
export const SVG_NS = 'http://www.w3.org/2000/svg';

export function makeWave(el, id, desc) {
  const label = el.dataset.label || desc.label;
  const variants = desc.variants || [];
  el.innerHTML = `<div class="ctl-label">${label.toUpperCase()}</div>`;

  const size = 64;
  const cx = size / 2, cy = size / 2;
  const knobR = 13;
  const glyphR = 26;
  const glyphW = 14, glyphH = 10;

  // 270° arc with a 90° gap at the bottom. Angles measured in degrees CW
  // from "straight up" (0°), so -135° = SW corner, +135° = SE.
  const ARC_START = -135;
  const ARC_SWEEP = 270;
  const N = variants.length;
  const STEP_DEG = N > 1 ? ARC_SWEEP / (N - 1) : 0;
  const variantDeg = (i) => ARC_START + i * STEP_DEG;

  let value = 0;
  let displayedAngle = variantDeg(0);
  let lastDisplay = variants[0] || '';

  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('width', size);
  svg.setAttribute('height', size);
  svg.setAttribute('viewBox', `0 0 ${size} ${size}`);
  svg.classList.add('ctl-wave');
  el.appendChild(svg);

  // Glyph labels along the arc. Transparent rect behind the path makes
  // the whole label area clickable, not just the stroked pixels.
  const glyphEls = variants.map((name, i) => {
    const a = variantDeg(i) * Math.PI / 180;
    const gx = cx + glyphR * Math.sin(a);
    const gy = cy - glyphR * Math.cos(a);
    const g = document.createElementNS(SVG_NS, 'g');
    g.setAttribute('transform',
      `translate(${(gx - glyphW / 2).toFixed(2)} ${(gy - glyphH / 2).toFixed(2)})`);
    g.setAttribute('cursor', 'pointer');

    const hit = document.createElementNS(SVG_NS, 'rect');
    hit.setAttribute('x', -3); hit.setAttribute('y', -3);
    hit.setAttribute('width',  glyphW + 6);
    hit.setAttribute('height', glyphH + 6);
    hit.setAttribute('fill', 'transparent');
    g.appendChild(hit);

    const path = document.createElementNS(SVG_NS, 'path');
    const d = glyphPath(name, glyphW, glyphH);
    if (d) {
      path.setAttribute('d', d);
      path.setAttribute('fill', 'none');
      path.setAttribute('stroke-width', 1.4);
      path.setAttribute('stroke-linecap', 'round');
      path.setAttribute('stroke-linejoin', 'round');
    }
    g.appendChild(path);

    g.addEventListener('pointerdown', (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      window.vxn.send.discrete(id, i);
    });

    svg.appendChild(g);
    return { g, path, name };
  });

  // Knob face: rim + inner dimple, both purely visual.
  const rim = document.createElementNS(SVG_NS, 'circle');
  rim.setAttribute('cx', cx); rim.setAttribute('cy', cy);
  rim.setAttribute('r', knobR);
  rim.setAttribute('fill', 'var(--knob-face)');
  rim.setAttribute('stroke', 'var(--knob-rim)');
  rim.setAttribute('stroke-width', 1);
  svg.appendChild(rim);

  const dimple = document.createElementNS(SVG_NS, 'circle');
  dimple.setAttribute('cx', cx); dimple.setAttribute('cy', cy);
  dimple.setAttribute('r', knobR * 0.62);
  dimple.setAttribute('fill', 'var(--knob-dimple)');
  dimple.setAttribute('stroke', 'var(--knob-dimple-rim)');
  dimple.setAttribute('stroke-width', 0.5);
  svg.appendChild(dimple);

  // Rotating indicator — a line from centre to rim, rotated by a <g>.
  // CSS transition smooths automation moves between detents.
  const indicatorG = document.createElementNS(SVG_NS, 'g');
  indicatorG.setAttribute('transform-origin', `${cx} ${cy}`);
  indicatorG.style.transition = `transform ${KNOB_INDICATOR_TRANSITION_MS}ms ease-out`;
  const indicator = document.createElementNS(SVG_NS, 'line');
  indicator.setAttribute('x1', cx); indicator.setAttribute('y1', cy);
  indicator.setAttribute('x2', cx); indicator.setAttribute('y2', cy - knobR + 2);
  indicator.setAttribute('stroke', 'var(--knob-indicator)');
  indicator.setAttribute('stroke-width', 2);
  indicator.setAttribute('stroke-linecap', 'round');
  indicatorG.appendChild(indicator);
  svg.appendChild(indicatorG);

  // ── Hover + vertical-drag rotation (no wrap) ───────────────────────────
  // Glyph hits stopPropagation; the knob face falls through to wireDrag.
  // `downContext` stashes the pixel anchor + the value at grab-time so the
  // pointer-to-value map is delta-based, not absolute.
  // `pop` is forward-declared because the drag callbacks reference it but
  // `attachValuePop` needs the drag's hover/drag getters as its host.
  let pop;
  const drag = wireDrag(svg, {
    downContext: (ev) => ({ y0: ev.clientY, v0: value }),
    pointerToValue: (ev, ctx) =>
      clampVariant(ctx.v0 + (ctx.y0 - ev.clientY) / PIXELS_PER_DETENT, variants),
  }, {
    onEnter: (ev) => pop.markEntered(ev),
    onLeave: () => pop.markLeft(),
    onDown:  (ev) => {
      window.vxn.send.beginGesture(id);
      pop.markGrabbed(ev);
    },
    onMove:  (_ev, v) => {
      if (v !== value) window.vxn.send.setParam(id, v);
    },
    onUp:    () => {
      window.vxn.send.endGesture(id);
      pop.markReleased();
    },
  });
  pop = attachValuePop(drag, () => lastDisplay);

  function applyValue(v, display) {
    value = v;
    displayedAngle = variantDeg(v);
    indicatorG.setAttribute('transform', `rotate(${displayedAngle.toFixed(2)})`);
    glyphEls.forEach((g, i) => {
      g.path.setAttribute('stroke',
        i === v ? 'var(--glyph-active)' : 'var(--glyph)');
    });
    lastDisplay = display;
    pop.refresh();
  }

  // Seed the initial pose so the indicator + active-glyph state are right
  // before the first ParamChanged echo lands.
  applyValue(0, variants[0] || '');

  return {
    update(plain, norm, display) {
      const v = clampVariant(plain, variants);
      applyValue(v, display);
    },
  };
}

// ─── Switch / ButtonGroup / Dropdown ──────────────────────────────────────
//
// All three share the same write semantics: a click sends
// `begin_gesture` → `set_param` → `end_gesture` so the host records a
// single discrete edit (zero gesture-less writes would otherwise drop on
// some hosts that only commit between brackets). No drag, no popup —
// these are point-and-click variant pickers.

// Plain → variant index clamp. Round to nearest, clamp to [0, len - 1].
// The four enum-shaped primitives (Switch, ButtonGroup, Dropdown, Wave-
// knob drag) all need exactly this.
export function clampVariant(plain, variants) {
  return Math.max(0, Math.min(variants.length - 1, Math.round(plain)));
}

// `tgRow(name)` returns a fresh `.ctl-tg-row` containing the box + label
// pair. `tgRow(name, { mount })` instead fills the supplied target and
// returns it — used by composites whose container is already classed
// (`.ctl-detune-legato.ctl-tg-row`) and need to drop the same inner markup
// in place.
export function tgRow(name, opts) {
  const target = (opts && opts.mount) || document.createElement('div');
  if (!opts || !opts.mount) target.className = 'ctl-tg-row';
  target.innerHTML =
    '<div class="ctl-tg-box"></div>' +
    '<div class="ctl-tg-lbl">' + name.toUpperCase() + '</div>';
  return target;
}

// `Switch(id, label)` — vertical toggle for bools; also handles 2-variant
// enums (NoiseColor, FilterSlope, LfoSync, …) the way vizia's
// `Ctl::Switch` does, by rendering one toggle per variant in a row.
export function makeSwitch(el, id, desc) {
  const label = el.dataset.label || desc.label;
  const isEnum = desc.kind === 'enum';
  const entries = isEnum
    ? (desc.variants || []).map((name, i) => ({ idx: i, name }))
    : [{ idx: 1, name: label }];
  el.innerHTML = '';
  el.style.display = 'inline-flex';
  el.style.flexDirection = 'row';
  el.style.gap = '12px';
  el.style.alignItems = 'center';

  const rows = entries.map(({ idx, name }) => {
    const row = tgRow(name);
    row.addEventListener('pointerdown', (ev) => {
      ev.preventDefault();
      let plain;
      if (isEnum) {
        plain = idx;
      } else {
        // Bool: toggle current. `row.classList.contains('active')` is the
        // local truth; the round-trip echo will reconcile if the engine
        // refuses (clamped, gated).
        plain = row.classList.contains('active') ? 0 : 1;
      }
      window.vxn.send.discrete(id, plain);
    });
    el.appendChild(row);
    return { row, idx };
  });

  return {
    update(plain) {
      const p = isEnum
        ? clampVariant(plain, entries)
        : (plain >= 0.5 ? 1 : 0);
      rows.forEach(({ row, idx }) => row.classList.toggle('active', idx === p));
    },
  };
}

// `ButtonGroup(id, label, variants)` — for Oversample, CrossModType,
// AssignMode. Vertical stack of labelled toggles under a column label
// (matches vizia's `enum_list_body`).
//
// `data-no-label` — render no column header (used inside `.route-col`,
// where the route header (LFO/Env) is the only column label).
// `data-order` — comma-separated display permutation of the variant
// indices (e.g. `0,3,1,2` for AssignMode → Poly/Twin/Unison/Solo); the
// stored value stays each variant's own descriptor index. Mirrors
// vxn-ui-vizia's `ASSIGN_DISPLAY_ORDER`.
export function makeButtonGroup(el, id, desc) {
  const label = el.dataset.label || desc.label;
  const variants = desc.variants || [];
  const noLabel = el.hasAttribute('data-no-label');
  const orderRaw = (el.dataset.order || '').split(',')
    .map((s) => parseInt(s, 10))
    .filter((n) => !isNaN(n) && n >= 0 && n < variants.length);
  const order = orderRaw.length === variants.length
    ? orderRaw
    : variants.map((_, i) => i);
  // Tag the cell so `.ctl-buttongroup .ctl-tg-rows { flex-direction: column }`
  // kicks in — without this the inline-flex `.ctl-tg-row` children flow
  // horizontally and overflow the column. The shape (vertical alongside
  // faders inside panel-body) matches vizia's `enum_list_body`.
  el.classList.add('ctl-buttongroup');
  el.innerHTML =
    (noLabel ? '' : '<div class="ctl-label">' + label.toUpperCase() + '</div>') +
    '<div class="ctl-tg-rows"></div>';
  const rowsHost = el.querySelector('.ctl-tg-rows');
  // `rows[i]` corresponds to variant index `i` (not display position), so
  // the update path can flip the active class by plain value directly.
  const rows = new Array(variants.length);
  for (const n of order) {
    const row = tgRow(variants[n]);
    row.addEventListener('pointerdown', (ev) => {
      ev.preventDefault();
      window.vxn.send.discrete(id, n);
    });
    rowsHost.appendChild(row);
    rows[n] = row;
  }
  return {
    update(plain) {
      const p = clampVariant(plain, variants);
      rows.forEach((row, i) => row && row.classList.toggle('active', i === p));
    },
  };
}

// `Dropdown(id, label, variants)` — native <select> fallback. Used when
// the variant list is too long for a row of toggles to fit the cell.
export function makeDropdown(el, id, desc) {
  const label = el.dataset.label || desc.label;
  const variants = desc.variants || [];
  el.classList.add('ctl-dropdown');
  el.innerHTML =
    '<div class="ctl-label">' + label.toUpperCase() + '</div>' +
    '<select></select>';
  const select = el.querySelector('select');
  variants.forEach((v, i) => {
    const opt = document.createElement('option');
    opt.value = String(i);
    opt.textContent = v;
    select.appendChild(opt);
  });
  select.addEventListener('change', () => {
    const i = parseInt(select.value, 10);
    window.vxn.send.discrete(id, i);
  });
  return {
    update(plain) {
      const p = clampVariant(plain, variants);
      select.value = String(p);
    },
  };
}

// ─── Header switch (Chorus / Delay, 0045) ──────────────────────────────────
//
// A small toggle box centred inside a panel header's
// `.panel-header-toggle-slot`. Same wire shape as a plain bool `Switch` —
// gesture-bracketed `set_param` on click; update() flips the `.active`
// class on echo. The box is a child of the slot rather than the slot
// itself so the 16 px slot keeps its layout reservation while the visible
// box stays small enough to sit inside the header bar.
export function makeHeaderSwitch(el, id, _desc) {
  el.innerHTML = '<div class="panel-header-switch"></div>';
  const box = el.querySelector('.panel-header-switch');
  el.addEventListener('pointerdown', (ev) => {
    ev.preventDefault();
    const on = box.classList.contains('active') ? 0 : 1;
    window.vxn.send.discrete(id, on);
  });
  return {
    update(plain) { box.classList.toggle('active', plain >= 0.5); },
  };
}

// ─── Detune + Legato composite (Voice panel, 0045) ─────────────────────────
//
// Two params + one watch in a single column: the Detune fader on top and
// the Legato toggle beneath it, both driven by Assign Mode for visual hints
// (dim Legato in Poly/Twin) and behaviour (Detune fader's full-travel
// meaning is 50 ct in Unison vs 20 ct in Twin — mirrors
// `vxn_ui_vizia::detune_top`). Plain values stay in descriptor units (0–50
// ct); only the fader's [0,1] → cents map changes per mode.
//
// `data-legato-param` / `data-mode-param` name the descriptor names this
// cell pairs with; both are resolved per layer at bind time so a layer
// rebind (0045) rebuilds the cell with the new ids.
export function makeDetuneLegato(el, ids, descs, modeName, layer) {
  const { detune, legato, mode } = ids;
  const label = el.dataset.label || descs.detune.label;
  el.classList.add('ctl-detune');
  el.innerHTML =
    '<div class="ctl-label">' + label.toUpperCase() + '</div>' +
    '<div class="ctl-detune-body">' +
      '<div class="ctl-fader">' +
        '<div class="ctl-fader-track"></div>' +
        '<div class="ctl-fader-thumb"></div>' +
      '</div>' +
      '<div class="ctl-detune-legato ctl-tg-row"></div>' +
    '</div>';
  const fader = el.querySelector('.ctl-fader');
  const thumb = el.querySelector('.ctl-fader-thumb');
  const legatoRow = el.querySelector('.ctl-detune-legato');
  tgRow('LEGATO', { mount: legatoRow });

  const DESC_TOP = descs.detune.max;
  // Twin's variant index lives in the assign descriptor (current order:
  // Poly, Unison, Solo, Twin → index 3). Look it up by name so a reorder
  // in ASSIGN_LABELS doesn't desync.
  const lookupVariant = (name) => variantIdx(modeName, name, layer);
  const TWIN_IDX = lookupVariant('Twin');
  const MONO_IDXS = new Set();
  // Mono assign modes (Legato applies in these): Unison, Solo. Found by
  // name so an ASSIGN_LABELS reorder doesn't desync.
  ['Unison', 'Solo'].forEach((n) => {
    const i = lookupVariant(n);
    if (i >= 0) MONO_IDXS.add(i);
  });

  let lastDetunePlain = 0;
  let lastModePlain = 0;

  function currentTop() {
    return Math.round(lastModePlain) === TWIN_IDX ? TWIN_TOP_CT : DESC_TOP;
  }
  function setThumbFromPlain(plain) {
    const top = currentTop();
    paintFader(fader, thumb, top > 0 ? plain / top : 0);
  }

  let drag;
  let lastDetuneDisplay = null;
  const detuneLabel = () =>
    lastDetuneDisplay || (lastDetunePlain.toFixed(1) + ' ct');
  const pop = attachValuePop({
    isHovered:  () => drag.isHovered(),
    isDragging: () => drag.isDragging(),
  }, detuneLabel);
  drag = wireFaderDrag(fader, {
    onEnter: (ev) => pop.markEntered(ev),
    onLeave: () => pop.markLeft(),
    onDown: (ev, n) => {
      window.vxn.send.beginGesture(detune);
      const plain = n * currentTop();
      lastDetunePlain = plain;
      lastDetuneDisplay = plain.toFixed(1) + ' ct';
      setThumbFromPlain(plain);
      window.vxn.send.setParam(detune, plain);
      pop.markGrabbed(ev);
    },
    onMove: (_ev, n) => {
      const plain = n * currentTop();
      lastDetunePlain = plain;
      lastDetuneDisplay = plain.toFixed(1) + ' ct';
      setThumbFromPlain(plain);
      window.vxn.send.setParam(detune, plain);
      pop.refresh();
    },
    onUp: () => {
      window.vxn.send.endGesture(detune);
      pop.markReleased();
    },
  });

  legatoRow.addEventListener('pointerdown', (ev) => {
    ev.preventDefault();
    const on = legatoRow.classList.contains('active') ? 0 : 1;
    window.vxn.send.discrete(legato, on);
  });
  // Double-click resets the detune fader (descriptor default).
  el.addEventListener('dblclick', (ev) => {
    ev.preventDefault();
    window.vxn.send.discrete(detune, descs.detune.default);
  });

  function applyLegatoDim() {
    legatoRow.classList.toggle('disabled', !MONO_IDXS.has(Math.round(lastModePlain)));
  }

  return {
    // The composite registers three model.controls entries (detune, legato, mode)
    // pointing at three updater closures returned here — `init()` then
    // routes each ParamChanged into the matching closure.
    detuneUpdate(plain, _norm, display) {
      lastDetunePlain = plain;
      lastDetuneDisplay = display || (plain.toFixed(1) + ' ct');
      setThumbFromPlain(plain);
      pop.refresh();
    },
    legatoUpdate(plain) {
      legatoRow.classList.toggle('active', plain >= 0.5);
    },
    modeUpdate(plain) {
      const prevTwin = Math.round(lastModePlain) === TWIN_IDX;
      lastModePlain = plain;
      // On entering Twin, clamp the stored detune down to the Twin ceiling
      // (mirrors `vxn_ui_vizia::clamp_detune_on_twin`). The engine doesn't
      // enforce this — Twin's "useful" range is purely a view convention,
      // so the editor that surfaces the mode is the one that has to clamp.
      if (!prevTwin && Math.round(plain) === TWIN_IDX && lastDetunePlain > TWIN_TOP_CT) {
        window.vxn.send.discrete(detune, TWIN_TOP_CT);
        lastDetunePlain = TWIN_TOP_CT;
      }
      setThumbFromPlain(lastDetunePlain);
      applyLegatoDim();
    },
  };
}
