//! The helper's request handler. It trusts nothing: every request is parsed with the
//! strict schema, every name re-validated, and every target checked against the
//! never-touch rules both by the requested name and by what the backend reports (the
//! canonical name, the service type, the hosting PID, the canonical task path), before
//! anything is changed. All Windows access goes through [`AdminBackend`], so the whole
//! handler is tested against an in-memory fake.

use super::proto::*;
use super::rules;
use std::time::{Duration, Instant};

/// Everything the handler needs from Windows. Implementations do exactly what they are
/// asked and nothing else: the safety rules live in the handler, not here.
pub trait AdminBackend {
    /// Current state of one service. `protected` is ignored (the handler sets it).
    fn service_query(&mut self, name: &str) -> Result<ServiceState, AdminError>;
    /// Names of every service that depends on `name`, directly or indirectly, in any
    /// state, in a safe STOP order (a service always comes before what it depends on).
    fn service_dependents(&mut self, name: &str) -> Result<Vec<String>, AdminError>;
    /// Send a stop request and wait until stopped or `timeout`.
    fn service_stop(&mut self, name: &str, timeout: Duration) -> Result<(), AdminError>;
    /// Start and wait until running or `timeout`.
    fn service_start(&mut self, name: &str, timeout: Duration) -> Result<(), AdminError>;
    fn service_set_start(&mut self, name: &str, start: StartChoice) -> Result<(), AdminError>;
    /// Current state of one task. `protected` is ignored (the handler sets it).
    fn task_query(&mut self, path: &str) -> Result<TaskState, AdminError>;
    fn task_set_enabled(&mut self, path: &str, enabled: bool) -> Result<(), AdminError>;
}

#[derive(Debug, Clone)]
pub struct Policy {
    /// Processes whose services must never be touched: the launcher and the helper.
    pub protected_pids: Vec<u32>,
    /// Wait per service stop or start.
    pub service_timeout: Duration,
    /// Total time one ServiceStop may spend, dependents included.
    pub stop_budget: Duration,
    /// Refuse to stop a service with more running dependents than this.
    pub max_dependents: usize,
}

impl Policy {
    pub fn new(protected_pids: Vec<u32>) -> Policy {
        Policy {
            protected_pids,
            service_timeout: Duration::from_secs(20),
            stop_budget: Duration::from_secs(60),
            max_dependents: 16,
        }
    }
}

pub struct Handler<B: AdminBackend> {
    backend: B,
    policy: Policy,
}

fn err(code: ErrorCode, m: impl Into<String>) -> AdminError {
    AdminError::new(code, m)
}

