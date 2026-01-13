use fastrender::dom2;
use fastrender::js::webidl::VmJsWebIdlBindingsHostDispatch;
use fastrender::js::window_realm::DomBindingsBackend;
use fastrender::js::window_timers::VmJsEventLoopHooks;
use fastrender::js::{
  CurrentScriptHost, DomHost, DocumentHostState, EventLoop, WindowRealm, WindowRealmConfig,
  WindowRealmHost,
};
use selectors::context::QuirksMode;
use vm_js::{Value, VmError, VmHost};

#[test]
fn window_realm_webidl_element_insert_adjacent_apis_mutate_dom() -> Result<(), VmError> {
  struct Host {
    document: DocumentHostState,
    realm: WindowRealm,
    webidl_bindings_host: VmJsWebIdlBindingsHostDispatch<Host>,
  }

  impl DomHost for Host {
    fn with_dom<R, F>(&self, f: F) -> R
    where
      F: FnOnce(&dom2::Document) -> R,
    {
      DomHost::with_dom(&self.document, f)
    }

    fn mutate_dom<R, F>(&mut self, f: F) -> R
    where
      F: FnOnce(&mut dom2::Document) -> (R, bool),
    {
      DomHost::mutate_dom(&mut self.document, f)
    }
  }

  impl WindowRealmHost for Host {
    fn vm_host_and_window_realm(
      &mut self,
    ) -> fastrender::error::Result<(&mut dyn VmHost, &mut WindowRealm)> {
      Ok((&mut self.document, &mut self.realm))
    }

    fn webidl_bindings_host(&mut self) -> Option<&mut dyn webidl_vm_js::WebIdlBindingsHost> {
      Some(&mut self.webidl_bindings_host)
    }
  }

  let document = DocumentHostState::new(dom2::Document::new(QuirksMode::NoQuirks));
  let mut realm = WindowRealm::new(
    WindowRealmConfig::new("https://example.invalid/")
      .with_current_script_state(document.current_script_state().clone())
      .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
  )?;
  let webidl_bindings_host =
    VmJsWebIdlBindingsHostDispatch::<Host>::new(realm.global_object());
  let mut host = Host {
    document,
    realm,
    webidl_bindings_host,
  };

  let mut event_loop = EventLoop::<Host>::new();
  let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host)
    .expect("create VmJsEventLoopHooks");
  hooks.set_event_loop(&mut event_loop);

  let (vm_host, window) = host
    .vm_host_and_window_realm()
    .expect("Host::vm_host_and_window_realm");
  let out = window.exec_script_with_host_and_hooks(
    vm_host,
    &mut hooks,
    r#"
      (() => {
        const t = document.createElement('div');
        t.id = 't';

        // Ensure these APIs come from the WebIDL-generated prototype, not per-wrapper fallback shims.
        if (Object.prototype.hasOwnProperty.call(t, 'insertAdjacentHTML')) return false;
        if (Object.prototype.hasOwnProperty.call(t, 'insertAdjacentElement')) return false;
         if (Object.prototype.hasOwnProperty.call(t, 'insertAdjacentText')) return false;
 
         t.insertAdjacentHTML('beforeend', '<span id="a">x</span>');
         let called = false;
         t.insertAdjacentHTML('beforeend', {
           toString() {
             called = true;
             return '<span id="b">z</span>';
           },
         });
         if (!called) return false;
         if (!t.firstChild) return false;
         if (t.firstChild.tagName !== 'SPAN') return false;
         if (t.firstChild.id !== 'a') return false;
         if (!t.lastChild) return false;
         if (t.lastChild.tagName !== 'SPAN') return false;
         if (t.lastChild.id !== 'b') return false;
         if (t.textContent !== 'xz') return false;
 
         const p = document.createElement('p');
         p.id = 'p';
         const inserted = t.insertAdjacentElement('afterbegin', p);
         if (inserted !== p) return false;
         if (t.firstChild !== p) return false;
 
         t.insertAdjacentText('beforeend', 'y');
         if (t.textContent !== 'xzy') return false;

         // Detached element insertAdjacentElement beforebegin/afterend is a no-op that returns null.
         const s = document.createElement('span');
         if (t.insertAdjacentElement('beforebegin', s) !== null) return false;
         if (s.parentNode !== null) return false;

         // Detached element insertAdjacentText beforebegin/afterend is a no-op.
         t.insertAdjacentText('afterend', 'nope');
         if (t.textContent !== 'xzy') return false;

         // Invalid position throws SyntaxError.
         try {
           t.insertAdjacentText('bogus', 'x');
           return false;
         } catch (e) {
           if (!e || e.name !== 'SyntaxError') return false;
         }

         // Detached element insertAdjacentHTML beforebegin/afterend throws NoModificationAllowedError.
         try {
           t.insertAdjacentHTML('beforebegin', '<em></em>');
           return false;
         } catch (e) {
           if (!e || e.name !== 'NoModificationAllowedError') return false;
         }

         return true;
       })()
     "#,
   )?;
  if let Some(err) = hooks.finish(window.heap_mut()) {
    panic!("VmJsEventLoopHooks finish returned error: {err}");
  }
  assert_eq!(out, Value::Bool(true));
  Ok(())
}
