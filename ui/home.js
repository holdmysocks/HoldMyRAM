'use strict';
// Home: status card, profile picker, live preview of what turning on will do, add-ons,
// admin helper card, last result and the quiet update banner.

const KIND_GROUP = { process: 'Apps and background programs', service: 'Services', task: 'Scheduled tasks', startup: 'Startup items' };
const KIND_ORDER = ['process', 'service', 'task', 'startup'];
const SKIP_TEXT = {
  not_present: 'not on this PC', not_running: 'not running', disabled: 'already disabled',
  foreground: 'in use right now', self_process: 'HoldMyRAM itself', protected: 'protected system items',
  never_touch: 'on the never-touch list', dangerous: 'rated Dangerous', invalid: 'not valid',
  not_scanned: 'not scanned yet', condition_not_met: 'rule does not apply to this PC',
};
const ADDON_GROUPS = [
  ['optional', 'Optional apps', 'Apps you may not mind closing. Review them before ticking.'],
  ['optin', 'Other opt-in items', 'Services, tasks and other items that are off unless you ask for them.'],
];

/* ---------- preview ---------- */

function schedulePreview(delay) {
  clearTimeout(S.pv.timer);
  S.pv.timer = setTimeout(loadPreview, delay || 0); // one-shot debounce, not a loop
}

async function loadPreview() {
  if (!S.cfg) return;
  const seq = ++S.pv.seq;
  S.pv.loading = true;
  S.pv.error = '';
  renderPreview();
  try {
    const args = { name: activeName() };
    // Unsaved edits are only previewed because the draft is passed along explicitly.
    if (isDirty()) args.config = S.cfg;
    const d = await invoke('preview_profile', args);
    if (seq !== S.pv.seq) return;
    S.pv.data = d;
  } catch (e) {
    if (seq !== S.pv.seq) return;
    S.pv.error = String(e);
  }
  S.pv.loading = false;
  renderHome();
  if (S.tab === 'advanced') renderAdvanced();
}

function skippedSummary(d) {
  const c = d.skipped_counts || {};
  const parts = [];
  for (const k of Object.keys(c)) if (c[k] > 0) parts.push(c[k] + ' ' + (SKIP_TEXT[k] || k.replace(/_/g, ' ')));
  return parts.length ? 'Left alone: ' + parts.join(', ') + '.' : '';
}

function confBadge(it) {
  if (!(it.candidate || it.confidence === 'low')) return null;
  const b = el('span', 'badge candidate', 'Candidate');
  b.title = 'Lower confidence: suggested by a rule, worth a look before you rely on it.';
  return b;
}

function itemLi(it, showRam) {
  const li = el('li', 'pv-item');
  const top = el('div', 'pv-top');
  top.appendChild(el('strong', '', it.display || it.name));
  const cb = confBadge(it);
  if (cb) top.appendChild(cb);
  if (it.method && it.kind === 'process') top.appendChild(el('span', 'tag', it.method));
  top.appendChild(el('span', 'spacer'));
  if (showRam && it.ram_bytes) top.appendChild(el('span', 'ram', fmtRam(it.ram_bytes)));
  li.appendChild(top);
  if (it.why) li.appendChild(el('div', 'small muted', it.why));
  if (it.side_effect) {
    const p = el('div', 'small');
    p.appendChild(el('span', 'kv', 'Side effect: '));
    p.appendChild(document.createTextNode(it.side_effect));
    li.appendChild(p);
  }
  return li;
}

