#![feature(test)]
extern crate futures;
extern crate synchrotron;
extern crate test;
extern crate tokio_core;

use std::time::Instant;
use futures::{Async, Poll, future, task};

#[bench]
fn busy_synchrotron_main(b: &mut test::Bencher) {
    let mut core = synchrotron::Core::default();
    let mut run = core.run_future(future::poll_fn(|| -> Poll<(), ()> {
        task::park().unpark();
        Ok(Async::NotReady)
    }));
    b.iter(|| {
        run.turn();
    });
}

#[bench]
fn busy_synchrotron_spawn(b: &mut test::Bencher) {
    let mut core = synchrotron::Core::default();
    core.handle().spawn(future::poll_fn(|| {
        task::park().unpark();
        Ok(Async::NotReady)
    }));
    b.iter(|| {
        core.turn();
    });
}

#[bench]
fn busy_tokio(b: &mut test::Bencher) {
    let mut core = tokio_core::reactor::Core::new().unwrap();
    core.handle().spawn(future::poll_fn(|| {
        task::park().unpark();
        Ok(Async::NotReady)
    }));
    b.iter(|| {
        core.turn(None);
    });
}
