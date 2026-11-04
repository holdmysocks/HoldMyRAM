'use strict';
// HoldMyRAM UI. Plain DOM, no framework. The page is destroyed when the window closes,
// so nothing here persists beyond the saved config.

const T = window.__TAURI__;
const invoke = (cmd, args) => T.core.invoke(cmd, args);
const $ = id => document.getElementById(id);

const PAGE = 200;
const KINDS = [
  ['all', 'All'], ['process', 'Processes'], ['service', 'Services'],
  ['startup', 'Startup'], ['task', 'Tasks'], ['app', 'Apps'],
];
const KIND_NOTE = {
  service: 'Services are shown for information. Closing services is not supported yet.',
  startup: 'Startup items run when you log in to Windows. Use the switch to stop one starting at login; every change is logged and can be undone. This is separate from the RAM toggle and hotkey, which do not apply to startup items. Items that need administrator rights are shown read-only.',
  task: 'Scheduled tasks are shown for information. Disabling them is not supported yet.',
  app: 'Installed apps are shown for information. Only running processes can be closed for now.',
  all: 'The toggle and hotkey only close running processes. Startup items have their own switch (it affects login only); services, tasks and apps are information only.',
};
const ADMIN_NOTE = 'Needs administrator rights, not changeable here yet';
const RISK_ORDER = { suggested: 0, caution: 1, unrated: 2, dangerous: 3 };

const S = {
  cfg: null,            // draft config (what the user is editing)
  savedJson: '',        // JSON of the last saved config, for the dirty check
  state: null,          // last get_state
  rows: [],             // last scan
  byKey: new Map(),
  view: [],             // filtered + sorted rows
  shown: 0,
  tab: 'all',
  scanning: false,
  expanded: new Set(),
  pending: new Set(),   // startup rows with a change in flight
};

/* ---------- helpers ---------- */

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}
const key = r => r.kind + '|' + r.name.toLowerCase();
function fmtRam(b) {
  if (!b) return '';
  const mb = b / 1048576;
  return mb >= 1024 ? (mb / 1024).toFixed(2) + ' GB' : mb.toFixed(1) + ' MB';
}
function showErr(msg) {
  const e = $('err');
  e.hidden = !msg;
  e.textContent = msg || '';
}
function cap(s) { return s.charAt(0).toUpperCase() + s.slice(1); }
const norm = s => s.trim().toLowerCase();

/* ---------- modal ---------- */

function ask({ title, text, input, ok = 'OK', cancel = 'Cancel', danger = false }) {
  return new Promise(resolve => {
    const d = $('dlg');
    $('dlg-title').textContent = title;
    const body = $('dlg-body');
    body.textContent = text || '';
    let field = null;
    if (input !== undefined) {
      field = el('input');
      field.type = 'text';
      field.value = input;
      body.appendChild(field);
    }
    const btns = $('dlg-buttons');
    btns.replaceChildren();
    const done = v => { d.close(); d.removeEventListener('cancel', onCancel); resolve(v); };
    const onCancel = () => resolve(field ? null : false);
    d.addEventListener('cancel', onCancel, { once: true });
    if (cancel) {
      const c = el('button', '', cancel);
      c.onclick = () => done(field ? null : false);
      btns.appendChild(c);
    }
    const o = el('button', danger ? 'danger' : 'primary', ok);
    o.onclick = () => done(field ? field.value : true);
    btns.appendChild(o);
    if (field) field.addEventListener('keydown', e => { if (e.key === 'Enter') o.click(); });
    d.showModal();
    (field || (cancel ? btns.firstChild : o)).focus();
    if (field) field.select();
  });
}

/* ---------- config / selections ---------- */

function activeProfile() {
  if (!S.cfg.active_profile) return null;
  return S.cfg.profiles.find(p => norm(p.name) === norm(S.cfg.active_profile)) || null;
}
/** The selection list being edited: the active profile's, else the default one. */
function sels() {
  const p = activeProfile();
  return p ? p.selections : S.cfg.selections;
}
function selIndex(name) {
  const n = norm(name);
  return sels().findIndex(s => norm(s.name) === n);
}
function isDirty() { return S.cfg && JSON.stringify(S.cfg) !== S.savedJson; }

