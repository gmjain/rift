use std::collections::VecDeque;
use std::time::{Duration, Instant};

use objc2_core_foundation::CGPoint;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use crate::actor::app::{AppThreadHandle, Quiet, Request, WindowId};
use crate::actor::{self, input, reactor};
use crate::common::collections::{HashMap, HashSet};
use crate::sys::app::pid_t;
use crate::sys::timer::Timer;

/// Messages that can be sent to the raise manager
#[derive(Debug)]
pub enum Event {
    RaiseRequest(RaiseRequest),
    RaiseCompleted {
        window_id: WindowId,
        sequence_id: u64,
    },
    RaiseTimeout {
        sequence_id: u64,
    },
}

/// A queued layout response waiting to be processed.
#[derive(Debug)]
pub struct RaiseRequest {
    /// A set of windows to raise concurrently. Each inner vec should contain a
    /// set of windows from the same app and on the same screen.
    pub raise_windows: Vec<Vec<WindowId>>,
    /// The window to raise and focus last.
    pub focus_window: Option<(WindowId, Option<CGPoint>)>,
    /// Windows to order above the focus window once it is raised, bottom to top,
    /// without activating their apps (floating windows kept above tiled ones).
    /// Raised one app at a time so the order holds across apps.
    pub restack_windows: Vec<WindowId>,
    pub app_handles: HashMap<i32, AppThreadHandle>,
    pub focus_quiet: Quiet,
}

pub struct RaiseManager {
    /// The currently active sequence, if any
    active_sequence: Option<ActiveSequence>,
    /// Queued sequences waiting to be processed
    queued_sequences: VecDeque<RaiseRequest>,
    next_sequence_id: u64,
    input_tx: Option<input::Sender>,
}

/// Tracks an executing sequence of raises.
#[derive(Debug)]
struct ActiveSequence {
    sequence_id: u64,
    pending_raises: HashSet<WindowId>,
    focus_batch: Option<(pid_t, Vec<WindowId>, Option<CGPoint>, Quiet)>,
    /// Per-app batches still to restack after the focus window, in order.
    restack: VecDeque<Vec<WindowId>>,
    app_handles: HashMap<i32, AppThreadHandle>,
    raise_token: CancellationToken,
    started_at: Instant,
    timed_out: bool,
}

pub type Sender = actor::Sender<Event>;
type Receiver = actor::Receiver<Event>;

const TIMEOUT_DURATION: Duration = Duration::from_millis(250);

impl RaiseManager {
    /// Run the raise manager task.
    pub async fn run(
        mut rx: Receiver,
        events_tx: reactor::Sender,
        input_tx: Option<input::Sender>,
    ) {
        let mut raise_manager = RaiseManager::new();
        raise_manager.input_tx = input_tx;
        let mut timeout_timer = Timer::manual();

        let sequence_timeout = |sequence: &ActiveSequence| {
            if !sequence.timed_out {
                let elapsed = sequence.started_at.elapsed();
                TIMEOUT_DURATION.saturating_sub(elapsed)
            } else {
                Duration::MAX
            }
        };

        loop {
            // Calculate next timeout timer if we have an active sequence.
            let timeout = if let Some(sequence) = &raise_manager.active_sequence {
                sequence_timeout(sequence)
            } else {
                Duration::MAX
            };
            timeout_timer.set_next_fire(timeout);

            tokio::select! {
                // Handle messages from reactor
                Some((span, msg)) = rx.recv() => {
                    let _guard = span.enter();
                    raise_manager.handle_message(msg);
                }

                // Handle timeout - send timeout event to reactor
                _ = timeout_timer.next() => {
                    // Send timeout event for the active sequence
                    if let Some(sequence) = &mut raise_manager.active_sequence {
                        if sequence_timeout(sequence) <= Duration::ZERO {
                            // Send timeout event to reactor; this will get
                            // relayed back to us. We send these events through
                            // the reactor so that we can record/replay them.
                            sequence.timed_out = true;
                            events_tx.send(reactor::Event::RaiseTimeout { sequence_id: sequence.sequence_id });
                        }
                    }
                }
            }
        }
    }

    fn new() -> Self {
        Self {
            active_sequence: None,
            queued_sequences: VecDeque::new(),
            next_sequence_id: 1,
            input_tx: None,
        }
    }

