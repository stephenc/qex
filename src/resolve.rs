//! This module reads the text that a person wrote in place of a job id.
//!
//! The text can be a full id, the start of an id, the name of a job, the id of
//! a pipeline or the name of a pipeline. ONE set of rules decides what the text
//! names, and two callers use it:
//!
//!   * The coordinator, for the `Resolve`, `CancelMany` and `KillMany`
//!     requests. It finds the candidates in its [`Index`], so the cost of one
//!     text does not grow with the number of jobs.
//!   * The CLI, when the coordinator is an earlier version that has none of
//!     those requests. It then finds the candidates in the whole job list.
//!
//! The two must give the same answer for the same jobs, and they do, because
//! both call [`targets`].

use crate::job::{safe_name, JobState};
use crate::proto::NamedJob;
use crate::units::count_of;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use uuid::Uuid;

/// What the rules read about one job.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: Uuid,
    pub name: String,
    pub group: Option<Uuid>,
    pub group_name: Option<String>,
    pub state: JobState,
    pub submitted_at: u64,
    pub sequence: u64,
}

impl Candidate {
    pub fn of(s: &crate::job::JobStatus) -> Self {
        Self {
            id: s.id,
            name: s.name.clone(),
            group: s.group,
            group_name: s.group_name.clone(),
            state: s.state,
            submitted_at: s.submitted_at,
            sequence: s.sequence,
        }
    }

    fn named(&self) -> NamedJob {
        NamedJob {
            id: self.id,
            name: self.name.clone(),
            state: self.state,
        }
    }
}

/// Tells whether the text names a job with this id and this name.
///
/// The user can write the full id, or the start of the id, or the name. A short
/// id is easier to copy from the output of `qex list`.
///
/// The name has two accepted forms: the name that the user gave, AND the safe
/// name that qex shows. `qex list` and `qex status --json` give the safe form,
/// so a script that reads a name from qex and gives it back must find the job.
/// See `job::safe_name`.
pub fn names_job(id: Uuid, name: &str, raw: &str) -> bool {
    id.to_string().starts_with(raw) || name == raw || safe_name(name) == raw
}

/// Tells whether the text names a group with this id and this name.
///
/// A group takes its id, the start of its id, or its name, in the same way as a
/// job. `qex list --group` already accepts these three forms.
pub fn names_group(group: Option<Uuid>, group_name: Option<&str>, raw: &str) -> bool {
    group
        .map(|g| g.to_string().starts_with(raw))
        .unwrap_or(false)
        // The name that the user gave, AND the name that qex shows. `qex list
        // --json` gives the safe form, so a script that reads that value and
        // gives it back here must find the jobs. See `job::safe_name`.
        || group_name == Some(raw)
        || group_name.map(safe_name).as_deref() == Some(raw)
}

/// What the text of the user named.
///
/// The caller needs more than the ids. A command that stops a job treats a stage
/// that already stopped as a fault when the user named that one job, and as
/// normal when the user named the whole pipeline. `qex status --json` also
/// chooses its shape from this, and not from the number of jobs: a pipeline of
/// one stage must still give an array.
#[derive(Debug)]
pub struct Targets {
    pub ids: Vec<Uuid>,
    /// The pipeline, when the text named one.
    pub group: Option<Uuid>,
    /// The same jobs as `ids`, with the name and the state of each.
    pub jobs: Vec<NamedJob>,
}

impl Targets {
    fn of(mut jobs: Vec<&Candidate>, group: Option<Uuid>) -> Self {
        // The order of submission. A pipeline then reads from the first stage
        // to the last stage. Two stages can start in the same second, so the
        // sequence separates them.
        jobs.sort_by_key(|j| (j.submitted_at, j.sequence));
        Self {
            ids: jobs.iter().map(|j| j.id).collect(),
            group,
            jobs: jobs.iter().map(|j| j.named()).collect(),
        }
    }

    /// Makes the value again from the answer of a coordinator.
    pub fn from_answer(jobs: Vec<NamedJob>, group: Option<Uuid>) -> Self {
        Self {
            ids: jobs.iter().map(|j| j.id).collect(),
            group,
            jobs,
        }
    }
}

