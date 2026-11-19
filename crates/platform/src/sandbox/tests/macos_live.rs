use super::*;

/// Unattended, a command cannot have another process do outside the
/// sandbox what the sandbox refuses it: launchd, LaunchServices, an
/// Apple event, `cfprefsd`.
#[cfg(target_os = "macos")]
#[test]
fn unattended_a_command_cannot_act_through_another_process() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let elsewhere = s.elsewhere.display().to_string();
    let label = format!("com.agix.sandbox-test.{}", std::process::id());

    for runs in [UnderSandboxExec::runs, UnderSandboxExec::runs_unattended] {
        let submitted = runs(
            &s,
            &format!("launchctl submit -l {label} -- /usr/bin/touch '{elsewhere}/launchd'"),
        );
        outside("launchctl", &["remove", &label]);
        assert!(!submitted, "launchd took a job from a sandboxed command");
    }

    let wrote = s.runs_unattended(&format!("defaults write {label} k v"));
    let stored = outside("defaults", &["read", &label, "k"]);
    outside("defaults", &["delete", &label]);
    assert!(!wrote && !stored, "cfprefsd wrote a preference for it");

    let applet = s.elsewhere.join("Applet.app");
    let applet_built = outside(
        "osacompile",
        &[
            "-o",
            &applet.display().to_string(),
            "-e",
            &format!("do shell script \"touch '{elsewhere}/opened'\""),
        ],
    );
    if applet_built {
        let opened = s.runs_unattended(&format!("open -g '{}'", applet.display()));
        if opened {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        assert!(!opened, "LaunchServices opened an app for it");
    }

    assert!(!s.runs_unattended("osascript -e 'tell application \"Finder\" to count windows'"));

    for marker in ["launchd", "opened"] {
        assert!(!s.elsewhere.join(marker).exists(), "{marker}");
    }
}

/// Those refusals leave ordinary work alone, reading preferences included.
#[cfg(target_os = "macos")]
#[test]
fn unattended_ordinary_work_still_runs() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    for command in [
        "git add a && git commit -qm first",
        "echo x > notes.md && mktemp && echo x > \"$TMPDIR/f\"",
        "defaults read -g > /dev/null",
    ] {
        assert!(s.runs_unattended(command), "`{command}` was refused");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn only_an_unattended_profile_refuses_the_ipc_routes() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    let attended = wrap(o.clone()).spec.args[1].clone();
    o.policy.deny_ipc_escapes = true;
    let unattended = wrap(o).spec.args[1].clone();
    for rule in [
        "(deny appleevent-send)",
        "(deny lsopen)",
        "(deny user-preference-write)",
    ] {
        assert!(!attended.contains(rule), "{rule}");
        assert!(unattended.contains(rule), "{rule}");
    }
}

/// What a sandboxed command could leave for git or the engine to act on
/// outside the sandbox is refused, against the real `sandbox-exec`.
#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_command_cannot_plant_what_runs_outside_the_sandbox() {
    for own in [".agix", ".codix"] {
        let Some(s) = UnderSandboxExec::in_layout(base::paths::Layout::new(own).unwrap()) else {
            return;
        };
        cannot_plant_under(&s, own);
    }
}

