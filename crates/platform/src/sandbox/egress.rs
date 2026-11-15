//! `NetworkMode::Proxy` for both backends: the `*_PROXY` variables a confined
//! command gets, the seatbelt rules that let it reach only the proxy, and for
//! bwrap — whose only lever is a network namespace of the command's own — a
//! relay: a unix socket outside forwarded to the proxy, bound into the
//! namespace, where `socat` turns it back into a port on the loopback.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use super::fence::fenced_ports;
use super::{sandbox_escape, EgressProxy};

pub(super) const NO_PROXY_CONFIGURED: &str =
    "network_mode proxy needs sandbox.egress_proxy, so the network is cut entirely";

/// What a command reads to find the proxy at `http` (and `socks`). Both
/// cases, since curl reads only the lowercase `http_proxy`. An empty
/// `no_proxy` is set too, to override an inherited one.
pub(super) fn proxy_env(
    http: SocketAddr,
    socks: Option<SocketAddr>,
    no_proxy: &[String],
) -> Vec<(String, String)> {
    let http = format!("http://{http}");
    let all = socks.map_or_else(|| http.clone(), |s| format!("socks5h://{s}"));
    let none = no_proxy.join(",");
    [
        ("HTTP_PROXY", &http),
        ("HTTPS_PROXY", &http),
        ("ALL_PROXY", &all),
        ("NO_PROXY", &none),
    ]
    .into_iter()
    .flat_map(|(name, value)| {
        [
            (name.to_string(), value.clone()),
            (name.to_lowercase(), value.clone()),
        ]
    })
    .collect()
}

/// Why `proxy` cannot be let through: it is a port this process serves, and
/// a command connected to it would be the daemon's client.
pub(super) fn served_here(proxy: &EgressProxy) -> Option<String> {
    let clash = proxy.addresses().find(|addr| served(*addr))?;
    Some(format!(
        "egress proxy {clash} is a port this daemon serves, so it is refused"
    ))
}

fn served(addr: SocketAddr) -> bool {
    // `::ffff:127.0.0.1` is `127.0.0.1` on the wire; compare what is dialled.
    let ip = addr.ip().to_canonical();
    fenced_ports().iter().any(|own| {
        let own_ip = own.ip().to_canonical();
        own.port() == addr.port()
            && (own_ip == ip
                || own_ip.is_unspecified()
                || (own_ip.is_loopback() && ip.is_loopback()))
    })
}

/// The seatbelt half: the rules that replace the network ones, the parts not
/// held, and the variables.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) struct Seatbelt {
    pub rules: String,
    pub unmet: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// A profile names a remote host only as `localhost` or `*`, so a proxy off
/// the loopback is held only to its port. The daemon's own ports are denied
/// after these rules, so a proxy on one of them stays shut.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn seatbelt(proxy: Option<&EgressProxy>) -> Seatbelt {
    let mut rules = String::from("(deny network*)\n");
    let Some(proxy) = proxy else {
        return Seatbelt {
            rules,
            unmet: vec![NO_PROXY_CONFIGURED.into()],
            env: Vec::new(),
        };
    };
    let mut unmet: Vec<String> = served_here(proxy).into_iter().collect();
    for addr in proxy.addresses() {
        let host = if addr.ip().is_loopback() {
            "localhost"
        } else {
            unmet.push(format!(
                "egress proxy {addr}: sandbox-exec can restrict a proxy off this machine only \
                 by port, so any host's port {} is reachable",
                addr.port()
            ));
            "*"
        };
        rules.push_str(&format!(
            "(allow network-outbound (remote tcp \"{}\"))\n",
            sandbox_escape(&format!("{host}:{}", addr.port()))
        ));
    }
    Seatbelt {
        rules,
        unmet,
        env: proxy_env(proxy.http, proxy.socks, &proxy.no_proxy),
    }
}

/// Where the relay's sockets appear inside the command's `/tmp`.
pub(super) const INSIDE_DIR: &str = "/tmp/.agix-egress";

/// The bwrap half: the arguments that follow `--unshare-net`, the script
/// that runs the command (`None`: the plain one), the parts not held and the
/// variables.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) struct Namespaced {
    pub args: Vec<String>,
    pub inside: Option<String>,
    pub unmet: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The programs the inside script runs, by absolute path.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) struct Programs<'a> {
    pub socat: Option<&'a str>,
    pub grep: &'a str,
    pub sleep: &'a str,
}

