//! Startup work that may block where std can't time it out, run on a thread of its own by the
//! rules in docs/how-it-works.md. A permit bounds how many such threads run at once, the
//! deadline stops each phase, and one atomic step decides whether a result is handed over.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How many startup threads may run at once in a process.
pub(crate) const MAX_STARTS: usize = 4;

/// The process's permits for startup threads.
pub(crate) static STARTS: Permits = Permits::new();

/// The permits for startup threads.
pub(crate) struct Permits {
    taken: AtomicUsize,
}

impl Permits {
    pub(crate) const fn new() -> Permits {
        Permits {
            taken: AtomicUsize::new(0),
        }
    }

    /// Takes a permit, waiting for one until `deadline` when all are taken.
    fn take(&'static self, deadline: Instant) -> Option<Permit> {
        loop {
            let taken = self.taken.load(Ordering::SeqCst);
            if taken < MAX_STARTS {
                if self
                    .taken
                    .compare_exchange(taken, taken + 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return Some(Permit { permits: self });
                }
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            // Permits come back without a signal, so look again shortly.
            std::thread::sleep(left.min(Duration::from_millis(2)));
        }
    }

    #[cfg(test)]
    pub(crate) fn taken(&self) -> usize {
        self.taken.load(Ordering::SeqCst)
    }
}

/// One startup thread's right to run, given back when dropped, unwinding included.
struct Permit {
    permits: &'static Permits,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.permits.taken.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Why a startup gave no result.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Late {
    /// The deadline passed first.
    Deadline,
    /// Every permit stayed taken until the deadline, or no thread could start.
    Crowded,
    /// The startup thread ended without a result, as by a panic.
    Failed,
}

/// A startup's outcome. It starts pending, and whichever of the startup thread and the waiting
/// caller gets there first decides it, once.
///
/// | From | To | By | When |
/// |---|---|---|---|
/// | pending | handed over | the startup thread | it finishes before the deadline |
/// | pending | given up | the startup thread | it finishes at or after the deadline |
/// | pending | given up | the caller | its wait times out, or the thread ends without a result |
///
/// Handed over and given up are final. Each side acts on the outcome that holds after its
/// attempt, its own or the other side's, and the caller waits for the result only once it's
/// handed over.
struct Outcome(AtomicU8);

const PENDING: u8 = 0;
const HANDED_OVER: u8 = 1;
const GIVEN_UP: u8 = 2;

impl Outcome {
    /// Decides `to` if the outcome is still pending. Returns the final outcome, and whether
    /// this call decided it.
    fn decide(&self, to: u8) -> (u8, bool) {
        match self
            .0
            .compare_exchange(PENDING, to, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => (to, true),
            Err(earlier) => (earlier, false),
        }
    }

    fn is_pending(&self) -> bool {
        self.0.load(Ordering::SeqCst) == PENDING
    }
}

/// The startup thread's side of a startup. Dropping it gives the permit back, so the thread
/// keeps it until it has cleaned up whatever it didn't hand over.
pub(crate) struct Start<T> {
    deadline: Instant,
    outcome: Arc<Outcome>,
    // Dropped before `result`, so a caller that sees the thread end sees the permit back.
    _permit: Permit,
    result: mpsc::SyncSender<T>,
}

impl<T> Start<T> {
    /// Whether the startup is over, so no further phase should begin.
    pub(crate) fn is_over(&self) -> bool {
        Instant::now() >= self.deadline || !self.outcome.is_pending()
    }

