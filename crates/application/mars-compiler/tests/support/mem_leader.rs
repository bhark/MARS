//! in-memory `LeaderLock` for snapshot integration tests.

#![allow(dead_code)] // not every test binary uses always_grants/always_denies

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use mars_source::{LeaderLock, LeaderLockGuard, SourceError};

#[derive(Debug, Default)]
pub(crate) struct MemLeader {
    held: AtomicBool,
    deny: AtomicBool,
}

impl MemLeader {
    pub(crate) fn always_grants() -> Self {
        Self {
            held: AtomicBool::new(false),
            deny: AtomicBool::new(false),
        }
    }

    pub(crate) fn always_denies() -> Self {
        Self {
            held: AtomicBool::new(false),
            deny: AtomicBool::new(true),
        }
    }
}

#[derive(Debug)]
struct MemGuard;

impl LeaderLockGuard for MemGuard {}

#[async_trait]
impl LeaderLock for MemLeader {
    async fn try_acquire(&self, _key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
        if self.deny.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if self.held.swap(true, Ordering::Relaxed) {
            return Ok(None);
        }
        Ok(Some(Box::new(MemGuard)))
    }
}
