use super::*;

/// A command reads none of the credential stores in the user's home, nor
/// what in the state root holds keys — the settings files, whose
/// `api_key` and MCP headers are plain text, and the MCP OAuth tokens, of
/// the root and of every scene. Everything else in the state root, which
/// skills, memory and the session's own files live in, stays readable.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn a_sandboxed_command_reads_no_credentials_and_the_rest_of_the_state_root() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let project = dir.path().join("project");
    // In the project, which the command sees on every backend: bwrap
    // gives a command a `/tmp` of its own, and the temporary directory
    // may well be there.
    let home = project.join("home");
    std::fs::create_dir_all(project.join(".agix")).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let secret = "agix-test-secret";
    let secrets = [
        home.join(".git-credentials"),
        home.join(".config/git/credentials"),
        home.join(".config/gcloud/application_default_credentials.json"),
        home.join(".cargo/credentials"),
        home.join(".cargo/credentials.toml"),
        home.join(".vault-token"),
        home.join(".docker/config.json"),
        home.join(".kube/config"),
        home.join(".npmrc"),
        home.join(".pypirc"),
        home.join(".netrc"),
        state.join("settings.json"),
        state.join("settings.local.json"),
        state.join(".settings.local.json.1234.tmp"),
        state.join("scenes/code/settings.json"),
        state.join("scenes/code/settings.local.json"),
        state.join("scenes/code/mcp/oauth/server.json"),
        state.join("scenes/triage/settings.local.json"),
        state.join("mcp/oauth/server.json"),
        state.join("daemon/auth/desktop.token"),
    ];
    for path in &secrets {
        write_file(path, secret);
    }
    let readable = [
        home.join(".gitconfig"),
        home.join(".cargo/config.toml"),
        state.join("skills/s/SKILL.md"),
        state.join("scenes/code/skills/t/run.sh"),
        state.join("scenes/code/scene.json"),
        state.join("scenes/code/scheduled_tasks.json"),
        state.join("memory/MEMORY.md"),
        state.join("scratchpad/notes.md"),
        state.join("sessions/s.jsonl"),
        state.join("daemon/instances.d/desktop.json"),
    ];
    for path in &readable {
        write_file(path, "fine");
    }
    let policy = SandboxPolicy {
        deny_read: default_deny_read_in(&home),
        state_root: Some(state.clone()),
        ..Default::default()
    };
    let cat = |command: &str| {
        let out = run_confined(&project, &policy, &[("HOME", &home)], command);
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };

    for command in [
        "cat ~/.git-credentials".to_string(),
        format!("cat '{}/settings.local.json'", state.display()),
    ] {
        let (ok, shown) = cat(&command);
        assert!(!ok && !shown.contains(secret), "`{command}` read {shown:?}");
    }
    for path in &secrets {
        let (ok, shown) = cat(&format!("cat '{}'", path.display()));
        assert!(
            !ok && !shown.contains(secret),
            "{} was read: {shown:?}",
            path.display()
        );
    }
    for path in &readable {
        let (ok, shown) = cat(&format!("cat '{}'", path.display()));
        assert!(ok && shown == "fine", "{} was not read", path.display());
    }
    let (ok, listed) = cat(&format!("ls '{}'", state.join("scenes/code").display()));
    assert!(ok && listed.contains("skills"), "{listed}");
}

/// 场景目录是指向别处的符号链接时（把场景放在 dotfiles 仓库里再链进来），
/// 它的 `settings.json` 仍是这个场景的凭据，沙箱里的命令照样读不到。macOS
/// 的读拒绝匹配的是解析后的路径，链接目标不在 `<状态目录>/scenes/` 下。
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn a_scene_linked_in_from_elsewhere_keeps_its_settings_unreadable() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let project = dir.path().join("project");
    let elsewhere = dir.path().join("dotfiles/scene");
    std::fs::create_dir_all(state.join("scenes")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let secret = "agix-audit3-scene-secret";
    write_file(&elsewhere.join("settings.json"), secret);
    write_file(&state.join("scenes/code/settings.json"), secret);
    std::os::unix::fs::symlink(&elsewhere, state.join("scenes/linked")).unwrap();
    let policy = SandboxPolicy {
        state_root: Some(state.clone()),
        ..Default::default()
    };
    for path in [
        state.join("scenes/code/settings.json"),
        state.join("scenes/linked/settings.json"),
    ] {
        let out = run_confined(&project, &policy, &[], &format!("cat '{}'", path.display()));
        let shown = String::from_utf8_lossy(&out.stdout);
        assert!(
            !out.status.success() && !shown.contains(secret),
            "{} was read: {shown:?}",
            path.display()
        );
    }
}

