//! `SessionHub` 把帧交给订阅者时不持着自己的锁：订阅者在 `deliver` 里回调同一个
//! hub 不会卡死，每个订阅者收到的帧仍按发出的先后。

use super::*;
use std::sync::Mutex;

fn kind(frame: &SessionFrame) -> &'static str {
    match frame.event {
        SessionEvent::EngineStopped => "stopped",
        SessionEvent::Closed => "closed",
        _ => "other",
    }
}

/// Records what it is handed; on its first frame it publishes another into
/// the same hub and asks the hub something.
struct CallsBack {
    id: u64,
    hub: std::sync::Weak<SessionHub>,
    seen: Mutex<Vec<&'static str>>,
}

impl SessionWatcher for CallsBack {
    fn id(&self) -> WatcherId {
        WatcherId(self.id)
    }

    fn deliver(&self, frame: &SessionFrame) -> bool {
        let first = {
            let mut seen = self.seen.lock().unwrap();
            seen.push(kind(frame));
            seen.len() == 1
        };
        if first {
            if let Some(hub) = self.hub.upgrade() {
                let _ = hub.ask_kind("nothing-asked");
                hub.publish(&frame_between_turns("s", SessionEvent::Closed));
            }
        }
        true
    }
}

#[test]
fn a_watcher_calling_back_into_its_hub_does_not_deadlock_and_frames_stay_in_order() {
    let hub = Arc::new(SessionHub::active_at(std::time::Instant::now()));
    let watchers: Vec<Arc<CallsBack>> = (1..=2)
        .map(|id| {
            Arc::new(CallsBack {
                id,
                hub: Arc::downgrade(&hub),
                seen: Mutex::new(Vec::new()),
            })
        })
        .collect();
    for w in &watchers {
        hub.subscribe(w.clone());
    }

    let (done, finished) = std::sync::mpsc::channel();
    let publisher = hub.clone();
    std::thread::spawn(move || {
        publisher.publish(&frame_between_turns("s", SessionEvent::EngineStopped));
        let _ = done.send(());
    });
    finished
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("publishing deadlocked on a watcher that called back into the hub");

    for w in &watchers {
        assert_eq!(
            *w.seen.lock().unwrap(),
            ["stopped", "closed", "closed"],
            "watcher {}",
            w.id
        );
    }
}

/// Takes frames unless `full`; records what it took.
struct Fills {
    id: u64,
    full: bool,
    seen: Mutex<Vec<&'static str>>,
}

impl SessionWatcher for Fills {
    fn id(&self) -> WatcherId {
        WatcherId(self.id)
    }

    fn deliver(&self, frame: &SessionFrame) -> bool {
        if self.full {
            return false;
        }
        self.seen.lock().unwrap().push(kind(frame));
        true
    }
}

/// On its first frame, subscribes `again` and publishes one more frame, as a
/// client re-subscribing while the delivery that dropped it is still going.
struct Resubscribes {
    hub: std::sync::Weak<SessionHub>,
    again: Arc<Fills>,
    done: std::sync::atomic::AtomicBool,
}

impl SessionWatcher for Resubscribes {
    fn id(&self) -> WatcherId {
        WatcherId(99)
    }

    fn deliver(&self, _: &SessionFrame) -> bool {
        if !self.done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            if let Some(hub) = self.hub.upgrade() {
                hub.subscribe(self.again.clone());
                hub.publish(&frame_between_turns("s", SessionEvent::Closed));
            }
        }
        true
    }
}

#[test]
fn a_watcher_dropped_and_subscribed_again_within_one_delivery_stays_subscribed() {
    let hub = Arc::new(SessionHub::active_at(std::time::Instant::now()));
    let fills = |full| {
        Arc::new(Fills {
            id: 1,
            full,
            seen: Mutex::new(Vec::new()),
        })
    };
    let again = fills(false);
    hub.subscribe(fills(true));
    hub.subscribe(Arc::new(Resubscribes {
        hub: Arc::downgrade(&hub),
        again: again.clone(),
        done: Default::default(),
    }));

    hub.publish(&frame_between_turns("s", SessionEvent::EngineStopped));

    assert!(
        hub.is_watched_by(WatcherId(1)),
        "the new subscription was dropped"
    );
    assert_eq!(*again.seen.lock().unwrap(), ["closed"]);
}

