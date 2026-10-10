//! The queue of waiting work (spec §11): synchronous requests before jobs,
//! and within one priority first in, first out.

use std::collections::BTreeMap;

use crate::macros::string_enum;

string_enum! {
    /// Whose work something is. The order of the variants is the order in
    /// which waiting work is looked at.
    pub enum Priority {
        /// A caller is waiting for the answer.
        Sync => "sync",
        /// Part of a job: a batch, a flow or a crawl.
        Job => "job",
    }
}

/// Names one entry of a [`WorkQueue`]. Tickets of one queue are handed out
/// in rising order and never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticket {
    priority: Priority,
    sequence: u64,
}

impl Ticket {
    /// Whose work the entry is.
    pub const fn priority(self) -> Priority {
        self.priority
    }

    /// The position in which the entry arrived, counted over the whole
    /// queue.
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Waiting work in the order it is to be looked at.
#[derive(Debug, Clone)]
pub struct WorkQueue<T> {
    next: u64,
    entries: BTreeMap<Ticket, T>,
}

impl<T> Default for WorkQueue<T> {
    fn default() -> Self {
        Self {
            next: 0,
            entries: BTreeMap::new(),
        }
    }
}

impl<T> WorkQueue<T> {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an entry behind everything of its priority.
    pub fn push(&mut self, priority: Priority, entry: T) -> Ticket {
        let ticket = Ticket {
            priority,
            sequence: self.next,
        };
        self.next += 1;
        self.entries.insert(ticket, entry);
        ticket
    }

    /// Takes an entry out, wherever it stands.
    pub fn remove(&mut self, ticket: Ticket) -> Option<T> {
        self.entries.remove(&ticket)
    }

    /// The entry of a ticket.
    pub fn get(&self, ticket: Ticket) -> Option<&T> {
        self.entries.get(&ticket)
    }

    /// Every entry in the order it is to be looked at.
    pub fn iter(&self) -> impl Iterator<Item = (Ticket, &T)> {
        self.entries.iter().map(|(ticket, entry)| (*ticket, entry))
    }

    /// The tickets in the order their entries are to be looked at.
    pub fn tickets(&self) -> Vec<Ticket> {
        self.entries.keys().copied().collect()
    }

    /// How many entries wait.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// How many entries of one priority wait.
    pub fn len_of(&self, priority: Priority) -> usize {
        self.entries
            .keys()
            .filter(|ticket| ticket.priority == priority)
            .count()
    }

    /// Whether nothing waits.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{Priority, WorkQueue};
    use crate::macros::test_support::assert_string_enum;

    #[test]
    fn priority_names_are_stable_and_sync_comes_first() {
        assert_string_enum(Priority::ALL, Priority::as_str, &["sync", "job"]);
        assert!(Priority::Sync < Priority::Job);
    }

    #[test]
    fn sync_work_is_looked_at_before_jobs_and_each_in_arrival_order() {
        let mut queue = WorkQueue::new();
        assert!(queue.is_empty());
        let first_job = queue.push(Priority::Job, "job 1");
        let first_sync = queue.push(Priority::Sync, "sync 1");
        queue.push(Priority::Job, "job 2");
        queue.push(Priority::Sync, "sync 2");

        let order: Vec<&str> = queue.iter().map(|(_, entry)| *entry).collect();

        assert_eq!(order, ["sync 1", "sync 2", "job 1", "job 2"]);
        assert_eq!(queue.len(), 4);
        assert!(!queue.is_empty());
        assert_eq!(queue.len_of(Priority::Sync), 2);
        assert_eq!(queue.len_of(Priority::Job), 2);
        assert_eq!(first_sync.priority(), Priority::Sync);
        assert_eq!(first_job.sequence(), 0);
        assert_eq!(first_sync.sequence(), 1);
        assert_eq!(queue.tickets()[0], first_sync);
        assert_eq!(queue.tickets()[2], first_job);
    }

    #[test]
    fn an_entry_can_be_taken_out_of_the_middle() {
        let mut queue = WorkQueue::new();
        let a = queue.push(Priority::Job, 'a');
        let b = queue.push(Priority::Job, 'b');
        let c = queue.push(Priority::Job, 'c');

        assert_eq!(queue.get(b), Some(&'b'));
        assert_eq!(queue.remove(b), Some('b'));
        assert_eq!(queue.remove(b), None);
        assert_eq!(queue.get(b), None);

        let left: Vec<char> = queue.iter().map(|(_, entry)| *entry).collect();
        assert_eq!(left, ['a', 'c']);
        assert_eq!(queue.len(), 2);
        // A ticket is never handed out twice.
        let d = queue.push(Priority::Job, 'd');
        assert!(d > c && c > a);
        assert_eq!(d.sequence(), 3);
        assert_eq!(queue.len_of(Priority::Sync), 0);
    }
}
