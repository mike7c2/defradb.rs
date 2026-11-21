//! A worker's own state: its tasks, timers and per-key queues, and how a task
//! is spawned, polled under `catch_unwind` with a heartbeat, and finished.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use super::heartbeat::REPORTED;
use super::keyed::Keyed;
use super::shared::{bump, Admitted, Job, LocalFuture, WorkerShared};
use super::signal::TaskWaker;
use super::slab::{Key, Slab};
use super::timers::Timers;
use super::{drop_quietly, micros, panic_message, report_long_poll, Tuning};
use crate::clock::Clock;
use crate::completion::{self, Completion, Fail};
use crate::Error;

/// What a worker attaches to a task.
struct Meta {
    key: Option<u64>,
    /// Held for its drop: gives the worker's admission back when the task ends.
    _admitted: Option<Admitted>,
    /// Shutdown does not wait for it; it is dropped when the worker stops.
    daemon: bool,
}

struct Task {
    future: Option<LocalFuture>,
    waker: Arc<TaskWaker>,
    fail: Option<Arc<dyn Fail>>,
    name: &'static str,
    meta: Meta,
    /// Already reported as stuck: never reported again.
    reported: bool,
    /// Where its time went, while slow tasks are reported.
    times: Option<TaskTimes>,
}

/// Where a task's time went, in microseconds on the worker's clock.
#[derive(Clone, Copy, Default)]
struct TaskTimes {
    /// When it arrived on the worker.
    arrived: u64,
    /// Waiting behind earlier tasks of its key before it started.
    behind_key: u64,
    /// Ready to run but behind other tasks on the run queue.
    ready: u64,
    /// Running.
    busy: u64,
    polls: u32,
}

impl Drop for Task {
    /// A task that ends without its future sending a result (dropped at
    /// shutdown or cancelled) fails its completion; one that sent or
    /// panicked has settled it already, and this changes nothing.
    fn drop(&mut self) {
        if let Some(fail) = &self.fail {
            fail.fail(Error::ShutDown);
        }
    }
}

/// A worker's state, owned by its thread and shared there by `Rc` with the
/// futures (timers) that need it.
pub(crate) struct Local {
    pub(super) shared: Arc<WorkerShared>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) tuning: Tuning,
    tasks: RefCell<Slab<Task>>,
    pub(super) timers: RefCell<Timers>,
    keyed: RefCell<Keyed<Job>>,
    /// Tasks shutdown waits for (everything but daemons).
    live: Cell<usize>,
    pub(super) draining: Cell<bool>,
    closed: Cell<bool>,
}

impl Local {
    pub(super) fn new(shared: Arc<WorkerShared>, clock: Arc<dyn Clock>, tuning: Tuning) -> Local {
        Local {
            shared,
            clock,
            tuning,
            tasks: RefCell::new(Slab::new()),
            timers: RefCell::new(Timers::new()),
            keyed: RefCell::new(Keyed::new()),
            live: Cell::new(0),
            draining: Cell::new(false),
            closed: Cell::new(false),
        }
    }

    /// Stops taking work and drops every task, waiting job and timer, on this
    /// thread: each task's completion fails and its admission is given back.
    pub(super) fn close(&self) {
        self.closed.set(true);
        let tasks = self.tasks.borrow_mut().drain();
        tasks.into_iter().for_each(drop_quietly);
        self.live.set(0);
        let waiting = self.keyed.borrow_mut().drain();
        waiting.into_iter().for_each(drop_quietly);
        self.timers.borrow_mut().clear();
    }

    pub(crate) fn now(&self) -> u64 {
        self.clock.now_micros()
    }

