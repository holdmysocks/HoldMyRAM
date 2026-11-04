//! holdmyram-helper: the short-lived elevated half of HoldMyRAM.
//!
//! Started only by holdmyram.exe through ShellExecuteEx "runas" with
//! `--pipe <name> --parent <pid> --nonce <hex>`. It refuses to do anything unless:
//! its own token is elevated; the arguments are exactly well-formed; the parent process
//! is alive and its image is `holdmyram.exe` in this exe's directory; the pipe is a local
//! HoldMyRAM pipe whose SERVER is that parent process (checked against a handle opened
//! before connecting, so the PID cannot be recycled); and the parent proves the nonce.
//!
//! It then serves requests (core `admin::handler`, which re-checks every request against
//! the never-touch rules) and exits when the pipe closes, on Shutdown, or as soon as the
//! parent process exits. Nothing else: no window, no console, no files, no background
//! work.
//!
//! Exit codes: 0 normal, 2 not elevated, 3 bad arguments, 4 parent not found,
//! 5 parent is not holdmyram.exe next to this helper, 6 cannot connect to the pipe,
//! 7 pipe server is not the parent, 8 handshake failed or timed out, 9 cannot open the
//! service manager, 10 protocol violation.

#![windows_subsystem = "windows"]

use holdmyram_core::admin::handler::{Handler, Policy};
use holdmyram_core::admin::pipe::{self, HelperArgs, LineReader};
use holdmyram_core::admin::proto::MAX_REQUEST_LINE;
use holdmyram_core::admin::sys;
use holdmyram_core::admin::win::WinBackend;
use std::sync::atomic::{AtomicBool, Ordering};

const APP_EXE: &str = "holdmyram.exe";
/// The app must complete the handshake within this time after the helper connects.
const HANDSHAKE_DEADLINE_MS: u32 = 30_000;

static HANDSHAKE_DONE: AtomicBool = AtomicBool::new(false);

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    if !sys::is_elevated() {
        return 2;
    }
    let Ok(args) = HelperArgs::parse(std::env::args().skip(1)) else { return 3 };

    // Pin the parent's identity first: while we hold this handle its PID cannot be reused.
    let Ok(parent) = sys::open_process(args.parent) else { return 4 };
    if sys::has_exited(&parent) {
        return 4;
    }
    let Some(expected) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(APP_EXE))) else {
        return 5;
    };
    match sys::image_path(&parent) {
        Ok(actual) if sys::same_file(&actual, &expected) => {}
        _ => return 5,
    }

    let Ok(file) = pipe::connect_client(&args.pipe) else { return 6 };
    match pipe::server_pid(&file) {
        Ok(pid) if pid == args.parent => {}
        _ => return 7,
    }

    // Exit as soon as the parent exits, or if the handshake does not complete in time.
    // One blocking wait on the process handle; no polling.
    std::thread::spawn(move || {
        if sys::wait_exit(&parent, HANDSHAKE_DEADLINE_MS) {
            std::process::exit(0);
        }
        if !HANDSHAKE_DONE.load(Ordering::SeqCst) {
            std::process::exit(8);
        }
        sys::wait_exit(&parent, u32::MAX);
        std::process::exit(0);
    });

    let mut reader = LineReader::new(&file, MAX_REQUEST_LINE);
    let mut writer = &file;
    if pipe::helper_handshake(&mut reader, &mut writer, &args.nonce, args.parent).is_err() {
        return 8;
    }
    HANDSHAKE_DONE.store(true, Ordering::SeqCst);

    // COM and the SCM connection live on this thread, which handles every request.
    let Ok(backend) = WinBackend::new() else { return 9 };
    let mut handler = Handler::new(backend, Policy::new(vec![args.parent, std::process::id()]));
    match pipe::serve(&mut handler, &mut reader, &mut writer) {
        Ok(()) => 0,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => 10,
        Err(_) => 0, // pipe broke: the app went away
    }
}