/// The project's own `.agix/settings.local.json` and `.agix/mcp.local.json`
/// are where the engine writes the project's keys; a command reads neither,
/// even with the whole project in `allow_read`. The committed
/// `settings.json` beside them stays readable.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn a_sandboxed_command_reads_none_of_the_projects_keys() {
    if !a_backend_runs() {
        return;
    }
    for (own, keys_in) in [
        (".agix", ".agix"),
        (".codix", ".codix"),
        (".codix", ".agix"),
    ] {
        reads_none_of_the_keys_in(&base::paths::Layout::new(own).unwrap(), keys_in);
    }
}

/// Under `layout`, the keys in the project's `keys_in`: its own directory,
/// or a foreign `.agix`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(clippy::unwrap_used)]
fn reads_none_of_the_keys_in(layout: &base::paths::Layout, keys_in: &str) {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let agix = project.join(keys_in);
    let secret = "agix-test-secret";
    let secrets = [
        agix.join("settings.local.json"),
        agix.join("mcp.local.json"),
        agix.join(".settings.local.json.1234.tmp"),
    ];
    for path in &secrets {
        write_file(path, secret);
    }
    write_file(&agix.join("settings.json"), "fine");
    let policy = SandboxPolicy {
        deny_read: vec![dir.path().join("nowhere")],
        allow_read: vec![project.clone()],
        state_root: Some(dir.path().join("state")),
        layout: layout.clone(),
        ..Default::default()
    };
    let cat = |path: &Path| {
        let out = run_confined(&project, &policy, &[], &format!("cat '{}'", path.display()));
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    for path in &secrets {
        let (ok, shown) = cat(path);
        assert!(
            !ok && !shown.contains(secret),
            "{}: {} was read: {shown:?}",
            layout.project_dir_name(),
            path.display()
        );
    }
    assert_eq!(cat(&agix.join("settings.json")), (true, "fine".to_string()));
}

/// What a process a confined command left running in `project` reads of
/// `paths` after each of them is written — the engine's way, a
/// temporary file renamed over it — once it is running.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(clippy::unwrap_used)]
fn read_after_the_fact(project: &Path, policy: &SandboxPolicy, paths: &[PathBuf]) -> String {
    let listed: Vec<String> = paths.iter().map(|p| format!("'{}'", p.display())).collect();
    let command = format!(
        "( until [ -e go ]; do sleep 0.02; done; \
              for f in {}; do cat \"$f\"; echo; done > out 2>/dev/null; \
              touch done ) & exit 0",
        listed.join(" ")
    );
    let confined = PlatformSandbox.confine(
        ProcessSpec::new("bash", project).args(["-c", &command].map(String::from)),
        policy,
    );
    assert_eq!(
        confined.enforcement,
        Enforcement::Full,
        "{:?}",
        confined.unmet
    );
    let mut child = std::process::Command::new(&confined.spec.program)
        .args(&confined.spec.args)
        .current_dir(project)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _ = child.wait();
    for path in paths {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let tmp = path.with_file_name(".agix-rename.tmp");
        std::fs::write(&tmp, "agix-after-the-fact-secret").unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }
    std::fs::write(project.join("go"), "").unwrap();
    let done = (0..500).any(|_| {
        if project.join("done").exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        false
    });
    assert!(done, "the background reader never finished");
    std::fs::read_to_string(project.join("out")).unwrap_or_default()
}

/// The layouts that keep the state root from being masked as a whole —
/// the project is the state root, or a scene directory in it — and the
/// project's own `.agix`: a credential the engine writes there after a
/// command started, or rename-writes over an old one, does not reach
/// what that command left running. On Linux the key files directly in
/// a project that is the state root or a scene directory are the known
/// limit (`mask`'s header) and not asserted; macOS denies them by
/// pattern.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn a_credential_written_later_stays_unreadable_where_the_project_holds_credentials() {
    if !a_backend_runs() {
        return;
    }
    let pattern_held = cfg!(target_os = "macos");
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();

    let state = base.join("state-is-project");
    write_file(&state.join("scenes/code/settings.local.json"), "{}");
    write_file(&state.join("scenes/code/scene.json"), "fine");
    write_file(&state.join("mcp/servers.json"), "fine");
    write_file(&state.join("settings.local.json"), "{}");
    let policy = SandboxPolicy {
        state_root: Some(state.clone()),
        ..Default::default()
    };
    let mut later = vec![
        state.join("scenes/code/settings.local.json"),
        state.join("scenes/new/settings.json"),
        state.join("mcp/oauth/server.json"),
        state.join(".agix/settings.local.json"),
    ];
    if pattern_held {
        later.push(state.join("settings.local.json"));
    }
    let saw = read_after_the_fact(&state, &policy, &later);
    assert!(
        !saw.contains("after-the-fact"),
        "project = state root: {saw:?}"
    );

    let state = base.join("state");
    let scene = state.join("scenes/code");
    write_file(&state.join("settings.local.json"), "{}");
    write_file(&state.join("scenes/triage/settings.json"), "{}");
    write_file(&scene.join("settings.local.json"), "{}");
    write_file(&scene.join("scene.json"), "{}");
    let policy = SandboxPolicy {
        state_root: Some(state.clone()),
        ..Default::default()
    };
    let mut later = vec![
        state.join("settings.local.json"),
        state.join("settings.json"),
        state.join("scenes/triage/settings.json"),
        state.join("scenes/new/settings.json"),
        scene.join(".agix/mcp.local.json"),
    ];
    if pattern_held {
        later.push(scene.join("settings.local.json"));
    }
    let saw = read_after_the_fact(&scene, &policy, &later);
    assert!(!saw.contains("after-the-fact"), "project = scene: {saw:?}");
    assert!(
        run_confined(
            &scene,
            &policy,
            &[],
            "echo x > notes.md && ! echo x > scene.json"
        )
        .status
        .success(),
        "a scene opened as a project keeps its scene files read-only, the rest writable"
    );

    for own in [".agix", ".codix"] {
        let project = base.join("project").join(own);
        let layout = base::paths::Layout::new(own).unwrap();
        let in_own = |p: &str| layout.project_dir(&project).join(p);
        write_file(&in_own("settings.json"), "fine");
        write_file(&in_own("settings.local.json"), "{}");
        std::fs::create_dir_all(in_own("state/scratchpad")).unwrap();
        write_file(&project.join(".agix/settings.local.json"), "{}");
        let policy = SandboxPolicy {
            state_root: Some(base.join("elsewhere")),
            layout: layout.clone(),
            ..Default::default()
        };
        let later = [
            in_own("settings.local.json"),
            in_own("mcp.local.json"),
            project.join(".agix/settings.local.json"),
        ];
        let saw = read_after_the_fact(&project, &policy, &later);
        assert!(
            !saw.contains("after-the-fact"),
            "the project's {own}: {saw:?}"
        );
        assert!(
            run_confined(
                &project,
                &policy,
                &[],
                &format!("cat {own}/settings.json && echo x > {own}/state/scratchpad/n")
            )
            .status
            .success(),
            "the project's {own} keeps its other entries, the working ones writable"
        );
        if cfg!(target_os = "linux") {
            assert!(
                !run_confined(&project, &policy, &[], &format!("echo x > {own}/new"))
                    .status
                    .success(),
                "a new entry directly in {own} was created"
            );
        }
    }
}

