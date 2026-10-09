use crate::{Error, Limits};
use std::time::{Duration, Instant};

/// A monotonic, cooperative deadline. Nested work tightens the duration while
/// retaining the same start; cleanup and completed commits must not be cancelled.
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    started: Instant,
    duration: Duration,
}
impl Deadline {
    pub fn new(milliseconds: usize) -> Self {
        Self::from_start(Instant::now(), milliseconds)
    }
    pub fn from_start(started: Instant, milliseconds: usize) -> Self {
        Self {
            started,
            duration: Duration::from_millis(milliseconds as u64),
        }
    }
    pub fn for_limits(limits: &Limits) -> Self {
        Self::new(limits.max_elapsed_ms)
    }
    pub fn tighten(self, milliseconds: usize) -> Self {
        Self {
            duration: self
                .duration
                .min(Duration::from_millis(milliseconds as u64)),
            ..self
        }
    }
    pub fn expires_at(self) -> Option<Instant> {
        self.started.checked_add(self.duration)
    }
    pub fn elapsed_ms(self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
    pub fn check(self, phase: &str) -> Result<(), Error> {
        if self.started.elapsed() >= self.duration {
            Err(Error(format!(
                "graph statement time budget exceeded (limit {} ms, phase {phase})",
                self.duration.as_millis()
            )))
        } else {
            Ok(())
        }
    }
}
