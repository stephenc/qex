//! The rule that decides which records a deletion must keep.
//!
//! `qex clean` and `qex gc` select the records to delete, and the coordinator
//! decides each deletion. Both must give ONE answer. Two places that hold the
//! same rule go apart, and a reader then meets a command that says it kept a
//! record and a coordinator that deletes it.
//!
//! This module holds the rule once. Each caller builds a [`Node`] for each job
//! that it knows, and reads the answer from [`held_by_unfinished`].

use std::collections::{BTreeSet, HashMap};
use uuid::Uuid;

/// What the rule needs to know about one job.
///
/// The coordinator holds a job as a specification and a status, and a command
/// holds a record. This structure carries the fields that both have.
pub struct Node {
    pub id: Uuid,
    /// The pipeline of this job, when a pipeline made it.
    pub group: Option<Uuid>,
    /// True when the job reached a final state.
    pub terminal: bool,
    /// The jobs that this job waits for.
    pub needs: Vec<Uuid>,
    /// The jobs that this job follows.
    pub after: Vec<Uuid>,
}

/// Gives the records that a job which has not stopped still needs.
///
/// Such a record is not finished for the purpose of a deletion, whatever its
/// own state says. A job in the queue reads the record of the job that it waits
/// for: it needs the state to decide whether to run, and it needs the name and
/// the log to explain why it did not.
///
/// A deletion of that record takes the answer away from a job that has not yet
/// asked the question. It also makes a reader repeat work that already ran,
/// because the record is the only proof that the work happened.
///
/// Two rules hold a record, and a record that one rule holds stays:
///
/// 1. Every job that an unfinished job waits for, through the whole chain. A
///    job that waits for one job also depends on the jobs behind that job. The
///    walk therefore continues through each record that it holds, and it does
///    not stop at the first step.
/// 2. Every job of a pipeline that has one job which has not stopped. The jobs
///    of a pipeline are one piece of work, and a reader reads the whole
///    pipeline to see where it is. A pipeline can divide, so a stage that no
///    later stage waits for is still a stage of that pipeline.
///
/// Each rule needs a job that has not stopped, so every record goes when that
/// work stops. No record stays for ever.
pub fn held_by_unfinished(nodes: &[Node]) -> BTreeSet<Uuid> {
    let by_id: HashMap<Uuid, &Node> = nodes.iter().map(|n| (n.id, n)).collect();

    // The pipelines that hold one job or more which has not stopped.
    let live_groups: BTreeSet<Uuid> = nodes
        .iter()
        .filter(|n| !n.terminal)
        .filter_map(|n| n.group)
        .collect();

    let mut held = BTreeSet::new();

    // Rule 1. Start at each job that has not stopped, and walk every step.
    let mut todo: Vec<Uuid> = nodes
        .iter()
        .filter(|n| !n.terminal)
        .flat_map(|n| n.needs.iter().chain(n.after.iter()).copied())
        .collect();
    while let Some(id) = todo.pop() {
        // `insert` gives false for a record that the walk holds already. This
        // test ends the walk, and it also makes a circle of dependencies safe.
        if !held.insert(id) {
            continue;
        }
        if let Some(node) = by_id.get(&id) {
            todo.extend(node.needs.iter().chain(node.after.iter()).copied());
        }
    }

    // Rule 2. Hold every job of a pipeline that has work left.
    for node in nodes {
        if node.group.is_some_and(|g| live_groups.contains(&g)) {
            held.insert(node.id);
        }
    }

    held
}

/// Gives the jobs that have not stopped and that hold this record.
///
/// The message of a refusal names them, so a reader knows what to wait for.
/// A job that waits for the record directly comes first, because that is the
/// answer a reader can act on with no other step.
pub fn holders_of(nodes: &[Node], id: Uuid) -> Vec<Uuid> {
    let direct: Vec<Uuid> = nodes
        .iter()
        .filter(|n| !n.terminal)
        .filter(|n| n.needs.contains(&id) || n.after.contains(&id))
        .map(|n| n.id)
        .collect();
    if !direct.is_empty() {
        return direct;
    }

    // No job waits for this record directly. Give the jobs of the same
    // pipeline that have work left, and then the jobs whose chain reaches it.
    let group = nodes.iter().find(|n| n.id == id).and_then(|n| n.group);
    if let Some(group) = group {
        let same: Vec<Uuid> = nodes
            .iter()
            .filter(|n| !n.terminal && n.group == Some(group))
            .map(|n| n.id)
            .collect();
        if !same.is_empty() {
            return same;
        }
    }

    nodes
        .iter()
        .filter(|n| !n.terminal)
        .filter(|n| reaches(nodes, n, id))
        .map(|n| n.id)
        .collect()
}

/// True when the chain of one job reaches this record.
fn reaches(nodes: &[Node], from: &Node, id: Uuid) -> bool {
    let by_id: HashMap<Uuid, &Node> = nodes.iter().map(|n| (n.id, n)).collect();
    let mut seen = BTreeSet::new();
    let mut todo: Vec<Uuid> = from
        .needs
        .iter()
        .chain(from.after.iter())
        .copied()
        .collect();
    while let Some(next) = todo.pop() {
        if next == id {
            return true;
        }
        if !seen.insert(next) {
            continue;
        }
        if let Some(node) = by_id.get(&next) {
            todo.extend(node.needs.iter().chain(node.after.iter()).copied());
        }
    }
    false
}
