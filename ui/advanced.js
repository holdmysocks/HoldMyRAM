'use strict';
// Advanced tab: the full checklist (processes, services, startup items, tasks, apps),
// per-row method, risk badges, typed confirmation, lasting changes and the Changes panel.

const PAGE = 200;
const KINDS = [
  ['all', 'All'], ['process', 'Processes'], ['service', 'Services'],
  ['startup', 'Startup'], ['task', 'Tasks'], ['app', 'Apps'],
];
const KIND_NOTE = {
  service: 'Tick a service to have it stopped when you turn on. The ... button on a row changes its start type for good (until undone). Core Windows services are blocked and cannot be selected.',
  startup: 'Startup items run when you sign in to Windows. Use the switch to stop one starting at sign-in; every change is logged and can be undone. This is separate from the RAM toggle and hotkey. Items that need administrator rights are shown read-only.',
  task: 'Tick a task to have it disabled when you turn on. The ... button on a row turns it on or off for good (until undone). Blocked tasks cannot be selected.',
  app: 'Installed apps are shown for information. Only running processes can be closed for now.',
  all: 'Turning on closes selected processes, stops selected services and disables selected tasks. Startup items have their own switch (it affects sign-in only); apps are information only.',
};
const ADMIN_TAB_NOTE = 'Needs administrator approval. Selected services are stopped and selected tasks are disabled when you turn on, and restored when you turn off. Turning on raises one Windows prompt. The prompt can pull a full-screen game out of full-screen, so approve ahead of time from Home.';
const START_LABEL = { Auto: 'Automatic', AutoDelayed: 'Automatic (delayed)', Manual: 'Manual', Disabled: 'Disabled', enabled: 'enabled', disabled: 'disabled' };
const METHOD_LABEL = { service: 'Stop', task: 'Disable' };
const METHOD_WIRE = { process: 'close', service: 'stop', task: 'disable' };
const ADMIN_NOTE = 'Needs administrator rights, not changeable here yet';
const RISK_ORDER = { suggested: 0, caution: 1, unrated: 2, dangerous: 3 };
const RISK_HINT = {
  suggested: 'Suggested: normally safe to close',
  caution: 'Caution: closing it may lose unsaved work or a feature',
  dangerous: 'Dangerous: can break Windows or hardware features',
  unrated: 'Unrated: not in the risk list, treat with care',
};

const A = {
  rows: [], byKey: new Map(), view: [], shown: 0, kind: 'all',
  scanning: false, scanned: false, lastScanAt: 0,
  expanded: new Set(),
  pending: new Set(),   // startup rows with a change in flight
  selMap: null,         // selection key -> scan row, rebuilt lazily
  adminBusy: false,
};

const key = r => r.kind + '|' + r.name.toLowerCase();
/** Admin-capable (service/task) row that can be selected: not blocked, has a target. */
const isAdminSel = r => !!(r.admin && r.admin.actionable && r.admin.target && (r.kind === 'service' || r.kind === 'task'));
const selectable = r => r.actionable || isAdminSel(r);
const selName = r => (isAdminSel(r) ? r.admin.target : r.name);
const rowSelKey = r => selKey(r.kind, selName(r));
function selIndex(kind, name) {
  const k = selKey(kind, name);
  return sels().findIndex(s => selKey(selKindOf(s), s.name) === k);
}

/** Keys of what the built-in profile would act on (from the preview), for the read-only view. */
function previewKeys() {
  const d = S.pv.data;
  const set = new Set();
  if (d) for (const it of d.items || []) set.add(selKey(it.kind, it.name));
  return set;
}

/* ---------- screen ---------- */

function renderAdvanced() {
  const ro = !editable();
  const b = $('ro-banner');
  b.hidden = !ro;
  if (ro) $('ro-text').textContent = activeName() + ' is built in and read-only. Duplicate it to customise.';
  renderTabs();
  renderList(false);
}

function ensureScan() {
  if (!A.scanned && !A.scanning) rescan();
}

