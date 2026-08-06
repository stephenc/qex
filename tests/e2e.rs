//! End-to-end tests for qex.
//!
//! Each test makes its own config directory, state directory and runtime
//! directory in a temporary location. A test thus starts its own coordinator,
//! and it does not touch the coordinator of the user.
//!
//! These tests start real processes. They are slower than the unit tests, but
//! they are the only tests that measure the behaviour that matters: a job that
//! runs, a job that stops, and a result that stays correct after a failure.
//!
//! Run these tests with two threads:
//!
//! ```sh
//! cargo test -- --test-threads=2
//! ```
//!
//! Each test starts real processes and waits for them. With more threads, the
//! machine becomes busy, a job starts late, and a test reports a failure that
//! the program does not have.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One isolated qex installation.
struct Harness {
    root: PathBuf,
}

impl Harness {
    /// Makes a new installation with the given config file.
    fn new(name: &str, config: &str) -> Self {
        // Keep the path short. The socket path must fit in `sun_path`, and qex
        // uses a directory in /tmp when it does not fit. Both paths are tested,
        // but a short path here tests the usual one.
        let root = std::env::temp_dir().join(format!(
            "qx{}-{}-{}",
            std::process::id(),
            name,
            Instant::now().elapsed().subsec_nanos()
        ));
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::write(root.join("cfg/qex.toml"), config).unwrap();
        Self { root }
    }

    fn with_default_config(name: &str) -> Self {
        // Turn the peer system off. A test must not read the records of the
        // other users of the machine, or its result changes with the load.
        Self::new(
            name,
            "[peers]\nenabled = false\n\
             [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
        )
    }

    fn qex(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_qex"))
            .args(args)
            .env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            // Keep the coordinator for the length of the test only.
            .env("QEX_IDLE_EXIT_SECS", "120")
            .output()
            .expect("qex did not start")
    }

    /// Runs a command and requires that it succeeds.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.qex(args);
        assert!(
            out.status.success(),
            "the command `qex {}` failed with the code {:?}\nstdout: {}\nstderr: {}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn submit(&self, args: &[&str]) -> String {
        let id = self.ok(args);
        assert_eq!(id.lines().count(), 1, "submit must write the id only: {id}");
        assert!(
            id.parse::<uuid::Uuid>().is_ok(),
            "submit must write a job id, and it wrote: {id}"
        );
        id
    }

    fn status_json(&self, id: &str) -> serde_json::Value {
        let text = self.ok(&["status", id, "--json"]);
        serde_json::from_str(&text).expect("the status output is not valid JSON")
    }

    fn list_json(&self) -> Vec<serde_json::Value> {
        let text = self.ok(&["list", "--json"]);
        serde_json::from_str(&text).expect("the list output is not valid JSON")
    }

    fn state_of(&self, id: &str) -> String {
        self.status_json(id)["state"].as_str().unwrap().to_string()
    }

    /// Waits until a condition is true, or fails after the time limit.
    fn until(&self, what: &str, limit: Duration, mut test: impl FnMut() -> bool) {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if test() {
                return;
            }
            // Each test spawns a `qex` process, and two tests operate together.
            // At 50ms that was 40 processes each second on a machine with four
            // cores, and the test then held the cores that the job needed. The
            // test must not make the load that it waits for.
            std::thread::sleep(Duration::from_millis(200));
        }

        // Say WHY the condition did not arrive.
        //
        // Without this, a failure on a build machine gives "the job starts"
        // and nothing else, and the machine goes away with the answer. qex
        // holds the reason for each job that waits, and `qex info` holds the
        // budget and the load, so a test must show them both.
        let jobs = String::from_utf8_lossy(&self.qex(&["list"]).stdout).to_string();
        let info = String::from_utf8_lossy(&self.qex(&["info", "--no-start"]).stdout).to_string();

        // A job that did not reach its state leaves its evidence in its own
        // directory: the record, the log of its supervisor, and the supervisor
        // process itself. A build machine goes away with that evidence, so the
        // test must read it here.
        let mut detail = String::new();
        for job in self.list_json() {
            let state = job["state"].as_str().unwrap_or("");
            if matches!(
                state,
                "completed" | "failed" | "killed" | "cancelled" | "skipped"
            ) {
                continue;
            }
            let id = job["id"].as_str().unwrap_or("").to_string();
            let sup = job["supervisor_pid"].as_i64();
            let alive = match sup {
                Some(pid) => {
                    if unsafe { libc::kill(pid as i32, 0) } == 0 {
                        "alive"
                    } else {
                        "DEAD"
                    }
                }
                None => "none",
            };
            detail.push_str(&format!(
                "\njob {id}  state={state}  supervisor={sup:?} ({alive})\n"
            ));

            let dir = self.root.join("state/qex/jobs").join(&id);
            for file in ["status.json", "supervisor.log", "stderr.log"] {
                if let Ok(text) = std::fs::read(dir.join(file)) {
                    let text = String::from_utf8_lossy(&text);
                    let text = text.trim();
                    if !text.is_empty() {
                        detail.push_str(&format!("  --- {file} ---\n  {:.900}\n", text));
                    }
                }
            }
            if let Some(pid) = sup {
                if let Ok(out) = std::process::Command::new("ps")
                    .args(["-o", "pid=,stat=,wchan:20=,args=", "-p", &pid.to_string()])
                    .output()
                {
                    detail.push_str(&format!(
                        "  --- ps ---\n  {}\n",
                        String::from_utf8_lossy(&out.stdout).trim()
                    ));
                }
            }
        }

        panic!(
            "qex did not reach this condition in {limit:?}: {what}\n\n\
             --- qex list ---\n{jobs}\n--- qex info ---\n{info}\n{detail}"
        );
    }

    /// Tests if a job left the queue.
    ///
    /// THIS IS MONOTONIC: once a job starts, it stays true. A test that waits
    /// for `state == "running"` waits for a window, and a job that stops before
    /// the test looks makes that condition false FOR EVER — which is the fault
    /// that qex exists to remove, in the tests of qex.
    fn has_started(&self, id: &str) -> bool {
        !matches!(self.state_of(id).as_str(), "queued" | "starting")
    }

    /// Gives the process id of the coordinator.
    ///
    /// The value comes from the coordinator itself. A search of the process
    /// list with `pgrep -f qex` also matches the command that contains those
    /// letters, so this code does not use that method.
    fn coordinator_pid(&self) -> i32 {
        let text = self.ok(&["info", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        v["pid"].as_i64().unwrap() as i32
    }

    fn job_dir(&self, id: &str) -> PathBuf {
        self.root.join("state/qex/jobs").join(id)
    }

    /// Starts `qex run` beside this test, and gives the child and the job id.
    ///
    /// A test that stops the job of a `qex run` needs the id while the command
    /// still waits, so the command writes the id to a file with `--id-file`.
    /// The two output streams go to a pipe, so that the test can read what the
    /// command said about the reason that the job stopped.
    ///
    /// The caller gives the child to `wait_run`, which waits for it. Clippy
    /// cannot see that, because the wait is in a different function.
    #[allow(clippy::zombie_processes)]
    fn run_bg(&self, args: &[&str]) -> (std::process::Child, String) {
        let id_file = self.root.join(format!(
            "run-{}.id",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let id_path = id_file.to_str().unwrap().to_string();
        let mut all: Vec<&str> = vec!["run", "--id-file", &id_path];
        all.extend_from_slice(args);

        let child = Command::new(env!("CARGO_BIN_EXE_qex"))
            .args(&all)
            .env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("QEX_IDLE_EXIT_SECS", "120")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("qex run did not start");

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(&id_file) {
                let id = text.trim().to_string();
                if id.parse::<uuid::Uuid>().is_ok() {
                    return (child, id);
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("`qex run` did not write its id file in 30 seconds");
    }

    /// Writes the config file again, while the installation operates.
    ///
    /// A user does this with an editor. The coordinator that already operates
    /// keeps the values that it read when it started, and each NEW process
    /// reads what this function wrote.
    fn write_config(&self, config: &str) {
        std::fs::write(self.root.join("cfg/qex.toml"), config).unwrap();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Stop the coordinator of this test, then delete the directory.
        let out = self.qex(&["info", "--json"]);
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            if let Some(pid) = v["pid"].as_i64() {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
        std::fs::remove_dir_all(&self.root).ok();
    }
}

#[test]
fn a_job_that_succeeds_gives_the_exit_code_zero() {
    let h = Harness::with_default_config("ok");
    let id = h.submit(&["submit", "--", "true"]);
    let out = h.qex(&["wait", &id]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(h.state_of(&id), "completed");
}

#[test]
fn a_job_that_fails_gives_the_exit_code_one() {
    let h = Harness::with_default_config("fail");
    let id = h.submit(&["submit", "--", "false"]);
    let out = h.qex(&["wait", &id]);
    assert_eq!(out.status.code(), Some(1));

    let status = h.status_json(&id);
    assert_eq!(status["state"], "failed");
    assert_eq!(status["exit_code"], 1);
}

/// The option `--passthrough` gives the exit code of the job.
#[test]
fn the_passthrough_option_gives_the_exit_code_of_the_job() {
    let h = Harness::with_default_config("pass");
    let id = h.submit(&["submit", "--", "sh", "-c", "exit 42"]);
    let out = h.qex(&["wait", &id, "--passthrough"]);
    assert_eq!(out.status.code(), Some(42));
}

/// A wait that reaches its limit gives the code 124, and the job continues.
/// The command `timeout` uses the same code.
#[test]
fn a_wait_that_reaches_its_limit_gives_the_code_124() {
    let h = Harness::with_default_config("waitlimit");
    let id = h.submit(&["submit", "--", "sleep", "30"]);

    let out = h.qex(&["wait", &id, "--timeout", "2s"]);
    assert_eq!(out.status.code(), Some(124));

    // The limit stops the wait only. The job must continue.
    assert_eq!(
        h.state_of(&id),
        "running",
        "a wait that reaches its limit must not stop the job"
    );
    h.ok(&["kill", &id, "--grace", "1s"]);
}

#[test]
fn a_missing_job_gives_the_code_127() {
    let h = Harness::with_default_config("missing");
    let out = h.qex(&["wait", "3f5a1c2e-0000-4000-8000-000000000000"]);
    assert_eq!(out.status.code(), Some(127));
}

#[test]
fn the_output_of_a_job_is_recorded() {
    let h = Harness::with_default_config("logs");
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "echo to-stdout; echo to-stderr >&2",
    ]);
    h.ok(&["wait", &id]);

    let both = h.ok(&["logs", &id]);
    assert!(both.contains("to-stdout"), "the standard output is missing");
    assert!(both.contains("to-stderr"), "the standard error is missing");

    // The two streams must stay separate.
    let out_only = h.ok(&["logs", &id, "--stdout"]);
    assert!(out_only.contains("to-stdout"));
    assert!(!out_only.contains("to-stderr"), "the streams are mixed");
}

/// Many CLI processes can start at the same time with no coordinator. Exactly
/// one coordinator must start, and every job must reach the queue.
#[test]
fn many_submissions_at_once_start_one_coordinator_only() {
    let h = Harness::with_default_config("race");
    let exe = env!("CARGO_BIN_EXE_qex");

    let mut children = Vec::new();
    for _ in 0..20 {
        children.push(
            Command::new(exe)
                .args(["submit", "--", "true"])
                .env("XDG_CONFIG_HOME", h.root.join("cfg"))
                .env("XDG_STATE_HOME", h.root.join("state"))
                .env("XDG_RUNTIME_DIR", h.root.join("run"))
                .env("QEX_IDLE_EXIT_SECS", "120")
                .spawn()
                .expect("qex did not start"),
        );
    }
    for mut c in children {
        let status = c.wait().unwrap();
        assert!(status.success(), "one submission failed during the race");
    }

    assert_eq!(h.list_json().len(), 20, "qex lost a job during the race");

    // Count the job directories. Each submission must have exactly one.
    let dirs = std::fs::read_dir(h.root.join("state/qex/jobs"))
        .unwrap()
        .count();
    assert_eq!(dirs, 20);
}

/// The budget must limit the number of jobs that operate together. This rule is
/// the reason for qex: it stops two agents from starting too much work.
#[test]
fn the_budget_limits_the_jobs_that_operate_together() {
    let h = Harness::new(
        "budget",
        "[budget]\ncpu = \"4\"\nmem = \"2GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    // Each job sleeps for a long time, and the test stops them all at its end.
    // With a short sleep, a machine with other work can start and finish the
    // jobs OUTSIDE the window in which the test measures, and the test then
    // reports that no job ever started.
    let ids: Vec<String> = (0..3)
        .map(|_| {
            h.submit(&[
                "submit", "--cpu", "2", "--mem", "128MB", "--", "sleep", "300",
            ])
        })
        .collect();

    // Two jobs of two cores fill a budget of four cores. Wait for that state,
    // and then measure: the question is whether a THIRD job joins them.
    h.until("two jobs operate", Duration::from_secs(45), || {
        h.list_json()
            .iter()
            .filter(|j| j["state"] == "running")
            .count()
            == 2
    });

    // Measure the number that operate together, many times.
    let mut peak = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let running = h
            .list_json()
            .iter()
            .filter(|j| j["state"] == "running")
            .count();
        peak = peak.max(running);
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(peak > 0, "no job ever started");
    assert!(
        peak <= 2,
        "the budget of 4 cores must hold two jobs of 2 cores, and {peak} jobs operated together"
    );

    // Stop each job. Two of them operate and one waits, so this uses both
    // commands and it tests neither.
    for id in &ids {
        h.qex(&["kill", id, "--grace", "1s"]);
        h.qex(&["cancel", id]);
    }
}

/// A job that is larger than the budget must not wait for ever. qex starts it
/// alone when no other job operates, so the agent receives a result.
#[test]
fn a_job_that_is_too_large_runs_when_the_queue_is_empty() {
    let h = Harness::new(
        "oversized",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [queue]\noversized = \"run-when-idle\"\nsettle = \"1s\"\n",
    );

    // Fill the queue first, so the large job must wait.
    // A LONG sleep, and the test stops the job when it no longer needs it to
    // hold the capacity. A short sleep makes a window: the test waits for the
    // job to be `running`, and a machine with other work can look after the job
    // has stopped. The condition is then false for ever.
    let small = h.submit(&[
        "submit", "--cpu", "2", "--mem", "128MB", "--", "sleep", "300",
    ]);
    h.until("the small job starts", Duration::from_secs(45), || {
        h.state_of(&small) == "running"
    });

    let out = h.qex(&[
        "submit", "--cpu", "64", "--mem", "64GB", "--", "echo", "big",
    ]);
    assert!(out.status.success());
    let big = String::from_utf8_lossy(&out.stdout).trim().to_string();

    // The warning must go to stderr. The id must stay alone on stdout, so the
    // command `ID=$(qex submit ...)` continues to operate.
    let warning = String::from_utf8_lossy(&out.stderr);
    assert!(
        warning.contains("64 cores") && warning.contains("budget"),
        "qex must warn at the submission: {warning}"
    );
    assert!(
        big.parse::<uuid::Uuid>().is_ok(),
        "stdout must hold the id only"
    );

    // The large job must wait while the small job operates.
    assert_eq!(h.state_of(&big), "queued");
    let reason = h.status_json(&big)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(!reason.is_empty(), "qex must give the reason for the wait");

    // The large job must start after the queue becomes empty. Stop the small
    // job, so this happens at a moment that the test chooses.
    h.ok(&["kill", &small, "--grace", "1s"]);
    h.until("the large job stops", Duration::from_secs(30), || {
        h.state_of(&big) == "completed"
    });

    let status = h.status_json(&big);
    assert_eq!(status["forced"], true, "qex must mark a forced job");
    assert!(
        status["forced_reason"]
            .as_str()
            .unwrap_or("")
            .contains("budget"),
        "the reason must name the budget"
    );
    assert!(h.ok(&["logs", &big]).contains("big"), "the job did not run");
}

/// The config file can refuse a job that is too large.
#[test]
fn the_reject_policy_refuses_a_job_that_is_too_large() {
    let h = Harness::new(
        "reject",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [queue]\noversized = \"reject\"\n",
    );

    let out = h.qex(&["submit", "--cpu", "64", "--", "true"]);
    assert!(!out.status.success(), "qex must refuse this job");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("64 cores"),
        "the error must name the claim: {err}"
    );
}

/// A job that starts children must stop completely. A process that stays holds
/// memory that qex counted, and the next job then meets a smaller machine.
#[test]
fn a_kill_stops_every_process_of_a_job() {
    let h = Harness::with_default_config("kill");
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "sleep 60 & sleep 60 & sleep 60 & wait",
    ]);

    h.until("the job starts", Duration::from_secs(45), || {
        h.state_of(&id) == "running"
    });

    let pid = h.status_json(&id)["pid"].as_i64().unwrap() as i32;
    assert!(
        count_in_group(pid) >= 2,
        "the job did not start its children"
    );

    h.ok(&["kill", &id, "--grace", "1s"]);

    h.until(
        "every process of the job stops",
        Duration::from_secs(20),
        || count_in_group(pid) == 0,
    );

    // The supervisor writes the result after the job stops, and the coordinator
    // reads that file. Give the record time to arrive. A job that stays in the
    // state `running` is a fault, and the time limit here finds it.
    h.until(
        "the record shows the job stopped",
        Duration::from_secs(20),
        || h.state_of(&id) == "killed",
    );
}

/// Counts the processes of one process group.
///
/// This function reads `/proc` and compares the process group id. It does not
/// compare a command line, so it cannot match the test itself.
#[cfg(target_os = "linux")]
fn count_in_group(pgid: i32) -> usize {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut count = 0;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if unsafe { libc::getpgid(pid) } == pgid {
            count += 1;
        }
    }
    count
}

/// The same count on macOS, which has no `/proc`.
///
/// This function read `/proc` on both systems before, so it gave 0 on macOS.
/// The first assertion of the test then failed, and the LAST one — that no
/// process of the job continues after `qex kill` — passed with no work, because
/// 0 is also the correct answer for a job that stopped. A test that cannot fail
/// gives no information.
#[cfg(not(target_os = "linux"))]
fn count_in_group(pgid: i32) -> usize {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-A", "-o", "pgid="])
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.trim().parse::<i32>() == Ok(pgid))
        .count()
}

/// The job result must stay correct after the coordinator fails. The supervisor
/// owns the result, so a coordinator that stops loses nothing.
#[test]
fn a_job_survives_the_failure_of_the_coordinator() {
    let h = Harness::with_default_config("crash");
    // Ten seconds, and not three: the coordinator must fail WHILE the job
    // operates, and a machine with other work can need several seconds to reach
    // the line below.
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "sleep 10; echo survived; exit 7",
    ]);

    h.until("the job starts", Duration::from_secs(45), || {
        h.state_of(&id) == "running"
    });

    let pid = h.coordinator_pid();
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        unsafe { libc::kill(pid, 0) } != 0,
        "the coordinator did not stop"
    );

    // `qex wait` must read the status file when no coordinator operates.
    let out = h.qex(&["wait", &id, "--timeout", "30s"]);
    assert_eq!(out.status.code(), Some(1), "the job exits with the code 7");

    let status = h.status_json(&id);
    assert_eq!(status["state"], "failed");
    assert_eq!(status["exit_code"], 7);
    assert!(h.ok(&["logs", &id]).contains("survived"));
}

/// A job that reaches its time limit gives the state `timeout`, and not the
/// state `killed`. The two states need different corrections.
#[test]
fn a_job_that_reaches_its_time_limit_has_the_state_timeout() {
    let h = Harness::with_default_config("jobtimeout");
    let id = h.submit(&["submit", "--timeout", "1s", "--", "sleep", "60"]);

    h.until("the job stops", Duration::from_secs(30), || {
        h.state_of(&id) == "timeout"
    });
}

/// qex must record the environment and the directory of the shell that
/// submitted the job.
#[test]
fn a_job_receives_the_environment_and_the_directory_of_the_shell() {
    let h = Harness::with_default_config("env");
    let dir = h.root.join("workdir");
    std::fs::create_dir_all(&dir).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_qex"))
        .args(["submit", "--", "sh", "-c", "pwd; echo MARK=$QEX_TEST_MARK"])
        .env("XDG_CONFIG_HOME", h.root.join("cfg"))
        .env("XDG_STATE_HOME", h.root.join("state"))
        .env("XDG_RUNTIME_DIR", h.root.join("run"))
        .env("QEX_IDLE_EXIT_SECS", "120")
        .env("QEX_TEST_MARK", "captured")
        .current_dir(&dir)
        .output()
        .unwrap();
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();

    h.ok(&["wait", &id]);
    let logs = h.ok(&["logs", &id]);
    assert!(
        logs.contains("MARK=captured"),
        "the job must receive the environment of the shell: {logs}"
    );
    assert!(
        logs.contains(dir.canonicalize().unwrap().to_str().unwrap()),
        "the job must operate in the directory of the shell: {logs}"
    );
}

/// The mode `none` must remove the environment of the shell.
#[test]
fn the_environment_mode_none_removes_the_variables_of_the_shell() {
    let h = Harness::with_default_config("envnone");

    let out = Command::new(env!("CARGO_BIN_EXE_qex"))
        .args([
            "submit",
            "--no-env-capture",
            "--env",
            "KEPT=yes",
            "--",
            "sh",
            "-c",
            "echo MARK=$QEX_TEST_MARK KEPT=$KEPT",
        ])
        .env("XDG_CONFIG_HOME", h.root.join("cfg"))
        .env("XDG_STATE_HOME", h.root.join("state"))
        .env("XDG_RUNTIME_DIR", h.root.join("run"))
        .env("QEX_IDLE_EXIT_SECS", "120")
        .env("QEX_TEST_MARK", "leaked")
        .output()
        .unwrap();
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();

    h.ok(&["wait", &id]);
    let logs = h.ok(&["logs", &id]);
    assert!(
        logs.contains("MARK= "),
        "a variable of the shell leaked: {logs}"
    );
    assert!(
        logs.contains("KEPT=yes"),
        "the --env value is missing: {logs}"
    );
}

