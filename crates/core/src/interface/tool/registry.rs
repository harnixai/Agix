//! `ToolRegistry` and its two implementations, in-memory and layered.

use super::*;

/// Identifies one registration, so it can be withdrawn without disturbing
/// anything else that happens to share the tool's name.
///
/// A name is not enough. `register` appends, so a name can legitimately be
/// present more than once, and "remove the tool called X" would then withdraw
/// whichever copy happened to be first — including one that a different
/// contributor added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RegistrationId(u64);

impl RegistrationId {
    /// Mint an id. Public because every registry in the engine hands out
    /// disposers keyed by one, and they do not all live in this module.
    pub fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// A handle that withdraws one registration.
///
/// **Dropping it does nothing.** Undoing on drop would be the more idiomatic
/// Rust shape, but it makes the overwhelmingly common call —
/// `registry.register(tool);` with the result ignored — silently unregister
/// the tool it just added. Withdrawal is therefore explicit: hold the handle
/// if you will need it, ignore it if you will not.
/// Not `#[must_use]`: the overwhelmingly common case is registering something
/// for the lifetime of the process and never withdrawing it, and a lint on
/// every one of those would be noise that teaches people to silence lints.
pub struct Disposer {
    id: RegistrationId,
    undo: Option<Box<dyn FnOnce(RegistrationId) + Send + Sync>>,
}

impl std::fmt::Debug for Disposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disposer").field("id", &self.id).finish()
    }
}

impl Disposer {
    pub fn new(
        id: RegistrationId,
        undo: impl FnOnce(RegistrationId) + Send + Sync + 'static,
    ) -> Self {
        Self {
            id,
            undo: Some(Box::new(undo)),
        }
    }

    /// A handle over a registration that cannot be withdrawn — for an
    /// implementation whose contents are fixed after construction.
    pub fn inert(id: RegistrationId) -> Self {
        Self { id, undo: None }
    }

    pub fn id(&self) -> RegistrationId {
        self.id
    }

    /// Withdraw the registration this handle stands for.
    pub fn dispose(mut self) {
        if let Some(undo) = self.undo.take() {
            undo(self.id);
        }
    }
}

pub trait ToolRegistry: Send + Sync + 'static {
    fn list(&self) -> Vec<Arc<dyn Tool>>;
    fn get(&self, n: &str) -> Option<Arc<dyn Tool>>;

    /// Add a tool, returning a handle that can withdraw exactly this
    /// registration.
    fn register(&self, t: Arc<dyn Tool>) -> Disposer;

    /// Substitute the entry for `t.name()`, or append when there is none.
    ///
    /// Distinct from [`register`](Self::register) because that one appends and
    /// [`get`](Self::get) returns the first name match, so re-registering an
    /// existing name leaves the original in front and the new instance
    /// unreachable.
    fn replace(&self, t: Arc<dyn Tool>) -> Disposer;

    /// Withdraw every registration under `name`. Returns how many went.
    ///
    /// Present so a contributor can be unloaded cleanly. Masking a tool with a
    /// deny rule leaves it in the registry, still visible to anything that
    /// enumerates it; removal is what actually takes it out.
    fn remove(&self, name: &str) -> usize;

    fn names(&self) -> Vec<String> {
        self.list().iter().map(|t| t.name().to_string()).collect()
    }
    fn len(&self) -> usize {
        self.list().len()
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone)]
pub struct InMemoryToolRegistry {
    pub(crate) tools: Arc<std::sync::RwLock<Vec<Entry>>>,
}

