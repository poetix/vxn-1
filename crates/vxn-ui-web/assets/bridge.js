// ─── Bridge bootstrap ──────────────────────────────────────────────────────
//
// `window.vxn.send` is a typed sender namespace: one method per opcode the
// page emits (e.g. `send.setParam(id, plain)`, `send.beginGesture(id)`).
// Each builds the same `{op, …}` JSON the old free-form sender did and
// hands it to the private `_post` which posts via wry's IPC handler.
// `window.__vxn.applyViewEvents(arr)` is the batched entry point Rust
// calls via `webview.evaluate_script` once per controller tick (0046).
// `window.vxn.onViewEvent(ev)` is a single-event convenience that routes
// to the same dispatcher — kept for ad-hoc calls and for in-page tests.
//
// `window.vxn.params` is the param descriptor table — Rust splices its JSON
// in via the `__PARAMS_JSON__` placeholder below at editor-open time. Keys
// are CLAP ids (numeric); the page builds a name → id index in `init()`.

// Buffer ViewEvents that arrive before `init()` runs. The clack shell's
// CLAP timer (vxn-clap `on_timer`) starts pushing `param_changed` events
// as soon as `set_parent` registers it — that's often before the inline
// `init()` call below has populated the model.controls map and rebound the
// dispatcher. Without a buffer the first broadcast (which is the only
// one for params that aren't being automated) is dropped on the floor
// and `lastDisplay` on every fader stays empty until the user wiggles
// something.
const _earlyViewEvents = [];
// `_post` is the one place that constructs the wire object. Senders below
// are thin façades; a future debug-log or batching hook is a one-line
// change here.
function _post(msg) {
  try { window.ipc.postMessage(JSON.stringify(msg)); }
  catch (e) { console.warn('vxn.send failed', e); }
}
window.vxn = {
  send: {
    _post,
    setParam:        (id, plain)            => _post({ op: 'set_param', id, plain }),
    setParamNorm:    (id, norm)             => _post({ op: 'set_param_norm', id, norm }),
    beginGesture:    (id)                   => _post({ op: 'begin_gesture', id }),
    endGesture:      (id)                   => _post({ op: 'end_gesture', id }),
    // One-click discrete write. Brackets the set_param in a
    // begin/end gesture so the host records a single edit rather
    // than a zero-width gesture-less write some hosts drop.
    discrete(id, plain) {
      this.beginGesture(id);
      this.setParam(id, plain);
      this.endGesture(id);
    },
    resetLayer:      (layer)                => _post({ op: 'reset_layer', layer }),
    loadFactory:     (index)                => _post({ op: 'load_factory', index }),
    loadUser:        (path)                 => _post({ op: 'load_user', path }),
    renamePreset:    (path, new_name)       => _post({ op: 'rename_preset', path, new_name }),
    deletePreset:    (path)                 => _post({ op: 'delete_preset', path }),
    movePreset:      (path, dest_folder)    => _post({ op: 'move_preset', path, dest_folder }),
    renameFolder:    (old_name, new_name)   => _post({ op: 'rename_folder', old_name, new_name }),
    deleteFolder:    (name)                 => _post({ op: 'delete_folder', name }),
    newFolder:       (suggested)            => _post({ op: 'new_folder', suggested }),
    stepPreset:      (delta)                => _post({ op: 'step_preset', delta }),
    savePreset:      (name, folder)         => _post({ op: 'save_preset', name, folder }),
    setKeyMode:      (mode)                 => _post({ op: 'set_key_mode', mode }),
    setSplitPoint:   (note)                 => _post({ op: 'set_split_point', note }),
    setEditLayer:    (layer)                => _post({ op: 'set_edit_layer', layer }),
    requestTextInput:(id, title, initial)   => _post({ op: 'request_text_input', id, title, initial }),
    ready:           ()                     => _post({ op: 'ready' }),
  },
  onViewEvent: function (ev) { _earlyViewEvents.push(ev); },
  params: __PARAMS_JSON__,
  // Tempo-sync subdivision labels (coarse → fine). When an LFO rate's sync
  // partner is on, the fader's display reads from this list instead of the
  // descriptor's Hz formatting (0042 / 0015) — mirrors the vizia editor's
  // `sync_partner` override.
  subdivisions: __SUBDIVISIONS_JSON__,
  // Per-patch slot count — used by layer rebinding (0045) to translate an
  // Upper-side CLAP id into its Lower-side twin: Lower id = Upper id +
  // patchCount, for any id under 2 × patchCount.
  patchCount: __PATCH_COUNT__,
};
// Batched bridge entry point. `init()` replaces this with the real dispatcher
// once model.controls is built; until then, every event in the batch is buffered.
// `applyPresetCorpus` is the 0050 corpus-snapshot entry point; same buffer
// pattern — the latest snapshot held until the browser panel is alive to
// consume it.
let _earlyPresetCorpus = null;
window.__vxn = {
  applyViewEvents: function (arr) {
    for (const ev of arr) _earlyViewEvents.push(ev);
  },
  applyPresetCorpus: function (snap) { _earlyPresetCorpus = snap; },
};