function applyTheme() {
  const t = S.cfg ? S.cfg.theme : 'dark';
  document.documentElement.setAttribute('data-theme', ['dark', 'light', 'system'].includes(t) ? t : 'dark');
}

function setConfig(cfg, markSaved) {
  S.cfg = cfg;
  cfg.selections = cfg.selections || [];
  cfg.profiles = cfg.profiles || [];
  if (markSaved) S.savedJson = JSON.stringify(cfg);
  applyTheme();
}

/* ---------- header ---------- */

function renderHeader() {
  const st = S.state;
  const pill = $('state-pill');
  const busy = st && st.busy;
  const active = st && st.active;
  pill.className = 'pill ' + (busy ? 'busy' : active ? 'on' : 'off');
  pill.textContent = busy ? 'WORKING' : active ? 'ON (apps closed)' : 'OFF';
  const btn = $('toggle-btn');
  btn.textContent = active ? 'Turn off (restore)' : 'Turn on (free RAM)';
  const n = S.cfg ? sels().length : 0;
  btn.disabled = !!busy || (!active && n === 0);
  btn.title = !active && n === 0 ? 'Select at least one process first' : '';

  let ram = 0;
  if (S.cfg) for (const s of sels()) {
    const r = S.byKey.get('process|' + norm(s.name));
    if (r) ram += r.ram_bytes;
  }
  $('sel-summary').textContent = n + ' selected' + (ram ? ', about ' + fmtRam(ram) + ' in use' : '');

  const save = $('save-btn');
  const dirty = isDirty();
  save.classList.toggle('dirty', dirty);
  save.textContent = dirty ? 'Save *' : 'Save';
  save.disabled = !dirty;

  const sel = $('profile-sel');
  sel.replaceChildren();
  const base = el('option', '', 'Default selection');
  base.value = '';
  sel.appendChild(base);
  for (const p of S.cfg.profiles) {
    const o = el('option', '', p.name);
    o.value = p.name;
    sel.appendChild(o);
  }
  const ap = activeProfile();
  sel.value = ap ? ap.name : '';
  $('profile-rename').disabled = !ap;
  $('profile-delete').disabled = !ap;

  const wc = $('warn-config');
  wc.hidden = !(st && st.warning);
  wc.textContent = st && st.warning ? 'Config problem: ' + st.warning : '';
  const wh = $('warn-hotkey');
  const he = st && st.hotkey && st.hotkey.error;
  wh.hidden = !he;
  wh.textContent = he ? 'Hotkey: ' + he : '';
  const lr = $('last-report');
  lr.hidden = !(st && st.last_report);
  lr.textContent = st && st.last_report ? 'Last toggle: ' + st.last_report : '';
}

/* ---------- list ---------- */

function isMsTask(r) {
  if (r.kind !== 'task') return false;
  return /^\\?Microsoft/i.test(r.name) || /\\Microsoft\\/i.test(r.name) || /\\Microsoft\\/i.test(r.detail);
}

function buildView() {
  const q = norm($('search').value);
  const hideMs = $('hide-ms').checked;
  const onlySel = $('only-selected').checked;
  const sort = $('sort').value;
  const selSet = new Set(sels().map(s => norm(s.name)));
  const out = [];
  for (const r of S.rows) {
    if (S.tab !== 'all' && r.kind !== S.tab) continue;
    if (hideMs && isMsTask(r)) continue;
    if (onlySel && !(r.kind === 'process' && selSet.has(norm(r.name)))) continue;
    if (q && !(r._hay || (r._hay = (r.name + ' ' + r.display + ' ' + r.detail).toLowerCase())).includes(q)) continue;
    out.push(r);
  }
  if (sort === 'risk') {
    out.sort((a, b) => RISK_ORDER[a.risk] - RISK_ORDER[b.risk] || b.ram_bytes - a.ram_bytes);
  } else if (sort === 'name') {
    out.sort((a, b) => a.display.localeCompare(b.display, undefined, { sensitivity: 'base' }));
  } else {
    out.sort((a, b) => b.ram_bytes - a.ram_bytes || a.display.localeCompare(b.display));
  }
  S.view = out;
}