    pub(super) fn spawn<F>(
        &self,
        name: &'static str,
        future: F,
        daemon: bool,
    ) -> Result<Completion<F::Output>, Error>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        if self.closed.get() {
            return Err(Error::ShutDown);
        }
        let (sender, completion) = completion::channel();
        let fail: Arc<dyn Fail> = sender.shared();
        let slot = sender.into_slot();
        let future: LocalFuture = Box::pin(async move { slot.send(future.await) });
        self.insert(
            name,
            future,
            Some(fail),
            Meta {
                key: None,
                _admitted: None,
                daemon,
            },
            None,
        );
        Ok(completion)
    }

    /// Whether slow tasks are reported, and so timed.
    fn timing(&self) -> bool {
        !self.tuning.slow_task_after.is_zero()
    }

    fn insert(
        &self,
        name: &'static str,
        future: LocalFuture,
        fail: Option<Arc<dyn Fail>>,
        meta: Meta,
        arrived: Option<u64>,
    ) {
        let mut tasks = self.tasks.borrow_mut();
        let key = tasks.next_key();
        if !meta.daemon {
            self.live.set(self.live.get() + 1);
        }
        let times = self.timing().then(|| {
            let now = self.now();
            let arrived = arrived.unwrap_or(now);
            TaskTimes {
                arrived,
                behind_key: now.saturating_sub(arrived),
                ..TaskTimes::default()
            }
        });
        let clock = times.map(|_| Arc::clone(&self.clock));
        let waker = TaskWaker::timed(self.shared.signal.clone(), key, clock);
        tasks.insert(Task {
            future: Some(future),
            waker,
            fail,
            name,
            meta,
            reported: false,
            times,
        });
        drop(tasks);
        bump(&self.shared.counters.tasks);
        self.shared.signal.schedule(key);
    }

    /// Starts a submitted job, or queues it behind its key's running task.
    pub(super) fn start(&self, mut job: Job) {
        if self.closed.get() {
            return drop_quietly(job);
        }
        if self.timing() {
            job.arrived = self.now();
        }
        let job = match job.key {
            Some(key) => match self.keyed.borrow_mut().admit(key, job) {
                Some(job) => job,
                None => return,
            },
            None => job,
        };
        self.launch(job);
    }

    fn launch(&self, job: Job) {
        let Job {
            start,
            fail,
            name,
            key,
            admitted,
            arrived,
            ..
        } = job;
        let fail: Arc<dyn Fail> = fail;
        self.insert(
            name,
            start(),
            Some(fail),
            Meta {
                key,
                _admitted: Some(admitted),
                daemon: false,
            },
            Some(arrived),
        );
    }

    /// Polls the task `key`; `false` when it no longer exists (a stale wake).
    pub(super) fn poll(&self, key: Key) -> bool {
        let taken = self.tasks.borrow_mut().get_mut(key).and_then(|task| {
            task.waker.clear();
            let future = task.future.take()?;
            let queued_at = task.times.map(|_| task.waker.queued_at());
            Some((
                future,
                Waker::from(task.waker.clone()),
                task.name,
                task.reported,
                queued_at,
            ))
        });
        let Some((mut future, waker, name, reported, queued_at)) = taken else {
            return false;
        };
        let start = self.now();
        let flag = if reported { REPORTED } else { 0 };
        self.shared
            .heartbeat
            .begin(start, key.to_bits() | flag, name);
        bump(&self.shared.counters.polls);
        let mut cx = Context::from_waker(&waker);
        let result = panic::catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(&mut cx)));
        let long = self.after_poll(name, start, reported);
        if let Some(queued_at) = queued_at {
            let end = self.now();
            if let Some(times) = self
                .tasks
                .borrow_mut()
                .get_mut(key)
                .and_then(|task| task.times.as_mut())
            {
                times.ready += start.saturating_sub(queued_at);
                times.busy += end.saturating_sub(start);
                times.polls += 1;
            }
        }
        match result {
            Ok(Poll::Pending) => match self.tasks.borrow_mut().get_mut(key) {
                Some(task) => {
                    task.future = Some(future);
                    task.reported |= long;
                }
                None => drop_quietly(future),
            },
            Ok(Poll::Ready(())) => self.finish(key, future, None),
            Err(payload) => self.finish(key, future, Some(panic_message(payload.as_ref()))),
        }
        true
    }

    /// Whether the poll that started at `start` ran past the limit: flagged by
    /// the monitor while it ran, or measured here when nothing watches.
    fn after_poll(&self, name: &'static str, start: u64, reported: bool) -> bool {
        let shared = &self.shared;
        if shared.stuck.load(Ordering::SeqCst) {
            shared.stuck.store(false, Ordering::SeqCst);
            return true;
        }
        if self.tuning.monitored {
            return false;
        }
        let elapsed = self.now().saturating_sub(start);
        if elapsed <= micros(self.tuning.stuck_after) {
            return false;
        }
        bump(&shared.counters.long_polls);
        if !reported {
            report_long_poll(shared.id.index, name, elapsed, self.tuning.stuck_after);
        }
        true
    }

    fn finish(&self, key: Key, future: LocalFuture, panic: Option<String>) {
        let task = self.tasks.borrow_mut().remove(key);
        if let (Some(message), Some(task)) = (panic, &task) {
            bump(&self.shared.counters.panics);
            tracing::warn!(
                task = task.name,
                worker = self.shared.id.index,
                message = message.as_str(),
                "a task panicked; its caller gets the error and the worker goes on"
            );
            if let Some(fail) = &task.fail {
                fail.fail(Error::Panicked {
                    task: task.name,
                    message,
                });
            }
        }
        drop_quietly(future);
        let Some(task) = task else { return };
        if let Some(times) = task.times {
            self.report_if_slow(task.name, times);
        }
        if !task.meta.daemon {
            self.live.set(self.live.get().saturating_sub(1));
        }
        let next = task
            .meta
            .key
            .and_then(|key| self.keyed.borrow_mut().finished(key));
        drop_quietly(task);
        if let Some(job) = next {
            self.launch(job);
        }
    }

    /// Reports a task that took longer than the limit, with where its time
    /// went: the rest of its life it waited for what it awaited.
    fn report_if_slow(&self, name: &'static str, times: TaskTimes) {
        let lived = self.now().saturating_sub(times.arrived);
        if lived < micros(self.tuning.slow_task_after) {
            return;
        }
        bump(&self.shared.counters.slow_tasks);
        let awaiting = lived.saturating_sub(times.behind_key + times.ready + times.busy);
        tracing::warn!(
            task = name,
            worker = self.shared.id.index,
            total_ms = lived / 1000,
            behind_key_ms = times.behind_key / 1000,
            ready_ms = times.ready / 1000,
            busy_ms = times.busy / 1000,
            awaiting_ms = awaiting / 1000,
            polls = times.polls,
            "a slow task: its time behind its key, ready behind other tasks, running, and awaiting"
        );
    }

    /// Nothing admitted is left to run.
    pub(super) fn drained(&self) -> bool {
        self.live.get() == 0 && self.keyed.borrow().is_empty() && !self.shared.has_messages()
    }

    pub(super) fn timers_due(&self) -> bool {
        self.timers
            .borrow_mut()
            .next_due()
            .is_some_and(|due| due <= self.now())
    }
}
