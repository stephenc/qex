//! This module holds the job specification.
//!
//! A specification records the request from a user or an agent. This module
//! then calculates the command, the environment and the directory for the job.
//!
//! The CLI does this calculation at the time of submission. The coordinator can
//! start many hours before, from a different shell. Its own environment and its
//! own directory are thus not correct for the job. The specification that the
//! coordinator receives is complete. It does not use any external values.

use crate::config::{Config, EnvCapture};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The contents of a job file, as a person or an agent writes it.
///
/// The command is necessary. Each other field is optional. qex has a default
/// value for each other field, or it copies the value from the environment.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobFile {
    pub name: Option<String>,
    pub cwd: Option<String>,
    /// The command as a list of arguments: `["uv", "run", "train.py"]`.
    ///
    /// This field is not a shell command line. qex does not start a shell, so
    /// no quotation marks and no word division are necessary.
    pub command: Vec<String>,
    pub timeout: Option<String>,
    pub tags: Vec<String>,
    pub priority: Option<i32>,
    pub env_capture: Option<EnvCapture>,
    /// The jobs that must succeed before this job starts.
    ///
    /// Give an id or a name. If one of these jobs does not succeed, this job
    /// does not start and its state becomes `skipped`.
    pub needs: Vec<String>,
    /// The jobs that must stop before this job starts.
    ///
    /// Their result is not important. Use this field to control the order only.
    pub after: Vec<String>,
    pub resources: Resources,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Resources {
    /// The number of cores. Give an integer, or `half`, `guess`, `full`, `max`.
    pub cpu: Option<crate::claim::Claim>,
    /// The memory. Give a size such as `8GB`, or `half`, `guess`, `full`, `max`.
    pub mem: Option<crate::claim::Claim>,
}

impl JobFile {
    /// Reads a job file. The file extension selects the format.
    ///
    /// TOML is the format in the documentation. An agent frequently writes YAML
    /// or JSON. qex accepts these two formats also, and the agent does not need
    /// a second try. All three formats use the same structure and the same
    /// tests.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading job file {}", path.display()))?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("toml")
            .to_ascii_lowercase();

        let parsed: Self = match ext.as_str() {
            "yaml" | "yml" => serde_yaml_ng::from_str(&text)
                .map_err(|e| job_file_error(path, &e.to_string()))?,
            "json" => {
                serde_json::from_str(&text).map_err(|e| job_file_error(path, &e.to_string()))?
            }
            _ => toml::from_str(&text).map_err(|e| job_file_error(path, &e.to_string()))?,
        };
        Ok(parsed)
    }
}

/// Makes an error message for an incorrect job file.
///
/// The message contains a small correct example. The reader is usually an
/// agent. The agent can then correct the file and does not read the manual.
fn job_file_error(path: &Path, detail: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "incorrect job file {}: {detail}\n\n\
         A correct job file has this form:\n\n\
         \x20   command = [\"uv\", \"run\", \"train.py\"]\n\n\
         \x20   [resources]\n\
         \x20   cpu = 2\n\
         \x20   mem = \"4GB\"\n\n\
         For a list of all the fields, run `qex help job-file`.",
        path.display()
    )
}

/// The values from the command line, before qex adds the job file and the config.
#[derive(Debug, Clone, Default)]
pub struct SubmitOptions {
    pub name: Option<String>,
    pub cwd: Option<PathBuf>,
    pub cpu: Option<crate::claim::Claim>,
    pub mem: Option<crate::claim::Claim>,
    pub timeout: Option<String>,
    pub tags: Vec<String>,
    pub priority: Option<i32>,
    pub env: Vec<(String, String)>,
    pub env_capture: Option<EnvCapture>,
    pub command: Vec<String>,
    pub job_file: Option<PathBuf>,
    /// The names or ids of the jobs that must succeed first.
    pub needs: Vec<String>,
    /// The names or ids of the jobs that must stop first.
    pub after: Vec<String>,
}

