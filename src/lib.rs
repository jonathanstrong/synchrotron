//! A single-threaded busy-wait executor.
//!
//! All tasks are cooperatively run on the same thread and no I/O polling is
//! done.

extern crate futures;
extern crate index_queue;
extern crate vec_arena;
extern crate void;

#[cfg(test)]
mod tests;

use std::fmt;
use std::cell::RefCell;
use std::rc::{self, Rc};
use std::sync::{Arc, Mutex};
use futures::executor::{Spawn, Unpark};
use futures::{Async, Future};
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
        debug_assert!(queue_index != !0);
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

struct Spawned<'a> {
    spawn: Spawn<Box<Future<Item=(), Error=Void> + 'a>>,
    ticket: Arc<Ticket>,
}

impl<'a> fmt::Debug for Spawned<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Spawned")
            .field(&self.ticket)
            .finish()
    }
}

#[derive(Default)]
struct Inner<'a> {
    spawns: Arena<Option<Spawned<'a>>>,
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
    /// Spawn a new task into this future.  The spawned tasks are executed
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
            spawn: futures::executor::spawn(Box::new(f)),
            ticket: ticket,
        });
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
    pub fn run<F: Future>(&mut self, f: F) -> Result<F::Item, F::Error> {
        let mut main_spawn = futures::executor::spawn(f);
        let main_ticket = self.0.borrow().new_ticket(SpawnId::main());
        loop {
            let index = match {
                let inner = self.0.borrow();
                let popped = inner.queue.lock().unwrap().pop_front();
                popped
            } {
                None => continue,
                Some(index) => index,
            };
            match SpawnId::from_queue_index(index).to_aux() {
                None => {
                    let ticket = main_ticket.clone();
                    let status = main_spawn.poll_future(ticket)?;
                    if let Async::Ready(x) = status {
                        main_ticket.deactivate();
                        return Ok(x);
                    }
                }
                Some(aux) => {
                    let spawned = self.0.borrow_mut().spawns.get_mut(aux)
                        .and_then(|x| x.take());
                    if let Some(mut spawned) = spawned {
                        let ticket = spawned.ticket.clone();
                        let status = spawned.spawn.poll_future(ticket)
                            .map_err(|void| match void {})?;
                        let mut inner = self.0.borrow_mut();
                        if let Async::Ready(()) = status {
                            spawned.ticket.deactivate();
                            inner.spawns.remove(aux);
                        } else {
                            inner.spawns[aux] = Some(spawned);
                        }
                    } else {
                        self.0.borrow_mut().spawns.remove(aux);
                    }
                }
            }
        }
    }
}
