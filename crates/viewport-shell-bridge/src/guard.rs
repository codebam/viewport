// SPDX-License-Identifier: MIT
//
// Which document the bridge may speak to, and when.
//
// The DevTools-driven backends read the same bookkeeping out of the protocol
// — the frame tree, the main frame's navigations, its execution contexts —
// and enforce the same two rules with it: only the main frame's own default
// context may talk to the compositor, and queued compositor events may only
// be evaluated into a document that has loaded and is still the shell. That
// bookkeeping is CDP JSON, engine-neutral, so it lives here once rather than
// once per backend, where it drifted.

use serde_json::Value;

/// Whether queued events may be evaluated into the document.
///
/// Two facts, and the gate is their conjunction: the document is *loaded*
/// (`Page.loadEventFired` has fired for the frame the guard is tracking) and
/// its origin passes the policy. Kept apart from URL knowledge because they
/// answer different questions at different times: a committed navigation
/// names a URL long before its document's scripts have run, and both backends
/// used to open the gate on that bookkeeping — CEF on the reload's own
/// `Page.reload` method result, Chromium on
/// `Page.frameNavigated`/`executionContextCreated` — and drained queued
/// events into a document whose listener did not exist yet. What was queued
/// during a reload was what was lost.
#[derive(Default)]
pub struct LoadGate {
    /// Whether the current document has loaded. `false` at startup, across a
    /// reload, and across a navigation until the new document's load event.
    loaded: bool,
    /// Whether a URL is known and passes the policy. Separate so the gate
    /// cannot open for a document that navigated away.
    url_allowed: Option<bool>,
}

impl LoadGate {
    /// A shut gate, for a document nothing is known about yet. `const` so a
    /// backend can keep one in a `Mutex` static.
    pub const fn new() -> Self {
        Self {
            loaded: false,
            url_allowed: None,
        }
    }

    pub fn reload_requested(&mut self) {
        self.loaded = false;
    }

    /// Frame bookkeeping named a main URL. It does not open the gate: the
    /// navigation may still be committing.
    pub fn frame_navigated(&mut self, url: Option<&str>) {
        self.loaded = false;
        self.url_allowed = Some(url.is_some_and(viewport_ipc::js::shell_url_allowed));
    }

    /// Record the URL reported by a `Page.getFrameTree` query.
    ///
    /// Unlike `Page.frameNavigated`, a frame-tree reply inspects the current
    /// document rather than starting a navigation, so it updates the origin
    /// check without clearing `loaded`.
    pub fn url_seen(&mut self, url: Option<&str>) {
        self.url_allowed = Some(url.is_some_and(viewport_ipc::js::shell_url_allowed));
    }

    /// Context bookkeeping: a cleared or replaced main-frame context is the
    /// old document going away, so the gate closes until its load event.
    pub fn context_created(&mut self) {
        self.loaded = false;
    }

    /// The document has loaded. The one event that can open the gate.
    pub fn load_event_fired(&mut self) {
        self.loaded = true;
    }

    pub fn ready(&self) -> bool {
        self.loaded && self.url_allowed.unwrap_or(false)
    }
}

/// The document the origin policy is enforced against.
///
/// Every state here is client-controlled input read back from DevTools, and
/// every field is optional because the answer to "which document is this" is
/// genuinely unknown until the frame tree or a navigation names one.
///
/// The bridge talks two directions, and both have to be guarded:
///
/// * outbound, the compositor's events are evaluated into the page — but
///   only into a page that is still the shell. A document that navigated or
///   was redirected elsewhere must not be handed the desktop's clipboard,
///   notifications or session state, so the gate's URL answer is re-checked
///   before every drain of the queue.
/// * inbound, a page-to-compositor message is accepted only from the main
///   frame's default context. `Runtime.addBinding` installs the binding in
///   every execution context in the target, and a subframe is a different
///   document: the shim only refuses to build `window.webkit` in a subframe,
///   so a page that knows the binding's name could still call
///   `__viewport_send` directly from an iframe.
#[derive(Default)]
pub struct FrameGuard {
    pub main_frame: Option<String>,
    pub main_url: Option<String>,
    pub main_context: Option<i64>,
    /// The newest default context seen for each frame, because
    /// `Runtime.enable` can report contexts before `Page.getFrameTree` has
    /// told us which frame is the main one.
    contexts: Vec<(String, i64)>,
}

