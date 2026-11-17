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
        .args([
            "-c",
            &bwrap_stdio_outside(&Cleanup::default()),
            "sh",
            "sh",
            "-c",
            fake_bwrap,
        ])
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
    for (own, dir) in [
        (".agix", ".agix"),
        (".codix", ".codix"),
        (".codix", ".agix"),
    ] {
        an_absent_settings_file_cannot_be_created_in(own, dir);
    }
}

/// In `dir` of a project whose instance's own directory is `own`.
#[cfg(target_os = "linux")]
#[allow(clippy::unwrap_used)]
fn an_absent_settings_file_cannot_be_created_in(own: &str, dir: &str) {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(dir).join("scratchpad")).unwrap();
    let command = format!("echo '{{}}' > {dir}/settings.local.json");
    let mut o = opts(&command, project.path());
    o.policy.layout = base::paths::Layout::new(own).unwrap();
    let cmd = wrap(o);
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    let agix = project
        .path()
        .canonicalize()
        .unwrap()
        .join(dir)
        .display()
        .to_string();
    let end = cmd.spec.args.iter().position(|a| a == "--").unwrap();
    let args = &cmd.spec.args[..end];
    let at = |pair: [&str; 2]| args.windows(2).position(|w| w == pair);
    let scratchpad = format!("{agix}/scratchpad");
    let masked = at(["--tmpfs", &agix]).unwrap_or_else(|| panic!("{args:?}"));
    // A foreign root has no working space: its scratchpad stays read-only.
    let how = if own == dir {
        "--bind-try"
    } else {
        "--ro-bind-try"
    };
    let opened = at([how, &scratchpad]).unwrap_or_else(|| panic!("{args:?}"));
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
        .join(dir)
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

#[cfg(target_os = "linux")]
#[test]
fn linux_proxy_mode_unshares_the_network_and_relays_after_the_tmp_tmpfs() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy.network_mode = NetworkMode::Proxy;
    o.policy.egress_proxy = Some(EgressProxy {
        http: "127.0.0.1:3128".parse().unwrap(),
        socks: None,
        no_proxy: Vec::new(),
    });
    let cmd = wrap(o);
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    let args = &cmd.spec.args;
    assert!(args.iter().any(|a| a == "--unshare-net"));
    let http_proxy = cmd
        .spec
        .env
        .iter()
        .find(|(name, _)| name == "http_proxy")
        .map(|(_, value)| value.as_str());
    assert_eq!(http_proxy, Some("http://127.0.0.1:3128"));
    if find_trusted_bin("socat").is_none() {
        assert_eq!(cmd.enforcement, Enforcement::Partial);
        assert!(
            cmd.unmet.iter().any(|u| u.contains("socat not found")),
            "{:?}",
            cmd.unmet
        );
        return;
    }
    assert_eq!(cmd.enforcement, Enforcement::Full, "{:?}", cmd.unmet);
    let tmpfs = args
        .windows(2)
        .position(|w| w[0] == "--tmpfs" && w[1] == "/tmp")
        .expect("a /tmp tmpfs");
    let bind = args
        .iter()
        .position(|a| a == "/tmp/.agix-egress/http.sock")
        .expect("the relay's socket bound in");
    assert!(tmpfs < bind, "{args:?}");
    assert!(args
        .iter()
        .any(|a| a.contains("TCP-LISTEN:3128,bind=127.0.0.1")));
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

