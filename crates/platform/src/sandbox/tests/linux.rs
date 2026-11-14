use super::*;

#[cfg(target_os = "linux")]
#[test]
fn linux_wrap_either_uses_bwrap_or_falls_back() {
    let cmd = wrap(opts("ls", Path::new("/tmp/work")));
    match cmd.mode {
        SandboxMode::LinuxBwrap => {
            assert_eq!(cmd.spec.program, "sh");
            assert_eq!(cmd.spec.args[3], "bwrap");
            assert!(cmd.spec.args.iter().any(|a| a == "--ro-bind"));
            assert!(cmd.spec.args.iter().any(|a| a == "/tmp/work"));
            let bash_pos = cmd
                .spec
                .args
                .iter()
                .position(|a| a == "bash")
                .expect("bwrap args must end with `-- bash -c <cmd>`");
            assert_eq!(cmd.spec.args[bash_pos + 1], "-c");
            assert_eq!(cmd.spec.args[bash_pos + 2], "ls");
        }
        SandboxMode::Unavailable => {
            // bwrap 没装 —— 平台合理 fallback
            assert_eq!(cmd.spec.program, "bash");
            assert_eq!(cmd.spec.args, vec!["-c", "ls"]);
        }
        other => panic!("unexpected mode on linux: {other:?}"),
    }
}

/// bwrap's own messages — a mount it could not make, namespaces it may
/// not create — still reach the command's stderr, after what the command
/// itself printed, and its exit status is the command's.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn what_bwrap_itself_says_reaches_stderr() {
    let fake_bwrap = "echo printed >&3; echo complained >&4; echo 'bwrap: refused' >&2; exit 1";
    let out = std::process::Command::new("sh")
        .args(["-c", &bwrap_stdio_outside(), "sh", "sh", "-c", fake_bwrap])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "printed\n");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "complained\nbwrap: refused\n"
    );
    assert_eq!(out.status.code(), Some(1));
}

/// The outer wrapper, which runs outside the sandbox, keeps its scratch
/// file off every writable root: in the session `TMPDIR`, which a confined
/// process shares, a process an earlier command left running could swap it
/// for a symlink and make the wrapper write through it outside the sandbox.
/// The file is pinned to the host's `/tmp`, which the sandbox always
/// shadows with a private tmpfs, so no confined command reaches it. The helpers are
/// addressed absolutely so `PATH` cannot redirect them.
#[cfg(target_os = "linux")]
#[test]
fn the_outer_wrapper_keeps_its_scratch_file_off_every_writable_root() {
    let project = Path::new("/tmp/work/project");
    let session_tmp = PathBuf::from("/home/u/.agix/tmp/session-1");
    let mut o = opts("ls", project);
    let extras = [session_tmp.clone()];
    o.additional_writable = &extras;
    let cmd = wrap(o);
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    // The outer shell script is the one the wrapping `sh -c` runs.
    assert_eq!(cmd.spec.args[0], "-c");
    let script = &cmd.spec.args[1];
    assert_eq!(cmd.spec.args[2], "sh");
    assert_eq!(cmd.spec.args[3], "bwrap");

    assert!(
        script.contains("-p /tmp"),
        "the scratch file must be pinned to the host /tmp: {script}"
    );
    for root in [
        project.display().to_string(),
        session_tmp.display().to_string(),
    ] {
        assert!(
            !script.contains(&root),
            "the wrapper names a writable root ({root}): {script}"
        );
    }
    for util in ["mktemp", "rm", "cat"] {
        let absolute = trusted_bin(util);
        if absolute.starts_with('/') {
            assert!(
                script.contains(&format!("{absolute} "))
                    || script.contains(&format!("{absolute}\n")),
                "{util} is not addressed by its absolute path {absolute}: {script}"
            );
        }
    }
}

