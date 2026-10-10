//! The scheduler's rules, without its I/O (spec §8, §11).
//!
//! Everything here is a function of its inputs and of the time it is told:
//! no clock is read, nothing sleeps, nothing is stored. The scheduler crate
//! drives these rules with real time, the store and the two fetch paths; a
//! test drives them with numbers, which is how a simulated day of twenty
//! personas runs in milliseconds.
//!
//! - [`queue`]: the queue of waiting work, synchronous requests before
//!   jobs, then first in, first out.
//! - [`day`]: when a persona is awake.
//! - [`selection`]: the score that chooses a persona (spec §8.2), and what
//!   two personas' networks have in common.
//! - [`retry`]: what follows an attempt: done, the same persona in its
//!   browser, another persona; and how long a blocked persona stays away.
//! - [`breaker`]: the circuit breaker of a domain (spec §8.3).
//! - [`politeness`]: the pause after server errors in a row.
//! - [`budget`]: the bandwidth budget of the proxy plan (decision D5).
//! - [`planner`]: the rules together: which waiting request leaves now, on
//!   which persona, and when to look again.

pub mod breaker;
pub mod budget;
pub mod day;
pub mod planner;
pub mod politeness;
pub mod queue;
pub mod retry;
pub mod selection;