/// What a command leaves running cannot write a worktree made after it
/// started: `.git/worktrees/` and `.wkts/` are read-only, the
/// worktrees already in them opened again beneath and their control files
/// shut on top. The engine's own git, which makes worktrees, is not held so.
/// A command in a worktree has its `.git` link shut and guards the git
/// directory the policy names.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn linux_a_worktree_made_later_is_read_only_to_an_earlier_command() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let git = root.join(".git");
    let admin = git.join("worktrees/wt");
    let worktree = root.join(".wkts/wt");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::write(admin.join("gitdir"), "x\n").unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", admin.display()),
    )
    .unwrap();
    let args_of = |cmd: Confined| -> Vec<String> {
        let end = cmd.spec.args.iter().position(|a| a == "--").unwrap();
        cmd.spec.args[..end].to_vec()
    };
    let cmd = wrap(opts("true", &root));
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    let args = args_of(cmd);
    let shown = |p: &Path| p.display().to_string();
    let at = |args: &[String], how: &str, p: &Path| {
        args.windows(3)
            .position(|w| w == [how, shown(p).as_str(), shown(p).as_str()])
    };
    for (parent, entry, control) in [
        (
            git.join("worktrees"),
            admin.clone(),
            admin.join("commondir"),
        ),
        (root.join(".wkts"), worktree.clone(), worktree.join(".git")),
    ] {
        let held = at(&args, "--ro-bind", &parent).unwrap_or_else(|| panic!("{args:?}"));
        let reopened = at(&args, "--bind", &entry).unwrap_or_else(|| panic!("{args:?}"));
        let shut = at(&args, "--ro-bind-try", &control).unwrap_or_else(|| panic!("{args:?}"));
        assert!(held < reopened && reopened < shut, "{args:?}");
    }

    let writable = vec![git.clone(), root.join(".wkts")];
    let engine = args_of(wrap(SandboxOptions {
        command: "true",
        cwd: &root,
        additional_writable: &writable,
        disable: false,
        policy: SandboxPolicy {
            only_additional_writable: true,
            ..Default::default()
        },
    }));
    assert!(
        at(&engine, "--ro-bind", &git.join("worktrees")).is_none(),
        "{engine:?}"
    );
    assert!(
        at(&engine, "--ro-bind-try", &admin.join("commondir")).is_some(),
        "{engine:?}"
    );

    let linked = worktree_git(&root, "wt");
    let inside = args_of(wrap(SandboxOptions {
        command: "true",
        cwd: &worktree,
        additional_writable: &linked.writable(),
        disable: false,
        policy: SandboxPolicy {
            worktree_git: Some(linked.clone()),
            ..Default::default()
        },
    }));
    assert!(at(&inside, "--bind", &git).is_none(), "{inside:?}");
    assert!(at(&inside, "--ro-bind-try", &git).is_some(), "{inside:?}");
    assert!(
        at(&inside, "--ro-bind", &worktree.join(".git")).is_some(),
        "{inside:?}"
    );
    assert!(
        at(&inside, "--ro-bind-try", &git.join("config")).is_some(),
        "{inside:?}"
    );
    assert!(
        at(&inside, "--ro-bind-try", &admin.join("gitdir")).is_some(),
        "{inside:?}"
    );
    let absent = shown(&admin.join("config.worktree"));
    assert!(
        inside
            .windows(3)
            .any(|w| w == ["--ro-bind-try", "/dev/null", absent.as_str()]),
        "{inside:?}"
    );
}

/// The same, run: a worktree made after the command's mounts were laid
/// shows up read-only in it, control files and `.git` link alike, while a
/// worktree that was there keeps the entry its commits write.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn linux_a_worktree_made_after_the_mounts_cannot_be_written() {
    let trees = base::paths::Layout::new(".codix")
        .unwrap()
        .with_worktrees_dir(".trees")
        .unwrap();
    for (layout, dir) in [
        (base::paths::Layout::default(), ".wkts"),
        (trees.clone(), ".trees"),
        (trees, ".wkts"),
    ] {
        a_worktree_made_after_the_mounts_cannot_be_written_in(&layout, dir, true);
    }
}

