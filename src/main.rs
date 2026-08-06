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

mod claim;
mod cli;
mod client;
mod commands;
mod config;
mod daemon;
mod enforce;
mod help;
mod job;
mod lifecycle;
mod logsel;
mod paths;
mod peers;
mod proto;
mod sched;
mod schema;
mod spec;
mod supervisor;
mod sys;
#[cfg(test)]
mod testutil;
mod units;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use cli::{Cli, Command};

/// The exit code for a command line that qex cannot read.
const EXIT_USAGE: i32 = 2;

fn main() {
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
        Command::List(args) => commands::list(args),
        Command::Status(args) => commands::status(args),
        Command::Wait(args) => commands::wait(args),
        Command::Logs(args) => commands::logs(args),
        Command::Kill(args) => commands::kill(args),
        Command::Cancel(args) => commands::cancel(args),
        Command::Clean(args) => commands::clean(args),
        Command::Info(args) => commands::info(args),
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
fn print_root_help() {
    print!("{}", help::BANNER);
    println!();
    let mut cmd = Cli::command();
    cmd.print_help().ok();
    println!();
    println!("Topics for `qex help <topic>`: {}", help::TOPICS.join(", "));
}

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
fn print_config_summary(cfg: &config::Config) -> Result<()> {
    let path = paths::config_file()?;
    println!(
        "config file: {} ({})",
        path.display(),
        if path.exists() { "read" } else { "absent; qex uses the default values" }
    );
    println!();
    println!("machine:      {} cores, {}", sys::cpu_count(), units::format_size(sys::total_memory()));
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
