//! A target's queue: the batches it has yet to get, oldest first, in memory. It only fills while
//! the target is slow or down; when full, the oldest batch makes way for the newest. Batches are
//! shared, so a batch queued for five targets is held once.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

use crate::wire::Batch;

pub struct Queue {
    items: Mutex<VecDeque<Arc<Batch>>>,
    capacity: AtomicUsize,
    wake: Notify,
}

impl Queue {
    pub fn new(capacity: usize) -> Queue {
        Queue {
            items: Mutex::new(VecDeque::new()),
            capacity: AtomicUsize::new(capacity.max(1)),
            wake: Notify::new(),
        }
    }

    pub fn set_capacity(&self, capacity: usize) {
        self.capacity.store(capacity.max(1), Relaxed);
    }

    /// Adds a batch at the back, dropping the oldest while over capacity. Answers how many went.
    pub fn push(&self, batch: Arc<Batch>) -> u64 {
        let dropped = {
            let mut q = self.lock();
            q.push_back(batch);
            let capacity = self.capacity.load(Relaxed);
            let mut dropped = 0;
            while q.len() > capacity {
                q.pop_front();
                dropped += 1;
            }
            dropped
        };
        self.wake.notify_one();
        dropped
    }

    pub fn pop(&self) -> Option<Arc<Batch>> {
        self.lock().pop_front()
    }

    /// Puts back, at the front, a batch the lane took but did not finish with.
    pub fn put_back(&self, batch: Arc<Batch>) {
        self.lock().push_front(batch);
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Waits for the next push (at once if one came since the last wait).
    pub async fn wait(&self) {
        self.wake.notified().await;
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<Arc<Batch>>> {
        self.items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn batch(n: u64) -> Arc<Batch> {
        Arc::new(Batch {
            received_at_ms: n,
            user_agent: String::new(),
            via: String::new(),
            body: Bytes::new(),
        })
    }

    fn drain(q: &Queue) -> Vec<u64> {
        std::iter::from_fn(|| q.pop())
            .map(|b| b.received_at_ms)
            .collect()
    }

    #[test]
    fn oldest_first_and_the_oldest_makes_way() {
        let q = Queue::new(3);
        assert_eq!(
            (q.push(batch(1)), q.push(batch(2)), q.push(batch(3))),
            (0, 0, 0)
        );
        assert_eq!(q.push(batch(4)), 1);
        assert_eq!(q.len(), 3);
        assert_eq!(drain(&q), [2, 3, 4]);
        assert!(q.is_empty());
    }

    #[test]
    fn a_batch_put_back_comes_out_first() {
        let q = Queue::new(10);
        q.push(batch(2));
        q.put_back(batch(1));
        assert_eq!(drain(&q), [1, 2]);
    }

    #[test]
    fn a_smaller_capacity_applies_at_the_next_push() {
        let q = Queue::new(10);
        for n in 1..=5 {
            q.push(batch(n));
        }
        q.set_capacity(2);
        assert_eq!(q.push(batch(6)), 4);
        assert_eq!(drain(&q), [5, 6]);
    }

    #[tokio::test]
    async fn a_push_wakes_the_waiter_even_if_it_came_first() {
        let q = Queue::new(10);
        q.push(batch(1));
        // The push came before the wait: its wake-up is kept, so this returns at once.
        tokio::time::timeout(std::time::Duration::from_secs(1), q.wait())
            .await
            .unwrap();
    }
}
