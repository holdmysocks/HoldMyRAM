//! Types shared across modules so they can be built independently.

use serde::{Deserialize, Serialize};

/// What a selection names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SelectionKind {
    /// A running process, by executable name. The default, so configs written before
    /// services and tasks existed still load.
    #[default]
    Process,
    /// A Windows service, by its short (key) name, e.g. `SysMain`.
    Service,
    /// A scheduled task, by its full path, e.g. `\Microsoft\Windows\Defrag\ScheduledDefrag`.
    Task,
}

impl SelectionKind {
    /// The method a selection of this kind gets when its config entry names none.
    pub fn default_method(self) -> Method {
        match self {
            SelectionKind::Process => Method::Close,
            SelectionKind::Service => Method::Stop,
            SelectionKind::Task => Method::Disable,
        }
    }

    /// Whether `method` is meaningful for this kind.
    pub fn allows(self, method: Method) -> bool {
        match self {
            SelectionKind::Process => matches!(method, Method::Close | Method::Suspend | Method::Kill),
            SelectionKind::Service => method == Method::Stop,
            SelectionKind::Task => method == Method::Disable,
        }
    }

    /// Needs the elevated helper.
    pub fn is_admin(self) -> bool {
        self != SelectionKind::Process
    }
}

/// How a selected item is dealt with on toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// Process: ask windows to close, then force after the grace period.
    #[default]
    Close,
    /// Process: freeze; its memory stays allocated.
    Suspend,
    /// Process: terminate immediately.
    Kill,
    /// Service: stop now (running dependents first), start again on toggle off. The
    /// start type is never changed.
    Stop,
    /// Task: disable now, enable again on toggle off.
    Disable,
}

/// One user-selected item. `name` is an executable name (Process, case-insensitive), a
/// service short name (Service) or a full task path (Task).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawSelection")]
pub struct Selection {
    pub name: String,
    pub method: Method,
    pub kind: SelectionKind,
}

impl Selection {
    pub fn process(name: impl Into<String>, method: Method) -> Selection {
        Selection { name: name.into(), method, kind: SelectionKind::Process }
    }
    pub fn service(name: impl Into<String>) -> Selection {
        Selection { name: name.into(), method: Method::Stop, kind: SelectionKind::Service }
    }
    pub fn task(path: impl Into<String>) -> Selection {
        Selection { name: path.into(), method: Method::Disable, kind: SelectionKind::Task }
    }
}

/// On-disk / wire form: `kind` and `method` may be missing. A missing kind is Process; a
/// missing method is the kind's default (so `kind = "service"` alone means Stop).
#[derive(Deserialize)]
struct RawSelection {
    name: String,
    #[serde(default)]
    kind: SelectionKind,
    #[serde(default)]
    method: Option<Method>,
}

impl From<RawSelection> for Selection {
    fn from(r: RawSelection) -> Selection {
        Selection { method: r.method.unwrap_or(r.kind.default_method()), name: r.name, kind: r.kind }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_kind_and_method_default_per_kind() {
        let s: Selection = serde_json::from_str(r#"{"name":"a.exe"}"#).unwrap();
        assert_eq!(s, Selection::process("a.exe", Method::Close));
        let s: Selection = serde_json::from_str(r#"{"name":"a.exe","method":"suspend"}"#).unwrap();
        assert_eq!(s.method, Method::Suspend);
        let s: Selection = serde_json::from_str(r#"{"name":"SysMain","kind":"service"}"#).unwrap();
        assert_eq!(s, Selection::service("SysMain"));
        let s: Selection = serde_json::from_str(r#"{"name":"\\X","kind":"task"}"#).unwrap();
        assert_eq!(s, Selection::task(r"\X"));
        // Serialised form always carries both.
        let j = serde_json::to_string(&Selection::service("SysMain")).unwrap();
        assert_eq!(j, r#"{"name":"SysMain","method":"stop","kind":"service"}"#);
    }

    #[test]
    fn kind_method_compatibility() {
        use Method::*;
        for m in [Close, Suspend, Kill] {
            assert!(SelectionKind::Process.allows(m));
            assert!(!SelectionKind::Service.allows(m));
            assert!(!SelectionKind::Task.allows(m));
        }
        assert!(SelectionKind::Service.allows(Stop) && !SelectionKind::Service.allows(Disable));
        assert!(SelectionKind::Task.allows(Disable) && !SelectionKind::Task.allows(Stop));
        assert!(!SelectionKind::Process.allows(Stop) && !SelectionKind::Process.allows(Disable));
    }
}