// ─── Constants ─────────────────────────────────────────────────────────────

// Status-pill visibility window. Re-fired messages reset the timer so a
// rapid burst stays readable; the CSS fade transition handles the dismount.
const STATUS_PILL_FLASH_MS = 3000;

// ─── Floating text-input popup bridge (0048) ───────────────────────────────
//
// `window.vxn.promptText(title, initial, cb)` posts a `request_text_input`
// UiEvent and stashes `cb` in `_textInputCallbacks` keyed by a fresh id.
// Rust opens a native NSWindow (macOS) outside the host's NSEvent monitor
// scope, so Space and friends work as text input rather than transport.
// On commit / cancel the controller emits a `text_input_result` ViewEvent;
// the dispatcher looks up `id` and fires the matching callback exactly
// once. `value` is `null` on Esc / click outside, a string on Enter.
const _textInputCallbacks = new Map();
let _textInputCounter = 0;
window.vxn.promptText = function (title, initial, cb) {
  const id = 'ti' + (++_textInputCounter);
  _textInputCallbacks.set(id, cb);
  window.vxn.send.requestTextInput(id, title || '', initial || '');
};

// ─── Floating value popup (single shared instance) ─────────────────────────
//
// One <div> per page, used by every control. Show/update/hide as the
// pointer enters / drags / leaves. Anchored at the pointer's first
// relevant position (entry for hover, grab for drag) so the popup stays
// put while the indicator moves — matches the vizia editor's grabbed-cell
// behaviour. `fixed` positioning + body-level mount means it can't push
// any layout around or be clipped by a panel's overflow.
const valuePop = (() => {
  const el = document.createElement('div');
  el.className = 'value-pop';
  document.body.appendChild(el);
  return {
    show(text, clientX, clientY) {
      el.textContent = text;
      el.style.left = (clientX + 12) + 'px';
      el.style.top  = (clientY - 8)  + 'px';
      el.style.display = 'block';
    },
    update(text) { el.textContent = text; },
    hide() { el.style.display = 'none'; },
  };
})();

// ─── Status pill (0046, repositioned in 0049) ──────────────────────────────
//
// Inline chip in the preset bar, right of the current preset name. The
// markup lives in faceplate.html so the bar layout is one HTML edit, not
// JS-built; this IIFE just binds and exposes a `.flash(text)` API.
// `Status { line }` ViewEvents flash for ~3s then fade (CSS transition).
// Re-fired messages reset the timer so a rapid burst stays readable.
const statusPill = (() => {
  const el = document.getElementById('pbar-status');
  let timer = null;
  return {
    flash(text) {
      if (!el) return;
      el.textContent = text;
      el.classList.add('visible');
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => el.classList.remove('visible'), STATUS_PILL_FLASH_MS);
    },
  };
})();
