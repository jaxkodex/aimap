//! SIGTERM/SIGINT capture. The Python services install signal handlers that
//! set the stop event; a raw signal handler may only touch atomics, so the
//! handler sets a flag and a watcher thread copies it into the stop event
//! (and the ingest stop flag), giving every waiter sub-second latency.

use crate::classifier::StopEvent;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

static RECEIVED: AtomicBool = AtomicBool::new(false);
static LAST_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    LAST_SIGNAL.store(sig, Ordering::SeqCst);
    RECEIVED.store(true, Ordering::SeqCst);
}

/// `install_signal_handlers`: catch SIGTERM and SIGINT.
pub fn install() {
    let handler: extern "C" fn(libc::c_int) = on_signal;
    unsafe {
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }
}

pub fn received() -> bool {
    RECEIVED.load(Ordering::SeqCst)
}

/// `signal.Signals(signum).name` for the last caught signal.
pub fn last_name() -> &'static str {
    match LAST_SIGNAL.load(Ordering::SeqCst) {
        libc::SIGINT => "SIGINT",
        libc::SIGTERM => "SIGTERM",
        _ => "SIGTERM",
    }
}

/// Copy the signal flag into the stop event and the ingest stop flag, so
/// `stop.wait()` wakes up and `sync_mailbox` (which checks an `AtomicBool`)
/// stops between batches. Runs until a signal arrives.
pub fn spawn_watcher(stop: &StopEvent, ingest_stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    let stop = stop.clone();
    std::thread::spawn(move || loop {
        if stop.is_set() {
            return; // stopped for another reason; nothing to watch
        }
        if RECEIVED.load(Ordering::SeqCst) {
            ingest_stop.store(true, Ordering::SeqCst);
            stop.set();
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    })
}

/// Test helper: forget that a signal was caught.
pub fn reset_for_tests() {
    RECEIVED.store(false, Ordering::SeqCst);
    LAST_SIGNAL.store(0, Ordering::SeqCst);
}
