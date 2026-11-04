//! Decides which selected processes may be touched on a toggle.
//! Pure functions over a process snapshot, so the safety rules are unit-testable.

use crate::process::ProcInfo;
use std::collections::{HashMap, HashSet};

/// Never touched, regardless of selection. Matched case-insensitively.
const PROTECTED_NAMES: &[&str] = &[
    "system", "registry", "smss.exe", "csrss.exe", "wininit.exe", "winlogon.exe",
    "services.exe", "lsass.exe", "svchost.exe", "fontdrvhost.exe", "dwm.exe",
    "explorer.exe", "sihost.exe", "ctfmon.exe", "msmpeng.exe", "securityhealthservice.exe",
    "memory compression", "runtimebroker.exe", "taskhostw.exe", "audiodg.exe",
    // Anti-cheat services/clients: touching these can get a player flagged or banned.
    "vgc.exe", "vgtray.exe", "beservice.exe", "battleye.exe", "easyanticheat.exe",
    "easyanticheat_eos.exe", "eaanticheat.gameservice.exe", "faceitservice.exe",
    "faceitclient.exe",
];

#[derive(Debug, PartialEq, Eq)]
pub enum Skipped {
    Foreground,
    SelfProcess,
    Protected,
}

/// `root` and everything below it in the parent/child tree.
pub fn descendants(procs: &[ProcInfo], root: u32) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs {
        children.entry(p.parent).or_default().push(p.pid);
    }
    let mut seen = HashSet::from([root]);
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        for &c in children.get(&pid).into_iter().flatten() {
            if seen.insert(c) {
                stack.push(c);
            }
        }
    }
    seen
}

/// Parents of `pid` up the chain. A visited set guards against PID reuse cycles.
pub fn ancestors(procs: &[ProcInfo], pid: u32) -> HashSet<u32> {
    let parent_of: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.parent)).collect();
    let mut out = HashSet::new();
    let mut cur = pid;
    while let Some(&parent) = parent_of.get(&cur) {
        if parent == 0 || !out.insert(parent) {
            break;
        }
        cur = parent;
    }
    out
}

/// Split the selected process names into targets to act on and processes skipped,
/// with the reason. The foreground app's whole tree and its ancestors are always skipped.
pub fn select_targets<'a>(
    procs: &'a [ProcInfo],
    selected_names: &[String],
    foreground: Option<u32>,
    self_pid: u32,
) -> (Vec<&'a ProcInfo>, Vec<(&'a ProcInfo, Skipped)>) {
    let mut guarded: HashSet<u32> = HashSet::new();
    if let Some(fg) = foreground {
        guarded.extend(descendants(procs, fg));
        guarded.extend(ancestors(procs, fg));
    }
    let selected: HashSet<String> = selected_names.iter().map(|n| n.to_lowercase()).collect();

    let mut targets = Vec::new();
    let mut skipped = Vec::new();
    for p in procs {
        let lname = p.name.to_lowercase();
        if !selected.contains(&lname) {
            continue;
        }
        if p.pid == self_pid {
            skipped.push((p, Skipped::SelfProcess));
        } else if PROTECTED_NAMES.contains(&lname.as_str()) {
            skipped.push((p, Skipped::Protected));
        } else if guarded.contains(&p.pid) {
            skipped.push((p, Skipped::Foreground));
        } else {
            targets.push(p);
        }
    }
    (targets, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent: u32, name: &str) -> ProcInfo {
        ProcInfo { pid, parent, name: name.into(), working_set_bytes: 0 }
    }

    fn sample() -> Vec<ProcInfo> {
        vec![
            p(4, 0, "System"),
            p(100, 4, "steam.exe"),
            p(200, 100, "game.exe"),
            p(210, 200, "game-helper.exe"),
            p(300, 4, "chrome.exe"),
            p(310, 300, "chrome.exe"),
            p(400, 4, "discord.exe"),
            p(500, 4, "svchost.exe"),
        ]
    }

    #[test]
    fn descendants_include_root_and_children() {
        let d = descendants(&sample(), 200);
        assert_eq!(d, HashSet::from([200, 210]));
    }

    #[test]
    fn ancestors_walk_up_and_stop_at_root() {
        let a = ancestors(&sample(), 210);
        assert_eq!(a, HashSet::from([200, 100, 4]));
    }

    #[test]
    fn ancestors_survive_a_pid_cycle() {
        let cyc = vec![p(1, 2, "a"), p(2, 1, "b")];
        assert_eq!(ancestors(&cyc, 1), HashSet::from([1, 2]));
    }

    #[test]
    fn foreground_tree_and_ancestors_are_never_targets() {
        let procs = sample();
        let sel = vec!["steam.exe".into(), "game-helper.exe".into(), "discord.exe".into()];
        let (targets, skipped) = select_targets(&procs, &sel, Some(200), 9999);
        let names: Vec<_> = targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["discord.exe"]);
        assert_eq!(skipped.len(), 2);
        assert!(skipped.iter().all(|(_, why)| *why == Skipped::Foreground));
    }

    #[test]
    fn browser_helpers_are_spared_when_browser_is_foreground() {
        let procs = sample();
        let sel = vec!["chrome.exe".into()];
        let (targets, skipped) = select_targets(&procs, &sel, Some(300), 9999);
        assert!(targets.is_empty());
        assert_eq!(skipped.len(), 2);
    }

    #[test]
    fn protected_and_self_are_never_targets_even_if_selected() {
        let procs = sample();
        let sel = vec!["svchost.exe".into(), "discord.exe".into()];
        let (targets, skipped) = select_targets(&procs, &sel, None, 400);
        assert!(targets.is_empty());
        assert!(skipped.iter().any(|(_, w)| *w == Skipped::Protected));
        assert!(skipped.iter().any(|(_, w)| *w == Skipped::SelfProcess));
    }

    #[test]
    fn anti_cheat_processes_are_protected() {
        let procs = vec![p(10, 4, "EasyAntiCheat_EOS.exe"), p(11, 4, "vgc.exe")];
        let sel = vec!["easyanticheat_eos.exe".into(), "VGC.EXE".into()];
        let (targets, skipped) = select_targets(&procs, &sel, None, 9999);
        assert!(targets.is_empty());
        assert_eq!(skipped.len(), 2);
    }

    #[test]
    fn name_match_is_case_insensitive() {
        let procs = sample();
        let sel = vec!["DISCORD.EXE".into()];
        let (targets, _) = select_targets(&procs, &sel, None, 9999);
        assert_eq!(targets.len(), 1);
    }
}