function renderTabs() {
  const counts = { all: 0 };
  const hideMs = $('hide-ms').checked;
  for (const r of S.rows) {
    if (hideMs && isMsTask(r)) continue;
    counts.all++;
    counts[r.kind] = (counts[r.kind] || 0) + 1;
  }
  const box = $('tabs');
  box.replaceChildren();
  for (const [k, label] of KINDS) {
    const b = el('button', 'tab' + (k === S.tab ? ' active' : ''));
    b.append(el('span', '', label), el('span', 'muted', String(counts[k] || 0)));
    b.onclick = () => { S.tab = k; renderTabs(); renderList(true); };
    box.appendChild(b);
  }
  const note = $('kind-note');
  note.hidden = false;
  note.textContent = KIND_NOTE[S.tab] || '';
}

function renderList(reset) {
  buildView();
  if (reset) { S.shown = Math.min(PAGE, S.view.length); $('list').scrollTop = 0; }
  else S.shown = Math.min(Math.max(S.shown, PAGE), S.view.length);
  const list = $('list');
  list.replaceChildren();
  if (!S.view.length) {
    list.appendChild(el('div', 'empty',
      S.scanning ? 'Scanning...' :
      S.rows.length ? 'Nothing matches the current filters.' :
      'Nothing found yet. If the scan is still being built, this list stays empty.'));
    renderHeader();
    return;
  }
  const frag = document.createDocumentFragment();
  for (let i = 0; i < S.shown; i++) frag.appendChild(buildRow(S.view[i]));
  list.appendChild(frag);
  if (S.shown < S.view.length) list.appendChild(moreButton());
  renderHeader();
}

function moreButton() {
  const d = el('div', 'more');
  const b = el('button', '', 'Show more (' + (S.view.length - S.shown) + ' left)');
  b.onclick = () => {
    const list = $('list');
    const end = Math.min(S.view.length, S.shown + PAGE);
    const frag = document.createDocumentFragment();
    for (let i = S.shown; i < end; i++) frag.appendChild(buildRow(S.view[i]));
    S.shown = end;
    d.remove();
    list.appendChild(frag);
    if (S.shown < S.view.length) list.appendChild(moreButton());
  };
  d.appendChild(b);
  return d;
}

function buildRow(r) {
  const k = key(r);
  const idx = r.actionable ? selIndex(r.name) : -1;
  const isStartup = r.kind === 'startup';
  const row = el('div', 'row' + (idx >= 0 ? ' sel' : '') + (r.actionable || (isStartup && r.can_change) ? '' : ' ro'));
  row.dataset.key = k;
  const line = el('div', 'line');

  if (r.actionable) {
    const cb = el('input');
    cb.type = 'checkbox';
    cb.checked = idx >= 0;
    cb.dataset.act = 'select';
    cb.setAttribute('aria-label', 'Select ' + r.display);
    line.appendChild(cb);
  } else {
    const t = el('span', 'tag', 'info');
    t.title = isStartup
      ? 'The RAM toggle does not apply to startup items; they only affect what starts at login.'
      : 'Not actionable yet: the toggle can only close processes.';
    line.appendChild(t);
  }

  const name = el('span', 'name', r.display || r.name);
  name.title = r.name;
  name.dataset.act = 'expand';
  if (isStartup) name.appendChild(el('small', '', r.can_change ? 'startup, applies at login' : 'startup, read-only'));
  else if (!r.actionable) name.appendChild(el('small', '', r.kind + ', not actionable yet'));
  else if (r.pids.length > 1) name.appendChild(el('small', '', r.pids.length + ' processes'));
  line.appendChild(name);

  line.appendChild(el('span', 'cell-ram', fmtRam(r.ram_bytes)));
  line.appendChild(el('span', 'tag', r.kind === 'process' || r.kind === 'service'
    ? (r.running ? 'running' : 'stopped') : (r.running ? 'enabled' : 'disabled')));

  const badge = el('span', 'badge ' + r.risk, cap(r.risk));
  line.appendChild(badge);

  if (r.actionable) {
    const m = el('select');
    m.dataset.act = 'method';
    m.setAttribute('aria-label', 'Method for ' + r.display);
    for (const v of ['close', 'suspend', 'kill']) {
      const o = el('option', '', cap(v));
      o.value = v;
      m.appendChild(o);
    }
    m.value = idx >= 0 ? (sels()[idx].method || 'close') : 'close';
    m.disabled = idx < 0;
    line.appendChild(m);
  } else if (isStartup && r.can_change) {
    const sw = el('label', 'switch' + (r.running ? ' on' : ''));
    const cb = el('input');
    cb.type = 'checkbox';
    cb.setAttribute('role', 'switch');
    cb.checked = !!r.running;
    cb.dataset.act = 'startup';
    cb.disabled = S.pending.has(k);
    cb.setAttribute('aria-label', (r.running ? 'Enabled' : 'Disabled') + ' at login: ' + r.display);
    sw.append(cb, el('span', 'track'), el('span', 'sw-text', r.running ? 'Enabled' : 'Disabled'));
    line.appendChild(sw);
  } else {
    line.appendChild(el('span'));
  }

  const x = el('button', 'expander', S.expanded.has(k) ? '▾' : '▸');
  x.dataset.act = 'expand';
  x.setAttribute('aria-label', 'Show explanation');
  line.appendChild(x);

  row.appendChild(line);
  if (isStartup && !r.can_change) row.appendChild(el('div', 'ro-note', ADMIN_NOTE));
  if (S.expanded.has(k)) row.appendChild(buildDetails(r));
  return row;
}

