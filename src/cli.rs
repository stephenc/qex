//! This module defines the command line of qex.
//!
//! An agent is a frequent reader of this help text. Each description is thus
//! short, and it says what the option does, not how the option operates.

use crate::config::EnvCapture;
use crate::job::JobState;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "qex",
    version,
    about = "Queued EXecutor. A job queue for long tasks on this machine.",
    long_about = None,
    disable_help_subcommand = true,
    subcommand_required = false,
    arg_required_else_help = false
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Put a job in the queue. Writes the job id to stdout.
    Submit(SubmitArgs),

    /// Show the jobs and their states.
    List(ListArgs),

    /// Show one job in detail.
    Status(StatusArgs),

    /// Wait until a job stops. Use this command. Do not write a monitor script.
    Wait(WaitArgs),

    /// Show the output of a job.
    Logs(LogsArgs),

    /// Stop a job that operates.
    Kill(KillArgs),

    /// Remove a job from the queue before it starts.
    Cancel(CancelArgs),

    /// Delete the records of the jobs that stopped.
    Clean(CleanArgs),

    /// Watch the queue. Shows the claim and the true use of each job.
    Top(TopArgs),

    /// Show the coordinator: its process id, its budget and its load.
    Info(InfoArgs),

    /// Show the configuration or its location.
    Config(ConfigArgs),

    /// Write the JSON Schema for a job file or for the status output.
    Schema(SchemaArgs),

    /// Explain a topic. Agents: run `qex help agents` first.
    Help(HelpArgs),

    /// Run the coordinator. qex starts this process for you.
    #[command(hide = true)]
    Daemon(DaemonArgs),

    /// Supervise one job. The coordinator starts this process.
    #[command(hide = true)]
    Supervise(SuperviseArgs),
}

#[derive(Debug, Args)]
pub struct SubmitArgs {
    /// Cores for the job: a number, or `half`, `guess`, `full` or `max`.
    #[arg(long, value_name = "N|half|full", value_parser = parse_cpu_claim)]
    pub cpu: Option<crate::claim::Claim>,

    /// Memory for the job: a size such as 8GB, or `half`, `guess`, `full`, `max`.
    #[arg(long, value_name = "SIZE|half|full", value_parser = parse_mem_claim)]
    pub mem: Option<crate::claim::Claim>,

    /// A name for the job, to show in `qex list`.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// The directory for the job. The default is your current directory.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Add or replace one environment variable. Repeat this option as needed.
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = crate::spec::parse_env_pair)]
    pub env: Vec<(String, String)>,

    /// Select the environment that the job receives.
    #[arg(long, value_name = "MODE", value_parser = parse_env_capture)]
    pub env_capture: Option<EnvCapture>,

    /// Give the job no environment from your shell. Same as --env-capture none.
    #[arg(long, conflicts_with = "env_capture")]
    pub no_env_capture: bool,

    /// Add a tag, to select the job in `qex list --tag`.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// The queue priority. A larger number starts earlier.
    #[arg(long, value_name = "N", allow_negative_numbers = true)]
    pub priority: Option<i32>,

    /// Stop the job after this time. Example: 4h. Use 0 for no limit.
    #[arg(long, value_name = "TIME")]
    pub timeout: Option<String>,

    /// Wait for these jobs, and stop if one of them does not succeed.
    ///
    /// Give an id or a name. Repeat the option, or separate the values with a
    /// comma. If a needed job does not succeed, this job does not start and its
    /// state becomes `skipped`.
    #[arg(long = "needs", value_name = "ID,ID", value_delimiter = ',')]
    pub needs: Vec<String>,

    /// Wait for these jobs, whatever their result.
    ///
    /// Use this option to control the order only. This job starts after those
    /// jobs stop, and the result of those jobs is not important.
    #[arg(long = "after", value_name = "ID,ID", value_delimiter = ',')]
    pub after: Vec<String>,

    /// Read the job from a TOML, YAML or JSON file.
    #[arg(long = "job", value_name = "FILE")]
    pub job_file: Option<PathBuf>,

    /// The command to run. Write it after `--`.
    #[arg(trailing_var_arg = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Show the jobs in this state only.
    #[arg(long, value_name = "STATE")]
    pub state: Option<String>,

    /// Show the jobs with this tag only.
    #[arg(long, value_name = "TAG")]
    pub tag: Option<String>,

    /// Write the output as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// The job id, or the start of the id.
    pub id: String,

    /// Write the output as JSON.
    #[arg(long)]
    pub json: bool,

    /// Show the environment of the job. It can contain secrets.
    #[arg(long)]
    pub show_env: bool,

    /// Do not show any output of the job.
    ///
    /// Without this option, the status of a job that did not succeed holds the
    /// last lines of its standard error.
    #[arg(long)]
    pub no_logs: bool,

    /// Wait until the job stops, then show the status.
    ///
    /// The exit code is the code of `qex wait`. This option gives the result
    /// and the cause of a failure with one command.
    #[arg(long)]
    pub wait: bool,

    /// Stop the wait after this time. The job continues. Example: 30m.
    #[arg(long, value_name = "TIME", requires = "wait")]
    pub timeout: Option<String>,

    #[command(flatten)]
    pub select: crate::logsel::LogSelect,
}