/// Inside the namespace the proxy is `127.0.0.1:<its port>`, wherever it
/// really is: the relay outside connects to the configured address. Without
/// `socat`, or with a relay that will not start, the network stays cut.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn namespaced(
    proxy: Option<&EgressProxy>,
    programs: &Programs<'_>,
    relay: impl Fn(SocketAddr) -> std::io::Result<PathBuf>,
) -> Namespaced {
    let cut = |unmet: String, env| Namespaced {
        args: Vec::new(),
        inside: None,
        unmet: vec![unmet],
        env,
    };
    let Some(proxy) = proxy else {
        return cut(NO_PROXY_CONFIGURED.into(), Vec::new());
    };
    let loopback = |addr: SocketAddr| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port());
    let env = proxy_env(
        loopback(proxy.http),
        proxy.socks.map(loopback),
        &proxy.no_proxy,
    );
    if let Some(why) = served_here(proxy) {
        return cut(why, env);
    }
    let Some(socat) = programs.socat else {
        return cut(
            "egress proxy: socat not found, so the network is cut entirely".into(),
            env,
        );
    };
    let names = std::iter::once("http").chain(proxy.socks.map(|_| "socks"));
    let mut args = Vec::new();
    let mut listeners = Vec::new();
    for (name, addr) in names.zip(proxy.addresses()) {
        let outside = match relay(addr) {
            Ok(path) => path,
            Err(e) => {
                return cut(
                    format!(
                        "egress proxy: the relay to {addr} could not start ({e}), so the \
                         network is cut entirely"
                    ),
                    env,
                )
            }
        };
        let inside = format!("{INSIDE_DIR}/{name}.sock");
        args.extend([
            "--bind".into(),
            outside.display().to_string(),
            inside.clone(),
        ]);
        listeners.push((addr.port(), inside));
    }
    Namespaced {
        args,
        inside: Some(inside_script(socat, programs, &listeners)),
        unmet: Vec::new(),
        env,
    }
}

/// The inside script with relays: fds 3 and 4 back to 1 and 2, a `socat`
/// per listener in the background, a wait until each listens (its line in
/// `/proc/net/tcp`), then the command. Not `exec "$@"`: bwrap's PID 1 waits
/// for every process in the namespace, so a relay left running would keep
/// the sandbox, and the command's output, open.
fn inside_script(socat: &str, programs: &Programs<'_>, listeners: &[(u16, String)]) -> String {
    let mut s = String::from("exec 1>&3 2>&4 3>&- 4>&-\nrelays=\n");
    for (port, socket) in listeners {
        s.push_str(&format!(
            "{socat} TCP-LISTEN:{port},bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:{socket} \
             </dev/null >/dev/null 2>&1 &\nrelays=\"$relays $!\"\n"
        ));
    }
    let listening: Vec<String> = listeners
        .iter()
        .map(|(port, _)| {
            format!(
                "{} -q ': 0100007F:{port:04X} 00000000:0000 0A' /proc/net/tcp",
                programs.grep
            )
        })
        .collect();
    s.push_str(&format!(
        "n=0\nwhile [ \"$n\" -lt 50 ] && ! {{ {}; }}; do {} 0.02; n=$((n+1)); done\n",
        listening.join(" && "),
        programs.sleep
    ));
    s.push_str("\"$@\"\nstatus=$?\nkill $relays 2>/dev/null\nexit \"$status\"");
    s
}

/// The unix socket that forwards to `target`, started on first use and kept
/// for the life of the process: one per proxy address, in a directory of
/// this process's own (0700) in the system temporary directory, which the
/// command's `/tmp` tmpfs hides.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn relay(target: SocketAddr) -> std::io::Result<PathBuf> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static RELAYS: Mutex<Option<HashMap<SocketAddr, PathBuf>>> = Mutex::new(None);
    let mut relays = RELAYS.lock().unwrap_or_else(|e| e.into_inner());
    let relays = relays.get_or_insert_with(HashMap::new);
    if let Some(path) = relays.get(&target).filter(|p| p.exists()) {
        return Ok(path.clone());
    }
    let path = relay_dir()?.join(format!("{}-{}.sock", target.port(), relays.len()));
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    std::thread::Builder::new()
        .name("agix-egress-relay".into())
        .spawn(move || {
            for conn in listener.incoming().flatten() {
                let _ = std::thread::Builder::new()
                    .name("agix-egress-conn".into())
                    .spawn(move || forward(conn, target));
            }
        })?;
    relays.insert(target, path.clone());
    Ok(path)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn relay_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    if let Some(dir) = DIR.get() {
        return Ok(dir.clone());
    }
    let base = std::env::temp_dir();
    let pid = std::process::id();
    let mut last = None;
    for n in 0..16 {
        let dir = base.join(format!("agix-egress-{pid}-{n}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(DIR.get_or_init(|| dir).clone()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no relay directory")))
}

/// One relayed connection. The fence table is read again here: a port the
/// daemon starts serving after the relay did is refused too.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn forward(client: std::os::unix::net::UnixStream, target: SocketAddr) {
    use std::net::{Shutdown, TcpStream};
    if served(target) {
        return;
    }
    let Ok(upstream) = TcpStream::connect_timeout(&target, std::time::Duration::from_secs(10))
    else {
        return;
    };
    let (Ok(mut client_in), Ok(mut upstream_out)) = (client.try_clone(), upstream.try_clone())
    else {
        return;
    };
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_in, &mut upstream_out);
        let _ = upstream_out.shutdown(Shutdown::Write);
    });
    let (mut upstream_in, mut client_out) = (upstream, client);
    let _ = std::io::copy(&mut upstream_in, &mut client_out);
    let _ = client_out.shutdown(Shutdown::Write);
    let _ = up.join();
}