impl FrameGuard {
    /// A guard that knows nothing yet, for a document still coming up.
    /// `const` so a backend can keep one in a `Mutex` static.
    pub const fn new() -> Self {
        Self {
            main_frame: None,
            main_url: None,
            main_context: None,
            contexts: Vec::new(),
        }
    }

    /// Fold one DevTools message in. True when it was frame bookkeeping.
    pub fn note(&mut self, message: &Value) -> bool {
        self.note_frame_tree(message)
            || self.note_frame_navigated(message)
            || self.note_context_created(message)
            || self.note_contexts_cleared(message)
    }

    fn note_frame_tree(&mut self, message: &Value) -> bool {
        let Some(frame) = message.pointer("/result/frameTree/frame") else {
            return false;
        };
        self.set_main_frame(frame);
        true
    }

    fn note_frame_navigated(&mut self, message: &Value) -> bool {
        if message.get("method").and_then(Value::as_str) != Some("Page.frameNavigated") {
            return false;
        }
        let Some(frame) = message.get("params").and_then(|params| params.get("frame")) else {
            return true;
        };
        // A subframe navigation is not a navigation of the shell document.
        let top = match frame.get("parentId") {
            None | Some(Value::Null) => true,
            Some(_) => false,
        };
        if top {
            self.set_main_frame(frame);
            // A cross-document navigation replaces the default context. Until
            // its `executionContextCreated` arrives, fail closed rather than
            // accept messages from the outgoing document.
            if let Some(frame_id) = self.main_frame.clone() {
                self.contexts.retain(|(id, _)| id != &frame_id);
            }
            self.main_context = None;
        }
        true
    }

    fn note_context_created(&mut self, message: &Value) -> bool {
        if message.get("method").and_then(Value::as_str) != Some("Runtime.executionContextCreated")
        {
            return false;
        }
        let Some(context) = message
            .get("params")
            .and_then(|params| params.get("context"))
        else {
            return true;
        };
        let Some(aux) = context.get("auxData") else {
            return true;
        };
        if aux.get("isDefault").and_then(Value::as_bool) != Some(true) {
            return true;
        }
        let (Some(frame_id), Some(context_id)) = (
            aux.get("frameId").and_then(Value::as_str),
            context.get("id").and_then(Value::as_i64),
        ) else {
            return true;
        };
        self.contexts.retain(|(id, _)| id != frame_id);
        self.contexts.push((frame_id.to_owned(), context_id));
        if self.contexts.len() > 64 {
            self.contexts.remove(0);
        }
        if self.main_frame.as_deref() == Some(frame_id) {
            self.main_context = Some(context_id);
        }
        true
    }

    fn note_contexts_cleared(&mut self, message: &Value) -> bool {
        if message.get("method").and_then(Value::as_str) != Some("Runtime.executionContextsCleared")
        {
            return false;
        }
        self.contexts.clear();
        self.main_context = None;
        true
    }

    /// Forget the URL the origin policy was checking.
    ///
    /// The frame id and contexts stay: a `Page.getFrameTree` reply fills the
    /// URL back in, and until then the guard refuses what it cannot place.
    pub fn forget_url(&mut self) {
        self.main_url = None;
    }

    fn set_main_frame(&mut self, frame: &Value) {
        let Some(id) = frame.get("id").and_then(Value::as_str) else {
            return;
        };
        let url = frame.get("url").and_then(Value::as_str).unwrap_or("");
        self.main_frame = Some(id.to_owned());
        self.main_url = Some(url.to_owned());
        self.main_context = self
            .contexts
            .iter()
            .rev()
            .find(|(context_frame, _)| context_frame == id)
            .map(|(_, context)| *context);
    }

