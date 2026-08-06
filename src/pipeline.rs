//! This module reads a file that describes several stages, and submits them
//! together.
//!
//! # Why a pipeline file, and not several submissions
//!
//! A dependency by name is easy to write, and a name is not unique in time. An
//! agent that runs the same four stages twice writes `--needs solve` in both
//! runs, and that name then gives two jobs. A dependency by id is unique, and
//! it needs the agent to keep each id between the commands.
//!
//! A pipeline file removes the choice. The names in the file belong to that
//! file and to that one submission. qex reads them, changes each one into the
//! id that it made a moment before, and no name leaves the file. A second run
//! of the same file makes new jobs with new ids, and the two runs never meet.
//!
//! The stages of one submission also share a group id, so one command can wait
//! for the whole pipeline, stop it, or delete its records.

use crate::spec::{JobFile, JobSpec, SubmitOptions};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A file that describes several stages.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineFile {
    /// A name for the whole pipeline, for `qex list` to show.
    pub name: Option<String>,
    /// The stages. Each one is a job file with a name.
    pub jobs: Vec<Stage>,
}

/// One stage of a pipeline.
///
/// A stage holds every field of a job file, and its name is necessary. The
/// other stages use that name.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Stage {
    /// The name of this stage. The other stages use it in `needs` and `after`.
    pub name: String,
    pub cwd: Option<String>,
    pub command: Vec<String>,
    pub timeout: Option<String>,
    pub tags: Vec<String>,
    pub priority: Option<i32>,
    pub env_capture: Option<crate::config::EnvCapture>,
    /// The stages that must succeed before this one.
    pub needs: Vec<String>,
    /// The stages that must stop before this one, whatever their result.
    pub after: Vec<String>,
    pub resources: crate::spec::Resources,
    pub env: BTreeMap<String, String>,
}

impl PipelineFile {
    /// Reads a pipeline file. The extension selects the format.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the pipeline file {}", path.display()))?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("toml")
            .to_ascii_lowercase();

        let parsed: Self = match ext.as_str() {
            "yaml" | "yml" => serde_yaml_ng::from_str(&text).map_err(|e| error(path, &e.to_string()))?,
            "json" => serde_json::from_str(&text).map_err(|e| error(path, &e.to_string()))?,
            _ => toml::from_str(&text).map_err(|e| error(path, &e.to_string()))?,
        };
        parsed.validate()?;
        Ok(parsed)
    }

    /// Tests the file for the faults that a person makes.
    pub fn validate(&self) -> Result<()> {
        if self.jobs.is_empty() {
            bail!("this pipeline file has no stage. Add a `[[jobs]]` section.");
        }

        let mut seen = BTreeSet::new();
        for stage in &self.jobs {
            if stage.name.trim().is_empty() {
                bail!("each stage needs a name, because the other stages use it");
            }
            if stage.name.parse::<uuid::Uuid>().is_ok() {
                bail!(
                    "the stage name `{}` has the form of a job id. Use a name that a person \
                     can read, such as `build`.",
                    stage.name
                );
            }
            if !seen.insert(stage.name.clone()) {
                bail!(
                    "two stages have the name `{}`. Each name in a pipeline must be \
                     different, because the other stages use it.",
                    stage.name
                );
            }
            if stage.command.is_empty() {
                bail!("the stage `{}` has no command", stage.name);
            }
        }

        // Each dependency must name a stage of this file.
        for stage in &self.jobs {
            for dep in stage.needs.iter().chain(stage.after.iter()) {
                if !seen.contains(dep) {
                    bail!(
                        "the stage `{}` waits for `{dep}`, and this file has no stage with \
                         that name. A stage can wait for the stages of its own file only.\n\n\
                         The stages of this file are: {}",
                        stage.name,
                        self.jobs
                            .iter()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }

        self.order()?;
        Ok(())
    }

    /// Gives the stages in an order where each stage comes after the stages
    /// that it waits for.
    ///
    /// This function also finds a circle. A file can describe one, because the
    /// stages of a file can name each other in any direction.
    pub fn order(&self) -> Result<Vec<usize>> {
        let index: BTreeMap<&str, usize> = self
            .jobs
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.as_str(), i))
            .collect();

        let mut done: Vec<usize> = Vec::new();
        let mut state = vec![0u8; self.jobs.len()];
        let mut path: Vec<usize> = Vec::new();

        for start in 0..self.jobs.len() {
            if state[start] != 0 {
                continue;
            }
            self.visit(start, &index, &mut state, &mut done, &mut path)?;
        }
        Ok(done)
    }

    fn visit(
        &self,
        at: usize,
        index: &BTreeMap<&str, usize>,
        state: &mut [u8],
        done: &mut Vec<usize>,
        path: &mut Vec<usize>,
    ) -> Result<()> {
        // 1 means "this stage is on the path now", so a second visit is a circle.
        if state[at] == 1 {
            let mut names: Vec<&str> = path
                .iter()
                .skip_while(|i| **i != at)
                .map(|i| self.jobs[*i].name.as_str())
                .collect();
            names.push(self.jobs[at].name.as_str());
            bail!(
                "the stages make a circle: {}. A stage cannot wait for itself, and no \
                 stage can start.",
                names.join(" -> ")
            );
        }
        if state[at] == 2 {
            return Ok(());
        }

        state[at] = 1;
        path.push(at);
        let stage = &self.jobs[at];
        for dep in stage.needs.iter().chain(stage.after.iter()) {
            if let Some(next) = index.get(dep.as_str()) {
                self.visit(*next, index, state, done, path)?;
            }
        }
        path.pop();
        state[at] = 2;
        done.push(at);
        Ok(())
    }
}