function renderPreview() {
  const body = $('preview-body');
  const st = S.state;
  body.replaceChildren();
  body.removeAttribute('aria-busy');
  if (st && st.active) {
    body.appendChild(el('p', '', 'HoldMyRAM is on. Turn off to restore what it closed.'));
    return;
  }
  const d = S.pv.data;
  if (S.pv.error && !d) {
    body.appendChild(el('p', 'bad-text', 'Could not work out the preview: ' + S.pv.error));
    const b = el('button', '', 'Try again');
    b.type = 'button';
    b.onclick = () => schedulePreview(0);
    body.appendChild(b);
    return;
  }
  if (!d) {
    body.setAttribute('aria-busy', 'true');
    for (let i = 0; i < 3; i++) body.appendChild(el('div', 'skel-line'));
    return;
  }
  body.classList.toggle('stale', S.pv.loading);
  if (S.pv.error) body.appendChild(el('p', 'bad-text small', 'Could not refresh the preview: ' + S.pv.error));
  const items = d.items || [];
  const sum = el('div', 'pv-sum');
  if (!items.length) {
    sum.appendChild(el('div', 'pv-big', 'Nothing to close'));
    sum.appendChild(el('p', 'muted', 'Nothing in this profile is running right now, so turning on would not change anything.'));
  } else {
    sum.appendChild(el('div', 'pv-big', d.total_ram_bytes ? 'About ' + fmtRam(d.total_ram_bytes) + ' freed' : plural(items.length, 'item') + ' to close'));
    sum.appendChild(el('p', 'muted', plural(items.length, 'item') + ' would be closed or stopped. The app you are using is left running.'));
  }
  body.appendChild(sum);
  if (isDirty()) body.appendChild(el('p', 'small note-inline', 'This preview includes your unsaved changes.'));
  if (items.length) {
    const det = el('details', 'pv-details');
    det.open = S.pv.open;
    det.addEventListener('toggle', () => { S.pv.open = det.open; });
    det.appendChild(el('summary', '', (det.open ? 'Hide' : 'Show') + ' the ' + plural(items.length, 'item')));
    det.addEventListener('toggle', () => { det.firstChild.textContent = (det.open ? 'Hide' : 'Show') + ' the ' + plural(items.length, 'item'); });
    for (const kind of KIND_ORDER) {
      const g = items.filter(i => i.kind === kind);
      if (!g.length) continue;
      det.appendChild(el('h3', 'pv-group', KIND_GROUP[kind] + ' (' + g.length + ')'));
      const ul = el('ul', 'plain-list');
      g.slice().sort((a, b) => (b.ram_bytes || 0) - (a.ram_bytes || 0)).forEach(i => ul.appendChild(itemLi(i, true)));
      det.appendChild(ul);
    }
    body.appendChild(det);
  }
  const sk = skippedSummary(d);
  if (sk) body.appendChild(el('p', 'small muted', sk));
}

/* ---------- add-ons ---------- */

const addonRefOn = a => S.cfg.recommended_addons.some(x => x.kind === a.kind && norm(x.name) === norm(a.name));
const groupOn = g => S.cfg.recommended_addon_groups.includes(g);

function toggleAddon(a, on) {
  const list = S.cfg.recommended_addons;
  const i = list.findIndex(x => x.kind === a.kind && norm(x.name) === norm(a.name));
  if (on && i < 0) list.push({ kind: a.kind, name: a.name });
  if (!on && i >= 0) list.splice(i, 1);
  onCfgChange();
}
function toggleGroup(g, on) {
  const list = S.cfg.recommended_addon_groups;
  const i = list.indexOf(g);
  if (on && i < 0) list.push(g);
  if (!on && i >= 0) list.splice(i, 1);
  onCfgChange();
}

function addonRow(a) {
  const viaGroup = groupOn(a.group);
  const row = el('label', 'addon');
  const cb = el('input');
  cb.type = 'checkbox';
  cb.checked = viaGroup || addonRefOn(a);
  cb.disabled = viaGroup;
  cb.dataset.fid = 'addon:' + a.kind + ':' + a.name;
  cb.onchange = () => toggleAddon(a, cb.checked);
  const txt = el('span', 'addon-txt');
  const top = el('span', 'pv-top');
  top.appendChild(el('strong', '', a.display || a.name));
  const cbd = confBadge(a);
  if (cbd) top.appendChild(cbd);
  if (a.kind !== 'process') top.appendChild(el('span', 'tag', a.kind === 'service' ? 'service' : 'task'));
  top.appendChild(el('span', 'spacer'));
  if (a.present && a.ram_bytes) top.appendChild(el('span', 'ram', fmtRam(a.ram_bytes)));
  txt.appendChild(top);
  if (a.why) txt.appendChild(el('span', 'small muted block', a.why));
  if (a.side_effect) {
    const p = el('span', 'small block');
    p.appendChild(el('span', 'kv', 'Side effect: '));
    p.appendChild(document.createTextNode(a.side_effect));
    txt.appendChild(p);
  }
  row.append(cb, txt);
  return row;
}

