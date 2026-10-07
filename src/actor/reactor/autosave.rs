//! Debounced saves of the layout file while rift runs (`settings.persistence.autosave`).
//!
//! The reactor notes every event that may have changed the layout. The first change arms a
//! save `autosave_debounce_ms` later; each further change pushes it back by the same amount,
//! up to [`MAX_DELAY_FACTOR`] debounces after the first one, so a burst of changes is saved
//! once, shortly after it ends, and a never-ending stream still gets saved.
//!
//! When the save is due, the reactor only serializes the layout (the part that needs its
//! state). The `rift-autosave` thread wakes the reactor at the due time and writes the
//! snapshot atomically, so file I/O and fsync never run on the event loop. A snapshot equal
//! to the last one written is not written again.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use super::{Event, Reactor, Sender};
use crate::common::config::PersistenceSettings;
use crate::layout_engine::{LayoutSnapshot, write_layout_snapshot};

/// A save is due at most this many debounce intervals after the first unsaved change.
const MAX_DELAY_FACTOR: u32 = 5;

/// Hash of the empty snapshot slot: nothing written yet.
const NOTHING_WRITTEN: u64 = 0;

enum Job {
    /// Send [`Event::AutosaveDue`] to the reactor at this instant (or now, if it has passed).
    Wake(Instant),
    /// Write a snapshot; `hash` identifies its contents.
    Write {
        path: PathBuf,
        snapshot: LayoutSnapshot,
        hash: u64,
    },
}

pub(crate) struct Autosave {
    enabled: bool,
    debounce: Duration,
    path: PathBuf,
    /// The first change not saved yet, and when its save is due.
    first_change: Option<Instant>,
    due: Option<Instant>,
    /// Whether the worker has been asked to wake the reactor (one wake at a time).
    wake_requested: bool,
    /// Where wakes go. `None` in tests: they drive time, and snapshots are written inline.
    reactor_tx: Option<Sender>,
    jobs: Option<mpsc::Sender<Job>>,
    /// Hash of the last snapshot written, by whichever thread wrote it.
    written: Arc<AtomicU64>,
    #[cfg(test)]
    pub(crate) writes: usize,
    #[cfg(test)]
    pub(crate) failures: usize,
}

impl Autosave {
    pub(crate) fn new(settings: &PersistenceSettings, path: PathBuf) -> Self {
        let mut autosave = Self {
            enabled: false,
            debounce: Duration::ZERO,
            path,
            first_change: None,
            due: None,
            wake_requested: false,
            reactor_tx: None,
            jobs: None,
            written: Arc::new(AtomicU64::new(NOTHING_WRITTEN)),
            #[cfg(test)]
            writes: 0,
            #[cfg(test)]
            failures: 0,
        };
        autosave.configure(settings, Instant::now());
        autosave
    }

    /// Wake the reactor through `tx` and write on a background thread.
    pub(crate) fn run_in_background(&mut self, tx: Sender) {
        self.reactor_tx = Some(tx);
        if let Some(due) = self.due {
            self.request_wake(due);
        }
    }

    /// Apply new settings. Turning autosave on saves the current layout soon.
    pub(crate) fn configure(&mut self, settings: &PersistenceSettings, now: Instant) {
        let was_enabled = self.enabled;
        self.enabled = settings.autosave;
        self.debounce = Duration::from_millis(
            settings.autosave_debounce_ms.max(PersistenceSettings::MIN_AUTOSAVE_DEBOUNCE_MS),
        );
        if !self.enabled {
            self.first_change = None;
            self.due = None;
        } else if !was_enabled {
            self.note_change(now);
        }
    }

    #[cfg(test)]
    pub(crate) fn due(&self) -> Option<Instant> { self.due }

    /// The layout may have changed at `now`: (re)arm the debounced save.
    pub(crate) fn note_change(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        let first_change = *self.first_change.get_or_insert(now);
        let latest = first_change + self.debounce * MAX_DELAY_FACTOR;
        let due = (now + self.debounce).min(latest);
        self.due = Some(due);
        self.request_wake(due);
    }

    /// Called when a wake arrives. True when the save is due now; otherwise the wake was
    /// early (later changes pushed the save back) and another one is requested.
    pub(crate) fn take_due(&mut self, now: Instant) -> bool {
        self.wake_requested = false;
        match self.due {
            None => false,
            Some(due) if now < due => {
                self.request_wake(due);
                false
            }
            Some(_) => {
                self.first_change = None;
                self.due = None;
                true
            }
        }
    }