/// The dependencies of a job, as the user wrote them.
///
/// These values are names or ids. The CLI changes them into ids, because that
/// step needs the list of the jobs from the coordinator.
#[derive(Debug, Clone, Default)]
pub struct DependencyNames {
    pub needs: Vec<String>,
    pub after: Vec<String>,
}

/// A complete job specification. Each value here is final.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobSpec {
    pub id: uuid::Uuid,
    pub name: String,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cpu: u64,
    pub mem: u64,
    /// The time limit in seconds. The value `None` means that there is no limit.
    pub timeout: Option<u64>,
    pub tags: Vec<String>,
    pub priority: i32,
    /// The environment mode that qex used for this job.
    ///
    /// `qex status` shows this value. The reader can then see why the job has
    /// this environment, and does not calculate the sequence of the sources.
    pub env_capture: EnvCapture,
    /// The jobs that must succeed before this job starts.
    #[serde(default)]
    pub needs: Vec<uuid::Uuid>,
    /// The jobs that must stop before this job starts.
    #[serde(default)]
    pub after: Vec<uuid::Uuid>,
    pub submitted_at: u64,
}

impl JobSpec {
    /// Makes a complete specification.
    ///
    /// This function combines the command line options, the job file and the
    /// config file.
    ///
    /// A later source replaces an earlier source. The sequence is the same for
    /// a command after `--` and for a job file:
    ///
    /// ```text
    /// environment from the shell  ->  job file [env]  ->  --env K=V
    /// directory from the shell    ->  job file cwd    ->  --cwd D
    /// config file defaults        ->  job file        ->  command line options
    /// ```
    /// Makes a complete specification, without the dependencies.
    ///
    /// The unit tests use this function. The CLI uses `resolve_with_deps`,
    /// because it must also change each dependency name into an id.
    #[cfg(test)]
    pub fn resolve(opts: &SubmitOptions, cfg: &Config) -> Result<Self> {
        Self::resolve_with_deps(opts, cfg).map(|(spec, _)| spec)
    }