/// Another instance's state root on this machine is guarded as the
/// session's own is: a command reads none of its keys and tokens, connects
/// to no socket in it, and writes nothing in it — here even with it inside
/// the writable project. The rest of it stays readable.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn another_instances_state_root_is_guarded_as_the_own_one() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let project = base.join("p");
    let other = project.join("h/.agix");
    let secret = "agix-other-instance-secret";
    let secrets = [
        other.join("settings.json"),
        other.join("settings.local.json"),
        other.join("scenes/code/settings.json"),
        other.join("mcp/oauth/server.json"),
        other.join("daemon/auth/desktop.token"),
    ];
    for path in &secrets {
        write_file(path, secret);
    }
    write_file(&other.join("skills/s/SKILL.md"), "fine");
    std::fs::create_dir_all(other.join("memory")).unwrap();
    let socket = other.join("a.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            drop(conn);
        }
    });
    let policy = SandboxPolicy {
        state_root: Some(base.join("state")),
        foreign_state_roots: vec![other.clone()],
        ..Default::default()
    };
    let run = |command: &str| {
        let out = run_confined(&project, &policy, &[], command);
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    for path in &secrets {
        let (ok, shown) = run(&format!("cat '{}'", path.display()));
        assert!(
            !ok && !shown.contains(secret),
            "{} was read: {shown:?}",
            path.display()
        );
    }
    assert_eq!(
        run(&format!(
            "cat '{}'",
            other.join("skills/s/SKILL.md").display()
        )),
        (true, "fine".to_string())
    );
    for write in ["memory/m.md", "new.json", "skills/s/SKILL.md"] {
        let path = other.join(write);
        let (ok, _) = run(&format!("echo x > '{}'", path.display()));
        assert!(!ok, "{} was written", path.display());
    }
    let (_, connected) = run(&format!(
        "python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); print(\"in\")' '{}'",
        socket.display()
    ));
    assert!(!connected.contains("in"), "the socket was reached");
    assert!(run("echo x > p.txt").0, "the project stays writable");
}

