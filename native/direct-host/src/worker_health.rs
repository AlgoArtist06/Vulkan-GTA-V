//! Error/liveness observation never queues work behind the worker it observes.
use std::sync::{atomic::{AtomicBool, AtomicU32, Ordering}, Mutex};

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum HealthError { Failed(String), Closed }

pub(crate) struct WorkerHealth {
    alive: AtomicBool,
    failure: Mutex<Option<String>>,
    presents: AtomicU32,
    world: AtomicBool,
}
impl WorkerHealth {
    pub(crate) fn new() -> Self { Self { alive: AtomicBool::new(true), failure: Mutex::new(None), presents: AtomicU32::new(0), world: AtomicBool::new(false) } }
    pub(crate) fn fail(&self, error: String) {
        if let Ok(mut failure) = self.failure.lock() { failure.get_or_insert(error); }
    }
    /// Presented frames and whether the world is shown (the title screen and FPS counter).
    pub(crate) fn set_progress(&self, presents: u32, world: bool) {
        self.presents.store(presents, Ordering::Release);
        self.world.store(world, Ordering::Release);
    }
    pub(crate) fn progress(&self) -> (u32, bool) { (self.presents.load(Ordering::Acquire), self.world.load(Ordering::Acquire)) }
    pub(crate) fn stopped(&self) { self.alive.store(false, Ordering::Release); }
    pub(crate) fn check(&self) -> Result<(), HealthError> {
        let failure = self.failure.lock().map_err(|_| HealthError::Failed("GPU worker health mutex poisoned".into()))?;
        if let Some(error) = failure.as_ref() { return Err(HealthError::Failed(error.clone())); }
        if !self.alive.load(Ordering::Acquire) { return Err(HealthError::Closed); }
        Ok(())
    }
}


