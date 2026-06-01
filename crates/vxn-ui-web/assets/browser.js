// ─── Preset browser panel (0050 / 0051) ────────────────────────────────────
//
// Floating two-pane folders/presets browser anchored under the preset bar.
// Corpus snapshot arrives via `__vxn.applyPresetCorpus(snapshot)` — Rust
// pushes a fresh one at first flush and on every `PresetCorpusChanged`
// (`vxn_ui_web::EditorHandle::flush_view_events`). Folder selection +
// search are pure view state. Loading a preset posts `load_factory` or
// `load_user` UiEvents; the controller publishes a `preset_loaded` back
// which feeds the highlight in `setCurrentSource`.
//
// 0051 adds the user-side mutation flows: right-click context menu on
// user rows (Rename / Delete / Move to ▸ for presets; Rename / Delete
// for folders), an inline "+ New" button on the user header, and a
// modal "Delete X?" confirm for the irreversible op (the Vizia version's
// two-click row-armed pattern was unreadable here — the right-click menu
// obscured the row text).
// Label shown for the virtual user-root folder (presets with `name: null`).
// Lifted to module scope so the pure helpers below can be imported by the
// E015 test suite without instantiating the IIFE — they reach this directly.
export const UNCATEGORISED = 'Uncategorised';

// Map a folder name to the `<select>` value used in the Save As modal. The
// virtual root has no real name; we sentinel it as `__root__` so the option
// can carry the human-readable "Uncategorised" label without colliding with
// a real user folder of that name.
export function folderValue(name) {
  return name == null ? '__root__' : name;
}

// Build the dropdown option list for the Save As folder selector. Always
// surfaces the virtual root first (so the first-save case has a target
// even when the corpus hasn't seeded any user-root presets yet), then
// alpha-sorted named user folders, case-insensitive.
export function folderOptions(corpus) {
  const named = [];
  for (const g of (corpus && corpus.user) || []) {
    if (g.name == null) continue;
    named.push(g.name);
  }
  named.sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase()));
  const out = [{ value: '__root__', label: UNCATEGORISED }];
  for (const n of named) out.push({ value: n, label: n });
  return out;
}

// Move-target list for the user-preset context menu. Mirrors
// `vxn_ui_vizia::move_targets`: Uncategorised first if the corpus has a
// root group and `currentName` isn't already root (moving root → root is a
// no-op), then alpha-sorted named user folders excluding the current one.
export function moveTargets(currentName, corpus) {
  const out = [];
  let hasRoot = false;
  const named = [];
  for (const g of (corpus && corpus.user) || []) {
    if (g.name == null) { hasRoot = true; continue; }
    named.push(g.name);
  }
  named.sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase()));
  if (hasRoot && currentName !== null) {
    out.push({ name: null, label: UNCATEGORISED });
  }
  for (const n of named) {
    if (n === currentName) continue;
    out.push({ name: n, label: n });
  }
  return out;
}