    /// Hands `result` to the caller if the deadline hasn't passed and nothing was decided yet,
    /// else gives it back for the startup thread to dispose of.
    pub(crate) fn finish(&self, result: T) -> Result<(), T> {
        let to = if Instant::now() < self.deadline {
            HANDED_OVER
        } else {
            GIVEN_UP
        };
        match self.outcome.decide(to) {
            (HANDED_OVER, true) => {
                // The channel holds one value, so this never blocks.
                let _ = self.result.send(result);
                Ok(())
            }
            _ => Err(result),
        }
    }
}

/// Runs `work` on a startup thread named `name` and waits until `deadline` for the result it
/// finishes with. Returns the result and the thread, which ends when `work` returns.
pub(crate) fn run<T: Send + 'static>(
    name: &str,
    deadline: Instant,
    permits: &'static Permits,
    work: impl FnOnce(Start<T>) + Send + 'static,
) -> Result<(T, JoinHandle<()>), Late> {
    let permit = permits.take(deadline).ok_or(Late::Crowded)?;
    if Instant::now() >= deadline {
        return Err(Late::Deadline);
    }
    let outcome = Arc::new(Outcome(AtomicU8::new(PENDING)));
    let (result, results) = mpsc::sync_channel(1);
    let start = Start {
        deadline,
        outcome: Arc::clone(&outcome),
        _permit: permit,
        result,
    };
    // If the thread can't start, dropping `work` and `start` gives the permit back.
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || work(start))
        .map_err(|_| Late::Crowded)?;
    let received = match results.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(received) => Ok(received),
        Err(mpsc::RecvTimeoutError::Timeout) => match outcome.decide(GIVEN_UP) {
            // Handed over before the deadline, and the value follows at once.
            (HANDED_OVER, _) => results.recv().map_err(|_| Late::Failed),
            // Given up, by this call or by the thread, which disposes of its result itself.
            _ => Err(Late::Deadline),
        },
        // The thread ended without sending a result.
        Err(mpsc::RecvTimeoutError::Disconnected) => match outcome.decide(GIVEN_UP) {
            // It stopped for the deadline, or gave its result up.
            (GIVEN_UP, decided_here) if !decided_here || Instant::now() >= deadline => {
                Err(Late::Deadline)
            }
            // It panicked before the deadline, or between handing over and sending.
            _ => Err(Late::Failed),
        },
    };
    received.map(|received| (received, thread))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicBool;

    fn soon() -> Instant {
        Instant::now() + Duration::from_millis(300)
    }

    #[test]
    fn concurrent_starts_stay_within_the_permits() {
        const CALLERS: usize = 12;
        static PERMITS: Permits = Permits::new();
        let barrier = Arc::new(Barrier::new(CALLERS));
        let entered = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let calls: Vec<_> = (0..CALLERS)
            .map(|_| {
                let (barrier, entered, release) = (
                    Arc::clone(&barrier),
                    Arc::clone(&entered),
                    Arc::clone(&release),
                );
                std::thread::spawn(move || {
                    barrier.wait();
                    run("test", soon(), &PERMITS, move |start: Start<()>| {
                        entered.fetch_add(1, Ordering::SeqCst);
                        while !release.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        let _ = start.finish(());
                    })
                    .map(|_| ())
                })
            })
            .collect();
        let results: Vec<_> = calls.into_iter().map(|call| call.join().unwrap()).collect();
        assert_eq!(entered.load(Ordering::SeqCst), MAX_STARTS);
        assert_eq!(PERMITS.taken(), MAX_STARTS);
        assert_eq!(
            results
                .iter()
                .filter(|r| **r == Err(Late::Deadline))
                .count(),
            MAX_STARTS
        );
        assert_eq!(
            results.iter().filter(|r| **r == Err(Late::Crowded)).count(),
            CALLERS - MAX_STARTS
        );
        release.store(true, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(2);
        while PERMITS.taken() != 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(PERMITS.taken(), 0);
    }

    #[test]
    fn a_start_waits_its_turn_for_a_permit() {
        static PERMITS: Permits = Permits::new();
        let release = Arc::new(AtomicBool::new(false));
        let holders: Vec<_> = (0..MAX_STARTS)
            .map(|_| {
                let release = Arc::clone(&release);
                std::thread::spawn(move || {
                    run("test", soon(), &PERMITS, move |start: Start<u8>| {
                        while !release.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        let _ = start.finish(1);
                    })
                    .map(|(value, _)| value)
                })
            })
            .collect();
        while PERMITS.taken() < MAX_STARTS {
            std::thread::sleep(Duration::from_millis(1));
        }
        let waiting = std::thread::spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(2);
            run("test", deadline, &PERMITS, |start: Start<u8>| {
                let _ = start.finish(2);
            })
            .map(|(value, _)| value)
        });
        std::thread::sleep(Duration::from_millis(50));
        release.store(true, Ordering::SeqCst);
        for holder in holders {
            assert_eq!(holder.join().unwrap(), Ok(1));
        }
        assert_eq!(waiting.join().unwrap(), Ok(2));
    }

    #[test]
    fn an_expired_deadline_starts_nothing() {
        static PERMITS: Permits = Permits::new();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_inside = Arc::clone(&ran);
        let past = Instant::now() - Duration::from_secs(1);
        let result = run("test", past, &PERMITS, move |start: Start<()>| {
            ran_inside.store(true, Ordering::SeqCst);
            let _ = start.finish(());
        });
        assert!(matches!(result, Err(Late::Crowded | Late::Deadline)));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(PERMITS.taken(), 0);
    }

    #[test]
    fn a_panic_gives_the_permit_back() {
        static PERMITS: Permits = Permits::new();
        // A panic can take long to print, as when it captures a backtrace on Windows, and the
        // caller learns of it as soon as the thread ends.
        let deadline = Instant::now() + Duration::from_secs(30);
        let result = run("test", deadline, &PERMITS, |_: Start<()>| {
            panic!("a failing start")
        });
        assert_eq!(result.map(|_| ()), Err(Late::Failed));
        assert_eq!(PERMITS.taken(), 0);
    }

    #[test]
    fn a_late_result_is_disposed_of_before_the_permit_comes_back() {
        static PERMITS: Permits = Permits::new();
        let (disposed, dispose) = mpsc::channel();
        let (release, proceed) = mpsc::channel::<()>();
        let deadline = Instant::now() + Duration::from_millis(30);
        let result = run("test", deadline, &PERMITS, move |start: Start<u8>| {
            proceed.recv().unwrap();
            assert!(start.is_over());
            if let Err(late) = start.finish(5) {
                disposed.send((late, PERMITS.taken())).unwrap();
            }
        });
        assert_eq!(result.map(|_| ()), Err(Late::Deadline));
        release.send(()).unwrap();
        let (late, taken_while_disposing) = dispose.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!((late, taken_while_disposing), (5, 1));
        let until = Instant::now() + Duration::from_secs(1);
        while PERMITS.taken() != 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(PERMITS.taken(), 0);
    }

    #[test]
    fn a_result_in_time_is_handed_over_once() {
        static PERMITS: Permits = Permits::new();
        let (value, thread) = run("test", soon(), &PERMITS, |start: Start<u8>| {
            assert_eq!(start.finish(9), Ok(()));
            assert!(start.is_over());
        })
        .unwrap();
        thread.join().unwrap();
        assert_eq!(value, 9);
        assert_eq!(PERMITS.taken(), 0);
    }

    #[test]
    fn a_thread_that_gives_up_doesnt_make_the_caller_wait_for_its_cleanup() {
        static PERMITS: Permits = Permits::new();
        let mut waited = Vec::new();
        // The cleanup is long next to the deadline, so a late timer, as on a busy macOS
        // machine, doesn't pass for waiting.
        for attempt in 0..12 {
            let deadline = Instant::now() + Duration::from_millis(10);
            let began = Instant::now();
            let result = run("test", deadline, &PERMITS, move |start: Start<()>| {
                while Instant::now() < deadline {
                    std::hint::spin_loop();
                }
                assert_eq!(start.finish(()), Err(()));
                // Stands in for a native stream that takes a while to drop.
                std::thread::sleep(Duration::from_millis(300));
                drop(start);
            });
            let elapsed = began.elapsed();
            let result = result.map(|_| ());
            if elapsed > Duration::from_millis(150) || result != Err(Late::Deadline) {
                waited.push((attempt, elapsed, result));
            }
            while PERMITS.taken() != 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(waited.is_empty(), "the caller waited or failed: {waited:?}");
    }

    #[test]
    fn an_in_time_handover_can_be_received_after_the_deadline() {
        static PERMITS: Permits = Permits::new();
        let deadline = Instant::now() + Duration::from_millis(30);
        let (value, thread) = run("test", deadline, &PERMITS, |start: Start<u8>| {
            // A thread preempted between deciding the handover and sending.
            assert!(Instant::now() < start.deadline);
            assert_eq!(start.outcome.decide(HANDED_OVER), (HANDED_OVER, true));
            std::thread::sleep(Duration::from_millis(80));
            start.result.send(7).unwrap();
        })
        .unwrap();
        thread.join().unwrap();
        assert_eq!(value, 7);
        assert!(Instant::now() > deadline);
        assert_eq!(PERMITS.taken(), 0);
    }

    #[test]
    fn a_decision_is_final() {
        let outcome = Outcome(AtomicU8::new(PENDING));
        assert_eq!(outcome.decide(GIVEN_UP), (GIVEN_UP, true));
        assert_eq!(outcome.decide(HANDED_OVER), (GIVEN_UP, false));
        assert_eq!(outcome.decide(GIVEN_UP), (GIVEN_UP, false));

        static PERMITS: Permits = Permits::new();
        let (value, thread) = run("test", soon(), &PERMITS, |start: Start<u8>| {
            assert_eq!(start.finish(1), Ok(()));
            assert_eq!(start.finish(2), Err(2));
            assert_eq!(start.outcome.decide(GIVEN_UP), (HANDED_OVER, false));
        })
        .unwrap();
        thread.join().unwrap();
        assert_eq!(value, 1);
    }
}