function renderAddons() {
  const card = $('addons-card');
  const d = S.pv.data;
  const show = !!(d && d.builtin && (d.addons || []).length) && !(S.state && S.state.active);
  card.hidden = !show;
  if (!show) return;
  const body = $('addons-body');
  body.replaceChildren();
  for (const [g, label, hint] of ADDON_GROUPS) {
    const all = d.addons.filter(a => a.group === g);
    if (!all.length) continue;
    const sec = el('div', 'addon-group');
    const head = el('div', 'addon-head');
    head.appendChild(el('h3', '', label));
    const gl = el('label', 'inline small');
    const gc = el('input');
    gc.type = 'checkbox';
    gc.checked = groupOn(g);
    gc.dataset.fid = 'group:' + g;
    gc.onchange = () => toggleGroup(g, gc.checked);
    gl.append(gc, document.createTextNode(' Include all'));
    head.appendChild(gl);
    sec.appendChild(head);
    sec.appendChild(el('p', 'small muted', hint));
    const here = all.filter(a => a.present), gone = all.filter(a => !a.present);
    if (here.length) here.forEach(a => sec.appendChild(addonRow(a)));
    else sec.appendChild(el('p', 'small muted', 'None of these are open on this PC right now.'));
    if (gone.length) {
      // This panel is rebuilt on every change and every focus refresh, so remember which
      // groups the user opened, or they would snap shut after each tick or alt-tab.
      S.addonOpen = S.addonOpen || {};
      const det = el('details', 'pv-details');
      det.open = !!S.addonOpen[g];
      det.addEventListener('toggle', () => { S.addonOpen[g] = det.open; });
      // `present` is false for an app that is installed but not running, so say both.
      det.appendChild(el('summary', '', 'Not open or not on this PC (' + gone.length + ')'));
      gone.forEach(a => det.appendChild(addonRow(a)));
      sec.appendChild(det);
    }
    body.appendChild(sec);
  }
}

/* ---------- picker ---------- */

function pickerEntries() {
  const out = [];
  const bi = infos().filter(p => p.builtin);
  const builtins = bi.length ? bi : BUILTIN_FALLBACK.map(n => ({ name: n, description: '', needs_admin: n.endsWith('+') }));
  for (const p of builtins) out.push({ name: p.name, desc: p.description, builtin: true, needs_admin: !!p.needs_admin });
  for (const p of S.cfg.profiles) {
    out.push({ name: p.name, desc: p.description || '', builtin: false, count: p.selections.length,
      needs_admin: p.selections.some(s => selKindOf(s) !== 'process') });
  }
  if (S.cfg.selections.length || !activeName()) {
    out.push({ name: '', label: 'My own selection', desc: 'The list you tick in Advanced when no profile is chosen.', builtin: false,
      count: S.cfg.selections.length, needs_admin: S.cfg.selections.some(s => selKindOf(s) !== 'process') });
  }
  return out;
}

function renderPicker() {
  const box = $('picker');
  box.replaceChildren();
  const cur = norm(activeName());
  const entries = pickerEntries();
  for (const p of entries) {
    const row = el('label', 'opt' + (norm(p.name) === cur ? ' selected' : ''));
    const rb = el('input');
    rb.type = 'radio';
    rb.name = 'profile';
    rb.value = p.name;
    rb.checked = norm(p.name) === cur;
    rb.dataset.fid = 'prof:' + p.name;
    rb.onchange = () => { S.cfg.active_profile = p.name || null; onCfgChange({ delay: 0 }); };
    const txt = el('span', 'opt-txt');
    const top = el('span', 'opt-top');
    top.appendChild(el('strong', '', p.label || p.name));
    if (p.builtin) top.appendChild(el('span', 'tag', 'Built in'));
    else if (p.count !== undefined) top.appendChild(el('span', 'tag', plural(p.count, 'item')));
    if (p.needs_admin) top.appendChild(el('span', 'tag admin', 'Needs admin approval'));
    txt.appendChild(top);
    if (p.desc) txt.appendChild(el('span', 'small muted block', p.desc));
    row.append(rb, txt);
    box.appendChild(row);
  }
}

/* ---------- status card ---------- */

function hotkeyParts() {
  const h = S.state && S.state.hotkey;
  return String((h && h.active) || S.cfg.hotkey || '').split('+').filter(Boolean);
}

function needsAdminNow() {
  const d = S.pv.data;
  if (d && (d.needs_admin || (d.items || []).some(i => i.kind === 'service' || i.kind === 'task'))) return true;
  return editable() && sels().some(s => selKindOf(s) !== 'process');
}

