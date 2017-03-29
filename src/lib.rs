//! A single-threaded busy-wait executor.
//!
//! All tasks are cooperatively run on the same thread and no I/O polling is
//! done.

extern crate futures;
extern crate index_queue;
extern crate vec_arena;
extern crate void;

use std::fmt;
use std::cell::RefCell;
use std::rc::{self, Rc};
use std::sync::{Arc, Mutex};
use futures::executor::{self, Spawn, Unpark};
use futures::{Async, Future, Poll, future, task};
use index_queue::IndexQueue;
use vec_arena::Arena;
use void::Void;

/// Helper struct for writing `Debug` implementations.
struct DebugWith<F>(F);

impl<F: Fn(&mut fmt::Formatter) -> fmt::Result> fmt::Debug for DebugWith<F> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpawnId(usize);

impl SpawnId {
    fn from_queue_index(queue_index: usize) -> Self {
        debug_assert_ne!(queue_index, !0);
        SpawnId(queue_index)
    }

    fn to_queue_index(self) -> usize {
        self.0
    }

    fn main() -> Self {
        Self::from_queue_index(0)
    }

    fn aux(aux_index: usize) -> Self {
        debug_assert!(aux_index < !0 - 1);
        Self::from_queue_index(aux_index + 1)
    }

    fn to_aux(self) -> Option<usize> {
        if self == Self::main() {
            None
        } else {
            Some(self.to_queue_index() - 1)
        }
    }
}

// we need atomics here because Unpark requires Send + Sync :/
struct TicketInner {
    // keep the id out of the 'Option': this helps debuggability (so we know
    // which spawn this ticket belongs to) and also allows null-Arc optimizations
    id: SpawnId,
    queue: Option<Arc<Mutex<IndexQueue>>>,
}

impl fmt::Debug for TicketInner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let name = if self.queue.is_none() {
            "TicketInner[inactive]"
        } else {
            "TicketInner"
        };
        f.debug_tuple(name)
            .field(&self.id.to_queue_index())
            .finish()
    }
}

#[derive(Debug)]
struct Ticket(Mutex<TicketInner>);

impl Ticket {
    fn deactivate(&self) {
        let inner = self.0.lock().unwrap();
        inner.queue.as_ref().map(|queue| {
            queue.lock().unwrap().remove(inner.id.to_queue_index());
        });
    }
}

impl Unpark for Ticket {
    fn unpark(&self) {
        let inner = self.0.lock().unwrap();
        inner.queue.as_ref().map(|queue| {
            queue.lock().unwrap().push_back(inner.id.to_queue_index());
        });
    }
}

struct Spawned<F> {
    spawn: Spawn<F>,
    ticket: Arc<Ticket>,
}

impl<F> fmt::Debug for Spawned<F> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Spawned")
            .field(&self.ticket)
            .finish()
    }
}

type SpawnedBox<'a> = Spawned<Box<Future<Item=(), Error=Void> + 'a>>;

#[derive(Default)]
struct Inner<'a> {
    spawns: Arena<Option<SpawnedBox<'a>>>,
    queue: Arc<Mutex<IndexQueue>>,
}

impl<'a> Inner<'a> {
    fn new_ticket(&self, id: SpawnId) -> Arc<Ticket> {
        let ticket = Arc::new(Ticket(Mutex::new(TicketInner {
            id: id,
            queue: Some(self.queue.clone()),
        })));
        ticket.unpark();
        ticket
    }
}

impl<'a> fmt::Debug for Inner<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Inner")
            .field("spawns", &DebugWith(|f: &mut fmt::Formatter| {
                f.debug_list().entries(self.spawns.iter().map(|(i, _)| i))
                    .finish()
            }))
            .field("queue", &self.queue)
            .finish()
    }
}

/// A cloneable handle to a [`Core`](struct.Core.html).
///
/// Cloned handles always refer to the same `Core` instance.
///
/// `Handle` can be used to `spawn` tasks even when the `Core` is running.
#[derive(Debug, Clone)]
pub struct Handle<'a>(rc::Weak<RefCell<Inner<'a>>>);

impl<'a> Handle<'a> {
    /// Spawn a new task into the executor.  The spawned tasks are executed
    /// when [`run`](struct.Core.html#method.run) is called.
    pub fn spawn<F: Future<Item=(), Error=Void> + 'a>(&self, f: F) {
        let inner = match self.0.upgrade() {
            Some(inner) => inner,
            None => return,
        };
        let mut inner = inner.borrow_mut();
        let aux = inner.spawns.insert(None);
        let ticket = inner.new_ticket(SpawnId::aux(aux));
        inner.spawns[aux] = Some(Spawned {
            spawn: executor::spawn(Box::new(f) as Box<_>),
            ticket: ticket,
        });
    }
}

/// Unpark the current task if the `status` is `Some(Ok(NotReady))` or `None`.
fn yield_turn<T, E>(status: Option<Poll<T, E>>) -> Poll<T, E> {
    let result = status.unwrap_or(Ok(Async::NotReady));
    if let Ok(Async::NotReady) = result {
        task::park().unpark();
    }
    result
}

