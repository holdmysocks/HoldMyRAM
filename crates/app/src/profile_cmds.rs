//! Window commands for profiles: list, preview, duplicate, per-profile export/import, and
//! the native file dialogs. The rules live in `holdmyram_core::profiles`; this module only
//! reads/saves the running config and moves blocking work off the async runtime.

use crate::{adopt_saved_config, blocking, debug_log, rt, StateView, WINDOW_LABEL};
use holdmyram_core::config::Config;
use holdmyram_core::foreground_pid;
use holdmyram_core::profiles::{self, Builtin, Ctx, ImportResult, Inventory, ProfileRef, Resolution, PROFILE_EXTENSION};
use holdmyram_core::risk::RiskDb;
use holdmyram_core::Selection;
use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

/// One entry of the profile list (built-ins first).
#[derive(Serialize)]
pub struct ProfileInfo {
    pub name: String,
    pub description: String,
    /// Read-only, rule-based; offer "Duplicate to customise" instead of edit/delete.
    pub builtin: bool,
    pub editable: bool,
    /// Can include services/tasks (the toggle may ask for admin approval).
    pub needs_admin: bool,
    pub active: bool,
    /// Number of stored selections; None for built-ins (resolved at toggle time).
    pub selection_count: Option<usize>,
}

pub fn profile_infos(cfg: &Config) -> Vec<ProfileInfo> {
    let active = |n: &str| cfg.active_profile.as_deref().is_some_and(|a| a.trim().eq_ignore_ascii_case(n.trim()));
    let mut out: Vec<ProfileInfo> = Builtin::ALL
        .into_iter()
        .map(|b| ProfileInfo {
            name: b.name().into(),
            description: b.description().into(),
            builtin: true,
            editable: false,
            needs_admin: b.includes_admin(),
            active: active(b.name()),
            selection_count: None,
        })
        .collect();
    out.extend(cfg.profiles.iter().map(|p| ProfileInfo {
        name: p.name.clone(),
        description: p.description.clone(),
        builtin: false,
        editable: true,
        needs_admin: p.selections.iter().any(|s| s.kind.is_admin()),
        active: active(&p.name),
        selection_count: Some(p.selections.len()),
    }));
    out
}

fn ctx() -> Ctx {
    Ctx { foreground: foreground_pid(), self_pid: std::process::id() }
}

fn risk_db() -> RiskDb {
    let (db, w) = RiskDb::load();
    if let Some(w) = w {
        debug_log(&format!("risk db: {w}"));
    }
    db
}

/// For the toggle worker: what to apply. Built-ins resolve now (same path as the preview).
pub fn toggle_selections(cfg: &Config, fg: Option<u32>, self_pid: u32) -> Vec<Selection> {
    let (sels, res) = profiles::toggle_selections(cfg, Ctx { foreground: fg, self_pid }, &risk_db(), Inventory::scan);
    if let Some(r) = res {
        debug_log(&format!(
            "toggle: {} resolved to {} item(s), {} bytes known; skipped {:?}",
            r.profile,
            r.items.len(),
            r.total_ram_bytes,
            r.skipped_counts
        ));
    }
    sels
}

fn current_or(config: Option<Config>) -> Config {
    config.unwrap_or_else(|| rt().config.clone())
}

#[tauri::command]
pub fn list_profiles(config: Option<Config>) -> Vec<ProfileInfo> {
    profile_infos(&current_or(config))
}

/// What profile `name` would act on right now ("" = the top-level selections), from a
/// fresh read of the system. Changes nothing. `config` lets the window preview unsaved
/// edits (e.g. add-ons just ticked); omitted = the saved config.
#[tauri::command]
pub async fn preview_profile(name: String, config: Option<Config>) -> Result<Resolution, String> {
    blocking(move || profiles::preview(&current_or(config), &name, ctx(), &risk_db())).await
}

#[derive(Serialize)]
pub struct DuplicateResult {
    pub profile_name: String,
    pub state: StateView,
}

/// "Duplicate to customise". A built-in is frozen from its current resolution. Saves.
#[tauri::command]
pub async fn duplicate_profile(name: String, new_name: Option<String>) -> Result<DuplicateResult, String> {
    blocking(move || {
        let mut cfg = rt().config.clone();
        let res = match profiles::lookup(&cfg, &name) {
            Some(ProfileRef::Builtin(_)) => Some(profiles::preview(&cfg, &name, ctx(), &risk_db())?),
            _ => None,
        };
        let profile_name = profiles::duplicate(&mut cfg, &name, new_name.as_deref(), res.as_ref())?;
        let state = adopt_saved_config(cfg)?;
        Ok(DuplicateResult { profile_name, state })
    })
    .await
}