$('ro-dup').onclick = () => duplicateProfile(activeName(), { activate: true });

function isMsTask(r) {
  if (r.kind !== 'task') return false;
  return /^\\?Microsoft/i.test(r.name) || /\\Microsoft\\/i.test(r.name) || /\\Microsoft\\/i.test(r.detail);
}

function buildView() {
  const q = norm($('search').value);
  const hideMs = $('hide-ms').checked;
  const onlySel = $('only-selected').checked;
  const sort = $('sort').value;
  const ro = !editable();
  const pk = ro ? previewKeys() : null;
  const selSet = ro ? pk : new Set(sels().map(s => selKey(selKindOf(s), s.name)));
  const out = [];
  for (const r of A.rows) {
    if (A.kind !== 'all' && r.kind !== A.kind) continue;
    if (hideMs && isMsTask(r)) continue;
    if (onlySel && !(selectable(r) && selSet.has(rowSelKey(r)))) continue;
    if (q && !(r._hay || (r._hay = (r.name + ' ' + r.display + ' ' + r.detail).toLowerCase())).includes(q)) continue;
    out.push(r);
  }
  if (sort === 'risk') out.sort((a, b) => RISK_ORDER[a.risk] - RISK_ORDER[b.risk] || b.ram_bytes - a.ram_bytes);
  else if (sort === 'name') out.sort((a, b) => a.display.localeCompare(b.display, undefined, { sensitivity: 'base' }));
  else out.sort((a, b) => b.ram_bytes - a.ram_bytes || a.display.localeCompare(b.display));
  A.view = out;
}

function renderTabs() {
  const counts = { all: 0 };
  const hideMs = $('hide-ms').checked;
  for (const r of A.rows) {
    if (hideMs && isMsTask(r)) continue;
    counts.all++;
    counts[r.kind] = (counts[r.kind] || 0) + 1;
  }
  const box = $('tabs');
  box.replaceChildren();
  for (const [k, label] of KINDS) {
    const b = el('button', 'chip' + (k === A.kind ? ' active' : ''));
    b.type = 'button';
    b.setAttribute('aria-pressed', k === A.kind ? 'true' : 'false');
    b.append(el('span', '', label), el('span', 'count', String(counts[k] || 0)));
    b.onclick = () => { A.kind = k; renderTabs(); renderList(true); };
    box.appendChild(b);
  }
  const note = $('kind-note');
  note.hidden = false;
  note.textContent = KIND_NOTE[A.kind] || '';
  const an = $('admin-note');
  an.hidden = !(A.kind === 'service' || A.kind === 'task');
  an.textContent = ADMIN_TAB_NOTE;
}

function renderSummary() {
  const cur = S.cfg ? sels() : [];
  let ram = 0, nAdmin = 0;
  for (const s of cur) {
    const r = selRow(s);
    if (selKindOf(s) !== 'process') nAdmin++;
    if (r) ram += r.ram_bytes || 0;
  }
  const ro = !editable();
  $('sel-summary').textContent = ro ? '' : cur.length + ' selected' + (nAdmin ? ' (' + nAdmin + ' service/task)' : '') +
    (ram ? ', about ' + fmtRam(ram) + ' in use' : '');
}

function selRow(s) {
  if (!A.selMap) {
    A.selMap = new Map();
    for (const r of A.rows) if (selectable(r) || r.admin) A.selMap.set(rowSelKey(r), r);
  }
  return A.selMap.get(selKey(selKindOf(s), s.name)) || null;
}

