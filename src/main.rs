//! qex — Queued EXecutor.
//!
//! qex is a job queue for long tasks on a local machine. It controls the
//! quantity of memory and the number of cores that the jobs use together.
//!
//! qex gives three guarantees:
//!
//! 1. Each job has a UUID. qex is the parent process of each job, and it uses
//!    `waitpid` on that process. qex does not read `/proc` and it does not
//!    search command lines. A monitor process thus cannot find itself and then
//!    wait for ever.
//! 2. The supervisor of each job writes the job result. The result stays
//!    correct if the coordinator stops, fails or restarts.
//! 3. qex uses the resource claims to select the jobs that operate together.
//!    Two agents on one machine thus do not start the out-of-memory killer.

/// qex needs the process control of Unix, so it does not build for Windows.
///
/// Without this message, a build for Windows gives 114 errors from the modules
/// below, and not one of them says what a reader must do. A tool that reports a
/// fault must give the cause and the remedy, and that rule holds for the build
/// as much as for a job.
#[cfg(not(unix))]
compile_error!(
    "qex builds for Linux and macOS only.\n\n     qex controls processes with the session and the process group of Unix, and it holds a job \
     with `waitpid` on one process id. Windows has no equivalent of those, so this is a port \
     and not an option that somebody can turn on.\n\n     ON WINDOWS, USE WSL2. qex builds and operates there with no change, and the jobs that an \
     agent starts (make, cargo, uv) are usually in WSL2 as well.\n\n     See https://github.com/stephenc/qex for the reason in full."
);

#[cfg(unix)]
mod capabilities;
#[cfg(unix)]
mod claim;
#[cfg(unix)]
mod cli;
#[cfg(unix)]
mod client;
#[cfg(unix)]
mod commands;
#[cfg(unix)]
mod config;
#[cfg(unix)]
mod daemon;
#[cfg(unix)]
mod enforce;
#[cfg(unix)]
mod fanout;
#[cfg(unix)]
mod help;
#[cfg(unix)]
mod history;
#[cfg(unix)]
mod job;
#[cfg(unix)]
mod keys;
#[cfg(unix)]
mod lifecycle;
#[cfg(unix)]
mod logsel;
#[cfg(unix)]
mod paths;
#[cfg(unix)]
mod peers;
#[cfg(unix)]
mod pipeline;
#[cfg(unix)]
mod proto;
#[cfg(unix)]
mod sched;
#[cfg(unix)]
mod schema;
#[cfg(unix)]
mod spec;
#[cfg(unix)]
mod style;
#[cfg(unix)]
mod supervisor;
#[cfg(unix)]
mod sys;
#[cfg(test)]
#[cfg(unix)]
mod testutil;
#[cfg(unix)]
mod top;
#[cfg(unix)]
mod units;
#[cfg(unix)]
mod usage;
#[cfg(unix)]
mod watchers;

#[cfg(unix)]
use anyhow::{Context, Result};
#[cfg(unix)]
use clap::{CommandFactory, Parser};
#[cfg(unix)]
use cli::{Cli, Command};

/// The exit code for a command line that qex cannot read.
#[cfg(unix)]
const EXIT_USAGE: i32 = 2;

#[cfg(not(unix))]
fn main() {}