#[derive(Debug, Args)]
pub struct WaitArgs {
    /// The job ids to wait for.
    #[arg(required = true)]
    pub ids: Vec<String>,

    /// Stop the wait after this time. The job continues. Example: 30m.
    #[arg(long, value_name = "TIME")]
    pub timeout: Option<String>,

    /// Exit with the exit code of the job.
    #[arg(long)]
    pub passthrough: bool,

    /// Write the output as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// The job id.
    pub id: String,

    /// Show the output while the job operates.
    ///
    /// Use --grep with this option to see the lines that matter as they arrive.
    #[arg(long, short)]
    pub follow: bool,

    /// Write the output as JSON, with one field for each stream.
    #[arg(long, conflicts_with = "follow")]
    pub json: bool,

    #[command(flatten)]
    pub select: crate::logsel::LogSelect,
}

#[derive(Debug, Args)]
pub struct KillArgs {
    /// The job ids to stop.
    #[arg(required = true)]
    pub ids: Vec<String>,

    /// The first signal to send. The default is TERM.
    #[arg(long, value_name = "SIGNAL", default_value = "TERM")]
    pub signal: String,

    /// The time to wait before qex sends KILL. Example: 10s.
    #[arg(long, value_name = "TIME", default_value = "10s")]
    pub grace: String,
}

#[derive(Debug, Args)]
pub struct CancelArgs {
    /// The job ids to remove from the queue.
    #[arg(required = true)]
    pub ids: Vec<String>,
}

#[derive(Debug, Args)]
pub struct CleanArgs {
    /// The job ids to delete.
    pub ids: Vec<String>,

    /// Delete the records of every job.
    #[arg(long)]
    pub all: bool,

    /// Delete the records of the jobs in this state. Use `done` for all the
    /// final states.
    #[arg(long, value_name = "STATE")]
    pub state: Option<String>,

    /// Delete the records that are older than this time. Example: 7d.
    #[arg(long, value_name = "TIME")]
    pub older_than: Option<String>,
}

#[derive(Debug, Args)]
pub struct TopArgs {
    /// The time between two refreshes, in seconds.
    #[arg(long, short = 'i', value_name = "SECONDS", default_value = "2")]
    pub interval: f64,

    /// Write the page one time and stop. Use this option in a script.
    #[arg(long)]
    pub once: bool,
}

#[derive(Debug, Args)]
pub struct InfoArgs {
    /// Write the output as JSON.
    #[arg(long)]
    pub json: bool,

    /// Do not start a coordinator. Use this option to test for one.
    ///
    /// Without this option, the command starts a coordinator if none operates.
    /// A script that stops a coordinator thus needs this option, or it starts
    /// the process that it wants to stop.
    #[arg(long)]
    pub no_start: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: Option<ConfigAction>,