function renderList(reset) {
  buildView();
  if (reset) { A.shown = Math.min(PAGE, A.view.length); $('list').scrollTop = 0; }
  else A.shown = Math.min(Math.max(A.shown, PAGE), A.view.length);
  const list = $('list');
  list.replaceChildren();
  renderSummary();
  if (!A.view.length) {
    if (A.scanning && !A.rows.length) {
      for (let i = 0; i < 8; i++) list.appendChild(el('div', 'skel-row'));
      list.setAttribute('aria-busy', 'true');
    } else {
      list.removeAttribute('aria-busy');
      list.appendChild(el('div', 'empty', A.rows.length ? 'Nothing matches the current filters.' : 'Nothing found yet. Press Rescan to look again.'));
    }
    return;
  }
  list.removeAttribute('aria-busy');
  const frag = document.createDocumentFragment();
  for (let i = 0; i < A.shown; i++) frag.appendChild(buildRow(A.view[i]));
  list.appendChild(frag);
  if (A.shown < A.view.length) list.appendChild(moreButton());
}

function moreButton() {
  const d = el('div', 'more');
  const b = el('button', '', 'Show more (' + (A.view.length - A.shown) + ' left)');
  b.type = 'button';
  b.onclick = () => {
    const list = $('list');
    const end = Math.min(A.view.length, A.shown + PAGE);
    const frag = document.createDocumentFragment();
    for (let i = A.shown; i < end; i++) frag.appendChild(buildRow(A.view[i]));
    A.shown = end;
    d.remove();
    list.appendChild(frag);
    if (A.shown < A.view.length) list.appendChild(moreButton());
  };
  d.appendChild(b);
  return d;
}

function checkCell(r, checked, disabled) {
  const wrap = el('label', 'cbcell');
  const cb = el('input');
  cb.type = 'checkbox';
  cb.checked = checked;
  cb.disabled = disabled;
  cb.dataset.act = 'select';
  cb.setAttribute('aria-label', (disabled ? 'In profile: ' : 'Select ') + r.display);
  wrap.appendChild(cb);
  return wrap;
}