    /// Makes a complete specification, and gives the dependency names.
    ///
    /// The names come from the command line and from the job file. The CLI
    /// changes them into ids, because that step needs the coordinator.
    pub fn resolve_with_deps(
        opts: &SubmitOptions,
        cfg: &Config,
    ) -> Result<(Self, DependencyNames)> {
        let file = match &opts.job_file {
            Some(p) => JobFile::load(p)?,
            None => JobFile::default(),
        };

        let command = if !opts.command.is_empty() {
            opts.command.clone()
        } else {
            file.command.clone()
        };
        if command.is_empty() {
            bail!(
                "no command.\n\n\
                 Write the command after `--`:\n\
                 \x20   qex submit --cpu 2 --mem 4GB -- uv run train.py\n\n\
                 Or set `command` in a job file:\n\
                 \x20   qex submit --job train.toml"
            );
        }
        if !opts.command.is_empty() && !file.command.is_empty() {
            bail!(
                "there is a command after `--` and a command in the job file. \
                 Delete one command. qex must have one command only."
            );
        }

        // The command line replaces the job file. The job file replaces the
        // config file.
        let capture = opts
            .env_capture
            .or(file.env_capture)
            .unwrap_or(cfg.submit.env_capture);

        let mut env = capture_env(capture, &cfg.submit.minimal_env);
        // The job file replaces the values from the shell.
        for (k, v) in &file.env {
            env.insert(k.clone(), v.clone());
        }
        // `--env` replaces the two earlier sources. The last value wins.
        for (k, v) in &opts.env {
            env.insert(k.clone(), v.clone());
        }

        // Find the directory: the shell, then the job file, then `--cwd`.
        // Make the path absolute. The coordinator operates in the root
        // directory, so a relative path there points to a different location.
        let cwd = match (&opts.cwd, &file.cwd) {
            (Some(p), _) => p.clone(),
            (None, Some(p)) => PathBuf::from(p),
            (None, None) => std::env::current_dir()
                .context("cannot determine the current directory to capture as the job's cwd")?,
        };
        let cwd = cwd.canonicalize().with_context(|| {
            format!(
                "the job directory {} does not exist, or qex cannot read it",
                cwd.display()
            )
        })?;
        if !cwd.is_dir() {
            bail!("the job directory {} is a file, not a directory", cwd.display());
        }

        // A claim can be a number, or a word such as `half` or `full`. qex
        // calculates the word against the budget here, so the coordinator
        // receives an exact value and the record shows what the job asked for.
        let cpu = match opts.cpu.as_ref().or(file.resources.cpu.as_ref()) {
            Some(c) => c.cores(cfg),
            None => cfg.default_cpu(),
        }
        .max(1);

        let mem = match opts.mem.as_ref().or(file.resources.mem.as_ref()) {
            Some(c) => c.bytes(cfg),
            None => cfg.default_mem()?,
        };

        let timeout = match opts.timeout.as_ref().or(file.timeout.as_ref()) {
            Some(s) => crate::units::parse_duration(s)
                .map_err(|e| anyhow::anyhow!("--timeout: {e}"))?,
            None => cfg.default_timeout()?,
        };

        let name = opts
            .name
            .clone()
            .or(file.name)
            // The program name is the default. `qex list` is then easy to read,
            // and the user does not need to remember the `--name` option.
            .unwrap_or_else(|| default_name(&command));

        // A name must not have the form of an id.
        //
        // qex uses the form of a dependency value to choose a rule: an id must
        // exist, and a name must give a job that has not stopped. A name with
        // the form of an id would take the rule for an id, and the test that
        // protects against a job of an earlier run would not happen.
        if name.parse::<uuid::Uuid>().is_ok() {
            bail!(
                "the name `{name}` has the form of a job id, and qex does not accept it.\n\n\
                 qex reads a dependency value as an id when it has this form, and an id \
                 follows a different rule from a name. A name with this form would avoid \
                 that rule.\n\n\
                 Use a name that a person can read, such as `build` or `test`."
            );
        }

        let mut tags = file.tags;
        tags.extend(opts.tags.iter().cloned());
        tags.sort();
        tags.dedup();

        let mut deps = DependencyNames {
            needs: file.needs,
            after: file.after,
        };
        deps.needs.extend(opts.needs.iter().cloned());
        deps.after.extend(opts.after.iter().cloned());

        Ok((Self {
            id: uuid::Uuid::new_v4(),
            name,
            cwd,
            command,
            env,
            cpu,
            mem,
            timeout: timeout.map(|d| d.as_secs()),
            tags,
            priority: opts.priority.or(file.priority).unwrap_or(0),
            env_capture: capture,
            // The CLI changes each name into an id after this function, because
            // that step needs the coordinator.
            needs: Vec::new(),
            after: Vec::new(),
            submitted_at: crate::sys::now_secs(),
        }, deps))
    }
}

/// Gives the last part of the program path.
///
/// For example, `/usr/bin/python3` gives `python3`.
fn default_name(command: &[String]) -> String {
    command
        .first()
        .map(|c| {
            Path::new(c)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(c)
                .to_string()
        })
        .unwrap_or_else(|| "job".to_string())
}

/// Makes the first environment for the selected mode.
fn capture_env(mode: EnvCapture, minimal: &[String]) -> BTreeMap<String, String> {
    match mode {
        EnvCapture::All => std::env::vars().collect(),
        EnvCapture::Minimal => minimal
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.clone(), v)))
            .collect(),
        EnvCapture::None => BTreeMap::new(),
    }
}

