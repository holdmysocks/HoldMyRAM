'use strict';
// HoldMyRAM UI core: shared state, helpers, dialogs, config drafts, header, navigation.
// Plain DOM, no framework. The page is destroyed when the window closes.
// No polling anywhere: work happens on user action, on window focus, or on backend events.

const T = window.__TAURI__;
const invoke = (cmd, args) => T.core.invoke(cmd, args);
const $ = id => document.getElementById(id);

const TABS = ['home', 'advanced', 'profiles', 'settings'];
const BUILTIN_FALLBACK = ['Recommended', 'Recommended+'];

// Plain-language reasons. Used for the last toggle result, skipped items and import problems.
const REASON_TEXT = {
  foreground: 'it is the app you are using (or part of it), so it was left running',
  self_process: 'it is HoldMyRAM itself',
  protected: 'it is a protected system item',
  never_touch: 'it is on the never-touch list and is never changed',
  protected_dependent: 'stopping it would also stop a protected service',
  locked: 'Windows does not allow this change, even for an administrator',
  not_stoppable: 'the service does not accept a stop request',
  not_found: 'it no longer exists',
  invalid: 'the name or path was not valid',
  not_armed: 'the admin helper was not running',
  uac_cancelled: 'administrator approval was declined',
  helper_failed: 'the admin helper could not be started or stopped responding',
  timeout: 'it took too long',
  already_done: 'it was already in the wanted state',
  changed_by_user: 'you changed it afterwards, so it was left as you set it',
  ambiguous: 'it was selected twice with different methods',
  not_present: 'it is not on this PC',
  not_running: 'it is not running',
  disabled: 'it is already disabled',
  dangerous: 'it is rated Dangerous, so it is never added automatically',
  not_scanned: 'it has not been scanned yet',
  condition_not_met: 'its rule does not apply to this PC',
  duplicate: 'it was already in the profile',
  unknown: 'it is not something HoldMyRAM knows about',
};

const S = {
  cfg: null,            // draft config (what the user is editing)
  savedJson: '',        // snapshot of the last saved config, for the dirty check
  state: null,          // last get_state
  tab: 'home',
  helper: { armed: false, arming: false, last_error: null, toggle_declined: false, admin_pending: 0 },
  pv: { seq: 0, data: null, error: '', loading: false, timer: 0, open: false },
  upd: null, updErr: '', updDismissed: false,
  auto: null, autoErr: '',
};

/* ---------- helpers ---------- */

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}
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
const cap = s => s.charAt(0).toUpperCase() + s.slice(1);
const norm = s => String(s == null ? '' : s).trim().toLowerCase();
const plural = (n, one, many) => n + ' ' + (n === 1 ? one : many || one + 's');
const reasonText = r => REASON_TEXT[r] || String(r || '').replace(/_/g, ' ');
function lsGet(k) { try { return localStorage.getItem(k); } catch (_) { return null; } }
function lsSet(k, v) { try { localStorage.setItem(k, v); } catch (_) { /* ignore */ } }

/* ---------- modal ---------- */

/** Simple modal. `node` adds extra content under the text. Resolves true/false, or the text / null with `input`. */
function ask({ title, text, input, ok = 'OK', cancel = 'Cancel', danger = false, node = null }) {
  return new Promise(resolve => {
    const d = $('dlg');
    $('dlg-title').textContent = title;
    const body = $('dlg-body');
    body.replaceChildren();
    if (text) body.appendChild(document.createTextNode(text));
    if (node) body.appendChild(node);
    let field = null;
    if (input !== undefined) {
      field = el('input');
      field.type = 'text';
      field.value = input;
      field.setAttribute('aria-label', title);
      body.appendChild(field);
    }
    const btns = $('dlg-buttons');
    btns.replaceChildren();
    let finished = false;
    const finish = v => { if (finished) return; finished = true; d.removeEventListener('cancel', onCancel); if (d.open) d.close(); resolve(v); };
    const onCancel = () => finish(field ? null : false);
    d.addEventListener('cancel', onCancel);
    if (cancel) {
      const c = el('button', '', cancel);
      c.type = 'button';
      c.onclick = () => finish(field ? null : false);
      btns.appendChild(c);
    }
    const o = el('button', danger ? 'danger' : 'primary', ok);
    o.type = 'button';
    o.onclick = () => finish(field ? field.value : true);
    btns.appendChild(o);
    if (field) field.addEventListener('keydown', e => { if (e.key === 'Enter') o.click(); });
    d.showModal();
    (field || (cancel && danger ? btns.firstChild : o)).focus();
    if (field) field.select();
  });
}

