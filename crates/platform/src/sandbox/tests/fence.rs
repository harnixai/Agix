use super::*;

/// No sandboxed command connects to a unix socket in the state root or
/// one this process serves, however it names the socket: the daemon's own
/// socket is one or the other, and a command connected to it is the
/// daemon's host. Other sockets stay reachable.
#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_command_cannot_connect_to_the_daemons_socket() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let scene = s.state_root.join("scenes").join("code");
    std::fs::create_dir_all(&scene).unwrap();
    let in_state = listening(scene.join("daemon.sock"));
    let served = listening(s.elsewhere.join("served.sock"));
    let other = listening(s.elsewhere.join("other.sock"));
    let _fence = fence_socket(&served);

    let nc = |path: &Path| format!("nc -U '{}' < /dev/null", path.display());
    for socket in [&in_state, &served, &other] {
        assert!(
            outside("nc", &["-U", &socket.display().to_string()]),
            "{} is not reachable at all",
            socket.display()
        );
    }
    assert!(s.runs(&nc(&other)), "an unrelated socket was refused");
    for socket in [&in_state, &served] {
        assert!(!s.runs(&nc(socket)), "{} was reached", socket.display());
    }
    let link = format!(
        "ln -s '{}' link.sock && nc -U link.sock < /dev/null",
        in_state.display()
    );
    assert!(!s.runs(&link), "reached through a symlink");
    let relative = format!("cd '{}' && nc -U daemon.sock < /dev/null", scene.display());
    assert!(!s.runs(&relative), "reached by a relative name");
}

#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_command_cannot_read_a_fenced_secret() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let secret = s.elsewhere.join("unix.token");
    let other = s.elsewhere.join("other.txt");
    std::fs::write(&secret, "t").unwrap();
    std::fs::write(&other, "o").unwrap();
    let fence = fence_secret(&secret);
    assert!(s.runs(&format!("cat '{}'", other.display())));
    assert!(!s.runs(&format!("cat '{}'", secret.display())));
    drop(fence);
    assert!(
        s.runs(&format!("cat '{}'", secret.display())),
        "the file stayed fenced after its fence was dropped"
    );
}

/// The loopback port a daemon listens on is refused the same way: the
/// token is not in a command's environment, but `--token` is in `ps`.
/// Once the fence is dropped with its listener, the port is anybody's
/// again.
#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_command_cannot_connect_to_the_daemons_port() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let accepting = |listener: std::net::TcpListener| {
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                drop(conn);
            }
        });
        port
    };
    let served = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let fence = fence_port(served.local_addr().unwrap());
    let served = accepting(served);
    let other = accepting(std::net::TcpListener::bind("127.0.0.1:0").unwrap());

    assert!(s.runs(&format!("nc -z 127.0.0.1 {other}")));
    for host in ["127.0.0.1", "localhost"] {
        assert!(
            !s.runs(&format!("nc -z {host} {served}")),
            "{host}:{served} was reached"
        );
    }
    drop(fence);
    assert!(
        s.runs(&format!("nc -z 127.0.0.1 {served}")),
        "the port stayed fenced after its fence was dropped"
    );
}

/// sandbox-exec names a remote host only as `*` or `localhost`, and a
/// profile naming any other does not compile: an allowlist of real
/// domains must still run the command, with the network shut but for
/// what the allowlist names and the profile can say.
#[cfg(target_os = "macos")]
#[test]
fn an_allowlist_of_domains_runs_the_command_with_the_rest_of_the_network_shut() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            drop(conn);
        }
    });
    let reach = format!("nc -z 127.0.0.1 {port}");
    let domains = ["example.com", "registry.npmjs.org"];

    assert!(s.runs_allowing(&domains, "true"), "the command did not run");
    assert!(
        !s.runs_allowing(&domains, &reach),
        "127.0.0.1:{port}, which the allowlist does not name, was reached"
    );
    assert!(
        s.runs_allowing(&["example.com", "localhost"], &reach),
        "localhost, which the allowlist names, was refused"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn a_socket_is_fenced_until_its_fence_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = base::path::resolve_physically(&dir.path().join("served.sock"));
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let fence = fence_socket(&path);
    let twice = fence_socket(&path);
    assert!(fenced_sockets().contains(&path));
    drop(twice);
    assert!(
        fenced_sockets().contains(&path),
        "one listener's fence came down with another's"
    );
    drop(fence);
    assert!(!fenced_sockets().contains(&path));
}

#[cfg(target_os = "macos")]
#[test]
fn the_profile_refuses_unix_sockets_in_the_state_root_and_the_fenced_ones() {
    let socket = std::env::temp_dir().join(format!("fenced-{}.sock", std::process::id()));
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let _fence = fence_socket(&socket);
    let policy = SandboxPolicy {
        state_root: Some(PathBuf::from("/srv/agix-state")),
        network_mode: NetworkMode::Allowlist,
        allowed_domains: vec!["localhost".into()],
        ..Default::default()
    };
    let profile = build_macos_profile(
        Path::new("/work/project"),
        &[],
        &policy,
        &read_scope::UserDirs::of(INHERITED),
    );
    let _ = std::fs::remove_file(&socket);

    let state_rule = "(deny network-outbound (remote unix-socket (subpath \"/srv/agix-state\")))";
    let socket_rule = format!(
        "(deny network-outbound (remote unix-socket (literal \"{}\")))",
        base::path::resolve_physically(&socket).display()
    );
    let allow = profile.find("(allow network-outbound").unwrap();
    for rule in [state_rule, &socket_rule] {
        let at = profile
            .find(rule)
            .unwrap_or_else(|| panic!("{rule}\n{profile}"));
        assert!(at > allow, "an allowlist entry would reopen it:\n{profile}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn a_fenced_secret_stays_unreadable_whatever_allow_read_says() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("unix.token");
    std::fs::write(&secret, "t").unwrap();
    let _fence = fence_secret(&secret);
    let policy = SandboxPolicy {
        deny_read: vec![PathBuf::from("/nowhere"), dir.path().to_path_buf()],
        allow_read: vec![dir.path().to_path_buf()],
        ..Default::default()
    };
    let profile = build_macos_profile(
        Path::new("/work/project"),
        &[],
        &policy,
        &read_scope::UserDirs::of(INHERITED),
    );
    let rule = format!(
        "(deny file-read* (literal \"{}\"))",
        base::path::resolve_physically(&secret).display()
    );
    let denied = profile
        .find(&rule)
        .unwrap_or_else(|| panic!("{rule}\n{profile}"));
    let allowed = profile.find("(allow file-read*").unwrap();
    assert!(denied > allowed, "allow_read would reopen it:\n{profile}");
}
