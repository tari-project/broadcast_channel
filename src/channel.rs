// Use std mpsc's error types as our own
pub use std::sync::mpsc::{RecvError, RecvTimeoutError, SendError, TryRecvError};
use std::{
    fmt::Debug,
    iter::Iterator,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use arc_swap::ArcSwapOption;

use crate::atomic_counter::AtomicCounter;

const ID_MULTIPLIER: usize = 10_000;

/// Function used to create and initialise a (Sender, Receiver) tuple.
pub fn bounded<T>(size: usize, receiver_id: usize) -> (Sender<T>, Receiver<T>) {
    let mut buffer = Vec::new();
    buffer.resize(size, ArcSwapOption::new(None));
    let buffer = Arc::new(buffer);
    let wi = Arc::new(AtomicCounter::new(0));

    let sub_count = Arc::new(AtomicCounter::new(1));
    let is_sender_available = Arc::new(AtomicBool::new(true));

    (
        Sender {
            buffer: buffer.clone(),
            size,
            wi: wi.clone(),
            sub_count: sub_count.clone(),
            is_available: is_sender_available.clone(),
        },
        Receiver {
            buffer,
            size,
            wi,
            ri: AtomicCounter::new(0),
            sub_count,
            is_sender_available,
            messages_dropped_count: AtomicCounter::new(0),
            messages_dropped_state: AtomicBool::new(false),
            id: std::cmp::max(receiver_id, 1) * ID_MULTIPLIER,
        },
    )
}

/// Bare implementation of the publisher.
#[derive(Debug, Clone)]
pub struct Sender<T> {
    /// Shared reference to the circular buffer
    buffer: Arc<Vec<ArcSwapOption<T>>>,
    /// Size of the buffer
    size: usize,
    /// Write index pointer
    wi: Arc<AtomicCounter>,
    /// Number of subscribers
    sub_count: Arc<AtomicCounter>,
    /// true if this sender is still available
    is_available: Arc<AtomicBool>,
}

/// Bare implementation of the subscriber.
#[derive(Debug)]
pub struct Receiver<T> {
    /// Shared reference to the circular buffer
    buffer: Arc<Vec<ArcSwapOption<T>>>,
    /// Write index pointer
    wi: Arc<AtomicCounter>,
    /// Read index pointer
    ri: AtomicCounter,
    /// This size of the buffer
    size: usize,
    /// Number of subscribers
    sub_count: Arc<AtomicCounter>,
    /// true if the sender is available
    is_sender_available: Arc<AtomicBool>,
    /// Messages discarded counter
    messages_dropped_count: AtomicCounter,
    /// Message dropped when read
    messages_dropped_state: AtomicBool,
    /// Name of the receiver channel
    id: usize,
}

impl<T> Sender<T> {
    /// Publishes values to the circular buffer at wi % size
    ///
    /// # Arguments
    /// * `object` - owned object to be published
    pub fn broadcast(&self, object: T) -> Result<(), SendError<T>> {
        if self.sub_count.get() == 0 {
            return Err(SendError(object));
        }
        self.buffer[self.wi.get() % self.size].store(Some(Arc::new(object)));
        self.wi.inc();
        Ok(())
    }
}

/// Drop trait is used to let subscribers know that publisher is no longer available.
impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.is_available.store(false, Ordering::Relaxed);
    }
}

impl<T> PartialEq for Sender<T> {
    fn eq(&self, other: &Sender<T>) -> bool {
        Arc::ptr_eq(&self.buffer, &other.buffer)
    }
}

impl<T> Eq for Sender<T> {}

impl<T> Receiver<T> {
    /// Returns true if the sender is available, otherwise false
    fn is_sender_available(&self) -> bool {
        self.is_sender_available.load(Ordering::Relaxed)
    }

    /// Receives some atomic reference to an object if queue is not empty, or None if it is. Never
    /// Blocks
    pub fn try_recv(&self) -> Result<Arc<T>, TryRecvError> {
        self.messages_dropped_state.store(false, Ordering::Relaxed);
        if self.ri.get() == self.wi.get() {
            if self.is_sender_available() {
                return Err(TryRecvError::Empty);
            } else {
                return Err(TryRecvError::Disconnected);
            }
        }

        // Reader has not read enough to keep up with (writer - buffer size) so
        // set the reader pointer to be (writer - buffer size)
        let temp_ri = self.ri.get();
        loop {
            let val = self.buffer[self.ri.get() % self.size].load_full().unwrap();
            if self.wi.get() - self.ri.get() > self.size {
                self.ri.set(self.wi.get() - self.size);
            } else {
                if temp_ri < self.ri.get() {
                    self.messages_dropped_count
                        .set(self.messages_dropped_count.get() + self.ri.get() - temp_ri);
                    self.messages_dropped_state.store(true, Ordering::Relaxed);
                }
                self.ri.inc();
                return Ok(val);
            }
        }
    }

    /// Returns the total number of dropped messages for the entire history of this receiver
    pub fn get_dropped_messages_count(&self) -> usize {
        self.messages_dropped_count.get()
    }

    /// Indicates whether messages were dropped the last time the message buffer was read
    pub fn get_dropped_messages_state(&self) -> bool {
        self.messages_dropped_state.load(Ordering::Relaxed)
    }

    /// Returns the receiver name
    pub fn get_id(&self) -> usize {
        self.id
    }
}