/// `own` is `s`'s project directory; `.agix` and `.claude` are guarded
/// beside it either way.
#[cfg(target_os = "macos")]
fn cannot_plant_under(s: &UnderSandboxExec, own: &str) {
    let state = s.state_root.display().to_string();
    let elsewhere = s.elsewhere.display().to_string();
    for command in [
        "git config x.y z".to_string(),
        "echo /tmp > .git/commondir".to_string(),
        "echo 'touch pwned' > .git/hooks/post-commit".to_string(),
        format!("mv {own} {own}.old"),
        format!("echo '{{}}' > {own}/mcp.local.json"),
        format!("echo '{{}}' > {own}/settings.json"),
        format!("mkdir {own}/mcp"),
        format!("mkdir {own}/hooks"),
        "mv .agix .agix.old".to_string(),
        "echo '{}' > .agix/settings.json".to_string(),
        "mkdir .agix/hooks".to_string(),
        "echo '{}' > .claude/settings.json".to_string(),
        "mkdir .claude/hooks".to_string(),
        "mv .claude .claude.old".to_string(),
        format!("echo '{{}}' > '{state}/mcp.json'"),
        format!("mkdir '{state}/hooks'"),
        format!("echo x > '{elsewhere}/x'"),
    ] {
        assert!(!s.runs(&command), "{own}: `{command}` was allowed");
    }
    s.runs(&format!("rm -rf {own}; ln -s '{elsewhere}' {own}"));
    let dir = s.project.join(own).symlink_metadata().unwrap();
    assert!(dir.is_dir(), "{own} was replaced");

    let config = std::fs::read_to_string(s.project.join(".git").join("config")).unwrap();
    assert!(!config.contains("[x]"), "{config}");
    assert!(!s.project.join(".git").join("commondir").exists());
    assert!(!s.project.join(own).join("settings.json").exists());
    assert!(!s.project.join(".agix").join("settings.json").exists());
    assert!(!s.state_root.join("mcp.json").exists());
}

/// The engine writes and reads `.agix/state/tool-results/` outside the
/// sandbox: export packs what it holds, fork links it, resume deletes from
/// it. The same holds under a writable root the policy names in `state/`.
#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_command_cannot_plant_a_saved_tool_result() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    std::fs::create_dir_all(s.project.join(".agix/state/tool-results/s")).unwrap();
    let elsewhere = s.elsewhere.display().to_string();
    for command in [
        format!("ln -s '{elsewhere}/secret' .agix/state/tool-results/s/k.txt"),
        "echo forged > .agix/state/tool-results/s/f.txt".to_string(),
        "mkdir .agix/state/tool-results/t".to_string(),
        "mkdir .agix/state/history".to_string(),
        "mkdir .agix/state/agix".to_string(),
        "mv .agix/state/tool-results .agix/state/tool-results.old".to_string(),
        "mv .agix/state .agix/state.old".to_string(),
    ] {
        assert!(!s.runs(&command), "`{command}` was allowed");
    }
    let named = [s.project.join(".agix/state")];
    let under_named =
        |command: &str| s.run_at(&s.project, &named, command, SandboxPolicy::default());
    for command in [
        "echo forged > .agix/state/tool-results/s/f.txt",
        "mkdir -p .agix/state/history/s",
        "mkdir -p .agix/state/agix/x",
    ] {
        assert!(!under_named(command), "`{command}` was allowed");
    }
    assert!(
        under_named("echo x > .agix/state/notes.md"),
        "the named root stays writable"
    );
}

/// The filesystem here compares names without case, so a protected file
/// spelled in another case is the same file. Seatbelt matches the path as
/// the filesystem names it, not as the command spelled it, which is why
/// the profile's rules can be written in one case.
#[cfg(target_os = "macos")]
#[test]
fn a_protected_path_spelled_in_another_case_is_still_protected() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    std::fs::write(s.state_root.join("settings.json"), "agix-test-secret").unwrap();
    std::fs::write(s.elsewhere.join(".netrc"), "agix-test-secret").unwrap();
    let state = s.state_root.display().to_string();
    let elsewhere = s.elsewhere.display().to_string();
    std::fs::write(s.state_root.join("notes.txt"), "fine").unwrap();
    if !s.state_root.join("SETTINGS.JSON").exists() {
        return;
    }
    assert!(s.runs(&format!(
        "cat A '{}/NOTES.TXT' && echo b > A && echo x > .GIT/description",
        s.state_root.display()
    )));
    let denied_netrc = SandboxPolicy {
        deny_read: vec![s.elsewhere.join(".netrc")],
        ..Default::default()
    };
    for policy in [SandboxPolicy::default(), denied_netrc.clone()] {
        for command in [
            format!("cat '{state}/SETTINGS.JSON'"),
            format!("cat '{state}/Settings.json'"),
        ] {
            assert!(
                !s.run_under(&command, policy.clone()),
                "`{command}` read it"
            );
        }
    }
    assert!(!s.run_under(&format!("cat '{elsewhere}/.NETRC'"), denied_netrc));
    for command in [
        "printf '[x]\\n\\ty = z\\n' >> .GIT/config",
        "printf '[x]\\n\\ty = z\\n' >> .git/CONFIG",
        "echo 'touch pwned' > .GIT/hooks/post-commit",
        "echo '{}' > .AGIX/settings.json",
        "echo '{}' > .agix/SETTINGS.JSON",
        "echo '{}' > .Claude/settings.json",
    ] {
        assert!(!s.runs(command), "`{command}` was allowed");
        assert!(!s.runs_unattended(command), "`{command}` was allowed");
    }
    let config = std::fs::read_to_string(s.project.join(".git").join("config")).unwrap();
    assert!(!config.contains("[x]"), "{config}");
    assert!(!s.project.join(".agix").join("settings.json").exists());
}

