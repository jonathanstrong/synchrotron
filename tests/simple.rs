extern crate futures;
extern crate synchrotron;
extern crate void;

use std::{thread, time};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use futures::{future, task, Async, BoxFuture, Future};
use void::{ResultVoidExt, Void};

#[derive(Default)]
struct Inbox {
    messages: VecDeque<&'static str>,
    waiting: Vec<task::Task>,
}

fn send(inbox: &Arc<Mutex<Inbox>>, message: &'static str)
        -> BoxFuture<(), Void> {
    let inbox = inbox.clone();
    thread::spawn(move || {
        println!("sending {:?} ...", message);
        thread::sleep(time::Duration::from_millis(25 * message.len() as u64));
        let mut inbox = inbox.lock().unwrap();
        inbox.messages.push_back(message);
        for task in inbox.waiting.drain(..) {
            task.unpark();
        }
        println!("sent {:?}!", message);
    });
    future::ok(()).boxed()
}

fn receive(inbox: &Arc<Mutex<Inbox>>) -> BoxFuture<&'static str, Void> {
    let inbox = inbox.clone();
    future::poll_fn(move || {
        let mut inbox = inbox.lock().unwrap();
        match inbox.messages.pop_front() {
            Some(message) => {
                println!("received {:?}!", message);
                Ok(Async::Ready(message))
            }
            None => {
                inbox.waiting.push(task::park());
                Ok(Async::NotReady)
            }
        }
    }).boxed()
}

#[test]
fn main() {
    let main_inbox = &Default::default();
    let aux_inbox = &Default::default();
    let mut core = synchrotron::Core::default();
    let handle = core.handle();
    handle.spawn({
        receive(aux_inbox).and_then(|message| {
            assert_eq!(message, "hello");
            send(main_inbox, "hi")
        }).and_then(|()| {
            receive(aux_inbox)
        }).and_then(|message| {
            assert_eq!(message, "goodbye");
            send(main_inbox, "bye")
        })
    });
    core.run({
        send(aux_inbox, "hello").and_then(|()| {
            receive(main_inbox)
        }).and_then(|message| {
            assert_eq!(message, "hi");
            send(aux_inbox, "goodbye")
        }).and_then(|()| {
            receive(main_inbox)
        }).map(|message| {
            assert_eq!(message, "bye");
        })
    }).void_unwrap()
}