/// A captured environment can hold secrets, so the files must not be readable
/// by the other users of the machine.
#[test]
fn the_job_files_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let h = Harness::with_default_config("modes");
    let id = h.submit(&["submit", "--", "true"]);
    h.ok(&["wait", &id]);

    let dir = h.job_dir(&id);
    let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "the job directory must be private");

    let spec_mode = std::fs::metadata(dir.join("spec.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(spec_mode, 0o600, "the job specification must be private");

    // The status output must not show the environment without the option.
    let status = h.ok(&["status", &id, "--json"]);
    assert!(
        !status.contains("\"env\""),
        "the status output must hide the environment: {status}"
    );
}

/// qex must not use a command line to find a job. This test submits a job whose
/// command holds the letters `qex daemon`, which is the pattern that a person
/// writes for `pgrep -f`. Every command must still operate.
#[test]
fn a_command_line_that_holds_the_word_qex_does_not_confuse_qex() {
    let h = Harness::with_default_config("selfmatch");
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "echo pretending to be qex daemon supervise; sleep 30",
    ]);

    h.until("the job starts", Duration::from_secs(45), || {
        h.state_of(&id) == "running"
    });

    // The coordinator must still find itself, and it must not find the job.
    let coordinator = h.coordinator_pid();
    let job_pid = h.status_json(&id)["pid"].as_i64().unwrap() as i32;
    assert_ne!(coordinator, job_pid);

    // The job must stop by its id, and not by its command line.
    h.ok(&["kill", &id, "--grace", "1s"]);
    h.until("the job stops", Duration::from_secs(20), || {
        h.state_of(&id) == "killed"
    });

    // The coordinator must continue after the job stops.
    assert_eq!(h.coordinator_pid(), coordinator);
}

/// A job in the queue leaves with `cancel`. A job that operates needs `kill`.
#[test]
fn cancel_removes_a_job_from_the_queue() {
    let h = Harness::new(
        "cancel",
        "[budget]\ncpu = \"1\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    let first = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
    ]);
    h.until("the first job starts", Duration::from_secs(45), || {
        h.state_of(&first) == "running"
    });

    let second = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "5"]);
    assert_eq!(h.state_of(&second), "queued");

    h.ok(&["cancel", &second]);
    assert_eq!(h.state_of(&second), "cancelled");

    // A job that operates must not accept `cancel`.
    let out = h.qex(&["cancel", &first]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("kill"),
        "the error must give the correct command: {err}"
    );

    h.ok(&["kill", &first, "--grace", "1s"]);
}

/// `qex clean` deletes the record of a job that stopped.
#[test]
fn clean_deletes_the_record_of_a_job_that_stopped() {
    let h = Harness::with_default_config("clean");
    let id = h.submit(&["submit", "--", "true"]);
    h.ok(&["wait", &id]);
    assert!(h.job_dir(&id).exists());

    h.ok(&["clean", &id]);
    assert!(!h.job_dir(&id).exists(), "qex did not delete the directory");
    assert!(h.list_json().is_empty());
}

/// `qex clean` must not delete the record of a job that operates.
#[test]
fn clean_refuses_a_job_that_operates() {
    let h = Harness::with_default_config("cleanrun");
    let id = h.submit(&["submit", "--", "sleep", "30"]);
    h.until("the job starts", Duration::from_secs(45), || {
        h.state_of(&id) == "running"
    });

    let out = h.qex(&["clean", &id]);
    assert!(!out.status.success(), "qex must refuse this command");
    assert!(h.job_dir(&id).exists(), "the record must stay");

    h.ok(&["kill", &id, "--grace", "1s"]);
}

/// A command that does not exist must give a clear message, and the job must
/// reach a final state. A job that stays in the state `running` would make
/// every later job wait.
#[test]
fn a_command_that_does_not_exist_gives_a_clear_message() {
    let h = Harness::with_default_config("nocmd");
    let id = h.submit(&["submit", "--", "this-program-does-not-exist"]);

    h.until("the job stops", Duration::from_secs(45), || {
        h.status_json(&id)["state"]
            .as_str()
            .map(|s| s == "failed")
            .unwrap_or(false)
    });

    // The reason is in the `error` field. A job that failed waits for nothing,
    // so `blocked_reason` is not the correct place for this text.
    let reason = h.status_json(&id)["error"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        reason.contains("this-program-does-not-exist"),
        "the message must name the program: {reason}"
    );
    assert!(
        reason.contains("PATH"),
        "the message must give the correction: {reason}"
    );
}

/// A short id is easier to copy from `qex list`, so each command accepts one.
#[test]
fn a_command_accepts_the_first_characters_of_an_id() {
    let h = Harness::with_default_config("shortid");
    let id = h.submit(&["submit", "--", "true"]);
    h.ok(&["wait", &id]);

    let short = &id[..8];
    assert_eq!(h.state_of(short), "completed");
}

/// The help text must point an agent to the topic for agents. This pointer is
/// the first text that an agent reads.
#[test]
fn the_first_screen_points_to_the_topic_for_agents() {
    let h = Harness::with_default_config("help");
    let text = h.ok(&[]);
    assert!(
        text.contains("qex help agents"),
        "the first screen must name the topic for agents"
    );

    let agents = h.ok(&["help", "agents"]);
    assert!(
        agents.contains("pgrep"),
        "the topic must warn about the pgrep fault"
    );
    assert!(
        agents.contains("qex wait"),
        "the topic must give the solution"
    );
}

/// The schemas must be valid JSON, because an agent reads them with a parser.
#[test]
fn the_schemas_are_valid_json() {
    let h = Harness::with_default_config("schema");
    for name in ["job", "status"] {
        let text = h.ok(&["schema", name]);
        serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|e| panic!("the schema `{name}` is not valid JSON: {e}"));
    }
}

/// A job file must give the same result as the options on the command line.
#[test]
fn a_job_file_describes_a_job() {
    let h = Harness::with_default_config("jobfile");
    let file = h.root.join("job.toml");
    std::fs::write(
        &file,
        "name = \"from-file\"\n\
         command = [\"sh\", \"-c\", \"echo from-the-file\"]\n\
         tags = [\"test\"]\n\
         [resources]\n\
         cpu = 2\n\
         mem = \"256MB\"\n\
         [env]\n\
         FILE_VAR = \"present\"\n",
    )
    .unwrap();

    let id = h.submit(&["submit", "--job", file.to_str().unwrap()]);
    h.ok(&["wait", &id]);

    let status = h.status_json(&id);
    assert_eq!(status["name"], "from-file");
    assert_eq!(status["cpu"], 2);
    assert_eq!(status["mem"], 256 * 1024 * 1024);
    assert_eq!(status["tags"][0], "test");
    assert!(h.ok(&["logs", &id]).contains("from-the-file"));
}

/// The word `guess` gives one half of the budget, so an agent can start a task
/// of an unknown size safely. Two such jobs must operate together.
#[test]
fn the_claim_word_guess_gives_one_half_of_the_budget() {
    let h = Harness::new(
        "guess",
        "[budget]\ncpu = \"8\"\nmem = \"4GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    let a = h.submit(&[
        "submit", "--cpu", "guess", "--mem", "guess", "--", "sleep", "300",
    ]);
    let status = h.status_json(&a);
    assert_eq!(status["cpu"], 4, "one half of 8 cores is 4 cores");
    assert_eq!(
        status["mem"],
        2u64 * 1024 * 1024 * 1024,
        "one half of 4GB is 2GB"
    );

    // A second job of the same size must operate at the same time.
    //
    // Both jobs sleep for a long time, and the test stops them when it has its
    // answer. With a short sleep the two jobs must be `running` in one window,
    // and a machine with other work can look after that window has closed.
    let b = h.submit(&[
        "submit", "--cpu", "half", "--mem", "half", "--", "sleep", "300",
    ]);
    h.until("both jobs operate", Duration::from_secs(45), || {
        h.list_json()
            .iter()
            .filter(|j| j["state"] == "running")
            .count()
            == 2
    });

    // A third job must wait, because the budget is full.
    let c = h.submit(&["submit", "--cpu", "guess", "--mem", "guess", "--", "true"]);
    assert_eq!(h.state_of(&c), "queued");

    // Give the capacity back, and the third job then operates.
    h.ok(&["kill", &a, "--grace", "1s"]);
    h.ok(&["kill", &b, "--grace", "1s"]);
    h.ok(&["wait", &c, "--timeout", "30s"]);
}

/// The word `full` gives the whole budget, so the job operates alone. qex must
/// treat it as a normal job, and it must not mark the job as forced.
#[test]
fn the_claim_word_full_gives_the_whole_budget() {
    let h = Harness::new(
        "full",
        "[budget]\ncpu = \"4\"\nmem = \"2GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    let big = h.submit(&[
        "submit", "--cpu", "full", "--mem", "max", "--", "sleep", "3",
    ]);
    let status = h.status_json(&big);
    assert_eq!(status["cpu"], 4);
    assert_eq!(status["mem"], 2u64 * 1024 * 1024 * 1024);
    assert_eq!(
        status["forced"], false,
        "a job that asks for the budget is a normal job, and qex must not force it"
    );

    h.until("the full job starts", Duration::from_secs(45), || {
        h.state_of(&big) == "running"
    });

    // Every other job must wait while this job operates.
    let other = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    assert_eq!(h.state_of(&other), "queued");

    // Give the capacity back, so the other job operates now.
    h.ok(&["kill", &big, "--grace", "1s"]);

    h.ok(&["wait", &other, "--timeout", "30s"]);
}