/// `.agents/skills/` is where the engine loads the project's skills
/// from, so a sandboxed command may not plant one there, nor move the
/// directories above it aside for others — whether they exist yet or
/// not. The rest of `.agents/` is the project's.
#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_command_cannot_plant_a_project_skill() {
    let s = UnderSandboxExec::new().expect("sandbox-exec runs here");
    assert!(
        !s.runs("echo x > .agix/settings.json"),
        "precondition: engine configuration is protected"
    );
    let planted = s.runs("mkdir -p .agents/skills/x && echo 'name: x' > .agents/skills/x/SKILL.md");
    assert!(
        !planted && !s.project.join(".agents/skills/x/SKILL.md").exists(),
        "a sandboxed command wrote .agents/skills/x/SKILL.md"
    );

    std::fs::create_dir_all(s.project.join(".agents/skills/kept")).unwrap();
    let elsewhere = s.elsewhere.display().to_string();
    for command in [
        "mkdir .agents/skills/x".to_string(),
        "echo x > .agents/skills/kept/SKILL.md".to_string(),
        "mv .agents/skills .agents/skills.old".to_string(),
        "mv .agents .agents.old && mkdir -p .agents/skills/x".to_string(),
        "rm -rf .agents/skills".to_string(),
        format!("rm -rf .agents && ln -s '{elsewhere}' .agents"),
    ] {
        assert!(!s.runs(&command), "`{command}` was allowed");
    }
    assert!(s.project.join(".agents/skills/kept").is_dir());
    assert!(!s.project.join(".agents/skills/x").exists());
    assert!(
        s.runs("echo x > .agents/notes.md"),
        "the rest of .agents was shut too"
    );
}

/// A repository embedded in the project (`sub/.git`, a directory, not a
/// `.git/modules/` entry) is a submodule once the index holds it as a
/// gitlink, and the user's `git status` in the project then runs git in
/// it, under its own configuration. A sandboxed command may not leave
/// one whose configuration names a program for that git to run.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "known escape: the backends cannot stop a nested repository; tools::nested_repos catches it after the command"]
fn a_sandboxed_command_cannot_plant_an_embedded_repositorys_configuration() {
    let s = UnderSandboxExec::new().expect("sandbox-exec runs here");
    assert!(
        !s.runs("git config core.fsmonitor 'touch x'"),
        "precondition: the project's own configuration is protected"
    );
    let pwned = s.elsewhere.join("pwned");
    let planted = s.runs(&format!(
        "mkdir sub && cd sub && git init -q && touch f && git add f && \
             git -c user.email=t@example.com -c user.name=T commit -q -m s && \
             git config core.fsmonitor \"touch '{}'; false\" && cd .. && git add sub",
        pwned.display()
    ));
    let status = std::process::Command::new("git")
        .arg("status")
        .current_dir(&s.project)
        .output()
        .unwrap();
    assert!(status.status.success(), "{status:?}");
    assert!(
        !pwned.exists(),
        "the user's `git status` ran a program the sandboxed command named \
             (planting it succeeded: {planted})"
    );
}