/// A registered tool plus the id its `Disposer` refers to.
#[derive(Clone)]
pub struct Entry {
    pub id: RegistrationId,
    pub tool: Arc<dyn Tool>,
}
impl InMemoryToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Arc::new(std::sync::RwLock::new(Vec::new())),
        }
    }
    pub fn register(&self, t: Arc<dyn Tool>) -> Disposer {
        let id = RegistrationId::next();
        self.tools
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(Entry { id, tool: t });
        self.disposer_for(id)
    }

    fn disposer_for(&self, id: RegistrationId) -> Disposer {
        let tools = self.tools.clone();
        Disposer::new(id, move |id| {
            tools
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|e| e.id != id);
        })
    }

    /// Swap the entry for `t.name()`, or append when there is none.
    ///
    /// [`register`](Self::register) appends unconditionally while
    /// [`get`](Self::get) returns the *first* name match, so re-registering an
    /// existing name leaves the original in front and the new instance
    /// unreachable. Use this wherever the intent is to substitute a tool that
    /// is already present — wrapping one in a decorator, for instance.
    pub fn replace(&self, t: Arc<dyn Tool>) -> Disposer {
        let id = RegistrationId::next();
        {
            let mut w = self.tools.write().unwrap_or_else(|e| e.into_inner());
            match w.iter().position(|x| x.tool.name() == t.name()) {
                Some(i) => w[i] = Entry { id, tool: t },
                None => w.push(Entry { id, tool: t }),
            }
        }
        self.disposer_for(id)
    }

    /// Withdraw every registration.
    pub fn clear(&self) {
        self.tools
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Withdraw every registration under `name`; returns how many went.
    pub fn remove(&self, name: &str) -> usize {
        let mut w = self.tools.write().unwrap_or_else(|e| e.into_inner());
        let before = w.len();
        w.retain(|e| e.tool.name() != name);
        before - w.len()
    }
    pub fn get(&self, n: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|e| e.tool.name() == n)
            .map(|e| e.tool.clone())
    }
    pub fn list(&self) -> Vec<Arc<dyn Tool>> {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|e| e.tool.clone())
            .collect()
    }
    pub fn names(&self) -> Vec<String> {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|e| e.tool.name().to_string())
            .collect()
    }
    pub fn len(&self) -> usize {
        self.tools.read().unwrap_or_else(|e| e.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
impl ToolRegistry for InMemoryToolRegistry {
    fn list(&self) -> Vec<Arc<dyn Tool>> {
        InMemoryToolRegistry::list(self)
    }
    fn get(&self, n: &str) -> Option<Arc<dyn Tool>> {
        InMemoryToolRegistry::get(self, n)
    }
    fn register(&self, t: Arc<dyn Tool>) -> Disposer {
        InMemoryToolRegistry::register(self, t)
    }
    fn replace(&self, t: Arc<dyn Tool>) -> Disposer {
        InMemoryToolRegistry::replace(self, t)
    }
    fn remove(&self, name: &str) -> usize {
        InMemoryToolRegistry::remove(self, name)
    }
}
impl Default for InMemoryToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A registry that layers one set of tools over another.
///
/// Exists to answer a question a single implementation cannot: is
/// [`ToolRegistry`] actually a contract, or just the shape of
/// [`InMemoryToolRegistry`]? Writing a second implementation is what finds the
/// methods that leaked an assumption about the first — and this one does not
/// store `Vec<Entry>` at all, so anything that only worked because the backing
/// store was a vector shows up here.
///
/// Reads consult the overlay first, then the base, so an overlay entry shadows
/// a base entry of the same name. Writes only ever touch the overlay: the base
/// is borrowed, and silently mutating something you were handed is how a
/// "layer" becomes a leak.
pub struct LayeredToolRegistry {
    base: Arc<dyn ToolRegistry>,
    overlay: InMemoryToolRegistry,
    hidden: std::sync::RwLock<std::collections::HashSet<String>>,
}

impl LayeredToolRegistry {
    pub fn new(base: Arc<dyn ToolRegistry>) -> Self {
        Self {
            base,
            overlay: InMemoryToolRegistry::new(),
            hidden: Default::default(),
        }
    }

    /// Names hidden from `list`/`get` without touching the base registry.
    ///
    /// The layer's reason for existing: an embedder that wants a narrower tool
    /// surface than the one it was handed, without the right to modify it.
    pub fn hide(&self, name: &str) {
        self.hidden
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string());
    }

    fn is_hidden(&self, name: &str) -> bool {
        self.hidden
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
    }
}

impl ToolRegistry for LayeredToolRegistry {
    fn list(&self) -> Vec<Arc<dyn Tool>> {
        let mut out = self.overlay.list();
        let shadowed: std::collections::HashSet<String> =
            out.iter().map(|t| t.name().to_string()).collect();
        for t in self.base.list() {
            if !self.is_hidden(t.name()) && !shadowed.contains(t.name()) {
                out.push(t);
            }
        }
        out
    }

    fn get(&self, n: &str) -> Option<Arc<dyn Tool>> {
        match self.overlay.get(n) {
            Some(t) => Some(t),
            None if self.is_hidden(n) => None,
            None => self.base.get(n),
        }
    }

    /// Registering a name the layer hid makes the new entry visible; the base
    /// entry stays hidden behind it.
    fn register(&self, t: Arc<dyn Tool>) -> Disposer {
        self.overlay.register(t)
    }

    fn replace(&self, t: Arc<dyn Tool>) -> Disposer {
        self.overlay.replace(t)
    }

    /// Removes from the overlay, then hides whatever the base still exposes.
    /// "Removed" has to mean "not visible through this registry" — leaving a
    /// base entry reachable after a `remove` would be the exact leak this
    /// layer exists to avoid.
    fn remove(&self, name: &str) -> usize {
        let removed = self.overlay.remove(name);
        if !self.is_hidden(name) && self.base.get(name).is_some() {
            self.hide(name);
            return removed + 1;
        }
        removed
    }
}

#[cfg(test)]
mod registry_contract_tests {
    use super::*;

    #[derive(Debug)]
    struct Stub(&'static str);

    #[async_trait::async_trait]
    impl Tool for Stub {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec::new()
        }
        fn input_schema(&self, _: PathStyle) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _i: Value,
            _c: ToolContext,
            _p: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    fn stub(n: &'static str) -> Arc<dyn Tool> {
        Arc::new(Stub(n))
    }

    /// The same assertions against every implementation. A contract only one
    /// type satisfies is not a contract.
    fn contract(reg: &dyn ToolRegistry) {
        let d = reg.register(stub("Alpha"));
        assert!(reg.get("Alpha").is_some());
        assert!(reg.names().contains(&"Alpha".to_string()));

        // A disposer withdraws its own registration and nothing else.
        let keep = reg.register(stub("Beta"));
        d.dispose();
        assert!(reg.get("Alpha").is_none(), "disposed tool still reachable");
        assert!(
            reg.get("Beta").is_some(),
            "disposer took an unrelated entry"
        );
        let _ = keep;

        // `replace` substitutes rather than shadowing.
        reg.register(stub("Gamma"));
        reg.replace(stub("Gamma"));
        assert_eq!(
            reg.list().iter().filter(|t| t.name() == "Gamma").count(),
            1,
            "replace left the original in place"
        );

        assert_eq!(reg.remove("Gamma"), 1);
        assert!(reg.get("Gamma").is_none());
        assert_eq!(reg.remove("NeverRegistered"), 0);
    }

    #[test]
    fn in_memory_registry_satisfies_the_contract() {
        contract(&InMemoryToolRegistry::new());
    }

    #[test]
    fn layered_registry_satisfies_the_contract() {
        contract(&LayeredToolRegistry::new(Arc::new(
            InMemoryToolRegistry::new(),
        )));
    }

    /// Dropping a disposer must *not* withdraw — the common call ignores the
    /// return value, and undoing on drop would silently unregister everything.
    #[test]
    fn dropping_a_disposer_keeps_the_registration() {
        let reg = InMemoryToolRegistry::new();
        drop(reg.register(stub("Alpha")));
        assert!(reg.get("Alpha").is_some());
    }

    /// Two registrations of one name are distinct, and a disposer takes only
    /// the one it was handed back for.
    #[test]
    fn a_disposer_targets_its_own_registration_not_the_name() {
        let reg = InMemoryToolRegistry::new();
        let first = reg.register(stub("Dup"));
        reg.register(stub("Dup"));
        assert_eq!(reg.list().iter().filter(|t| t.name() == "Dup").count(), 2);
        first.dispose();
        assert_eq!(
            reg.list().iter().filter(|t| t.name() == "Dup").count(),
            1,
            "disposing one registration took both"
        );
    }

    /// The layer must not mutate what it was handed.
    #[test]
    fn the_layer_never_writes_through_to_its_base() {
        let base = Arc::new(InMemoryToolRegistry::new());
        base.register(stub("FromBase"));
        let layer = LayeredToolRegistry::new(base.clone());

        layer.register(stub("FromLayer"));
        assert_eq!(base.len(), 1, "layer wrote into its base");

        assert!(layer.get("FromBase").is_some(), "base entry not visible");
        assert_eq!(layer.remove("FromBase"), 1);
        assert!(layer.get("FromBase").is_none(), "removal did not hide it");
        assert!(
            base.get("FromBase").is_some(),
            "removal reached through into the base"
        );
    }

    /// An overlay entry shadows a base entry of the same name.
    #[test]
    fn overlay_shadows_the_base_without_duplicating() {
        let base = Arc::new(InMemoryToolRegistry::new());
        base.register(stub("Shared"));
        let layer = LayeredToolRegistry::new(base);
        layer.register(stub("Shared"));
        assert_eq!(
            layer.list().iter().filter(|t| t.name() == "Shared").count(),
            1
        );
    }
}