/// A foreign configuration root is shut to a command's writes as a whole,
/// as the gate takes every entry of one for configuration — its working
/// entries and new entries directly in it too — but for a writable root the
/// policy names inside it. In the instance's own root the working entries
/// of its `state/` stay writable — `state/scratchpad/` and `state/memory/`,
/// all the gate leaves out of configuration — and nothing else in it: not
/// `commands/`, not `state/teams/`, `state/telemetry/` or
/// `state/tool-results/`, which the engine writes itself, not the
/// embedder's `state/<product id>/`, not a `scratchpad/` directly in the
/// root, nor a `worktrees/`, which the engine's worktrees are not in.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn a_foreign_configuration_root_is_shut_to_writes_as_a_whole() {
    if !a_backend_runs() {
        return;
    }
    for own in [".agix", ".codix"] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap().join("project");
        let layout = base::paths::Layout::new(own).unwrap();
        for d in [
            ".agix/memory",
            ".agix/scratchpad",
            ".claude/commands",
            ".claude/memory",
        ] {
            std::fs::create_dir_all(project.join(d)).unwrap();
        }
        let host = format!("state/{}", layout.product_id());
        for entry in [
            "scratchpad",
            "memory",
            "state/scratchpad",
            "state/teams",
            "state/telemetry",
            "state/tool-results",
            host.as_str(),
            "worktrees/w",
        ] {
            std::fs::create_dir_all(project.join(own).join(entry)).unwrap();
        }
        let named = project.join(".claude/named");
        std::fs::create_dir_all(&named).unwrap();
        let policy = SandboxPolicy {
            state_root: Some(dir.path().join("state")),
            layout: layout.clone(),
            additional_writable: vec![named.clone()],
            ..Default::default()
        };
        let writes = |path: &str| {
            run_confined(
                &project,
                &policy,
                &[],
                &format!("echo x > '{}'", project.join(path).display()),
            )
            .status
            .success()
        };
        let mut shut = vec![
            ".claude/commands/c.md",
            ".claude/memory/m.md",
            ".claude/new.md",
        ];
        if own != ".agix" {
            shut.extend([".agix/memory/m.md", ".agix/scratchpad/n", ".agix/new.md"]);
        }
        let own_commands = format!("{own}/commands");
        let created = run_confined(
            &project,
            &policy,
            &[],
            &format!("mkdir -p {own_commands} && echo x > {own_commands}/c.md"),
        );
        assert!(
            !created.status.success() && !project.join(&own_commands).join("c.md").exists(),
            "{own}: {own_commands}/c.md was written"
        );
        for path in shut {
            assert!(!writes(path), "{own}: {path} was written");
        }
        let in_host = format!("{host}/x");
        for entry in [
            "state/teams/t.json",
            "state/telemetry/x.jsonl",
            "state/tool-results/r.txt",
            "state/notes.md",
            in_host.as_str(),
            "scratchpad/n",
            "memory/m.md",
            "notes.md",
            "worktrees/w/x",
        ] {
            let path = format!("{own}/{entry}");
            assert!(!writes(&path), "{own}: {path} was written");
        }
        let own_scratch = format!("{own}/state/scratchpad/n");
        for path in [own_scratch.as_str(), ".claude/named/n"] {
            assert!(writes(path), "{own}: {path} was not writable");
        }
    }
}

