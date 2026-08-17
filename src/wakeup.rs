//! Docker events wake the daemon planner. They never delete anything.
//!
//! The event stream is a trigger only. Ownership, protection, age, and
//! authorization stay in the existing plan and executor path. A burst of
//! events becomes one wake after a quiet debounce. The configured interval
//! remains the backstop when events never go quiet.

use bollard::query_parameters::EventsOptions;
use bollard::Docker;
use futures_util::StreamExt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;

/// Quiet time required after the last Docker event before a wake.
pub const DEFAULT_EVENT_DEBOUNCE: Duration = Duration::from_millis(500);

/// Pause between a dropped event stream and the next connect attempt.
pub const EVENT_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Why the daemon left its wait and will run one planner pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonWake {
    Interval,
    Event { events: u64, reconnects: u64 },
    Reload,
    Terminate,
}

/// Coalesced Docker-event and reconnect ticks.
pub struct EventInbox {
    events: AtomicU64,
    reconnects: AtomicU64,
    notify: Notify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct TickCounts {
    events: u64,
    reconnects: u64,
}

impl EventInbox {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            events: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            notify: Notify::new(),
        })
    }

    pub fn record_event(&self) {
        self.events.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
    }

    pub fn record_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn take(&self) -> TickCounts {
        TickCounts {
            events: self.events.swap(0, Ordering::SeqCst),
            reconnects: self.reconnects.swap(0, Ordering::SeqCst),
        }
    }

    async fn notified(&self) {
        self.notify.notified().await;
    }
}

/// Follow the local Docker event stream and count arrivals.
///
/// The payload is discarded. Labels, state, and actor identity are not read,
/// so this path cannot infer ownership or inspect liveness.
#[must_use]
pub fn spawn_docker_event_listener(inbox: Arc<EventInbox>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(listen_event_streams(
        inbox,
        connect_docker_event_stream,
        EVENT_RECONNECT_BACKOFF,
    ))
}

async fn connect_docker_event_stream(
) -> Result<impl futures_util::Stream<Item = Result<(), ()>> + Unpin, ()> {
    let docker = Docker::connect_with_defaults().map_err(|_| ())?;
    Ok(docker
        .events(None::<EventsOptions>)
        .map(|item| item.map(|_| ()).map_err(|_| ())))
}

