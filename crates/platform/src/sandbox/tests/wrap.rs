use super::*;

/// The command reaches the shell once, on whichever backend runs.
///
/// It reached it twice on Linux for a while: generalizing the backends to
/// take a process spec added a generic tail without removing the hardcoded
/// one, so the argv ended `-- bash -c CMD bash -c CMD`. That runs, and
/// looks right, and quietly sets `$0`/`$1`/`$2` to `bash`, `-c` and the
/// command itself — so anything reading a positional parameter is wrong.
/// Asserted as a count rather than a position because a second copy is
/// exactly what a position-based assertion cannot see.
#[test]
fn the_command_reaches_the_shell_exactly_once() {
    let c = wrap(opts("echo hi", Path::new("/tmp/work")));
    assert_eq!(
        c.spec
            .args
            .iter()
            .filter(|a| a.as_str() == "echo hi")
            .count(),
        1,
        "argv: {:?}",
        c.spec.args
    );
    assert_eq!(
        c.spec
            .args
            .windows(2)
            .filter(|w| w[0] == "bash" && w[1] == "-c")
            .count(),
        1,
        "argv: {:?}",
        c.spec.args
    );
}

#[test]
fn disable_yields_plain_bash() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.disable = true;
    let cmd = wrap(o);
    assert_eq!(cmd.spec.program, "bash");
    assert_eq!(cmd.spec.args[0], "-c");
    assert_eq!(cmd.spec.args[1], "ls");
    assert_eq!(cmd.mode, SandboxMode::Disabled);
}

/// With the project outside the state root, the state root and every scene
/// directory are masked with a tmpfs and their readable entries bound back,
/// leaving the key files under the tmpfs — which a credential written after
/// wrap cannot climb out of, where a `/dev/null` bind over the file could.
#[test]
fn sandbox_escape_handles_quotes_and_backslash() {
    assert_eq!(sandbox_escape("/tmp/normal"), "/tmp/normal");
    assert_eq!(sandbox_escape("/tmp/q\"quote"), "/tmp/q\\\"quote");
    assert_eq!(sandbox_escape("/tmp/back\\slash"), "/tmp/back\\\\slash");
}

#[test]
fn default_deny_read_includes_credential_paths() {
    // Only meaningful when HOME is set; in CI it usually is. Skip if not.
    let Some(_home) = base::paths::home_dir() else {
        return;
    };
    let list = default_deny_read();
    let joined: String = list
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("|");
    assert!(joined.contains(".ssh"));
    assert!(joined.contains(".aws"));
    assert!(joined.contains(".gnupg"));
    assert!(joined.contains(".kube"));
    assert!(joined.contains("gh"));
}

#[test]
fn sandbox_escape_handles_special_whitespace() {
    // sandbox_escape only escapes `\` and `"`. Newlines/tabs pass through
    // unmodified — the test just verifies no panic or corruption.
    let with_nl = sandbox_escape("/path/with\nnewline");
    assert!(with_nl.contains('\n'), "newline passes through unescaped");
    assert!(with_nl.contains("newline"));

    let with_tab = sandbox_escape("/path/with\t tab");
    assert!(with_tab.contains('\t'), "tab passes through unescaped");

    // Double-quote still gets escaped even when surrounded by whitespace
    assert_eq!(sandbox_escape("\" start"), "\\\" start");
}

#[test]
fn sandbox_escape_handles_unicode_and_long_paths() {
    // Unicode and deeply nested paths that could trigger buffer issues.
    let unicode = "/tmp/日本語/パス";
    let escaped = sandbox_escape(unicode);
    assert_eq!(escaped, unicode); // no escaping needed
    let long = "/".repeat(1000) + "a";
    let escaped = sandbox_escape(&long);
    assert!(escaped.len() > 500);
    assert_eq!(escaped, long);
}