/// A job file must accept the claim words.
#[test]
fn a_job_file_accepts_the_claim_words() {
    let h = Harness::new(
        "guessfile",
        "[budget]\ncpu = \"8\"\nmem = \"4GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    let file = h.root.join("guess.toml");
    std::fs::write(
        &file,
        "command = [\"true\"]\n[resources]\ncpu = \"guess\"\nmem = \"half\"\n",
    )
    .unwrap();

    let id = h.submit(&["submit", "--job", file.to_str().unwrap()]);
    let status = h.status_json(&id);
    assert_eq!(status["cpu"], 4);
    assert_eq!(status["mem"], 2u64 * 1024 * 1024 * 1024);
    h.ok(&["wait", &id, "--timeout", "30s"]);
}

/// A supervisor that stops must not leave the job process alive.
///
/// Without this rule, a job continues and uses memory and cores, the budget
/// shows that memory as free, and no qex command can stop the job, because its
/// record says that the job stopped.
#[test]
fn a_dead_supervisor_does_not_leave_the_job_alive() {
    let h = Harness::with_default_config("orphan");
    let id = h.submit(&["submit", "--", "sleep", "120"]);

    h.until("the job starts", Duration::from_secs(45), || {
        h.state_of(&id) == "running"
    });

    let job_pid = h.status_json(&id)["pid"].as_i64().unwrap() as i32;
    let supervisor_pid = h.status_json(&id)["supervisor_pid"]
        .as_i64()
        .expect("the status must record the supervisor") as i32;

    // Stop the supervisor only. The job continues at this moment.
    unsafe {
        libc::kill(supervisor_pid, libc::SIGKILL);
    }

    h.until(
        "the job reaches a final state",
        Duration::from_secs(30),
        || {
            h.status_json(&id)["state"]
                .as_str()
                .map(|s| s != "running" && s != "starting")
                .unwrap_or(false)
        },
    );

    // The job process must stop. A record that says the job stopped, with the
    // job still alive, is the worst result: the memory is in use and no command
    // can free it.
    h.until("the job process stops", Duration::from_secs(30), || {
        let rc = unsafe { libc::kill(job_pid, 0) };
        rc != 0
    });

    // The budget must show the capacity as free again.
    let info = h.ok(&["info", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(v["cpu_claimed"].as_u64(), Some(0));
}

/// A job that stops at the same moment as its time limit must keep its true
/// result. A job that succeeded must never get the state `timeout`.
#[test]
fn a_job_that_stops_at_its_time_limit_keeps_its_result() {
    let h = Harness::with_default_config("timerrace");

    // Start many jobs whose length is near the limit, to meet the moment in
    // which the timer and the job both finish.
    let mut ids = Vec::new();
    for i in 0..12 {
        let sleep = format!("0.{:03}", 995 + i);
        ids.push(h.submit(&["submit", "--timeout", "1s", "--", "sleep", &sleep]));
    }

    for id in &ids {
        // Not `ok`: a job that reaches its limit gives the code 125, and that
        // is the case that this test looks for. On a fast machine each job
        // finishes first and the code is 0. Both are correct, and the test is
        // about the RECORD, which must never say `timeout` and `exit code 0`
        // together.
        h.qex(&["wait", id, "--timeout", "60s"]);
        let s = h.status_json(id);
        let state = s["state"].as_str().unwrap();
        let code = s["exit_code"].as_i64();

        // A record that says `timeout` with the exit code 0 is self
        // contradictory. A reader cannot tell what happened.
        if state == "timeout" {
            assert_ne!(
                code,
                Some(0),
                "the job stopped with the code 0, so its state must not be `timeout`: {s}"
            );
        }
        if code == Some(0) {
            assert_eq!(
                state, "completed",
                "a job that stopped with the code 0 must be `completed`: {s}"
            );
        }
    }
}

/// `qex logs` must show the output of a job that writes bytes which are not
/// UTF-8. A build in a different language, or a program that writes a byte from
/// a binary file, gives such output.
///
/// Without this rule, the command writes nothing and gives the code 0, and a
/// reader believes that the job wrote nothing.
#[test]
fn logs_shows_output_that_is_not_utf8() {
    let h = Harness::with_default_config("badbytes");
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        r#"printf 'FIRST-LINE\n'; printf 'BAD\377\376\n'; printf 'LAST-LINE\n'"#,
    ]);
    h.ok(&["wait", &id, "--timeout", "30s"]);

    let logs = h.ok(&["logs", &id]);
    assert!(
        logs.contains("FIRST-LINE") && logs.contains("LAST-LINE"),
        "one byte that is not UTF-8 hid the whole output: {logs:?}"
    );

    // The JSON output must show the same text.
    let json = h.ok(&["logs", &id, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(v["stdout"].as_str().unwrap().contains("LAST-LINE"));
}

/// A command that does not exist must give its reason in the `error` field.
/// The field `blocked_reason` says why a job waits, and a job that failed waits
/// for nothing.
#[test]
fn a_spawn_failure_uses_the_error_field() {
    let h = Harness::with_default_config("spawnfail");
    let id = h.submit(&["submit", "--", "this-program-does-not-exist"]);

    h.until("the job stops", Duration::from_secs(45), || {
        h.state_of(&id) == "failed"
    });

    let s = h.status_json(&id);
    assert!(
        s["error"]
            .as_str()
            .unwrap_or("")
            .contains("this-program-does-not-exist"),
        "the error field must name the program: {s}"
    );
    assert!(
        s["blocked_reason"].is_null(),
        "a job that failed waits for nothing: {s}"
    );
}

/// Every queued job must say why it waits, and not the first job only.
#[test]
fn every_queued_job_gives_a_reason() {
    let h = Harness::new(
        "reasons",
        "[budget]\ncpu = \"2\"\nmem = \"2GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    let running = h.submit(&[
        "submit", "--cpu", "2", "--mem", "64MB", "--", "sleep", "300",
    ]);
    h.until("the first job starts", Duration::from_secs(45), || {
        h.state_of(&running) == "running"
    });

    let a = h.submit(&["submit", "--cpu", "2", "--mem", "64MB", "--", "true"]);
    let b = h.submit(&["submit", "--cpu", "2", "--mem", "64MB", "--", "true"]);

    // Give the scheduler one cycle to write the reasons.
    h.until("both jobs give a reason", Duration::from_secs(45), || {
        let ra = h.status_json(&a)["blocked_reason"]
            .as_str()
            .map(String::from);
        let rb = h.status_json(&b)["blocked_reason"]
            .as_str()
            .map(String::from);
        ra.is_some() && rb.is_some()
    });

    h.ok(&["kill", &running, "--grace", "1s"]);
}

/// The status must record what the job ran. Without these fields, a reader must
/// open a file in the state directory to learn the command.
#[test]
fn the_status_records_the_command_and_the_directory() {
    let h = Harness::with_default_config("cmdfield");
    let id = h.submit(&["submit", "--", "echo", "hello", "world"]);
    h.ok(&["wait", &id, "--timeout", "30s"]);

    let s = h.status_json(&id);
    let command: Vec<String> = serde_json::from_value(s["command"].clone()).unwrap();
    assert_eq!(command, vec!["echo", "hello", "world"]);
    assert!(!s["cwd"].as_str().unwrap().is_empty());
}

/// `qex info --no-start` must not start a coordinator.
///
/// A script that stops the coordinator needs this option. Without it, the
/// command starts the process that the script wants to stop.
#[test]
fn info_can_test_for_a_coordinator_without_starting_one() {
    let h = Harness::with_default_config("nostart");

    let out = h.qex(&["info", "--no-start", "--json"]);
    assert!(!out.status.success(), "there is no coordinator yet");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["running"], false);

    // No coordinator may exist after that command.
    assert!(
        !h.root.join("state/qex/run/s").exists(),
        "the command started a coordinator"
    );

    // With a coordinator, the same command reports it.
    let id = h.submit(&["submit", "--", "true"]);
    h.ok(&["wait", &id, "--timeout", "30s"]);
    let out = h.qex(&["info", "--no-start", "--json"]);
    assert!(out.status.success());
}

/// A claim of zero cores must give an error. qex must not change the number in
/// silence, and a claim of zero would let qex start jobs with no limit.
#[test]
fn a_claim_of_zero_cores_is_refused() {
    let h = Harness::with_default_config("zeroclaim");
    let out = h.qex(&["submit", "--cpu", "0", "--", "true"]);
    assert!(
        !out.status.success(),
        "qex must refuse a claim of zero cores"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("1 core"),
        "the error must give the correction: {err}"
    );
}

/// The same fault must give the same exit code, whatever the form of the name.
#[test]
fn an_unknown_job_gives_the_same_code_for_each_form_of_the_name() {
    let h = Harness::with_default_config("codes");
    for name in ["3f5a1c2e-0000-4000-8000-000000000000", "not-a-uuid"] {
        for command in ["status", "wait"] {
            let out = h.qex(&[command, name]);
            assert_eq!(
                out.status.code(),
                Some(127),
                "`qex {command} {name}` must give the code 127"
            );
        }
    }
}

/// A stage that fails must stop the stages after it, and each of those stages
/// must name the stage that failed.
///
/// This behaviour is the reason for the feature. A pipeline in one script gives
/// one exit code and one log file with every stage mixed together.
#[test]
fn a_failed_stage_stops_the_stages_after_it() {
    let h = Harness::with_default_config("pipeline");

    let build = h.submit(&[
        "submit",
        "--name",
        "build",
        "--",
        "sh",
        "-c",
        "echo compiling; echo 'error: undefined symbol' >&2; exit 2",
    ]);
    // Use the ids. The build can fail before the next submit, and a name must
    // give a job that has not stopped.
    let test = h.submit(&["submit", "--name", "test", "--needs", &build, "--", "true"]);
    let ship = h.submit(&["submit", "--name", "ship", "--needs", &test, "--", "true"]);

    // The last stage must give the code 126: it did not run.
    let out = h.qex(&["wait", &ship, "--timeout", "60s"]);
    assert_eq!(out.status.code(), Some(126));

    assert_eq!(h.state_of(&build), "failed");
    assert_eq!(h.state_of(&test), "skipped");
    assert_eq!(h.state_of(&ship), "skipped");

    // The last stage must name the FIRST stage that failed, and not the stage
    // before it. A reader of the last stage thus learns the true cause.
    let s = h.status_json(&ship);
    assert_eq!(
        s["caused_by"].as_str(),
        Some(build.as_str()),
        "the last stage must name the build, and not the test: {s}"
    );
    assert!(
        s["error"].as_str().unwrap_or("").contains("build"),
        "the reason must name the stage that failed: {s}"
    );

    // There must be one failure only, so a reader finds the cause at once.
    let failed = h
        .list_json()
        .iter()
        .filter(|j| j["state"] == "failed")
        .count();
    assert_eq!(failed, 1, "a pipeline must report one failure only");

    // The log of the stage that failed must hold its output only.
    let logs = h.ok(&["logs", &build]);
    assert!(logs.contains("undefined symbol"));
}

/// Each stage of a pipeline that succeeds must run, in order.
#[test]
fn a_pipeline_that_succeeds_runs_each_stage_in_order() {
    let h = Harness::with_default_config("pipeok");

    let build = h.submit(&["submit", "--name", "build", "--", "sh", "-c", "echo one"]);
    let test = h.submit(&[
        "submit", "--name", "test", "--needs", &build, "--", "sh", "-c", "echo two",
    ]);
    let ship = h.submit(&[
        "submit",
        "--name",
        "ship",
        "--needs",
        &test,
        "--",
        "sh",
        "-c",
        "echo three",
    ]);

    let out = h.qex(&["wait", &ship, "--timeout", "60s"]);
    assert_eq!(out.status.code(), Some(0));

    for id in [&build, &test, &ship] {
        assert_eq!(h.state_of(id), "completed");
    }

    // Each stage must start after the stage before it stopped.
    let s1 = h.status_json(&build);
    let s3 = h.status_json(&ship);
    assert!(
        s3["started_at"].as_u64().unwrap() >= s1["finished_at"].as_u64().unwrap(),
        "the last stage started before the first stage stopped"
    );

    // `qex list` must show the stages in the order of submission.
    let names: Vec<String> = h
        .list_json()
        .iter()
        .map(|j| j["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["build", "test", "ship"]);
}

/// `--after` controls the order only. Such a job runs also when the job before
/// it fails. A cleanup step needs this behaviour.
#[test]
fn an_after_job_runs_when_the_job_before_it_fails() {
    let h = Harness::with_default_config("afterjob");

    let build = h.submit(&["submit", "--name", "build", "--", "sh", "-c", "exit 3"]);
    let cleanup = h.submit(&[
        "submit",
        "--name",
        "cleanup",
        "--after",
        &build,
        "--",
        "sh",
        "-c",
        "echo cleaned",
    ]);

    let out = h.qex(&["wait", &cleanup, "--timeout", "60s"]);
    assert_eq!(out.status.code(), Some(0), "an --after job must run");
    assert_eq!(h.state_of(&build), "failed");
    assert_eq!(h.state_of(&cleanup), "completed");
    assert!(h.ok(&["logs", &cleanup]).contains("cleaned"));
}

/// A job that waits for a different job must not hold capacity.
///
/// Without this rule, one long chain of jobs stops every job behind it.
#[test]
fn a_job_that_waits_for_another_job_does_not_hold_capacity() {
    let h = Harness::new(
        "depcapacity",
        "[budget]\ncpu = \"2\"\nmem = \"2GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    // A long job, and a job that waits for it.
    let slow = h.submit(&[
        "submit", "--name", "slow", "--cpu", "1", "--mem", "64MB", "--", "sleep", "4",
    ]);
    let waiter = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--needs", &slow, "--", "true",
    ]);

    // A job with no dependency must not wait for the job above it.
    let free = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    let out = h.qex(&["wait", &free, "--timeout", "20s"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a job with no dependency must not wait for a job that has one"
    );

    h.ok(&["wait", &waiter, "--timeout", "60s"]);
    h.ok(&["wait", &slow, "--timeout", "60s"]);
}

/// A dependency must exist at the submission.
///
/// This rule makes a circle of dependencies impossible, and it gives the error
/// at once, and not when the job waits with no end.
#[test]
fn a_dependency_that_does_not_exist_is_refused() {
    let h = Harness::with_default_config("nodep");
    let out = h.qex(&["submit", "--needs", "no-such-job", "--", "true"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no-such-job"),
        "the error must name the value: {err}"
    );
}

/// `qex clean` must keep a job that a job in the queue needs. Without that
/// record, the job that waits cannot report why it did not run.
#[test]
fn clean_keeps_a_job_that_another_job_needs() {
    let h = Harness::with_default_config("cleandep");

    let first = h.submit(&[
        "submit",
        "--name",
        "first",
        "--",
        "sh",
        "-c",
        "sleep 2; exit 1",
    ]);
    let second = h.submit(&["submit", "--needs", &first, "--", "true"]);

    let out = h.qex(&["clean", &first]);
    assert!(!out.status.success(), "qex must keep this job");

    // The second job does not run, so `wait` gives the code 126.
    let out = h.qex(&["wait", &second, "--timeout", "60s"]);
    assert_eq!(out.status.code(), Some(126));
    assert_eq!(h.state_of(&second), "skipped");
}

/// A state name must operate in place of a job id, so `qex clean completed`
/// gives the same result as `qex clean --state completed`.
#[test]
fn clean_accepts_a_state_name() {
    let h = Harness::with_default_config("cleanword");

    let good = h.submit(&["submit", "--", "true"]);
    let bad = h.submit(&["submit", "--", "false"]);
    h.ok(&["wait", &good, "--timeout", "30s"]);
    h.qex(&["wait", &bad, "--timeout", "30s"]);

    h.ok(&["clean", "completed"]);

    let states: Vec<String> = h
        .list_json()
        .iter()
        .map(|j| j["state"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(states, vec!["failed"], "qex deleted the wrong jobs");
}

/// A job file must accept the dependencies.
#[test]
fn a_job_file_accepts_dependencies() {
    let h = Harness::with_default_config("depfile");
    let first = h.submit(&["submit", "--name", "first", "--", "sh", "-c", "exit 1"]);

    let file = h.root.join("second.toml");
    std::fs::write(
        &file,
        format!("command = [\"true\"]\nname = \"second\"\nneeds = [\"{first}\"]\n"),
    )
    .unwrap();

    let second = h.submit(&["submit", "--job", file.to_str().unwrap()]);
    let out = h.qex(&["wait", &second, "--timeout", "60s"]);
    assert_eq!(out.status.code(), Some(126));
    assert_eq!(h.state_of(&second), "skipped");
    assert_eq!(
        h.status_json(&second)["caused_by"].as_str(),
        Some(first.as_str())
    );
}

/// A dependency that names no job must be refused.
///
/// A value with the form of a UUID gave no error before, and qex dropped the
/// dependency. The job then started with no wait and no warning, and a pipeline
/// reported success although the order was wrong.
#[test]
fn a_dependency_with_an_unknown_uuid_is_refused() {
    let h = Harness::with_default_config("depuuid");
    let out = h.qex(&[
        "submit",
        "--needs",
        "11111111-2222-3333-4444-555555555555",
        "--",
        "true",
    ]);
    assert!(
        !out.status.success(),
        "qex must refuse an unknown dependency"
    );
    assert!(h.list_json().is_empty(), "qex must not accept the job");
}

/// An id and a name have different rules.
///
/// An id names one job for ever, so its existence is enough. A name can give a
/// job of an earlier run, so it must give a job that has not stopped.
///
/// This difference is what keeps a pipeline script correct. A script that keeps
/// each id can submit its last stage even when the first stage already failed.
#[test]
fn a_name_must_be_live_but_an_id_need_only_exist() {
    let h = Harness::with_default_config("depnameid");
    let first = h.submit(&["submit", "--name", "build", "--", "true"]);
    h.ok(&["wait", &first, "--timeout", "45s"]);
    assert_eq!(h.state_of(&first), "completed");

    // By NAME: refused, because the name can give a job of an earlier run.
    for option in ["--needs", "--after"] {
        let out = h.qex(&["submit", option, "build", "--", "true"]);
        assert!(
            !out.status.success(),
            "{option} with a name that gives a job which stopped must be refused"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("already stopped"),
            "the error must say that the job stopped: {err}"
        );
        assert!(
            err.contains("earlier run"),
            "the error must name the usual cause: {err}"
        );
    }

    // By ID: accepted, because an id names one job for ever.
    for option in ["--needs", "--after"] {
        let out = h.qex(&["submit", option, &first, "--", "true"]);
        assert!(
            out.status.success(),
            "{option} with an id must be accepted: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A dependency that did NOT succeed is accepted, and it makes this job
/// `skipped`.
///
/// A script that submits the stages one at a time must be able to finish when
/// an early stage already failed. Without this rule, the script stops in the
/// middle and leaves a pipeline with no end stages.
#[test]
fn a_dependency_that_failed_is_accepted_and_makes_the_job_skipped() {
    let h = Harness::with_default_config("depfailed");
    let first = h.submit(&["submit", "--name", "first", "--", "false"]);
    h.qex(&["wait", &first, "--timeout", "30s"]);
    assert_eq!(h.state_of(&first), "failed");

    // Use the id. A name would be refused, because the job already stopped.
    let second = h.submit(&["submit", "--needs", &first, "--", "true"]);
    let out = h.qex(&["wait", &second, "--timeout", "45s"]);
    assert_eq!(out.status.code(), Some(126));
    assert_eq!(h.state_of(&second), "skipped");
    assert_eq!(
        h.status_json(&second)["caused_by"].as_str(),
        Some(first.as_str())
    );
}

/// A job whose dependency fails must be marked at once, and not wait for the
/// capacity of the jobs in front of it.
///
/// The dependency test and the capacity test are separate passes for this
/// reason. In one pass, the walk stops at the first job that waits for
/// capacity, and a job behind it is never tested. Such a job stays in the queue
/// for ever and `qex wait` never gives an answer.
#[test]
fn a_failed_dependency_is_seen_behind_a_blocked_queue() {
    let h = Harness::new(
        "depblocked",
        "[budget]\ncpu = \"2\"\nmem = \"2GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    // This job operates now, and it fails soon.
    let failer = h.submit(&[
        "submit",
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--name",
        "failer",
        "--",
        "sh",
        "-c",
        "sleep 2; exit 1",
    ]);
    // This job holds the rest of the budget while the test operates.
    let blocker = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--name", "blocker", "--", "sleep", "60",
    ]);

    // Wait for a condition that CANNOT go false again.
    //
    // This test waited for both jobs to be `running` together. `failer` lives
    // for two seconds, so that window is short, and on a machine with other
    // work the test looked after the window had closed. The condition was then
    // false for ever and the test waited to its limit — the exact fault that
    // this tool exists to remove.
    //
    // `failer` must only have STARTED, because the test needs it to fail; and
    // `blocker` must hold its core, which it does for 60 seconds.
    h.until("both jobs started", Duration::from_secs(45), || {
        h.has_started(&failer) && h.state_of(&blocker) == "running"
    });

    // This job needs two cores, so it waits for capacity at the front.
    h.submit(&[
        "submit", "--cpu", "2", "--mem", "64MB", "--name", "mid", "--", "true",
    ]);
    // This job is behind the one above, and its dependency is about to fail.
    let skipped = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--needs", &failer, "--", "true",
    ]);

    // The job must reach `skipped` while `mid` still waits for capacity.
    h.until("the job is skipped", Duration::from_secs(45), || {
        h.state_of(&skipped) == "skipped"
    });

    let out = h.qex(&["wait", &skipped, "--timeout", "10s"]);
    assert_eq!(out.status.code(), Some(126));

    h.ok(&["kill", &blocker, "--grace", "1s"]);
}

/// A job that operates must say `running`, and it must give its pid.
///
/// # The fault that this test holds
///
/// Two processes wrote one record. The coordinator wrote `starting` and the
/// process id of the supervisor AFTER it started the supervisor, and the
/// supervisor wrote `running` and the process id of the job. The supervisor
/// frequently won that race, and the write from the coordinator then returned
/// the record to `starting`.
///
/// The supervisor does not write again until the job stops, so a job of five
/// minutes said `starting` for five minutes. `qex top` could measure nothing,
/// `qex kill` refused the job with "the job starts now. Try the command again.",
/// and the tests of qex failed on a busy machine about one time in six.
///
/// The record now has ONE writer at each moment.
#[test]
fn a_job_that_operates_says_running_and_gives_its_pid() {
    let h = Harness::with_default_config("onewriter");

    // Several jobs together, because the fault is a race and one job hides it.
    let ids: Vec<String> = (0..6)
        .map(|_| {
            h.submit(&[
                "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
            ])
        })
        .collect();

    for id in &ids {
        h.until("the job says running", Duration::from_secs(45), || {
            h.state_of(id) == "running"
        });

        // The pid must arrive with the state. A job that operates with no pid
        // cannot be measured and cannot be stopped.
        let status = h.status_json(id);
        assert!(
            status["pid"].as_i64().is_some(),
            "a job that operates must give its pid: {status}"
        );

        // The record must not return to `starting` after that. This is the
        // write that the coordinator used to make.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            h.state_of(id),
            "running",
            "the record must not return to an earlier state"
        );

        // `qex kill` must accept the job. It refused a job with no pid.
        h.ok(&["kill", id, "--grace", "1s"]);
    }
}

/// `qex logs` with a job that does not exist must not give the code 0 with no
/// output. A reader could not separate "this job wrote nothing" from "this job
/// does not exist".
#[test]
fn every_command_gives_one_code_for_a_job_that_does_not_exist() {
    let h = Harness::with_default_config("codes2");
    let unknown = "11111111-2222-3333-4444-555555555555";

    for command in ["status", "wait", "logs", "kill", "cancel"] {
        let out = h.qex(&[command, unknown]);
        assert_eq!(
            out.status.code(),
            Some(127),
            "`qex {command}` must give the code 127 for a job that does not exist"
        );
    }
}

/// The record of a job that failed must not send the reader to a log file that
/// no longer exists.
#[test]
fn clean_keeps_the_cause_readable_for_the_jobs_that_it_leaves() {
    let h = Harness::with_default_config("cleancause");

    let first = h.submit(&[
        "submit",
        "--name",
        "first",
        "--",
        "sh",
        "-c",
        "sleep 1; exit 1",
    ]);
    let second = h.submit(&[
        "submit", "--name", "second", "--needs", &first, "--", "true",
    ]);

    h.until("the second job is skipped", Duration::from_secs(45), || {
        h.state_of(&second) == "skipped"
    });

    // Delete the job that failed. The record of the second job must still
    // answer the question "why did this job not run".
    h.ok(&["clean", &first]);

    let s = h.status_json(&second);
    let error = s["error"].as_str().unwrap_or("");
    assert!(
        error.contains("first"),
        "the record must still name the job that failed: {error}"
    );
    assert!(
        !error.contains("qex logs"),
        "the record must not send the reader to a log that is deleted: {error}"
    );
    assert!(s["caused_by"].is_null(), "the id points at nothing now");
}

/// `qex logs` must not write a very large file to the terminal by default.
/// The reader is frequently an agent with a limited context.
#[test]
fn logs_shows_the_last_lines_by_default() {
    let h = Harness::with_default_config("logcap");
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "i=0; while [ $i -lt 2000 ]; do echo line-$i; i=$((i+1)); done",
    ]);
    h.ok(&["wait", &id, "--timeout", "45s"]);

    let result = h.qex(&["logs", &id, "--stdout"]);
    let out = String::from_utf8_lossy(&result.stdout);
    let notice = String::from_utf8_lossy(&result.stderr);
    assert!(
        out.lines().count() < 600,
        "the default output must be short, and it had {} lines",
        out.lines().count()
    );
    assert!(out.contains("line-1999"), "the last line must be there");
    // The notice goes to stderr, so stdout holds the log lines only.
    assert!(
        notice.contains("not shown"),
        "qex must say that it hid the earlier lines: {notice}"
    );

    // The option `--all` must give every line.
    let full = h.ok(&["logs", &id, "--stdout", "--all"]);
    assert!(full.contains("line-0"), "--all must give the first line");
    assert!(full.lines().count() >= 2000);
}

/// `qex status` of a job that failed must also give the last lines of its
/// standard error, with no option at all.
///
/// A reader of a failure always wants those lines. Without them, every failure
/// costs two commands and two answers.
#[test]
fn the_status_of_a_job_that_failed_holds_its_error_output() {
    let h = Harness::with_default_config("statuslogs");
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "echo normal; echo 'BOOM: it broke' >&2; exit 3",
    ]);
    h.qex(&["wait", &id, "--timeout", "45s"]);

    // No option at all.
    let text = h.ok(&["status", &id]);
    assert!(
        text.contains("BOOM: it broke"),
        "the status must hold the error output: {text}"
    );

    // The JSON output holds one field for each stream that has content.
    let v = h.status_json(&id);
    assert!(v["logs"]["stderr"]["text"]
        .as_str()
        .unwrap()
        .contains("BOOM"));

    // A job that succeeded gives no output, because the reader did not ask.
    let good = h.submit(&["submit", "--", "sh", "-c", "echo quiet"]);
    h.ok(&["wait", &good, "--timeout", "45s"]);
    assert!(h.status_json(&good)["logs"].is_null());

    // The option --no-logs removes the output.
    let text = h.ok(&["status", &id, "--no-logs"]);
    assert!(!text.contains("BOOM"), "--no-logs must remove the output");
}

/// The status of a job that failed must give BOTH streams.
///
/// A test program writes its failure summary to the standard error and its
/// result to the standard output. The standard error alone reads as a complete
/// failure, and the reader then needs a second command to learn what really
/// happened.
#[test]
fn the_status_of_a_failure_gives_both_streams() {
    let h = Harness::with_default_config("bothstreams");
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "echo 'exact 27793/27793 OK'; echo 'FAIL: 64087 mismatched' >&2; exit 1",
    ]);
    h.qex(&["wait", &id, "--timeout", "45s"]);

    let text = h.ok(&["status", &id]);
    assert!(
        text.contains("FAIL: 64087"),
        "the status must hold the error output: {text}"
    );
    assert!(
        text.contains("27793/27793"),
        "the status must also hold the standard output, or the reader sees a \
         failure with no result: {text}"
    );

    // The JSON output holds one field for each stream.
    let v = h.status_json(&id);
    assert!(v["logs"]["stderr"]["text"]
        .as_str()
        .unwrap()
        .contains("FAIL"));
    assert!(v["logs"]["stdout"]["text"]
        .as_str()
        .unwrap()
        .contains("27793"));

    // A reader that names one stream gets that stream only.
    let only = h.ok(&["status", &id, "--stderr"]);
    assert!(only.contains("FAIL"));
    assert!(
        !only.contains("27793"),
        "--stderr must give one stream only"
    );
}

/// The options that select lines must operate on both commands.
#[test]
fn the_log_options_select_the_lines() {
    let h = Harness::with_default_config("logopts");
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "i=1; while [ $i -le 300 ]; do echo line-$i; i=$((i+1)); done",
    ]);
    h.ok(&["wait", &id, "--timeout", "45s"]);

    let head = h.ok(&["logs", &id, "--stdout", "--head", "3"]);
    assert_eq!(head.lines().next().unwrap(), "line-1");
    assert_eq!(head.lines().count(), 3);

    let range = h.ok(&["logs", &id, "--stdout", "--lines", "100:102"]);
    assert_eq!(range, "line-100\nline-101\nline-102");

    let numbered = h.ok(&["logs", &id, "--stdout", "--head", "1", "--number"]);
    assert!(numbered.contains("1  line-1"), "got: {numbered}");

    // A search reports the number of matches, so a wide pattern is visible.
    // The report goes to stderr, and the lines go to stdout.
    let out = h.qex(&[
        "logs",
        &id,
        "--stdout",
        "--grep",
        "line-1[0-9]$",
        "--max-matches",
        "3",
    ]);
    let found = String::from_utf8_lossy(&out.stdout);
    let notice = String::from_utf8_lossy(&out.stderr);
    assert!(notice.contains("10 line(s) match"), "got: {notice}");
    assert!(found.contains("line-10") && found.contains("line-12"));
    assert!(!found.contains("line-13"), "the limit must hold");
    // The standard output holds the log lines only, so a file or a parser gets
    // clean data.
    for line in found.lines() {
        assert!(
            line.starts_with("line-"),
            "stdout must hold log lines only: {line}"
        );
    }

    // The same options operate on `status`.
    let s = h.ok(&["status", &id, "--stdout", "--head", "2"]);
    assert!(s.contains("line-1") && s.contains("line-2"));
    assert!(!s.contains("line-3"), "the head limit must hold in status");
}

/// `--tail N --follow` must give the last N lines and then the new lines, in
/// the same way as `tail -f -n N`. It must not write the whole file first.
#[test]
fn follow_leads_with_the_last_lines_only() {
    let h = Harness::with_default_config("followtail");
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "i=1; while [ $i -le 200 ]; do echo old-$i; i=$((i+1)); done; sleep 2; echo NEW-A",
    ]);
    h.until(
        "the job writes its first lines",
        Duration::from_secs(45),
        || {
            h.job_dir(&id)
                .join("stdout.log")
                .metadata()
                .map(|m| m.len() > 100)
                .unwrap_or(false)
        },
    );

    let out = h.ok(&["logs", &id, "--stdout", "--tail", "3", "--follow"]);
    assert!(
        out.contains("NEW-A"),
        "follow must give the new lines: {out}"
    );
    assert!(
        !out.contains("old-1\n"),
        "follow must not write the whole file"
    );
    assert!(
        out.lines().count() <= 6,
        "got {} lines",
        out.lines().count()
    );
}

/// qex must use the measurement of an earlier job of the same command as the
/// claim for the next one.
///
/// This behaviour is the reason that qex measures each job. `guess` is safe and
/// frequently far too large, and an agent should not have to tune a claim.
#[test]
fn a_second_job_of_one_command_uses_the_measurement_of_the_first() {
    let h = Harness::with_default_config("learn");

    // A command that uses a measurable quantity of memory.
    let program = [
        "sh",
        "-c",
        "head -c 40000000 /dev/zero | tail -c 1 > /dev/null",
    ];

    let first = h.submit(&[&["submit", "--name", "one", "--"], &program[..]].concat());
    h.ok(&["wait", &first, "--timeout", "60s"]);

    let s1 = h.status_json(&first);
    assert_eq!(
        s1["claim_source"], "default",
        "the first job has no measurement to use"
    );
    let used = s1["usage"]["max_rss"].as_u64().unwrap();
    assert!(used > 0);

    // The same command, with no claim from the user.
    let second = h.submit(&[&["submit", "--name", "two", "--"], &program[..]].concat());
    let s2 = h.status_json(&second);
    assert_eq!(
        s2["claim_source"], "learned",
        "the second job must use the measurement: {s2}"
    );

    let claimed = s2["mem"].as_u64().unwrap();
    assert!(
        claimed >= used,
        "the claim {claimed} must not be below the measurement {used}"
    );
    assert!(
        claimed < s1["mem"].as_u64().unwrap(),
        "the claim must be below the default, or the measurement gave nothing"
    );
    h.ok(&["wait", &second, "--timeout", "60s"]);
}

/// A claim from the user must always win over a measurement.
#[test]
fn a_claim_from_the_user_wins_over_a_measurement() {
    let h = Harness::with_default_config("learnwins");
    let first = h.submit(&["submit", "--", "true"]);
    h.ok(&["wait", &first, "--timeout", "45s"]);

    let second = h.submit(&["submit", "--cpu", "2", "--mem", "1GB", "--", "true"]);
    let s = h.status_json(&second);
    assert_eq!(s["claim_source"], "explicit");
    assert_eq!(s["cpu"], 2);
    assert_eq!(s["mem"], 1024u64 * 1024 * 1024);
    h.ok(&["wait", &second, "--timeout", "45s"]);
}

/// A job that did not complete must not become a measurement.
///
/// Such a job shows the memory that it reached before something stopped it, and
/// not the memory that it needs. A record from it would make the next claim too
/// small, and the next job would stop in the same way.
#[test]
fn a_job_that_did_not_complete_is_not_a_measurement() {
    let h = Harness::with_default_config("learnfail");
    let program = ["sh", "-c", "exit 1"];

    let first = h.submit(&[&["submit", "--"], &program[..]].concat());
    h.qex(&["wait", &first, "--timeout", "45s"]);
    assert_eq!(h.state_of(&first), "failed");

    let second = h.submit(&[&["submit", "--"], &program[..]].concat());
    assert_eq!(
        h.status_json(&second)["claim_source"],
        "default",
        "a job that failed must not become a measurement"
    );
    h.qex(&["wait", &second, "--timeout", "45s"]);
}

