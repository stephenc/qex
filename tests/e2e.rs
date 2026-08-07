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