    /// The save was due, but now is a bad time for it: try again one debounce later.
    pub(crate) fn postpone(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        self.first_change.get_or_insert(now);
        let due = now + self.debounce;
        self.due = Some(due);
        self.request_wake(due);
    }

    /// Write `snapshot` unless it equals the last one written.
    pub(crate) fn save(&mut self, snapshot: LayoutSnapshot) {
        let hash = content_hash(snapshot.contents());
        if hash == self.written.load(Ordering::Acquire) {
            debug!(path = %self.path.display(), "Layout unchanged since the last autosave");
            return;
        }
        if self.reactor_tx.is_some() {
            let path = self.path.clone();
            if let Some(jobs) = self.worker() {
                if jobs.send(Job::Write { path, snapshot, hash }).is_err() {
                    warn!("Autosave thread is gone; layout not saved");
                }
            }
            return;
        }
        // No reactor to wake (tests): write inline.
        let ok = write_snapshot(&self.path, &snapshot, hash, &self.written);
        #[cfg(test)]
        {
            self.writes += usize::from(ok);
            self.failures += usize::from(!ok);
        }
        #[cfg(not(test))]
        let _ = ok;
    }

    fn request_wake(&mut self, at: Instant) {
        if self.wake_requested || self.reactor_tx.is_none() {
            return;
        }
        let Some(jobs) = self.worker() else { return };
        self.wake_requested = jobs.send(Job::Wake(at)).is_ok();
    }

    /// The worker thread's queue, starting the thread on first use.
    fn worker(&mut self) -> Option<&mpsc::Sender<Job>> {
        if self.jobs.is_none() {
            let reactor_tx = self.reactor_tx.clone()?;
            let (jobs_tx, jobs_rx) = mpsc::channel();
            let written = self.written.clone();
            let spawned =
                std::thread::Builder::new().name("rift-autosave".into()).spawn(move || {
                    run_worker(jobs_rx, move || reactor_tx.send(Event::AutosaveDue), &written)
                });
            match spawned {
                Ok(_) => self.jobs = Some(jobs_tx),
                Err(error) => {
                    warn!(%error, "Could not start the autosave thread; autosave is off");
                    self.enabled = false;
                    return None;
                }
            }
        }
        self.jobs.as_ref()
    }
}

fn content_hash(contents: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    contents.hash(&mut hasher);
    match hasher.finish() {
        NOTHING_WRITTEN => NOTHING_WRITTEN + 1,
        hash => hash,
    }
}

fn write_snapshot(
    path: &std::path::Path,
    snapshot: &LayoutSnapshot,
    hash: u64,
    written: &AtomicU64,
) -> bool {
    match write_layout_snapshot(path, snapshot) {
        Ok(true) => {
            written.store(hash, Ordering::Release);
            debug!(path = %path.display(), "Autosaved layout");
            true
        }
        Ok(false) => {
            debug!(path = %path.display(), "A newer layout save got there first");
            true
        }
        Err(error) => {
            warn!(path = %path.display(), %error, "Could not autosave the layout");
            false
        }
    }
}

/// The `rift-autosave` thread: wakes the reactor when asked and writes snapshots in order.
/// Ends when the reactor drops its end of the queue.
fn run_worker(jobs: mpsc::Receiver<Job>, wake: impl Fn(), written: &AtomicU64) {
    let mut wake_at: Option<Instant> = None;
    loop {
        let job = match wake_at {
            None => match jobs.recv() {
                Ok(job) => job,
                Err(mpsc::RecvError) => return,
            },
            Some(at) => match jobs.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(job) => job,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    wake_at = None;
                    wake();
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            },
        };
        match job {
            Job::Wake(at) => wake_at = Some(wake_at.map_or(at, |current| current.min(at))),
            Job::Write { path, snapshot, hash } => {
                write_snapshot(&path, &snapshot, hash, written);
            }
        }
    }
}

impl Reactor {
    /// Arm the autosave: the event being handled may have changed the layout.
    pub(super) fn note_layout_change(&mut self) { self.autosave.note_change(Instant::now()); }

