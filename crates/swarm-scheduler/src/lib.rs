//! Scheduling and inference allocation for `swarm`.
//!
//! Pure policy, unit-testable without tokio: it decides *which* ready agents
//! run next, *where* their inference goes (which provider), and *how much* of
//! the scarce inference budget each research direction receives. It does not
//! run tokio, touch the database, or own providers — the runtime drives it.
//!
//! Given the ready tasks and the lanes already in flight,
//! [`Scheduler::plan`] produces a batch that:
//!
//! * orders tasks by effective priority (base priority plus a research boost),
//!   breaking ties by age and then id so plans are deterministic;
//! * admits only as many as the global and per-provider concurrency limits
//!   allow, given the occupancy the runtime already reports; and
//! * round-robins across research groups so one direction cannot starve the
//!   rest of the swarm.
//!
//! The runtime translates an admitted batch into real work: it dispatches each
//! task to a provider lane, updates the [`Occupancy`] snapshot as lanes open and
//! close, and calls `plan` again when capacity frees up.

pub mod config;
pub mod plan;
pub mod task;

pub use config::{Occupancy, SchedulerConfig};
pub use plan::{DeferReason, PlannedDispatch, SchedulePlan, Scheduler};
pub use task::{SchedulableTask, compare, effective_priority};
