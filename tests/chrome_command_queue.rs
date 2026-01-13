use fastrender::js::chrome_api::ChromeCommand;
use fastrender::js::chrome_command_queue::ChromeCommandQueue;

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
  queue.push(ChromeCommand::Navigate { url: "1".to_string() });
  queue.push(ChromeCommand::Navigate { url: "2".to_string() });
  queue.push(ChromeCommand::Navigate { url: "3".to_string() });

  // Overflow: "1" should be dropped.
  queue.push(ChromeCommand::Navigate { url: "4".to_string() });

  assert_eq!(
    queue.take_all(),
    vec![
      ChromeCommand::Navigate { url: "2".to_string() },
      ChromeCommand::Navigate { url: "3".to_string() },
      ChromeCommand::Navigate { url: "4".to_string() },
    ]
  );
}