export const browserPanel = (() => {
  const panelEl    = document.getElementById('browser-panel');
  // E015 / 0077: under Node ESM `import` (no faceplate DOM) bail out with a
  // shape-matching stub so pure-helper test imports (`moveTargets`,
  // `folderValue`, `folderOptions`) don't crash on the addEventListener
  // wiring below — every entry point is a no-op in that environment.
  if (!panelEl) {
    return {
      setCorpus()        {},
      setCurrentSource() {},
      setOpen()          {},
      isOpen()           { return false; },
      getSaveFolder()    { return null; },
      openSaveAs()       {},
      onOpenChange()     {},
      followPath()       {},
    };
  }
  const backdropEl = document.getElementById('browser-backdrop');
  const foldersEl  = document.getElementById('browser-folders');
  const presetsEl  = document.getElementById('browser-presets');
  const inputEl    = document.getElementById('browser-search-input');
  const clearEl    = document.getElementById('browser-search-clear');
  const closeEl    = document.getElementById('browser-close');

  // Default selection: user root (so Save As targets the user dir on first
  // open before the user has chosen a folder explicitly).
  let corpus = { factory: [], user: [] };
  let selectedFolder = { kind: 'user', name: null };
  let query = '';
  let currentSource = null;
  let isOpen = false;
  let onOpenChange = null;

  // 0051: single open menu at a time + at most one modal confirm.
  let menuEl = null;
  let modalEl = null;

  // 0052: HTML5 DnD drag state. `dragSourcePath` is non-null while the
  // user is dragging one of our user-preset rows; `dragSourceFolder` is
  // the folder it came from (string for named folders, null for
  // Uncategorised root). The external `dataTransfer` payload still uses
  // the `vxn/preset` MIME so a stray drop on an external dropzone never
  // delivers a path; the in-page logic reads these vars because
  // `dataTransfer.getData` is not callable during `dragover`.
  let dragSourcePath = null;
  let dragSourceFolder = null;

  // Match the Vizia browser's section labels (vxn_ui_vizia `browser-section`).
  const FACTORY_HEADER = 'FACTORY';
  const USER_HEADER    = 'USER';

  function setCorpus(snap) {
    corpus = snap || { factory: [], user: [] };
    if (!folderExists(selectedFolder)) {
      selectedFolder = { kind: 'user', name: null };
    }
    // Any in-flight menu / modal from before the corpus changed is
    // stale — the target row may have been renamed / moved / deleted.
    closeMenu();
    closeModal();
    renderFolders();
    renderPresets();
  }
  function folderExists(key) {
    if (!key) return false;
    const list = key.kind === 'factory' ? corpus.factory : corpus.user;
    if (!Array.isArray(list)) return false;
    for (const g of list) {
      const gn = key.kind === 'factory' ? g.category : g.name;
      if (gn === key.name || (gn == null && key.name == null)) return true;
    }
    return false;
  }
  function folderLabel(key, group) {
    if (key.kind === 'factory') return group.category || UNCATEGORISED;
    return group.name || UNCATEGORISED;
  }
  function renderFolders() {
    foldersEl.innerHTML = '';
    appendSection(FACTORY_HEADER, null);
    for (const g of corpus.factory) {
      appendFolderRow({ kind: 'factory', name: g.category }, folderLabel({kind:'factory'}, g));
    }
    appendSection(USER_HEADER, () => {
      window.vxn.promptText('New folder', 'New Folder', (value) => {
        if (value == null) return;
        const trimmed = value.trim();
        if (!trimmed) return;
        window.vxn.send.newFolder(trimmed);
      });
    });
    for (const g of corpus.user) {
      appendFolderRow({ kind: 'user', name: g.name }, folderLabel({kind:'user'}, g));
    }
  }
  function appendSection(text, onNewFolder) {
    if (onNewFolder) {
      const row = document.createElement('div');
      row.className = 'browser-section-row';
      const h = document.createElement('div');
      h.className = 'browser-section';
      h.textContent = text;
      row.appendChild(h);
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'browser-new-folder';
      btn.textContent = '+ New';
      btn.addEventListener('click', (e) => {
        e.stopPropagation();
        closeMenu();
        onNewFolder();
      });
      row.appendChild(btn);
      foldersEl.appendChild(row);
    } else {
      const h = document.createElement('div');
      h.className = 'browser-section';
      h.textContent = text;
      foldersEl.appendChild(h);
    }
  }
  function appendFolderRow(key, label) {
    const r = document.createElement('div');
    r.className = 'browser-row';
    r.textContent = label;
    if (selectedFolder.kind === key.kind && selectedFolder.name === key.name) {
      r.classList.add('selected');
    }
    r.addEventListener('click', () => {
      selectedFolder = key;
      closeMenu();
      renderFolders();
      renderPresets();
    });
    // Only user folders carry a context menu. Factory rows are read-only.
    // The virtual user-root entry (`name == null`) can't be renamed or
    // deleted either (it represents the user preset dir itself).
    if (key.kind === 'user' && key.name != null) {
      r.addEventListener('contextmenu', (e) => {
        e.preventDefault();
        openMenu(e, { kind: 'folder', name: key.name });
      });
    }
    // 0052: every user folder (incl. Uncategorised root) is a drop
    // target. Factory folders deliberately have no DnD listeners — the
    // browser's default behaviour rejects the drop and no highlight is
    // shown. The source folder mid-drag shows a `drag-blocked` style
    // and does not preventDefault, so its drop is rejected too.
    if (key.kind === 'user') {
      r.addEventListener('dragover', (e) => {
        if (dragSourcePath == null) return;
        if (key.name === dragSourceFolder) {
          r.classList.add('drag-blocked');
          return;
        }
        e.preventDefault();
        e.dataTransfer.dropEffect = 'move';
        r.classList.add('drag-over');
      });
      r.addEventListener('dragleave', () => {
        r.classList.remove('drag-over', 'drag-blocked');
      });
      r.addEventListener('drop', (e) => {
        e.preventDefault();
        r.classList.remove('drag-over', 'drag-blocked');
        if (dragSourcePath == null) return;
        if (key.name === dragSourceFolder) return;
        window.vxn.send.movePreset(dragSourcePath, key.name);
      });
    }
    foldersEl.appendChild(r);
  }
  function findGroup() {
    const list = selectedFolder.kind === 'factory' ? corpus.factory : corpus.user;
    for (const g of list) {
      const gn = selectedFolder.kind === 'factory' ? g.category : g.name;
      if (gn === selectedFolder.name) return g;
    }
    return null;
  }
  function renderPresets() {
    presetsEl.innerHTML = '';
    const q = query.trim().toLowerCase();
    // Search mode: query spans the whole corpus, ignoring the folder
    // selection. The right pane becomes a flat list of matches with an
    // origin label per row so cross-folder duplicates stay readable.
    if (q) {
      const hits = collectSearchHits(q);
      if (hits.length === 0) { appendEmpty('No matches'); return; }
      for (const h of hits) appendSearchRow(h);
      return;
    }
    // No query: browse the selected folder normally.
    const group = findGroup();
    if (!group) { appendEmpty('No presets'); return; }
    for (const p of group.presets) {
      const r = document.createElement('div');
      r.className = 'browser-row';
      r.textContent = p.name;
      if (isCurrent(p)) r.classList.add('current');
      r.addEventListener('click', () => {
        closeMenu();
        loadEntry(p);
      });
      // Only user-side presets get a context menu. Factory is read-only.
      if (selectedFolder.kind === 'user') {
        r.dataset.path = p.path;
        r.addEventListener('contextmenu', (e) => {
          e.preventDefault();
          openMenu(e, { kind: 'preset', path: p.path, name: p.name, folder: selectedFolder.name });
        });
        // 0052: only user-side rows are drag sources. Factory rows are
        // immutable and never originate a move.
        wirePresetDragSource(r, p.path, selectedFolder.name);
      }
      presetsEl.appendChild(r);
    }
    if (!group.presets.length) appendEmpty('No presets');
  }
  // Flat search across factory + user. Returns `{name, source, origin}`
  // tuples in factory-then-user order, alpha-by-name within each origin
  // (same order corpus_snapshot_json ships).
  function collectSearchHits(q) {
    const out = [];
    for (const g of corpus.factory) {
      const cat = g.category || UNCATEGORISED;
      for (const p of g.presets) {
        if (!p.name.toLowerCase().includes(q)) continue;
        out.push({
          name: p.name,
          source: { kind: 'factory', index: p.index },
          origin: 'Factory · ' + cat,
        });
      }
    }
    for (const g of corpus.user) {
      const folder = g.name || UNCATEGORISED;
      for (const p of g.presets) {
        if (!p.name.toLowerCase().includes(q)) continue;
        out.push({
          name: p.name,
          source: { kind: 'user', path: p.path, folder: g.name },
          origin: 'User · ' + folder,
        });
      }
    }
    return out;
  }
  function appendSearchRow(h) {
    const r = document.createElement('div');
    r.className = 'browser-row search-row';
    const name = document.createElement('span');
    name.className = 'browser-row-name';
    name.textContent = h.name;
    const origin = document.createElement('span');
    origin.className = 'browser-row-origin';
    origin.textContent = h.origin;
    r.appendChild(name);
    r.appendChild(origin);
    if (isCurrentSource(h.source)) r.classList.add('current');
    r.addEventListener('click', () => {
      closeMenu();
      if (h.source.kind === 'factory') {
        window.vxn.send.loadFactory(h.source.index);
      } else {
        window.vxn.send.loadUser(h.source.path);
      }
    });
    // User-side search hits keep their context menu (Rename / Delete /
    // Move to), referencing the hit's own folder so Move-to's submenu
    // excludes the right one.
    if (h.source.kind === 'user') {
      r.dataset.path = h.source.path;
      r.addEventListener('contextmenu', (e) => {
        e.preventDefault();
        openMenu(e, {
          kind: 'preset',
          path: h.source.path,
          name: h.name,
          folder: h.source.folder,
        });
      });
      // 0052: search hits also drag — source folder is the hit's own,
      // not the panel's `selectedFolder` (search ignores the selection).
      wirePresetDragSource(r, h.source.path, h.source.folder);
    }
    presetsEl.appendChild(r);
  }
  // 0052: shared dragstart/dragend wiring for any user-side preset row.
  // The `vxn/preset` MIME guards against accidental external drops; the
  // module-level `dragSourcePath` / `dragSourceFolder` carry the same
  // values for in-page logic (dataTransfer payloads can't be read
  // during `dragover`).
  function wirePresetDragSource(row, path, folder) {
    row.draggable = true;
    row.addEventListener('dragstart', (e) => {
      dragSourcePath = path;
      dragSourceFolder = folder == null ? null : folder;
      try { e.dataTransfer.setData('vxn/preset', path); } catch (_) {}
      e.dataTransfer.effectAllowed = 'move';
      row.classList.add('dragging');
    });
    row.addEventListener('dragend', () => {
      dragSourcePath = null;
      dragSourceFolder = null;
      row.classList.remove('dragging');
      // Clear any stuck highlight if dragleave didn't fire (Safari has
      // been known to swallow the final dragleave when the drop is
      // cancelled by ESC mid-hover).
      for (const el of foldersEl.querySelectorAll('.drag-over, .drag-blocked')) {
        el.classList.remove('drag-over', 'drag-blocked');
      }
    });
  }
  // 0052: after a Move-induced corpus refresh, jump the panel to the
  // folder the moved preset now lives in and scroll the row into view.
  // No-op if the path doesn't match any user preset (e.g. a stale follow
  // pointer from a racing op).
  function followPath(pathStr) {
    if (!pathStr) return;
    for (const g of corpus.user) {
      for (const p of g.presets) {
        if (p.path !== pathStr) continue;
        selectedFolder = { kind: 'user', name: g.name };
        // Clear any active search so the moved row is actually rendered
        // in the folder pane (search would still show it, but the
        // ticket reads as "the panel scrolls to it" in folder view).
        if (query) {
          query = '';
          inputEl.value = '';
        }
        renderFolders();
        renderPresets();
        const row = presetsEl.querySelector(
          `.browser-row[data-path="${cssEscapePath(pathStr)}"]`,
        );
        if (row) {
          try { row.scrollIntoView({ block: 'nearest' }); } catch (_) {}
        }
        return;
      }
    }
  }
  // Path strings can contain quotes/backslashes/spaces — escape for a
  // CSS attribute selector. `CSS.escape` is available in WKWebView.
  function cssEscapePath(s) {
    if (typeof CSS !== 'undefined' && typeof CSS.escape === 'function') {
      return CSS.escape(s);
    }
    return s.replace(/(["\\])/g, '\\$1');
  }
  function isCurrentSource(src) {
    if (!currentSource || !src) return false;
    if (src.kind === 'factory') {
      return currentSource.kind === 'factory' && currentSource.index === src.index;
    }
    return currentSource.kind === 'user' && currentSource.path === src.path;
  }
  function appendEmpty(text) {
    const e = document.createElement('div');
    e.className = 'browser-empty';
    e.textContent = text;
    presetsEl.appendChild(e);
  }
  function isCurrent(p) {
    if (!currentSource) return false;
    if (selectedFolder.kind === 'factory') {
      return currentSource.kind === 'factory' && currentSource.index === p.index;
    }
    return currentSource.kind === 'user' && currentSource.path === p.path;
  }
  function loadEntry(p) {
    if (selectedFolder.kind === 'factory') {
      window.vxn.send.loadFactory(p.index);
    } else {
      window.vxn.send.loadUser(p.path);
    }
  }
  function setOpen(open) {
    isOpen = !!open;
    panelEl.hidden = !isOpen;
    backdropEl.hidden = !isOpen;
    if (isOpen) {
      renderFolders();
      renderPresets();
      try { inputEl.focus(); } catch (_) {}
    } else {
      closeMenu();
      closeModal();
    }
    if (onOpenChange) onOpenChange(isOpen);
  }
  function setCurrentSource(src) {
    currentSource = src || null;
    if (isOpen) renderPresets();
  }
  function getSaveFolder() {
    if (selectedFolder.kind !== 'user') return null;
    return selectedFolder.name;
  }

  // ── Context menu (0051) ──────────────────────────────────────────────
  //
  // Menu is a child of the browser panel so it inherits the panel's
  // clipping; coordinates are relative to the panel. Outside-click and
  // ESC both close it (panel ESC handler closes the whole panel after
  // closing any open menu first, so ESC is "close one level").
  function closeMenu() {
    if (menuEl) {
      menuEl.remove();
      menuEl = null;
    }
  }
  function openMenu(ev, target) {
    closeMenu();
    const m = document.createElement('div');
    m.className = 'browser-menu';
    // Position relative to the panel — convert client coords into panel-
    // local coords using the panel's bounding box.
    const rect = panelEl.getBoundingClientRect();
    m.style.left = (ev.clientX - rect.left) + 'px';
    m.style.top  = (ev.clientY - rect.top)  + 'px';

    // Rename — both targets use the same op shape internals; preset takes
    // `path`, folder takes `old_name`.
    const renameLabel = target.name;
    appendMenuItem(m, 'Rename', () => {
      closeMenu();
      window.vxn.promptText('Rename', renameLabel, (value) => {
        if (value == null) return;
        const trimmed = value.trim();
        if (!trimmed || trimmed === renameLabel) return;
        if (target.kind === 'preset') {
          window.vxn.send.renamePreset(target.path, trimmed);
        } else {
          window.vxn.send.renameFolder(target.name, trimmed);
        }
      });
    });
    // Delete — modal confirm. The menu closes first so the row text
    // isn't obscured behind the menu while the user reads the prompt.
    appendMenuItem(m, 'Delete', () => {
      closeMenu();
      openDeleteConfirm(target);
    });
    // Move to ▸ — preset only. Submenu lists user folders except the
    // current one; CSS shows the submenu on hover.
    if (target.kind === 'preset') {
      appendMoveSubmenu(m, target);
    }
    panelEl.appendChild(m);
    menuEl = m;
  }
  function appendMenuItem(parent, label, onClick) {
    const item = document.createElement('div');
    item.className = 'browser-menu-item';
    item.textContent = label;
    item.addEventListener('click', (e) => {
      e.stopPropagation();
      onClick();
    });
    parent.appendChild(item);
    return item;
  }
  function appendMoveSubmenu(parent, target) {
    const item = document.createElement('div');
    item.className = 'browser-menu-item has-submenu';
    item.textContent = 'Move to';
    const sub = document.createElement('div');
    sub.className = 'browser-submenu';
    const targets = moveTargets(target.folder, corpus);
    if (targets.length === 0) {
      const empty = document.createElement('div');
      empty.className = 'browser-submenu-empty';
      empty.textContent = 'no other folders';
      sub.appendChild(empty);
    } else {
      for (const t of targets) {
        const subItem = document.createElement('div');
        subItem.className = 'browser-submenu-item';
        subItem.textContent = t.label;
        subItem.addEventListener('click', (e) => {
          e.stopPropagation();
          closeMenu();
          window.vxn.send.movePreset(target.path, t.name);
        });
        sub.appendChild(subItem);
      }
    }
    item.appendChild(sub);
    parent.appendChild(item);
    return item;
  }
  // ── Modal confirm (0051) ─────────────────────────────────────────────
  //
  // Click Delete in the context menu → menu closes → modal opens, centred
  // inside the browser panel. Cancel / Esc dismisses; Delete posts the
  // corresponding UiEvent. Backdrop click cancels too.
  function openDeleteConfirm(target) {
    const isPreset = target.kind === 'preset';
    const kindLabel = isPreset ? 'preset' : 'folder';
    const name = target.name;
    const message = isPreset
      ? `Delete preset “${name}”? This cannot be undone.`
      : `Delete folder “${name}” and every preset inside it? This cannot be undone.`;
    openConfirmModal({
      title: `Delete ${kindLabel}`,
      message,
      confirmLabel: 'Delete',
      danger: true,
      onConfirm: () => {
        if (isPreset) {
          window.vxn.send.deletePreset(target.path);
        } else {
          window.vxn.send.deleteFolder(target.name);
        }
      },
    });
  }
  function closeModal() {
    if (modalEl) {
      modalEl.remove();
      modalEl = null;
    }
  }
  // Shared modal scaffold for both flows: backdrop, dialog frame, title,
  // body host, Cancel/OK actions row. Returns the OK button so callers
  // can wire validation directly. Anchors over the whole faceplate so
  // the modal works whether the browser panel is open or not.
  function mountModal({ title, danger, okLabel, onOk }) {
    closeModal();
    closeMenu();
    const wrap = document.createElement('div');
    wrap.className = 'browser-modal-wrap';

    const back = document.createElement('div');
    back.className = 'browser-modal-backdrop';
    back.addEventListener('click', closeModal);
    wrap.appendChild(back);

    const dialog = document.createElement('div');
    dialog.className = 'browser-modal';

    const t = document.createElement('div');
    t.className = 'browser-modal-title';
    t.textContent = title;
    dialog.appendChild(t);

    const body = document.createElement('div');
    body.className = 'browser-modal-body';
    dialog.appendChild(body);

    const actions = document.createElement('div');
    actions.className = 'browser-modal-actions';
    const cancel = document.createElement('button');
    cancel.type = 'button';
    cancel.className = 'browser-modal-btn';
    cancel.textContent = 'Cancel';
    cancel.addEventListener('click', closeModal);
    actions.appendChild(cancel);

    const ok = document.createElement('button');
    ok.type = 'button';
    ok.className = 'browser-modal-btn' + (danger ? ' danger' : '');
    ok.textContent = okLabel || 'OK';
    ok.addEventListener('click', () => {
      if (ok.disabled) return;
      try { onOk(); } catch (e) { console.warn('modal onConfirm threw', e); }
      closeModal();
    });
    actions.appendChild(ok);

    dialog.appendChild(actions);
    wrap.appendChild(dialog);

    document.getElementById('faceplate').appendChild(wrap);
    modalEl = wrap;
    return { body, ok };
  }
  function openConfirmModal({ title, message, confirmLabel, danger, onConfirm }) {
    const { body, ok } = mountModal({
      title,
      danger,
      okLabel: confirmLabel,
      onOk: onConfirm,
    });
    body.textContent = message;
    try { ok.focus(); } catch (_) {}
  }

  // ── Save As modal (0049 / 0051) ──────────────────────────────────────
  //
  // Single dialog with a name field + folder dropdown. The name field is
  // captured via the existing native popup (`promptText`) so Space + other
  // transport-mapped keys aren't swallowed by the host's NSEvent monitor;
  // the dropdown is a plain `<select>` (mouse-driven select uses WKWebView's
  // native popup menu — no key capture concerns). Save posts
  // `save_preset { name, folder }`.
  function openSaveAsModal(initialName) {
    let name = (initialName || '').trim();
    // Default folder selection: whichever folder the user has chosen in
    // the browser panel (user side only); factory selections collapse to
    // root, matching `getSaveFolder`.
    const initialFolder = (selectedFolder.kind === 'user') ? selectedFolder.name : null;
    let folder = initialFolder;

    const valid = () => name.length > 0;

    const { body, ok } = mountModal({
      title: 'Save preset as',
      okLabel: 'Save',
      onOk: () => {
        if (!valid()) return;
        window.vxn.send.savePreset(name, folder);
      },
    });
    body.classList.add('save-as-body');

    function gateOk() {
      const on = valid();
      ok.disabled = !on;
      ok.classList.toggle('disabled', !on);
    }

    // Name row: read-only-looking label + Edit button that funnels
    // through the native popup. Updates `name` and the label on commit.
    const nameRow = document.createElement('div');
    nameRow.className = 'save-as-row';
    const nameLab = document.createElement('div');
    nameLab.className = 'save-as-label';
    nameLab.textContent = 'Name';
    nameRow.appendChild(nameLab);
    const nameLabel = document.createElement('div');
    nameLabel.className = 'save-as-name';
    nameLabel.textContent = name || '(untitled)';
    if (!name) nameLabel.classList.add('placeholder');
    nameRow.appendChild(nameLabel);
    const editBtn = document.createElement('button');
    editBtn.type = 'button';
    editBtn.className = 'browser-modal-btn';
    editBtn.textContent = name ? 'Edit' : 'Name…';
    editBtn.addEventListener('click', () => {
      window.vxn.promptText('Preset name', name, (value) => {
        if (value == null) return;
        const trimmed = value.trim();
        if (!trimmed) return;
        name = trimmed;
        nameLabel.textContent = name;
        nameLabel.classList.remove('placeholder');
        editBtn.textContent = 'Edit';
        gateOk();
      });
    });
    nameRow.appendChild(editBtn);
    body.appendChild(nameRow);

    // Folder row: dropdown over user folders. Null = root
    // (Uncategorised); named folders sorted alpha. Mirrors the left-
    // pane order so the choices read the same.
    const folderRow = document.createElement('div');
    folderRow.className = 'save-as-row';
    const folderLab = document.createElement('div');
    folderLab.className = 'save-as-label';
    folderLab.textContent = 'Folder';
    folderRow.appendChild(folderLab);
    const select = document.createElement('select');
    select.className = 'save-as-select';
    for (const opt of folderOptions(corpus)) {
      const o = document.createElement('option');
      o.value = opt.value;
      o.textContent = opt.label;
      if (opt.value === folderValue(folder)) o.selected = true;
      select.appendChild(o);
    }
    select.addEventListener('change', () => {
      folder = (select.value === '__root__') ? null : select.value;
    });
    folderRow.appendChild(select);
    body.appendChild(folderRow);

    gateOk();
    try { ok.focus(); } catch (_) {}
  }

  inputEl.addEventListener('input', () => {
    query = inputEl.value || '';
    renderPresets();
  });
  clearEl.addEventListener('click', () => {
    inputEl.value = '';
    query = '';
    renderPresets();
    inputEl.focus();
  });
  backdropEl.addEventListener('click', () => setOpen(false));
  closeEl.addEventListener('click', (e) => {
    e.stopPropagation();
    setOpen(false);
  });
  // Click anywhere inside the panel that isn't the menu closes the menu
  // (matches the Vizia browser's overlay click-out).
  panelEl.addEventListener('click', (e) => {
    if (menuEl && !menuEl.contains(e.target)) closeMenu();
  });
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    if (!isOpen) return;
    e.preventDefault();
    // ESC closes one level: open modal → menu → panel.
    if (modalEl) { closeModal(); return; }
    if (menuEl)  { closeMenu();  return; }
    setOpen(false);
  });

  return {
    setCorpus,
    setCurrentSource,
    setOpen,
    isOpen: () => isOpen,
    getSaveFolder,
    openSaveAs: openSaveAsModal,
    onOpenChange: (cb) => { onOpenChange = cb; },
    // 0052: dispatcher calls this on `preset_corpus_changed` when
    // `ev.follow` is set (Move / Rename emit a follow path).
    followPath,
  };
})();
// Replace the bootstrap stub so future Rust pushes go straight to the
// panel; drain any snapshot that arrived during bootstrap. The
// `window.__vxn` / `_earlyPresetCorpus` guards short-circuit cleanly under
// Node ESM imports (E015 / 0077) where the bridge bootstrap hasn't run.
if (typeof window !== 'undefined' && window.__vxn) {
  window.__vxn.applyPresetCorpus = (snap) => browserPanel.setCorpus(snap);
  if (typeof _earlyPresetCorpus !== 'undefined' && _earlyPresetCorpus) {
    browserPanel.setCorpus(_earlyPresetCorpus);
    _earlyPresetCorpus = null;
  }
}
