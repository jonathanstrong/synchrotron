use std::{fmt, mem};
use futures::{Async, Future, Poll};
use futures::task::{self, Task};
use void::Void;
use super::drop_off;
use super::Handle;

struct SpawnedFuture<F: Future> {
    future: F,
    sender: Option<drop_off::Sender<Result<F::Item, F::Error>>>,
    task: Task,
}

impl<F> fmt::Debug for SpawnedFuture<F>
    where F: Future + fmt::Debug,
          F::Item: fmt::Debug,
          F::Error: fmt::Debug
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SpawnedFuture")
            .field("future", &self.future)
            .field("sender", &self.sender)
            .field("task", &self.task)
            .finish()
    }
}

impl<F: Future> Future for SpawnedFuture<F> {
    type Item = ();
    type Error = Void;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let result = match self.future.poll() {
            Ok(Async::NotReady) => return Ok(Async::NotReady),
            Ok(Async::Ready(item)) => Ok(item),
            Err(err) => Err(err),
        };
        let _ = self.sender.take()
            .expect("polled too many times")
            .send(result);
        self.task.unpark();
        Ok(Async::Ready(()))
    }
}

enum State<'a, F: Future> {
    Starting { handle: Handle<'a>, future: F },
    Waiting { receiver: drop_off::Receiver<Result<F::Item, F::Error>> },
    Invalid,
}

impl<'a, F> fmt::Debug for State<'a, F>
    where F: Future + fmt::Debug,
          F::Item: fmt::Debug,
          F::Error: fmt::Debug
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &State::Starting { ref handle, ref future } => {
                f.debug_struct("State::Starting")
                    .field("handle", handle)
                    .field("future", future)
                    .finish()
            }
            &State::Waiting { ref receiver } => {
                f.debug_struct("State::Waiting")
                    .field("receiver", receiver)
                    .finish()
            }
            &State::Invalid => {
                f.debug_struct("State::Invalid")
                    .finish()
            }
        }
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct SpawnFuture<'a, F: Future>(State<'a, F>);

impl<'a, F: Future> SpawnFuture<'a, F> {
    pub fn new(handle: Handle<'a>, future: F) -> Self {
        SpawnFuture(State::Starting { handle: handle, future: future })
    }
}

impl<'a, F> fmt::Debug for SpawnFuture<'a, F>
    where F: Future + fmt::Debug,
          F::Item: fmt::Debug,
          F::Error: fmt::Debug
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("SpawnFuture")
            .field(&self.0)
            .finish()
    }
}

impl<'a, F: Future + 'a> Future for SpawnFuture<'a, F> {
    type Item = F::Item;
    type Error = F::Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match mem::replace(&mut self.0, State::Invalid) {
            State::Starting { handle, future } => {
                let (sender, receiver) = drop_off::new();
                handle.spawn(SpawnedFuture {
                    future: future,
                    sender: Some(sender),
                    task: task::park(),
                });
                self.0 = State::Waiting { receiver: receiver };
                Ok(Async::NotReady)
            }
            State::Waiting { receiver } => match receiver.take() {
                Ok(Ok(item)) => Ok(Async::Ready(item)),
                Ok(Err(err)) => Err(err),
                Err(Some(receiver)) => {
                    // spurious wake-up
                    self.0 = State::Waiting { receiver: receiver };
                    Ok(Async::NotReady)
                }
                Err(None) => panic!("SpawnedFuture was dropped"),
            },
            State::Invalid => panic!("invalid State"),
        }
    }
}
