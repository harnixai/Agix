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
        std::fs::create_dir_all(in_own("scratchpad")).unwrap();
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
                &format!("cat {own}/settings.json && echo x > {own}/scratchpad/n")
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
