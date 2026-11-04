//! In-memory [`AdminBackend`] for tests. Touches no real service or task. Behaves like the
//! SCM where it matters to the handler: names are case-insensitive, a service with
//! running dependents cannot be stopped, a disabled service cannot be started, a locked
//! service or task denies every change.

use super::handler::AdminBackend;
use super::proto::*;
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct FakeService {
    pub name: String,
    pub running: bool,
    pub start: StartType,
    pub delayed_auto: Option<bool>,
    pub service_type: u32,
    pub pid: u32,
    /// Names of services this one depends on.
    pub depends_on: Vec<String>,
    pub locked: bool,
    pub stop_fails: bool,
    pub stop_hangs: bool,
    pub start_fails: bool,
    /// Accepts start-type changes without error but does not apply them.
    pub ignore_config: bool,
}

impl FakeService {
    fn new(name: &str, running: bool, start: StartType) -> FakeService {
        FakeService {
            name: name.into(),
            running,
            start,
            delayed_auto: Some(false),
            service_type: 0x10,
            pid: if running { 1000 } else { 0 },
            depends_on: Vec::new(),
            locked: false,
            stop_fails: false,
            stop_hangs: false,
            start_fails: false,
            ignore_config: false,
        }
    }
    pub fn running(name: &str, start: StartType) -> FakeService {
        FakeService::new(name, true, start)
    }
    pub fn stopped(name: &str, start: StartType) -> FakeService {
        FakeService::new(name, false, start)
    }
    pub fn depends_on(mut self, other: &str) -> FakeService {
        self.depends_on.push(other.into());
        self
    }
    pub fn delayed(mut self, d: Option<bool>) -> FakeService {
        self.delayed_auto = d;
        self
    }
    pub fn locked(mut self) -> FakeService {
        self.locked = true;
        self
    }
    pub fn service_type(mut self, t: u32) -> FakeService {
        self.service_type = t;
        self
    }
    pub fn pid(mut self, pid: u32) -> FakeService {
        self.pid = pid;
        self
    }

