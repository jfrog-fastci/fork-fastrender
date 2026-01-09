use super::event_loop::EventLoop;
use crate::error::{Error, Result};
use std::cell::RefCell;
use std::rc::Rc;

type PromiseResult<T> = Result<T>;

type Reaction<Host, T> =
  Box<dyn FnOnce(&mut Host, &mut EventLoop<Host>, PromiseResult<T>) -> Result<()> + 'static>;

struct PromiseState<Host: 'static, T: Clone + 'static> {
  result: Option<PromiseResult<T>>,
  reactions: Vec<Reaction<Host, T>>,
}

impl<Host: 'static, T: Clone + 'static> Default for PromiseState<Host, T> {
  fn default() -> Self {
    Self {
      result: None,
      reactions: Vec::new(),
    }
  }
}

/// Minimal, deterministic Promise-like type integrated with FastRender's [`EventLoop`].
///
/// This is **not** a full ECMAScript Promise implementation. It exists as scaffolding for JS host
/// APIs (e.g. `fetch()`) so we can deterministically test event-loop task/microtask ordering before
/// a full JS engine embedding exists.
pub struct JsPromise<Host: 'static, T: Clone + 'static> {
  state: Rc<RefCell<PromiseState<Host, T>>>,
}

/// Resolver pair for a [`JsPromise`].
pub struct JsPromiseResolver<Host: 'static, T: Clone + 'static> {
  state: Rc<RefCell<PromiseState<Host, T>>>,
}

/// The value returned from a Promise `then` callback: either an immediate value or another Promise.
pub enum JsPromiseValue<Host: 'static, T: Clone + 'static> {
  Value(T),
  Promise(JsPromise<Host, T>),
}

impl<Host: 'static, T: Clone + 'static> Clone for JsPromise<Host, T> {
  fn clone(&self) -> Self {
    Self {
      state: Rc::clone(&self.state),
    }
  }
}

impl<Host: 'static, T: Clone + 'static> Clone for JsPromiseResolver<Host, T> {
  fn clone(&self) -> Self {
    Self {
      state: Rc::clone(&self.state),
    }
  }
}

impl<Host: 'static, T: Clone + 'static> JsPromise<Host, T> {
  /// Create a new pending Promise with its resolver.
  pub fn new() -> (Self, JsPromiseResolver<Host, T>) {
    let state = Rc::new(RefCell::new(PromiseState::default()));
    (
      Self {
        state: Rc::clone(&state),
      },
      JsPromiseResolver { state },
    )
  }

  fn add_reaction(&self, event_loop: &mut EventLoop<Host>, reaction: Reaction<Host, T>) -> Result<()> {
    let maybe_result = { self.state.borrow().result.clone() };
    if let Some(result) = maybe_result {
      // Promise reactions run as microtasks, even when the promise is already settled.
      event_loop.queue_microtask(move |host, event_loop| reaction(host, event_loop, result))?;
      return Ok(());
    }

    self.state.borrow_mut().reactions.push(reaction);
    Ok(())
  }

  /// Attach a fulfillment handler, returning a new chained Promise.
  ///
  /// If this Promise rejects, the rejection is propagated to the returned Promise.
  pub fn then<U: Clone + 'static>(
    &self,
    event_loop: &mut EventLoop<Host>,
    on_fulfilled: impl FnOnce(&mut Host, &mut EventLoop<Host>, T) -> Result<JsPromiseValue<Host, U>> + 'static,
  ) -> Result<JsPromise<Host, U>> {
    let (next, next_resolver) = JsPromise::<Host, U>::new();

    self.add_reaction(
      event_loop,
      Box::new(move |host, event_loop, result| {
        match result {
          Ok(value) => match on_fulfilled(host, event_loop, value)? {
            JsPromiseValue::Value(v) => next_resolver.resolve(event_loop, v)?,
            JsPromiseValue::Promise(p) => {
              // Promise flattening: if the handler returns a promise, the chained promise settles
              // to its eventual value.
              let next_resolver = next_resolver.clone();
              p.add_reaction(
                event_loop,
                Box::new(move |_host, event_loop, result| {
                  match result {
                    Ok(v) => next_resolver.resolve(event_loop, v)?,
                    Err(err) => next_resolver.reject(event_loop, err)?,
                  }
                  Ok(())
                }),
              )?;
            }
          },
          Err(err) => next_resolver.reject(event_loop, err)?,
        }
        Ok(())
      }),
    )?;

    Ok(next)
  }
}

impl<Host: 'static, T: Clone + 'static> JsPromiseResolver<Host, T> {
  pub fn resolve(&self, event_loop: &mut EventLoop<Host>, value: T) -> Result<()> {
    self.finish(event_loop, Ok(value))
  }

  pub fn reject(&self, event_loop: &mut EventLoop<Host>, error: Error) -> Result<()> {
    self.finish(event_loop, Err(error))
  }

  fn finish(&self, event_loop: &mut EventLoop<Host>, result: PromiseResult<T>) -> Result<()> {
    let reactions = {
      let mut state = self.state.borrow_mut();
      if state.result.is_some() {
        // Spec behavior: resolving a settled promise is a no-op.
        return Ok(());
      }
      state.result = Some(result.clone());
      std::mem::take(&mut state.reactions)
    };

    for reaction in reactions {
      let result = result.clone();
      event_loop.queue_microtask(move |host, event_loop| reaction(host, event_loop, result))?;
    }
    Ok(())
  }
}