    /// Whether a binding call came from the main frame's own default context.
    pub fn context_allows(&self, context: Option<i64>) -> bool {
        match (self.main_context, context) {
            (Some(main), Some(called)) => main == called,
            _ => false,
        }
    }

    /// Whether the main frame is on an origin the bridge may speak from.
    pub fn url_allows(&self) -> bool {
        match self.main_url.as_deref() {
            Some(url) => viewport_ipc::js::shell_url_allowed(url),
            None => false,
        }
    }

    /// Whether events may be evaluated into the current document.
    ///
    /// Before the frame tree arrives (`None`) the URL the browser was started
    /// with already passed the policy, so waiting events are allowed. Once a
    /// URL is known, navigation away from the allowed origin stops events at
    /// the door instead of handing them to the remote document.
    pub fn events_allowed(&self) -> bool {
        match self.main_url.as_deref() {
            Some(url) => viewport_ipc::js::shell_url_allowed(url),
            None => true,
        }
    }
}

/// The frame guard and the load gate together, because the wiring between
/// them is security policy too: bookkeeping that moves the document must
/// close the gate, and sharing the pieces without the wiring is how the two
/// backends drifted apart.
#[derive(Default)]
pub struct DocumentGuard {
    pub guard: FrameGuard,
    pub gate: LoadGate,
}

impl DocumentGuard {
    /// Both halves shut and knowing nothing, for a `Mutex` static.
    pub const fn new() -> Self {
        Self {
            guard: FrameGuard::new(),
            gate: LoadGate::new(),
        }
    }

    /// Fold one DevTools message in. True when it was frame bookkeeping.
    ///
    /// The frame tree or a main-frame navigation just arrived. The URL is now
    /// known, but the gate does not open on bookkeeping: a committed navigation
    /// is not a loaded document, and the queue waits for its load event.
    pub fn note(&mut self, message: &Value) -> bool {
        // FrameGuard recognises subframe bookkeeping too, but a subframe does
        // not replace the shell document. Treating its navigation as the main
        // document's would close an open gate until a main-document load event
        // that the subframe will never cause, stranding queued shell events.
        let frame_tree = message.pointer("/result/frameTree/frame").is_some();
        let main_navigation = message.get("method").and_then(Value::as_str)
            == Some("Page.frameNavigated")
            && message
                .pointer("/params/frame")
                .is_none_or(|frame| frame.get("parentId").is_none_or(Value::is_null));
        if !self.guard.note(message) {
            return false;
        }
        if main_navigation {
            self.gate.frame_navigated(self.guard.main_url.as_deref());
        } else if frame_tree {
            self.gate.url_seen(self.guard.main_url.as_deref());
        }
        // Context bookkeeping rides the same note — a cleared or replaced
        // execution context is the old document going away, which is what
        // the gate exists to hold events back from.
        if self.guard.main_context.is_none() {
            self.gate.context_created();
        }
        true
    }