    fn state(&self) -> ServiceState {
        ServiceState {
            name: self.name.clone(),
            display: format!("{} (fake)", self.name),
            state: if self.running { RunState::Running } else { RunState::Stopped },
            running: self.running,
            start: self.start,
            delayed_auto: self.delayed_auto,
            service_type: self.service_type,
            pid: if self.running { self.pid } else { 0 },
            protected: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FakeTask {
    pub path: String,
    pub enabled: bool,
    pub locked: bool,
}

impl FakeTask {
    pub fn new(path: &str, enabled: bool) -> FakeTask {
        FakeTask { path: path.into(), enabled, locked: false }
    }
    pub fn locked(mut self) -> FakeTask {
        self.locked = true;
        self
    }
}

#[derive(Debug, Default)]
pub struct FakeBackend {
    services: BTreeMap<String, FakeService>,
    tasks: BTreeMap<String, FakeTask>,
    /// lowercase requested name -> lowercase real name.
    service_aliases: BTreeMap<String, String>,
    task_aliases: BTreeMap<String, String>,
    mutations: Vec<String>,
}

fn key(s: &str) -> String {
    s.to_lowercase()
}

fn e(code: ErrorCode, m: impl Into<String>) -> AdminError {
    AdminError::new(code, m)
}

impl FakeBackend {
    pub fn add(&mut self, s: FakeService) {
        self.services.insert(key(&s.name), s);
    }
    pub fn add_task(&mut self, t: FakeTask) {
        self.tasks.insert(key(&t.path), t);
    }
    /// Requests for `from` resolve to the service `to` (models the SCM resolving names).
    pub fn alias(&mut self, from: &str, to: &str) {
        self.service_aliases.insert(key(from), key(to));
    }
    pub fn alias_task(&mut self, from: &str, to: &str) {
        self.task_aliases.insert(key(from), key(to));
    }
    /// Every change made, in order, as "stop X" / "start X" / "config X Manual" /
    /// "task X false". Refused or failed calls are not listed.
    pub fn mutations(&self) -> Vec<String> {
        self.mutations.clone()
    }
    pub fn service(&self, name: &str) -> &FakeService {
        &self.services[&key(name)]
    }
    pub fn service_mut(&mut self, name: &str) -> &mut FakeService {
        self.services.get_mut(&key(name)).expect("fake service")
    }
    pub fn task(&self, path: &str) -> &FakeTask {
        &self.tasks[&key(path)]
    }
    pub fn set_running(&mut self, name: &str, running: bool) {
        self.service_mut(name).running = running;
    }

    fn resolve(&self, name: &str) -> Result<String, AdminError> {
        let k = key(name);
        let k = self.service_aliases.get(&k).cloned().unwrap_or(k);
        if self.services.contains_key(&k) {
            Ok(k)
        } else {
            Err(e(ErrorCode::NotFound, format!("service \"{name}\" does not exist")))
        }
    }

    fn resolve_task(&self, path: &str) -> Result<String, AdminError> {
        let k = key(path);
        let k = self.task_aliases.get(&k).cloned().unwrap_or(k);
        if self.tasks.contains_key(&k) {
            Ok(k)
        } else {
            Err(e(ErrorCode::NotFound, format!("task \"{path}\" does not exist")))
        }
    }

    /// Direct dependents of service key `k`.
    fn direct_dependents(&self, k: &str) -> Vec<String> {
        self.services
            .iter()
            .filter(|(_, s)| s.depends_on.iter().any(|d| key(d) == k))
            .map(|(dk, _)| dk.clone())
            .collect()
    }

    fn walk(&self, k: &str, seen: &mut Vec<String>, out: &mut Vec<String>) {
        for d in self.direct_dependents(k) {
            if seen.contains(&d) {
                continue;
            }
            seen.push(d.clone());
            self.walk(&d, seen, out);
            out.push(d);
        }
    }
}

impl AdminBackend for FakeBackend {
    fn service_query(&mut self, name: &str) -> Result<ServiceState, AdminError> {
        let k = self.resolve(name)?;
        Ok(self.services[&k].state())
    }

    fn service_dependents(&mut self, name: &str) -> Result<Vec<String>, AdminError> {
        let k = self.resolve(name)?;
        let mut out = Vec::new();
        self.walk(&k, &mut vec![k.clone()], &mut out);
        Ok(out.into_iter().map(|d| self.services[&d].name.clone()).collect())
    }

    fn service_stop(&mut self, name: &str, _timeout: Duration) -> Result<(), AdminError> {
        let k = self.resolve(name)?;
        let running_dep = self.direct_dependents(&k).into_iter().find(|d| self.services[d].running);
        let s = &self.services[&k];
        if s.locked {
            return Err(e(ErrorCode::Locked, format!("access to \"{}\" is denied", s.name)));
        }
        if let Some(d) = running_dep {
            return Err(e(ErrorCode::Failed, format!("dependent \"{d}\" is running")));
        }
        if s.stop_hangs {
            return Err(e(ErrorCode::Timeout, format!("\"{}\" did not stop in time", s.name)));
        }
        if s.stop_fails {
            return Err(e(ErrorCode::Failed, format!("\"{}\" failed to stop", s.name)));
        }
        let name = s.name.clone();
        let s = self.services.get_mut(&k).unwrap();
        if s.running {
            s.running = false;
            self.mutations.push(format!("stop {name}"));
        }
        Ok(())
    }

    fn service_start(&mut self, name: &str, _timeout: Duration) -> Result<(), AdminError> {
        let k = self.resolve(name)?;
        let s = &self.services[&k];
        if s.locked {
            return Err(e(ErrorCode::Locked, format!("access to \"{}\" is denied", s.name)));
        }
        if s.start == StartType::Disabled {
            return Err(e(ErrorCode::Failed, format!("\"{}\" is disabled", s.name)));
        }
        if s.start_fails {
            return Err(e(ErrorCode::Failed, format!("\"{}\" failed to start", s.name)));
        }
        let name = s.name.clone();
        let s = self.services.get_mut(&k).unwrap();
        if !s.running {
            s.running = true;
            self.mutations.push(format!("start {name}"));
        }
        Ok(())
    }

    fn service_set_start(&mut self, name: &str, start: StartChoice) -> Result<(), AdminError> {
        let k = self.resolve(name)?;
        let s = self.services.get_mut(&k).unwrap();
        if s.locked {
            return Err(e(ErrorCode::Locked, format!("access to \"{}\" is denied", s.name)));
        }
        self.mutations.push(format!("config {} {start:?}", s.name));
        if s.ignore_config {
            return Ok(());
        }
        match start {
            StartChoice::Auto => {
                s.start = StartType::Auto;
                s.delayed_auto = Some(false);
            }
            StartChoice::AutoDelayed => {
                s.start = StartType::Auto;
                s.delayed_auto = Some(true);
            }
            StartChoice::Manual => s.start = StartType::Manual,
            StartChoice::Disabled => s.start = StartType::Disabled,
        }
        Ok(())
    }

    fn task_query(&mut self, path: &str) -> Result<TaskState, AdminError> {
        let k = self.resolve_task(path)?;
        let t = &self.tasks[&k];
        Ok(TaskState { path: t.path.clone(), enabled: t.enabled, protected: false })
    }

    fn task_set_enabled(&mut self, path: &str, enabled: bool) -> Result<(), AdminError> {
        let k = self.resolve_task(path)?;
        let t = self.tasks.get_mut(&k).unwrap();
        if t.locked {
            return Err(e(ErrorCode::Locked, format!("access to \"{}\" is denied", t.path)));
        }
        t.enabled = enabled;
        self.mutations.push(format!("task {} {enabled}", t.path));
        Ok(())
    }
}
