//! Receiving a server's notifications.
//!
//! A server tells its client when its tool, prompt or resource list changed.
//! [`NotifyingHandler`] is the client handler rmcp drives for a connection;
//! it forwards those notifications to a [`NotificationSink`], and
//! [`Announcements`] is the sink every connection of one manager reports
//! into. A manager — the pool's, and each session's shared from it — reads
//! the counts there against the ones it last acted on and lists again only
//! the servers that announced a change (`McpManager::refresh_if_changed`).
//!
//! Tool and prompt changes are acted on. A resource change is counted and
//! otherwise ignored: the resource tools ask the server on every call.
//!
//! The handler also declares the tasks extension, so a server may turn one
//! tool call into a task (see `crate::tasks`).
//!
//! The handler also declares that this client answers elicitation, and takes
//! an older server's own `elicitation/create` to [`crate::elicit::ElicitRoute`].
//!
//! And it declares roots, answering an older server's own `roots/list` with
//! the project the connection serves (see `crate::project`).

use crate::elicit::ElicitRoute;
use crate::manager::McpNotification;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    ClientConfig, ElicitRequestParams, ElicitResult, ElicitationCapability,
    FormElicitationCapability, RootsCapabilities, UrlElicitationCapability,
};
use rmcp::service::{NotificationContext, RequestContext, RoleClient};
use std::collections::HashMap;
use std::sync::Arc;

/// Called on the transport's task when a server sends a notification.
pub type NotificationSink = Arc<dyn Fn(McpNotification) + Send + Sync>;

/// What a connection's handler reports into, kept by the client so a
/// reconnect wires the new handler the same way.
#[derive(Clone, Default)]
pub struct HandlerWiring {
    pub sink: Option<NotificationSink>,
    pub route: Arc<ElicitRoute>,
    pub project: Option<std::path::PathBuf>,
}

/// The client handler rmcp drives for one connection. Everything but the
/// list-changed notifications, elicitation and roots keeps `ClientHandler`'s
/// default behavior.
#[derive(Clone)]
pub struct NotifyingHandler {
    server: String,
    sink: Option<NotificationSink>,
    route: Arc<ElicitRoute>,
    project: Option<std::path::PathBuf>,
    keep: Option<Arc<dyn Send + Sync>>,
}

impl NotifyingHandler {
    pub fn new(server: impl Into<String>, wiring: HandlerWiring) -> Self {
        Self {
            server: server.into(),
            sink: wiring.sink,
            route: wiring.route,
            project: wiring.project,
            keep: None,
        }
    }

    /// A handler that receives notifications and discards them.
    pub fn silent(server: impl Into<String>) -> Self {
        Self::new(server, HandlerWiring::default())
    }

    /// Hold `value` for as long as the connection runs: rmcp drops the
    /// handler when the connection's task ends.
    pub fn keeping(mut self, value: impl Send + Sync + 'static) -> Self {
        self.keep = Some(Arc::new(value));
        self
    }

    fn emit(&self, notification: McpNotification) {
        if let Some(sink) = &self.sink {
            sink(notification);
        }
    }
}

impl ClientHandler for NotifyingHandler {
    fn get_info(&self) -> ClientConfig {
        let mut info = ClientConfig::default();
        info.capabilities.elicitation = Some(
            ElicitationCapability::new()
                .with_form(FormElicitationCapability::new().with_schema_validation(true))
                .with_url(UrlElicitationCapability::new()),
        );
        info.capabilities.roots = Some(RootsCapabilities::default());
        info.capabilities
            .extensions
            .get_or_insert_with(Default::default)
            .insert(
                rmcp::model::TASKS_EXTENSION_ID.to_string(),
                Default::default(),
            );
        info
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        Ok(self.route.answer(&self.server, request).await)
    }

    #[expect(
        deprecated,
        reason = "roots are deprecated by SEP-2577 but still answered"
    )]
    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<rmcp::model::ListRootsResult, rmcp::ErrorData> {
        Ok(crate::project::roots_of(self.project.as_deref()))
    }

    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server = self.server.clone();
        async move {
            tracing::debug!(server = %server, "MCP server announced a tool list change");
            self.emit(McpNotification::ToolListChanged { server });
        }
    }

    fn on_resource_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server = self.server.clone();
        async move {
            self.emit(McpNotification::ResourceListChanged { server });
        }
    }

    fn on_prompt_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server = self.server.clone();
        async move {
            self.emit(McpNotification::PromptListChanged { server });
        }
    }
}

