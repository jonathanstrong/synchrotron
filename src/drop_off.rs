//! A single-threaded one-shot channel.
//!
//! # Example
//!
//! ```
//! use synchrotron::drop_off;
//!
//! let (sender, receiver) = drop_off::new();
//! let receiver = receiver.take().unwrap_err().unwrap();
//! sender.send(42).unwrap();
//! assert_eq!(42, receiver.take().unwrap());
//! ```

use std::cell::RefCell;
use std::rc::{Rc, Weak};

/// Sending end of the channel.
#[derive(Debug)]
pub struct Sender<T>(Weak<RefCell<Option<T>>>);

impl<T> Sender<T> {
    /// If the receiver is still alive, then the result will be sent
    /// successfully.  Otherwise, it returns `Err(value)`.
    pub fn send(self, value: T) -> Result<(), T> {
        match self.0.upgrade() {
            None => Err(value),
            Some(ref_cell) => {
                *ref_cell.borrow_mut() = Some(value);
                Ok(())
            }
        }
    }
}

/// Receiving end of the channel.
#[derive(Debug)]
pub struct Receiver<T>(Rc<RefCell<Option<T>>>);

impl<T> Receiver<T> {
    /// If a value has been received, take it out and return `Ok`.  If a value
    /// has not been received and the `Sender` still exists, `Err(Some(self))`
    /// is returned.  If a value has not been received and the `Sender` has
    /// been dropped, `Err(None)` is returned.
    pub fn take(self) -> Result<T, Option<Self>> {
        let taken = self.0.borrow_mut().take();
        match taken {
            None => Err({
                if Rc::weak_count(&self.0) == 0 {
                    None
                } else {
                    Some(self)
                }
            }),
            Some(value) => Ok(value),
        }
    }
}

/// Create a single-threaded one-shot channel.
pub fn new<T>() -> (Sender<T>, Receiver<T>) {
    let rc = Rc::new(RefCell::new(None));
    (Sender(Rc::downgrade(&rc)), Receiver(rc))
}