/// A replacement of the qex program must not stop the jobs.
///
/// A coordinator can operate for hours. During development, a new build
/// replaces the program file. The kernel then adds ` (deleted)` to the name in
/// `/proc/self/exe`, and a start of that name fails. Every job after the
/// replacement failed at once with "No such file or directory", and that
/// message named no cause.
#[test]
fn a_replacement_of_the_program_does_not_stop_the_jobs() {
    let h = Harness::with_default_config("skew");

    // Start a coordinator with a copy of the program.
    let copy = h.root.join("qex-copy");
    std::fs::copy(env!("CARGO_BIN_EXE_qex"), &copy).unwrap();

    let run = |args: &[&str], exe: &std::path::Path| -> Output {
        Command::new(exe)
            .args(args)
            .env("XDG_CONFIG_HOME", h.root.join("cfg"))
            .env("XDG_STATE_HOME", h.root.join("state"))
            .env("QEX_IDLE_EXIT_SECS", "120")
            .output()
            .expect("qex did not start")
    };

    let first = run(&["submit", "--", "true"], &copy);
    assert!(first.status.success());
    let id = String::from_utf8_lossy(&first.stdout).trim().to_string();
    run(&["wait", &id, "--timeout", "45s"], &copy);

    // Replace the program file while the coordinator operates.
    std::fs::remove_file(&copy).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_qex"), &copy).unwrap();

    // A job must still start. The coordinator holds the old code, and it starts
    // the supervisor from the program that is on the disk now.
    let after = run(&["submit", "--", "sh", "-c", "echo it-ran"], &copy);
    assert!(
        after.status.success(),
        "the submission failed after the replacement: {}",
        String::from_utf8_lossy(&after.stderr)
    );
    let id2 = String::from_utf8_lossy(&after.stdout).trim().to_string();

    let waited = run(&["wait", &id2, "--timeout", "45s"], &copy);
    assert_eq!(
        waited.status.code(),
        Some(0),
        "the job did not run after the replacement: {}",
        String::from_utf8_lossy(&waited.stderr)
    );

    // `qex info` must report the replacement, so a reader learns the cause.
    //
    // The test for the replacement reads `/proc/self/exe`, which gives the path
    // and the words `(deleted)` after somebody replaces the file. macOS has no
    // equivalent, so qex cannot report the replacement there. The part above —
    // THE JOB CONTINUES — is the part that matters, and it operates on both.
    let info = run(&["info", "--no-start", "--json"], &copy);
    let v: serde_json::Value = serde_json::from_slice(&info.stdout).unwrap();
    #[cfg(target_os = "linux")]
    assert_eq!(v["program_replaced"], true);

    if let Some(pid) = v["pid"].as_i64() {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

/// A closed pipe must not give a Rust panic.
///
/// An agent writes `qex list | head` frequently. Rust ignores SIGPIPE, so a
/// write to a closed pipe gives an error and the print macros panic on it. The
/// output then holds a panic and a note about a backtrace, which reads as a
/// fault in qex.
#[test]
fn a_closed_pipe_does_not_give_a_panic() {
    let h = Harness::with_default_config("pipe");
    for _ in 0..5 {
        h.submit(&["submit", "--", "true"]);
    }

    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("{} list | head -2", env!("CARGO_BIN_EXE_qex")))
        .env("XDG_CONFIG_HOME", h.root.join("cfg"))
        .env("XDG_STATE_HOME", h.root.join("state"))
        .env("QEX_IDLE_EXIT_SECS", "120")
        .output()
        .unwrap();

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("panicked"),
        "a closed pipe gave a panic: {err}"
    );
    assert!(!err.contains("Broken pipe"), "got: {err}");
}

/// `--id-file` must hold the id, because a shell variable does not last
/// between the commands of an agent.
#[test]
fn the_id_file_holds_the_id() {
    let h = Harness::with_default_config("idfile");
    let file = h.root.join("job.id");

    let id = h.submit(&["submit", "--id-file", file.to_str().unwrap(), "--", "true"]);
    let written = std::fs::read_to_string(&file).unwrap();
    assert_eq!(written.trim(), id, "the file must hold the id");

    // The id in the file must work in a later command.
    h.ok(&["wait", written.trim(), "--timeout", "45s"]);
}

/// An id file in a directory that does not last must give a warning.
///
/// The id is the handle to a job, and the job continues when the session of an
/// agent stops. An agent that writes the id into the scratch directory of its
/// harness thus loses the handle at the moment that it needs the handle: the
/// harness deletes that directory with the session, and the job continues with
/// no name. The file operates correctly, so this warning is the one opportunity
/// to prevent the fault.
#[test]
fn an_id_file_in_a_temporary_directory_gives_a_warning() {
    let h = Harness::with_default_config("idtmp");

    // The root of this harness is under the temporary directory of the machine.
    let out = h.qex(&[
        "submit",
        "--id-file",
        h.root.join("job.id").to_str().unwrap(),
        "--",
        "true",
    ]);
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("does not last"),
        "an id file in a temporary directory must give a warning; got: {err}"
    );

    // The warning must not reach stdout. `ID=$(qex submit ...)` must still give
    // the id and nothing else.
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        id.parse::<uuid::Uuid>().is_ok(),
        "stdout must hold the id only, and it held: {id}"
    );

    // A directory that lasts must give no warning. A warning that appears each
    // time teaches a reader to ignore it.
    let lasting = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("qex-id-file-test");
    std::fs::create_dir_all(&lasting).unwrap();
    let out = h.qex(&[
        "submit",
        "--id-file",
        lasting.join("job.id").to_str().unwrap(),
        "--",
        "true",
    ]);
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("does not last"),
        "an id file in a directory that lasts must give no warning; got: {err}"
    );
    std::fs::remove_dir_all(&lasting).ok();
}

/// The id file of a pipeline must hold every stage, in a form that a shell
/// reads and in a form that a parser reads.
#[test]
fn the_id_file_of_a_pipeline_holds_every_stage() {
    let h = Harness::with_default_config("pipeidfile");

    let pipeline = h.root.join("ci.toml");
    std::fs::write(
        &pipeline,
        "[[jobs]]\nname = \"build\"\ncommand = [\"true\"]\n\n\
         [[jobs]]\nname = \"test\"\ncommand = [\"true\"]\nneeds = [\"build\"]\n",
    )
    .unwrap();

    // The shell form.
    let env_file = h.root.join("ids.env");
    let group = h.ok(&[
        "pipeline",
        pipeline.to_str().unwrap(),
        "--id-file",
        env_file.to_str().unwrap(),
    ]);
    let text = std::fs::read_to_string(&env_file).unwrap();
    assert!(text.contains(&format!("group={group}")), "got: {text}");
    assert!(
        text.contains("build="),
        "the build stage is missing: {text}"
    );
    assert!(text.contains("test="), "the test stage is missing: {text}");

    // Every value must be a job that exists.
    for line in text.lines() {
        let (_, id) = line.split_once('=').unwrap();
        assert!(id.parse::<uuid::Uuid>().is_ok(), "not an id: {line}");
    }

    // The JSON form.
    let json_file = h.root.join("ids.json");
    h.ok(&[
        "pipeline",
        pipeline.to_str().unwrap(),
        "--id-file",
        json_file.to_str().unwrap(),
    ]);
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json_file).unwrap()).unwrap();
    assert!(v["group"].as_str().is_some());
    assert!(v["jobs"]["build"].as_str().is_some());
    assert!(v["jobs"]["test"].as_str().is_some());
}

/// `--cwd` gives one directory, and `--under` gives a directory and the
/// directories below it. Neither must reach the work of a different project.
#[test]
fn the_directory_filters_select_the_right_jobs() {
    let h = Harness::with_default_config("dirs");
    let project = h.root.join("project");
    let inner = project.join("inner");
    let other = h.root.join("other");
    for d in [&project, &inner, &other] {
        std::fs::create_dir_all(d).unwrap();
    }

    let run_in = |dir: &std::path::Path, name: &str| -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_qex"))
            .args(["submit", "--name", name, "--", "true"])
            .env("XDG_CONFIG_HOME", h.root.join("cfg"))
            .env("XDG_STATE_HOME", h.root.join("state"))
            .env("QEX_IDLE_EXIT_SECS", "120")
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let top = run_in(&project, "top");
    let deep = run_in(&inner, "deep");
    let away = run_in(&other, "away");
    for id in [&top, &deep, &away] {
        h.ok(&["wait", id, "--timeout", "45s"]);
    }

    let names = |args: &[&str], dir: &std::path::Path| -> Vec<String> {
        let out = Command::new(env!("CARGO_BIN_EXE_qex"))
            .args(args)
            .env("XDG_CONFIG_HOME", h.root.join("cfg"))
            .env("XDG_STATE_HOME", h.root.join("state"))
            .env("QEX_IDLE_EXIT_SECS", "120")
            .current_dir(dir)
            .output()
            .unwrap();
        let jobs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
        jobs.iter()
            .map(|j| j["name"].as_str().unwrap().to_string())
            .collect()
    };

    assert_eq!(names(&["list", "--json", "--cwd"], &project), vec!["top"]);
    let under = names(&["list", "--json", "--under"], &project);
    assert!(under.contains(&"top".to_string()) && under.contains(&"deep".to_string()));
    assert!(
        !under.contains(&"away".to_string()),
        "a different directory must not appear"
    );

    // A deletion must respect the same limit.
    let out = Command::new(env!("CARGO_BIN_EXE_qex"))
        .args(["clean", "--under"])
        .env("XDG_CONFIG_HOME", h.root.join("cfg"))
        .env("XDG_STATE_HOME", h.root.join("state"))
        .env("QEX_IDLE_EXIT_SECS", "120")
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(out.status.success());

    let left: Vec<String> = h
        .list_json()
        .iter()
        .map(|j| j["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(left, vec!["away"], "the other directory must stay");
}

/// `--auto` deletes a job that stopped long ago, and keeps a job that stopped
/// a moment ago. The age is the safety: a job of the last hour is frequently
/// the job that the user reads now.
#[test]
fn clean_auto_keeps_the_recent_jobs() {
    let h = Harness::with_default_config("auto");
    let id = h.submit(&["submit", "--", "true"]);
    h.ok(&["wait", &id, "--timeout", "45s"]);

    let out = h.qex(&["clean", "--auto"]);
    assert!(out.status.success());
    assert_eq!(
        h.list_json().len(),
        1,
        "a job that stopped a moment ago must stay: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// A job that a job in the queue still needs must not be deleted, whatever its
/// own state says.
///
/// The job in the queue reads that record to decide whether to run, and to
/// explain why it did not run. A deletion would take the answer away from a job
/// that has not yet asked the question.
#[test]
fn a_dependency_of_a_queued_job_is_not_finished() {
    let h = Harness::new(
        "depclean",
        "[budget]\ncpu = \"1\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    let first = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    h.ok(&["wait", &first, "--timeout", "45s"]);
    assert_eq!(h.state_of(&first), "completed");

    // Hold the budget, so the job below stays in the queue.
    let blocker = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "20"]);
    h.until("the blocker starts", Duration::from_secs(45), || {
        h.state_of(&blocker) == "running"
    });
    let second = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--needs", &first, "--", "true",
    ]);
    assert_eq!(h.state_of(&second), "queued");

    // The first job completed, and a job in the queue needs it.
    let out = h.ok(&["clean", "--all"]);
    assert!(out.contains("0 job(s)"), "nothing must go: {out}");
    assert!(
        out.contains("still needs them"),
        "the message must give the reason: {out}"
    );
    assert!(
        h.list_json().iter().any(|j| j["id"] == first),
        "the record of the first job must stay"
    );

    h.ok(&["kill", &blocker, "--grace", "1s"]);
}

/// `qex du` must say how much space qex holds.
#[test]
fn du_reports_the_space_that_qex_holds() {
    let h = Harness::with_default_config("du");
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "i=0; while [ $i -lt 500 ]; do echo padding-line-$i; i=$((i+1)); done",
    ]);
    h.ok(&["wait", &id, "--timeout", "45s"]);

    let text = h.ok(&["du", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(v["total_bytes"].as_u64().unwrap() > 0);
    assert!(v["jobs_bytes"].as_u64().unwrap() > 0);
    assert_eq!(v["largest"][0]["id"].as_str(), Some(id.as_str()));
}

/// A deep directory must not stop qex. The socket path must fit in `sun_path`,
/// and a test harness gives a long path.
#[test]
fn a_long_runtime_directory_still_works() {
    let long = std::env::temp_dir()
        .join(format!("qex-long-{}", std::process::id()))
        .join("a-directory-with-a-very-long-name")
        .join("another-directory-with-a-long-name")
        .join("and-one-more-to-pass-the-limit-of-sun-path");

    let h = Harness::with_default_config("longpath");
    std::fs::create_dir_all(&long).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_qex"))
        .args(["submit", "--", "echo", "long-path-ok"])
        .env("XDG_CONFIG_HOME", h.root.join("cfg"))
        .env("XDG_STATE_HOME", h.root.join("state"))
        .env("XDG_RUNTIME_DIR", &long)
        .env("QEX_IDLE_EXIT_SECS", "120")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "qex failed with a long runtime directory: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let wait = Command::new(env!("CARGO_BIN_EXE_qex"))
        .args(["wait", &id, "--timeout", "30s"])
        .env("XDG_CONFIG_HOME", h.root.join("cfg"))
        .env("XDG_STATE_HOME", h.root.join("state"))
        .env("XDG_RUNTIME_DIR", &long)
        .env("QEX_IDLE_EXIT_SECS", "120")
        .output()
        .unwrap();
    assert_eq!(wait.status.code(), Some(0));

    // Stop the coordinator of this test.
    let info = Command::new(env!("CARGO_BIN_EXE_qex"))
        .args(["info", "--json"])
        .env("XDG_CONFIG_HOME", h.root.join("cfg"))
        .env("XDG_STATE_HOME", h.root.join("state"))
        .env("XDG_RUNTIME_DIR", &long)
        .env("QEX_IDLE_EXIT_SECS", "120")
        .output()
        .unwrap();
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&info.stdout) {
        if let Some(pid) = v["pid"].as_i64() {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
    std::fs::remove_dir_all(long.ancestors().nth(3).unwrap()).ok();
}

/// The measured use must appear in the status, so an agent can correct its
/// claim. This value is the feedback that makes the next claim more accurate.
#[test]
fn the_status_gives_the_measured_use_of_a_job() {
    let h = Harness::with_default_config("usage");
    let id = h.submit(&[
        "submit",
        "--mem",
        "512MB",
        "--",
        "sh",
        "-c",
        "head -c 8000000 /dev/zero > /dev/null",
    ]);
    h.ok(&["wait", &id]);

    let status = h.status_json(&id);
    let rss = status["usage"]["max_rss"].as_u64().unwrap();
    assert!(rss > 0, "qex must measure the memory of a job");
    assert!(
        rss < 512 * 1024 * 1024,
        "the measurement {rss} is larger than the claim, so the unit is wrong"
    );
}

/// A job that starts again must never show the state of the attempt that
/// failed.
///
/// The supervisor wrote the record two times: `failed`, and then `queued` a
/// moment later. A reader between the two writes saw a state that the job never
/// reached, and the coordinator is such a reader. It keeps the state that it
/// reads and it stops reading a job that stopped, so it kept `failed` for a job
/// that continued, and it kept it for ever.
///
/// The measured result was a coordinator that reported `failed` while the job
/// operated, and `qex wait` that gave the result of an attempt that was not the
/// last one. Every rule that asks "did this job stop?" then receives the wrong
/// answer.
///
/// This test reads the record and the list many times while the job starts
/// again. Neither must ever say that the job stopped.
#[test]
fn a_job_that_starts_again_never_shows_the_attempt_that_failed() {
    let h = Harness::with_default_config("retrylatch");

    // The first attempt fails, and the second one takes long enough for the
    // test to read the record many times.
    let counter = h.root.join("attempts");
    let script = format!(
        "n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; \
         if [ $n -lt 2 ]; then exit 3; fi; sleep 5",
        c = counter.display()
    );
    let id = h.submit(&["submit", "--retries", "4", "--", "sh", "-c", &script]);

    // Read the record FILE, and not `qex status`.
    //
    // The window between the two writes is short. A reader that starts a
    // process for each sample takes far longer than the window, so it would
    // pass whether the fault is there or not. A file read takes microseconds
    // and it samples the same record that the coordinator reads.
    let record = h.root.join("state/qex/jobs").join(&id).join("status.json");

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut saw_a_later_attempt = false;
    let mut samples = 0u64;
    loop {
        if let Ok(text) = std::fs::read_to_string(&record) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                samples += 1;
                let state = v["state"].as_str().unwrap_or("");
                let attempts = v["attempts"].as_u64().unwrap_or(0);

                // The final attempt succeeds, so the only terminal state that
                // this record may ever hold is `completed`.
                assert_ne!(
                    state, "failed",
                    "the record must never hold the state of an attempt that starts \
                     again; it held `failed` at attempt {attempts}"
                );

                if state == "running" && attempts > 1 {
                    saw_a_later_attempt = true;
                }
                if state == "completed" {
                    break;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "the job did not finish after {samples} samples"
        );
    }

    assert!(
        saw_a_later_attempt,
        "the test must see an attempt after the first one, or it tests nothing"
    );
    assert!(
        samples > 100,
        "the test must sample the record often enough to meet the window; it took \
         {samples} samples"
    );

    // The result is the result of the LAST attempt.
    let status = h.status_json(&id);
    assert_eq!(status["state"], "completed", "got: {status}");
    assert_eq!(status["attempts"], 2, "got: {status}");
    assert_eq!(status["exit_code"], 0, "got: {status}");
}

/// The record of a job must hold the SHORT form of a config fault.
///
/// # Why this test starts real processes
///
/// `Config::load` gives a long message: it names the version of this build, it
/// explains that a coordinator holds the code that started it, and it gives
/// three numbered steps for an upgrade. That is correct for a person whose
/// command stopped at a terminal.
///
/// The supervisor of a job must NOT ask for that form. It puts the fault in the
/// record of the job, and `qex status` prints that record, so the long message
/// would fill the `error:` field of a job that already ran with advice about an
/// upgrade — which reads as a fault in qex, and which hides the words that
/// matter there: that no limit operates.
///
/// A unit test on the message alone cannot hold this. The message is a pure
/// function of its arguments, so a test of it passes whichever form the
/// supervisor asks for. `src/supervisor.rs` calling `Config::load()` instead of
/// `Config::load_short()` brings the fault back, and only a test that
/// reads a real `qex status` sees it.
///
/// # Why the job starts in this state at all
///
/// `qex submit` reads the config file, so it stops while the file holds a field
/// that qex does not know. The job must therefore be in the QUEUE before the
/// file changes. The budget of one core holds it there, and the end of the job
/// that occupies the budget releases it.
#[test]
fn a_config_fault_in_the_record_of_a_job_stays_short() {
    let good = "[budget]\ncpu = \"1\"\nmem = \"2GB\"\n\
                [peers]\nenabled = false\n\
                [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n";
    let h = Harness::new("cfgrec", good);

    // One core of budget, and one job that takes it. The next job waits.
    let occupier = h.submit(&[
        "submit", "--cpu", "1", "--mem", "128MB", "--", "sleep", "300",
    ]);
    h.until("the first job operates", Duration::from_secs(45), || {
        h.has_started(&occupier)
    });

    let victim = h.submit(&["submit", "--cpu", "1", "--mem", "128MB", "--", "true"]);
    assert_eq!(
        h.state_of(&victim),
        "queued",
        "the budget of one core must hold the second job in the queue"
    );

    // NOW the file gets a field that this qex does not know. The coordinator
    // keeps the values that it read when it started, so it still schedules the
    // job; the supervisor of that job is a new process, and it reads this.
    h.write_config(&format!("{good}\n[hooks]\non_stop = [\"true\"]\n"));

    // Release the budget. The waiting job then starts under the broken file.
    h.qex(&["kill", &occupier, "--grace", "1s"]);
    h.until("the second job stops", Duration::from_secs(60), || {
        h.state_of(&victim) == "completed"
    });

    let status = h.ok(&["status", &victim]);

    // The test must prove that the supervisor MET the fault. Without this, a
    // run in which the supervisor read a correct file would pass this test
    // while measuring nothing at all.
    assert!(
        status.contains("NO LIMIT OPERATES"),
        "the supervisor did not meet the config fault, so this test measured \
         nothing: {status}"
    );

    // The long message names the coordinator in every paragraph. One word is
    // therefore enough to separate the two forms.
    assert!(
        !status.contains("coordinator"),
        "the record of a job must hold the short form of a config fault. The \
         supervisor asked for the long form, which belongs to a person at a \
         terminal and not to the `error:` field of a job that already ran: \
         {status}"
    );
}

/// A job that writes far more than the limit must leave a file that is not
/// larger than the limit, with the first lines, the last lines, and a line that
/// says how much went.
///
/// A job wrote 386MB of standard output in a review, and nothing stopped it.
/// qex is made to be started and left, so nobody sees a disk fill, and the same
/// disk holds the record of each job. This test holds that fault.
#[test]
fn a_job_that_writes_more_than_the_limit_keeps_the_head_and_the_tail() {
    let h = Harness::new(
        "logcap",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [logs]\nmax_bytes = \"64KB\"\n",
    );
    // About 3.4MB of output, which is 50 times the limit.
    let id = h.submit(&["submit", "--", "sh", "-c", "seq 1 500000"]);
    let wait = h.qex(&["wait", &id, "--timeout", "60s"]);

    // The limit must never fail a job. Reaching it is normal.
    assert_eq!(
        wait.status.code(),
        Some(0),
        "the limit on the output must not fail the job"
    );
    assert_eq!(h.state_of(&id), "completed");

    let path = h.job_dir(&id).join("stdout.log");
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(
        size <= 64 * 1024,
        "the file holds {size} bytes, and the limit is 65536"
    );

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.lines().any(|l| l == "1"),
        "the first line went, and it holds the start of the job"
    );
    assert!(
        text.lines().any(|l| l == "500000"),
        "the last line went, and it holds the end of the job"
    );
    assert!(
        !text.lines().any(|l| l == "250000"),
        "the middle must go, and it stayed"
    );
    assert!(
        text.contains("are not in this file"),
        "the file must say what went: {:.400}",
        text
    );

    // The file beside the log file must go with the job.
    assert!(
        !h.job_dir(&id).join("stdout.log.tail").exists(),
        "the file that held the last output stayed"
    );

    // The record must say what went. A reader that gives `--tail 20` never sees
    // the line in the file, so the count must also be in the status.
    let status = h.status_json(&id);
    let dropped = &status["logs_dropped"];
    assert!(
        dropped["stdout_bytes"].as_u64().unwrap() > 0,
        "the record must say how many bytes went: {status}"
    );
    assert!(
        dropped["stdout_lines"].as_u64().unwrap() > 1000,
        "the record must say how many lines went: {status}"
    );
    assert_eq!(dropped["limit"].as_u64().unwrap(), 64 * 1024);

    // THE COUNT MUST BALANCE. The lines that the file holds and the lines that
    // the record says went are together the lines that the job wrote.
    //
    // A count that is only "large" hides a part that qex does not count. The
    // measured example: qex cuts the head back when the output passes the
    // limit, and it reads the log file to count the lines in that cut. With a
    // log file that qex opened for writing only, that read gives nothing, the
    // count loses about 8000 lines, and the record then says that the job wrote
    // 492000 lines when it wrote 500000.
    // A line is a line end, so the count is the line ends that are not the line
    // ends of the notes of qex.
    let notes = text.lines().filter(|l| l.starts_with("[qex]")).count() as u64;
    let kept = text.matches('\n').count() as u64 - notes;
    assert_eq!(
        kept + dropped["stdout_lines"].as_u64().unwrap(),
        500_000,
        "the file holds {kept} line(s) and the record says that {} went. Together they \
         must be the 500000 lines that the job wrote.",
        dropped["stdout_lines"]
    );

    // THE BYTES MUST BALANCE IN THE SAME WAY, and for a stronger reason: this
    // is the number that `qex status` prints to a person, as "qex removed
    // 3.2MB". An assertion of `> 0` beside a line count that balances exactly
    // left `stdout_bytes` with no exact test anywhere: a change that multiplied
    // that count in place of adding it passed every unit test and every
    // end-to-end test.
    //
    // The bytes of the notes of qex are not output of the job, so they come
    // off. Each note is one line and it ends with one line end.
    let note_bytes: u64 = text
        .lines()
        .filter(|l| l.starts_with("[qex]"))
        .map(|l| l.len() as u64 + 1)
        .sum();
    let kept_bytes = text.len() as u64 - note_bytes;
    assert_eq!(
        kept_bytes + dropped["stdout_bytes"].as_u64().unwrap(),
        3_388_895,
        "the file holds {kept_bytes} byte(s) of the job and the record says that {} went. \
         Together they must be the 3388895 bytes that `seq 1 500000` writes.",
        dropped["stdout_bytes"]
    );

    // The head must hold whole lines only. It stops at a byte, so qex moves the
    // cut back to the last line end. Measured before that change: the head
    // ended `3498` and then `3`, where the job wrote `3499`, and a reader has
    // no way to see that `3` is half of a line.
    let head = &text[..text
        .find("[qex]")
        .expect("the file must hold a note of qex")];
    for line in head.lines() {
        let n: u64 = line
            .parse()
            .unwrap_or_else(|_| panic!("the head holds `{line}`, which is not a whole line"));
        assert!((1..=500_000).contains(&n), "`{line}` is not in the output");
    }

    // THE CUT BACK MUST COST ONE LINE, AND NOT A QUARTER OF THE HEAD. qex looks
    // back from the cut for the LAST line end, inside a window. A search that
    // takes the FIRST line end of that window gives a head that is still a true
    // prefix and still ends at a line end, so every assertion above holds, and
    // the reader silently loses 4090 of the 16383 bytes.
    //
    // The cost is therefore the length of ONE line, and the lines here are the
    // numbers 1 to 500000, so no line is longer than seven bytes. A window is
    // 4096 bytes, so this test separates the two rules by a wide margin.
    let head_budget: u64 = 64 * 1024 / 4;
    let cut_cost = head_budget - head.len() as u64;
    assert!(
        cut_cost < 64,
        "the cut back to a line end removed {cut_cost} bytes of the head budget of \
         {head_budget}. No line of this output is longer than seven bytes, so the cut back \
         must move to the LAST line end before the cut, and not to the first one of the \
         window."
    );

    // The commands must not present a part of the output as the whole output.
    let logs = h.qex(&["logs", &id, "--stdout", "--tail", "5"]);
    let notice = String::from_utf8_lossy(&logs.stderr);
    assert!(
        notice.contains("qex removed"),
        "`qex logs` must say what went: {notice}"
    );

    let json = h.ok(&["logs", &id, "--stdout", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(value["stdout_dropped_lines"].as_u64().unwrap() > 1000);
}

/// Output with no line end must keep its last part.
///
/// qex removes the incomplete first line of the tail, so that a reader never
/// meets one half of a line. An earlier version applied that rule when the tail
/// held no line end at all, and it then removed the whole tail: a job that
/// wrote one enormous line left the head only. One JSON document, one base64
/// block, and a progress display that uses `\r` all give output of that form.
#[test]
fn a_job_that_writes_one_enormous_line_keeps_its_end() {
    let h = Harness::new(
        "logcap-line",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [logs]\nmax_bytes = \"64KB\"\n",
    );
    // 8MB with no line end at all, and a mark at the very end.
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "dd if=/dev/zero bs=1M count=8 2>/dev/null | tr '\\0' 'A'; printf THE-VERY-END",
    ]);
    let wait = h.qex(&["wait", &id, "--timeout", "60s"]);
    assert_eq!(wait.status.code(), Some(0));

    let path = h.job_dir(&id).join("stdout.log");
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size <= 64 * 1024, "the file holds {size} bytes");

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.ends_with("THE-VERY-END"),
        "the end of the output went, and the file holds the head only"
    );
    assert!(
        text.contains("middle of a line"),
        "the file must say that the last part is not a whole line"
    );
}

