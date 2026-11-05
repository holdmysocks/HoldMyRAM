'use strict';
// Settings tab: hotkey, theme, grace, start with Windows, updates, backup, tutorial, about, quit.
// Drafts until Save, except autostart and update actions, which apply immediately.

const UPD_ERR = {
  offline: 'Could not reach GitHub. Check your internet connection and try again.',
  not_found: 'No published releases found yet.',
  rate_limited: 'GitHub asked for a short break. Try again in a few minutes.',
  no_asset: 'The newest release has no Windows download yet.',
  bad_checksum: 'The download did not match its checksum, so it was thrown away. Try downloading again.',
  bad_archive: 'The downloaded file was damaged and was not used. Try downloading again.',
  not_writable: 'HoldMyRAM cannot write to the folder it is installed in. Move it to a folder you can write to, or update by hand from the release page.',
  restore_needed_admin: 'Something HoldMyRAM closed needs administrator approval to restore. Turn off first (approve the admin helper if asked), then update.',
  swap_failed: 'The old version could not be replaced. Nothing was changed. Try again, or update by hand from the release page.',
  network: 'Could not reach GitHub because of a connection problem. Try again in a moment.',
  forbidden: 'GitHub refused the request. Try again later.',
  http_error: 'GitHub returned an unexpected error. Try again later.',
  bad_release: 'The newest release is not in a format HoldMyRAM understands, so it was ignored.',
  bad_url: 'The update pointed somewhere other than GitHub, so HoldMyRAM refused it.',
  io: 'Could not save the update files on this PC (the disk may be full, or access was denied).',
  restore_failed: 'Some apps could not be restored, so the update was not installed. Turn off first, then try again.',
  launch_failed: 'The new version could not start, so the previous version was put back.',
  busy: 'HoldMyRAM is in the middle of turning on or off. Try again in a moment.',
  no_update: 'There is no newer version to install.',
  internal: 'Something unexpected went wrong in the updater. Try again, or update by hand from the release page.',
};
const neutralErr = c => c === 'not_found' || c === 'no_update';

/* ---------- render ---------- */

function renderSettings() {
  if (!S.cfg) return;
  const hk = $('set-hotkey');
  if (document.activeElement !== hk) hk.value = S.cfg.hotkey;
  $('set-theme').value = S.cfg.theme;
  $('set-grace').value = S.cfg.grace_seconds;
  $('set-info').textContent = S.state ? 'Settings file: ' + S.state.config_path : '';
  const h = S.state && S.state.hotkey;
  const hs = $('hotkey-status');
  if (h && h.active) {
    hs.className = 'status-line ' + (h.error ? 'bad-text' : 'ok-text');
    hs.textContent = 'Active hotkey: ' + h.active + (h.error ? ' (' + h.error + ')' : '');
  } else {
    hs.className = 'status-line bad-text';
    hs.textContent = h && h.error ? h.error : '';
  }
  renderHotkeyMode(h);
  renderNotify();
  const ac = S.cfg.auto_check_updates !== false, ad = S.cfg.auto_download_updates !== false;
  $('set-autocheck').checked = ac;
  $('set-autocheck-t').textContent = ac ? 'On' : 'Off';
  $('set-autodl').checked = ad;
  $('set-autodl-t').textContent = ad ? 'On' : 'Off';
  $('set-autodl').disabled = !ac;
  renderAutostart();
  renderUpdate();
  const u = S.upd;
  $('about-text').textContent = 'HoldMyRAM' + (u && u.current ? ' version ' + u.current : '') +
    '. A small tray app that frees RAM on demand.' + (u && u.repo ? ' Source: ' + u.repo + '.' : '');
}

/* ---------- hotkey mode and notifications (drafts; the test applies at once) ---------- */