/// The same where no worktree, and so no worktrees directory, was there yet
/// when the mounts were laid: the absent directory is shut, so the command
/// can neither make it nor write the first worktree the engine adds to it.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn linux_a_first_worktree_made_after_the_mounts_cannot_be_written() {
    let trees = base::paths::Layout::new(".codix")
        .unwrap()
        .with_worktrees_dir(".trees")
        .unwrap();
    for (layout, dir) in [
        (base::paths::Layout::default(), ".wkts"),
        (trees.clone(), ".trees"),
        (trees, ".wkts"),
    ] {
        a_worktree_made_after_the_mounts_cannot_be_written_in(&layout, dir, false);
    }
}

/// The worktrees in `<project>/<dir>` under `layout`; `existing` says
/// whether one was there before the command started.
#[cfg(target_os = "linux")]
#[allow(clippy::unwrap_used)]
fn a_worktree_made_after_the_mounts_cannot_be_written_in(
    layout: &base::paths::Layout,
    dir: &str,
    existing: bool,
) {
    let own = layout.worktrees_dir_name();
    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let make_worktree = |name: &str| {
        let admin = root.join(".git/worktrees").join(name);
        let worktree = root.join(dir).join(name);
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}/.git\n", worktree.display()),
        )
        .unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
    };
    std::fs::create_dir(root.join(".git")).unwrap();
    if existing {
        make_worktree("wt");
    }
    let later = [
        ".git/worktrees/later/commondir".to_string(),
        ".git/worktrees/later/gitdir".to_string(),
        ".git/worktrees/later/config.worktree".to_string(),
        format!("{dir}/later/.git"),
        format!("{dir}/later/planted"),
    ];
    let script = format!(
        "for f in {}; do echo x > \"$f\" 2>/dev/null && echo \"wrote $f\"; done; \
         echo x > .git/worktrees/wt/index && echo index",
        later.join(" ")
    );
    let mut o = opts(&script, &root);
    o.policy.layout = layout.clone();
    let cmd = wrap(o);
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    let namespaces_work = std::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "true"])
        .status()
        .is_ok_and(|s| s.success());
    if !namespaces_work {
        return;
    }
    make_worktree("later");

    let out = std::process::Command::new(&cmd.spec.program)
        .args(&cmd.spec.args)
        .current_dir(&root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let expected = if existing { "index" } else { "" };
    assert_eq!(stdout.trim(), expected, "{own} {dir}: {stdout}{stderr}");
    assert_eq!(
        std::fs::read_to_string(root.join(".git/worktrees/later/commondir")).unwrap(),
        "../..\n"
    );
    assert!(!root.join(".git/worktrees/later/config.worktree").exists());
    assert!(!root.join(dir).join("later/planted").exists());
}

/// A project with no `.agents` yet: the project skills directory must not
/// be creatable, which on Linux means `.agents` itself is held read-only.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn linux_a_project_skill_cannot_be_planted_where_agents_is_absent() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let cmd = wrap(opts("true", &root));
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    let end = cmd.spec.args.iter().position(|a| a == "--").unwrap();
    let args = &cmd.spec.args[..end];
    let agents = root.join(".agents").display().to_string();
    let at = |pair: [&str; 2]| args.windows(2).position(|w| w == pair);
    let tmpfs = at(["--tmpfs", &agents]).unwrap_or_else(|| panic!("{args:?}"));
    let shut = at(["--remount-ro", &agents]).unwrap_or_else(|| panic!("{args:?}"));
    assert!(tmpfs < shut, "{args:?}");
}

