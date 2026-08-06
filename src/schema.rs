//! This module holds the JSON Schema for the two formats that qex publishes.
//!
//! An agent reads a schema to write a job file, or to read the `--json` output.
//! The schemas are text in this file. qex does not build them from the Rust
//! types, because the text also holds the descriptions and the examples.

/// Gives the schema for one name.
pub fn schema(which: &str) -> Option<&'static str> {
    match which.trim().to_ascii_lowercase().as_str() {
        "job" | "job-file" | "spec" => Some(JOB),
        "status" | "job-status" => Some(STATUS),
        "pipeline" | "pipelines" | "pipeline-file" => Some(PIPELINE),
        _ => None,
    }
}

pub const NAMES: &[&str] = &["job", "status", "pipeline"];

pub const JOB: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://github.com/qex/schema/job.json",
  "title": "qex job file",
  "description": "One job for `qex submit --job FILE`. qex reads TOML, YAML and JSON.",
  "type": "object",
  "required": ["command"],
  "additionalProperties": false,
  "properties": {
    "command": {
      "type": "array",
      "items": { "type": "string" },
      "minItems": 1,
      "description": "The program and its arguments. This is not a shell command line. To use a shell feature, name the shell: [\"bash\", \"-lc\", \"a | b\"].",
      "examples": [["uv", "run", "train.py", "--epochs", "50"]]
    },
    "name": {
      "type": "string",
      "description": "The name in `qex list`. The default is the program name."
    },
    "cwd": {
      "type": "string",
      "description": "The directory for the job. The default is the directory of the `qex submit` command."
    },
    "timeout": {
      "type": "string",
      "description": "The time limit. Use s, m, h or d. Use \"0\" for no limit. The default is no limit.",
      "examples": ["30s", "5m", "4h", "0"]
    },
    "tags": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Tags for `qex list --tag`."
    },
    "priority": {
      "type": "integer",
      "description": "The queue priority. A larger number starts earlier. The default is 0."
    },
    "env_capture": {
      "type": "string",
      "enum": ["all", "minimal", "none"],
      "description": "The environment that the job receives. \"all\" copies your shell. \"minimal\" copies PATH, HOME, USER, LOGNAME, SHELL, LANG and TZ. \"none\" copies nothing. The default is \"all\"."
    },
    "resources": {
      "type": "object",
      "additionalProperties": false,
      "description": "The claim for this job. qex uses the claim to decide how many jobs operate together.",
      "properties": {
        "cpu": {
          "anyOf": [
            { "type": "integer", "minimum": 1 },
            { "type": "string", "enum": ["half", "guess", "auto", "full", "max", "all"] }
          ],
          "description": "The number of cores. Give an integer, or a word: \"half\" and \"guess\" give one half of the budget, and \"full\" and \"max\" give the full budget. The default is 1.",
          "examples": [2, "guess", "full"]
        },
        "mem": {
          "type": "string",
          "description": "The memory. Give a size, or a word: \"half\" and \"guess\" give one half of the budget, and \"full\" and \"max\" give the full budget. One unit step is 1024. The default is the machine memory divided by the core count.",
          "examples": ["8GB", "512MB", "guess", "full"]
        }
      }
    },
    "env": {
      "type": "object",
      "additionalProperties": { "type": "string" },
      "description": "Environment variables. These values replace the values from your shell."
    },
    "needs": {
      "type": "array",
      "items": { "type": "string" },
      "description": "The jobs that must succeed before this job starts. Give an id or a name. If one of these jobs does not succeed, this job does not run and its state becomes skipped.",
      "examples": [["build", "lint"]]
    },
    "after": {
      "type": "array",
      "items": { "type": "string" },
      "description": "The jobs that must stop before this job starts. Their result is not important. Use this field for a cleanup step that must run also when an earlier stage fails.",
      "examples": [["build"]]
    }
  }
}
"##;

pub const PIPELINE: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://github.com/qex/schema/pipeline.json",
  "title": "qex pipeline file",
  "description": "Several stages for `qex pipeline FILE`. qex reads TOML, YAML and JSON. The names in this file belong to this file and to one submission: qex changes each one into the id of the job that it just made, so two runs of the same file never share a name.",
  "type": "object",
  "required": ["jobs"],
  "additionalProperties": false,
  "properties": {
    "name": {
      "type": "string",
      "description": "A name for the whole pipeline, for `qex list --group` to use. The default is the name of the file."
    },
    "jobs": {
      "type": "array",
      "minItems": 1,
      "description": "The stages. qex reads the whole file before it submits anything, so a circle of stages or a name that no stage has gives an error and no job starts.",
      "items": {
        "type": "object",
        "required": ["name", "command"],
        "additionalProperties": false,
        "properties": {
          "name": {
            "type": "string",
            "description": "The name of this stage. The other stages use it in `needs` and `after`. Each name in one file must be different, and a name must not have the form of a job id."
          },
          "command": {
            "type": "array",
            "items": { "type": "string" },
            "minItems": 1,
            "description": "The program and its arguments. This is not a shell command line.",
            "examples": [["make", "test"]]
          },
          "needs": {
            "type": "array",
            "items": { "type": "string" },
            "description": "The stages of THIS file that must succeed before this stage. If one of them does not succeed, this stage does not run and its state becomes skipped.",
            "examples": [["build"]]
          },
          "after": {
            "type": "array",
            "items": { "type": "string" },
            "description": "The stages of THIS file that must stop before this stage, whatever their result. Use it for a cleanup stage.",
            "examples": [["ship"]]
          },
          "cwd": { "type": "string", "description": "The directory for this stage." },
          "timeout": { "type": "string", "description": "The time limit. Use s, m, h or d." },
          "tags": { "type": "array", "items": { "type": "string" } },
          "priority": { "type": "integer" },
          "env_capture": {
            "type": "string",
            "enum": ["all", "minimal", "none"]
          },
          "resources": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
              "cpu": {
                "anyOf": [
                  { "type": "integer", "minimum": 1 },
                  { "type": "string", "enum": ["half", "guess", "auto", "full", "max", "all"] }
                ]
              },
              "mem": { "type": "string" }
            }
          },
          "env": {
            "type": "object",
            "additionalProperties": { "type": "string" }
          }
        }
      }
    }
  }
}
"##;