    /// The document has loaded — the one event that can open the gate.
    /// Answers whether the gate is open now.
    pub fn load_event_fired(&mut self) -> bool {
        self.gate.load_event_fired();
        self.gate.ready()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The reload gate opens on a load event, and on nothing before it.
    ///
    /// `ready` used to be re-opened by the frame bookkeeping itself —
    /// `Page.frameNavigated` fires the moment navigation commits, long before
    /// the new document's module scripts have attached their listener — and
    /// by the reload's own `Page.reload` method result in the CEF twin of
    /// this gate. Events drained between the commit and the load went into a
    /// document nobody was listening on and were lost.
    #[test]
    fn the_reload_gate_opens_on_the_load_event_alone() {
        let mut gate = LoadGate::default();

        // A reload closes the gate and nothing else happens yet.
        gate.reload_requested();
        assert!(!gate.ready(), "the gate is closed across a reload");

        // The navigation commits: frame bookkeeping, not a load. The gate
        // stays shut, however good the URL looks.
        gate.frame_navigated(Some("file:///shell/index.html"));
        assert!(
            !gate.ready(),
            "a committed navigation is not a loaded document"
        );

        // An execution context arriving is not a load either — this is the
        // exact event that used to re-open the gate.
        gate.context_created();
        assert!(!gate.ready());

        // The load event is the one thing that opens it.
        gate.load_event_fired();
        assert!(gate.ready());
    }

    /// A replaced execution context closes the gate on a document that had
    /// already loaded.
    ///
    /// The sibling test above pins that context bookkeeping is not a load —
    /// but before this fix `context_created` was a no-op on the state and
    /// never wired into the note handler, so it asserted nothing at all:
    /// the gate was already shut for the reload and stayed shut for the
    /// wrong reason. A cleared or replaced main-frame context is the old
    /// document going away — `Page.frameNavigated` drops the tracked
    /// contexts (`guard.main_context.is_none()`) before the new document's
    /// `executionContextCreated` arrives — so a gate that is *open* must
    /// close when one is announced, and only the new document's own load
    /// event may open it again.
    #[test]
    fn a_replaced_context_closes_the_reload_gate() {
        let mut gate = LoadGate::default();
        // The gate is a conjunction, so the URL half has to be satisfied
        // before "open" means anything: without this the test would fail at
        // both commits for the wrong reason.
        gate.frame_navigated(Some("file:///shell/index.html"));
        gate.load_event_fired(); // gate open, document loaded
        assert!(gate.ready(), "the gate is open on the loaded document");
        gate.context_created(); // a NEW context replaced the old one
        assert!(
            !gate.ready(),
            "a replaced execution context closes the gate until the new document loads"
        );
        gate.load_event_fired();
        assert!(gate.ready(), "the new document's own load reopens it");
    }

    /// And the initial attach: the frame tree naming a good URL does not
    /// open the gate before the document has loaded, either.
    #[test]
    fn the_first_frame_tree_does_not_open_the_gate() {
        let mut gate = LoadGate::default();
        gate.frame_navigated(Some("file:///shell/index.html"));
        assert!(!gate.ready(), "waiting for the load event");
        gate.load_event_fired();
        assert!(gate.ready());

        // A second navigation closes the gate again until its own load.
        gate.frame_navigated(Some("file:///shell/index.html"));
        assert!(!gate.ready());
        gate.load_event_fired();
        assert!(gate.ready());
    }

    /// A document that navigated away from an allowed origin keeps the gate
    /// shut even when its load event fires.
    #[test]
    fn a_disallowed_origin_keeps_the_gate_shut() {
        let mut gate = LoadGate::default();
        gate.frame_navigated(Some("https://example.com/"));
        gate.load_event_fired();
        assert!(!gate.ready());
    }

    /// A subframe's default context is not the shell's, even though it has the
    /// same `isDefault` flag.
    #[test]
    fn only_the_main_frames_context_can_speak() {
        let mut guard = FrameGuard::default();
        guard.note(&json!({
            "method": "Page.frameNavigated",
            "params": {"frame": {"id": "main", "url": "file:///shell/index.html"}},
        }));
        guard.note(&json!({
            "method": "Runtime.executionContextCreated",
            "params": {"context": {
                "id": 1,
                "auxData": {"isDefault": true, "frameId": "main"},
            }},
        }));
        guard.note(&json!({
            "method": "Runtime.executionContextCreated",
            "params": {"context": {
                "id": 2,
                "auxData": {"isDefault": true, "frameId": "sub"},
            }},
        }));
        assert!(guard.context_allows(Some(1)));
        assert!(!guard.context_allows(Some(2)));
        assert!(!guard.context_allows(None));
    }

    #[test]
    fn navigating_the_main_frame_away_revokes_the_bridge() {
        let mut guard = FrameGuard::default();
        guard.note(&json!({
            "method": "Page.frameNavigated",
            "params": {"frame": {"id": "main", "url": "http://localhost:3000/"}},
        }));
        guard.note(&json!({
            "method": "Runtime.executionContextCreated",
            "params": {"context": {
                "id": 1,
                "auxData": {"isDefault": true, "frameId": "main"},
            }},
        }));
        assert!(guard.url_allows());
        assert!(guard.context_allows(Some(1)));

        guard.note(&json!({
            "method": "Page.frameNavigated",
            "params": {"frame": {"id": "main", "url": "https://example.com/"}},
        }));
        // The new document's context has not been announced yet; until it is,
        // the old context id is not accepted either.
        assert!(!guard.url_allows());
        assert!(!guard.context_allows(Some(1)));
    }

    /// The shared wiring: a top-frame navigation closes a gate that was
    /// open, and the outgoing document's context is not accepted while the
    /// new one has not been announced.
    #[test]
    fn a_top_frame_navigation_closes_an_open_gate() {
        let mut doc = DocumentGuard::default();
        assert!(doc.note(&json!({
            "method": "Page.frameNavigated",
            "params": {"frame": {"id": "main", "url": "file:///shell/index.html"}},
        })));
        assert!(doc.note(&json!({
            "method": "Runtime.executionContextCreated",
            "params": {"context": {
                "id": 1,
                "auxData": {"isDefault": true, "frameId": "main"},
            }},
        })));
        assert!(doc.load_event_fired(), "the loaded shell document is open");

        assert!(doc.note(&json!({
            "method": "Page.frameNavigated",
            "params": {"frame": {"id": "main", "url": "http://localhost:3000/"}},
        })));
        assert!(!doc.gate.ready(), "a committed navigation is not a load");
        assert!(!doc.guard.context_allows(Some(1)));
        assert!(
            doc.load_event_fired(),
            "the new document's own load event reopens the gate"
        );
    }

    /// Clearing the execution contexts is the old document going away: an
    /// open gate closes, and only a load event may open it again.
    #[test]
    fn contexts_cleared_closes_an_open_gate() {
        let mut doc = DocumentGuard::default();
        doc.note(&json!({
            "method": "Page.frameNavigated",
            "params": {"frame": {"id": "main", "url": "file:///shell/index.html"}},
        }));
        doc.note(&json!({
            "method": "Runtime.executionContextCreated",
            "params": {"context": {
                "id": 1,
                "auxData": {"isDefault": true, "frameId": "main"},
            }},
        }));
        assert!(doc.load_event_fired());

        assert!(doc.note(&json!({"method": "Runtime.executionContextsCleared"})));
        assert!(!doc.gate.ready());
        assert!(!doc.guard.context_allows(Some(1)));
        assert!(doc.load_event_fired());
    }

    /// Subframe bookkeeping is not a navigation of the shell document: the
    /// main URL, its origin answer, its context and an already-open load gate
    /// are all left alone. Closing the main gate here would strand queued
    /// events until a main-document load event that this navigation will never
    /// cause.
    #[test]
    fn subframe_bookkeeping_leaves_the_loaded_main_document_alone() {
        let mut doc = DocumentGuard::default();
        doc.note(&json!({
            "method": "Page.frameNavigated",
            "params": {"frame": {"id": "main", "url": "file:///shell/index.html"}},
        }));
        doc.note(&json!({
            "method": "Runtime.executionContextCreated",
            "params": {"context": {
                "id": 1,
                "auxData": {"isDefault": true, "frameId": "main"},
            }},
        }));
        assert!(doc.load_event_fired(), "the main document is loaded");

        assert!(doc.note(&json!({
            "method": "Page.frameNavigated",
            "params": {"frame": {
                "id": "sub",
                "parentId": "main",
                "url": "https://example.com/",
            }},
        })));
        assert_eq!(
            doc.guard.main_url.as_deref(),
            Some("file:///shell/index.html")
        );
        assert!(doc.guard.url_allows());
        assert!(doc.guard.context_allows(Some(1)));
        assert!(
            doc.gate.ready(),
            "a subframe navigation does not replace the loaded shell document"
        );

        assert!(doc.note(&json!({
            "method": "Runtime.executionContextCreated",
            "params": {"context": {
                "id": 2,
                "auxData": {"isDefault": true, "frameId": "sub"},
            }},
        })));
        assert!(doc.guard.context_allows(Some(1)));
        assert!(!doc.guard.context_allows(Some(2)));
        assert!(
            doc.gate.ready(),
            "a subframe context does not replace the loaded shell document"
        );
    }

    /// Before any bookkeeping names a URL, waiting events are allowed at the
    /// door — the startup URL already passed the policy — but the origin
    /// answer is fail-closed and a load event alone opens nothing.
    #[test]
    fn an_unknown_url_queues_events_but_opens_nothing() {
        let mut doc = DocumentGuard::default();
        assert!(doc.guard.events_allowed());
        assert!(!doc.guard.url_allows());
        assert!(!doc.load_event_fired());
    }

    /// A known disallowed origin stops events at the door, not just at the
    /// gate.
    #[test]
    fn a_known_disallowed_origin_stops_events_at_the_door() {
        let mut doc = DocumentGuard::default();
        doc.note(&json!({
            "method": "Page.frameNavigated",
            "params": {"frame": {"id": "main", "url": "https://example.com/"}},
        }));
        assert!(!doc.guard.events_allowed());
        assert!(!doc.guard.url_allows());
        assert!(!doc.load_event_fired());
    }

    /// The frame-tree result names the URL but is bookkeeping, not a load:
    /// the gate stays shut until the load event.
    #[test]
    fn a_frame_tree_result_names_the_url_but_does_not_open_the_gate() {
        let mut doc = DocumentGuard::default();
        assert!(doc.note(&json!({
            "result": {"frameTree": {"frame": {
                "id": "main",
                "url": "file:///shell/index.html",
            }}},
        })));
        assert_eq!(
            doc.guard.main_url.as_deref(),
            Some("file:///shell/index.html")
        );
        assert!(!doc.gate.ready());
        assert!(doc.load_event_fired());
    }

    /// Re-querying `Page.getFrameTree` on an already-loaded document (for
    /// example after a stale-guard probe) must not clear `loaded`, because no
    /// second `Page.loadEventFired` will ever arrive.
    #[test]
    fn frame_tree_requery_keeps_a_loaded_document_ready() {
        let mut doc = DocumentGuard::default();
        doc.note(&json!({
            "method": "Page.frameNavigated",
            "params": {"frame": {"id": "main", "url": "file:///shell/index.html"}},
        }));
        doc.note(&json!({
            "method": "Runtime.executionContextCreated",
            "params": {"context": {
                "id": 1,
                "auxData": {"isDefault": true, "frameId": "main"},
            }},
        }));
        assert!(doc.load_event_fired());

        // Stale-guard re-query: forget URL, then receive Page.getFrameTree.
        doc.guard.forget_url();
        assert!(doc.note(&json!({
            "result": {"frameTree": {"frame": {
                "id": "main",
                "url": "file:///shell/index.html",
            }}},
        })));
        assert!(
            doc.gate.ready(),
            "a frame-tree reply for the loaded shell must keep the gate open"
        );

        // If the reply reveals a disallowed origin, the gate still shuts.
        assert!(doc.note(&json!({
            "result": {"frameTree": {"frame": {
                "id": "main",
                "url": "https://example.com/",
            }}},
        })));
        assert!(!doc.gate.ready());
    }

    /// If `Page.loadEventFired` beats the initial `Page.getFrameTree` reply,
    /// the frame-tree reply opens the gate once the context and URL are known.
    #[test]
    fn frame_tree_after_load_event_opens_the_gate() {
        let mut doc = DocumentGuard::default();
        doc.note(&json!({
            "method": "Runtime.executionContextCreated",
            "params": {"context": {
                "id": 1,
                "auxData": {"isDefault": true, "frameId": "main"},
            }},
        }));
        // Load fires before Page.getFrameTree names the URL.
        assert!(!doc.load_event_fired());
        assert!(doc.note(&json!({
            "result": {"frameTree": {"frame": {
                "id": "main",
                "url": "file:///shell/index.html",
            }}},
        })));
        assert!(doc.gate.ready());
    }
}