/// The same profile leaves ordinary work alone: git commits, files in the
/// project and in the working space of `.agix/`, the state root's
/// scratchpad and memory, and the session's TMPDIR. Not elsewhere in
/// `.agix/`, nor under `.claude/`, a foreign root.
#[cfg(target_os = "macos")]
#[test]
fn ordinary_work_still_runs_under_the_hardened_profile() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let state = s.state_root.display().to_string();
    for command in [
        "git add a && git commit -qm first".to_string(),
        "git init -q nested && cd nested && echo b > b && git add b \
             && git -c user.name=T -c user.email=t@example.com commit -qm nested"
            .to_string(),
        "echo x > notes.md && mkdir -p .agix/state/scratchpad \
             && echo x > .agix/state/scratchpad/notes.txt && mkdir -p .agix/state/memory \
             && ! echo x > .agix/notes.txt && ! echo x > .agix/state/notes.txt \
             && ! mkdir .agix/scratchpad && ! echo x > .claude/notes.txt"
            .to_string(),
        format!("echo x > '{state}/scratchpad/a' && echo x > '{state}/memory/a'"),
        "test -n \"$TMPDIR\" && echo x > \"$TMPDIR/f\" && mktemp".to_string(),
    ] {
        assert!(s.runs(&command), "`{command}` was refused");
    }
    assert!(s.project.join(".agix/state/scratchpad/notes.txt").is_file());
    assert!(s.tmp.join("f").is_file());
}

/// A sub-agent working in a worktree of the project commits there — again,
/// and amending — as the root session does in the project, and cannot
/// rewrite what git runs.
#[cfg(target_os = "macos")]
#[test]
fn a_command_in_a_worktree_commits_and_cannot_plant_what_git_runs() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&s.project)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    git(&["worktree", "add", "-q", "-b", "wt", ".wkts/wt"]);
    let common = s.project.join(".git");
    let runs = |command: &str| s.runs_in_worktree("wt", command);

    for command in [
        "echo b > b && git add b && git commit -qm in-the-worktree",
        "echo c > c && git add c && git commit -qm again",
        "git commit -q --amend -m amended",
    ] {
        assert!(runs(command), "`{command}` in the worktree was refused");
    }
    for command in [
        "git config x.y z".to_string(),
        format!(
            "echo '[x]' > '{}/worktrees/wt/config.worktree'",
            common.display()
        ),
        format!(
            "echo 'touch pwned' > '{}/hooks/post-commit'",
            common.display()
        ),
        format!("echo /tmp > '{}/worktrees/wt/commondir'", common.display()),
        format!(
            "echo /tmp/.git > '{}/worktrees/wt/gitdir'",
            common.display()
        ),
        "echo gitdir: /tmp > .git".to_string(),
    ] {
        assert!(!runs(&command), "`{command}` was allowed");
    }
    let config = std::fs::read_to_string(common.join("config")).unwrap();
    assert!(!config.contains("[x]"), "{config}");
    assert!(!common.join("hooks/post-commit").exists());
}

/// A session's temporary directory sits in the state root, which is
/// otherwise shut: its own is writable, a sibling session's is not.
#[cfg(target_os = "macos")]
#[test]
fn a_session_writes_its_own_temporary_directory_and_no_other() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let tmp = s.state_root.join(STATE_ROOT_SESSION_TMP);
    let (mine, theirs) = (tmp.join("mine"), tmp.join("theirs"));
    for d in [&mine, &theirs] {
        std::fs::create_dir_all(d).unwrap();
    }
    let policy = SandboxPolicy {
        state_root: Some(s.state_root.clone()),
        additional_writable: vec![mine.clone()],
        ..Default::default()
    };
    let runs = |command: String| {
        let confined = PlatformSandbox.confine(
            ProcessSpec::new("bash", &s.project).args(["-c".to_string(), command]),
            &policy,
        );
        std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .current_dir(&s.project)
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(runs(format!("echo x > '{}/f'", mine.display())));
    for target in [theirs.join("f"), tmp.join("f")] {
        assert!(
            !runs(format!("echo x > '{}'", target.display())),
            "{}",
            target.display()
        );
    }
}

