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
///
/// A call costs the gateway almost nothing while it runs - one outbound
/// stream, a task, and a row written when it ends - so the old ceiling of
/// eight was far below what the box can carry, and the cap of two per
/// licence meant an operator sending from a dozen profiles had ten of them
/// queueing behind two. Paid models on OpenRouter have no platform-level
/// request cap of their own; what is left to protect is the box, and these
/// numbers leave it plenty of room.
pub const DEFAULT_INFLIGHT: usize = 128;
pub const DEFAULT_PER_LICENSE: usize = 32;
pub const DEFAULT_QUEUED: usize = 1024;
/// How long a caller may wait for a slot.
///
/// Long, on purpose. A queue that gives up after a minute is not a queue: the
/// operator gets an error for a request the gateway would have served a few
/// seconds later. The wait ends by itself the moment the caller hangs up -
/// the whole handler is dropped - so this ceiling only catches a client that
/// is still holding the line after ten minutes, which no answer is worth.
pub const DEFAULT_WAIT_SECONDS: u64 = 600;

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
    /// One semaphore per licence, with the size it was made at.
    per_license: Mutex<HashMap<String, (usize, Arc<Semaphore>)>>,
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

    /// The licence's own gate, sized by what its tier allows - and resized
    /// when that changes, so an upgrade takes effect on the next call rather
    /// than on the next restart.
    fn license_gate(&self, license_id: &str, allowed: usize) -> Arc<Semaphore> {
        let allowed = allowed.clamp(1, self.limits.inflight.max(1));
        let mut gates = self.per_license.lock();
        match gates.get(license_id) {
            Some((size, gate)) if *size == allowed => gate.clone(),
            _ => {
                let gate = Arc::new(Semaphore::new(allowed));
                gates.insert(license_id.to_string(), (allowed, gate.clone()));
                gate
            }
        }
    }

    /// Wait for a slot, or say why there will not be one.
    ///
    /// The per-licence slot is taken first: a licence that is already running
    /// its share should wait without occupying one of the global slots, or it
    /// would block callers the gateway has room for.
    /// `allowed` is what this licence's tier may run at once; zero means the
    /// gateway's own per-licence default.
    pub async fn admit(&self, license_id: &str, allowed: u32) -> Result<Slot, Rejected> {
        let Ok(_queued) = self.waiting.clone().try_acquire_owned() else {
            return Err(Rejected::Full);
        };

        let allowed = if allowed == 0 {
            self.limits.per_license
        } else {
            allowed as usize
        };
        let gate = self.license_gate(license_id, allowed);
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
        let _first = queue.admit("busy", 0).await.unwrap();
        let _second = queue.admit("busy", 0).await.unwrap();
        // Its third call waits, and times out rather than hanging.
        assert_eq!(queue.admit("busy", 0).await.err(), Some(Rejected::TimedOut));
        // Meanwhile everyone else is served at once.
        assert!(queue.admit("someone-else", 0).await.is_ok());
    }

    #[tokio::test]
    async fn a_finished_call_hands_its_slot_on() {
        let queue = Queue::new(Limits {
            inflight: 1,
            per_license: 1,
            queued: 8,
            wait: Duration::from_millis(200),
        });
        let slot = queue.admit("a", 0).await.unwrap();
        assert_eq!(queue.inflight(), 1);
        drop(slot);
        assert_eq!(queue.inflight(), 0);
        assert!(queue.admit("a", 0).await.is_ok());
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
        let held = queue.admit("a", 0).await.unwrap();

        // One caller waits, and takes the only place in the waiting room.
        let waiter = tokio::spawn({
            let queue = queue.clone();
            async move { queue.admit("b", 0).await.map(|_| ()) }
        });
        while queue.queued() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let started = std::time::Instant::now();
        assert_eq!(queue.admit("c", 0).await.err(), Some(Rejected::Full));
        assert!(started.elapsed() < Duration::from_secs(1));

        drop(held);
        assert!(waiter.await.unwrap().is_ok());
    }

    /// The point of a queue: a caller waits and is served when a slot frees,
    /// instead of being told to come back.
    #[tokio::test]
    async fn a_waiting_caller_is_served_rather_than_refused() {
        let queue = Arc::new(Queue::new(Limits {
            inflight: 1,
            per_license: 1,
            queued: 8,
            wait: Duration::from_secs(10),
        }));
        let held = queue.admit("a", 0).await.unwrap();
        let waiter = tokio::spawn({
            let queue = queue.clone();
            async move { queue.admit("a", 0).await.map(|_| ()) }
        });
        while queue.queued() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(held);
        assert!(
            waiter.await.unwrap().is_ok(),
            "the slot went to the one waiting for it"
        );
    }

    /// A licence runs what its tier allows, and a changed tier is felt on the
    /// next call.
    #[tokio::test]
    async fn a_tier_decides_how_much_one_licence_runs_at_once() {
        let queue = Queue::new(Limits {
            inflight: 16,
            per_license: 2,
            queued: 16,
            wait: Duration::from_millis(50),
        });
        let _a = queue.admit("team", 3).await.unwrap();
        let _b = queue.admit("team", 3).await.unwrap();
        let _c = queue.admit("team", 3).await.unwrap();
        assert_eq!(queue.admit("team", 3).await.err(), Some(Rejected::TimedOut));
        // Same licence on a smaller tier: two at a time, and the gate is
        // rebuilt at the new size.
        let queue = Queue::new(Limits {
            inflight: 16,
            per_license: 32,
            queued: 16,
            wait: Duration::from_millis(50),
        });
        let _first = queue.admit("solo", 1).await.unwrap();
        assert_eq!(queue.admit("solo", 1).await.err(), Some(Rejected::TimedOut));
        assert!(
            queue.admit("solo", 5).await.is_ok(),
            "an upgrade is felt at once"
        );
    }

    /// The shipped numbers, so raising them is a deliberate edit and not a
    /// typo: the gateway carries a crowd, and one licence carries a batch.
    #[test]
    fn the_defaults_fit_a_room_full_of_operators() {
        let limits = Limits::default();
        assert_eq!(limits.inflight, 128);
        assert_eq!(limits.per_license, 32);
        assert_eq!(limits.queued, 1024);
        assert_eq!(limits.wait, Duration::from_secs(600));
    }
}
