'use strict';
// Profiles tab: list, activate, duplicate, rename, delete, export and import.

function allProfileNames() {
  const names = infos().filter(p => p.builtin).map(p => p.name).concat(BUILTIN_FALLBACK);
  return names.concat(S.cfg.profiles.map(p => p.name));
}

function profileNameError(name, except) {
  const n = norm(name);
  if (!n) return 'Enter a name.';
  if (allProfileNames().some(x => norm(x) === n && norm(x) !== norm(except || ''))) return 'A profile with that name already exists.';
  return '';
}

function uniqueName(base) {
  let n = base, i = 2;
  while (profileNameError(n)) n = base + ' ' + i++;
  return n;
}

/** Ask for a profile name until it is valid. Resolves the trimmed name or null. */
async function askName({ title, text, value, ok, except }) {
  let name = await ask({ title, text, input: value, ok });
  while (name !== null) {
    const err = profileNameError(name, except);
    if (!err) break;
    name = await ask({ title, text: err, input: name.trim(), ok });
  }
  return name === null ? null : name.trim();
}

function profileEntries() {
  const out = [];
  const bi = infos().filter(p => p.builtin);
  const builtins = bi.length ? bi : BUILTIN_FALLBACK.map(n => ({ name: n, description: '', needs_admin: n.endsWith('+') }));
  for (const p of builtins) out.push({ name: p.name, desc: p.description, builtin: true, needs_admin: !!p.needs_admin });
  for (const p of S.cfg.profiles) {
    out.push({ name: p.name, desc: p.description || '', builtin: false, count: p.selections.length,
      needs_admin: p.selections.some(s => selKindOf(s) !== 'process') });
  }
  if (S.cfg.selections.length) {
    out.push({ name: '', label: 'My own selection', desc: 'The unnamed list used when no profile is chosen.', builtin: false,
      isDefault: true, count: S.cfg.selections.length, needs_admin: S.cfg.selections.some(s => selKindOf(s) !== 'process') });
  }
  return out;
}

function renderProfiles() {
  if (!S.cfg) return;
  const box = $('prof-list');
  const a = document.activeElement;
  const fid = a && a.dataset ? a.dataset.fid : null;
  box.replaceChildren();
  const cur = norm(activeName());
  for (const p of profileEntries()) {
    const isActive = norm(p.name) === cur;
    const card = el('div', 'card prof' + (isActive ? ' is-active' : ''));
    const head = el('div', 'card-head');
    head.appendChild(el('h3', '', p.label || p.name));
    if (isActive) head.appendChild(el('span', 'tag active-tag', 'Active'));
    head.appendChild(el('span', 'tag', p.builtin ? 'Built in, read-only' : plural(p.count || 0, 'item')));
    if (p.needs_admin) head.appendChild(el('span', 'tag admin', 'Needs admin approval'));
    card.appendChild(head);
    if (p.desc) card.appendChild(el('p', 'muted small', p.desc));
    const row = el('div', 'btn-row');
    const mk = (label, fn, cls, disabled) => {
      const b = el('button', cls || '', label);
      b.type = 'button';
      b.dataset.fid = 'p:' + label + ':' + p.name;
      b.setAttribute('aria-label', label + ' ' + (p.label || p.name));
      b.disabled = !!disabled;
      b.onclick = fn;
      row.appendChild(b);
    };
    mk(isActive ? 'In use' : 'Use', () => { S.cfg.active_profile = p.name || null; onCfgChange({ preview: false }); schedulePreview(0); }, isActive ? '' : 'primary', isActive);
    mk('Duplicate', () => duplicateProfile(p.name));
    if (!p.builtin && !p.isDefault) {
      mk('Rename', () => renameProfile(p.name));
      mk('Delete', () => deleteProfile(p.name), 'danger');
    }
    if (!p.isDefault) mk('Export', () => exportProfile(p.name, p.builtin));
    card.appendChild(row);
    box.appendChild(card);
  }
  if (fid) for (const e of box.querySelectorAll('[data-fid]')) if (e.dataset.fid === fid) { e.focus(); break; }
}