/// A configuration file that does not exist yet is what a read-only bind
/// of the file itself cannot cover: bwrap skips that bind, and the file
/// can then be created. The directory around it is what is read-only.
#[cfg(target_os = "linux")]
#[test]
fn an_absent_project_settings_file_cannot_be_created() {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".agix").join("scratchpad")).unwrap();
    let cmd = wrap(opts(
        "echo '{}' > .agix/settings.local.json",
        project.path(),
    ));
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    let agix = project
        .path()
        .canonicalize()
        .unwrap()
        .join(".agix")
        .display()
        .to_string();
    let end = cmd.spec.args.iter().position(|a| a == "--").unwrap();
    let args = &cmd.spec.args[..end];
    let at = |pair: [&str; 2]| args.windows(2).position(|w| w == pair);
    let scratchpad = format!("{agix}/scratchpad");
    let masked = at(["--tmpfs", &agix]).unwrap_or_else(|| panic!("{args:?}"));
    let opened = at(["--bind-try", &scratchpad]).unwrap_or_else(|| panic!("{args:?}"));
    let shut = at(["--remount-ro", &agix]).unwrap_or_else(|| panic!("{args:?}"));
    assert!(masked < opened && opened < shut, "{args:?}");
    assert!(
        !args.iter().any(|a| a.ends_with("settings.local.json")),
        "{args:?}"
    );

    let namespaces_work = std::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "true"])
        .status()
        .is_ok_and(|s| s.success());
    if !namespaces_work {
        return;
    }
    let out = std::process::Command::new(&cmd.spec.program)
        .args(&cmd.spec.args)
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!project
        .path()
        .join(".agix")
        .join("settings.local.json")
        .exists());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_deny_all_network_uses_unshare_net() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy.network_mode = NetworkMode::DenyAll;
    let cmd = wrap(o);
    if cmd.mode != SandboxMode::LinuxBwrap {
        return; // bwrap not installed in this environment
    }
    assert!(cmd.spec.args.iter().any(|a| a == "--unshare-net"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_a_read_scope_binds_its_roots_one_by_one() {
    let project = PathBuf::from("/tmp/work");
    let policy = SandboxPolicy::default().strict([project.clone()], None);
    let cmd = wrap(SandboxOptions {
        command: "ls",
        cwd: &project,
        additional_writable: &[],
        disable: false,
        policy,
    });
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    let args = &cmd.spec.args;
    let at = |triple: [&str; 3]| args.windows(3).position(|w| w == triple);
    assert_eq!(at(["--ro-bind", "/", "/"]), None, "{args:?}");
    assert!(at(["--ro-bind-try", "/usr", "/usr"]).is_some(), "{args:?}");
    let tmpfs = args
        .windows(2)
        .position(|w| w == ["--tmpfs", "/tmp"])
        .unwrap();
    let bound = at(["--ro-bind-try", "/tmp/work", "/tmp/work"]).unwrap();
    assert!(
        tmpfs < bound,
        "the project is hidden under /tmp's tmpfs: {args:?}"
    );
    assert!(args.iter().any(|a| a == "--unshare-net"));
    assert!(!args
        .windows(3)
        .any(|w| w[0] == "--bind" && w[1] == "/tmp/work"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_unattended_unshares_ipc() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    let attended = wrap(o.clone());
    if attended.mode != SandboxMode::LinuxBwrap {
        return;
    }
    o.policy.deny_ipc_escapes = true;
    let unattended = wrap(o);
    assert!(!attended.spec.args.iter().any(|a| a == "--unshare-ipc"));
    assert!(unattended.spec.args.iter().any(|a| a == "--unshare-ipc"));
}

/// `/dev/null` is mounted over every socket in the state root's scene
/// directories and over every socket this process serves, after the state
/// root's tmpfs mask and the re-exposure of its contents so it lands on
/// top: a socket re-bound as a readable sibling must still end up hidden.
#[cfg(target_os = "linux")]
#[test]
fn linux_hides_the_daemons_sockets() {
    let state = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let scene = state.path().join("scenes").join("code");
    std::fs::create_dir_all(&scene).unwrap();
    let in_state = scene.join("daemon.sock");
    let served = elsewhere.path().join("served.sock");
    let _listeners =
        [&in_state, &served].map(|p| std::os::unix::net::UnixListener::bind(p).unwrap());
    let _fence = fence_socket(&served);

    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy.state_root = Some(state.path().to_path_buf());
    let args = wrap(o).spec.args;
    if !args.iter().any(|a| a == "--") {
        return;
    }
    let state_mask = args
        .windows(2)
        .position(|w| {
            w[0] == "--tmpfs" && Path::new(&w[1]) == base::path::resolve_physically(state.path())
        })
        .expect("the state root is masked with a tmpfs");
    for socket in [&in_state, &served] {
        let socket = base::path::resolve_physically(socket).display().to_string();
        let hidden = args
            .windows(3)
            .position(|w| w[0] == "--ro-bind-try" && w[1] == "/dev/null" && w[2] == socket)
            .unwrap_or_else(|| panic!("{socket} is not hidden: {args:?}"));
        assert!(
            hidden > state_mask,
            "{socket} is hidden before the state root's mask lands"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn the_session_bus_is_found_where_the_environment_says() {
    assert_eq!(
        bus_paths(
            Some(PathBuf::from("/run/user/1000")),
            Some("unix:path=/run/user/1000/bus,guid=x;unix:guid=y,path=/tmp/dbus-z"),
        ),
        [
            PathBuf::from("/run/user/1000/bus"),
            PathBuf::from("/run/user/1000/systemd"),
            PathBuf::from("/tmp/dbus-z"),
        ]
    );
    assert!(bus_paths(None, Some("unix:abstract=/tmp/dbus-x")).is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_unrestricted_network_omits_unshare_net() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy.network_mode = NetworkMode::Unrestricted;
    let cmd = wrap(o);
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    assert!(!cmd.spec.args.iter().any(|a| a == "--unshare-net"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_allowlist_network_falls_back_to_unshare_net() {
    // bwrap has no domain-level filter — Allowlist must fail closed
    // (full isolation), not silently degrade to Unrestricted.
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy.network_mode = NetworkMode::Allowlist;
    o.policy.allowed_domains = vec!["api.example.com".into()];
    let cmd = wrap(o);
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    assert!(cmd.spec.args.iter().any(|a| a == "--unshare-net"));
}

/// A process an earlier command left running cannot read a credential the
/// engine writes into the state root while a later command is confined.
/// Binding over each file at wrap time would miss a file absent then, and
/// `/dev/null` bound over an existing one falls off when the engine rewrites
/// it through a temporary file and a rename (both measured on bwrap 0.8).
/// The state root's credential directories are covered with a tmpfs
/// instead, so neither a late create nor a rename reaches the confined
/// command — while a
/// readable sibling stays readable.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn a_lingering_process_cannot_read_a_credential_written_after_it_started() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join(".agix")).unwrap();
    write_file(&state.join("skills/s/SKILL.md"), "fine");
    let existing = state.join("settings.local.json");
    write_file(&existing, "{\"old\":true}");
    let fresh = state.join("settings.json");
    let policy = SandboxPolicy {
        state_root: Some(state.clone()),
        ..Default::default()
    };
    let command = format!(
        "( until [ -e go ]; do sleep 0.02; done; \
              {{ cat '{existing}'; echo; cat '{fresh}'; echo; cat '{skill}'; }} > out 2>/dev/null; \
              touch done ) & exit 0",
        existing = existing.display(),
        fresh = fresh.display(),
        skill = state.join("skills/s/SKILL.md").display(),
    );
    let confined = PlatformSandbox.confine(
        ProcessSpec::new("bash", &project).args(["-c", &command].map(String::from)),
        &policy,
    );
    assert_eq!(
        confined.enforcement,
        Enforcement::Full,
        "{:?}",
        confined.unmet
    );
    let mut child = std::process::Command::new(&confined.spec.program)
        .args(&confined.spec.args)
        .current_dir(&project)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let secret = "agix-after-the-fact-secret";
    std::fs::write(&fresh, secret).unwrap();
    let tmp = state.join(".settings.local.json.9999.tmp");
    std::fs::write(&tmp, secret).unwrap();
    std::fs::rename(&tmp, &existing).unwrap();
    std::fs::write(project.join("go"), "").unwrap();

    let done = (0..500).any(|_| {
        if project.join("done").exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        false
    });
    let _ = child.wait();
    assert!(done, "the background reader never finished");
    let saw = std::fs::read_to_string(project.join("out")).unwrap_or_default();
    assert!(
        !saw.contains(secret),
        "a lingering process read a credential written after it started: {saw:?}"
    );
    assert!(
        saw.contains("fine"),
        "the mask shut a readable file too: {saw:?}"
    );
}

/// The hooks directory `core.hooksPath` names and the files `.git/config`
/// includes are read-only mounts, absent or not.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn linux_the_hooks_path_and_included_config_are_read_only() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let git = root.join(".git");
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(
        git.join("config"),
        "[core]\n\thooksPath = .githooks\n[include]\n\tpath = ../.gitconfig.local\n",
    )
    .unwrap();
    std::fs::write(root.join(".gitconfig.local"), "").unwrap();
    let cmd = wrap(opts("true", &root));
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    let end = cmd.spec.args.iter().position(|a| a == "--").unwrap();
    let args = &cmd.spec.args[..end];
    let hooks = root.join(".githooks").display().to_string();
    let included = root.join(".gitconfig.local").display().to_string();
    let at = |triple: [&str; 3]| args.windows(3).position(|w| w == triple);
    let bound = at([
        "--bind",
        &root.display().to_string(),
        &root.display().to_string(),
    ])
    .unwrap();
    assert!(
        at(["--tmpfs", &hooks, "--remount-ro"]).is_some_and(|i| i > bound),
        "{args:?}"
    );
    assert!(
        at(["--ro-bind-try", &included, &included]).is_some_and(|i| i > bound),
        "{args:?}"
    );
}