function buildRow(r) {
  const k = key(r);
  const can = selectable(r);
  const ro = !editable();
  const idx = can && !ro ? selIndex(r.kind, selName(r)) : -1;
  const inBuiltin = can && ro && previewKeys().has(rowSelKey(r));
  const on = idx >= 0 || inBuiltin;
  const isStartup = r.kind === 'startup';
  const isAdminKind = r.kind === 'service' || r.kind === 'task';
  const blockedWhy = isAdminKind && r.admin && r.admin.blocked ? r.admin.blocked : '';
  const row = el('div', 'row' + (on ? ' sel' : '') + (can || (isStartup && r.can_change) ? '' : ' ro'));
  row.dataset.key = k;
  const line = el('div', 'line');

  if (can) {
    line.appendChild(checkCell(r, on, ro));
  } else {
    const t = el('span', 'tag lead', blockedWhy ? 'locked' : 'info');
    t.setAttribute('aria-label', blockedWhy ? 'Blocked' : 'Information only');
    t.title = blockedWhy ? blockedWhy : isStartup
      ? 'The RAM toggle does not apply to startup items; they only affect what starts at sign-in.'
      : 'Information only: the toggle cannot act on this kind of item.';
    line.appendChild(t);
  }

  const name = el('span', 'name', r.display || r.name);
  name.title = r.name;
  name.dataset.act = 'expand';
  if (isStartup) name.appendChild(el('small', '', r.can_change ? 'startup, applies at sign-in' : 'startup, read-only'));
  else if (blockedWhy) name.appendChild(el('small', '', 'blocked'));
  else if (!can) name.appendChild(el('small', '', r.kind + ', information only'));
  else if (r.pids && r.pids.length > 1 && r.kind === 'process') name.appendChild(el('small', '', r.pids.length + ' processes'));
  line.appendChild(name);

  line.appendChild(el('span', 'cell-ram', fmtRam(r.ram_bytes)));
  line.appendChild(el('span', 'tag', r.kind === 'process' || r.kind === 'service'
    ? (r.running ? 'running' : 'stopped') : (r.running ? 'enabled' : 'disabled')));

  const badge = el('span', 'badge ' + r.risk, cap(r.risk));
  badge.title = RISK_HINT[r.risk] || '';
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
    m.disabled = idx < 0 || ro;
    line.appendChild(m);
  } else if (can) {
    const lab = el('span', 'fixed-method', METHOD_LABEL[r.kind]);
    lab.title = r.kind === 'service' ? 'Stopped when you turn on, started again when you turn off.'
      : 'Disabled when you turn on, enabled again when you turn off.';
    line.appendChild(lab);
  } else if (isStartup && r.can_change) {
    const sw = el('label', 'switch' + (r.running ? ' on' : ''));
    const cb = el('input');
    cb.type = 'checkbox';
    cb.setAttribute('role', 'switch');
    cb.checked = !!r.running;
    cb.dataset.act = 'startup';
    cb.disabled = A.pending.has(k);
    cb.setAttribute('aria-label', (r.running ? 'Enabled' : 'Disabled') + ' at sign-in: ' + r.display);
    sw.append(cb, el('span', 'track'), el('span', 'sw-text', r.running ? 'Enabled' : 'Disabled'));
    line.appendChild(sw);
  } else {
    line.appendChild(el('span'));
  }

  if (isAdminKind) {
    const mb = el('button', 'morebtn', '...');
    mb.type = 'button';
    mb.dataset.act = 'more';
    const ok = !blockedWhy && r.admin && r.admin.target;
    mb.disabled = !ok;
    mb.title = !ok ? (blockedWhy || 'This item cannot be changed from here')
      : r.kind === 'service' ? 'Change start type... (a lasting change)' : (r.running ? 'Disable task... (a lasting change)' : 'Enable task... (a lasting change)');
    mb.setAttribute('aria-label', mb.title);
    line.appendChild(mb);
  } else {
    line.appendChild(el('span'));
  }

  const x = el('button', 'expander', A.expanded.has(k) ? '▾' : '▸');
  x.type = 'button';
  x.dataset.act = 'expand';
  x.setAttribute('aria-expanded', A.expanded.has(k) ? 'true' : 'false');
  x.setAttribute('aria-label', 'Show explanation for ' + r.display);
  line.appendChild(x);

  row.appendChild(line);
  if (isStartup && !r.can_change) row.appendChild(el('div', 'ro-note', ADMIN_NOTE));
  if (blockedWhy) row.appendChild(el('div', 'ro-note', 'Blocked: ' + blockedWhy));
  if (A.expanded.has(k)) row.appendChild(buildDetails(r));
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
      ? 'The switch controls whether this item starts when you sign in. '
      : ADMIN_NOTE + '. ') +
      'The RAM toggle and hotkey do not apply to startup items.'));
  } else if (r.admin && r.admin.blocked && (r.kind === 'service' || r.kind === 'task')) {
    d.appendChild(el('p', 'bad-text', 'Blocked: ' + r.admin.blocked + '. HoldMyRAM will never change this item.'));
  } else if (isAdminSel(r)) {
    d.appendChild(el('p', 'muted', 'Needs administrator approval. ' + (r.kind === 'service'
      ? 'When selected, it is stopped when you turn on and started again when you turn off. Its start type is not changed.'
      : 'When selected, it is disabled when you turn on and enabled again when you turn off.')));
  } else if (!r.actionable) {
    d.appendChild(el('p', 'muted', 'Information only: the toggle cannot act on this item.'));
  }
  return d;
}

function refreshRow(rowEl, r) { rowEl.replaceWith(buildRow(r)); }

/* ---------- list events (delegated) ---------- */

$('list').addEventListener('click', e => {
  const more = e.target.closest('[data-act="more"]');
  if (more) {
    const rr = A.byKey.get(more.closest('.row').dataset.key);
    if (rr && !more.disabled) openChange(rr);
    return;
  }
  const t = e.target.closest('[data-act="expand"]');
  if (!t) return;
  const rowEl = t.closest('.row');
  const r = A.byKey.get(rowEl.dataset.key);
  if (!r) return;
  const k = rowEl.dataset.key;
  if (A.expanded.has(k)) A.expanded.delete(k); else A.expanded.add(k);
  refreshRow(rowEl, r);
});

