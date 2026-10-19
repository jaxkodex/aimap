//! Port of `tests/test_supervise.py`: one child exiting stops the others and
//! its code is passed through, a clean exit still counts as failure, a stop
//! signal terminates every child, and a SIGTERM-ignoring child is killed
//! after the grace period. Real child processes (`/bin/sh`), real signals.

use aimap::logs::{Capture, Logger};
use aimap::supervise::{aimap_commands, Supervisor};
use std::time::{Duration, Instant};

fn logger() -> Logger {
    Logger::new(Capture::new(), "INFO", "aimap.supervise")
}

fn supervisor(commands: Vec<(&str, &str)>, grace: f64, poll: f64) -> Supervisor {
    let commands = commands
        .into_iter()
        .map(|(name, sh)| {
            (
                name.to_string(),
                vec!["/bin/sh".to_string(), "-c".to_string(), sh.to_string()],
            )
        })
        .collect();
    Supervisor::new(commands, grace, poll, logger())
}

const SLEEP: &str = "sleep 60";
const EXIT_3: &str = "exit 3";
const STUBBORN: &str = "trap '' TERM; sleep 60";

#[test]
fn one_child_exiting_stops_the_others_and_passes_its_code() {
    let mut sup = supervisor(
        vec![("a", SLEEP), ("b", EXIT_3), ("c", SLEEP)],
        5.0,
        0.05,
    );
    let started = Instant::now();
    assert_eq!(sup.run(), 3);
    // the sleepers were terminated, not waited out
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}

#[test]
fn a_clean_exit_still_counts_as_failure() {
    let mut sup = supervisor(vec![("a", SLEEP), ("b", "true")], 5.0, 0.05);
    assert_eq!(sup.run(), 1);
}

#[test]
fn stop_terminates_every_child_and_exits_zero() {
    let mut sup = supervisor(vec![("a", SLEEP), ("b", SLEEP)], 5.0, 0.05);
    let stopper = sup.stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        stopper.set();
    });
    assert_eq!(sup.run(), 0);
}

#[test]
fn child_ignoring_sigterm_is_killed_after_grace() {
    let mut sup = supervisor(vec![("a", STUBBORN)], 0.5, 0.05);
    let stopper = sup.stop.clone();
    std::thread::spawn(move || {
        // after the child has installed its handler
        std::thread::sleep(Duration::from_millis(1000));
        stopper.set();
    });
    let started = Instant::now();
    assert_eq!(sup.run(), 0);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}

#[test]
fn aimap_commands_run_each_service_with_the_same_env_file() {
    let cmds = aimap_commands("custom.env");
    let names: Vec<&str> = cmds.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, vec!["worker", "classify", "api"]);
    let expected_last = ["run", "classify", "api"];
    for ((_, argv), last) in cmds.iter().zip(expected_last) {
        // [current_exe, --env-file, custom.env, <command>]
        assert_eq!(argv.len(), 4, "{argv:?}");
        assert_eq!(argv[1], "--env-file");
        assert_eq!(argv[2], "custom.env");
        assert_eq!(argv[3], last);
        let exe = std::env::current_exe().unwrap();
        assert_eq!(argv[0], exe.display().to_string());
    }
}