/// Clone trait is used to create a Receiver which receives messages from the same Sender
impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.sub_count.inc();
        Self {
            buffer: self.buffer.clone(),
            wi: self.wi.clone(),
            ri: AtomicCounter::new(self.ri.get()),
            size: self.size,
            sub_count: Arc::clone(&self.sub_count),
            is_sender_available: self.is_sender_available.clone(),
            messages_dropped_count: AtomicCounter::new(0),
            messages_dropped_state: AtomicBool::new(self.messages_dropped_state.load(Ordering::Relaxed)),
            id: self.id + self.sub_count.get() - 1,
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.sub_count.dec();
    }
}

impl<T> PartialEq for Receiver<T> {
    fn eq(&self, other: &Receiver<T>) -> bool {
        Arc::ptr_eq(&self.buffer, &other.buffer) && self.ri == other.ri
    }
}

impl<T> Eq for Receiver<T> {}

impl<T> Iterator for Receiver<T> {
    type Item = Arc<T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.try_recv().ok()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn subcount() {
        let (sender, receiver) = bounded::<()>(1, 1);
        let receiver2 = receiver.clone();
        assert_eq!(sender.sub_count.get(), 2);
        assert_eq!(receiver.sub_count.get(), 2);
        assert_eq!(receiver2.sub_count.get(), 2);
        drop(receiver2);

        assert_eq!(sender.sub_count.get(), 1);
        assert_eq!(receiver.sub_count.get(), 1);
    }

    #[test]
    fn eq() {
        let (_sender, receiver) = bounded::<()>(1, 1);
        let receiver2 = receiver.clone();
        assert_eq!(receiver, receiver2);
    }

    #[test]
    fn bounded_channel() {
        let (sender, receiver) = bounded(1, 1);
        let receiver2 = receiver.clone();
        sender.broadcast(123).unwrap();
        assert_eq!(*receiver.try_recv().unwrap(), 123);
        assert_eq!(*receiver2.try_recv().unwrap(), 123);
    }

    #[test]
    fn bounded_channel_no_subs() {
        let (sender, receiver) = bounded(1, 1);
        let rx2 = receiver.clone();
        drop(receiver);
        assert!(sender.broadcast(123).is_ok());
        drop(rx2);
        let err = sender.broadcast(123);
        assert!(err.is_err());
    }

    #[test]
    fn bounded_channel_no_sender() {
        let (sender, receiver) = bounded::<()>(1, 1);
        drop(sender);
        assert_eq!(receiver.is_sender_available(), false);
    }

    #[test]
    fn bounded_channel_size() {
        let (sender, receiver) = bounded::<()>(3, 1);
        assert_eq!(sender.buffer.len(), 3);
        assert_eq!(receiver.buffer.len(), 3);
    }

    #[test]
    fn bounded_within_size() {
        let (sender, receiver) = bounded(3, 1);
        assert_eq!(sender.buffer.len(), 3);

        for i in 0..3 {
            sender.broadcast(i).unwrap();
        }

        let values = receiver.into_iter().map(|v| *v).collect::<Vec<_>>();
        assert_eq!(values, (0..=2).collect::<Vec<i32>>());
    }

    #[test]
    fn bounded_overflow() {
        let (sender, receiver) = bounded(3, 1);
        assert_eq!(sender.buffer.len(), 3);

        for i in 0..4 {
            sender.broadcast(i).unwrap();
        }

        let values = receiver.into_iter().map(|v| *v).collect::<Vec<_>>();
        assert_eq!(values, (1..=3).collect::<Vec<i32>>());
    }

    #[test]
    fn bounded_overflow_with_reads() {
        let (sender, receiver) = bounded(3, 1);
        assert_eq!(sender.buffer.len(), 3);
        let receiver1 = receiver.clone();

        for i in 0..3 {
            sender.broadcast(i).unwrap();
        }

        assert_eq!(*receiver.try_recv().unwrap(), 0);
        assert_eq!(receiver.get_dropped_messages_state(), false);
        assert_eq!(*receiver.try_recv().unwrap(), 1);
        assert_eq!(receiver.get_dropped_messages_state(), false);

        // "Cycle" buffer around twice
        for i in 3..11 {
            sender.broadcast(i).unwrap();
        }

        // Should be reading from the last element in the buffer
        assert_eq!(*receiver.buffer[receiver.buffer.len() - 1].load_full().unwrap(), 8);
        assert_eq!(*receiver.try_recv().unwrap(), 8);
        assert_eq!(receiver.get_dropped_messages_state(), true);
        assert_eq!(receiver.get_dropped_messages_count(), 6);

        // Cloned receiver start reading where the original receiver left off
        let mut receiver2 = receiver.clone();
        assert_eq!(*receiver2.try_recv().unwrap(), 9);
        assert_eq!(*receiver2.messages_dropped_state.get_mut(), false);

        sender.broadcast(11).unwrap();

        // Test reader has moved forward in the buffer
        let values = receiver.into_iter().map(|v| *v).collect::<Vec<_>>();
        assert_eq!(values, (9..=11).collect::<Vec<i32>>());

        // Test messages discarded
        for i in 12..20 {
            sender.broadcast(i).unwrap();
        }
        assert_eq!(*receiver1.try_recv().unwrap(), 17);
        assert_eq!(receiver1.get_dropped_messages_count(), 17);
        assert_eq!(receiver1.get_dropped_messages_state(), true);
        assert_eq!(*receiver1.try_recv().unwrap(), 18);
        assert_eq!(receiver1.get_dropped_messages_state(), false);

        assert_eq!(*receiver2.try_recv().unwrap(), 17);
        assert_eq!(receiver2.get_dropped_messages_count(), 7);
        assert_eq!(receiver2.get_dropped_messages_state(), true);
        assert_eq!(*receiver2.try_recv().unwrap(), 18);
        assert_eq!(receiver2.get_dropped_messages_state(), false);
    }
}
