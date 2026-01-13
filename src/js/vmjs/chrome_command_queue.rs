use crate::js::chrome_api::{ChromeApiHost, ChromeCommand};
use std::collections::VecDeque;

const DEFAULT_MAX_QUEUE_LEN: usize = 1024;

/// A reusable bounded FIFO buffer for chrome JS commands.
///
/// This is primarily intended for:
/// - unit tests that want to execute chrome JS and inspect which commands were emitted
/// - browser embeddings that want to collect commands and apply them in a separate step
///
/// The queue is bounded: when at capacity, pushing a new command drops the *oldest* command.
#[derive(Debug)]
pub struct ChromeCommandQueue {
  queue: VecDeque<ChromeCommand>,
  max_len: usize,
}

impl ChromeCommandQueue {
  /// Create an empty queue with the default max length.
  pub fn new() -> Self {
    Self::with_max_len(DEFAULT_MAX_QUEUE_LEN)
  }

  /// Create an empty queue with a custom max length.
  ///
  /// A max length of `0` means all pushed commands are dropped.
  pub fn with_max_len(max_len: usize) -> Self {
    Self {
      queue: VecDeque::new(),
      max_len,
    }
  }

  /// Push a new command onto the back of the queue.
  ///
  /// When the queue has reached its max length, the oldest command is dropped first.
  pub fn push(&mut self, cmd: ChromeCommand) {
    if self.max_len == 0 {
      return;
    }
    if self.queue.len() >= self.max_len {
      self.queue.pop_front();
    }
    self.queue.push_back(cmd);
  }

  /// Drain the entire queue, returning commands in FIFO order.
  pub fn take_all(&mut self) -> Vec<ChromeCommand> {
    self.queue.drain(..).collect()
  }

  /// Peek at the next command to be drained.
  pub fn peek(&self) -> Option<&ChromeCommand> {
    self.queue.front()
  }
}

impl Default for ChromeCommandQueue {
  fn default() -> Self {
    Self::new()
  }
}

impl ChromeApiHost for ChromeCommandQueue {
  fn chrome_dispatch(&mut self, cmd: ChromeCommand) -> Result<(), crate::error::Error> {
    self.push(cmd);
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn fifo_ordering() {
    let mut queue = ChromeCommandQueue::with_max_len(16);
    let cmd1 = ChromeCommand::Back;
    let cmd2 = ChromeCommand::Forward;
    let cmd3 = ChromeCommand::Navigate {
      url: "https://example.com/".to_string(),
    };

    queue.push(cmd1.clone());
    queue.push(cmd2.clone());
    queue.push(cmd3.clone());

    assert_eq!(queue.peek(), Some(&cmd1));
    assert_eq!(queue.take_all(), vec![cmd1, cmd2, cmd3]);
  }

  #[test]
  fn take_all_drains() {
    let mut queue = ChromeCommandQueue::with_max_len(16);
    queue.push(ChromeCommand::NewTab { url: None });
    assert!(queue.peek().is_some());

    let drained = queue.take_all();
    assert_eq!(drained.len(), 1);
    assert!(queue.peek().is_none());
    assert!(queue.take_all().is_empty());
  }

  #[test]
  fn max_queue_len_drops_oldest() {
    let mut queue = ChromeCommandQueue::with_max_len(3);
    queue.push(ChromeCommand::Navigate {
      url: "1".to_string(),
    });
    queue.push(ChromeCommand::Navigate {
      url: "2".to_string(),
    });
    queue.push(ChromeCommand::Navigate {
      url: "3".to_string(),
    });

    // Overflow: "1" should be dropped.
    queue.push(ChromeCommand::Navigate {
      url: "4".to_string(),
    });

    assert_eq!(
      queue.take_all(),
      vec![
        ChromeCommand::Navigate { url: "2".to_string() },
        ChromeCommand::Navigate { url: "3".to_string() },
        ChromeCommand::Navigate { url: "4".to_string() },
      ]
    );
  }
}