/// `sandbox.deny_write` reaches bwrap as read-only binds of the entries it
/// names, at any depth below a writable root, laid after the root's own
/// writable bind.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn linux_deny_write_names_are_read_only_binds() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("secrets")).unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/.env.production"), "").unwrap();
    std::fs::write(root.join("secretsx"), "").unwrap();
    let mut o = opts("true", &root);
    o.policy.deny_write = vec!["secrets".into(), ".env".into()];
    let cmd = wrap(o);
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    let end = cmd.spec.args.iter().position(|a| a == "--").unwrap();
    let args = &cmd.spec.args[..end];
    let at = |triple: [&str; 3]| args.windows(3).position(|w| w == triple);
    let dir = root.display().to_string();
    let bound = at(["--bind", &dir, &dir]).unwrap();
    for denied in ["secrets", "sub/.env.production"] {
        let path = root.join(denied).display().to_string();
        assert!(
            at(["--ro-bind", &path, &path]).is_some_and(|i| i > bound),
            "{denied}: {args:?}"
        );
    }
    let other = root.join("secretsx").display().to_string();
    assert_eq!(at(["--ro-bind", &other, &other]), None, "{args:?}");
}

/// A link in a denied directory that leads outside it: its target is
/// hidden too.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn linux_a_denied_directorys_outside_link_targets_are_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (denied, managed) = (root.join("aws"), root.join("dotfiles"));
    std::fs::create_dir_all(&denied).unwrap();
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::write(managed.join("credentials"), "k").unwrap();
    std::os::unix::fs::symlink(managed.join("credentials"), denied.join("credentials")).unwrap();
    let project = root.join("project");
    let mut o = opts("true", &project);
    o.policy.deny_read = vec![denied];
    let cmd = wrap(o);
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    let target = managed.join("credentials").display().to_string();
    let args = &cmd.spec.args;
    assert!(
        args.windows(3)
            .any(|w| w == ["--ro-bind-try", "/dev/null", &target]),
        "{args:?}"
    );
}

/// A worktree sub-agent's command commits on its own branch — again, and
/// amending — and writes nothing else of the shared git directory: not
/// another branch's ref or reflog, `packed-refs`, `info/`, `objects/info/`,
/// the main worktree's `HEAD` and index, or another worktree's entry.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn linux_a_worktree_command_commits_on_its_branch_and_nothing_else() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    git(&["branch", "main-ish"]);
    for slug in ["wt", "other"] {
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            &format!("agix/worktree-{slug}"),
            &format!(".wkts/{slug}"),
        ]);
    }
    let common = root.join(".git");
    std::fs::create_dir_all(common.join("objects/info")).unwrap();
    std::fs::create_dir_all(common.join("info")).unwrap();
    let linked = base::paths::WorktreeGit {
        branch: "agix/worktree-wt".into(),
        ..worktree_git(&root, "wt")
    };
    let worktree = root.join(".wkts/wt");
    let runs = |command: &str| {
        let cmd = wrap(SandboxOptions {
            command,
            cwd: &worktree,
            additional_writable: &linked.writable(),
            disable: false,
            policy: SandboxPolicy {
                worktree_git: Some(linked.clone()),
                ..Default::default()
            },
        });
        if cmd.mode != SandboxMode::LinuxBwrap {
            return None;
        }
        let out = std::process::Command::new(&cmd.spec.program)
            .args(&cmd.spec.args)
            .current_dir(&worktree)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        Some((out.status.success(), out))
    };
    if runs("true").is_none_or(|(ran, _)| !ran) {
        return;
    }
    let commit = "git -c user.name=t -c user.email=t@t commit -q";
    for command in [
        format!("echo b > b && git add b && {commit} -m in-the-worktree"),
        format!("echo c > c && git add c && {commit} -m again"),
        format!("{commit} --amend -m amended"),
    ] {
        let (ran, out) = runs(&command).unwrap();
        assert!(ran, "`{command}` was refused: {out:?}");
    }
    assert_eq!(
        git(&["log", "-1", "--format=%s", "agix/worktree-wt"]).trim(),
        "amended"
    );

    let main_ish = git(&["rev-parse", "main-ish"]);
    let g = common.display().to_string();
    let mut writable = Vec::new();
    for rel in [
        "refs/heads/main-ish",
        "refs/heads/main-ish.lock",
        "refs/heads/planted",
        "refs/heads/agix/worktree-other",
        "refs/tags/planted",
        "logs/refs/heads/main-ish",
        "logs/HEAD",
        "packed-refs",
        "info/exclude",
        "info/attributes",
        "objects/info/alternates",
        "HEAD",
        "index",
        "config",
        "worktrees/other/HEAD",
        "worktrees/other/index",
        "worktrees/wt/commondir",
        "worktrees/wt/gitdir",
        "worktrees/wt/config.worktree",
    ] {
        runs(&format!(
            "mkdir -p \"$(dirname '{g}/{rel}')\" 2>/dev/null; echo XX >> '{g}/{rel}'"
        ));
        if std::fs::read_to_string(common.join(rel)).is_ok_and(|c| c.contains("XX")) {
            writable.push(rel);
        }
    }
    runs(&format!(
        "git branch -f main-ish HEAD; mv '{g}/refs/heads/agix/worktree-other' '{g}/x'"
    ));
    assert!(
        writable.is_empty(),
        "a worktree sub-agent rewrote shared git state: {writable:#?}"
    );
    assert_eq!(git(&["rev-parse", "main-ish"]), main_ish);
    assert!(common.join("refs/heads/agix/worktree-other").is_file());
}