function buildDetails(r) {
  const d = el('div', 'details');
  d.appendChild(el('p', '', r.summary));
  if (r.notes) d.appendChild(el('p', 'muted', r.notes));
  if (r.detail) {
    const p = el('p');
    p.appendChild(el('span', 'kv', 'Details: '));
    p.appendChild(document.createTextNode(r.detail));
    d.appendChild(p);
  }
  if (r.name !== r.display) {
    const p = el('p');
    p.appendChild(el('span', 'kv', 'Name: '));
    p.appendChild(document.createTextNode(r.name));
    d.appendChild(p);
  }
  if (r.kind === 'startup') {
    d.appendChild(el('p', 'muted', (r.can_change
      ? 'The switch controls whether this item starts when you log in. '
      : ADMIN_NOTE + '. ') +
      'The RAM toggle and hotkey do not apply to startup items.'));
  } else if (!r.actionable) {
    d.appendChild(el('p', 'muted', 'Not actionable yet: the toggle can currently only close, suspend or kill running processes.'));
  }
  return d;
}

function refreshRow(rowEl, r) {
  const fresh = buildRow(r);
  rowEl.replaceWith(fresh);
}

/* ---------- list events (delegated) ---------- */

$('list').addEventListener('click', e => {
  const t = e.target.closest('[data-act="expand"]');
  if (!t) return;
  const rowEl = t.closest('.row');
  const r = S.byKey.get(rowEl.dataset.key);
  if (!r) return;
  const k = rowEl.dataset.key;
  if (S.expanded.has(k)) S.expanded.delete(k); else S.expanded.add(k);
  refreshRow(rowEl, r);
});

$('list').addEventListener('change', async e => {
  const t = e.target;
  const rowEl = t.closest('.row');
  if (!rowEl) return;
  const r = S.byKey.get(rowEl.dataset.key);
  if (r && r.kind === 'startup' && r.can_change && t.dataset.act === 'startup') {
    await onStartupSwitch(r, t.checked);
    return;
  }
  if (!r || !r.actionable) return;
  if (t.dataset.act === 'select') {
    if (t.checked) {
      if (r.risk === 'dangerous') {
        const ok = await ask({
          title: 'Dangerous item',
          text: r.display + ' is rated Dangerous.\n\n' + r.summary + (r.notes ? '\n\n' + r.notes : '') +
            '\n\nClosing it can break Windows or hardware features. Select it anyway?',
          ok: 'Select anyway', danger: true,
        });
        if (!ok) { t.checked = false; return; }
      }
      if (selIndex(r.name) < 0) sels().push({ name: r.name, method: 'close' });
    } else {
      const i = selIndex(r.name);
      if (i >= 0) sels().splice(i, 1);
    }
    if ($('only-selected').checked && !t.checked) renderList(false); else refreshRow(rowEl, r);
    renderHeader();
  } else if (t.dataset.act === 'method') {
    const i = selIndex(r.name);
    if (i >= 0) sels()[i].method = t.value;
    renderHeader();
  }
});

