//! This module decides if two commands come from one context.
//!
//! `qex abort` with no scope option acts on the jobs of the caller, and not on
//! the jobs of a different agent that runs as the same user. A queue belongs
//! to one user, and several agents run as that user on one machine, so the
//! user is too wide. The working directory alone is too wide as well: two
//! agents can work in one directory.
//!
//! The context of a command is the chain of processes above it. `qex submit`
//! records that chain on the job, and `qex abort` sends its own chain. A job
//! is in the context of the caller when the two chains share one process that
//! is still the SAME process, below the point where the session ends.
//!
//! # Where a session ends
//!
//! Every process on the machine has the first process of the machine above
//! it, and every pane of a terminal multiplexer has the multiplexer above it.
//! A shared process at that height says nothing. The chain of an agent looks
//! like this, from the command upward:
//!
//! ```text
//!     bash            the shell that the agent runs for one command
//!     claude          the agent
//!     bash            the shell of the terminal pane
//!     tmux_server     the multiplexer: every pane shares it
//!     systemd         the first process: everything shares it
//! ```
//!
//! The agent is the process to match on. Four rules find the end of the
//! session, and the first process that one of them names is the boundary:
//!
//! 1. The first process of the machine, or of a container: pid 1, or a
//!    process with no parent.
//! 2. A process with one of the names in [`BOUNDARY_NAMES`]: a multiplexer, a
//!    login service, a terminal program, a service manager, or the supervisor
//!    of a qex job.
//! 3. A process with no controlling terminal, above a process that has one.
//!    A terminal program holds the other end of the terminal, so it has no
//!    controlling terminal itself. This rule finds a terminal program that the
//!    list does not name.
//! 4. The top of the chain. The walk stops at a process whose parent qex
//!    could not read. When that parent is the first process (macOS refuses
//!    to describe it to a user), the boundary is that first process, above
//!    the chain, and the whole chain is the context: a chain with no terminal
//!    and no named boundary, such as a command under a service, still matches
//!    its own earlier commands. When that parent is any other process, qex
//!    cannot say what it is, so the top process itself is the boundary: a
//!    match through it could reach the work of everybody under it.
//!
//! The context is the part of the chain BELOW the boundary. Only an empty
//! chain gives an empty context, and an empty context matches nothing.
//!
//! # What a reader can predict
//!
//! Two commands from one agent share the agent process, so they share a
//! context, whatever shell the agent ran for each one. Two agents in two panes
//! of one multiplexer share nothing below the multiplexer, so they do not.
//! Two commands that a person types in one shell share that shell, and so do
//! two agents that one shell started: the shell is below the boundary. A job that
//! a job submitted has the supervisor of that job as its boundary, so the jobs
//! that one job submits share a context with each other and with nothing
//! above them. A command with no terminal anywhere above it, such as a command
//! under `cron` or under a runner of a build service, shares a context with
//! everything under the same service.
//!
//! `qex status` shows the chain of a job with the boundary marked, so a reader
//! can see why a job was, or was not, in scope.

use crate::job::Ancestor;

/// The names of the programs at which a session ends.
///
/// THIS LIST IS A CLAIM ABOUT THE MACHINES THAT RUN QEX, and a later reader
/// extends it. Each name is in the SAFE FORM of `job::safe_name`, because the
/// chain stores the names in that form: `tmux: server` is stored as
/// `tmux_server`. Linux gives the first 15 characters of a program name.
/// Rules 1 and 3 in the module documentation catch most of what this list
/// does not name.
pub const BOUNDARY_NAMES: &[&str] = &[
    // The first process.
    "init",
    "systemd",
    "launchd",
    // A container.
    "tini",
    "dumb-init",
    "docker-init",
    "conmon",
    "containerd-shim",
    // A multiplexer, a login service, a scheduler.
    "tmux_server",
    "tmux",
    "screen",
    "sshd",
    "sshd-session",
    "login",
    "cron",
    "crond",
    // A terminal program, or an editor with a terminal inside.
    "gnome-terminal-",
    "konsole",
    "xterm",
    "alacritty",
    "kitty",
    "wezterm-gui",
    "foot",
    "urxvt",
    "terminator",
    "tilix",
    "Terminal",
    "iTerm2",
    "code",
    "code-insiders",
    "cursor",
    "windsurf",
    "zed",
    // The supervisor of a qex job. The jobs that one job submits form a
    // context of their own.
    "qex",
];