/// A session without a project works in a directory of its own in the
/// state root's scratchpad. Working there does not open the rest of the
/// state root, nor its sockets.
#[cfg(target_os = "macos")]
#[test]
fn a_session_without_a_project_leaves_the_state_root_fenced() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let work = s.state_root.join("scratchpad/sessions/s1");
    std::fs::create_dir_all(&work).unwrap();
    let socket = listening(s.state_root.join("daemon.sock"));
    assert!(outside("nc", &["-U", &socket.display().to_string()]));
    let policy = SandboxPolicy {
        state_root: Some(s.state_root.clone()),
        additional_writable: vec![s.tmp.clone()],
        ..Default::default()
    };
    let runs = |command: String| {
        let confined = PlatformSandbox.confine(
            ProcessSpec::new("bash", &work).args(["-c".to_string(), command]),
            &policy,
        );
        std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .current_dir(&work)
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(runs("echo x > notes.md".into()));
    assert!(work.join("notes.md").is_file());
    for command in [
        format!("echo x > '{}/AGENTS.md'", s.state_root.display()),
        format!("mkdir -p '{}/skills/x'", s.state_root.display()),
        format!("nc -U '{}' < /dev/null", socket.display()),
    ] {
        assert!(!runs(command.clone()), "`{command}` was allowed");
    }
}

/// A policy that names its writable roots leaves nothing else writable:
/// not the project, the state root's working directories, or the
/// per-user temporary directory `mktemp` uses. And the backend confines
/// a program that is not a shell the same way.
#[cfg(target_os = "macos")]
#[test]
fn only_the_named_roots_are_writable_when_the_policy_says_so() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let policy = SandboxPolicy {
        state_root: Some(s.state_root.clone()),
        additional_writable: vec![s.tmp.clone()],
        only_additional_writable: true,
        ..Default::default()
    };
    let runs = |program: &str, args: &[&str]| {
        let confined = PlatformSandbox.confine(
            ProcessSpec::new(program, &s.project).args(args.iter().copied()),
            &policy,
        );
        assert_eq!(confined.enforcement, Enforcement::Full);
        std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .current_dir(&s.project)
            .output()
            .unwrap()
            .status
            .success()
    };
    let state = s.state_root.display().to_string();
    let tmp = s.tmp.display().to_string();

    assert!(runs("bash", &["-c", &format!("echo x > '{tmp}/f'")]));
    assert!(runs("git", &["status", "--short"]));
    for command in [
        "echo x > a".to_string(),
        "echo x > .agix/notes.txt".to_string(),
        format!("echo x > '{state}/scratchpad/a'"),
        "mktemp".to_string(),
    ] {
        assert!(!runs("bash", &["-c", &command]), "`{command}` was allowed");
    }
    assert!(!runs("git", &["add", "a"]));
    assert!(!s.project.join(".git").join("index").exists());
}

/// Unattended, a command cannot have the keychain read for it through
/// `securityd`, the way `security find-generic-password` does, nor type
/// into a terminal multiplexer's session outside the sandbox. Attended,
/// both stay: the user approves each such command, and `git push`
/// through the keychain's credential helper or `codesign` need them.
#[cfg(target_os = "macos")]
#[test]
#[allow(clippy::unwrap_used)]
fn unattended_a_command_cannot_reach_the_keychain_or_a_multiplexer() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let attended = SandboxPolicy {
        state_root: Some(dir.path().join("state")),
        ..Default::default()
    };
    let unattended = SandboxPolicy {
        deny_ipc_escapes: true,
        ..attended.clone()
    };
    let runs = |policy: &SandboxPolicy, command: &str| {
        run_confined(dir.path(), policy, &[], command)
            .status
            .success()
    };
    assert!(runs(&attended, "security list-keychains"));
    assert!(!runs(&unattended, "security list-keychains"));
    assert!(!runs(
        &unattended,
        "security find-generic-password -s agix-none"
    ));

    let uid = unsafe { libc::getuid() };
    let tmux = PathBuf::from(format!("/private/tmp/tmux-{uid}"));
    let made = !tmux.exists();
    if made {
        std::fs::create_dir(&tmux).unwrap();
    }
    let socket = listening(tmux.join(format!("agix-test-{}", std::process::id())));
    let nc = format!("nc -U '{}' < /dev/null", socket.display());
    let attended_reached = runs(&attended, &nc);
    let unattended_reached = runs(&unattended, &nc);
    let _ = std::fs::remove_file(&socket);
    if made {
        let _ = std::fs::remove_dir(&tmux);
    }
    assert!(attended_reached, "the multiplexer socket is not reachable");
    assert!(!unattended_reached, "an unattended command reached tmux");
}