/* ---------- startup items ---------- */

function rowElFor(r) {
  const k = key(r);
  for (const e of $('list').querySelectorAll('.row')) if (e.dataset.key === k) return e;
  return null;
}
function redrawRow(r) {
  const e = rowElFor(r);
  if (e) refreshRow(e, r);
}

let toastTimer = 0;
function toast(text, undo) {
  const t = $('toast');
  clearTimeout(toastTimer);
  $('toast-text').textContent = text;
  const u = $('toast-undo');
  u.hidden = !undo;
  u.onclick = undo ? () => { t.hidden = true; undo(); } : null;
  t.hidden = false;
  toastTimer = setTimeout(() => { t.hidden = true; }, 15000);
}
$('toast-close').onclick = () => { $('toast').hidden = true; };

/** Enable or disable one startup item and update its row in place (no rescan). */
async function setStartup(r, enabled, opts = {}) {
  const k = key(r);
  S.pending.add(k);
  redrawRow(r);
  try {
    const ch = await invoke('set_startup_enabled', { name: r.name, detail: r.detail, enabled });
    r.running = ch && typeof ch.now_enabled === 'boolean' ? ch.now_enabled : enabled;
    showErr('');
    if (!opts.silent) {
      toast(r.display + (r.running ? ' will start at login again. ' : ' will no longer start at login. '),
        () => setStartup(r, !r.running));
    }
    if ($('startup-dlg').open) renderStartupChanges();
  } catch (e) {
    showErr('Could not change ' + r.display + ': ' + e);
  }
  S.pending.delete(k);
  redrawRow(r);
}

async function onStartupSwitch(r, want) {
  const k = key(r);
  if (S.pending.has(k)) return;
  if (!want && (r.risk === 'dangerous' || r.risk === 'caution')) {
    const dangerous = r.risk === 'dangerous';
    const ok = await ask({
      title: dangerous ? 'Dangerous item' : 'Disable at login?',
      text: r.display + ' is rated ' + cap(r.risk) + '.\n\n' + r.summary + (r.notes ? '\n\n' + r.notes : '') +
        '\n\n' + (dangerous ? 'Stopping it starting at login can break Windows or hardware features. ' : '') +
        'You can undo this from the Startup changes list.',
      ok: 'Disable at login', danger: dangerous,
    });
    if (!ok) { redrawRow(r); return; }
  }
  await setStartup(r, want);
}

async function renderStartupChanges() {
  const box = $('startup-list');
  let list = [];
  try {
    list = await invoke('startup_changes');
  } catch (e) {
    box.replaceChildren(el('p', 'bad-text', 'Could not read the changes: ' + e));
    return;
  }
  box.replaceChildren();
  if (!list.length) {
    box.appendChild(el('p', 'muted', 'No startup changes yet.'));
    return;
  }
  for (const c of list.slice().reverse()) {
    const row = el('div', 'chg');
    const what = el('div', 'chg-what');
    what.appendChild(el('strong', '', c.name));
    what.appendChild(el('span', 'muted small', ' ' + (c.source || '') + ' - ' + (c.now_enabled ? 'enabled' : 'disabled') + ' at ' + c.at));
    row.appendChild(what);
    const b = el('button', '', 'Undo');
    b.title = 'Set it back to ' + (c.was_enabled ? 'enabled' : 'disabled');
    b.onclick = async () => {
      b.disabled = true;
      const r = S.byKey.get('startup|' + c.name.toLowerCase());
      if (r) {
        await setStartup(r, c.was_enabled, { silent: true });
      } else {
        // Not in the current scan: the source text alone is enough for the core to find it.
        try {
          await invoke('set_startup_enabled', { name: c.name, detail: 'source: ' + c.source, enabled: c.was_enabled });
          showErr('');
        } catch (e) { showErr('Could not undo ' + c.name + ': ' + e); }
      }
      renderStartupChanges();
    };
    row.appendChild(b);
    box.appendChild(row);
  }
}

$('startup-btn').onclick = () => { renderStartupChanges(); $('startup-dlg').showModal(); };
$('startup-close').onclick = () => $('startup-dlg').close();

/* ---------- scan / state ---------- */