/// A path denied as named (`deny_read_exact`, what a `Read` deny rule
/// names) is hidden as it is, under a read-only tmpfs: what a link in it
/// leads to is not hidden with it, and `allow_read` does not reopen it.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn linux_a_path_denied_as_named_is_shut_read_only_and_follows_no_link() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (denied, package) = (root.join("node_modules"), root.join("packages/pkg"));
    std::fs::create_dir_all(&denied).unwrap();
    std::fs::create_dir_all(&package).unwrap();
    std::os::unix::fs::symlink(&package, denied.join("pkg")).unwrap();
    let mut o = opts("true", &root);
    o.policy.deny_read_exact = vec![denied.clone()];
    o.policy.allow_read = vec![denied.clone()];
    let cmd = wrap(o);
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    let (d, p) = (denied.display().to_string(), package.display().to_string());
    let args = &cmd.spec.args;
    assert!(
        args.windows(4)
            .any(|w| w == ["--tmpfs", &d, "--remount-ro", &d]),
        "{args:?}"
    );
    assert!(
        !args.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == p),
        "{args:?}"
    );
}

/// Inside the project, a directory a command may not read it may not write
/// either: the write fails rather than landing in a tmpfs nobody sees, and
/// the directory on the host is as it was. A file denied below it as well
/// does not stop the command from starting.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn linux_a_write_into_a_read_denied_directory_fails_and_leaves_the_host_alone() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap();
    let (denied, credentials) = (project.join("secrets"), project.join("creds"));
    std::fs::create_dir_all(&denied).unwrap();
    std::fs::create_dir_all(&credentials).unwrap();
    std::fs::write(denied.join("key"), "KEY-7c2\n").unwrap();
    let mut o = opts(
        "cat secrets/key; echo new > secrets/new; echo w=$?; \
         echo new > creds/new; echo c=$?; echo end",
        &project,
    );
    o.policy.deny_read = vec![credentials.clone()];
    o.policy.deny_read_exact = vec![denied.clone(), denied.join("key")];
    let cmd = wrap(o);
    if cmd.mode != SandboxMode::LinuxBwrap {
        return;
    }
    let out = std::process::Command::new(&cmd.spec.program)
        .args(&cmd.spec.args)
        .current_dir(&project)
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&out.stdout);
    assert!(shown.contains("end"), "{out:?}");
    assert!(!shown.contains("KEY-7c2"), "{out:?}");
    assert!(!shown.contains("w=0") && !shown.contains("c=0"), "{out:?}");
    assert!(!denied.join("new").exists() && !credentials.join("new").exists());
    assert_eq!(
        std::fs::read_to_string(denied.join("key")).unwrap(),
        "KEY-7c2\n"
    );
}
