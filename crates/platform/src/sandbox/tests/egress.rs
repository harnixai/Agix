use super::*;
use crate::sandbox::egress::{
    namespaced, proxy_env, relay, seatbelt, Programs, NO_PROXY_CONFIGURED,
};
use std::io::{Read, Write};
use std::net::SocketAddr;

fn proxy(http: &str, socks: Option<&str>) -> EgressProxy {
    EgressProxy {
        http: http.parse().unwrap(),
        socks: socks.map(|s| s.parse().unwrap()),
        no_proxy: Vec::new(),
    }
}

const PROGRAMS: Programs<'static> = Programs {
    socat: Some("/usr/bin/socat"),
    grep: "/usr/bin/grep",
    sleep: "/bin/sleep",
};

fn outside_socket(addr: SocketAddr) -> std::io::Result<PathBuf> {
    Ok(PathBuf::from(format!(
        "/run/agix-test/{}.sock",
        addr.port()
    )))
}

fn var<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

#[test]
fn the_variables_point_every_client_at_the_proxy_and_nothing_past_it() {
    let env = proxy_env(
        "127.0.0.1:3128".parse().unwrap(),
        Some("127.0.0.1:1080".parse().unwrap()),
        &[],
    );
    assert_eq!(env.len(), 8);
    for name in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
        assert_eq!(var(&env, name), Some("http://127.0.0.1:3128"), "{name}");
    }
    for name in ["ALL_PROXY", "all_proxy"] {
        assert_eq!(var(&env, name), Some("socks5h://127.0.0.1:1080"), "{name}");
    }
    for name in ["NO_PROXY", "no_proxy"] {
        assert_eq!(var(&env, name), Some(""), "{name}");
    }
    let env = proxy_env(
        "[::1]:3128".parse().unwrap(),
        None,
        &["a".into(), "b".into()],
    );
    assert_eq!(var(&env, "all_proxy"), Some("http://[::1]:3128"));
    assert_eq!(var(&env, "NO_PROXY"), Some("a,b"));
}

#[test]
fn seatbelt_lets_through_only_the_proxys_ports_on_the_loopback() {
    let held = seatbelt(Some(&proxy("127.0.0.1:3128", Some("[::1]:1080"))));
    assert!(held.unmet.is_empty(), "{:?}", held.unmet);
    assert_eq!(
        held.rules,
        "(deny network*)\n\
         (allow network-outbound (remote tcp \"localhost:3128\"))\n\
         (allow network-outbound (remote tcp \"localhost:1080\"))\n"
    );
    assert_eq!(var(&held.env, "http_proxy"), Some("http://127.0.0.1:3128"));

    let elsewhere = seatbelt(Some(&proxy("10.0.0.5:3128", None)));
    assert!(elsewhere.rules.contains("(remote tcp \"*:3128\")"));
    assert_eq!(
        elsewhere.unmet,
        [
            "egress proxy 10.0.0.5:3128: sandbox-exec can restrict a proxy off this machine \
          only by port, so any host's port 3128 is reachable"
        ]
    );
    assert_eq!(
        var(&elsewhere.env, "HTTPS_PROXY"),
        Some("http://10.0.0.5:3128")
    );

    let missing = seatbelt(None);
    assert_eq!(missing.rules, "(deny network*)\n");
    assert_eq!(missing.unmet, [NO_PROXY_CONFIGURED]);
}

#[test]
fn the_namespace_reaches_the_proxy_on_its_own_loopback_through_the_relay() {
    let relayed = namespaced(
        Some(&proxy("10.0.0.5:3128", Some("10.0.0.5:1080"))),
        &PROGRAMS,
        outside_socket,
    );
    assert!(relayed.unmet.is_empty(), "{:?}", relayed.unmet);
    assert_eq!(
        relayed.args,
        [
            "--bind",
            "/run/agix-test/3128.sock",
            "/tmp/.agix-egress/http.sock",
            "--bind",
            "/run/agix-test/1080.sock",
            "/tmp/.agix-egress/socks.sock",
        ]
    );
    let inside = relayed.inside.unwrap();
    for part in [
        "exec 1>&3 2>&4 3>&- 4>&-\n",
        "/usr/bin/socat TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr \
         UNIX-CONNECT:/tmp/.agix-egress/http.sock",
        "TCP-LISTEN:1080,bind=127.0.0.1",
        "/usr/bin/grep -q ': 0100007F:0C38 00000000:0000 0A' /proc/net/tcp",
        "/usr/bin/grep -q ': 0100007F:0438 00000000:0000 0A' /proc/net/tcp",
        "\"$@\"\nstatus=$?\nkill $relays",
    ] {
        assert!(inside.contains(part), "{part:?} not in:\n{inside}");
    }
    assert!(!inside.contains("exec \"$@\""), "{inside}");
    assert_eq!(
        var(&relayed.env, "http_proxy"),
        Some("http://127.0.0.1:3128")
    );
    assert_eq!(
        var(&relayed.env, "ALL_PROXY"),
        Some("socks5h://127.0.0.1:1080")
    );
}