async function rescan() {
  if (S.scanning) return;
  S.scanning = true;
  $('scan-status').textContent = 'Scanning...';
  $('rescan').disabled = true;
  renderList(false);
  try {
    // Selected processes that are not running still need a row so they can be deselected.
    const extra = sels().map(s => s.name);
    const rows = await invoke('scan', { extra });
    S.rows = rows;
    S.byKey = new Map(rows.map(r => [key(r), r]));
    $('scan-status').textContent = rows.length + ' items';
    showErr('');
  } catch (e) {
    showErr('Scan failed: ' + e);
    $('scan-status').textContent = '';
  }
  S.scanning = false;
  S.lastScanAt = Date.now();
  $('rescan').disabled = false;
  renderTabs();
  renderList(true);
}

/* ---------- silent refresh on focus (no polling: only runs when the window regains focus) ---------- */

const MIN_SCAN_GAP_MS = 2000;
const rowSig = r => JSON.stringify(r, (k, v) => (k.charAt(0) === '_' ? undefined : v));

/** Merge a fresh scan into S.rows, keeping object identity so open closures stay valid. */
function mergeRows(fresh) {
  const seen = new Set();
  const changed = [];
  let structural = false;
  for (const n of fresh) {
    const k = key(n);
    seen.add(k);
    const old = S.byKey.get(k);
    if (!old) {
      S.rows.push(n);
      S.byKey.set(k, n);
      structural = true;
    } else if (!S.pending.has(k) && rowSig(old) !== rowSig(n)) {
      for (const f of Object.keys(old)) if (f.charAt(0) === '_') delete old[f];
      Object.assign(old, n);
      changed.push(old);
    }
  }
  for (const [k, old] of S.byKey) {
    if (seen.has(k) || S.pending.has(k)) continue;
    S.byKey.delete(k);
    S.expanded.delete(k);
    structural = true;
  }
  if (structural) S.rows = S.rows.filter(r => S.byKey.get(key(r)) === r);
  return { changed, structural };
}

async function silentRefresh() {
  if (!S.cfg || S.scanning || S.pending.size) return;
  if (Date.now() - (S.lastScanAt || 0) < MIN_SCAN_GAP_MS) return;
  // Any modal other than the Startup changes panel blocks the refresh.
  for (const d of document.querySelectorAll('dialog[open]')) if (d.id !== 'startup-dlg') return;
  S.scanning = true;
  let fresh = null;
  try {
    fresh = await invoke('scan', { extra: sels().map(s => s.name) });
  } catch (_) { /* silent: keep the current view */ }
  S.scanning = false;
  S.lastScanAt = Date.now();
  if (!fresh) return;
  if ($('startup-dlg').open) renderStartupChanges();
  const { changed, structural } = mergeRows(fresh);
  if (!changed.length && !structural) return;
  const list = $('list');
  const top = list.scrollTop;
  const before = S.view.slice(0, S.shown).map(key).join('\n');
  const oldShown = S.shown, oldLen = S.view.length;
  buildView();
  const after = S.view.slice(0, Math.min(oldShown, S.view.length)).map(key).join('\n');
  if (structural || before !== after || S.view.length !== oldLen) {
    renderTabs();
    renderList(false);
    list.scrollTop = top;
  } else {
    for (const r of changed) redrawRow(r);
    renderHeader();
  }
  $('scan-status').textContent = S.rows.length + ' items, updated';
}

let focusTimer = 0;
function onRegainFocus() {
  clearTimeout(focusTimer);
  focusTimer = setTimeout(silentRefresh, 150); // one-shot debounce, not a loop
}
window.addEventListener('focus', onRegainFocus);
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible') onRegainFocus();
});

async function refreshState() {
  try {
    S.state = await invoke('get_state');
  } catch (e) { showErr('Could not read state: ' + e); }
  if (S.cfg) renderHeader();
}

async function save() {
  try {
    const st = await invoke('save_config', { config: S.cfg });
    S.state = st;
    S.savedJson = JSON.stringify(S.cfg);
    showErr('');
  } catch (e) {
    showErr('Could not save: ' + e);
    return false;
  }
  renderHeader();
  return true;
}

$('save-btn').onclick = save;