    fn handle_message(&mut self, msg: Event) {
        match msg {
            Event::RaiseRequest(RaiseRequest {
                raise_windows,
                focus_window,
                restack_windows,
                app_handles,
                focus_quiet,
            }) => {
                debug!(
                    "Processing layout response with {} raise_windows, {} restack_windows",
                    raise_windows.len(),
                    restack_windows.len()
                );

                // Always queue the sequence
                self.queued_sequences.push_back(RaiseRequest {
                    raise_windows,
                    focus_window,
                    restack_windows,
                    app_handles,
                    focus_quiet,
                });
            }
            Event::RaiseCompleted { window_id, sequence_id } => {
                trace!("Raise completed for {:?} in sequence {}", window_id, sequence_id);

                // Remove this window from the active sequence's pending raises
                if let Some(sequence) = &mut self.active_sequence {
                    if sequence.sequence_id == sequence_id {
                        sequence.pending_raises.remove(&window_id);
                    }
                }
            }
            Event::RaiseTimeout { sequence_id } => {
                trace!("Raise sequence {} timed out", sequence_id);

                // Clear pending raises for the active sequence if it matches
                if let Some(sequence) = &mut self.active_sequence {
                    if sequence.sequence_id == sequence_id {
                        warn!(
                            "Sequence {} timed out, clearing pending raises: {:?}",
                            sequence_id, sequence.pending_raises
                        );
                        sequence.pending_raises.clear();
                        // Restacking is best effort: once the focus window is
                        // raised (or there was none), a timeout drops the floats
                        // still to raise rather than spending another timeout on
                        // each of them.
                        if sequence.focus_batch.is_none() {
                            sequence.restack.clear();
                        }
                        // Cancel the raises that hung, then install a fresh
                        // token. The focus raise is sent next (in
                        // process_active_sequence) and must use a live token, or
                        // the app rejects it as cancelled and never reports
                        // completion, blocking the manager permanently.
                        sequence.raise_token.cancel();
                        sequence.raise_token = CancellationToken::new();
                        // Restart the timeout window so the focus phase gets its
                        // own timeout. Otherwise a hung focus raise could never
                        // time out again and would block all future raises.
                        sequence.started_at = Instant::now();
                        sequence.timed_out = false;
                    }
                }
            }
        }

        // Process sequences until no more progress can be made (fixed-point iteration)
        loop {
            let mut changed = false;
            changed |= self.process_active_sequence();
            changed |= self.process_queued_responses();
            if !changed {
                break;
            }
        }
    }

    fn process_queued_responses(&mut self) -> bool {
        if self.active_sequence.is_none() {
            if let Some(queued) = self.queued_sequences.pop_front() {
                self.start_new_sequence(queued);
                return true;
            }
        }
        false
    }

    fn start_new_sequence(
        &mut self,
        RaiseRequest {
            raise_windows,
            focus_window,
            restack_windows,
            app_handles,
            focus_quiet,
        }: RaiseRequest,
    ) {
        let sequence_id = self.next_sequence_id;
        self.next_sequence_id += 1;

        // Send all raise requests with completion notification
        let mut pending_raises = HashSet::default();
        let raise_token = CancellationToken::new();

        let mut focus_batch = None;
        for mut wids in raise_windows {
            let Some(&WindowId { pid, .. }) = wids.first() else {
                continue;
            };
            if let Some(focus_idx) =
                wids.iter().position(|wid| matches!(focus_window, Some((w, _)) if w == *wid))
            {
                // Make sure focus window is always last.
                let last = wids.len() - 1;
                wids.swap(focus_idx, last);
                if focus_batch.is_none() {
                    focus_batch = Some((pid, wids, focus_window.unwrap().1, focus_quiet));
                    continue;
                }
            }
            let Some(app_handle) = app_handles.get(&pid) else {
                warn!("App not found for pid {:?}", pid);
                continue;
            };
            if app_handle
                .send(Request::Raise(
                    wids.clone(),
                    raise_token.clone(),
                    sequence_id,
                    Quiet::Yes,
                ))
                .is_ok()
            {
                pending_raises.extend(wids);
            }
        }
        if let Some((wid, warp)) = focus_window
            && focus_batch.is_none()
        {
            focus_batch = Some((wid.pid, vec![wid], warp, focus_quiet));
        }

        // Consecutive windows of one app form a batch (one request, raised in order).
        let mut restack: VecDeque<Vec<WindowId>> = VecDeque::new();
        for wid in restack_windows {
            match restack.back_mut() {
                Some(batch) if batch[0].pid == wid.pid => batch.push(wid),
                _ => restack.push_back(vec![wid]),
            }
        }

        if !pending_raises.is_empty() || focus_batch.is_some() || !restack.is_empty() {
            self.active_sequence = Some(ActiveSequence {
                sequence_id,
                pending_raises,
                focus_batch,
                restack,
                app_handles,
                raise_token,
                started_at: Instant::now(),
                timed_out: false,
            });
        }
    }

