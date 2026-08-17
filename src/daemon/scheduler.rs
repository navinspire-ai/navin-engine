//! Job scheduler: a bounded queue drained by workers.
//!
//! Sprint 1 ships the queue and job states; later sprints enqueue proof
//! and evolve campaigns here. Event-driven by design: an empty queue
//! costs nothing.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobRecord {
    pub id: u64,
    pub kind: String,
    pub state: JobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug)]
pub struct Job {
    pub id: u64,
    pub kind: String,
    pub params: Value,
}

/// Shared scheduler handle: enqueue jobs and inspect their states.
#[derive(Clone)]
pub struct Scheduler {
    next_id: Arc<AtomicU64>,
    records: Arc<Mutex<HashMap<u64, JobRecord>>>,
    tx: mpsc::Sender<Job>,
}

const QUEUE_DEPTH: usize = 64;

impl Scheduler {
    /// Returns the handle plus the receiving end the worker drains.
    pub fn new() -> (Self, mpsc::Receiver<Job>) {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        (
            Scheduler {
                next_id: Arc::new(AtomicU64::new(1)),
                records: Arc::new(Mutex::new(HashMap::new())),
                tx,
            },
            rx,
        )
    }

    pub fn enqueue(&self, kind: &str, params: Value) -> Result<u64, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let job = Job { id, kind: kind.to_owned(), params };
        self.records.lock().expect("scheduler lock").insert(
            id,
            JobRecord { id, kind: kind.to_owned(), state: JobState::Queued, detail: None },
        );
        self.tx
            .try_send(job)
            .map_err(|_| "engine queue is full, retry later".to_owned())?;
        Ok(id)
    }

    pub fn set_state(&self, id: u64, state: JobState, detail: Option<String>) {
        if let Some(record) = self.records.lock().expect("scheduler lock").get_mut(&id) {
            record.state = state;
            record.detail = detail;
        }
    }

    pub fn snapshot(&self) -> Vec<JobRecord> {
        let mut jobs: Vec<JobRecord> = self
            .records
            .lock()
            .expect("scheduler lock")
            .values()
            .cloned()
            .collect();
        jobs.sort_by_key(|j| j.id);
        jobs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn jobs_move_through_states() {
        let (scheduler, mut rx) = Scheduler::new();
        let id = scheduler.enqueue("baseline.run", Value::Null).unwrap();
        assert_eq!(scheduler.snapshot()[0].state, JobState::Queued);

        let job = rx.recv().await.unwrap();
        assert_eq!(job.id, id);
        scheduler.set_state(id, JobState::Running, None);
        assert_eq!(scheduler.snapshot()[0].state, JobState::Running);
        scheduler.set_state(id, JobState::Completed, None);
        assert_eq!(scheduler.snapshot()[0].state, JobState::Completed);
    }
}