/// A second attempt must not remove output that fits in the limit.
///
/// qex removes nothing before the output passes the limit. An earlier version
/// cut the file back at the first byte of the second attempt: a retry of a job
/// that wrote a third of the limit lost 87KB, and the file said that the output
/// had reached the limit, which was not true.
#[test]
fn a_retry_that_fits_the_limit_keeps_the_output_of_both_attempts() {
    let h = Harness::new(
        "logcap-retry",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [logs]\nmax_bytes = \"1MB\"\n",
    );
    // Each attempt writes about 110KB, and the two together fit in the limit.
    let id = h.submit(&[
        "submit",
        "--retries",
        "1",
        "--",
        "sh",
        "-c",
        "seq 1 20000; echo THE-LAST-LINE; exit 1",
    ]);
    h.qex(&["wait", &id, "--timeout", "60s"]);

    let text = std::fs::read_to_string(h.job_dir(&id).join("stdout.log")).unwrap();
    assert!(
        !text.contains("[qex]"),
        "qex wrote a note about the limit, and the output fits in the limit"
    );
    assert_eq!(
        text.lines().filter(|l| *l == "THE-LAST-LINE").count(),
        2,
        "each attempt must keep its output"
    );
    assert!(
        text.contains("--- attempt 2 ---"),
        "the mark between the attempts went"
    );
    assert_eq!(
        h.status_json(&id)["logs_dropped"],
        serde_json::Value::Null,
        "the record says that qex removed output, and it removed nothing"
    );
}

/// `qex logs --follow` must not lose the output after the limit.
///
/// The log file of a job becomes SHORTER at the moment that the output passes
/// `[logs] max_bytes`, because the supervisor keeps the head and removes the
/// middle. That never happened before this limit existed. The position of the
/// follower was then after the end of the file, it read nothing more, and the
/// command gave the code 0 and no word: the reader saw the output stop in the
/// middle and had no reason to doubt it. This is the command that an agent uses
/// to watch a job.
#[test]
fn follow_does_not_lose_the_output_of_a_job_that_passes_the_limit() {
    let h = Harness::new(
        "logcap-follow",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [logs]\nmax_bytes = \"64KB\"\n",
    );
    // The job writes in three steps, and it waits between them:
    //
    // 1. About 61KB, which is below the limit. The follower reads it, and its
    //    position is then near 61KB.
    // 2. 6KB more. The output passes the limit, and the file becomes about
    //    17KB. The position of the follower is now far after the end.
    // 3. The last line, which arrives in the file when the job stops.
    //
    // The file at the end is smaller than the position of the follower, so a
    // follower that does not watch the length gives nothing after step 1.
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "seq 1 12000; sleep 2; seq 12001 13000; sleep 2; echo THE-FINAL-LINE",
    ]);

    let out = h.qex(&["logs", &id, "--stdout", "--follow"]);
    assert_eq!(out.status.code(), Some(0));
    let lines = String::from_utf8_lossy(&out.stdout).into_owned();
    let notice = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        lines.contains("THE-FINAL-LINE"),
        "the follower lost each line after the limit. It gave:\n{:.600}\n--- stderr ---\n{}",
        lines,
        notice
    );
    assert!(
        notice.contains("removed"),
        "the follower must say that qex removed output. It said: {notice}"
    );
}

/// A follower that starts AFTER the limit must still learn what went.
///
/// The test above starts before the limit, so the follower sees the log file
/// become shorter and it says so from that event. A follower that starts after
/// that moment never sees a shorter file: the file grows only, and the record
/// still says nothing, because the supervisor writes the count when the job
/// stops. Without the last read of the record, that follower gets the head, the
/// tail, and no word at all about the millions of lines between them.
///
/// The `.tail` file exists between the two moments, and only then, so the test
/// waits for it and starts the follower inside that window.
#[test]
fn a_follower_that_starts_after_the_limit_still_learns_what_went() {
    let h = Harness::new(
        "logcap-late",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [logs]\nmax_bytes = \"64KB\"\n",
    );
    // The job passes the limit at once, and then it waits. The follower thus
    // starts after the file became shorter, and before the job stops.
    let id = h.submit(&["submit", "--", "sh", "-c", "seq 1 500000; sleep 5"]);
    let tail = h.job_dir(&id).join("stdout.log.tail");
    h.until(
        "the output passes the limit",
        Duration::from_secs(60),
        || tail.exists(),
    );

    let out = h.qex(&["logs", &id, "--stdout", "--follow"]);
    assert_eq!(out.status.code(), Some(0));
    let lines = String::from_utf8_lossy(&out.stdout).into_owned();
    let notice = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        lines.contains("\n500000\n"),
        "the follower lost the end of the output: {:.400}",
        lines
    );
    assert!(
        notice.contains("from the middle of this stream"),
        "the follower must give the count of the output that went, and it said: {notice}"
    );
}

fn _unused(_: &Path) {}