#[test]
fn wrap_never_panics_on_edge_case_inputs() {
    // wrap() must always return a valid SandboxedCommand regardless
    // of unusual inputs — the contract is "never fails, never panics".
    let empty_opts = SandboxOptions {
        command: "",
        cwd: Path::new(""),
        additional_writable: &[],
        disable: false,
        policy: SandboxPolicy::default(),
    };
    let cmd = wrap(empty_opts);
    assert!(!cmd.spec.program.is_empty());
    assert!(!cmd.spec.args.is_empty());

    // Unicode command with no cwd
    let unicode_opts = SandboxOptions {
        command: "echo 🦀",
        cwd: Path::new("/"),
        additional_writable: &[],
        disable: false,
        policy: SandboxPolicy::default(),
    };
    let cmd2 = wrap(unicode_opts);
    assert!(cmd2.spec.args.iter().any(|a| a.contains("🦀")));

    // Explicit disable must always yield plain bash
    let disabled_opts = SandboxOptions {
        command: "ls",
        cwd: Path::new("/tmp"),
        additional_writable: &[],
        disable: true,
        policy: SandboxPolicy::default(),
    };
    let cmd3 = wrap(disabled_opts);
    assert_eq!(cmd3.spec.program, "bash");
    assert_eq!(cmd3.mode, SandboxMode::Disabled);
}

/// `npm run dev > log &` and the like: what a confined command leaves in
/// the background, its output sent elsewhere, neither holds the command's
/// output open nor goes when the command exits. It is in the command's
/// process group, where stopping the group reaches it.
///
/// The background loop gives up after a while, so that a sandbox holding
/// the output open fails this rather than hangs it. What it leaves
/// running holds a FIFO open, so that the group emptying out of reach —
/// bwrap's `--new-session` takes its PID 1 into a group of its own — is
/// not taken for the group having stopped.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used, unsafe_code)]
fn what_a_confined_command_leaves_running_outlives_it_in_its_process_group() {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    if !a_backend_runs() {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let fifo =
        std::ffi::CString::new(project.path().join("held").as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a NUL-terminated path that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let held = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(project.path().join("held"))
        .unwrap();
    let command = "echo out; echo err >&2; \
                       (i=0; while [ ! -e exited ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done; \
                        [ -e exited ] && exec 3>held && touch outlived && exec sleep 600) >/dev/null 2>&1 & \
                       exit 3";
    let confined = PlatformSandbox.confine(
        ProcessSpec::new("bash", project.path()).args(["-c", command].map(String::from)),
        &SandboxPolicy::default(),
    );
    assert_eq!(
        confined.enforcement,
        Enforcement::Full,
        "{:?}",
        confined.unmet
    );
    let child = std::process::Command::new(&confined.spec.program)
        .args(&confined.spec.args)
        .current_dir(project.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let group = child.id() as libc::pid_t;
    let output = child.wait_with_output().unwrap();
    std::fs::write(project.path().join("exited"), "").unwrap();

    let until = |what: &dyn Fn() -> bool| {
        (0..1500).any(|_| {
            let done = what();
            if !done {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            done
        })
    };
    let outlived = until(&|| project.path().join("outlived").exists());
    // SAFETY: plain integers; the group is the one just started.
    unsafe { libc::killpg(group, libc::SIGKILL) };
    // SAFETY: as above; signal 0 only asks whether the group exists.
    let stopped = until(&|| unsafe { libc::killpg(group, 0) } != 0);
    let released = until(&|| matches!((&held).read(&mut [0u8; 1]), Ok(0)));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "out\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "err\n");
    assert_eq!(output.status.code(), Some(3));
    assert!(outlived, "the background loop went with the command");
    assert!(stopped, "stopping the group stops what the command left");
    assert!(released, "what the command left runs on outside its group");
}

/// Profile and bwrap arguments are text. A path that is not UTF-8 would be
/// written lossily, and a rule for a path that does not exist matches
/// nothing (bwrap's `-try` mounts skip it silently), so the command is not
/// wrapped and says so.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn a_path_that_is_not_utf8_is_reported_not_written_lossily() {
    use std::os::unix::ffi::OsStrExt;
    let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/cred\xff"));
    let work = Path::new("/tmp/work");
    let cases = [
        SandboxPolicy {
            deny_read: vec![odd.clone()],
            ..Default::default()
        },
        SandboxPolicy {
            additional_writable: vec![odd.clone()],
            ..Default::default()
        },
    ];
    for policy in cases {
        let mut o = opts("true", work);
        o.policy = policy;
        let c = wrap(o);
        assert_eq!(c.enforcement, Enforcement::None, "{:?}", c.unmet);
        assert!(c.unmet.iter().any(|u| u.contains("UTF-8")), "{:?}", c.unmet);
    }
    let mut o = opts("true", &odd);
    o.policy = SandboxPolicy::default();
    assert_eq!(wrap(o).enforcement, Enforcement::None);
}