/// Gives the part of a chain below the point where the session ends.
///
/// The chain runs from the command upward. The result is empty for an empty
/// chain only, and an empty result matches nothing.
pub fn below_boundary(chain: &[Ancestor]) -> &[Ancestor] {
    match boundary_index(chain) {
        Some(index) => &chain[..index.min(chain.len())],
        None => &[],
    }
}

/// Gives the position at which the session ends.
///
/// A position equal to the length of the chain says that the boundary is the
/// first process of the machine, above the chain (rule 4). `None` is an empty
/// chain.
pub fn boundary_index(chain: &[Ancestor]) -> Option<usize> {
    let mut terminal_below = false;
    for (index, process) in chain.iter().enumerate() {
        if process.pid == 1 || process.ppid == 0 {
            return Some(index);
        }
        if BOUNDARY_NAMES.contains(&process.name.as_str()) {
            return Some(index);
        }
        if terminal_below && !process.terminal {
            return Some(index);
        }
        terminal_below |= process.terminal;
    }
    // Rule 4: the top of the chain.
    let top = chain.last()?;
    if top.ppid == 1 {
        Some(chain.len())
    } else {
        Some(chain.len() - 1)
    }
}

/// Tests if two chains share one process below the end of the session.
///
/// A process matches by its number AND its start time. The machine gives the
/// number of a process that stopped to a later process, and a match on the
/// number alone would put a new session into the context of an old one. A
/// process with no start time matches nothing, for the same reason.
pub fn shared(job: &[Ancestor], caller: &[Ancestor]) -> bool {
    let job = below_boundary(job);
    let caller = below_boundary(caller);
    job.iter()
        .any(|a| a.start.is_some() && caller.iter().any(|c| c.pid == a.pid && c.start == a.start))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: i32, ppid: i32, name: &str, terminal: bool) -> Ancestor {
        Ancestor {
            pid,
            ppid,
            start: Some(1000 + pid as u64),
            name: name.into(),
            cwd: None,
            terminal,
        }
    }

    /// The chain of an agent in one pane of a multiplexer.
    fn agent(shell: i32, agent: i32, pane: i32) -> Vec<Ancestor> {
        vec![
            process(shell, agent, "bash", false),
            process(agent, pane, "claude", true),
            process(pane, 6152, "bash", true),
            process(6152, 1, "tmux_server", false),
            process(1, 0, "systemd", false),
        ]
    }

    #[test]
    fn two_commands_of_one_agent_share_a_context() {
        let earlier = agent(100, 50, 40);
        let later = agent(101, 50, 40);
        assert!(shared(&earlier, &later), "the agent process is shared");
    }

    #[test]
    fn two_agents_in_two_panes_share_no_context() {
        let one = agent(100, 50, 40);
        let two = agent(200, 60, 41);
        assert!(!shared(&one, &two), "the multiplexer is above the boundary");
    }

    /// A later process with the number of the agent is not the agent.
    ///
    /// The later chain is a new session in a new pane, whose agent took the
    /// number of the earlier agent. The number is the only thing that the
    /// two chains share.
    #[test]
    fn a_reused_number_does_not_match() {
        let earlier = agent(100, 50, 40);
        let same_number = agent(101, 50, 41);
        assert!(
            shared(&earlier, &same_number),
            "the test must share the number before it changes the start time"
        );

        let mut later = agent(101, 50, 41);
        later[1].start = Some(999_999);
        assert!(!shared(&earlier, &later), "the start time differs");

        let mut unknown = agent(101, 50, 41);
        unknown[1].start = None;
        assert!(
            !shared(&earlier, &unknown),
            "a process with no start time matches nothing"
        );
    }

    /// The first process is a boundary by its position, whatever its name.
    #[test]
    fn the_first_process_of_a_container_is_a_boundary() {
        let one = vec![
            process(100, 50, "bash", false),
            process(50, 1, "claude", false),
            process(1, 0, "bash", false),
        ];
        let two = vec![
            process(200, 60, "bash", false),
            process(60, 1, "claude", false),
            process(1, 0, "bash", false),
        ];
        assert_eq!(boundary_index(&one), Some(2));
        assert!(!shared(&one, &two));
        let same = vec![
            process(101, 50, "bash", false),
            one[1].clone(),
            one[2].clone(),
        ];
        assert!(shared(&one, &same));
    }

    /// A terminal program that the list does not name is found by rule 3.
    #[test]
    fn a_terminal_program_with_no_terminal_of_its_own_is_a_boundary() {
        let chain = vec![
            process(100, 50, "bash", false),
            process(50, 40, "claude", true),
            process(40, 30, "zsh", true),
            process(30, 1, "some-new-term", false),
            process(1, 0, "systemd", false),
        ];
        assert_eq!(boundary_index(&chain), Some(3));
    }

    /// A chain that ends at a process whose parent qex could not read ends
    /// there: that process is the boundary, because the unknown parent can
    /// be a terminal program or a service that everybody shares.
    #[test]
    fn a_chain_whose_parent_is_unknown_ends_at_its_top() {
        let one = vec![
            process(100, 50, "bash", false),
            process(50, 40, "claude", false),
        ];
        let two = vec![
            process(101, 50, "bash", false),
            process(50, 40, "claude", false),
        ];
        assert_eq!(boundary_index(&one), Some(1));
        assert_eq!(below_boundary(&one).len(), 1);
        assert!(!shared(&one, &two), "the shared process is the boundary");
    }

    /// A chain with no terminal and no named boundary, whose top is a child
    /// of the first process, is one context as a whole: two commands under a
    /// runner of a build service share the shell that started the tests.
    ///
    /// This is the chain of a macOS build machine, where qex cannot read the
    /// first process and so never records it.
    #[test]
    fn a_chain_under_a_service_with_no_terminal_matches_its_own_commands() {
        let runner = |test_pid: i32| {
            vec![
                process(test_pid, 7465, "e2e-c044ff0b01e", false),
                process(7465, 7464, "cargo", false),
                process(7464, 6536, "bash", false),
                process(6536, 6530, "Runner.Worker", false),
                process(6530, 898, "Runner.Listener", false),
                process(898, 885, "hosted-compute-", false),
                process(885, 1, "bash", false),
            ]
        };
        let one = runner(7741);
        let two = runner(7742);
        assert_eq!(boundary_index(&one), Some(one.len()));
        assert_eq!(below_boundary(&one).len(), one.len());
        assert!(
            shared(&one, &two),
            "the same caller must match its own jobs"
        );
    }

    #[test]
    fn an_empty_chain_has_no_boundary() {
        assert_eq!(boundary_index(&[]), None);
        assert!(below_boundary(&[]).is_empty());
    }

    /// The multiplexer is found by its stored name, with no terminal below
    /// it to make rule 3 fire.
    #[test]
    fn the_multiplexer_is_a_boundary_by_its_stored_name() {
        let chain = vec![
            process(100, 50, "bash", false),
            process(50, 6152, "claude", false),
            process(6152, 1, "tmux_server", false),
            process(1, 0, "systemd", false),
        ];
        assert_eq!(boundary_index(&chain), Some(2));
    }

    /// The jobs that a job submits share the supervisor as their boundary.
    #[test]
    fn the_supervisor_of_a_job_is_a_boundary() {
        let chain = vec![
            process(300, 200, "sh", false),
            process(200, 190, "qex", false),
            process(190, 1, "qex", false),
            process(1, 0, "systemd", false),
        ];
        assert_eq!(boundary_index(&chain), Some(1));
    }

    #[test]
    fn an_empty_chain_matches_nothing() {
        let one = agent(100, 50, 40);
        assert!(!shared(&one, &[]));
        assert!(!shared(&[], &one));
    }
}