    /// An autosave wake arrived: save if it is due and rift's state is settled.
    pub(super) fn run_due_autosave(&mut self, now: Instant) {
        if !self.autosave.take_due(now) {
            return;
        }
        // Mid-sleep/wake, display churn, a drag or Mission Control: the state is in flux.
        // Saving it could store a half-applied topology; wait for it to settle.
        if self.refreshes_blocked() || self.is_in_drag() || self.is_mission_control_active() {
            self.autosave.postpone(now);
            return;
        }
        let active_space = self.active_display_space();
        match self
            .layout_manager
            .layout_engine
            .snapshot_layout_for_autosave(&self.state.windows, active_space)
        {
            Ok(snapshot) => self.autosave.save(snapshot),
            Err(error) => warn!(%error, "Could not autosave the layout"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::RecvTimeoutError;

    use super::*;
    use crate::actor::reactor::testing::*;
    use crate::layout_engine::{LayoutCommand, LayoutEngine};
    use crate::sys::screen::SpaceId;

    fn settings(autosave: bool) -> PersistenceSettings {
        PersistenceSettings {
            autosave,
            autosave_debounce_ms: 1000,
            restore_on_start: false,
        }
    }

    /// A reactor on one display with three windows, autosaving to a file in `dir`.
    fn autosaving_reactor(dir: &tempfile::TempDir) -> (Apps, Reactor, PathBuf) {
        let path = dir.path().join("layout.ron");
        let (mut apps, mut reactor) = test_context();
        reactor.autosave = Autosave::new(&settings(true), path.clone());
        reactor.handle_event(space_state_event(vec![left_screen()], vec![Some(SpaceId::new(
            1,
        ))]));
        apps.make_app_and_settle(&mut reactor, 1, make_windows(3));
        (apps, reactor, path)
    }

    /// Fire the armed save: the wake the worker would send at the due time.
    fn fire_due_save(reactor: &mut Reactor) {
        let due = reactor.autosave.due().expect("a save should be armed");
        reactor.run_due_autosave(due);
    }

    #[test]
    fn a_burst_of_changes_is_saved_once_after_the_debounce() {
        let dir = tempfile::tempdir().unwrap();
        let (mut apps, mut reactor, path) = autosaving_reactor(&dir);
        fire_due_save(&mut reactor);
        assert_eq!(reactor.autosave.writes, 1);
        let start = Instant::now();

        for workspace in [1, 2, 3, 2] {
            reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(workspace));
            apps.simulate_until_quiet(&mut reactor);
        }
        let due = reactor.autosave.due().expect("the switches arm a save");
        assert!(
            due >= start + Duration::from_millis(1000),
            "saved only after the debounce"
        );
        reactor.run_due_autosave(due - Duration::from_millis(1));
        assert_eq!(
            reactor.autosave.writes, 1,
            "nothing is written before the save is due"
        );

        reactor.run_due_autosave(due);
        assert_eq!(reactor.autosave.writes, 2, "the burst is one write");
        assert_eq!(reactor.autosave.due(), None);
        let saved = LayoutEngine::load(path).unwrap();
        assert_eq!(
            saved.workspaces().active_workspace_idx(SpaceId::new(1)),
            Some(2),
            "the file holds the state after the burst"
        );
    }

    #[test]
    fn a_save_is_due_at_most_five_debounces_after_the_first_change() {
        let mut autosave = Autosave::new(&settings(true), PathBuf::from("/nonexistent"));
        let start = Instant::now();
        assert!(
            autosave.take_due(start + Duration::from_secs(1)),
            "enabling arms a first save"
        );
        for step in 0..20 {
            autosave.note_change(start + Duration::from_millis(500 * step));
        }
        assert_eq!(autosave.due(), Some(start + Duration::from_secs(5)));
        assert!(!autosave.take_due(start + Duration::from_millis(4999)));
        assert!(autosave.take_due(start + Duration::from_secs(5)));
    }

    #[test]
    fn no_change_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (_apps, mut reactor, path) = autosaving_reactor(&dir);
        fire_due_save(&mut reactor);
        assert_eq!(reactor.autosave.writes, 1);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();

        // Nothing happened: no save is armed, and a stray wake writes nothing.
        assert_eq!(reactor.autosave.due(), None);
        reactor.run_due_autosave(Instant::now() + Duration::from_secs(60));
        // A change that leaves the layout as it was is not written either.
        reactor.note_layout_change();
        fire_due_save(&mut reactor);

        assert_eq!(reactor.autosave.writes, 1);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }

    #[test]
    fn autosave_off_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.ron");
        let (mut apps, mut reactor) = test_context();
        reactor.autosave = Autosave::new(&settings(false), path.clone());
        reactor.handle_event(space_state_event(vec![left_screen()], vec![Some(SpaceId::new(
            1,
        ))]));
        apps.make_app_and_settle(&mut reactor, 1, make_windows(2));
        reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(1));