    /// Write the output as JSON. Same as `qex config show --json`.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Show the configuration that qex uses now.
    Show {
        /// Write the output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the location of the config file.
    Path,
}

#[derive(Debug, Args)]
pub struct SchemaArgs {
    /// The schema to write: job or status.
    #[arg(value_name = "WHICH")]
    pub which: Option<String>,
}

#[derive(Debug, Args)]
pub struct HelpArgs {
    /// The topic. Agents: use `agents`.
    pub topic: Option<String>,
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Stay in the foreground. qex uses this option for the tests.
    #[arg(long)]
    pub foreground: bool,
}

#[derive(Debug, Args)]
pub struct SuperviseArgs {
    /// The id of the job to supervise.
    pub id: String,
}

fn parse_env_capture(s: &str) -> Result<EnvCapture, String> {
    s.parse()
}

fn parse_cpu_claim(s: &str) -> Result<crate::claim::Claim, String> {
    crate::claim::Claim::parse(s, false)
}

fn parse_mem_claim(s: &str) -> Result<crate::claim::Claim, String> {
    crate::claim::Claim::parse(s, true)
}

/// Reads a state name for the `--state` options.
///
/// The name `done` selects each final state. An agent frequently wants "the
/// jobs that stopped" and does not want to name each state.
pub enum StateFilter {
    One(JobState),
    Done,
    Active,
}

impl StateFilter {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "done" | "finished" | "terminal" => Ok(Self::Done),
            "active" => Ok(Self::Active),
            other => other.parse::<JobState>().map(Self::One),
        }
    }

    pub fn matches(&self, state: JobState) -> bool {
        match self {
            Self::One(s) => state == *s,
            Self::Done => state.is_terminal(),
            Self::Active => !state.is_terminal(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_definition_is_valid() {
        // This test finds a conflict between two options, or a duplicate name.
        Cli::command().debug_assert();
    }

    #[test]
    fn qex_with_no_arguments_is_not_an_error() {
        // An agent that runs `qex` must get the help text. It must not get a
        // usage error, because the help text points to `qex help agents`.
        let cli = Cli::try_parse_from(["qex"]).expect("`qex` alone must parse");
        assert!(cli.command.is_none());
    }

    #[test]
    fn submit_takes_the_command_after_two_dashes() {
        let cli = Cli::try_parse_from([
            "qex", "submit", "--cpu", "2", "--mem", "4GB", "--", "uv", "run", "train.py",
        ])
        .unwrap();
        let Some(Command::Submit(a)) = cli.command else {
            panic!("expected a submit command")
        };
        assert_eq!(a.cpu, Some(crate::claim::Claim::Exact(2)));
        assert_eq!(a.mem, Some(crate::claim::Claim::Exact(4 << 30)));
        assert_eq!(a.command, vec!["uv", "run", "train.py"]);
    }

    /// The claim words must operate on the command line, for both options.
    #[test]
    fn the_claim_words_parse_on_the_command_line() {
        use crate::claim::Claim;

        let cli = Cli::try_parse_from([
            "qex", "submit", "--cpu", "guess", "--mem", "half", "--", "true",
        ])
        .unwrap();
        let Some(Command::Submit(a)) = cli.command else {
            panic!("expected a submit command")
        };
        assert_eq!(a.cpu, Some(Claim::Half));
        assert_eq!(a.mem, Some(Claim::Half));

        let cli =
            Cli::try_parse_from(["qex", "submit", "--cpu", "max", "--mem", "full", "--", "true"])
                .unwrap();
        let Some(Command::Submit(a)) = cli.command else {
            panic!("expected a submit command")
        };
        assert_eq!(a.cpu, Some(Claim::Full));
        assert_eq!(a.mem, Some(Claim::Full));

        // An unknown word must give an error at the command line, and not a
        // silent claim of an unexpected size.
        assert!(Cli::try_parse_from(["qex", "submit", "--cpu", "lots", "--", "true"]).is_err());
    }

    /// The job command can have options with the same names as the qex options.
    /// qex must give each of these options to the job.
    #[test]
    fn options_after_two_dashes_belong_to_the_job() {
        let cli =
            Cli::try_parse_from(["qex", "submit", "--", "prog", "--cpu", "99", "--json"]).unwrap();
        let Some(Command::Submit(a)) = cli.command else {
            panic!("expected a submit command")
        };
        assert_eq!(a.cpu, None, "qex must not read the option of the job");
        assert_eq!(a.command, vec!["prog", "--cpu", "99", "--json"]);
    }

    #[test]
    fn env_options_repeat_and_parse() {
        let cli = Cli::try_parse_from([
            "qex", "submit", "--env", "A=1", "--env", "B=x=y", "--", "true",
        ])
        .unwrap();
        let Some(Command::Submit(a)) = cli.command else {
            panic!("expected a submit command")
        };
        assert_eq!(
            a.env,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "x=y".to_string())
            ]
        );
    }

    #[test]
    fn an_incorrect_env_option_is_refused() {
        assert!(Cli::try_parse_from(["qex", "submit", "--env", "novalue", "--", "true"]).is_err());
    }

    #[test]
    fn the_two_environment_options_conflict() {
        // Two options that set the same value must not appear together.
        assert!(Cli::try_parse_from([
            "qex",
            "submit",
            "--no-env-capture",
            "--env-capture",
            "all",
            "--",
            "true"
        ])
        .is_err());
    }

    #[test]
    fn a_negative_priority_parses() {
        let cli =
            Cli::try_parse_from(["qex", "submit", "--priority", "-5", "--", "true"]).unwrap();
        let Some(Command::Submit(a)) = cli.command else {
            panic!("expected a submit command")
        };
        assert_eq!(a.priority, Some(-5));
    }

    #[test]
    fn each_help_topic_has_text() {
        for name in crate::help::TOPICS {
            let text = crate::help::topic(name)
                .unwrap_or_else(|| panic!("the topic `{name}` has no text"));
            assert!(text.len() > 200, "the topic `{name}` is too short");
        }
    }

    /// The user asked for `qex help agents`. The name `agent` must also operate,
    /// because an agent can write either name.
    #[test]
    fn the_agents_topic_has_two_names() {
        assert_eq!(crate::help::topic("agents"), crate::help::topic("agent"));
        assert!(crate::help::topic("agents").is_some());
    }

    /// The banner must name the agents topic. That pointer is the reason for
    /// the banner.
    #[test]
    fn the_banner_points_to_the_agents_topic() {
        assert!(crate::help::banner().contains("qex help agents"));
        // The banner must give the length of the page, so a reader opens it one
        // time only.
        assert!(
            crate::help::banner().contains(&crate::help::AGENTS.lines().count().to_string()),
            "the banner must give the number of lines: {}",
            crate::help::banner()
        );
    }

    /// The agents topic must warn about the monitor script fault. That warning
    /// is the reason for the topic.
    #[test]
    fn the_agents_topic_warns_about_monitor_scripts() {
        let text = crate::help::AGENTS;
        assert!(text.contains("pgrep"), "the topic must name the pgrep fault");
        assert!(text.contains("qex wait"), "the topic must give the solution");
    }

    /// The topic must show the pattern that operates inside a harness.
    ///
    /// `qex wait` blocks, and the harness of an agent reports the end of a
    /// background command. The two together need no timer.
    #[test]
    fn the_agents_topic_gives_the_pattern_for_a_harness() {
        let text = crate::help::AGENTS;
        assert!(
            text.contains("--wait"),
            "the topic must name `qex status --wait`"
        );
        assert!(
            text.contains("background"),
            "the topic must say to run it in the background of the harness"
        );
    }

    /// The help text must steer an agent away from a test job.
    ///
    /// The advice to measure a job invites an agent to add a measurement step
    /// to each task. That step costs time and gives no benefit for a task that
    /// the agent runs one time.
    #[test]
    fn the_help_text_steers_away_from_a_test_job() {
        let agents = crate::help::AGENTS;
        assert!(
            agents.contains("Do not run a small test job"),
            "the topic must tell an agent not to measure a task first"
        );
        assert!(
            agents.contains("guess"),
            "the topic must give the alternative to a test job"
        );

        let resources = crate::help::RESOURCES;
        assert!(
            resources.contains("many times"),
            "the topic must say when a measurement is useful"
        );
        assert!(
            resources.contains("not necessary"),
            "the topic must say when a measurement is not necessary"
        );
    }

    #[test]
    fn state_filters_accept_a_state_or_a_group() {
        assert!(StateFilter::parse("running")
            .unwrap()
            .matches(JobState::Running));
        assert!(!StateFilter::parse("running")
            .unwrap()
            .matches(JobState::Queued));
        assert!(StateFilter::parse("done").unwrap().matches(JobState::Failed));
        assert!(!StateFilter::parse("done")
            .unwrap()
            .matches(JobState::Running));
        assert!(StateFilter::parse("active")
            .unwrap()
            .matches(JobState::Queued));
        assert!(StateFilter::parse("nonsense").is_err());
    }
}