#[test]
fn without_socat_or_a_relay_the_namespace_stays_cut() {
    let one = proxy("127.0.0.1:3128", None);
    let no_socat = namespaced(
        Some(&one),
        &Programs {
            socat: None,
            ..PROGRAMS
        },
        outside_socket,
    );
    assert!(no_socat.args.is_empty() && no_socat.inside.is_none());
    assert_eq!(
        no_socat.unmet,
        ["egress proxy: socat not found, so the network is cut entirely"]
    );

    let no_relay = namespaced(Some(&one), &PROGRAMS, |_| {
        Err(std::io::Error::other("no room"))
    });
    assert!(no_relay.args.is_empty() && no_relay.inside.is_none());
    assert!(
        no_relay.unmet[0].contains("relay to 127.0.0.1:3128 could not start (no room)"),
        "{:?}",
        no_relay.unmet
    );

    let none = namespaced(None, &PROGRAMS, outside_socket);
    assert_eq!(none.unmet, [NO_PROXY_CONFIGURED]);
}

#[test]
#[allow(clippy::unwrap_used)]
fn a_proxy_on_the_daemons_own_port_is_refused_by_both_halves() {
    let served = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = served.local_addr().unwrap();
    let _fence = fence_port(addr);
    let on_it = proxy(&addr.to_string(), None);
    let why = format!("egress proxy {addr} is a port this daemon serves, so it is refused");
    assert_eq!(seatbelt(Some(&on_it)).unmet, std::slice::from_ref(&why));
    let cut = namespaced(Some(&on_it), &PROGRAMS, outside_socket);
    assert_eq!(cut.unmet, [why]);
    assert!(cut.args.is_empty() && cut.inside.is_none());
}

#[test]
#[allow(clippy::unwrap_used)]
fn a_proxy_on_the_daemons_own_port_written_as_a_mapped_v6_address_is_refused() {
    let served = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = served.local_addr().unwrap();
    let _fence = fence_port(addr);
    let mapped = proxy(&format!("[::ffff:127.0.0.1]:{}", addr.port()), None);
    let cut = namespaced(Some(&mapped), &PROGRAMS, outside_socket);
    assert!(
        cut.unmet
            .iter()
            .any(|u| u.contains("port this daemon serves")),
        "{:?}",
        cut.unmet
    );
}

#[test]
#[allow(clippy::unwrap_used)]
fn the_relay_forwards_its_socket_to_the_proxy_and_not_to_a_fenced_port() {
    let upstream = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let target = upstream.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut conn in upstream.incoming().flatten() {
            let mut buf = [0u8; 5];
            if conn.read_exact(&mut buf).is_ok() {
                let _ = conn.write_all(&buf);
            }
        }
    });
    let path = relay(target).unwrap();
    assert_eq!(relay(target).unwrap(), path, "one relay per address");
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = path
            .parent()
            .unwrap()
            .metadata()
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir & 0o777, 0o700);
    }
    let mut conn = std::os::unix::net::UnixStream::connect(&path).unwrap();
    conn.write_all(b"hello").unwrap();
    let mut echoed = String::new();
    conn.read_to_string(&mut echoed).unwrap_or_default();
    assert_eq!(echoed, "hello");

    let _fence = fence_port(target);
    let mut conn = std::os::unix::net::UnixStream::connect(&path).unwrap();
    let _ = conn.write_all(b"again");
    let mut echoed = String::new();
    let _ = conn.read_to_string(&mut echoed);
    assert!(echoed.is_empty(), "a fenced port was relayed to: {echoed}");
}
