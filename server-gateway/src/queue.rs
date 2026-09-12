//! The waiting room.
//!
//! A hundred operators pressing "reply" in the same minute is the normal
//! shape of this traffic, and the upstreams behind the gateway have their own
//! rate limits: letting all hundred requests through at once earns a wall of
//! 429s, and letting none through wastes the keys. So the gateway admits a
//! fixed number of calls at a time and makes the rest queue.
//!
//! This is a queue inside the process, not a broker. A broker (Rabbit, NATS,
//! Redis) buys one thing: a queue that survives the gateway dying, and can be
//! shared by several gateways. Neither applies here — an LLM call is answered
//! on the connection that asked for it, so a request that outlives the
//! process has nobody left to answer, and a second gateway would want its own
//! admission control anyway. What a broker would cost is a second service to
//! run, back up and move. If the day comes that the gateway runs on more than
//! one box, the fair thing to add is a shared limiter, not a message bus.
//!
//! Two rules, and both matter:
//!
//!   * a cap on how many calls run at once, so the upstream sees a steady
//!     stream rather than a spike;
//!   * a cap per licence, so one operator running a batch cannot fill every
//!     slot and leave everyone else waiting behind them.
//!
//! A request that cannot be admitted before the deadline is turned away with
//! a `Retry-After`, which is kinder than a connection held open for a minute
//! and then dropped.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Defaults sized for one VPS talking to hosted providers.
pub const DEFAULT_INFLIGHT: usize = 8;
pub const DEFAULT_PER_LICENSE: usize = 2;
pub const DEFAULT_QUEUED: usize = 128;
pub const DEFAULT_WAIT_SECONDS: u64 = 45;

/// Why a request was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// The queue itself is full: the gateway is over capacity, not this
    /// licence.
    Full,
    /// Admitted nobody before the deadline.
    TimedOut,
}

impl Rejected {
    pub fn message(self) -> &'static str {
        match self {
            Rejected::Full => "the gateway is at capacity; try again shortly",
            Rejected::TimedOut => "the gateway is busy and the request waited too long",
        }
    }
}

/// A slot in the gateway. Held for the length of one upstream call and
/// released by dropping it — including when the handler fails, which is the
/// reason it is a guard and not a pair of calls.
pub struct Slot {
    _global: OwnedSemaphorePermit,
    _per_license: OwnedSemaphorePermit,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub inflight: usize,
    pub per_license: usize,
    pub queued: usize,
    pub wait: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            inflight: DEFAULT_INFLIGHT,
            per_license: DEFAULT_PER_LICENSE,
            queued: DEFAULT_QUEUED,
            wait: Duration::from_secs(DEFAULT_WAIT_SECONDS),
        }
    }
}

pub struct Queue {
    limits: Limits,
    global: Arc<Semaphore>,
    /// One semaphore per licence, made on first sight.
    per_license: Mutex<HashMap<String, Arc<Semaphore>>>,
    /// How many requests are waiting for a slot right now.
    waiting: Arc<Semaphore>,
}

impl Queue {
    pub fn new(limits: Limits) -> Queue {
        Queue {
            global: Arc::new(Semaphore::new(limits.inflight.max(1))),
            per_license: Mutex::new(HashMap::new()),
            waiting: Arc::new(Semaphore::new(limits.queued.max(1))),
            limits,
        }
    }

    fn license_gate(&self, license_id: &str) -> Arc<Semaphore> {
        let mut gates = self.per_license.lock();
        gates
            .entry(license_id.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.limits.per_license.max(1))))
            .clone()
    }

    /// Wait for a slot, or say why there will not be one.
    ///
    /// The per-licence slot is taken first: a licence that is already running
    /// its share should wait without occupying one of the global slots, or it
    /// would block callers the gateway has room for.
    pub async fn admit(&self, license_id: &str) -> Result<Slot, Rejected> {
        let Ok(_queued) = self.waiting.clone().try_acquire_owned() else {
            return Err(Rejected::Full);
        };

        let gate = self.license_gate(license_id);
        let per_license = match tokio::time::timeout(self.limits.wait, gate.acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(Rejected::Full),
            Err(_) => return Err(Rejected::TimedOut),
        };
        let global =
            match tokio::time::timeout(self.limits.wait, self.global.clone().acquire_owned()).await
            {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => return Err(Rejected::Full),
                Err(_) => return Err(Rejected::TimedOut),
            };

        Ok(Slot {
            _global: global,
            _per_license: per_license,
        })
    }

    /// Calls running right now, and how many of the gateway's slots are free.
    pub fn inflight(&self) -> usize {
        self.limits
            .inflight
            .saturating_sub(self.global.available_permits())
    }

    pub fn queued(&self) -> usize {
        self.limits
            .queued
            .saturating_sub(self.waiting.available_permits())
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// How long to tell a turned-away caller to wait. A whole minute is too
    /// long to sit on a screen; a second invites a stampede.
    pub fn retry_after(&self) -> u64 {
        5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_licence_cannot_take_every_slot() {
        let queue = Queue::new(Limits {
            inflight: 4,
            per_license: 2,
            queued: 16,
            wait: Duration::from_millis(50),
        });
        let _first = queue.admit("busy").await.unwrap();
        let _second = queue.admit("busy").await.unwrap();
        // Its third call waits, and times out rather than hanging.
        assert_eq!(queue.admit("busy").await.err(), Some(Rejected::TimedOut));
        // Meanwhile everyone else is served at once.
        assert!(queue.admit("someone-else").await.is_ok());
    }

    #[tokio::test]
    async fn a_finished_call_hands_its_slot_on() {
        let queue = Queue::new(Limits {
            inflight: 1,
            per_license: 1,
            queued: 8,
            wait: Duration::from_millis(200),
        });
        let slot = queue.admit("a").await.unwrap();
        assert_eq!(queue.inflight(), 1);
        drop(slot);
        assert_eq!(queue.inflight(), 0);
        assert!(queue.admit("a").await.is_ok());
    }

    /// A waiting room has a size, and the caller who finds it full is told
    /// so at once rather than after the whole timeout.
    #[tokio::test]
    async fn a_full_waiting_room_says_so_at_once() {
        let queue = Arc::new(Queue::new(Limits {
            inflight: 1,
            per_license: 1,
            queued: 1,
            wait: Duration::from_secs(30),
        }));
        let held = queue.admit("a").await.unwrap();

        // One caller waits, and takes the only place in the waiting room.
        let waiter = tokio::spawn({
            let queue = queue.clone();
            async move { queue.admit("b").await.map(|_| ()) }
        });
        while queue.queued() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let started = std::time::Instant::now();
        assert_eq!(queue.admit("c").await.err(), Some(Rejected::Full));
        assert!(started.elapsed() < Duration::from_secs(1));

        drop(held);
        assert!(waiter.await.unwrap().is_ok());
    }
}
