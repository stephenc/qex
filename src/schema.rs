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
        "event" | "events" | "stream" => Some(EVENT),
        _ => None,
    }
}

pub const NAMES: &[&str] = &["job", "status", "pipeline", "event"];

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
      "description": "The name in `qex list`. Letters, numbers, `-`, `_` and `.` only. It must not start with `-`. At most 128 characters. The default is the program name."
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
    "max_queue_time": {
      "type": "string",
      "description": "The time that the job may wait in the queue before it starts. Use s, m, h or d. Use \"0\" for no limit. The default is no limit. A job that reaches this limit does not start, and its state becomes expired. `timeout` limits the run of the job; this field limits the wait.",
      "examples": ["30m", "2h", "0"]
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
    "nice": {
      "type": "integer",
      "description": "How politely this job uses the processor, from -20 to 19. A larger number gives way to everything else. The default comes from [politeness] nice in the configuration.",
      "examples": [10, 19]
    },
    "no_limit_env_hints": {
      "type": "boolean",
      "description": "Do not tell the job how large its claim is. qex writes the claim into the environment of the job, so a runtime sizes its thread pool to the claim. Give true for a job that must see the machine as it is. true from any source turns the claim off."
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
        },
        "gpu": {
          "type": "integer",
          "minimum": 1,
          "description": "The number of devices from the pool `gpu`. The devices come from [[pool]] in ~/.config/qex.toml. qex says WHICH index the job gets and writes it into the environment of the job. qex reads no driver, so this field operates on a machine with no CUDA library.",
          "examples": [1, 2]
        },
        "vram": {
          "type": "string",
          "description": "The quantity on EACH device that this job gets. qex does NOT add the memory of the devices together: a job that needs 40GB on one device cannot run on two devices of 24GB. With no value, the job takes the whole of each device that it gets.",
          "examples": ["20GB"]
        },
        "claims": {
          "type": "object",
          "description": "The claims on the other pools. The key is the pool name. Give a number, or a table with a count and a size for a quantity on each device. A name that the configuration does not declare is a lock of one unit.",
          "additionalProperties": {
            "anyOf": [
              { "type": "integer", "minimum": 1 },
              {
                "type": "object",
                "required": ["count"],
                "additionalProperties": false,
                "properties": {
                  "count": { "type": "integer", "minimum": 1 },
                  "size": { "type": "string" }
                }
              }
            ]
          },
          "examples": [{ "net": 1 }]
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
    },
    "locks": {
      "type": "array",
      "items": { "type": "string" },
      "description": "The locks that this job holds while it operates. Two jobs with one lock name never operate together. Use a lock for a build directory, a port or a database.",
      "examples": [["target"]]
    },
    "retries": {
      "type": "integer",
      "minimum": 0,
      "description": "The number of times to run this job again when it fails. One id, one record, every attempt in the log. A kill for memory starts no new attempt.",
      "examples": [3]
    },
    "dedupe_key": {
      "type": "string",
      "description": "Start no second job when a job with this key already exists. qex gives the id of that job and exits with the code 0. A key holds a job while that job waits or operates, and the key is free when the job stops. Use it in a script that can run a second time.",
      "examples": ["build:/home/me/project", "train:experiment-7"]
    },
    "dedupe_window": {
      "type": "string",
      "description": "Keep the key of a job that SUCCEEDED for this time. Use s, m, h or d. The default is \"0\", which frees the key when the job stops. A job that did not succeed never keeps its key. This field needs dedupe_key.",
      "examples": ["1h", "30m"]
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
          "locks": {
            "type": "array",
            "items": { "type": "string" },
            "description": "The locks that this stage holds while it operates. Two jobs with one lock name never operate together."
          },
          "retries": {
            "type": "integer",
            "minimum": 0,
            "description": "The number of times to run this stage again when it fails."
          },
          "cwd": { "type": "string", "description": "The directory for this stage." },
          "timeout": { "type": "string", "description": "The time limit. Use s, m, h or d." },
          "max_queue_time": { "type": "string", "description": "The time that this stage may wait in the queue before it starts. Use s, m, h or d. The wait for an earlier stage counts, so give a value that covers the whole pipeline." },
          "tags": { "type": "array", "items": { "type": "string" } },
          "priority": { "type": "integer" },
          "env_capture": {
            "type": "string",
            "enum": ["all", "minimal", "none"]
          },
          "nice": {
            "type": "integer",
            "description": "How politely this stage uses the processor, from -20 to 19. A larger number gives way to everything else."
          },
          "no_limit_env_hints": {
            "type": "boolean",
            "description": "Do not tell this stage how large its claim is. Give true for a stage that must see the machine as it is."
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
              "mem": { "type": "string" },
              "gpu": { "type": "integer", "minimum": 1 },
              "vram": { "type": "string" },
              "claims": {
                "type": "object",
                "additionalProperties": {
                  "anyOf": [
                    { "type": "integer", "minimum": 1 },
                    {
                      "type": "object",
                      "required": ["count"],
                      "additionalProperties": false,
                      "properties": {
                        "count": { "type": "integer", "minimum": 1 },
                        "size": { "type": "string" }
                      }
                    }
                  ]
                }
              }
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
      "description": "The program and its arguments, in the form that a person can read. A control byte is a C-style escape, so ESC is `\\x1b`. The record on the disk holds the bytes that the job received."
    },
    "cwd": { "type": "string", "description": "The directory of the job, in the same readable form as command." },
    "state": {
      "type": "string",
      "enum": ["queued", "starting", "running", "completed", "failed", "killed", "timeout", "expired", "oom", "cancelled", "skipped"],
      "description": "The job state. The states queued, starting and running are not final. Each other state is final. The state skipped means that a job which this job needed did not succeed, so this job did not run. The state expired means that the job waited more time than its max_queue_time value, so it never started and it has no output."
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
    "mem": { "type": "integer", "description": "The memory in bytes that the job claims." },
    "claim_source": {
      "type": "string",
      "enum": ["explicit", "learned", "default", "fan-out"],
      "description": "Where the claim came from. `learned` is a claim from the earlier jobs of THIS command. `fan-out` is a claim from the earlier jobs of the fan-out, which qex measures against the template and not against the command of one line."
    },
    "attempts": { "type": "integer", "description": "The number of times that qex started this job." },
    "retries_left": { "type": "integer", "description": "The number of times that qex may still start this job again after a failure. See --retries." },
    "locks": {
      "type": "array",
      "items": { "type": "string" },
      "description": "The locks that the job holds, in the same SAFE form as name. The record on the disk keeps the word that the user gave."
    },
    "claims": {
      "type": "object",
      "description": "The counted claims of the job. The key is the pool name. This is the REQUEST. Read `assigned` for what qex gave.",
      "additionalProperties": {
        "type": "object",
        "properties": {
          "count": { "type": "integer", "description": "The number of units, or of devices." },
          "size": { "type": ["integer", "null"], "description": "The quantity in bytes on EACH device. null means the whole of each device. qex never adds the capacity of the devices together." }
        }
      }
    },
    "assigned": {
      "type": "object",
      "description": "The units and the devices that qex GAVE this job. The key is the pool name. The coordinator writes this value when the job starts, and it stays after the job stops. The environment of the job holds the same indices, and the environment goes away with the job while this record stays.",
      "additionalProperties": {
        "type": "object",
        "properties": {
          "units": { "type": "integer", "description": "The number of units, or of devices." },
          "devices": {
            "type": "array",
            "items": { "type": "integer" },
            "description": "The device indices, for an indexed pool. The list is empty for a pool that has no devices."
          },
          "size": { "type": ["integer", "null"], "description": "The quantity in bytes on EACH device. null means the whole of each device." }
        }
      }
    },
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
    "queue_pause_secs": {
      "type": "integer",
      "description": "The seconds of the wait of this job that the queue was paused. --max-queue-time does not count them, so a pause does not expire a job."
    },
    "blocked_reason": {
      "type": ["string", "null"],
      "description": "The reason that the job waits in the queue. The value is null for a job that started. The text names the holder of the capacity: the jobs of this queue, another user, the machine, or the size of the job."
    },
    "blocked_since": {
      "type": ["integer", "null"],
      "description": "The time when this job first failed the capacity test at the front of the queue, in seconds after the Unix epoch. Read it to see how long the front of the queue has not moved."
    },
    "passed_by": {
      "type": "integer",
      "description": "The number of jobs that qex started after this job reached the front of the queue. At the value of `[queue] max_bypass`, qex keeps the capacity for this job and starts nothing else. The count is not reset when the cause of the wait changes, so a job that another user held becomes unpassable as soon as that user releases the capacity."
    },
    "error": {
      "type": ["string", "null"],
      "description": "The reason that the job failed, when qex gives the reason. A command that does not exist is the usual cause. qex also writes here for a job that it could not start, a job that reached the limit of its wait in the queue, a job that it skipped because a job that it needed did not succeed, and an attempt that failed while qex has an attempt to give. This field gives the REASON; read `state` for what became of the job."
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
        "limit": { "type": "integer", "description": "The limit in bytes that operated, from [logs] max_bytes." },
        "incomplete": { "type": "boolean", "description": "True when qex could not complete a log file, and could not count what went. A process of the job held the output open after the job stopped. The counts above are then the counts that arrived, and not the full quantity." }
      }
    },
    "dedupe_key": {
      "type": ["string", "null"],
      "description": "The key that made the submission of this job idempotent, if the submission gave one. A second submission with this key gave this id and started no job."
    },
    "sequence": {
      "type": "integer",
      "description": "The position of the job in the order of submission. Sort by submitted_at and then by this value to see a pipeline in order."
    },
    "tags": {
      "type": "array",
      "items": { "type": "string" },
      "description": "The tags of the job, in the same SAFE form as name. The record on the disk keeps the word that the user gave."
    }
  }
}
"##;