/// `sandbox.deny_write` names are filename segments, as for the file tools:
/// a command may not write a path with a component that equals one or
/// begins with it and a dot, in the project or deeper, new or existing.
#[cfg(target_os = "macos")]
#[test]
fn deny_write_names_hold_for_a_sandboxed_command() {
    let s = UnderSandboxExec::new().expect("sandbox-exec runs here");
    std::fs::create_dir_all(s.project.join("secrets")).unwrap();
    let policy = || SandboxPolicy {
        deny_write: vec!["secrets".into(), ".env".into()],
        ..Default::default()
    };
    for command in [
        "echo x > secrets/a",
        "mkdir -p sub/secrets",
        "echo x > sub/.env.production",
        "echo x > .env",
    ] {
        assert!(!s.run_under(command, policy()), "`{command}` was allowed");
    }
    assert!(!s.project.join("secrets/a").exists());
    assert!(!s.project.join("sub/secrets").exists());
    for command in [
        "echo x > secretsx",
        "echo x > notes.md",
        "mkdir -p sub/environment",
    ] {
        assert!(s.run_under(command, policy()), "`{command}` was refused");
    }
}

/// `allow_read` re-allows the `deny_read` entry it names and nothing else
/// (as on Linux): a parent directory in it does not reopen what is denied
/// below it.
#[cfg(target_os = "macos")]
#[test]
fn allow_read_naming_a_parent_does_not_reopen_a_denied_directory() {
    let s = UnderSandboxExec::new().expect("sandbox-exec runs here");
    let secret = s.elsewhere.join("secret");
    std::fs::create_dir_all(&secret).unwrap();
    std::fs::write(secret.join("k"), "key\n").unwrap();
    let read = format!("cat '{}'", secret.join("k").display());
    let policy = |allow: &Path| SandboxPolicy {
        deny_read: vec![secret.clone()],
        allow_read: vec![allow.to_path_buf()],
        ..Default::default()
    };
    assert!(
        !s.run_under(&read, policy(&s.elsewhere)),
        "parent reopened it"
    );
    assert!(
        s.run_under(&read, policy(&secret)),
        "exact allow was ignored"
    );
}

/// A symbolic link inside a denied directory leads outside it, and the
/// kernel matches the path it resolves to: what it points at has to be
/// denied as well, or the link reads through.
#[cfg(target_os = "macos")]
#[test]
fn a_link_in_a_denied_directory_does_not_read_through() {
    let s = UnderSandboxExec::new().expect("sandbox-exec runs here");
    let denied = s.elsewhere.join("aws");
    let managed = s.elsewhere.join("dotfiles/aws");
    std::fs::create_dir_all(&denied).unwrap();
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::write(managed.join("credentials"), "key\n").unwrap();
    std::os::unix::fs::symlink(managed.join("credentials"), denied.join("credentials")).unwrap();
    let policy = SandboxPolicy {
        deny_read: vec![denied.clone()],
        ..Default::default()
    };
    for path in [denied.join("credentials"), managed.join("credentials")] {
        let read = format!("cat '{}'", path.display());
        assert!(!s.run_under(&read, policy.clone()), "{}", path.display());
    }
}