/** Duplicate a profile (built-in or user). Needs the saved config, so unsaved edits are saved first. */
async function duplicateProfile(name, opts = {}) {
  if (isDirty() && !(await save())) return null;
  const label = name || 'My own selection';
  const nn = await askName({ title: 'Duplicate profile', text: 'Name for the copy of "' + label + '".', value: uniqueName(label + ' copy'), ok: 'Duplicate' });
  if (nn === null) return null;
  try {
    const r = await invoke('duplicate_profile', { name, new_name: nn });
    adoptState(r.state);
    if (opts.activate) {
      S.cfg.active_profile = r.profile_name;
      await save();
    }
    showErr('');
    onCfgChange();
    toast('Created "' + r.profile_name + '".' + (opts.activate ? ' You can now edit it.' : ''));
    return r.profile_name;
  } catch (e) {
    showErr('Could not duplicate the profile: ' + e);
    return null;
  }
}

async function renameProfile(name) {
  const p = S.cfg.profiles.find(x => norm(x.name) === norm(name));
  if (!p) return;
  const nn = await askName({ title: 'Rename profile', value: p.name, ok: 'Rename', except: p.name });
  if (nn === null) return;
  const was = norm(S.cfg.active_profile) === norm(p.name);
  p.name = nn;
  if (was) S.cfg.active_profile = nn;
  await save();
  onCfgChange({ preview: false });
}

async function deleteProfile(name) {
  const p = S.cfg.profiles.find(x => norm(x.name) === norm(name));
  if (!p) return;
  const ok = await ask({ title: 'Delete profile', text: 'Delete "' + p.name + '"? It is removed from your settings right away. Built-in profiles are not affected.', ok: 'Delete', danger: true });
  if (!ok) return;
  S.cfg.profiles = S.cfg.profiles.filter(x => x !== p);
  if (norm(S.cfg.active_profile) === norm(p.name)) S.cfg.active_profile = BUILTIN_FALLBACK[0];
  await save();
  onCfgChange();
}

async function exportProfile(name, builtin) {
  let path = null;
  try { path = await invoke('pick_save_path', { default_name: name.replace(/[^\w+.-]+/g, '-') + '.toml' }); }
  catch (e) { showErr('Could not open the save dialog: ' + e); return; }
  if (!path) return;
  if (isDirty() && !(await save())) return; // export reads the saved config
  try {
    const out = await invoke('export_profile', { name, path });
    showErr('');
    toast((builtin ? 'Saved a snapshot of "' : 'Exported "') + name + '" to ' + (out || path) + '.');
  } catch (e) { showErr('Could not export: ' + e); }
}

function importResultNode(r) {
  const n = el('div', 'import-result');
  n.appendChild(el('p', '', '"' + r.profile_name + '" was added with ' + plural(r.imported_count, 'item') + '.'));
  const dropped = r.dropped || [];
  if (dropped.length) {
    n.appendChild(el('p', 'warn-text', plural(dropped.length, 'item') + ' ' + (dropped.length === 1 ? 'was' : 'were') + ' left out:'));
    const ul = el('ul', 'plain-list');
    for (const d of dropped) ul.appendChild(el('li', '', d.name + ': ' + reasonText(d.reason) + '.'));
    n.appendChild(ul);
  }
  const w = r.warnings || [];
  if (w.length) {
    n.appendChild(el('p', 'small', 'Notes:'));
    const ul = el('ul', 'plain-list');
    for (const x of w) ul.appendChild(el('li', 'small', String(x)));
    n.appendChild(ul);
  }
  return n;
}

async function importProfileFlow() {
  let path = null;
  try { path = await invoke('pick_open_path'); }
  catch (e) { showErr('Could not open the file dialog: ' + e); return; }
  if (!path) return;
  if (isDirty() && !(await save())) return; // the import saves immediately
  let res;
  try {
    res = await invoke('import_profile', { path });
  } catch (e) {
    await ask({ title: 'Import failed', text: 'Nothing was imported. ' + e, ok: 'OK', cancel: null });
    return;
  }
  showErr('');
  await refreshState();
  await ask({ title: 'Profile imported', node: importResultNode(res), ok: 'OK', cancel: null });
}

$('prof-import').onclick = importProfileFlow;
$('prof-new').onclick = async () => {
  const nn = await askName({ title: 'New profile', text: 'Name for the new, empty profile. Tick items for it in Advanced.', value: '', ok: 'Create' });
  if (nn === null) return;
  S.cfg.profiles.push({ name: nn, description: '', selections: [] });
  S.cfg.active_profile = nn;
  await save();
  onCfgChange({ preview: false });
  showTab('advanced');
};