/// Waits until a `qex run` that a test started stops, and gives its output.
///
/// The test must not wait for ever. A `qex run` that never stops is the fault
/// that these tests look for, so the wait has a limit and it says what failed.
fn wait_run(mut child: std::process::Child, what: &str) -> Output {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait().unwrap() {
            Some(_) => return child.wait_with_output().unwrap(),
            None => {
                if Instant::now() >= deadline {
                    child.kill().ok();
                    panic!("`qex run` did not stop in 60 seconds: {what}");
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// A DIFFERENT command stopped the job, so `qex run` gives 125 and not 1.
///
/// This is the fault that this test prevents. A job of `qex run` is a job like
/// any other, so another agent on the machine can run `qex kill` on it. With
/// the code 1 the caller cannot separate "my work failed" from "somebody
/// stopped my work", so it starts the work again or it reports a fault that the
/// work does not have. The message on stderr must also say that this command
/// did not stop the job.
#[test]
fn qex_run_gives_125_when_a_different_command_kills_the_job() {
    let h = Harness::with_default_config("runkill");
    let (child, id) = h.run_bg(&["--", "sleep", "60"]);
    h.until(
        "the job of `qex run` starts",
        Duration::from_secs(30),
        || h.has_started(&id),
    );

    let kill = h.qex(&["kill", &id, "--grace", "1s"]);
    assert!(kill.status.success(), "`qex kill` failed");

    let out = wait_run(child, "a different command killed the job");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(125),
        "`qex run` must give 125 when something stopped the job: {err}"
    );
    assert!(
        err.contains("did not send it"),
        "`qex run` must say that it did not stop the job: {err}"
    );
}

/// A different command cancelled the queued job, so `qex run` gives 125.
///
/// The job never ran and it wrote nothing, so `qex run` has no exit code of the
/// job to give. It gives the code of the state, and the same code as `qex
/// wait`, and it says that the job left the queue.
#[test]
fn qex_run_gives_125_when_a_different_command_cancels_the_queued_job() {
    let h = Harness::with_default_config("runcancel");

    // Hold the job of `qex run` in the queue with a dependency, so the test
    // controls the moment at which the cancel arrives.
    let blocker = h.submit(&["submit", "--", "sleep", "60"]);
    let (child, id) = h.run_bg(&["--needs", &blocker, "--", "echo", "never"]);
    h.until(
        "the job of `qex run` waits",
        Duration::from_secs(30),
        || h.state_of(&id) == "queued",
    );

    let cancel = h.qex(&["cancel", &id]);
    assert!(cancel.status.success(), "`qex cancel` failed");

    let out = wait_run(child, "a different command cancelled the job");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(125),
        "`qex run` must give 125 for a job that left the queue: {err}"
    );
    assert!(
        err.contains("removed the job"),
        "`qex run` must say that the job left the queue: {err}"
    );

    h.qex(&["kill", &blocker, "--grace", "1s"]);
}

/// `qex run` writes the output of the job, so it gives the exit code of the
/// job when the job RAN.
///
/// Without this, `qex run` would change the result of the command that it goes
/// in front of. The test also compares the code against `qex wait
/// --passthrough`, which answers the same question, so the two cannot drift.
#[test]
fn qex_run_gives_the_exit_code_of_a_job_that_ran() {
    let h = Harness::with_default_config("runcode");
    for code in [0, 1, 7] {
        let command = format!("exit {code}");
        let (child, id) = h.run_bg(&["--", "sh", "-c", &command]);
        let out = wait_run(child, "the job gives its own exit code");
        assert_eq!(
            out.status.code(),
            Some(code),
            "`qex run` must give the exit code {code} of the job: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let wait = h.qex(&["wait", &id, "--passthrough"]);
        assert_eq!(
            out.status.code(),
            wait.status.code(),
            "`qex run` and `qex wait --passthrough` gave two codes for one job"
        );
    }
}

/// `qex run` and `qex wait` must give one code for one state.
///
/// The test compares the two commands against each other, and not against a
/// number, so a later change cannot move one of them alone. It also proves that
/// the two cases of the fault are now separate: a job that failed gives 1, and a
/// job that something stopped does not.
#[test]
fn qex_run_and_qex_wait_give_the_same_code_for_the_same_job() {
    let h = Harness::with_default_config("runagree");

    let (child, killed) = h.run_bg(&["--", "sleep", "60"]);
    h.until(
        "the job of `qex run` starts",
        Duration::from_secs(30),
        || h.has_started(&killed),
    );
    h.qex(&["kill", &killed, "--grace", "1s"]);
    let stopped = wait_run(child, "the job that a kill stopped");

    let (child, failed) = h.run_bg(&["--", "sh", "-c", "exit 1"]);
    let ran = wait_run(child, "the job that failed");

    // A time limit is a separate state. Without it, a change that gave the
    // exit code of the job for the state `timeout` would pass this test.
    let (child, limited) = h.run_bg(&["--timeout", "1s", "--", "sleep", "60"]);
    let timed_out = wait_run(child, "the job that reached its time limit");

    assert_eq!(
        stopped.status.code(),
        h.qex(&["wait", &killed]).status.code(),
        "`qex run` and `qex wait` gave two codes for a job that something stopped"
    );
    assert_eq!(
        timed_out.status.code(),
        h.qex(&["wait", &limited]).status.code(),
        "`qex run` and `qex wait` gave two codes for a job that reached its time limit"
    );
    assert_eq!(
        ran.status.code(),
        h.qex(&["wait", &failed]).status.code(),
        "`qex run` and `qex wait` gave two codes for a job that failed"
    );

    assert_eq!(ran.status.code(), Some(1), "a job that failed gives 1");
    assert_ne!(
        stopped.status.code(),
        Some(1),
        "a job that something stopped must not give the code of a job that failed"
    );
    assert_ne!(
        timed_out.status.code(),
        Some(1),
        "a job that reached its time limit must not give the code of a job that failed"
    );
}

/// `qex run` must say when IT stopped the job, and not blame a different
/// command.
///
/// The branch that writes this sentence is the branch that gives the wrong
/// blame when it is wrong, so a test must reach it. Without this test, a change
/// that always blames a different command passes every other test.
#[test]
fn qex_run_says_when_this_command_stopped_the_job() {
    let h = Harness::with_default_config("runctrlc");
    let (child, id) = h.run_bg(&["--", "sleep", "60"]);
    h.until(
        "the job of `qex run` starts",
        Duration::from_secs(30),
        || h.has_started(&id),
    );

    // Ctrl-C sends SIGINT to the process. The test sends the same signal.
    let pid = child.id() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);

    let out = wait_run(child, "Ctrl-C stopped the job");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(125),
        "a job that Ctrl-C stopped gives 125: {err}"
    );
    assert!(
        err.contains("this command stopped the job"),
        "`qex run` must say that IT stopped the job: {err}"
    );
    assert!(
        !err.contains("did not send it"),
        "`qex run` must not blame a different command for its own kill: {err}"
    );
}

/// Ctrl-C must take the job out of the queue when the job has not started.
///
/// The coordinator refuses to kill a job that waits in the queue, because that
/// job has no process. Without the cancel, `qex run` waited for a job that
/// still had to run, and it then said that a different command stopped a job
/// that this command tried to stop.
#[test]
fn ctrl_c_removes_the_job_of_qex_run_from_the_queue() {
    let h = Harness::with_default_config("runctrlcq");

    let blocker = h.submit(&["submit", "--", "sleep", "60"]);
    let (child, id) = h.run_bg(&["--needs", &blocker, "--", "echo", "never"]);
    h.until(
        "the job of `qex run` waits",
        Duration::from_secs(30),
        || h.state_of(&id) == "queued",
    );

    let pid = child.id() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);

    let out = wait_run(child, "Ctrl-C removed the job from the queue");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(125),
        "a job that left the queue gives 125: {err}"
    );
    assert_eq!(
        h.state_of(&id),
        "cancelled",
        "Ctrl-C must take the job out of the queue: {err}"
    );
    assert!(
        err.contains("this command removed the job"),
        "`qex run` must say that IT removed the job from the queue: {err}"
    );

    h.qex(&["kill", &blocker, "--grace", "1s"]);
}
/// A change to the configuration must reach a coordinator that operates.
///
/// # The fault that this test holds
///
/// The coordinator read the file one time, at its start, and it then operates
/// for hours. A user who changed `[budget] cpu` saw `qex config show` report
/// the NEW value, because that command reads the file, and `qex info` report
/// the OLD one, because that command asks the coordinator. The two commands of
/// qex disagreed about the budget of qex, and neither said that one was old.
#[test]
fn a_change_to_the_configuration_reaches_the_coordinator() {
    let h = Harness::new(
        "reload",
        "[budget]\ncpu = \"8\"\nmem = \"4GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    // Start the coordinator, and read the budget that it holds.
    h.ok(&["list"]);
    let info = h.ok(&["info"]);
    assert!(info.contains("of 8 in use"), "the budget must be 8: {info}");

    // Change the file while the coordinator operates.
    std::fs::write(
        h.root.join("cfg/qex.toml"),
        "[budget]\ncpu = \"2\"\nmem = \"4GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    )
    .unwrap();

    h.until(
        "the coordinator reads the file again",
        Duration::from_secs(45),
        || h.ok(&["info"]).contains("of 2 in use"),
    );

    // A file that qex cannot read must NOT become the default values. The
    // default budget is far larger than 2 cores, so a silent fall back would
    // give this coordinator a budget that nobody asked for.
    //
    // THE VALUE HERE IS VALID TOML. It is the reload that must refuse it, in
    // the same way as the start of a coordinator: an earlier version of this
    // work installed `cpu = "two"` and gave every job a budget of 0 cores with
    // no word to anybody.
    std::fs::write(h.root.join("cfg/qex.toml"), "[budget]\ncpu = \"two\"\n").unwrap();

    h.until("qex reports the fault", Duration::from_secs(45), || {
        let out = h.qex(&["info"]);
        String::from_utf8_lossy(&out.stderr).contains("cannot read it")
    });

    let info = h.ok(&["info"]);
    assert!(
        info.contains("of 2 in use"),
        "the coordinator must keep the values that it had: {info}"
    );

    // The fault travels as DATA as well. An agent reads the JSON, and a number
    // with no word about its age is worse than no number.
    let json: serde_json::Value =
        serde_json::from_slice(&h.qex(&["info", "--json"]).stdout).unwrap();
    assert!(
        json["config_error"].is_string(),
        "`qex info --json` must carry the fault: {json}"
    );

    // A FAULT OF THE FORM OF THE FILE gives a message of several lines, with a
    // caret under the column. Every line must carry the `qex:` prefix, or the
    // lines after the first read as output of the command.
    std::fs::write(h.root.join("cfg/qex.toml"), "[budget\n").unwrap();

    h.until(
        "qex reports the fault of the form",
        Duration::from_secs(45),
        || String::from_utf8_lossy(&h.qex(&["info"]).stderr).contains("TOML parse error"),
    );
    let err = String::from_utf8_lossy(&h.qex(&["info"]).stderr).to_string();
    for line in err.lines() {
        assert!(
            line.starts_with("qex:"),
            "every line of the warning must carry the prefix: {err}"
        );
    }

    // CORRECT THE FILE, AND THE WARNING GOES AWAY. A warning that stays after
    // the remedy teaches the reader to ignore every warning of qex.
    std::fs::write(
        h.root.join("cfg/qex.toml"),
        "[budget]\ncpu = \"5\"\nmem = \"4GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    )
    .unwrap();

    h.until(
        "the coordinator takes the corrected file",
        Duration::from_secs(45),
        || h.ok(&["info"]).contains("of 5 in use"),
    );
    let err = String::from_utf8_lossy(&h.qex(&["info"]).stderr).to_string();
    assert!(
        !err.contains("cannot read it"),
        "the warning must go away when the file is correct: {err}"
    );
}

/// The states that an EDITOR leaves behind must not change the budget.
///
/// # The fault that this test holds
///
/// A file that qex cannot read must never become the DEFAULT values. The
/// default budget is 75% of the machine, so a coordinator that took the
/// defaults would turn a budget of 2 cores into a budget of 12, with no word to
/// anybody. An empty file and a file that is gone are the two states that a
/// write leaves for a moment, so the reload meets them by itself and not by a
/// fault of the user.
///
/// The rename is the third state. An editor writes a temporary file and renames
/// it over the old one, so the name then points at a different file.
#[test]
fn an_empty_or_missing_configuration_does_not_become_the_default_values() {
    let file = "[budget]\ncpu = \"2\"\nmem = \"4GB\"\n\
                [peers]\nenabled = false\n\
                [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n";
    let h = Harness::new("reload-gone", file);
    let path = h.root.join("cfg/qex.toml");

    h.ok(&["list"]);
    let info = h.ok(&["info"]);
    assert!(info.contains("of 2 in use"), "the budget must be 2: {info}");

    // An empty file, which is the state that a truncate leaves.
    std::fs::write(&path, "").unwrap();
    h.until(
        "qex reports the empty file",
        Duration::from_secs(45),
        || String::from_utf8_lossy(&h.qex(&["info"]).stderr).contains("the file is empty"),
    );
    let info = h.ok(&["info"]);
    assert!(
        info.contains("of 2 in use"),
        "an empty file must not change the budget: {info}"
    );

    // A file that is gone, which is the state that a rename leaves.
    std::fs::remove_file(&path).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let info = h.ok(&["info"]);
    assert!(
        info.contains("of 2 in use"),
        "a file that is gone must not change the budget: {info}"
    );

    // A RENAME OVER THE FILE. The name then points at a different file, so a
    // watch of the name would hold a file that nobody reads again.
    let temp = h.root.join("cfg/.qex.toml.new");
    std::fs::write(&temp, file.replace("cpu = \"2\"", "cpu = \"6\"")).unwrap();
    std::fs::rename(&temp, &path).unwrap();

    h.until(
        "the coordinator reads the file that the rename put there",
        Duration::from_secs(45),
        || h.ok(&["info"]).contains("of 6 in use"),
    );
}

/// A WRITE THAT IS NOT FINISHED must not become the default budget.
///
/// # The fault that this test holds
///
/// A shell `>` and a redirect, and every program that writes one line at a
/// time, make the file empty and then fill it. The guard for an empty file is
/// not sufficient, because A FILE THAT STOPS IN THE MIDDLE IS STILL VALID
/// TOML: `[budget]` with no line under it parses, it validates, and every key
/// takes its DEFAULT value.
///
/// A review measured that on the first form of this work. The budget went from
/// 2 cores to 12 — 12 is 75% of that machine — and 10 jobs started together in
/// place of 2. The coordinator said nothing, because it could read the file.
/// That is worse than the fault this branch removes, because it STARTS WORK.
///
/// The test writes the SAME content each time, so the budget has no reason to
/// change at all. Every value that `qex info` gives must be the value in the
/// file.
#[test]
fn a_write_that_is_not_finished_does_not_change_the_budget() {
    // `mem` is the LAST value, and 1GB is a value that no machine gives by
    // default: the default is 75% of the memory of the machine. A file that
    // stops before this line thus shows itself at once.
    let file = "[peers]\nenabled = false\n\
                [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
                [budget]\ncpu = \"2\"\nmem = \"1GB\"\n";
    let h = Harness::new("reload-partial", file);
    let path = h.root.join("cfg/qex.toml");

    h.ok(&["list"]);
    let info: serde_json::Value =
        serde_json::from_slice(&h.qex(&["info", "--json"]).stdout).unwrap();
    assert_eq!(info["mem_budget"].as_u64(), Some(1024 * 1024 * 1024));

    // Write the file line by line, again and again, for as long as the reader
    // looks. Each round leaves the same content.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (path, stop, file) = (path.clone(), stop.clone(), file.to_string());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let mut f = std::fs::File::create(&path).unwrap();
                for line in file.lines() {
                    use std::io::Write;
                    writeln!(f, "{line}").unwrap();
                    f.flush().unwrap();
                    std::thread::sleep(Duration::from_millis(2));
                }
                drop(f);
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut looks = 0;
    while Instant::now() < deadline {
        let info: serde_json::Value =
            serde_json::from_slice(&h.qex(&["info", "--json"]).stdout).unwrap();
        looks += 1;
        assert_eq!(
            info["mem_budget"].as_u64(),
            Some(1024 * 1024 * 1024),
            "a write that is not finished must never give the default budget: {info}"
        );
        assert_eq!(
            info["cpu_budget"].as_u64(),
            Some(2),
            "a write that is not finished must never give the default budget: {info}"
        );
        assert!(
            info["config_error"].is_null(),
            "the file is correct each time it is complete: {info}"
        );
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(
        looks > 20,
        "the test must look many times, and it looked {looks}"
    );
}

/// A writer that goes BACK AND FORTH between two whole files.
///
/// # The fault that this test holds
///
/// qex looks at the file about ten times in half a second and takes the content
/// when every look gave the same content. With one look every 500ms the window
/// holds TWO looks, and two looks cannot separate "the file did not change"
/// from "the file changed and changed back between them".
///
/// A writer that puts a half-written file and the whole file at the path in
/// turn, 300ms each, with a rename, gives a period of 600ms against a sampler
/// of 500ms. The two walk past each other, so a pair of looks lands on the same
/// half-written file, and a coordinator that looks twice takes a budget that
/// was never in the file for more than 300ms.
///
/// # Why the test above cannot find this
///
/// The writer above puts a file down one line at a time with 2ms between the
/// lines. It passes through seven states, each for 2ms, so no single one of
/// them holds a pair of looks. That test passed 8 runs of 8 against the fault
/// that this one finds first time. A test that cannot fail proves nothing, and
/// "no test can find this" was an assertion that nobody had tried.
///
/// The RENAME matters: each look then gives one whole file or the other, and
/// never a file that a write is inside.
#[test]
fn a_file_that_goes_back_and_forth_does_not_change_the_budget() {
    let whole = "[peers]\nenabled = false\n\
                 [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
                 [budget]\ncpu = \"2\"\nmem = \"1GB\"\n";
    // The same file, stopped before `mem`. It parses, it validates, and `mem`
    // takes its default value: 75% of the memory of the machine.
    let half = whole.strip_suffix("mem = \"1GB\"\n").unwrap().to_string();

    let h = Harness::new("reload-alternating", whole);
    let path = h.root.join("cfg/qex.toml");

    h.ok(&["list"]);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&h.qex(&["info", "--json"]).stdout).unwrap()
            ["mem_budget"]
            .as_u64(),
        Some(1024 * 1024 * 1024)
    );

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (path, stop, whole) = (path.clone(), stop.clone(), whole.to_string());
        let temp = h.root.join("cfg/.qex.toml.new");
        std::thread::spawn(move || {
            let mut turn = false;
            while !stop.load(Ordering::Relaxed) {
                let text = if turn { &whole } else { &half };
                std::fs::write(&temp, text).unwrap();
                std::fs::rename(&temp, &path).unwrap();
                turn = !turn;
                std::thread::sleep(Duration::from_millis(300));
            }
            // Leave the whole file behind, whatever the turn.
            std::fs::write(&temp, &whole).unwrap();
            std::fs::rename(&temp, &path).unwrap();
        })
    };

    let deadline = Instant::now() + Duration::from_secs(12);
    let mut looks = 0;
    let mut fault = None;
    while Instant::now() < deadline {
        let out = h.qex(&["info", "--json"]);
        if let Ok(info) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            looks += 1;
            if info["mem_budget"].as_u64() != Some(1024 * 1024 * 1024) {
                fault = Some(format!("{} looks, then {info}", looks));
                break;
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    assert!(
        fault.is_none(),
        "a file that goes back and forth must not change the budget: {}",
        fault.unwrap_or_default()
    );
    assert!(
        looks > 20,
        "the test must look many times, and it looked {looks}"
    );
}

/// The same fault, on a coordinator that HAS WORK TO DO.
///
/// # The fault that this test holds
///
/// The first form of this guard counted two TURNS of the scheduler, and the
/// words around it said half a second. A turn is not half a second. The
/// scheduler waits on a condition variable with a timeout of 500ms, and every
/// request thread wakes it, so a coordinator with work in the queue turns much
/// faster. A mark on each turn measured a median gap of 500.7ms with nothing to
/// do and 17.0ms with a loop of `qex submit` running, with a minimum of 1.2ms.
///
/// A review then made a partial write with a pause of 300ms in the middle,
/// under load, and `qex info` gave `cpu_budget: 12` — the default of that
/// machine — while the file said 2, with `config_error: null`. The test above
/// cannot find this, because an idle coordinator never shows it.
///
/// The guard is now a TIME. This test holds the words and the guard together.
#[test]
fn a_write_that_is_not_finished_does_not_change_the_budget_under_load() {
    let file = "[peers]\nenabled = false\n\
                [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
                [budget]\ncpu = \"2\"\nmem = \"1GB\"\n";
    let h = Harness::new("reload-partial-load", file);
    let path = h.root.join("cfg/qex.toml");

    h.ok(&["list"]);

    let stop = Arc::new(AtomicBool::new(false));

    // THE LOAD. Each submission wakes the scheduler, so the turns become
    // short. Without this the guard cannot be measured at all.
    let load = {
        let (root, stop) = (h.root.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                Command::new(env!("CARGO_BIN_EXE_qex"))
                    .args(["submit", "--", "true"])
                    .env("XDG_CONFIG_HOME", root.join("cfg"))
                    .env("XDG_STATE_HOME", root.join("state"))
                    .env("XDG_RUNTIME_DIR", root.join("run"))
                    .env("QEX_IDLE_EXIT_SECS", "120")
                    .output()
                    .ok();
            }
        })
    };

    // THE WRITER. It stops in the middle for 300ms, which is less than the
    // guard and far more than a turn under this load.
    let writer = {
        let (path, stop, file) = (path.clone(), stop.clone(), file.to_string());
        std::thread::spawn(move || {
            let half = file.find("[budget]").unwrap();
            while !stop.load(Ordering::Relaxed) {
                std::fs::write(&path, &file[..half]).unwrap();
                std::thread::sleep(Duration::from_millis(300));
                std::fs::write(&path, &file).unwrap();
                std::thread::sleep(Duration::from_millis(700));
            }
        })
    };

    let deadline = Instant::now() + Duration::from_secs(12);
    let mut looks = 0;
    let mut fault = None;
    while Instant::now() < deadline {
        let out = h.qex(&["info", "--json"]);
        if let Ok(info) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            looks += 1;
            if info["mem_budget"].as_u64() != Some(1024 * 1024 * 1024)
                || info["cpu_budget"].as_u64() != Some(2)
            {
                fault = Some(info.to_string());
                break;
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    load.join().unwrap();

    assert!(
        fault.is_none(),
        "a write that is not finished must never change the budget, and a busy \
         coordinator is where that fault lives: {}",
        fault.unwrap_or_default()
    );
    assert!(
        looks > 20,
        "the test must look many times, and it looked {looks}"
    );
}

/// A command that reads the config file must not wait for ever either.
///
/// # The fault that this test holds
///
/// The coordinator refuses a path that is not a regular file. `qex config show`
/// and `qex submit` read the file for THEMSELVES, so the guard must be in the
/// reader that they share. A review measured both of them with no answer at all
/// with a FIFO at the config path.
///
/// The warning of `qex info` names `qex config show` as the way to see the whole
/// message. Without this guard, that advice walks the reader into the wait that
/// this branch exists to remove.
#[test]
fn a_command_refuses_a_configuration_path_that_is_not_a_regular_file() {
    let file = "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
                [peers]\nenabled = false\n\
                [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n";
    let h = Harness::new("client-not-a-file", file);
    let path = h.root.join("cfg/qex.toml");

    assert!(h.ok(&["config", "show"]).contains("2 cores"));

    std::fs::remove_file(&path).unwrap();
    let made = Command::new("mkfifo").arg(&path).status();
    if !made.map(|s| s.success()).unwrap_or(false) {
        // No `mkfifo` on this machine. A directory has the same type test,
        // although it is not the case that waits.
        std::fs::create_dir(&path).unwrap();
    }

    for args in [vec!["config", "show"], vec!["submit", "--", "true"]] {
        let child = Command::new(env!("CARGO_BIN_EXE_qex"))
            .args(&args)
            .env("XDG_CONFIG_HOME", h.root.join("cfg"))
            .env("XDG_STATE_HOME", h.root.join("state"))
            .env("XDG_RUNTIME_DIR", h.root.join("run"))
            .env("QEX_IDLE_EXIT_SECS", "120")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let out = wait_for_child(
            child,
            Duration::from_secs(20),
            "a path that is not a regular file",
        );
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(
            !out.status.success(),
            "`qex {}` must stop: {err}",
            args.join(" ")
        );
        assert!(
            err.contains("not a regular file"),
            "`qex {}` must name the cause: {err}",
            args.join(" ")
        );
    }
}

/// A path that is NOT A REGULAR FILE must not stop the coordinator.
///
/// # The fault that this test holds
///
/// The coordinator opens this path on every turn of the scheduler. A FIFO
/// stops the OPEN until somebody writes to the FIFO. A review measured `qex
/// info` with no answer in 15 seconds: the scheduler waited in the open while
/// it held the state mutex, and the three other threads waited for that mutex.
/// A write to the FIFO did not end it, and a delete of the FIFO did not end it.
/// Only `kill -9` did. A network file system that stops answering does the
/// same thing by a different road.
///
/// The coordinator read this file one time before this work, so this fault
/// arrived with the reload.
#[test]
fn a_configuration_path_that_is_not_a_regular_file_does_not_stop_the_coordinator() {
    let file = "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
                [peers]\nenabled = false\n\
                [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n";
    let h = Harness::new("reload-not-a-file", file);
    let path = h.root.join("cfg/qex.toml");

    h.ok(&["list"]);
    assert!(h.ok(&["info"]).contains("of 2 in use"));

    // A DIRECTORY at the path. A read gives an error at once, so this case
    // tests the answer and not the wait.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    h.until(
        "qex reports the type of the path",
        Duration::from_secs(45),
        || String::from_utf8_lossy(&h.qex(&["info"]).stderr).contains("not a regular file"),
    );
    assert!(
        h.ok(&["info"]).contains("of 2 in use"),
        "a path that is not a regular file must not change the budget"
    );
    std::fs::remove_dir(&path).unwrap();

    // A FIFO at the path, which is the case that stopped the coordinator.
    // `qex info` runs as a child with a limit on the wait, so a coordinator
    // that stops gives a failure of this test and not a test that never ends.
    let made = Command::new("mkfifo").arg(&path).status();
    if made.map(|s| s.success()).unwrap_or(false) {
        for _ in 0..6 {
            let child = Command::new(env!("CARGO_BIN_EXE_qex"))
                .args(["info"])
                .env("XDG_CONFIG_HOME", h.root.join("cfg"))
                .env("XDG_STATE_HOME", h.root.join("state"))
                .env("XDG_RUNTIME_DIR", h.root.join("run"))
                .env("QEX_IDLE_EXIT_SECS", "120")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let out = wait_for_child(child, Duration::from_secs(20), "a FIFO at the config path");
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("of 2 in use"),
                "the coordinator must answer and keep its budget: {}",
                String::from_utf8_lossy(&out.stdout)
            );
            std::thread::sleep(Duration::from_millis(400));
        }
        std::fs::remove_file(&path).unwrap();
    }

    // The file comes back, and the coordinator takes it again.
    std::fs::write(&path, file.replace("cpu = \"2\"", "cpu = \"4\"")).unwrap();
    h.until(
        "the coordinator takes the file that replaced the FIFO",
        Duration::from_secs(45),
        || h.ok(&["info"]).contains("of 4 in use"),
    );
}

/// Waits for a child with a limit, and fails rather than waiting for ever.
fn wait_for_child(mut child: std::process::Child, limit: Duration, what: &str) -> Output {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait().unwrap() {
            Some(_) => return child.wait_with_output().unwrap(),
            None => {
                if Instant::now() >= deadline {
                    child.kill().ok();
                    panic!("`qex info` gave no answer in {limit:?}: {what}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// The completions must name the commands that qex has.
///
/// A completion file that a person installs and then finds to be wrong is worse
/// than no completion: it teaches a command that does not exist.
#[test]
fn the_completions_hold_the_commands_of_qex() {
    let h = Harness::with_default_config("completions");

    for shell in ["bash", "zsh", "fish"] {
        let out = h.ok(&["completions", shell]);
        assert!(!out.is_empty(), "{shell} gave nothing");
        for command in ["submit", "wait", "status", "logs", "kill", "watchers"] {
            assert!(
                out.contains(command),
                "the {shell} completions must name `{command}`"
            );
        }
    }

    // A shell that qex does not know must give an error, and not an empty file.
    let out = h.qex(&["completions", "not-a-shell"]);
    assert!(!out.status.success(), "an unknown shell must give an error");

    // A NAME MUST NOT RUN. `compgen -W` expands its word list again, so the
    // first version of this code ran a job named `$(...)` when somebody pressed
    // TAB. An agent chooses the names of the jobs and a person presses the TAB,
    // so a name is not trusted text.
    let bash = h.ok(&["completions", "bash"]);
    let jobs_part = &bash[bash.find("_qex_jobs()").expect("bash needs `_qex_jobs`")..];
    // The comment in that part NAMES `compgen`, and a comment is not code.
    assert!(
        !jobs_part
            .lines()
            .any(|l| !l.trim_start().starts_with('#') && l.contains("compgen")),
        "the bash completions must not expand the names again: {jobs_part}"
    );
    assert!(
        jobs_part.contains("while IFS= read -r candidate"),
        "the bash completions must read the names as lines"
    );

    // A NAME MUST STAY ONE WORD. bash writes a candidate to the line as it
    // stands, so a name that holds `;` would put a command on the line beside
    // `qex`, and the next press of ENTER would run it. `printf %q` puts a
    // backslash before each character that the shell reads.
    assert!(
        jobs_part.contains("printf -v candidate '%q'"),
        "the bash completions must make each name safe for the command line"
    );
    // A leading `~` as well. bash 5.1 escapes it with `%q` and bash 3.2 does
    // NOT, so a name of `~/x` arrived as a home directory on macOS.
    //
    // `bash_keeps_a_hostile_candidate_in_one_word` measures the outcome, and it
    // can only measure the bash of the machine that runs it. This assertion
    // holds the rule on every machine.
    assert!(
        jobs_part.contains(r#"case "$candidate" in "~"*) candidate="\\$candidate" ;; esac"#),
        "the bash completions must make a leading `~` safe as well"
    );
    // `compopt -o filenames` asks bash to do that work instead, and it treats
    // each name as a FILE: a name that is also a directory got a `/`, and a
    // name that starts with `~` was expanded. Do not go back to it.
    assert!(
        !jobs_part
            .lines()
            .any(|l| !l.trim_start().starts_with('#') && l.contains("compopt")),
        "the bash completions must not treat a job name as a file name"
    );

    // BASH MUST CALL THE FUNCTION THAT ADDS THE JOBS.
    //
    // This is the fault that zsh had: the words were in the file and the shell
    // never ran them. A test that calls `_qex_with_jobs` by name cannot see it,
    // because the name is right and the registration is wrong.
    assert!(
        bash.contains("complete -F _qex_with_jobs "),
        "bash must give `_qex_with_jobs` to the shell, and not `_qex`: {bash}"
    );
    // `-o nosort` came with bash 4.4, and `complete` refuses the WHOLE command
    // when it meets an option name that it does not know. An earlier version
    // wrote that option with no test of the version, so on the bash 3.2 of
    // macOS the file bound NOTHING and it wrote an error at each shell start.
    assert!(
        bash.contains("BASH_VERSINFO[0]}\" -eq 4 && \"${BASH_VERSINFO[1]}\" -ge 4")
            && bash.contains("complete -F _qex_with_jobs -o bashdefault -o default qex"),
        "the registration must test the version of bash before it uses `-o nosort`: {bash}"
    );
    // `-o nosort` must stay for a bash that has it: it is what keeps the newest
    // job first, and bash sorts the candidates again without it.
    assert!(
        bash.contains("complete -F _qex_with_jobs -o nosort -o bashdefault -o default qex"),
        "a bash that has `-o nosort` must get it: {bash}"
    );
    assert!(
        bash.contains("_qex_with_jobs() {") && bash.contains("    _qex_jobs\n}"),
        "`_qex_with_jobs` must run `_qex` and then add the jobs"
    );

    // An option that takes a VALUE must be named, and a flag must not. The
    // guard read every word that starts with a dash, so `qex status --json
    // <TAB>` offered no job at all.
    let guard = bash
        .lines()
        .find(|l| l.contains("in *\" $prev \"*)"))
        .expect("bash needs the guard for an option value");
    for valued in ["--signal", "--grace", "--timeout", "--tail", "-C"] {
        assert!(
            guard.contains(&format!(" {valued} ")),
            "`{valued}` takes a value, so it must be in the guard: {guard}"
        );
    }
    for flag in ["--json", "--show-env", "--no-logs", "--all"] {
        assert!(
            !guard.contains(&format!(" {flag} ")),
            "`{flag}` takes no value, so the word after it is a job: {guard}"
        );
    }

    // The zsh completions must ASK for the ids where a job goes.
    //
    // An earlier version put the function at the end of the file. zsh gives
    // the completion to the shell in the lines above it, so that function
    // never ran and zsh offered no job at all. It is not enough that the file
    // holds the words; the job argument must name the function.
    let zsh = h.ok(&["completions", "zsh"]);
    for (line, what) in [
        ("':id -- The job id, or the start of the id", "ids"),
        ("'*::ids -- The job ids to wait for", "ids"),
        ("'*::ids -- The job ids to stop", "active"),
        ("'*::ids -- The job ids to remove from the queue", "queued"),
        ("'*::ids -- The job ids to delete", "ids"),
    ] {
        assert!(
            zsh.contains(&format!("{line}: _qex_jobs {what}' \\")),
            "the zsh job argument must offer the jobs: {line}"
        );
    }
    // The SPACE before `_qex_jobs` is not decoration: zsh runs an action as a
    // command only when the action starts with a space.
    assert!(
        !zsh.contains(":_qex_jobs "),
        "a zsh action needs a space before the name of the function"
    );
    // An option that takes a value is not a job. `qex kill --signal <TAB>`
    // must offer a signal, and never a job.
    assert!(
        zsh.contains("]:SIGNAL:_default'"),
        "the value of `--signal` must not become a job"
    );
    // The function must exist BEFORE the file gives the completion to the
    // shell, and `#compdef` must stay on the first line.
    let helper = zsh.find("_qex_jobs()").expect("zsh needs `_qex_jobs`");
    let hand_over = zsh.find("compdef _qex qex").expect("zsh needs `compdef`");
    assert!(
        zsh.starts_with("#compdef qex"),
        "zsh needs `#compdef` first"
    );
    assert!(
        helper < hand_over,
        "`_qex_jobs` must exist before zsh gets the completion"
    );

    // The commands that qex gives to itself must not be offered. A person who
    // pressed TAB was invited to run `qex daemon`.
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let out = h.ok(&["completions", shell]);
        for hidden in ["daemon", "supervise", "__complete"] {
            // Look in the lines that OFFER a command, and not in the whole
            // file: the part that asks qex for the ids names `qex __complete`
            // itself, and that line is code and not a candidate. The block that
            // says what a hidden command accepts also stays, and it offers
            // nothing.
            for line in out.lines() {
                let trimmed = line.trim_start();
                let offers = trimmed.starts_with("opts=\"")
                    || trimmed.starts_with(&format!("'{hidden}:"))
                    || trimmed.starts_with("cand ")
                    || trimmed.starts_with("[CompletionResult]::new(")
                    || (trimmed.starts_with("complete -c qex")
                        && trimmed.contains("__fish_use_subcommand"));
                if !offers {
                    continue;
                }
                assert!(
                    !line
                        .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                        .any(|word| word == hidden),
                    "the {shell} completions must not offer `{hidden}`: {line}"
                );
            }
        }
    }

    // The shells that offer the ids must hold the command that asks for them.
    for shell in ["bash", "zsh", "fish"] {
        let out = h.ok(&["completions", shell]);
        assert!(
            out.contains("qex __complete"),
            "the {shell} completions must ask qex for the ids"
        );
    }

    // fish gets one line for each set, and each line names the commands that
    // take that set.
    let fish = h.ok(&["completions", "fish"]);
    for (commands, what) in [
        ("status wait logs rerun clean", "ids"),
        ("kill", "active"),
        ("cancel", "queued"),
    ] {
        assert!(
            fish.contains(&format!(
                "complete -c qex -n \"__fish_seen_subcommand_from {commands}\" \
                 -f -a \"(qex __complete {what})\""
            )),
            "fish must offer `{what}` after `{commands}`: {fish}"
        );
    }
}

/// The candidates for a shell must come from the disk, and they must never
/// start a coordinator.
///
/// A press of TAB is not a request to start a process. A user who presses TAB
/// in a directory with no work must not leave a coordinator behind.
#[test]
fn the_completion_candidates_start_no_coordinator() {
    let h = Harness::with_default_config("candidates");

    // No coordinator, and no jobs.
    let empty = h.ok(&["__complete", "ids"]);
    assert!(empty.is_empty(), "an empty state must give no candidate");
    let info = h.qex(&["info", "--no-start"]);
    let text = String::from_utf8_lossy(&info.stdout);
    assert!(
        text.contains("no coordinator"),
        "TAB must not start a coordinator: {text}"
    );

    // A job that operates, and a job that waits for it.
    let running = h.submit(&[
        "submit", "--name", "holder", "--cpu", "1", "--mem", "64MB", "--lock", "one", "--",
        "sleep", "300",
    ]);
    let queued = h.submit(&[
        "submit", "--name", "waiter", "--cpu", "1", "--mem", "64MB", "--lock", "one", "--", "true",
    ]);
    h.until("the first job operates", Duration::from_secs(45), || {
        h.state_of(&running) == "running"
    });

    // Every job, with the id and the name of each.
    let all = h.ok(&["__complete", "ids"]);
    for want in [running.as_str(), queued.as_str(), "holder", "waiter"] {
        assert!(all.lines().any(|l| l == want), "`ids` must hold {want}");
    }

    // `qex kill` takes a job that operates, so it must not offer one that
    // waits. `qex cancel` is the opposite. A candidate that the command
    // refuses teaches the wrong command.
    let active = h.ok(&["__complete", "active"]);
    assert!(active.lines().any(|l| l == running));
    assert!(
        !active.lines().any(|l| l == queued),
        "`active` must not hold a job that waits: {active}"
    );

    let waiting = h.ok(&["__complete", "queued"]);
    assert!(waiting.lines().any(|l| l == queued));
    assert!(
        !waiting.lines().any(|l| l == running),
        "`queued` must not hold a job that operates: {waiting}"
    );

    // THE LIST HOLDS A SAFE FORM OF EACH NAME.
    //
    // Each character outside the set `A-Z a-z 0-9 - _ .` becomes `_`, a run of
    // them becomes ONE `_`, a first character of `-` becomes `_`, and the
    // result stops at 128 characters.
    //
    // A record on the disk is not a promise about its content: qex wrote
    // records before this rule, and one of them can hold a name with a space, a
    // `$` or a `;`. The safe form comes from the name that the record holds, so
    // the rule reaches every record at once and `qex gc` is not the thing that
    // applies it.
    let pairs = [
        ("deploy prod$(id)", "deploy_prod_id_"),
        ("cost $HOME", "cost_HOME"),
        ("a; touch", "a_touch"),
        ("two\nlines", "two_lines"),
        ("two\tparts", "two_parts"),
        ("esc\u{1b}[2Jname", "esc_2Jname"),
        ("src/main", "src_main"),
        ("a:b", "a_b"),
        ("build-*", "build-_"),
        ("-version", "_version"),
        ("caf\u{e9}", "caf_"),
        ("plain-name_1.2", "plain-name_1.2"),
    ];
    for (name, safe) in pairs {
        let id = h.submit(&["submit", &format!("--name={name}"), "--", "true"]);
        // qex shows the safe form. `every_output_shows_the_safe_name` holds
        // the other half: the record on the disk keeps the name that the user
        // gave.
        assert_eq!(
            h.status_json(&id)["name"].as_str(),
            Some(safe),
            "qex must show {safe:?} for the name {name:?}"
        );
        // The list holds the SAFE form, and never the name itself.
        let all = h.ok(&["__complete", "ids"]);
        assert!(
            all.lines().any(|l| l == safe),
            "the list must offer {safe:?} for the name {name:?}: {all}"
        );
        if safe != name {
            assert!(
                !all.lines().any(|l| l == name),
                "the list must not offer the name {name:?} itself: {all}"
            );
        }
        // AND THE SAFE FORM FINDS THE JOB. Without this a press of TAB would
        // give a word that no command takes.
        assert_eq!(
            h.status_json(safe)["id"].as_str(),
            Some(id.as_str()),
            "`qex status {safe}` must find the job named {name:?}"
        );
        // The stored name still finds it as well. A user who knows the real
        // name must not lose it. A name that starts with `-` goes after `--`,
        // which is how every command line takes a value of that form.
        let found = if name.starts_with('-') {
            h.qex(&["status", "--", name])
        } else {
            h.qex(&["status", name])
        };
        assert!(
            found.status.success(),
            "`qex status` must still find the job by its stored name {name:?}"
        );
    }

    // A LONG NAME stops at 128 characters.
    let long = "y".repeat(200);
    h.submit(&["submit", &format!("--name={long}"), "--", "true"]);
    let all = h.ok(&["__complete", "ids"]);
    assert!(
        all.lines().any(|l| l == "y".repeat(128)),
        "a long name must stop at 128 characters"
    );
    assert!(
        all.lines().all(|l| l.chars().count() <= 128),
        "no candidate may be longer than 128 characters"
    );

    // TWO NAMES THAT GIVE ONE SAFE FORM fall into the answer that qex already
    // has for a name that names more than one job. There is no second error.
    //
    // `a b` and `a_b` both give `a_b`. `a-b` does NOT collide with them,
    // because `-` is inside the set and it is not replaced.
    h.submit(&["submit", "--name=x y", "--", "true"]);
    h.submit(&["submit", "--name=x_y", "--", "true"]);
    let out = h.qex(&["status", "x_y"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an ambiguous name must give an error"
    );
    assert!(
        err.contains("`x_y` names 2 jobs"),
        "the error must say how many jobs the word names: {err}"
    );
    assert!(
        err.contains("Give the id of the job that you want"),
        "the error must say what the reader must do: {err}"
    );
    assert_eq!(
        err.lines().filter(|l| l.starts_with("  ")).count(),
        2,
        "the error must list the two jobs: {err}"
    );
    // The list of the error shows the SAFE name of each job, so a reader can
    // copy an id and see which job it is.
    assert_eq!(
        err.lines().filter(|l| l.ends_with(" x_y")).count(),
        2,
        "the error must name each job: {err}"
    );

    // One candidate only, for two jobs with one safe form.
    let all = h.ok(&["__complete", "ids"]);
    assert_eq!(
        all.lines().filter(|l| *l == "x_y").count(),
        1,
        "one safe form gives one candidate: {all}"
    );

    h.ok(&["kill", &running, "--grace", "1s"]);
}

/// qex must SHOW the safe form of a name, and only that, in every output.
///
/// A job name is text that another agent chose. A name that holds an ESC byte,
/// written to a terminal by `qex list`, moves the cursor and writes over the
/// text around it: no shell and no TAB are needed. Sanitising at each output
/// closes that whole class.
///
/// The record on the disk keeps the name that the user gave, and `qex status`
/// still finds the job by it.
#[test]
fn every_output_shows_the_safe_name() {
    let h = Harness::with_default_config("safename");

    let stored = "deploy prod$(id)";
    let safe = "deploy_prod_id_";
    let id = h.submit(&["submit", &format!("--name={stored}"), "--", "true"]);
    // A name that holds an ESC byte. This is the one that hurts a terminal.
    let esc = "esc\u{1b}[2Jbad";
    let esc_id = h.submit(&["submit", &format!("--name={esc}"), "--", "true"]);
    h.until("both jobs stop", Duration::from_secs(45), || {
        h.state_of(&id) == "completed" && h.state_of(&esc_id) == "completed"
    });

    // 1. THE RECORD KEEPS THE STORED NAME. This change rewrites no history.
    let record = h.root.join("state/qex/jobs").join(&id).join("status.json");
    let text = std::fs::read_to_string(&record).expect("the record must exist");
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        value["name"].as_str(),
        Some(stored),
        "the record on the disk must keep the name that the user gave"
    );

    // 2. EVERY OUTPUT SHOWS THE SAFE FORM.
    let list = h.ok(&["list"]);
    assert!(list.contains(safe), "`qex list` must show {safe}: {list}");
    let status = h.ok(&["status", &id]);
    assert!(
        status.contains(&format!("name:      {safe}")),
        "`qex status` must show {safe}: {status}"
    );
    assert_eq!(
        h.status_json(&id)["name"].as_str(),
        Some(safe),
        "the JSON of `qex status` must hold the safe name"
    );
    let listed = h.list_json();
    assert!(
        listed
            .iter()
            .any(|j| j["name"].as_str() == Some(safe) && j["id"].as_str() == Some(id.as_str())),
        "the JSON of `qex list` must hold the safe name: {listed:?}"
    );
    for out in [
        h.ok(&["wait", &id]),
        h.ok(&["wait", &id, "--json"]),
        h.ok(&["du", "--json"]),
        h.ok(&["gc", "--dry-run", "--older-than", "0s", "--json"]),
        h.ok(&["__complete", "ids"]),
    ] {
        assert!(
            !out.contains(stored),
            "an output showed the stored name: {out}"
        );
    }

    // 3. THE ESC BYTE REACHES NO OUTPUT. Test the BYTES, and not the text: a
    // reader of a terminal never sees the byte, and that is the whole point.
    for args in [
        vec!["list"],
        vec!["list", "--json"],
        vec!["status", &esc_id],
        vec!["status", &esc_id, "--json"],
        vec!["wait", &esc_id],
        vec!["wait", &esc_id, "--json"],
        vec!["du", "--json"],
        vec!["du"],
        vec!["gc", "--dry-run", "--older-than", "0s", "--json"],
        vec!["__complete", "ids"],
        vec!["top", "--once"],
    ] {
        let out = h.qex(&args);
        // `qex top` paints with its own escape codes, so look for the bytes of
        // the NAME and not for an escape byte anywhere.
        for stream in [&out.stdout, &out.stderr] {
            assert!(
                !stream.windows(6).any(|w| w == b"esc\x1b[2"),
                "`qex {}` wrote the ESC byte of a job name",
                args.join(" ")
            );
        }
    }

    // 5. A NAME THAT REACHES A READER INSIDE ANOTHER SENTENCE.
    //
    // The sentence that says why a job waits, the sentence that says which job
    // failed, and the sentence that says that a record is gone each carry the
    // name of a DIFFERENT job. Those sentences go to the reader in the same
    // way, so they hold the safe name too.
    let holder = h.submit(&[
        "submit",
        &format!("--name={esc}"),
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--lock",
        "one",
        "--",
        "sleep",
        "300",
    ]);
    let waiter = h.submit(&[
        "submit", "--name", "waiter", "--cpu", "1", "--mem", "64MB", "--lock", "one", "--", "true",
    ]);
    h.until(
        "the second job waits for the lock",
        Duration::from_secs(45),
        || h.status_json(&waiter)["blocked_reason"].is_string(),
    );

    // A job that FAILED, and a job that needed it.
    let broken = h.submit(&[
        "submit",
        &format!("--name={esc}"),
        "--",
        "sh",
        "-c",
        "exit 3",
    ]);
    let dependent = h.submit(&["submit", "--needs", &broken, "--", "true"]);
    h.until(
        "the dependent job is skipped",
        Duration::from_secs(45),
        || h.state_of(&dependent) == "skipped",
    );

    // And a record that something deleted.
    let gone = h.submit(&["submit", &format!("--name={esc}"), "--", "true"]);
    h.until("that job stops", Duration::from_secs(45), || {
        h.state_of(&gone) == "completed"
    });
    h.ok(&["clean", &gone]);

    for args in [
        vec!["list"],
        vec!["list", "--json"],
        vec!["status", &waiter],
        vec!["status", &waiter, "--json"],
        vec!["status", &dependent],
        vec!["status", &dependent, "--json"],
        vec!["status", &gone],
        vec!["top", "--once"],
    ] {
        let out = h.qex(&args);
        for stream in [&out.stdout, &out.stderr] {
            assert!(
                !stream.windows(6).any(|w| w == b"esc\x1b[2"),
                "`qex {}` wrote the ESC byte of a job name",
                args.join(" ")
            );
        }
    }
    // The sentences must still NAME the other job, in its safe form. A test
    // that only looks for the absence of a byte passes when the name is gone.
    let reason = h.status_json(&waiter)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        reason.contains("esc_2Jbad"),
        "the sentence must name the job that holds the lock: {reason}"
    );
    let failed = h.status_json(&dependent)["error"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        failed.contains("esc_2Jbad"),
        "the sentence must name the job that failed: {failed}"
    );
    let missing = String::from_utf8_lossy(&h.qex(&["status", &gone]).stderr).to_string();
    assert!(
        missing.contains("esc_2Jbad"),
        "the sentence must name the job whose record is gone: {missing}"
    );

    // The sentence that names a job that a QUEUED job waits for.
    let needs_holder = h.submit(&["submit", "--needs", &holder, "--", "true"]);
    h.until("that job waits", Duration::from_secs(45), || {
        h.status_json(&needs_holder)["blocked_reason"].is_string()
    });
    let reason = h.status_json(&needs_holder)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        reason.contains("esc_2Jbad") && !reason.contains('\u{1b}'),
        "the sentence must name the job that this one waits for, safely: {reason}"
    );

    // The sentence that `qex clean` gives when a job in the queue needs the
    // record. It names the job that WAITS.
    let done = h.submit(&["submit", "--", "true"]);
    h.until("a job to delete stops", Duration::from_secs(45), || {
        h.state_of(&done) == "completed"
    });
    // The waiting job needs that record AND the lock, so it stays in the queue
    // while the record it needs is already deletable.
    let waiting_esc = h.submit(&[
        "submit",
        &format!("--name={esc}"),
        "--needs",
        &done,
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--lock",
        "one",
        "--",
        "true",
    ]);
    h.until("that job waits too", Duration::from_secs(45), || {
        h.state_of(&waiting_esc) == "queued"
    });
    let refused = h.qex(&["clean", &done]);
    let text = String::from_utf8_lossy(&refused.stderr).to_string();
    assert!(
        text.contains("is needed by"),
        "`qex clean` must refuse a record that a job in the queue needs: {text}"
    );
    assert!(
        text.contains("esc_2Jbad") && !text.contains('\u{1b}'),
        "that sentence must name the waiting job, safely: {text}"
    );

    // The sentence that `qex clean` writes into a job that did not run. It
    // names the job whose record goes.
    h.ok(&["clean", &broken]);
    let after = h.status_json(&dependent)["error"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        after.contains("esc_2Jbad") && !after.contains('\u{1b}'),
        "the sentence must name the deleted job, safely: {after}"
    );

    // A PIPELINE gives a group a name, and that name reaches a reader too.
    //
    // ONE FIELD NAME CARRIES ONE VALUE. `qex list --json`, `qex pipeline
    // --json` and the id file all give the safe form, and `qex list --group`
    // takes it. A script that reads the value out of one of them and gives it
    // back to `--group` must find the jobs.
    let file = h.root.join("p.toml");
    std::fs::write(
        &file,
        "name = \"my grp\\u001B[2Jbad\"\n\n         [[jobs]]\nname = \"stg\\u001B[2Jbad\"\ncommand = [\"true\"]\n",
    )
    .unwrap();
    let id_file = h.root.join("ids.json");
    let made = h.qex(&[
        "pipeline",
        file.to_str().unwrap(),
        "--json",
        "--id-file",
        id_file.to_str().unwrap(),
    ]);
    assert!(made.status.success(), "the pipeline must start");

    // `qex pipeline --json` gives the SAME `group_name` as `qex list --json`.
    let started: serde_json::Value = serde_json::from_slice(&made.stdout).unwrap();
    assert_eq!(
        started["group_name"].as_str(),
        Some("my_grp_2Jbad"),
        "`qex pipeline --json` must give the safe group name: {started}"
    );

    // The line that `qex pipeline` writes for a PERSON holds the safe name of
    // each stage. The JSON above and the id file keep the name of the stage,
    // because a machine reads it as a key.
    let file2 = h.root.join("p2.toml");
    std::fs::write(
        &file2,
        "name = \"two grp\"\n\n         [[jobs]]\nname = \"stg\\u001B[2Jbad\"\ncommand = [\"true\"]\n",
    )
    .unwrap();
    let echo = h.qex(&["pipeline", file2.to_str().unwrap()]);
    assert!(
        !echo.stderr.windows(6).any(|w| w == b"stg\x1b[2"),
        "`qex pipeline` wrote the ESC byte of a stage name: {}",
        String::from_utf8_lossy(&echo.stderr)
    );
    assert!(
        String::from_utf8_lossy(&echo.stderr).contains("stg_2Jbad"),
        "`qex pipeline` must still name each stage: {}",
        String::from_utf8_lossy(&echo.stderr)
    );
    let listed = h.list_json();
    let group = listed
        .iter()
        .filter_map(|j| j["group_name"].as_str())
        .find(|n| n.contains("grp"))
        .expect("the pipeline must give the group a name")
        .to_string();
    assert_eq!(
        group, "my_grp_2Jbad",
        "the group name must reach a reader in its safe form"
    );
    let ids: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&id_file).unwrap()).unwrap();
    assert_eq!(
        ids["group_name"].as_str(),
        Some(group.as_str()),
        "the id file must give the same value as `qex list --json`: {ids}"
    );

    // `qex list --group` takes the value that qex showed, AND the name that
    // the file gave.
    for word in [group.as_str(), "my grp\u{1b}[2Jbad"] {
        let out = h.ok(&["list", "--group", word, "--json"]);
        let jobs: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(
            jobs.len(),
            1,
            "`qex list --group {word:?}` must find the job of the pipeline: {out}"
        );
        assert!(
            !out.contains('\u{1b}'),
            "no output holds an ESC byte: {out}"
        );
    }
    // And the group id still works.
    let gid = listed
        .iter()
        .find(|j| j["group_name"].as_str() == Some(group.as_str()))
        .unwrap()["group"]
        .as_str()
        .unwrap()
        .to_string();
    let out = h.ok(&["list", "--group", &gid, "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(jobs.len(), 1, "`qex list --group <id>` must still work");

    // The log of the coordinator is an output as well. A person reads it in a
    // terminal after a fault, so a name in it holds no ESC byte either.
    let log = std::fs::read(h.root.join("state/qex/run/daemon.log")).unwrap_or_default();
    assert!(
        !log.windows(6).any(|w| w == b"esc\x1b[2"),
        "the log of the coordinator wrote the ESC byte of a job name"
    );
    assert!(
        log.windows(9).any(|w| w == b"esc_2Jbad"),
        "the log of the coordinator must still name the job"
    );

    h.ok(&["kill", &holder, "--grace", "1s"]);

    // 4. THE ROUND TRIP, AND ITS LIMIT.
    //
    // The NAME column of the table stops at 16 characters, as it did before
    // this rule, so a word copied out of that column is not always the whole
    // name. `qex list --json` and `qex status` give the whole name, and the
    // documentation names those two. This test holds the boundary, so that a
    // later change to the column does not make the sentence false in silence.
    let long_name = "a-very-long-name-for-one-job";
    let long_id = h.submit(&["submit", "--name", long_name, "--", "true"]);
    let table = h.ok(&["list"]);
    assert!(
        !table.contains(long_name),
        "the table stops the name at 16 characters: {table}"
    );
    assert_eq!(
        h.status_json(long_name)["id"].as_str(),
        Some(long_id.as_str()),
        "the whole name must find the job"
    );
    assert_eq!(
        h.list_json()
            .iter()
            .find(|j| j["id"].as_str() == Some(long_id.as_str()))
            .unwrap()["name"]
            .as_str(),
        Some(long_name),
        "`qex list --json` must give the whole name"
    );

    // 4b. THE ROUND TRIP for a name that fits. A safe name that a reader copies out of `qex list`
    // goes back into `qex status` as it stands.
    let from_list = listed
        .iter()
        .find(|j| j["id"].as_str() == Some(id.as_str()))
        .unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        h.status_json(&from_list)["id"].as_str(),
        Some(id.as_str()),
        "the name that `qex list` shows must find the job"
    );

    // And the name that the user gave still finds it as well.
    assert_eq!(
        h.status_json(stored)["id"].as_str(),
        Some(id.as_str()),
        "the stored name must still find the job"
    );
}

/// bash must keep a hostile candidate in ONE word, whatever the answer holds.
///
/// `qex __complete` gives a SAFE form of each name now, so the real command no
/// longer answers with a `;` or a `$(`. That is not a reason to stop making the
/// word safe in the shell, and it is not a reason to stop testing it: **the
/// answer is text that came off a disk, and it is not a guarantee.** A record
/// that another program wrote, a record of a qex that is older or newer, and a
/// fault in the sanitiser all reach this function in the same way.
///
/// The test puts a `qex` of its own in front of the real one on the PATH, and
/// that one answers with the names that an attacker would choose.
#[test]
fn bash_keeps_a_hostile_candidate_in_one_word() {
    let h = Harness::with_default_config("bashquote");
    let script = h.root.join("qex.bash");
    std::fs::write(&script, h.ok(&["completions", "bash"])).unwrap();

    // The answer of the stand-in. Each line is one candidate.
    let bait = h.root.join("BAIT");
    let hostile = [
        format!("bait$(touch {})", bait.display()),
        format!("tick`touch {}`", bait.display()),
        format!("semi; touch {}", bait.display()),
        format!("pipe | touch {}", bait.display()),
        format!("amp & touch {}", bait.display()),
        "has space inside".to_string(),
        "quote\"double".to_string(),
        "quote'single".to_string(),
        "glob-*".to_string(),
        "[abc]".to_string(),
        "back\\slash".to_string(),
        "trailing\\".to_string(),
        "${IFS}brace".to_string(),
        "$HOME".to_string(),
        "~/tilde".to_string(),
        "!hist".to_string(),
        "esc\u{1b}[2Jname".to_string(),
        "caf\u{e9} \u{65e5}\u{672c}".to_string(),
        "x".repeat(2002),
    ];

    // A `qex` that answers with those names, and gives every other command to
    // the real one.
    let bin = h.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let stand_in = bin.join("qex");
    std::fs::write(
        &stand_in,
        format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = __complete ]; then cat {answers}; exit 0; fi\n\
             exec {real} \"$@\"\n",
            answers = h.root.join("answers").display(),
            real = env!("CARGO_BIN_EXE_qex"),
        ),
    )
    .unwrap();
    std::fs::write(h.root.join("answers"), format!("{}\n", hostile.join("\n"))).unwrap();
    std::process::Command::new("chmod")
        .args(["+x", stand_in.to_str().unwrap()])
        .status()
        .unwrap();

    // `_qex_jobs` alone, and not `_qex_with_jobs`. The wrapper adds the
    // options of clap first, and this test asks about the ONE candidate that
    // the jobs part gives. The wrapper and the registration have their own test.
    let ask = |prefix: &str, tail: &str| -> String {
        let program = format!(
            "source {script}\n\
             COMP_WORDS=(qex status '{prefix}')\n\
             COMP_CWORD=2\n\
             COMP_LINE='qex status {prefix}'\n\
             COMP_POINT=${{#COMP_LINE}}\n\
             COMPREPLY=()\n\
             _qex_jobs 2>/dev/null\n\
             {tail}\n",
            script = script.display(),
        );
        let out = Command::new("bash")
            .arg("-c")
            .arg(&program)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .output()
            .expect("bash did not start");
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    for name in &hostile {
        // One name in the answer, so `COMPREPLY[0]` is that name and no other.
        // An empty word matches every candidate.
        std::fs::write(h.root.join("answers"), format!("{name}\n")).unwrap();
        let read = ask(
            "",
            "eval \"set -- ${COMPREPLY[0]}\" 2>/dev/null; printf '%s\\n' \"$#\"; printf '%s' \"$1\"",
        );
        let mut lines = read.splitn(2, '\n');
        assert_eq!(
            lines.next(),
            Some("1"),
            "the candidate for {name:?} must be ONE argument: {read:?}"
        );
        assert_eq!(
            lines.next(),
            Some(name.as_str()),
            "the argument must be the name itself, for {name:?}"
        );
    }

    // AND NOTHING RAN, at the press of TAB or at the press of ENTER.
    assert!(!bait.exists(), "a candidate RAN: {}", bait.display());

    // A candidate that starts with `-` must not arrive where an option goes.
    std::fs::write(h.root.join("answers"), "-version\n--json\n").unwrap();
    let reply = ask("-", "printf '%s\\n' \"${COMPREPLY[@]}\"");
    assert!(
        !reply.lines().any(|l| l == "-version"),
        "a candidate must not be offered where an option goes: {reply}"
    );
}

/// The bash completions must offer the jobs when a REAL bash runs them.
///
/// A test that reads the generated text says only that the words are there. It
/// cannot say that bash gives the right candidates, and this project has
/// shipped a completion whose shell part never ran.
///
/// **A job name must not run when somebody presses TAB.** An agent chooses the
/// names of the jobs and a person presses the TAB, so a name is text that an
/// attacker writes. The test gives a job the name `bait$(touch FILE)` and
/// requires that the file does not appear.
#[test]
fn a_real_bash_offers_the_jobs_and_runs_no_name() {
    let h = Harness::with_default_config("bashcomp");

    let running = h.submit(&[
        "submit", "--name", "holder", "--cpu", "1", "--mem", "64MB", "--lock", "one", "--",
        "sleep", "300",
    ]);
    let queued = h.submit(&[
        "submit", "--name", "waiter", "--cpu", "1", "--mem", "64MB", "--lock", "one", "--", "true",
    ]);
    h.until("the first job operates", Duration::from_secs(45), || {
        h.state_of(&running) == "running"
    });

    // The names that an attacker would choose.
    let bait = h.root.join("BAIT");
    h.submit(&[
        "submit",
        "--name",
        &format!("bait$(touch {})", bait.display()),
        "--",
        "true",
    ]);
    h.submit(&["submit", "--name", "two words", "--", "true"]);

    let script = h.root.join("qex.bash");
    std::fs::write(&script, h.ok(&["completions", "bash"])).unwrap();

    // Ask bash for the candidates the way that bash asks for them.
    //
    // The completion makes each word safe by itself, with `printf %q`. This
    // test measures the bash that is on the PATH of the machine that runs it.
    let ask_with = |prelude: &str, line: &[&str], tail: &str| -> String {
        let words = line
            .iter()
            .map(|w| format!("'{}'", w.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");
        let last = line.len() - 1;
        let program = format!(
            "{prelude}\n\
             source {script}\n\
             COMP_WORDS=({words})\n\
             COMP_CWORD={last}\n\
             COMP_LINE='{comp_line}'\n\
             COMP_POINT=${{#COMP_LINE}}\n\
             COMPREPLY=()\n\
             _qex_with_jobs qex \"${{COMP_WORDS[{last}]}}\" 2>/dev/null\n\
             {tail}\n",
            script = script.display(),
            comp_line = line.join(" ").replace('\'', "'\\''"),
        );
        let bin = Path::new(env!("CARGO_BIN_EXE_qex")).parent().unwrap();
        let out = Command::new("bash")
            .arg("-c")
            .arg(&program)
            .env("XDG_CONFIG_HOME", h.root.join("cfg"))
            .env("XDG_STATE_HOME", h.root.join("state"))
            .env("XDG_RUNTIME_DIR", h.root.join("run"))
            .env("QEX_IDLE_EXIT_SECS", "120")
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .output()
            .expect("bash did not start");
        assert!(
            out.status.success(),
            "bash failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    // THE REGISTRATION MUST RUN, AND IT MUST BIND OUR FUNCTION.
    //
    // Every other test here calls `_qex_with_jobs` by its name, so none of them
    // can see a registration that failed. `-o nosort` came with bash 4.4, and
    // `complete` refuses the whole command when it meets an option name that it
    // does not know: on the bash 3.2 of macOS the file thus bound nothing, and
    // it wrote an error at each shell start. This test reads what the shell
    // holds after the file ran, and it requires a silent stderr.
    let out = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "set -e\nsource {}\ncomplete -p qex\n",
            script.display()
        ))
        .output()
        .expect("bash did not start");
    let bound = String::from_utf8_lossy(&out.stdout).to_string();
    let noise = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "sourcing the completions failed: {noise}"
    );
    assert_eq!(
        noise, "",
        "sourcing the completions must write nothing: {noise}"
    );
    assert!(
        bound.contains("-F _qex_with_jobs"),
        "the shell must call `_qex_with_jobs` after TAB, and it holds: {bound}"
    );

    let ask = |line: &[&str]| ask_with("", line, "printf '%s\\n' \"${COMPREPLY[@]}\"");
    // What the shell reads when the user then presses ENTER. `set --` splits
    // the completed word the way that the command line does, so the count says
    // whether the name stayed ONE argument, and a name that holds `$(...)`
    // runs here when the completion did not make it safe.
    let read_back = |line: &[&str]| {
        ask_with(
            "",
            line,
            "eval \"set -- ${COMPREPLY[0]}\" 2>/dev/null; printf '%s\\n' \"$#\" \"$@\"",
        )
    };
    check_the_bash_candidates(&ask, &read_back, &running, &queued, &bait);

    h.ok(&["kill", &running, "--grace", "1s"]);
}

/// The candidates that bash gives, for one way of making a word safe.
#[allow(clippy::type_complexity)]
fn check_the_bash_candidates(
    ask: &dyn Fn(&[&str]) -> String,
    read_back: &dyn Fn(&[&str]) -> String,
    running: &str,
    queued: &str,
    bait: &Path,
) {
    // A command that reads a record offers every job, by id and by name.
    let reply = ask(&["qex", "status", ""]);
    for want in [running, queued, "holder", "waiter"] {
        assert!(
            reply.lines().any(|l| l == want),
            "bash must offer {want}: {reply}"
        );
    }

    // The prefix chooses. `qex status hol<TAB>` gives one job.
    let reply = ask(&["qex", "status", "hol"]);
    assert_eq!(reply.trim(), "holder", "bash must complete the name");

    // `qex kill` takes a job that operates, and `qex cancel` takes one that
    // waits. A candidate that the command refuses teaches the wrong command.
    let reply = ask(&["qex", "kill", ""]);
    assert!(reply.lines().any(|l| l == running), "kill: {reply}");
    assert!(!reply.lines().any(|l| l == queued), "kill: {reply}");
    let reply = ask(&["qex", "cancel", ""]);
    assert!(reply.lines().any(|l| l == queued), "cancel: {reply}");
    assert!(!reply.lines().any(|l| l == running), "cancel: {reply}");

    // The value of an option is not a job.
    let reply = ask(&["qex", "kill", "--signal", ""]);
    assert!(
        !reply.lines().any(|l| l == running),
        "`qex kill --signal <TAB>` must not offer a job: {reply}"
    );

    // A FLAG takes no value, so the word after it IS a job. The guard read
    // every word that starts with a dash, and `qex status --json <TAB>` then
    // offered nothing.
    for flag in ["--json", "--show-env"] {
        let reply = ask(&["qex", "status", flag, ""]);
        assert!(
            reply.lines().any(|l| l == "holder"),
            "`qex status {flag} <TAB>` must still offer a job: {reply}"
        );
    }

    // `qex clean` takes a job id or a job name, so it offers them. Without
    // this it offered the files of the current directory.
    let reply = ask(&["qex", "clean", ""]);
    assert!(
        reply.lines().any(|l| l == "holder"),
        "`qex clean <TAB>` must offer a job: {reply}"
    );

    // THE LIST HOLDS THE SAFE FORM of a name that is not plain.
    //
    // `two words` arrives as `two_words`, and that word finds the job.
    let reply = ask(&["qex", "status", "two"]);
    assert_eq!(reply.trim(), "two_words", "the safe form only: {reply}");
    let read = read_back(&["qex", "status", "two"]);
    assert_eq!(
        read.trim(),
        "1\ntwo_words",
        "the completed word must be ONE argument of qex: {read}"
    );

    // AND NOTHING RAN. The job named `bait$(touch FILE)` made no file, at the
    // press of TAB and at the press of ENTER.
    let reply = ask(&["qex", "status", "bait"]);
    assert_eq!(reply.lines().count(), 1, "one candidate only: {reply}");
    assert!(
        !reply.contains("$("),
        "the list must hold the safe form: {reply}"
    );
    let read = read_back(&["qex", "status", "bait"]);
    assert_eq!(read.lines().next(), Some("1"), "one argument only: {read}");
    assert!(
        !bait.exists(),
        "a job name RAN when bash asked for the candidates: {}",
        bait.display()
    );
}

/// A job must give way to the work of a person.
///
/// The queue controls how many cores a job uses. It does not control how rudely
/// the job uses them, and a build inside its budget still makes an editor
/// stutter. qex knows what the scheduler of the machine does not: this work sat
/// in a queue, so nobody waits for the next second of it.
#[test]
fn a_job_gives_way_to_the_work_of_a_person() {
    let h = Harness::with_default_config("polite");

    // `ps` reports the nice value on Linux and on macOS alike.
    let id = h.submit(&["submit", "--", "sh", "-c", "ps -o ni= -p $$"]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(
        out.trim(),
        "10",
        "a job must be polite by default, and this one was not: {out}"
    );

    // A job that asks not to give way says so. The coordinator of this test
    // runs at nice 0, so qex does not have to LOWER the value here and 0
    // reaches the job. A coordinator under `nice 5` could not do this.
    let id = h.submit(&["submit", "--nice", "0", "--", "sh", "-c", "ps -o ni= -p $$"]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(out.trim(), "0", "`--nice 0` must reach the job: {out}");

    // A machine that refuses the change must still run the job. The system
    // refuses a number below the one that the coordinator has, unless qex has
    // privilege, and qex does not ask for privilege.
    let id = h.submit(&["submit", "--nice", "-5", "--", "true"]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    assert_eq!(
        h.status_json(&id)["state"],
        "completed",
        "a nice value that the machine refuses must not stop the job"
    );
}

/// Each `[politeness]` value must reach the job AND every child of the job.
///
/// The child matters more than the job itself. A job is frequently a shell that
/// starts a build, and the build does the work. Linux keeps the nice value, the
/// io class and the oom score across a fork, so one call between the fork and
/// the exec covers the whole tree. This test measures the tree, and not the
/// call, because a later change could set the values on the wrong process.
///
/// EVERY io class needs its own run. `ionice_set` takes one number that holds
/// the class and the level together, so `best-effort` and `idle` share no code
/// beyond the call. A test of `idle` alone left the whole `best-effort` line
/// free: a review deleted it, and changed `|` to `&`, to `^`, and `<<` to `>>`,
/// and every one of the four still passed.
#[test]
#[cfg(target_os = "linux")]
fn every_politeness_value_reaches_the_job_and_its_children() {
    // The name of the class as `ionice -p` gives it, for each name that
    // `[politeness] io` takes. `best-effort` carries the level, because the
    // level is half of the number that qex builds.
    for (io, expect) in [
        ("idle", "idle"),
        ("best-effort", "best-effort: prio 4"),
        ("none", "none: prio 0"),
    ] {
        let h = Harness::new(
            &format!("polite-{io}"),
            &format!(
                "[peers]\nenabled = false\n\
                 [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
                 [politeness]\nnice = 12\nio = \"{io}\"\noom_score_adj = 500\n"
            ),
        );

        // Field 19 of /proc/<pid>/stat is the nice value. The command reads the
        // three values for itself, and then again in a child of itself.
        let probe = "report() { \
                     echo \"$1 nice=$(awk '{print $19}' /proc/$2/stat) \
                     oom=$(cat /proc/$2/oom_score_adj) io=$(ionice -p $2)\"; }; \
                     report parent $$; sh -c 'report() { \
                     echo \"$1 nice=$(awk \"{print \\$19}\" /proc/$2/stat) \
                     oom=$(cat /proc/$2/oom_score_adj) io=$(ionice -p $2)\"; }; \
                     report child $$'";
        // `[politeness]` changes the priority of every job, so the text form of
        // `qex config show` must name the values. A user who asks why a build
        // is slow reads this text, and not the JSON.
        let shown = h.ok(&["config", "show"]);
        assert!(
            shown.contains("nice 12") && shown.contains(&format!("io {io}")),
            "`qex config show` must name the politeness values: {shown}"
        );

        let id = h.submit(&["submit", "--", "sh", "-c", probe]);
        h.ok(&["wait", &id, "--timeout", "45s"]);
        let out = h.ok(&["logs", &id, "--stdout"]);

        for who in ["parent", "child"] {
            let line = out
                .lines()
                .find(|l| l.starts_with(who))
                .unwrap_or_else(|| panic!("the job gave no {who} line: {out}"));
            assert!(
                line.contains("nice=12"),
                "the {who} must take `[politeness] nice`: {line}"
            );
            assert!(
                line.contains("oom=500"),
                "the {who} must take `[politeness] oom_score_adj`: {line}"
            );
            assert!(
                line.contains(expect),
                "the {who} must take `io = \"{io}\"` as `{expect}`: {line}"
            );
        }
    }
}

/// A `[politeness]` value with a fault, at the moment that a job starts, must
/// give the DEFAULT values and name the fault.
///
/// `qex submit` tests the file, but the file can change after the submission: a
/// job can wait in the queue while somebody edits the file, and `qex rerun`
/// needs no config file at all. The supervisor reads the file for itself and
/// parses it WITHOUT validating it, so without a second test the job would take
/// the value as it is. Measured: `nice = 100` ran a job at nice 19, because
/// `setpriority` moves the number into the range and reports success, and
/// nothing said so.
#[test]
#[cfg(target_os = "linux")]
fn a_politeness_value_with_a_fault_at_the_start_gives_the_default_values() {
    let good = "[peers]\nenabled = false\n\
                [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
                [politeness]\nnice = 12\n";
    let h = Harness::new("polite-late", good);
    let probe = "awk '{print $19}' /proc/$$/stat";

    let first = h.submit(&["submit", "--", "sh", "-c", probe]);
    h.ok(&["wait", &first, "--timeout", "45s"]);
    assert_eq!(h.ok(&["logs", &first, "--stdout"]).trim(), "12");

    // The file goes wrong AFTER the submission. The coordinator refuses it and
    // keeps the values that it had; the supervisor must refuse it as well.
    std::fs::write(
        h.root.join("cfg/qex.toml"),
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [politeness]\nnice = 100\n",
    )
    .unwrap();

    let out = h.qex(&["rerun", &first]);
    assert!(out.status.success(), "`qex rerun` must still start the job");
    let again = String::from_utf8_lossy(&out.stdout).trim().to_string();

    h.ok(&["wait", &again, "--timeout", "45s"]);
    let status = h.status_json(&again);
    assert_eq!(
        status["state"], "completed",
        "a config file with a fault must not stop the job: {status}"
    );
    assert_eq!(
        h.ok(&["logs", &again, "--stdout"]).trim(),
        "10",
        "the job must take the DEFAULT nice value, and not the value with the fault"
    );
    let error = status["error"].as_str().unwrap_or("");
    assert!(
        error.contains("[politeness] nice"),
        "the record of the job must name the fault: {status}"
    );
}

/// A job must be told how large its claim is.
///
/// A claim controls the queue and not the job. A job that asks the machine how
/// many cores it has receives the number of the MACHINE, so a job with a claim
/// of two cores on a machine of sixteen starts sixteen threads and takes the
/// capacity that qex gave to the other jobs.
#[test]
fn a_job_is_told_the_size_of_its_claim() {
    let h = Harness::with_default_config("claimenv");

    let id = h.submit(&[
        "submit",
        "--cpu",
        "2",
        "--mem",
        "2GB",
        "--",
        "sh",
        "-c",
        "echo \"$QEX_CPU $QEX_MEM_MB $GOMAXPROCS $OMP_NUM_THREADS\"",
    ]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(
        out.trim(),
        "2 2048 2 2",
        "the job must see its own claim: {out}"
    );

    // A value that somebody chose must stay. `--env` is a decision.
    let id = h.submit(&[
        "submit",
        "--cpu",
        "2",
        "--env",
        "GOMAXPROCS=9",
        "--",
        "sh",
        "-c",
        "echo \"$GOMAXPROCS $OMP_NUM_THREADS\"",
    ]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(
        out.trim(),
        "9 2",
        "`--env` must win, and the rest must still arrive: {out}"
    );

    // `--no-limit-env-hints` is the answer for a job that must see the machine
    // as it is.
    let id = h.submit(&[
        "submit",
        "--cpu",
        "2",
        "--no-limit-env-hints",
        "--",
        "sh",
        "-c",
        "echo \"[$QEX_CPU][$GOMAXPROCS]\"",
    ]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(out.trim(), "[][]", "the option must write nothing: {out}");

    // A job file must be able to say the same thing. Without the field, the
    // file is refused, because a job file rejects a name that qex does not
    // know.
    let file = h.root.join("off.toml");
    std::fs::write(
        &file,
        "name = \"off\"\n\
         command = [\"sh\", \"-c\", \"echo \\\"[$QEX_CPU][$GOMAXPROCS]\\\"\"]\n\
         no_limit_env_hints = true\n\n\
         [resources]\ncpu = 2\n",
    )
    .unwrap();
    let id = h.submit(&["submit", "--job", file.to_str().unwrap()]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(out.trim(), "[][]", "the job file must turn it off: {out}");

    // And the same file without that line gets the claim.
    let file = h.root.join("on.toml");
    std::fs::write(
        &file,
        "name = \"on\"\n\
         command = [\"sh\", \"-c\", \"echo \\\"[$QEX_CPU][$GOMAXPROCS]\\\"\"]\n\n\
         [resources]\ncpu = 2\n",
    )
    .unwrap();
    let id = h.submit(&["submit", "--job", file.to_str().unwrap()]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(
        out.trim(),
        "[2][2]",
        "the job file must get the claim: {out}"
    );
}
