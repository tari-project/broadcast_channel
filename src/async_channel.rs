use std::{pin::Pin, sync::Arc, task::Poll};

use crossbeam_channel as mpsc;
use futures::{
    task::{self, AtomicWaker},
    Sink,
    Stream,
};
use log::*;

use crate::channel::{bounded as raw_bounded, Receiver, SendError, Sender, TryRecvError};

const LOG_TARGET: &str = "tari_broadcast_channel::async_channel";
const ID_MULTIPLIER: usize = 10_000;

pub fn bounded<T>(size: usize, receiver_id: usize) -> (Publisher<T>, Subscriber<T>) {
    let (sender, receiver) = raw_bounded(size, receiver_id);
    let (waker, sleeper) = alarm();
    debug!(
        target: LOG_TARGET,
        "Buffer created with ID '{}' ({}) and size {}, consecutive subscriptions add 1 to the ID.",
        receiver_id * ID_MULTIPLIER,
        receiver_id,
        size
    );
    (Publisher { sender, waker }, Subscriber { receiver, sleeper })
}

#[derive(Debug)]
pub struct Publisher<T> {
    sender: Sender<T>,
    waker: Waker,
}

impl<T> Sink<T> for Publisher<T> {
    type Error = SendError<T>;

    fn poll_ready(self: Pin<&mut Self>, _: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        self.waker.collect_new_wakers();
        self.sender.broadcast(item).map(|_| {
            self.waker.wake_all();
        })
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl<T> PartialEq for Publisher<T> {
    fn eq(&self, other: &Publisher<T>) -> bool {
        self.sender == other.sender
    }
}

impl<T> Eq for Publisher<T> {}

pub struct Subscriber<T> {
    receiver: Receiver<T>,
    sleeper: Sleeper,
}

impl<T> Stream for Subscriber<T> {
    type Item = Arc<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        self.sleeper.register(cx.waker());
        match self.receiver.try_recv() {
            Ok(item) => {
                if self.receiver.get_dropped_messages_state() {
                    debug!(
                        target: LOG_TARGET,
                        "Subscriber with ID '{}' has {} buffer messages dropped.",
                        self.receiver.get_id(),
                        self.receiver.get_dropped_messages_count()
                    );
                }
                Poll::Ready(Some(item))
            },
            Err(error) => {
                trace!(
                    target: LOG_TARGET,
                    "Subscriber with ID '{}', receive error: '{}'",
                    self.receiver.get_id(),
                    error
                );
                match error {
                    TryRecvError::Empty => Poll::Pending,
                    TryRecvError::Disconnected => Poll::Ready(None),
                }
            },
        }
    }
}

impl<T> Clone for Subscriber<T> {
    fn clone(&self) -> Self {
        trace!(
            target: LOG_TARGET,
            "Subscriber with ID '{}' was cloned, new subscription.",
            self.receiver.get_id()
        );
        Self {
            receiver: self.receiver.clone(),
            sleeper: self.sleeper.clone(),
        }
    }
}

impl<T: Send> PartialEq for Subscriber<T> {
    fn eq(&self, other: &Subscriber<T>) -> bool {
        self.receiver == other.receiver
    }
}

impl<T: Send> Eq for Subscriber<T> {}

impl<T> Subscriber<T> {
    /// Returns the receiver id
    pub fn get_receiver_id(&self) -> usize {
        self.receiver.get_id()
    }

    /// Returns the amount of dropped messages for the receiver
    pub fn get_dropped_messages_count(&self) -> usize {
        self.receiver.get_dropped_messages_count()
    }

    /// Returns the receiver's dropped messgages state for the last message read
    pub fn get_dropped_messages_state(&self) -> bool {
        self.receiver.get_dropped_messages_state()
    }
}
// Helper struct used by sync and async implementations to wake Tasks / Threads
#[derive(Debug)]
pub struct Waker {
    /// Vector of Wakers to use to wake up subscribers.
    wakers: Vec<Arc<AtomicWaker>>,
    /// A mpsc Receiver used to receive Wakers
    receiver: mpsc::Receiver<Arc<AtomicWaker>>,
}

impl Waker {
    fn wake_all(&self) {
        for waker in &self.wakers {
            waker.wake();
        }
    }