/// A combined `Core` and future `F` that can be run.
#[derive(Debug)]
pub struct RunFuture<'b, 'a: 'b, F> {
    core: &'b mut Core<'a>,
    spawned: Spawned<F>,
}

impl<'b, 'a, F: Future> RunFuture<'b, 'a, F> {
    /// Run the future `F` on the current thread until completion.  Spawned
    /// tasks are run concurrently as well, but may or may not complete.
    pub fn run(&mut self) -> Result<F::Item, F::Error> {
        loop {
            match self.turn().unwrap_or(Ok(Async::NotReady))? {
                Async::Ready(x) => return Ok(x),
                Async::NotReady => continue,
            }
        }
    }

    /// Perform one iteration of the executor loop.  Returns `None` if all
    /// tasks are parked (no apparent progress was made).
    pub fn turn(&mut self) -> Option<Poll<F::Item, F::Error>> {
        self.core.turn_with(Ok(&mut self.spawned))
    }
}

impl<'b, 'a, F: Future> Future for RunFuture<'b, 'a, F> {
    type Item = F::Item;
    type Error = F::Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        yield_turn(self.turn())
    }
}

/// The task executor.
#[derive(Debug, Default)]
pub struct Core<'a>(Rc<RefCell<Inner<'a>>>);

impl<'a> Core<'a> {
    /// Create a [`Handle`](struct.Handle.html) to this executor, which can be
    /// used to [`spawn`](struct.Handle.html#method.spawn) additional tasks.
    pub fn handle(&self) -> Handle<'a> {
        Handle(Rc::downgrade(&self.0))
    }

    /// Run the given future on the current thread until completion.  Spawned
    /// tasks are run concurrently as well, but may or may not complete.
    ///
    /// This is equivalent to `self.run_future().run()`.
    pub fn run<F: Future>(&mut self, f: F) -> Result<F::Item, F::Error> {
        self.run_future(f).run()
    }

    /// Like [`run`](#method.run), but creates a
    /// [`RunFuture`](struct.RunFuture.html) object, which allows one to
    /// manually [`turn`](struct.RunFuture.html#method.turn) the executor.
    pub fn run_future<'b, F: Future>(&'b mut self, f: F)
                                     -> RunFuture<'b, 'a, F> {
        let ticket = {
            let inner = self.0.borrow();
            // if the main spawn is still queued somehow (because the user did
            // not complete a previous RunFuture), remove it
            let id = SpawnId::main();
            inner.queue.lock().unwrap().remove(id.to_queue_index());
            inner.new_ticket(id)
        };
        RunFuture {
            core: self,
            spawned: Spawned {
                spawn: executor::spawn(f),
                ticket: ticket,
            },
        }
    }

    /// Perform one iteration of the executor loop.  Returns `None` if all
    /// tasks are parked (no apparent progress was made).  Returns
    /// `Some(Ok(Ready(())))` if all spawned tasks have completed.
    pub fn turn(&mut self) -> Option<Poll<(), Void>> {
        self.turn_with::<future::Empty<(), Void>>(Err(()))
    }

    /// Perform one iteration of the executor loop, optionally with a given
    /// main spawn.  Returns `None` if all tasks are parked (no apparent
    /// progress could be made).  If `main` is set to `Err(e)`, returns
    /// `Some(Ok(Ready(e)))` if there are no more spawns.
    fn turn_with<F: Future>(&mut self, main: Result<&mut Spawned<F>, F::Item>)
                            -> Option<Poll<F::Item, F::Error>> {
        let index = {
            let inner = self.0.borrow();
            let popped = inner.queue.lock().unwrap().pop_front();
            match popped {
                None => return if inner.spawns.is_empty() {
                    match main {
                        Err(item) => Some(Ok(Async::Ready(item))),
                        Ok(_) => None
                    }
                } else {
                    None
                },
                Some(index) => index,
            }
        };
        match SpawnId::from_queue_index(index).to_aux() {
            None => {
                match main {
                    Err(_) => Some(Ok(Async::NotReady)),
                    Ok(main) => {
                        let ticket = main.ticket.clone();
                        let poll = main.spawn.poll_future(ticket);
                        if let Ok(Async::Ready(_)) = poll {
                            main.ticket.deactivate();
                        }
                        Some(poll)
                    }
                }
            }
            Some(aux) => {
                let spawned = self.0.borrow_mut().spawns.get_mut(aux)
                    .and_then(|x| x.take());
                if let Some(mut spawned) = spawned {
                    let ticket = spawned.ticket.clone();
                    let poll = spawned.spawn.poll_future(ticket);
                    let mut inner = self.0.borrow_mut();
                    if let Ok(Async::Ready(())) = poll {
                        spawned.ticket.deactivate();
                        inner.spawns.remove(aux);
                    } else {
                        inner.spawns[aux] = Some(spawned);
                    }
                } else {
                    self.0.borrow_mut().spawns.remove(aux);
                }
                Some(Ok(Async::NotReady))
            }
        }
    }
}

impl<'a> Future for Core<'a> {
    type Item = ();
    type Error = Void;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        yield_turn(self.turn())
    }
}
