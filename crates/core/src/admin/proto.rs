//! Wire protocol between the app (pipe server) and the elevated helper (pipe client).
//!
//! Newline-delimited JSON, one request then one response, matched by `id`. The schema is
//! strict: unknown ops, unknown or duplicate fields, missing fields and wrong types are
//! rejected, lines over [`MAX_REQUEST_LINE`] bytes are rejected, and every name/path is
//! validated against [`super::rules`] while parsing. Examples:
//!
//! ```text
//! -> {"id":1,"op":"Ping"}
//! <- {"id":1,"ok":{"Pong":{"version":1,"pid":4242}}}
//! -> {"id":2,"op":{"ServiceStop":{"name":"SysMain"}}}
//! <- {"id":2,"ok":{"Stopped":{"before":{...},"stopped":["SysMain"]}}}
//! -> {"id":3,"op":{"ServiceStop":{"name":"RpcSs"}}}
//! <- {"id":3,"err":{"code":"never_touch","message":"..."}}
//! ```

use super::rules;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
/// Longest request line the helper accepts (bytes, without the newline).
pub const MAX_REQUEST_LINE: usize = 4096;
/// Longest response line the app accepts.
pub const MAX_RESPONSE_LINE: usize = 256 * 1024;
/// Longest handshake line either side accepts.
pub const MAX_HANDSHAKE_LINE: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: u64,
    pub op: Op,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Op {
    Ping,
    ServiceStop { name: String },
    ServiceStart { name: String },
    ServiceSetStartType { name: String, start: StartChoice },
    ServiceQuery { name: String },
    TaskSetEnabled { path: String, enabled: bool },
    TaskQuery { path: String },
    Shutdown,
}

impl Op {
    /// Schema-level validation of names and paths (rules.rs). The never-touch lists are
    /// checked later by the handler, which also reports them with their own error code.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Op::Ping | Op::Shutdown => Ok(()),
            Op::ServiceStop { name }
            | Op::ServiceStart { name }
            | Op::ServiceSetStartType { name, .. }
            | Op::ServiceQuery { name } => rules::validate_service_name(name),
            Op::TaskSetEnabled { path, .. } | Op::TaskQuery { path } => rules::validate_task_path(path),
        }
    }

    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            Op::ServiceStop { .. } | Op::ServiceStart { .. } | Op::ServiceSetStartType { .. } | Op::TaskSetEnabled { .. }
        )
    }
}

/// Start type a request may ask for. `AutoDelayed` is automatic with the delayed-start
/// flag; `Auto` clears that flag. Manual/Disabled leave the flag alone (Windows ignores
/// it for non-automatic services).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartChoice {
    Auto,
    AutoDelayed,
    Manual,
    Disabled,
}

/// Start type as read from the SCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartType {
    Boot,
    System,
    Auto,
    Manual,
    Disabled,
    /// The configuration could not be read.
    Unknown,
}

impl StartType {
    pub fn from_raw(v: u32) -> StartType {
        match v {
            0 => StartType::Boot,
            1 => StartType::System,
            2 => StartType::Auto,
            3 => StartType::Manual,
            4 => StartType::Disabled,
            _ => StartType::Unknown,
        }
    }

    /// The request that puts a service back into this state, if there is one.
    pub fn to_choice(self, delayed_auto: Option<bool>) -> Option<StartChoice> {
        match self {
            StartType::Auto if delayed_auto == Some(true) => Some(StartChoice::AutoDelayed),
            StartType::Auto => Some(StartChoice::Auto),
            StartType::Manual => Some(StartChoice::Manual),
            StartType::Disabled => Some(StartChoice::Disabled),
            StartType::Boot | StartType::System | StartType::Unknown => None,
        }
    }
}