function renderHotkeyMode(h) {
  const hold = S.cfg.hotkey_mode === 'hold';
  $('hk-mode-tap').checked = !hold;
  $('hk-mode-hold').checked = hold;
  const ms = clampHold(S.cfg.hold_ms);
  $('set-hold').value = ms;
  $('set-hold').disabled = !hold;
  $('hold-field').classList.toggle('disabled', !hold);
  $('hold-val').textContent = fmtSec(ms) + ' s';
  $('hold-inline').textContent = fmtSec(ms);
  const note = h && h.hold_note;
  const n = $('hold-note');
  n.hidden = !note;
  n.textContent = note ? String(note) : '';
}

function renderNotify() {
  const style = S.cfg.notify_style;
  $('nt-toast').checked = style === 'toast';
  $('nt-popup').checked = style === 'popup';
  $('nt-off').checked = style === 'off';
  $('set-sound').checked = !!S.cfg.notify_sound;
  $('set-sound').disabled = style === 'off';
  const noToast = S.state && S.state.toast_available === false;
  $('toast-na').hidden = !noToast;
  const off = style === 'off';
  $('nt-test-on').disabled = off || ntBusy;
  $('nt-test-off').disabled = off || ntBusy;
  $('set-sndset').value = S.cfg.notify_sound_set;
  $('snd-prev-on').disabled = sndBusy;
  $('snd-prev-off').disabled = sndBusy;
  const hint = $('snd-hint');
  hint.hidden = !(off || !S.cfg.notify_sound);
  hint.textContent = off
    ? 'Off means no message and no sound when HoldMyRAM turns on or off. You can still hear the sounds here.'
    : 'Sounds are off. Turn on "Play a short sound" to hear this when HoldMyRAM turns on or off.';
}

let sndBusy = false;
function sndMsg(text, kind) {
  const m = $('snd-msg');
  m.className = 'status-line ' + (kind === 'ok' ? 'ok-text' : kind === 'bad' ? 'bad-text' : 'muted');
  m.textContent = text;
}
async function runSoundPreview(kind) {
  if (sndBusy) return;
  sndBusy = true; renderNotify();
  sndMsg('Playing...', 'muted');
  const started = Date.now();
  let err = null;
  try { await invoke('preview_sound', { set: S.cfg.notify_sound_set, kind }); }
  catch (e) { err = e; }
  // Keep the buttons off for about a second so they cannot be spammed.
  const wait = Math.max(0, 1000 - (Date.now() - started));
  if (wait) await new Promise(r => setTimeout(r, wait));
  sndBusy = false; renderNotify();
  if (err) sndMsg('The sound could not be played: ' + err, 'bad');
  else sndMsg('Played the "turn ' + kind + '" sound.', 'ok');
}
$('snd-prev-on').onclick = () => runSoundPreview('on');
$('snd-prev-off').onclick = () => runSoundPreview('off');
$('set-sndset').onchange = e => { S.cfg.notify_sound_set = e.target.value; $('snd-msg').textContent = ''; renderHeader(); renderNotify(); };

let ntBusy = false;
function savedNotify() {
  try {
    const c = JSON.parse(S.savedJson);
    return { s: c.notify_style, snd: c.notify_sound, set: c.notify_sound_set };
  } catch (_) { return null; }
}
function ntMsg(text, kind, withSave) {
  const m = $('nt-msg');
  m.className = 'status-line ' + (kind === 'ok' ? 'ok-text' : kind === 'bad' ? 'bad-text' : 'muted');
  m.textContent = text;
  if (withSave) {
    const b = el('button', '', 'Save now');
    b.type = 'button';
    b.onclick = async () => { await save(); runNotifyTest(withSave); };
    m.append(' ', b);
  }
}
async function runNotifyTest(kind) {
  const sv = savedNotify();
  const changed = sv && (sv.s !== S.cfg.notify_style || sv.snd !== S.cfg.notify_sound || sv.set !== S.cfg.notify_sound_set);
  if (changed) {
    ntMsg('The test uses your saved settings, not the unsaved changes above. Save first to try the new ones. ', 'muted', kind);
    return;
  }
  ntBusy = true; renderNotify();
  ntMsg('Showing a test...', 'muted');
  try {
    await invoke('test_notification', { kind });
    ntMsg('Sent a "turned ' + kind + '" test with your saved settings.', 'ok');
  } catch (e) {
    ntMsg('The test could not be shown: ' + e, 'bad');
  }
  ntBusy = false; renderNotify();
}
$('nt-test-on').onclick = () => runNotifyTest('on');
$('nt-test-off').onclick = () => runNotifyTest('off');