/// The engine's worktrees directory is ordinary project space, yet a
/// command can neither move it nor a worktree in it aside — and rewrite a
/// worktree's `.git` link where no rule names it — nor add a directory of
/// its own there or rewrite a link in place. Inside a worktree it writes as
/// anywhere in the project.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn a_command_cannot_move_the_worktrees_aside_or_rewrite_their_links() {
    if !a_backend_runs() {
        return;
    }
    for layout in [
        base::paths::Layout::default(),
        base::paths::Layout::new(".codix")
            .unwrap()
            .with_worktrees_dir(".trees")
            .unwrap(),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap().join("project");
        let wt = layout.worktrees_dir(&project).join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /nowhere\n").unwrap();
        let policy = SandboxPolicy {
            state_root: Some(dir.path().join("state")),
            layout: layout.clone(),
            ..Default::default()
        };
        let runs = |command: &str| {
            run_confined(&project, &policy, &[], command)
                .status
                .success()
        };
        let wk = layout.worktrees_dir_name();
        for command in [
            format!("mv {wk} moved"),
            format!("mv {wk}/wt {wk}/other"),
            format!("mv {wk}/wt moved"),
            format!("mkdir {wk}/planted"),
            format!("echo 'gitdir: /tmp' > {wk}/wt/.git"),
            format!("rm -rf {wk}/wt"),
        ] {
            assert!(!runs(&command), "{wk}: `{command}` was allowed");
        }
        assert_eq!(
            std::fs::read_to_string(wt.join(".git")).unwrap(),
            "gitdir: /nowhere\n"
        );
        // Across Linux's bind mounts `mv` falls back to copying, so a copy
        // may appear; what matters is that nothing left its place.
        for gone in [format!("{wk}/other"), format!("{wk}/planted")] {
            assert!(!project.join(&gone).exists(), "{gone}");
        }
        assert!(
            runs(&format!(
                "echo x > {wk}/wt/f && mkdir -p {wk}/wt/src && echo y > {wk}/wt/src/a"
            )),
            "{wk}: a worktree's files were not writable"
        );
    }
}

/// Where a configuration root or the project's skills directory is not
/// there, a command cannot make it and put configuration in it — the
/// instance's own directory, a foreign root, `.agents/skills` — and the
/// project is left as it was: no empty directory stays behind where a
/// foreign root or `.agents` was held.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn no_command_creates_a_configuration_root_that_is_not_there() {
    if !a_backend_runs() {
        return;
    }
    for own in [".agix", ".codix"] {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let policy = SandboxPolicy {
            state_root: Some(dir.path().join("state")),
            layout: base::paths::Layout::new(own).unwrap(),
            ..Default::default()
        };
        let mut planted = vec![
            format!("{own}/settings.json"),
            format!("{own}/hooks/a.sh"),
            ".claude/settings.json".to_string(),
            ".claude/commands/c.md".to_string(),
            ".agents/skills/s/SKILL.md".to_string(),
        ];
        if own != ".agix" {
            planted.push(".agix/settings.json".to_string());
        }
        for path in &planted {
            let parent = Path::new(path).parent().unwrap().display().to_string();
            let out = run_confined(
                &project,
                &policy,
                &[],
                &format!("mkdir -p {parent} && echo x > {path}"),
            );
            assert!(
                !out.status.success() && !project.join(path).exists(),
                "{own}: {path} was created"
            );
        }
        for left in [".claude", ".agents", ".agix"]
            .into_iter()
            .filter(|d| *d != own)
        {
            assert!(
                project.join(left).symlink_metadata().is_err(),
                "{own}: {left} was left behind"
            );
        }
        assert!(
            run_confined(&project, &policy, &[], "echo x > p.txt")
                .status
                .success(),
            "{own}: the project stays writable"
        );
    }
}