/// The schema of one line of `qex events`.
///
/// The stream is the interface for a program, so it needs a schema as much as
/// the status does. A reader that writes a parser from an example writes a
/// parser for the fields that its example held.
pub const EVENT: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://github.com/qex/schema/event.json",
  "title": "qex event line",
  "description": "One line of `qex events --json`. The command writes one of these objects on each line, at the moment of the change. Read the field `event` first. IGNORE A VALUE OF `event` THAT YOU DO NOT KNOW: a later version of qex can add one, and a reader that stops at an unknown line loses the lines after it.",
  "type": "object",
  "required": ["event", "time"],
  "properties": {
    "event": {
      "type": "string",
      "enum": ["stream", "job", "gap", "bye"],
      "description": "The type of the line. `stream` is the first line and names the coordinator. `job` is a change of the record of one job. `gap` counts the events that you lost. `bye` says that the coordinator stops now."
    },
    "time": { "type": "integer", "description": "The time of the line, in seconds after the Unix epoch." },
    "seq": {
      "type": "integer",
      "description": "The number of this event, on a `job` line. KEEP THE LARGEST NUMBER THAT YOU READ, WITH THE `stream_id` OF THE FIRST LINE. Give both to `qex events --since <stream_id>:<seq>` when your program starts again, and you lose nothing. The numbers belong to one stream: a new coordinator starts them at 1 again, so a number alone names a different event there and qex cannot see the difference."
    },
    "stream_id": {
      "type": "string",
      "format": "uuid",
      "description": "On a `stream` line: the name of this stream. Keep it with the largest `seq` that you read, and give both to `--since`. qex then compares them, and it gives a `gap` line when the coordinator changed. Two coordinators can start in one second, so the start time does not separate them; this name does."
    },
    "version": { "type": "string", "description": "On a `stream` line: the version of the coordinator." },
    "pid": { "type": "integer", "description": "On a `stream` line: the process id of the coordinator." },
    "coordinator_started_at": {
      "type": "integer",
      "description": "On a `stream` line: the time when this coordinator started. Use `stream_id` to compare two streams: two coordinators can start in one second."
    },
    "first_seq": { "type": "integer", "description": "On a `stream` line: the oldest event that the coordinator still holds." },
    "last_seq": { "type": "integer", "description": "On a `stream` line: the newest event that the coordinator holds." },
    "id": { "type": "string", "format": "uuid", "description": "On a `job` line: the id of the job." },
    "name": { "type": "string", "description": "On a `job` line: the name of the job." },
    "state": {
      "type": "string",
      "enum": ["queued", "starting", "running", "completed", "failed", "killed", "timeout", "expired", "oom", "cancelled", "skipped"],
      "description": "On a `job` line: the state of the job now. Run `qex help states` for each state. The stream gives one line for each state THAT THE COORDINATOR SAW: it reads the record of a job twice each second, so a job that is shorter than that period goes from `starting` to `completed` with no `running` line. The field `previous` then says `starting`, so the sequence that you read is the true sequence."
    },
    "previous": {
      "type": ["string", "null"],
      "description": "On a `job` line: the state before this event. The value is null for the first line of a job, which is the line that says that qex accepted it."
    },
    "change": {
      "type": "string",
      "enum": ["state", "reason"],
      "description": "On a `job` line: `state` when the job moved to a different state, and `reason` when the job stays in the queue and the reason that it waits changed. Read `job.blocked_reason` for that reason."
    },
    "job": {
      "type": "object",
      "description": "On a `job` line: the whole record of the job, in the form that `qex status --json` gives. See `qex schema status`. You thus need no second command to learn the exit code, the measured use or the cause of a failure."
    },
    "missed": {
      "type": ["integer", "null"],
      "description": "On a `gap` line: the number of events that you did not receive, or null when qex cannot count them. The value is null when your number comes from a different stream, because the two streams have no common measure; the field `reason` then says what happened. The coordinator keeps the last events only, and it never waits for a reader. Read the stream in a loop, and do the work of an event in a different thread or process."
    },
    "next_seq": { "type": "integer", "description": "On a `gap` line: the number of the next event that you receive." },
    "reason": { "type": "string", "description": "On a `gap` or `bye` line: the reason, in words for a person to read." }
  }
}
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The names of the top-level fields that `T` serializes.
    ///
    /// `JobFile` and `Stage` serialize every field, so a default value is the
    /// list that the compiler owns. A list typed into this file is the fault
    /// that this test exists to prevent: the schema and the struct go apart,
    /// and the test agrees with the wrong answer.
    ///
    /// Do not add `skip_serializing_if` to those types without changing this
    /// helper. A default value would then hide the field, and the test would
    /// go silent.
    fn serialized_field_names<T: serde::Serialize>(value: T) -> BTreeSet<String> {
        serde_json::to_value(value)
            .expect("the value must serialize")
            .as_object()
            .expect("the value must be an object")
            .keys()
            .cloned()
            .collect()
    }

    fn schema_property_names(schema: &str, path: &[&str]) -> BTreeSet<String> {
        let mut value: serde_json::Value =
            serde_json::from_str(schema).expect("the schema must be valid JSON");
        for key in path {
            value = value[key].take();
        }
        value
            .as_object()
            .unwrap_or_else(|| panic!("no object at {path:?}"))
            .keys()
            .cloned()
            .collect()
    }

    /// The pipeline schema must list every field that a stage accepts. A field
    /// that the schema does not name looks unsupported, and the parser refuses
    /// a field that the schema names but the code does not accept.
    ///
    /// The names come from `Stage`, and not from a list written here. A list
    /// written here would pass while a field that a stage gained reached
    /// neither the schema nor the test — which is the whole fault, one step
    /// further back.
    #[test]
    fn the_pipeline_schema_lists_every_field_of_a_stage() {
        assert_eq!(
            serialized_field_names(crate::pipeline::Stage::default()),
            schema_property_names(PIPELINE, &["properties", "jobs", "items", "properties"])
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

    /// The two schemas must list EVERY state that qex has, and no other.
    ///
    /// A missing state gives an agent an incorrect list, and a reader that
    /// validates the stream refuses a line that is correct. The event schema had
    /// lost `expired` in that way.
    ///
    /// The list comes from `JobState::ALL`, and not from a list written here. A
    /// list written here would pass while a state that qex added reached
    /// NEITHER schema — which is the whole fault, one step further back.
    #[test]
    fn each_schema_lists_every_state_that_qex_has() {
        use crate::job::JobState;
        let want: Vec<&str> = JobState::ALL.iter().map(|s| s.as_str()).collect();

        for (name, text) in [("status", STATUS), ("event", EVENT)] {
            let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
            let listed: Vec<&str> = parsed["properties"]["state"]["enum"]
                .as_array()
                .unwrap_or_else(|| panic!("the `{name}` schema has no enum of states"))
                .iter()
                .map(|v| v.as_str().expect("a state must be a string"))
                .collect();
            for state in &want {
                assert!(
                    listed.contains(state),
                    "the `{name}` schema does not list the state `{state}`"
                );
            }
            assert_eq!(
                listed.len(),
                want.len(),
                "the `{name}` schema lists a state that qex does not have: {listed:?}"
            );
        }
    }

    /// The job schema must list each field of the job file. A missing field
    /// looks unsupported to an agent that reads `qex schema`, and a field that
    /// the schema names but the parser refuses is an error.
    ///
    /// The names come from `JobFile`, and not from a list written here. See
    /// `the_pipeline_schema_lists_every_field_of_a_stage`.
    #[test]
    fn the_job_schema_lists_every_field_of_the_job_file() {
        assert_eq!(
            serialized_field_names(crate::spec::JobFile::default()),
            schema_property_names(JOB, &["properties"])
        );
    }

    /// The event schema must name each type of line. A type that the schema
    /// does not name looks like a fault to a reader that meets it.
    #[test]
    fn the_event_schema_names_each_type_of_line() {
        let parsed: serde_json::Value = serde_json::from_str(EVENT).unwrap();
        let types = parsed["properties"]["event"]["enum"].as_array().unwrap();
        for name in ["stream", "job", "gap", "bye"] {
            assert!(
                types.iter().any(|t| t == name),
                "the schema does not name the line `{name}`"
            );
        }
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
