//! The commit thread's queue (issue #29, design 0024 V8): threads put
//! requests in, and the commit thread takes them out in the order they
//! came, the commits waiting behind one another as one batch (design 0021
//! G1), and hands them to a [`Handler`]. What a commit does to the files is
//! the handler's; this is only the queue, small enough for loom to run
//! every order its threads can take (the models at the end).
//!
//! The queue is a list under a lock with a condition variable, not
//! `std::sync::mpsc`: loom's channel does not see the senders go, so a
//! commit thread waiting on it when the database is dropped would look
//! stuck forever. Here dropping the database closes the queue, which loom
//! follows.

use std::collections::VecDeque;

use crate::sync::{Condvar, Mutex, MutexGuard};

/// Requests in the order they came, until it is closed.
pub(crate) struct Queue<T> {
    state: Mutex<State<T>>,
    /// Signalled when a request comes or the queue closes.
    ready: Condvar,
}

struct State<T> {
    items: VecDeque<T>,
    closed: bool,
}

impl<T> Queue<T> {
    pub(crate) fn new() -> Self {
        Queue {
            state: Mutex::new(State {
                items: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State<T>> {
        // Nothing panics while holding it.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Puts `item` at the back.
    ///
    /// # Panics
    ///
    /// If the queue is closed: the database closes it when it is dropped,
    /// and every sender borrows the database.
    pub(crate) fn send(&self, item: T) {
        let mut state = self.state();
        assert!(!state.closed, "the commit queue is closed");
        state.items.push_back(item);
        drop(state);
        self.ready.notify_one();
    }

    /// Takes the request at the front, waiting for one. `None` once the
    /// queue is closed and empty.
    pub(crate) fn recv(&self) -> Option<T> {
        let mut state = self.state();
        loop {
            if let Some(item) = state.items.pop_front() {
                return Some(item);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Takes the request at the front, if there is one, without waiting.
    pub(crate) fn try_recv(&self) -> Option<T> {
        self.state().items.pop_front()
    }

    /// Lets [`Queue::recv`] return `None` once the requests already in are
    /// taken.
    pub(crate) fn close(&self) {
        self.state().closed = true;
        self.ready.notify_all();
    }
}

/// What the commit thread is asked to do.
pub(crate) enum Request<C, P> {
    Commit(C),
    Checkpoint(P),
}

/// Does what the requests ask. Each method answers the requests it is
/// given.
pub(crate) trait Handler {
    type Commit;
    type Checkpoint;

    /// Commits a batch, in the order it came.
    fn commit(&mut self, batch: Vec<Self::Commit>);

    fn checkpoint(&mut self, request: Self::Checkpoint);
}

/// Hands `handler` the requests of `queue` until it is closed and empty:
/// a commit with every commit waiting behind it, without waiting for more,
/// up to `max_batch`, and a checkpoint on its own. A checkpoint ends a
/// batch, so it runs after the commits sent before it and before those
/// sent after it.
pub(crate) fn serve<H: Handler>(
    handler: &mut H,
    queue: &Queue<Request<H::Commit, H::Checkpoint>>,
    max_batch: usize,
) {
    let mut next = queue.recv();
    while let Some(request) = next.take() {
        match request {
            Request::Checkpoint(request) => handler.checkpoint(request),
            Request::Commit(first) => {
                let mut batch = vec![first];
                while batch.len() < max_batch {
                    match queue.try_recv() {
                        Some(Request::Commit(commit)) => batch.push(commit),
                        Some(other) => {
                            next = Some(other);
                            break;
                        }
                        None => break,
                    }
                }
                handler.commit(batch);
            }
        }
        if next.is_none() {
            next = queue.recv();
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Writes down what it was handed.
    #[derive(Default)]
    struct Log(Vec<String>);

    impl Handler for Log {
        type Commit = u32;
        type Checkpoint = ();

        fn commit(&mut self, batch: Vec<u32>) {
            self.0.push(format!("{batch:?}"));
        }

        fn checkpoint(&mut self, (): ()) {
            self.0.push("checkpoint".into());
        }
    }

    #[test]
    fn commits_waiting_go_as_one_batch_up_to_the_limit_and_a_checkpoint_ends_one() {
        let queue = Queue::new();
        for request in [
            Request::Commit(1),
            Request::Commit(2),
            Request::Commit(3),
            Request::Checkpoint(()),
            Request::Commit(4),
        ] {
            queue.send(request);
        }
        queue.close();
        let mut log = Log::default();
        serve(&mut log, &queue, 2);
        assert_eq!(log.0, ["[1, 2]", "[3]", "checkpoint", "[4]"]);
    }
}

/// Every order the threads can take through the queue (design 0024 V8),
/// run with `RUSTFLAGS="--cfg loom" cargo test -p oakdb --release --lib
/// loom`. `LOOM_CLIENTS` (2 or 3) and `LOOM_MAX_PREEMPTIONS` choose the
/// model's size; CI runs 2 and 2.
#[cfg(all(test, loom))]
mod loom_models {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use loom::sync::Arc;
    use loom::thread;

    use super::*;
    use crate::sync::mpsc;

    /// What a commit's sender is told.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Answer {
        Ok,
        Stopped,
    }

    /// A commit: a key and value to write, and where to answer.
    struct Commit {
        key: u8,
        value: u8,
        answer: mpsc::Sender<Answer>,
    }

    /// A checkpoint is answered with what it saw committed.
    type Checkpoint = mpsc::Sender<BTreeMap<u8, u8>>;

    /// What the database shows: [`crate::db`]'s `Committed`, a map here.
    #[derive(Default)]
    struct Shared {
        committed: Mutex<BTreeMap<u8, u8>>,
    }

    /// A commit thread with no disk: it publishes each batch under the
    /// lock, as the engine does once the batch is synced, then answers.
    /// With `stop_after` it stops after that many batches and answers
    /// every commit after them `Stopped`, as the engine does after a
    /// failed sync.
    struct Fake {
        shared: Arc<Shared>,
        batches: usize,
        stop_after: Option<usize>,
    }

    impl Handler for Fake {
        type Commit = Commit;
        type Checkpoint = Checkpoint;

        fn commit(&mut self, batch: Vec<Commit>) {
            let stopped = self.stop_after.is_some_and(|n| self.batches >= n);
            self.batches += 1;
            if stopped {
                for commit in batch {
                    commit.answer.send(Answer::Stopped).unwrap();
                }
                return;
            }
            {
                let mut committed = self.shared.committed.lock().unwrap();
                for commit in &batch {
                    committed.insert(commit.key, commit.value);
                }
            }
            for commit in batch {
                commit.answer.send(Answer::Ok).unwrap();
            }
        }

        fn checkpoint(&mut self, answer: Checkpoint) {
            let committed = self.shared.committed.lock().unwrap().clone();
            answer.send(committed).unwrap();
        }
    }

    type Requests = Queue<Request<Commit, Checkpoint>>;

    fn clients() -> u8 {
        std::env::var("LOOM_CLIENTS").map_or(2, |v| v.parse().expect("LOOM_CLIENTS"))
    }

    /// Runs `model` under loom and reports how many orders it ran and how
    /// long it took, for docs/experiments/0017-loom.md.
    fn run(name: &str, model: impl Fn() + Sync + Send + 'static) {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let mut builder = loom::model::Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(2);
        }
        let bound = builder.preemption_bound;
        let started = Instant::now();
        RUNS.store(0, Ordering::Relaxed);
        builder.check(move || {
            RUNS.fetch_add(1, Ordering::Relaxed);
            model();
        });
        eprintln!(
            "loom {name}: clients {}, preemption bound {bound:?}: {} orders in {:.2?}",
            clients(),
            RUNS.load(Ordering::Relaxed),
            started.elapsed()
        );
    }

    /// Starts the commit thread on `queue`.
    fn commit_thread(
        queue: &Arc<Requests>,
        shared: &Arc<Shared>,
        stop_after: Option<usize>,
    ) -> thread::JoinHandle<()> {
        let queue = Arc::clone(queue);
        let mut fake = Fake {
            shared: Arc::clone(shared),
            batches: 0,
            stop_after,
        };
        thread::spawn(move || serve(&mut fake, &queue, crate::MAX_BATCH))
    }

    /// Sends a commit of `key` = `value` and waits for its answer, which
    /// must come exactly once.
    fn commit(queue: &Requests, key: u8, value: u8) -> Answer {
        let (answer, answered) = mpsc::channel();
        queue.send(Request::Commit(Commit { key, value, answer }));
        let got = answered.recv().unwrap();
        assert!(answered.try_recv().is_err(), "a commit answered twice");
        got
    }

    #[test]
    fn loom_every_commit_is_answered_once_and_shows_before_its_answer() {
        run("answered once, read your writes", || {
            let queue = Arc::new(Queue::new());
            let shared = Arc::new(Shared::default());
            let committer = commit_thread(&queue, &shared, None);
            let threads: Vec<_> = (0..clients())
                .map(|key| {
                    let (queue, shared) = (Arc::clone(&queue), Arc::clone(&shared));
                    thread::spawn(move || {
                        assert_eq!(commit(&queue, key, 1), Answer::Ok);
                        // A transaction begun after the answer reads it.
                        assert_eq!(shared.committed.lock().unwrap().get(&key), Some(&1));
                    })
                })
                .collect();
            for client in threads {
                client.join().unwrap();
            }
            // Dropping the database: the commit thread ends, no deadlock.
            queue.close();
            committer.join().unwrap();
            let n = usize::from(clients());
            assert_eq!(shared.committed.lock().unwrap().len(), n);
        });
    }

    #[test]
    fn loom_a_checkpoint_runs_after_the_commits_sent_before_it_and_before_those_after() {
        run("checkpoint order", || {
            let queue = Arc::new(Queue::new());
            let shared = Arc::new(Shared::default());
            let committer = commit_thread(&queue, &shared, None);
            // Others commit meanwhile, to be batched with these or not.
            let others: Vec<_> = (1..clients())
                .map(|key| {
                    let queue = Arc::clone(&queue);
                    thread::spawn(move || assert_eq!(commit(&queue, 10 + key, 1), Answer::Ok))
                })
                .collect();
            // One thread sends a commit, a checkpoint and a commit without
            // waiting in between.
            let (first, first_answered) = mpsc::channel();
            let (checkpoint, checkpointed) = mpsc::channel();
            let (after, after_answered) = mpsc::channel();
            queue.send(Request::Commit(Commit {
                key: 0,
                value: 1,
                answer: first,
            }));
            queue.send(Request::Checkpoint(checkpoint));
            queue.send(Request::Commit(Commit {
                key: 0,
                value: 2,
                answer: after,
            }));
            let seen = checkpointed.recv().unwrap();
            assert_eq!(seen.get(&0), Some(&1), "the checkpoint saw {seen:?}");
            assert_eq!(first_answered.recv().unwrap(), Answer::Ok);
            assert_eq!(after_answered.recv().unwrap(), Answer::Ok);
            for other in others {
                other.join().unwrap();
            }
            queue.close();
            committer.join().unwrap();
            assert_eq!(shared.committed.lock().unwrap().get(&0), Some(&2));
        });
    }

    #[test]
    fn loom_once_stopped_every_later_commit_is_answered_stopped() {
        run("stopped", || {
            let queue = Arc::new(Queue::new());
            let shared = Arc::new(Shared::default());
            // The first batch goes; every one after it is refused.
            let committer = commit_thread(&queue, &shared, Some(1));
            let threads: Vec<_> = (0..clients())
                .map(|key| {
                    let queue = Arc::clone(&queue);
                    thread::spawn(move || commit(&queue, key, 1))
                })
                .collect();
            let answers: Vec<Answer> = threads.into_iter().map(|c| c.join().unwrap()).collect();
            queue.close();
            committer.join().unwrap();
            let committed = shared.committed.lock().unwrap();
            // Those answered Ok are exactly those that show.
            for (key, answer) in answers.iter().enumerate() {
                let shows = committed.contains_key(&(key as u8));
                assert_eq!(shows, *answer == Answer::Ok, "{answers:?}");
            }
            assert!(answers.contains(&Answer::Ok), "{answers:?}");
        });
    }
}