/// A command that finishes while another runs in the same project leaves
/// the directory the other one's sandbox holds where it is: removing it
/// would take the other's mount away and let it make the directory.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn a_command_that_ends_first_leaves_the_others_held_directory_alone() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let policy = SandboxPolicy {
        state_root: Some(dir.path().join("state")),
        ..Default::default()
    };
    let confined = PlatformSandbox.confine(
        ProcessSpec::new("bash", &project).args(
            [
                "-c",
                "until [ -e go ]; do sleep 0.02; done; \
                 mkdir -p .claude; echo x > .claude/settings.json; touch done",
            ]
            .map(String::from),
        ),
        &policy,
    );
    let mut first = std::process::Command::new(&confined.spec.program)
        .args(&confined.spec.args)
        .current_dir(&project)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    if cfg!(target_os = "linux") {
        let held = (0..500).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            project.join(".claude").exists()
        });
        assert!(held, "the first command's sandbox never held .claude");
    }
    assert!(run_confined(&project, &policy, &[], "true")
        .status
        .success());
    std::fs::write(project.join("go"), "").unwrap();
    let _ = first.wait();
    assert!(project.join("done").exists());
    assert!(
        !project.join(".claude/settings.json").exists(),
        "the first command made .claude once the second removed it"
    );
    assert!(project.join(".claude").symlink_metadata().is_err());
}

/// A command stopped the way the engine stops one — `SIGTERM` to its group,
/// then `SIGKILL` — keeps the directory its sandbox holds in place until
/// that sandbox is gone, though the next command in the project ends while
/// what the stopped one left in a session of its own still runs.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::unwrap_used)]
async fn linux_a_stopped_command_leaves_nothing_to_make_a_directory_after_the_cleanup() {
    use base::interface::exec::Process;
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let policy = SandboxPolicy {
        state_root: Some(dir.path().join("state")),
        ..Default::default()
    };
    let confined = PlatformSandbox.confine(
        ProcessSpec::new("bash", &project).args(
            [
                "-c",
                "setsid sh -c 'until [ -e go ]; do sleep 0.02; done; \
                 mkdir -p .claude; echo x > .claude/settings.json; touch done' & sleep 30",
            ]
            .map(String::from),
        ),
        &policy,
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut stopped = crate::LocalProcess::default()
        .spawn(confined.spec, cancel.clone())
        .await
        .unwrap();
    let held = (0..500).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(10));
        project.join(".claude").exists()
    });
    assert!(held, "the command's sandbox never held .claude");
    cancel.cancel();
    let _ = stopped.wait().await;
    let cleaned = tokio::task::spawn_blocking({
        let (project, policy) = (project.clone(), policy.clone());
        move || run_confined(&project, &policy, &[], "true")
    })
    .await
    .unwrap();
    assert!(cleaned.status.success());
    std::fs::write(project.join("go"), "").unwrap();
    let _tried = (0..250).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(20));
        project.join("done").exists()
    });
    assert!(
        !project.join(".claude/settings.json").exists(),
        "what the stopped command left running made .claude after the cleanup"
    );
}

/// A command that ends leaving something in the background keeps the
/// directory its sandbox holds until that is gone too: removing it when the
/// command ends would let what still runs there make it.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::unwrap_used)]
fn linux_what_a_command_leaves_running_cannot_make_a_directory_once_it_ends() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let policy = SandboxPolicy {
        state_root: Some(dir.path().join("state")),
        ..Default::default()
    };
    let ended = run_confined(
        &project,
        &policy,
        &[],
        "(until [ -e go ]; do sleep 0.02; done; \
         mkdir -p .claude; echo x > .claude/settings.json; touch done) >/dev/null 2>&1 &",
    );
    assert!(ended.status.success());
    assert!(
        project.join(".claude").exists(),
        "the directory went while held"
    );
    std::fs::write(project.join("go"), "").unwrap();
    let tried = (0..500).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(20));
        project.join("done").exists()
    });
    assert!(tried, "what the command left running never got to try");
    assert!(!project.join(".claude/settings.json").exists());
    let removed = (0..500).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(20));
        project.join(".claude").symlink_metadata().is_err()
    });
    assert!(removed, "the directory stayed once nothing held it");
}
