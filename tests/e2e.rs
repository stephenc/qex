//! End-to-end tests for qex.
//!
//! Each test makes its own config directory, state directory, runtime
//! directory and peer directory in a temporary location. A test thus starts
//! its own coordinator, and it does not touch the coordinator of the user or
//! the shared peer directory of the machine.
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
    /// Variables that each command of this test gets in addition.
    extra_env: Vec<(String, String)>,
    /// The peer directory of this test. Every command sets `QEX_PEERS_DIR` to
    /// this path, so a suite run cannot write into `/tmp/qex`.
    peers_dir: PathBuf,
}

/// The name of the variable that moves the peer directory. It must match
/// `src/peers.rs`. An integration test cannot read that constant.
const QEX_PEERS_DIR: &str = "QEX_PEERS_DIR";

/// The live peer directory of this machine. A test that would write here
/// writes into the queue of the person who runs the suite.
const LIVE_PEERS_DIR: &str = "/tmp/qex";

/// Gives the peer directory for one test.
///
/// A config that already names a directory that is not `/tmp/qex` keeps that
/// directory, so two coordinators can still share a private one. Every other
/// config gets `isolated`. `/tmp/qex` is never the answer: that path is the
/// live queue.
fn peers_dir_for_test(config: &str, isolated: &Path) -> PathBuf {
    for line in config.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("dir") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let value = rest.trim().trim_matches('"');
        if !value.is_empty() && value != LIVE_PEERS_DIR {
            return PathBuf::from(value);
        }
    }
    isolated.to_path_buf()
}

/// Puts the isolation of one test onto a command.
///
/// Threads and a copied program cannot call `Harness::command`. They must
/// still set the same variables, or a coordinator they start writes into
/// `/tmp/qex`.
fn isolate(cmd: &mut Command, root: &Path, peers: &Path) {
    cmd.env("XDG_CONFIG_HOME", root.join("cfg"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("QEX_IDLE_EXIT_SECS", "120")
        .env(QEX_PEERS_DIR, peers);
}

/// Gives one stream of a command in a form for a failure message.
///
/// A command that wrote NOTHING is a different fault from a command that wrote
/// a part of its output and then stopped. A reader must see which one
/// happened, so an empty stream says `no output` with a word. An empty space
/// in a message reads as an accident of the format.
///
/// A long output keeps the LAST lines, because the end of the output shows the
/// state of the command when it stopped. The message names the count of the
/// lines that it does not show, so a reader knows that more exist.
fn describe_stream(name: &str, bytes: &[u8]) -> String {
    /// The count of lines that a message holds for one stream.
    const KEEP: usize = 20;

    let raw = String::from_utf8_lossy(bytes);
    let text = raw.trim_end_matches('\n');
    if text.is_empty() {
        return format!("{name}: no output");
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= KEEP {
        return format!("{name}:\n{}", lines.join("\n"));
    }
    format!(
        "{name}: the last {KEEP} lines of {}, and {} more above:\n{}",
        lines.len(),
        lines.len() - KEEP,
        lines[lines.len() - KEEP..].join("\n")
    )
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
        let isolated = root.join("peers");
        std::fs::create_dir_all(&isolated).unwrap();
        // The product refuses a peer directory without the sticky bit.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&isolated, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let peers_dir = peers_dir_for_test(config, &isolated);
        assert_ne!(
            peers_dir,
            Path::new(LIVE_PEERS_DIR),
            "the suite must not write into the live peer directory {LIVE_PEERS_DIR}"
        );
        std::fs::write(root.join("cfg/qex.toml"), config).unwrap();
        Self {
            root,
            extra_env: Vec::new(),
            peers_dir,
        }
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

    /// Builds a command for this test.
    ///
    /// Every environment value of a test lives here. With a second place, a
    /// reader must change both, and a miss is silent.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_qex"));
        cmd.args(args);
        isolate(&mut cmd, &self.root, &self.peers_dir);
        cmd.envs(self.extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd
    }

    fn qex(&self, args: &[&str]) -> Output {
        self.command(args)
            .output()
            .unwrap_or_else(|e| panic!("`qex {}` did not start: {e}", args.join(" ")))
    }

    /// Runs a command and requires an answer inside a time limit.
    ///
    /// Use this for each command that WAITS. A test that hangs reports
    /// nothing, and the reader must find the cause by hand. This helper stops
    /// the command at the limit and FAILS.
    ///
    /// The failure message carries what the command wrote. On a build machine
    /// nobody can run the command again, so that output is the only evidence
    /// of the fault. See `describe_stream` for the form of it.
    ///
    /// The limit belongs to the TEST. A `--timeout` of qex acts after qex
    /// resolves the id, so it cannot bound a fault before that point.
    fn qex_within(&self, args: &[&str], limit: Duration) -> Output {
        let shown = args.join(" ");
        let mut child = self
            .command(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("`qex {shown}` did not start: {e}"));

        // Read both streams while the command runs. A pipe that fills stops
        // the command, and this test must not make that condition itself.
        let mut out_pipe = child
            .stdout
            .take()
            .unwrap_or_else(|| panic!("`qex {shown}` gives no stdout"));
        let mut err_pipe = child
            .stderr
            .take()
            .unwrap_or_else(|| panic!("`qex {shown}` gives no stderr"));
        let read_out = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut out_pipe, &mut buf);
            buf
        });
        let read_err = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut err_pipe, &mut buf);
            buf
        });

        // This function holds the child and reaps it. The process thus stays
        // in the table of processes until the kill, and the number always
        // names this command. A number that a test reads after a different
        // process reaps the child can name a later process.
        let started = Instant::now();
        let status = loop {
            match child
                .try_wait()
                .unwrap_or_else(|e| panic!("the test cannot read `qex {shown}`: {e}"))
            {
                Some(status) => break status,
                None if started.elapsed() >= limit => {
                    // Stop the command first. The pipes then close, so each
                    // reader ends and gives what the command wrote.
                    child.kill().ok();
                    child.wait().ok();
                    let out = read_out.join().unwrap_or_default();
                    let err = read_err.join().unwrap_or_default();
                    panic!(
                        "`qex {shown}` gave no answer in {limit:?}. A wait that \
                         nothing can satisfy is the fault that qex removes.\n{}\n{}",
                        describe_stream("stdout", &out),
                        describe_stream("stderr", &err),
                    );
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        };

        Output {
            status,
            stdout: read_out.join().unwrap_or_default(),
            stderr: read_err.join().unwrap_or_default(),
        }
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

    /// Stops a job that a test started, and leaves no process behind.
    ///
    /// `qex kill` refuses a job that is between the queue and its first
    /// process, so this function waits for the job to start. Without it, a test
    /// leaves a `sleep` process on the machine, and the next test then measures
    /// a machine that is busy.
    fn stop(&self, id: &str) {
        self.until("the job starts", Duration::from_secs(45), || {
            self.has_started(id)
        });
        // Do not require the kill to succeed. A job can stop by ITSELF between
        // the test above and this command, and the coordinator then refuses
        // the kill with "the job has no process". That is a race in the test
        // and never a fault of the code under test: the purpose here is to
        // leave no process behind.
        self.qex(&["kill", id, "--grace", "1s"]);
    }

    /// Writes a stored name into an existing job record, as an earlier qex did.
    ///
    /// A later qex refuses such a name at submission. The record on the disk
    /// can still hold one. Stop the coordinator first, so the next command
    /// loads this name from the disk.
    fn plant_stored_name(&self, id: &str, name: &str) {
        for file in ["status.json", "spec.json"] {
            let path = self.root.join("state/qex/jobs").join(id).join(file);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {} for a planted name: {e}", path.display()));
            let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
            value["name"] = serde_json::Value::String(name.to_string());
            std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
        }
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

    /// Starts a qex command beside this test, and gives the child.
    ///
    /// A test that sends a signal to a WAIT needs the command to be alive while
    /// the test decides what to do, so the test cannot use `qex()`, which waits
    /// for the command to stop.
    #[allow(clippy::zombie_processes)]
    fn spawn(&self, args: &[&str]) -> std::process::Child {
        self.command(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("qex did not start")
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

        let child = self
            .command(&all)
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

    /// Runs a command and gives it this text on standard input.
    ///
    /// `qex submit --each-line -` reads the lines from another program, and a
    /// test must measure that path with real pipes.
    fn qex_stdin(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write;
        let mut child = self
            .command(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("qex did not start");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().expect("qex did not stop")
    }

    /// Writes the config file again, while the installation operates.
    ///
    /// A user does this with an editor. The coordinator that already operates
    /// keeps the values that it read when it started, and each NEW process
    /// reads what this function wrote.
    fn write_config(&self, config: &str) {
        std::fs::write(self.root.join("cfg/qex.toml"), config).unwrap();
    }

    /// Names the process that ran the stop hook of this job.
    ///
    /// The last word of `hook.ran` is `supervisor` or `coordinator`. Three
    /// paths reach the hook, and each one alone gives the person exactly one
    /// message, so a test that counts the messages passes when any one of them
    /// operates. This helper lets a test say WHICH one must operate.
    fn hook_origin(&self, id: &str) -> String {
        let text = std::fs::read_to_string(self.job_dir(id).join("hook.ran"))
            .unwrap_or_else(|e| panic!("hook.ran is not there for the job {id}: {e}"));
        text.split_whitespace()
            .next_back()
            .unwrap_or_default()
            .to_string()
    }

    /// Gives the lines that the stop hook of a test wrote.
    fn hook_lines(&self) -> Vec<String> {
        match std::fs::read_to_string(self.root.join("hook.txt")) {
            Ok(text) => text.lines().map(|l| l.trim().to_string()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Stops every process that still holds a file of this test.
    ///
    /// Do not ask the socket, and do not trust the pid file alone. A test can
    /// hide the socket, delete the pid file, or force the short socket path.
    /// The coordinator still holds `daemon.log` under this root. A number in
    /// a file is not enough: the system gives that number to a new process.
    fn stop_coordinator(&self) {
        let pids = self.pids_holding_this_root();
        for pid in &pids {
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        for pid in pids {
            while Instant::now() < deadline && unsafe { libc::kill(pid, 0) } == 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn may_stop(pid: i32) -> bool {
        pid > 1 && pid != std::process::id() as i32 && pid != unsafe { libc::getppid() }
    }

    /// Finds processes that still hold a file under this harness root.
    fn pids_holding_this_root(&self) -> Vec<i32> {
        let mut pids = std::collections::BTreeSet::new();
        if let Ok(proc) = std::fs::read_dir("/proc") {
            for entry in proc.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<i32>().ok())
                else {
                    continue;
                };
                if !Self::may_stop(pid) {
                    continue;
                }
                let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
                    continue;
                };
                let holds = fds.flatten().any(|fd| {
                    std::fs::read_link(fd.path()).is_ok_and(|target| target.starts_with(&self.root))
                });
                if holds {
                    pids.insert(pid);
                }
            }
            return pids.into_iter().collect();
        }
        if let Ok(out) = Command::new("lsof")
            .args(["-t", "+D"])
            .arg(&self.root)
            .output()
        {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Ok(pid) = line.trim().parse::<i32>() {
                    if Self::may_stop(pid) {
                        pids.insert(pid);
                    }
                }
            }
        }
        pids.into_iter().collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Stop the coordinator of this test, then delete the directory.
        //
        // Do not ask the socket for the pid. A test can hide that socket or
        // replace it, and the coordinator then stays after cargo test returns.
        self.stop_coordinator();
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

/// Every wait gives the exit code of the job.
#[test]
fn a_wait_gives_the_exit_code_of_the_job() {
    let h = Harness::with_default_config("pass");
    let id = h.submit(&["submit", "--", "sh", "-c", "exit 42"]);
    assert_eq!(h.qex(&["wait", &id]).status.code(), Some(42));
}

/// A job that gives a code that qex keeps for itself gives the sentinel 97, and
/// the record holds the true code.
///
/// THIS TEST IS THE WHOLE FEATURE. Without the sentinel, a job that exits 124 of
/// its own accord looks exactly like a wait that reached its time limit, and the
/// two need different actions from the reader.
#[test]
fn a_job_code_that_qex_keeps_gives_the_sentinel() {
    let h = Harness::with_default_config("band124");
    let id = h.submit(&["submit", "--", "sh", "-c", "exit 124"]);

    let out = h.qex(&["wait", &id]);
    assert_eq!(
        out.status.code(),
        Some(97),
        "a job that exits 124 must not look like a wait that reached its limit"
    );
    assert_eq!(h.status_json(&id)["exit_code"], 124);
}

/// The two boundaries of the band. An error of one moves a job code into the
/// band of qex, or a code of qex out of it.
#[test]
fn the_boundaries_of_the_band_hold() {
    let h = Harness::with_default_config("bandedge");

    let low = h.submit(&["submit", "--", "sh", "-c", "exit 96"]);
    assert_eq!(h.qex(&["wait", &low]).status.code(), Some(96));

    // The sentinel value itself. The answer is the sentinel, so the rule stays
    // true for every code in the band.
    let sentinel = h.submit(&["submit", "--", "sh", "-c", "exit 97"]);
    assert_eq!(h.qex(&["wait", &sentinel]).status.code(), Some(97));
    assert_eq!(h.status_json(&sentinel)["exit_code"], 97);
}

/// A signal that stopped the job gives 98, and the record names the signal.
///
/// The usual form `128 + N` cannot serve: a job that the out-of-memory killer
/// stops gives 137, and a WAIT that the out-of-memory killer stops gives 137 as
/// well. The two need different actions.
#[test]
fn a_signal_that_stopped_the_job_gives_its_own_code() {
    let h = Harness::with_default_config("jobsignal");
    let id = h.submit(&["submit", "--", "sh", "-c", "kill -SEGV $$"]);

    let out = h.qex(&["wait", &id]);
    assert_eq!(
        out.status.code(),
        Some(98),
        "a signal in the job must not give `128 + N`, which qex keeps for itself"
    );
    let status = h.status_json(&id);
    assert_eq!(
        status["signal"],
        libc::SIGSEGV,
        "the record names the signal"
    );
}

/// A signal to the WAIT gives 122, and the job continues.
///
/// Ctrl-C on a wait gives 130 from the shell, which is `128 + 2`, and that form
/// says "the job died from a signal". The job did not: the wait died. A dead
/// process cannot label its own exit, so qex catches the signal.
#[test]
fn a_signal_to_the_wait_gives_the_code_of_a_broken_wait() {
    let h = Harness::with_default_config("waitsig");
    let id = h.submit(&["submit", "--", "sleep", "30"]);
    h.until("the job starts", Duration::from_secs(45), || {
        h.has_started(&id)
    });

    let child = h.spawn(&["wait", &id]);
    // Give the command time to reach its wait, so the signal meets the trap
    // and not the moments before it.
    std::thread::sleep(Duration::from_millis(800));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().expect("the wait did not stop");
    assert_eq!(
        out.status.code(),
        Some(122),
        "a wait that a signal stopped must not give 130"
    );

    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        said.contains("qex status") && said.contains("--wait"),
        "the message must give the command that attaches again: {said}"
    );

    // The job is the thing that matters. It must still operate.
    assert_eq!(
        h.state_of(&id),
        "running",
        "a signal to the wait must not stop the job"
    );
    h.stop(&id);
}

/// `--follow` must obey `--timeout`, and the job must continue.
#[test]
fn follow_obeys_the_time_limit_of_the_reader() {
    let h = Harness::with_default_config("followlimit");
    let id = h.submit(&["submit", "--", "sleep", "30"]);
    h.until("the job starts", Duration::from_secs(45), || {
        h.has_started(&id)
    });

    let started = Instant::now();
    let out = h.qex(&["status", &id, "--follow", "--timeout", "2s"]);
    assert_eq!(
        out.status.code(),
        Some(124),
        "a wait that reaches its limit gives 124: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "`--follow` ignored the limit of the reader"
    );
    assert_eq!(
        h.state_of(&id),
        "running",
        "the limit of the reader must not stop the job"
    );
    h.stop(&id);
}

/// The help of `qex submit` must say WHERE the id goes.
///
/// An agent that reads "stdout" writes `ID=$(qex submit --wait ...)` and
/// captures the record of the job in place of the id.
#[test]
fn the_help_of_submit_says_that_the_id_goes_to_stderr() {
    let h = Harness::with_default_config("submithelp");
    let text = String::from_utf8_lossy(&h.qex(&["submit", "--help"]).stdout).to_string();
    let wait_part = text
        .split("--wait")
        .nth(1)
        .expect("`--wait` must be in the help of `qex submit`")
        .to_string();
    assert!(
        wait_part.contains("STDERR") || wait_part.contains("stderr"),
        "the help must say that the id goes to stderr: {wait_part:.600}"
    );
}

/// `qex submit --wait` gives the exit code of the job, and it writes the id file
/// BEFORE the wait begins.
///
/// The id file is the handle after a crash, and not after a tidy stop only. A
/// user lost the result of a job that had succeeded, because the id was in the
/// memory of a command that died.
#[test]
fn submit_with_a_wait_gives_the_code_of_the_job_and_writes_the_id_first() {
    let h = Harness::with_default_config("submitwait");
    let id_file = h.root.join("job.id");
    let path = id_file.to_str().unwrap().to_string();

    let child = h.spawn(&[
        "submit",
        "--wait",
        "--id-file",
        &path,
        "--",
        "sh",
        "-c",
        "sleep 3; exit 7",
    ]);

    // The file must hold the id while the job still operates.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut id = String::new();
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&id_file) {
            if text.trim().parse::<uuid::Uuid>().is_ok() {
                id = text.trim().to_string();
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !id.is_empty(),
        "`--id-file` must reach the disk before the wait begins"
    );
    assert!(
        !matches!(h.state_of(&id).as_str(), "completed" | "failed"),
        "the test must read the id file while the job still operates"
    );

    let out = child.wait_with_output().expect("the command did not stop");
    assert_eq!(
        out.status.code(),
        Some(7),
        "`qex submit --wait` must give the exit code of the job: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // stdout holds the RECORD of the job, so a reader needs no second command.
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        said.contains("exit code: 7") && said.contains("failed"),
        "`--wait` must end with the record of the job: {said}"
    );
    // The id goes to stderr, because stdout carries the result.
    assert!(String::from_utf8_lossy(&out.stderr).contains(&id));
}

/// `qex run` and `qex submit --wait` must never answer one question two ways.
#[test]
fn submit_with_a_wait_and_qex_run_give_one_code() {
    let h = Harness::with_default_config("waitagree");
    for code in [0, 3, 96] {
        let command = format!("exit {code}");
        let submit = h.qex(&["submit", "--wait", "--", "sh", "-c", &command]);
        let (child, _) = h.run_bg(&["--", "sh", "-c", &command]);
        let run = wait_run(child, "the job gives its own exit code");
        assert_eq!(
            submit.status.code(),
            run.status.code(),
            "`qex submit --wait` and `qex run` gave two codes for the code {code}"
        );
        assert_eq!(submit.status.code(), Some(code));
    }
}

/// A command line that qex cannot read must not speak in the voice of the job.
///
/// The code 2 says "the job exited 2" under the table of the band, and no job
/// ran. An agent that gives a wrong option meets this on its first call.
#[test]
fn a_usage_error_of_a_command_that_speaks_for_a_job_uses_the_band() {
    let h = Harness::with_default_config("usage");

    for args in [
        vec!["status"],
        vec!["wait"],
        vec!["submit", "--wait", "--cpu", "not-a-number", "--", "true"],
        vec!["pipeline", "--no-such-option"],
        vec!["rerun", "--no-such-option"],
    ] {
        let out = h.qex(&args);
        assert_eq!(
            out.status.code(),
            Some(121),
            "`qex {}` must give a code from the band: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // A command that never starts a job and never gives a job result keeps the
    // conventional code 2.
    let out = h.qex(&["list", "--no-such-option"]);
    assert_eq!(out.status.code(), Some(2));
}

/// `qex pipeline` and `qex rerun` start a job, so they speak in the same voice
/// as `qex submit`. A refusal is 121. An id that does not exist is 127.
#[test]
fn pipeline_and_rerun_use_the_band_of_a_job() {
    let h = Harness::with_default_config("pipeband");

    let missing = h.root.join("no-such-pipeline.toml");
    let out = h.qex(&["pipeline", missing.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(121),
        "a pipeline file that qex cannot read is a refusal: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = h.qex(&["rerun", "00000000-0000-0000-0000-000000000000"]);
    assert_eq!(
        out.status.code(),
        Some(127),
        "a rerun of a job that does not exist is 127: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `qex status --follow` is the way back to a job of `qex run`.
///
/// It writes the output of the job and no text of its own on stdout, and it
/// gives the exit code of the job.
#[test]
fn status_with_follow_attaches_to_a_job_and_gives_its_output() {
    let h = Harness::with_default_config("follow");
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "echo first; sleep 1; echo second; exit 5",
    ]);

    let out = h.qex(&["status", &id, "--follow"]);
    assert_eq!(
        out.status.code(),
        Some(5),
        "`--follow` must give the exit code of the job"
    );
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(
        said, "first\nsecond\n",
        "stdout must hold the output of the job and no text of qex: {said:?}"
    );
}

/// `qex submit --follow` is `qex run` in a longer form, so the two must give
/// one output and one code.
#[test]
fn submit_with_follow_is_qex_run() {
    let h = Harness::with_default_config("longrun");
    let follow = h.qex(&["submit", "--follow", "--", "sh", "-c", "echo hello; exit 4"]);
    let run = h.qex(&["run", "--", "sh", "-c", "echo hello; exit 4"]);
    assert_eq!(follow.status.code(), run.status.code());
    assert_eq!(follow.status.code(), Some(4));
    assert_eq!(
        String::from_utf8_lossy(&follow.stdout),
        String::from_utf8_lossy(&run.stdout)
    );
    assert_eq!(String::from_utf8_lossy(&follow.stdout).trim(), "hello");
}

/// `--quiet` gives the exit code and nothing else.
#[test]
fn quiet_gives_the_exit_code_and_no_text() {
    let h = Harness::with_default_config("quiet");
    let id = h.submit(&["submit", "--", "sh", "-c", "echo noise; exit 3"]);

    let out = h.qex(&["status", &id, "--wait", "--quiet"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");

    let out = h.qex(&["wait", &id, "--quiet"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");

    // Without a wait, `--quiet` reads the record as it stands now.
    let out = h.qex(&["status", &id, "--quiet"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");

    // A job that did not stop has no result, and the code says so.
    let running = h.submit(&["submit", "--", "sleep", "30"]);
    h.until("the job starts", Duration::from_secs(45), || {
        h.has_started(&running)
    });
    let out = h.qex(&["status", &running, "--quiet"]);
    assert_eq!(
        out.status.code(),
        Some(100),
        "a job that did not stop has no result, and this reader set no wait"
    );
    h.stop(&running);
}

/// `--quiet` on a PIPELINE must describe every stage.
///
/// A code that reads the first stage alone says that a build succeeded while a
/// later stage failed, which is the fault that this branch exists to remove: a
/// code of qex that does not describe the job.
#[test]
fn quiet_on_a_pipeline_reports_the_stage_that_failed() {
    let h = Harness::with_default_config("quietpipe");
    let file = h.root.join("pl.toml");
    std::fs::write(
        &file,
        "[[jobs]]\nname = \"one\"\ncommand = [\"true\"]\n\
         [[jobs]]\nname = \"two\"\nneeds = [\"one\"]\ncommand = [\"sh\", \"-c\", \"exit 6\"]\n",
    )
    .unwrap();
    let group = h.ok(&["pipeline", file.to_str().unwrap()]);
    h.qex(&["wait", &group, "--timeout", "45s"]);

    // Every form of the question gives one answer.
    for args in [
        vec!["status", &group, "--quiet"],
        vec!["status", &group, "--wait", "--quiet"],
        vec!["wait", &group, "--quiet"],
    ] {
        let out = h.qex(&args);
        assert_eq!(
            out.status.code(),
            Some(6),
            "`qex {}` must give the code of the stage that failed",
            args.join(" ")
        );
    }
}

/// A wait for MANY jobs must say why EACH of them waits.
///
/// One reporter for many jobs stops at the first job that starts, and every
/// later job of that wait is then silent. The first job here RUNS, so the
/// reporter meets a job that is not queued; the second job is blocked behind a
/// third. With one reporter for each job, the second job speaks.
#[test]
fn a_wait_for_many_jobs_says_why_the_later_job_waits() {
    let h = Harness::new(
        "manyreason",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    // The first job of the wait. It RUNS, so a shared reporter stops here.
    let first = h.submit(&["submit", "--cpu", "2", "--mem", "100MB", "--", "sleep", "6"]);
    h.until("the first job starts", Duration::from_secs(45), || {
        h.has_started(&first)
    });
    // The job that holds the second job back. It outlives the first job.
    let holder = h.submit(&[
        "submit", "--name", "holder", "--cpu", "2", "--mem", "100MB", "--", "sleep", "25",
    ]);
    // The second job of the wait. It cannot start until the holder stops.
    let second = h.submit(&[
        "submit", "--cpu", "1", "--mem", "100MB", "--needs", &holder, "--", "true",
    ]);

    let child = h.spawn(&["wait", &first, &second, "--timeout", "30s"]);
    // The first job stops at 6 seconds, and the wait then moves to the second.
    std::thread::sleep(Duration::from_secs(12));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().expect("the wait did not stop");
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        said.contains(&holder[..8]) || said.contains("holder"),
        "the wait must say that the second job waits for the holder: {said}"
    );

    h.stop(&holder);
    h.qex(&["cancel", &second]);
}

/// `--quiet` silences the REPORT, and never a fault of the wait.
///
/// A quiet wait that stops with no message leaves an agent with a number and no
/// id. The lines that name a fault carry the handle, so they stay.
#[test]
fn quiet_keeps_the_faults_of_the_wait() {
    let h = Harness::new(
        "quietfault",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    // A job that waits in the queue. The REASON is narration, and `--quiet`
    // silences it; the time limit is a fault, and `--quiet` keeps it.
    let holder = h.submit(&[
        "submit", "--cpu", "2", "--mem", "100MB", "--", "sleep", "20",
    ]);
    h.until("the first job starts", Duration::from_secs(45), || {
        h.has_started(&holder)
    });
    let waiter = h.submit(&["submit", "--cpu", "2", "--mem", "100MB", "--", "true"]);
    h.until("the second job waits", Duration::from_secs(30), || {
        !h.status_json(&waiter)["blocked_reason"].is_null()
    });
    let reason = h.status_json(&waiter)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();

    for args in [
        vec!["wait", &waiter, "--quiet", "--timeout", "3s"],
        vec!["status", &waiter, "--wait", "--quiet", "--timeout", "3s"],
    ] {
        let out = h.qex(&args);
        assert_eq!(out.status.code(), Some(124), "`qex {}`", args.join(" "));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "",
            "`--quiet` must write nothing on stdout"
        );
        let said = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(
            !said.contains(&reason),
            "`--quiet` must not narrate the queue: {said}"
        );
        assert!(
            said.contains("time limit"),
            "`--quiet` must keep the fault of the wait: {said}"
        );
    }

    // A job that does not exist is a fault as well.
    let out = h.qex(&["wait", "0f0f0f0f", "--quiet"]);
    assert_eq!(out.status.code(), Some(127));
    assert!(
        !String::from_utf8_lossy(&out.stderr).is_empty(),
        "`--quiet` must say that there is no such job"
    );

    h.stop(&holder);
    h.qex(&["cancel", &waiter]);
}

/// `--follow` must not give the code of a wait for a job that stopped.
///
/// The loop leaves a terminal job in the step that still moved output, so a
/// limit that comes in that same step reported 124 for a job that had a result.
#[test]
fn follow_gives_the_code_of_the_job_and_not_the_limit_of_the_reader() {
    let h = Harness::with_default_config("followrace");
    for _ in 0..6 {
        let id = h.submit(&["submit", "--", "sh", "-c", "echo out; exit 5"]);
        // A limit that lands near the end of the job.
        let out = h.qex(&["status", &id, "--follow", "--timeout", "1s"]);
        assert_eq!(
            out.status.code(),
            Some(5),
            "a job that stopped must give its own code: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A limit with no wait says nothing that qex can obey, so qex refuses it.
#[test]
fn a_limit_with_no_wait_is_refused() {
    let h = Harness::with_default_config("nolimit");
    let id = h.submit(&["submit", "--", "true"]);
    h.qex(&["wait", &id, "--timeout", "30s"]);

    let out = h.qex(&["status", &id, "--timeout", "5s"]);
    assert_eq!(out.status.code(), Some(121));
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        said.contains("--wait") && said.contains("--follow"),
        "the message must give the options that wait: {said}"
    );
}

/// `--next` must say why a job waits, in the same way as every other wait.
#[test]
fn wait_with_next_says_why_a_job_waits() {
    let h = Harness::new(
        "nextreason",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );
    let holder = h.submit(&[
        "submit", "--cpu", "2", "--mem", "100MB", "--", "sleep", "20",
    ]);
    h.until("the first job starts", Duration::from_secs(45), || {
        h.has_started(&holder)
    });
    let waiter = h.submit(&["submit", "--cpu", "2", "--mem", "100MB", "--", "true"]);
    h.until("the second job waits", Duration::from_secs(30), || {
        !h.status_json(&waiter)["blocked_reason"].is_null()
    });
    let reason = h.status_json(&waiter)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let out = h.qex(&["wait", "--next", &waiter, "--timeout", "4s"]);
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        said.contains(&reason),
        "`--next` must say why the job waits `{reason}`: {said}"
    );

    h.stop(&holder);
    h.qex(&["cancel", &waiter]);
}

/// qex inside a real sandbox must name the sandbox.
///
/// bubblewrap is the sandbox that Codex uses on Linux, and a state directory
/// that it mounts read-only is the fault that an agent reports. The message
/// must send the reader to the page, because the remedy belongs to the person
/// who starts the agent and not to the agent.
///
/// The test gives no verdict on a machine with no `bwrap`. It is the tool of
/// one sandbox, and qex does not need it.
#[test]
fn qex_inside_bubblewrap_names_the_sandbox() {
    if Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        return;
    }

    let h = Harness::with_default_config("bwrap");
    let state = h.root.join("state");
    std::fs::create_dir_all(state.join("qex/run")).unwrap();

    // A sandbox that gives qex a read-only state directory. qex writes its
    // records, its logs and its socket there, so nothing works.
    let out = Command::new("bwrap")
        .args([
            "--bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--ro-bind",
            state.to_str().unwrap(),
            state.to_str().unwrap(),
            "--setenv",
            "XDG_STATE_HOME",
            state.to_str().unwrap(),
            "--setenv",
            "XDG_CONFIG_HOME",
            h.root.join("cfg").to_str().unwrap(),
            "--",
            env!("CARGO_BIN_EXE_qex"),
            "list",
        ])
        .output()
        .expect("bwrap did not start");

    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        said.contains("docs/sandbox.md"),
        "qex inside a sandbox must give the page for a person: {said}"
    );
    // The CAUSE comes first, and the remedy after it. A reader that meets the
    // remedy first does not know what it corrects.
    //
    // Find the cause by the PATH that the system names, and not by the words
    // of the error: those words come from the language of the machine.
    let cause = said
        .find(state.to_str().unwrap())
        .expect("the message must name the directory");
    let page = said
        .find("docs/sandbox.md")
        .expect("the message must give the page");
    assert!(
        cause < page,
        "the message must give the fault before the remedy: {said}"
    );
}

/// A job that does not exist gives 127 at once, and never after ten seconds.
///
/// With no coordinator, a wait looked for a new one for ten seconds before it
/// read the disk. A job with no record never reached a queue, so the answer is
/// ready immediately.
#[test]
fn a_job_that_does_not_exist_answers_at_once_with_no_coordinator() {
    let h = Harness::with_default_config("nojobfast");
    // Start a coordinator, then stop it. Nothing else runs qex here, so
    // nothing starts another one.
    h.ok(&["info", "--json"]);
    let pid = h.coordinator_pid();
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    h.until("the coordinator is gone", Duration::from_secs(10), || {
        (unsafe { libc::kill(pid, 0) }) != 0
    });

    // `--quiet` is the form that shows the fault: it writes no line about a
    // pause, so it starts no coordinator on the way, and the wait then has
    // none to ask.
    let missing = "3f2b1c0d-0000-4000-8000-000000000000";
    let started = Instant::now();
    let out = h.qex(&["wait", missing, "--quiet", "--timeout", "60s"]);
    assert_eq!(out.status.code(), Some(127));
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "the answer took {:?}, and the record was ready at once",
        started.elapsed()
    );
}

/// A failed id file must not take the id with it.
///
/// The job exists at that moment, and a command that stops there leaves work
/// that runs with nobody who knows its id. A full disk is exactly the condition
/// in which a caller needs the handle most, and it is the condition that gave
/// `--id-file` its rule.
#[test]
fn an_id_file_that_fails_does_not_lose_the_job() {
    let h = Harness::with_default_config("idfilefail");
    let locked = h.root.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

    // A test that runs as root writes anywhere, so it cannot make this fault.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let path = locked.join("job.id");
    let out = h.qex(&[
        "submit",
        "--wait",
        "--id-file",
        path.to_str().unwrap(),
        "--",
        "sh",
        "-c",
        "exit 4",
    ]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).ok();

    // The wait ran to the end and gave the code of the JOB.
    assert_eq!(
        out.status.code(),
        Some(4),
        "a file that failed must not stop the wait: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The id reached the caller, and the fault of the file is loud.
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    let id = said
        .lines()
        .find_map(|l| l.strip_prefix("qex: job "))
        .expect("the id must reach stderr before anything can fail");
    assert!(
        id.trim().parse::<uuid::Uuid>().is_ok(),
        "the line must hold a job id: {id}"
    );
    assert!(
        said.contains("did not reach the disk"),
        "the fault of the file must be loud: {said}"
    );
    // The job is real, and the id names it.
    assert_eq!(h.status_json(id.trim())["exit_code"], 4);
}

/// `qex version --check` asks the service that the config file names.
///
/// The test gives a `file://` address, so it measures qex and not a network.
#[test]
fn version_check_asks_the_service_and_says_what_it_found() {
    let h = Harness::with_default_config("updatecheck");
    let answer = h.root.join("latest.json");
    std::fs::write(&answer, r#"{"tag_name": "v9.9.9"}"#).unwrap();
    h.write_config(&format!(
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [update]\nurl = \"file://{}\"\n",
        answer.display()
    ));

    let out = h.qex(&["version", "--check"]);
    assert_eq!(out.status.code(), Some(0), "the answer arrived");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        said.contains("9.9.9"),
        "the newest release must be named: {said}"
    );
    assert!(
        said.contains(&answer.display().to_string()),
        "the service that answered must be named: {said}"
    );
    // `--check` ADDS to the answer of `qex version`, so the version and the
    // coordinator stay where a reader of that command expects them.
    let value: serde_json::Value =
        serde_json::from_slice(&h.qex(&["version", "--check", "--json"]).stdout).unwrap();
    assert!(value["version"].is_string(), "got: {value}");
    assert!(value["coordinator"].is_object(), "got: {value}");
    let update = &value["update"];
    assert_eq!(update["newest"], "9.9.9");
    assert!(update["error"].is_null());

    // TEST BOTH SHAPES OF A BUILD.
    //
    // The tests usually run on a development build, and the RELEASE workflow
    // runs them on a release: it sets the number in `Cargo.toml`, so the same
    // command then takes the other path. A test that requires the words of a
    // development build stopped the release of 0.24.0 after the tag existed.
    // REQUIRE the field. `unwrap_or(false)` would take the branch for a
    // release when the field went away, and on a release build that branch
    // passes: a schema that lost `development` would then reach a user with
    // every test green.
    let development = update["development"]
        .as_bool()
        .unwrap_or_else(|| panic!("`update.development` must be a boolean: {update}"));
    if development {
        assert!(
            said.contains("development build"),
            "a development build must be named as one: {said}"
        );
        assert_eq!(
            update["newer"], false,
            "a development build takes no place in the order"
        );
    } else {
        assert!(
            said.contains("A newer release exists"),
            "a release below 9.9.9 must be told about it: {said}"
        );
        assert_eq!(update["newer"], true, "9.9.9 is above every release of qex");
    }
}

/// A service that does not answer gives 1, and it changes nothing.
#[test]
fn version_check_says_what_stopped_it() {
    let h = Harness::with_default_config("updatefail");
    h.write_config(
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [update]\nurl = \"file:///qex-no-such-file-9e3a\"\n",
    );

    let out = h.qex(&["version", "--check"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "qex could not ask, and that is the only case that gives 1"
    );
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        said.contains("could not ask"),
        "the message must say that qex could not ask: {said}"
    );
    assert!(
        said.contains("still operates") || said.contains("Nothing changed"),
        "the message must say that nothing changed: {said}"
    );

    // A job still runs. A service that does not answer is not a fault of the
    // queue.
    let id = h.submit(&["submit", "--", "true"]);
    assert_eq!(h.qex(&["wait", &id]).status.code(), Some(0));
}

/// `never` is absolute: no file, and no message.
#[test]
fn never_asks_nothing_and_writes_nothing() {
    let h = Harness::with_default_config("updatenever");
    h.write_config(
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [update]\ncheck = \"never\"\nurl = \"file:///qex-no-such-file-9e3a\"\n",
    );

    let id = h.submit(&["submit", "--", "true"]);
    assert_eq!(h.qex(&["wait", &id]).status.code(), Some(0));

    // WAIT LONGER THAN THE FIRST LOOK OF THE COORDINATOR.
    //
    // That thread waits two seconds before it looks at all, so a test that
    // ends at once passes whatever `never` does. A test of an absolute promise
    // must give that promise a chance to break.
    let record = h.root.join("state/qex/update.json");
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        assert!(
            !record.exists(),
            "`never` must write no file: {}",
            record.display()
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // The coordinator was there for the whole of that time.
    assert!(h.qex(&["info", "--no-start"]).status.success());
}

/// The first run of a fresh install asks nothing.
///
/// It writes the time and stays quiet, so qex opens no connection in its first
/// interval. A `file://` address proves it: the file holds a release far above
/// this build, and no message names it.
#[test]
fn a_fresh_install_asks_nothing_and_says_nothing() {
    let h = Harness::with_default_config("updatefirst");
    let answer = h.root.join("latest.json");
    std::fs::write(&answer, r#"{"tag_name": "v9.9.9"}"#).unwrap();
    h.write_config(&format!(
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [update]\ncheck = \"300s\"\nurl = \"file://{}\"\n",
        answer.display()
    ));

    let id = h.submit(&["submit", "--", "true"]);
    let out = h.qex(&["wait", &id]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("newer qex"),
        "a fresh install must say nothing about a release"
    );

    // The record holds the time, and no version: qex wrote the time and asked
    // nothing.
    h.until(
        "the coordinator wrote the record",
        Duration::from_secs(45),
        || h.root.join("state/qex/update.json").exists(),
    );
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(h.root.join("state/qex/update.json")).unwrap())
            .unwrap();
    assert!(value["last_checked"].as_u64().unwrap_or(0) > 0);
    assert!(
        value["newest"].is_null(),
        "the first run must ask nothing: {value}"
    );
}

/// `qex status --wait` ends with the record, so one command gives everything.
#[test]
fn status_with_wait_ends_with_the_record_of_the_job() {
    let h = Harness::with_default_config("statusrec");
    let id = h.submit(&["submit", "--", "sh", "-c", "echo bad >&2; exit 9"]);
    let out = h.qex(&["status", &id, "--wait"]);
    assert_eq!(out.status.code(), Some(9));
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        said.contains("exit code: 9") && said.contains("bad"),
        "the record must hold the code and the error output: {said}"
    );
}

/// A wait must say why the job does not start, and `qex run` already did.
///
/// Issue #28: the documentation tells an agent to use a wait, so the command
/// that qex recommends was the command that went silent. A queue that does
/// nothing and does not say why is the fault that this tool exists to remove.
#[test]
fn a_wait_says_why_the_job_does_not_start() {
    let h = Harness::new(
        "waitreason",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    let holder = h.submit(&["submit", "--cpu", "2", "--", "sleep", "20"]);
    h.until("the first job starts", Duration::from_secs(45), || {
        h.has_started(&holder)
    });
    let waiter = h.submit(&["submit", "--cpu", "2", "--", "true"]);
    h.until("the second job waits", Duration::from_secs(30), || {
        !h.status_json(&waiter)["blocked_reason"].is_null()
    });

    // Take the reason from the RECORD, and require the wait to say the same
    // thing. A test that accepts any line of qex passes when the whole feature
    // is deleted: the message of the interrupt below satisfies it alone.
    let reason = h.status_json(&waiter)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(!reason.is_empty(), "the record must hold a reason");

    let child = h.spawn(&["wait", &waiter, "--timeout", "6s"]);
    std::thread::sleep(Duration::from_secs(5));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().expect("the wait did not stop");
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        said.contains(&reason),
        "the wait must give the reason of the record `{reason}`: {said}"
    );

    h.stop(&holder);
    h.qex(&["cancel", &waiter]);
}

/// A coordinator that stops while the wait OPENS must not give an answer about
/// the job.
///
/// The wait asks the coordinator for the job list, and then sends the request.
/// A coordinator that stops in that window gave two wrong answers: the code 127,
/// which says "there is no job with that id" about a job that operates, and the
/// broken pipe of issue #45. Both describe the coordinator in the voice of the
/// job.
///
/// This test looks for a window in time, so it cannot prove that the window is
/// shut. It caught both faults while they were open, and it can never fail for
/// a reason that is not a fault.
#[test]
fn a_coordinator_that_stops_while_the_wait_opens_gives_no_answer_about_the_job() {
    let h = Harness::with_default_config("openrace");

    for step in 0..8 {
        let id = h.submit(&["submit", "--", "sh", "-c", "sleep 1; exit 0"]);
        // The job must be RUNNING before the coordinator stops. A job that is
        // still in the queue has no supervisor, and nothing starts it when the
        // coordinator goes: the wait then reaches its limit, which is correct
        // and is not the window that this test measures.
        h.until("the job starts", Duration::from_secs(45), || {
            h.state_of(&id) == "running"
        });

        let child = h.spawn(&["wait", &id, "--timeout", "30s"]);
        // Move the moment of the kill through the window of the handshake.
        std::thread::sleep(Duration::from_millis(2 + step * 4));
        let pid = h.coordinator_pid();
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }

        let out = child.wait_with_output().expect("the wait did not stop");
        let code = out.status.code();
        assert!(
            code == Some(0),
            "a coordinator that stopped gave the code {code:?} about a job that succeeded: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(h.state_of(&id), "completed");
    }
}

/// A coordinator that stops in the middle of a wait must not end the wait.
///
/// Issue #45: the wait died with the code 1, which is the code of a job that
/// failed, and a pipeline of two hours that had entirely succeeded reported a
/// failure. A coordinator stops for a new program only when no job is active,
/// so a wait meets this loss when something stops the coordinator abnormally:
/// a signal, an out-of-memory kill, or a failure of the machine.
#[test]
fn a_wait_survives_a_coordinator_that_stops() {
    let h = Harness::with_default_config("waitcrash");
    let id = h.submit(&["submit", "--", "sh", "-c", "sleep 6; exit 0"]);
    h.until("the job starts", Duration::from_secs(45), || {
        h.state_of(&id) == "running"
    });

    let child = h.spawn(&["wait", &id, "--timeout", "60s"]);
    // Let the wait reach the coordinator before the coordinator stops.
    std::thread::sleep(Duration::from_millis(800));

    let pid = h.coordinator_pid();
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }

    h.until("the coordinator is gone", Duration::from_secs(10), || {
        (unsafe { libc::kill(pid, 0) }) != 0
    });

    let out = child.wait_with_output().expect("the wait did not stop");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a coordinator that stops must not report a failure of the job: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(h.state_of(&id), "completed");
}

/// `--next` gives control back one time, so it must name the jobs that stay.
#[test]
fn wait_with_next_names_the_jobs_that_have_no_watcher() {
    let h = Harness::with_default_config("anyrest");
    let fast = h.submit(&["submit", "--", "true"]);
    let slow = h.submit(&["submit", "--", "sleep", "20"]);

    let out = h.qex(&["wait", "--next", &fast, &slow]);
    assert_eq!(out.status.code(), Some(0));

    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        said.contains(&slow) && said.contains("qex wait --next"),
        "`--next` must name the job that stays, and the command that waits for it: {said}"
    );

    h.stop(&slow);
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

    let mut children = Vec::new();
    for _ in 0..20 {
        children.push(
            h.command(&["submit", "--", "true"])
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

/// A second submission with one key must give the first job and start nothing.
///
/// This is the fault that `--dedupe-key` removes. An agent that loses its
/// context runs its script again. Without the key, qex starts a second copy of
/// a four-hour run beside the first copy, and both copies hold the machine.
#[test]
fn a_second_submission_with_one_key_gives_the_first_job_and_starts_no_job() {
    let h = Harness::with_default_config("dedupe");
    let first = h.submit(&["submit", "--dedupe-key", "build:/x", "--", "sleep", "30"]);

    let out = h.qex(&["submit", "--dedupe-key", "build:/x", "--", "sleep", "30"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the second submission must exit with the code 0, so that \
         `ID=$(qex submit ...)` operates"
    );
    let second = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(
        second, first,
        "the second submission must give the first id"
    );

    // The reason goes to stderr, so the id stays alone on stdout.
    let message = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        message.contains("started no job"),
        "the message must say what happened: {message}"
    );
    // The SAFE form of the key. A key is text that another agent chose, and it
    // goes to a terminal here, so it follows the same rule as a job name. See
    // `job::safe_name`.
    assert!(
        message.contains("build_x"),
        "the message must name the key: {message}"
    );

    // The count of jobs must not go up.
    assert_eq!(h.list_json().len(), 1, "qex started a second job");
    assert_eq!(
        std::fs::read_dir(h.root.join("state/qex/jobs"))
            .unwrap()
            .count(),
        1,
        "qex wrote a second job record"
    );

    // The key is in the record, so a reader can see which key gave the id. It
    // reaches the reader in its safe form, like every other name that qex
    // shows.
    assert_eq!(h.status_json(&first)["dedupe_key"], "build_x");

    // A key that holds an ESC byte must never reach a terminal as it stands.
    // Without this rule, `qex status` of a job that a different agent
    // submitted moves the cursor of the reader and writes over the text
    // around it.
    let evil = h.submit(&["submit", "--dedupe-key", "x\u{1b}[2Jy", "--", "true"]);
    let shown = h.status_json(&evil)["dedupe_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        !shown.contains('\u{1b}'),
        "the ESC byte reached the reader: {shown:?}"
    );

    h.stop(&first);
}

/// Several submissions with one key in the same moment must make ONE job.
///
/// Two agents run the same script together. The test of the key and the
/// reservation of the key are one step in the coordinator, so no submission can
/// arrive between them. Without that rule, both agents start a job.
#[test]
fn many_submissions_with_one_key_at_once_make_one_job() {
    let h = Harness::with_default_config("dedupe-race");

    let mut children = Vec::new();
    for _ in 0..20 {
        children.push(
            h.command(&["submit", "--dedupe-key", "one", "--", "sleep", "30"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("qex did not start"),
        );
    }

    let mut ids = Vec::new();
    for c in children {
        let out = c.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "one submission failed during the race: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        ids.push(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }

    let first = ids[0].clone();
    assert!(
        ids.iter().all(|id| *id == first),
        "the submissions gave more than one id: {ids:?}"
    );
    assert_eq!(h.list_json().len(), 1, "the race made more than one job");
    assert_eq!(
        std::fs::read_dir(h.root.join("state/qex/jobs"))
            .unwrap()
            .count(),
        1,
        "the race wrote more than one job record"
    );

    h.stop(&first);
}

/// A job that stopped must free its key, and a window must keep it.
///
/// A key that held a job for ever would give an agent the id of a job of
/// yesterday. That answer looks like a success, and the work never runs again.
#[test]
fn a_job_that_stopped_frees_its_key_and_a_window_keeps_it() {
    let h = Harness::with_default_config("dedupe-free");

    let first = h.submit(&["submit", "--dedupe-key", "k", "--", "true"]);
    h.ok(&["wait", &first]);

    let second = h.submit(&["submit", "--dedupe-key", "k", "--", "true"]);
    assert_ne!(
        second, first,
        "a job that stopped must not hold its key, or the work never runs again"
    );
    h.ok(&["wait", &second]);

    // A window keeps the key of a job that SUCCEEDED.
    let third = h.submit(&[
        "submit",
        "--dedupe-key",
        "k",
        "--dedupe-window",
        "1h",
        "--",
        "true",
    ]);
    assert_eq!(
        third, second,
        "the window must keep the key of the job that succeeded"
    );

    // A job that did NOT succeed frees its key inside the window also. The one
    // remedy for a failure is another run, so the key must never stop it.
    let failed = h.submit(&["submit", "--dedupe-key", "bad", "--", "false"]);
    h.qex(&["wait", &failed]);
    let again = h.submit(&[
        "submit",
        "--dedupe-key",
        "bad",
        "--dedupe-window",
        "1h",
        "--",
        "false",
    ]);
    assert_ne!(
        again, failed,
        "a job that failed must not hold its key, or a second run is not possible"
    );
    h.qex(&["wait", &again]);
}

/// A job that stopped one moment ago must free its key at once.
///
/// `handle_submit` reads the record of each job that operates BEFORE it tests
/// the key. Without that read, the coordinator answers from a copy in memory
/// that still says `running`, and it gives the caller the id of a job that has
/// already stopped, while starting no job.
///
/// The test waits on the status file ON THE DISK, and it sends no qex command
/// while it waits. A qex command would make the coordinator read the records,
/// which is the very thing under test. The scheduler also reads them every
/// 500ms, so ONE attempt is not proof: measured over 30 attempts, the fault
/// appeared 22 times with the line removed and 0 times with it. Ten attempts
/// that must all start a new job is therefore a reliable test, and it passed
/// 100 times out of 100 on the code as it stands.
#[test]
fn a_key_is_free_the_moment_that_its_job_stops() {
    let h = Harness::with_default_config("dedupe-refresh");

    for attempt in 0..10 {
        let key = format!("fresh-{attempt}");
        let first = h.submit(&["submit", "--dedupe-key", &key, "--", "true"]);

        // Watch the disk. Send no command, or the coordinator reads the
        // records for that command and the test proves nothing.
        let file = h
            .root
            .join("state/qex/jobs")
            .join(&first)
            .join("status.json");
        let limit = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(text) = std::fs::read_to_string(&file) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if v["state"] == "completed" {
                        break;
                    }
                }
            }
            assert!(Instant::now() < limit, "the job never stopped");
            std::thread::sleep(Duration::from_millis(1));
        }

        let second = h.submit(&["submit", "--dedupe-key", &key, "--", "true"]);
        assert_ne!(
            second, first,
            "attempt {attempt}: the key still held a job that had stopped, so this \
             submission started no work and gave the id of the finished job"
        );
        h.ok(&["wait", &second]);
    }
}

/// Deleting the record must free the key. `qex clean` deletes both together.
///
/// The reason this test exists: the one line that frees the key lives in
/// `lifecycle::clean`, and the whole suite passed with that line deleted. The
/// live fault is worse than a lost key. The key kept naming the deleted job,
/// so every later submission with that key was answered with an id that
/// `qex status` then refused with the code 127, and no submission with that
/// key could ever start work again.
#[test]
fn qex_clean_frees_the_key_with_the_record() {
    let h = Harness::with_default_config("dedupe-clean");

    let first = h.submit(&["submit", "--dedupe-key", "c", "--", "true"]);
    h.ok(&["wait", &first]);
    h.ok(&["clean", &first]);

    let second = h.submit(&["submit", "--dedupe-key", "c", "--", "true"]);
    assert_ne!(
        second, first,
        "the key still names the deleted job, so no later submission can start the work"
    );
    // The id that a submission gives must answer. This is the fault that the
    // caller meets: `qex status` refused the id with the code 127, and the
    // caller could learn nothing about the work that it asked for.
    let out = h.qex(&["status", &second, "--json"]);
    assert!(
        out.status.success(),
        "`qex status` cannot answer the id that the submission gave: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    h.ok(&["wait", &second]);
}

/// The same rule for `qex gc`, which deletes a record by age.
#[test]
fn qex_gc_frees_the_key_with_the_record() {
    let h = Harness::with_default_config("dedupe-gc");

    let first = h.submit(&["submit", "--dedupe-key", "g", "--", "true"]);
    h.ok(&["wait", &first]);
    h.ok(&["gc", "--older-than", "0s"]);

    let second = h.submit(&["submit", "--dedupe-key", "g", "--", "true"]);
    assert_ne!(second, first, "gc deleted the record and left the key");
    h.ok(&["wait", &second]);
}

/// `qex rerun` must start a job, and the key of the first job must not stop it.
///
/// The reason this test exists: the one line that clears the key in `rerun`
/// was deleted and the whole suite passed. The live fault is a command that
/// reports work that it did not start — `qex rerun` printed "the job 8902039f
/// runs again as 8902039f", the same id twice, and nothing ran.
#[test]
fn qex_rerun_of_a_keyed_job_starts_a_new_job() {
    let h = Harness::with_default_config("dedupe-rerun");

    let first = h.submit(&["submit", "--dedupe-key", "rr", "--", "sleep", "30"]);
    h.until("the job starts", Duration::from_secs(45), || {
        h.has_started(&first)
    });

    // The first job still operates, so it still holds the key. This is the
    // case that fails: a rerun that kept the key would be answered with the
    // id of the job that already operates.
    let out = h.ok(&["rerun", &first]);
    let second = out.split_whitespace().last().unwrap().to_string();
    assert_ne!(
        second, first,
        "rerun gave the id of the first job, so it started nothing: {out}"
    );
    assert_eq!(h.list_json().len(), 2, "rerun started no second job");

    // The new job must not hold the key either, or the next rerun repeats the
    // fault.
    assert!(
        h.status_json(&second)["dedupe_key"].is_null(),
        "the job of a rerun must hold no key"
    );

    h.stop(&first);
    h.stop(&second);
}

/// After a restart, the job that has NOT stopped must hold the key.
///
/// The reason this test exists: `recover` sorts the holders of a key so that
/// the best one is written last, and the whole suite passed with that order
/// reversed. One key with TWO jobs is the only shape that can see it, and the
/// earlier restart test used one job for each key. With the order reversed, a
/// record of yesterday took the key from the job that operates now, and the
/// next submission started a SECOND COPY of a job that was already running.
#[test]
fn a_restart_gives_the_key_to_the_job_that_still_operates() {
    let h = Harness::with_default_config("dedupe-recover-order");

    // A job of yesterday: same key, already stopped.
    let old = h.submit(&["submit", "--dedupe-key", "two", "--", "true"]);
    h.ok(&["wait", &old]);

    // The key is free now, so the next submission takes it and keeps running.
    let live = h.submit(&["submit", "--dedupe-key", "two", "--", "sleep", "30"]);
    h.until("the job starts", Duration::from_secs(45), || {
        h.has_started(&live)
    });

    // Two records now name the key, and only one of them may hold it.
    let pid = h.coordinator_pid();
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    h.until("the coordinator stops", Duration::from_secs(30), || {
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        !alive
    });

    let again = h.submit(&["submit", "--dedupe-key", "two", "--", "sleep", "30"]);
    assert_eq!(
        again, live,
        "the key went to the job that stopped, so qex started a second copy of the work"
    );
    assert_eq!(
        h.list_json().len(),
        2,
        "qex started a third job, so the key did not hold the job that operates"
    );

    h.stop(&live);
}

/// The window is the time that the key STAYS. At the end of it, the key goes.
///
/// The test is at the edge of the window, because that is the one place where
/// a rule of this shape goes wrong. A window of 1 second that kept the key at
/// exactly 1 second would give the caller the job of the earlier run, and the
/// caller would call that a success.
///
/// The edge is a whole second wide, because the coordinator counts in whole
/// seconds, so this test waits for that second and does not race.
#[test]
fn a_key_goes_at_the_end_of_the_window_and_not_after_it() {
    let h = Harness::with_default_config("dedupe-edge");

    let first = h.submit(&[
        "submit",
        "--dedupe-key",
        "e",
        "--dedupe-window",
        "1",
        "--",
        "true",
    ]);
    h.ok(&["wait", &first]);
    let finished = h.status_json(&first)["finished_at"].as_u64().unwrap();

    // Wait until one whole second has passed since the job stopped.
    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if now.saturating_sub(finished) >= 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let second = h.submit(&[
        "submit",
        "--dedupe-key",
        "e",
        "--dedupe-window",
        "1",
        "--",
        "true",
    ]);
    assert_ne!(
        second, first,
        "the key must go at the end of the window, and not one second after it"
    );
    h.ok(&["wait", &second]);
}

/// `qex submit --json` must say if THIS command started the work.
///
/// The plain command writes the id alone, so a caller that needs this answer
/// must have a way to ask for it.
#[test]
fn submit_json_says_if_this_command_started_the_work() {
    let h = Harness::with_default_config("dedupe-json");

    let text = h.ok(&["submit", "--json", "--dedupe-key", "j", "--", "sleep", "30"]);
    let first: serde_json::Value = serde_json::from_str(&text).expect("the output is not JSON");
    assert_eq!(first["deduplicated"], false);
    let id = first["id"].as_str().unwrap().to_string();

    let text = h.ok(&["submit", "--json", "--dedupe-key", "j", "--", "sleep", "30"]);
    let second: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(second["deduplicated"], true);
    assert_eq!(second["id"], first["id"]);

    h.stop(&id);
}

/// A signal to `qex run` must not stop a job that a different caller started.
///
/// A dedupe key gives `qex run` the job of somebody else. Before this rule, a
/// SIGTERM to that command stopped the job of the first agent: the job went
/// from `running` to `killed`, and the first agent lost a run of four hours
/// with no cause that it could see.
#[test]
fn a_signal_to_a_deduplicated_run_stops_the_wait_and_not_the_job() {
    let h = Harness::with_default_config("dedupe-run");

    // The first agent starts the work.
    let owner = h.submit(&["submit", "--dedupe-key", "shared", "--", "sleep", "30"]);
    h.until("the job starts", Duration::from_secs(45), || {
        h.has_started(&owner)
    });

    // The second agent runs the same script. The key gives it the first job.
    let err_path = h.root.join("run.err");
    let mut child = h
        .command(&["run", "--dedupe-key", "shared", "--", "sleep", "30"])
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .spawn()
        .expect("qex did not start");

    // Wait for the message that says the rule is active. qex writes it after it
    // installs the handler, so this text is proof that a signal now meets the
    // rule and not the default behaviour of the system.
    h.until(
        "qex run attaches to the job",
        Duration::from_secs(45),
        || {
            std::fs::read_to_string(&err_path)
                .map(|t| t.contains("did not start it"))
                .unwrap_or(false)
        },
    );

    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let code = child.wait().unwrap().code();
    assert_eq!(
        code,
        Some(122),
        "the wait must give the code that says `the job continues`"
    );

    // The job of the first agent must continue.
    let state = h.state_of(&owner);
    assert_eq!(
        state, "running",
        "a signal to the second agent stopped the job of the first agent"
    );

    let message = std::fs::read_to_string(&err_path).unwrap();
    assert!(
        message.contains(&format!("qex kill {owner}")),
        "the message must give the way to stop the job: {message}"
    );

    h.stop(&owner);
}

/// A signal to `qex run` that OWNS its job must still stop that job.
///
/// This is the behaviour that a user expects, because `qex run` goes in front
/// of a command that Ctrl-C stops. The rule for a deduplicated job must not
/// change it.
#[test]
fn a_signal_to_a_run_that_started_its_job_stops_the_job() {
    let h = Harness::with_default_config("run-signal");

    let out_path = h.root.join("run.out");
    let mut child = h
        .command(&["run", "--", "sh", "-c", "echo ready; sleep 30"])
        .stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("qex did not start");

    // The output of the job arrives from the loop that follows the log file,
    // and that loop starts after the handler exists. This text is thus proof
    // that a signal now meets the handler.
    h.until("the job writes its output", Duration::from_secs(45), || {
        std::fs::read_to_string(&out_path)
            .map(|t| t.contains("ready"))
            .unwrap_or(false)
    });

    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    child.wait().unwrap();

    let id = h.list_json()[0]["id"].as_str().unwrap().to_string();
    h.until("the job stops", Duration::from_secs(45), || {
        h.state_of(&id) == "killed"
    });
}

/// A coordinator that starts again must give each key back to its job.
///
/// The key is in the record of the job. Without that, a restart would free
/// every key, and the next submission would start a second copy of work that
/// still operates, with no message.
#[test]
fn a_key_stays_with_its_job_after_the_coordinator_stops() {
    let h = Harness::with_default_config("dedupe-restart");
    let first = h.submit(&["submit", "--dedupe-key", "r", "--", "sleep", "30"]);
    h.until("the job starts", Duration::from_secs(45), || {
        h.has_started(&first)
    });

    // Take the pid from the coordinator itself. A search of the process list
    // finds the command that holds those letters also.
    let pid = h.coordinator_pid();
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    h.until("the coordinator stops", Duration::from_secs(30), || {
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        !alive
    });

    // The next command starts a new coordinator, and it reads the records.
    let second = h.submit(&["submit", "--dedupe-key", "r", "--", "sleep", "30"]);
    assert_eq!(
        second, first,
        "a new coordinator lost the key, and it started a second copy of the work"
    );
    assert_eq!(h.list_json().len(), 1);

    h.stop(&first);
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
    assert_eq!(out.status.code(), Some(7), "the job exits with the code 7");

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

/// A job that never starts must give up and say what it waited for.
///
/// Without this rule, an agent that submits a job which the budget can never
/// admit waits for ever, and it learns nothing. The state `expired` and the
/// exit code 123 also separate "the job ran too long" from "the job never ran":
/// the two need different corrections, and an expired job has no output at all.
#[test]
fn a_job_that_never_starts_gives_up_and_says_why() {
    let h = Harness::new(
        "queuelimit",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [queue]\noversized = \"queue\"\n",
    );

    // This claim is larger than the budget, and the config file keeps such a job
    // in the queue. The job thus can never start, whatever the machine does.
    let id = h.submit(&[
        "submit",
        "--cpu",
        "64",
        "--max-queue-time",
        "3s",
        "--",
        "echo",
        "never",
    ]);

    h.until("the job gives up", Duration::from_secs(45), || {
        h.state_of(&id) == "expired"
    });

    let status = h.status_json(&id);
    assert!(
        status["started_at"].is_null(),
        "a job that expired must never have a start time: {status}"
    );
    assert!(
        status["exit_code"].is_null(),
        "a job that expired has no exit code: {status}"
    );

    // The text must say what the job waited for and how long it waited. A
    // reader that gets the state alone cannot correct anything.
    let error = status["error"].as_str().unwrap_or("");
    assert!(
        error.contains("did not start"),
        "the text must say that the job never ran: {error}"
    );
    assert!(
        error.contains("--max-queue-time"),
        "the text must name the limit: {error}"
    );
    assert!(
        error.contains("budget") || error.contains("cores"),
        "the text must name the wait: {error}"
    );

    // `qex wait` must give the code of a job that never started, and not the
    // code 125 of a job that something stopped.
    let out = h.qex(&["wait", &id, "--timeout", "30s"]);
    assert_eq!(
        out.status.code(),
        Some(123),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // A job that needed this one must NOT be told to read a log file.
    //
    // An expired job never started, so it wrote no output. A reader who obeys
    // that instruction finds an empty file, `qex logs` gives the code 0, and
    // the reader learns nothing. The same rule already holds for a job that a
    // user cancelled.
    let after = h.submit(&["submit", "--needs", &id, "--", "echo", "after"]);
    h.until(
        "the job behind it gives up",
        Duration::from_secs(45),
        || h.state_of(&after) == "skipped",
    );
    let status = h.status_json(&after);
    let text = format!(
        "{} {}",
        status["error"].as_str().unwrap_or(""),
        status["blocked_reason"].as_str().unwrap_or("")
    );
    assert!(
        text.contains("expired"),
        "the text must name the state of the job that it needed: {text}"
    );
    assert!(
        !text.contains("qex logs"),
        "an expired job wrote no log, so the text must not name one: {text}"
    );
}

/// qex must record the environment and the directory of the shell that
/// submitted the job.
#[test]
fn a_job_receives_the_environment_and_the_directory_of_the_shell() {
    let h = Harness::with_default_config("env");
    let dir = h.root.join("workdir");
    std::fs::create_dir_all(&dir).unwrap();

    let out = h
        .command(&["submit", "--", "sh", "-c", "pwd; echo MARK=$QEX_TEST_MARK"])
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

    let out = h
        .command(&[
            "submit",
            "--no-env-capture",
            "--env",
            "KEPT=yes",
            "--",
            "sh",
            "-c",
            "echo MARK=$QEX_TEST_MARK KEPT=$KEPT",
        ])
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

/// `qex logs --follow` can make the log file of a job. It must make it with
/// the same mode as the supervisor: 0600.
///
/// # The fault that this test prevents
///
/// The follower opens with `.create(true)` so it can start before the
/// supervisor makes the file. Without `.mode(0o600)`, the file takes 0666
/// and the umask. The supervisor then keeps that mode, because `.mode()`
/// applies to a new file only. The output of a job holds a token as
/// frequently as its environment.
///
/// A follower that starts AFTER the supervisor made the file does not
/// exercise this path. This test starts the follower while the job waits
/// in a paused queue, so the file is not there yet.
#[test]
fn a_follower_that_makes_the_log_file_makes_it_private() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;

    let h = Harness::with_default_config("followmode");
    h.ok(&["pause", "queue"]);
    let id = h.submit(&["submit", "--", "echo", "secret"]);
    let stdout_log = h.job_dir(&id).join("stdout.log");
    let stderr_log = h.job_dir(&id).join("stderr.log");
    assert!(
        !stdout_log.exists() && !stderr_log.exists(),
        "the supervisor must not have made the files yet"
    );

    // A umask of 0077 would hide the fault: 0666 & !0077 is 0600. The issue
    // was measured at 0002, which leaves 0664.
    let mut child = {
        let mut cmd = h.command(&["logs", &id, "--follow"]);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        unsafe {
            cmd.pre_exec(|| {
                libc::umask(0o002);
                Ok(())
            });
        }
        cmd.spawn().expect("qex logs --follow did not start")
    };

    h.until(
        "the follower makes the log files",
        Duration::from_secs(15),
        || stdout_log.exists() && stderr_log.exists(),
    );

    for path in [&stdout_log, &stderr_log] {
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "{} must be private, and not take the umask",
            path.file_name().unwrap().to_string_lossy()
        );
    }

    h.ok(&["resume", "queue"]);
    h.ok(&["wait", &id, "--timeout", "30s"]);

    for path in [&stdout_log, &stderr_log] {
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "{} must stay private after the job writes",
            path.file_name().unwrap().to_string_lossy()
        );
    }

    // The job stopped, so the follower must stop too.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait().unwrap() {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                child.kill().ok();
                panic!("the follower did not stop after the job");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
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
    for name in ["job", "status", "pipeline", "event"] {
        let text = h.ok(&["schema", name]);
        serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|e| panic!("the schema `{name}` is not valid JSON: {e}"));
    }
}

/// THE SCHEMA MUST ACCEPT EVERY RECORD THAT QEX WRITES.
///
/// An agent reads the shipped schema to know what a field can hold, and some
/// agents refuse a record that the schema does not permit. A value that qex
/// writes and the schema refuses thus makes qex look broken to the reader that
/// this schema exists for.
///
/// `claim_source` is the field that carries this risk: qex writes `fan-out` for
/// a job of a fan-out, because such a job learns against its template and not
/// against the command of its own line.
///
/// The test reads the value from a REAL job and the list from the SHIPPED
/// schema. A test that named the four values itself would agree with itself.
#[test]
fn the_schema_accepts_every_claim_source_that_qex_writes() {
    let h = Harness::with_default_config("claimsource");

    let schema: serde_json::Value = serde_json::from_str(&h.ok(&["schema", "status"])).unwrap();
    let permitted: Vec<String> = schema["properties"]["claim_source"]["enum"]
        .as_array()
        .expect("the schema must give the values of `claim_source`")
        .iter()
        .map(|v| v.as_str().expect("each value is a string").to_string())
        .collect();

    // Run the same fan-out two times. The first run measures the template, and
    // the second run gets its claim from those measurements, which is the case
    // that gives `fan-out`.
    let input = h.root.join("lines.txt");
    std::fs::write(&input, "alpha\nbeta\n").unwrap();
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..2 {
        let group = h.ok(&[
            "submit",
            "--each-line",
            input.to_str().unwrap(),
            "--",
            "echo",
            "{}",
        ]);
        let text = h.ok(&["list", "--group", group.trim(), "--json"]);
        let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        for job in &jobs {
            let id = job["id"].as_str().unwrap().to_string();
            h.ok(&["wait", &id, "--timeout", "60s"]);
            let source = h.status_json(&id)["claim_source"]
                .as_str()
                .expect("every record names where its claim came from")
                .to_string();
            if !seen.contains(&source) {
                seen.push(source);
            }
        }
    }

    assert!(
        seen.contains(&"fan-out".to_string()),
        "the second run of a fan-out must give the claim source `fan-out`, and it gave {seen:?}"
    );
    for source in &seen {
        assert!(
            permitted.contains(source),
            "qex writes the claim source `{source}`, and the shipped schema permits {permitted:?} \
             only. An agent that tests a record against this schema refuses a record that is \
             correct."
        );
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

/// A recovered job whose supervisor is gone must not stay `running` for ever.
///
/// The test above covers a supervisor that stops under a live coordinator.
/// This test stops the coordinator FIRST, so only recovery can see the job.
/// Before this rule, the next coordinator kept such a job and watched nothing:
/// the supervisor was the only process that could write the result, so the
/// record never changed, the job held its claim and its locks for ever, and
/// `qex wait` on it never returned.
#[test]
fn a_recovered_job_without_a_supervisor_does_not_stay_running() {
    let h = Harness::with_default_config("recoverorphan");
    let id = h.submit(&["submit", "--name", "orphan", "--", "sleep", "300"]);

    h.until("the job operates", Duration::from_secs(45), || {
        h.state_of(&id) == "running" && h.status_json(&id)["supervisor_pid"].as_i64().is_some()
    });

    let job_pid = h.status_json(&id)["pid"].as_i64().unwrap() as i32;
    let supervisor = h.status_json(&id)["supervisor_pid"].as_i64().unwrap() as i32;
    let coordinator = h.coordinator_pid();

    // The coordinator goes first. A supervisor that stops under a live
    // coordinator is the case that `supervisor::reap` already covers, and this
    // test must not measure that path.
    unsafe {
        libc::kill(coordinator, libc::SIGKILL);
        libc::kill(supervisor, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while unsafe { libc::kill(coordinator, 0) } == 0 || unsafe { libc::kill(supervisor, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "the coordinator and the supervisor did not stop"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The job process continues at this moment. Without this step, the test
    // would pass for a job that stopped with its supervisor.
    assert_eq!(
        unsafe { libc::kill(job_pid, 0) },
        0,
        "the job process must still operate when recovery finds it"
    );

    // The next command starts the next coordinator, and that coordinator
    // recovers the job. It must make the job terminal, not keep it.
    h.until("the job is failed", Duration::from_secs(45), || {
        h.state_of(&id) == "failed"
    });

    // The job process must stop. A record that says the job stopped, with the
    // job still alive, leaves memory in use that no command can free.
    h.until("the job process stops", Duration::from_secs(30), || {
        let rc = unsafe { libc::kill(job_pid, 0) };
        rc != 0
    });

    // The record must name the cause.
    let error = h.status_json(&id)["error"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        error.contains("the supervisor of this job did not continue"),
        "the record must say that the supervisor is the cause: {error}"
    );

    // The budget must show the capacity as free again, and a wait must return.
    let info = h.ok(&["info", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(v["cpu_claimed"].as_u64(), Some(0));
    let out = h.qex(&["wait", &id]);
    assert_eq!(
        out.status.code(),
        Some(121),
        "a wait for a failed job must return at once"
    );
}

/// A record from an earlier start of the machine is dead, whatever its pids
/// say.
///
/// The machine uses each pid again after a restart, so a recovered record can
/// point at live processes that have no connection with the job. A coordinator
/// that trusts such a pid keeps the job `running` for ever, or watches a
/// stranger for days. This test makes that exact shape: the record names a
/// different start of the machine, while both of its pids point at processes
/// that are alive.
#[test]
fn a_record_from_an_earlier_start_of_the_machine_is_dead() {
    let h = Harness::with_default_config("recoverboot");
    let id = h.submit(&["submit", "--name", "stale", "--", "sleep", "300"]);

    h.until("the job operates", Duration::from_secs(45), || {
        h.state_of(&id) == "running" && h.status_json(&id)["supervisor_pid"].as_i64().is_some()
    });

    let job_pid = h.status_json(&id)["pid"].as_i64().unwrap() as i32;
    let supervisor = h.status_json(&id)["supervisor_pid"].as_i64().unwrap() as i32;
    let coordinator = h.coordinator_pid();

    // Stop the coordinator ONLY. The supervisor and the job stay alive: after
    // a true restart the numbers in the record can belong to live strangers,
    // and this test uses the live processes of the job itself as those
    // strangers.
    unsafe {
        libc::kill(coordinator, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while unsafe { libc::kill(coordinator, 0) } == 0 {
        assert!(Instant::now() < deadline, "the coordinator did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Say in the record that the machine started again after the supervisor
    // wrote it.
    let path = h.job_dir(&id).join("status.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["boot_id"] = serde_json::Value::String("an-earlier-start".to_string());
    std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

    // The next coordinator must call the job dead, although both pids of the
    // record point at processes that are alive.
    h.until("the job is failed", Duration::from_secs(45), || {
        h.state_of(&id) == "failed"
    });
    assert_eq!(
        unsafe { libc::kill(job_pid, 0) },
        0,
        "the job process must still be alive; otherwise this test proves nothing"
    );

    // The record must name the true cause, and it must not blame the
    // supervisor: the supervisor of a machine that restarted did nothing
    // wrong.
    let error = h.status_json(&id)["error"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        error.contains("the machine restarted"),
        "the record must name the restart of the machine: {error}"
    );

    // The budget must show the capacity as free again.
    let info = h.ok(&["info", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(v["cpu_claimed"].as_u64(), Some(0));

    // The pids move to history. The published rule is that a non-null `pid`
    // means the job operates and a reader can act on it; after this path the
    // numbers belong to strangers, so they must not stay where a reader is
    // told to trust them.
    let s = h.status_json(&id);
    assert!(
        s["pid"].is_null(),
        "a terminal record must hold no pid: {s}"
    );
    assert_eq!(s["last_pid"].as_i64(), Some(job_pid as i64));
    assert!(
        s["supervisor_pid"].is_null(),
        "a terminal record must hold no supervisor pid: {s}"
    );

    // The processes were stand-ins for strangers, so recovery must not have
    // signaled them; stop them here.
    unsafe {
        libc::kill(supervisor, libc::SIGKILL);
        libc::killpg(job_pid, libc::SIGKILL);
    }
}

/// A supervisor pid that a NEW process holds must not keep a job `running`.
///
/// The machine uses each pid again within one start of the machine. Recovery
/// must compare the start time that the supervisor recorded with the start
/// time of the process that holds the number now; a process that is merely
/// alive with the right number is not the supervisor. Before this rule, a
/// recycled supervisor pid attached the coordinator to a stranger: the job
/// stayed `running` for ever and `watch_until_gone` followed that stranger.
#[test]
fn a_new_process_with_the_supervisor_pid_is_not_the_supervisor() {
    let h = Harness::with_default_config("recoversupreuse");
    let id = h.submit(&["submit", "--name", "reused", "--", "sleep", "300"]);

    h.until("the job operates", Duration::from_secs(45), || {
        h.state_of(&id) == "running" && h.status_json(&id)["supervisor_pid"].as_i64().is_some()
    });

    let job_pid = h.status_json(&id)["pid"].as_i64().unwrap() as i32;
    let supervisor = h.status_json(&id)["supervisor_pid"].as_i64().unwrap() as i32;
    let coordinator = h.coordinator_pid();

    // The coordinator goes first, then the supervisor; the job stays alive.
    unsafe {
        libc::kill(coordinator, libc::SIGKILL);
        libc::kill(supervisor, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while unsafe { libc::kill(coordinator, 0) } == 0 || unsafe { libc::kill(supervisor, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "the coordinator and the supervisor did not stop"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // A stand-in for the reuse of the pid: a live process of this user that
    // is not the supervisor. The record keeps the start time of the TRUE
    // supervisor, so the identity test must refuse this process.
    #[allow(clippy::zombie_processes)]
    let decoy = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .expect("starting the stand-in process");
    let decoy_pid = decoy.id() as i32;

    let path = h.job_dir(&id).join("status.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        value["supervisor_start_token"].as_u64().is_some(),
        "the record must hold the start time of the supervisor: {value}"
    );
    value["supervisor_pid"] = serde_json::json!(decoy_pid);
    std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

    // The next coordinator must not attach to the stand-in. The supervisor is
    // gone, the job continues, so recovery stops the job and marks it failed.
    h.until("the job is failed", Duration::from_secs(45), || {
        h.state_of(&id) == "failed"
    });
    let error = h.status_json(&id)["error"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        error.contains("the supervisor of this job did not continue"),
        "the record must say that the supervisor is the cause: {error}"
    );
    h.until("the job process stops", Duration::from_secs(30), || {
        let rc = unsafe { libc::kill(job_pid, 0) };
        rc != 0
    });

    // The stand-in was never the supervisor, so no signal may reach it.
    assert_eq!(
        unsafe { libc::kill(decoy_pid, 0) },
        0,
        "the stand-in process must not receive a signal"
    );
    unsafe {
        libc::kill(decoy_pid, libc::SIGKILL);
    }
}

/// A record of an earlier version of qex, found after a restart of the
/// machine, must not take live processes for the job.
///
/// Such a record holds no `boot_id` and no start times — only raw pids.
/// Recovery dates it by the time of its file against the boot time of the
/// machine: a status written before this start of the machine belongs to an
/// earlier start, and its numbers name nobody. This test builds that exact
/// record: the identity fields are removed, the file is dated before the
/// boot, and the pids point at processes that are alive.
#[test]
fn an_old_record_from_before_the_boot_is_dead() {
    let h = Harness::with_default_config("recoveroldboot");
    let id = h.submit(&["submit", "--name", "old", "--", "sleep", "300"]);

    h.until("the job operates", Duration::from_secs(45), || {
        h.state_of(&id) == "running" && h.status_json(&id)["supervisor_pid"].as_i64().is_some()
    });

    let job_pid = h.status_json(&id)["pid"].as_i64().unwrap() as i32;
    let supervisor = h.status_json(&id)["supervisor_pid"].as_i64().unwrap() as i32;
    let coordinator = h.coordinator_pid();

    unsafe {
        libc::kill(coordinator, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while unsafe { libc::kill(coordinator, 0) } == 0 {
        assert!(Instant::now() < deadline, "the coordinator did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Make the record one that an earlier version of qex wrote, in an earlier
    // start of the machine: no identity fields, and a file time before the
    // boot. The supervisor and the job stay alive as the live strangers that
    // the recycled numbers would point at.
    let path = h.job_dir(&id).join("status.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let fields = value.as_object_mut().unwrap();
    fields.remove("boot_id");
    fields.remove("supervisor_start_token");
    fields.remove("pid_start_token");
    std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
    let touched = std::process::Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(&path)
        .status()
        .expect("dating the record before the boot");
    assert!(touched.success(), "touch refused the date");

    // The next coordinator must call the job dead and send no signal.
    h.until("the job is failed", Duration::from_secs(45), || {
        h.state_of(&id) == "failed"
    });
    let error = h.status_json(&id)["error"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        error.contains("the machine restarted"),
        "the record must name the restart of the machine: {error}"
    );
    assert_eq!(
        unsafe { libc::kill(job_pid, 0) },
        0,
        "the job process stands in for a stranger and must not receive a signal"
    );
    assert_eq!(
        unsafe { libc::kill(supervisor, 0) },
        0,
        "the supervisor process stands in for a stranger and must not receive a signal"
    );

    unsafe {
        libc::kill(supervisor, libc::SIGKILL);
        libc::killpg(job_pid, libc::SIGKILL);
    }
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

/// `qex wait` MUST RETURN WHEN THE JOB GIVES UP, AND NOT 30 SECONDS LATER.
///
/// This test starts the wait BEFORE the limit ends. That order is the whole
/// point: a wait that begins after the job expired returns at once, whatever
/// the coordinator signalled, so it proves nothing.
///
/// The fault that this test holds: the scheduler wrote the state `expired` and
/// signalled no waiter. Every waiter then slept on the 30 second fallback in
/// `handle_wait`. The record said 3s and `qex wait` returned at 30.0s, so the
/// promise "this gives an answer inside 30 minutes" was false by 30 seconds for
/// every user of the option.
#[test]
fn a_wait_returns_when_the_job_reaches_its_queue_limit() {
    let h = Harness::new(
        "queuewait",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [queue]\noversized = \"queue\"\n",
    );

    let id = h.submit(&[
        "submit",
        "--cpu",
        "64",
        "--max-queue-time",
        "3s",
        "--",
        "echo",
        "never",
    ]);

    // Wait from HERE, while the job is still in the queue.
    let start = std::time::Instant::now();
    let out = h.qex(&["wait", &id, "--timeout", "60s"]);
    let elapsed = start.elapsed();

    assert_eq!(
        out.status.code(),
        Some(123),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The limit is 3s. Ten seconds is far above every measured run and far
    // below the 30 second fallback, so a failure here names the fault and not
    // the speed of the machine.
    assert!(
        elapsed < Duration::from_secs(10),
        "`qex wait` returned after {elapsed:?}, and the limit is 3s. The \
         scheduler expired the job and signalled no waiter."
    );
}

/// `qex config show` MUST NAME THE QUEUE LIMIT, AND NAME IT ALWAYS.
///
/// This command says what qex uses NOW. A value that it never prints is a value
/// that a reader cannot confirm, and "no limit" is the answer that most users
/// get: it says that a job waits for capacity with no end. A user who asks why
/// a job waited for hours needs to read that line and see that no limit
/// operates.
#[test]
fn the_config_summary_names_the_queue_limit() {
    // With no value, the line must still appear and must say what happens.
    let h = Harness::new(
        "cfgqueuelimit",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );
    let out = h.ok(&["config", "show"]);
    assert!(
        out.contains("queue limit:"),
        "the summary must hold the queue limit line: {out}"
    );
    assert!(
        out.contains("no limit"),
        "with no value the line must say that a job waits with no end: {out}"
    );

    // With a value, the line must give it and say what the limit does.
    let h = Harness::new(
        "cfgqueuelimit2",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [defaults]\nmax_queue_time = \"30m\"\n",
    );
    let out = h.ok(&["config", "show"]);
    assert!(
        out.contains("queue limit:") && out.contains("30m"),
        "the summary must give the value that qex uses: {out}"
    );
    assert!(
        out.contains("expired"),
        "the line must say what happens to a job that waits longer: {out}"
    );
}

/// A WAIT ON A JOB WHOSE DEPENDENCY EXPIRED MUST RETURN AT THE SAME MOMENT.
///
/// This is the configuration that `docs/reference.md` recommends: a limit on
/// the first stage, and no limit on the stage that waits for it. Turn N expires
/// the first job. Turn N+1 sees that the first job stopped without success and
/// makes the second job `skipped`.
///
/// The fault that this test holds: a scheduling pass counted the EXPIRED jobs
/// only. A skipped job also has no supervisor and no request thread to announce
/// it, so the pass signalled nobody and every waiter slept on the 30 second
/// fallback in `handle_wait`. Measured with that count: `qex wait` on the
/// second job returned at 33.0s, 3 of 3 runs, with a limit of 3s. With the
/// count it returned at 3.5s.
///
/// THE TEST MEASURES THE PROMISE, AND NOT THE RECORD. A test that waits for the
/// status file to say `skipped` passes while `qex wait` sleeps, because the
/// file holds the answer that the waiter did not receive.
#[test]
fn a_wait_returns_when_the_job_that_it_needs_gives_up_in_the_queue() {
    let h = Harness::new(
        "queuedep",
        "[budget]\ncpu = \"2\"\nmem = \"8GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [queue]\noversized = \"queue\"\n",
    );

    // This claim is larger than the budget, and the config file keeps such a
    // job in the queue, so the first job can never start.
    let first = h.submit(&[
        "submit",
        "--cpu",
        "64",
        "--max-queue-time",
        "3s",
        "--",
        "echo",
        "never",
    ]);
    let second = h.submit(&["submit", "--needs", &first, "--", "echo", "after"]);

    // Wait from HERE, while both jobs are still in the queue.
    let start = std::time::Instant::now();
    let out = h.qex(&["wait", &second, "--timeout", "60s"]);
    let elapsed = start.elapsed();

    assert_eq!(
        out.status.code(),
        Some(126),
        "a job that did not run because a job that it needs failed gives 126. \
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The limit is 3s. Ten seconds is far above every measured run and far
    // below the 30 second fallback, so a failure here names the fault and not
    // the speed of the machine.
    assert!(
        elapsed < Duration::from_secs(10),
        "`qex wait` returned after {elapsed:?}, and the limit of the job that \
         this job needs is 3s. The scheduler skipped this job and signalled no \
         waiter."
    );
}

/// A JOB THAT STARTED MUST NEVER GET THE STATE `expired`.
///
/// The scheduler chooses a job, releases the lock, and the start of that job
/// writes `starting`. A scheduling pass that removed a job in that moment would
/// write `expired` over a job that ran, and `qex wait` would then report a
/// failure for a job that succeeded.
///
/// These jobs run one at a time on a budget of one core, and their queue limit
/// is short. Some of them thus reach the start and the limit in the same moment.
#[test]
fn a_job_that_started_at_its_queue_limit_keeps_its_result() {
    let h = Harness::new(
        "queuerace",
        "[budget]\ncpu = \"1\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    let mut ids = Vec::new();
    for _ in 0..10 {
        ids.push(h.submit(&[
            "submit",
            "--cpu",
            "1",
            "--mem",
            "64MB",
            "--max-queue-time",
            "2s",
            "--",
            "sleep",
            "0.4",
        ]));
    }

    for id in &ids {
        // Not `ok`: a job that reaches its queue limit gives the code 123, and
        // that is one of the two correct results here.
        h.qex(&["wait", id, "--timeout", "60s"]);
        let s = h.status_json(id);
        let state = s["state"].as_str().unwrap();

        // A record that says `expired` and holds a start time or an exit code is
        // self contradictory: the job ran, so it did not expire.
        if state == "expired" {
            assert!(
                s["started_at"].is_null(),
                "a job that started must never be `expired`: {s}"
            );
            assert!(
                s["exit_code"].is_null(),
                "a job with an exit code must never be `expired`: {s}"
            );
        }
        if s["exit_code"].as_i64() == Some(0) {
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

/// Gives the command of a job that waits until the test releases it.
///
/// A TEST CONTROLS THE BUDGET AND THE END OF ITS OWN JOBS. It controls nothing
/// else about the machine: not the moment a job starts, and not the speed of
/// the runner.
///
/// A job that ends on a timer is therefore a race. A slow machine finishes the
/// job before the test reads the answer, and the failure then names the test
/// and not a fault of qex. This job waits for a FILE, so it stays unfinished
/// until the test makes that file, however slow the machine is.
///
/// Use `release` to end every job that waits for the same file.
///
/// THE JOB MUST ALSO END BY ITSELF. A test that fails, or one that stops
/// before it releases the job, leaves a job that waits for a file which
/// nobody will make. A job that sleeps ends by itself; a job that waits does
/// not, so this command gives it two more ways to stop:
///
/// * the directory of the test goes. The harness deletes it when the test
///   ends, so the job stops with it, and the machine keeps no process that
///   waits for a file in a directory that is not there.
/// * a count of the attempts. It is the last guard, for a state that neither
///   the gate nor the directory covers.
///
/// The gate is what the TEST uses. The other two are what the MACHINE needs.
fn waits_for_the_test(gate: &std::path::Path) -> String {
    let root = gate
        .parent()
        .expect("the gate file is in the directory of the test");
    format!(
        "i=0; while [ ! -f {gate} ]; do \
         [ -d {root} ] || exit 0; \
         i=$((i+1)); [ $i -gt 2400 ] && exit 0; \
         sleep 0.05; done",
        gate = gate.display(),
        root = root.display()
    )
}

/// Ends every job that waits for this file.
fn release(gate: &std::path::Path) {
    std::fs::write(gate, "go").expect("the test makes its own gate file");
}

/// A record that an unfinished pipeline needs must survive a deletion.
///
/// A pipeline of three stages runs its last stage. The reader deletes the jobs
/// that stopped. The records of BOTH earlier stages must stay: the work of
/// those stages happened, and the record is the only proof of it. A rule that
/// walks one step keeps the middle stage and loses the first.
#[test]
fn clean_keeps_every_record_of_a_pipeline_that_operates() {
    let h = Harness::with_default_config("cleanchain");
    let gate = h.root.join("gate");
    let waiting = waits_for_the_test(&gate);

    let a = h.submit(&["submit", "--name", "c-a", "--mem", "64MB", "--", "true"]);
    let b = h.submit(&[
        "submit", "--name", "c-b", "--mem", "64MB", "--needs", &a, "--", "true",
    ]);
    let c = h.submit(&[
        "submit", "--name", "c-c", "--mem", "64MB", "--needs", &b, "--", "sh", "-c", &waiting,
    ]);
    h.until("the last stage operates", Duration::from_secs(30), || {
        h.state_of(&c) == "running"
    });

    h.ok(&["clean", "--state", "done"]);

    let ids: Vec<String> = h
        .list_json()
        .iter()
        .map(|j| j["id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        ids.contains(&b),
        "the stage that the running stage waits for must stay: {ids:?}"
    );
    assert!(
        ids.contains(&a),
        "the FIRST stage must stay: a walk of one step loses it, and its work happened: {ids:?}"
    );

    h.ok(&["kill", &c]);
}

/// The message names the work that HOLDS a record, and no other work.
///
/// A queue holds jobs that have no relation to each other. A message that
/// lists every job which has not stopped says that each one needs the records
/// that stayed, and that relation does not exist. A reader acts on the output
/// of qex as fact, so a message may state only the relation that qex computed.
#[test]
fn the_message_names_the_work_that_holds_a_record() {
    // THREE jobs must be unfinished in one moment, so the budget must admit
    // three. The test sets that; it does not control how fast a runner starts
    // a job. With a smaller budget the queue runs them one after the other,
    // which is what a queue exists to do.
    let h = Harness::new(
        "cleanwho",
        "[budget]\ncpu = \"4\"\nmem = \"2GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );
    let gate = h.root.join("gate");
    let waiting = waits_for_the_test(&gate);

    let dep = h.submit(&["submit", "--name", "m-dep", "--mem", "64MB", "--", "true"]);
    h.ok(&["wait", &dep, "--timeout", "30s"]);
    let holder = h.submit(&[
        "submit", "--name", "m-holder", "--mem", "64MB", "--needs", &dep, "--", "sh", "-c",
        &waiting,
    ]);
    // Two jobs that hold nothing. They wait for no record and no pipeline
    // carries them.
    let lone_one = h.submit(&[
        "submit",
        "--name",
        "m-lone-one",
        "--mem",
        "64MB",
        "--",
        "sh",
        "-c",
        &waiting,
    ]);
    let lone_two = h.submit(&[
        "submit",
        "--name",
        "m-lone-two",
        "--mem",
        "64MB",
        "--",
        "sh",
        "-c",
        &waiting,
    ]);
    for id in [&holder, &lone_one, &lone_two] {
        h.until("each job operates", Duration::from_secs(45), || {
            h.state_of(id) == "running"
        });
    }

    let out = h.ok(&["clean", "--state", "done"]);

    assert!(
        out.contains(&holder[..8]),
        "the message must name the job that holds the record: {out}"
    );
    assert!(
        !out.contains(&lone_one[..8]),
        "the message must not name a job that holds nothing: {out}"
    );
    assert!(
        !out.contains(&lone_two[..8]),
        "the message must not name a job that holds nothing: {out}"
    );

    release(&gate);
}

/// `qex gc` counts a record as held only when the age selected it.
///
/// `gc` deletes a record when it is old enough. A record that is too young is
/// not a record that a job holds back, and a count that mixes the two tells the
/// reader that qex refused a deletion which the reader never asked for.
#[test]
fn gc_counts_only_the_records_that_the_age_selected() {
    let h = Harness::with_default_config("gccount");
    let gate = h.root.join("gate");
    let waiting = waits_for_the_test(&gate);

    let a = h.submit(&["submit", "--name", "g-a", "--mem", "64MB", "--", "true"]);
    h.ok(&["wait", &a, "--timeout", "30s"]);
    let b = h.submit(&[
        "submit", "--name", "g-b", "--mem", "64MB", "--needs", &a, "--", "sh", "-c", &waiting,
    ]);
    h.until("the second job operates", Duration::from_secs(30), || {
        h.state_of(&b) == "running"
    });

    // Nothing is an hour old, so `gc` selects nothing. The record of `a` is
    // held, and the reader did not ask for it.
    let out = h.ok(&["gc", "--older-than", "1h"]);

    assert!(
        !out.contains("stayed"),
        "the count must hold the records that the age selected, and it said: {out}"
    );

    release(&gate);
}

/// The coordinator refuses a deletion that breaks a chain.
///
/// `qex clean <id>` names one record, and the coordinator decides every
/// deletion. That decision must use the same rule as the selection, and it must
/// look through the WHOLE chain: a record two steps behind a job that operates
/// is still a record that the work needs.
///
/// The refusal must carry an exit code and name the job to wait for. A reader
/// who names one record must learn that qex kept it.
#[test]
fn the_coordinator_refuses_a_deletion_that_a_chain_needs() {
    let h = Harness::with_default_config("cleandeep");
    let gate = h.root.join("gate");
    let waiting = waits_for_the_test(&gate);

    let a = h.submit(&["submit", "--name", "d-a", "--mem", "64MB", "--", "true"]);
    let b = h.submit(&[
        "submit", "--name", "d-b", "--mem", "64MB", "--needs", &a, "--", "true",
    ]);
    let c = h.submit(&[
        "submit", "--name", "d-c", "--mem", "64MB", "--needs", &b, "--", "sh", "-c", &waiting,
    ]);
    h.until("the last stage operates", Duration::from_secs(30), || {
        h.state_of(&c) == "running"
    });

    // `a` is TWO steps behind the job that operates.
    let out = h.qex(&["clean", &a]);
    let text = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        !out.status.success(),
        "qex must refuse this deletion and say so with a code: {text}"
    );
    assert!(
        text.contains("is needed by"),
        "the refusal must name the job to wait for: {text}"
    );
    assert!(
        h.list_json().iter().any(|j| j["id"] == a),
        "the record must stay"
    );

    release(&gate);
}

/// The coordinator keeps every stage of a pipeline that operates.
///
/// A pipeline can divide. A stage on one branch is not a step behind the stage
/// that operates, so a rule that follows the dependencies alone lets that
/// record go while the pipeline is still work in progress. The coordinator
/// decides every deletion, so it must hold the pipeline rule as well.
#[test]
fn the_coordinator_keeps_a_stage_of_a_pipeline_that_operates() {
    let h = Harness::with_default_config("cleanbranch");

    let gate = h.root.join("gate");
    let file = h.root.join("p.toml");
    std::fs::write(
        &file,
        format!(
            "[[jobs]]\nname = \"s1\"\ncommand = [\"true\"]\n\
             [jobs.resources]\nmem = \"64MB\"\n\
             [[jobs]]\nname = \"s2b\"\ncommand = [\"true\"]\nneeds = [\"s1\"]\n\
             [jobs.resources]\nmem = \"64MB\"\n\
             [[jobs]]\nname = \"s3\"\ncommand = [\"sh\", \"-c\", \"{}\"]\nneeds = [\"s1\"]\n\
             [jobs.resources]\nmem = \"64MB\"\n",
            waits_for_the_test(&gate)
        ),
    )
    .expect("the test writes its own pipeline file");

    h.ok(&["pipeline", file.to_str().unwrap()]);
    h.until("the last stage operates", Duration::from_secs(45), || {
        h.list_json()
            .iter()
            .any(|j| j["name"] == "s3" && j["state"] == "running")
    });

    let branch = h
        .list_json()
        .iter()
        .find(|j| j["name"] == "s2b")
        .map(|j| j["id"].as_str().unwrap().to_string())
        .expect("the branch stage has a record");

    // Nothing waits for `s2b`, and its pipeline has work left.
    let out = h.qex(&["clean", &branch]);
    let text = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        !out.status.success(),
        "qex must refuse to delete a stage of a pipeline that operates: {text}"
    );
    assert!(
        h.list_json().iter().any(|j| j["id"] == branch),
        "the record of the branch stage must stay"
    );

    release(&gate);
}

/// The count of a deletion says what the command did.
///
/// `qex clean` keeps a record that a job which has not stopped needs, and it
/// says how many records stayed. That count must hold the records that the
/// reader ASKED for. A count that holds a record which no option selected tells
/// the reader that qex refused a deletion that the reader never requested.
#[test]
fn clean_counts_only_the_records_that_the_reader_asked_for() {
    let h = Harness::with_default_config("cleancount");
    let gate = h.root.join("gate");
    let waiting = waits_for_the_test(&gate);

    let a = h.submit(&["submit", "--name", "n-a", "--mem", "64MB", "--", "true"]);
    h.ok(&["wait", &a, "--timeout", "30s"]);
    let b = h.submit(&[
        "submit", "--name", "n-b", "--mem", "64MB", "--needs", &a, "--", "sh", "-c", &waiting,
    ]);
    h.until("the second job operates", Duration::from_secs(30), || {
        h.state_of(&b) == "running"
    });

    // The reader asks about ONE job that operates. The record of `a` stays,
    // and the reader did not ask for it.
    let out = h.qex(&["clean", &b]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !text.contains("stayed"),
        "the count must hold the records that the reader asked for, and it said: {text}"
    );

    release(&gate);
}

/// A word that names WORK and a state must give an error, and delete nothing.
///
/// `qex clean completed` deletes every job that succeeded, and a job or a
/// pipeline can carry the name `completed`. qex must not choose one of the two
/// readings: one reading deletes one record, and the other deletes every record
/// that the user kept.
#[test]
fn clean_refuses_a_word_that_names_work_and_a_state() {
    let h = Harness::with_default_config("cleanboth");

    // A job that carries the name of a state.
    let named = h.submit(&["submit", "--name", "completed", "--", "true"]);
    let other = h.submit(&["submit", "--name", "other", "--", "true"]);
    h.ok(&["wait", &named, "--timeout", "45s"]);
    h.ok(&["wait", &other, "--timeout", "45s"]);

    let out = h.qex(&["clean", "completed"]);
    assert_eq!(
        out.status.code(),
        Some(127),
        "the word has two readings, so the command must refuse it\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("name of a job") && err.contains("name of a state"),
        "the message must name both readings: {err}"
    );
    assert!(
        err.contains("--state completed"),
        "the message must give the way to ask for the state: {err}"
    );

    // Nothing went. A command that refuses must delete nothing at all.
    assert_eq!(
        h.list_json().len(),
        2,
        "the refusal must leave every record where it was"
    );

    // A pipeline that carries the name of a state reads the same way, and it
    // must read that way ON ITS OWN.
    //
    // The job named `completed` goes first. While that job is there the JOB
    // reading is true, and the pipeline reading is never reached: a test that
    // keeps the job passes with the pipeline reading deleted, and `qex clean
    // completed` would then delete every record that succeeded.
    h.ok(&["clean", &named]);
    h.ok(&["clean", &other]);
    assert_eq!(h.list_json().len(), 0, "no job may carry the name now");

    let file = h.root.join("completed.toml");
    std::fs::write(&file, "[[jobs]]\nname = \"stage\"\ncommand = [\"true\"]\n").unwrap();
    let group = h.ok(&["pipeline", file.to_str().unwrap()]);
    h.qex(&["wait", &group, "--timeout", "45s"]);
    let out = h.qex(&["clean", "completed"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("name of a pipeline"),
        "the message must name the PIPELINE reading: {err}"
    );
    assert_eq!(
        h.list_json().len(),
        1,
        "the refusal must leave the stage of the pipeline: {err}"
    );
    assert_eq!(
        out.status.code(),
        Some(127),
        "a pipeline reads the same way"
    );
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

/// A command that WAITS must answer at once for a job that does not exist.
///
/// A wait that nothing can satisfy is the fault that qex exists to remove. An
/// agent that meets one holds a terminal until a person looks. It has no exit
/// code to act on, and no message that says the wait cannot end.
///
/// The limit of qex and the limit of `qex_within` bound each form together. A
/// guard that goes away thus gives a failure, and never a suite that stops.
#[test]
fn a_command_that_waits_answers_at_once_for_a_job_that_does_not_exist() {
    let h = Harness::with_default_config("waitnojob");
    let unknown = "11111111-2222-3333-4444-666666666666";

    // Each form that takes a time limit carries one BELOW the limit of
    // `qex_within`, because the two limits catch two faults.
    //
    // A fault that qex can bound ends at the limit of qex and gives 124, and
    // that code names the cause. A fault BEFORE qex resolves the id ends at
    // `qex_within`, which reports only that no answer arrived.
    //
    // `logs --follow` takes no time limit, so `qex_within` alone stops it.
    let forms: [&[&str]; 5] = [
        &["wait", unknown, "--timeout", "10s"],
        &["wait", unknown, "--next", "--timeout", "10s"],
        &["status", unknown, "--wait", "--timeout", "10s", "--no-logs"],
        &[
            "status",
            unknown,
            "--follow",
            "--timeout",
            "10s",
            "--no-logs",
        ],
        &["logs", unknown, "--follow"],
    ];

    for form in forms {
        let started = Instant::now();
        let out = h.qex_within(form, Duration::from_secs(30));
        let took = started.elapsed();
        assert_eq!(
            out.status.code(),
            Some(127),
            "`qex {}` must give the code 127, and it gave this: {}",
            form.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            took < Duration::from_secs(8),
            "`qex {}` took {took:?}, and the answer was ready at once",
            form.join(" ")
        );
    }
}

/// A wait for a job whose RECORD a `qex clean` deleted must end, and it must
/// say that the work happened.
///
/// The two states need two answers. A job that never existed is a fault of the
/// id that the reader gave. A record that qex deleted names work that RAN, so
/// an agent that submits it again repeats it. Both give the code 127, because
/// the code answers one question: qex holds no job with that id now.
#[test]
fn a_wait_for_a_record_that_clean_deleted_says_the_work_happened() {
    let h = Harness::with_default_config("waitcleaned");
    let id = h.submit(&["submit", "--name", "gone", "--", "true"]);
    h.until("the job stops", Duration::from_secs(45), || {
        h.state_of(&id) == "completed"
    });
    h.ok(&["clean", &id]);

    let started = Instant::now();
    let out = h.qex_within(&["wait", &id, "--timeout", "10s"], Duration::from_secs(30));
    let took = started.elapsed();
    let said = String::from_utf8_lossy(&out.stderr).to_string();

    assert_eq!(out.status.code(), Some(127), "the answer is 127: {said}");
    assert!(
        took < Duration::from_secs(8),
        "the wait took {took:?}, and the answer was ready at once"
    );
    assert!(
        said.contains("HAPPENED"),
        "the message must say that the work happened, so that an agent does \
         not repeat it: {said}"
    );
    assert!(
        said.contains("gone"),
        "the message must name the job: {said}"
    );
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
        let mut cmd = Command::new(exe);
        cmd.args(args);
        isolate(&mut cmd, &h.root, &h.peers_dir);
        cmd.output().expect("qex did not start")
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

/// The group id of a pipeline is a handle that every command accepts.
///
/// `qex pipeline` writes that id to stdout, so it is the value that a user
/// keeps. It answered "there is no job with the id ..." in `qex wait`, `qex
/// status` and `qex kill` with the code 127, and in `qex clean` with the code
/// one. The code 127 also says that a value names nothing, so a script could
/// separate neither the two readings nor the two commands, and the documented
/// way to use a pipeline ended with the user finding the last stage by hand.
#[test]
fn the_group_id_of_a_pipeline_is_a_handle() {
    let h = Harness::with_default_config("grouphandle");

    let pipeline = h.root.join("ci.toml");
    std::fs::write(
        &pipeline,
        "[[jobs]]\nname = \"build\"\ncommand = [\"true\"]\n\n\
         [[jobs]]\nname = \"test\"\ncommand = [\"true\"]\nneeds = [\"build\"]\n",
    )
    .unwrap();

    let group = h.ok(&["pipeline", pipeline.to_str().unwrap()]);
    assert!(group.parse::<uuid::Uuid>().is_ok(), "got: {group}");

    // `qex wait` waits for every stage, and it succeeds because both stages
    // succeed.
    let out = h.ok(&["wait", &group, "--timeout", "60s"]);
    assert!(out.contains("completed"), "got: {out}");
    assert_eq!(out.lines().count(), 2, "each stage gives one line: {out}");

    // A user who names the pipeline AND one of its stages waits for that stage
    // ONE time. Each job must appear once, or a reader counts two results for
    // one job.
    let build_id = h.status_json("build")["id"].as_str().unwrap().to_string();
    let out = h.ok(&["wait", &group, &build_id, "--timeout", "60s"]);
    assert_eq!(
        out.lines().count(),
        2,
        "the pipeline and one of its stages give two jobs, not three: {out}"
    );

    // `qex status --json` gives an array for a pipeline, and one object for one
    // job. A script that reads one job must not become a script that reads an
    // array.
    let text = h.ok(&["status", &group, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let stages = v.as_array().expect("a pipeline gives an array");
    assert_eq!(stages.len(), 2, "got: {text}");
    // The stages arrive in the order of submission.
    assert_eq!(stages[0]["name"], "build");
    assert_eq!(stages[1]["name"], "test");

    // ONE pipeline of several stages is one run, and it gets no warning. The
    // warning counts RUNS, and not stages, so a pipeline of two stages must
    // not read as two pipelines.
    let out = h.qex(&["list", "--group", &group]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("pipelines"),
        "one run of two stages is one pipeline: {err}"
    );

    // One stage still gives one object.
    let one = h.ok(&["status", "build", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&one).unwrap();
    assert!(v.is_object(), "one job must give one object: {one}");

    // Without `--json`, one empty line separates the stages. A reader of a
    // pipeline of ten stages needs to see where each record starts.
    //
    // Read the output that the command WROTE. `Harness::ok` trims, and a
    // trimmed answer cannot show a leading empty line, so an assertion on it
    // would pass whatever the command wrote.
    let out = h.qex(&["status", &group]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("\n\nid:"),
        "an empty line must separate the stages: {text}"
    );
    // The separator goes BETWEEN the stages. An empty first line wastes the
    // first line of the answer, and `qex status $ID` for one job never had one.
    assert!(
        text.starts_with("id:"),
        "the answer must not open with an empty line: {text:?}"
    );

    // `qex logs` reads one job. It must refuse a pipeline and name the stages,
    // because qex must not choose a stage for the reader.
    let out = h.qex(&["logs", &group]);
    assert_eq!(out.status.code(), Some(127), "a pipeline is not one job");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("takes one job"),
        "the message must say that this command reads one job: {err}"
    );
    assert!(
        err.contains("build") && err.contains("test"),
        "the message must name every stage: {err}"
    );

    // A pipeline of ONE stage is still a pipeline, so `qex logs` refuses it
    // too. A rule that changed on the day a pipeline has one stage would give
    // a reader the logs of a stage that the reader did not name.
    let solo = h.root.join("solo.toml");
    std::fs::write(
        &solo,
        "name = \"solo\"\n\n[[jobs]]\nname = \"only\"\ncommand = [\"true\"]\n",
    )
    .unwrap();
    let one_stage = h.ok(&["pipeline", solo.to_str().unwrap()]);
    let out = h.qex(&["logs", &one_stage]);
    assert_eq!(
        out.status.code(),
        Some(127),
        "a pipeline of one stage is still a pipeline: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("takes one job") && err.contains("only"),
        "the message must name the stage: {err}"
    );

    // `qex clean` takes the group and deletes every stage.
    h.ok(&["clean", &group]);
    let left = h.ok(&["list", "--group", &group]);
    assert!(left.contains("no jobs"), "got: {left}");

    // `qex status $GROUP --wait` reports the FIRST fault of the pipeline.
    //
    // A stage that qex skipped would otherwise hide the stage that failed: the
    // last stage of a pipeline is nearly always the skipped one, and its code
    // 126 tells the reader that an earlier job failed without saying that THIS
    // pipeline holds the failure.
    let broken = h.root.join("broken.toml");
    std::fs::write(
        &broken,
        "name = \"broken\"\n\n\
         [[jobs]]\nname = \"bad\"\ncommand = [\"false\"]\n\n\
         [[jobs]]\nname = \"after\"\ncommand = [\"true\"]\nneeds = [\"bad\"]\n",
    )
    .unwrap();
    let bad_group = h.ok(&["pipeline", broken.to_str().unwrap()]);
    let out = h.qex(&["status", &bad_group, "--wait", "--timeout", "60s"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "the code must name the stage that FAILED, and not the stage that qex \
         skipped\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // A value that names nothing still gives the code 127, and the message now
    // says that a pipeline is also a handle.
    let out = h.qex(&["status", "no-such-thing"]);
    assert_eq!(out.status.code(), Some(127));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("pipeline"), "got: {err}");
}

/// One word must never reach two pipelines.
///
/// A pipeline takes its name from its file, so a second run of the same file
/// carries the same name. `qex kill ci` thus stopped the work of two separate
/// runs when the user named one. A command that stops or deletes must refuse
/// the word and name the runs.
#[test]
fn a_word_that_names_two_pipelines_is_refused() {
    let h = Harness::with_default_config("grouptwice");

    let pipeline = h.root.join("twice.toml");
    std::fs::write(
        &pipeline,
        "[[jobs]]\nname = \"one\"\ncommand = [\"sleep\", \"30\"]\n",
    )
    .unwrap();

    // Two runs of one file. Both carry the name `twice`.
    let first = h.ok(&["pipeline", pipeline.to_str().unwrap()]);
    let second = h.ok(&["pipeline", pipeline.to_str().unwrap()]);
    assert_ne!(first, second);

    for command in [
        vec!["kill", "twice"],
        vec!["cancel", "twice"],
        vec!["status", "twice"],
        vec!["clean", "twice"],
        vec!["wait", "twice", "--timeout", "5s"],
    ] {
        let out = h.qex(&command);
        assert_eq!(
            out.status.code(),
            Some(127),
            "`qex {}` must refuse the word",
            command.join(" ")
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("2 pipelines"),
            "`qex {}` must say how many runs the word names, and it said: {err}",
            command.join(" ")
        );
    }

    // `qex list --group` is where the user looks after that refusal. It reads
    // and deletes nothing, so it shows every run; but a reader who does not
    // know that the table holds two runs reads ONE pipeline that ran each
    // stage two times. It must thus say how many runs the word names, and give
    // the group id of each.
    let out = h.qex(&["list", "--group", "twice"]);
    assert!(out.status.success(), "`qex list --group` must not refuse");
    let table = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        table.lines().filter(|l| l.contains("one")).count(),
        2,
        "the table must hold the stage of both runs: {table}"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("2 pipelines"),
        "the warning must say how many runs the word names: {err}"
    );
    assert!(
        err.contains(first.as_str()) && err.contains(second.as_str()),
        "the warning must give the group id of each run: {err}"
    );

    // `--json` is read by a machine, and that reader needs no sentence: the
    // `group` field of each job already separates the runs. The warning is for
    // a person, so it does not go with the JSON.
    let out = h.qex(&["list", "--group", "twice", "--json"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("2 pipelines"),
        "the warning is for a person, and `--json` is for a machine: {err}"
    );

    // One run only gives no warning. A sentence that appears every time is a
    // sentence that a reader stops reading.
    let out = h.qex(&["list", "--group", &first]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("pipelines"),
        "one run must give no warning: {err}"
    );

    // Both runs must still operate. A refusal that stopped one of them would
    // be the fault that this test prevents.
    for group in [&first, &second] {
        let text = h.ok(&["list", "--group", group, "--json"]);
        let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        assert_eq!(jobs.len(), 1, "the run {group} must still hold its stage");
        assert_ne!(jobs[0]["state"], "killed", "the run {group} must continue");
    }

    // The group id still names one run, and it stops that run only.
    //
    // Wait for the stage to hold a process first. Between "the state is
    // running" and "the coordinator recorded the pid" a kill answers `the job
    // starts now. Try the command again.`, which is the same answer that
    // `qex kill $ID` gives for that moment. This step stops the work of the
    // test, so it must not race with the start of that work.
    for group in [&first, &second] {
        h.until(
            "the stage of the run holds a process",
            Duration::from_secs(30),
            || {
                let text = h.ok(&["list", "--group", group, "--json"]);
                let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
                jobs.iter().all(|j| j["pid"].as_u64().is_some())
            },
        );
        h.ok(&["kill", group]);
    }

    // Each kill reached one run, and the runs stopped separately.
    for group in [&first, &second] {
        let text = h.ok(&["list", "--group", group, "--json"]);
        let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        assert_eq!(jobs.len(), 1);
    }
}

/// `qex kill $GROUP` must stop a stage that WAITS, and not leave it to start.
///
/// The documentation says that the command stops every stage. An early version
/// reported the refusal for a queued stage as information and gave the code 0,
/// so a script read that the pipeline stopped, and the queued stage then
/// started and did its work.
///
/// `qex cancel $GROUP` must not do the opposite: a stage that OPERATES cannot
/// leave the queue, and that refusal keeps its code.
#[test]
fn a_kill_of_a_group_stops_a_stage_that_waits_in_the_queue() {
    // One core, so the second stage cannot start while the first operates.
    let h = Harness::new(
        "groupqueued",
        "[budget]\ncpu = \"1\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    let pipeline = h.root.join("both.toml");
    // Two stages that need nothing. The budget holds one of them.
    std::fs::write(
        &pipeline,
        "[[jobs]]\nname = \"first\"\ncommand = [\"sleep\", \"60\"]\n\n\
         [[jobs]]\nname = \"second\"\ncommand = [\"sleep\", \"60\"]\n",
    )
    .unwrap();

    let group = h.ok(&["pipeline", pipeline.to_str().unwrap()]);

    // Wait until one stage operates and one waits.
    let mut ready = false;
    for _ in 0..100 {
        let text = h.ok(&["list", "--group", &group, "--json"]);
        let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        ready = jobs.iter().any(|j| j["state"] == "running")
            && jobs.iter().any(|j| j["state"] == "queued");
        if ready {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "one stage must operate and one must wait");

    // `qex kill $ID` for ONE job that waits keeps its fault and names `qex
    // cancel`. That user asked about that one job, and the answer is that a
    // job in the queue has no process to signal. Only a user who named the
    // whole pipeline asked for the queue to be emptied.
    let text = h.ok(&["list", "--group", &group, "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    let queued = jobs.iter().find(|j| j["state"] == "queued").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let out = h.qex(&["kill", &queued]);
    assert_ne!(
        out.status.code(),
        Some(0),
        "`qex kill $ID` for one job that waits must give a fault"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("cancel"),
        "the fault must name `qex cancel`: {err}"
    );
    assert_eq!(
        h.state_of(&queued),
        "queued",
        "that job must stay in the queue"
    );

    // `qex cancel $GROUP` must NOT report success: the stage that operates
    // cannot leave the queue.
    let out = h.qex(&["cancel", &group]);
    assert_ne!(
        out.status.code(),
        Some(0),
        "a stage that operates cannot leave the queue, and the command must say so\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let out = h.qex(&["kill", &group]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // No stage must operate after this, now or later. A stage that qex left in
    // the queue would start when the first stage releases the core.
    for _ in 0..40 {
        let text = h.ok(&["list", "--group", &group, "--json"]);
        let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        for j in &jobs {
            assert_ne!(
                j["state"], "queued",
                "a stage that waits must leave the queue: {j}"
            );
        }
        if jobs.iter().all(|j| j["state"] != "running") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let text = h.ok(&["list", "--group", &group, "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(jobs.len(), 2);
    for j in &jobs {
        assert!(
            j["state"] == "killed" || j["state"] == "cancelled",
            "every stage must stop: {j}"
        );
    }
}

/// `qex kill $GROUP` must succeed when a stage already stopped.
///
/// The stages of a pipeline stop in order, so at the moment that a user stops a
/// pipeline the early stages have usually finished. An early version gave the
/// code 1 for that ordinary case, and the documentation says that the command
/// stops every stage.
#[test]
fn a_kill_of_a_group_accepts_a_stage_that_already_stopped() {
    let h = Harness::with_default_config("groupdone");

    let pipeline = h.root.join("mixed.toml");
    std::fs::write(
        &pipeline,
        "[[jobs]]\nname = \"quick\"\ncommand = [\"true\"]\n\n\
         [[jobs]]\nname = \"slow\"\ncommand = [\"sleep\", \"60\"]\nneeds = [\"quick\"]\n",
    )
    .unwrap();

    let group = h.ok(&["pipeline", pipeline.to_str().unwrap()]);

    // Wait until the first stage stopped and the second operates.
    let mut ready = false;
    for _ in 0..100 {
        let text = h.ok(&["list", "--group", &group, "--json"]);
        let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        ready = jobs.iter().any(|j| j["state"] == "completed")
            && jobs.iter().any(|j| j["state"] == "running");
        if ready {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "one stage must stop and one must operate");

    // The command must succeed. The stage that stopped is information, and the
    // stage that operates receives the signal.
    let out = h.qex(&["kill", &group]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a stage that already stopped is not a fault when the user named the whole \
         pipeline\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // One job that already stopped is still a fault. That user asked about
    // that job, and the answer is that it stopped.
    let text = h.ok(&["list", "--group", &group, "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    let quick = jobs
        .iter()
        .find(|j| j["name"] == "quick")
        .unwrap()
        .get("id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let out = h.qex(&["kill", &quick]);
    assert_ne!(
        out.status.code(),
        Some(0),
        "one job that already stopped must still give a fault"
    );

    // `qex cancel $ID` reads the same way. Only a user who named the WHOLE
    // pipeline said "stop what is left"; this user asked about one job, and a
    // job that already stopped cannot leave the queue.
    let out = h.qex(&["cancel", &quick]);
    assert_ne!(
        out.status.code(),
        Some(0),
        "`qex cancel $ID` for one job that already stopped must give a fault\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `qex kill` takes the group id and stops every stage that operates.
#[test]
fn a_kill_of_a_group_reaches_every_stage() {
    let h = Harness::with_default_config("groupkill");

    let pipeline = h.root.join("slow.toml");
    // Two stages that need nothing, so both operate together.
    std::fs::write(
        &pipeline,
        "[[jobs]]\nname = \"one\"\ncommand = [\"sleep\", \"60\"]\n\n\
         [[jobs]]\nname = \"two\"\ncommand = [\"sleep\", \"60\"]\n",
    )
    .unwrap();

    let group = h.ok(&["pipeline", pipeline.to_str().unwrap()]);

    // Wait until both stages operate.
    let mut running = 0;
    for _ in 0..100 {
        let text = h.ok(&["list", "--group", &group, "--json"]);
        let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        running = jobs.iter().filter(|j| j["state"] == "running").count();
        if running == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(running, 2, "both stages must operate before the kill");

    // The time limit belongs to the COMMAND, and not to each stage. When it
    // arrives the command stops and says so ONE time. A limit that starts
    // again for each stage gives a pipeline of ten stages ten times the time
    // that the user gave, and it repeats the sentence for every stage.
    let out = h.qex(&["status", &group, "--wait", "--timeout", "1s"]);
    assert_eq!(out.status.code(), Some(124), "the wait reached its limit");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        err.matches("reached its time limit").count(),
        1,
        "the command stops at the first stage that reaches the limit: {err}"
    );

    h.ok(&["kill", &group]);

    // Both stages must stop. `qex wait` gives a code that is not 0, because a
    // job that something stopped did not succeed.
    let out = h.qex(&["wait", &group, "--timeout", "60s"]);
    assert_ne!(out.status.code(), Some(0));

    let text = h.ok(&["list", "--group", &group, "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(jobs.len(), 2);
    for j in &jobs {
        assert_eq!(j["state"], "killed", "got: {j}");
    }
}

/// `--needs $GROUP` must wait for the WHOLE pipeline.
///
/// The reference and `qex help pipelines` both say so. A job that named a
/// pipeline could otherwise start while a stage of that pipeline still
/// operates, and the pipeline would report success although the order was
/// wrong.
///
/// The test for "a run of an earlier day" applies to the pipeline as ONE unit.
/// The stages of a pipeline stop in order, so a test of each stage would refuse
/// the ordinary case, in which the early stages have finished and a late stage
/// still operates.
#[test]
fn needs_a_group_waits_for_every_stage() {
    let h = Harness::with_default_config("needsgroup");

    let pipeline = h.root.join("stages.toml");
    // `quick` stops at once. `slow` operates for a long time.
    std::fs::write(
        &pipeline,
        "name = \"waves\"\n\n\
         [[jobs]]\nname = \"quick\"\ncommand = [\"true\"]\n\n\
         [[jobs]]\nname = \"slow\"\ncommand = [\"sleep\", \"60\"]\nneeds = [\"quick\"]\n",
    )
    .unwrap();

    let group = h.ok(&["pipeline", pipeline.to_str().unwrap()]);

    // Wait until `quick` stopped and `slow` operates. This is the ordinary
    // case: one stage of the pipeline already stopped.
    h.until(
        "one stage must stop and one must operate",
        Duration::from_secs(30),
        || {
            let text = h.ok(&["list", "--group", &group, "--json"]);
            let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
            jobs.iter().any(|j| j["state"] == "completed")
                && jobs.iter().any(|j| j["state"] == "running")
        },
    );

    // The NAME of the pipeline is accepted while one stage still operates.
    let out = h.qex(&["submit", "--needs", "waves", "--", "true"]);
    assert!(
        out.status.success(),
        "`--needs <pipeline name>` must be accepted while a stage operates: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The group id gives EVERY stage, so the new job waits for the whole
    // pipeline and not for the stage that already stopped.
    let last = h.submit(&["submit", "--needs", &group, "--", "true"]);
    let status = h.status_json(&last);
    assert_eq!(
        status["needs"].as_array().map(|n| n.len()),
        Some(2),
        "`--needs $GROUP` must name every stage: {status}"
    );
    assert_eq!(
        status["state"].as_str(),
        Some("queued"),
        "the job must wait while a stage of the pipeline operates: {status}"
    );

    // Every stage stops, and the job then leaves the queue.
    h.ok(&["kill", &group]);
    h.qex(&["wait", &last, "--timeout", "60s"]);
    assert_ne!(
        h.state_of(&last),
        "queued",
        "the job must leave the queue when every stage stopped"
    );

    // A pipeline whose every stage stopped is a run of an earlier day, and the
    // NAME of it is refused.
    let out = h.qex(&["submit", "--needs", "waves", "--", "true"]);
    assert!(
        !out.status.success(),
        "a pipeline that stopped must be refused by name"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("every stage already stopped"),
        "the error must say that the pipeline stopped: {err}"
    );
    assert!(
        err.contains("GROUP=$(qex pipeline"),
        "the error must give the remedy: {err}"
    );

    // The group id is still accepted, because an id names one run for ever.
    let out = h.qex(&["submit", "--needs", &group, "--", "true"]);
    assert!(
        out.status.success(),
        "a group id must be accepted whatever the state of its stages: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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
        let out = h
            .command(&["submit", "--name", name, "--", "true"])
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
        let out = h.command(args).current_dir(dir).output().unwrap();
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
    let out = h
        .command(&["clean", "--under"])
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
    assert!(out.contains("deleted 0 records"), "nothing must go: {out}");
    assert!(
        out.contains("record stayed") && out.contains("needs a record"),
        "the message must give the reason: {out}"
    );
    assert!(
        out.contains(&second[..8]),
        "the message must name the work that holds the record: {out}"
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

    let out = h
        .command(&["submit", "--", "echo", "long-path-ok"])
        .env("XDG_RUNTIME_DIR", &long)
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "qex failed with a long runtime directory: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let wait = h
        .command(&["wait", &id, "--timeout", "30s"])
        .env("XDG_RUNTIME_DIR", &long)
        .output()
        .unwrap();
    assert_eq!(wait.status.code(), Some(0));

    // Stop the coordinator of this test.
    let info = h
        .command(&["info", "--json"])
        .env("XDG_RUNTIME_DIR", &long)
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
         if [ $n -lt 2 ]; then \
             i=0; while [ $i -lt 1000000 ]; do i=$((i+1)); done; exit 3; \
         fi; sleep 5",
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
                // this record may ever hold is `completed`. The job also stays
                // active: `queued` would release its slot.
                assert_ne!(
                    state, "failed",
                    "the record must never hold the state of an attempt that starts \
                     again; it held `failed` at attempt {attempts}"
                );
                if attempts >= 1 {
                    assert_ne!(
                        state, "queued",
                        "a job with retries left must keep its slot; it held \
                         `queued` at attempt {attempts}"
                    );
                }

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
    assert_eq!(
        status["error"],
        serde_json::Value::Null,
        "a successful final attempt must not keep the error of the failed attempt: {status}"
    );
    let cpu_secs = status["usage"]["cpu_secs"].as_f64().unwrap();
    assert!(
        cpu_secs < 0.5,
        "the final sleeping attempt must not include the CPU-heavy failed attempt; got \
         {cpu_secs:.3}s: {status}"
    );
}

/// A job with retries keeps its slot until it stops. Another job must wait.
///
/// # The fault that this test prevents
///
/// A retry used to write `queued` between attempts. `claimed()` counts only
/// `starting` and `running`, so a coordinator that started again — or that
/// took the write — treated the slot as free. A second job of one core then
/// started while the first job's next attempt still ran, and the machine
/// used more cores than the budget.
///
/// The job stays `running`. The waiter stays in the queue until the last
/// attempt stops.
#[test]
fn a_job_with_retries_keeps_its_slot_until_it_stops() {
    let h = Harness::new(
        "retryslot",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [budget]\ncpu = \"1\"\nmem = \"1GB\"\n",
    );

    let counter = h.root.join("attempts");
    let script = format!(
        "n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; \
         if [ $n -lt 2 ]; then exit 3; fi; sleep 8",
        c = counter.display()
    );
    let first = h.submit(&[
        "submit",
        "--name",
        "retry",
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--retries",
        "1",
        "--",
        "sh",
        "-c",
        &script,
    ]);
    h.until(
        "the first attempt operates",
        Duration::from_secs(45),
        || h.state_of(&first) == "running",
    );

    let waiter = h.submit(&[
        "submit", "--name", "waiter", "--cpu", "1", "--mem", "64MB", "--", "true",
    ]);

    // Cover the gap between the two attempts, and the second attempt itself.
    // Read the record FILE as well as the coordinator: `queued` on the disk
    // is the write that would release the slot after a restart.
    let record = h.job_dir(&first).join("status.json");
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let first_state = h.state_of(&first);
        if first_state == "completed" {
            break;
        }
        assert_eq!(
            first_state, "running",
            "a job with retries left must stay active"
        );
        if let Ok(text) = std::fs::read_to_string(&record) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                let on_disk = v["state"].as_str().unwrap_or("");
                if on_disk != "completed" {
                    assert_ne!(
                        on_disk, "queued",
                        "the record must not release the slot between attempts"
                    );
                    assert_eq!(
                        on_disk, "running",
                        "the record must stay active between attempts: {on_disk}"
                    );
                }
            }
        }
        assert_eq!(
            h.state_of(&waiter),
            "queued",
            "a job with retries left must keep its slot"
        );
        assert!(
            !h.has_started(&waiter),
            "the waiter started while the retrying job still held the core"
        );
        assert!(Instant::now() < deadline, "the retrying job did not finish");
        std::thread::sleep(Duration::from_millis(200));
    }

    h.until("the waiter starts", Duration::from_secs(45), || {
        h.has_started(&waiter)
    });
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
    //
    // The section must be one that NO qex knows. This test wrote `[hooks]`
    // until qex gained a `[hooks]` section, and it then wrote a file that
    // PARSES: the supervisor met no fault, and the test measured nothing.
    // Choose a name that this build refuses, and change it again when qex takes
    // that name.
    h.write_config(&format!(
        "{good}\n[telemetry]\nendpoint = \"https://example.invalid\"\n"
    ));

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

/// A job that stops must run the stop hook one time, and the hook must receive
/// the result of the job.
///
/// One run for each job is the point of this feature. A person who receives the
/// same notification two times learns to ignore every notification, and the
/// notification then has no value.
#[test]
fn a_job_that_stops_runs_the_stop_hook_one_time_with_its_result() {
    let h = Harness::with_default_config("hookone");
    let mark = h.root.join("hook.txt");
    h.write_config(&format!(
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [hooks]\non_stop = [\"sh\", \"-c\", \
         \"echo \\\"$QEX_JOB_ID $QEX_STATE $QEX_EXIT_CODE $QEX_JOB_NAME\\\" >> {}\"]\n",
        mark.display()
    ));

    let id = h.submit(&["submit", "--name", "report", "--", "sh", "-c", "exit 7"]);
    let out = h.qex(&["wait", &id]);
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(h.state_of(&id), "failed");

    // The hook starts after the record says that the job stopped, so `qex wait`
    // can give its answer first. Wait for the hook itself.
    h.until(
        "the stop hook wrote its line",
        Duration::from_secs(30),
        || !h.hook_lines().is_empty(),
    );

    let lines = h.hook_lines();
    assert_eq!(lines.len(), 1, "the hook must run one time: {lines:?}");
    assert_eq!(
        lines[0],
        format!("{id} failed 7 report"),
        "the hook must receive the id, the state and the exit code"
    );

    // The job directory holds the record of the run. A second process that
    // makes this job terminal reads it and does nothing.
    assert!(h.job_dir(&id).join("hook.ran").exists());

    // THE SUPERVISOR notified, and not the coordinator. Both can, and the count
    // above passes for either, so name the one that must.
    assert_eq!(
        h.hook_origin(&id),
        "supervisor",
        "the supervisor of a job that ran must be the process that notifies"
    );

    // Give a second process the time to run the hook again. The count must not
    // change.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(h.hook_lines().len(), 1, "the hook ran more than one time");
}

/// A hook that hangs must not hold the job, the queue or the coordinator.
///
/// The hook is a command that a user wrote, so it can hang. qex runs it after
/// the final state is on the disk. The job thus has its result, its claim
/// leaves the budget, and the next job starts while the hook still hangs.
#[test]
fn a_stop_hook_that_hangs_holds_neither_the_job_nor_the_queue() {
    let h = Harness::with_default_config("hookhang");
    let mark = h.root.join("hook.txt");
    // A budget of one core. The second job can start only after the first job
    // gives its claim back.
    h.write_config(&format!(
        "[budget]\ncpu = \"1\"\nmem = \"512MB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [hooks]\ntimeout = \"2s\"\n\
         on_stop = [\"sh\", \"-c\", \"echo hanging >> {}; sleep 300\"]\n",
        mark.display()
    ));

    let first = h.submit(&["submit", "--cpu", "1", "--mem", "128MB", "--", "true"]);
    let out = h.qex(&["wait", &first, "--timeout", "30s"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a hook that hangs must not hold the job in a state that is not final"
    );
    assert_eq!(h.state_of(&first), "completed");

    // Wait until the hook hangs. The test measures nothing before that moment.
    h.until("the stop hook started", Duration::from_secs(30), || {
        !h.hook_lines().is_empty()
    });

    // The coordinator must still answer.
    let info = h.ok(&["info", "--json"]);
    assert!(
        info.contains("\"pid\""),
        "the coordinator must answer: {info}"
    );

    // The next job must start and stop while the hook of the first job hangs.
    let second = h.submit(&["submit", "--cpu", "1", "--mem", "128MB", "--", "true"]);
    let out = h.qex(&["wait", &second, "--timeout", "30s"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a hook that hangs must not delay the next job"
    );
    assert_eq!(h.state_of(&second), "completed");
}

/// The states in the config file select the jobs that give a message, and a job
/// that the coordinator stops runs the hook as well.
///
/// A queue with many jobs and a message for each job is a set of messages that
/// a person turns off. A job that never ran has no supervisor, so the
/// coordinator runs its hook.
#[test]
fn the_configured_states_select_the_jobs_that_run_the_stop_hook() {
    let h = Harness::with_default_config("hookstates");
    let mark = h.root.join("hook.txt");
    h.write_config(&format!(
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [hooks]\non_stop_states = [\"skipped\"]\n\
         on_stop = [\"sh\", \"-c\", \"echo \\\"$QEX_STATE $QEX_JOB_NAME\\\" >> {}\"]\n",
        mark.display()
    ));

    let build = h.submit(&["submit", "--name", "build", "--", "false"]);
    let test = h.submit(&["submit", "--name", "test", "--needs", &build, "--", "true"]);

    h.qex(&["wait", &build]);
    h.until("the second job is skipped", Duration::from_secs(30), || {
        h.state_of(&test) == "skipped"
    });

    h.until(
        "the stop hook wrote its line",
        Duration::from_secs(30),
        || !h.hook_lines().is_empty(),
    );
    std::thread::sleep(Duration::from_secs(1));

    let lines = h.hook_lines();
    assert_eq!(
        lines,
        vec!["skipped test".to_string()],
        "the filter must select the state `skipped` only"
    );
    assert_eq!(
        h.hook_origin(&test),
        "coordinator",
        "a job that never ran has no supervisor, so the coordinator notifies"
    );
}

/// A job whose supervisor stopped must still notify, one time.
///
/// The supervisor runs the hook. A supervisor that a signal stops runs nothing,
/// and the person then waits for a message that cannot arrive. The coordinator
/// makes that job `failed`, so the coordinator runs the hook. The claim file
/// stops a second message.
///
/// This test also reads `qex logs --hook`. The verdict of qex must reach a file
/// that a qex command shows: a user whose notification did not arrive had no
/// command that gave the reason.
#[test]
fn a_job_whose_supervisor_stopped_still_runs_the_stop_hook_one_time() {
    let h = Harness::with_default_config("hookdead");
    let mark = h.root.join("hook.txt");
    h.write_config(&format!(
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [hooks]\non_stop = [\"sh\", \"-c\", \"echo \\\"$QEX_STATE\\\" >> {}\"]\n",
        mark.display()
    ));

    let id = h.submit(&["submit", "--name", "long", "--", "sleep", "60"]);
    h.until("the job operates", Duration::from_secs(45), || {
        h.status_json(&id)["supervisor_pid"].as_i64().is_some() && h.state_of(&id) == "running"
    });

    // Stop the supervisor of this job. This test started that process.
    let supervisor = h.status_json(&id)["supervisor_pid"].as_i64().unwrap() as i32;
    unsafe {
        libc::kill(supervisor, libc::SIGKILL);
    }

    h.until("the job is failed", Duration::from_secs(45), || {
        h.state_of(&id) == "failed"
    });
    h.until(
        "the stop hook wrote its line",
        Duration::from_secs(30),
        || !h.hook_lines().is_empty(),
    );

    // Give a second process the time to notify again.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        h.hook_lines(),
        vec!["failed".to_string()],
        "the hook must run one time for a job that lost its supervisor"
    );
    assert_eq!(
        h.hook_origin(&id),
        "coordinator",
        "with no supervisor, the coordinator must be the process that notifies"
    );

    // The verdict of qex must reach a qex command.
    let text = h.ok(&["logs", &id, "--hook"]);
    assert!(
        text.contains("qex: the stop hook"),
        "`qex logs --hook` must give the verdict of qex: {text}"
    );
}

/// A hook that a user deletes from the config file must not run again, and a
/// hook that a user adds must run at once.
///
/// The coordinator reads its configuration one time, at its start, and it
/// operates for hours. With that configuration, a hook that the user deleted
/// still ran on each job, and a hook that the user added never ran on the paths
/// of the coordinator — while `qex config show` gave the new value. A stale
/// configuration that makes qex do nothing is a fault; a stale configuration
/// that makes qex RUN A COMMAND THAT THE USER DELETED is a different thing.
#[test]
fn a_stop_hook_that_the_user_deletes_does_not_run_again() {
    let h = Harness::with_default_config("hookstale");
    let mark = h.root.join("hook.txt");
    let base = "[peers]\nenabled = false\n\
                [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n";
    h.write_config(&format!(
        "{base}[hooks]\non_stop = [\"sh\", \"-c\", \"echo \\\"$QEX_JOB_NAME\\\" >> {}\"]\n",
        mark.display()
    ));

    let first = h.submit(&["submit", "--name", "first", "--", "true"]);
    h.ok(&["wait", &first]);
    h.until(
        "the stop hook wrote its line",
        Duration::from_secs(30),
        || !h.hook_lines().is_empty(),
    );
    assert_eq!(h.hook_lines(), vec!["first".to_string()]);

    // Delete the hook. The same coordinator continues to operate.
    h.write_config(base);

    let second = h.submit(&["submit", "--name", "second", "--", "true"]);
    h.ok(&["wait", &second]);
    std::thread::sleep(Duration::from_secs(2));

    assert_eq!(
        h.hook_lines(),
        vec!["first".to_string()],
        "a hook that the user deleted must not run"
    );
    assert!(
        !h.job_dir(&second).join("hook.ran").exists(),
        "qex must not record a run of a hook that does not exist"
    );

    // The other direction: a hook that the user adds must run at once, also on
    // a path of the coordinator. A skipped job has no supervisor.
    h.write_config(&format!(
        "{base}[hooks]\non_stop_states = [\"skipped\"]\n\
         on_stop = [\"sh\", \"-c\", \"echo \\\"$QEX_STATE\\\" >> {}\"]\n",
        mark.display()
    ));

    let build = h.submit(&["submit", "--name", "build", "--", "false"]);
    let dependent = h.submit(&["submit", "--name", "dep", "--needs", &build, "--", "true"]);
    h.qex(&["wait", &build]);
    h.until(
        "the dependent job is skipped",
        Duration::from_secs(30),
        || h.state_of(&dependent) == "skipped",
    );
    h.until(
        "the new hook wrote its line",
        Duration::from_secs(30),
        || h.hook_lines().len() > 1,
    );
    assert_eq!(
        h.hook_lines(),
        vec!["first".to_string(), "skipped".to_string()],
        "a hook that the user adds must run on the next job that stops"
    );
}

/// A job must notify when NO COORDINATOR OPERATES.
///
/// # This is the sentence that the design of this feature rests on
///
/// `src/supervisor.rs` says: THE SUPERVISOR RUNS THE HOOK, because a
/// coordinator can stop and start again while a job runs, and a coordinator
/// that ran the hook would miss the jobs of the period in which it did not
/// operate. Nothing measured that sentence.
///
/// # Why the other tests cannot measure it
///
/// Three paths reach the hook of an ordinary job: the supervisor at the end of
/// its work, the supervisor when the command does not exist, and the
/// coordinator when it reaps that supervisor. EACH ONE ALONE gives the person
/// exactly one message. A hand deletion of each of the three, one at a time,
/// left the whole suite green, because every test asserted the COUNT of the
/// messages and the count was still one.
///
/// This test takes the coordinator away, so one path remains. It also asserts
/// the word in `hook.ran`, so it fails if the surviving path is the wrong one.
#[test]
fn a_job_notifies_from_its_supervisor_when_no_coordinator_operates() {
    let h = Harness::with_default_config("hooknocoord");
    let mark = h.root.join("hook.txt");
    h.write_config(&format!(
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [hooks]\non_stop = [\"sh\", \"-c\", \
         \"echo \\\"$QEX_STATE $QEX_JOB_NAME\\\" >> {}\"]\n",
        mark.display()
    ));

    // The job must still be running when the coordinator goes, so that the
    // supervisor writes the result and runs the hook with nothing else alive.
    let id = h.submit(&["submit", "--name", "alone", "--", "sleep", "5"]);
    h.until("the job operates", Duration::from_secs(45), || {
        h.state_of(&id) == "running" && h.status_json(&id)["supervisor_pid"].as_i64().is_some()
    });

    // Take the coordinator away. The supervisor is a session of its own, so it
    // continues. This test started this coordinator, and the pid comes from
    // qex itself.
    let coordinator = h.coordinator_pid();
    unsafe {
        libc::kill(coordinator, libc::SIGKILL);
    }

    // Prove that it is gone BEFORE the job stops. Without this step, a
    // coordinator that was still alive could notify, and the test would pass
    // while measuring the path that it means to take away.
    let deadline = Instant::now() + Duration::from_secs(30);
    while unsafe { libc::kill(coordinator, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "the coordinator {coordinator} did not stop"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // FROM HERE, RUN NO qex COMMAND UNTIL THE HOOK HAS RUN. `qex status` and
    // `qex list` start a coordinator when none operates, and that new
    // coordinator would reap the job and could notify. Read the disk instead.
    h.until(
        "the stop hook wrote its line",
        Duration::from_secs(60),
        || !h.hook_lines().is_empty(),
    );
    std::thread::sleep(Duration::from_secs(1));

    assert_eq!(
        h.hook_lines(),
        vec!["completed alone".to_string()],
        "a job must give one message when no coordinator operates"
    );
    assert_eq!(
        h.hook_origin(&id),
        "supervisor",
        "with no coordinator, only the supervisor can have notified"
    );
}

/// A job that gives up waiting must notify.
///
/// #14 added the state `expired` after this feature was written, and
/// `sched::expire` wrote the terminal record and ran no hook. A job that gave
/// up waiting is the case that a person MOST wants to hear about: nothing ran,
/// so no output, no exit code and no other message says so. Such a job never
/// had a supervisor, so the COORDINATOR must notify.
#[test]
fn a_job_that_gives_up_waiting_runs_the_stop_hook() {
    let h = Harness::new(
        "hookexpire",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [queue]\noversized = \"queue\"\n",
    );
    let mark = h.root.join("hook.txt");
    h.write_config(&format!(
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [queue]\noversized = \"queue\"\n\
         [hooks]\non_stop = [\"sh\", \"-c\", \
         \"echo \\\"$QEX_STATE $QEX_JOB_NAME\\\" >> {}\"]\n",
        mark.display()
    ));

    // A claim larger than the budget, which this config keeps in the queue. The
    // job thus can never start, whatever the machine does.
    let id = h.submit(&[
        "submit",
        "--name",
        "waiter",
        "--cpu",
        "64",
        "--max-queue-time",
        "3s",
        "--",
        "echo",
        "never",
    ]);

    h.until("the job gives up", Duration::from_secs(45), || {
        h.state_of(&id) == "expired"
    });
    h.until(
        "the stop hook wrote its line",
        Duration::from_secs(30),
        || !h.hook_lines().is_empty(),
    );
    std::thread::sleep(Duration::from_secs(1));

    assert_eq!(
        h.hook_lines(),
        vec!["expired waiter".to_string()],
        "a job that gave up waiting must give one message"
    );
    assert_eq!(
        h.hook_origin(&id),
        "coordinator",
        "such a job never had a supervisor, so the coordinator notifies"
    );
}

/// A job whose command does not exist must still notify.
///
/// The supervisor never starts a process for such a job, so it reaches the
/// state `failed` on a path of its own, and it leaves `main` before the end.
/// A typing fault in a command is one of the most frequent ways for a job to
/// fail, and it must not be the one failure that gives no message.
#[test]
fn a_job_whose_command_does_not_exist_still_runs_the_stop_hook() {
    let h = Harness::with_default_config("hooknocmd");
    let mark = h.root.join("hook.txt");
    h.write_config(&format!(
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [hooks]\non_stop = [\"sh\", \"-c\", \
         \"echo \\\"$QEX_STATE $QEX_JOB_NAME\\\" >> {}\"]\n",
        mark.display()
    ));

    let id = h.submit(&["submit", "--name", "typo", "--", "qex-no-such-program"]);
    h.until("the job failed", Duration::from_secs(45), || {
        h.state_of(&id) == "failed"
    });
    h.until(
        "the stop hook wrote its line",
        Duration::from_secs(30),
        || !h.hook_lines().is_empty(),
    );
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        h.hook_lines(),
        vec!["failed typo".to_string()],
        "a job whose command does not exist must give one message"
    );
    // The supervisor of this job exists; it never started a process. This is a
    // path of its own to `failed`, and it leaves `main` before the end.
    assert_eq!(
        h.hook_origin(&id),
        "supervisor",
        "the supervisor must notify for a command that does not exist"
    );
}

/// A cancelled job notifies when, and only when, the config file asks for it.
///
/// `cancelled` is not in the default list, because the person who cancelled the
/// job already knows. A person who WANTS that message adds the name, and the
/// message must then arrive. Such a job never started, so it has no supervisor
/// and the COORDINATOR runs its hook.
#[test]
fn a_cancelled_job_runs_the_stop_hook_when_the_config_file_asks_for_it() {
    let h = Harness::with_default_config("hookcancel");
    let mark = h.root.join("hook.txt");
    // One core of budget, and one job that takes it. The next job then stays in
    // the queue, where `qex cancel` can take it out before it ever runs.
    h.write_config(&format!(
        "[budget]\ncpu = \"1\"\nmem = \"512MB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [hooks]\non_stop_states = [\"cancelled\"]\n\
         on_stop = [\"sh\", \"-c\", \
         \"echo \\\"$QEX_STATE $QEX_JOB_NAME\\\" >> {}\"]\n",
        mark.display()
    ));

    let occupier = h.submit(&[
        "submit", "--cpu", "1", "--mem", "128MB", "--", "sleep", "300",
    ]);
    h.until("the first job operates", Duration::from_secs(45), || {
        h.has_started(&occupier)
    });

    let waiting = h.submit(&[
        "submit", "--name", "later", "--cpu", "1", "--mem", "128MB", "--", "true",
    ]);
    assert_eq!(
        h.state_of(&waiting),
        "queued",
        "the budget of one core must hold this job in the queue"
    );

    h.qex(&["cancel", &waiting]);
    h.until("the job is cancelled", Duration::from_secs(30), || {
        h.state_of(&waiting) == "cancelled"
    });
    h.until(
        "the stop hook wrote its line",
        Duration::from_secs(30),
        || !h.hook_lines().is_empty(),
    );
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        h.hook_lines(),
        vec!["cancelled later".to_string()],
        "a cancelled job must give the message that the config file asks for"
    );
    assert_eq!(
        h.hook_origin(&waiting),
        "coordinator",
        "a job that never ran has no supervisor, so the coordinator notifies"
    );

    // The job that holds the budget stops as `killed`, which this filter does
    // not select, so the count above stands.
    h.qex(&["kill", &occupier, "--grace", "1s"]);
}

/// `qex logs --hook` must SAY that qex ran no hook for this job.
///
/// An empty answer looks like a hook that ran and wrote nothing. The reader
/// then tests the hook itself, and the true cause — that no hook is configured,
/// or that this state is not in the filter — is somewhere else.
#[test]
fn qex_logs_hook_says_when_there_was_no_stop_hook() {
    let h = Harness::with_default_config("hooknone");
    let id = h.submit(&["submit", "--", "true"]);
    h.ok(&["wait", &id]);

    let out = h.qex(&["logs", &id, "--hook"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "this is not a fault of the user"
    );
    let notice = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        notice.contains("ran no stop hook"),
        "the reader must learn that there was no hook: {notice}"
    );
    // The message goes to stderr. `qex logs $ID --hook > file` must give a
    // file that holds the log and no sentence of qex.
    assert!(
        String::from_utf8_lossy(&out.stdout).is_empty(),
        "the standard output must hold the log only"
    );
}

/// `--each-line` must give one job for each line, all in one group, and the
/// group id must reach stdout alone.
///
/// The group id is the handle to the whole fan-out. Without it on stdout,
/// `GROUP=$(qex submit --each-line ...)` gives nothing and the user must find
/// the jobs again by hand.
#[test]
fn each_line_gives_one_job_for_each_line_in_one_group() {
    let h = Harness::with_default_config("eachline");

    let input = h.root.join("inputs.txt");
    std::fs::write(&input, "alpha\nbeta\ngamma\n").unwrap();

    let ids = h.root.join("ids.env");
    let group = h.ok(&[
        "submit",
        "--each-line",
        input.to_str().unwrap(),
        "--id-file",
        ids.to_str().unwrap(),
        "--",
        "echo",
        "value={}",
    ]);
    assert_eq!(
        group.lines().count(),
        1,
        "stdout must hold the group id only: {group}"
    );
    assert!(
        group.parse::<uuid::Uuid>().is_ok(),
        "stdout must hold a group id, and it holds: {group}"
    );

    // Three jobs, and every one of them in this group.
    let text = h.ok(&["list", "--group", &group, "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(jobs.len(), 3, "three lines must give three jobs: {text}");
    assert_eq!(h.list_json().len(), 3, "no other job may exist");

    // The group takes its NAME as well as its id, which is the rule that a
    // group id follows everywhere in qex. The name of this group comes from
    // the file, so it is `inputs`. Without `spec.group_name`, the id half
    // still operates and this half gives nothing.
    for job in &jobs {
        assert_eq!(
            job["group_name"].as_str(),
            Some("inputs"),
            "each job must carry the name of its group: {job}"
        );
    }
    let text = h.ok(&["list", "--group", "inputs", "--json"]);
    let by_name: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(
        by_name.len(),
        3,
        "`qex list --group inputs` must give the 3 jobs: {text}"
    );
    // The start of the id names the group as well.
    let text = h.ok(&["list", "--group", &group[..8], "--json"]);
    let by_prefix: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(by_prefix.len(), 3, "the start of the group id must name it");

    // Each job must have received its own line, and no other line.
    let mut seen: Vec<String> = Vec::new();
    for job in &jobs {
        let id = job["id"].as_str().unwrap();
        h.ok(&["wait", id, "--timeout", "60s"]);
        assert_eq!(h.state_of(id), "completed");
        seen.push(h.ok(&["logs", id, "--stdout"]).trim().to_string());
    }
    seen.sort();
    assert_eq!(seen, vec!["value=alpha", "value=beta", "value=gamma"]);

    // The name must hold the position and the line, so `qex list` is readable.
    let names: Vec<String> = jobs
        .iter()
        .map(|j| j["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&"echo-1-alpha".to_string()),
        "got the names: {names:?}"
    );

    // The id file must hold the group and every job, for a later command.
    let text = std::fs::read_to_string(&ids).unwrap();
    assert!(text.contains(&format!("group={group}")), "got: {text}");
    assert_eq!(text.lines().count(), 4, "group and three jobs: {text}");
}

/// A line of an input file is DATA. It must become exactly one argument, and no
/// shell may read it.
///
/// This is the security property of `--each-line`. An input file frequently
/// comes from a directory listing, a database or another program, and a line
/// that became a command would give that source the machine of the user.
#[test]
fn a_line_with_a_space_a_quotation_mark_and_a_semicolon_is_one_argument() {
    let h = Harness::with_default_config("eachsafe");

    // The mark that a shell would leave. `$HOME` is a real directory, so a
    // shell that read this line would make the file.
    let mark = h.root.join("SHELL-RAN");
    let line = format!(
        "a b\"; touch {}; echo $HOME `id` $(id) 'x'",
        mark.to_str().unwrap()
    );

    let input = h.root.join("inputs.txt");
    std::fs::write(&input, format!("{line}\n")).unwrap();

    let group = h.ok(&[
        "submit",
        "--each-line",
        input.to_str().unwrap(),
        "--",
        "printf",
        "[%s]",
        "{}",
    ]);

    let text = h.ok(&["list", "--group", &group, "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(jobs.len(), 1);
    let id = jobs[0]["id"].as_str().unwrap();
    h.ok(&["wait", id, "--timeout", "60s"]);

    // `printf "[%s]"` writes its arguments one after the other. One argument
    // thus gives ONE pair of brackets. Two arguments would give two pairs.
    let out = h.ok(&["logs", id, "--stdout"]);
    assert_eq!(
        out.trim(),
        format!("[{line}]"),
        "the line must become exactly one argument"
    );

    // The command of the record must hold the line as one element, so nothing
    // divided it on the way to the job either.
    let status = h.status_json(id);
    let command: Vec<String> = status["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(command, vec!["printf", "[%s]", &line]);

    // Nothing ran a shell.
    assert!(
        !mark.exists(),
        "a shell read the line and made the file {}",
        mark.display()
    );
}

/// A line of an input file must never put a terminal control sequence into a
/// job name.
///
/// A name goes to the terminal of the user at the submission and in every later
/// `qex list`. `{}` can go in the program position, which the documentation
/// advertises, so BOTH parts of the name can come from the file. An escape
/// sequence there changes the colour, moves the cursor, empties the screen or
/// writes the title of the window, and rows of `qex list` go away in front of
/// the reader.
#[test]
fn a_line_never_puts_a_control_character_into_a_job_name() {
    let h = Harness::with_default_config("eachname");

    let line = "\u{1b}[31mBOOM\u{1b}[0m \u{1b}[2J \u{1b}]0;title\u{7}";
    let input = h.root.join("inputs.txt");
    std::fs::write(&input, format!("{line}\nsecond\n")).unwrap();

    // `{}` in the program position as well, so the line also reaches the BASE
    // of the name. `echo` is the program, and the line is its argument.
    let out = h.qex(&[
        "submit",
        "--each-line",
        input.to_str().unwrap(),
        "--",
        "echo",
        "{}",
    ]);
    assert!(out.status.success());
    let group = String::from_utf8_lossy(&out.stdout).trim().to_string();

    // Nothing that qex wrote about the submission may hold a control character.
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !err.contains('\u{1b}') && !err.contains('\u{7}'),
        "the submission output holds a control character: {err:?}"
    );

    // The name in the record, which `qex list` writes, must be clean.
    let text = h.ok(&["list", "--group", &group, "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(jobs.len(), 2);
    for job in &jobs {
        let name = job["name"].as_str().unwrap();
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'),
            "a job name holds a character that must not reach a terminal: {name:?}"
        );
    }

    // `qex list` itself must write no control character.
    let listing = h.ok(&["list"]);
    assert!(
        !listing.contains('\u{1b}') && !listing.contains('\u{7}'),
        "`qex list` holds a control character: {listing:?}"
    );

    // The COMMAND that the job received still holds the line exactly. qex
    // cleans the name, and it shows the command with an escape for a control
    // byte. The record on the disk is the data that the job received.
    let id = jobs[0]["id"].as_str().unwrap();
    let spec: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(h.root.join("state/qex/jobs").join(id).join("spec.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        spec["command"][1].as_str().unwrap(),
        line,
        "the job must receive the line as the file holds it"
    );
    let shown = h.status_json(id);
    let arg = shown["command"][1].as_str().unwrap();
    assert!(
        arg.contains("\\x1b") && !arg.contains('\u{1b}'),
        "the shown command must escape the control byte: {arg:?}"
    );
}

/// The BASE of the name must be clean as well, and not the part from the line
/// only.
///
/// `{}` in the program position is a use that the documentation advertises. The
/// base is then the program name of the FIRST line, and it is file data. The
/// program name has no `/` here, so the whole line becomes the base: this is
/// the path that the part from the line does not cover.
///
/// The jobs do not start, because no such program exists. That is correct for
/// this test: the name reaches `qex list` and the terminal of the user whatever
/// the job does after it.
#[test]
fn a_placeholder_in_the_program_position_gives_a_clean_name() {
    let h = Harness::with_default_config("eachprog");

    let input = h.root.join("inputs.txt");
    std::fs::write(&input, "\u{1b}[31mBOOM\u{1b}[0m x\nsecond\n").unwrap();

    let out = h.qex(&["submit", "--each-line", input.to_str().unwrap(), "--", "{}"]);
    assert!(out.status.success());

    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !err.contains('\u{1b}'),
        "the base of the name holds an escape: {err:?}"
    );

    let names: Vec<String> = h
        .list_json()
        .iter()
        .map(|j| j["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names.len(), 2);
    for name in &names {
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'),
            "the base of the name holds a character that must not reach a terminal: {name:?}"
        );
    }

    // Both jobs take the base from the FIRST line, so the escape of that line
    // would reach the name of the second job as well.
    assert!(
        !h.ok(&["list"]).contains('\u{1b}'),
        "`qex list` holds an escape"
    );
}

/// An input with a fault must submit NO job.
///
/// A fan-out that stops in the middle leaves a part of the work in the queue
/// and a part with no job. The user then holds a group that is not the file.
#[test]
fn an_input_with_a_bad_line_submits_no_job_at_all() {
    let h = Harness::with_default_config("eachbad");

    // Ninety correct lines, and one line that is not UTF-8.
    let input = h.root.join("inputs.txt");
    let mut bytes: Vec<u8> = Vec::new();
    for i in 1..=90 {
        bytes.extend_from_slice(format!("line{i}\n").as_bytes());
    }
    bytes.extend_from_slice(b"bad \xff line\n");
    std::fs::write(&input, &bytes).unwrap();

    let out = h.qex(&[
        "submit",
        "--each-line",
        input.to_str().unwrap(),
        "--",
        "echo",
        "{}",
    ]);
    assert!(!out.status.success(), "a bad line must give an error");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("line 91"), "the error must say where: {err}");

    assert!(
        h.list_json().is_empty(),
        "the 90 correct lines must not become jobs"
    );

    // A command with no place for the line is the same rule: no job at all.
    let out = h.qex(&[
        "submit",
        "--each-line",
        input.to_str().unwrap(),
        "--",
        "echo",
        "the same",
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("holds no `{}`"), "got: {err}");
    assert!(h.list_json().is_empty());
}

/// An empty line and a comment line must give no job, and qex must SAY how many
/// lines it passed over.
///
/// A line that a user expected to run, and that qex passed over in silence, is
/// the quiet wrong answer that this project exists to prevent.
#[test]
fn an_empty_line_and_a_comment_line_give_no_job_and_qex_reports_them() {
    let h = Harness::with_default_config("eachskip");

    let input = h.root.join("inputs.txt");
    // No final newline on the last line, and one CRLF ending.
    std::fs::write(&input, "# a note\n\nalpha\r\n   \n#\nbeta").unwrap();

    let out = h.qex(&[
        "submit",
        "--each-line",
        input.to_str().unwrap(),
        "--",
        "echo",
        "{}",
    ]);
    assert!(out.status.success());
    let group = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr);

    assert!(
        err.contains("2 empty lines") && err.contains("2 comment lines"),
        "qex must say what it passed over: {err}"
    );

    let text = h.ok(&["list", "--group", &group, "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(jobs.len(), 2, "two lines give a job: {text}");

    let mut seen: Vec<String> = Vec::new();
    for job in &jobs {
        let id = job["id"].as_str().unwrap();
        h.ok(&["wait", id, "--timeout", "60s"]);
        seen.push(h.ok(&["logs", id, "--stdout"]).trim().to_string());
    }
    seen.sort();
    // The CRLF ending must not reach the job, and the last line counts although
    // the file has no final newline.
    assert_eq!(seen, vec!["alpha", "beta"]);
}

/// The name `-` must read the lines from another program.
///
/// `qex submit` reads nothing else from standard input, so this name is free.
/// A fan-out over the output of `ls` or of a query is the common use.
#[test]
fn each_line_reads_the_lines_from_standard_input() {
    let h = Harness::with_default_config("eachstdin");

    let out = h.qex_stdin(
        &["submit", "--each-line", "-", "--", "echo", "{}"],
        "one\ntwo\n",
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let group = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(group.parse::<uuid::Uuid>().is_ok(), "got: {group}");

    let text = h.ok(&["list", "--group", &group, "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(jobs.len(), 2);
}

/// Every option of the submission must reach EVERY job of the fan-out.
///
/// The documentation promises this, and each value takes a different path to
/// the job. `--needs` is the dangerous one: qex changes the names into ids one
/// time, and then copies them into each job. A fan-out that lost that copy
/// would start immediately, in front of the build that it needs, and it would
/// give no error at all.
///
/// `learn_key` is the other one. It is not an option; it holds the TEMPLATE, so
/// the whole fan-out makes one measurement record. A fan-out that lost it makes
/// one record for every line, and no later job can read any of them.
///
/// Measured by hand-deletion: with `spec.needs`/`spec.after`, `learn_key`,
/// `nice`, `no_limit_env_hints` or `max_queue_time` removed from
/// `submit_each_line`, the whole suite stayed green before this test existed.
#[test]
fn every_option_of_a_fan_out_reaches_every_job() {
    let h = Harness::with_default_config("eachline-options");

    // A job that the fan-out must wait for. It never runs, so the fan-out
    // stays in the queue and the test reads the records with no race.
    let gate = h.submit(&["submit", "--name", "gate", "--", "sleep", "300"]);

    let input = h.root.join("inputs.txt");
    std::fs::write(&input, "alpha\nbeta\n").unwrap();

    let work = h.root.join("work");
    std::fs::create_dir_all(&work).unwrap();

    let out = h.qex(&[
        "submit",
        "--each-line",
        input.to_str().unwrap(),
        "--needs",
        "gate",
        "--max-queue-time",
        "20m",
        "--nice",
        "7",
        "--cwd",
        work.to_str().unwrap(),
        "--timeout",
        "99s",
        "--priority",
        "5",
        "--env",
        "FOO=bar",
        "--lock",
        "db",
        "--retries",
        "2",
        // Both halves of the claim, because qex writes the claim into the
        // environment of the job only when the USER chose both. Without them,
        // the test of `--no-limit-env-hints` below proves nothing.
        "--cpu",
        "1",
        "--mem",
        "256MB",
        "--no-limit-env-hints",
        "--tag",
        "batch",
        "--",
        "echo",
        "{}",
    ]);
    assert!(
        out.status.success(),
        "the fan-out must operate: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let group = String::from_utf8_lossy(&out.stdout).trim().to_string();

    let jobs: Vec<serde_json::Value> = h
        .list_json()
        .into_iter()
        .filter(|j| j["group"].as_str() == Some(group.as_str()))
        .collect();
    // ANTI-VACUITY: the loop below proves nothing when the group holds no job.
    assert_eq!(jobs.len(), 2, "the group must hold the 2 jobs of the file");

    let template = vec!["echo".to_string(), "{}".to_string()];
    for job in &jobs {
        let id = job["id"].as_str().unwrap();
        let text = std::fs::read_to_string(h.job_dir(id).join("spec.json")).unwrap();
        let spec: serde_json::Value = serde_json::from_str(&text).unwrap();

        let needs: Vec<String> = spec["needs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            needs,
            vec![gate.clone()],
            "the job {id} must wait for the gate"
        );
        assert_eq!(spec["max_queue_time"], serde_json::json!(1200), "job {id}");
        assert_eq!(spec["nice"], serde_json::json!(7), "job {id}");
        assert_eq!(
            spec["tags"],
            serde_json::json!(["batch"]),
            "the tag must reach the job {id}"
        );
        // Measured by hand-deletion: each of the six below could go away from
        // `submit_each_line` with the whole suite green. A fan-out then ran in
        // the wrong directory, with no time limit, at priority 0, with no
        // variable of the user, with no lock and with no retry.
        assert_eq!(
            spec["cwd"].as_str(),
            work.canonicalize().unwrap().to_str(),
            "the job {id} must run in the directory that the user gave"
        );
        assert_eq!(spec["timeout"], serde_json::json!(99), "job {id}");
        assert_eq!(spec["priority"], serde_json::json!(5), "job {id}");
        assert_eq!(spec["env"]["FOO"], serde_json::json!("bar"), "job {id}");
        assert_eq!(spec["locks"], serde_json::json!(["db"]), "job {id}");
        assert_eq!(spec["retries"], serde_json::json!(2), "job {id}");
        // The template, and NOT the command of the line. This is what makes
        // the whole fan-out give one measurement record.
        assert_eq!(
            spec["learn_key"],
            serde_json::json!(template),
            "the job {id} must measure against the template"
        );
        assert_ne!(
            spec["learn_key"], spec["command"],
            "the command of the line must not become the key of the record"
        );
        // `--no-limit-env-hints` says: do not write the claim into the
        // environment of the job.
        let env = spec["env"].as_object().unwrap();
        assert!(
            !env.contains_key("QEX_CPU") && !env.contains_key("QEX_MEM"),
            "`--no-limit-env-hints` must reach the job {id}: {env:?}"
        );
    }

    // ANTI-VACUITY for `--no-limit-env-hints`. The same fan-out WITHOUT that
    // option must write the claim, and the test above then measures the
    // option and not the absence of the claim.
    let out = h.qex(&[
        "submit",
        "--each-line",
        input.to_str().unwrap(),
        "--needs",
        "gate",
        "--cpu",
        "1",
        "--mem",
        "256MB",
        "--",
        "echo",
        "{}",
    ]);
    assert!(out.status.success());
    let other = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let with_hints: Vec<serde_json::Value> = h
        .list_json()
        .into_iter()
        .filter(|j| j["group"].as_str() == Some(other.as_str()))
        .collect();
    assert_eq!(with_hints.len(), 2);
    for job in &with_hints {
        let id = job["id"].as_str().unwrap();
        let text = std::fs::read_to_string(h.job_dir(id).join("spec.json")).unwrap();
        let spec: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            spec["env"]["QEX_CPU"],
            serde_json::json!("1"),
            "with no `--no-limit-env-hints`, the job {id} must hear its claim"
        );
    }

    // The two jobs must still hold their OWN line.
    let mut commands: Vec<String> = jobs
        .iter()
        .map(|j| {
            let id = j["id"].as_str().unwrap();
            let text = std::fs::read_to_string(h.job_dir(id).join("spec.json")).unwrap();
            let spec: serde_json::Value = serde_json::from_str(&text).unwrap();
            spec["command"][1].as_str().unwrap().to_string()
        })
        .collect();
    commands.sort();
    assert_eq!(commands, vec!["alpha".to_string(), "beta".to_string()]);

    h.qex(&["kill", &gate]);
}

/// A fan-out must refuse the options that hold a meaning for ONE job only.
///
/// `--dedupe-key` is the dangerous one. A key holds one job, so every job of
/// the fan-out would carry the same key: the coordinator would start the first
/// line, answer every other line with the id of that job, and give exit code 0.
/// A fan-out of 100 lines would then give 1 job and NO error at all.
#[test]
fn a_fan_out_refuses_the_options_that_hold_a_meaning_for_one_job() {
    let h = Harness::with_default_config("eachline-refuse");

    let input = h.root.join("inputs.txt");
    std::fs::write(&input, "alpha\nbeta\ngamma\n").unwrap();
    let path = input.to_str().unwrap().to_string();

    for (option, value, must_say) in [
        ("--dedupe-key", Some("nightly"), "A key holds one job"),
        ("--dedupe-window", Some("1h"), "A key holds one job"),
        ("--json", None, "--id-file"),
    ] {
        let mut args: Vec<&str> = vec!["submit", "--each-line", &path];
        args.push(option);
        if let Some(v) = value {
            args.push(v);
        }
        args.extend_from_slice(&["--", "echo", "{}"]);
        let out = h.qex(&args);

        assert!(
            !out.status.success(),
            "`{option}` must not submit a fan-out"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(option),
            "the error must name the option `{option}`: {err}"
        );
        assert!(
            err.contains(must_say),
            "the error must say why, and what to do: {err}"
        );
        // The message must WRAP. A single-line literal with the indentation of
        // the source baked into it gives a run of 13 spaces where each line
        // break belongs, and `contains` does not see that at all. A message
        // indents an example by 4, so 6 separates the two.
        assert!(
            !err.contains("      "),
            "the message holds the indentation of the source in place of a              line break: {err:?}"
        );
        // ANTI-VACUITY: the refusal must come BEFORE the coordinator, so no
        // job of the fan-out reaches the queue. A message with jobs behind it
        // is the fault that this test looks for.
        assert!(
            h.list_json().is_empty(),
            "`{option}` refused the fan-out and still left a job in the queue"
        );
    }

    // ANTI-VACUITY: the same command with no refused option MUST submit. A
    // typing error in `--each-line` or in the input file would make every
    // assertion above pass for the wrong reason.
    let out = h.qex(&["submit", "--each-line", &path, "--", "echo", "{}"]);
    assert!(
        out.status.success(),
        "the fan-out without those options must operate: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(h.list_json().len(), 3, "3 lines must give 3 jobs");
}

/// The limit must stop a fan-out that would fill the state directory, and the
/// message must say how to raise it. It must never wait for a key: the main
/// user of qex is an agent, and an agent cannot answer a prompt.
#[test]
fn a_fan_out_above_the_limit_is_refused_with_no_prompt() {
    let h = Harness::with_default_config("eachlimit");

    let input = h.root.join("inputs.txt");
    let text: String = (1..=20).map(|i| format!("line{i}\n")).collect();
    std::fs::write(&input, text).unwrap();

    let out = h.qex(&[
        "submit",
        "--each-line",
        input.to_str().unwrap(),
        "--max-jobs",
        "5",
        "--",
        "echo",
        "{}",
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--max-jobs 20"), "got: {err}");
    assert!(h.list_json().is_empty(), "no job may start");
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
/// in front of. The test also compares the code against `qex wait`, which
/// answers the same question, so the two cannot drift.
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

        let wait = h.qex(&["wait", &id]);
        assert_eq!(
            out.status.code(),
            wait.status.code(),
            "`qex run` and `qex wait` gave two codes for one job"
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
        // The file is correct each time it is complete, so qex must report no
        // FAULT of it. A note that qex WAITS for the writer is not a fault:
        // qex says that while a writer holds the file, so that a person who
        // reads `qex info` learns why the new values did not arrive yet.
        let reported = info["config_error"].as_str().unwrap_or_default();
        assert!(
            reported.is_empty() || reported.contains("waits for it"),
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
/// The guard was then a TIME between two LOOKS, and that rests on the rate of
/// the looks: this test met the same fault again on a loaded machine and on
/// macOS in CI. The guard is now the AGE OF THE FILE, which no rate of looks
/// can miss. This test holds the words and the guard together.
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
        let (root, peers, stop) = (h.root.clone(), h.peers_dir.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let mut cmd = Command::new(env!("CARGO_BIN_EXE_qex"));
                cmd.args(["submit", "--", "true"]);
                isolate(&mut cmd, &root, &peers);
                cmd.output().ok();
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
        let child = h
            .command(&args)
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
            let child = h
                .command(&["info"])
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
    // A later qex refuses these names at submission. Plant them in a record
    // that an earlier qex could have written. The safe form comes from the
    // name that the record holds, so the rule reaches every record at once.
    let pairs: Vec<(String, String)> = [
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
    ]
    .into_iter()
    .map(|(n, s)| (n.to_string(), s.to_string()))
    .chain(std::iter::once(("y".repeat(200), "y".repeat(128))))
    .collect();
    let mut planted = Vec::new();
    for (i, (name, safe)) in pairs.iter().enumerate() {
        let id = h.submit(&["submit", "--name", &format!("plant{i}"), "--", "true"]);
        planted.push((id, name.clone(), safe.clone()));
    }
    h.until("the planted jobs stop", Duration::from_secs(45), || {
        planted
            .iter()
            .all(|(id, _, _)| h.state_of(id) == "completed")
    });
    h.stop_coordinator();
    for (id, name, _) in &planted {
        h.plant_stored_name(id, name);
    }
    for (id, name, safe) in &planted {
        // qex shows the safe form. `every_output_shows_the_safe_name` holds
        // the other half: the record on the disk keeps the name that the user
        // gave.
        assert_eq!(
            h.status_json(id)["name"].as_str(),
            Some(safe.as_str()),
            "qex must show {safe:?} for the name {name:?}"
        );
        // The list holds the SAFE form, and never the name itself.
        let all = h.ok(&["__complete", "ids"]);
        assert!(
            all.lines().any(|l| l == safe.as_str()),
            "the list must offer {safe:?} for the name {name:?}: {all}"
        );
        if safe != name {
            assert!(
                !all.lines().any(|l| l == name.as_str()),
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

    let all = h.ok(&["__complete", "ids"]);
    assert!(
        all.lines().all(|l| l.chars().count() <= 128),
        "no candidate may be longer than 128 characters"
    );

    // TWO NAMES THAT GIVE ONE SAFE FORM fall into the answer that qex already
    // has for a name that names more than one job. There is no second error.
    //
    // `a b` and `a_b` both give `a_b`. `a-b` does NOT collide with them,
    // because `-` is inside the set and it is not replaced.
    let spaced = h.submit(&["submit", "--name", "plant-space", "--", "true"]);
    h.submit(&["submit", "--name=x_y", "--", "true"]);
    h.until("the collision jobs stop", Duration::from_secs(45), || {
        h.state_of(&spaced) == "completed"
    });
    h.stop_coordinator();
    h.plant_stored_name(&spaced, "x y");
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

    // Each `stop_coordinator` above stopped the supervisor of the holder and
    // left its job process. The next coordinator found the job without its
    // supervisor, stopped the process, and made the job `failed`, so there is
    // no job left to kill.
    assert_eq!(h.state_of(&running), "failed");
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
    let id = h.submit(&["submit", "--name", "deploy-plant", "--", "true"]);
    // A name that holds an ESC byte. This is the one that hurts a terminal.
    // An earlier qex stored it. A later qex refuses it at submission.
    let esc = "esc\u{1b}[2Jbad";
    let esc_id = h.submit(&["submit", "--name", "esc-plant", "--", "true"]);
    h.until("both jobs stop", Duration::from_secs(45), || {
        h.state_of(&id) == "completed" && h.state_of(&esc_id) == "completed"
    });
    h.stop_coordinator();
    h.plant_stored_name(&id, stored);
    h.plant_stored_name(&esc_id, esc);

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
        "--name",
        "esc_2Jbad",
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
        "--name",
        "esc_2Jbad-broken",
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
    let gone = h.submit(&["submit", "--name", "esc_2Jbad-gone", "--", "true"]);
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
        "--name",
        "esc_2Jbad-wait",
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

/// A name that a person chose must be in the set. The error must name the
/// cause and the remedy, and it must not write an ESC byte to the terminal.
#[test]
fn a_chosen_name_outside_the_set_is_refused() {
    let h = Harness::with_default_config("refusename");
    for name in ["deploy prod", "-version", &"y".repeat(129), "esc\u{1b}[2J"] {
        // `--name=VALUE` so a name that starts with `-` is not an option.
        let out = h.qex(&["submit", &format!("--name={name}"), "--", "true"]);
        assert!(!out.status.success(), "qex accepted the name {name:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("not a name that qex accepts"),
            "the error must say what happened: {err}"
        );
        assert!(
            err.contains("build") || err.contains("test"),
            "the error must say what to write: {err}"
        );
        assert!(
            !out.stderr.windows(4).any(|w| w == b"\x1b[2J"),
            "the error wrote an ESC byte for the name {name:?}"
        );
    }
    // A name in the set starts a job.
    let id = h.submit(&["submit", "--name", "build", "--", "true"]);
    assert_eq!(h.status_json(&id)["name"].as_str(), Some("build"));
}

/// A name that qex takes from the program must become the safe form. The user
/// did not select it, so the job still starts.
#[test]
fn a_derived_name_outside_the_set_becomes_the_safe_form() {
    let h = Harness::with_default_config("derivedname");
    let script = h.root.join("my build.sh");
    std::fs::write(&script, "#!/bin/sh\ntrue\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&script).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&script, p).unwrap();
    }
    let id = h.submit(&["submit", "--", script.to_str().unwrap()]);
    assert_eq!(
        h.status_json(&id)["name"].as_str(),
        Some("my_build.sh"),
        "the derived name must be the safe form of the program"
    );
}

/// A tag, a lock, the program and the environment reach a reader in a form
/// that cannot move the cursor.
#[test]
fn a_tag_a_lock_a_command_and_the_environment_are_safe_on_the_page() {
    let h = Harness::with_default_config("safevals");
    let esc = "t\u{1b}[2Jag";
    let lock = "lk\u{1b}[2J";
    let id = h.submit(&[
        "submit",
        "--name",
        "vals",
        "--tag",
        esc,
        "--lock",
        lock,
        "--env",
        "K=\u{1b}[2Jx",
        "--env-capture",
        "none",
        "--",
        "true",
        "a\u{1b}[2Jb",
    ]);
    h.until("the job stops", Duration::from_secs(45), || {
        h.state_of(&id) == "completed"
    });

    let status = h.ok(&["status", &id, "--show-env"]);
    assert!(
        !status.contains('\u{1b}'),
        "`qex status` wrote an ESC byte: {status:?}"
    );
    assert!(
        status.contains("t_2Jag") || status.contains("t_ag"),
        "the tag must reach the reader in its safe form: {status}"
    );
    assert!(
        status.contains("lk_2J") || status.contains("lk_"),
        "the lock must reach the reader in its safe form: {status}"
    );
    assert!(
        status.contains("\\x1b"),
        "the environment must show the ESC byte as an escape: {status}"
    );

    let json = h.status_json(&id);
    assert_eq!(json["tags"][0].as_str(), Some("t_2Jag"));
    assert_eq!(json["locks"][0].as_str(), Some("lk_2J"));
    let cmd = json["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        cmd.iter().any(|a| a.contains("\\x1b")),
        "the command must show the ESC byte as an escape: {cmd:?}"
    );

    let listed = h.ok(&["list"]);
    assert!(!listed.contains('\u{1b}'), "`qex list` wrote an ESC byte");
}

/// A lock word that `qex status` shows must find the lock that the job holds.
#[test]
fn a_safe_lock_word_pauses_and_resumes_the_stored_lock() {
    let h = Harness::with_default_config("safelock");
    let stored = "lk\u{1b}[2J";
    let id = h.submit(&[
        "submit", "--name", "holder", "--lock", stored, "--", "sleep", "30",
    ]);
    h.until("the holder runs", Duration::from_secs(45), || {
        h.state_of(&id) == "running"
    });
    let shown = h.status_json(&id)["locks"][0].as_str().unwrap().to_string();
    assert_eq!(shown, "lk_2J");

    h.ok(&["pause", "lock", &shown]);
    let waiter = h.submit(&["submit", "--name", "waiter", "--lock", stored, "--", "true"]);
    h.until("the waiter sees the pause", Duration::from_secs(45), || {
        h.status_json(&waiter)["blocked_reason"]
            .as_str()
            .is_some_and(|r| r.contains("person holds"))
    });
    h.ok(&["resume", "lock", &shown]);
    h.until(
        "the waiter starts after the resume",
        Duration::from_secs(45),
        || h.state_of(&waiter) == "completed",
    );
    h.stop(&id);
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

    // The names that an attacker would choose. A later qex refuses them at
    // submission. Plant them in records that an earlier qex could have written.
    let bait = h.root.join("BAIT");
    let bait_id = h.submit(&["submit", "--name", "bait-plant", "--", "true"]);
    let two_id = h.submit(&["submit", "--name", "two-plant", "--", "true"]);
    h.until("the planted jobs stop", Duration::from_secs(45), || {
        h.state_of(&bait_id) == "completed" && h.state_of(&two_id) == "completed"
    });
    h.stop_coordinator();
    h.plant_stored_name(&bait_id, &format!("bait$(touch {})", bait.display()));
    h.plant_stored_name(&two_id, "two words");

    // The pair for `kill` and `cancel` comes AFTER the stop above. A job that
    // loses its supervisor in that stop is `failed` when the next coordinator
    // recovers it, and the candidates below need a job that operates and a job
    // that waits.
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
///
/// `--nice 0` needs a coordinator at nice 0. The kernel refuses a value below
/// the value of the process that asks, and qex does not ask for privilege. A
/// queue that starts this suite gives the coordinator the nice value of the
/// job. The step then sees that value, expects 0, and fails as though the
/// code is wrong.
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

    // `getpriority` returns -1 as a value or as a fault. The gate only skips
    // a known value above 0, so a fault is treated as "run".
    let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    if nice > 0 {
        eprintln!(
            "`--nice 0` did not run: this process is at nice {nice}, and qex cannot \
             lower a nice value, so that step needs a coordinator at nice 0"
        );
        return;
    }

    // A job that asks not to give way says so.
    let id = h.submit(&["submit", "--nice", "0", "--", "sh", "-c", "ps -o ni= -p $$"]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(out.trim(), "0", "`--nice 0` must reach the job: {out}");
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
///
/// THIS TEST USES `env_capture = "minimal"`, and the reason is not secrecy.
/// With the default `all`, the shell of the developer reaches the job, and an
/// ambient `GOMAXPROCS` or `MAKEFLAGS` defeats every assertion here: qex fills
/// a value that nobody chose, so a value that the shell chose stays and the
/// test reads it. Measured: `GOMAXPROCS=4 cargo test` gave `[][4][]` where the
/// test asks for `[][][]`.
///
/// `cargo mutants` makes this certain and not merely possible. It starts a
/// jobserver and gives the child cargo `MAKEFLAGS=--jobserver-auth=...`, so
/// the UNMUTATED baseline fails and no mutant is ever reached. The same is
/// true for anybody who runs `cargo test` under a `make -j` wrapper.
#[test]
fn a_job_is_told_the_size_of_its_claim() {
    let h = Harness::new(
        "claimenv",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [submit]\nenv_capture = \"minimal\"\n",
    );

    // A JOB THAT MADE NO CLAIM IS TOLD NOTHING.
    //
    // The default claim is one core. A job with no claim that heard "one core"
    // would run sixteen times slower on a machine of sixteen cores, with no
    // error and no warning. A node job would receive a SMALLER heap than it
    // receives with no qex at all: measured on this machine, the default claim
    // of 1805MB gives node a heap of 1353MB, and node 12 takes 2096MB by
    // itself. qex tells a job the claim that SOMEBODY CHOSE, and never the
    // claim that qex invented.
    let id = h.submit(&[
        "submit",
        "--",
        "sh",
        "-c",
        "echo \"[$QEX_CPU][$GOMAXPROCS][$NODE_OPTIONS]\"",
    ]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(
        out.trim(),
        "[][][]",
        "a job that made no claim must be told nothing: {out}"
    );

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

    // HALF A CLAIM IS NOT A CLAIM.
    //
    // With `--mem` and no `--cpu`, the number of cores comes from
    // `[defaults]`, and it is one. A job that heard that number would run
    // single-threaded on a machine of sixteen cores. qex writes the claim only
    // when the user answered BOTH questions.
    let id = h.submit(&[
        "submit",
        "--mem",
        "2GB",
        "--",
        "sh",
        "-c",
        "echo \"[$QEX_CPU][$GOMAXPROCS]\"",
    ]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(
        out.trim(),
        "[][]",
        "`--mem` with no `--cpu` must write nothing: {out}"
    );

    // And the same rule for the other half.
    let id = h.submit(&[
        "submit",
        "--cpu",
        "2",
        "--",
        "sh",
        "-c",
        "echo \"[$QEX_CPU][$GOMAXPROCS]\"",
    ]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(
        out.trim(),
        "[][]",
        "`--cpu` with no `--mem` must write nothing: {out}"
    );

    // A value that somebody chose must stay. `--env` is a decision.
    let id = h.submit(&[
        "submit",
        "--cpu",
        "2",
        "--mem",
        "2GB",
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
        "--mem",
        "2GB",
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
         [resources]\ncpu = 2\nmem = \"2GB\"\n",
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
         [resources]\ncpu = 2\nmem = \"2GB\"\n",
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

    // A stage of a pipeline says the same thing as a job file.
    let pipeline = h.root.join("p.toml");
    std::fs::write(
        &pipeline,
        "[[jobs]]\nname = \"stage-on\"\n\
         command = [\"sh\", \"-c\", \"echo \\\"[$QEX_CPU][$GOMAXPROCS]\\\"\"]\n\
         [jobs.resources]\ncpu = 2\nmem = \"2GB\"\n\n\
         [[jobs]]\nname = \"stage-off\"\n\
         command = [\"sh\", \"-c\", \"echo \\\"[$QEX_CPU][$GOMAXPROCS]\\\"\"]\n\
         no_limit_env_hints = true\n\
         [jobs.resources]\ncpu = 2\nmem = \"2GB\"\n",
    )
    .unwrap();
    let ids_file = h.root.join("stage-ids.json");
    h.ok(&[
        "pipeline",
        pipeline.to_str().unwrap(),
        "--id-file",
        ids_file.to_str().unwrap(),
    ]);
    let ids: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&ids_file).unwrap()).unwrap();
    for (stage, want) in [("stage-on", "[2][2]"), ("stage-off", "[][]")] {
        let id = ids["jobs"][stage].as_str().unwrap().to_string();
        h.ok(&["wait", &id, "--timeout", "60s"]);
        let out = h.ok(&["logs", &id, "--stdout"]);
        assert_eq!(
            out.trim(),
            want,
            "the stage {stage} must give {want}: {out}"
        );
    }

    // `qex run` HAS ITS OWN WIRING, and it needs its own test.
    //
    // `commands::run` builds `SubmitOptions` in a second place, so the option
    // can arrive for `qex submit` and be lost for `qex run`. A mutation that
    // put `false` in that one line passed every other test in this file.
    let (child, id) = h.run_bg(&[
        "--cpu",
        "2",
        "--mem",
        "2GB",
        "--no-limit-env-hints",
        "--",
        "sh",
        "-c",
        "echo \"[$QEX_CPU][$GOMAXPROCS]\"",
    ]);
    wait_run(child, "`qex run` with --no-limit-env-hints");
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(
        out.trim(),
        "[][]",
        "`qex run --no-limit-env-hints` must write nothing: {out}"
    );

    // And `qex run` without the option still receives the claim, or the test
    // above would pass with the feature removed from `qex run` altogether.
    let (child, id) = h.run_bg(&[
        "--cpu",
        "2",
        "--mem",
        "2GB",
        "--",
        "sh",
        "-c",
        "echo \"[$QEX_CPU][$GOMAXPROCS]\"",
    ]);
    wait_run(child, "`qex run` with a claim");
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(
        out.trim(),
        "[2][2]",
        "`qex run` must give the claim to the job: {out}"
    );
}

/// `[claims]` in the config file controls every job of this machine.
///
/// `export_env = false` turns the claim off for all of them, and `also` adds
/// the two variables that qex does not write without a request. Each of those
/// two has a cost, so neither is a default: every JVM writes `Picked up
/// JAVA_TOOL_OPTIONS: ...` to its standard error, and `MAKEFLAGS` makes a
/// build parallel that its author never ran in parallel.
///
/// `env_capture = "minimal"` for the same reason as the test above: a
/// `MAKEFLAGS` in the shell of the developer otherwise reaches the job and
/// defeats the assertion. `cargo mutants` sets exactly that variable.
#[test]
fn the_config_file_controls_the_claim_in_the_environment() {
    let show = [
        "sh",
        "-c",
        "echo \"[$QEX_CPU][$GOMAXPROCS][$JAVA_TOOL_OPTIONS][$MAKEFLAGS]\"",
    ];

    let h = Harness::new(
        "claimsoff",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [submit]\nenv_capture = \"minimal\"\n\
         [claims]\nexport_env = false\n",
    );
    let mut args = vec!["submit", "--cpu", "2", "--mem", "2GB", "--"];
    args.extend_from_slice(&show);
    let id = h.submit(&args);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(
        out.trim(),
        "[][][][]",
        "`export_env = false` must write nothing: {out}"
    );

    // `qex config show` MUST SAY SO. A reader who cannot see this value cannot
    // tell whether a job receives GOMAXPROCS, and the config file is not the
    // answer: a machine with no file still has these values.
    let shown = h.ok(&["config", "show"]);
    assert!(
        shown.contains("claim in job: no; [claims] export_env = false"),
        "`qex config show` must report that the claim is off: {shown}"
    );

    // THE LINE MUST READ EVERY CONDITION THAT THE CODE READS.
    //
    // `[submit] env_capture = "none"` turns the claim off as completely as
    // `export_env = false` does. A line that reported "yes" here was worse than
    // no line at all: the owner of the machine sets the value, asks this
    // command, and is told the opposite of what the job receives.
    let h = Harness::new(
        "claimsnone",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [submit]\nenv_capture = \"none\"\n",
    );
    let shown = h.ok(&["config", "show"]);
    assert!(
        shown.contains("claim in job: no; [submit] env_capture"),
        "`env_capture = none` must report that the claim is off: {shown}"
    );
    let mut args = vec!["submit", "--cpu", "2", "--mem", "2GB", "--"];
    args.extend_from_slice(&show);
    let id = h.submit(&args);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(
        out.trim(),
        "[][][][]",
        "and the job must in fact receive nothing: {out}"
    );

    // The DEFAULT text needs a test too. Without this, a mutation of that one
    // string passes every other assertion here, because the others read the
    // `no` branch and the `also` substring only.
    let h = Harness::new(
        "claimsdefault",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );
    let shown = h.ok(&["config", "show"]);
    assert!(
        shown.contains("claim in job: yes, with --cpu and --mem together"),
        "the default configuration must report that the claim reaches the job: {shown}"
    );

    let h = Harness::new(
        "claimsalso",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [submit]\nenv_capture = \"minimal\"\n\
         [claims]\nalso = [\"java\", \"make\"]\n",
    );
    let mut args = vec!["submit", "--cpu", "2", "--mem", "2GB", "--"];
    args.extend_from_slice(&show);
    let id = h.submit(&args);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert_eq!(
        out.trim(),
        "[2][2][-XX:ActiveProcessorCount=2 -Xmx1536m][-j2]",
        "`also` must add the two variables: {out}"
    );
    let shown = h.ok(&["config", "show"]);
    assert!(
        shown.contains("also java, make"),
        "`qex config show` must name the hints that operate: {shown}"
    );

    // A NAME WITH A SPELLING FAULT MUST STOP THE COMMAND.
    //
    // A silent no-op is the wrong answer for a feature whose purpose is to
    // stop a job from taking more than it claimed: the user reads the config
    // file, believes the JVM is limited, and it is not.
    let h = Harness::new(
        "claimsbad",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [claims]\nalso = [\"jvm\"]\n",
    );
    let out = h.qex(&["submit", "--cpu", "2", "--mem", "2GB", "--", "true"]);
    assert!(
        !out.status.success(),
        "an unknown name must stop the submit"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    // The three parts: what happened, why it matters, what the reader must do.
    assert!(
        err.contains("jvm"),
        "the message must name the value: {err}"
    );
    assert!(
        err.contains("size of the machine"),
        "the message must say why it matters: {err}"
    );
    assert!(
        err.contains("`java`") && err.contains("`make`"),
        "the message must give the remedy: {err}"
    );
}

// ---------------------------------------------------------------------------
// The event stream
// ---------------------------------------------------------------------------

/// Starts `qex events` in this harness, with the given extra arguments.
///
/// EVERY reader in a test has a time limit. A test that reads a stream with no
/// limit hangs for ever when the stream gives nothing, and a test that hangs is
/// worse than a test that fails: it says nothing and it holds the machine.
fn events_reader(h: &Harness, args: &[&str]) -> std::process::Child {
    let mut all = vec!["events"];
    all.extend_from_slice(args);
    h.command(&all)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("qex events did not start")
}

/// Reads the lines of a reader that stopped, as JSON.
fn events_lines(child: std::process::Child) -> Vec<serde_json::Value> {
    let out = child.wait_with_output().expect("the reader did not stop");
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| {
                panic!("the stream must give one JSON object for each line: {e}\nline: {line}")
            })
        })
        .collect()
}

/// The stream must report each change of state, in order, while the job runs.
///
/// This test prevents the fault that the command exists to remove: an agent
/// that drives many jobs asks about each job in a loop. Without the stream, it
/// learns of a result late and it writes a monitor with a timer.
///
/// The reader starts BEFORE the job, so this test also proves that the stream
/// is live and is not a report of the records at the end.
#[test]
fn the_event_stream_reports_each_change_of_state_in_order() {
    let h = Harness::with_default_config("events");
    // The reader stops by itself. See `events_reader`.
    let reader = events_reader(&h, &["--json", "--timeout", "12s"]);

    // Give the reader time to attach. A job that starts first still appears,
    // because the stream begins with the events that the coordinator holds.
    std::thread::sleep(Duration::from_millis(500));

    let id = h.submit(&["submit", "--name", "streamed", "--", "sh", "-c", "sleep 2"]);
    h.ok(&["wait", &id]);

    let lines = events_lines(reader);
    assert!(!lines.is_empty(), "the stream gave no line");
    assert_eq!(
        lines[0]["event"], "stream",
        "the first line must name the coordinator: {:?}",
        lines[0]
    );

    let mine: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["event"] == "job" && l["id"] == id.as_str() && l["change"] == "state")
        .collect();
    let states: Vec<&str> = mine.iter().map(|l| l["state"].as_str().unwrap()).collect();
    assert_eq!(
        states,
        vec!["queued", "starting", "running", "completed"],
        "the stream must give each change of state in order: {states:?}"
    );

    // The numbers must increase, because an agent keeps the last number and
    // continues from it.
    let seqs: Vec<u64> = mine.iter().map(|l| l["seq"].as_u64().unwrap()).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] > w[0]),
        "the numbers must increase: {seqs:?}"
    );

    // Each line carries the whole record, so a reader needs no second command.
    let last = mine.last().unwrap();
    assert_eq!(last["previous"], "running");
    assert_eq!(last["job"]["exit_code"], 0);
    assert_eq!(last["job"]["name"], "streamed");
}

/// The stream must show the SAFE name, in both of its forms.
///
/// A job name is text that another agent chose. `qex events` with no `--json`
/// writes that name to a terminal, so an ESC byte in a name moves the cursor
/// and writes over the lines around it. `qex list` and `qex status` close this
/// already; a stream that does not close it is a hole of the same shape in a
/// command that a person leaves open for hours.
///
/// The JSON form takes the same name. `serde_json` escapes a control byte, so
/// the JSON is safe to PARSE either way; the rule here is that one line of the
/// stream and one line of `qex status --json` must never give two names for
/// one job.
#[test]
fn the_stream_shows_the_safe_name() {
    let h = Harness::with_default_config("eventsname");
    let esc = "esc\u{1b}[2Jbad";
    let id = h.submit(&["submit", "--name", "esc-event", "--", "sleep", "30"]);
    h.until("the job operates", Duration::from_secs(45), || {
        h.state_of(&id) == "running"
    });
    h.stop_coordinator();
    h.plant_stored_name(&id, esc);

    // Read AFTER the plant. The coordinator that starts now holds the stored
    // name, so the stream and `qex status` show one safe form.
    let reader = events_reader(&h, &["--json", "--timeout", "20s"]);
    let plain = events_reader(&h, &["--timeout", "20s"]);
    std::thread::sleep(Duration::from_millis(500));
    // The readers started the next coordinator. That coordinator found the job
    // without its supervisor, stopped it, and made it `failed` — with the name
    // that the plant put in the record. That change is the job event that the
    // readers must see, and `publish_changes` at the end of recovery puts it
    // in the stream.
    h.until("the job is failed", Duration::from_secs(30), || {
        h.state_of(&id) == "failed"
    });

    // The text form goes to a terminal, so test the BYTES.
    let text = plain.wait_with_output().expect("the reader did not stop");
    for stream in [&text.stdout, &text.stderr] {
        assert!(
            !stream.windows(6).any(|w| w == b"esc\x1b[2"),
            "`qex events` wrote the ESC byte of a job name"
        );
    }
    assert!(
        String::from_utf8_lossy(&text.stdout).contains("esc_2Jbad"),
        "`qex events` must show the safe name: {}",
        String::from_utf8_lossy(&text.stdout)
    );

    // The JSON form must hold the same name as `qex status --json`.
    let shown = h.status_json(&id)["name"].as_str().unwrap().to_string();
    let lines = events_lines(reader);
    let mine: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["event"] == "job" && l["id"] == id.as_str())
        .collect();
    assert!(!mine.is_empty(), "the stream gave no event for the job");
    for line in mine {
        assert_eq!(
            line["name"].as_str(),
            Some(shown.as_str()),
            "the stream and `qex status --json` must give one name: {line}"
        );
        assert_eq!(
            line["job"]["name"].as_str(),
            Some(shown.as_str()),
            "the record in the stream must hold the safe name: {line}"
        );
    }
}

/// The stream must show no control byte in the SENTENCES either.
///
/// A sentence that qex writes is not safe because qex wrote it. It carries
/// values that the caller chose: `blocked_reason` names the LOCK that a job
/// waits for, and a lock name is a word from the command line. `expire` then
/// folds that reason into `error`, so the same word reaches the reader a second
/// time on a TERMINAL line — and a terminal line is the one that an agent acts
/// on.
///
/// `safe_name` is the wrong rule for a sentence; it would destroy the prose.
/// `job::printable` takes the bytes that move a cursor and nothing else.
#[test]
fn the_stream_shows_no_control_byte_in_a_sentence() {
    let h = Harness::new(
        "eventsreason",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    let plain = events_reader(&h, &["--timeout", "20s"]);
    let json = events_reader(&h, &["--json", "--timeout", "20s"]);
    std::thread::sleep(Duration::from_millis(500));

    // The lock name holds an ESC byte, and it reaches `blocked_reason` of the
    // job that waits, and then `error` of that job when it gives up waiting.
    let lock = "lk\u{1b}[2Jbad";
    let holder = h.submit(&[
        "submit", "--name", "holder", "--lock", lock, "--", "sleep", "10",
    ]);
    // The holder must HOLD the lock before the waiter arrives. A waiter that
    // arrives first waits for capacity, and the sentence then names no lock.
    h.until("the holder runs", Duration::from_secs(30), || {
        h.state_of(&holder) == "running"
    });
    let waiter = h.submit(&[
        "submit",
        "--name",
        "waiter",
        "--lock",
        lock,
        "--max-queue-time",
        "3s",
        "--",
        "true",
    ]);
    h.qex(&["wait", &waiter, "--timeout", "40s"]);
    h.qex(&["kill", &holder]);

    let text = plain.wait_with_output().expect("the reader did not stop");
    for stream in [&text.stdout, &text.stderr] {
        assert!(
            !stream.windows(5).any(|w| w == b"lk\x1b[2"),
            "`qex events` wrote the ESC byte of a lock name"
        );
    }

    // The JSON form takes the same rule, so a reader of the stream and a reader
    // of `qex status --json` never see two forms of one sentence.
    let lines = events_lines(json);
    let mine: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["event"] == "job" && l["id"] == waiter.as_str())
        .collect();
    assert!(!mine.is_empty(), "the stream gave no event for the job");
    let mut saw_the_lock = false;
    for line in mine {
        for field in ["blocked_reason", "error"] {
            if let Some(sentence) = line["job"][field].as_str() {
                if sentence.contains("lk") {
                    saw_the_lock = true;
                }
                assert!(
                    !sentence.chars().any(|c| c.is_control()),
                    "the field `{field}` holds a control byte: {sentence:?}"
                );
            }
        }
    }
    assert!(
        saw_the_lock,
        "the test proved nothing: no sentence carried the lock name"
    );
}

/// `--since now` must give the NEW events only.
///
/// The coordinator holds the last 512 events. A reader that asks for the live
/// stream and receives that history acts a second time on every result that it
/// handled already — the harm that the numbers exist to stop. Without this
/// test, `Cursor::Now` can start at the oldest event that the coordinator holds
/// and the whole suite stays green.
#[test]
fn a_live_reader_receives_the_new_events_only() {
    let h = Harness::with_default_config("eventsnow");

    // This job stops BEFORE the reader connects, so its events are history.
    let old = h.submit(&["submit", "--name", "before", "--", "true"]);
    h.ok(&["wait", &old]);

    let reader = events_reader(&h, &["--json", "--since", "now", "--timeout", "12s"]);
    std::thread::sleep(Duration::from_millis(500));

    let new = h.submit(&["submit", "--name", "after", "--", "true"]);
    h.ok(&["wait", &new]);

    let lines = events_lines(reader);
    assert!(
        lines.iter().any(|l| l["id"] == new.as_str()),
        "the live stream gave no event for the job that started after it"
    );
    assert!(
        !lines.iter().any(|l| l["id"] == old.as_str()),
        "`--since now` gave the events of a job that stopped before the reader \
         connected: {lines:?}"
    );
}

/// `--count N` must stop the reader after N events.
///
/// A script that wants one result writes `--count 1` and expects the command to
/// give control back. A reader that counts a line that is not an event, or that
/// counts none, either stops early with nothing or never stops at all.
#[test]
fn the_reader_stops_after_the_number_of_events_that_it_asked_for() {
    let h = Harness::with_default_config("eventscount");
    let reader = events_reader(&h, &["--json", "--count", "2", "--timeout", "20s"]);
    std::thread::sleep(Duration::from_millis(500));

    let id = h.submit(&["submit", "--name", "counted", "--", "true"]);
    h.ok(&["wait", &id]);

    let out = reader.wait_with_output().expect("the reader did not stop");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a reader that read its count stops with 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // The `stream` line is not an event, so it does not count. Two events and
    // the header make three lines.
    let counted = lines
        .iter()
        .filter(|l| l["event"] == "job" || l["event"] == "gap")
        .count();
    assert_eq!(
        counted, 2,
        "the reader must stop after two events, and it wrote: {text}"
    );
    assert_eq!(
        lines[0]["event"], "stream",
        "the header must not count as an event: {text}"
    );
}

/// The stream must give an event for EVERY terminal state, and not for the
/// happy path alone.
///
/// This is the fault that a stream must never have. `expired` and `skipped` are
/// the two terminal states that NO SUPERVISOR reports: the scheduler writes
/// them on its own, and nothing else says that they happened. A stream that
/// carries `completed` and `failed` and drops these two leaves an agent waiting
/// for ever for a job that already gave up, and a silent stream is worse than
/// no stream. The same fault was in the stop hooks: `expire` told nobody.
///
/// The first job claims more than the budget and the config file keeps such a
/// job in the queue, so it can never start and it reaches its queue limit. The
/// second job needs the first, so it is skipped.
#[test]
fn the_stream_reports_the_terminal_states_that_no_supervisor_reports() {
    let h = Harness::new(
        "eventsterm",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [queue]\noversized = \"queue\"\n",
    );

    // The reader stops by itself. See `events_reader`.
    let reader = events_reader(&h, &["--json", "--timeout", "25s"]);
    std::thread::sleep(Duration::from_millis(500));

    let first = h.submit(&[
        "submit",
        "--cpu",
        "64",
        "--max-queue-time",
        "3s",
        "--",
        "echo",
        "never",
    ]);
    let second = h.submit(&["submit", "--needs", &first, "--", "echo", "after"]);

    // Both jobs stop without running. Wait for the second, which stops last.
    h.qex(&["wait", &second, "--timeout", "60s"]);

    let lines = events_lines(reader);
    let state_of = |id: &str| -> Vec<String> {
        lines
            .iter()
            .filter(|l| l["event"] == "job" && l["id"] == id && l["change"] == "state")
            .map(|l| l["state"].as_str().unwrap().to_string())
            .collect()
    };

    let one = state_of(&first);
    assert!(
        one.contains(&"expired".to_string()),
        "the stream must report the job that gave up waiting: {one:?}"
    );
    let two = state_of(&second);
    assert!(
        two.contains(&"skipped".to_string()),
        "the stream must report the job that did not run: {two:?}"
    );

    // The line carries the whole record, so the reader learns WHY without a
    // second command. A terminal event with no cause makes an agent ask again,
    // which is the loop that this command removes.
    let expired = lines
        .iter()
        .find(|l| l["event"] == "job" && l["id"] == first.as_str() && l["state"] == "expired")
        .unwrap();
    assert!(
        expired["job"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("--max-queue-time"),
        "the record must name the limit: {expired}"
    );
    assert!(
        !expired["job"]["finished_at"].is_null(),
        "a terminal record must hold the time when the job stopped: {expired}"
    );
}

/// A reader that starts again must continue with no loss and no repetition.
///
/// An agent stops and starts. Without the numbers, it must read the stream from
/// the start and act on each event a second time, or start at "now" and lose
/// the results that arrived while it was away.
#[test]
fn a_reader_continues_from_the_number_that_it_read() {
    let h = Harness::with_default_config("eventssince");
    let id = h.submit(&["submit", "--name", "again", "--", "true"]);
    h.ok(&["wait", &id]);

    let all = events_lines(events_reader(
        &h,
        &["--json", "--since", "start", "--timeout", "3s"],
    ));
    let jobs: Vec<&serde_json::Value> = all.iter().filter(|l| l["event"] == "job").collect();
    assert!(jobs.len() >= 2, "the stream must hold the events: {all:?}");

    // The reader keeps the name of the stream with the number. The name is what
    // lets the coordinator see that it is not the coordinator that gave that
    // number.
    let stream_id = all[0]["stream_id"].as_str().unwrap().to_string();
    let first = jobs[0]["seq"].as_u64().unwrap();
    let after = events_lines(events_reader(
        &h,
        &[
            "--json",
            "--since",
            &format!("{stream_id}:{first}"),
            "--timeout",
            "3s",
        ],
    ));
    let got: Vec<u64> = after
        .iter()
        .filter(|l| l["event"] == "job")
        .map(|l| l["seq"].as_u64().unwrap())
        .collect();
    let expected: Vec<u64> = jobs
        .iter()
        .map(|l| l["seq"].as_u64().unwrap())
        .filter(|s| *s > first)
        .collect();
    assert_eq!(got, expected, "the reader must continue after its number");
    assert!(
        !got.contains(&first),
        "the reader must not read one event a second time"
    );
}

/// The stream must SAY when it dropped events.
///
/// The coordinator keeps the last events only, and it never waits for a reader.
/// A reader that arrives late thus loses events. A gap in silence is worse than
/// a gap that is reported: an agent that loses the line "the job failed" and
/// hears nothing waits for a result that will never arrive.
#[test]
fn the_stream_counts_the_events_that_it_dropped() {
    let h = Harness::with_default_config("eventsgap");

    // Make the ring small. With the usual size this test needs more than five
    // hundred events, and a test that takes minutes is a test that nobody runs.
    let out = h
        .command(&["submit", "--name", "gap0", "--", "true"])
        .env("QEX_EVENTS_RETAINED", "3")
        .output()
        .expect("qex did not start");
    assert!(out.status.success(), "the first submission failed");
    let first = String::from_utf8_lossy(&out.stdout).trim().to_string();
    h.ok(&["wait", &first]);

    // Each job gives three or four events, so these jobs pass the ring.
    for i in 1..4 {
        let id = h.submit(&["submit", "--name", &format!("gap{i}"), "--", "true"]);
        h.ok(&["wait", &id]);
    }

    let lines = events_lines(events_reader(
        &h,
        &["--json", "--since", "start", "--timeout", "3s"],
    ));
    let gap = lines
        .iter()
        .find(|l| l["event"] == "gap")
        .unwrap_or_else(|| panic!("the stream must report the gap: {lines:?}"));
    assert!(
        gap["missed"].as_u64().unwrap() > 0,
        "the gap must count the events that went away: {gap:?}"
    );

    // The stream must continue after the gap, and not stop.
    assert!(
        lines.iter().any(|l| l["event"] == "job"),
        "the stream must continue after a gap: {lines:?}"
    );
}

/// A reader must not hold the coordinator open, and it must hear the stop.
///
/// A stream that keeps a coordinator alive for ever is a leak. A stream that
/// dies in silence under its reader is a fault, because the reader cannot tell
/// an orderly stop from a broken socket.
#[test]
fn the_coordinator_retires_under_a_reader_and_says_goodbye() {
    let h = Harness::new(
        "eventsbye",
        "[peers]\nenabled = false\n[system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    // The reader is the FIRST command, so the coordinator that it starts holds
    // this short idle time.
    let reader = h
        .command(&["events", "--json", "--timeout", "60s"])
        .env("QEX_IDLE_EXIT_SECS", "3")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("qex events did not start");

    let out = reader.wait_with_output().expect("the reader did not stop");
    assert_eq!(
        out.status.code(),
        Some(0),
        "an orderly stop is not a failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap_or("")).unwrap();
    assert_eq!(
        last["event"], "bye",
        "the coordinator must say why the stream ends: {text}"
    );
    assert!(
        last["reason"].as_str().unwrap().contains("no job operates"),
        "the goodbye must give the reason: {last:?}"
    );
}

/// A coordinator that cannot obey `qex events` must REFUSE it.
///
/// This is the capability rule of qex, in the one place where a silent refusal
/// is worst. An earlier coordinator gave no line and no error, and the reader
/// then cannot tell "no job changed" from "this coordinator has no stream". It
/// waits for ever, which is the fault that qex exists to remove.
///
/// The test uses a coordinator of its own that answers like an earlier version.
/// It never starts a qex coordinator, so it never kills a process.
#[test]
fn a_coordinator_that_has_no_event_stream_refuses_the_command() {
    use std::io::{BufRead, BufReader, Write};

    let root = std::env::temp_dir().join(format!("qxold{}", std::process::id()));
    let run = root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::create_dir_all(root.join("cfg")).unwrap();
    let socket = run.join("s");

    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        // Answer one CLI process, then stop.
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut writer = stream.try_clone().unwrap();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            // An earlier version knows `info` and `capabilities`, and it does
            // not know `events`.
            let answer = if line.contains("\"info\"") {
                serde_json::json!({
                    "result": "info", "pid": 4321, "version": "0.7.1",
                    "started_at": 0, "program_replaced": false,
                    "jobs_running": 0, "jobs_queued": 0,
                    "cpu_budget": 1, "mem_budget": 1, "cpu_claimed": 0, "mem_claimed": 0,
                })
            } else if line.contains("\"capabilities\"") {
                serde_json::json!({ "result": "capabilities", "names": ["locks", "retries"] })
            } else {
                serde_json::json!({
                    "result": "error", "kind": "internal",
                    "message": "qex could not read this request",
                })
            };
            writeln!(writer, "{answer}").ok();
            writer.flush().ok();
        }
    });

    let peers = root.join("peers");
    std::fs::create_dir_all(&peers).ok();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_qex"));
    cmd.args(["events", "--json", "--timeout", "10s"]);
    isolate(&mut cmd, &root, &peers);
    let out = cmd.output().expect("qex did not start");

    let err = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    drop(server);
    std::fs::remove_dir_all(&root).ok();

    assert_eq!(
        out.status.code(),
        Some(1),
        "the command must fail. stdout: {stdout} stderr: {err}"
    );
    assert!(
        err.contains("cannot obey `qex events`"),
        "the message must say what happened: {err}"
    );
    assert!(
        err.contains("kill 4321"),
        "the message must give the remedy: {err}"
    );
    assert!(
        stdout.is_empty(),
        "a refused command must give no stream: {stdout}"
    );
}

/// A coordinator that cannot obey an option of a LATER stage must refuse the
/// whole pipeline, and no job of that file may start.
///
/// A test of the first stage speaks for that stage alone. The coordinator then
/// takes the later stage, ignores the option, and gives an id, so the reader
/// receives a pipeline that does not hold the rule that they wrote.
///
/// The test uses a coordinator of its own that answers like an earlier version.
/// It never starts a qex coordinator, so it never kills a process. It also
/// RECORDS every request, so the test proves that no job reached the queue.
#[test]
fn a_pipeline_with_an_option_on_a_later_stage_is_refused() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};

    let root = std::env::temp_dir().join(format!("qxpipecap{}", std::process::id()));
    // A run that a signal stopped leaves this directory and its socket, and the
    // next run with the same number then cannot bind. Take it first.
    std::fs::remove_dir_all(&root).ok();
    let run = root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::create_dir_all(root.join("cfg")).unwrap();
    let socket = run.join("s");

    let file = root.join("p.toml");
    std::fs::write(
        &file,
        "[[jobs]]\nname = \"build\"\ncommand = [\"true\"]\n\n\
         [[jobs]]\nname = \"test\"\ncommand = [\"true\"]\nneeds = [\"build\"]\nnice = 19\n",
    )
    .unwrap();

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let recorder = Arc::clone(&seen);
    let server = std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut writer = stream.try_clone().unwrap();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            recorder.lock().unwrap().push(line.clone());
            // An earlier version knows the groups and the dependencies, and it
            // does not know the politeness.
            let answer = if line.contains("\"info\"") {
                serde_json::json!({
                    "result": "info", "pid": 4321, "version": "0.7.1",
                    "started_at": 0, "program_replaced": false,
                    "jobs_running": 0, "jobs_queued": 0,
                    "cpu_budget": 1, "mem_budget": 1, "cpu_claimed": 0, "mem_claimed": 0,
                })
            } else if line.contains("\"capabilities\"") {
                serde_json::json!({
                    "result": "capabilities",
                    "names": ["dependencies", "groups", "locks", "retries"],
                })
            } else {
                serde_json::json!({
                    "result": "error", "kind": "internal",
                    "message": "qex could not read this request",
                })
            };
            writeln!(writer, "{answer}").ok();
            writer.flush().ok();
        }
    });

    let peers = root.join("peers");
    std::fs::create_dir_all(&peers).ok();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_qex"));
    cmd.args(["pipeline", file.to_str().unwrap()]);
    isolate(&mut cmd, &root, &peers);
    let out = cmd.output().expect("qex did not start");

    let err = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let requests = seen.lock().unwrap().clone();
    drop(server);
    std::fs::remove_dir_all(&root).ok();

    assert_eq!(
        out.status.code(),
        Some(121),
        "a refused pipeline must give 121, as `qex submit` does. stdout: {stdout} stderr: {err}"
    );
    assert!(
        err.contains("--nice"),
        "the message must name the option: {err}"
    );
    assert!(
        err.contains("`test`"),
        "the message must name the stage that the reader corrects: {err}"
    );
    assert!(
        !requests.iter().any(|r| r.contains("\"submit\"")),
        "no job of the file may reach the queue: {requests:?}"
    );
    assert!(
        stdout.is_empty(),
        "a refused pipeline must give no job id: {stdout}"
    );
}

/// A `qex rerun` against a coordinator that cannot obey an option of the job
/// must be refused.
///
/// A rerun keeps the locks, the retries, the politeness, the queue limit and
/// the claims of the first job. Without this test the coordinator takes the new
/// job, ignores each of those, and gives an id: the reader asked qex to run the
/// work AGAIN, and receives work that holds none of its rules.
///
/// The test starts a real coordinator to make the record, stops it, and then
/// answers the socket itself like an earlier version. The answer to the list
/// comes from the record on the disk, so no JSON of a job is written by hand.
#[test]
fn a_rerun_of_a_job_that_holds_a_lock_is_refused_by_an_earlier_coordinator() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};

    let h = Harness::with_default_config("reruncap");
    let id = h.submit(&["submit", "--lock", "deploy", "--mem", "64MB", "--", "true"]);
    h.ok(&["wait", &id]);

    // Stop the coordinator, and take its socket. The pid comes from qex, and
    // never from a search of the process table.
    let info = h.ok(&["info", "--no-start", "--json"]);
    let pid: i32 = serde_json::from_str::<serde_json::Value>(&info).unwrap()["pid"]
        .as_i64()
        .expect("the coordinator must report its pid") as i32;
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let socket = h.root.join("state/qex/run/s");
    for _ in 0..200 {
        if !socket.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::fs::remove_file(&socket).ok();

    // The record of the job, as the coordinator would give it.
    let raw = std::fs::read_to_string(h.root.join(format!("state/qex/jobs/{id}/status.json")))
        .expect("the record of the job must exist");
    // The protocol gives one answer for each line, and the record on the disk
    // is written for a reader. Take the value, and write it as one line.
    let status: serde_json::Value =
        serde_json::from_str(&raw).expect("the record must hold one job");

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let recorder = Arc::clone(&seen);
    let server = std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut writer = stream.try_clone().unwrap();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            recorder.lock().unwrap().push(line.clone());
            let answer = if line.contains("\"info\"") {
                serde_json::json!({
                    "result": "info", "pid": 4321, "version": "0.7.1",
                    "started_at": 0, "program_replaced": false,
                    "jobs_running": 0, "jobs_queued": 0,
                    "cpu_budget": 1, "mem_budget": 1, "cpu_claimed": 0, "mem_claimed": 0,
                })
                .to_string()
            } else if line.contains("\"capabilities\"") {
                // An earlier version with no locks.
                serde_json::json!({
                    "result": "capabilities",
                    "names": ["dependencies", "groups", "retries", "politeness"],
                })
                .to_string()
            } else if line.contains("\"list\"") {
                serde_json::json!({ "result": "jobs", "jobs": [status] }).to_string()
            } else {
                serde_json::json!({
                    "result": "error", "kind": "internal",
                    "message": "qex could not read this request",
                })
                .to_string()
            };
            writeln!(writer, "{answer}").ok();
            writer.flush().ok();
        }
    });

    let out = h
        .command(&["rerun", &id])
        .output()
        .expect("qex did not start");

    let err = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let requests = seen.lock().unwrap().clone();
    drop(server);

    assert_eq!(
        out.status.code(),
        Some(121),
        "a refused rerun must give 121, as `qex submit` does. stdout: {stdout} stderr: {err}"
    );
    assert!(
        err.contains("--lock"),
        "the message must name the option: {err}"
    );
    assert!(
        !requests.iter().any(|r| r.contains("\"submit\"")),
        "no job may reach the queue: {requests:?}"
    );
}

/// A number from a coordinator that stopped must give a gap, and never a
/// silent continuation.
///
/// The numbers start at 1 in each coordinator, and a new coordinator makes one
/// event for each record that it reads. The number 2 of the earlier stream thus
/// names a DIFFERENT event in the new stream. Without the name of the stream,
/// the coordinator continues from that number, and the reader loses the events
/// before it with no message — which is the class of fault that qex exists to
/// remove.
#[test]
fn a_number_from_a_coordinator_that_stopped_gives_a_gap() {
    let h = Harness::with_default_config("eventsrestart");
    for i in 0..3 {
        let id = h.submit(&["submit", "--name", &format!("old{i}"), "--", "true"]);
        h.ok(&["wait", &id]);
    }

    let before = events_lines(events_reader(
        &h,
        &["--json", "--since", "now", "--timeout", "2s"],
    ));
    let old_stream = before[0]["stream_id"].as_str().unwrap().to_string();

    // Stop the coordinator of THIS test. The next command starts a new one,
    // which reads the same records and makes its own numbers.
    let pid = h.coordinator_pid();
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    h.until("the coordinator goes away", Duration::from_secs(20), || {
        (unsafe { libc::kill(pid, 0) }) != 0
    });

    let lines = events_lines(events_reader(
        &h,
        &[
            "--json",
            "--since",
            &format!("{old_stream}:2"),
            "--timeout",
            "3s",
        ],
    ));

    assert_ne!(
        lines[0]["stream_id"].as_str().unwrap(),
        old_stream,
        "the new coordinator must have a stream of its own"
    );
    let gap = lines
        .iter()
        .find(|l| l["event"] == "gap")
        .unwrap_or_else(|| {
            panic!("a number from a stream that stopped must give a gap: {lines:?}")
        });
    assert!(
        gap["missed"].is_null(),
        "qex cannot count across two streams, and it must not invent a number: {gap:?}"
    );
    assert!(
        gap["reason"].as_str().unwrap().contains(&old_stream),
        "the reason must name the stream that gave the number: {gap:?}"
    );

    // The stream must continue with everything that the new coordinator holds,
    // and not from the number of the stream that stopped.
    let seqs: Vec<u64> = lines
        .iter()
        .filter(|l| l["event"] == "job")
        .map(|l| l["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(
        seqs.first(),
        Some(&1),
        "the stream must continue from the first event that the new coordinator holds: {seqs:?}"
    );
}

/// A reader that goes away must not leave a thread and two file handles behind.
///
/// A bounded read is the shape that `--since`, `--count` and `--timeout` exist
/// for: an agent reads for a few seconds, does its work, and comes back. On a
/// quiet queue there is nothing to write to such a reader, so a write cannot
/// find that it went away. Without a test of the connection itself, each of
/// those readers held a thread and two handles for the hour of the idle time,
/// and at the limit of the handles the coordinator accepts no connection.
///
/// This test reads `/proc`, so it runs on Linux only.
#[cfg(target_os = "linux")]
#[test]
fn a_reader_that_goes_away_leaves_no_thread_behind() {
    let h = Harness::with_default_config("eventsfds");
    let pid = h.coordinator_pid();
    let count = |what: &str| {
        std::fs::read_dir(format!("/proc/{pid}/{what}"))
            .map(|d| d.count())
            .unwrap_or(0)
    };

    let threads = count("task");
    let handles = count("fd");
    assert!(threads > 0, "this test needs /proc");

    // Each reader stops by itself. See `events_reader`.
    for _ in 0..8 {
        let reader = events_reader(&h, &["--json", "--since", "now", "--timeout", "1s"]);
        reader.wait_with_output().expect("the reader did not stop");
    }

    // The coordinator finds a reader that went away at its next test of the
    // connection, so give it a moment.
    h.until(
        "the coordinator releases the threads of the readers that stopped",
        Duration::from_secs(20),
        || count("task") <= threads + 1 && count("fd") <= handles + 2,
    );
}

/// A paused queue must start no job, and the jobs that operate must continue.
///
/// # The fault that this test prevents
///
/// A person pauses the queue to take the machine back. A pause that still
/// started a small job would change the measurement that the person is trying
/// to take. A pause that stopped the job which already operates would lose the
/// work AND the capacity that the job holds, and no command gives that back.
#[test]
fn a_paused_queue_starts_no_job_and_the_jobs_that_operate_continue() {
    let h = Harness::with_default_config("pausequeue");

    let running = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
    ]);
    h.until("the first job operates", Duration::from_secs(45), || {
        h.state_of(&running) == "running"
    });

    h.ok(&["pause", "queue"]);

    let waiter = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);

    // Measure for a period, and not one time. A job that starts late would pass
    // a test that looks one time only.
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        assert_eq!(
            h.state_of(&waiter),
            "queued",
            "a paused queue must start no job"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    assert_eq!(
        h.state_of(&running),
        "running",
        "a pause must not stop the job that already operates"
    );

    h.ok(&["resume", "queue"]);
    h.until("the job starts again", Duration::from_secs(45), || {
        h.has_started(&waiter)
    });

    h.ok(&["kill", &running, "--grace", "1s"]);
}

/// The pause must survive a coordinator that stops.
///
/// # The fault that this test prevents
///
/// This is the test that protects the whole feature. A coordinator stops when
/// no job operates, and when a new build replaces the program file. qex itself
/// tells a user to run `kill <pid>` on it. A pause that lived in the memory of
/// that process would go away in silence, the next command would start a new
/// coordinator, and the queue would start work while the person believes that
/// the machine is quiet.
#[test]
fn the_pause_survives_a_coordinator_that_stops() {
    let h = Harness::with_default_config("pausesurvives");

    h.ok(&["pause", "queue", "--reason", "recording a demo"]);

    // Take the pid from the coordinator itself. A search of the process list
    // also matches the command that holds those letters.
    let first = h.ok(&["info", "--no-start", "--json"]);
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    let first = first["pid"].as_i64().unwrap() as i32;

    unsafe {
        libc::kill(first, libc::SIGKILL);
    }
    h.until("the coordinator stopped", Duration::from_secs(30), || {
        let alive = unsafe { libc::kill(first, 0) } == 0;
        !alive
    });

    // This command starts a new coordinator.
    let id = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);

    let second = h.ok(&["info", "--no-start", "--json"]);
    let second: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(second["queue_state"], "paused", "the pause did not survive");
    assert_eq!(second["paused_reason"], "recording a demo");
    assert_ne!(
        second["pid"].as_i64().unwrap() as i32,
        first,
        "the test must measure a NEW coordinator"
    );

    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        assert_eq!(
            h.state_of(&id),
            "queued",
            "the new coordinator must start no job"
        );
        std::thread::sleep(Duration::from_millis(300));
    }

    h.ok(&["resume"]);
    h.until("the job starts again", Duration::from_secs(45), || {
        h.has_started(&id)
    });
}

/// A person can ask for a lock that a job holds.
///
/// # The fault that this test prevents
///
/// A command that refused while a job held the lock would be a command that a
/// person cannot use: the person would try it again and again, and one of the
/// waiting jobs would take the lock between two tries. The request must be safe
/// to type at any moment, so qex records it, the job that holds the lock keeps
/// it, and no other job takes it in the time between.
#[test]
fn a_person_gets_a_lock_when_the_job_that_holds_it_stops() {
    let h = Harness::with_default_config("pauselock");

    let holder = h.submit(&[
        "submit", "--lock", "gpu0", "--cpu", "1", "--mem", "64MB", "--", "sleep", "6",
    ]);
    h.until("the job holds the lock", Duration::from_secs(45), || {
        h.state_of(&holder) == "running"
    });

    let out = h.ok(&["pause", "lock", "gpu0"]);
    assert!(
        out.contains("holds the lock"),
        "the answer must name the job that holds the lock now: {out}"
    );

    // This job needs the same lock. It must never take it.
    let waiter = h.submit(&[
        "submit", "--lock", "gpu0", "--cpu", "1", "--mem", "64MB", "--", "true",
    ]);

    h.until("the first job stopped", Duration::from_secs(60), || {
        h.state_of(&holder) == "completed"
    });

    // The lock is now the person's, and the job still waits.
    h.until(
        "the lock belongs to the person",
        Duration::from_secs(30),
        || h.ok(&["pause"]).contains("it is yours now"),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        assert_eq!(
            h.state_of(&waiter),
            "queued",
            "no job may take a lock that a person holds"
        );
        std::thread::sleep(Duration::from_millis(300));
    }

    let reason = h.status_json(&waiter)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        reason.contains("which a person holds"),
        "the reason must name the person: {reason}"
    );

    h.ok(&["resume", "lock", "gpu0"]);
    h.until("the job takes the lock", Duration::from_secs(45), || {
        h.has_started(&waiter)
    });
}

/// A job that waits for a pause must say the pause, and not the capacity.
///
/// # The fault that this test prevents
///
/// A paused job that said "waits for capacity" sends the reader to look at the
/// budget, at the memory and at the other users of the machine. None of those
/// is the cause, and no change to any of them starts the job.
#[test]
fn a_job_that_waits_for_a_pause_says_the_pause() {
    let h = Harness::with_default_config("pausereason");

    h.ok(&["pause", "queue", "--reason", "recording a demo"]);

    let out = h.qex(&["submit", "--", "true"]);
    assert!(out.status.success());
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let warning = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        warning.contains("the queue is paused"),
        "the submission must warn immediately: {warning}"
    );

    h.until("the job has a reason", Duration::from_secs(30), || {
        !h.status_json(&id)["blocked_reason"].is_null()
    });
    let reason = h.status_json(&id)["blocked_reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        reason.contains("the queue is paused"),
        "the reason must give the pause: {reason}"
    );
    assert!(
        reason.contains("recording a demo"),
        "the reason must give the text of --reason: {reason}"
    );
    assert!(
        !reason.contains("waits for"),
        "the pause replaces the capacity reason, and does not stand beside it: {reason}"
    );

    h.ok(&["resume"]);
}

/// A pause with `--for` must end by itself.
///
/// # The fault that this test prevents
///
/// A pause that needed a second command would become a queue that a person
/// forgets, and an empty queue in the morning.
#[test]
fn a_pause_with_a_time_ends_by_itself() {
    let h = Harness::with_default_config("pausefor");

    h.ok(&["pause", "queue", "--for", "10s"]);
    let id = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);

    assert_eq!(
        h.state_of(&id),
        "queued",
        "the job must wait while the pause lasts"
    );

    h.until(
        "the job starts when the pause ends",
        Duration::from_secs(60),
        || h.has_started(&id),
    );
    assert!(
        h.ok(&["pause"]).contains("queue: running"),
        "the pause must go away by itself"
    );
}

/// A job whose dependency failed must become `skipped` while the queue is
/// paused.
///
/// # The fault that this test prevents
///
/// The dependency pass and the capacity pass are separate. If the pause test
/// stopped the dependency pass as well, a job whose dependency already failed
/// would stay in the queue for the whole length of the pause, and `qex wait` on
/// it would block for ever. Skipping starts no process, so a pause has no
/// reason to stop it.
#[test]
fn a_failed_dependency_is_still_skipped_while_the_queue_is_paused() {
    let h = Harness::with_default_config("pausedeps");

    let failer = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "false"]);
    let out = h.qex(&["wait", &failer]);
    assert_eq!(out.status.code(), Some(1));

    h.ok(&["pause", "queue"]);

    let skipped = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--needs", &failer, "--", "true",
    ]);

    h.until("the job is skipped", Duration::from_secs(45), || {
        h.state_of(&skipped) == "skipped"
    });

    let out = h.qex(&["wait", &skipped, "--timeout", "10s"]);
    assert_eq!(
        out.status.code(),
        Some(126),
        "`qex wait` must give an answer while the queue is paused"
    );

    h.ok(&["resume"]);
}

/// A retry continues while the queue is paused. The job already started.
///
/// # The fault that this test prevents
///
/// The supervisor used to read the pause file and hold the next attempt. A
/// pause stops new work. This job already holds its slot, so a pause that
/// delayed the retry treated an operating job as a new submission. New work
/// still waits.
#[test]
fn a_retry_continues_while_the_queue_is_paused() {
    let h = Harness::with_default_config("pauseretry");

    let id = h.submit(&[
        "submit",
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--retries",
        "3",
        "--",
        "sh",
        "-c",
        "echo attempt; sleep 2; exit 1",
    ]);
    h.until(
        "the first attempt operates",
        Duration::from_secs(45),
        || h.state_of(&id) == "running",
    );

    h.ok(&["pause", "queue"]);
    let waiter = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);

    let log = h.job_dir(&id).join("stdout.log");
    let count = |path: &Path| {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .matches("attempt")
            .count()
    };
    let at_the_pause = count(&log);
    h.until(
        "the next attempt starts while the queue is paused",
        Duration::from_secs(45),
        || count(&log) > at_the_pause,
    );
    assert_eq!(
        h.state_of(&waiter),
        "queued",
        "a pause must still hold work that has not started"
    );

    // Between attempts the record is `running` with no pid, so `qex kill` can
    // refuse. The job stops by itself when the retries run out.
    h.qex(&["kill", &id, "--grace", "1s"]);
    h.ok(&["resume"]);
}

/// A retry keeps the lock of the job until the job itself stops.
///
/// # The fault that this test prevents
///
/// The supervisor used to wait when a person asked for the lock, so the next
/// attempt treated the lock as free. The person then held `gpu0` in the record
/// while the same job used it again, or another job took it between attempts.
/// The lock comes to the person only when the job stops.
#[test]
fn a_retry_keeps_its_lock_until_the_job_stops() {
    let h = Harness::with_default_config("pauseretrylock");

    let id = h.submit(&[
        "submit",
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--lock",
        "gpu0",
        "--retries",
        "1",
        "--",
        "sh",
        "-c",
        "echo attempt; sleep 2; exit 1",
    ]);
    h.until(
        "the first attempt operates",
        Duration::from_secs(45),
        || h.state_of(&id) == "running",
    );

    h.ok(&["pause", "lock", "gpu0"]);
    let waiter = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--lock", "gpu0", "--", "true",
    ]);

    let log = h.job_dir(&id).join("stdout.log");
    let count = |path: &Path| {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .matches("attempt")
            .count()
    };
    let at_the_pause = count(&log);
    h.until(
        "the next attempt starts while a person has asked for the lock",
        Duration::from_secs(45),
        || count(&log) > at_the_pause,
    );
    assert_eq!(
        h.state_of(&waiter),
        "queued",
        "another job must wait for the lock that the retrying job still holds"
    );

    // Do not kill. Between attempts the record is `running` with no pid, and
    // `qex kill` then says "starts now". The last attempt stops by itself.
    h.until("the retrying job stops", Duration::from_secs(45), || {
        h.state_of(&id) == "failed"
    });
    assert_eq!(
        h.state_of(&waiter),
        "queued",
        "the lock goes to the person when the job stops, not to the next job"
    );

    h.ok(&["resume", "lock", "gpu0"]);
    h.until("the waiter takes the lock", Duration::from_secs(45), || {
        h.has_started(&waiter)
    });
}

/// A pause record that qex cannot read must HOLD the queue, and say why.
///
/// # The fault that this test prevents
///
/// A parse fault that gave "nothing is paused" would start the work while the
/// person believes that the machine is quiet, with no line in any log and no
/// word in any command. The two directions are not equal in cost: a queue that
/// qex holds by mistake costs latency and one command corrects it, and a queue
/// that operates by mistake cannot be corrected after the work started.
#[test]
fn a_pause_record_that_qex_cannot_read_holds_the_queue() {
    let h = Harness::with_default_config("pausebroken");

    // Make the runtime directory, then write a record that no version of qex
    // can read.
    h.ok(&["pause", "queue"]);
    let file = h.root.join("state/qex/run/paused.json");
    assert!(file.exists(), "the pause must be a file");
    h.ok(&["resume"]);

    // Stop the coordinator, so the next command reads the file from the start.
    let text = h.ok(&["info", "--no-start", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let pid = v["pid"].as_i64().unwrap() as i32;
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    h.until("the coordinator stopped", Duration::from_secs(30), || {
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        !alive
    });

    std::fs::write(&file, "{\"queue\": {\"paused_at\": ").unwrap();

    let id = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        assert_eq!(
            h.state_of(&id),
            "queued",
            "a record that qex cannot read must hold the queue"
        );
        std::thread::sleep(Duration::from_millis(300));
    }

    // The report must say what happened and what to do.
    let info = h.ok(&["info", "--no-start", "--json"]);
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(info["queue_state"], "paused-by-fault");

    let words = h.ok(&["pause"]);
    assert!(
        words.contains("could not read"),
        "the report must say what happened: {words}"
    );
    assert!(
        words.contains("qex resume queue"),
        "the report must give the remedy: {words}"
    );

    let reason = h.status_json(&id)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        reason.contains("could not read"),
        "the job must give the true reason: {reason}"
    );

    // The remedy must operate.
    h.ok(&["resume"]);
    h.until("the job starts again", Duration::from_secs(45), || {
        h.has_started(&id)
    });
}

/// `--for 0` must be an error.
///
/// `parse_duration` reads `0` as "no limit", which is correct for `--timeout`
/// and is the opposite of what `--for` asks for. A person who writes `--for 0`
/// wants a short pause, and must not get a pause with no end.
#[test]
fn a_pause_for_zero_is_refused() {
    let h = Harness::with_default_config("pausezero");
    let out = h.qex(&["pause", "queue", "--for", "0"]);
    assert!(!out.status.success(), "`--for 0` must not give a pause");
    let words = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        words.contains("qex resume queue"),
        "the error must give the remedy: {words}"
    );
    assert!(h.ok(&["pause"]).contains("nothing is paused"));
}

/// A second `qex pause queue` must keep the end that the first one gave.
#[test]
fn a_second_pause_keeps_the_end_of_the_first() {
    let h = Harness::with_default_config("pausetwice");

    h.ok(&["pause", "queue", "--for", "30m", "--reason", "a video call"]);
    h.ok(&["pause", "queue"]);

    let words = h.ok(&["pause"]);
    assert!(
        !words.contains("NO END"),
        "the second command must not remove the end: {words}"
    );
    assert!(
        words.contains("a video call"),
        "the second command must not remove the reason: {words}"
    );

    h.ok(&["resume"]);
}

/// A pause must NOT expire a job that carries `--max-queue-time`.
///
/// # The fault that this test prevents
///
/// `--max-queue-time` and the pause are two features that landed apart, and
/// their meeting is the sharp edge of both. A person pauses the queue to take
/// the machine for half an hour. Every job with a smaller limit then reaches
/// that limit while NOBODY can start it: qex writes `expired`, it fires the
/// stop hook of each one, and the person — who is away, which is the whole
/// point of a pause — comes back to an empty queue and a set of dead records.
///
/// A pause protects the machine. It must not delete the queue. The clock of the
/// limit therefore stops while the queue is paused, and it runs again at the
/// resume.
#[test]
fn a_pause_does_not_expire_a_job_that_has_a_queue_limit() {
    let h = Harness::with_default_config("pauseexpire");

    h.ok(&["pause", "queue"]);

    // A limit of 3 seconds, and a pause much longer than that.
    let id = h.submit(&[
        "submit",
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--max-queue-time",
        "3s",
        "--",
        "true",
    ]);

    // Measure over a period. The scheduler tests the queue every 500ms, so a
    // job that expires does so inside this window.
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let state = h.state_of(&id);
        assert_eq!(
            state, "queued",
            "a paused queue must not expire a job; the job became `{state}`"
        );
        std::thread::sleep(Duration::from_millis(300));
    }

    // The record must say that the pause gave the time back. Without this the
    // test above would also pass on a build that merely DELAYED the expiry to
    // the resume, which kills the same job one moment later.
    let credited = h.status_json(&id)["queue_pause_secs"].as_u64().unwrap_or(0);
    assert_eq!(
        credited, 0,
        "the credit belongs to the END of the pause, not to each second of it"
    );

    h.ok(&["resume", "queue"]);

    let credited = h.status_json(&id)["queue_pause_secs"].as_u64().unwrap_or(0);
    assert!(
        credited >= 5,
        "the resume must give the paused time back; got {credited} seconds"
    );

    // The job runs. It never expired.
    h.until(
        "the job runs after the resume",
        Duration::from_secs(45),
        || h.state_of(&id) == "completed",
    );
    assert_eq!(
        h.state_of(&id),
        "completed",
        "the job must run, and not expire"
    );
}

/// The limit must still expire a job after the pause ends.
///
/// # The fault that this test prevents
///
/// The rule above stops the clock of `--max-queue-time` while the queue is
/// paused. A build that stopped that clock and never started it again would
/// turn one pause into a queue with no limits at all, and `--max-queue-time`
/// would give an id and wait for ever — which is the fault that
/// `--max-queue-time` exists to remove.
#[test]
fn the_limit_still_expires_a_job_after_the_pause_ends() {
    let h = Harness::with_default_config("pauseexpire2");

    // Block the job with a DEPENDENCY, and not with the capacity of the
    // machine. A test that holds every core gives a different answer on a
    // machine that is busy.
    let holder = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
    ]);
    h.until("the first job operates", Duration::from_secs(45), || {
        h.state_of(&holder) == "running"
    });

    h.ok(&["pause", "queue"]);
    let id = h.submit(&[
        "submit",
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--needs",
        &holder,
        "--max-queue-time",
        "4s",
        "--",
        "true",
    ]);
    std::thread::sleep(Duration::from_secs(7));
    assert_eq!(
        h.state_of(&id),
        "queued",
        "the pause must hold the clock of the limit"
    );

    h.ok(&["resume", "queue"]);

    // The clock runs again from the resume. The job still waits for a job that
    // does not stop, so it reaches its limit and gives up.
    h.until("the job reaches its limit", Duration::from_secs(45), || {
        h.state_of(&id) == "expired"
    });

    h.ok(&["kill", &holder, "--grace", "1s"]);
}

/// `qex wait` must say that the queue is paused.
///
/// # The fault that this test prevents
///
/// Issue #28: `qex wait` gives no word where `qex run` explains itself. A pause
/// makes that silence unbounded. An agent that runs `qex wait <id>` with no
/// `--timeout`, behind a pause with no end, waits for ever and reads nothing —
/// no line, no reason, no remedy. The wait must name the pause and the command
/// that ends it.
#[test]
fn a_wait_behind_a_pause_says_the_pause() {
    let h = Harness::with_default_config("pausewait");

    h.ok(&["pause", "queue", "--reason", "recording a demo"]);
    let id = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);

    let out = h.qex(&["wait", &id, "--timeout", "3s"]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        err.contains("THE QUEUE IS PAUSED"),
        "the wait must say that the queue is paused: {err}"
    );
    assert!(
        err.contains("recording a demo"),
        "the wait must give the reason of the pause: {err}"
    );
    assert!(
        err.contains("qex resume queue"),
        "the wait must give the command that ends the pause: {err}"
    );
    assert_eq!(
        out.status.code(),
        Some(124),
        "the wait still reaches its own limit and gives 124"
    );

    // The result of a wait stays alone on stdout. An agent reads that stream.
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        !stdout.contains("PAUSED"),
        "the pause belongs on stderr: {stdout}"
    );

    h.ok(&["resume"]);
}

/// A pause reason and a lock name are user text. No control byte of either may
/// reach a terminal.
///
/// # The fault that this test prevents
///
/// `--reason` is a SENTENCE that `qex info`, `qex top`, `qex list` and `qex
/// pause` each print, and a lock name is a NAME that goes into the
/// `blocked_reason` of every job that waits for it. An ESC byte in either one
/// clears the screen of the next reader or moves the cursor over the lines
/// above, and the reader then trusts a display that the text wrote.
#[test]
fn a_pause_shows_no_control_byte_of_a_reason_or_a_lock_name() {
    let h = Harness::with_default_config("pausebytes");

    h.ok(&["pause", "queue", "--reason", "esc\x1b[2Jbad"]);
    h.ok(&["pause", "lock", "esc\x1b[2Jlock"]);

    let id = h.submit(&[
        "submit",
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--lock",
        "esc\x1b[2Jlock",
        "--",
        "true",
    ]);
    h.until(
        "the job takes the reason of the pause",
        Duration::from_secs(45),
        || !h.status_json(&id)["blocked_reason"].is_null(),
    );

    for args in [
        vec!["pause"],
        vec!["info"],
        vec!["list"],
        vec!["status", id.as_str()],
    ] {
        let out = h.qex(&args);
        let mut stream = out.stdout.clone();
        stream.extend_from_slice(&out.stderr);
        assert!(
            !stream.windows(4).any(|w| w == b"\x1b[2J"),
            "`qex {}` wrote the ESC byte of a pause",
            args.join(" ")
        );
    }

    // The sentences must still NAME the reason and the lock, in their safe
    // form. A test that only looks for the absence of a byte passes when the
    // text is gone.
    let shown = h.ok(&["pause"]);
    assert!(
        shown.contains("esc [2Jbad"),
        "`qex pause` must keep the reason, without the control byte: {shown}"
    );
    assert!(
        shown.contains("esc_2Jlock"),
        "`qex pause` must keep the lock name, in its safe form: {shown}"
    );

    h.ok(&["resume", "lock", "esc\x1b[2Jlock"]);
    h.ok(&["resume"]);
}

/// An oversized job in a paused queue must say the PAUSE, and not the budget.
///
/// # The fault that this test prevents
///
/// A job that is larger than the budget carries its own reason, and the pause
/// carries another. A record that kept the budget reason sends the reader to
/// change the budget, and no budget starts a job while the queue is paused. The
/// reader then makes a change that cannot help, and reads the same line again.
///
/// The pause REPLACES the capacity reason; it never stands beside one.
#[test]
fn an_oversized_job_in_a_paused_queue_says_the_pause() {
    let h = Harness::new(
        "pauseoversized",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [queue]\noversized = \"run-when-idle\"\nsettle = \"1s\"\n",
    );

    h.ok(&["pause", "queue", "--reason", "recording a demo"]);

    // Larger than the budget, so the submission also has the oversized reason.
    let id = h.submit(&["submit", "--cpu", "4", "--mem", "128MB", "--", "true"]);

    let reason = h.status_json(&id)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        reason.contains("the queue is paused"),
        "the record must give the pause as the reason: {reason}"
    );
    assert!(
        !reason.contains("the budget is"),
        "the record must not send the reader to the budget: {reason}"
    );

    // The warning of the submission still gives BOTH, because the person who
    // typed the command must learn about the claim as well.
    let out = h.qex(&["submit", "--cpu", "4", "--mem", "128MB", "--", "true"]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains("the queue is paused"),
        "the submission must name the pause: {err}"
    );
    assert!(
        err.contains("the budget is"),
        "the submission must still name the claim: {err}"
    );

    h.ok(&["resume"]);
}

/// Every command that reports a pause must name the SAME pid, and it must be
/// the pid of the process that paused.
///
/// # The fault that this test prevents
///
/// `Response::Info` carried the moment, the reason and the end of a pause, and
/// no pauser pid. The two readers of that answer therefore invented one: `qex
/// top` gave 0, and `qex info` gave the pid of the COORDINATOR. One paused
/// queue thus gave three answers to "who paused this".
///
/// The `qex info` invention is the dangerous one. Each pause message tells the
/// reader that a pid can be killed, so a second person reads `qex info`,
/// believes that the coordinator is the pauser, and runs `kill <pid>` on the
/// one process that must keep operating.
///
/// A unit test on `pause::queue_line` cannot see this: it builds the record
/// itself, so it measures the formatter and never the caller. This test goes
/// through the commands.
#[test]
fn every_command_names_the_same_pauser_and_not_the_coordinator() {
    let h = Harness::with_default_config("pausewho");

    h.ok(&["pause", "queue", "--reason", "recording a demo"]);

    // The pid of the coordinator. No report of the pause may give this number.
    let coordinator = h.ok(&["info", "--no-start", "--json"]);
    let coordinator: serde_json::Value = serde_json::from_str(&coordinator).unwrap();
    let coordinator_pid = coordinator["pid"].as_i64().expect("the coordinator pid");

    let pauser = coordinator["paused_by_pid"]
        .as_i64()
        .expect("`qex info --json` must give the pid that asked for the pause");
    assert_ne!(
        pauser, coordinator_pid,
        "the pauser is the CLI process, and never the coordinator"
    );
    assert!(pauser > 0, "a pid of 0 is not a process: {pauser}");

    // Every text report must name that same pid, and none of them may name the
    // coordinator. `qex list` and `qex wait` write the pause to STDERR, so this
    // loop reads both streams.
    for args in [
        vec!["pause"],
        vec!["info"],
        vec!["top", "--once"],
        vec!["list"],
    ] {
        let out = h.qex(&args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains(&format!("pid {pauser}")),
            "`qex {}` must name the process that paused: {text}",
            args.join(" ")
        );
        assert!(
            !text.contains(&format!("by pid {coordinator_pid}")),
            "`qex {}` must not name the coordinator as the pauser: {text}",
            args.join(" ")
        );
        assert!(
            !text.contains("by an unknown process"),
            "this coordinator reports the pid, so no report may say unknown: {text}",
        );
    }

    h.ok(&["resume"]);
}

/// A pause that reaches its `--for` while NO coordinator operates must still
/// give the time back.
///
/// # The fault that this test prevents
///
/// A pause of the queue ends in three places, and only two of them were
/// covered. The third is a coordinator that is not there: qex itself tells a
/// user to run `kill <pid>` on it, a new build replaces it, and a reboot or an
/// out-of-memory kill removes it. `recover` read the pause and expired it, and
/// it gave no credit — and it could not, because the job records are read after
/// that point, so there was nobody to credit yet.
///
/// The whole queue-deleting behaviour came back through this door: the job
/// below reached its `--max-queue-time` on time that the pause took, and its
/// message blamed the person for the wait.
#[test]
fn a_pause_that_ends_while_the_coordinator_is_down_still_gives_the_time_back() {
    let h = Harness::with_default_config("pausedown");

    // A job that holds the machine, so the job below waits for the queue and
    // not for a moment of capacity.
    let holder = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
    ]);
    h.until("the first job operates", Duration::from_secs(45), || {
        h.state_of(&holder) == "running"
    });

    h.ok(&["pause", "queue", "--for", "8s"]);
    let id = h.submit(&[
        "submit",
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--needs",
        &holder,
        "--max-queue-time",
        "4s",
        "--",
        "true",
    ]);

    // Take the pid from the coordinator itself, and never from a process list.
    let info = h.ok(&["info", "--no-start", "--json"]);
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    let pid = info["pid"].as_i64().expect("the coordinator pid") as i32;

    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }

    // Let the pause reach its end with no coordinator to see it.
    std::thread::sleep(Duration::from_secs(12));

    // The next command starts a new coordinator, which runs `recover`.
    let credited = h.status_json(&id)["queue_pause_secs"].as_u64().unwrap_or(0);
    assert!(
        credited >= 6,
        "a pause that ended while no coordinator operated must still give the \
         time back; got {credited} seconds"
    );

    // The credit must not exceed the pause. The time between the end of the
    // pause and the restart was time that the queue was free to run.
    assert!(
        credited <= 10,
        "the credit must be the length of the PAUSE, and not the time until \
         the restart; got {credited} seconds"
    );

    h.ok(&["kill", &holder, "--grace", "1s"]);
}

/// A job that ALREADY waited when the pause began must learn the pause too.
///
/// # The fault that this test prevents
///
/// Two different pieces of code put the pause into the reason of a job.
/// `handle_submit` covers a job that arrives DURING a pause. `sched::choose`
/// covers a job that was already in the queue when the pause began, and only
/// that one — the job keeps whatever it waited for before, "waits for cores: 1
/// of 1 is in use", for the whole length of the pause.
///
/// That is the worse of the two. The reader takes the sentence at its word,
/// looks at a budget that is not the cause, and can wait for hours before
/// anything says the true reason. Deleting the loop in `choose` left the whole
/// suite green.
#[test]
fn a_job_that_already_waited_learns_the_pause() {
    let h = Harness::new(
        "pausealready",
        "[budget]
cpu = \"1\"
mem = \"1GB\"
\
         [peers]
enabled = false
\
         [system]
reserve_mem = \"0\"
max_pressure = 100
",
    );

    // A job that holds the machine, and a job behind it. The second job waits
    // for the first, and its reason says so.
    let holder = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
    ]);
    h.until("the first job operates", Duration::from_secs(45), || {
        h.state_of(&holder) == "running"
    });
    let waiter = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    h.until(
        "the second job has a reason",
        Duration::from_secs(45),
        || !h.status_json(&waiter)["blocked_reason"].is_null(),
    );

    // The reason before the pause. Without this the test cannot tell a reason
    // that CHANGED from a reason that was always the pause.
    let before = h.status_json(&waiter)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        !before.contains("paused"),
        "the job must wait for something other than a pause first: {before}"
    );

    h.ok(&["pause", "queue", "--reason", "recording a demo"]);

    h.until("the job learns the pause", Duration::from_secs(30), || {
        h.status_json(&waiter)["blocked_reason"]
            .as_str()
            .unwrap_or("")
            .contains("the queue is paused")
    });

    let after = h.status_json(&waiter)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        after.contains("qex resume queue"),
        "the reason must give the remedy: {after}"
    );

    h.ok(&["resume"]);
    h.ok(&["kill", &holder, "--grace", "1s"]);
}

/// The directory that a test uses to control a job, and the job directory of a
/// job.
///
/// # Why these tests simulate the kill and do not make one
///
/// A genuine kill by the out-of-memory killer needs one of two things: a cgroup
/// memory limit, or a machine with no free memory. qex can apply a limit only
/// when the coordinator owns its cgroup, and a usual test machine gives the
/// login session to the root user, so the limit is not available. A test that
/// fills the machine is worse: other work operates on the same machine, and the
/// kernel chooses its victim itself.
///
/// These tests therefore make the EVIDENCE that a true kill leaves — the
/// out-of-memory record in the job directory — and the job then stops itself
/// with the same signal that the kernel uses, `SIGKILL`. Every step after that
/// point is the true code: the classification, the new claim, the new attempt,
/// the words in the record, and the learner. The unit tests in `src/enforce.rs`
/// and `src/supervisor.rs` cover the reading of the cgroup counter itself.
///
/// The record says `job`, which is the evidence of a cgroup that qex made for
/// this job. qex acts on that evidence only, so the test must produce it. On a
/// machine with a delegated cgroup, `[enforce] mode` gives it. These tests must
/// not use that mode: qex then starts the coordinator again in a systemd unit,
/// which does not receive the `XDG_*` values of the test, and the test would
/// use the state directory of the user.
struct OomJob {
    control: PathBuf,
}

impl OomJob {
    fn new(h: &Harness) -> Self {
        let control = h.root.join("control");
        std::fs::create_dir_all(&control).unwrap();
        Self { control }
    }

    /// Tells the job where its own directory is. The job then starts.
    fn release(&self, h: &Harness, id: &str) {
        let dir = h.root.join("state/qex/jobs").join(id);
        assert!(
            dir.is_dir(),
            "the job directory {} is missing",
            dir.display()
        );
        std::fs::write(self.control.join("dir"), dir.to_string_lossy().as_bytes()).unwrap();
    }
}

/// A job that a USER stopped must NOT run again, and it must teach the learner
/// nothing.
///
/// This test protects the feature from doing harm. The kernel and `qex kill`
/// both use `SIGKILL`. A job that somebody stopped on purpose must never run
/// again with a larger claim: qex would repeat work that the user stopped, and
/// it would also record that the command needs more memory than it does.
#[test]
fn a_job_that_a_user_killed_is_not_retried_and_teaches_the_learner_nothing() {
    let h = Harness::new(
        "userkill",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [budget]\ncpu = \"2\"\nmem = \"1GB\"\n",
    );

    // Give the job the evidence of a kill for memory as well.
    //
    // This step is the point of the test. qex reads the out-of-memory count of
    // the SESSION when it applies no limit, and that count also holds a kill in
    // a different program of the same user. The mark from `qex kill` must win
    // against that evidence. Without the mark, this job looks like a job that
    // the kernel stopped for memory, and qex starts it again with a larger
    // claim.
    let job = OomJob::new(&h);
    let control = h.root.join("control");
    let script = format!(
        "until [ -f {c}/dir ]; do sleep 0.1; done; echo job > \"$(cat {c}/dir)/oom\"; \
         echo ready > {c}/ready; sleep 60",
        c = control.display()
    );
    let id = h.submit(&["submit", "--mem", "128MB", "--", "bash", "-c", &script]);
    h.until("the job operates", Duration::from_secs(30), || {
        h.state_of(&id) == "running"
    });
    job.release(&h, &id);
    h.until("the job made the record", Duration::from_secs(30), || {
        control.join("ready").exists()
    });

    // Use KILL, which is the signal that the out-of-memory killer uses. With
    // TERM the two causes are already separate.
    h.ok(&["kill", &id, "--signal", "KILL", "--grace", "1s"]);
    h.until("the job stops", Duration::from_secs(30), || {
        h.status_json(&id)["state"]
            .as_str()
            .map(|s| s != "running" && s != "starting")
            .unwrap_or(false)
    });

    let s = h.status_json(&id);
    assert_eq!(s["state"], "killed", "a command stopped this job: {s}");
    assert_eq!(s["attempts"], 1, "qex must not start the job again: {s}");
    // The record carries NO count of raises. qex raises no claim, so a field
    // that counts the raises names a behaviour that went, and every agent that
    // reads `qex status --json` gets it.
    assert!(
        s.get("oom_raises").is_none(),
        "the record must hold no count of raises: {s}"
    );
    assert_eq!(
        s["mem"].as_u64().unwrap(),
        128 * 1024 * 1024,
        "the claim must not change: {s}"
    );

    // The learner must hold nothing. The memory that a job reached before
    // somebody stopped it says nothing about the memory that it needs.
    let store = h.root.join("state/qex/usage.json");
    assert!(
        !store.exists(),
        "a job that a command stopped must teach the learner nothing, and the store holds: {}",
        std::fs::read_to_string(&store).unwrap_or_default()
    );
}

/// NO SHIPPED WORD MAY PROMISE THE MEMORY LIMIT THAT WENT.
///
/// qex limits no job, it raises no claim, and it starts no attempt of its own
/// after a kill for memory. A reader who finds one of these words believes a
/// promise that no code holds.
///
/// This test reads the text that SHIPS — the documentation, the skill file, the
/// help topics, the command line help and the JSON schema — and refuses the
/// words of that feature. A later change that writes one of them meets this
/// test and not a reader.
///
/// THE PAGES COME FROM THE DIRECTORY, and not from a list. A list covers the
/// pages that exist on the day that somebody writes it, and a page that joins
/// the product after that day gets no gate. A file that this test cannot read
/// is a FAILURE and not a page to pass over: a gate that reads nothing refuses
/// nothing, and it reports success.
///
/// # Why this gate reads five files of `src/` and not all of them
///
/// It reads the files that hold text FOR A READER, and A FIELD NAME IS SUCH
/// TEXT: `src/job.rs` declares the record, and serde gives every field of it to
/// `qex status --json`. A field that counts the raises after a kill for memory
/// thus reached every agent, while a comment beside it said that no command
/// prints it. The name of a field is shipped text.
///
/// The gate must not read the rest, because a word that names a key that went
/// HAS TO appear in the code that refuses that key: `src/config.rs` holds
/// `mem_overcommit`, `use_systemd` and `[retry]` so that a file which sets one
/// of them gets an answer, and `src/usage.rs` holds `lower-bound` so that qex
/// can read the file of an earlier version. A gate over every file needs a list
/// of exceptions, and such a list grows until it hides the thing that the gate
/// looks for.
///
/// EACH REFUSED WORD STAYS BROAD. A narrow word looks safer, and it is not: a
/// gate that misses says nothing, and a gate that fires is a question that an
/// author answers in a minute. `[retry]` and "the new claim" are broad for that
/// reason. Both carried real text of the feature that went, and that text was
/// INLINE — a table row wrote `` `[retry] growth` `` with no line end, and a
/// help topic wrote "the new claim." with a full stop. A form that asks for the
/// end of the line, or for the word that follows, reads past both of them.
#[test]
fn no_shipped_word_promises_the_limit_that_went() {
    // Each word, and why it may not come back.
    let refused = [
        ("mem_overcommit", "the multiplier for a second memory limit"),
        ("use_systemd", "a coordinator that restarts for a cgroup"),
        ("on_oom", "the count of raises after a kill for memory"),
        ("GOMEMLIMIT", "a memory hint for Go"),
        ("max-old-space-size", "a heap limit for node"),
        ("mode = \"soft\"", "a mode that went"),
        ("mode = \"hard\"", "a mode that went"),
        // The words of the learner that went with the limit. A gate that names
        // the config keys only passed over two whole sections of prose.
        ("lower bound", "a measurement that qex no longer writes"),
        ("lower-bound", "the same, in the words of the file"),
        ("cgroup of the job", "a cgroup that qex no longer makes"),
        ("raises the claim", "a correction that qex no longer makes"),
        ("raise the claim", "the same"),
        // The section name, with nothing after it. A page named the keys of
        // this section in a table, in the middle of a line.
        ("[retry]", "a section that went"),
        // The words of the raise, in the form that carries no verb. A gate
        // that names the verbs only passes over a description of the record,
        // and over a help topic that ended the sentence here with a full stop.
        ("the new claim", "a claim that qex no longer makes"),
        ("claim that failed", "the same, in the words of a record"),
    ];

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut pages: Vec<std::path::PathBuf> = vec![
        root.join("README.md"),
        root.join("skills/qex/SKILL.md"),
        root.join("CONTRIBUTING.md"),
        root.join("SECURITY.md"),
        // The help topics, the command line help and the JSON schema ship
        // INSIDE the program, so read them from the source of the program.
        root.join("src/help.rs"),
        root.join("src/cli.rs"),
        root.join("src/schema.rs"),
        // The record itself. Every field name goes to `qex status --json`.
        root.join("src/job.rs"),
    ];
    // Every page of `docs/` ships. Read the directory, so that a new page gets
    // this gate on the day that somebody writes it.
    let mut docs: Vec<std::path::PathBuf> = std::fs::read_dir(root.join("docs"))
        .expect("the docs directory must be readable")
        .map(|e| {
            e.expect("each entry of the docs directory must be readable")
                .path()
        })
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    docs.sort();
    assert!(
        docs.len() >= 6,
        "the gate must read every page of docs/, and it found {}: {docs:?}",
        docs.len()
    );
    pages.append(&mut docs);

    for page in pages {
        let text = std::fs::read_to_string(&page)
            .unwrap_or_else(|e| panic!("the gate must read {}: {e}", page.display()));
        for (word, what) in refused {
            assert!(
                !text.contains(word),
                "{} names `{word}` ({what}). qex limits no job, so no shipped word may \
                 promise that it does.",
                page.display()
            );
        }
    }
}

/// A configuration that names a key of an earlier qex must say what to do.
///
/// A reader wrote that key because an earlier qex asked for it. "unknown field"
/// is true and it names no remedy, so qex names the key, says what it does now,
/// and gives the step.
///
/// THE ANSWER MUST GIVE THE REMEDY FOR THE KEY THAT THE FILE HOLDS. Every
/// message here says "does not limit a job", so a test for those words alone
/// passes when qex gives the answer of one key for a different key.
///
/// A test that the message merely NAMES the key is not enough either. The
/// answer for a bad value of `[enforce] mode` carries the words of the parser,
/// and those words hold the name of whatever the file wrote. This test thus
/// asks for the STEP that the reader must take, which only the right answer
/// holds.
///
/// THE ANSWER FOR A VALUE MUST NAME NO KEY. serde writes ``unknown variant `X`
/// `` both for a VALUE that a key refuses and for a key in an array of tables,
/// and the two messages are the same. A rule that reads that form as the name
/// of a key thus answers `[enforce] mode = "retry"`, a file that is well
/// formed, with the step "Delete `retry`". That file holds no `retry` key. The
/// last row holds that rule out.
#[test]
fn a_config_that_names_a_key_that_went_says_what_to_do() {
    for (section, text, must_say, must_not_say) in [
        (
            "[enforce]",
            "mem_overcommit = 1.5",
            "Delete `mem_overcommit`",
            None,
        ),
        (
            "[enforce]",
            "use_systemd = true",
            "Delete `use_systemd`",
            None,
        ),
        ("[retry]", "on_oom = 2", "Delete `retry`", None),
        ("[enforce]", "mode = \"hard\"", "Use `cooperative`", None),
        // A VALUE, and not a key. The file is well formed and it holds no key
        // that went, so qex names `[enforce] mode`, which the file DOES hold,
        // and it asks the reader to delete nothing.
        (
            "[enforce]",
            "mode = \"retry\"",
            "Use `cooperative`",
            Some("Delete `retry`"),
        ),
    ] {
        let h = Harness::new("wentkey", &format!("{section}\n{text}\n"));
        let out = h.qex(&["config", "show"]);
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_ne!(out.status.code(), Some(0), "qex must refuse: {said}");
        assert!(
            said.contains("does not limit a job"),
            "the answer must say what qex does now, for `{section} {text}`: {said}"
        );
        assert!(
            said.contains(must_say),
            "the answer must give the step `{must_say}`, for `{section} {text}`: {said}"
        );
        if let Some(forbidden) = must_not_say {
            assert!(
                !said.contains(forbidden),
                "the answer must not say `{forbidden}`, because the file holds no such \
                 key, for `{section} {text}`: {said}"
            );
        }
    }
}

/// `qex config show` gives the values IN FORCE, in both of its forms.
///
/// `[enforce] mode` decides the budget, the reserve and the search for peers
/// when the config file names none of them. The struct then holds an empty
/// sentinel for each one. A reader of either form must never see that sentinel:
/// an agent that reads `""` for the budget cannot size a claim, and it cannot
/// recover the number that qex uses.
///
/// The pure function that fills the sentinels has its own test. THIS test reads
/// the two forms that a reader gets, because a correct function that no command
/// calls answers nobody.
#[test]
fn config_show_gives_the_values_in_force_in_both_forms() {
    for (name, mode, budget, reserve, peers, label) in [
        (
            "forcecoop",
            "cooperative",
            "75%",
            "2GB",
            true,
            "mode:         cooperative",
        ),
        (
            "forcealone",
            "single-user",
            "90%",
            "512MB",
            false,
            "mode:         single-user",
        ),
    ] {
        let h = Harness::new(name, &format!("[enforce]\nmode = \"{mode}\"\n"));

        // THE JSON FORM. An agent reads this one.
        let out = h.ok(&["config", "show", "--json"]);
        let cfg: serde_json::Value =
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("{name}: {e}: {out}"));
        assert_eq!(
            cfg["budget"]["cpu"], budget,
            "{mode}: the JSON form must give the budget in force: {out}"
        );
        assert_eq!(
            cfg["budget"]["mem"], budget,
            "{mode}: the JSON form must give the budget in force: {out}"
        );
        assert_eq!(
            cfg["system"]["reserve_mem"], reserve,
            "{mode}: the JSON form must give the reserve in force: {out}"
        );
        assert_eq!(
            cfg["peers"]["enabled"], peers,
            "{mode}: the JSON form must say if qex looks for peers: {out}"
        );

        // THE TEXT FORM. A person reads this one, and the line must name the
        // MODE. A line that named the machine told the reader nothing that
        // this reader can change.
        let text = h.ok(&["config", "show"]);
        assert!(
            text.contains(label),
            "{mode}: the text form must name the mode as `{label}`: {text}"
        );
    }
}

/// A RERUN TAKES THE CLAIM OF THE SPECIFICATION, and not the claim of a record.
///
/// For a job that this qex submits the two agree. They disagree for a record of
/// an EARLIER qex, which could write a larger number in the record after a kill
/// for memory. A rerun that took that number would carry `learned` with it, and
/// tell the reader that a job measured a value that qex invented.
///
/// This test writes the record of such a job, because this version cannot make
/// one. It is a test of what `rerun` READS, and the shape it reads from disk is
/// the shape that an earlier qex left there.
#[test]
fn a_rerun_takes_the_claim_of_the_specification_and_not_of_the_record() {
    let h = Harness::new("rerunclaim", "[peers]\nenabled = false\n");

    let first = h.submit(&["submit", "--cpu", "1", "--mem", "300MB", "--", "true"]);
    h.ok(&["wait", &first, "--timeout", "60s"]);

    // Put the record in the shape that an earlier qex left after a raise: the
    // claim of the record is above the claim that the submission asked for.
    let record = h.root.join(format!("state/qex/jobs/{first}/status.json"));
    let text = std::fs::read_to_string(&record).expect("the record must be readable");
    let mut value: serde_json::Value =
        serde_json::from_str(&text).expect("the record must be JSON");
    value["mem"] = serde_json::json!(2u64 << 30);
    value["cpu"] = serde_json::json!(4);
    value["claim_source"] = serde_json::json!("learned");
    std::fs::write(&record, value.to_string()).expect("the record must be writable");

    // THE INSTRUMENT MUST PROVE ITSELF, or this test judges nothing. Read the
    // FILE back, and not `qex status`: that command asks the coordinator, which
    // answers from the record that it holds in memory, and `rerun` reads the
    // file. A test that asked the coordinator here would find its own write
    // missing and blame the product.
    let back: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record).expect("the record must be read"))
            .expect("the record must be JSON");
    assert_eq!(
        back["mem"], 2147483648u64,
        "this test needs a record whose claim is above the specification: {back}"
    );

    let out = h.ok(&["rerun", &first]);
    let second = out.split_whitespace().last().unwrap().to_string();
    h.ok(&["wait", &second, "--timeout", "60s"]);

    let s = h.status_json(&second);
    assert_eq!(
        s["mem"], 314572800u64,
        "the rerun must take the 300MB of the specification, and not the claim of the \
         record: {s}"
    );
    assert_eq!(
        s["cpu"], 1,
        "the rerun must take the cores of the specification: {s}"
    );
    assert_eq!(
        s["claim_source"], "explicit",
        "the claim came from a person, so the record must not say that a job measured it: {s}"
    );
}

/// qex makes NO cgroup for a job, in any configuration.
///
/// This is the property that the removal must hold. A test of the messages
/// alone would pass while the code still made a cgroup for each job.
#[test]
fn qex_makes_no_cgroup_for_a_job() {
    let h = Harness::new(
        "nocgroup",
        "[peers]\nenabled = false\n[enforce]\nmode = \"single-user\"\n",
    );
    let id = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    h.ok(&["wait", &id, "--timeout", "60s"]);

    // The record of the job must name no cgroup, and no file of the job may
    // hold one. `record_cgroup_path` wrote that file.
    let dir = h.root.join("state/qex/jobs").join(&id);
    assert!(
        !dir.join("cgroup").exists(),
        "a job must have no cgroup file: {}",
        dir.display()
    );

    let s = h.status_json(&id);
    assert_eq!(s["state"], "completed", "got: {s}");
}

/// No hint in the environment of a job carries memory.
///
/// qex limits no job, so it must not tell a runtime to limit itself. A hint
/// that named a memory value would hold a Go job and a node job to the claim
/// and leave every other program free.
#[test]
fn no_hint_in_the_environment_of_a_job_carries_memory() {
    let h = Harness::new("nomemhint", "[peers]\nenabled = false\n");
    let id = h.submit(&[
        "submit",
        "--cpu",
        "2",
        "--mem",
        "512MB",
        "--",
        "sh",
        "-c",
        "echo \"GOMAXPROCS=$GOMAXPROCS QEX_MEM=$QEX_MEM \
         GOMEMLIMIT=[$GOMEMLIMIT] NODE_OPTIONS=[$NODE_OPTIONS]\"",
    ]);
    h.ok(&["wait", &id, "--timeout", "60s"]);
    let out = h.qex(&["logs", &id]);
    let said = String::from_utf8_lossy(&out.stdout).to_string();

    assert!(
        said.contains("GOMAXPROCS=2"),
        "a core hint must reach the job: {said}"
    );
    assert!(
        said.contains("QEX_MEM=536870912"),
        "the claim must reach the job as a value to read: {said}"
    );
    assert!(
        said.contains("GOMEMLIMIT=[]"),
        "qex must write no memory limit for Go: {said}"
    );
    assert!(
        said.contains("NODE_OPTIONS=[]"),
        "qex must write no heap limit for node: {said}"
    );
}

/// A kill for memory is REPORTED, and qex acts on it in no way.
///
/// qex reads the out-of-memory count of the cgroup of its own process. That
/// count also rises when the kernel stops a DIFFERENT program of the same user,
/// and a machine that is short of memory is also the machine on which a person
/// uses `kill -9`, so the two events arrive together.
///
/// qex therefore names the state `oom` and stops. It starts no new attempt, it
/// changes no claim, and it teaches the learner nothing: the machine can be
/// full while the claim of this job is correct.
#[test]
fn a_kill_for_memory_is_reported_and_qex_acts_on_it_in_no_way() {
    let h = Harness::new(
        "oomsession",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [budget]\ncpu = \"2\"\nmem = \"1GB\"\n",
    );
    let job = OomJob::new(&h);
    let control = h.root.join("control");

    let script = format!(
        "until [ -f {c}/dir ]; do sleep 0.1; done; echo 1 > \"$(cat {c}/dir)/oom\"; \
         kill -9 $$",
        c = control.display()
    );
    let id = h.submit(&["submit", "--mem", "128MB", "--", "bash", "-c", &script]);
    job.release(&h, &id);

    let out = h.qex(&["wait", &id, "--timeout", "60s"]);
    assert_eq!(out.status.code(), Some(99), "got: {out:?}");

    let s = h.status_json(&id);
    // The report is correct: the kernel stopped the job for memory.
    assert_eq!(s["state"], "oom", "got: {s}");
    // No action may follow it.
    assert_eq!(s["attempts"], 1, "qex must not start the job again: {s}");
    assert_eq!(
        s["mem"].as_u64().unwrap(),
        128 * 1024 * 1024,
        "the claim must not change: {s}"
    );

    let note = s["error"].as_str().unwrap_or("");
    assert!(
        note.contains("no count that belongs to this job alone"),
        "the record must say what qex holds: {note}"
    );
    assert!(
        note.contains("THE CLAIM CAN BE CORRECT"),
        "the record must not send a reader to raise a claim that was right: {note}"
    );
    assert!(
        !note.contains("[enforce] mode"),
        "no setting gives qex a count for one job, so the record must name none: {note}"
    );

    // The learner must hold nothing. A job that did not COMPLETE is not a
    // measurement of the memory that the work needs.
    let store = h.root.join("state/qex/usage.json");
    assert!(
        !store.exists(),
        "a kill for memory must teach the learner nothing, and the store holds: {}",
        std::fs::read_to_string(&store).unwrap_or_default()
    );
}

/// A mark that says how an attempt stopped must not stay for the next attempt.
///
/// A job with `--retries` can stop in two different ways. `qex kill` stopped
/// attempt 1 here, and attempt 2 ran to its end. The mark of the first attempt
/// stayed before, so attempt 2 said that a command stopped it. The record then
/// named a cause that no command made.
#[test]
fn a_mark_from_one_attempt_does_not_decide_the_next_attempt() {
    let h = Harness::new(
        "oommarks",
        "[peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [budget]\ncpu = \"2\"\nmem = \"1GB\"\n",
    );
    let c = h.root.join("control");
    std::fs::create_dir_all(&c).unwrap();

    let script = format!(
        "n=$(cat {c}/n 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}/n; \
         if [ $n -eq 1 ]; then trap 'exit 3' TERM; touch {c}/ready; sleep 60 & wait; fi; \
         exit 0",
        c = c.display()
    );

    let id = h.submit(&[
        "submit",
        "--retries",
        "2",
        "--mem",
        "128MB",
        "--cpu",
        "1",
        "--",
        "bash",
        "-c",
        &script,
    ]);

    // Stop attempt 1 with a command. The job answers with the exit code 3, so
    // its state is `failed` and `--retries` starts it again.
    h.until("attempt 1 operates", Duration::from_secs(30), || {
        c.join("ready").exists()
    });
    h.ok(&["kill", &id, "--signal", "TERM", "--grace", "30s"]);

    h.ok(&["wait", &id, "--timeout", "90s"]);
    let s = h.status_json(&id);

    assert_eq!(
        s["state"], "completed",
        "the mark of attempt 1 must not decide attempt 2: {s}"
    );
    assert_eq!(s["attempts"], 2, "one kill, then one run: {s}");
    assert_eq!(
        s["retries_left"], 1,
        "attempt 1 must spend one `--retries` credit: {s}"
    );
}

// ---------------------------------------------------------------------------
// The order of the queue, and the job at the front that cannot start.
//
// The fault that these tests prevent is issue #10: one job that could not be
// admitted, because another user held part of the budget, kept two small jobs
// in the queue for ever, and the reason named no cause.
// ---------------------------------------------------------------------------

/// Makes a directory that two coordinators share for their peer records.
///
/// The directory needs the sticky bit, or qex refuses it and each coordinator
/// then operates for one user only — and the test measures nothing.
fn shared_peer_dir(tag: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("qxpeers-{}-{tag}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
    dir
}

/// The config of two coordinators that share one machine and one peer
/// directory. Each one counts the claims of the other.
fn peer_config(dir: &Path, cpu: &str, max_bypass: u32) -> String {
    format!(
        "[budget]\ncpu = \"{cpu}\"\nmem = \"2GB\"\n\
         [peers]\nenabled = true\ndir = \"{}\"\nstale_after = \"1h\"\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [queue]\nmax_bypass = {max_bypass}\n",
        dir.display()
    )
}

fn blocked_reason(h: &Harness, id: &str) -> String {
    h.status_json(id)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// THE MEASURED FAULT of issue #10.
///
/// A job that cannot start because another user holds part of the budget must
/// not keep the jobs behind it in the queue. qex does not control that user, so
/// an empty machine gives the job at the front nothing and stops every other
/// job. The earlier code parked the whole queue and told each job behind it
/// "waits for the job at the front of the queue", which names no cause.
#[test]
fn a_job_that_another_user_holds_back_does_not_park_the_jobs_behind_it() {
    let dir = shared_peer_dir("hol-a");
    let config = peer_config(&dir, "4", 2);
    let other = Harness::new("holpeerb", &config);
    let mine = Harness::new("holpeera", &config);

    // The other user takes 3 of the 4 cores of the budget.
    let held = other.submit(&[
        "submit", "--cpu", "3", "--mem", "64MB", "--", "sleep", "300",
    ]);
    other.until(
        "the other user's job starts",
        Duration::from_secs(45),
        || other.state_of(&held) == "running",
    );

    // This job needs the whole budget, so the other user's claim stops it.
    let big = mine.submit(&["submit", "--cpu", "4", "--mem", "64MB", "--", "true"]);
    mine.until(
        "the job at the front names the other user",
        Duration::from_secs(45),
        || blocked_reason(&mine, &big).contains("another user holds capacity"),
    );

    // These two jobs are behind it, and they fit the free capacity.
    let a = mine.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    let b = mine.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);

    mine.until(
        "the two small jobs behind the front of the queue run",
        Duration::from_secs(60),
        || mine.state_of(&a) == "completed" && mine.state_of(&b) == "completed",
    );

    // The job at the front still waits, and its reason gives the cause and a
    // remedy. It must never read as a queue position.
    assert_eq!(mine.state_of(&big), "queued");
    let reason = blocked_reason(&mine, &big);
    assert!(
        reason.contains("another user holds capacity") && reason.contains("no known end"),
        "the reason must name the other user: {reason}"
    );
    assert!(
        !reason.contains("front of the queue"),
        "the job at the front must not read a position sentence: {reason}"
    );

    // `qex info` must say the same thing in one line.
    let info: serde_json::Value = serde_json::from_str(&mine.ok(&["info", "--json"])).unwrap();
    assert_eq!(info["queue_state"], "waits-for-peer", "info: {info}");
    assert!(
        info["peer_cpu"].as_u64().unwrap_or(0) >= 3,
        "qex info must report the cores of the other user: {info}"
    );

    other.ok(&["kill", &held, "--grace", "1s"]);
    mine.qex(&["cancel", &big]);
    std::fs::remove_dir_all(&dir).ok();
}

/// A job behind a job that keeps NO capacity gets a reason of its own.
///
/// The earlier code stopped at the first job that could not start, so a job
/// behind it was never tested. Each one read "waits for the job X at the front
/// of the queue", which names no cause — and a job that the other user also
/// held back was told the wrong thing.
#[test]
fn a_job_behind_a_job_that_keeps_no_capacity_gets_its_own_reason() {
    let dir = shared_peer_dir("hol-own");
    let config = peer_config(&dir, "4", 2);
    let other = Harness::new("holownb", &config);
    let mine = Harness::new("holowna", &config);

    // The other user takes 3 of the 4 cores.
    let held = other.submit(&[
        "submit", "--cpu", "3", "--mem", "64MB", "--", "sleep", "300",
    ]);
    other.until(
        "the other user's job starts",
        Duration::from_secs(45),
        || other.state_of(&held) == "running",
    );

    // The job at the front needs the whole budget. The other user stops it, so
    // qex keeps no capacity for it.
    let big = mine.submit(&[
        "submit", "--cpu", "4", "--mem", "64MB", "--", "sleep", "300",
    ]);
    mine.until(
        "the job at the front names the other user",
        Duration::from_secs(45),
        || blocked_reason(&mine, &big).contains("another user holds capacity"),
    );

    // The first job behind it fits and starts. The second one does not fit,
    // because the other user holds the rest of the budget.
    let a = mine.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
    ]);
    let b = mine.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
    ]);
    mine.until(
        "the first job behind starts",
        Duration::from_secs(45),
        || mine.state_of(&a) == "running",
    );

    mine.until(
        "the second job behind gives a reason of its own",
        Duration::from_secs(45),
        || !blocked_reason(&mine, &b).is_empty(),
    );
    let reason = blocked_reason(&mine, &b);
    assert!(
        reason.contains("another user holds capacity"),
        "the job behind must learn the true cause: {reason}"
    );
    assert!(
        !reason.contains("front of the queue"),
        "a job behind a job that keeps no capacity must not read a position: {reason}"
    );

    other.ok(&["kill", &held, "--grace", "1s"]);
    for id in [&big, &a, &b] {
        mine.qex(&["kill", id, "--grace", "1s"]);
        mine.qex(&["cancel", id]);
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// A job that another user held back becomes unpassable in the same cycle in
/// which the holder becomes a job of this queue.
///
/// This is the subtle half of the rule, and a reviewer must look for it. The
/// count of the jobs that passed the job at the front is NOT reset when the
/// holder changes, and it is not reset when the other user releases the
/// capacity. Without the carry over, the count starts again at zero at each
/// change of the holder, and a stream of small jobs passes the job at the front
/// for ever — the starvation that the strict order existed to prevent.
#[test]
fn a_job_blocked_by_a_peer_becomes_unpassable_when_the_holder_changes() {
    let dir = shared_peer_dir("hol-b");
    let config = peer_config(&dir, "4", 2);
    let other = Harness::new("holcarryb", &config);
    let mine = Harness::new("holcarrya", &config);

    // The other user takes 2 of the 4 cores.
    let held = other.submit(&[
        "submit", "--cpu", "2", "--mem", "64MB", "--", "sleep", "300",
    ]);
    other.until(
        "the other user's job starts",
        Duration::from_secs(45),
        || other.state_of(&held) == "running",
    );

    // This job needs 3 cores. The other user holds 2, so it cannot start, and
    // qex must keep no capacity for it.
    let big = mine.submit(&[
        "submit", "--cpu", "3", "--mem", "64MB", "--", "sleep", "300",
    ]);
    mine.until(
        "the job at the front names the other user",
        Duration::from_secs(45),
        || blocked_reason(&mine, &big).contains("another user holds capacity"),
    );
    assert_eq!(
        mine.status_json(&big)["passed_by"].as_u64(),
        Some(0),
        "no job passed it yet"
    );

    // Three small jobs. Two of them pass the job at the front while the other
    // user holds the capacity. The count then reaches the limit.
    let small: Vec<String> = (0..3)
        .map(|_| {
            mine.submit(&[
                "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
            ])
        })
        .collect();

    mine.until(
        "two small jobs pass the job at the front",
        Duration::from_secs(60),
        || mine.state_of(&small[0]) == "running" && mine.state_of(&small[1]) == "running",
    );

    // The holder is now a job of THIS queue, and the count carried across that
    // change. The job at the front is therefore unpassable at once.
    mine.until(
        "the queue keeps the capacity",
        Duration::from_secs(45),
        || {
            let info: serde_json::Value =
                serde_json::from_str(&mine.ok(&["info", "--json"])).unwrap_or_default();
            info["queue_state"] == "held"
        },
    );
    assert_eq!(
        mine.status_json(&big)["passed_by"].as_u64(),
        Some(2),
        "the count must carry across the change of the holder, and not start again"
    );

    // The other user releases the capacity. That release must not give the
    // queue a new licence to pass the job at the front.
    other.ok(&["kill", &held, "--grace", "1s"]);

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        assert_eq!(
            mine.state_of(&small[2]),
            "queued",
            "a small job passed the job at the front after the other user released"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        mine.status_json(&big)["passed_by"].as_u64(),
        Some(2),
        "the release of the other user must not reset the count"
    );
    let reason = blocked_reason(&mine, &small[2]);
    assert!(
        reason.contains("qex keeps the capacity for that job"),
        "the job behind must read WHY qex holds it: {reason}"
    );
    assert!(
        blocked_reason(&mine, &big).contains("qex starts no other job before this one"),
        "the job at the front must say that it keeps the capacity"
    );

    // The job at the front collects the capacity as the jobs of this queue
    // stop. One core is sufficient.
    mine.ok(&["kill", &small[0], "--grace", "1s"]);
    mine.until(
        "the job at the front starts",
        Duration::from_secs(45),
        || mine.has_started(&big),
    );

    for id in &small {
        mine.qex(&["kill", id, "--grace", "1s"]);
        mine.qex(&["cancel", id]);
    }
    mine.qex(&["kill", &big, "--grace", "1s"]);
    std::fs::remove_dir_all(&dir).ok();
}

/// A stream of small jobs must not pass a large job for ever.
///
/// This is the reason that the strict order existed. The bypass is bounded, so
/// the rule that removes the head-of-line fault must not bring starvation back.
#[test]
fn a_stream_of_small_jobs_does_not_pass_a_large_job_for_ever() {
    let h = Harness::new(
        "holstarve",
        "[budget]\ncpu = \"4\"\nmem = \"2GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [queue]\nmax_bypass = 2\n",
    );

    // This job holds 2 of the 4 cores for the length of the test.
    let holder = h.submit(&[
        "submit", "--cpu", "2", "--mem", "64MB", "--", "sleep", "300",
    ]);
    h.until("the first job starts", Duration::from_secs(45), || {
        h.state_of(&holder) == "running"
    });

    // This job needs the whole budget, so the job above stops it. The jobs of
    // this queue hold the capacity, and qex schedules their release.
    let big = h.submit(&["submit", "--cpu", "4", "--mem", "64MB", "--", "true"]);

    // Six small jobs, each of which stops immediately. Without a bound, all six
    // pass the large job.
    let small: Vec<String> = (0..6)
        .map(|_| h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]))
        .collect();

    let done = |h: &Harness| -> usize { small.iter().filter(|id| h.has_started(id)).count() };

    h.until(
        "two small jobs pass the large job",
        Duration::from_secs(60),
        || done(&h) >= 2,
    );

    // Measure many times. No third job may pass, although a core is free.
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        assert!(
            done(&h) <= 2,
            "more than 2 small jobs passed the large job, so the bypass has no bound"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(h.state_of(&big), "queued");

    // `qex info` must know when a job last started. The health line answers
    // "does the queue move", and it cannot answer it without this time.
    let info: serde_json::Value = serde_json::from_str(&h.ok(&["info", "--json"])).unwrap();
    assert!(
        info["last_start_at"].as_u64().is_some(),
        "qex info must report the time of the last start: {info}"
    );

    // The large job starts when the holder stops, and the rest follow.
    h.ok(&["kill", &holder, "--grace", "1s"]);
    h.until("the large job runs", Duration::from_secs(45), || {
        h.state_of(&big) == "completed"
    });
    h.until("each small job runs", Duration::from_secs(60), || {
        done(&h) == 6
    });

    // THE COUNT GOES BACK TO ZERO WHEN THE JOB STARTS.
    //
    // `start_job` is the one writer of these two fields for a job that starts.
    // A record that kept them would tell a reader that a job which already ran
    // is still at the front of the queue and still waiting.
    let record = h.status_json(&big);
    assert_eq!(
        record["passed_by"].as_u64(),
        Some(0),
        "a job that started waits for nothing: {record}"
    );
    assert!(
        record["blocked_since"].is_null(),
        "a job that started holds no blocked_since: {record}"
    );
}

/// `max_bypass = 0` gives the strict order of the earlier releases.
///
/// A site that needs the strict order of the earlier releases sets this value,
/// so the value must start NO job before the job at the front.
#[test]
fn max_bypass_zero_lets_no_job_pass_the_front_of_the_queue() {
    let h = Harness::new(
        "holstrict",
        "[budget]\ncpu = \"4\"\nmem = \"2GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [queue]\nmax_bypass = 0\n",
    );

    let holder = h.submit(&[
        "submit", "--cpu", "2", "--mem", "64MB", "--", "sleep", "300",
    ]);
    h.until("the first job starts", Duration::from_secs(45), || {
        h.state_of(&holder) == "running"
    });

    let big = h.submit(&["submit", "--cpu", "4", "--mem", "64MB", "--", "true"]);
    let small: Vec<String> = (0..3)
        .map(|_| h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]))
        .collect();

    h.until("the queue is held", Duration::from_secs(45), || {
        let info: serde_json::Value =
            serde_json::from_str(&h.ok(&["info", "--json"])).unwrap_or_default();
        info["queue_state"] == "held"
    });

    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        for id in &small {
            assert_eq!(
                h.state_of(id),
                "queued",
                "with max_bypass = 0 no job may pass the job at the front"
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        h.status_json(&big)["passed_by"].as_u64(),
        Some(0),
        "no job passed the job at the front"
    );

    h.ok(&["kill", &holder, "--grace", "1s"]);
    h.until("each job runs", Duration::from_secs(60), || {
        h.state_of(&big) == "completed" && small.iter().all(|id| h.has_started(id))
    });
}

/// A job that is larger than the budget, and that the config keeps in the
/// queue, must not park the jobs behind it.
///
/// Such a job never starts, so capacity that qex keeps for it gives it nothing.
/// This was the second head-of-line path: with `oversized = "queue"` the job
/// stopped every job behind it FOR EVER.
#[test]
fn a_job_that_the_config_parks_does_not_park_the_jobs_behind_it() {
    let h = Harness::new(
        "holparked",
        "[budget]\ncpu = \"2\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [queue]\noversized = \"queue\"\n",
    );

    let big = h.submit(&["submit", "--cpu", "64", "--mem", "64MB", "--", "true"]);

    // THREE jobs behind it, and `max_bypass` is 2.
    //
    // The third job is what gives this test its power, and a later change must
    // not remove it. With two jobs only, both of them start whether or not
    // `OversizedParked` keeps capacity, because two bypasses are permitted:
    // the test would then pass with the fault and without it. The third job
    // can start ONLY when the parked job keeps no capacity at all.
    //
    // This is the class that matters most. A job that the config parks can
    // NEVER start, so a change that lets it keep capacity parks the queue for
    // ever — the exact defect that this branch exists to remove.
    let behind: Vec<String> = (0..3)
        .map(|_| h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]))
        .collect();

    h.until(
        "every job behind the large job runs",
        Duration::from_secs(60),
        || behind.iter().all(|id| h.state_of(id) == "completed"),
    );

    assert_eq!(h.state_of(&big), "queued");
    let reason = blocked_reason(&h, &big);
    assert!(
        reason.contains("keeps this job in the queue"),
        "the reason must name the config file: {reason}"
    );
    assert!(
        reason.contains("starts the jobs behind it"),
        "the reason must say that the queue continues: {reason}"
    );
}

// ---------------------------------------------------------------------------
// Pools: GPUs, VRAM and counted locks
// ---------------------------------------------------------------------------

/// The config that these tests use.
///
/// THE MACHINE THAT RUNS THIS TEST HAS NO GPU. That is the point of the
/// feature: a pool is a promise from the configuration, and not a probe of a
/// driver, so every test below must pass with no card in the machine.
fn pool_config(devices: &str, extra: &str) -> String {
    format!(
        "[budget]\ncpu = \"8\"\nmem = \"4GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [[pool]]\nname = \"gpu\"\nsize = \"vram\"\ndevices = [{devices}]\n\
         env = \"CUDA_VISIBLE_DEVICES\"\n{extra}"
    )
}

/// A machine with no GPU must schedule a GPU claim from the configuration, and
/// it must give the job the index and the environment variable.
///
/// Without this, qex would need a driver library to schedule a promise, and a
/// build machine could not run the test at all.
#[test]
fn a_machine_with_no_gpu_schedules_a_gpu_claim_from_the_configuration() {
    let h = Harness::new("gpupromise", &pool_config("\"24GB\", \"24GB\"", ""));

    let id = h.submit(&[
        "submit",
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--gpu",
        "1",
        "--",
        "sh",
        "-c",
        "echo cuda=$CUDA_VISIBLE_DEVICES; echo qex=$QEX_GPU_DEVICES; echo vram=$QEX_GPU_VRAM",
    ]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    assert_eq!(h.state_of(&id), "completed");

    let out = h.ok(&["logs", &id, "--stdout"]);
    assert!(
        out.contains("cuda=0"),
        "the pool must write its variable: {out}"
    );
    assert!(
        out.contains("qex=0"),
        "every indexed pool gets QEX_..._DEVICES: {out}"
    );
    assert!(
        out.contains(&format!("vram={}", 24u64 << 30)),
        "a whole device gives its capacity: {out}"
    );

    // The record holds the same answer, and the record stays after the job.
    let status = h.status_json(&id);
    assert_eq!(status["assigned"]["gpu"]["devices"][0], 0);
    assert_eq!(status["assigned"]["gpu"]["units"], 1);
}

/// Two jobs that claim one GPU each must operate together on a pool of two,
/// and they must get DIFFERENT devices. A third job must wait.
///
/// A fault here gives two jobs one card, and both jobs then meet a memory
/// error on the device that qex said was free.
#[test]
fn two_gpu_jobs_get_different_devices_and_a_third_waits() {
    let h = Harness::new("gpushare", &pool_config("\"24GB\", \"24GB\"", ""));

    let ids: Vec<String> = (0..3)
        .map(|i| {
            h.submit(&[
                "submit",
                "--cpu",
                "1",
                "--mem",
                "64MB",
                "--gpu",
                "1",
                "--name",
                &format!("g{i}"),
                "--",
                "sleep",
                "60",
            ])
        })
        .collect();

    h.until("two GPU jobs operate", Duration::from_secs(45), || {
        ids.iter().filter(|id| h.state_of(id) == "running").count() == 2
    });

    let running: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| h.status_json(id))
        .filter(|s| s["state"] == "running")
        .collect();
    let mut devices: Vec<i64> = running
        .iter()
        .map(|s| s["assigned"]["gpu"]["devices"][0].as_i64().unwrap())
        .collect();
    devices.sort_unstable();
    assert_eq!(
        devices,
        vec![0, 1],
        "two jobs must hold two different devices"
    );

    // The third job waits, and its reason names the pool.
    let waiting = ids
        .iter()
        .find(|id| h.state_of(id) == "queued")
        .expect("one job must wait");
    let reason = h.status_json(waiting)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        reason.contains("gpu"),
        "the reason must name the pool: {reason}"
    );

    for id in &ids {
        h.qex(&["kill", id, "--grace", "1s"]);
    }
}

/// qex must never add the memory of the devices together.
///
/// Four devices of 24GB are not 96GB for one job. An arithmetic that says they
/// are admits a job that cannot run, and the job then fails on the card with a
/// message that names no cause.
#[test]
fn a_vram_claim_above_the_largest_device_is_refused_at_the_submission() {
    let h = Harness::new(
        "vramsum",
        &pool_config("\"24GB\", \"24GB\", \"24GB\", \"24GB\"", ""),
    );

    let out = h.qex(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--gpu", "2", "--vram", "40GB", "--", "true",
    ]);
    assert!(!out.status.success(), "qex must refuse this job");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("never start"),
        "the message must say that the job can never start: {err}"
    );
    assert!(
        err.contains("24GB"),
        "the message must name the largest device: {err}"
    );

    // No record must exist. A refusal at the submission gives no job id.
    assert!(
        h.list_json().is_empty(),
        "a refused job must leave no record"
    );
}

/// A claim above the pool total is a refusal, whatever `[queue] oversized`
/// says. An empty machine does not make a fifth device, so `run-when-idle`
/// cannot help this job.
#[test]
fn a_gpu_claim_above_the_pool_total_is_refused_whatever_the_oversized_policy() {
    let h = Harness::new(
        "gputoobig",
        &pool_config(
            "\"24GB\", \"24GB\"",
            "[queue]\noversized = \"run-when-idle\"\n",
        ),
    );

    let out = h.qex(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--gpu", "8", "--", "true",
    ]);
    assert!(!out.status.success(), "qex must refuse this job");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("never start"), "got: {err}");
}

/// The assignment must survive a coordinator that stops and starts while the
/// job still operates.
///
/// The assignment is in `status.json`, which the supervisor carries forward. A
/// new coordinator reads it and rebuilds the occupancy of the pool. Without
/// this, the new coordinator gives the same device to a second job.
#[test]
fn a_gpu_assignment_survives_a_coordinator_that_stops_and_starts() {
    let h = Harness::new("gpurecover", &pool_config("\"24GB\"", ""));

    let long = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--gpu", "1", "--", "sleep", "60",
    ]);
    h.until("the GPU job operates", Duration::from_secs(45), || {
        h.state_of(&long) == "running"
    });
    let device = h.status_json(&long)["assigned"]["gpu"]["devices"][0]
        .as_i64()
        .unwrap();

    // Take the pid from the coordinator itself. A search of the process list
    // also matches the command that holds the letters `qex`.
    let pid = h.coordinator_pid();
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    h.until("the coordinator stops", Duration::from_secs(20), || {
        (unsafe { libc::kill(pid, 0) }) != 0
    });

    // The next command starts a new coordinator. It reads the records, so the
    // job still holds its device and a second job must wait.
    let second = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--gpu", "1", "--", "true",
    ]);
    assert_ne!(h.coordinator_pid(), pid, "a new coordinator must operate");

    assert_eq!(
        h.status_json(&long)["assigned"]["gpu"]["devices"][0]
            .as_i64()
            .unwrap(),
        device,
        "the assignment must not change when a coordinator restarts"
    );

    // Give the new coordinator time to test the queue, then read the state.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        h.state_of(&second),
        "queued",
        "the only device is in use, so the second job must wait"
    );

    h.qex(&["kill", &long, "--grace", "1s"]);
}

/// `--lock NAME` must behave exactly as before: no configuration, and two jobs
/// with one name never operate together.
#[test]
fn a_lock_needs_no_configuration_and_still_excludes() {
    let h = Harness::new(
        "lockstill",
        "[budget]\ncpu = \"8\"\nmem = \"4GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );

    let first = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--lock", "target", "--", "sleep", "60",
    ]);
    h.until("the first job operates", Duration::from_secs(45), || {
        h.state_of(&first) == "running"
    });

    let second = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--lock", "target", "--", "true",
    ]);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        h.state_of(&second),
        "queued",
        "two jobs with one lock name must never operate together"
    );
    let reason = h.status_json(&second)["blocked_reason"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(reason.contains("lock `target`"), "got: {reason}");

    // A lock does not park the queue. A job with no lock must pass it.
    let free = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    h.ok(&["wait", &free, "--timeout", "45s"]);
    assert_eq!(h.state_of(&free), "completed");

    h.qex(&["kill", &first, "--grace", "1s"]);
    h.ok(&["wait", &second, "--timeout", "45s"]);
    assert_eq!(h.state_of(&second), "completed");
}

/// A counted lock gives "at most N jobs that use this thing".
#[test]
fn a_counted_pool_lets_n_jobs_operate_and_makes_the_next_one_wait() {
    let h = Harness::new(
        "netpool",
        "[budget]\ncpu = \"8\"\nmem = \"4GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [[pool]]\nname = \"net\"\ncount = 2\n",
    );

    let ids: Vec<String> = (0..3)
        .map(|_| {
            h.submit(&[
                "submit", "--cpu", "1", "--mem", "64MB", "--claim", "net=1", "--", "sleep", "60",
            ])
        })
        .collect();

    h.until(
        "two jobs of the pool operate",
        Duration::from_secs(45),
        || ids.iter().filter(|id| h.state_of(id) == "running").count() == 2,
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        ids.iter().filter(|id| h.state_of(id) == "running").count(),
        2,
        "a pool of 2 must never hold 3 jobs"
    );

    for id in &ids {
        h.qex(&["kill", id, "--grace", "1s"]);
    }
}

/// A pool name that the configuration does not declare is a lock of one unit,
/// and more than one unit of it is a fault that qex must name.
#[test]
fn an_undeclared_pool_is_a_lock_and_two_units_of_it_are_refused() {
    let h = Harness::with_default_config("undeclared");

    let one = h.submit(&["submit", "--claim", "thing=1", "--", "true"]);
    h.ok(&["wait", &one, "--timeout", "45s"]);
    assert_eq!(h.state_of(&one), "completed");

    let out = h.qex(&["submit", "--claim", "thing=2", "--", "true"]);
    assert!(
        !out.status.success(),
        "qex must refuse 2 of an undeclared pool"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("lock of size 1"), "got: {err}");
    assert!(
        err.contains("[[pool]]"),
        "the message must give the remedy: {err}"
    );
}

/// A pool with no devices must also reach the job, through `QEX_CLAIM_<NAME>`.
///
/// A counted claim that the job cannot see is half a feature: a downloader that
/// claims one of four network slots has no way to size its own parallelism.
#[test]
fn a_pool_with_no_devices_gives_the_job_its_count() {
    let h = Harness::new(
        "netenv",
        "[budget]\ncpu = \"8\"\nmem = \"4GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [[pool]]\nname = \"net\"\ncount = 4\n",
    );

    // The pre-condition: a job with no claim gets no such variable.
    let plain = h.submit(&["submit", "--", "sh", "-c", "echo net=[$QEX_CLAIM_NET]"]);
    h.ok(&["wait", &plain, "--timeout", "45s"]);
    assert!(
        h.ok(&["logs", &plain, "--stdout"]).contains("net=[]"),
        "a job with no claim must get no variable"
    );

    let id = h.submit(&[
        "submit",
        "--claim",
        "net=2",
        "--",
        "sh",
        "-c",
        "echo net=[$QEX_CLAIM_NET]",
    ]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    assert_eq!(h.state_of(&id), "completed");
    let out = h.ok(&["logs", &id, "--stdout"]);
    assert!(
        out.contains("net=[2]"),
        "a counted claim must reach the job: {out}"
    );
}

/// A REFUSED CLAIM MUST NOT KEEP THE DEDUPE KEY.
///
/// `qex submit --dedupe-key K --gpu 8` against a pool of 2 is refused. If that
/// refusal left the key in the coordinator, every later submission with the
/// same key would give the id of a job that does not exist, and no job would
/// ever free the key. A script that retries would then never run again.
#[test]
fn a_pool_claim_that_is_refused_gives_the_dedupe_key_back() {
    let h = Harness::new("pooldedupe", &pool_config("\"24GB\", \"24GB\"", ""));

    let out = h.qex(&[
        "submit",
        "--dedupe-key",
        "poolkey",
        "--gpu",
        "8",
        "--",
        "true",
    ]);
    assert!(
        !out.status.success(),
        "a claim of 8 of a pool of 2 must be refused"
    );

    // The key must be free. This submission must START A JOB, and not give the
    // id of the job that qex refused.
    let id = h.submit(&["submit", "--dedupe-key", "poolkey", "--", "true"]);
    h.ok(&["wait", &id, "--timeout", "45s"]);
    assert_eq!(
        h.state_of(&id),
        "completed",
        "the key of a refused job must be free"
    );
}

/// A job that claims a GPU must not also set the variable that qex writes.
/// The two values would disagree, and the job would then use a card that qex
/// gave to a different job.
#[test]
fn a_job_that_sets_the_device_variable_itself_is_refused() {
    let h = Harness::new("cudaconflict", &pool_config("\"24GB\"", ""));
    let out = h.qex(&[
        "submit",
        "--gpu",
        "1",
        "--env",
        "CUDA_VISIBLE_DEVICES=0",
        "--",
        "true",
    ]);
    assert!(!out.status.success(), "qex must refuse this job");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("would disagree"), "got: {err}");
}

/// TWO USERS MUST NOT BOTH GET THE DEVICE 0.
///
/// This is the one test that runs TWO coordinators over ONE pool, and it is the
/// only test of the whole cross-user path: `Held::device_indices` collects the
/// indices this coordinator gave away, `peers::publish` writes them, the other
/// coordinator reads them in `peers::claims`, and `free_devices` then leaves
/// those devices alone.
///
/// Every other pool test sets `[peers] enabled = false`, and the `peers` unit
/// test gives `publish` a device list that the test itself wrote. Neither one
/// reaches `device_indices`, so its body could be replaced with an empty map
/// and nothing failed. The two users then each start a job on the device 0, and
/// both meet a memory error on a card that qex said was free.
///
/// The pool has ONE device, so the answer is not ambiguous: the second user has
/// no other card to take.
#[test]
fn two_users_do_not_both_get_the_one_device() {
    let dir = shared_peer_dir("gpu-share");
    let config = format!(
        "[budget]\ncpu = \"8\"\nmem = \"4GB\"\n\
         [peers]\nenabled = true\ndir = \"{}\"\nstale_after = \"1h\"\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
         [[pool]]\nname = \"gpu\"\nsize = \"vram\"\ndevices = [\"24GB\"]\n\
         env = \"CUDA_VISIBLE_DEVICES\"\n",
        dir.display()
    );
    let other = Harness::new("gpupeerb", &config);
    let mine = Harness::new("gpupeera", &config);

    // The other user takes the one device.
    let held = other.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--gpu", "1", "--", "sleep", "300",
    ]);
    other.until(
        "the other user's GPU job starts",
        Duration::from_secs(45),
        || other.state_of(&held) == "running",
    );
    assert_eq!(
        other.status_json(&held)["assigned"]["gpu"]["devices"][0],
        0,
        "the other user must hold the device 0"
    );

    // THE PRE-CONDITION. This user's own queue is empty and the whole budget is
    // free, so nothing except the other user can hold this job back.
    let info: serde_json::Value = serde_json::from_str(&mine.ok(&["info", "--json"])).unwrap();
    assert_eq!(info["jobs_running"], 0, "my queue must be empty: {info}");

    let want = mine.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--gpu", "1", "--", "true",
    ]);
    mine.until(
        "the GPU job names the other user",
        Duration::from_secs(45),
        || blocked_reason(&mine, &want).contains("another user holds the pool"),
    );

    // THE STATE CHANGED, and it changed in the one way that matters: the job
    // did NOT start, and it did not receive the device that the other user
    // holds.
    assert_eq!(mine.state_of(&want), "queued");
    assert!(
        mine.status_json(&want)["assigned"]["gpu"].is_null(),
        "a job that waits has received no device: {}",
        mine.status_json(&want)
    );
    let reason = blocked_reason(&mine, &want);
    assert!(
        reason.contains("`gpu`") && reason.contains("no known end"),
        "the reason must name the pool and say that qex cannot schedule the end: {reason}"
    );
    assert!(
        !reason.contains("front of the queue"),
        "a wait on another user must not read as a queue position: {reason}"
    );

    // A `Peer` keeps no capacity, so a job behind this one still starts. A
    // reservation here would hold the machine empty for as long as the other
    // user runs.
    let behind = mine.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    mine.until(
        "a job behind the GPU job still starts",
        Duration::from_secs(45),
        || mine.state_of(&behind) == "completed",
    );

    // `qex info` must say the same thing, and it must mark the device.
    let info: serde_json::Value = serde_json::from_str(&mine.ok(&["info", "--json"])).unwrap();
    assert_eq!(info["queue_state"], "waits-for-peer", "info: {info}");
    assert_eq!(
        info["pools"][0]["devices"][0]["peer"], true,
        "qex info must mark the device that another user holds: {info}"
    );

    // The other user gives the device back, and this job then gets it.
    other.ok(&["kill", &held, "--grace", "1s"]);
    mine.until("the device comes free", Duration::from_secs(60), || {
        mine.state_of(&want) == "completed"
    });
    assert_eq!(
        mine.status_json(&want)["assigned"]["gpu"]["devices"][0],
        0,
        "the one device must go to this user once the other user stops"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// `qex info` must report the pools, and a coordinator that cannot answer must
/// give `null` and never `0`. A defaulted number would read as "no other user
/// holds anything", which can be a lie.
#[test]
fn info_reports_the_pools_and_their_devices() {
    let h = Harness::new("poolinfo", &pool_config("\"24GB\", \"16GB\"", ""));
    let text = h.ok(&["info", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let pools = v["pools"].as_array().expect("info must report the pools");
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0]["name"], "gpu");
    assert_eq!(pools[0]["total"], 2);
    assert_eq!(pools[0]["devices"][1]["capacity"], 16u64 << 30);

    let human = h.ok(&["info"]);
    assert!(human.contains("pool gpu"), "got: {human}");
}

/// Closes the sockets of a test when the test ends, and also when it panics.
struct OpenSockets(Vec<libc::c_int>);

impl Drop for OpenSockets {
    fn drop(&mut self) {
        for fd in self.0.drain(..) {
            unsafe { libc::close(fd) };
        }
    }
}

/// Opens a socket that accepts no connection, and fills its backlog.
///
/// A connect to this socket gives no answer. The standard `UnixListener` asks
/// for a large backlog, so this test uses `libc` and asks for a backlog of one.
///
/// The result holds the listener and the connections. The caller must keep it,
/// because a closed socket answers at once with a refusal.
///
/// THE RESULT IS `None` ON A SYSTEM THAT REFUSES A CONNECTION WHEN THE QUEUE OF
/// THE SOCKET IS FULL. Such a system gives no way to make a socket that never
/// answers, so a test that needs one cannot run there.
///
/// THIS IS THE REASON THAT THE LOCK IS THE EVIDENCE, AND THE ANSWER OF THE
/// SOCKET IS NOT. On such a system a socket with a full queue and a socket
/// whose owner is gone give ONE answer: a refusal. The answer of a socket thus
/// cannot separate a coordinator that operates from a coordinator that stopped,
/// and only the lock on the pid file can. The tests that need a socket which
/// gives no answer stop there, and the tests of the lock carry the property.
///
/// `src/paths.rs` holds a copy of this helper for its own tests. An integration
/// test cannot read a test item of the library, so the two must stay apart.
fn a_socket_that_never_answers(path: &Path) -> Option<OpenSockets> {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    assert!(
        bytes.len() < address.sun_path.len(),
        "the path of the test socket is too long"
    );
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let size = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    let target = &address as *const libc::sockaddr_un as *const libc::sockaddr;
    let mut open = Vec::new();

    let can_wait = unsafe {
        let listener = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        assert!(listener >= 0, "the test cannot open a socket");
        open.push(listener);
        assert_eq!(
            libc::bind(listener, target, size),
            0,
            "the test cannot bind {}",
            path.display()
        );
        assert_eq!(libc::listen(listener, 1), 0, "the test cannot listen");

        // Fill the backlog. The listener accepts nothing, so each connection
        // stays in the queue.
        let mut full = false;
        for _ in 0..256 {
            let client = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(client >= 0, "the test cannot open a socket");
            let flags = libc::fcntl(client, libc::F_GETFL);
            libc::fcntl(client, libc::F_SETFL, flags | libc::O_NONBLOCK);
            if libc::connect(client, target, size) != 0 {
                let error = std::io::Error::last_os_error().raw_os_error();
                libc::close(client);
                // A queue that is full gives one of these two answers, and the
                // test can then hold the connect open. Each other answer is a
                // refusal, and this system gives no such socket.
                full = matches!(error, Some(libc::EAGAIN) | Some(libc::EINPROGRESS));
                break;
            }
            open.push(client);
        }
        full
    };

    // Make the guard before the test of `can_wait`, so that a system which
    // cannot hold a connect open still closes every socket of this test.
    let sockets = OpenSockets(open);
    if !can_wait {
        return None;
    }
    Some(sockets)
}

/// Makes a pid file and holds the lock on it, as a coordinator does.
///
/// The caller must keep the result. A file that closes gives the lock back.
fn a_held_pid_file(path: &Path) -> std::fs::File {
    use std::io::Write as _;
    use std::os::unix::io::AsRawFd;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .unwrap();
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "the test cannot lock the pid file");
    write!(file, "{}", std::process::id()).unwrap();
    file.flush().unwrap();
    file
}

/// A socket that a live process holds, and never accepts, must not stop qex.
///
/// THIS TEST HOLDS TWO PROPERTIES TOGETHER: the command finishes, and the
/// directory of the process that holds that socket stays. It does not hold the
/// time limit or the thread on its own, because either one alone is enough to
/// let the command finish. `src/paths.rs` holds each mechanism with its own
/// test.
///
/// The coordinator deletes the short socket directories of the coordinators
/// that stopped, and it asks each socket to answer. A socket that gives no
/// answer must neither stop the command nor lose its directory.
///
/// THIS TEST DOES NOT RUN ON A SYSTEM THAT REFUSES A CONNECTION WHEN THE QUEUE
/// OF A SOCKET IS FULL, because no socket there gives "no answer". The sweep
/// keeps the directory of a coordinator by the LOCK on such a system, and
/// `paths::tests::the_sweep_keeps_the_directory_of_a_coordinator_that_operates`
/// holds that on every system.
#[test]
fn a_socket_that_never_answers_does_not_stop_a_command() {
    let mut h = Harness::with_default_config("stucksock");
    let tmp = h.root.join("t");
    std::fs::create_dir_all(&tmp).unwrap();
    let uid = unsafe { libc::getuid() };
    let stuck = tmp.join(format!("qex-{uid}-stuck"));
    std::fs::create_dir_all(&stuck).unwrap();
    let Some(_open) = a_socket_that_never_answers(&stuck.join("s")) else {
        // This system refuses a connection when the queue of a socket is full,
        // so no socket here can give "no answer". The state that this test
        // measures does not exist on it, and the lock on the pid file carries
        // the property instead.
        eprintln!(
            "this test did not run: this system gives a refusal, and not a wait, for a \
             socket with a full queue"
        );
        return;
    };
    h.extra_env
        .push(("TMPDIR".into(), tmp.display().to_string()));

    let out = h.qex(&["info", "--json"]);

    // WATCH THE DIRECTORY, AND DO NOT READ IT ONE TIME. The command gives an
    // answer in a few milliseconds, and the sweep runs on a thread of the
    // coordinator. A test that reads the directory at once reads it before the
    // sweep looks at it, and it then holds nothing.
    //
    // The time is longer than the limit of a whole sweep, so the sweep reaches
    // its verdict inside it.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut survived = true;
    while Instant::now() < deadline {
        if !stuck.exists() {
            survived = false;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    assert!(
        out.status.success(),
        "the command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        survived,
        "the sweep deleted the directory of a coordinator that operates"
    );
}

/// The coordinator holds the lock on its pid file for its whole life.
///
/// The sweep of a different coordinator tries that lock. Without it, a system
/// that refuses a connection when the queue of the socket is full lets that
/// sweep delete the directory of a coordinator that operates.
#[test]
fn a_coordinator_holds_the_lock_on_its_pid_file() {
    use std::os::unix::io::AsRawFd;

    let h = Harness::with_default_config("pidfile");
    let info: serde_json::Value = serde_json::from_str(&h.ok(&["info", "--json"])).unwrap();
    let pid = info["pid"].as_i64().expect("info must give the pid");
    let path = h.root.join("state/qex/run/pid");

    // The lock is the evidence that the coordinator operates.
    // Open for writing as well: an exclusive lock needs a descriptor that can
    // write on some systems and on some file systems.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("the coordinator must make a pid file beside its socket");
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if taken == 0 {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    }
    assert_ne!(
        taken, 0,
        "the coordinator must hold the lock on its pid file while it operates"
    );

    // The number is for a reader, and no decision uses it.
    let written = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        written.trim().parse::<i64>().unwrap(),
        pid,
        "the pid file must name the coordinator that operates"
    );
}

/// A coordinator must not bind a socket that a different coordinator holds.
///
/// The socket file is present and it gives no answer, so a coordinator can
/// operate there. Two coordinators on one state directory each hold the full
/// budget, and together they start twice the permitted work.
///
/// THIS TEST DOES NOT RUN ON A SYSTEM THAT REFUSES A CONNECTION WHEN THE QUEUE
/// OF A SOCKET IS FULL, because no socket there gives "no answer". A different
/// coordinator is stopped by the LOCK on such a system, and
/// `a_locked_pid_file_stops_a_new_coordinator` holds that on every system.
#[test]
fn a_socket_with_no_answer_stops_a_new_coordinator() {
    let mut h = Harness::with_default_config("ownstuck");
    // A coordinator that starts here is a failure of this test. Give it a
    // short life, so the test reports that failure and does not wait.
    h.extra_env.push(("QEX_IDLE_EXIT_SECS".into(), "1".into()));
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    let socket = run.join("s");
    let Some(_open) = a_socket_that_never_answers(&socket) else {
        // This system refuses a connection when the queue of a socket is full,
        // so no socket here can give "no answer". The lock on the pid file
        // stops a new coordinator there, and its own tests hold that.
        eprintln!(
            "this test did not run: this system gives a refusal, and not a wait, for a \
             socket with a full queue"
        );
        return;
    };
    let before = std::fs::symlink_metadata(&socket).unwrap();

    let out = h.qex(&["daemon"]);
    let after = std::fs::symlink_metadata(&socket);

    assert!(
        out.status.success(),
        "the coordinator must stop with no fault: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = after.expect("the coordinator deleted a socket that it could not test");
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&before),
        std::os::unix::fs::MetadataExt::ino(&after),
        "the coordinator deleted the socket of a different coordinator and bound its own"
    );
}

/// A symbolic link with no target must not stop qex for ever.
///
/// Such a link occupies the socket path. A test that follows the link says that
/// the path is free, and `bind` then fails with "Address already in use" at
/// every start. qex tests the LINK, so it clears the link and starts.
#[test]
fn a_symbolic_link_with_no_target_does_not_stop_a_coordinator() {
    let h = Harness::with_default_config("danglink");
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    std::os::unix::fs::symlink(h.root.join("no-such-target"), run.join("s")).unwrap();

    // The coordinator must start, and the command must give its answer.
    let info: serde_json::Value = serde_json::from_str(&h.ok(&["info", "--json"])).unwrap();

    assert!(
        info["pid"].as_i64().unwrap_or(0) > 0,
        "a coordinator must start when a link with no target holds the socket path"
    );
    assert!(
        std::os::unix::fs::FileTypeExt::is_socket(
            &std::fs::symlink_metadata(run.join("s"))
                .unwrap()
                .file_type()
        ),
        "the coordinator must replace the link with its own socket"
    );
}

/// A file that qex cannot use must not stop qex for ever with no way out.
///
/// qex keeps the directory and stops, because it cannot prove that no
/// coordinator operates. That state stays until a person acts, so the message
/// must name the file and the one step that clears it.
#[test]
fn a_pid_file_that_qex_cannot_open_names_the_way_out() {
    use std::os::unix::fs::PermissionsExt;

    // The root user opens every file, so this test measures nothing there.
    if unsafe { libc::getuid() } == 0 {
        return;
    }

    let mut h = Harness::with_default_config("badpid");
    h.extra_env.push(("QEX_IDLE_EXIT_SECS".into(), "1".into()));
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    let pid = run.join("pid");
    std::fs::write(&pid, b"").unwrap();
    std::fs::set_permissions(&pid, std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = h.qex(&["daemon"]);
    let made_a_socket = run.join("s").exists();

    std::fs::set_permissions(&pid, std::fs::Permissions::from_mode(0o600)).unwrap();

    assert!(
        out.status.success(),
        "the coordinator must stop with no fault: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !made_a_socket,
        "qex cannot test the file, so it must not start a second coordinator"
    );
    let log = String::from_utf8_lossy(&out.stdout);
    assert!(
        log.contains(&pid.display().to_string()),
        "the message must name the file, and it says: {log}"
    );
    assert!(
        log.contains("Delete that file if no coordinator operates"),
        "the message must name the one step that clears this state, and it says: {log}"
    );
}

/// A socket that ANSWERS must stop a new coordinator, with no lock to help.
///
/// A coordinator that started before qex made a pid file leaves this state: its
/// socket answers, and no process holds a lock. The answer of the socket is
/// then the one guard, and it must stop a second coordinator.
///
/// This test runs on every system, because a socket that a coordinator accepts
/// answers on every system. It gives the property cover where a socket cannot
/// give "no answer".
#[test]
fn a_socket_that_answers_stops_a_new_coordinator() {
    let h = Harness::with_default_config("ownanswer");

    // Start a coordinator in the ordinary way.
    let info: serde_json::Value = serde_json::from_str(&h.ok(&["info", "--json"])).unwrap();
    let first = info["pid"].as_i64().expect("info must give the pid");

    // Delete the pid file. The coordinator keeps its lock on a file with no
    // name, so a new coordinator takes the lock of a NEW file and the lock
    // cannot stop it. Only the answer of the socket can.
    let run = h.root.join("state/qex/run");
    std::fs::remove_file(run.join("pid")).unwrap();

    let out = h.qex(&["daemon"]);

    assert!(
        out.status.success(),
        "the coordinator must stop with no fault: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = String::from_utf8_lossy(&out.stdout);
    assert!(
        log.contains("a different coordinator operates"),
        "the log must say that a different coordinator operates, and it says: {log}"
    );

    // The first coordinator must still be the one that operates.
    let after: serde_json::Value = serde_json::from_str(&h.ok(&["info", "--json"])).unwrap();
    assert_eq!(
        after["pid"].as_i64(),
        Some(first),
        "a second coordinator took the queue of the first one"
    );
}

/// A lock alone must stop a new coordinator, with NO socket file present.
///
/// A socket file can go while its coordinator operates: a cleaner of the
/// temporary directory deletes it, a person deletes it, and the coordinator
/// deletes its own socket file one moment before it stops. A test that starts
/// at the socket file thus tests nothing in each of those states.
#[test]
fn a_lock_with_no_socket_file_stops_a_new_coordinator() {
    let mut h = Harness::with_default_config("nosocket");
    h.extra_env.push(("QEX_IDLE_EXIT_SECS".into(), "1".into()));
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    let socket = run.join("s");
    // NO socket file. The lock is the only evidence.
    let held = a_held_pid_file(&run.join("pid"));

    let out = h.qex(&["daemon"]);
    let made_a_socket = socket.exists();

    drop(held);

    assert!(
        out.status.success(),
        "the coordinator must stop with no fault: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !made_a_socket,
        "a second coordinator started and bound a socket while a different process \
         held the lock"
    );
    let log = String::from_utf8_lossy(&out.stdout);
    assert!(
        log.contains("a different coordinator operates"),
        "the log must say that a different coordinator operates, and it says: {log}"
    );
}

/// A lock on the pid file must stop a new coordinator.
///
/// A system that refuses a connection when the queue of the socket is full
/// gives the same answer as a socket that nobody holds. The lock is thus the
/// test that does not change with the system.
#[test]
fn a_locked_pid_file_stops_a_new_coordinator() {
    let mut h = Harness::with_default_config("ownpid");
    // A coordinator that starts here is a failure of this test. Give it a
    // short life, so the test reports that failure and does not wait.
    h.extra_env.push(("QEX_IDLE_EXIT_SECS".into(), "1".into()));
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    let socket = run.join("s");
    // A socket file that no process holds. Alone, it lets a coordinator start.
    std::fs::write(&socket, b"").unwrap();
    let held = a_held_pid_file(&run.join("pid"));

    let out = h.qex(&["daemon"]);
    let kind = std::fs::symlink_metadata(&socket).map(|m| m.file_type());

    drop(held);

    assert!(
        out.status.success(),
        "the coordinator must stop with no fault: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let kind = kind.expect("the coordinator deleted the socket of a process that operates");
    assert!(
        !std::os::unix::fs::FileTypeExt::is_socket(&kind),
        "the coordinator bound its own socket while a different process holds the lock"
    );
}

/// `describe_stream` must separate a command that wrote NOTHING from a command
/// that stopped in the middle of its output.
///
/// The failure message of `qex_within` is the only evidence of a fault on a
/// build machine, because nobody runs the command again there. A message that
/// loses the difference between "no output" and "some output" sends the reader
/// to look for a fault of the wrong shape.
#[test]
fn a_message_names_what_a_command_wrote() {
    // No bytes, and bytes that hold no text, both say `no output`. An empty
    // space in a message reads as an accident of the format.
    assert_eq!(describe_stream("stdout", b""), "stdout: no output");
    assert_eq!(describe_stream("stderr", b"\n\n\n"), "stderr: no output");

    // A short output goes in whole, and it keeps the name of the stream.
    assert_eq!(
        describe_stream("stdout", b"one\ntwo\n"),
        "stdout:\none\ntwo"
    );

    // A long output keeps the LAST lines, because the end of the output shows
    // the state of the command when it stopped.
    let many: String = (1..=50).map(|i| format!("line-{i}\n")).collect();
    let text = describe_stream("stdout", many.as_bytes());

    // The counts must be true. A message that names a count it does not hold
    // is worse than a message with no count: a reader acts on the number.
    assert!(
        text.starts_with("stdout: the last 20 lines of 50, and 30 more above:"),
        "the message must name both counts: {text}"
    );
    let lines: Vec<&str> = text.lines().skip(1).collect();
    assert_eq!(lines.len(), 20, "the message must hold 20 lines: {text}");
    assert_eq!(lines[0], "line-31", "the first line kept: {text}");
    assert_eq!(lines[19], "line-50", "the last line kept: {text}");
    assert!(
        !text.contains("line-30\n"),
        "a line above the last 20 must not appear: {text}"
    );
}

/// A signal that this command cannot catch must not leave a job in the queue.
///
/// The job of `qex run` has one reader, and it is that command. A job that
/// keeps a place in the queue for a reader that went away holds every job
/// behind it, and it later takes a claim for output that nobody reads.
///
/// The test uses SIGKILL, because the client runs NO code for it. The signal
/// handler of `qex run` cannot cover this case, and neither can any other code
/// in the command. Only the coordinator can, and it reads the connection that
/// the kernel closes.
#[test]
fn a_kill_of_qex_run_cancels_a_job_that_never_started() {
    let h = Harness::new("runkill", "[budget]\ncpu = \"1\"\n");

    // The BUDGET holds the queue, and not the memory of the machine.
    //
    // This test sets a budget of one core, and the blocker takes that core, so
    // the job under test waits on every machine. Each job gives a small memory
    // claim for the same reason: admission for memory asks the MACHINE how much
    // is free, so a job with the default claim waits on a small machine and
    // starts on a large one, and the result of the test would follow the
    // machine and not the code.
    let blocker = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "30"]);
    h.until("the blocker operates", Duration::from_secs(30), || {
        h.state_of(&blocker) == "running"
    });

    let (mut child, id) = h.run_bg(&["--cpu", "1", "--mem", "64MB", "--", "sleep", "5"]);
    h.until("the job of qex run waits", Duration::from_secs(30), || {
        h.state_of(&id) == "queued"
    });

    // SIGKILL. The command runs no code after this point.
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    child.wait().unwrap();

    h.until(
        "the coordinator cancels the job of a command that stopped",
        Duration::from_secs(30),
        || h.state_of(&id) == "cancelled",
    );

    // The reason must separate this from a cancel that a person typed.
    let status = h.status_json(&id);
    let error = status["error"].as_str().unwrap_or("");
    assert!(
        error.contains("the command that started this job stopped"),
        "the record must say why qex cancelled the job: {status}"
    );

    h.ok(&["kill", &blocker]);
}

/// A job that OPERATES must survive the loss of the command that started it.
///
/// The job holds a claim and does work, and its output stays in the log file
/// for a reader that attaches again. A coordinator that stopped such a job
/// would destroy work that a reader can still use.
///
/// This test holds the boundary of the rule above it. Both tests must pass
/// together: one says that qex cancels a job that never started, and this one
/// says that qex stops there.
#[test]
fn a_kill_of_qex_run_leaves_a_job_that_operates() {
    let h = Harness::new("runkillrun", "");

    // A small memory claim, so this job starts on a machine of any size.
    let (mut child, id) = h.run_bg(&["--cpu", "1", "--mem", "64MB", "--", "sleep", "12"]);
    h.until("the job operates", Duration::from_secs(30), || {
        h.state_of(&id) == "running"
    });

    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    child.wait().unwrap();

    // Give the coordinator more time than it needs to make a wrong decision.
    std::thread::sleep(Duration::from_secs(3));
    let state = h.state_of(&id);
    assert!(
        state == "running" || state == "completed",
        "a job that operates must continue when its reader stops, and it is `{state}`"
    );

    h.ok(&["kill", &id]);
}

/// A command that does not own its job must leave that job in the queue.
///
/// `qex submit --wait` puts the job in the queue to live on its own. A reader
/// that stops the wait has not asked qex to throw the work away, so the job
/// waits for a reader that attaches again.
///
/// This test holds the boundary from the other side. Nothing else fails when
/// the ownership reaches this path, because a job that goes away looks like a
/// job that a person cancelled.
#[test]
fn a_kill_of_submit_with_a_wait_leaves_the_job_in_the_queue() {
    let h = Harness::new("subwaitkill", "[budget]\ncpu = \"1\"\n");

    // The budget holds the queue. Each job gives a small memory claim, so the
    // memory of the machine never decides what this test measures.
    let blocker = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "30"]);
    h.until("the blocker operates", Duration::from_secs(30), || {
        h.state_of(&blocker) == "running"
    });

    let id_file = h.root.join("subwait.id");
    let id_path = id_file.to_str().unwrap().to_string();
    let mut child = h.spawn(&[
        "submit",
        "--wait",
        "--id-file",
        &id_path,
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--",
        "sleep",
        "5",
    ]);

    let deadline = Instant::now() + Duration::from_secs(30);
    let id = loop {
        if let Ok(text) = std::fs::read_to_string(&id_file) {
            let id = text.trim().to_string();
            if id.parse::<uuid::Uuid>().is_ok() {
                break id;
            }
        }
        assert!(Instant::now() < deadline, "submit --wait wrote no id");
        std::thread::sleep(Duration::from_millis(100));
    };
    h.until("the job waits", Duration::from_secs(30), || {
        h.state_of(&id) == "queued"
    });

    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    child.wait().unwrap();

    // Give the coordinator more time than it needs to make a wrong decision.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        h.state_of(&id),
        "queued",
        "a job of `qex submit --wait` must wait for a reader that attaches again"
    );

    h.ok(&["cancel", &id]);
    h.ok(&["kill", &blocker]);
}

/// The ownership of a job must survive a new coordinator.
///
/// A CONNECTION carries the ownership, so a new connection holds none. A
/// command that asks once and never again loses the rule in silence, and it
/// loses it exactly when the rule is most needed: something stopped the
/// coordinator abnormally, and the job of that command waits in the queue.
#[test]
fn the_ownership_of_a_job_survives_a_new_coordinator() {
    let h = Harness::new("ownagain", "[budget]\ncpu = \"1\"\n");

    // The budget holds the queue. Each job gives a small memory claim, so the
    // memory of the machine never decides what this test measures.
    let blocker = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "60"]);
    h.until("the blocker operates", Duration::from_secs(30), || {
        h.state_of(&blocker) == "running"
    });

    let (mut child, id) = h.run_bg(&["--cpu", "1", "--mem", "64MB", "--", "sleep", "5"]);
    h.until("the job of qex run waits", Duration::from_secs(30), || {
        h.state_of(&id) == "queued"
    });

    // Stop the coordinator. The supervisor of the blocker continues, and a
    // later command starts a new coordinator that reads the same records.
    let info: serde_json::Value =
        serde_json::from_str(&h.ok(&["info", "--no-start", "--json"])).unwrap();
    let pid = info["pid"].as_i64().unwrap() as i32;
    unsafe { libc::kill(pid, libc::SIGKILL) };

    // `qex run` meets the loss, finds the new coordinator, and asks it again.
    //
    // Measure how long the new coordinator takes to answer. A reconnect that
    // ends before it binds loses the ownership in silence, so the number
    // belongs in the failure.
    let killed_at = Instant::now();
    h.until("a new coordinator answers", Duration::from_secs(40), || {
        h.state_of(&id) == "queued" && {
            let fresh: serde_json::Value =
                serde_json::from_str(&h.ok(&["info", "--json"])).unwrap();
            fresh["pid"].as_i64().unwrap() as i32 != pid
        }
    });

    let new_coordinator_took = killed_at.elapsed();

    // `qex run` must MEET the loss and ask the new coordinator before this test
    // takes the command away. The command reads the state every 80ms, so this
    // wait is far longer than it needs.
    std::thread::sleep(Duration::from_secs(3));

    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let said = {
        use std::io::Read as _;
        let mut text = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            pipe.read_to_string(&mut text).ok();
        }
        text
    };
    child.wait().unwrap();

    // SAY WHY, ON FAILURE ONLY.
    //
    // The question is one thing: did `qex run` ask the NEW coordinator for the
    // ownership of its job, and what did that coordinator answer? Its stderr
    // holds the client half, and the log of the coordinator holds the other.
    // A test that fails in silence costs a whole run of CI and teaches nothing.
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut cancelled = false;
    while Instant::now() < deadline {
        if h.state_of(&id) == "cancelled" {
            cancelled = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if !cancelled {
        let log = std::fs::read_to_string(h.root.join("state/qex/run/daemon.log"))
            .unwrap_or_else(|e| format!("(no log: {e})"));
        // REPORT WHAT THE CLIENT DID, AND MATCH NOTHING BY ACCIDENT.
        //
        // An earlier form of this line tested `said.contains("own")`, and the
        // directory of this test is named `ownagain`, which the warning about
        // the id file prints. It answered `true` whatever the client did.
        //
        // `noticed` is the fact that decides everything: the client prints that
        // line when it meets the loss, and it reconnects and asks again in the
        // same function. No line means the client never got there, so the
        // ownership went to the coordinator that died, or nowhere.
        let noticed = said.contains("the coordinator stopped");
        let refused = said.contains("keeps the job");
        let about_ownership: Vec<&str> = said
            .lines()
            .filter(|l| l.contains("keeps the job") || l.contains("the coordinator stopped"))
            .collect();
        panic!(
            "the new coordinator did not cancel the job of a command that stopped.\n\
             \n--- the job ---\nstate: {}\n\
             \n--- the time from the kill of the first coordinator to the answer of the \
             second ---\n{:?}\n\
             \n--- did the client MEET the loss and reconnect? ---\n{}\n\
             \n--- did the new coordinator refuse the ownership? ---\n{}\n\
             \n--- the lines about the ownership ---\n{}\n\
             \n--- stderr of the `qex run` that had to ask again ---\n{}\n\
             \n--- the log of the coordinator ---\n{}\n\
             \n--- qex list ---\n{}",
            h.state_of(&id),
            new_coordinator_took,
            noticed,
            refused,
            if about_ownership.is_empty() {
                "none".to_string()
            } else {
                about_ownership.join("\n")
            },
            if said.trim().is_empty() {
                "no output".to_string()
            } else {
                said
            },
            log,
            h.ok(&["list"]),
        );
    }

    h.ok(&["kill", &blocker]);
}

/// The limit for a coordinator that does not answer, in seconds.
///
/// The tests below bound themselves ABOVE this number, so a command that keeps
/// its limit passes and a command with no limit fails.
const COORDINATOR_LIMIT_SECS: u64 = 3;

/// Names the variable that gives the ceiling for a coordinator a short value.
fn qex_ceiling_variable() -> String {
    "QEX_COORDINATOR_CEILING_SECS".to_string()
}

/// A command must end when a socket gives no answer.
///
/// A process that holds the socket and never accepts makes a connect with no
/// limit wait for ever. The queue of a busy coordinator fills in the same way,
/// so this is not a rare state on a machine that many agents share.
#[test]
fn a_socket_that_gives_no_answer_does_not_stop_a_command() {
    let mut h = Harness::with_default_config("nocoordans");
    // The ceiling for a coordinator is long on purpose: a loaded machine
    // answers late, and a short ceiling gives a FALSE failure. A test
    // cannot wait that long, so it gives the product a short ceiling.
    h.extra_env.push((qex_ceiling_variable(), "3".to_string()));
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    let Some(_open) = a_socket_that_never_answers(&run.join("s")) else {
        eprintln!(
            "this test did not run: this system gives a refusal, and not a wait, for a \
             socket with a full queue"
        );
        return;
    };

    let out = h.qex_within(&["list"], Duration::from_secs(COORDINATOR_LIMIT_SECS + 20));

    assert_eq!(
        out.status.code(),
        Some(124),
        "a command that reached the limit for a coordinator gives 124: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("gave no answer"),
        "the message must name what qex tried: {said}"
    );
    assert!(
        said.contains("qex info --no-start"),
        "the message must name the step for the reader: {said}"
    );
}

/// A command must end when a different command holds the spawn lock.
///
/// The lock covers the start of a coordinator, so a command that stopped while
/// it held the lock kept every later command waiting with no end.
#[test]
fn a_spawn_lock_that_nobody_gives_back_does_not_stop_a_command() {
    use std::os::unix::io::AsRawFd;

    let mut h = Harness::with_default_config("nocoordlock");
    // The ceiling for a coordinator is long on purpose: a loaded machine
    // answers late, and a short ceiling gives a FALSE failure. A test
    // cannot wait that long, so it gives the product a short ceiling.
    h.extra_env.push((qex_ceiling_variable(), "3".to_string()));
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    let path = run.join("spawn.lock");
    let held = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    let rc = unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "the test cannot hold the spawn lock");

    let out = h.qex_within(&["list"], Duration::from_secs(COORDINATOR_LIMIT_SECS + 20));

    assert_eq!(
        out.status.code(),
        Some(124),
        "a command that reached the limit for the lock gives 124: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("held the lock"),
        "the message must name the lock: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A limit must NOT start a second coordinator.
///
/// Two coordinators on one state directory each hold the whole budget, and the
/// machine then gets the kill for memory that qex exists to prevent. A socket
/// that gives no answer proves nothing about the process that holds it, so it
/// can never authorise a second coordinator.
#[test]
fn a_socket_that_gives_no_answer_starts_no_second_coordinator() {
    let mut h = Harness::with_default_config("nosecond");
    // The ceiling for a coordinator is long on purpose: a loaded machine
    // answers late, and a short ceiling gives a FALSE failure. A test
    // cannot wait that long, so it gives the product a short ceiling.
    h.extra_env.push((qex_ceiling_variable(), "3".to_string()));
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    let socket = run.join("s");
    let Some(_open) = a_socket_that_never_answers(&socket) else {
        eprintln!(
            "this test did not run: this system gives a refusal, and not a wait, for a \
             socket with a full queue"
        );
        return;
    };

    let out = h.qex_within(
        &["submit", "--", "true"],
        Duration::from_secs(COORDINATOR_LIMIT_SECS + 20),
    );
    assert_eq!(out.status.code(), Some(124));

    // The socket of the test is still the socket. A command that bound its own
    // over it would have taken the state directory from the process that holds
    // it.
    assert!(
        socket.exists(),
        "the socket of the process that holds it must stay"
    );
    assert!(
        !h.root.join("state/qex/run/daemon.log").exists()
            || std::fs::read_to_string(h.root.join("state/qex/run/daemon.log"))
                .unwrap_or_default()
                .is_empty(),
        "no coordinator of this command may have started"
    );
}

/// `qex wait` must not take the spawn lock to print a notice.
///
/// The notice about a pause is a courtesy, and the command already drops its
/// result. A full connect takes the spawn lock and can start a coordinator, so
/// the command whose whole contract is a bounded wait began with a wait that
/// had no end.
#[test]
fn a_wait_does_not_take_the_spawn_lock_for_a_notice() {
    use std::os::unix::io::AsRawFd;

    let h = Harness::with_default_config("waitnolock");
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(run.join("spawn.lock"))
        .unwrap();
    let rc = unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "the test cannot hold the spawn lock");

    // No coordinator operates, and the id names nothing. The answer is "there
    // is no job with that id", and the lock must not delay it.
    let start = Instant::now();
    let out = h.qex_within(
        &["wait", "11111111-2222-3333-4444-555555555555"],
        Duration::from_secs(COORDINATOR_LIMIT_SECS + 20),
    );
    let took = start.elapsed();

    assert_eq!(
        out.status.code(),
        Some(127),
        "a wait for an id that names nothing gives 127: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        took < Duration::from_secs(COORDINATOR_LIMIT_SECS),
        "the wait must not take the spawn lock: it took {took:?}"
    );
}

/// `qex top --once` is a QUERY, and a query that qex could not answer gives 124.
///
/// The page that a person watches keeps drawing and gives 0, because a display
/// that drew did its work. An agent scripts `--once`, and 0 from a query says
/// that qex answered the question. A page that qex could not fill must not
/// carry that code: the agent reads an empty page as the state of the machine.
///
/// Both forms write the SAME page. Only the code differs.
#[test]
fn a_page_that_qex_could_not_fill_gives_124_for_a_query() {
    let mut h = Harness::with_default_config("topquery");
    // The ceiling for a coordinator is long on purpose: a loaded machine
    // answers late, and a short ceiling gives a FALSE failure. A test
    // cannot wait that long, so it gives the product a short ceiling.
    h.extra_env.push((qex_ceiling_variable(), "3".to_string()));
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    let Some(_open) = a_socket_that_never_answers(&run.join("s")) else {
        eprintln!(
            "this test did not run: this system gives a refusal, and not a wait, for a \
             socket with a full queue"
        );
        return;
    };

    let out = h.qex_within(
        &["top", "--once"],
        Duration::from_secs(COORDINATOR_LIMIT_SECS + 20),
    );

    assert_eq!(
        out.status.code(),
        Some(124),
        "a query that qex could not answer gives 124: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The page is still there. The two forms write one page, and a reader who
    // compares them must see the same text.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("budget"),
        "the page must still be written: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("gave no answer"),
        "the cause must reach the reader: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// NO COORDINATOR IS AN ANSWER, and it gives 0.
///
/// The records on the disk describe a machine with no coordinator, so the page
/// is true and the query succeeded. Only a coordinator that qex could not reach
/// gives 124.
#[test]
fn a_page_with_no_coordinator_gives_zero_for_a_query() {
    let h = Harness::with_default_config("topnocoord");

    let out = h.qex_within(&["top", "--once"], Duration::from_secs(30));

    assert_eq!(
        out.status.code(),
        Some(0),
        "no coordinator is an answer, so the query succeeded: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("budget"));
}

/// Answers the connect and NEVER answers a request.
///
/// The helper that fills a backlog exercises the CONNECT. This one exercises
/// the READ: qex reaches the coordinator, sends its request, and the answer
/// never arrives. The two faults are different, and a test of one says nothing
/// about the other.
///
/// The result holds the listener. A caller that drops it ends the test socket.
struct ASocketThatAcceptsAndSaysNothing {
    _listener: std::os::unix::net::UnixListener,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ASocketThatAcceptsAndSaysNothing {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn a_socket_that_accepts_and_says_nothing(path: &Path) -> ASocketThatAcceptsAndSaysNothing {
    let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let copy = listener.try_clone().unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::spawn(move || {
        // Hold every connection open and answer nothing. A connection that
        // closes would give the reader an end of file, which is a different
        // answer from silence.
        let mut held = Vec::new();
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            match copy.accept() {
                Ok((stream, _)) => held.push(stream),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    });
    ASocketThatAcceptsAndSaysNothing {
        _listener: listener,
        stop,
        thread: Some(thread),
    }
}

/// A coordinator that takes a request and answers nothing must not hold a
/// command for ever.
///
/// This test drives the READ, and no other test does: the connect succeeds, so
/// a limit on the connect alone leaves this fault open.
///
/// The reader gives `--timeout`, because the ceiling for a coordinator is long
/// on purpose — a loaded machine answers late and a short ceiling gives a false
/// failure. The number of the reader bounds the whole command.
#[test]
fn a_coordinator_that_answers_nothing_does_not_stop_a_command() {
    let h = Harness::with_default_config("readstall");
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    let _silent = a_socket_that_accepts_and_says_nothing(&run.join("s"));

    let start = Instant::now();
    let out = h.qex_within(&["wait", "--timeout", "3s", "abc"], Duration::from_secs(60));
    let took = start.elapsed();

    assert!(
        took < Duration::from_secs(30),
        "the read must end: it took {took:?}"
    );
    assert_ne!(
        out.status.code(),
        Some(127),
        "a coordinator that did not answer is NOT `no such job`: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A coordinator that did not answer is NEVER "there is no job with that id".
///
/// `status`, `logs`, `cancel` and `kill` resolve an id through the coordinator.
/// A resolver that reported a LIMIT was read as "no such job", so a caller was
/// told that its RUNNING job does not exist, and the caller then stopped
/// watching work that continues. That is the worst answer a queue can give.
#[test]
fn a_coordinator_that_answers_nothing_is_not_no_such_job() {
    let mut h = Harness::with_default_config("notnosuchjob");
    h.extra_env.push((qex_ceiling_variable(), "3".to_string()));
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    let _silent = a_socket_that_accepts_and_says_nothing(&run.join("s"));

    for form in [
        vec!["status", "abc"],
        vec!["logs", "abc"],
        vec!["cancel", "abc"],
        vec!["kill", "abc"],
    ] {
        let out = h.qex_within(&form, Duration::from_secs(60));
        assert_ne!(
            out.status.code(),
            Some(127),
            "`qex {}` said that a job does not exist, and the coordinator gave no answer: {}",
            form.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A coordinator that answers CORRECTLY, but later than one step of the wait.
///
/// Every other instrument in this file answers NEVER. That proves a wait ends,
/// and it says nothing about the wait that must NOT end: a coordinator on a
/// loaded machine answers late, and a client that gives up at its first step
/// turns a healthy machine into a failure.
///
/// This helper is the one that stops a short client coming back. It forwards
/// every request to the real coordinator, and it holds back the FIRST answer
/// only.
///
/// One slow answer, and not one for each request: a command opens several
/// connections and asks several questions, so a delay on every answer makes the
/// time of the test depend on the number of round trips and on the speed of the
/// machine that runs the proxy. One delay proves the same property — the client
/// waits past a step instead of refusing — and it costs the same on a fast
/// machine and a slow one.
struct ASlowCoordinator {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// What the proxy did, and when. A failure prints it.
    ///
    /// The test cannot see the machine that CI runs, so the proxy says what it
    /// accepted and what it wrote. A proxy that holds several connections, or
    /// that writes later than it was told to, shows here.
    events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl ASlowCoordinator {
    fn what_it_did(&self) -> String {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .join("\n")
    }
}

impl Drop for ASlowCoordinator {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Puts a slow coordinator in front of a real one.
///
/// `front` is the socket that qex reaches. `real` is the socket of the
/// coordinator that answers.
fn a_coordinator_that_answers_late(front: &Path, real: &Path, delay: Duration) -> ASlowCoordinator {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::os::unix::net::UnixListener::bind(front).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let real = real.to_path_buf();
    // The first answer of the whole test waits. Every answer after it goes
    // straight through.
    let held_one = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let events: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = events.clone();
    let began = Instant::now();
    let thread = std::thread::spawn(move || {
        let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            match listener.accept() {
                Ok((front_side, _)) => {
                    // AN ACCEPTED SOCKET MUST BLOCK.
                    //
                    // The listener is not blocking, so that the accept loop can
                    // stop. One system gives the accepted socket that flag as
                    // well, and a read on it then answers `WouldBlock` the
                    // moment the client is slow to send its next request. A
                    // proxy that reads that answer as an end of file serves one
                    // request and closes, and the client then meets a broken
                    // pipe that qex did not cause.
                    front_side.set_nonblocking(false).unwrap();
                    let real = real.clone();
                    let first = held_one.clone();
                    let note = log.clone();
                    note.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(format!("{:?} accepted a connection", began.elapsed()));
                    workers.push(std::thread::spawn(move || {
                        let Ok(back) = std::os::unix::net::UnixStream::connect(&real) else {
                            return;
                        };
                        let mut from_qex = BufReader::new(front_side.try_clone().unwrap());
                        let mut to_real = back.try_clone().unwrap();
                        let mut from_real = BufReader::new(back);
                        let mut to_qex = front_side;
                        let mut line = String::new();
                        let mut served = 0usize;
                        // One request, one answer, and the FIRST answer waits.
                        // AN ERROR IS NOT AN END OF FILE: a proxy that reads it
                        // as one closes the connection under the client, and the
                        // failure then names qex for a fault of this helper.
                        loop {
                            line.clear();
                            match from_qex.read_line(&mut line) {
                                Ok(0) => {
                                    note.lock().unwrap_or_else(|e| e.into_inner()).push(format!(
                                        "{:?} the client closed after {} request(s)",
                                        began.elapsed(),
                                        served
                                    ));
                                    return;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    note.lock().unwrap_or_else(|e| e.into_inner()).push(format!(
                                        "{:?} READING THE REQUEST FAILED after {} request(s): {e}",
                                        began.elapsed(),
                                        served
                                    ));
                                    return;
                                }
                            }
                            served += 1;
                            if to_real.write_all(line.as_bytes()).is_err() {
                                note.lock().unwrap_or_else(|e| e.into_inner()).push(format!(
                                    "{:?} FORWARDING REQUEST {} FAILED",
                                    began.elapsed(),
                                    served
                                ));
                                return;
                            }
                            to_real.flush().ok();
                            let mut answer = String::new();
                            match from_real.read_line(&mut answer) {
                                Ok(0) | Err(_) => {
                                    note.lock().unwrap_or_else(|e| e.into_inner()).push(format!(
                                        "{:?} THE REAL COORDINATOR GAVE NO ANSWER TO REQUEST {}",
                                        began.elapsed(),
                                        served
                                    ));
                                    return;
                                }
                                Ok(_) => {}
                            }
                            // THE COORDINATOR IS BUSY. The answer is correct and
                            // it arrives late. Only the first one waits, so the
                            // cost of this test does not follow the number of
                            // questions that the command asks.
                            // HOLD THE FIRST ANSWER, THEN BEHAVE. A coordinator
                            // does not close after one answer, and a proxy that
                            // did would test the closing and not the waiting.
                            let held = !first.swap(true, std::sync::atomic::Ordering::SeqCst);
                            if held {
                                std::thread::sleep(delay);
                            }
                            note.lock().unwrap_or_else(|e| e.into_inner()).push(format!(
                                "{:?} answered request {} with {} bytes, held: {}",
                                began.elapsed(),
                                served,
                                answer.len(),
                                held
                            ));
                            if to_qex.write_all(answer.as_bytes()).is_err() {
                                note.lock().unwrap_or_else(|e| e.into_inner()).push(format!(
                                    "{:?} WRITING ANSWER {} FAILED",
                                    began.elapsed(),
                                    served
                                ));
                                return;
                            }
                            to_qex.flush().ok();
                        }
                    }));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        for w in workers {
            let _ = w.join();
        }
    });
    ASlowCoordinator {
        stop,
        thread: Some(thread),
        events,
    }
}

/// A COORDINATOR THAT ANSWERS LATE MUST BE ANSWERED, AND NOT REFUSED.
///
/// This is the test that stops a short client coming back. The client looks at
/// the socket in steps, so that it can say that it still waits — and a client
/// that gave up at the END of its first step would refuse a coordinator that
/// was working. A loaded machine answers late, and a false failure costs more
/// than a wait.
///
/// The answer must also be CORRECT. A client that read a part of a line, or
/// that took the answer of an earlier question, would give a wrong answer here
/// rather than a slow one.
#[test]
fn a_coordinator_that_answers_late_is_answered_and_not_refused() {
    let mut h = Harness::with_default_config("slowcoord");
    // A step of the wait is one second, so the answer arrives after several
    // steps and the client must keep waiting through them.
    h.extra_env
        .push(("QEX_SAY_WAITING_AFTER_SECS".into(), "1".into()));

    // A real coordinator, and its socket.
    let first = h.qex(&["submit", "--", "sh", "-c", "exit 7"]);
    assert!(first.status.success(), "the test needs a real coordinator");
    let id = String::from_utf8_lossy(&first.stdout).trim().to_string();
    h.qex(&["wait", &id]);

    let run = h.root.join("state/qex/run");
    let real = run.join("real");
    std::fs::rename(run.join("s"), &real).unwrap();
    let _slow = a_coordinator_that_answers_late(&run.join("s"), &real, Duration::from_secs(3));

    let started = Instant::now();
    let out = h.qex_within(&["status", &id, "--json"], Duration::from_secs(60));
    let took = started.elapsed();

    // THE PROXY MUST PROVE ITS OWN BEHAVIOUR.
    //
    // This helper is an instrument, and an instrument that is wrong in a way
    // that looks like a product fault costs a whole run of CI. If the proxy
    // served fewer requests than the command sent, the failure must name the
    // PROXY and not qex.
    let what_the_proxy_did = _slow.what_it_did();
    assert!(
        !what_the_proxy_did.contains("READING THE REQUEST FAILED")
            && !what_the_proxy_did.contains("FORWARDING REQUEST")
            && !what_the_proxy_did.contains("WRITING ANSWER")
            && !what_the_proxy_did.contains("GAVE NO ANSWER"),
        "THE PROXY OF THIS TEST FAILED, and qex is not the cause:\n{what_the_proxy_did}"
    );

    // SAY WHY, ON FAILURE ONLY. An exit status with no code means a SIGNAL
    // ended the command, which is this test taking it away rather than qex
    // refusing. The proxy log then says whether it wrote when it was told to.
    if out.status.code() != Some(0) {
        use std::os::unix::process::ExitStatusExt as _;
        panic!(
            "a coordinator that answers late must be answered.\n\
             \ncode: {:?}   signal: {:?}   wall time: {:?}\n\
             \n--- what the proxy did ---\n{}\n\
             \n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.status.code(),
            out.status.signal(),
            took,
            _slow.what_it_did(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    let said = String::from_utf8_lossy(&out.stdout);
    let record: serde_json::Value = serde_json::from_str(&said)
        .unwrap_or_else(|e| panic!("the answer must be whole and correct: {e}: {said}"));
    assert_eq!(
        record["exit_code"], 7,
        "the answer must belong to the question that qex asked: {said}"
    );
}

/// Sends the FIRST HALF of each answer and then goes quiet.
///
/// A coordinator writes a large answer in several writes. A machine that stops
/// it between two of them leaves a line with no end, and the socket reports
/// READABLE for the part that arrived. A client that treats readable as
/// answered then reads with no limit and waits for ever.
///
/// This is not the same instrument as a coordinator that says nothing: that one
/// never writes a byte, and this one writes most of the answer.
fn a_coordinator_that_cuts_its_answer(front: &Path, real: &Path) -> ASlowCoordinator {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::os::unix::net::UnixListener::bind(front).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let real = real.to_path_buf();
    let thread = std::thread::spawn(move || {
        let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            match listener.accept() {
                Ok((front_side, _)) => {
                    // The accepted socket must BLOCK. One system gives it the
                    // flag of the listener, and a read then answers
                    // `WouldBlock` when the client is merely slow.
                    front_side.set_nonblocking(false).unwrap();
                    let real = real.clone();
                    let mine = flag.clone();
                    workers.push(std::thread::spawn(move || {
                        let Ok(back) = std::os::unix::net::UnixStream::connect(&real) else {
                            return;
                        };
                        let mut from_qex = BufReader::new(front_side.try_clone().unwrap());
                        let mut to_real = back.try_clone().unwrap();
                        let mut from_real = BufReader::new(back);
                        let mut to_qex = front_side;
                        // ONE request, and half of ONE answer. This helper never
                        // serves a second question: the connection it cut can
                        // carry no more answers, which is the state under test.
                        let mut line = String::new();
                        if from_qex.read_line(&mut line).unwrap_or(0) > 0 {
                            if to_real.write_all(line.as_bytes()).is_err() {
                                return;
                            }
                            to_real.flush().ok();
                            let mut answer = String::new();
                            if from_real.read_line(&mut answer).unwrap_or(0) == 0 {
                                return;
                            }
                            // A CUT THAT LEAVES NO END OF LINE, AND NO BROKEN
                            // CHARACTER.
                            //
                            // The test needs an answer that BEGINS and does not
                            // finish, so that the socket is readable and the
                            // line has no end. A cut in the middle of a
                            // character would make the client fail on text that
                            // it cannot read, and the test would then pass for
                            // the wrong reason.
                            //
                            // Cut at a boundary of a character, near the middle.
                            let mut at = answer.len() / 2;
                            while at > 0 && !answer.is_char_boundary(at) {
                                at -= 1;
                            }
                            let half = &answer.as_bytes()[..at];
                            if to_qex.write_all(half).is_err() {
                                return;
                            }
                            to_qex.flush().ok();
                            // Hold the connection open and write nothing more.
                            while !mine.load(std::sync::atomic::Ordering::SeqCst) {
                                std::thread::sleep(Duration::from_millis(50));
                            }
                        }
                    }));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        for w in workers {
            let _ = w.join();
        }
    });
    ASlowCoordinator {
        stop,
        thread: Some(thread),
        events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    }
}

/// READABLE IS NOT ANSWERED.
///
/// A wait ends when qex holds the WHOLE answer, or when the budget is gone. It
/// never ends because the first byte arrived. A coordinator stopped between two
/// writes leaves a line with no end, and a read with no limit then waits for
/// ever — with no message, and with no limit of the reader reaching it.
#[test]
fn an_answer_that_never_finishes_does_not_stop_a_command() {
    let mut h = Harness::with_default_config("cutanswer");
    h.extra_env.push((qex_ceiling_variable(), "4".to_string()));

    let first = h.qex(&["submit", "--", "true"]);
    assert!(first.status.success(), "the test needs a real coordinator");
    let id = String::from_utf8_lossy(&first.stdout).trim().to_string();
    h.qex(&["wait", &id]);

    let run = h.root.join("state/qex/run");
    let real = run.join("real");
    std::fs::rename(run.join("s"), &real).unwrap();
    let _cut = a_coordinator_that_cuts_its_answer(&run.join("s"), &real);

    let start = Instant::now();
    let out = h.qex_within(&["list", "--json"], Duration::from_secs(60));
    let took = start.elapsed();

    assert!(
        took < Duration::from_secs(30),
        "a cut answer must reach a limit: it took {took:?}"
    );
    assert_eq!(
        out.status.code(),
        Some(124),
        "a cut answer reaches the limit of a wait: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("did not finish it"),
        "the message must say that the answer began and did not finish: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A test that hides the socket must still stop its coordinator.
///
/// The tests that put a proxy in front of a real coordinator used to leave
/// that coordinator after `cargo test` returned. Drop asked `qex info`, which
/// talks to the proxy or to nobody. Drop now stops any process that still
/// holds a file under the harness, including `daemon.log`.
#[test]
fn the_harness_stops_a_coordinator_whose_socket_it_cannot_reach() {
    let h = Harness::with_default_config("dropcoord");
    let pid = h.coordinator_pid();
    assert!(
        unsafe { libc::kill(pid, 0) } == 0,
        "the test needs a coordinator that operates"
    );
    let run = h.root.join("state/qex/run");
    std::fs::rename(run.join("s"), run.join("real")).unwrap();
    drop(h);
    assert!(
        unsafe { libc::kill(pid, 0) } != 0,
        "the coordinator {pid} stayed after the test hid its socket"
    );
}

/// A test that deletes the pid file must still stop its coordinator.
///
/// `a_socket_that_answers_stops_a_new_coordinator` unlinks that file. A later
/// `qex daemon` then writes a new file with its own number and exits. Drop
/// that trusted the file alone left the first coordinator.
#[test]
fn the_harness_stops_a_coordinator_whose_pid_file_is_gone() {
    let h = Harness::with_default_config("dropnopid");
    let pid = h.coordinator_pid();
    std::fs::remove_file(h.root.join("state/qex/run/pid")).unwrap();
    drop(h);
    assert!(
        unsafe { libc::kill(pid, 0) } != 0,
        "the coordinator {pid} stayed after the test deleted its pid file"
    );
}

/// A long state path must still stop its coordinator.
///
/// When `state/qex/run/s` is longer than 100 bytes, the pid file lives in a
/// short directory under the temporary directory, not under the harness root.
/// Drop that read only `state/qex/run/pid` left that coordinator.
#[test]
fn the_harness_stops_a_coordinator_on_a_long_socket_path() {
    // `/tmp/qx1-<name>-0/state/qex/run/s` is 27 bytes plus the name. The name
    // must pass 73 bytes so the path is longer than 100 when TMPDIR is `/tmp`
    // and the pid and the nanos are one digit.
    let h = Harness::with_default_config(
        "dropcoord-long-name-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    );
    let preferred = h.root.join("state/qex/run/s");
    assert!(
        preferred.as_os_str().len() > 100,
        "this test needs a socket path that does not fit: {}",
        preferred.display()
    );
    let pid = h.coordinator_pid();
    assert!(
        !h.root.join("state/qex/run/pid").exists(),
        "the pid file must be in the short directory, not under the harness"
    );
    drop(h);
    assert!(
        unsafe { libc::kill(pid, 0) } != 0,
        "the coordinator {pid} stayed after a long socket path"
    );
}

/// A coordinator that a test starts must not write into `/tmp/qex`.
///
/// The default config enables the peer system and names that directory. A
/// test that forgets `[peers] enabled = false` used to publish there, next
/// to the record of the coordinator that a person is using.
#[test]
fn the_suite_does_not_write_into_the_live_peer_directory() {
    let h = Harness::new("livepeers", "[budget]\ncpu = \"1\"\n");
    let id = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    let pid = h.coordinator_pid();
    h.ok(&["wait", &id]);
    let uid = unsafe { libc::getuid() };
    let private = h
        .peers_dir
        .join(format!("u{uid}"))
        .join(format!("peer-{pid}.json"));
    h.until(
        "this test published its claims in the private directory",
        Duration::from_secs(10),
        || private.exists(),
    );
    let live = PathBuf::from(LIVE_PEERS_DIR)
        .join(format!("u{uid}"))
        .join(format!("peer-{pid}.json"));
    assert!(
        !live.exists(),
        "the suite wrote the claims of this test into the live peer directory: {}",
        live.display()
    );
}

/// The suite refuses the live peer directory even when a config names it.
#[test]
fn a_config_that_names_the_live_peer_directory_gets_a_private_one() {
    let isolated = PathBuf::from("/tmp/qx-isolated-peers");
    assert_eq!(
        peers_dir_for_test("[peers]\ndir = \"/tmp/qex\"\n", &isolated),
        isolated
    );
    assert_eq!(
        peers_dir_for_test("[peers]\nenabled = false\n", &isolated),
        isolated
    );
    let shared = PathBuf::from("/tmp/qxpeers-shared");
    assert_eq!(
        peers_dir_for_test(
            &format!("[peers]\nenabled = true\ndir = \"{}\"\n", shared.display()),
            &isolated
        ),
        shared
    );
}

/// A coordinator that closes the connection must not KILL the command.
///
/// `main` restores the default action of SIGPIPE, so that `qex list | head`
/// ends in silence like every other tool. That is right for the pipe of the
/// reader. It is WRONG for the socket of the coordinator: a coordinator that
/// closes a connection in the middle of a conversation would then stop the
/// command with a signal — no message, no exit code, and no cause.
///
/// A reader who gets nothing is the fault that this whole area exists to
/// remove, so a write to the coordinator must give an ERROR and not a signal.
///
/// **THIS TEST DOES NOT DISCRIMINATE ON EVERY SYSTEM.** On a system where the
/// close of the peer reaches the READ before the command writes again, the
/// command reports the fault either way and the signal never rises. The test
/// still holds the answer that a reader gets — a cause, and not silence — and
/// the system where the signal rises is the one that proves the rest.
#[test]
fn a_coordinator_that_closes_the_connection_does_not_kill_the_command() {
    use std::io::{BufRead, BufReader, Write};

    let mut h = Harness::with_default_config("closemid");
    h.extra_env.push((qex_ceiling_variable(), "5".to_string()));
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();

    // Answer the FIRST request, then close. The command asks more than one
    // question, so its next write meets a socket that is gone.
    let listener = std::os::unix::net::UnixListener::bind(run.join("s")).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let door = std::thread::spawn(move || {
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            let Ok((client, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(client.try_clone().unwrap());
            let mut writer = client;
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) > 0 {
                // One answer that qex cannot use, and then the door shuts.
                let _ = writer.write_all(b"{\"kind\":\"error\",\"message\":\"go away\"}\n");
                let _ = writer.flush();
            }
            drop(writer);
            drop(reader);
        }
    });

    let out = h.qex_within(&["list"], Duration::from_secs(40));

    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    std::os::unix::net::UnixStream::connect(run.join("s")).ok();
    let _ = door.join();

    use std::os::unix::process::ExitStatusExt as _;
    assert!(
        out.status.signal().is_none(),
        "a coordinator that closed the connection stopped the command with the signal {:?}. \
         A write to the socket of the coordinator must give an error, and not a signal.\n\
         stdout: {}\nstderr: {}",
        out.status.signal(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).is_empty(),
        "the reader must get a cause, and not silence"
    );
}

/// A coordinator that did not answer must not read as NO coordinator.
///
/// `qex version` reports whether a coordinator operates and which version it
/// is. Reporting `none` for one that qex could not reach answers the question
/// wrongly AND exits 0, so an agent reads a false success. The exit-code table
/// promises 124 for this state in every command, and a promise that one command
/// breaks is a promise that an agent cannot use.
#[test]
fn a_command_that_asks_about_a_coordinator_does_not_report_absence_for_silence() {
    let mut h = Harness::with_default_config("askscoord");
    h.extra_env.push((qex_ceiling_variable(), "3".to_string()));
    let run = h.root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    // A SOCKET THAT DOES NOT ACCEPT, so the CONNECT reaches the limit.
    //
    // The other instrument accepts and then says nothing, which reaches the
    // limit of the READ. The two are different arms of the same rule, and a
    // test of one says nothing about the other.
    let Some(_open) = a_socket_that_never_answers(&run.join("s")) else {
        eprintln!(
            "this test did not run: this system gives a refusal, and not a wait, for a \
             socket with a full queue"
        );
        return;
    };

    for form in [vec!["version"], vec!["version", "--json"], vec!["pause"]] {
        let out = h.qex_within(&form, Duration::from_secs(60));
        assert_eq!(
            out.status.code(),
            Some(124),
            "`qex {}` must say that it stopped waiting, and not that no coordinator \
             operates: {}",
            form.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

// ---------------------------------------------------------------------------
// qex abort
// ---------------------------------------------------------------------------

/// The configuration for the tests of `qex abort`: one core, so one job
/// operates and every other job waits.
const ABORT_CONFIG: &str = "[budget]\ncpu = \"1\"\nmem = \"1GB\"\n\
     [peers]\nenabled = false\n\
     [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n";

impl Harness {
    /// Runs `qex abort` with these options, and gives its JSON answer.
    fn abort_json(&self, args: &[&str]) -> serde_json::Value {
        let mut full = vec!["abort", "--json", "--grace", "1s"];
        full.extend_from_slice(args);
        let out = self.qex_within(&full, Duration::from_secs(60));
        assert!(
            out.status.success(),
            "`qex {}` failed with the code {:?}\nstdout: {}\nstderr: {}",
            full.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("the abort output is not valid JSON")
    }

    /// Submits a job from a process tree that is NOT the tree of this test.
    ///
    /// The instrument: a copy of `sh` under the name `tmux`, which the
    /// context rule of qex treats as the end of a session. The submission
    /// runs as `tmux -c 'sh -c "qex submit ..."'`, so the chain of the job is
    /// `qex <- sh <- tmux <- this test`. Its context is the one `sh`, which
    /// no other command shares. `direct` runs the submit as a child of the
    /// copy itself, so the chain is `qex <- tmux <- this test` and the
    /// context is EMPTY: a job with no context is outside every default
    /// scope.
    fn submit_from_another_context(
        &self,
        direct: bool,
        dir: Option<&Path>,
        args: &[&str],
    ) -> String {
        let wrapper = self.root.join("tmux");
        if !wrapper.exists() {
            // A copy of bash, and not of sh. On macOS `/bin/sh` is a program
            // that replaces itself with the selected shell, so the process
            // takes the name `bash` and the fake boundary is gone. The
            // context rule then sees the test process in the chain of the
            // job, and the job is in the context of the test.
            let shell = if Path::new("/bin/bash").exists() {
                "/bin/bash"
            } else {
                "/bin/sh"
            };
            std::fs::copy(shell, &wrapper).expect("the test cannot copy the shell");
        }
        let mut cmd = Command::new(&wrapper);
        // The `exit` after the command forces a fork: a shell that runs one
        // command alone replaces itself with it, and the chain would then
        // lose the shell.
        if direct {
            cmd.arg("-c").arg("\"$0\" \"$@\"; exit $?");
        } else {
            cmd.arg("-c")
                .arg("sh -c '\"$0\" \"$@\"; exit $?' \"$0\" \"$@\"; exit $?");
        }
        cmd.arg(env!("CARGO_BIN_EXE_qex")).arg("submit").args(args);
        isolate(&mut cmd, &self.root, &self.peers_dir);
        if let Some(dir) = dir {
            cmd.current_dir(dir);
        }
        let out = cmd.output().expect("the wrapper did not start");
        assert!(
            out.status.success(),
            "the submit through the wrapper failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(id.parse::<uuid::Uuid>().is_ok(), "not a job id: {id}");
        id
    }
}

/// `qex abort` stops the jobs that operate, empties the queue, and leaves the
/// queue paused so that nothing new starts until a resume.
///
/// THE PROPERTIES OF THE COMMAND, in the order that the assertions test them:
///
///   * a job that was queued before the command is gone, and it never starts;
///   * the counts in the report are what the coordinator did;
///   * the job that operated stops, through the kill mechanism of qex;
///   * a job submitted after the command waits until `qex resume`.
#[test]
fn abort_stops_the_running_job_and_empties_the_queue() {
    let h = Harness::new("abort", ABORT_CONFIG);

    let running = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
    ]);
    h.until("the first job operates", Duration::from_secs(45), || {
        h.state_of(&running) == "running"
    });
    let mut queued = Vec::new();
    for _ in 0..5 {
        queued.push(h.submit(&[
            "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
        ]));
    }
    for id in &queued {
        assert_eq!(h.state_of(id), "queued");
    }

    let report = h.abort_json(&[]);
    assert_eq!(report["cancelled"], 5, "the report: {report}");
    assert_eq!(
        report["signalled"],
        serde_json::json!([running]),
        "the report: {report}"
    );
    assert_eq!(
        report["not_stopped"],
        serde_json::json!([]),
        "the report: {report}"
    );
    assert_eq!(report["outside"], 0, "the report: {report}");
    assert_eq!(report["queue_paused"], true, "the report: {report}");

    // The queued jobs are cancelled, and their records stay, as after
    // `qex cancel`. `qex clean cancelled` then deletes them.
    for id in &queued {
        assert_eq!(
            h.state_of(id),
            "cancelled",
            "the job {id} must be cancelled"
        );
    }
    h.ok(&["clean", "cancelled"]);
    for id in &queued {
        assert!(
            !h.job_dir(id).exists(),
            "the directory of {id} must be deleted"
        );
    }
    let ids: Vec<String> = h
        .list_json()
        .iter()
        .map(|j| j["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        ids,
        vec![running.clone()],
        "only the signalled job keeps a record"
    );

    // The running job stops, and its state says that qex stopped it.
    h.until("the running job stops", Duration::from_secs(30), || {
        h.state_of(&running) == "killed"
    });

    // Nothing starts until a resume. Measure for a period, and not one time.
    let later = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        assert_eq!(
            h.state_of(&later),
            "queued",
            "no job starts after an abort until a resume"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    let pause = h.ok(&["pause", "--json"]);
    let pause: serde_json::Value = serde_json::from_str(&pause).unwrap();
    assert_eq!(pause["paused"], true, "the queue must be paused: {pause}");

    h.ok(&["resume", "queue"]);
    h.until("the later job starts", Duration::from_secs(45), || {
        h.has_started(&later)
    });
}

/// `--keep-running` cancels the queued jobs and leaves the running one.
#[test]
fn abort_keep_running_leaves_the_job_that_operates() {
    let h = Harness::new("abortkeep", ABORT_CONFIG);

    let running = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
    ]);
    h.until("the first job operates", Duration::from_secs(45), || {
        h.state_of(&running) == "running"
    });
    let queued = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
    ]);

    let report = h.abort_json(&["--keep-running"]);
    assert_eq!(report["cancelled"], 1, "the report: {report}");
    assert_eq!(
        report["continues"],
        serde_json::json!([running]),
        "the report: {report}"
    );
    assert_eq!(
        report["signalled"],
        serde_json::json!([]),
        "the report: {report}"
    );

    assert_eq!(
        h.state_of(&queued),
        "cancelled",
        "the queued job must be cancelled"
    );
    // Measure for a period. A job that a signal reaches stops a moment later.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        h.state_of(&running),
        "running",
        "the job that operates must continue"
    );
    let pid = h.status_json(&running)["pid"].as_i64().unwrap() as i32;
    assert!(
        count_in_group(pid) >= 1,
        "the process of the job must still exist"
    );

    h.ok(&["kill", &running, "--grace", "1s"]);
}

/// The three levels of the scope, and the tag that narrows each one.
///
/// THE PROPERTY: when `qex abort` returns with no scope option, no job of a
/// different context or of a different directory has changed state; with
/// `--cwd`, no job of a different directory has; with `--all`, the queue is
/// empty. Every submitter here is THE SAME UNIX USER, which is the population
/// that the scope exists to protect.
///
/// The queue is paused first, so every job waits and no job can start
/// between two steps of this test.
#[test]
fn abort_touches_only_the_jobs_of_the_caller() {
    let h = Harness::new("abortscope", ABORT_CONFIG);
    h.ok(&["pause", "queue"]);
    let elsewhere = h.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let elsewhere = elsewhere.canonicalize().unwrap();

    let job = ["--cpu", "1", "--mem", "64MB", "--", "true"];
    let submit = |extra: &[&str]| {
        let mut args = vec!["submit"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&job);
        h.submit(&args)
    };
    // Mine: this process tree, this directory.
    let mine = [submit(&[]), submit(&[])];
    let mine_tagged = submit(&["--tag", "phase"]);
    // This process tree, another directory.
    let mut other_dir = Vec::new();
    for _ in 0..2 {
        let mut cmd = h.command(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
        cmd.current_dir(&elsewhere);
        let out = cmd.output().unwrap();
        assert!(out.status.success());
        other_dir.push(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    // Another process tree, this directory.
    let other_context = [
        h.submit_from_another_context(false, None, &job),
        h.submit_from_another_context(false, None, &job),
    ];
    // No context at all, this directory.
    let no_context = h.submit_from_another_context(true, None, &job);

    let state = |id: &str| h.state_of(id);
    let all_queued = |ids: &[&String]| {
        for id in ids {
            assert_eq!(state(id), "queued", "the job {id} must not change state");
        }
    };

    // `--tag` narrows the default scope.
    let report = h.abort_json(&["--tag", "phase"]);
    assert_eq!(report["cancelled"], 1, "the report: {report}");
    assert_eq!(report["outside"], 7, "the report: {report}");
    assert_eq!(state(&mine_tagged), "cancelled");
    all_queued(&[&mine[0], &mine[1], &other_dir[0], &other_dir[1]]);
    all_queued(&[&other_context[0], &other_context[1], &no_context]);

    // The chain of each job that must stay outside, for the message of a
    // failure: a reader of a build log can then see why the rule took it.
    let chains = || {
        let mut text = String::new();
        for (what, id) in [
            ("other context", &other_context[0]),
            ("other context", &other_context[1]),
            ("no context", &no_context),
            ("other dir", &other_dir[0]),
        ] {
            text.push_str(&format!(
                "\n{what} {id}: {}",
                h.status_json(id)["submitter"]
            ));
        }
        text.push_str(&format!("\ncaller: {}", report_scope_chain(&h)));
        text
    };

    // Level 1: this process tree AND this directory.
    let report = h.abort_json(&[]);
    assert_eq!(report["cancelled"], 2, "the report: {report}{}", chains());
    assert_eq!(report["outside"], 5, "the report: {report}{}", chains());
    assert_eq!(state(&mine[0]), "cancelled");
    assert_eq!(state(&mine[1]), "cancelled");
    all_queued(&[&other_dir[0], &other_dir[1]]);
    all_queued(&[&other_context[0], &other_context[1], &no_context]);

    // Level 2: this directory, whatever process submitted the job.
    let report = h.abort_json(&["--cwd"]);
    assert_eq!(report["cancelled"], 3, "the report: {report}");
    assert_eq!(report["outside"], 2, "the report: {report}");
    all_queued(&[&other_dir[0], &other_dir[1]]);
    assert_eq!(state(&other_context[0]), "cancelled");
    assert_eq!(state(&no_context), "cancelled");

    // Level 3: every job of the queue.
    let report = h.abort_json(&["--all"]);
    assert_eq!(report["cancelled"], 2, "the report: {report}");
    assert_eq!(report["outside"], 0, "the report: {report}");
    assert!(
        h.list_json().iter().all(|j| j["state"] == "cancelled"),
        "every job of the queue must be cancelled"
    );
}

/// Gives the chain of the caller, as `qex abort --json` reports its scope.
///
/// A dry form: the scope of a `--keep-running` abort on a tag that no job
/// carries touches no job, and it carries the chain that the CLI sent.
///
/// CALL IT ON A PAUSED QUEUE ONLY. Every abort pauses the queue first, so on
/// a queue that operates this helper would pause it, with the reason
/// `qex abort`, and the test would then measure a queue that it changed.
fn report_scope_chain(h: &Harness) -> String {
    let report = h.abort_json(&["--keep-running", "--tag", "no-such-tag-for-the-chain"]);
    report["scope"]["submitter"].to_string()
}

/// The counts say what the coordinator did, and a job outside the scope
/// keeps its state.
#[test]
fn abort_counts_what_it_did_and_keeps_a_record_that_another_job_needs() {
    let h = Harness::new("abortcount", ABORT_CONFIG);
    let elsewhere = h.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();

    // A job of this directory that already stopped. It is in the directory
    // and in this process tree, and the abort must not count it.
    let done = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    h.ok(&["wait", &done]);

    h.ok(&["pause", "queue"]);
    let cause = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    // A job in another directory that needs the first one. It is outside the
    // default scope, so it holds the record of the cause.
    let mut cmd = h.command(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--needs", &cause, "--", "true",
    ]);
    cmd.current_dir(&elsewhere);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dependent = String::from_utf8_lossy(&out.stdout).trim().to_string();

    let report = h.abort_json(&[]);
    assert_eq!(
        report["cancelled"], 1,
        "the stopped job is not cancelled: {report}"
    );
    assert_eq!(report["outside"], 1, "the dependent is outside: {report}");

    assert_eq!(h.state_of(&cause), "cancelled");
    assert_eq!(
        h.state_of(&done),
        "completed",
        "a stopped job keeps its record"
    );
    // The abort did not touch the dependent. The SCHEDULER then skips it,
    // because the job that it needs did not succeed; that is the rule for a
    // dependency, and `qex cancel` on the cause gives the same result.
    let state = h.state_of(&dependent);
    assert!(
        state == "queued" || state == "skipped",
        "a job outside the scope is not cancelled: {state}"
    );
    assert!(
        h.job_dir(&dependent).exists(),
        "a job outside the scope keeps its record"
    );
}

/// A coordinator that cannot obey `qex abort` must REFUSE it, by name.
///
/// An earlier coordinator does not know the request. It would answer with an
/// error and change nothing, and a reader who did not see that answer would
/// believe that the queue is empty while every job in it starts.
#[test]
fn a_coordinator_that_cannot_abort_refuses_the_command() {
    use std::io::{BufRead, BufReader, Write};

    let root = std::env::temp_dir().join(format!("qxoldabort{}", std::process::id()));
    let run = root.join("state/qex/run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::create_dir_all(root.join("cfg")).unwrap();
    let socket = run.join("s");

    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut writer = stream.try_clone().unwrap();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            let answer = if line.contains("\"info\"") {
                serde_json::json!({
                    "result": "info", "pid": 4321, "version": "0.9.0",
                    "started_at": 0, "program_replaced": false,
                    "jobs_running": 0, "jobs_queued": 0,
                    "cpu_budget": 1, "mem_budget": 1, "cpu_claimed": 0, "mem_claimed": 0,
                })
            } else if line.contains("\"capabilities\"") {
                serde_json::json!({ "result": "capabilities", "names": ["locks", "pause"] })
            } else {
                serde_json::json!({
                    "result": "error", "kind": "internal",
                    "message": "qex could not read this request",
                })
            };
            writeln!(writer, "{answer}").ok();
            writer.flush().ok();
        }
    });

    let peers = root.join("peers");
    std::fs::create_dir_all(&peers).ok();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_qex"));
    cmd.args(["abort", "--all"]);
    isolate(&mut cmd, &root, &peers);
    let out = cmd.output().expect("qex did not start");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    drop(server);
    std::fs::remove_dir_all(&root).ok();

    assert_eq!(
        out.status.code(),
        Some(1),
        "the command must fail. stdout: {stdout} stderr: {err}"
    );
    assert!(
        err.contains("cannot obey `qex abort`"),
        "the message must say what happened: {err}"
    );
    assert!(
        err.contains("kill 4321"),
        "the message must give the remedy: {err}"
    );
}

/// The record of a job shows the chain of its submitter, so a reader can see
/// why `qex abort` did, or did not, take it.
#[test]
fn a_record_shows_the_chain_of_its_submitter() {
    let h = Harness::with_default_config("chain");
    let id = h.submit(&["submit", "--", "true"]);
    let me = std::process::id() as i64;

    let status = h.status_json(&id);
    let chain = status["submitter"].as_array().expect("the chain is a list");
    assert!(
        chain.iter().any(|a| a["pid"].as_i64() == Some(me)),
        "the chain must name this test process: {chain:?}"
    );
    assert!(
        chain.iter().all(|a| a["start"].is_number()),
        "each process must carry its start time: {chain:?}"
    );

    let text = h.ok(&["status", &id]);
    assert!(
        text.contains("submitter:"),
        "the text must show the chain: {text}"
    );
    assert!(
        text.contains(&format!(" {me} ")),
        "the text must name this test process: {text}"
    );
    h.ok(&["wait", &id]);
}

/// `qex pause` with no word reports, and it says how to pause when nothing is
/// paused. A reader who typed it to pause must not read "nothing is paused"
/// as "my pause did not take".
#[test]
fn pause_with_no_word_says_how_to_pause() {
    let h = Harness::with_default_config("pausehow");
    h.ok(&["info"]);
    let text = h.ok(&["pause"]);
    assert!(
        text.contains("qex pause queue"),
        "the report must name the command that pauses: {text}"
    );
}

/// After an abort, every reader that asks about a cancelled job gets the
/// answer that it gets after `qex cancel` of that job.
///
/// THE READERS: `qex wait`, `qex submit --wait`, `qex run` (each gives 125),
/// `qex status` (the state `cancelled`), the stop hook (one line for each
/// job), and `qex status` after `qex clean` (the job never ran).
#[test]
fn after_an_abort_every_reader_gets_the_answer_of_a_cancel() {
    let h = Harness::new("abortreaders", ABORT_CONFIG);
    let mark = h.root.join("hook.txt");
    h.write_config(&format!(
        "{ABORT_CONFIG}[hooks]\non_stop_states = [\"cancelled\"]\n\
         on_stop = [\"sh\", \"-c\", \"echo \\\"$QEX_STATE $QEX_JOB_NAME\\\" >> {}\"]\n",
        mark.display()
    ));

    let occupier = h.submit(&[
        "submit", "--cpu", "1", "--mem", "64MB", "--", "sleep", "300",
    ]);
    h.until("the first job operates", Duration::from_secs(45), || {
        h.state_of(&occupier) == "running"
    });

    let waited = h.submit(&[
        "submit", "--name", "waited", "--cpu", "1", "--mem", "64MB", "--", "true",
    ]);
    let waiter = h.spawn(&["wait", &waited]);
    let submit_wait = h.spawn(&[
        "submit",
        "--wait",
        "--name",
        "submitted",
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--",
        "true",
    ]);
    let (run, ran) = h.run_bg(&["--name", "ran", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    h.until("every job waits", Duration::from_secs(30), || {
        h.state_of(&ran) == "queued" && h.list_json().len() == 4
    });

    let report = h.abort_json(&[]);
    assert_eq!(report["cancelled"], 3, "the report: {report}");

    let out = wait_for_child(waiter, Duration::from_secs(30), "qex wait after an abort");
    assert_eq!(
        out.status.code(),
        Some(125),
        "`qex wait` must give 125 for a cancelled job: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = wait_for_child(
        submit_wait,
        Duration::from_secs(30),
        "submit --wait after an abort",
    );
    assert_eq!(
        out.status.code(),
        Some(125),
        "`qex submit --wait` must give 125 for a cancelled job: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = wait_run(run, "an abort cancelled the job of `qex run`");
    assert_eq!(
        out.status.code(),
        Some(125),
        "`qex run` must give 125 for a cancelled job: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(h.state_of(&waited), "cancelled");
    h.until(
        "the stop hook wrote three lines",
        Duration::from_secs(30),
        || h.hook_lines().len() == 3,
    );
    let mut lines = h.hook_lines();
    lines.sort();
    assert_eq!(
        lines,
        vec![
            "cancelled ran".to_string(),
            "cancelled submitted".to_string(),
            "cancelled waited".to_string()
        ],
        "the stop hook must run for each cancelled job"
    );

    // After `qex clean`, the history says that the job never ran.
    h.ok(&["clean", "cancelled"]);
    let out = h.qex(&["status", &waited]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        err.contains("NEVER RAN") && !err.contains("HAPPENED"),
        "a cancelled job never ran, and the reader must not repeat nothing: {err}"
    );

    h.qex(&["kill", &occupier, "--grace", "1s"]);
}

/// An abort that arrives at the moment of a resume accounts for every job:
/// each one is cancelled or signalled, and none is reported as not stopped.
///
/// WHAT THIS TEST DOES NOT PROVE: that the abort waits for the pid of a job
/// between the queue and its first process. A test cannot hold that window
/// open, and on a quiet machine the abort meets no job in it. The unit test
/// `lifecycle::tests::an_abort_waits_for_the_pid_of_a_job_that_starts` makes
/// the state and proves the wait. This test runs the real path under load.
#[test]
fn an_abort_at_the_moment_of_a_resume_accounts_for_every_job() {
    let h = Harness::new(
        "abortstarting",
        "[budget]\ncpu = \"8\"\nmem = \"1GB\"\n\
         [peers]\nenabled = false\n\
         [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n",
    );
    h.ok(&["info"]);
    for round in 0..3 {
        h.ok(&["pause", "queue"]);
        for _ in 0..20 {
            h.submit(&["submit", "--cpu", "1", "--mem", "1MB", "--", "sleep", "5"]);
        }
        h.ok(&["resume", "queue"]);
        let report = h.abort_json(&["--all"]);
        assert_eq!(
            report["not_stopped"],
            serde_json::json!([]),
            "round {round}: every job must receive the signal: {report}"
        );
        let cancelled = report["cancelled"].as_u64().unwrap();
        let signalled = report["signalled"].as_array().unwrap().len() as u64;
        assert_eq!(cancelled + signalled, 20, "round {round}: {report}");
        h.until("no job operates", Duration::from_secs(30), || {
            h.list_json()
                .iter()
                .all(|j| matches!(j["state"].as_str(), Some("cancelled" | "killed")))
        });
        h.ok(&["clean", "done"]);
    }
}

/// When the coordinator dies during an abort, the next coordinator holds every
/// job that the abort cancelled as cancelled.
///
/// The coordinator of this test stops in the moment after the lock section of
/// the abort, before any answer, through a variable that exists for this test.
/// The cancel of each job must be on the disk by then: a record that was
/// cancelled in memory only comes back as a queued job, and one
/// `qex resume queue` then starts the work that the abort stopped.
#[test]
fn a_coordinator_that_dies_during_an_abort_leaves_every_cancel_on_the_disk() {
    // The switch that stops the coordinator exists in a debug build only.
    if !cfg!(debug_assertions) {
        eprintln!("skipped: a release build has no test switch to stop the coordinator");
        return;
    }
    let mut h = Harness::new("abortcrash", ABORT_CONFIG);
    h.extra_env
        .push(("QEX_TEST_CRASH_AFTER_ABORT_PLAN".into(), "1".into()));
    h.ok(&["pause", "queue"]);
    let mut ids = Vec::new();
    for _ in 0..12 {
        ids.push(h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]));
    }
    let coordinator = h.coordinator_pid();

    let out = h.qex_within(&["abort", "--all"], Duration::from_secs(60));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the connection must end: {err}");
    assert!(
        err.contains("Run `qex abort` again"),
        "the command must say what the reader does next: {err}"
    );
    h.until("the coordinator stopped", Duration::from_secs(30), || {
        (unsafe { libc::kill(coordinator, 0) }) != 0
    });

    // The disk, before any coordinator reads it.
    for id in &ids {
        let text = std::fs::read_to_string(h.job_dir(id).join("status.json")).unwrap();
        let record: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            record["state"], "cancelled",
            "the record of {id} on the disk"
        );
    }

    // The next coordinator, without the variable.
    h.extra_env.clear();
    h.ok(&["info"]);
    for id in &ids {
        assert_eq!(h.state_of(id), "cancelled");
    }
    let pause = h.ok(&["pause", "--json"]);
    let pause: serde_json::Value = serde_json::from_str(&pause).unwrap();
    assert_eq!(pause["paused"], true, "the pause survives too: {pause}");
}

/// The report never counts as cancelled a job whose cancel is not on the disk.
///
/// A record that qex could not write would come back as a queued job at the
/// next coordinator, so the abort leaves such a job in the queue, names it,
/// and gives the exit code 1.
#[test]
fn an_abort_does_not_count_a_cancel_that_did_not_reach_the_disk() {
    use std::os::unix::fs::PermissionsExt;

    let h = Harness::new("abortunwritable", ABORT_CONFIG);
    h.ok(&["pause", "queue"]);
    let fine = h.submit(&["submit", "--cpu", "1", "--mem", "64MB", "--", "true"]);
    let unwritable = h.submit(&[
        "submit",
        "--name",
        "unwritable",
        "--cpu",
        "1",
        "--mem",
        "64MB",
        "--",
        "true",
    ]);
    let dir = h.job_dir(&unwritable);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = h.qex_within(&["abort", "--all", "--json"], Duration::from_secs(60));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the abort output is not valid JSON");
    assert_eq!(
        out.status.code(),
        Some(1),
        "a cancel that failed gives 1: {report}"
    );
    assert_eq!(report["cancelled"], 1, "the report: {report}");
    let failed = report["not_cancelled"].as_array().expect("a list");
    assert_eq!(failed.len(), 1, "the report: {report}");
    assert_eq!(failed[0]["id"], unwritable, "the report: {report}");
    assert_eq!(failed[0]["name"], "unwritable", "the report: {report}");

    assert_eq!(h.state_of(&fine), "cancelled");
    assert_eq!(
        h.state_of(&unwritable),
        "queued",
        "a job whose cancel did not reach the disk stays in the queue"
    );
    let text = std::fs::read_to_string(dir.join("status.json")).unwrap();
    let record: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(record["state"], "queued", "the disk agrees with the memory");
}
