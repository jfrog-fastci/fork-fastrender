use std::collections::VecDeque;

use crate::gc::OwnedGcHandle;

pub trait Discard {
  fn discard(self);
}

impl<T> Discard for OwnedGcHandle<T> {
  #[inline]
  fn discard(self) {
    self.release();
  }
}

#[derive(Debug)]
pub struct TeardownQueue<T: Discard> {
  queue: VecDeque<T>,
}

impl<T: Discard> Default for TeardownQueue<T> {
  #[inline]
  fn default() -> Self {
    Self {
      queue: VecDeque::new(),
    }
  }
}

impl<T: Discard> TeardownQueue<T> {
  #[inline]
  pub fn new() -> Self {
    Self::default()
  }

  #[inline]
  pub fn push_back(&mut self, value: T) {
    self.queue.push_back(value);
  }

  #[inline]
  pub fn pop_front(&mut self) -> Option<T> {
    self.queue.pop_front()
  }

  #[inline]
  pub fn peek_front(&self) -> Option<&T> {
    self.queue.front()
  }

  #[inline]
  pub fn len(&self) -> usize {
    self.queue.len()
  }

  #[inline]
  pub fn is_empty(&self) -> bool {
    self.queue.is_empty()
  }

  #[inline]
  pub fn teardown(&mut self) {
    while let Some(value) = self.queue.pop_front() {
      value.discard();
    }
  }
}

impl<T: Discard> Drop for TeardownQueue<T> {
  fn drop(&mut self) {
    self.teardown();
  }
}