/** Dangerous services/tasks: the user must type the item's name. Resolves true/false. */
async function askTyped({ title, text, expect, ok }) {
  const want = expect.filter(Boolean);
  let msg = text + '\n\nType "' + want[0] + '" to confirm.';
  for (;;) {
    const v = await ask({ title, text: msg, input: '', ok, danger: true });
    if (v === null || v === false) return false;
    if (want.some(w => norm(w) === norm(v))) return true;
    msg = text + '\n\nThat does not match. Type "' + want[0] + '" to confirm.';
  }
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

/* ---------- config / selections ---------- */

const infos = () => (S.state && S.state.profiles) || [];
function isBuiltinName(n) {
  const k = norm(n);
  if (!k) return false;
  return infos().some(p => p.builtin && norm(p.name) === k) || BUILTIN_FALLBACK.some(b => norm(b) === k);
}
const activeName = () => (S.cfg && S.cfg.active_profile) || '';
const activeIsBuiltin = () => isBuiltinName(activeName());
/** The user profile being edited, or null (built-in active, or the unnamed default selection). */
function activeProfile() {
  const n = activeName();
  if (!n || activeIsBuiltin()) return null;
  return S.cfg.profiles.find(p => norm(p.name) === norm(n)) || null;
}
/** Built-ins cannot be edited. Everything else (a user profile or the default selection) can. */
const editable = () => !activeIsBuiltin();
/** The selection list being edited: the active user profile's, else the default one. */
function sels() {
  const p = activeProfile();
  return p ? p.selections : S.cfg.selections;
}
/** Selections are keyed by kind + name, so a process and a service with the same name never collide. */
const selKey = (kind, name) => (kind || 'process') + '|' + norm(name);
const selKindOf = s => s.kind || 'process';

/** Snapshot used by the dirty check. The tutorial flag is saved by its own command, so it is ignored. */
const snap = cfg => JSON.stringify(Object.assign({}, cfg, { tutorial_seen: false }));
function isDirty() { return !!S.cfg && snap(S.cfg) !== S.savedJson; }

function applyTheme() {
  const t = S.cfg ? S.cfg.theme : 'dark';
  document.documentElement.setAttribute('data-theme', ['dark', 'light', 'system'].includes(t) ? t : 'dark');
}

function setConfig(cfg, markSaved) {
  S.cfg = cfg;
  cfg.selections = cfg.selections || [];
  cfg.profiles = cfg.profiles || [];
  if (cfg.active_profile === undefined) cfg.active_profile = null;
  cfg.recommended_addons = cfg.recommended_addons || [];
  cfg.recommended_addon_groups = cfg.recommended_addon_groups || [];
  const fix = s => { if (!s.kind) s.kind = 'process'; };
  cfg.selections.forEach(fix);
  cfg.profiles.forEach(p => (p.selections = p.selections || []).forEach(fix));
  if (markSaved) S.savedJson = snap(cfg);
  applyTheme();
}

/** Take the config from a state returned by the backend (after duplicate / import / tutorial). */
function adoptState(st) {
  S.state = st;
  setConfig(st.config, true);
}

async function save() {
  try {
    if (S.state) S.cfg.tutorial_seen = S.state.tutorial_seen;
    const st = await invoke('save_config', { config: S.cfg });
    S.state = st;
    S.savedJson = snap(S.cfg);
    showErr('');
  } catch (e) {
    showErr('Could not save: ' + e);
    return false;
  }
  onSaved();
  return true;
}
$('save-btn').onclick = save;

async function refreshState() {
  try {
    const st = await invoke('get_state');
    S.state = st;
    // Pick up outside changes (tray, hotkey) to the saved config only when there are no unsaved edits.
    if (!isDirty() && snap(st.config) !== S.savedJson) setConfig(st.config, true);
  } catch (e) { showErr('Could not read state: ' + e); }
  await refreshHelper();
  renderHeader();
  renderCurrent();
}

/* ---------- admin helper ---------- */

async function refreshHelper() {
  try { S.helper = await invoke('helper_status'); } catch (_) { /* keep the last known status */ }
  renderHelper();
}

function renderHelper() {
  const h = S.helper;
  const pill = $('helper-pill');
  pill.className = 'pill ' + (h.arming ? 'wait' : h.armed ? 'on' : 'off');
  pill.textContent = h.arming ? 'waiting for approval' : h.armed ? 'approved' : 'not approved';
  $('helper-spin').hidden = !h.arming;
  const arm = $('helper-arm');
  arm.hidden = h.armed;
  arm.disabled = h.arming;
  arm.textContent = h.arming ? 'Waiting for approval...' : 'Approve admin helper';
  $('helper-disarm').hidden = !h.armed;
  $('helper-disarm').disabled = !!(S.state && S.state.busy);
  const er = $('helper-err');
  er.hidden = !h.last_error;
  er.textContent = h.last_error ? 'Last error: ' + h.last_error : '';
  $('warn-declined').hidden = !h.toggle_declined;
  $('warn-pending').hidden = !(h.admin_pending > 0);
}

$('helper-arm').onclick = async () => {
  if (S.helper.arming) return;
  S.helper = Object.assign({}, S.helper, { arming: true });
  renderHelper();
  try {
    S.helper = await invoke('arm_helper');
    showErr('');
  } catch (e) {
    showErr('Could not approve the admin helper: ' + e);
    await refreshHelper();
  }
  renderHeader();
  renderHome();
};
$('helper-disarm').onclick = async () => {
  try { await invoke('disarm_helper'); } catch (e) { showErr('Could not disarm the admin helper: ' + e); }
  await refreshHelper();
  renderHeader();
};

/* ---------- header ---------- */

function renderHeader() {
  const st = S.state;
  const busy = st && st.busy;
  const active = st && st.active;
  const pill = $('state-pill');
  pill.className = 'pill ' + (busy ? 'busy' : active ? 'on' : 'off');
  pill.textContent = busy ? 'WORKING' : active ? 'ON' : 'OFF';

  const dirty = isDirty();
  const save = $('save-btn');
  save.classList.toggle('dirty', dirty);
  save.disabled = !dirty;
  $('unsaved').hidden = !dirty;

  const wc = $('warn-config');
  wc.hidden = !(st && st.warning);
  wc.textContent = st && st.warning ? 'Config problem: ' + st.warning : '';
  const wh = $('warn-hotkey');
  const he = st && st.hotkey && st.hotkey.error;
  wh.hidden = !he;
  wh.textContent = he ? 'Hotkey: ' + he : '';
}

/* ---------- navigation ---------- */

function showTab(t, opts = {}) {
  if (!TABS.includes(t)) t = 'home';
  if (S.tab === 'settings' && t !== 'settings') stopRecording();
  S.tab = t;
  if (!opts.noStore) lsSet('hmr.tab', t);
  for (const name of TABS) $('screen-' + name).hidden = name !== t;
  for (const b of $('main-nav').querySelectorAll('button')) {
    if (b.dataset.tab === t) b.setAttribute('aria-current', 'page'); else b.removeAttribute('aria-current');
  }
  renderCurrent();
  if (t === 'advanced') { ensureScan(); if (!editable() && !S.pv.data) schedulePreview(0); }
  if (t === 'home') schedulePreview(0);
  if (t === 'settings') { loadAutostart(); loadUpdate(); }
  const m = document.querySelector('main');
  if (m) m.scrollTop = 0;
}
$('main-nav').addEventListener('click', e => {
  const b = e.target.closest('button[data-tab]');
  if (b) showTab(b.dataset.tab);
});

function renderCurrent() {
  switch (S.tab) {
    case 'home': renderHome(); break;
    case 'advanced': renderAdvanced(); break;
    case 'profiles': renderProfiles(); break;
    case 'settings': renderSettings(); break;
  }
}

/** Called after any change to the draft config. */
function onCfgChange(opts = {}) {
  renderHeader();
  renderCurrent();
  if (opts.preview !== false && S.tab === 'home') schedulePreview(opts.delay === undefined ? 250 : opts.delay);
}

/** Called after a successful save. */
function onSaved() {
  renderHeader();
  renderCurrent();
  if (S.tab === 'home') schedulePreview(0);
}

/* ---------- last toggle report ---------- */

function friendlySummary(t) {
  return t.replace(/need admin approval/, 'need administrator approval (they stay pending; turn off again to retry)');
}

/** Renders the last toggle result into `box`. Returns false when there is nothing to show. */
function renderReport(box, rep) {
  box.replaceChildren();
  if (!rep || (typeof rep === 'object' && !rep.action && !rep.error)) return false;
  if (typeof rep === 'string') { box.appendChild(el('p', '', friendlySummary(rep))); return true; }
  if (rep.error) { box.appendChild(el('p', 'bad-text', 'The last attempt was refused: ' + rep.error)); return true; }
  const a = rep.acted || [], s = rep.skipped || [], f = rep.failed || [];
  const head = (rep.action === 'revert' ? 'Restored: ' : 'Turned on: ') +
    plural(a.length, 'item') + ' done, ' + s.length + ' left alone' + (f.length ? ', ' + f.length + ' failed' : '') + '.';
  box.appendChild(el('p', f.length ? 'bad-text' : '', head));
  const ul = el('ul', 'plain-list');
  const add = (label, it, cls) => {
    const what = it.kind === 'service' ? 'service ' : it.kind === 'task' ? 'task ' : '';
    const why = it.reason ? ' because ' + reasonText(it.reason) : '';
    ul.appendChild(el('li', cls, label + ' ' + what + (it.display || it.name) + why + (!it.reason && it.detail ? ': ' + it.detail : '')));
  };
  a.forEach(i => add('Done:', i));
  s.forEach(i => add('Left alone:', i));
  f.forEach(i => add('Failed:', i, 'fail'));
  if (ul.childNodes.length) box.appendChild(ul);
  return true;
}