/// A path denied as named (`deny_read_exact`, what a `Read` deny rule
/// names) is denied as it is: what a link in it leads to stays readable,
/// and `allow_read` reopens it in no spelling, though it does reopen the
/// same path where `deny_read` names it.
#[cfg(target_os = "macos")]
#[test]
fn a_path_denied_as_named_follows_no_link_and_allow_read_does_not_reopen_it() {
    let s = UnderSandboxExec::new().expect("sandbox-exec runs here");
    let denied = s.elsewhere.join("node_modules");
    let package = s.elsewhere.join("packages/pkg");
    std::fs::create_dir_all(&denied).unwrap();
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("index.js"), "pkg\n").unwrap();
    std::fs::write(denied.join("own.js"), "own\n").unwrap();
    std::os::unix::fs::symlink(&package, denied.join("pkg")).unwrap();
    let policy = SandboxPolicy {
        deny_read: vec![denied.clone()],
        deny_read_exact: vec![denied.clone()],
        allow_read: vec![denied.clone(), base::path::resolve_physically(&denied)],
        ..Default::default()
    };
    let cat = |p: &Path| format!("cat '{}'", p.display());
    assert!(s.run_under(&cat(&package.join("index.js")), policy.clone()));
    assert!(!s.run_under(&cat(&denied.join("own.js")), policy));
}

/// A command confined before a sub-agent's worktree existed — a background
/// child an earlier call left running, a parallel sub-agent's — cannot
/// rewrite that worktree's links, and a rewrite made anyway does not widen
/// what the sub-agent's commands may write: their git directory is the one
/// the engine found when it made the worktree, never the one the links name.
#[cfg(target_os = "macos")]
#[test]
fn a_command_older_than_a_worktree_cannot_redirect_its_links() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&s.project)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    let victim = base::path::resolve_physically(&s.elsewhere);
    let fake = s.tmp.join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    std::fs::write(fake.join("commondir"), format!("{}\n", victim.display())).unwrap();
    let common = s.project.join(".git");
    for (slug, link, planted) in [
        ("wt1", "commondir", victim.display().to_string()),
        ("wt2", "gitdir", "/tmp/.git".to_string()),
        ("wt3", "link", format!("gitdir: {}", fake.display())),
    ] {
        let target = match link {
            "link" => s.project.join(format!(".wkts/{slug}/.git")),
            name => common.join(format!("worktrees/{slug}/{name}")),
        };
        let plant = format!("echo '{planted}' > '{}'", target.display());
        let additional = vec![s.tmp.clone()];
        let early = wrap(SandboxOptions {
            command: &plant,
            cwd: &s.project,
            additional_writable: &additional,
            disable: false,
            policy: SandboxPolicy {
                state_root: Some(s.state_root.clone()),
                ..Default::default()
            },
        });
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            slug,
            &format!(".wkts/{slug}"),
        ]);
        let rewrote = std::process::Command::new(&early.spec.program)
            .args(&early.spec.args)
            .current_dir(&s.project)
            .env("TMPDIR", &s.tmp)
            .status()
            .unwrap()
            .success();
        assert!(!rewrote, "{slug}: {} was rewritten", target.display());

        std::fs::write(&target, format!("{planted}\n")).unwrap();
        let escaped =
            s.runs_in_worktree(slug, &format!("echo pwned > '{}/{slug}'", victim.display()));
        assert!(!escaped, "{slug}: wrote {}", victim.join(slug).display());
    }
}

/// On a case-insensitive volume, a worktree sub-agent's command does not
/// reach the guarded files of the git directory by spelling their names in
/// another case, directly or by renaming over them.
#[cfg(target_os = "macos")]
#[test]
fn case_variants_of_guarded_git_files_are_refused_in_a_worktree() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&s.project)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    git(&["worktree", "add", "-q", "-b", "wt", ".wkts/wt"]);
    let common = s.project.join(".git");
    let worktree = s.project.join(".wkts/wt");
    if !common.join("CONFIG").exists() {
        return;
    }
    let snapshot = |p: &std::path::Path| std::fs::read(p).ok();
    let guarded = [
        common.join("config"),
        common.join("worktrees/wt/commondir"),
        common.join("worktrees/wt/gitdir"),
        common.join("objects/info/alternates"),
        worktree.join(".git"),
    ];
    let before: Vec<_> = guarded.iter().map(|p| snapshot(p)).collect();
    let g = common.display().to_string();
    let w = worktree.display().to_string();
    for cmd in [
        format!("echo '[x]' >> '{g}/CONFIG'"),
        format!("echo x > '{g}/objects/x.tmp' && mv -f '{g}/objects/x.tmp' '{g}/Config'"),
        format!("echo x > '{g}/worktrees/wt/COMMONDIR'"),
        format!(
            "echo x > '{g}/worktrees/wt/x2.tmp' && mv -f '{g}/worktrees/wt/x2.tmp' \
             '{g}/worktrees/wt/GitDir'"
        ),
        format!("echo /x > '{g}/objects/INFO/alternates'"),
        format!("echo x > '{w}/.GIT'"),
        format!("mkdir -p '{g}/HOOKS' && echo x > '{g}/HOOKS/post-commit'"),
    ] {
        s.runs_in_worktree("wt", &cmd);
    }
    let reached: Vec<String> = guarded
        .iter()
        .zip(&before)
        .filter(|(p, b)| snapshot(p) != **b)
        .map(|(p, _)| p.display().to_string())
        .chain(
            common
                .join("hooks/post-commit")
                .exists()
                .then(|| "hooks/post-commit".to_string()),
        )
        .collect();
    assert!(
        s.runs_in_worktree("wt", &format!("echo ok > '{g}/objects/control'")),
        "control: the object store is not writable at all"
    );
    assert!(reached.is_empty(), "rewritten: {reached:#?}");
}