pub const STATUS: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://github.com/qex/schema/status.json",
  "title": "qex job status",
  "description": "The output of `qex status --json`. The file status.json of each job has this format, with one difference: the file holds the job name as the user gave it, and the output holds the safe form. `qex list --json` gives an array of these objects.",
  "type": "object",
  "required": ["id", "name", "state", "submitted_at", "cpu", "mem", "usage", "tags"],
  "properties": {
    "id": { "type": "string", "format": "uuid", "description": "The job id." },
    "name": { "type": "string", "description": "The job name, in its SAFE form: letters, numbers, `-`, `_` and `.` only, never a first `-`, and 128 characters at most. qex replaces each other character with `_`, and a run of them with one `_`. Use this value in place of the id in each command; qex takes the name that the user gave as well." },
    "command": {
      "type": "array",
      "items": { "type": "string" },
      "description": "The program and its arguments, as qex ran them."
    },
    "cwd": { "type": "string", "description": "The directory of the job." },
    "state": {
      "type": "string",
      "enum": ["queued", "starting", "running", "completed", "failed", "killed", "timeout", "oom", "cancelled", "skipped"],
      "description": "The job state. The states queued, starting and running are not final. Each other state is final. The state skipped means that a job which this job needed did not succeed, so this job did not run."
    },
    "pid": {
      "type": ["integer", "null"],
      "description": "The process id of the job WHILE THE JOB OPERATES. The value is null before the job starts and null again after the job stops, because the machine gives that number to a different process soon after. A value that is not null thus means that the job operates now, and a reader can act on it. To stop a job, use `qex kill <id>`, which is correct at each moment."
    },
    "last_pid": {
      "type": ["integer", "null"],
      "description": "The process id that the job HAD. This value stays after the job stops, for a reader of a machine log. Never send a signal to it, and never look for it in the process list: the machine gives that number to another process later."
    },
    "exit_code": {
      "type": ["integer", "null"],
      "description": "The exit code, if the job stopped without a signal."
    },
    "signal": {
      "type": ["integer", "null"],
      "description": "The signal number, if a signal stopped the job."
    },
    "submitted_at": { "type": "integer", "description": "The submission time, in seconds after the Unix epoch." },
    "started_at": { "type": ["integer", "null"], "description": "The start time, in seconds after the Unix epoch." },
    "finished_at": { "type": ["integer", "null"], "description": "The stop time, in seconds after the Unix epoch." },
    "cpu": { "type": "integer", "description": "The number of cores that the job claimed." },
    "mem": { "type": "integer", "description": "The memory in bytes that the job claimed." },
    "usage": {
      "type": "object",
      "description": "The resources that the job used. Compare these values with the claim, then correct your next claim.",
      "properties": {
        "max_rss": { "type": "integer", "description": "The maximum memory in bytes." },
        "cpu_secs": { "type": "number", "description": "The CPU time in seconds for all the processes of the job." }
      }
    },
    "forced": {
      "type": "boolean",
      "description": "True if qex started the job although the claim is larger than the budget. qex starts such a job alone. The job can swap or stop with an out-of-memory error."
    },
    "forced_reason": {
      "type": ["string", "null"],
      "description": "The reason for a forced start."
    },
    "blocked_reason": {
      "type": ["string", "null"],
      "description": "The reason that the job waits in the queue. The value is null for a job that started."
    },
    "error": {
      "type": ["string", "null"],
      "description": "The reason that the job failed, when qex gives the reason. A command that does not exist is the usual cause."
    },
    "needs": {
      "type": "array",
      "items": { "type": "string", "format": "uuid" },
      "description": "The jobs that must succeed before this job starts."
    },
    "after": {
      "type": "array",
      "items": { "type": "string", "format": "uuid" },
      "description": "The jobs that must stop before this job starts. Their result is not important."
    },
    "caused_by": {
      "type": ["string", "null"],
      "format": "uuid",
      "description": "For a job in the state skipped, the first job that failed. This value is the root cause, and not the job before this one, so one read gives the true cause of a pipeline failure."
    },
    "logs_dropped": {
      "type": ["object", "null"],
      "description": "What qex removed from the output files, because the job wrote more than [logs] max_bytes. The value is null when qex kept everything. qex keeps the first part and the last part of each file, and it writes a line between them that says how much went. The removed lines are NOT on the disk, so no option of `qex logs` gives them back.",
      "properties": {
        "stdout_bytes": { "type": "integer", "description": "The bytes that qex did not keep from stdout.log." },
        "stdout_lines": { "type": "integer", "description": "The lines that qex did not keep from stdout.log." },
        "stderr_bytes": { "type": "integer", "description": "The bytes that qex did not keep from stderr.log." },
        "stderr_lines": { "type": "integer", "description": "The lines that qex did not keep from stderr.log." },
        "limit": { "type": "integer", "description": "The limit in bytes that operated, from [logs] max_bytes." }
      }
    },
    "sequence": {
      "type": "integer",
      "description": "The position of the job in the order of submission. Sort by submitted_at and then by this value to see a pipeline in order."
    },
    "tags": {
      "type": "array",
      "items": { "type": "string" },
      "description": "The tags of the job."
    }
  }
}
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// The pipeline schema must list every field that a stage accepts. A field
    /// that the schema does not name looks unsupported, and the parser refuses
    /// a field that the schema names but the code does not accept.
    #[test]
    fn the_pipeline_schema_lists_every_field_of_a_stage() {
        let parsed: serde_json::Value = serde_json::from_str(PIPELINE).unwrap();
        let props = parsed["properties"]["jobs"]["items"]["properties"]
            .as_object()
            .unwrap();
        for field in [
            "name",
            "command",
            "needs",
            "after",
            "cwd",
            "timeout",
            "tags",
            "priority",
            "env_capture",
            "resources",
            "env",
        ] {
            assert!(
                props.contains_key(field),
                "the schema has no field `{field}`"
            );
        }
        assert_eq!(
            props.len(),
            11,
            "the schema has a field that a stage does not accept"
        );
    }

    /// The example in the schema must parse as a pipeline file.
    #[test]
    fn the_pipeline_schema_example_parses() {
        let file: crate::pipeline::PipelineFile = serde_json::from_str(
            r#"{"jobs":[{"name":"build","command":["make"]},
                       {"name":"test","command":["make","test"],"needs":["build"]}]}"#,
        )
        .unwrap();
        file.validate().unwrap();
        assert_eq!(file.jobs.len(), 2);
    }

    #[test]
    fn each_schema_is_valid_json() {
        for name in NAMES {
            let text = schema(name).unwrap_or_else(|| panic!("no schema for `{name}`"));
            let parsed: serde_json::Value = serde_json::from_str(text)
                .unwrap_or_else(|e| panic!("the schema `{name}` is not valid JSON: {e}"));
            assert!(parsed.get("$schema").is_some(), "`{name}` has no $schema");
            assert!(parsed.get("title").is_some(), "`{name}` has no title");
        }
    }

    /// The status schema must list each state. A missing state gives an agent
    /// an incorrect list.
    #[test]
    fn the_status_schema_lists_every_state() {
        use crate::job::JobState;
        let parsed: serde_json::Value = serde_json::from_str(STATUS).unwrap();
        let listed = parsed["properties"]["state"]["enum"]
            .as_array()
            .expect("the state property has no enum");

        for state in [
            JobState::Queued,
            JobState::Starting,
            JobState::Running,
            JobState::Completed,
            JobState::Failed,
            JobState::Killed,
            JobState::Timeout,
            JobState::Oom,
            JobState::Cancelled,
        ] {
            assert!(
                listed.iter().any(|v| v == state.as_str()),
                "the schema does not list the state `{state}`"
            );
        }
        assert_eq!(
            listed.len(),
            10,
            "the schema lists a state that qex does not use"
        );
    }

    /// The job schema must list each field of the job file. A missing field
    /// gives an error, because the parser refuses an unknown field.
    #[test]
    fn the_job_schema_lists_every_field_of_the_job_file() {
        let parsed: serde_json::Value = serde_json::from_str(JOB).unwrap();
        let props = parsed["properties"].as_object().unwrap();
        for field in [
            "command",
            "name",
            "cwd",
            "timeout",
            "tags",
            "priority",
            "env_capture",
            "resources",
            "env",
            "needs",
            "after",
        ] {
            assert!(
                props.contains_key(field),
                "the schema has no field `{field}`"
            );
        }
        assert_eq!(
            props.len(),
            11,
            "the schema has a field that the job file does not accept"
        );
    }

    /// The example in the job schema must parse as a job file.
    #[test]
    fn the_job_schema_example_is_a_valid_job_file() {
        let parsed: serde_json::Value = serde_json::from_str(JOB).unwrap();
        let example = &parsed["properties"]["command"]["examples"][0];
        let doc = serde_json::json!({ "command": example });
        let job: crate::spec::JobFile = serde_json::from_value(doc).unwrap();
        assert_eq!(job.command[0], "uv");
    }
}