/// Reads one `KEY=VALUE` pair from the `--env` option.
pub fn parse_env_pair(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!(
            "incorrect --env value `{s}`. Use the form KEY=VALUE. \
             Example: --env RUST_LOG=debug"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testutil::{env_lock, EnvVar};

    fn opts(command: &[&str]) -> SubmitOptions {
        SubmitOptions {
            command: command.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// Writes a job file and gives its path.
    fn job_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qex-spec-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn env_is_captured_by_default() {
        let _guard = env_lock();
        let _m = EnvVar::set("QEX_TEST_MARKER", "present");
        let spec = JobSpec::resolve(&opts(&["true"]), &Config::default()).unwrap();
        assert_eq!(
            spec.env.get("QEX_TEST_MARKER").map(String::as_str),
            Some("present"),
            "default capture should inherit the invoking environment"
        );
    }

    #[test]
    fn capture_none_drops_everything_but_explicit_values() {
        let _guard = env_lock();
        let _m = EnvVar::set("QEX_TEST_MARKER", "present");
        let mut o = opts(&["true"]);
        o.env_capture = Some(EnvCapture::None);
        o.env = vec![("ONLY".into(), "this".into())];
        let spec = JobSpec::resolve(&o, &Config::default()).unwrap();
        assert_eq!(spec.env.len(), 1);
        assert_eq!(spec.env.get("ONLY").unwrap(), "this");
    }

    #[test]
    fn capture_minimal_keeps_only_the_allowlist() {
        let _guard = env_lock();
        let _m = EnvVar::set("QEX_TEST_MARKER", "present");
        let _h = EnvVar::set("HOME", "/home/example");
        let mut o = opts(&["true"]);
        o.env_capture = Some(EnvCapture::Minimal);
        let spec = JobSpec::resolve(&o, &Config::default()).unwrap();
        assert_eq!(spec.env.get("HOME").map(String::as_str), Some("/home/example"));
        assert!(
            !spec.env.contains_key("QEX_TEST_MARKER"),
            "minimal capture leaked a non-allowlisted variable"
        );
    }

    /// The command `--env-capture none --env PATH=...` must operate correctly.
    /// An override applies to each environment mode.
    #[test]
    fn overrides_apply_on_top_of_every_capture_mode() {
        let _guard = env_lock();
        for mode in [EnvCapture::All, EnvCapture::Minimal, EnvCapture::None] {
            let mut o = opts(&["true"]);
            o.env_capture = Some(mode);
            o.env = vec![("PATH".into(), "/only/here".into())];
            let spec = JobSpec::resolve(&o, &Config::default()).unwrap();
            assert_eq!(
                spec.env.get("PATH").unwrap(),
                "/only/here",
                "override lost under capture mode {mode:?}"
            );
        }
    }

    #[test]
    fn cli_env_overrides_captured_env() {
        let _guard = env_lock();
        let _m = EnvVar::set("QEX_TEST_OVERRIDE", "from-shell");
        let mut o = opts(&["true"]);
        o.env = vec![("QEX_TEST_OVERRIDE".into(), "from-cli".into())];
        let spec = JobSpec::resolve(&o, &Config::default()).unwrap();
        assert_eq!(spec.env.get("QEX_TEST_OVERRIDE").unwrap(), "from-cli");
    }

    /// Tests the full sequence of sources in one test.
    ///
    /// The job file replaces the shell. The command line replaces the job file.
    /// The documentation gives this rule.
    #[test]
    fn precedence_runs_captured_then_job_file_then_cli() {
        let _guard = env_lock();
        let _m = EnvVar::set("QEX_LAYER", "captured");
        let dir = tmpdir("precedence");

        // The shell is the only source.
        let mut o = opts(&["true"]);
        assert_eq!(
            JobSpec::resolve(&o, &Config::default()).unwrap().env["QEX_LAYER"],
            "captured"
        );

        // The job file replaces the shell.
        let jf = job_file(
            &dir,
            "j.toml",
            "command = [\"true\"]\n[env]\nQEX_LAYER = \"job-file\"\n",
        );
        o = SubmitOptions {
            job_file: Some(jf.clone()),
            ..Default::default()
        };
        assert_eq!(
            JobSpec::resolve(&o, &Config::default()).unwrap().env["QEX_LAYER"],
            "job-file"
        );

        // The command line replaces the job file.
        o.env = vec![("QEX_LAYER".into(), "cli".into())];
        assert_eq!(
            JobSpec::resolve(&o, &Config::default()).unwrap().env["QEX_LAYER"],
            "cli"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn job_files_parse_as_toml_yaml_or_json() {
        let _guard = env_lock();
        let dir = tmpdir("formats");
        let cases = [
            (
                "j.toml",
                "command = [\"echo\", \"hi\"]\nname = \"t\"\n[resources]\ncpu = 3\nmem = \"8GB\"\n",
            ),
            (
                "j.yaml",
                "command: [echo, hi]\nname: t\nresources:\n  cpu: 3\n  mem: 8GB\n",
            ),
            (
                "j.json",
                r#"{"command":["echo","hi"],"name":"t","resources":{"cpu":3,"mem":"8GB"}}"#,
            ),
        ];
        for (fname, body) in cases {
            let p = job_file(&dir, fname, body);
            let o = SubmitOptions {
                job_file: Some(p),
                ..Default::default()
            };
            let spec = JobSpec::resolve(&o, &Config::default())
                .unwrap_or_else(|e| panic!("{fname} failed to resolve: {e}"));
            assert_eq!(spec.command, vec!["echo", "hi"], "{fname}");
            assert_eq!(spec.cpu, 3, "{fname}");
            assert_eq!(spec.mem, 8 << 30, "{fname}");
            assert_eq!(spec.name, "t", "{fname}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_bad_job_file_error_carries_a_working_example() {
        let dir = tmpdir("badfile");
        let p = job_file(&dir, "bad.toml", "command = \"not an array\"\n");
        let err = JobFile::load(&p).unwrap_err().to_string();
        assert!(err.contains("qex help job-file"), "got: {err}");
        assert!(err.contains("command = ["), "error lacks an example: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_job_file_fields_are_rejected_rather_than_ignored() {
        let dir = tmpdir("unknown");
        // A field name with a spelling error must give an error. If qex ignores
        // the field, the job operates without the limit that the author set.
        let p = job_file(&dir, "typo.toml", "command = [\"true\"]\ntimeoutt = \"5m\"\n");
        let err = JobFile::load(&p).unwrap_err().to_string();
        assert!(err.contains("timeoutt"), "got: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A job that gives no size must use the sizes from the config file.
    /// Without this rule, an agent that forgets `--cpu` gets an unknown claim.
    #[test]
    fn config_defaults_supply_the_job_size() {
        let _guard = env_lock();
        let cfg: Config = toml::from_str(
            "[defaults]\ncpu = 4\nmem = \"6GB\"\ntimeout = \"30m\"\n",
        )
        .unwrap();
        let spec = JobSpec::resolve(&opts(&["true"]), &cfg).unwrap();
        assert_eq!(spec.cpu, 4);
        assert_eq!(spec.mem, 6 << 30);
        assert_eq!(spec.timeout, Some(1800));
    }

    /// The command line and the job file both replace the config defaults.
    #[test]
    fn explicit_sizes_replace_the_config_defaults() {
        let _guard = env_lock();
        let cfg: Config =
            toml::from_str("[defaults]\ncpu = 4\nmem = \"6GB\"\ntimeout = \"30m\"\n").unwrap();

        let mut o = opts(&["true"]);
        o.cpu = Some(crate::claim::Claim::Exact(2));
        o.mem = Some(crate::claim::Claim::Exact(1 << 30));
        o.timeout = Some("0".into());
        let spec = JobSpec::resolve(&o, &cfg).unwrap();
        assert_eq!(spec.cpu, 2);
        assert_eq!(spec.mem, 1 << 30);
        assert_eq!(spec.timeout, None, "`--timeout 0` must remove the limit");

        let dir = tmpdir("defaults");
        let p = job_file(
            &dir,
            "j.toml",
            "command = [\"true\"]\n[resources]\ncpu = 3\nmem = \"2GB\"\n",
        );
        let o = SubmitOptions {
            job_file: Some(p),
            ..Default::default()
        };
        let spec = JobSpec::resolve(&o, &cfg).unwrap();
        assert_eq!(spec.cpu, 3);
        assert_eq!(spec.mem, 2 << 30);
        // The job file gives no timeout, so the config default applies.
        assert_eq!(spec.timeout, Some(1800));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// With no config file, a job gets 1 core and an equal part of the memory.
    /// The default job size thus scales with the machine.
    #[test]
    fn built_in_defaults_supply_a_size() {
        let _guard = env_lock();
        let spec = JobSpec::resolve(&opts(&["true"]), &Config::default()).unwrap();
        assert_eq!(spec.cpu, 1, "the default job must claim 1 core");

        let cores = crate::sys::cpu_count().max(1);
        let expected = (crate::sys::total_memory() / cores).max(1 << 28);
        assert_eq!(
            spec.mem, expected,
            "the default memory must be the machine memory divided by the cores"
        );
        assert_eq!(spec.timeout, None, "a job must have no time limit by default");
    }

    #[test]
    fn a_command_in_both_places_is_ambiguous_and_rejected() {
        let dir = tmpdir("dupcmd");
        let p = job_file(&dir, "j.toml", "command = [\"from-file\"]\n");
        let mut o = opts(&["from-cli"]);
        o.job_file = Some(p);
        let err = JobSpec::resolve(&o, &Config::default()).unwrap_err().to_string();
        assert!(err.contains("job file"), "the error must name both sources: {err}");
        assert!(err.contains("--"), "the error must name both sources: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn last_env_flag_wins() {
        let mut o = opts(&["true"]);
        o.env = vec![("K".into(), "first".into()), ("K".into(), "second".into())];
        let spec = JobSpec::resolve(&o, &Config::default()).unwrap();
        assert_eq!(spec.env.get("K").unwrap(), "second");
    }

    #[test]
    fn cwd_is_captured_and_made_absolute() {
        let spec = JobSpec::resolve(&opts(&["true"]), &Config::default()).unwrap();
        assert!(spec.cwd.is_absolute());
        assert_eq!(spec.cwd, std::env::current_dir().unwrap().canonicalize().unwrap());
    }

    #[test]
    fn missing_cwd_is_rejected_at_submit_time_not_at_run_time() {
        let mut o = opts(&["true"]);
        o.cwd = Some(PathBuf::from("/nonexistent/qex/dir"));
        let err = JobSpec::resolve(&o, &Config::default()).unwrap_err().to_string();
        assert!(err.contains("does not exist"), "got: {err}");
    }

    #[test]
    fn a_command_is_required_and_the_error_shows_both_ways_to_give_one() {
        let err = JobSpec::resolve(&opts(&[]), &Config::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("qex submit"), "error should show usage: {err}");
        assert!(err.contains("--job"), "error should mention job files: {err}");
    }

    /// A name must not have the form of an id.
    ///
    /// qex chooses the rule for a dependency from the form of the value. A name
    /// with the form of an id would take the rule for an id, and it would avoid
    /// the test that protects against a job of an earlier run.
    #[test]
    fn a_name_with_the_form_of_an_id_is_refused() {
        let _guard = env_lock();
        let mut o = opts(&["true"]);
        o.name = Some("550e8400-e29b-41d4-a716-446655440000".into());
        let err = JobSpec::resolve(&o, &Config::default()).unwrap_err().to_string();
        assert!(err.contains("form of a job id"), "got: {err}");

        // A name that a person can read is accepted.
        o.name = Some("build".into());
        assert!(JobSpec::resolve(&o, &Config::default()).is_ok());
    }

    #[test]
    fn name_defaults_to_the_program_basename() {
        let spec = JobSpec::resolve(&opts(&["/usr/bin/python3", "x.py"]), &Config::default()).unwrap();
        assert_eq!(spec.name, "python3");
    }

    #[test]
    fn env_pairs_parse_and_values_may_contain_equals() {
        assert_eq!(
            parse_env_pair("K=a=b").unwrap(),
            ("K".to_string(), "a=b".to_string())
        );
        assert_eq!(
            parse_env_pair("K=").unwrap(),
            ("K".to_string(), String::new())
        );
        assert!(parse_env_pair("noequals").is_err());
        assert!(parse_env_pair("=novalue").is_err());
    }
}
