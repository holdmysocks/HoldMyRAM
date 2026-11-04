use super::*;
use crate::admin::rules;
use crate::config::Profile;
use crate::risk::RiskEntry;
use crate::types::AddonRef;

fn p(pid: u32, parent: u32, name: &str, ws: u64) -> ProcInfo {
    ProcInfo { pid, parent, name: name.into(), working_set_bytes: ws }
}

fn svc(name: &str, running: bool, pid: u32, ram: u64) -> SvcEntry {
    SvcEntry { name: name.into(), display: format!("{name} display"), running, pid, ram_bytes: ram }
}

fn task(path: &str, enabled: bool) -> TaskEntry {
    TaskEntry { path: path.into(), name: path.rsplit('\\').next().unwrap().into(), enabled }
}

const MB: u64 = 1024 * 1024;
const SELF: u32 = 999;

/// A synthetic PC: a foreground game (pid 20) with Widgets.exe as its child, Game Bar
/// running twice, Phone Link, a protected-dir CrossDeviceResume, Discord (optional) and a
/// service hosted inside the game's tree.
fn inventory() -> Inventory {
    let processes = vec![
        p(4, 0, "System", 0),
        p(10, 4, "GameBar.exe", 50 * MB),
        p(11, 10, "GameBar.exe", 10 * MB),
        p(12, 4, "PhoneExperienceHost.exe", 80 * MB),
        p(13, 4, "CrossDeviceResume.exe", 20 * MB),
        p(14, 4, "Discord.exe", 300 * MB),
        p(15, 4, "svchost.exe", 5 * MB),
        p(20, 4, "game.exe", 4000 * MB),
        p(21, 20, "Widgets.exe", 40 * MB),
        p(SELF, 4, "holdmyram.exe", 5 * MB),
    ];
    let mut exe_paths = HashMap::new();
    exe_paths.insert(10, r"C:\Program Files\WindowsApps\Microsoft.XboxGamingOverlay\GameBar.exe".to_string());
    exe_paths.insert(11, r"C:\Program Files\WindowsApps\Microsoft.XboxGamingOverlay\GameBar.exe".to_string());
    exe_paths.insert(13, r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS\CrossDeviceResume.exe".to_string());
    Inventory {
        processes,
        exe_paths,
        services: vec![
            svc("DiagTrack", true, 15, 0),
            svc("CscService", false, 0, 0),
            svc("dmwappushservice", true, 30, 3 * MB),
            svc("SysMain", true, 15, 0),
            svc("CDPUserSvc", false, 0, 0),
            svc("CDPUserSvc_8155c", true, 15, 0),
            svc("MapsBroker", true, 21, 12 * MB),
            svc("Spooler", true, 31, 8 * MB),
            svc("edgeupdate", true, 32, 3 * MB),
            svc("wuauserv", true, 15, 0),
            svc("RpcSs", true, 15, 0),
        ],
        tasks: vec![
            task(r"\Microsoft\Windows\Defrag\ScheduledDefrag", true),
            task(r"\Microsoft\Windows\Customer Experience Improvement Program\Consolidator", true),
            task(r"\Microsoft\Windows\Maps\MapsUpdateTask", false),
            task(r"\Microsoft\Windows\Shell\FamilySafetyMonitor", true),
            task(r"\Microsoft\Windows\Shell\FamilySafetyRefreshTask", true),
            task(r"\GoogleUpdateTaskMachineCore{1234-ABCD}", true),
            task(r"\MicrosoftEdgeUpdateTaskMachineCore{1234-ABCD}", true),
            task(r"\Other\Unrelated", true),
        ],
        admin_scanned: true,
        physical_printers: Some(0),
    }
}

fn ctx() -> Ctx {
    Ctx { foreground: Some(20), self_pid: SELF }
}

fn cfg() -> Config {
    Config::default()
}

fn names(r: &Resolution) -> Vec<&str> {
    r.items.iter().map(|i| i.name.as_str()).collect()
}

fn skipped(r: &Resolution, name: &str) -> Option<SkipReason> {
    r.skipped.iter().find(|s| s.name.eq_ignore_ascii_case(name)).map(|s| s.reason)
}

// ------------------------------------------------------------------- data file

#[test]
fn recommended_data_parses_and_never_collides_with_protections() {
    let items = parse_recommended(RECOMMENDED_TOML).expect("data/recommended.toml must parse");
    assert_eq!(items.len(), recommended_items().len());
    assert!(items.iter().any(|i| i.kind == SelectionKind::Process && !i.is_addon()));
    assert!(items.iter().any(|i| i.kind == SelectionKind::Service));
    assert!(items.iter().any(|i| i.kind == SelectionKind::Task));
    let db = RiskDb::builtin();
    for i in &items {
        // A prefix rule is checked with its prefix (the never-touch folders are prefixes too).
        let probe = i.name.trim_end_matches('*');
        assert!(admin_block_reason(i.kind, probe).is_none(), "{} is on a never-touch list", i.name);
        match i.kind {
            SelectionKind::Process => assert!(!is_protected_name(&i.name), "{} is a protected process", i.name),
            SelectionKind::Service => {
                assert!(rules::never_touch_service(&i.name).is_none(), "{}", i.name);
                rules::validate_service_name(&i.name).unwrap();
            }
            SelectionKind::Task => {
                for f in rules::NEVER_TOUCH_TASK_FOLDERS {
                    assert!(!f.to_lowercase().starts_with(&probe.to_lowercase()), "{} would cover never-touch folder {f}", i.name);
                }
            }
        }
        assert_ne!(risk_of(&db, i.kind, &i.name), Risk::Dangerous, "{} is rated Dangerous", i.name);
        // Never-recommend list: no collision at all.
        assert!(never_recommend(i.kind, &i.name).is_none(), "{} is on the never-recommend list", i.name);
        // Optional apps are processes; low confidence never reaches the defaults.
        if i.group == OPTIONAL_GROUP {
            assert_eq!(i.kind, SelectionKind::Process, "{}", i.name);
        }
        if i.confidence == Confidence::Low {
            assert!(i.is_addon() && ADDON_GROUPS.contains(&i.group.as_str()), "{} is low confidence but a default", i.name);
        }
    }
    // The defaults are not empty for either built-in.
    assert!(items.iter().any(|i| !i.is_addon() && i.kind == SelectionKind::Process));
    assert!(items.iter().any(|i| !i.is_addon() && i.kind == SelectionKind::Service));
    assert!(items.iter().any(|i| !i.is_addon() && i.kind == SelectionKind::Task));
}

#[test]
fn never_recommend_list() {
    for (k, n) in [
        (SelectionKind::Service, "wuauserv"),
        (SelectionKind::Service, "usosvc"),
        (SelectionKind::Service, "WaaSMedicSvc"),
        (SelectionKind::Service, "BITS"),
        (SelectionKind::Service, "DoSvc"),
        (SelectionKind::Service, "bthserv"),
        (SelectionKind::Service, "Tailscale"),
        (SelectionKind::Service, "Parsec"),
        (SelectionKind::Service, "edgeupdate"),
        (SelectionKind::Service, "BthAvctpSvc_1a2b3"),
        (SelectionKind::Task, r"\Microsoft\Windows\AppID\SmartScreenSpecific"),
        (SelectionKind::Task, r"\Microsoft\Windows\ExploitGuard\ExploitGuard MDM policy Refresh"),
        (SelectionKind::Task, r"\Microsoft\Windows\InstallService\ScanForUpdates"),
        (SelectionKind::Task, r"\Microsoft\Windows\UNP\RunUpdateNotificationMgr"),
        (SelectionKind::Task, r"\Microsoft\Windows\LanguageComponentsInstaller\Installation"),
        (SelectionKind::Task, r"\Microsoft\Windows\Chkdsk\SyspartRepair"),
        (SelectionKind::Task, r"\MicrosoftEdgeUpdateTaskMachineCore{X}"),
        // A wildcard that would cover a never-recommend prefix.
        (SelectionKind::Task, r"\Microsoft\Windows\Install*"),
        (SelectionKind::Process, "msedgewebview2.exe"),
        (SelectionKind::Process, "GHELPER.EXE"),
        (SelectionKind::Process, "tailscaled.exe"),
    ] {
        assert!(never_recommend(k, n).is_some(), "{n}");
    }
    for (k, n) in [
        (SelectionKind::Service, "DiagTrack"),
        (SelectionKind::Task, r"\Microsoft\Windows\Chkdsk\ProactiveScan"),
        (SelectionKind::Process, "tailscale-ipn.exe"),
        (SelectionKind::Process, "GameBar.exe"),
    ] {
        assert!(never_recommend(k, n).is_none(), "{n}");
    }
}

#[test]
fn printer_detection_counts_only_physical_printers() {
    let l = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>();
    let virtuals = l(&[
        ("Microsoft Print to PDF", "PORTPROMPT:"),
        ("Microsoft XPS Document Writer", "PORTPROMPT:"),
        ("Fax", "SHRFAX:"),
        ("OneNote (Desktop)", "nul:"),
        ("OneNote for Windows 10", "Microsoft.Office.OneNote_16001.14326.21802.0_x64__8wekyb3d8bbwe_microsoft.onenoteim_S-1-5-21"),
    ]);
    assert_eq!(printers::physical_count(&virtuals), 0);
    let mut with_real = virtuals.clone();
    with_real.extend(l(&[("HP LaserJet", "USB001")]));
    assert_eq!(printers::physical_count(&with_real), 1);
    // A real printer named like a virtual one still counts; an unreadable port counts.
    assert_eq!(printers::physical_count(&l(&[("Fax", "WSD-1234"), ("Mystery", "")])), 2);
}

#[test]
fn spooler_only_without_a_printer() {
    let db = RiskDb::builtin();
    let run = |printers: Option<usize>| {
        let mut inv = inventory();
        inv.physical_printers = printers;
        resolve(&cfg(), ProfileRef::Builtin(Builtin::RecommendedPlus), &inv, ctx(), &db)
    };
    assert!(names(&run(Some(0))).contains(&"Spooler"));
    for p in [Some(1), None] {
        let r = run(p);
        assert!(!names(&r).contains(&"Spooler"));
        assert_eq!(skipped(&r, "Spooler"), Some(SkipReason::ConditionNotMet));
    }
    // Recommended (no admin) never even considers it.
    let r = resolve(&cfg(), ProfileRef::Builtin(Builtin::Recommended), &inventory(), ctx(), &db);
    assert!(skipped(&r, "Spooler").is_none());
}

#[test]
fn every_suggested_risk_process_is_considered() {
    // Seeded from data/risk.toml: each Suggested process there is in the rule list.
    let items = recommended_items();
    for e in RiskDb::builtin().entries().filter(|e| e.risk == Risk::Suggested && e.kind == Kind::Process) {
        assert!(items.iter().any(|i| i.kind == SelectionKind::Process && i.matches(&e.name)), "{} missing", e.name);
    }
}

#[test]
fn rule_list_rejects_bad_entries() {
    let one = |body: &str| parse_recommended(&format!("format_version = 1\n[[item]]\n{body}\n"));
    let ok = "group = \"x\"\nconfidence = \"medium\"\nwhy = \"w\"\nside_effect = \"s\"";
    let base = |c: &str, g: &str| format!("kind = \"process\"\nname = \"a.exe\"\ngroup = \"{g}\"\nconfidence = \"{c}\"\nwhy = \"w\"\nside_effect = \"s\"");
    one(&base("low", "optin")).unwrap();
    one(&base("high", "optional")).unwrap();
    assert!(one(&base("low", "xbox")).is_err(), "low confidence must be an add-on");
    assert!(one(&base("sure", "xbox")).is_err(), "unknown confidence");
    assert!(one("kind = \"process\"\nname = \"a.exe\"\ngroup = \"x\"\nwhy = \"w\"\nside_effect = \"s\"").is_err(), "confidence required");
    assert!(one(&format!("kind = \"service\"\nname = \"wuauserv\"\n{ok}")).is_err(), "never-recommend");
    assert!(one(&format!("kind = \"service\"\nname = \"BEDaisy\"\n{ok}")).is_err(), "never-touch");
    assert!(one(&format!("kind = \"process\"\nname = \"lsass.exe\"\n{ok}")).is_err(), "protected process");
    assert!(one(&format!("kind = \"task\"\nname = '\\Microsoft\\Windows\\Windows*'\n{ok}")).is_err(), "wildcard over a never-touch folder");
    assert!(one(&format!("kind = \"service\"\nname = \"Spooler\"\ncondition = \"no_printer\"\n{ok}")).is_ok());
    assert!(one(&format!("kind = \"service\"\nname = \"Fax\"\ncondition = \"no_printer\"\n{ok}")).is_err(), "condition misuse");
    assert!(one(&format!("kind = \"service\"\nname = \"Spooler\"\ncondition = \"full_moon\"\n{ok}")).is_err());
    one(&format!("kind = \"process\"\nname = \"a.exe\"\n{ok}")).unwrap();
    one(&format!("kind = \"task\"\nname = '\\A\\B*'\n{ok}")).unwrap();
    assert!(one(&format!("kind = \"process\"\nname = \"a.exe\"\nbogus = 1\n{ok}")).is_err(), "unknown key");
    assert!(one(&format!("kind = \"driver\"\nname = \"a\"\n{ok}")).is_err(), "unknown kind");
    assert!(one(&format!("kind = \"service\"\nname = \"a\"\nmethod = \"kill\"\n{ok}")).is_err(), "method on service");
    assert!(one(&format!("kind = \"process\"\nname = 'C:\\x.exe'\n{ok}")).is_err(), "path as exe");
    assert!(one(&format!("kind = \"process\"\nname = \"a*.exe\"\n{ok}")).is_err(), "wildcard on process");
    assert!(one(&format!("kind = \"task\"\nname = '\\A\\*'\n{ok}")).is_err(), "empty prefix");
    assert!(one("kind = \"process\"\nname = \"a.exe\"\ngroup = \"x\"\nconfidence = \"high\"\nwhy = \"\"\nside_effect = \"s\"").is_err(), "empty why");
    assert!(one(&base("high", "Two Words")).is_err());
    let dup = format!("format_version = 1\n[[item]]\nkind = \"process\"\nname = \"a.exe\"\n{ok}\n[[item]]\nkind = \"process\"\nname = \"A.EXE\"\n{ok}\n");
    assert!(parse_recommended(&dup).is_err(), "duplicate");
    assert!(parse_recommended("format_version = 2\n").is_err());
}

#[test]
fn rule_matching() {
    let r = |kind, name: &str| RecItem {
        kind,
        name: name.into(),
        group: "g".into(),
        confidence: Confidence::Medium,
        why: "w".into(),
        side_effect: "s".into(),
        method: None,
        display: None,
        condition: None,
    };
    assert!(r(SelectionKind::Process, "GameBar.exe").matches("gamebar.EXE"));
    let s = r(SelectionKind::Service, "CDPUserSvc");
    assert!(s.matches("CDPUserSvc") && s.matches("cdpusersvc_8155C") && !s.matches("CDPUserSvcX") && !s.matches("CDPUserSvc_zz"));
    let t = r(SelectionKind::Task, r"\MicrosoftEdgeUpdateTaskMachineCore*");
    assert!(t.matches(r"\MicrosoftEdgeUpdateTaskMachineCore{X}") && t.matches(r"\microsofteDGEupdatetaskmachinecore"));
    assert!(!t.matches(r"\MicrosoftEdgeUpdateTaskMachineCore{X}\Deeper") && !t.matches(r"\Other"));
    let exact = r(SelectionKind::Task, r"\A\B");
    assert!(exact.matches(r"\a\b") && !exact.matches(r"\A\B\C"));
}

// ------------------------------------------------------------------- resolution

#[test]
fn recommended_resolves_processes_only_with_protections() {
    let r = resolve(&cfg(), ProfileRef::Builtin(Builtin::Recommended), &inventory(), ctx(), &RiskDb::builtin());
    assert_eq!(names(&r), vec!["GameBar.exe", "PhoneExperienceHost.exe"]);
    assert!(r.items.iter().all(|i| i.kind == SelectionKind::Process && i.method == Method::Close));
    assert!(!r.needs_admin);
    // Both Game Bar instances count.
    assert_eq!(r.items[0].pids, vec![10, 11]);
    assert_eq!(r.items[0].ram_bytes, Some(60 * MB));
    assert_eq!(r.total_ram_bytes, 140 * MB);
    assert_eq!(r.items[0].display, "Xbox Game Bar");
    assert!(!r.items[0].why.is_empty() && !r.items[0].side_effect.is_empty() && r.items[0].group == "xbox");
    // Child of the foreground game: left running.
    assert_eq!(skipped(&r, "Widgets.exe"), Some(SkipReason::Foreground));
    // Low-confidence opt-in and optional apps: not even considered by default.
    assert!(skipped(&r, "CrossDeviceResume.exe").is_none());
    assert!(!names(&r).contains(&"Discord.exe") && skipped(&r, "Discord.exe").is_none());
    assert_eq!(r.items[0].confidence, Some(Confidence::Medium));
    assert!(!r.items[0].candidate);
    // Absent items are counted, not listed.
    assert!(r.skipped_counts[&SkipReason::NotPresent] > 0);
    assert!(r.skipped.iter().all(|s| s.reason != SkipReason::NotPresent));
    assert_eq!(r.skipped_counts[&SkipReason::Foreground], 1);
    // Add-ons offered, with presence.
    let d = r.addons.iter().find(|a| a.name == "Discord.exe").unwrap();
    assert!(d.present && !d.enabled && d.ram_bytes == Some(300 * MB) && d.group == "optional");
    let x = r.addons.iter().find(|a| a.name == "CrossDeviceResume.exe").unwrap();
    assert!(x.candidate && x.group == "optin" && x.confidence == Confidence::Low);
    assert!(r.addons.iter().all(|a| a.kind == SelectionKind::Process));
}

#[test]
fn optin_group_brings_in_candidates_with_the_same_protections() {
    let mut c = cfg();
    c.recommended_addon_groups.push("OptIn".into());
    let r = resolve(&c, ProfileRef::Builtin(Builtin::Recommended), &inventory(), ctx(), &RiskDb::builtin());
    // Runs from C:\Windows: protected, like apply.
    assert_eq!(skipped(&r, "CrossDeviceResume.exe"), Some(SkipReason::Protected));
    // Optional apps are a different group.
    assert!(!names(&r).contains(&"Discord.exe"));
    assert!(r.addons.iter().filter(|a| a.group == "optin").all(|a| a.enabled));
    c.validate().unwrap();
    c.recommended_addon_groups.push("everything".into());
    assert!(c.validate().is_err(), "unknown group");
}

#[test]
fn recommended_never_contains_admin_items_even_on_a_full_inventory() {
    let mut c = cfg();
    // Even an add-on reference to a service is ignored by Recommended.
    c.recommended_addons.push(AddonRef::new(SelectionKind::Service, "SysMain"));
    let r = resolve(&c, ProfileRef::Builtin(Builtin::Recommended), &inventory(), ctx(), &RiskDb::builtin());
    assert!(r.items.iter().all(|i| !i.kind.is_admin()));
    assert!(!r.needs_admin);
    assert!(r.selections().iter().all(|s| s.kind == SelectionKind::Process));
}

#[test]
fn recommended_plus_adds_services_and_tasks() {
    let r = resolve(&cfg(), ProfileRef::Builtin(Builtin::RecommendedPlus), &inventory(), ctx(), &RiskDb::builtin());
    let n = names(&r);
    assert!(r.needs_admin);
    for want in [
        "GameBar.exe",
        "DiagTrack",
        "dmwappushservice",
        "Spooler",
        r"\Microsoft\Windows\Defrag\ScheduledDefrag",
        r"\Microsoft\Windows\Customer Experience Improvement Program\Consolidator",
    ] {
        assert!(n.contains(&want), "{want} missing from {n:?}");
    }
    // Opt-in (SysMain, CDPUserSvc, Family Safety, Google tasks), never-recommend (Edge
    // update, Windows Update), never-touch and unlisted items are not used.
    for not in [
        "SysMain",
        "CDPUserSvc_8155c",
        "edgeupdate",
        "wuauserv",
        "RpcSs",
        "MapsBroker",
        "CscService",
        r"\Microsoft\Windows\Shell\FamilySafetyMonitor",
        r"\GoogleUpdateTaskMachineCore{1234-ABCD}",
        r"\MicrosoftEdgeUpdateTaskMachineCore{1234-ABCD}",
        r"\Other\Unrelated",
    ] {
        assert!(!n.contains(&not), "{not} in {n:?}");
    }
    assert_eq!(skipped(&r, "CscService"), Some(SkipReason::NotRunning));
    // Hosted in the foreground game's tree.
    assert_eq!(skipped(&r, "MapsBroker"), Some(SkipReason::Foreground));
    assert_eq!(skipped(&r, r"\Microsoft\Windows\Maps\MapsUpdateTask"), Some(SkipReason::Disabled));
    let diag = r.items.iter().find(|i| i.name == "DiagTrack").unwrap();
    assert_eq!((diag.method, diag.ram_bytes, diag.confidence), (Method::Stop, None, Some(Confidence::High)));
    let wap = r.items.iter().find(|i| i.name == "dmwappushservice").unwrap();
    assert_eq!(wap.ram_bytes, Some(3 * MB));
    let t = r.items.iter().find(|i| i.kind == SelectionKind::Task).unwrap();
    assert_eq!((t.method, t.ram_bytes), (Method::Disable, None));
    // Optional-app add-ons are processes; opt-in add-ons include services and tasks.
    assert!(r.addons.iter().any(|a| a.kind == SelectionKind::Service && a.name == "SysMain" && a.present));
    assert!(r.addons.iter().any(|a| a.kind == SelectionKind::Task && a.present));
}

#[test]
fn optin_services_per_user_instance_and_wildcards() {
    let mut c = cfg();
    c.recommended_addon_groups.push("optin".into());
    let r = resolve(&c, ProfileRef::Builtin(Builtin::RecommendedPlus), &inventory(), ctx(), &RiskDb::builtin());
    let n = names(&r);
    for want in [
        "SysMain",
        "CDPUserSvc_8155c",
        r"\Microsoft\Windows\Shell\FamilySafetyMonitor",
        r"\Microsoft\Windows\Shell\FamilySafetyRefreshTask",
        r"\GoogleUpdateTaskMachineCore{1234-ABCD}",
    ] {
        assert!(n.contains(&want), "{want} missing from {n:?}");
    }
    // The stopped per-user template next to its running instance is not reported.
    assert!(skipped(&r, "CDPUserSvc").is_none());
    let g = r.items.iter().find(|i| i.name.starts_with(r"\GoogleUpdate")).unwrap();
    assert!(g.candidate && g.confidence == Some(Confidence::Low));
    // Never-recommend items stay out even with every add-on on.
    c.recommended_addon_groups.push("optional".into());
    let r = resolve(&c, ProfileRef::Builtin(Builtin::RecommendedPlus), &inventory(), ctx(), &RiskDb::builtin());
    assert!(!names(&r).iter().any(|x| x.contains("Edge") || *x == "wuauserv" || *x == "edgeupdate"));
}

#[test]
fn never_touch_and_dangerous_are_dropped_at_resolution() {
    // A user risk override can veto a built-in item.
    let mut db = RiskDb::builtin();
    db.extend([RiskEntry { name: "DiagTrack".into(), kind: Kind::Service, risk: Risk::Dangerous, summary: "mine".into(), notes: None }]);
    db.extend([RiskEntry { name: "GameBar.exe".into(), kind: Kind::Process, risk: Risk::Dangerous, summary: "mine".into(), notes: None }]);
    let r = resolve(&cfg(), ProfileRef::Builtin(Builtin::RecommendedPlus), &inventory(), ctx(), &db);
    assert_eq!(skipped(&r, "DiagTrack"), Some(SkipReason::Dangerous));
    assert_eq!(skipped(&r, "GameBar.exe"), Some(SkipReason::Dangerous));
    assert!(!names(&r).contains(&"DiagTrack"));
    // A user profile is not filtered by risk, but never-touch still applies.
    let mut c = cfg();
    c.profiles.push(Profile {
        name: "Mine".into(),
        selections: vec![Selection::service("SysMain"), Selection::process("svchost.exe", Method::Kill)],
        ..Default::default()
    });
    // Smuggle a blocked service past config validation to prove resolution re-checks.
    c.profiles[0].selections.push(Selection::service("RpcSs"));
    let r = resolve(&c, lookup(&c, "mine").unwrap(), &inventory(), ctx(), &db);
    assert_eq!(names(&r), vec!["SysMain"]);
    assert_eq!(skipped(&r, "svchost.exe"), Some(SkipReason::Protected));
    assert_eq!(skipped(&r, "RpcSs"), Some(SkipReason::NeverTouch));
    assert!(r.addons.is_empty() && !r.builtin);
}

#[test]
fn self_and_shell_foreground() {
    let mut inv = inventory();
    inv.processes.push(p(40, 4, "Widgets.exe", MB)); // a second instance outside the game tree
    // Self focused: no foreground protection at all.
    let r = resolve(&cfg(), ProfileRef::Builtin(Builtin::Recommended), &inv, Ctx { foreground: Some(SELF), self_pid: SELF }, &RiskDb::builtin());
    let w = r.items.iter().find(|i| i.name == "Widgets.exe").unwrap();
    assert_eq!(w.pids, vec![21, 40]);
    // Game focused: only the instance outside its tree.
    let r = resolve(&cfg(), ProfileRef::Builtin(Builtin::Recommended), &inv, ctx(), &RiskDb::builtin());
    assert_eq!(r.items.iter().find(|i| i.name == "Widgets.exe").unwrap().pids, vec![40]);
}

#[test]
fn addons_are_included_only_when_enabled() {
    let mut c = cfg();
    c.recommended_addons.push(AddonRef::new(SelectionKind::Process, "discord.exe"));
    let r = resolve(&c, ProfileRef::Builtin(Builtin::Recommended), &inventory(), ctx(), &RiskDb::builtin());
    assert!(names(&r).contains(&"Discord.exe"));
    assert!(r.addons.iter().find(|a| a.name == "Discord.exe").unwrap().enabled);
    // Unknown add-on names are ignored.
    c.recommended_addons.push(AddonRef::new(SelectionKind::Process, "nope.exe"));
    let r2 = resolve(&c, ProfileRef::Builtin(Builtin::Recommended), &inventory(), ctx(), &RiskDb::builtin());
    assert_eq!(names(&r), names(&r2));
}

#[test]
fn services_and_tasks_need_the_admin_scan() {
    let mut inv = inventory();
    inv.admin_scanned = false;
    let r = resolve(&cfg(), ProfileRef::Builtin(Builtin::RecommendedPlus), &inv, ctx(), &RiskDb::builtin());
    assert!(r.items.iter().all(|i| i.kind == SelectionKind::Process));
    assert!(r.skipped_counts[&SkipReason::NotScanned] > 0);
}

// ------------------------------------------------------------------- one code path

#[test]
fn preview_equals_toggle_selection() {
    let db = RiskDb::builtin();
    for b in Builtin::ALL {
        let mut c = cfg();
        c.active_profile = Some(b.name().to_lowercase());
        c.recommended_addons.push(AddonRef::new(SelectionKind::Process, "Discord.exe"));
        let mut asked = None;
        let preview = resolve_with(&c, lookup(&c, b.name()).unwrap(), ctx(), &db, |names, admin| {
            asked = Some((names.clone(), admin));
            inventory()
        });
        let (names_asked, admin) = asked.unwrap();
        assert!(names_asked.contains("gamebar.exe") && names_asked.contains("discord.exe"));
        assert_eq!(admin, b.includes_admin(), "{b:?}");
        let (sels, res) = toggle_selections(&c, ctx(), &db, |_, _| inventory());
        assert_eq!(sels, preview.selections(), "{b:?}");
        assert_eq!(res.unwrap(), preview);
        assert!(!sels.is_empty());
    }
}

#[test]
fn user_profiles_toggle_exactly_as_before_without_scanning() {
    let mut c = cfg();
    c.profiles.push(Profile { name: "Gaming".into(), selections: vec![Selection::process("x.exe", Method::Kill)], ..Default::default() });
    c.active_profile = Some("Gaming".into());
    let (sels, res) = toggle_selections(&c, ctx(), &RiskDb::builtin(), |_, _| panic!("must not scan"));
    assert_eq!(sels, vec![Selection::process("x.exe", Method::Kill)]);
    assert!(res.is_none());
    c.active_profile = None;
    c.selections = vec![Selection::process("y.exe", Method::Close)];
    let (sels, _) = toggle_selections(&c, ctx(), &RiskDb::builtin(), |_, _| panic!("must not scan"));
    assert_eq!(sels, c.selections);
}

// ------------------------------------------------------------------- immutability and duplicate

#[test]
fn builtins_are_immutable() {
    assert!(ensure_editable("Recommended").is_err());
    assert!(ensure_editable(" recommended+ ").is_err());
    assert!(ensure_editable("Gaming").is_ok());
    let mut c = cfg();
    c.profiles.push(Profile { name: "Recommended+".into(), ..Default::default() });
    assert!(c.validate().is_err());
    // A built-in can be active and is never stored.
    let c = cfg();
    c.validate().unwrap();
    assert!(c.profiles.is_empty());
    assert!(matches!(active(&c), ProfileRef::Builtin(Builtin::Recommended)));
}

#[test]
fn duplicate_freezes_the_current_resolution() {
    let db = RiskDb::builtin();
    let mut c = cfg();
    let res = resolve(&c, ProfileRef::Builtin(Builtin::RecommendedPlus), &inventory(), ctx(), &db);
    let name = duplicate(&mut c, "Recommended+", None, Some(&res)).unwrap();
    assert_eq!(name, "Recommended+ (copy)");
    let copy = c.find_profile(&name).unwrap().clone();
    assert_eq!(copy.selections, res.selections());
    assert!(!copy.description.is_empty());
    c.validate().unwrap();
    // Later changes on the PC do not change the frozen copy.
    let mut inv = inventory();
    inv.processes.retain(|q| q.name != "GameBar.exe");
    let later = resolve(&c, ProfileRef::Builtin(Builtin::RecommendedPlus), &inv, ctx(), &db);
    assert_ne!(later.selections(), copy.selections);
    assert_eq!(c.find_profile(&name).unwrap().selections, res.selections());
    // Again: unique name. Custom name honoured. A user profile duplicates as is.
    assert_eq!(duplicate(&mut c, "Recommended+", None, Some(&res)).unwrap(), "Recommended+ (copy) (2)");
    assert_eq!(duplicate(&mut c, "recommended", Some("Mine"), Some(&res)).unwrap(), "Mine");
    assert_eq!(duplicate(&mut c, "Mine", None, None).unwrap(), "Mine (copy)");
    assert_eq!(c.find_profile("Mine (copy)").unwrap().selections, c.find_profile("Mine").unwrap().selections);
    assert!(duplicate(&mut c, "Recommended", None, None).is_err(), "built-in without resolution");
    assert!(duplicate(&mut c, "nope", None, None).is_err());
    assert!(duplicate(&mut c, "Mine", Some("Recommended"), None).unwrap() != "Recommended");
}

// ------------------------------------------------------------------- export / import

fn file(name: &str, sels: &str) -> String {
    format!("format = \"holdmyram-profile\"\nversion = 1\nname = \"{name}\"\n{sels}")
}

fn sel_toml(name: &str, kind: &str, method: Option<&str>) -> String {
    let m = method.map(|m| format!("method = \"{m}\"\n")).unwrap_or_default();
    format!("[[selections]]\nname = '{name}'\nkind = \"{kind}\"\n{m}")
}

#[test]
fn import_validation_matrix() {
    let c = {
        let mut c = cfg();
        c.profiles.push(Profile { name: "Gaming".into(), ..Default::default() });
        c
    };
    let err = |text: &str| import_text(text, &c).unwrap_err();

    // Oversize.
    let big = format!("{}# {}\n", file("Big", ""), "x".repeat(MAX_PROFILE_FILE_BYTES as usize));
    assert!(err(&big).contains("larger than"));
    // Format and version.
    assert!(err("version = 1\nname = \"a\"\n").contains("not a HoldMyRAM profile"));
    assert!(err("format = \"other\"\nversion = 1\nname = \"a\"\n").contains("format"));
    assert!(err("format = \"holdmyram-profile\"\nname = \"a\"\n").contains("no version"));
    assert!(err("format = \"holdmyram-profile\"\nversion = 2\nname = \"a\"\nnew_thing = 1\n").contains("newer HoldMyRAM"));
    assert!(err("format = \"holdmyram-profile\"\nversion = 0\nname = \"a\"\n").contains("unsupported"));
    assert!(err("not toml [[[").contains("not a HoldMyRAM profile"));
    // Schema.
    assert!(err(&format!("{}extra = 1\n", file("A", ""))).contains("invalid profile file"));
    assert!(err(&file("A", "[[selections]]\nname = \"a.exe\"\nkind = \"process\"\ncolour = \"red\"\n")).contains("invalid"));
    assert!(err(&file("A", "[[selections]]\nname = \"a.exe\"\n")).contains("invalid"), "kind is required");
    // Unknown kind, wrong method, bad names.
    assert!(err(&file("A", &sel_toml("a", "driver", None))).contains("unknown kind"));
    assert!(err(&file("A", &sel_toml("a.exe", "process", Some("stop")))).contains("must be close, suspend or kill"));
    assert!(err(&file("A", &sel_toml("SysMain", "service", Some("kill")))).contains("must be stop"));
    assert!(err(&file("A", &sel_toml(r"\X", "task", Some("stop")))).contains("must be disable"));
    assert!(err(&file("A", &sel_toml("a.exe", "process", Some("nuke")))).contains("unknown method"));
    assert!(err(&file("A", &sel_toml(r"C:\a.exe", "process", None))).contains("bad process name"));
    assert!(err(&file("A", &sel_toml(r"..\RpcSs", "service", None))).contains("path separator"));
    assert!(err(&file("A", &sel_toml(r"\a\..\b", "task", None))).contains("."));
    // Profile name and description.
    assert!(err(&file("", "")).contains("empty"));
    assert!(err(&file(&"n".repeat(65), "")).contains("longer"));
    assert!(err("format = \"holdmyram-profile\"\nversion = 1\nname = \"a\\u0007\"\n").contains("control"));
    assert!(err(&format!("{}description = \"{}\"\n", file("A", "").replace("name = \"A\"\n", "name = \"A\"\n"), "d".repeat(1001))).contains("description"));

    // Blocked items are dropped and each drop reported; everything else imports.
    let text = file(
        "Gaming",
        &[
            sel_toml("Discord.exe", "process", Some("suspend")),
            sel_toml("lsass.exe", "process", None),
            sel_toml("RpcSs", "service", None),
            sel_toml(r"\Microsoft\Windows\WindowsUpdate\Scheduled Start", "task", None),
            sel_toml("SysMain", "service", None),
            sel_toml(r"\Microsoft\Windows\Defrag\ScheduledDefrag", "task", Some("disable")),
            sel_toml("discord.EXE", "process", Some("close")),
        ]
        .concat(),
    );
    let r = import_text(&text, &c).unwrap();
    // Collision with the existing "Gaming": suffixed, never overwritten.
    assert_eq!(r.profile_name, "Gaming (2)");
    assert_eq!(r.imported_count, 3);
    assert_eq!(
        r.profile.selections,
        vec![
            Selection::process("Discord.exe", Method::Suspend),
            Selection::service("SysMain"),
            Selection::task(r"\Microsoft\Windows\Defrag\ScheduledDefrag"),
        ]
    );
    let dropped: Vec<&str> = r.dropped.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(dropped, vec!["lsass.exe", "RpcSs", r"\Microsoft\Windows\WindowsUpdate\Scheduled Start"]);
    assert!(r.dropped.iter().all(|d| !d.reason.is_empty()));
    assert!(r.warnings.iter().any(|w| w.contains("more than once")));
    assert!(r.warnings.iter().any(|w| w.contains("already exists")));
    // A built-in name collides too.
    assert_eq!(import_text(&file("recommended", ""), &c).unwrap().profile_name, "recommended (2)");
    // Adopting adds, never replaces.
    let mut c2 = c.clone();
    adopt(&mut c2, &r).unwrap();
    assert!(adopt(&mut c2, &r).is_err());
    assert_eq!(c2.profiles.len(), 2);
    c2.validate().unwrap();
}

#[test]
fn export_import_round_trip_on_disk() {
    let dir = crate::journal::tests::temp_dir("profiles-export");
    let mut c = cfg();
    let prof = Profile {
        name: "Work \"quiet\"".into(),
        description: "Line one\nline two".into(),
        selections: vec![
            Selection::process("Spotify.exe", Method::Kill),
            Selection::service("SysMain"),
            Selection::task(r"\Microsoft\Windows\Defrag\ScheduledDefrag"),
        ],
    };
    c.profiles.push(prof.clone());
    let out = export_profile(&c, "work \"QUIET\"", &dir.join("work"), None).unwrap();
    assert_eq!(out, dir.join("work.hmrprofile"));
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains("format = \"holdmyram-profile\"") && text.contains("version = 1"), "{text}");
    // Into a config that does not have it: identical.
    let r = import_file(&out, &cfg()).unwrap();
    assert_eq!(r.profile, prof);
    assert!(r.dropped.is_empty() && r.warnings.is_empty(), "{:?}", r.warnings);
    // Into the same config: suffixed.
    assert_eq!(import_file(&out, &c).unwrap().profile_name, "Work \"quiet\" (2)");

    // A built-in exports as a frozen snapshot.
    let res = resolve(&c, ProfileRef::Builtin(Builtin::RecommendedPlus), &inventory(), ctx(), &RiskDb::builtin());
    let out = export_profile(&c, "Recommended+", &dir.join("snap.hmrprofile"), Some(&res)).unwrap();
    let r = import_file(&out, &c).unwrap();
    assert_eq!(r.profile_name, "Recommended+ snapshot");
    assert_eq!(r.profile.selections, res.selections());
    assert!(r.warnings.iter().any(|w| w.contains("snapshot")));
    assert!(export_profile(&c, "Recommended", &dir.join("x"), None).is_err(), "built-in needs a resolution");
    assert!(export_profile(&c, "missing", &dir.join("x"), None).is_err());

    // Size limit is checked before reading.
    let big = dir.join("big.hmrprofile");
    std::fs::write(&big, vec![b'#'; MAX_PROFILE_FILE_BYTES as usize + 1]).unwrap();
    assert!(import_file(&big, &c).unwrap_err().contains("larger than"));
    assert!(import_file(&dir.join("missing.hmrprofile"), &c).is_err());
    // A UTF-8 BOM is tolerated.
    let bom = dir.join("bom.hmrprofile");
    std::fs::write(&bom, format!("\u{feff}{}", file("Bom", ""))).unwrap();
    assert_eq!(import_file(&bom, &c).unwrap().profile_name, "Bom");
}

/// Read-only look at this PC (nothing is changed): `cargo test -p holdmyram-core
/// smoke_preview_live -- --ignored --nocapture`.
#[test]
#[ignore]
fn smoke_preview_live() {
    let db = RiskDb::builtin();
    let c = cfg();
    for b in Builtin::ALL {
        let r = preview(&c, b.name(), Ctx { foreground: None, self_pid: std::process::id() }, &db).unwrap();
        println!("{}: {} item(s), {} MB, skipped {:?}", r.profile, r.items.len(), r.total_ram_bytes / MB, r.skipped_counts);
        for i in &r.items {
            println!("  {:?} {} ({:?})", i.kind, i.name, i.ram_bytes.map(|b| b / MB));
        }
        for s in &r.skipped {
            println!("  skip {:?} {} [{:?}] {}", s.kind, s.name, s.reason, s.detail);
        }
    }
    println!("printers: {:?}", printers::installed());
}
