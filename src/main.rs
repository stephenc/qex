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
mod logcap;
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
mod version;
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
        Command::Completions(args) => cmd_completions(args.shell),
        Command::Complete(args) => commands::complete(args),
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

/// Writes the completions for one shell.
///
/// This command is for a person. An agent writes the full command and needs no
/// completion, so this text does not appear in `qex help agents`.
///
/// The words come from the command line definition itself, so they cannot
/// disagree with the commands that qex has.
#[cfg(unix)]
fn cmd_completions(shell: clap_complete::Shell) -> Result<i32> {
    use std::io::Write;

    let mut command = Cli::command();
    let hidden: Vec<String> = command
        .get_subcommands()
        .filter(|c| c.is_hide_set())
        .map(|c| c.get_name().to_string())
        .collect();
    let valued = options_that_take_a_value(&command);
    let mut out = Vec::new();
    clap_complete::generate(shell, &mut command, "qex", &mut out);

    // The commands and the options come from the definition above. The IDS AND
    // THE NAMES cannot: they change with each job, so the shell must ask qex at
    // the moment of the TAB. `qex __complete` answers that question, and it
    // reads the records on the disk and starts no coordinator.
    //
    // Each part below offers the correct set for its command: `qex kill` offers
    // the jobs that operate, `qex cancel` offers the jobs that wait, and the
    // commands that read a record offer every job.
    //
    // Remove the commands that qex hides. `daemon`, `supervise` and
    // `__complete` are commands that qex gives to itself, and
    // `#[command(hide = true)]` keeps them out of the help text. It does NOT
    // keep them out of the completions, so a person who pressed TAB was offered
    // `qex daemon` — a command that starts a coordinator in the foreground —
    // beside `qex submit`.
    let text = remove_hidden(&String::from_utf8_lossy(&out), &hidden, shell);
    let text = zsh_jobs(&text, shell);

    let mut out = text.into_bytes();
    out.extend_from_slice(dynamic_part(shell, &valued).as_bytes());

    std::io::stdout().write_all(&out)?;
    Ok(0)
}

/// Takes the hidden commands out of the completions.
///
/// Each shell holds them in a different form, so each needs its own removal.
/// A shell that qex cannot treat keeps them, which is the behaviour that qex
/// had; the words are untidy and they are not dangerous.
#[cfg(unix)]
fn remove_hidden(text: &str, hidden: &[String], shell: clap_complete::Shell) -> String {
    use clap_complete::Shell;

    if hidden.is_empty() {
        return text.to_string();
    }

    match shell {
        // bash gives one list of words for each level: `opts="submit run ..."`.
        Shell::Bash => text
            .lines()
            .map(|line| {
                if !line.trim_start().starts_with("opts=\"") {
                    return line.to_string();
                }
                let mut kept = line.to_string();
                for name in hidden {
                    kept = kept.replace(&format!(" {name}\""), "\"");
                    kept = kept.replace(&format!(" {name} "), " ");
                }
                kept
            })
            .collect::<Vec<_>>()
            .join("\n"),

        // zsh and fish each give one line for each command.
        Shell::Zsh | Shell::Fish => text
            .lines()
            .filter(|line| {
                !hidden.iter().any(|name| {
                    line.contains(&format!("'{name}:")) || line.contains(&format!("-a \"{name}\""))
                })
            })
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n"),

        // elvish and powershell each OFFER a command on one line. The block
        // that says what the command accepts stays, and it offers nothing.
        Shell::Elvish | Shell::PowerShell => text
            .lines()
            .filter(|line| {
                !hidden.iter().any(|name| {
                    line.trim_start().starts_with(&format!("cand {name} "))
                        || line.contains(&format!("::new('{name}', '{name}',"))
                })
            })
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n"),

        _ => text.to_string(),
    }
}