/// Makes the result for a text that named a pipeline.
///
/// The jobs must belong to ONE pipeline. A pipeline takes its name from its
/// file, so a second run of the same file carries the same name, and `qex
/// pipeline ci.toml` twice gives two pipelines that the word `ci` both names.
/// Without this test `qex kill ci` stopped the work of two runs, and the user
/// named one. A short group id has the same fault, because two ids can start
/// with the same characters.
fn group_targets(by_group: &[&Candidate], raw: &str) -> Result<Targets> {
    let mut groups: Vec<Uuid> = by_group.iter().filter_map(|j| j.group).collect();
    groups.sort();
    groups.dedup();

    if groups.len() > 1 {
        let lines: Vec<String> = groups
            .iter()
            .map(|g| {
                let count = by_group.iter().filter(|j| j.group == Some(*g)).count();
                format!("  {g}  {}", count_of(count, "stage"))
            })
            .collect();
        bail!(
            "`{raw}` names {} pipelines. A pipeline takes its name from its file, so a \
             second run of that file has the same name. Give the group id of the run \
             that you want:\n{}",
            groups.len(),
            lines.join("\n")
        );
    }

    Ok(Targets::of(by_group.to_vec(), groups.first().copied()))
}

/// Reads one or more job ids from the text that the user wrote.
///
/// A value that names a job gives that job. A value that names a pipeline gives
/// EVERY job of that pipeline, in the order of submission.
///
/// `qex pipeline` writes the group id to stdout, so that value is the handle
/// that a user keeps. Before this function, every command except `qex list
/// --group` refused it and gave "there is no job with the id ...", and the user
/// had to find the last stage by hand.
///
/// `jobs` can hold every job, or only the jobs that an [`Index`] gave for this
/// text. The rules test each job again, so a list that holds too much gives
/// the same answer as a list that holds exactly the matches.
pub fn targets(jobs: &[Candidate], raw: &str) -> Result<Targets> {
    if raw.is_empty() {
        bail!("give the id or the name of a job or a pipeline.");
    }

    let by_job: Vec<&Candidate> = jobs
        .iter()
        .filter(|j| names_job(j.id, &j.name, raw))
        .collect();
    let by_group: Vec<&Candidate> = jobs
        .iter()
        .filter(|j| names_group(j.group, j.group_name.as_deref(), raw))
        .collect();

    // Test each name in the same way, including a full id.
    //
    // An earlier version gave back each value with the form of a UUID without
    // a test. A `--needs` value with one incorrect character was then accepted,
    // the dependency did not exist, and the job started immediately with no
    // warning. `qex logs` with such a value also wrote nothing and gave the
    // code 0, so a reader could not separate "this job wrote nothing" from
    // "this job does not exist".
    if let Ok(id) = raw.parse::<Uuid>() {
        if let Some(job) = jobs.iter().find(|j| j.id == id) {
            return Ok(Targets::of(vec![job], None));
        }
        if !by_group.is_empty() {
            return group_targets(&by_group, raw);
        }
        // Say whether qex ever saw this id. An agent must be able to tell "the
        // record was deleted, and the work happened" from "this job never
        // existed, so submit it".
        bail!("{}", crate::history::describe_missing(id));
    }

    // The two sets can hold the same one job, because a short id can be the
    // start of the id of the job AND of the id of its group. That is not an
    // ambiguity: both readings give the same job.
    let same = !by_job.is_empty()
        && by_job.len() == by_group.len()
        && by_job.iter().all(|j| by_group.iter().any(|g| g.id == j.id));

    if !by_job.is_empty() && !by_group.is_empty() && !same {
        let mut groups: Vec<Uuid> = by_group.iter().filter_map(|j| j.group).collect();
        groups.sort();
        groups.dedup();
        bail!(
            "`{raw}` is the name of a job and the name of a pipeline. Give the \
             full id of the one that you want.\n  job:      {}\n  pipeline: {}",
            by_job
                .iter()
                .map(|j| j.id.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            groups
                .iter()
                .map(|g| g.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    if !by_group.is_empty() && by_job.is_empty() {
        return group_targets(&by_group, raw);
    }

    match by_job.len() {
        1 => Ok(Targets::of(vec![by_job[0]], None)),
        0 => bail!("there is no job or pipeline with the id or the name `{raw}`"),
        n => bail!(
            "`{raw}` names {n} jobs. Give the id of the job that you want, or delete \
             the old jobs with `qex clean done` and start again.\n{}",
            by_job
                .iter()
                // qex SHOWS the safe name only. See `job::safe_name`.
                .map(|j| format!("  {} {}", j.id, safe_name(&j.name)))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

/// What a filter reads about one job.
///
/// The coordinator holds a full record for a job that waits or operates, and a
/// short entry for a job that stopped. Both give this view, so ONE function
/// decides what a filter keeps.
pub struct Facts<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub state: JobState,
    pub tags: &'a [String],
    pub cwd: &'a str,
    pub group: Option<Uuid>,
    pub group_name: Option<&'a str>,
    pub finished_at: Option<u64>,
}

impl<'a> Facts<'a> {
    pub fn of(s: &'a crate::job::JobStatus) -> Self {
        Self {
            id: s.id,
            name: &s.name,
            state: s.state,
            tags: &s.tags,
            cwd: &s.cwd,
            group: s.group,
            group_name: s.group_name.as_deref(),
            finished_at: s.finished_at,
        }
    }
}

/// What a filter says about one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Yes,
    /// Every part of the filter keeps the job, except its age: it stopped
    /// before the window of `stopped_within`. The answer counts these jobs, so
    /// a reader learns that they exist.
    Older,
    No,
}

/// Tests a directory against `--cwd` and `--under`.
pub fn matches_directory(
    job_cwd: &str,
    exact: Option<&std::path::Path>,
    under: Option<&std::path::Path>,
) -> bool {
    if let Some(dir) = exact {
        if std::path::Path::new(job_cwd) != dir {
            return false;
        }
    }
    if let Some(dir) = under {
        let path = std::path::Path::new(job_cwd);
        // `starts_with` compares whole parts, so `/a/b` does not match `/a/bc`.
        if !path.starts_with(dir) {
            return false;
        }
    }
    true
}

/// Reads the state word of a filter. A word that this build cannot read is a
/// refusal, and never "every state".
pub fn state_filter(
    filter: &crate::proto::JobFilter,
) -> std::result::Result<Option<crate::cli::StateFilter>, String> {
    match filter.state.as_deref() {
        Some(word) => crate::cli::StateFilter::parse(word).map(Some),
        None => Ok(None),
    }
}

/// Tests one job against a filter. `state` comes from [`state_filter`].
pub fn keeps(
    filter: &crate::proto::JobFilter,
    state: Option<&crate::cli::StateFilter>,
    job: &Facts<'_>,
    now: u64,
) -> Keep {
    if let Some(f) = state {
        if !f.matches(job.state) {
            return Keep::No;
        }
    }
    if let Some(tag) = &filter.tag {
        if !job.tags.iter().any(|t| t == tag || safe_name(t) == *tag) {
            return Keep::No;
        }
    }
    if !matches_directory(
        job.cwd,
        filter.cwd.as_deref().map(std::path::Path::new),
        filter.under.as_deref().map(std::path::Path::new),
    ) {
        return Keep::No;
    }
    if let Some(group) = &filter.group {
        if !names_group(job.group, job.group_name, group) {
            return Keep::No;
        }
    }
    if let Some(name) = &filter.name {
        if !names_job(job.id, job.name, name) {
            return Keep::No;
        }
    }
    if let Some(window) = filter.stopped_within {
        if job.state.is_terminal() {
            // A record with no end time is a record of an earlier version.
            // Its age is not known, so the window does not hide it.
            if let Some(end) = job.finished_at {
                if now.saturating_sub(end) > window {
                    return Keep::Older;
                }
            }
        }
    }
    Keep::Yes
}

/// Applies the limit of a filter to jobs that are in the order of submission.
///
/// The NEWEST jobs stay, because a reader who limits a list looks for the work
/// of now. Gives the number of jobs that the limit left out.
pub fn apply_limit<T>(jobs: &mut Vec<T>, limit: Option<usize>) -> usize {
    match limit {
        Some(limit) if jobs.len() > limit => {
            let over = jobs.len() - limit;
            jobs.drain(..over);
            over
        }
        _ => 0,
    }
}

/// Gives the first id and the last id that a text can be the start of.
///
/// `None` says that the text is not the start of any id: it holds a character
/// that an id does not have, or it is too long.
fn id_range(raw: &str) -> Option<(Uuid, Uuid)> {
    let hex: String = raw.chars().filter(|c| *c != '-').collect();
    if hex.is_empty() || hex.len() > 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let pad = |with: char| {
        let mut text = hex.clone();
        while text.len() < 32 {
            text.push(with);
        }
        Uuid::parse_str(&text).ok()
    };
    Some((pad('0')?, pad('f')?))
}

/// What the index keeps about one job, so that it can take the job out again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keys {
    pub name: String,
    pub group: Option<Uuid>,
    pub group_name: Option<String>,
}

/// Finds the jobs that a text can name, without a look at every job.
///
/// The coordinator holds one of these beside its jobs. `lookup` gives a set
/// that holds EVERY job that the text names, and it can hold more: the rules of
/// [`targets`] test each one again, so this structure decides the cost of a
/// request and never its answer.
///
/// The name of a job and the group of a job never change after the submission,
/// so an entry changes only when a job arrives and when `qex clean` deletes a
/// record.
#[derive(Debug, Default)]
pub struct Index {
    ids: BTreeMap<Uuid, Keys>,
    /// The name as the user gave it, and its safe form. Both name the job.
    names: HashMap<String, BTreeSet<Uuid>>,
    groups: BTreeMap<Uuid, BTreeSet<Uuid>>,
    group_names: HashMap<String, BTreeSet<Uuid>>,
}

impl Index {
    pub fn insert(&mut self, id: Uuid, keys: Keys) {
        if self.ids.contains_key(&id) {
            self.remove(id);
        }
        for text in [keys.name.clone(), safe_name(&keys.name)] {
            self.names.entry(text).or_default().insert(id);
        }
        if let Some(group) = keys.group {
            self.groups.entry(group).or_default().insert(id);
        }
        if let Some(name) = &keys.group_name {
            for text in [name.clone(), safe_name(name)] {
                self.group_names.entry(text).or_default().insert(id);
            }
        }
        self.ids.insert(id, keys);
    }

    pub fn remove(&mut self, id: Uuid) {
        let Some(keys) = self.ids.remove(&id) else {
            return;
        };
        let take = |map: &mut HashMap<String, BTreeSet<Uuid>>, text: String| {
            if let Some(set) = map.get_mut(&text) {
                set.remove(&id);
                if set.is_empty() {
                    map.remove(&text);
                }
            }
        };
        take(&mut self.names, safe_name(&keys.name));
        take(&mut self.names, keys.name);
        if let Some(group) = keys.group {
            if let Some(set) = self.groups.get_mut(&group) {
                set.remove(&id);
                if set.is_empty() {
                    self.groups.remove(&group);
                }
            }
        }
        if let Some(name) = keys.group_name {
            take(&mut self.group_names, safe_name(&name));
            take(&mut self.group_names, name);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Gives the jobs that a text names when the text is a full id in ANY form
    /// that the parser of an id reads.
    ///
    /// [`targets`] reads a full id with that parser, and the parser also takes
    /// the braced form `{...}` and the form `urn:uuid:...`. [`id_range`] refuses
    /// those characters. Without this step the index gave nothing for such a
    /// text, and the rules then said that the record of a job in the queue was
    /// gone. A job that is not a member of a named group is harmless in the
    /// answer, because the rules test each job again.
    fn lookup_full_id(&self, raw: &str) -> BTreeSet<Uuid> {
        let mut found = BTreeSet::new();
        if let Ok(id) = raw.parse::<Uuid>() {
            if self.ids.contains_key(&id) {
                found.insert(id);
            }
            if let Some(jobs) = self.groups.get(&id) {
                found.extend(jobs.iter().copied());
            }
        }
        found
    }

    /// Gives every job that the text can name, as a job or through its group.
    pub fn lookup(&self, raw: &str) -> BTreeSet<Uuid> {
        let mut found = self.lookup_full_id(raw);
        if let Some((first, last)) = id_range(raw) {
            found.extend(self.ids.range(first..=last).map(|(id, _)| *id));
            for (_, jobs) in self.groups.range(first..=last) {
                found.extend(jobs.iter().copied());
            }
        }
        if let Some(jobs) = self.names.get(raw) {
            found.extend(jobs.iter().copied());
        }
        if let Some(jobs) = self.group_names.get(raw) {
            found.extend(jobs.iter().copied());
        }
        found
    }

    /// Gives every job of the groups that the text can name.
    pub fn lookup_group(&self, raw: &str) -> BTreeSet<Uuid> {
        let mut found = BTreeSet::new();
        if let Some((first, last)) = id_range(raw) {
            for (_, jobs) in self.groups.range(first..=last) {
                found.extend(jobs.iter().copied());
            }
        }
        if let Some(jobs) = self.group_names.get(raw) {
            found.extend(jobs.iter().copied());
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(name: &str, group: Option<Uuid>, group_name: Option<&str>) -> Keys {
        Keys {
            name: name.to_string(),
            group,
            group_name: group_name.map(str::to_string),
        }
    }

    fn candidate(id: Uuid, name: &str) -> Candidate {
        Candidate {
            id,
            name: name.to_string(),
            group: None,
            group_name: None,
            state: JobState::Queued,
            submitted_at: 0,
            sequence: 0,
        }
    }

    /// The index can give too much and must never give too little: a job that
    /// the rules would name and that the index leaves out is a job that a
    /// command cannot find.
    #[test]
    fn the_index_gives_every_job_that_a_text_names() {
        let mut index = Index::default();
        let group: Uuid = "12345678-0000-4000-8000-000000000001".parse().unwrap();
        let a: Uuid = "aabbccdd-0000-4000-8000-000000000001".parse().unwrap();
        let b: Uuid = "aabbccdd-0000-4000-8000-000000000002".parse().unwrap();
        let c: Uuid = "ffbbccdd-0000-4000-8000-000000000003".parse().unwrap();
        index.insert(a, keys("build", Some(group), Some("ci")));
        index.insert(b, keys("te st", Some(group), Some("ci")));
        index.insert(c, keys("build", None, None));

        let set = |ids: &[Uuid]| ids.iter().copied().collect::<BTreeSet<_>>();
        // The start of an id, with and without the hyphen of the full form.
        assert_eq!(index.lookup("aabbccdd"), set(&[a, b]));
        assert_eq!(index.lookup("aabbccdd-0000"), set(&[a, b]));
        assert_eq!(index.lookup(&a.to_string()), set(&[a]));
        // A full id in a form that `Uuid` reads and `to_string` does not give.
        assert_eq!(
            index.lookup(&a.simple().to_string().to_uppercase()),
            set(&[a])
        );
        // The parser of an id also reads the braced form and the urn form, and
        // the rules take such a text as a full id. The index gave nothing for
        // them, and `qex status "{id}"` then said that the record of a job in
        // the queue was gone.
        for text in [
            a.braced().to_string(),
            a.urn().to_string(),
            a.braced().to_string().to_uppercase(),
        ] {
            assert_eq!(index.lookup(&text), set(&[a]), "{text}");
            let jobs = vec![candidate(a, "build"), candidate(c, "build")];
            let found = targets(&jobs, &text).unwrap();
            assert_eq!(found.ids, vec![a], "{text}");
        }
        // A name, in the form that the user gave and in the safe form.
        assert_eq!(index.lookup("build"), set(&[a, c]));
        assert_eq!(index.lookup("te st"), set(&[b]));
        assert_eq!(index.lookup(&safe_name("te st")), set(&[b]));
        // A group, by the start of its id and by its name.
        assert_eq!(index.lookup("12345678"), set(&[a, b]));
        assert_eq!(index.lookup("ci"), set(&[a, b]));
        assert_eq!(index.lookup_group("ci"), set(&[a, b]));
        assert!(index.lookup("nothing").is_empty());
        assert!(index.lookup("").is_empty());
    }

    /// A record that `qex clean` deleted must leave no entry. An entry that
    /// stays names a job that no command can read.
    #[test]
    fn a_removed_job_leaves_nothing_in_the_index() {
        let mut index = Index::default();
        let group = Uuid::new_v4();
        let id = Uuid::new_v4();
        index.insert(id, keys("a b", Some(group), Some("g h")));
        index.remove(id);
        assert_eq!(index.len(), 0);
        assert!(index.names.is_empty());
        assert!(index.groups.is_empty());
        assert!(index.group_names.is_empty());
        assert!(index.lookup(&id.to_string()).is_empty());
    }

    /// The filter of a list: each part narrows, and the age gives `Older` only
    /// for a job that every other part keeps.
    #[test]
    fn a_filter_keeps_what_each_part_keeps() {
        let tags = vec!["sw eep".to_string()];
        let job = |state, finished_at| Facts {
            id: Uuid::nil(),
            name: "build",
            state,
            tags: &tags,
            cwd: "/work/a",
            group: None,
            group_name: None,
            finished_at,
        };
        let filter = |f: crate::proto::JobFilter| f;
        let now = 10_000;
        let test = |f: &crate::proto::JobFilter, j: &Facts<'_>| {
            keeps(f, state_filter(f).unwrap().as_ref(), j, now)
        };

        let window = filter(crate::proto::JobFilter {
            stopped_within: Some(3600),
            ..Default::default()
        });
        assert_eq!(test(&window, &job(JobState::Queued, None)), Keep::Yes);
        assert_eq!(
            test(&window, &job(JobState::Completed, Some(9_000))),
            Keep::Yes
        );
        assert_eq!(
            test(&window, &job(JobState::Completed, Some(1_000))),
            Keep::Older
        );
        // A record with no end time has no known age, so the window keeps it.
        assert_eq!(test(&window, &job(JobState::Completed, None)), Keep::Yes);

        // A job that a different part refuses is not `Older`: the count of the
        // old jobs must hold only the jobs that `--all` would add.
        let tagged = filter(crate::proto::JobFilter {
            tag: Some("other".into()),
            stopped_within: Some(3600),
            ..Default::default()
        });
        assert_eq!(
            test(&tagged, &job(JobState::Completed, Some(1_000))),
            Keep::No
        );

        for (f, want) in [
            (
                crate::proto::JobFilter {
                    tag: Some("sw eep".into()),
                    ..Default::default()
                },
                Keep::Yes,
            ),
            (
                crate::proto::JobFilter {
                    tag: Some(safe_name("sw eep")),
                    ..Default::default()
                },
                Keep::Yes,
            ),
            (
                crate::proto::JobFilter {
                    state: Some("done".into()),
                    ..Default::default()
                },
                Keep::No,
            ),
            (
                crate::proto::JobFilter {
                    cwd: Some("/work".into()),
                    ..Default::default()
                },
                Keep::No,
            ),
            (
                crate::proto::JobFilter {
                    under: Some("/work".into()),
                    ..Default::default()
                },
                Keep::Yes,
            ),
            (
                crate::proto::JobFilter {
                    under: Some("/wor".into()),
                    ..Default::default()
                },
                Keep::No,
            ),
            (
                crate::proto::JobFilter {
                    name: Some("build".into()),
                    ..Default::default()
                },
                Keep::Yes,
            ),
            (
                crate::proto::JobFilter {
                    name: Some("bui".into()),
                    ..Default::default()
                },
                Keep::No,
            ),
            (
                crate::proto::JobFilter {
                    group: Some("ci".into()),
                    ..Default::default()
                },
                Keep::No,
            ),
        ] {
            assert_eq!(test(&f, &job(JobState::Queued, None)), want, "{f:?}");
        }

        let unknown = crate::proto::JobFilter {
            state: Some("nonsense".into()),
            ..Default::default()
        };
        assert!(
            state_filter(&unknown).is_err(),
            "an unknown word is never every state"
        );

        let mut jobs = vec![1, 2, 3, 4];
        assert_eq!(apply_limit(&mut jobs, Some(3)), 1);
        assert_eq!(jobs, vec![2, 3, 4], "the NEWEST jobs stay");
        assert_eq!(apply_limit(&mut jobs, None), 0);
    }

    /// The rules give the same answer for the whole list and for the jobs that
    /// the index gave. This is the property that lets the coordinator answer
    /// from the index.
    #[test]
    fn the_rules_agree_on_the_whole_list_and_on_the_index() {
        let group = Uuid::new_v4();
        let mut all = Vec::new();
        let mut index = Index::default();
        for (n, (name, in_group)) in [
            ("build", true),
            ("test", true),
            ("build", false),
            ("x", false),
        ]
        .iter()
        .enumerate()
        {
            let c = Candidate {
                id: Uuid::new_v4(),
                name: name.to_string(),
                group: in_group.then_some(group),
                group_name: in_group.then(|| "ci".to_string()),
                state: JobState::Queued,
                submitted_at: 1,
                sequence: n as u64,
            };
            index.insert(c.id, keys(&c.name, c.group, c.group_name.as_deref()));
            all.push(c);
        }
        let short = all[3].id.to_string()[..8].to_string();
        for raw in [
            "build",
            "test",
            "ci",
            "x",
            "nothing",
            &group.to_string(),
            &short,
        ] {
            let ids = index.lookup(raw);
            let few: Vec<Candidate> = all
                .iter()
                .filter(|c| ids.contains(&c.id))
                .cloned()
                .collect();
            let whole = targets(&all, raw)
                .map(|t| (t.ids, t.group))
                .map_err(|e| e.to_string());
            let part = targets(&few, raw)
                .map(|t| (t.ids, t.group))
                .map_err(|e| e.to_string());
            assert_eq!(whole, part, "the two answers differ for `{raw}`");
        }
    }
}