#[cfg(unix)]
fn main() {
    // Let the system stop this process when a reader closes a pipe.
    //
    // Rust ignores SIGPIPE, so a write to a closed pipe gives an error, and the
    // print macros panic on that error. A command such as `qex list | head`
    // then writes a Rust panic and a note about a backtrace, which reads as a
    // fault in qex. Every other tool in a pipe stops in silence.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let code = match run() {
        Ok(code) => code,
        Err(e) => {
            // Write the cause of each error. An agent then sees the full
            // sequence and does not need to run the command a second time.
            eprintln!("qex: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(unix)]
fn run() -> Result<i32> {
    let cli = Cli::parse();

    let Some(command) = cli.command else {
        // `qex` without a command writes the help text. This is not an error.
        // An agent that runs `qex` to learn the tool must get the text, and the
        // text points to `qex help agents`.
        print_root_help();
        return Ok(0);
    };

    match command {
        Command::Help(args) => cmd_help(args.topic.as_deref()),
        Command::Schema(args) => cmd_schema(args.which.as_deref()),
        Command::Config(args) => cmd_config(args),
        Command::Submit(args) => commands::submit(args),
        Command::Run(args) => commands::run(args),
        Command::Pipeline(args) => commands::pipeline(args),
        Command::List(args) => commands::list(args),
        Command::Status(args) => commands::status(args),
        Command::Wait(args) => commands::wait(args),
        Command::Logs(args) => commands::logs(args),
        Command::Kill(args) => commands::kill(args),
        Command::Cancel(args) => commands::cancel(args),
        Command::Rerun(args) => commands::rerun(args),
        Command::Clean(args) => commands::clean(args),
        Command::Gc(args) => commands::gc(args),
        Command::Du(args) => commands::du(args),
        Command::Info(args) => commands::info(args),
        Command::Version(args) => commands::version(args),
        Command::Watchers(args) => watchers::report(args.json),
        Command::Top(args) => top::run(args),
        Command::Daemon(_) => {
            daemon::run()?;
            Ok(0)
        }
        Command::Supervise(args) => {
            let id = args
                .id
                .parse::<uuid::Uuid>()
                .context("the supervise command needs a job id")?;
            supervisor::main(id)
        }
    }
}

/// Writes the banner and the usage text.
///
/// The banner comes first. An agent that reads a few lines only must still see
/// the pointer to `qex help agents`.
#[cfg(unix)]
fn print_root_help() {
    print!("{}", help::banner());
    println!();
    let mut cmd = Cli::command();
    cmd.print_help().ok();
    println!();
    println!("Topics for `qex help <topic>`: {}", help::TOPICS.join(", "));
}

#[cfg(unix)]
fn cmd_help(topic: Option<&str>) -> Result<i32> {
    let Some(name) = topic else {
        print_root_help();
        return Ok(0);
    };

    match help::topic(name) {
        Some(text) => {
            print!("{text}");
            Ok(0)
        }
        None => {
            eprintln!(
                "qex: there is no help topic `{name}`.\n\nThe topics are: {}\n\nAgents: run `qex help agents`.",
                help::TOPICS.join(", ")
            );
            Ok(EXIT_USAGE)
        }
    }
}

#[cfg(unix)]
fn cmd_schema(which: Option<&str>) -> Result<i32> {
    let Some(name) = which else {
        eprintln!(
            "qex: name a schema. The schemas are: {}\n\nExample: qex schema job",
            schema::NAMES.join(", ")
        );
        return Ok(EXIT_USAGE);
    };

    match schema::schema(name) {
        Some(text) => {
            print!("{text}");
            Ok(0)
        }
        None => {
            eprintln!(
                "qex: there is no schema `{name}`. The schemas are: {}",
                schema::NAMES.join(", ")
            );
            Ok(EXIT_USAGE)
        }
    }
}

#[cfg(unix)]
fn cmd_config(args: cli::ConfigArgs) -> Result<i32> {
    use cli::ConfigAction;

    // Accept `qex config --json` as well as `qex config show --json`. A user
    // who writes the shorter form must not get a usage error.
    let json_flag = args.json;
    match args
        .action
        .unwrap_or(ConfigAction::Show { json: json_flag })
    {
        ConfigAction::Path => {
            let path = paths::config_file()?;
            let exists = path.exists();
            println!("{}", path.display());
            if !exists {
                eprintln!("qex: this file does not exist. qex uses the default values.");
            }
            Ok(0)
        }
        ConfigAction::Show { json } => {
            let cfg = config::Config::load()?;
            cfg.validate()?;
            if json || json_flag {
                println!("{}", serde_json::to_string_pretty(&cfg)?);
            } else {
                print_config_summary(&cfg)?;
            }
            Ok(0)
        }
    }
}

/// Writes the values that qex uses now.
///
/// This text shows the calculated values, not the text of the config file. A
/// reader can then see the true budget without a calculation.
#[cfg(unix)]
fn print_config_summary(cfg: &config::Config) -> Result<()> {
    let path = paths::config_file()?;
    println!(
        "config file: {} ({})",
        path.display(),
        if path.exists() {
            "read"
        } else {
            "absent; qex uses the default values"
        }
    );
    println!();
    println!(
        "machine:      {} cores, {}",
        sys::cpu_count(),
        units::format_size(sys::total_memory())
    );
    println!(
        "budget:       {} cores, {}",
        cfg.budget_cpu()?,
        units::format_size(cfg.budget_mem()?)
    );
    println!(
        "default job:  {} core(s), {}, {}",
        cfg.default_cpu(),
        units::format_size(cfg.default_mem()?),
        match cfg.default_timeout()? {
            Some(d) => format!("timeout {}", units::format_duration(d)),
            None => "no timeout".to_string(),
        }
    );
    println!(
        "keep free:    {} of memory",
        units::format_size(cfg.reserve_mem()?)
    );
    match enforce::startup_warning(cfg) {
        Some(warning) => {
            println!("enforcement:  {:?} — NOT ACTIVE", cfg.enforce.mode);
            println!("              {warning}");
        }
        None if cfg.enforce.mode.is_on() => {
            println!("enforcement:  {:?}, active", cfg.enforce.mode)
        }
        None => println!("enforcement:  off; the claims control the queue only"),
    }
    // Report the true state of the shared accounting. A message that says "on"
    // for a directory that qex cannot use is the fault that this program must
    // not have.
    println!("peers:        {}", peers::describe(cfg));
    println!("oversized:    {:?}", cfg.queue.oversized);
    println!("environment:  capture {:?}", cfg.submit.env_capture);
    Ok(())
}