$('toggle-btn').onclick = async () => {
  if (isDirty()) {
    const ok = await ask({
      title: 'Unsaved changes',
      text: 'The toggle uses the saved selection. Save your changes and continue?',
      ok: 'Save and continue',
    });
    if (!ok || !(await save())) return;
  }
  try {
    await invoke('toggle_now');
  } catch (e) { showErr('Toggle failed: ' + e); }
  await refreshState();
};

/* ---------- profiles ---------- */

function profileNameError(name, except) {
  const n = norm(name);
  if (!n) return 'Enter a name.';
  if (S.cfg.profiles.some(p => norm(p.name) === n && norm(p.name) !== norm(except || ''))) return 'A profile with that name already exists.';
  return '';
}

$('profile-sel').onchange = e => {
  S.cfg.active_profile = e.target.value || null;
  renderList(false);
};

$('profile-new').onclick = async () => {
  let name = await ask({ title: 'New profile', text: 'Name for the new profile. It starts as a copy of the current selection.', input: '', ok: 'Create' });
  while (name !== null) {
    const err = profileNameError(name);
    if (!err) break;
    name = await ask({ title: 'New profile', text: err, input: name.trim(), ok: 'Create' });
  }
  if (name === null) return;
  const copy = sels().map(s => ({ name: s.name, method: s.method }));
  S.cfg.profiles.push({ name: name.trim(), selections: copy });
  S.cfg.active_profile = name.trim();
  renderList(false);
};

$('profile-rename').onclick = async () => {
  const p = activeProfile();
  if (!p) return;
  let name = await ask({ title: 'Rename profile', input: p.name, ok: 'Rename' });
  while (name !== null) {
    const err = profileNameError(name, p.name);
    if (!err) break;
    name = await ask({ title: 'Rename profile', text: err, input: name.trim(), ok: 'Rename' });
  }
  if (name === null) return;
  p.name = name.trim();
  S.cfg.active_profile = p.name;
  renderHeader();
};

$('profile-delete').onclick = async () => {
  const p = activeProfile();
  if (!p) return;
  const ok = await ask({ title: 'Delete profile', text: 'Delete "' + p.name + '"? This applies when you press Save.', ok: 'Delete', danger: true });
  if (!ok) return;
  S.cfg.profiles = S.cfg.profiles.filter(x => x !== p);
  S.cfg.active_profile = null;
  renderList(false);
};

/* ---------- settings ---------- */

function renderSettings() {
  $('set-hotkey').value = S.cfg.hotkey;
  $('set-theme').value = S.cfg.theme;
  $('set-grace').value = S.cfg.grace_seconds;
  if (!$('set-path').value && S.state) $('set-path').value = S.state.app_dir + '\\holdmyram-export.toml';
  $('set-info').textContent = S.state ? 'Config is stored at ' + S.state.config_path : '';
  const h = S.state && S.state.hotkey;
  const hs = $('hotkey-status');
  if (h && h.active) {
    hs.className = 'status-line ' + (h.error ? 'bad-text' : 'ok-text');
    hs.textContent = 'Active hotkey: ' + h.active + (h.error ? ' (' + h.error + ')' : '');
  } else {
    hs.className = 'status-line bad-text';
    hs.textContent = h && h.error ? h.error : '';
  }
}

$('settings-btn').onclick = () => { renderSettings(); $('set-msg').textContent = ''; $('settings').showModal(); };
$('settings-close').onclick = () => { stopRecording(); $('settings').close(); };
$('set-hotkey').oninput = e => { S.cfg.hotkey = e.target.value; renderHeader(); };
$('set-theme').onchange = e => { S.cfg.theme = e.target.value; applyTheme(); renderHeader(); };
$('set-grace').onchange = e => {
  let v = parseInt(e.target.value, 10);
  if (!Number.isFinite(v) || v < 0) v = 0;
  if (v > 300) v = 300;
  e.target.value = v;
  S.cfg.grace_seconds = v;
  renderHeader();
};