/// The commands that take a job, and the set that each one offers.
///
/// `qex kill` takes a job that operates, and `qex cancel` takes a job that
/// waits in the queue. A candidate that the command would refuse teaches the
/// wrong command.
#[cfg(unix)]
const READS: &str = "status wait logs rerun clean";
#[cfg(unix)]
const ACTIVE: &str = "kill";
#[cfg(unix)]
const QUEUED: &str = "cancel";

/// The options of those commands that take a VALUE, as bash sees them.
///
/// bash gives the completion the word before the cursor and nothing else, so
/// the completion must know which of those words takes the next word. Without
/// this, `qex kill --signal <TAB>` offered a job, and `qex status --json
/// <TAB>` offered none.
///
/// The list comes from the command line definition, so an option that a later
/// change adds is here without a second edit.
#[cfg(unix)]
fn options_that_take_a_value(command: &clap::Command) -> String {
    let mut words: Vec<String> = Vec::new();
    let takes_a_job = |name: &str| {
        READS.split(' ').any(|c| c == name)
            || ACTIVE.split(' ').any(|c| c == name)
            || QUEUED.split(' ').any(|c| c == name)
    };
    for sub in command
        .get_subcommands()
        .filter(|s| takes_a_job(s.get_name()))
    {
        for arg in sub.get_arguments() {
            // A flag such as `--json` takes no value, and it must stay out.
            if !arg.get_action().takes_values() {
                continue;
            }
            if let Some(long) = arg.get_long() {
                words.push(format!("--{long}"));
            }
            for long in arg.get_all_aliases().unwrap_or_default() {
                words.push(format!("--{long}"));
            }
            if let Some(short) = arg.get_short() {
                words.push(format!("-{short}"));
            }
        }
    }
    words.sort();
    words.dedup();
    words.join(" ")
}

