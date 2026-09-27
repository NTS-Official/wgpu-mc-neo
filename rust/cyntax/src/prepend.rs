use std::{collections::VecDeque, fmt::Debug};

use peekmore::{PeekMore, PeekMoreIterator};

#[derive(Debug)]
pub struct PrependingPeekableIterator<I: Iterator + Debug> {
    pub queue: VecDeque<I::Item>,
    inner: PeekMoreIterator<I>,
}
impl<I: Iterator + Debug> Iterator for PrependingPeekableIterator<I>
where
    I::Item: Debug,
{
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.queue.pop_front() {
            return Some(item);
        } else {
            self.inner.next()
        }
    }
}
impl<I: Iterator + Debug> PrependingPeekableIterator<I> {
    pub fn new(i: I) -> Self {
        Self {
            queue: VecDeque::new(),
            inner: i.peekmore(),
        }
    }
    pub fn peek(&mut self) -> Option<&I::Item> {
        self.peek_nth(0)
    }
    pub fn peek_nth(&mut self, n: usize) -> Option<&I::Item> {
        self.queue
            .get(n)
            .or_else(|| self.inner.peek_nth(n - self.queue.len()))
    }
    pub fn prepend(&mut self, item: I::Item) {
        self.queue.insert(0, item);
    }
    pub fn prepend_extend<J: Iterator<Item = I::Item>>(&mut self, mut iter: J) -> usize
    where
        J::Item: Debug,
    {
        let mut index = 0;
        while let Some(item) = iter.next() {
            self.queue.insert(index, item);
            index += 1;
        }
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    fn make_iter() -> PrependingPeekableIterator<std::vec::IntoIter<i32>> {
        let base = vec![10, 20, 30, 40].into_iter();
        PrependingPeekableIterator::new(base)
    }

    #[test]
    fn test_peek_nth_within_queue_only() {
        let mut it = make_iter();
        it.prepend(2);
        it.prepend(1); // Queue now: [1, 2]

        assert_eq!(it.peek_nth(0), Some(&1));
        assert_eq!(it.peek_nth(1), Some(&2));
    }

    #[test]
    fn test_peek_nth_across_queue_and_inner() {
        let mut it = make_iter();
        it.prepend(5); // Queue: [5]

        // peek_nth(0) → queue[0]
        // peek_nth(1) → inner[0] (10)
        // peek_nth(2) → inner[1] (20)
        assert_eq!(it.peek_nth(0), Some(&5));
        assert_eq!(it.peek_nth(1), Some(&10));
        assert_eq!(it.peek_nth(2), Some(&20));
    }

    #[test]
    fn test_peek_nth_only_inner() {
        let mut it = make_iter();

        // peeks from base iterator only
        assert_eq!(it.peek_nth(0), Some(&10));
        assert_eq!(it.peek_nth(1), Some(&20));
        assert_eq!(it.peek_nth(3), Some(&40));
    }

    #[test]
    fn test_peek_nth_out_of_bounds() {
        let mut it = make_iter();
        it.prepend(1);
        it.prepend(2); // Queue: [2, 1], inner: [10, 20, 30, 40]

        // total len = 6
        assert_eq!(it.peek_nth(5), Some(&40));
        assert_eq!(it.peek_nth(6), None); // Out of bounds
    }

    #[test]
    fn test_peek_nth_does_not_consume() {
        let mut it = make_iter();

        assert_eq!(it.peek_nth(1), Some(&20));
        assert_eq!(it.next(), Some(10)); // Still starts from beginning
    }

    #[test]
    fn test_peek_nth_after_partial_consumption() {
        let mut it = make_iter();
        it.next(); // consume 10

        assert_eq!(it.peek_nth(0), Some(&20));
        assert_eq!(it.peek_nth(2), Some(&40));
        assert_eq!(it.peek_nth(3), None);
    }

    #[test]
    fn test_peek_nth_after_prepend_extend() {
        let mut it = make_iter();
        it.prepend_extend(vec![7, 8, 9].into_iter()); // Queue: [7, 8, 9]

        assert_eq!(it.peek_nth(0), Some(&7));
        assert_eq!(it.peek_nth(2), Some(&9));
        assert_eq!(it.peek_nth(3), Some(&10));
    }
}