    /// Receive any new Wakers and add them to the wakers Vec. These will be used to wake up the
    /// subscribers when a message is published
    fn collect_new_wakers(&mut self) {
        while let Ok(receiver) = self.receiver.try_recv() {
            self.wakers.push(receiver);
        }
    }
}

/// Helper struct used by sync and async implementations to register Tasks / Threads to
/// be woken up.
#[derive(Debug)]
pub struct Sleeper {
    /// Current Waker to be woken up
    waker: Arc<AtomicWaker>,
    /// mpsc Sender used to send Wakers to the Publisher
    sender: mpsc::Sender<Arc<AtomicWaker>>,
}

impl Sleeper {
    fn register(&self, waker: &task::Waker) {
        self.waker.register(waker);
    }
}

impl Clone for Sleeper {
    fn clone(&self) -> Self {
        let waker = Arc::new(AtomicWaker::new());
        // Send the new waker to the publisher.
        // If this fails (Receiver disconnected), presumably the Publisher
        // has dropped and when this is polled for the first time, the
        // Stream will end.
        let _ = self.sender.send(Arc::clone(&waker));
        Self {
            waker,
            sender: self.sender.clone(),
        }
    }
}

/// Function used to create a ( Waker, Sleeper ) tuple.
pub fn alarm() -> (Waker, Sleeper) {
    let (sender, receiver) = mpsc::unbounded();
    let waker = Arc::new(AtomicWaker::new());
    let wakers = vec![Arc::clone(&waker)];
    (Waker { wakers, receiver }, Sleeper { waker, sender })
}

#[cfg(test)]
mod test {
    use futures::{executor::block_on, stream, StreamExt};

    use crate::async_channel;

    #[test]
    fn channel() {
        let (publisher1, subscriber1) = super::bounded(10, 1);
        let subscriber2 = subscriber1.clone();
        let subscriber3 = subscriber1.clone();

        assert_eq!(subscriber1.get_receiver_id(), 10000);
        assert_eq!(subscriber2.get_receiver_id(), 10001);
        assert_eq!(subscriber3.get_receiver_id(), 10002);

        let (_publisher2, subscriber4): (async_channel::Publisher<usize>, async_channel::Subscriber<usize>) =
            super::bounded(10, 2);
        let subscriber5 = subscriber4.clone();
        let subscriber6 = subscriber4.clone();

        assert_eq!(subscriber4.get_receiver_id(), 20000);
        assert_eq!(subscriber5.get_receiver_id(), 20001);
        assert_eq!(subscriber6.get_receiver_id(), 20002);

        block_on(async move {
            stream::iter(1..15).map(|i| Ok(i)).forward(publisher1).await.unwrap();
        });

        let received1: Vec<u32> = block_on(async { subscriber1.map(|x| *x).collect().await });
        let received2: Vec<u32> = block_on(async { subscriber2.map(|x| *x).collect().await });
        // Test that only the last 10 elements are in the received list.
        let expected = (5..15).collect::<Vec<u32>>();
        assert_eq!(received1, expected);
        assert_eq!(received2, expected);
        // Test messages discarded
        subscriber3.receiver.try_recv().unwrap();
        assert_eq!(subscriber3.receiver.get_dropped_messages_state(), true);
        assert_eq!(subscriber3.receiver.get_dropped_messages_count(), 4);
        subscriber3.receiver.try_recv().unwrap();
        assert_eq!(subscriber3.receiver.get_dropped_messages_state(), false);
        assert_eq!(subscriber3.receiver.get_dropped_messages_count(), 4);
    }
}
