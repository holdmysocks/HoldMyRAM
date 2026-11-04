'use strict';
// Startup, window focus refresh (no polling) and backend events.

let focusTimer = 0;
function onRegainFocus() {
  clearTimeout(focusTimer);
  if (!S.cfg) return;
  refreshHelper().then(renderHeader);
  if ($('startup-dlg').open) renderStartupChanges();
  focusTimer = setTimeout(() => { // one-shot debounce, not a loop
    if (S.tab === 'advanced') silentRefresh();
    else if (S.tab === 'home') { schedulePreview(0); loadUpdate(); }
    else if (S.tab === 'settings') { loadAutostart(); loadUpdate(); }
  }, 150);
}
window.addEventListener('focus', onRegainFocus);
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible') onRegainFocus();
});

(async function init() {
  try { if (lsGet('hmr.hideMs') === '0') $('hide-ms').checked = false; } catch (_) { /* ignore */ }
  try {
    const st = await invoke('get_state');
    S.state = st;
    setConfig(st.config, true);
  } catch (e) {
    setConfig({ hotkey: 'Ctrl+Alt+R', theme: 'dark', grace_seconds: 5, selections: [], profiles: [], active_profile: 'Recommended' }, true);
    showErr('Could not read state: ' + e);
  }
  renderHeader();
  await refreshHelper();
  showTab(lsGet('hmr.tab') || 'home', { noStore: true });
  loadUpdate();
  try {
    await T.event.listen('state-changed', async () => {
      await refreshState();
      if (S.tab === 'home') schedulePreview(0);
    });
    await T.event.listen('update-changed', () => loadUpdate());
  } catch (_) { /* events unavailable; the state refreshes on the next action */ }
  if (S.state && S.state.tutorial_seen === false) openTour(true);
})();
