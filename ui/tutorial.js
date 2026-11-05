'use strict';
// First-run tutorial: a short stepper in a modal dialog. The native modal dialog traps focus,
// Esc closes it, and focus returns to where it came from. Replaying does not change the saved flag.

const TOUR_STEPS = [
  { title: 'Free up RAM when you need it',
    body: ['HoldMyRAM closes the background programs you do not need, right when you ask, and brings them back afterwards.',
      'It lives in the tray and does nothing while idle. The tray part costs almost no memory, and this window only exists while it is open.'],
    art: artRam },
  { title: 'Pick a profile',
    body: ['A profile is the list of things to close. Recommended is a safe default, built from rules for your PC.',
      'Recommended+ goes further and needs admin approval. You can also make your own copy and change it.'],
    art: artProfiles },
  { title: 'One switch, one hotkey',
    body: ['Press the big button or your hotkey (Ctrl+Alt+R by default) to turn on. Press it again to turn off. If you press it by accident, Settings has a hold-to-confirm mode for the hotkey.',
      'The app you are using is never closed. Everything is restored when you turn off, or when you quit.'],
    art: artToggle },
  { title: 'Admin approval, when asked',
    body: ['Stopping services and tasks needs administrator rights, so Windows shows a prompt that says "Unknown publisher". That is expected: HoldMyRAM is not code-signed yet.',
      'Approve it ahead of time from Home, so it never interrupts a game. If you decline, only apps are closed.'],
    art: artAdmin },
  { title: 'Advanced, and Dangerous items',
    body: ['The Advanced tab shows everything running, with a risk badge and a plain explanation for each item.',
      'Dangerous items can break Windows or hardware features. HoldMyRAM asks you to confirm, and for services to type the name.'],
    art: artBadges },
  { title: 'Tray, sign-in and updates',
    body: ['Closing this window keeps HoldMyRAM in the tray. Right-click the tray icon to turn on, off or quit.',
      'In Settings you can start it when you sign in and check for updates. You can replay this tutorial there too. Notifications (Windows notification, small pop-up or none) are also chosen in Settings.'],
    art: artTray },
];

let tour = { i: 0, first: false, opener: null, done: false };

/* ---------- small illustrations (plain CSS and SVG, nothing fetched) ---------- */

function svgEl(tag, attrs) {
  const e = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const k of Object.keys(attrs || {})) e.setAttribute(k, attrs[k]);
  return e;
}
function trayIcon(on) {
  const s = svgEl('svg', { viewBox: '0 0 24 24', width: 36, height: 36 });
  s.appendChild(svgEl('rect', { x: 2, y: 5, width: 20, height: 14, rx: 3, class: on ? 'ti-on' : 'ti-off' }));
  for (const x of [5, 10.5, 16]) s.appendChild(svgEl('rect', { x, y: 9, width: 3, height: 6, rx: 1, class: 'ti-pin' }));
  return s;
}
function bar(label, pct, cls) {
  const r = el('div', 'bar-row');
  r.appendChild(el('span', 'small', label));
  const t = el('span', 'bar-track');
  const f = el('span', 'bar-fill ' + (cls || ''));
  f.style.width = pct + '%';
  t.appendChild(f);
  r.appendChild(t);
  r.appendChild(el('span', 'small', pct + '%'));
  return r;
}
function artRam() {
  const w = el('div', 'art art-ram');
  w.appendChild(bar('Before', 82, 'hi'));
  w.appendChild(bar('After', 49, 'lo'));
  return w;
}
function artProfiles() {
  const w = el('div', 'art art-chips');
  for (const [t, c] of [['Recommended', 'sel'], ['Recommended+', ''], ['My profile', '']]) w.appendChild(el('span', 'chipart ' + c, t));
  return w;
}
function artToggle() {
  const w = el('div', 'art art-toggle');
  const a = el('span', 'bigpill off', 'OFF');
  const k = el('span', 'kbds');
  ['Ctrl', 'Alt', 'R'].forEach((t, i) => { if (i) k.appendChild(document.createTextNode(' + ')); k.appendChild(el('kbd', '', t)); });
  w.append(a, k, el('span', 'arrow', '→'), el('span', 'bigpill on', 'ON'));
  return w;
}
function artAdmin() {
  const w = el('div', 'art art-uac');
  w.appendChild(el('div', 'uac-title', 'User Account Control'));
  w.appendChild(el('div', 'small', 'Do you want to allow this app to make changes?'));
  w.appendChild(el('div', 'small', 'Publisher: Unknown'));
  const b = el('div', 'uac-btns');
  b.append(el('span', 'uac-yes', 'Yes'), el('span', 'uac-no', 'No'));
  w.appendChild(b);
  return w;
}
function artBadges() {
  const w = el('div', 'art art-chips');
  for (const r of ['suggested', 'caution', 'dangerous', 'unrated']) w.appendChild(el('span', 'badge ' + r, cap(r)));
  return w;
}
function artTray() {
  const w = el('div', 'art art-tray');
  const t = el('span', 'trayfake');
  t.append(trayIcon(false), trayIcon(true));
  w.appendChild(t);
  const sw = el('span', 'switch on');
  sw.append(el('span', 'track fake-on'), el('span', 'sw-text', 'Start with Windows'));
  w.appendChild(sw);
  w.appendChild(el('span', 'chipart sel', 'Update available'));
  return w;
}