$('list').addEventListener('change', async e => {
  const t = e.target;
  const rowEl = t.closest('.row');
  if (!rowEl) return;
  const r = A.byKey.get(rowEl.dataset.key);
  if (r && r.kind === 'startup' && r.can_change && t.dataset.act === 'startup') {
    await onStartupSwitch(r, t.checked);
    return;
  }
  if (!r || !selectable(r) || !editable()) return;
  const nm = selName(r);
  if (t.dataset.act === 'select') {
    if (t.checked) {
      if (r.risk === 'dangerous') {
        const admin = isAdminSel(r);
        const text = r.display + ' is rated Dangerous.\n\n' + r.summary + (r.notes ? '\n\n' + r.notes : '') +
          (admin
            ? '\n\nStopping or disabling it can break Windows or hardware features.'
            : '\n\nClosing it can break Windows or hardware features. Select it anyway?');
        const ok = admin
          ? await askTyped({ title: 'Dangerous ' + r.kind, text, expect: [r.display, nm], ok: 'Select anyway' })
          : await ask({ title: 'Dangerous item', text, ok: 'Select anyway', danger: true });
        if (!ok) { t.checked = false; return; }
      }
      if (selIndex(r.kind, nm) < 0) sels().push({ name: nm, method: METHOD_WIRE[r.kind] || 'close', kind: r.kind });
    } else {
      const i = selIndex(r.kind, nm);
      if (i >= 0) sels().splice(i, 1);
    }
    if ($('only-selected').checked && !t.checked) renderList(false); else refreshRow(rowEl, r);
    renderSummary();
    renderHeader();
  } else if (t.dataset.act === 'method' && r.actionable) {
    const i = selIndex(r.kind, nm);
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

/** Enable or disable one startup item and update its row in place (no rescan). */
async function setStartup(r, enabled, opts = {}) {
  const k = key(r);
  A.pending.add(k);
  redrawRow(r);
  try {
    const ch = await invoke('set_startup_enabled', { name: r.name, detail: r.detail, enabled });
    r.running = ch && typeof ch.now_enabled === 'boolean' ? ch.now_enabled : enabled;
    showErr('');
    if (!opts.silent) {
      toast(r.display + (r.running ? ' will start at sign-in again. ' : ' will no longer start at sign-in. '),
        () => setStartup(r, !r.running));
    }
    if ($('startup-dlg').open) renderStartupChanges();
  } catch (e) {
    showErr('Could not change ' + r.display + ': ' + e);
  }
  A.pending.delete(k);
  redrawRow(r);
}

async function onStartupSwitch(r, want) {
  const k = key(r);
  if (A.pending.has(k)) return;
  if (!want && (r.risk === 'dangerous' || r.risk === 'caution')) {
    const dangerous = r.risk === 'dangerous';
    const ok = await ask({
      title: dangerous ? 'Dangerous item' : 'Disable at sign-in?',
      text: r.display + ' is rated ' + cap(r.risk) + '.\n\n' + r.summary + (r.notes ? '\n\n' + r.notes : '') +
        '\n\n' + (dangerous ? 'Stopping it starting at sign-in can break Windows or hardware features. ' : '') +
        'You can undo this from the Changes list.',
      ok: 'Disable at sign-in', danger: dangerous,
    });
    if (!ok) { redrawRow(r); return; }
  }
  await setStartup(r, want);
}

const startLabel = v => START_LABEL[v] || String(v);

/** The Changes panel: startup changes and admin (service/task) changes, newest first. */
async function renderStartupChanges() {
  const box = $('startup-list');
  const items = [];
  const problems = [];
  try {
    for (const c of await invoke('startup_changes')) items.push({ src: 'startup', at: c.at || '', c });
  } catch (e) { problems.push('Could not read startup changes: ' + e); }
  try {
    for (const c of await invoke('admin_changes')) items.push({ src: 'admin', at: c.at || '', c });
  } catch (e) { problems.push('Could not read service/task changes: ' + e); }
  items.reverse();
  items.sort((a, b) => (a.at < b.at ? 1 : a.at > b.at ? -1 : 0));
  box.replaceChildren();
  for (const p of problems) box.appendChild(el('p', 'bad-text', p));
  if (!items.length) {
    if (!problems.length) box.appendChild(el('p', 'empty', 'No changes yet. Anything you switch off here will be listed with an Undo button.'));
    return;
  }
  for (const it of items) {
    const c = it.c;
    const row = el('div', 'chg');
    const what = el('div', 'chg-what');
    const b = el('button', '', 'Undo');
    b.type = 'button';
    if (it.src === 'startup') {
      what.appendChild(el('span', 'chg-kind', 'startup'));
      what.appendChild(el('strong', '', c.name));
      what.appendChild(el('span', 'muted small', ' ' + (c.source || '') + ': ' + (c.was_enabled ? 'enabled' : 'disabled') +
        ' to ' + (c.now_enabled ? 'enabled' : 'disabled') + ', ' + c.at));
      b.title = 'Set it back to ' + (c.was_enabled ? 'enabled' : 'disabled');
      b.onclick = async () => {
        b.disabled = true;
        const r = A.byKey.get('startup|' + c.name.toLowerCase());
        if (r) {
          await setStartup(r, c.was_enabled, { silent: true });
        } else {
          try {
            await invoke('set_startup_enabled', { name: c.name, detail: 'source: ' + c.source, enabled: c.was_enabled });
            showErr('');
          } catch (e) { showErr('Could not undo ' + c.name + ': ' + e); }
        }
        renderStartupChanges();
      };
    } else {
      what.appendChild(el('span', 'chg-kind', c.kind === 'task_enabled' ? 'task' : 'service'));
      what.appendChild(el('strong', '', c.name));
      what.appendChild(el('span', 'muted small', ' ' + startLabel(c.before) + ' to ' + startLabel(c.now) + ', ' + c.at));
      b.title = 'Set it back to ' + startLabel(c.before);
      b.onclick = async () => {
        b.disabled = true;
        await undoAdmin(c);
        renderStartupChanges();
      };
    }
    row.appendChild(what);
    row.appendChild(b);
    box.appendChild(row);
  }
}

$('startup-btn').onclick = () => { renderStartupChanges(); $('startup-dlg').showModal(); };
$('startup-close').onclick = () => $('startup-dlg').close();

/* ---------- lasting service / task changes (separate from the toggle) ---------- */

function syncTaskRow(id, enabled) {
  if (!id || id.indexOf('task:') !== 0) return;
  const path = id.slice(5);
  for (const r of A.rows) {
    if (r.kind === 'task' && r.admin && r.admin.target && norm(r.admin.target) === path) {
      r.running = enabled;
      redrawRow(r);
    }
  }
}

async function undoAdmin(c) {
  try {
    await invoke('admin_undo', { id: c.id });
    if (c.kind === 'task_enabled') syncTaskRow(c.id, c.before === 'enabled');
    showErr('');
    return true;
  } catch (e) {
    showErr('Could not undo the change to ' + c.name + ': ' + e);
    return false;
  }
}

let chgRow = null;
function chgError(msg) { const e = $('chg-err'); e.hidden = !msg; e.textContent = msg || ''; }

function updateChgWarn() {
  const r = chgRow;
  const w = $('chg-warn');
  let msg = '';
  const disabling = r.kind === 'service' ? $('chg-choice').value === 'Disabled' : !!r.running;
  if (disabling && (r.risk === 'caution' || r.risk === 'dangerous')) {
    msg = r.display + ' is rated ' + cap(r.risk) + '. ' + (r.kind === 'service' ? 'Disabling it' : 'Turning it off') +
      ' can break Windows or hardware features.' + (r.risk === 'dangerous' ? ' You will be asked to type its name to confirm.' : '');
  }
  w.hidden = !msg;
  w.textContent = msg;
}

function openChange(r) {
  if (!r.admin || r.admin.blocked || !r.admin.target) return;
  chgRow = r;
  chgError('');
  const svc = r.kind === 'service';
  $('chg-title').textContent = svc ? 'Change start type: ' + r.display : (r.running ? 'Disable task: ' : 'Enable task: ') + r.display;
  $('chg-body').textContent = (svc
    ? 'This changes how Windows starts the service. '
    : (r.running ? 'This turns the scheduled task off. ' : 'This turns the scheduled task on. ')) +
    'It is a lasting change: it stays until you undo it from the Changes list, and it does not depend on the hotkey toggle. ' +
    'It needs administrator approval (a Windows prompt if the helper is not approved yet).';
  $('chg-field').hidden = !svc;
  if (svc) $('chg-choice').value = 'Manual';
  $('chg-ok').textContent = svc ? 'Apply' : (r.running ? 'Disable task' : 'Enable task');
  $('chg-ok').disabled = false;
  $('chg-ok').classList.toggle('danger', !svc && !!r.running && r.risk === 'dangerous');
  updateChgWarn();
  $('chg-dlg').showModal();
  (svc ? $('chg-choice') : $('chg-cancel')).focus();
}
$('chg-choice').onchange = () => { if (chgRow) updateChgWarn(); };
$('chg-cancel').onclick = () => { chgRow = null; $('chg-dlg').close(); };
$('chg-dlg').addEventListener('close', () => { chgRow = null; });

$('chg-ok').onclick = async () => {
  const r = chgRow;
  if (!r || A.adminBusy) return;
  const svc = r.kind === 'service';
  const start = svc ? $('chg-choice').value : null;
  const disabling = svc ? start === 'Disabled' : !!r.running;
  if (disabling && r.risk === 'dangerous') {
    $('chg-dlg').close();
    const ok = await askTyped({
      title: 'Dangerous ' + r.kind,
      text: r.display + ' is rated Dangerous.\n\n' + r.summary + (r.notes ? '\n\n' + r.notes : '') +
        '\n\nThis is a lasting change and can break Windows or hardware features.',
      expect: [r.display, r.admin.target], ok: svc ? 'Disable it' : 'Disable task',
    });
    if (!ok) return;
    chgRow = r;
  }
  A.adminBusy = true;
  $('chg-ok').disabled = true;
  $('chg-ok').textContent = 'Waiting...';
  let ch = null;
  try {
    ch = svc
      ? await invoke('admin_set_service_start', { name: r.admin.target, start })
      : await invoke('admin_set_task_enabled', { path: r.admin.target, enabled: !r.running });
  } catch (e) {
    A.adminBusy = false;
    if (!$('chg-dlg').open) { showErr('Could not change ' + r.display + ': ' + e); await refreshHelper(); renderHeader(); return; }
    $('chg-ok').disabled = false;
    $('chg-ok').textContent = svc ? 'Apply' : (r.running ? 'Disable task' : 'Enable task');
    chgError('Could not change it: ' + e);
    refreshHelper();
    return;
  }
  A.adminBusy = false;
  chgRow = null;
  if ($('chg-dlg').open) $('chg-dlg').close();
  showErr('');
  await refreshHelper();
  renderHeader();
  if (!ch) { toast('Nothing changed: ' + r.display + ' was already set that way.'); return; }
  if (ch.kind === 'task_enabled') syncTaskRow(ch.id, ch.now === 'enabled');
  toast(r.display + ': ' + startLabel(ch.before) + ' to ' + startLabel(ch.now) + '. This lasts until undone.',
    async () => { await undoAdmin(ch); if ($('startup-dlg').open) renderStartupChanges(); });
  if ($('startup-dlg').open) renderStartupChanges();
};

/* ---------- scan / focus refresh ---------- */

async function rescan() {
  if (A.scanning) return;
  A.scanning = true;
  $('scan-status').textContent = 'Scanning...';
  $('rescan').disabled = true;
  if (S.tab === 'advanced') renderList(false);
  try {
    // Selected processes that are not running still need a row so they can be deselected.
    const extra = sels().filter(s => selKindOf(s) === 'process').map(s => s.name);
    const rows = await invoke('scan', { extra });
    A.rows = rows;
    A.byKey = new Map(rows.map(r => [key(r), r]));
    A.selMap = null;
    A.scanned = true;
    $('scan-status').textContent = rows.length + ' items';
    showErr('');
  } catch (e) {
    showErr('Scan failed: ' + e);
    $('scan-status').textContent = '';
  }
  A.scanning = false;
  A.lastScanAt = Date.now();
  $('rescan').disabled = false;
  renderTabs();
  renderList(true);
  renderHeader();
}
$('rescan').onclick = rescan;

const MIN_SCAN_GAP_MS = 2000;
const rowSig = r => JSON.stringify(r, (k, v) => (k.charAt(0) === '_' ? undefined : v));

/** Merge a fresh scan into A.rows, keeping object identity so open closures stay valid. */
function mergeRows(fresh) {
  A.selMap = null;
  const seen = new Set();
  const changed = [];
  let structural = false;
  for (const n of fresh) {
    const k = key(n);
    seen.add(k);
    const old = A.byKey.get(k);
    if (!old) {
      A.rows.push(n);
      A.byKey.set(k, n);
      structural = true;
    } else if (!A.pending.has(k) && rowSig(old) !== rowSig(n)) {
      for (const f of Object.keys(old)) if (f.charAt(0) === '_') delete old[f];
      Object.assign(old, n);
      changed.push(old);
    }
  }
  for (const [k] of A.byKey) {
    if (seen.has(k) || A.pending.has(k)) continue;
    A.byKey.delete(k);
    A.expanded.delete(k);
    structural = true;
  }
  if (structural) A.rows = A.rows.filter(r => A.byKey.get(key(r)) === r);
  return { changed, structural };
}

/** Rescan quietly when the window regains focus and the Advanced tab is showing. */
async function silentRefresh() {
  if (!S.cfg || !A.scanned || A.scanning || A.pending.size || S.tab !== 'advanced') return;
  if (Date.now() - (A.lastScanAt || 0) < MIN_SCAN_GAP_MS) return;
  for (const d of document.querySelectorAll('dialog[open]')) if (d.id !== 'startup-dlg') return;
  A.scanning = true;
  let fresh = null;
  try {
    fresh = await invoke('scan', { extra: sels().filter(s => selKindOf(s) === 'process').map(s => s.name) });
  } catch (_) { /* silent: keep the current view */ }
  A.scanning = false;
  A.lastScanAt = Date.now();
  if (!fresh) return;
  const { changed, structural } = mergeRows(fresh);
  if (!changed.length && !structural) return;
  const list = $('list');
  const top = list.scrollTop;
  const before = A.view.slice(0, A.shown).map(key).join('\n');
  const oldShown = A.shown, oldLen = A.view.length;
  buildView();
  const after = A.view.slice(0, Math.min(oldShown, A.view.length)).map(key).join('\n');
  if (structural || before !== after || A.view.length !== oldLen) {
    renderTabs();
    renderList(false);
    list.scrollTop = top;
  } else {
    for (const r of changed) redrawRow(r);
  }
  $('scan-status').textContent = A.rows.length + ' items, updated';
}

/* ---------- filters ---------- */

let searchTimer = 0;
$('search').addEventListener('input', () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => renderList(true), 120);
});
$('sort').onchange = () => renderList(true);
$('only-selected').onchange = () => renderList(true);
$('hide-ms').onchange = () => {
  lsSet('hmr.hideMs', $('hide-ms').checked ? '1' : '0');
  renderTabs();
  renderList(true);
};