fn error(path: &std::path::Path, detail: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "incorrect pipeline file {}: {detail}\n\n\
         A pipeline file has this form:\n\n\
         \x20   [[jobs]]\n\
         \x20   name = \"build\"\n\
         \x20   command = [\"make\"]\n\n\
         \x20   [[jobs]]\n\
         \x20   name = \"test\"\n\
         \x20   command = [\"make\", \"test\"]\n\
         \x20   needs = [\"build\"]\n\n\
         For every field, run `qex help pipeline`.",
        path.display()
    )
}

/// Makes the specification of one stage.
///
/// The dependencies are not here. The caller adds them, because it holds the
/// id that qex made for each earlier stage.
pub fn stage_spec(
    stage: &Stage,
    cfg: &crate::config::Config,
    group: uuid::Uuid,
    group_name: &str,
) -> Result<JobSpec> {
    let file = JobFile {
        name: Some(stage.name.clone()),
        cwd: stage.cwd.clone(),
        command: stage.command.clone(),
        timeout: stage.timeout.clone(),
        tags: stage.tags.clone(),
        priority: stage.priority,
        env_capture: stage.env_capture,
        // The caller resolves these names inside the file.
        needs: Vec::new(),
        after: Vec::new(),
        resources: stage.resources.clone(),
        env: stage.env.clone(),
    };

    let opts = SubmitOptions::default();
    let (mut spec, _) = JobSpec::resolve_from_file(&opts, cfg, file)?;
    spec.group = Some(group);
    spec.group_name = Some(group_name.to_string());
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(text: &str) -> Result<PipelineFile> {
        let parsed: PipelineFile = toml::from_str(text)?;
        parsed.validate()?;
        Ok(parsed)
    }

    #[test]
    fn a_file_with_stages_in_order_is_accepted() {
        let p = file(
            r#"
[[jobs]]
name = "build"
command = ["make"]

[[jobs]]
name = "test"
command = ["make", "test"]
needs = ["build"]
"#,
        )
        .unwrap();
        assert_eq!(p.jobs.len(), 2);

        // The order puts a stage after the stages that it waits for.
        let order = p.order().unwrap();
        let names: Vec<&str> = order.iter().map(|i| p.jobs[*i].name.as_str()).collect();
        assert_eq!(names, vec!["build", "test"]);
    }

    /// A file can describe its stages in any order.
    #[test]
    fn the_stages_can_come_in_any_order_in_the_file() {
        let p = file(
            r#"
[[jobs]]
name = "ship"
command = ["true"]
needs = ["test"]

[[jobs]]
name = "test"
command = ["true"]
needs = ["build"]

[[jobs]]
name = "build"
command = ["true"]
"#,
        )
        .unwrap();
        let order = p.order().unwrap();
        let names: Vec<&str> = order.iter().map(|i| p.jobs[*i].name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "ship"]);
    }

    /// A circle must give an error that names the stages in it.
    ///
    /// A submission by command line cannot make a circle, because a job can
    /// name the jobs before it only. A file can, because its stages name each
    /// other.
    #[test]
    fn a_circle_of_stages_is_refused() {
        let err = file(
            r#"
[[jobs]]
name = "a"
command = ["true"]
needs = ["b"]

[[jobs]]
name = "b"
command = ["true"]
needs = ["a"]
"#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("circle"), "got: {err}");
        assert!(err.contains("a") && err.contains("b"), "the error must name the stages: {err}");
    }

    #[test]
    fn a_stage_that_waits_for_itself_is_refused() {
        let err = file(
            r#"
[[jobs]]
name = "a"
command = ["true"]
needs = ["a"]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("circle"), "got: {err}");
    }

    /// A name that no stage has is a fault of the file, and qex must find it
    /// before it submits anything.
    #[test]
    fn a_dependency_on_a_stage_that_does_not_exist_is_refused() {
        let err = file(
            r#"
[[jobs]]
name = "test"
command = ["true"]
needs = ["build"]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no stage with that name"), "got: {err}");
        assert!(err.contains("test"), "the error must list the stages: {err}");
    }

    #[test]
    fn two_stages_with_one_name_are_refused() {
        let err = file(
            r#"
[[jobs]]
name = "build"
command = ["true"]

[[jobs]]
name = "build"
command = ["false"]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("two stages"), "got: {err}");
    }

    #[test]
    fn a_stage_needs_a_name_and_a_command() {
        assert!(file("[[jobs]]\ncommand = [\"true\"]\n")
            .unwrap_err()
            .to_string()
            .contains("needs a name"));
        assert!(file("[[jobs]]\nname = \"a\"\n")
            .unwrap_err()
            .to_string()
            .contains("no command"));
        assert!(file("")
            .unwrap_err()
            .to_string()
            .contains("no stage"));
    }

    /// A stage name with the form of an id would break the rule that qex uses
    /// to choose between a name and an id.
    #[test]
    fn a_stage_name_with_the_form_of_an_id_is_refused() {
        let err = file(
            r#"
[[jobs]]
name = "550e8400-e29b-41d4-a716-446655440000"
command = ["true"]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("form of a job id"), "got: {err}");
    }

    /// A diamond is a correct shape, and the order must respect it.
    #[test]
    fn a_diamond_gives_a_correct_order() {
        let p = file(
            r#"
[[jobs]]
name = "a"
command = ["true"]

[[jobs]]
name = "b"
command = ["true"]
needs = ["a"]

[[jobs]]
name = "c"
command = ["true"]
needs = ["a"]

[[jobs]]
name = "d"
command = ["true"]
needs = ["b", "c"]
"#,
        )
        .unwrap();

        let order = p.order().unwrap();
        let names: Vec<&str> = order.iter().map(|i| p.jobs[*i].name.as_str()).collect();
        let position = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(position("a") < position("b"));
        assert!(position("a") < position("c"));
        assert!(position("b") < position("d"));
        assert!(position("c") < position("d"));
    }
}