/* ---------- stepper ---------- */

function renderTour() {
  const s = TOUR_STEPS[tour.i];
  const last = tour.i === TOUR_STEPS.length - 1;
  $('tour-art').replaceChildren(s.art());
  $('tour-step').textContent = 'Step ' + (tour.i + 1) + ' of ' + TOUR_STEPS.length;
  $('tour-title').textContent = s.title;
  const body = $('tour-body');
  body.replaceChildren();
  s.body.forEach(t => body.appendChild(el('p', '', t)));
  const dots = $('tour-dots');
  dots.replaceChildren();
  TOUR_STEPS.forEach((_, i) => dots.appendChild(el('span', 'dot' + (i === tour.i ? ' cur' : i < tour.i ? ' past' : ''))));
  $('tour-back').disabled = tour.i === 0;
  $('tour-next').textContent = last ? 'Done' : 'Next';
  $('tour-skip').hidden = last;
}

function openTour(first) {
  const d = $('tour');
  if (d.open) return;
  tour = { i: 0, first: !!first, opener: document.activeElement, done: false };
  renderTour();
  d.showModal();
  $('tour-next').focus();
}

$('tour-next').onclick = () => {
  if (tour.i >= TOUR_STEPS.length - 1) { closeTour(); return; }
  tour.i++;
  renderTour();
  $('tour-next').focus();
};
$('tour-back').onclick = () => {
  if (tour.i > 0) { tour.i--; renderTour(); $('tour-next').focus(); }
};
$('tour-skip').onclick = closeTour;
$('tour').addEventListener('keydown', e => {
  if (e.key === 'ArrowRight' && e.target.tagName !== 'INPUT') { $('tour-next').click(); }
  else if (e.key === 'ArrowLeft') { $('tour-back').click(); }
});

// Esc, Done and Skip all end up here (the native dialog fires "close").
function closeTour() { const d = $('tour'); if (d.open) d.close(); finishTour(); }
$('tour').addEventListener('close', () => finishTour());
async function finishTour() {
  if (tour.done) return;
  tour.done = true;
  if (tour.first) {
    try {
      const st = await invoke('set_tutorial_seen', { seen: true });
      S.state = st;
      S.cfg.tutorial_seen = true;
    } catch (e) { showErr('Could not remember that the tutorial was shown: ' + e); }
  }
  const o = tour.opener;
  if (o && o.isConnected && typeof o.focus === 'function') o.focus();
  else $('toggle-btn').focus();
}