function onNotifyDraft() {
  $('nt-msg').textContent = '';
  renderHeader();
  renderNotify();
}
for (const id of ['nt-toast', 'nt-popup', 'nt-off']) {
  $(id).onchange = e => { if (e.target.checked) { S.cfg.notify_style = e.target.value; onNotifyDraft(); } };
}
$('set-sound').onchange = e => { S.cfg.notify_sound = e.target.checked; onNotifyDraft(); };
for (const id of ['hk-mode-tap', 'hk-mode-hold']) {
  $(id).onchange = e => { if (e.target.checked) { S.cfg.hotkey_mode = e.target.value; renderHeader(); renderSettings(); } };
}
$('set-hold').oninput = e => {
  S.cfg.hold_ms = clampHold(e.target.value);
  renderHeader();
  $('hold-val').textContent = fmtSec(S.cfg.hold_ms) + ' s';
  $('hold-inline').textContent = fmtSec(S.cfg.hold_ms);
};

/* ---------- draft fields ---------- */

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
$('set-autocheck').onchange = e => { S.cfg.auto_check_updates = e.target.checked; renderHeader(); renderSettings(); };
$('set-autodl').onchange = e => { S.cfg.auto_download_updates = e.target.checked; renderHeader(); renderSettings(); };

/* ---------- hotkey recorder ---------- */

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
    setMsg('Use at least one modifier (Ctrl, Alt, Shift) plus a letter, digit or F-key.');
    return;
  }
  $('set-hotkey').value = mods.concat(k).join('+');
  S.cfg.hotkey = $('set-hotkey').value;
  setMsg('Recorded. Press Save to apply.', 'ok');
  stopRecording();
  renderHeader();
}
$('set-record').onclick = () => {
  if (recording) { stopRecording(); return; }
  recording = true;
  $('set-record').textContent = 'Press keys... (Esc cancels)';
  setMsg('');
  document.addEventListener('keydown', onRecordKey, true);
};

function setMsg(text, kind) {
  const m = $('set-msg');
  m.className = 'status-line ' + (kind === 'ok' ? 'ok-text' : kind === 'bad' ? 'bad-text' : 'muted');
  m.textContent = text;
}

/* ---------- start with Windows (applies immediately) ---------- */

async function loadAutostart() {
  try { S.auto = await invoke('get_autostart'); S.autoErr = ''; }
  catch (e) { S.autoErr = String(e); }
  renderAutostart();
}

