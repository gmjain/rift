use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::Span;
use tracing_subscriber::registry::{LookupSpan, Registry};

pub mod app;
pub mod config;
pub mod config_watcher;
pub mod drag;
pub mod gesture;
pub mod input;
pub mod menu_bar;
pub mod mission_control;
pub mod mission_control_observer;
pub mod notification_center;
pub mod process;
pub mod raise_manager;
pub mod reactor;
pub mod spaces;
pub mod stack_line;
pub mod window_notify;
pub mod wm_controller;

/// Deepest span ancestry an actor message carries to its receiver.
///
/// Receivers enter the sender's span and handle the message in a child span, so a message sent
/// while handling a message extends the sender's span chain. Actors that keep answering each
/// other (e.g. the raise manager starting the next queued raise from the previous
/// `RaiseCompleted`) would grow one chain without bound, and tracing-subscriber closes a chain
/// recursively (tokio-rs/tracing#1147): when its last span closes, a long enough chain overflows
/// the stack of the thread that drops it. Messages sent from deeper than this start a new root.
const MAX_PROPAGATED_SPAN_DEPTH: usize = 32;

/// The span a message carries: the sender's current span, or none once that span is
/// `MAX_PROPAGATED_SPAN_DEPTH` levels deep.
fn propagated_span() -> Span {
    let span = Span::current();
    let too_deep = span.with_subscriber(|(id, dispatch)| {
        dispatch
            .downcast_ref::<Registry>()
            .and_then(|registry| registry.span(id))
            .is_some_and(|span| span.scope().nth(MAX_PROPAGATED_SPAN_DEPTH - 1).is_some())
    });
    if too_deep.unwrap_or(false) {
        Span::none()
    } else {
        span
    }
}

pub struct Sender<Event>(UnboundedSender<(Span, Event)>);
pub type Receiver<Event> = UnboundedReceiver<(Span, Event)>;

pub fn channel<Event>() -> (Sender<Event>, Receiver<Event>) {
    let (tx, rx) = unbounded_channel();
    (Sender(tx), rx)
}

impl<Event> Sender<Event> {
    pub(crate) fn same_channel(&self, other: &Self) -> bool { self.0.same_channel(&other.0) }

    pub fn send(&self, event: Event) {
        // Most of the time we can ignore send errors, they just indicate the
        // app is shutting down.
        _ = self.try_send(event)
    }

    pub(crate) fn is_closed(&self) -> bool { self.0.is_closed() }

    pub fn try_send(&self, event: Event) -> Result<(), SendError<(Span, Event)>> {
        self.0.send((propagated_span(), event))
    }
}

impl<Event> Clone for Sender<Event> {
    fn clone(&self) -> Self { Self(self.0.clone()) }
}

impl<Event> std::fmt::Debug for Sender<Event> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("actor::Sender(...)")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tracing::{Subscriber, instrument, span};
    use tracing_subscriber::Layer;
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::layer::{Context, SubscriberExt};

    use super::*;

    /// Records the deepest span ancestry and how many spans are alive.
    #[derive(Clone, Default)]
    struct SpanProbe {
        max_depth: Arc<AtomicUsize>,
        live: Arc<AtomicUsize>,
        max_live: Arc<AtomicUsize>,
    }

    impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for SpanProbe {
        fn on_new_span(&self, _: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
            let depth = ctx.span(id).map_or(0, |span| span.scope().count());
            self.max_depth.fetch_max(depth, Ordering::Relaxed);
            let live = self.live.fetch_add(1, Ordering::Relaxed) + 1;
            self.max_live.fetch_max(live, Ordering::Relaxed);
        }

        fn on_close(&self, _: span::Id, _: Context<'_, S>) {
            self.live.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[instrument(skip(tx))]
    fn handle_ping(hops_left: u32, tx: &Sender<u32>) {
        if hops_left > 0 {
            tx.send(hops_left - 1);
        }
    }

    /// Two actors answering each other at INFO, like the reactor and the raise manager while
    /// raise sequences keep queueing: each message is handled inside the sender's span. The span
    /// chain must stay bounded; unbounded, closing it overflowed the reactor thread's stack
    /// (after ~2,000 raise sequences in rift; ~20,000 hops in a minimal program).
    #[test]
    fn actor_ping_pong_keeps_span_chain_bounded() {
        const HOPS: u32 = 20_000;
        let probe = SpanProbe::default();
        // rift's log layer, so the span lookup runs through the production layer stack.
        let subscriber = tracing_subscriber::registry()
            .with(crate::common::log::tree_layer())
            .with(probe.clone())
            .with(LevelFilter::INFO);
        tracing::subscriber::with_default(subscriber, || {
            let (a_tx, mut a_rx) = channel::<u32>();
            let (b_tx, mut b_rx) = channel::<u32>();
            a_tx.send(HOPS);
            let mut hops = 0;
            loop {
                let (rx, tx) = if hops % 2 == 0 {
                    (&mut a_rx, &b_tx)
                } else {
                    (&mut b_rx, &a_tx)
                };
                let Ok((span, hops_left)) = rx.try_recv() else { break };
                {
                    let _guard = span.enter();
                    handle_ping(hops_left, tx);
                }
                drop(span);
                hops += 1;
                // Checked every hop, so an unbounded chain fails here while it is still short.
                let depth = probe.max_depth.load(Ordering::Relaxed);
                assert!(
                    depth <= MAX_PROPAGATED_SPAN_DEPTH,
                    "span depth {depth} after {hops} hops"
                );
            }
            assert_eq!(hops, HOPS + 1);
        });
        assert!(probe.max_live.load(Ordering::Relaxed) <= MAX_PROPAGATED_SPAN_DEPTH + 1);
        assert_eq!(probe.live.load(Ordering::Relaxed), 0, "every span closed");
    }
}