        assert_eq!(reactor.autosave.due(), None);
        reactor.run_due_autosave(Instant::now() + Duration::from_secs(60));
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_write_is_logged_and_retried_on_the_next_change() {
        let dir = tempfile::tempdir().unwrap();
        // The layout file's directory cannot be created: a plain file is in the way.
        std::fs::write(dir.path().join("not-a-directory"), b"").unwrap();
        let path = dir.path().join("not-a-directory").join("layout.ron");
        let (mut apps, mut reactor) = test_context();
        reactor.autosave = Autosave::new(&settings(true), path);
        reactor.handle_event(space_state_event(vec![left_screen()], vec![Some(SpaceId::new(
            1,
        ))]));
        apps.make_app_and_settle(&mut reactor, 1, make_windows(2));

        fire_due_save(&mut reactor);
        assert_eq!((reactor.autosave.writes, reactor.autosave.failures), (0, 1));

        // Unchanged layout, but nothing was written: the next save tries again.
        reactor.note_layout_change();
        fire_due_save(&mut reactor);
        assert_eq!((reactor.autosave.writes, reactor.autosave.failures), (0, 2));
        reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(1));
        assert!(
            reactor.autosave.due().is_some(),
            "rift keeps running and autosaving"
        );
    }

    #[test]
    fn a_save_due_mid_drag_or_topology_churn_waits() {
        let dir = tempfile::tempdir().unwrap();
        let (_apps, mut reactor, _path) = autosaving_reactor(&dir);
        reactor.space_state.authoritative = false;
        let due = reactor.autosave.due().unwrap();

        reactor.run_due_autosave(due);
        assert_eq!(reactor.autosave.writes, 0);
        assert_eq!(reactor.autosave.due(), Some(due + Duration::from_millis(1000)));

        reactor.space_state.authoritative = true;
        fire_due_save(&mut reactor);
        assert_eq!(reactor.autosave.writes, 1);
    }

    #[test]
    fn the_worker_wakes_once_per_armed_save_and_writes_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.ron");
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let (wake_tx, wake_rx) = mpsc::channel();
        let written = Arc::new(AtomicU64::new(NOTHING_WRITTEN));
        let worker_written = written.clone();
        let worker = std::thread::spawn(move || {
            run_worker(jobs_rx, move || wake_tx.send(()).unwrap(), &worker_written)
        });

        let start = Instant::now();
        jobs_tx.send(Job::Wake(start + Duration::from_millis(60))).unwrap();
        jobs_tx.send(Job::Wake(start + Duration::from_millis(30))).unwrap();
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(30));
        assert_eq!(
            wake_rx.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout),
            "one wake for the earliest request"
        );

        let mut engine = LayoutEngine::new(&Default::default(), &Default::default(), None);
        let failing = engine.snapshot().unwrap();
        let older = engine.snapshot().unwrap();
        let mut window_store = Default::default();
        let _ = engine.handle_event(
            &mut window_store,
            crate::layout_engine::LayoutEvent::SpaceExposed(
                SpaceId::new(1),
                objc2_core_foundation::CGSize::new(1000., 1000.),
            ),
        );
        let newer = engine.snapshot().unwrap();
        let newer_contents = newer.contents().to_owned();
        assert_ne!(newer_contents, older.contents());
        // A NUL byte: no such directory can exist.
        let bad_path = dir.path().join("missing\0").join("layout.ron");
        let jobs = [
            (bad_path, failing, 7),
            (path.clone(), newer, 8),
            (path.clone(), older, 9),
        ];
        for (path, snapshot, hash) in jobs {
            jobs_tx.send(Job::Write { path, snapshot, hash }).unwrap();
        }
        drop(jobs_tx);
        worker.join().expect("a failed write must not end the worker");

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            newer_contents,
            "a snapshot taken before the one in the file never replaces it"
        );
        assert_eq!(written.load(Ordering::Acquire), 8);
    }
}