function renderAutostart() {
  const box = $('auto-body');
  box.replaceChildren();
  const a = S.auto;
  if (!a) {
    box.appendChild(el('p', S.autoErr ? 'bad-text small' : 'muted small', S.autoErr ? 'Could not read the setting: ' + S.autoErr : 'Checking...'));
    return;
  }
  const row = el('div', 'auto-row');
  const sw = el('label', 'switch');
  const cb = el('input');
  cb.type = 'checkbox';
  cb.setAttribute('role', 'switch');
  cb.checked = !!a.enabled;
  cb.id = 'auto-sw';
  cb.onchange = () => applyAutostart(cb.checked);
  sw.append(cb, el('span', 'track'), el('span', 'sw-text', 'Start HoldMyRAM when I sign in'));
  row.appendChild(sw);
  box.appendChild(row);
  if (a.disabled_in_task_manager) {
    box.appendChild(el('p', 'warn-text small', 'Windows has this turned off in Task Manager (Startup apps). Turn it back on there; HoldMyRAM cannot override that choice.'));
  } else if (a.enabled && a.path_matches === false) {
    const p = el('div', 'warn-text small');
    p.appendChild(document.createTextNode('It points to a different folder (' + (a.path || 'unknown') + '), not this copy. '));
    const fix = el('button', '', 'Fix');
    fix.type = 'button';
    fix.onclick = () => applyAutostart(true);
    p.appendChild(fix);
    box.appendChild(p);
  } else if (a.enabled) {
    box.appendChild(el('p', 'muted small', 'HoldMyRAM starts quietly in the tray when you sign in. The window stays closed.'));
  } else {
    box.appendChild(el('p', 'muted small', 'Off. HoldMyRAM only runs when you start it yourself.'));
  }
  if (S.autoErr) box.appendChild(el('p', 'bad-text small', S.autoErr));
}

async function applyAutostart(enabled) {
  try {
    S.auto = await invoke('set_autostart', { enabled });
    S.autoErr = '';
  } catch (e) {
    S.autoErr = 'Could not change it: ' + e;
    await loadAutostart();
  }
  renderAutostart();
  const sw = $('auto-sw');
  if (sw) sw.focus();
}

/* ---------- updates (apply immediately) ---------- */

async function loadUpdate() {
  try { S.upd = await invoke('update_status'); S.updErr = ''; }
  catch (e) { S.updErr = String(e); }
  renderUpdate();
  renderUpdBanner();
  if (S.tab === 'settings') renderSettings();
}

function updateAction(fn) {
  return async () => {
    try { await fn(); S.updErr = ''; } catch (e) { S.updErr = String(e); }
    await loadUpdate();
  };
}
const checkUpdate = updateAction(() => invoke('check_for_update', { force: true }));
const downloadUpdate = updateAction(() => invoke('download_update'));
async function installUpdate() {
  const st = S.state;
  const ok = await ask({
    title: 'Restart to update',
    text: (st && st.active ? 'HoldMyRAM is on. It will restore what it closed first. ' : '') + 'HoldMyRAM will close and start again as the new version. Continue?',
    ok: 'Restart now',
  });
  if (!ok) return;
  try { await invoke('install_update'); } catch (e) { S.updErr = String(e); }
  await loadUpdate();
}
const openRelease = updateAction(() => invoke('open_release_page'));

function fmtWhen(iso) {
  if (!iso) return '';
  const d = new Date(iso);
  return isNaN(d) ? String(iso) : d.toLocaleString();
}