/// Write one profile to `path` (".hmrprofile" added if missing). A built-in is exported as
/// a frozen snapshot of what it resolves to now. Returns the path written.
#[tauri::command]
pub async fn export_profile(name: String, path: String) -> Result<String, String> {
    blocking(move || {
        let cfg = rt().config.clone();
        let res = match profiles::lookup(&cfg, &name) {
            Some(ProfileRef::Builtin(_)) => Some(profiles::preview(&cfg, &name, ctx(), &risk_db())?),
            _ => None,
        };
        let out = profiles::export_profile(&cfg, &name, &PathBuf::from(path.trim()), res.as_ref())?;
        Ok(out.display().to_string())
    })
    .await
}

/// Validate a ".hmrprofile" file and add it to the saved config as a NEW profile (a name
/// collision gets " (2)" etc.; nothing is overwritten). The window should save pending
/// edits first, then refresh with `get_state`.
#[tauri::command]
pub async fn import_profile(path: String) -> Result<ImportResult, String> {
    blocking(move || {
        let mut cfg = rt().config.clone();
        let result = profiles::import_file(&PathBuf::from(path.trim()), &cfg)?;
        profiles::adopt(&mut cfg, &result)?;
        adopt_saved_config(cfg)?;
        Ok(result)
    })
    .await
}

fn dialog(app: &AppHandle) -> tauri_plugin_dialog::FileDialogBuilder<tauri::Wry> {
    let mut d = app.dialog().file().add_filter("HoldMyRAM profile", &[PROFILE_EXTENSION]);
    if let Some(w) = app.get_webview_window(WINDOW_LABEL) {
        d = d.set_parent(&w);
    }
    d
}

/// Native "Save as" dialog for a profile file. None when cancelled.
#[tauri::command]
pub async fn pick_save_path(app: AppHandle, default_name: Option<String>) -> Result<Option<String>, String> {
    blocking(move || {
        let mut d = dialog(&app);
        if let Some(n) = default_name.filter(|n| !n.trim().is_empty()) {
            d = d.set_file_name(sanitize_file_name(&n));
        }
        Ok(d.blocking_save_file().and_then(|p| p.into_path().ok()).map(|p| profiles::with_profile_extension(&p).display().to_string()))
    })
    .await
}

/// Native "Open" dialog for a profile file. None when cancelled.
#[tauri::command]
pub async fn pick_open_path(app: AppHandle) -> Result<Option<String>, String> {
    blocking(move || Ok(dialog(&app).blocking_pick_file().and_then(|p| p.into_path().ok()).map(|p| p.display().to_string()))).await
}

/// A profile name as a file name: characters Windows forbids become '_'.
fn sanitize_file_name(name: &str) -> String {
    let s: String = name.trim().chars().map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    let s = s.trim_end_matches(['.', ' ']).to_string();
    let s = if s.is_empty() { "profile".to_string() } else { s };
    format!("{s}.{PROFILE_EXTENSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert_eq!(sanitize_file_name("Gaming"), "Gaming.hmrprofile");
        assert_eq!(sanitize_file_name("Recommended+ \"x\"/y:z."), "Recommended+ _x__y_z.hmrprofile");
        assert_eq!(sanitize_file_name("  ...  "), "profile.hmrprofile");
    }

    #[test]
    fn list_has_builtins_first_and_flags() {
        let mut c = Config::default();
        c.profiles.push(holdmyram_core::config::Profile {
            name: "Mine".into(),
            selections: vec![Selection::service("SysMain")],
            ..Default::default()
        });
        let l = profile_infos(&c);
        let names: Vec<_> = l.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Recommended", "Recommended+", "Mine"]);
        assert!(l[0].builtin && !l[0].editable && l[0].active && !l[0].needs_admin && l[0].selection_count.is_none());
        assert!(l[1].needs_admin && !l[1].active);
        assert!(!l[2].builtin && l[2].editable && l[2].needs_admin && l[2].selection_count == Some(1));
    }
}
