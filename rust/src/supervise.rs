//! Run the ingest worker, the classifier and the API as child processes of
//! one container. Port of `supervise.py`: each child is a normal `aimap run`,
//! `aimap classify` or `aimap api` process with its own signal handlers and
//! database pool. SIGTERM and SIGINT are caught here and terminate every
//! child; if one child exits, the others are stopped and the supervisor exits
//! with that child's code (or 1 for a clean exit), so the platform restarts
//! the whole container.

use crate::classifier::StopEvent;
use crate::logs::{LogValue, Logger};
use crate::signals;
use std::process::Child;
use std::time::{Duration, Instant};

/// (service name, aimap subcommand) in startup order.
pub const SERVICES: [(&str, &str); 3] = [("worker", "run"), ("classify", "classify"), ("api", "api")];

/// The command lines for the three children: this binary, with the same
/// --env-file, one per service (`sys.executable -m aimap --env-file ...` in
/// Python).
pub fn aimap_commands(env_file: &str) -> Vec<(String, Vec<String>)> {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "aimap".to_string());
    SERVICES
        .iter()
        .map(|(name, command)| {
            (
                name.to_string(),
                vec![
                    exe.clone(),
                    "--env-file".to_string(),
                    env_file.to_string(),
                    command.to_string(),
                ],
            )
        })
        .collect()
}

pub struct Supervisor {
    pub commands: Vec<(String, Vec<String>)>,
    /// how long terminated children get to finish
    pub grace: f64,
    /// how often children are polled for an early exit
    pub poll: f64,
    pub stop: StopEvent,
    pub log: Logger,
}

impl Supervisor {
    pub fn new(commands: Vec<(String, Vec<String>)>, grace: f64, poll: f64, log: Logger) -> Supervisor {
        Supervisor { commands, grace, poll, stop: StopEvent::new(), log }
    }

    /// Start every child, wait for one to exit or a stop signal, then shut
    /// the others down. Returns the exit code.
    pub fn run(&mut self) -> i32 {
        let mut children: Vec<(String, Child)> = self
            .commands
            .iter()
            .map(|(name, argv)| {
                let mut parts = argv.iter();
                let program = parts.next().expect("command line needs a program");
                let child = std::process::Command::new(program).args(parts).spawn();
                match child {
                    Ok(child) => Ok((name.clone(), child)),
                    Err(e) => {
                        self.log.error("cannot start child", &[
                            ("child", LogValue::str(name.clone())),
                            ("error", LogValue::str(e.to_string())),
                        ]);
                        Err(e)
                    }
                }
            })
            .collect::<Result<_, _>>()
            .expect("all children must start");
        for (name, child) in &children {
            self.log.info("child started", &[
                ("child", LogValue::str(name.clone())),
                ("pid", LogValue::Int(child.id() as i64)),
            ]);
        }
        let mut code = 0;
        loop {
            if self.stop.wait_timeout(Duration::from_secs_f64(self.poll)) {
                break; // SIGTERM/SIGINT: shut down and exit 0
            }
            let mut exited: Option<(String, i32)> = None;
            for (name, child) in children.iter_mut() {
                if let Some(status) = child.try_wait().expect("poll child") {
                    exited = Some((name.clone(), status.code().unwrap_or(1)));
                    break;
                }
            }
            if let Some((name, child_code)) = exited {
                code = if child_code == 0 { 1 } else { child_code }; // a child is never meant to exit on its own
                self.log.error("child exited; stopping the others", &[
                    ("child", LogValue::str(name)),
                    ("code", LogValue::Int(child_code as i64)),
                ]);
                break;
            }
        }
        self.shutdown(&mut children);
        code
    }

    fn shutdown(&mut self, children: &mut [(String, Child)]) {
        for (_, child) in children.iter_mut() {
            if child.try_wait().ok().flatten().is_none() {
                unsafe {
                    libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs_f64(self.grace);
        for (name, child) in children.iter_mut() {
            if wait_until(child, deadline).is_none() {
                self.log.warning("child did not stop in time; killing it", &[
                    ("child", LogValue::str(name.clone())),
                ]);
                let _ = child.kill();
                let _ = child.wait();
            }
            let code = child.try_wait().ok().flatten().and_then(|s| s.code()).unwrap_or(-1);
            self.log.info("child stopped", &[
                ("child", LogValue::str(name.clone())),
                ("code", LogValue::Int(code as i64)),
            ]);
        }
    }

    pub fn install_signal_handlers(&mut self) {
        signals::install();
        let stop = self.stop.clone();
        let ingest_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _ = signals::spawn_watcher(&stop, ingest_stop);
    }
}

/// Wait for the child, but only until `deadline`. None means it had to be
/// killed (or is still running at the deadline; kill() was issued by caller).
fn wait_until(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