impl StartChoice {
    /// Whether a service read back as (start, delayed) is in this state. An unreadable
    /// delayed flag is accepted for Auto/AutoDelayed (it cannot be verified either way).
    pub fn matches(self, start: StartType, delayed_auto: Option<bool>) -> bool {
        match self {
            StartChoice::Auto => start == StartType::Auto && delayed_auto != Some(true),
            StartChoice::AutoDelayed => start == StartType::Auto && delayed_auto != Some(false),
            StartChoice::Manual => start == StartType::Manual,
            StartChoice::Disabled => start == StartType::Disabled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    Stopped,
    StartPending,
    StopPending,
    Running,
    ContinuePending,
    PausePending,
    Paused,
    Unknown,
}

impl RunState {
    pub fn from_raw(v: u32) -> RunState {
        match v {
            1 => RunState::Stopped,
            2 => RunState::StartPending,
            3 => RunState::StopPending,
            4 => RunState::Running,
            5 => RunState::ContinuePending,
            6 => RunState::PausePending,
            7 => RunState::Paused,
            _ => RunState::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceState {
    /// Service key name (canonical spelling when the SCM could tell us).
    pub name: String,
    pub display: String,
    pub state: RunState,
    /// Anything but Stopped.
    pub running: bool,
    pub start: StartType,
    /// Delayed automatic start flag; None when it could not be read.
    pub delayed_auto: Option<bool>,
    pub service_type: u32,
    pub pid: u32,
    /// The helper will refuse to change this service (never-touch list, driver, or the
    /// helper's own launcher). Filled in by the handler, never by a backend.
    pub protected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskState {
    /// Full task path as registered (canonical spelling).
    pub path: String,
    pub enabled: bool,
    /// Under a never-touch folder. Filled in by the handler.
    pub protected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopReport {
    pub before: ServiceState,
    /// Every service this request stopped, in the order it stopped them (running
    /// dependents first, the requested service last). Restore = start in reverse order.
    /// Empty when the service was already stopped.
    pub stopped: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartReport {
    pub before: ServiceState,
    /// False when it was already running.
    pub started: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartTypeReport {
    pub before: ServiceState,
    pub after: ServiceState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskReport {
    pub before: TaskState,
    pub after: TaskState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Reply {
    Pong { version: u32, pid: u32 },
    Service(ServiceState),
    Stopped(StopReport),
    Started(StartReport),
    StartTypeSet(StartTypeReport),
    Task(TaskState),
    TaskSet(TaskReport),
    Bye,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed request: schema, size, bad name or path.
    BadRequest,
    /// On a never-touch list, a driver, or the launcher itself.
    NeverTouch,
    /// Stopping/disabling it would also hit a protected service that depends on it.
    ProtectedDependent,
    NotFound,
    /// Windows denied the change even to an administrator (e.g. CodexSandboxService).
    Locked,
    /// The service does not accept a stop request.
    NotStoppable,
    Timeout,
    Failed,
    /// App side: the user declined the UAC prompt.
    UacCancelled,
    /// App side: no helper is running.
    NotArmed,
    /// App side: the pipe broke, timed out or carried garbage.
    Transport,
    /// App side or helper side: the peer failed identity or nonce checks.
    Handshake,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminError {
    pub code: ErrorCode,
    pub message: String,
    /// Services left stopped by a request that failed part-way (after rollback was
    /// attempted). The app must journal these so they can be started again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stopped: Vec<String>,
}

impl AdminError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> AdminError {
        AdminError { code, message: message.into(), stopped: Vec::new() }
    }
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = serde_json::to_value(self.code).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        write!(f, "{code}: {}", self.message)
    }
}

impl std::error::Error for AdminError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<Reply>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<AdminError>,
}

impl Response {
    pub fn from_result(id: u64, r: Result<Reply, AdminError>) -> Response {
        match r {
            Ok(reply) => Response { id, ok: Some(reply), err: None },
            Err(e) => Response { id, ok: None, err: Some(e) },
        }
    }

    pub fn into_result(self) -> Result<Reply, AdminError> {
        match (self.ok, self.err) {
            (Some(r), None) => Ok(r),
            (None, Some(e)) => Err(e),
            _ => Err(AdminError::new(ErrorCode::Transport, "response must carry exactly one of ok/err")),
        }
    }

    pub fn to_line(&self) -> String {
        // Serialising these plain structs cannot fail.
        let mut s = serde_json::to_string(self).unwrap_or_else(|_| r#"{"id":0,"err":{"code":"failed","message":"serialise"}}"#.into());
        s.push('\n');
        s
    }
}

/// Parse and validate one request line (without its newline). On failure returns the
/// error and the request id when it could be recovered (0 otherwise).
pub fn parse_request(line: &[u8]) -> Result<Request, (u64, AdminError)> {
    let bad = |id: u64, m: String| (id, AdminError::new(ErrorCode::BadRequest, m));
    if line.len() > MAX_REQUEST_LINE {
        return Err(bad(0, format!("request is longer than {MAX_REQUEST_LINE} bytes")));
    }
    let text = std::str::from_utf8(line).map_err(|_| bad(0, "request is not UTF-8".into()))?;
    let req: Request = serde_json::from_str(text).map_err(|e| {
        // Recover the id for the error reply when the envelope is at least an object with
        // a numeric id; never trust anything else from it.
        let id = serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|v| v.get("id").and_then(|i| i.as_u64()))
            .unwrap_or(0);
        bad(id, format!("malformed request: {e}"))
    })?;
    req.op.validate().map_err(|e| bad(req.id, e))?;
    Ok(req)
}

pub fn request_line(req: &Request) -> String {
    let mut s = serde_json::to_string(req).unwrap_or_default();
    s.push('\n');
    s
}

/// Parse a response line (app side).
pub fn parse_response(line: &[u8]) -> Result<Response, AdminError> {
    if line.len() > MAX_RESPONSE_LINE {
        return Err(AdminError::new(ErrorCode::Transport, "response too long"));
    }
    let text = std::str::from_utf8(line).map_err(|_| AdminError::new(ErrorCode::Transport, "response is not UTF-8"))?;
    serde_json::from_str(text).map_err(|e| AdminError::new(ErrorCode::Transport, format!("malformed response: {e}")))
}

// ---------------------------------------------------------------------------------------
// Handshake messages
// ---------------------------------------------------------------------------------------

/// App -> helper, first line on the pipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    /// The nonce from the helper's command line, as 64 lowercase hex digits.
    pub hello: String,
    pub version: u32,
}

/// Helper -> app, reply to Hello.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proof {
    /// hex(SHA-256(domain | nonce | helper pid | app pid)), see pipe::proof.
    pub proof: String,
    pub pid: u32,
    pub version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(line: &str) -> Request {
        parse_request(line.as_bytes()).unwrap_or_else(|(_, e)| panic!("{line}: {e}"))
    }

    fn err(line: &str) -> (u64, AdminError) {
        match parse_request(line.as_bytes()) {
            Ok(r) => panic!("accepted {line}: {r:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn accepts_every_op() {
        assert_eq!(ok(r#"{"id":1,"op":"Ping"}"#).op, Op::Ping);
        assert_eq!(ok(r#"{"id":1,"op":"Shutdown"}"#).op, Op::Shutdown);
        assert_eq!(ok(r#"{"id":2,"op":{"ServiceStop":{"name":"SysMain"}}}"#).op, Op::ServiceStop { name: "SysMain".into() });
        assert_eq!(ok(r#"{"id":2,"op":{"ServiceStart":{"name":"SysMain"}}}"#).op, Op::ServiceStart { name: "SysMain".into() });
        assert_eq!(ok(r#"{"id":2,"op":{"ServiceQuery":{"name":"SysMain"}}}"#).op, Op::ServiceQuery { name: "SysMain".into() });
        for (s, c) in [("Auto", StartChoice::Auto), ("AutoDelayed", StartChoice::AutoDelayed), ("Manual", StartChoice::Manual), ("Disabled", StartChoice::Disabled)] {
            let line = format!(r#"{{"id":3,"op":{{"ServiceSetStartType":{{"name":"SysMain","start":"{s}"}}}}}}"#);
            assert_eq!(ok(&line).op, Op::ServiceSetStartType { name: "SysMain".into(), start: c });
        }
        assert_eq!(
            ok(r#"{"id":4,"op":{"TaskSetEnabled":{"path":"\\Microsoft\\Windows\\Defrag\\ScheduledDefrag","enabled":false}}}"#).op,
            Op::TaskSetEnabled { path: r"\Microsoft\Windows\Defrag\ScheduledDefrag".into(), enabled: false }
        );
        assert_eq!(ok(r#"{"id":5,"op":{"TaskQuery":{"path":"\\X"}}}"#).op, Op::TaskQuery { path: r"\X".into() });
        // Round trip through our own serialiser.
        let r = Request { id: 99, op: Op::ServiceSetStartType { name: "a".into(), start: StartChoice::Manual } };
        assert_eq!(ok(request_line(&r).trim_end()), r);
    }

    #[test]
    fn rejects_unknown_ops_and_fields() {
        for line in [
            r#"{"id":1,"op":"Reboot"}"#,
            r#"{"id":1,"op":{"ServiceDelete":{"name":"SysMain"}}}"#,
            r#"{"id":1,"op":{"ServiceStop":{"name":"SysMain","force":true}}}"#,
            r#"{"id":1,"op":{"ServiceStop":{}}}"#,
            r#"{"id":1,"op":{"ServiceStop":{"name":7}}}"#,
            r#"{"id":1,"op":{"ServiceStop":{"name":"A"},"ServiceStart":{"name":"B"}}}"#,
            r#"{"id":1,"op":"Ping","extra":1}"#,
            r#"{"id":1,"op":"ping"}"#,
            r#"{"id":1,"op":{"ServiceSetStartType":{"name":"SysMain","start":"Boot"}}}"#,
            r#"{"id":1,"op":{"ServiceSetStartType":{"name":"SysMain","start":"auto"}}}"#,
            r#"{"id":1,"op":{"ServiceSetStartType":{"name":"SysMain"}}}"#,
            r#"{"id":1,"op":{"TaskSetEnabled":{"path":"\\X","enabled":"no"}}}"#,
            r#"{"id":1,"op":{"TaskSetEnabled":{"path":"\\X"}}}"#,
            r#"{"id":1,"op":{"ServiceStop":{"name":"A","name":"B"}}}"#,
        ] {
            let (id, e) = err(line);
            assert_eq!(e.code, ErrorCode::BadRequest, "{line}");
            assert_eq!(id, 1, "{line}: id recovered for the reply");
        }
        for line in [r#"{"op":"Ping"}"#, r#"{"id":-1,"op":"Ping"}"#, r#"{"id":1.5,"op":"Ping"}"#, r#"{"id":"1","op":"Ping"}"#, "", "null", "[]", "Ping", r#"{"id":1}"#] {
            let (_, e) = err(line);
            assert_eq!(e.code, ErrorCode::BadRequest, "{line}");
        }
    }

    #[test]
    fn rejects_bad_names_and_paths() {
        for line in [
            r#"{"id":7,"op":{"ServiceStop":{"name":""}}}"#,
            r#"{"id":7,"op":{"ServiceStop":{"name":"..\\RpcSs"}}}"#,
            r#"{"id":7,"op":{"ServiceStop":{"name":"a/b"}}}"#,
            r#"{"id":7,"op":{"ServiceStop":{"name":"a\u0000b"}}}"#,
            r#"{"id":7,"op":{"ServiceQuery":{"name":" SysMain"}}}"#,
            r#"{"id":7,"op":{"TaskQuery":{"path":"X"}}}"#,
            r#"{"id":7,"op":{"TaskQuery":{"path":"\\a\\..\\b"}}}"#,
            r#"{"id":7,"op":{"TaskSetEnabled":{"path":"\\Microsoft\\Windows\\Windows Defender\\..\\x","enabled":false}}}"#,
            r#"{"id":7,"op":{"TaskSetEnabled":{"path":"\\\\server\\pipe","enabled":false}}}"#,
        ] {
            let (id, e) = err(line);
            assert_eq!((id, e.code), (7, ErrorCode::BadRequest), "{line}");
        }
        let long_name = "a".repeat(rules::MAX_SERVICE_NAME + 1);
        let (_, e) = err(&format!(r#"{{"id":1,"op":{{"ServiceStop":{{"name":"{long_name}"}}}}}}"#));
        assert_eq!(e.code, ErrorCode::BadRequest);
    }

    #[test]
    fn rejects_oversized_and_non_utf8_lines() {
        let pad = "x".repeat(MAX_REQUEST_LINE);
        let line = format!(r#"{{"id":1,"op":{{"ServiceQuery":{{"name":"{pad}"}}}}}}"#);
        let (id, e) = err(&line);
        assert_eq!((id, e.code), (0, ErrorCode::BadRequest));
        assert!(e.message.contains("longer"));
        let (_, e) = parse_request(&[0xFF, 0xFE, b'{']).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
    }

    #[test]
    fn response_round_trip_and_exactly_one_of_ok_err() {
        let r = Response::from_result(5, Err(AdminError::new(ErrorCode::Locked, "denied")));
        let line = r.to_line();
        assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
        assert!(line.contains(r#""code":"locked""#), "{line}");
        let back = parse_response(line.trim_end().as_bytes()).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.into_result().unwrap_err().code, ErrorCode::Locked);

        let ok = Response::from_result(6, Ok(Reply::Pong { version: 1, pid: 3 }));
        assert_eq!(parse_response(ok.to_line().trim_end().as_bytes()).unwrap().into_result().unwrap(), Reply::Pong { version: 1, pid: 3 });

        let both = parse_response(br#"{"id":1,"ok":"Bye","err":{"code":"failed","message":"x"}}"#).unwrap();
        assert_eq!(both.into_result().unwrap_err().code, ErrorCode::Transport);
        let neither = parse_response(br#"{"id":1}"#).unwrap();
        assert_eq!(neither.into_result().unwrap_err().code, ErrorCode::Transport);
        assert!(parse_response(br#"{"id":1,"ok":"Bye","x":1}"#).is_err());
    }

    #[test]
    fn start_choice_mapping() {
        assert_eq!(StartType::Auto.to_choice(Some(true)), Some(StartChoice::AutoDelayed));
        assert_eq!(StartType::Auto.to_choice(Some(false)), Some(StartChoice::Auto));
        assert_eq!(StartType::Auto.to_choice(None), Some(StartChoice::Auto));
        assert_eq!(StartType::Manual.to_choice(Some(true)), Some(StartChoice::Manual));
        assert_eq!(StartType::Boot.to_choice(None), None);
        assert_eq!(StartType::Unknown.to_choice(None), None);
        assert!(StartChoice::AutoDelayed.matches(StartType::Auto, Some(true)));
        assert!(!StartChoice::AutoDelayed.matches(StartType::Auto, Some(false)));
        assert!(!StartChoice::Auto.matches(StartType::Auto, Some(true)));
        assert!(StartChoice::Manual.matches(StartType::Manual, Some(true)));
        assert!(!StartChoice::Disabled.matches(StartType::Manual, None));
    }
}
