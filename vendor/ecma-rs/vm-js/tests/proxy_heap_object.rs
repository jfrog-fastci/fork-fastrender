use vm_js::{Heap, HeapLimits, Value, VmError};

#[test]
fn proxy_heap_object_traces_target_and_handler_until_revoked() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let (target, handler, proxy) = {
    let mut scope = heap.scope();
    let target = scope.alloc_object()?;
    let handler = scope.alloc_object()?;
    let proxy = scope.alloc_proxy(target, handler)?;
    (target, handler, proxy)
  };

  let proxy_root = heap.add_root(Value::Object(proxy))?;

  heap.collect_garbage();
  assert!(heap.is_valid_object(proxy));
  assert!(
    heap.is_valid_object(target),
    "proxy should strongly trace [[ProxyTarget]]"
  );
  assert!(
    heap.is_valid_object(handler),
    "proxy should strongly trace [[ProxyHandler]]"
  );

  assert!(heap.is_proxy_object(proxy));
  assert_eq!(heap.proxy_target(proxy)?, Some(target));
  assert_eq!(heap.proxy_handler(proxy)?, Some(handler));

  heap.proxy_revoke(proxy)?;
  assert_eq!(heap.proxy_target(proxy)?, None);
  assert_eq!(heap.proxy_handler(proxy)?, None);

  heap.collect_garbage();
  assert!(
    !heap.is_valid_object(target),
    "revoked proxies should not keep [[ProxyTarget]] alive"
  );
  assert!(
    !heap.is_valid_object(handler),
    "revoked proxies should not keep [[ProxyHandler]] alive"
  );

  heap.remove_root(proxy_root);
  Ok(())
}