async fn listen_event_streams<S, F, Fut>(inbox: Arc<EventInbox>, mut connect: F, backoff: Duration)
where
    S: futures_util::Stream<Item = Result<(), ()>> + Unpin,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<S, ()>>,
{
    let mut ever_live = false;
    loop {
        if let Ok(mut stream) = connect().await {
            if ever_live {
                inbox.record_reconnect();
            }
            while let Some(item) = stream.next().await {
                if item.is_ok() {
                    ever_live = true;
                    inbox.record_event();
                } else {
                    break;
                }
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

/// Wait until the interval, a debounced event burst, or `extra` (signals).
///
/// Events that arrive during the previous pass sit in `inbox` and start the
/// debounce as soon as this wait begins. A flood that never goes quiet still
/// wakes at `interval`; those ticks are dropped because the planner inventories
/// the whole daemon on every pass.
pub async fn next_wake<F>(
    inbox: &EventInbox,
    interval: Duration,
    debounce: Duration,
    extra: F,
) -> DaemonWake
where
    F: Future<Output = DaemonWake>,
{
    tokio::pin!(extra);
    let interval_deadline = Instant::now() + interval;
    let mut pending = inbox.take();
    let mut debounce_deadline = nonempty_deadline(pending, debounce);

    loop {
        let now = Instant::now();
        if now >= interval_deadline {
            return DaemonWake::Interval;
        }
        if let Some(deadline) = debounce_deadline {
            if now >= deadline && pending.has_ticks() {
                return DaemonWake::Event {
                    events: pending.events,
                    reconnects: pending.reconnects,
                };
            }
        }

        let sleep_until = match debounce_deadline {
            Some(deadline) => deadline.min(interval_deadline),
            None => interval_deadline,
        };

        tokio::select! {
            other = &mut extra => return other,
            () = inbox.notified() => {
                pending.add(inbox.take());
                debounce_deadline = nonempty_deadline(pending, debounce);
            }
            () = tokio::time::sleep_until(sleep_until) => {}
        }
    }
}

impl TickCounts {
    fn has_ticks(self) -> bool {
        self.events > 0 || self.reconnects > 0
    }

    fn add(&mut self, other: Self) {
        self.events = self.events.saturating_add(other.events);
        self.reconnects = self.reconnects.saturating_add(other.reconnects);
    }
}

fn nonempty_deadline(pending: TickCounts, debounce: Duration) -> Option<Instant> {
    pending.has_ticks().then(|| Instant::now() + debounce)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{stream, StreamExt};
    use std::future::pending;
    use std::sync::atomic::AtomicUsize;

    async fn wake(inbox: &EventInbox, interval: Duration, debounce: Duration) -> DaemonWake {
        next_wake(inbox, interval, debounce, pending()).await
    }

    #[tokio::test]
    async fn a_burst_of_events_is_one_wake() {
        let inbox = EventInbox::new();
        let waiter = inbox.clone();
        let handle = tokio::spawn(async move {
            wake(
                &waiter,
                Duration::from_millis(400),
                Duration::from_millis(30),
            )
            .await
        });

        for _ in 0..20 {
            inbox.record_event();
        }
        assert_eq!(
            handle.await.expect("join waiter"),
            DaemonWake::Event {
                events: 20,
                reconnects: 0
            }
        );
    }

    #[tokio::test]
    async fn events_that_keep_arriving_reset_the_debounce() {
        let inbox = EventInbox::new();
        let waiter = inbox.clone();
        let handle = tokio::spawn(async move {
            wake(
                &waiter,
                Duration::from_millis(400),
                Duration::from_millis(40),
            )
            .await
        });

        inbox.record_event();
        tokio::time::sleep(Duration::from_millis(20)).await;
        inbox.record_event();
        assert_eq!(
            handle.await.expect("join waiter"),
            DaemonWake::Event {
                events: 2,
                reconnects: 0
            }
        );
    }

    #[tokio::test]
    async fn quiet_hosts_wake_on_the_interval() {
        let inbox = EventInbox::new();
        assert_eq!(
            wake(&inbox, Duration::from_millis(20), Duration::from_millis(50)).await,
            DaemonWake::Interval
        );
    }

    #[tokio::test]
    async fn a_flood_that_never_goes_quiet_still_hits_the_interval() {
        let inbox = EventInbox::new();
        let waiter = inbox.clone();
        let handle = tokio::spawn(async move {
            wake(
                &waiter,
                Duration::from_millis(60),
                Duration::from_millis(40),
            )
            .await
        });

        let pumper = inbox.clone();
        tokio::spawn(async move {
            loop {
                pumper.record_event();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        assert_eq!(handle.await.expect("join waiter"), DaemonWake::Interval);
    }

    #[tokio::test]
    async fn reconnects_coalesce_with_events_into_one_wake() {
        let inbox = EventInbox::new();
        let waiter = inbox.clone();
        let handle = tokio::spawn(async move {
            wake(
                &waiter,
                Duration::from_millis(400),
                Duration::from_millis(20),
            )
            .await
        });

        inbox.record_event();
        inbox.record_reconnect();
        inbox.record_event();
        assert_eq!(
            handle.await.expect("join waiter"),
            DaemonWake::Event {
                events: 2,
                reconnects: 1
            }
        );
    }

    #[tokio::test]
    async fn extra_signal_wins_over_a_pending_debounce() {
        let inbox = EventInbox::new();
        inbox.record_event();
        let wake = next_wake(
            &inbox,
            Duration::from_secs(30),
            Duration::from_millis(500),
            async { DaemonWake::Terminate },
        )
        .await;
        assert_eq!(wake, DaemonWake::Terminate);
    }

    #[tokio::test]
    async fn a_second_successful_stream_records_one_reconnect() {
        let inbox = EventInbox::new();
        let connects = Arc::new(AtomicUsize::new(0));
        let factory_connects = connects.clone();
        let handle = tokio::spawn(listen_event_streams(
            inbox.clone(),
            move || {
                let n = factory_connects.fetch_add(1, Ordering::SeqCst);
                async move {
                    match n {
                        0 => Ok(stream::iter([Ok(())]).boxed()),
                        1 => Ok(stream::pending().boxed()),
                        _ => Err(()),
                    }
                }
            },
            Duration::from_millis(10),
        ));

        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            let seen = TickCounts {
                events: inbox.events.load(Ordering::SeqCst),
                reconnects: inbox.reconnects.load(Ordering::SeqCst),
            };
            if seen.events == 1 && seen.reconnects == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "did not observe reconnect tick: events={} reconnects={}",
                seen.events,
                seen.reconnects
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(connects.load(Ordering::SeqCst) >= 2);
        handle.abort();
    }
}