function renderStatus() {
  const st = S.state || {};
  const busy = !!st.busy, active = !!st.active;
  const pill = $('hs-pill');
  pill.className = 'bigpill ' + (busy ? 'busy' : active ? 'on' : 'off');
  pill.textContent = busy ? 'WORKING' : active ? 'ON' : 'OFF';
  $('hs-sub').textContent = busy ? 'Working. This can take a few seconds.'
    : active ? 'Apps are closed to free RAM. Turn off to bring them back.'
    : 'Ready. Nothing has been closed.';
  $('status-card').classList.toggle('is-on', active && !busy);
  const btn = $('toggle-btn');
  btn.textContent = busy ? 'Working...' : active ? 'Turn off and restore' : 'Turn on and free RAM';
  btn.classList.toggle('green', active);

  let blocked = [];
  if (!active && editable()) {
    for (const s of sels()) {
      const r = typeof selRow === 'function' ? selRow(s) : null;
      if (r && selKindOf(s) !== 'process' && r.admin && r.admin.blocked) blocked.push(r.display || s.name);
    }
  }
  const empty = !active && editable() && sels().length === 0;
  btn.disabled = busy || empty || blocked.length > 0;
  btn.title = blocked.length ? 'The selection contains blocked items (' + blocked.slice(0, 3).join(', ') + '). Untick them in Advanced.'
    : empty ? 'This profile is empty. Choose a profile, or tick items in Advanced.' : '';

  const hk = $('hs-hotkey');
  hk.replaceChildren(document.createTextNode('Hotkey: '));
  const parts = hotkeyParts();
  parts.forEach((p, i) => {
    if (i) hk.appendChild(document.createTextNode(' + '));
    hk.appendChild(el('kbd', '', p));
  });
  if (!parts.length) hk.appendChild(document.createTextNode('not set'));
  if (st.hotkey && st.hotkey.error) hk.appendChild(el('span', 'bad-text small', '  (' + st.hotkey.error + ')'));
  if (empty) hk.appendChild(el('span', 'small muted', '  Choose a profile or tick items first.'));

  const us = $('hs-unsaved');
  us.hidden = !isDirty();
  us.textContent = isDirty() ? 'You have unsaved changes. Turning on will ask to save them first.' : '';
}

/* ---------- update banner ---------- */

function renderUpdBanner() {
  const b = $('upd-banner');
  const u = S.upd;
  const show = !!(u && u.supported !== false && u.available && !S.updDismissed && ['idle', 'ready', 'downloading', 'installing'].includes(u.phase));
  b.hidden = !show;
  if (!show) return;
  b.replaceChildren();
  const v = u.latest ? 'HoldMyRAM ' + u.latest : 'A new version';
  let text = v + ' is available.';
  if (u.phase === 'downloading') text = 'Downloading ' + v + (u.progress != null ? ' (' + Math.round(u.progress <= 1 ? u.progress * 100 : u.progress) + '%)' : '') + '...';
  if (u.phase === 'ready') text = v + ' is ready to install.';
  if (u.phase === 'installing') text = 'Installing ' + v + '. HoldMyRAM will restart.';
  b.appendChild(el('span', '', text));
  b.appendChild(el('span', 'spacer'));
  if (u.phase === 'ready') {
    const r = el('button', 'primary', 'Restart to update');
    r.type = 'button';
    r.onclick = installUpdate;
    b.appendChild(r);
  } else if (u.phase === 'idle') {
    const d = el('button', 'primary', 'Download');
    d.type = 'button';
    d.onclick = downloadUpdate;
    b.appendChild(d);
  }
  if (u.phase !== 'installing') {
    const x = el('button', 'linkbtn', 'Later');
    x.type = 'button';
    x.onclick = () => { S.updDismissed = true; renderUpdBanner(); };
    b.appendChild(x);
  }
}

/* ---------- render ---------- */

function renderHome() {
  if (!S.cfg) return;
  const a = document.activeElement;
  const fid = a && a.dataset ? a.dataset.fid : null;
  renderStatus();
  renderPicker();
  renderPreview();
  renderAddons();
  const showHelper = needsAdminNow();
  $('helper-card').hidden = !showHelper && !S.helper.toggle_declined;
  renderHelper();
  const card = $('report-card');
  const has = renderReport($('last-report'), S.state && S.state.last_report);
  card.hidden = !has;
  renderUpdBanner();
  if (fid) {
    for (const e of document.querySelectorAll('#screen-home [data-fid]')) if (e.dataset.fid === fid) { e.focus(); break; }
  }
}

$('toggle-btn').onclick = async () => {
  if (isDirty()) {
    const ok = await ask({
      title: 'Unsaved changes',
      text: 'Turning on uses the saved settings. Save your changes and continue?',
      ok: 'Save and continue',
    });
    if (!ok || !(await save())) return;
  }
  $('toggle-btn').disabled = true;
  try {
    await invoke('toggle_now');
  } catch (e) { showErr('Toggle failed: ' + e); }
  await refreshState();
  if (S.tab === 'home') schedulePreview(0);
};