    /// Process the active sequence, handling completed raises and focus windows.
    pub fn process_active_sequence(&mut self) -> bool {
        let Some(sequence) = &mut self.active_sequence else {
            return false;
        };
        let mut changed = false;

        // If all regular raises are complete but we have a focus window, send
        // the focus request.
        if sequence.pending_raises.is_empty()
            && let Some((pid, wids, warp, quiet)) = sequence.focus_batch.take()
        {
            changed = true;
            debug!(focus_window = ?wids);
            let app_handle = sequence.app_handles.get(&pid);
            if let Some(handle) = app_handle {
                if handle
                    .send(Request::Raise(
                        wids.clone(),
                        sequence.raise_token.clone(),
                        sequence.sequence_id, // Use proper sequence ID for tracking
                        quiet,
                    ))
                    .is_ok()
                {
                    // Add focus window to pending raises so we wait for completion.
                    sequence.pending_raises.extend(wids);
                    trace!("Focus window request sent and added to pending raises");
                } else {
                    warn!("Failed to send focus window request");
                }

                if let Some(warp) = warp
                    && let Some(input_tx) = &self.input_tx
                {
                    // For now we don't wait for the last window to be raised;
                    // send the warp as soon as we send the raise request.
                    _ = input_tx.send(input::Request::Warp(warp));
                }
            }
        }

        // With the focus window raised, put the floats back above it: one app's
        // batch at a time, the next only once the previous one completed, so the
        // bottom-to-top order holds across apps.
        while sequence.pending_raises.is_empty()
            && sequence.focus_batch.is_none()
            && let Some(wids) = sequence.restack.pop_front()
        {
            changed = true;
            let Some(pid) = wids.first().map(|wid| wid.pid) else {
                continue;
            };
            let Some(handle) = sequence.app_handles.get(&pid) else {
                warn!("App not found for restack pid {:?}", pid);
                continue;
            };
            debug!(restack_windows = ?wids);
            if handle
                .send(Request::Restack(
                    wids.clone(),
                    sequence.raise_token.clone(),
                    sequence.sequence_id,
                ))
                .is_ok()
            {
                sequence.pending_raises.extend(wids);
            } else {
                warn!("Failed to send restack request");
            }
        }

        // If all raises (including focus and restacks) are complete, remove the
        // active sequence.
        if sequence.pending_raises.is_empty()
            && sequence.focus_batch.is_none()
            && sequence.restack.is_empty()
        {
            trace!(
                "Raise sequence completed after {:?}",
                sequence.started_at.elapsed(),
            );
            self.active_sequence = None;
            changed = true;
        }

        changed
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::actor;
    use crate::actor::app::{AppThreadHandle, WindowId};
    use crate::sys::executor::Executor;

    fn create_test_app_handles() -> (HashMap<i32, AppThreadHandle>, actor::Receiver<Request>) {
        let mut app_handles = HashMap::default();
        let (app_tx, app_rx) = actor::channel();
        let app_handle = AppThreadHandle::new_for_test(app_tx);
        app_handles.insert(1, app_handle);
        (app_handles, app_rx)
    }

    fn create_layout_response(
        raise_windows: Vec<WindowId>,
        focus_window: Option<(WindowId, Option<CGPoint>)>,
        app_handles: HashMap<i32, AppThreadHandle>,
        focus_quiet: Quiet,
    ) -> Event {
        Event::RaiseRequest(RaiseRequest {
            raise_windows: raise_windows.into_iter().map(|w| vec![w]).collect(),
            focus_window,
            restack_windows: vec![],
            app_handles,
            focus_quiet,
        })
    }

    fn restack_request(request: &Request) -> Option<(&Vec<WindowId>, u64)> {
        match request {
            Request::Restack(wids, _, seq_id) => Some((wids, *seq_id)),
            _ => None,
        }
    }

    fn collect_requests(app_rx: &mut actor::Receiver<Request>) -> Vec<Request> {
        let mut requests = Vec::new();
        while let Ok((_, request)) = app_rx.try_recv() {
            requests.push(request);
        }
        requests
    }

    #[track_caller]
    fn assert_raise_request(
        request: &Request,
        expected_wid: WindowId,
        expected_seq_id: u64,
        expected_quiet: Quiet,
    ) {
        if let Request::Raise(wid, _, seq_id, quiet) = request {
            assert_eq!(*wid, vec![expected_wid]);
            assert_eq!(*seq_id, expected_seq_id);
            assert_eq!(*quiet, expected_quiet);
        } else {
            panic!("Expected raise request, got: {:?}", request);
        }
    }

    fn find_raise_request(requests: &[Request], expected_wid: WindowId) -> bool {
        requests.iter().any(|r| {
            if let Request::Raise(wid, _, _, quiet) = r {
                *wid == vec![expected_wid] && *quiet == Quiet::No
            } else {
                false
            }
        })
    }

    #[test]
    fn test_raise_manager_handles_layout_response() {
        Executor::run(async {
            let mut raise_manager = RaiseManager::new();
            let (app_handles, _app_rx) = create_test_app_handles();

            let msg = create_layout_response(
                vec![WindowId::new(1, 1), WindowId::new(1, 2)],
                Some((WindowId::new(1, 3), None)),
                app_handles,
                Quiet::No,
            );

            // Handle the message synchronously
            raise_manager.handle_message(msg);
            // Verify that an active sequence was created
            assert!(raise_manager.active_sequence.is_some());
            let sequence = raise_manager.active_sequence.as_ref().unwrap();
            assert_eq!(sequence.sequence_id, 1);
            assert_eq!(sequence.pending_raises.len(), 2);
            assert!(sequence.focus_batch.is_some());
        });
    }

    #[test]
    fn test_raise_completion_removes_pending_window() {
        Executor::run(async {
            let mut raise_manager = RaiseManager::new();
            let (app_handles, _app_rx) = create_test_app_handles();

            let layout_msg = create_layout_response(
                vec![WindowId::new(1, 1), WindowId::new(1, 2)],
                Some((WindowId::new(1, 3), None)),
                app_handles,
                Quiet::No,
            );

            raise_manager.handle_message(layout_msg);

            // Verify initial state
            assert!(raise_manager.active_sequence.is_some());
            assert_eq!(
                raise_manager.active_sequence.as_ref().unwrap().pending_raises.len(),
                2
            );

            // Send completion for one window
            let completion_msg = Event::RaiseCompleted {
                window_id: WindowId::new(1, 1),
                sequence_id: 1,
            };

            raise_manager.handle_message(completion_msg);

            // Verify that one window was removed from pending
            assert!(raise_manager.active_sequence.is_some());
            let sequence = raise_manager.active_sequence.as_ref().unwrap();
            assert_eq!(sequence.pending_raises.len(), 1);
            assert!(!sequence.pending_raises.contains(&WindowId::new(1, 1)));
            assert!(sequence.pending_raises.contains(&WindowId::new(1, 2)));
        });
    }

    #[test]
    fn test_timeout_clears_pending_raises() {
        Executor::run(async {
            let mut raise_manager = RaiseManager::new();
            let (app_handles, _app_rx) = create_test_app_handles();

            let layout_msg =
                create_layout_response(vec![WindowId::new(1, 1)], None, app_handles, Quiet::No);

            raise_manager.handle_message(layout_msg);

            // Verify initial state
            assert!(raise_manager.active_sequence.is_some());
            assert_eq!(
                raise_manager.active_sequence.as_ref().unwrap().pending_raises.len(),
                1
            );

            // Send timeout directly
            raise_manager.handle_message(Event::RaiseTimeout { sequence_id: 1 });

            // Verify that the sequence was completed
            assert!(raise_manager.active_sequence.is_none());
        });
    }

    #[test]
    fn test_all_raises_complete_triggers_focus() {
        Executor::run(async {
            let mut raise_manager = RaiseManager::new();
            let (input_tx, mut input_rx) = actor::channel();
            raise_manager.input_tx = Some(input_tx);

            let (app_handles, mut app_rx) = create_test_app_handles();

            let layout_msg = create_layout_response(
                vec![WindowId::new(1, 1), WindowId::new(1, 2)],
                Some((WindowId::new(1, 3), Some(CGPoint::new(100.0, 200.0)))),
                app_handles,
                Quiet::No,
            );

            raise_manager.handle_message(layout_msg);

            // Verify initial state
            assert!(raise_manager.active_sequence.is_some());
            assert_eq!(
                raise_manager.active_sequence.as_ref().unwrap().pending_raises.len(),
                2
            );

            // Send completions for both raise windows
            raise_manager.handle_message(Event::RaiseCompleted {
                window_id: WindowId::new(1, 1),
                sequence_id: 1,
            });

            raise_manager.handle_message(Event::RaiseCompleted {
                window_id: WindowId::new(1, 2),
                sequence_id: 1,
            });

            // Check that focus window request was sent
            let requests = collect_requests(&mut app_rx);
            assert!(
                find_raise_request(&requests, WindowId::new(1, 3)),
                "Focus window request should have been sent"
            );

            // Check that warp request was sent
            let warp_request = input_rx.try_recv().expect("Warp request should have been sent");
            let input::Request::Warp(warp) = warp_request.1 else {
                panic!("Unexpected mouse request sent: {:?}", warp_request.1)
            };
            assert_eq!(warp, CGPoint::new(100.0, 200.0));

            // Complete the focus window raise
            raise_manager.handle_message(Event::RaiseCompleted {
                window_id: WindowId::new(1, 3),
                sequence_id: 1,
            });

            // Verify that the sequence was completed and removed
            assert!(raise_manager.active_sequence.is_none());
        });
    }

    #[test]
    fn test_timeout_clears_pending_and_triggers_focus() {
        Executor::run(async {
            let mut raise_manager = RaiseManager::new();
            let (app_handles, mut app_rx) = create_test_app_handles();

            let layout_msg = create_layout_response(
                vec![WindowId::new(1, 1), WindowId::new(1, 2)],
                Some((WindowId::new(1, 3), None)),
                app_handles,
                Quiet::No,
            );

            raise_manager.handle_message(layout_msg);

            // Verify initial state
            assert!(raise_manager.active_sequence.is_some());
            assert_eq!(
                raise_manager.active_sequence.as_ref().unwrap().pending_raises.len(),
                2
            );

            // Send timeout for the sequence
            raise_manager.handle_message(Event::RaiseTimeout { sequence_id: 1 });

            // Check that focus window request was sent after timeout
            let requests = collect_requests(&mut app_rx);
            assert!(
                find_raise_request(&requests, WindowId::new(1, 3)),
                "Focus window request should have been sent after timeout"
            );

            // Complete the focus window raise
            raise_manager.handle_message(Event::RaiseCompleted {
                window_id: WindowId::new(1, 3),
                sequence_id: 1,
            });

            // Verify that the sequence was completed and removed
            assert!(raise_manager.active_sequence.is_none());
        });
    }

    #[test]
    fn test_multiple_layout_responses_wait_for_focus_completion() {
        Executor::run(async {
            let mut raise_manager = RaiseManager::new();
            let (app_handles, mut app_rx) = create_test_app_handles();

            // Send two layout responses - second should be queued
            let msg1 = create_layout_response(
                vec![WindowId::new(1, 1)],
                Some((WindowId::new(1, 2), None)),
                app_handles.clone(),
                Quiet::No,
            );
            let msg2 = create_layout_response(
                vec![WindowId::new(1, 3)],
                Some((WindowId::new(1, 4), None)),
                app_handles.clone(),
                Quiet::No,
            );

            raise_manager.handle_message(msg1);
            raise_manager.handle_message(msg2);

            // Verify sequential processing: first active, second queued
            assert!(raise_manager.active_sequence.is_some());
            assert_eq!(raise_manager.active_sequence.as_ref().unwrap().sequence_id, 1);
            assert_eq!(raise_manager.queued_sequences.len(), 1);

            // Complete first sequence's regular raise
            raise_manager.handle_message(Event::RaiseCompleted {
                window_id: WindowId::new(1, 1),
                sequence_id: 1,
            });

            // Verify first sequence now has focus pending, second still queued
            let sequence = raise_manager.active_sequence.as_ref().unwrap();
            assert_eq!(sequence.pending_raises.len(), 1);
            assert!(sequence.pending_raises.contains(&WindowId::new(1, 2)));
            assert_eq!(raise_manager.queued_sequences.len(), 1);

            // Verify only first sequence requests sent (regular + focus)
            let requests = collect_requests(&mut app_rx);
            assert_eq!(requests.len(), 2);
            assert_raise_request(&requests[0], WindowId::new(1, 1), 1, Quiet::Yes);
            assert_raise_request(&requests[1], WindowId::new(1, 2), 1, Quiet::No);

            // Complete first sequence's focus window
            raise_manager.handle_message(Event::RaiseCompleted {
                window_id: WindowId::new(1, 2),
                sequence_id: 1,
            });

            // Verify second sequence now active
            let sequence = raise_manager.active_sequence.as_ref().unwrap();
            assert_eq!(sequence.sequence_id, 2);
            assert_eq!(sequence.pending_raises.len(), 1);
            assert!(sequence.pending_raises.contains(&WindowId::new(1, 3)));
            assert_eq!(raise_manager.queued_sequences.len(), 0);

            // Verify second sequence's regular raise sent
            let requests = collect_requests(&mut app_rx);
            assert_eq!(requests.len(), 1);
            assert_raise_request(&requests[0], WindowId::new(1, 3), 2, Quiet::Yes);

            // Complete second sequence's regular raise
            raise_manager.handle_message(Event::RaiseCompleted {
                window_id: WindowId::new(1, 3),
                sequence_id: 2,
            });

            // Verify second sequence's focus window sent
            let requests = collect_requests(&mut app_rx);
            assert_eq!(requests.len(), 1);
            assert_raise_request(&requests[0], WindowId::new(1, 4), 2, Quiet::No);

            // Complete second sequence's focus window
            raise_manager.handle_message(Event::RaiseCompleted {
                window_id: WindowId::new(1, 4),
                sequence_id: 2,
            });

            // Verify all sequences completed
            assert!(raise_manager.active_sequence.is_none());
            assert!(collect_requests(&mut app_rx).is_empty());
        });
    }

    #[test]
    fn test_multiple_iterations_required_for_chained_completions() {
        Executor::run(async {
            let mut raise_manager = RaiseManager::new();
            let (app_handles, mut app_rx) = create_test_app_handles();

            // Send three layout responses:
            // 1. First has one regular raise + focus
            // 2. Second has no regular raises, only focus (will start immediately when first completes)
            // 3. Third has one regular raise + focus
            let msg1 = create_layout_response(
                vec![WindowId::new(1, 1)],
                Some((WindowId::new(1, 2), None)),
                app_handles.clone(),
                Quiet::No,
            );
            let msg2 = create_layout_response(
                vec![],
                Some((WindowId::new(1, 3), None)),
                app_handles.clone(),
                Quiet::No,
            );
            let msg3 = create_layout_response(
                vec![WindowId::new(1, 4)],
                Some((WindowId::new(1, 5), None)),
                app_handles.clone(),
                Quiet::No,
            );

            // Queue all three sequences
            raise_manager.handle_message(msg1);
            raise_manager.handle_message(msg2);
            raise_manager.handle_message(msg3);

            // Verify first sequence is active, others queued
            assert!(raise_manager.active_sequence.is_some());
            assert_eq!(raise_manager.active_sequence.as_ref().unwrap().sequence_id, 1);
            assert_eq!(raise_manager.queued_sequences.len(), 2);

            // Complete first sequence's regular raise
            raise_manager.handle_message(Event::RaiseCompleted {
                window_id: WindowId::new(1, 1),
                sequence_id: 1,
            });

            // First sequence should now have focus pending
            assert!(raise_manager.active_sequence.is_some());
            assert_eq!(raise_manager.active_sequence.as_ref().unwrap().sequence_id, 1);
            assert!(
                raise_manager
                    .active_sequence
                    .as_ref()
                    .unwrap()
                    .pending_raises
                    .contains(&WindowId::new(1, 2))
            );

            // Complete first sequence's focus window - this should trigger multiple iterations:
            // 1. First sequence completes
            // 2. Second sequence starts (no regular raises, immediately sends focus)
            // 3. Focus window is sent and tracked
            // Without multiple iterations, the second sequence's focus wouldn't be sent
            raise_manager.handle_message(Event::RaiseCompleted {
                window_id: WindowId::new(1, 2),
                sequence_id: 1,
            });

            // After the completion, we should have:
            // - Second sequence active with focus window pending
            // - Third sequence still queued
            // - Focus request for second sequence should have been sent
            assert!(raise_manager.active_sequence.is_some());
            assert_eq!(raise_manager.active_sequence.as_ref().unwrap().sequence_id, 2);
            assert!(
                raise_manager
                    .active_sequence
                    .as_ref()
                    .unwrap()
                    .pending_raises
                    .contains(&WindowId::new(1, 3))
            );
            assert_eq!(raise_manager.queued_sequences.len(), 1);

            // Verify the focus request for second sequence was sent
            // This would fail with single iteration because the second sequence
            // wouldn't have a chance to send its focus request
            let requests = collect_requests(&mut app_rx);
            let second_focus_sent = requests.iter().any(|r| {
                if let Request::Raise(wid, _, seq_id, quiet) = r {
                    *wid == vec![WindowId::new(1, 3)] && *seq_id == 2 && *quiet == Quiet::No
                } else {
                    false
                }
            });
            assert!(
                second_focus_sent,
                "Second sequence's focus request should have been sent immediately after first sequence completed"
            );
        });
    }

    #[test]
    fn test_concurrent_window_raising_with_batched_windows() {
        // Test that the raise manager correctly handles batched windows
        // by sending multiple windows from the same app/screen as a single request
        Executor::run(async {
            let mut raise_manager = RaiseManager::new();
            let (app_handles, mut app_rx) = create_test_app_handles();

            // Create a raise request with batched windows from the same app
            let batched_windows = vec![
                // First batch: Contains the focus window, so it should be
                // processed last.
                vec![
                    WindowId::new(1, 1),
                    WindowId::new(1, 7), // focus window
                    WindowId::new(1, 2),
                ],
                // Second batch
                vec![WindowId::new(1, 3), WindowId::new(1, 4)],
                // Third batch
                vec![WindowId::new(1, 5), WindowId::new(1, 6)],
            ];
            let raise_request = Event::RaiseRequest(RaiseRequest {
                raise_windows: batched_windows,
                focus_window: Some((WindowId::new(1, 7), None)),
                restack_windows: vec![],
                app_handles,
                focus_quiet: Quiet::No,
            });

            // Handle the batched raise request
            raise_manager.handle_message(raise_request);

            // Verify that both batches were sent as separate requests
            let requests = collect_requests(&mut app_rx);

            // Verify second and third batches are processed first.
            if let Request::Raise(wids, _, seq_id, quiet) = &requests[0] {
                assert_eq!(*wids, vec![WindowId::new(1, 3), WindowId::new(1, 4)]);
                assert_eq!(*seq_id, 1);
                assert_eq!(*quiet, Quiet::Yes);
            } else {
                panic!("Expected Raise request for second batch");
            }
            if let Request::Raise(wids, _, seq_id, quiet) = &requests[1] {
                assert_eq!(*wids, vec![WindowId::new(1, 5), WindowId::new(1, 6)]);
                assert_eq!(*seq_id, 1);
                assert_eq!(*quiet, Quiet::Yes);
            } else {
                panic!("Expected Raise request for third batch");
            }

            // Complete all raises from both batches.
            for wid in [
                WindowId::new(1, 3),
                WindowId::new(1, 4),
                WindowId::new(1, 5),
                WindowId::new(1, 6),
            ] {
                let completion_msg = Event::RaiseCompleted { window_id: wid, sequence_id: 1 };
                raise_manager.handle_message(completion_msg);
            }

            // Process the sequence to trigger focus batch.
            raise_manager.process_active_sequence();
            let requests = collect_requests(&mut app_rx);

            // Verify first batch is processed last.
            // The focus_window should have been moved to the end.
            assert_eq!(requests.len(), 1);
            if let Request::Raise(wids, _, seq_id, quiet) = &requests[0] {
                assert_eq!(*wids, vec![
                    WindowId::new(1, 1),
                    WindowId::new(1, 2),
                    WindowId::new(1, 7),
                ]);
                assert_eq!(*seq_id, 1);
                assert_eq!(*quiet, Quiet::No);
            } else {
                panic!("Expected Raise request for first batch");
            }
        });
    }

    /// Two apps (pids 1 and 2) sharing one request channel.
    fn create_two_app_handles() -> (HashMap<i32, AppThreadHandle>, actor::Receiver<Request>) {
        let (mut app_handles, app_rx) = create_test_app_handles();
        let handle = app_handles[&1].clone();
        app_handles.insert(2, handle);
        (app_handles, app_rx)
    }

    #[track_caller]
    fn assert_restack_request(request: &Request, expected: &[WindowId], expected_seq_id: u64) {
        let (wids, seq_id) = restack_request(request)
            .unwrap_or_else(|| panic!("Expected restack request, got: {:?}", request));
        assert_eq!(wids.as_slice(), expected);
        assert_eq!(seq_id, expected_seq_id);
    }

    /// Floats are restacked only after the focus window completed, bottom to top, one
    /// app batch at a time (the next batch waits for the previous one), as plain
    /// restack requests (no quiet/focus raise), and the sequence ends after the last.
    #[test]
    fn test_restack_runs_after_focus_in_order_one_app_at_a_time() {
        Executor::run(async {
            let mut raise_manager = RaiseManager::new();
            let (app_handles, mut app_rx) = create_two_app_handles();
            let (w11, w12, w13, w14) = (
                WindowId::new(1, 1),
                WindowId::new(1, 2),
                WindowId::new(1, 3),
                WindowId::new(1, 4),
            );
            let (w21, w22) = (WindowId::new(2, 1), WindowId::new(2, 2));

            raise_manager.handle_message(Event::RaiseRequest(RaiseRequest {
                raise_windows: vec![vec![w11]],
                focus_window: Some((w12, None)),
                restack_windows: vec![w13, w21, w22, w14],
                app_handles,
                focus_quiet: Quiet::Yes,
            }));

            // Regular raise first; nothing else until it completes.
            let requests = collect_requests(&mut app_rx);
            assert_eq!(requests.len(), 1);
            assert_raise_request(&requests[0], w11, 1, Quiet::Yes);
            raise_manager.handle_message(Event::RaiseCompleted { window_id: w11, sequence_id: 1 });

            // Then the focus window, still no restack.
            let requests = collect_requests(&mut app_rx);
            assert_eq!(requests.len(), 1);
            assert_raise_request(&requests[0], w12, 1, Quiet::Yes);
            raise_manager.handle_message(Event::RaiseCompleted { window_id: w12, sequence_id: 1 });

            // First restack batch: app 1's leading window alone (app 2 follows it).
            let requests = collect_requests(&mut app_rx);
            assert_eq!(requests.len(), 1);
            assert_restack_request(&requests[0], &[w13], 1);
            assert_eq!(raise_manager.active_sequence.as_ref().unwrap().restack.len(), 2);
            raise_manager.handle_message(Event::RaiseCompleted { window_id: w13, sequence_id: 1 });

            // App 2's two windows as one batch, in order.
            let requests = collect_requests(&mut app_rx);
            assert_eq!(requests.len(), 1);
            assert_restack_request(&requests[0], &[w21, w22], 1);
            raise_manager.handle_message(Event::RaiseCompleted { window_id: w21, sequence_id: 1 });
            assert!(
                collect_requests(&mut app_rx).is_empty(),
                "the next batch waits for the whole previous batch"
            );
            raise_manager.handle_message(Event::RaiseCompleted { window_id: w22, sequence_id: 1 });

            // App 1's trailing window last, then the sequence is done.
            let requests = collect_requests(&mut app_rx);
            assert_eq!(requests.len(), 1);
            assert_restack_request(&requests[0], &[w14], 1);
            assert!(raise_manager.active_sequence.is_some());
            raise_manager.handle_message(Event::RaiseCompleted { window_id: w14, sequence_id: 1 });
            assert!(raise_manager.active_sequence.is_none());
            assert!(collect_requests(&mut app_rx).is_empty());
        });
    }

    /// A request carrying only restacks (a focus change macOS reported) is a sequence of
    /// its own: no raise, no focus, just the restack batches.
    #[test]
    fn test_restack_only_request_runs_without_focus() {
        Executor::run(async {
            let mut raise_manager = RaiseManager::new();
            let (app_handles, mut app_rx) = create_test_app_handles();
            let (w1, w2) = (WindowId::new(1, 1), WindowId::new(1, 2));

            raise_manager.handle_message(Event::RaiseRequest(RaiseRequest {
                raise_windows: vec![],
                focus_window: None,
                restack_windows: vec![w1, w2],
                app_handles,
                focus_quiet: Quiet::Yes,
            }));

            let requests = collect_requests(&mut app_rx);
            assert_eq!(requests.len(), 1);
            assert_restack_request(&requests[0], &[w1, w2], 1);
            assert!(raise_manager.active_sequence.is_some());
            raise_manager.handle_message(Event::RaiseCompleted { window_id: w1, sequence_id: 1 });
            raise_manager.handle_message(Event::RaiseCompleted { window_id: w2, sequence_id: 1 });
            assert!(raise_manager.active_sequence.is_none());
        });
    }

    /// A timeout while restacking drops the floats still to raise (best effort) instead of
    /// spending another timeout on each; a timeout before the focus raise keeps them.
    #[test]
    fn test_timeout_drops_remaining_restacks_only_after_focus() {
        Executor::run(async {
            // Before the focus raise: the restacks survive the timeout.
            let mut raise_manager = RaiseManager::new();
            let (app_handles, mut app_rx) = create_two_app_handles();
            let (w11, w12, w13, w21) = (
                WindowId::new(1, 1),
                WindowId::new(1, 2),
                WindowId::new(1, 3),
                WindowId::new(2, 1),
            );
            raise_manager.handle_message(Event::RaiseRequest(RaiseRequest {
                raise_windows: vec![vec![w11]],
                focus_window: Some((w12, None)),
                restack_windows: vec![w13, w21],
                app_handles,
                focus_quiet: Quiet::No,
            }));
            let _ = collect_requests(&mut app_rx);
            raise_manager.handle_message(Event::RaiseTimeout { sequence_id: 1 });
            let requests = collect_requests(&mut app_rx);
            assert_eq!(requests.len(), 1);
            assert_raise_request(&requests[0], w12, 1, Quiet::No);
            raise_manager.handle_message(Event::RaiseCompleted { window_id: w12, sequence_id: 1 });
            let requests = collect_requests(&mut app_rx);
            assert_eq!(requests.len(), 1);
            assert_restack_request(&requests[0], &[w13], 1);

            // During the restacks: the rest is dropped and the sequence ends.
            raise_manager.handle_message(Event::RaiseTimeout { sequence_id: 1 });
            assert!(raise_manager.active_sequence.is_none());
            assert!(collect_requests(&mut app_rx).is_empty());
        });
    }
}