/// Makes the job arguments of the zsh completions offer the jobs.
///
/// zsh needs more than an extra function at the end of the file, and for two
/// reasons.
///
/// 1. The file gives the completion to the shell in its last lines, so a
///    function that follows those lines arrives too late.
/// 2. clap gives each job argument the action `_default`, which offers every
///    file and every command on the machine. A press of TAB after `qex kill `
///    thus gave 2674 candidates, and the jobs were lost among them.
///
/// This puts `_qex_jobs` at the top of the file, below the `#compdef` line that
/// zsh needs first, and it then names that function as the action of each job
/// argument.
#[cfg(unix)]
fn zsh_jobs(text: &str, shell: clap_complete::Shell) -> String {
    if shell != clap_complete::Shell::Zsh {
        return text.to_string();
    }

    let helper = r#"
# The ids and the names of the jobs. `qex __complete` reads the records on the
# disk, so a press of TAB starts no coordinator.
_qex_jobs() {
    local -a candidates
    candidates=(${(f)"$(qex __complete $1 2>/dev/null)"})
    (( ${#candidates} )) && compadd -a candidates
}
"#;

    // Which set each command offers.
    let set_of = |name: &str| -> Option<&'static str> {
        if READS.split(' ').any(|c| c == name) {
            Some("ids")
        } else if ACTIVE.split(' ').any(|c| c == name) {
            Some("active")
        } else if QUEUED.split(' ').any(|c| c == name) {
            Some("queued")
        } else {
            None
        }
    };

    let mut done = false;
    let mut here: Option<&'static str> = None;
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if !done && line.trim() == "#compdef qex" {
            lines.push(line.to_string());
            lines.push(helper.to_string());
            done = true;
            continue;
        }
        // `(kill)` opens the arguments of `qex kill`.
        if line.starts_with('(') && line.ends_with(')') {
            here = set_of(line.trim_matches(['(', ')']));
        }
        // A job argument, such as `'*::ids -- The job ids to stop:_default' \`.
        //
        // The line must be a POSITIONAL one. An option line starts with the
        // name of the option, and it takes a value that is not a job: `qex kill
        // --signal <TAB>` must offer a signal and never a job.
        let positional =
            line.starts_with("':") || line.starts_with("'::") || line.starts_with("'*::");
        match here {
            Some(what) if positional && line.ends_with(":_default' \\") => {
                let head = &line[..line.len() - ":_default' \\".len()];
                // The SPACE after the colon is not decoration. zsh runs an
                // action as a command only when the action starts with a
                // space; without it zsh reads `_qex_jobs ids` as one name, and
                // it then offers nothing at all.
                lines.push(format!("{head}: _qex_jobs {what}' \\"));
            }
            _ => lines.push(line.to_string()),
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

/// The part of the completions that asks qex for the ids and the names.
#[cfg(unix)]
fn dynamic_part(shell: clap_complete::Shell, valued: &str) -> String {
    use clap_complete::Shell;

    #[allow(non_snake_case)]
    let VALUED = valued;

    match shell {
        Shell::Bash => format!(
            r#"
# The ids and the names of the jobs. `qex __complete` reads the records on the
# disk, so a press of TAB starts no coordinator.
_qex_jobs() {{
    local subcommand="" word
    for word in "${{COMP_WORDS[@]:1}}"; do
        case "$word" in
            -*) ;;
            *) subcommand="$word"; break ;;
        esac
    done

    local what=""
    case " {READS} " in *" $subcommand "*) what="ids" ;; esac
    case " {ACTIVE} " in *" $subcommand "*) what="active" ;; esac
    case " {QUEUED} " in *" $subcommand "*) what="queued" ;; esac
    [ -n "$what" ] || return 0

    local cur="${{COMP_WORDS[COMP_CWORD]}}"
    # An option, and not a job. Leave those to the part above.
    case "$cur" in -*) return 0 ;; esac

    # The word before takes a VALUE, so this word is that value and not a job:
    # `qex kill --signal <TAB>` must offer a signal.
    #
    # The test names the options that take a value, and it does not ask whether
    # the word starts with a dash. A flag such as `--json` takes no value, so
    # `qex status --json <TAB>` is still a job.
    local prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    case " {VALUED} " in *" $prev "*) return 0 ;; esac

    # Read the candidates one line at a time, and compare them as text.
    #
    # `compgen -W` EXPANDS ITS WORD LIST AGAIN, so a job named
    # `$(rm -rf ~)` would run when somebody pressed TAB. An agent chooses the
    # names of the jobs, and a person presses the TAB, so that name is not
    # trusted text. This loop expands nothing, and it keeps a name that holds a
    # space in one piece.
    local candidate
    while IFS= read -r candidate; do
        [ -n "$candidate" ] || continue
        case "$candidate" in "$cur"*) ;; *) continue ;; esac
        # Put the name on the line in a form that the shell reads as ONE word.
        #
        # bash writes a candidate to the line as it stands, so the name
        # `x; rm -rf ~` would put `; rm -rf ~` on the line beside the command,
        # and the next press of ENTER would then run it. `printf %q` puts a
        # backslash before each character that the shell reads, so the name
        # stays one argument of qex.
        #
        # `compopt -o filenames` asks bash to do the same work, and this code
        # used it first. bash then treats each name as a FILE: a job with the
        # name of a directory got a `/` after it, `$HOME` became a directory as
        # well, and a name that starts with `~` was expanded to a home
        # directory. Each of the three gave qex a name that no job has.
        # `printf %q` has none of that behaviour. It was measured on bash
        # 5.1.16 and on bash 3.2.57, and it gave the stored name in each.
        printf -v candidate '%q' "$candidate"
        # A leading `~` as well. bash 5.1 escapes it above and bash 3.2 does
        # NOT, and bash then reads `~/x` on the line as a home directory. A
        # name is not a path, so the word keeps the character that it holds.
        case "$candidate" in "~"*) candidate="\\$candidate" ;; esac
        COMPREPLY[${{#COMPREPLY[@]}}]="$candidate"
    done < <(qex __complete "$what" 2>/dev/null)
}}

_qex_with_jobs() {{
    _qex "$@"
    _qex_jobs
}}

# `-o nosort` came with bash 4.4, and `complete` refuses the WHOLE command when
# it meets an option name that it does not know. Without this test, sourcing
# this file on the bash 3.2 of macOS wrote `complete: nosort: invalid option
# name`, bound nothing, and the jobs never arrived. clap gates its own line in
# the same way, above.
#
# Do not answer that by dropping `-o nosort`: it is what keeps the newest job
# first, and bash sorts the candidates again without it.
if [[ "${{BASH_VERSINFO[0]}}" -eq 4 && "${{BASH_VERSINFO[1]}}" -ge 4 || "${{BASH_VERSINFO[0]}}" -gt 4 ]]; then
    complete -F _qex_with_jobs -o nosort -o bashdefault -o default qex
else
    complete -F _qex_with_jobs -o bashdefault -o default qex
fi
"#
        ),

        // zsh gets its part from `zsh_jobs`, because the function must exist
        // before the file gives the completion to the shell.
        Shell::Zsh => String::new(),

        Shell::Fish => format!(
            r#"
# The ids and the names of the jobs. `qex __complete` reads the records on the
# disk, so a press of TAB starts no coordinator.
complete -c qex -n "__fish_seen_subcommand_from {READS}" -f -a "(qex __complete ids)"
complete -c qex -n "__fish_seen_subcommand_from {ACTIVE}" -f -a "(qex __complete active)"
complete -c qex -n "__fish_seen_subcommand_from {QUEUED}" -f -a "(qex __complete queued)"
"#
        ),

        // elvish and powershell get the commands and the options, and no ids.
        // qex has no way to test them, and a completion that nobody tested is
        // worse than none: it teaches a value that may not exist.
        _ => String::new(),
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
    println!(
        "job output:   {}",
        match cfg.log_max_bytes()? {
            Some(limit) => format!(
                "{} for each stream of each job; qex keeps the first part and the last part",
                units::format_size(limit)
            ),
            None => "NO LIMIT; a job can fill the disk that holds the records".to_string(),
        }
    );

    // Show this, although the JSON form already holds it.
    //
    // `[politeness]` changes the priority of EVERY job, and it does so by
    // default. A user who asks why a build is slow reads this text form, and a
    // value that only the JSON form holds is a value that nobody reads.
    println!(
        "politeness:   nice {}; io {}, oom_score_adj {} (Linux only)",
        cfg.politeness.nice, cfg.politeness.io, cfg.politeness.oom_score_adj
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
    // SHOW `[claims]` HERE TOO. This command prints the configuration that qex
    // uses, and a reader who cannot see `export_env` cannot tell whether a job
    // receives GOMAXPROCS. The config file is not the answer, because a machine
    // with no file still has these values.
    //
    // READ EVERY CONDITION THAT `export_claim` READS. `[submit] env_capture =
    // "none"` turns the claim off as completely as `export_env = false` does,
    // and a line that reported "yes" for that machine was worse than no line:
    // the owner sets the value, asks this command, and is told the opposite of
    // what the job receives. The one condition that is NOT here is `--cpu` and
    // `--mem` together, because that belongs to a submission and not to a
    // configuration, so the text names it as the remaining requirement.
    println!(
        "claim in job: {}",
        if !cfg.claims.export_env {
            "no; [claims] export_env = false".to_string()
        } else if cfg.submit.env_capture == config::EnvCapture::None {
            "no; [submit] env_capture = \"none\" gives a job no variable that qex chose".to_string()
        } else if cfg.claims.also.is_empty() {
            "yes, with --cpu and --mem together".to_string()
        } else {
            let names: Vec<&str> = cfg.claims.also.iter().map(|h| h.name()).collect();
            format!(
                "yes, with --cpu and --mem together; also {}",
                names.join(", ")
            )
        }
    );
    Ok(())
}