impl<B: AdminBackend> Handler<B> {
    pub fn new(backend: B, policy: Policy) -> Self {
        Handler { backend, policy }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Handle one raw request line. Returns the response and whether the helper should
    /// exit after sending it.
    pub fn handle_line(&mut self, line: &[u8]) -> (Response, bool) {
        match parse_request(line) {
            Err((id, e)) => (Response::from_result(id, Err(e)), false),
            Ok(req) => {
                let shutdown = req.op == Op::Shutdown;
                (Response::from_result(req.id, self.handle(&req.op)), shutdown)
            }
        }
    }

    /// Handle one parsed request. Validates again (cheap) so this entry point is safe too.
    pub fn handle(&mut self, op: &Op) -> Result<Reply, AdminError> {
        op.validate().map_err(|e| err(ErrorCode::BadRequest, e))?;
        match op {
            Op::Ping => Ok(Reply::Pong { version: PROTOCOL_VERSION, pid: std::process::id() }),
            Op::Shutdown => Ok(Reply::Bye),
            Op::ServiceQuery { name } => {
                let mut s = self.backend.service_query(name)?;
                s.protected = self.service_refusal(name, &s).is_some();
                Ok(Reply::Service(s))
            }
            Op::ServiceStop { name } => self.stop(name).map(Reply::Stopped),
            Op::ServiceStart { name } => self.start(name).map(Reply::Started),
            Op::ServiceSetStartType { name, start } => self.set_start(name, *start).map(Reply::StartTypeSet),
            Op::TaskQuery { path } => {
                let mut t = self.backend.task_query(path)?;
                t.protected = rules::never_touch_task(path).is_some() || rules::never_touch_task(&t.path).is_some();
                Ok(Reply::Task(t))
            }
            Op::TaskSetEnabled { path, enabled } => self.set_task(path, *enabled).map(Reply::TaskSet),
        }
    }

    // -----------------------------------------------------------------------------------
    // Rules
    // -----------------------------------------------------------------------------------

    /// Why this service must not be changed, if it must not. Checks the requested name,
    /// the name the backend reported, the service type and the hosting PID.
    fn service_refusal(&self, requested: &str, s: &ServiceState) -> Option<String> {
        for n in [requested, s.name.as_str()] {
            if let Some(hit) = rules::never_touch_service(n) {
                return Some(format!("\"{n}\" is on the never-touch list ({hit})"));
            }
        }
        if rules::is_driver_type(s.service_type) {
            return Some(format!("\"{}\" is a driver (type {:#x})", s.name, s.service_type));
        }
        if matches!(s.start, StartType::Boot | StartType::System) {
            return Some(format!("\"{}\" is a boot/system-start service", s.name));
        }
        if s.pid != 0 && self.policy.protected_pids.contains(&s.pid) {
            return Some(format!("\"{}\" runs in HoldMyRAM's own process", s.name));
        }
        None
    }

    /// Name-only check, before asking the backend anything.
    fn name_refusal(name: &str) -> Result<(), AdminError> {
        match rules::never_touch_service(name) {
            Some(hit) => Err(err(ErrorCode::NeverTouch, format!("\"{name}\" is on the never-touch list ({hit})"))),
            None => Ok(()),
        }
    }

    fn checked_query(&mut self, name: &str) -> Result<ServiceState, AdminError> {
        Self::name_refusal(name)?;
        let mut s = self.backend.service_query(name)?;
        if let Some(why) = self.service_refusal(name, &s) {
            return Err(err(ErrorCode::NeverTouch, why));
        }
        s.protected = false;
        Ok(s)
    }

    /// Every dependent of `name` (stop order) with its state, refusing if any of them
    /// is protected and `only_running` is false or it is running.
    fn checked_dependents(&mut self, name: &str, only_running: bool) -> Result<Vec<ServiceState>, AdminError> {
        let names = self.backend.service_dependents(name)?;
        let mut out = Vec::new();
        for d in names {
            if let Some(hit) = rules::never_touch_service(&d) {
                // Don't even need its state to know: but a stopped one is fine for Stop.
                if !only_running {
                    return Err(err(
                        ErrorCode::ProtectedDependent,
                        format!("\"{d}\" ({hit}) depends on \"{name}\" and is on the never-touch list"),
                    ));
                }
            }
            let s = match self.backend.service_query(&d) {
                Ok(s) => s,
                Err(e) if e.code == ErrorCode::NotFound => continue, // deleted meanwhile
                Err(e) => {
                    return Err(err(
                        ErrorCode::ProtectedDependent,
                        format!("could not check \"{d}\", which depends on \"{name}\": {}", e.message),
                    ))
                }
            };
            if only_running && !s.running {
                continue;
            }
            if let Some(why) = self.service_refusal(&d, &s) {
                return Err(err(ErrorCode::ProtectedDependent, format!("{why}, and it depends on \"{name}\"")));
            }
            out.push(s);
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------------------
    // Services
    // -----------------------------------------------------------------------------------

    fn stop(&mut self, name: &str) -> Result<StopReport, AdminError> {
        let before = self.checked_query(name)?;
        if !before.running {
            return Ok(StopReport { before, stopped: Vec::new() });
        }
        // All running dependents must pass the same checks BEFORE anything is stopped.
        let deps = self.checked_dependents(name, true)?;
        if deps.len() > self.policy.max_dependents {
            return Err(err(
                ErrorCode::ProtectedDependent,
                format!("\"{name}\" has {} running dependents; refusing to stop that many", deps.len()),
            ));
        }
        let deadline = Instant::now() + self.policy.stop_budget;
        let mut stopped: Vec<String> = Vec::new();
        let targets: Vec<String> = deps.iter().map(|d| d.name.clone()).chain(std::iter::once(name.to_string())).collect();
        for t in &targets {
            let left = deadline.saturating_duration_since(Instant::now());
            let r = if left.is_zero() {
                Err(err(ErrorCode::Timeout, format!("ran out of time before stopping \"{t}\"")))
            } else {
                self.backend.service_stop(t, left.min(self.policy.service_timeout))
            };
            if let Err(mut e) = r {
                e.stopped = self.rollback(&stopped);
                if !stopped.is_empty() {
                    e.message = format!(
                        "{} (stopping \"{t}\" failed; restarted {} of {} dependents stopped before it)",
                        e.message,
                        stopped.len() - e.stopped.len(),
                        stopped.len()
                    );
                }
                return Err(e);
            }
            stopped.push(t.clone());
        }
        Ok(StopReport { before, stopped })
    }

    /// Start `stopped` again in reverse order, best effort. Returns those still stopped.
    fn rollback(&mut self, stopped: &[String]) -> Vec<String> {
        let mut left = Vec::new();
        for s in stopped.iter().rev() {
            if self.backend.service_start(s, self.policy.service_timeout).is_err() {
                left.push(s.clone());
            }
        }
        left.reverse();
        left
    }

    fn start(&mut self, name: &str) -> Result<StartReport, AdminError> {
        let before = self.checked_query(name)?;
        if before.running {
            return Ok(StartReport { before, started: false });
        }
        self.backend.service_start(name, self.policy.service_timeout)?;
        Ok(StartReport { before, started: true })
    }

    fn set_start(&mut self, name: &str, choice: StartChoice) -> Result<StartTypeReport, AdminError> {
        let before = self.checked_query(name)?;
        if choice.matches(before.start, before.delayed_auto) {
            return Ok(StartTypeReport { after: before.clone(), before });
        }
        if choice == StartChoice::Disabled {
            // A protected service that depends on this one would fail to start later.
            self.checked_dependents(name, false)?;
        }
        self.backend.service_set_start(name, choice)?;
        let mut after = self.backend.service_query(name)?;
        after.protected = false;
        if !choice.matches(after.start, after.delayed_auto) {
            return Err(err(
                ErrorCode::Failed,
                format!(
                    "\"{name}\" reads back as {:?} (delayed {:?}) after setting {choice:?}",
                    after.start, after.delayed_auto
                ),
            ));
        }
        Ok(StartTypeReport { before, after })
    }

    // -----------------------------------------------------------------------------------
    // Tasks
    // -----------------------------------------------------------------------------------

    fn set_task(&mut self, path: &str, enabled: bool) -> Result<TaskReport, AdminError> {
        if let Some(f) = rules::never_touch_task(path) {
            return Err(err(ErrorCode::NeverTouch, format!("\"{path}\" is under a never-touch folder ({f})")));
        }
        let mut before = self.backend.task_query(path)?;
        // The scheduler resolved the path; check what it actually resolved to.
        if let Some(f) = rules::never_touch_task(&before.path) {
            return Err(err(ErrorCode::NeverTouch, format!("\"{}\" is under a never-touch folder ({f})", before.path)));
        }
        if rules::validate_task_path(&before.path).is_err() {
            return Err(err(ErrorCode::NeverTouch, format!("\"{path}\" resolved to an unexpected path \"{}\"", before.path)));
        }
        before.protected = false;
        if before.enabled == enabled {
            return Ok(TaskReport { after: before.clone(), before });
        }
        self.backend.task_set_enabled(&before.path, enabled)?;
        let mut after = self.backend.task_query(&before.path)?;
        after.protected = false;
        if after.enabled != enabled {
            return Err(err(ErrorCode::Failed, format!("\"{}\" did not change state", before.path)));
        }
        Ok(TaskReport { before, after })
    }
}

#[cfg(test)]
mod tests {
    use super::super::fake::{FakeBackend, FakeService, FakeTask};
    use super::*;

    fn handler(b: FakeBackend) -> Handler<FakeBackend> {
        Handler::new(b, Policy::new(vec![777, 778]))
    }

    fn sample() -> FakeBackend {
        let mut b = FakeBackend::default();
        b.add(FakeService::running("Target", StartType::Auto));
        b.add(FakeService::running("DepA", StartType::Manual).depends_on("Target"));
        b.add(FakeService::running("DepB", StartType::Manual).depends_on("DepA"));
        b.add(FakeService::stopped("DepStopped", StartType::Manual).depends_on("Target"));
        b.add(FakeService::running("SysMain", StartType::Auto).delayed(Some(true)));
        b.add(FakeService::running("RpcSs", StartType::Auto));
        b.add(FakeService::running("Base", StartType::Auto));
        b.add(FakeService::running("AudioSrv", StartType::Auto).depends_on("Base"));
        b.add(FakeService::stopped("Base2", StartType::Manual));
        b.add(FakeService::stopped("Winmgmt", StartType::Auto).depends_on("Base2"));
        b.add(FakeService::running("CodexSandboxService.OpenAI.Codex", StartType::Auto).locked());
        b.add(FakeService::running("SomeDriver", StartType::Manual).service_type(0x1));
        b.add(FakeService::running("InOurProcess", StartType::Manual).pid(777));
        b.add(FakeService::running("CDPUserSvc_8155c", StartType::Auto).service_type(0xE0));
        b.add(FakeService::running("AudioSrv_1a2b3", StartType::Auto).service_type(0xE0));
        b.add_task(FakeTask::new(r"\Microsoft\Windows\Defrag\ScheduledDefrag", true));
        b.add_task(FakeTask::new(r"\Microsoft\Windows\Windows Defender\Windows Defender Scheduled Scan", true));
        b.add_task(FakeTask::new(r"\Locked\Task", true).locked());
        b
    }

    fn line(h: &mut Handler<FakeBackend>, s: &str) -> Response {
        h.handle_line(s.as_bytes()).0
    }

    #[test]
    fn ping_and_shutdown() {
        let mut h = handler(sample());
        let (r, bye) = h.handle_line(br#"{"id":1,"op":"Ping"}"#);
        assert!(!bye);
        assert!(matches!(r.into_result().unwrap(), Reply::Pong { version: PROTOCOL_VERSION, .. }));
        let (r, bye) = h.handle_line(br#"{"id":2,"op":"Shutdown"}"#);
        assert!(bye);
        assert_eq!(r.id, 2);
        assert_eq!(r.into_result().unwrap(), Reply::Bye);
        // Bad requests never shut down and never touch the backend.
        let (r, bye) = h.handle_line(br#"{"id":3,"op":"Explode"}"#);
        assert!(!bye);
        assert_eq!(r.into_result().unwrap_err().code, ErrorCode::BadRequest);
        assert!(h.backend().mutations().is_empty());
    }

    #[test]
    fn stop_stops_running_dependents_first_and_reports_them() {
        let mut h = handler(sample());
        let r = h.handle(&Op::ServiceStop { name: "target".into() }).unwrap();
        let Reply::Stopped(rep) = r else { panic!("{r:?}") };
        assert!(rep.before.running);
        assert_eq!(rep.before.start, StartType::Auto);
        assert_eq!(rep.before.name, "Target", "canonical name reported");
        assert_eq!(rep.stopped, vec!["DepB", "DepA", "target"], "dependents of dependents first, target (as requested) last");
        for s in ["Target", "DepA", "DepB"] {
            assert!(!h.backend().service(s).running, "{s}");
        }
        assert_eq!(h.backend().mutations(), vec!["stop DepB", "stop DepA", "stop Target"]);
        // Start type untouched by a stop.
        assert_eq!(h.backend().service("Target").start, StartType::Auto);

        // Restore = start in reverse order of `stopped`.
        for s in rep.stopped.iter().rev() {
            let Reply::Started(st) = h.handle(&Op::ServiceStart { name: s.clone() }).unwrap() else { panic!() };
            assert!(st.started);
            assert!(!st.before.running);
        }
        for s in ["Target", "DepA", "DepB"] {
            assert!(h.backend().service(s).running, "{s}");
        }
        // Stopping something already stopped changes nothing.
        let mut h = handler(sample());
        let Reply::Stopped(rep) = h.handle(&Op::ServiceStop { name: "DepStopped".into() }).unwrap() else { panic!() };
        assert!(rep.stopped.is_empty() && !rep.before.running);
        assert!(h.backend().mutations().is_empty());
    }

    #[test]
    fn never_touch_is_refused_in_every_spelling() {
        let mut h = handler(sample());
        for name in ["RpcSs", "rpcss", "RPCSS", "AudioSrv_1a2b3", "winmgmt"] {
            for op in [
                Op::ServiceStop { name: name.into() },
                Op::ServiceStart { name: name.into() },
                Op::ServiceSetStartType { name: name.into(), start: StartChoice::Disabled },
                Op::ServiceSetStartType { name: name.into(), start: StartChoice::Auto },
            ] {
                let e = h.handle(&op).unwrap_err();
                assert_eq!(e.code, ErrorCode::NeverTouch, "{op:?}");
            }
        }
        // Query is read-only: allowed, and it says the service is protected.
        let Reply::Service(s) = h.handle(&Op::ServiceQuery { name: "rpcss".into() }).unwrap() else { panic!() };
        assert!(s.protected);
        let Reply::Service(s) = h.handle(&Op::ServiceQuery { name: "SysMain".into() }).unwrap() else { panic!() };
        assert!(!s.protected);
        assert!(h.backend().mutations().is_empty());
    }

    #[test]
    fn drivers_own_process_and_canonical_names_are_refused() {
        let mut b = sample();
        // The backend resolves an innocent-looking alias to a protected service.
        b.alias("Innocent", "RpcSs");
        let mut h = handler(b);
        for name in ["SomeDriver", "InOurProcess", "Innocent"] {
            let e = h.handle(&Op::ServiceStop { name: name.into() }).unwrap_err();
            assert_eq!(e.code, ErrorCode::NeverTouch, "{name}: {e}");
            let e = h.handle(&Op::ServiceSetStartType { name: name.into(), start: StartChoice::Manual }).unwrap_err();
            assert_eq!(e.code, ErrorCode::NeverTouch, "{name}: {e}");
        }
        assert!(h.backend().mutations().is_empty());
        // A per-user instance of an ordinary template is fine.
        assert!(h.handle(&Op::ServiceStop { name: "CDPUserSvc_8155c".into() }).is_ok());
    }

    #[test]
    fn running_protected_dependent_blocks_stop_before_anything_happens() {
        let mut h = handler(sample());
        let e = h.handle(&Op::ServiceStop { name: "Base".into() }).unwrap_err();
        assert_eq!(e.code, ErrorCode::ProtectedDependent, "{e}");
        assert!(e.message.contains("AudioSrv"));
        assert!(h.backend().mutations().is_empty());
        assert!(h.backend().service("Base").running);

        // A STOPPED protected dependent does not block a stop...
        h.backend_mut().set_running("Base2", true);
        assert!(h.handle(&Op::ServiceStop { name: "Base2".into() }).is_ok());
        // ...but does block Disabled (it would fail to start later). Manual is fine.
        let e = h.handle(&Op::ServiceSetStartType { name: "Base2".into(), start: StartChoice::Disabled }).unwrap_err();
        assert_eq!(e.code, ErrorCode::ProtectedDependent);
        assert!(h.handle(&Op::ServiceSetStartType { name: "Base2".into(), start: StartChoice::Auto }).is_ok());
    }

    #[test]
    fn locked_service_reports_locked() {
        let mut h = handler(sample());
        let name = "CodexSandboxService.OpenAI.Codex";
        for op in [
            Op::ServiceStop { name: name.into() },
            Op::ServiceSetStartType { name: name.into(), start: StartChoice::Disabled },
        ] {
            let e = h.handle(&op).unwrap_err();
            assert_eq!(e.code, ErrorCode::Locked, "{op:?}");
        }
        assert!(h.backend().service(name).running);
        // Same over the wire.
        let r = line(&mut h, r#"{"id":9,"op":{"ServiceStop":{"name":"CodexSandboxService.OpenAI.Codex"}}}"#);
        assert_eq!(r.id, 9);
        assert!(r.to_line().contains(r#""code":"locked""#));
    }

    #[test]
    fn partial_stop_failure_rolls_back_and_reports_leftovers() {
        let mut b = sample();
        b.service_mut("Target").stop_fails = true;
        let mut h = handler(b);
        let e = h.handle(&Op::ServiceStop { name: "Target".into() }).unwrap_err();
        assert_eq!(e.code, ErrorCode::Failed);
        assert!(e.stopped.is_empty(), "rollback restarted both dependents");
        assert!(h.backend().service("DepA").running && h.backend().service("DepB").running);
        assert_eq!(h.backend().mutations(), vec!["stop DepB", "stop DepA", "start DepA", "start DepB"]);

        // When the rollback itself fails, the leftovers are reported for journaling.
        let mut b = sample();
        b.service_mut("Target").stop_fails = true;
        b.service_mut("DepB").start_fails = true;
        let mut h = handler(b);
        let e = h.handle(&Op::ServiceStop { name: "Target".into() }).unwrap_err();
        assert_eq!(e.stopped, vec!["DepB"]);
        assert!(e.message.contains("restarted 1 of 2"), "{}", e.message);
    }

    #[test]
    fn stop_timeout_is_reported() {
        let mut b = sample();
        b.service_mut("SysMain").stop_hangs = true;
        let mut h = handler(b);
        let e = h.handle(&Op::ServiceStop { name: "SysMain".into() }).unwrap_err();
        assert_eq!(e.code, ErrorCode::Timeout);
        assert!(e.stopped.is_empty());
    }

    #[test]
    fn too_many_dependents_is_refused() {
        let mut b = FakeBackend::default();
        b.add(FakeService::running("Hub", StartType::Auto));
        for i in 0..17 {
            b.add(FakeService::running(&format!("Leaf{i}"), StartType::Manual).depends_on("Hub"));
        }
        let mut h = handler(b);
        let e = h.handle(&Op::ServiceStop { name: "Hub".into() }).unwrap_err();
        assert_eq!(e.code, ErrorCode::ProtectedDependent);
        assert!(h.backend().mutations().is_empty());
    }

    #[test]
    fn start_type_round_trip_keeps_delayed_auto() {
        let mut h = handler(sample());
        let Reply::StartTypeSet(rep) = h.handle(&Op::ServiceSetStartType { name: "SysMain".into(), start: StartChoice::Disabled }).unwrap()
        else {
            panic!()
        };
        assert_eq!((rep.before.start, rep.before.delayed_auto), (StartType::Auto, Some(true)));
        assert_eq!(rep.after.start, StartType::Disabled);
        // Running state is untouched by a start-type change.
        assert!(rep.after.running);

        let back = rep.before.start.to_choice(rep.before.delayed_auto).unwrap();
        assert_eq!(back, StartChoice::AutoDelayed);
        let Reply::StartTypeSet(rep2) = h.handle(&Op::ServiceSetStartType { name: "SysMain".into(), start: back }).unwrap() else { panic!() };
        assert_eq!(rep2.before.start, StartType::Disabled);
        assert_eq!((rep2.after.start, rep2.after.delayed_auto), (StartType::Auto, Some(true)));

        // Plain Auto clears the delayed flag.
        let Reply::StartTypeSet(rep3) = h.handle(&Op::ServiceSetStartType { name: "SysMain".into(), start: StartChoice::Auto }).unwrap() else { panic!() };
        assert_eq!(rep3.after.delayed_auto, Some(false));

        // Asking for the current state writes nothing.
        let n = h.backend().mutations().len();
        h.handle(&Op::ServiceSetStartType { name: "SysMain".into(), start: StartChoice::Auto }).unwrap();
        assert_eq!(h.backend().mutations().len(), n);
    }

    #[test]
    fn start_type_that_does_not_stick_is_an_error() {
        let mut b = sample();
        b.service_mut("SysMain").ignore_config = true;
        let mut h = handler(b);
        let e = h.handle(&Op::ServiceSetStartType { name: "SysMain".into(), start: StartChoice::Manual }).unwrap_err();
        assert_eq!(e.code, ErrorCode::Failed);
    }

    #[test]
    fn task_enable_round_trip() {
        let mut h = handler(sample());
        let p = r"\microsoft\windows\defrag\scheduleddefrag";
        let Reply::TaskSet(rep) = h.handle(&Op::TaskSetEnabled { path: p.into(), enabled: false }).unwrap() else { panic!() };
        assert!(rep.before.enabled && !rep.after.enabled);
        assert_eq!(rep.before.path, r"\Microsoft\Windows\Defrag\ScheduledDefrag", "canonical path reported");
        let Reply::TaskSet(rep) = h.handle(&Op::TaskSetEnabled { path: p.into(), enabled: true }).unwrap() else { panic!() };
        assert!(!rep.before.enabled && rep.after.enabled);
        // Already in that state: no write.
        let n = h.backend().mutations().len();
        h.handle(&Op::TaskSetEnabled { path: p.into(), enabled: true }).unwrap();
        assert_eq!(h.backend().mutations().len(), n);
        let Reply::Task(t) = h.handle(&Op::TaskQuery { path: p.into() }).unwrap() else { panic!() };
        assert!(t.enabled && !t.protected);
    }

    #[test]
    fn protected_tasks_are_refused_including_via_resolution() {
        let mut b = sample();
        b.alias_task(r"\Sneaky\Alias", r"\Microsoft\Windows\Windows Defender\Windows Defender Scheduled Scan");
        let mut h = handler(b);
        for p in [
            r"\Microsoft\Windows\Windows Defender\Windows Defender Scheduled Scan",
            r"\MICROSOFT\WINDOWS\WINDOWS DEFENDER\Windows Defender Scheduled Scan",
            r"\Sneaky\Alias",
        ] {
            let e = h.handle(&Op::TaskSetEnabled { path: p.into(), enabled: false }).unwrap_err();
            assert_eq!(e.code, ErrorCode::NeverTouch, "{p}");
        }
        let Reply::Task(t) = h.handle(&Op::TaskQuery { path: r"\Sneaky\Alias".into() }).unwrap() else { panic!() };
        assert!(t.protected);
        let e = h.handle(&Op::TaskSetEnabled { path: r"\Locked\Task".into(), enabled: false }).unwrap_err();
        assert_eq!(e.code, ErrorCode::Locked);
        let e = h.handle(&Op::TaskSetEnabled { path: r"\Nope".into(), enabled: false }).unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        assert!(h.backend().mutations().is_empty());
    }

    #[test]
    fn handle_revalidates_parsed_ops() {
        let mut h = handler(sample());
        for op in [Op::ServiceStop { name: r"..\RpcSs".into() }, Op::TaskQuery { path: "nope".into() }] {
            assert_eq!(h.handle(&op).unwrap_err().code, ErrorCode::BadRequest);
        }
    }
}