function renderUpdate() {
  const box = $('upd-body');
  box.replaceChildren();
  const u = S.upd;
  if (!u) {
    box.appendChild(el('p', S.updErr ? 'bad-text small' : 'muted small', S.updErr ? 'Could not read update status: ' + S.updErr : 'Checking...'));
    return;
  }
  const ver = el('p', '');
  ver.appendChild(el('strong', '', 'Version ' + (u.current || '?')));
  if (u.updated_from) ver.appendChild(el('span', 'muted small', '  (updated from ' + u.updated_from + ')'));
  box.appendChild(ver);
  if (u.supported === false) {
    box.appendChild(el('p', 'muted small', 'Updates are not available for this copy. Check the release page by hand.'));
  }
  // Status line per phase.
  let line = '', cls = 'muted small';
  switch (u.phase) {
    case 'checking': line = 'Checking for updates...'; break;
    case 'downloading': line = 'Downloading ' + (u.latest || 'the update') + '...'; break;
    case 'ready': line = (u.latest || 'The update') + ' is downloaded and ready. Restart to finish.'; cls = 'ok-text small'; break;
    case 'installing': line = 'Installing. HoldMyRAM will restart in a moment.'; break;
    case 'error': {
      const c = u.error_code;
      line = UPD_ERR[c] || (u.error ? 'Update problem: ' + u.error : 'Something went wrong while updating.');
      cls = neutralErr(c) ? 'muted small' : 'bad-text small';
      break;
    }
    default:
      line = u.available ? 'Version ' + u.latest + ' is available.'
        : u.error_code && !neutralErr(u.error_code) ? 'Could not check for updates.'
        : u.latest ? 'You are up to date.' : u.checked_at ? 'No newer version found.' : 'Not checked yet.';
      if (u.available) cls = 'small';
  }
  const status = el('p', cls, line);
  status.setAttribute('role', 'status');
  box.appendChild(status);
  // A failed check leaves the phase at 'idle', and a blocked install leaves it at 'ready',
  // with only error_code set. Show that too, or those failures would be invisible.
  if (u.phase !== 'error' && (u.error_code || u.error)) {
    const c = u.error_code;
    const why = UPD_ERR[c] || (u.error ? 'Update problem: ' + u.error : '');
    if (why) {
      const note = el('p', neutralErr(c) ? 'muted small' : 'bad-text small', why);
      note.setAttribute('role', 'status');
      box.appendChild(note);
    }
  }
  if (u.phase === 'downloading') {
    const pr = el('progress', 'upd-progress');
    pr.max = 100;
    if (u.progress != null) pr.value = u.progress <= 1 ? u.progress * 100 : u.progress;
    pr.setAttribute('aria-label', 'Download progress');
    box.appendChild(pr);
  }
  if (u.checked_at) box.appendChild(el('p', 'muted small', 'Last checked ' + fmtWhen(u.checked_at)));
  if (u.available && u.notes) {
    const det = el('details', 'pv-details');
    det.appendChild(el('summary', '', 'What is new'));
    det.appendChild(el('pre', 'notes', String(u.notes)));
    box.appendChild(det);
  }
  const row = el('div', 'btn-row');
  const mk = (label, fn, cls2, dis) => {
    const b = el('button', cls2 || '', label);
    b.type = 'button';
    b.disabled = !!dis;
    b.onclick = fn;
    row.appendChild(b);
  };
  const busy = ['checking', 'downloading', 'installing'].includes(u.phase);
  if (u.phase === 'ready') mk('Restart to update', installUpdate, 'primary');
  else if (u.available && u.phase === 'idle') mk('Download', downloadUpdate, 'primary');
  mk(u.phase === 'checking' ? 'Checking...' : 'Check now', checkUpdate, '', busy || u.supported === false);
  mk('Open release page', openRelease, '', false);
  box.appendChild(row);
  if (S.updErr) box.appendChild(el('p', 'bad-text small', S.updErr));
}

/* ---------- backup ---------- */

$('set-export').onclick = async () => {
  try {
    const path = await invoke('pick_save_path', { default_name: 'holdmyram-export.toml' });
    if (!path) return;
    const p = await invoke('export_config', { path, config: S.cfg });
    setMsg('Exported to ' + p, 'ok');
  } catch (e) { setMsg('Export failed: ' + e, 'bad'); }
};
$('set-import').onclick = async () => {
  try {
    const path = await invoke('pick_open_path');
    if (!path) return;
    const cfg = await invoke('import_config', { path });
    setConfig(cfg, false);
    onCfgChange({ preview: false });
    setMsg('Imported as unsaved changes. Press Save to keep them.', 'ok');
  } catch (e) { setMsg('Import failed: ' + e, 'bad'); }
};

$('tour-replay').onclick = () => openTour(false);

$('quit-btn').onclick = async () => {
  const st = S.state;
  const ok = await ask({
    title: 'Quit HoldMyRAM',
    text: st && st.active
      ? 'HoldMyRAM is on. It will restore the apps it closed, then quit. This can take a few seconds.'
      : 'Quit HoldMyRAM and remove it from the tray?',
    ok: 'Quit', danger: !(st && st.active),
  });
  if (ok) invoke('quit_app');
};