/// How many times a server has announced each of its lists changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListEpochs {
    pub tools: u64,
    pub prompts: u64,
    pub resources: u64,
}

/// What servers have announced, as counts. Counts rather than a queue: every
/// manager sharing these connections reads the same announcements, each
/// against what it last acted on, and none of them takes them from the rest.
#[derive(Default)]
pub struct Announcements {
    epochs: std::sync::Mutex<HashMap<String, ListEpochs>>,
}

impl Announcements {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A sink that counts into this.
    pub fn sink(self: &Arc<Self>) -> NotificationSink {
        let announcements = Arc::clone(self);
        Arc::new(move |n| announcements.record(&n))
    }

    pub fn record(&self, notification: &McpNotification) {
        let (server, count): (&str, fn(&mut ListEpochs) -> &mut u64) = match notification {
            McpNotification::ToolListChanged { server } => (server, |e| &mut e.tools),
            McpNotification::PromptListChanged { server } => (server, |e| &mut e.prompts),
            McpNotification::ResourceListChanged { server } => (server, |e| &mut e.resources),
            McpNotification::Custom { .. } => return,
        };
        *count(self.lock().entry(server.to_string()).or_default()) += 1;
    }

    pub fn of(&self, server: &str) -> ListEpochs {
        self.lock().get(server).copied().unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ListEpochs>> {
        self.epochs.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn the_client_declares_the_tasks_extension() {
        let handler = NotifyingHandler::silent("demo");
        assert!(handler.get_info().capabilities.supports_tasks());
    }

    #[test]
    fn the_client_declares_roots_and_not_sampling() {
        let caps = NotifyingHandler::silent("demo").get_info().capabilities;
        assert!(caps.roots.is_some());
        assert!(caps.sampling.is_none());
    }

    fn recorder() -> (NotificationSink, Arc<Mutex<Vec<McpNotification>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        (Arc::new(move |n| sink.lock().unwrap().push(n)), seen)
    }

    #[test]
    fn a_handler_carries_the_server_it_belongs_to() {
        let (sink, seen) = recorder();
        let h = NotifyingHandler::new(
            "github",
            HandlerWiring {
                sink: Some(sink),
                ..Default::default()
            },
        );
        h.emit(McpNotification::ToolListChanged {
            server: h.server.clone(),
        });
        let recorded = seen.lock().unwrap();
        match &recorded[0] {
            McpNotification::ToolListChanged { server } => assert_eq!(server, "github"),
            other => panic!("expected ToolListChanged, got {other:?}"),
        }
    }

    #[test]
    fn a_silent_handler_drops_everything_without_complaint() {
        let h = NotifyingHandler::silent("github");
        h.emit(McpNotification::ToolListChanged {
            server: "github".into(),
        });
    }

    #[test]
    fn announcements_are_counted_per_server_and_kind_and_stay_for_every_reader() {
        let a = Announcements::new();
        let sink = a.sink();
        assert_eq!(a.of("github"), ListEpochs::default());

        sink(McpNotification::ToolListChanged {
            server: "github".into(),
        });
        sink(McpNotification::ToolListChanged {
            server: "github".into(),
        });
        sink(McpNotification::PromptListChanged {
            server: "github".into(),
        });
        sink(McpNotification::ResourceListChanged {
            server: "db".into(),
        });

        let github = a.of("github");
        assert_eq!((github.tools, github.prompts, github.resources), (2, 1, 0));
        assert_eq!(a.of("db").resources, 1);
        assert_eq!(a.of("github"), github, "reading takes nothing away");
    }

    #[test]
    fn a_kept_value_lives_as_long_as_the_handler_and_its_clones() {
        struct Flag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = NotifyingHandler::silent("s").keeping(Flag(dropped.clone()));
        let clone = h.clone();
        drop(h);
        assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));
        drop(clone);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }
}
