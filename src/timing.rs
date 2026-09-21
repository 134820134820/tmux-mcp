//! Per-tool measurements persisted with the existing bounded action log.
//! No command text, paths, credentials, or subprocess arguments are collected here.
use std::cell::RefCell;
use std::future::Future;
use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallTiming {
    pub duration_ms: u64,
    pub before_dispatch_ms: u64,
    pub outcome: String,
    pub requested_wait_ms: Option<u64>,
    pub wait_timed_out: bool,
    pub target: Option<String>,
    pub transports: Vec<TransportTiming>,
    pub omitted_transports: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportTiming {
    pub operation: String,
    pub transport: String,
    pub queue_ms: u64,
    pub duration_ms: u64,
    pub outcome: String,
    pub stdout_bytes: usize,
}

tokio::task_local! { static CURRENT: RefCell<CallTiming>; }

pub async fn measure<F: Future>(work: F) -> (F::Output, CallTiming) {
    let start = Instant::now();
    let initial = CallTiming {
        target: crate::targets::current(),
        ..Default::default()
    };
    CURRENT
        .scope(RefCell::new(initial), async {
            let result = work.await;
            let mut timing = CURRENT.with(|current| current.take());
            timing.duration_ms = millis(start);
            (result, timing)
        })
        .await
}

pub fn millis(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}

/// Drop also records interrupted operations. Spawned background watchers deliberately
/// do not inherit this scope: their lifetime is not the requesting tool's latency.
pub struct TransportTimer {
    start: Instant,
    dispatched: bool,
    sample: TransportTiming,
}

impl TransportTimer {
    pub fn new(operation: &str, ssh: bool) -> Self {
        Self {
            start: Instant::now(),
            dispatched: false,
            sample: TransportTiming {
                operation: operation.to_string(),
                transport: if ssh { "ssh" } else { "local" }.into(),
                queue_ms: 0,
                duration_ms: 0,
                outcome: "interrupted".into(),
                stdout_bytes: 0,
            },
        }
    }

    pub fn dispatched(&mut self) {
        self.dispatched = true;
        self.sample.queue_ms = millis(self.start);
    }

    pub fn finish(&mut self, outcome: &str, stdout_bytes: usize) {
        self.sample.outcome = outcome.into();
        self.sample.stdout_bytes = stdout_bytes;
    }
}

impl Drop for TransportTimer {
    fn drop(&mut self) {
        self.sample.duration_ms = millis(self.start);
        if !self.dispatched {
            self.sample.queue_ms = self.sample.duration_ms;
        }
        let _ = CURRENT.try_with(|current| {
            let mut current = current.borrow_mut();
            // ponytail: cap diagnostic detail per call; aggregate if >128 operations is common.
            if current.transports.len() < 128 {
                current.transports.push(self.sample.clone());
            } else {
                current.omitted_transports += 1;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_timing_isolates_targets_and_bounds_detail() {
        async fn run(target: &str) -> CallTiming {
            crate::targets::scope(target.into(), async {
                measure(async {
                    for _ in 0..130 {
                        let mut timer = TransportTimer::new("capture-pane", true);
                        timer.dispatched();
                        tokio::task::yield_now().await;
                        timer.finish("ok", 12);
                    }
                })
                .await
                .1
            })
            .await
        }
        let (a, b) = tokio::join!(run("host-admin"), run("host-intern"));
        assert_eq!(a.target.as_deref(), Some("host-admin"));
        assert_eq!(b.target.as_deref(), Some("host-intern"));
        assert_eq!(a.transports.len(), 128);
        assert_eq!(a.omitted_transports, 2);
        assert_eq!(a.transports[0].stdout_bytes, 12);
        let (_, interrupted) = measure(async {
            let _timer = TransportTimer::new("stat", true);
        })
        .await;
        assert_eq!(interrupted.transports[0].outcome, "interrupted");
    }
}
