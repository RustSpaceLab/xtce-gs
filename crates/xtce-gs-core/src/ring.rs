//! A fixed-capacity ring that overwrites its oldest element.
//!
//! Telemetry history is unbounded and memory is not, so the history a ground station keeps in
//! RAM is a window: the last *n* points, with the oldest falling off the back. `VecDeque`
//! would do this, and does not, because its `pop_front`-then-`push_back` is two operations
//! and its contents cannot be handed to a plot as two slices without a copy.

/// A ring buffer of fixed capacity.
///
/// Pushing into a full ring overwrites the oldest element. Iteration is oldest-first.
#[derive(Clone, Debug)]
pub struct RingBuffer<T> {
    items: Vec<T>,
    /// Index of the oldest element, once `items.len() == capacity`.
    head: usize,
    capacity: usize,
}

impl<T> RingBuffer<T> {
    /// An empty ring holding at most `capacity` elements.
    ///
    /// A capacity of zero is allowed and makes every push a no-op; the caller that configured
    /// a zero-length history gets an empty plot, not a panic on the decode thread.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            items: Vec::with_capacity(capacity.min(4096)),
            head: 0,
            capacity,
        }
    }

    /// How many elements this ring can hold.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many elements it holds now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether it holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether the next push will overwrite.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.capacity > 0 && self.items.len() == self.capacity
    }

    /// Appends an element, overwriting the oldest if the ring is full.
    ///
    /// Returns the element that was overwritten, if any.
    pub fn push(&mut self, item: T) -> Option<T> {
        if self.capacity == 0 {
            return Some(item);
        }
        if self.items.len() < self.capacity {
            self.items.push(item);
            return None;
        }
        let slot = self.items.get_mut(self.head)?;
        let evicted = std::mem::replace(slot, item);
        self.head = (self.head + 1) % self.capacity;
        Some(evicted)
    }

    /// The element `index` places after the oldest.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.items.len() {
            return None;
        }
        let physical = if self.items.len() < self.capacity {
            index
        } else {
            (self.head + index) % self.capacity
        };
        self.items.get(physical)
    }

    /// The element `index` places after the oldest, mutably.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        if index >= self.items.len() {
            return None;
        }
        let physical = if self.items.len() < self.capacity {
            index
        } else {
            (self.head + index) % self.capacity.max(1)
        };
        self.items.get_mut(physical)
    }

    /// The most recently pushed element.
    #[must_use]
    pub fn last(&self) -> Option<&T> {
        self.get(self.len().checked_sub(1)?)
    }

    /// The most recently pushed element, mutably.
    ///
    /// The event log collapses a line repeated back to back by bumping a counter through
    /// this, which is the difference between O(1) and rebuilding the ring on the exact path
    /// that fires thousands of times a second when a link is failing.
    pub fn last_mut(&mut self) -> Option<&mut T> {
        let index = self.len().checked_sub(1)?;
        self.get_mut(index)
    }

    /// The oldest element.
    #[must_use]
    pub fn first(&self) -> Option<&T> {
        self.get(0)
    }

    /// The contents as two contiguous slices, oldest first.
    ///
    /// The second is empty until the ring wraps. Decimation walks these directly rather than
    /// through the iterator, because the inner loop of LTTB is a slice scan and the modulo in
    /// [`RingBuffer::get`] would be in it.
    #[must_use]
    pub fn as_slices(&self) -> (&[T], &[T]) {
        if self.items.len() < self.capacity || self.head == 0 {
            (&self.items, &[])
        } else {
            // The oldest element is at `head`, so the tail of the vector comes first.
            let (wrapped, oldest) = self.items.split_at(self.head);
            (oldest, wrapped)
        }
    }

    /// Iterates oldest-first.
    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        let (old, new) = self.as_slices();
        old.iter().chain(new.iter())
    }

    /// Drops everything, keeping the capacity.
    pub fn clear(&mut self) {
        self.items.clear();
        self.head = 0;
    }

    /// Changes the capacity, keeping the newest elements that still fit.
    ///
    /// The operator moving the history slider must not lose what is already on screen, so
    /// growing keeps everything and shrinking keeps the *newest* — the opposite of what
    /// `Vec::truncate` would do.
    pub fn resize(&mut self, capacity: usize) {
        if capacity == self.capacity {
            return;
        }
        let old_capacity = self.capacity;
        let head = self.head;
        // Moving the elements out costs one `Option` per slot and no `T: Clone` bound, which
        // matters because a sample owns an `Arc` and this must not be a deep copy.
        let mut slots: Vec<Option<T>> = std::mem::take(&mut self.items)
            .into_iter()
            .map(Some)
            .collect();
        let len = slots.len();
        let skip = len.saturating_sub(capacity);

        let mut rebuilt = Vec::with_capacity(capacity.min(len));
        for logical in skip..len {
            let physical = if len < old_capacity {
                logical
            } else {
                (head + logical) % old_capacity.max(1)
            };
            if let Some(item) = slots.get_mut(physical).and_then(Option::take) {
                rebuilt.push(item);
            }
        }

        self.items = rebuilt;
        self.head = 0;
        self.capacity = capacity;
    }
}

impl<'a, T> IntoIterator for &'a RingBuffer<T> {
    type Item = &'a T;
    type IntoIter = std::iter::Chain<std::slice::Iter<'a, T>, std::slice::Iter<'a, T>>;

    fn into_iter(self) -> Self::IntoIter {
        let (old, new) = self.as_slices();
        old.iter().chain(new.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_overwrites_the_oldest() {
        let mut ring = RingBuffer::with_capacity(3);
        for value in 1..=5 {
            ring.push(value);
        }
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![3, 4, 5]);
        assert_eq!(ring.first(), Some(&3));
        assert_eq!(ring.last(), Some(&5));
    }

    #[test]
    fn push_returns_what_it_evicted() {
        let mut ring = RingBuffer::with_capacity(2);
        assert_eq!(ring.push(1), None);
        assert_eq!(ring.push(2), None);
        assert_eq!(ring.push(3), Some(1));
    }

    #[test]
    fn the_two_slices_are_the_whole_ring_in_order() {
        let mut ring = RingBuffer::with_capacity(4);
        for value in 1..=6 {
            ring.push(value);
        }
        let (old, new) = ring.as_slices();
        let joined: Vec<i32> = old.iter().chain(new.iter()).copied().collect();
        assert_eq!(joined, vec![3, 4, 5, 6]);
        assert_eq!(joined, ring.iter().copied().collect::<Vec<_>>());
    }

    #[test]
    fn a_zero_capacity_ring_accepts_nothing_and_survives() {
        let mut ring = RingBuffer::with_capacity(0);
        assert_eq!(ring.push(7), Some(7));
        assert!(ring.is_empty());
        assert_eq!(ring.last(), None);
    }

    #[test]
    fn indexing_is_logical_not_physical() {
        let mut ring = RingBuffer::with_capacity(3);
        for value in 1..=4 {
            ring.push(value);
        }
        assert_eq!(ring.get(0), Some(&2));
        assert_eq!(ring.get(2), Some(&4));
        assert_eq!(ring.get(3), None);
    }
}