mod hosted_calls {
    use super::*;
    use crate::session_pool::parts::hosts::HostConnections;
    use base::interface::host_call::{HostCallEnd, HostId, HostToolDecl};
    use runtime::agent::InputMessage;

    /// Keeps the `session_id` of every frame it is handed, so a test can tag
    /// a frame by the id it publishes it under.
    struct Tagged {
        id: u64,
        seen: Mutex<Vec<String>>,
        takes: bool,
    }

    impl Tagged {
        fn new(id: u64) -> Arc<Self> {
            Arc::new(Self {
                id,
                seen: Mutex::new(Vec::new()),
                takes: true,
            })
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl SessionWatcher for Tagged {
        fn id(&self) -> WatcherId {
            WatcherId(self.id)
        }

        fn deliver(&self, frame: &SessionFrame) -> bool {
            self.seen.lock().unwrap().push(frame.session_id.to_string());
            self.takes
        }
    }

    struct Fixture {
        hub: SessionHub,
        hosts: HostConnections,
        engine: tokio::sync::mpsc::UnboundedReceiver<InputMessage>,
    }

    fn fixture() -> Fixture {
        let hub = SessionHub::active_at(std::time::Instant::now());
        let hosts = HostConnections::default();
        hub.serve_hosts(hosts.clone());
        let (tx, engine) = tokio::sync::mpsc::unbounded_channel();
        hub.answer_host_calls_with(tx);
        Fixture { hub, hosts, engine }
    }

    impl Fixture {
        fn provide(&self, watcher: &Arc<Tagged>) {
            let decl = HostToolDecl::new("HostNote", "d", serde_json::json!({"type": "object"}));
            self.hosts
                .replace_tools(watcher.clone(), vec![decl])
                .unwrap();
        }

        fn ask(&self, tag: &str, call_id: &str, kind: AskKind, host: Option<u64>) {
            self.hub.publish_ask(
                Arc::new(frame_between_turns(tag, SessionEvent::EngineStopped)),
                call_id.to_string(),
                None,
                kind,
                host.map(HostId),
            );
        }

        fn ended(&mut self) -> Vec<(String, &'static str)> {
            std::iter::from_fn(|| self.engine.try_recv().ok())
                .map(|m| match m {
                    InputMessage::HostToolResponse { call_id, end } => (
                        call_id,
                        match end {
                            HostCallEnd::NoHost => "no_host",
                            HostCallEnd::HostGone => "host_gone",
                            _ => "other",
                        },
                    ),
                    _ => panic!("the engine was told something else"),
                })
                .collect()
        }
    }

    #[test]
    fn a_provided_tool_call_goes_to_the_subscribers_and_the_provider() {
        let f = fixture();
        let (watcher, provider) = (Tagged::new(1), Tagged::new(2));
        f.provide(&provider);
        f.hub.subscribe(watcher.clone());

        f.ask("call", "hc_1", AskKind::HostToolCall, Some(2));

        assert_eq!(watcher.seen(), ["call"]);
        assert_eq!(provider.seen(), ["call"]);
    }

    #[test]
    fn a_provider_that_also_subscribes_is_handed_the_frame_once() {
        let f = fixture();
        let provider = Tagged::new(2);
        f.provide(&provider);
        f.hub.subscribe(provider.clone());

        f.ask("call", "hc_1", AskKind::HostToolCall, Some(2));

        assert_eq!(provider.seen(), ["call"]);
    }

    #[test]
    fn a_hook_call_and_its_end_go_only_to_the_provider() {
        let f = fixture();
        let (watcher, provider) = (Tagged::new(1), Tagged::new(2));
        f.provide(&provider);
        f.hub.subscribe(watcher.clone());

        f.ask("hook", "hk_1", AskKind::HookCall, Some(2));
        f.hub.publish_resolved(
            &frame_between_turns("resolved", SessionEvent::Closed),
            "hk_1",
            AskKind::HookCall,
        );

        assert_eq!(provider.seen(), ["hook", "resolved"]);
        assert!(watcher.seen().is_empty());
    }

    #[test]
    fn subscribe_never_returns_a_hook_call() {
        let f = fixture();
        let provider = Tagged::new(2);
        f.provide(&provider);
        f.ask("hook", "hk_1", AskKind::HookCall, Some(2));
        f.ask("call", "hc_1", AskKind::HostToolCall, Some(2));

        let open = f.hub.subscribe(Tagged::new(1));

        let ids: Vec<&str> = open.iter().map(|a| a.prompt_id.as_str()).collect();
        assert_eq!(ids, ["hc_1"]);
    }

    #[test]
    fn a_call_of_an_unregistered_host_ends_at_once_with_no_host() {
        let mut f = fixture();
        f.hub.subscribe(Tagged::new(1));

        f.ask("call", "hc_1", AskKind::HostToolCall, Some(9));
        f.ask("hook", "hk_1", AskKind::HookCall, Some(9));

        assert_eq!(
            f.ended(),
            [("hc_1".into(), "no_host"), ("hk_1".into(), "no_host")]
        );
    }

    #[test]
    fn the_last_subscriber_leaving_does_not_end_a_providers_call() {
        let mut f = fixture();
        let (watcher, provider) = (Tagged::new(1), Tagged::new(2));
        f.provide(&provider);
        f.hub.subscribe(watcher.clone());
        f.ask("call", "hc_1", AskKind::HostToolCall, Some(2));
        f.ask("plain", "hc_2", AskKind::HostToolCall, None);
        assert!(f.ended().is_empty());

        f.hub.unsubscribe(watcher.id());
        f.hub
            .publish(&frame_between_turns("x", SessionEvent::EngineStopped));

        assert_eq!(f.ended(), [("hc_2".into(), "host_gone")]);
    }

    #[test]
    fn end_calls_of_ends_exactly_that_hosts_calls_with_host_gone() {
        let mut f = fixture();
        let (one, two) = (Tagged::new(2), Tagged::new(3));
        f.provide(&one);
        f.hosts.replace_hooks(two.clone(), Vec::new()).unwrap();
        f.hub.subscribe(Tagged::new(1));
        f.ask("a", "hc_1", AskKind::HostToolCall, Some(2));
        f.ask("b", "hk_1", AskKind::HookCall, Some(2));
        f.ask("c", "hc_2", AskKind::HostToolCall, Some(3));
        f.ask("d", "hc_3", AskKind::HostToolCall, None);
        let _ = f.ended();

        f.hub.end_calls_of(HostId(2));

        let mut ended = f.ended();
        ended.sort();
        assert_eq!(
            ended,
            [("hc_1".into(), "host_gone"), ("hk_1".into(), "host_gone")]
        );
    }

    #[test]
    fn a_provider_that_cannot_take_a_frame_has_its_calls_ended_host_gone() {
        let mut f = fixture();
        let provider = Arc::new(Tagged {
            id: 2,
            seen: Mutex::new(Vec::new()),
            takes: false,
        });
        f.provide(&provider);
        f.hub.subscribe(Tagged::new(1));

        f.ask("call", "hc_1", AskKind::HostToolCall, Some(2));

        assert_eq!(f.ended(), [("hc_1".into(), "host_gone")]);
    }

    /// Registered while open, then its connection closes under it.
    struct ClosesLater {
        open: std::sync::atomic::AtomicBool,
    }

    impl SessionWatcher for ClosesLater {
        fn id(&self) -> WatcherId {
            WatcherId(2)
        }

        fn deliver(&self, _: &SessionFrame) -> bool {
            self.is_open()
        }

        fn is_open(&self) -> bool {
            self.open.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn a_provider_found_closed_on_delivery_is_taken_out_of_the_registry() {
        let mut f = fixture();
        let provider = Arc::new(ClosesLater { open: true.into() });
        let decl = HostToolDecl::new("HostNote", "d", serde_json::json!({"type": "object"}));
        f.hosts.replace_tools(provider.clone(), vec![decl]).unwrap();
        provider
            .open
            .store(false, std::sync::atomic::Ordering::SeqCst);

        f.ask("call", "hc_1", AskKind::HostToolCall, Some(2));

        assert_eq!(f.ended(), [("hc_1".into(), "host_gone")]);
        assert!(f.hosts.provider(HostId(2)).is_none());
        assert!(f.hosts.tools().is_empty());
    }
}