/// A worktree sub-agent's command writes no more of the shared git
/// directory than a commit on its own branch needs: not another branch's
/// ref or reflog, `packed-refs`, `info/`, `objects/info/`, the main
/// worktree's `HEAD` and index, or another worktree's entry.
#[cfg(target_os = "macos")]
#[test]
fn a_worktree_command_cannot_rewrite_shared_refs_or_object_plumbing() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&s.project)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    git(&["branch", "main-ish"]);
    git(&["worktree", "add", "-q", "-b", "wt", ".wkts/wt"]);
    git(&["worktree", "add", "-q", "-b", "other", ".wkts/other"]);
    let common = s.project.join(".git");
    std::fs::create_dir_all(common.join("objects/info")).unwrap();
    std::fs::create_dir_all(common.join("info")).unwrap();
    let main_ish = git(&["rev-parse", "main-ish"]);
    let g = common.display().to_string();
    let shared = [
        "refs/heads/main-ish",
        "refs/heads/main-ish.lock",
        "refs/tags/planted",
        "logs/refs/heads/main-ish",
        "logs/HEAD",
        "packed-refs",
        "info/exclude",
        "info/attributes",
        "objects/info/alternates",
        "HEAD",
        "index",
        "worktrees/other/HEAD",
        "worktrees/other/index",
    ];
    let mut writable = Vec::new();
    for rel in shared {
        let cmd = format!("mkdir -p \"$(dirname '{g}/{rel}')\" ; echo XX >> '{g}/{rel}'");
        s.runs_in_worktree("wt", &cmd);
        if std::fs::read_to_string(common.join(rel)).is_ok_and(|c| c.contains("XX")) {
            writable.push(rel);
        }
    }
    assert!(
        writable.is_empty(),
        "a worktree sub-agent rewrote shared git state: {writable:#?}"
    );
    assert!(
        s.runs_in_worktree(
            "wt",
            "echo b > b && git add b && git commit -qm mine && git branch -f main-ish HEAD; \
             git rev-parse HEAD"
        ),
        "a commit on its own branch was refused"
    );
    assert_eq!(git(&["rev-parse", "main-ish"]), main_ish);
}

/// The filesystem ignores case, and so does the profile: another spelling of
/// a guarded name is the guarded file, refused as it is, whether it exists
/// or not. Skipped on a case-sensitive volume, where it is another file.
#[cfg(target_os = "macos")]
#[test]
fn another_spelling_of_a_guarded_name_is_refused_as_well() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    std::fs::write(s.project.join("probe"), "").unwrap();
    if !s.project.join("PROBE").exists() {
        return;
    }
    for command in [
        "echo x > .AGIX/Settings.json",
        "echo x > .agix/SETTINGS.LOCAL.JSON",
        "mkdir -p .Agix/HOOKS && echo x > .Agix/HOOKS/a.sh",
        "echo x > .CLAUDE/notes.md",
        "echo x > .GIT/Config",
    ] {
        assert!(!s.runs(command), "`{command}` was let through");
    }
}