let recording = false;
function stopRecording() {
  recording = false;
  $('set-record').textContent = 'Record';
  document.removeEventListener('keydown', onRecordKey, true);
}
function keyName(e) {
  const c = e.code;
  if (/^Key[A-Z]$/.test(c)) return c.slice(3);
  if (/^Digit[0-9]$/.test(c)) return c.slice(5);
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(c)) return c;
  const map = { ArrowUp: 'Up', ArrowDown: 'Down', ArrowLeft: 'Left', ArrowRight: 'Right' };
  if (map[c]) return map[c];
  if (/^(Space|Enter|Tab|Backspace|Delete|Insert|Home|End|PageUp|PageDown)$/.test(c)) return c;
  return null;
}
function onRecordKey(e) {
  e.preventDefault();
  e.stopPropagation();
  if (e.key === 'Escape') { stopRecording(); return; }
  if (['Control', 'Alt', 'Shift', 'Meta'].includes(e.key)) return;
  const k = keyName(e);
  const mods = [];
  if (e.ctrlKey) mods.push('Ctrl');
  if (e.altKey) mods.push('Alt');
  if (e.shiftKey) mods.push('Shift');
  if (e.metaKey) mods.push('Super');
  if (!k || !mods.length) {
    $('set-msg').textContent = 'Use at least one modifier (Ctrl, Alt, Shift) plus a letter, digit or F-key.';
    return;
  }
  $('set-hotkey').value = mods.concat(k).join('+');
  S.cfg.hotkey = $('set-hotkey').value;
  $('set-msg').textContent = 'Recorded. Press Save to apply.';
  stopRecording();
  renderHeader();
}
$('set-record').onclick = () => {
  if (recording) { stopRecording(); return; }
  recording = true;
  $('set-record').textContent = 'Press keys... (Esc cancels)';
  $('set-msg').textContent = '';
  document.addEventListener('keydown', onRecordKey, true);
};
$('settings').addEventListener('close', stopRecording);

$('set-export').onclick = async () => {
  try {
    const p = await invoke('export_config', { path: $('set-path').value, config: S.cfg });
    $('set-msg').className = 'status-line ok-text';
    $('set-msg').textContent = 'Exported to ' + p;
  } catch (e) {
    $('set-msg').className = 'status-line bad-text';
    $('set-msg').textContent = 'Export failed: ' + e;
  }
};
$('set-import').onclick = async () => {
  try {
    const cfg = await invoke('import_config', { path: $('set-path').value });
    setConfig(cfg, false);
    renderSettings();
    renderList(false);
    $('set-msg').className = 'status-line ok-text';
    $('set-msg').textContent = 'Imported as unsaved changes. Press Save to keep them.';
  } catch (e) {
    $('set-msg').className = 'status-line bad-text';
    $('set-msg').textContent = 'Import failed: ' + e;
  }
};

$('quit-btn').onclick = async () => {
  const st = S.state;
  const ok = await ask({
    title: 'Quit HoldMyRAM',
    text: st && st.active
      ? 'The free-RAM set is ON. HoldMyRAM will restore the apps it closed, then quit. This can take a few seconds.'
      : 'Quit HoldMyRAM and remove it from the tray?',
    ok: 'Quit', danger: !(st && st.active),
  });
  if (ok) invoke('quit_app');
};

/* ---------- filters ---------- */

let searchTimer = 0;
$('search').addEventListener('input', () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => renderList(true), 120);
});
$('sort').onchange = () => renderList(true);
$('only-selected').onchange = () => renderList(true);
$('hide-ms').onchange = () => {
  try { localStorage.setItem('hideMs', $('hide-ms').checked ? '1' : '0'); } catch (_) { /* ignore */ }
  renderTabs();
  renderList(true);
};
$('rescan').onclick = rescan;

/* ---------- start ---------- */

(async function init() {
  try { if (localStorage.getItem('hideMs') === '0') $('hide-ms').checked = false; } catch (_) { /* ignore */ }
  try {
    const st = await invoke('get_state');
    S.state = st;
    setConfig(st.config, true);
  } catch (e) {
    setConfig({ hotkey: 'Ctrl+Alt+R', theme: 'dark', grace_seconds: 5, selections: [], profiles: [], active_profile: null }, true);
    showErr('Could not read state: ' + e);
  }
  renderTabs();
  renderHeader();
  try {
    await T.event.listen('state-changed', refreshState);
  } catch (_) { /* events unavailable; the state refreshes on the next action */ }
  rescan();
})();
