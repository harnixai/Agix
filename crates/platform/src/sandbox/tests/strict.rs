use super::*;

#[cfg(target_os = "macos")]
#[test]
fn macos_a_read_scope_denies_everything_else_before_the_deny_list() {
    let home = base::paths::home_dir().unwrap_or_else(|| PathBuf::from("/Users/me"));
    let project = PathBuf::from("/tmp/work");
    let policy = SandboxPolicy {
        deny_read: vec![home.join(".ssh")],
        allow_read: vec![home.join(".npmrc"), project.join("vendored")],
        ..Default::default()
    }
    .strict([project.clone()], Some(PathBuf::from("/tmp/session-tmp")));
    let cmd = wrap(SandboxOptions {
        command: "true",
        cwd: &project,
        additional_writable: &policy.additional_writable.clone(),
        disable: false,
        policy,
    });
    let profile = &cmd.spec.args[1];
    let scoped = profile
        .find("(deny file-read*)\n(allow file-read-metadata)\n")
        .unwrap_or_else(|| panic!("no read scope:\n{profile}"));
    let ssh = profile
        .find(&format!(
            "(deny file-read* (subpath \"{}",
            home.join(".ssh").display()
        ))
        .unwrap_or_else(|| panic!("no deny_read rule:\n{profile}"));
    assert!(scoped < ssh, "the deny list would be reopened:\n{profile}");
    for root in [
        "/usr",
        "/System",
        "/private/tmp/work",
        "/private/tmp/session-tmp",
    ] {
        assert!(
            profile.contains(&format!("(subpath \"{root}\")")),
            "{root} is not readable:\n{profile}"
        );
    }
    assert!(
        !profile.contains(".npmrc"),
        "an allow_read outside the scope reopened it:\n{profile}"
    );
    assert!(profile.contains("vendored"), "{profile}");
    assert_eq!(profile.matches('(').count(), profile.matches(')').count());
}

/// The strict profile on this machine: the commands the read-only lane
/// lets through — `ls`, `cat`, git through the xcrun shim and directly —
/// run; reading outside the project, writing in it and starting from the
/// daemon's environment do not.
#[cfg(target_os = "macos")]
#[test]
fn a_strict_command_reads_the_project_and_the_system_and_nothing_else() {
    let Some(s) = UnderSandboxExec::new() else {
        return;
    };
    let commit = std::process::Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "add", "a"])
        .current_dir(&s.project)
        .status()
        .unwrap()
        .success()
        && std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null", "commit", "-qm", "a"])
            .current_dir(&s.project)
            .status()
            .unwrap()
            .success();
    assert!(commit);
    std::fs::write(s.elsewhere.join("secret"), "s3cret").unwrap();
    let policy = SandboxPolicy {
        state_root: Some(s.state_root.clone()),
        ..Default::default()
    }
    .strict([s.project.clone()], Some(s.tmp.clone()));
    let unmet = read_scope::unmet(
        &policy.read,
        &read_scope::UserDirs::of(INHERITED),
        std::env::var_os("PATH").as_deref(),
    );
    if !unmet.is_empty() {
        eprintln!("skipped: the strict profile cannot run this machine's git: {unmet:?}");
        return;
    }
    let run = |command: &str| {
        let confined = PlatformSandbox.confine(
            ProcessSpec::new("bash", &s.project).args(["-c", command]),
            &policy,
        );
        assert_eq!(
            confined.enforcement,
            Enforcement::Full,
            "{:?}",
            confined.unmet
        );
        assert_eq!(confined.spec.env_scope, EnvScope::Minimal);
        std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .current_dir(&s.project)
            .output()
            .unwrap()
    };
    let tmp = s.tmp.display().to_string();
    for command in [
        "ls".to_string(),
        "ls -la /".to_string(),
        "cat a".to_string(),
        "head -1 a && wc -l a && grep -r a . >/dev/null".to_string(),
        "date".to_string(),
        "git status --short".to_string(),
        "git log --oneline -1".to_string(),
        "git diff HEAD".to_string(),
        "/usr/bin/git rev-parse HEAD".to_string(),
        "/usr/bin/xcrun --find git".to_string(),
        format!("echo x > '{tmp}/f' && cat '{tmp}/f'"),
    ] {
        let out = run(&command);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "`{command}` failed: {stderr}");
        assert!(
            !stderr.contains("unable to access") && !stderr.contains("xcrun_db"),
            "`{command}`: {stderr}"
        );
    }
    let developer = std::process::Command::new("xcode-select")
        .arg("-p")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    if let Some(dir) = developer {
        let out = run(&format!("'{dir}/usr/bin/git' status --short"));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    for command in [
        format!("cat '{}/secret'", s.elsewhere.display()),
        format!("ls '{}'", s.state_root.display()),
        "echo x > a".to_string(),
        "git add a".to_string(),
    ] {
        assert!(!run(&command).status.success(), "`{command}` was allowed");
    }
}

/// A `git` that is a shim under the home directory (asdf, `~/bin`) and a
/// git configuration that includes a file from elsewhere are outside
/// what the strict profile reads, so git does not run under it: the
/// profile is reported as not fully held, with what is missing. The
/// session's own policy, which reads everything, holds.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn the_strict_profile_reports_a_git_it_cannot_run_as_unmet() {
    use std::os::unix::fs::PermissionsExt;
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap().join("home");
    let shims = home.join(".asdf/shims");
    write_file(&shims.join("git"), "#!/bin/sh\necho shim-ran\n");
    std::fs::set_permissions(shims.join("git"), std::fs::Permissions::from_mode(0o755)).unwrap();
    write_file(
        &home.join(".gitconfig"),
        "[user]\n\tname = T\n[include]\n\tpath = ~/dotfiles/gitconfig\n",
    );
    write_file(&home.join("dotfiles/gitconfig"), "");
    let spec = || {
        let mut spec = ProcessSpec::new("bash", dir.path()).args(["-c", "git"]);
        spec.env.extend([
            (
                "PATH".to_string(),
                format!("{}:/usr/bin:/bin", shims.display()),
            ),
            ("HOME".to_string(), home.display().to_string()),
        ]);
        spec
    };
    let strict = SandboxPolicy::default().strict(Vec::new(), None);

    let confined = PlatformSandbox.confine(spec(), &strict);
    assert_eq!(confined.enforcement, Enforcement::Partial, "{confined:?}");
    let says = |what: &Path| {
        confined
            .unmet
            .iter()
            .any(|line| line.contains(&what.display().to_string()))
    };
    assert!(says(&shims.join("git")), "{:?}", confined.unmet);
    assert!(
        says(&home.join("dotfiles/gitconfig")),
        "{:?}",
        confined.unmet
    );
    let ran = std::process::Command::new(&confined.spec.program)
        .args(&confined.spec.args)
        .envs(confined.spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&ran.stdout).contains("shim-ran"),
        "the shim ran under the strict profile: {ran:?}"
    );

    let session = PlatformSandbox.confine(spec(), &SandboxPolicy::default());
    assert_eq!(
        session.enforcement,
        Enforcement::Full,
        "{:?}",
        session.unmet
    );

    let report = base::interface::exec::ConfinementReport::probe(
        &PlatformSandbox,
        spec(),
        &SandboxPolicy::default(),
        true,
        false,
    );
    assert_eq!(report.strict, Enforcement::Partial);
    assert_eq!(report.strict_unmet, confined.unmet);
    assert_eq!(report.session, Enforcement::Full);
    assert!(report.session_unmet.is_empty(), "{report:?}");
}

/// git reads `$XDG_CONFIG_HOME/git/config` in place of
/// `~/.config/git/config` when the variable is set; the strict profile
/// reads that file but not what it includes from elsewhere, which is
/// reported unmet.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn the_strict_profile_looks_for_the_git_configuration_under_xdg_config_home() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let xdg = root.join("xdg");
    write_file(
        &xdg.join("git/config"),
        "[include]\n\tpath = ../../dotfiles/gitconfig\n",
    );
    write_file(&root.join("dotfiles/gitconfig"), "");
    let mut spec = ProcessSpec::new("bash", &root).args(["-c", "git"]);
    spec.env.extend([
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("HOME".to_string(), home.display().to_string()),
        ("XDG_CONFIG_HOME".to_string(), xdg.display().to_string()),
    ]);

    let confined =
        PlatformSandbox.confine(spec, &SandboxPolicy::default().strict(Vec::new(), None));
    assert_eq!(confined.enforcement, Enforcement::Partial, "{confined:?}");
    let says = |what: &Path| {
        confined
            .unmet
            .iter()
            .any(|line| line.contains(&what.display().to_string()))
    };
    assert!(
        says(&root.join("dotfiles/gitconfig")),
        "{:?}",
        confined.unmet
    );
    assert!(!says(&xdg.join("git/config")), "{:?}", confined.unmet);
}

/// The command's own `HOME`, not the daemon's, is the home whose git
/// configuration the strict profile reads: the one the probe checked.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn the_strict_profile_reads_the_git_configuration_of_the_commands_own_home() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_file(&home.join(".gitconfig"), "[user]\n\tname = SpecHome\n");
    let mut spec =
        ProcessSpec::new("bash", &project).args(["-c", "git config --global --get user.name"]);
    spec.env.extend([
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("HOME".to_string(), home.display().to_string()),
    ]);

    let confined = PlatformSandbox.confine(
        spec,
        &SandboxPolicy::default().strict([project.clone()], None),
    );
    assert_eq!(confined.enforcement, Enforcement::Full, "{confined:?}");
    let ran = std::process::Command::new(&confined.spec.program)
        .args(&confined.spec.args)
        .env_clear()
        .envs(confined.spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&project)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&ran.stdout).trim(),
        "SpecHome",
        "{ran:?}"
    );
}

/// A git configuration kept under `$XDG_CONFIG_HOME/git` is read under
/// the strict profile, file by file: the credentials beside it are not.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn the_strict_profile_reads_the_git_files_under_xdg_config_home_but_not_their_credentials() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    let project = root.join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let xdg = root.join("xdg");
    write_file(&xdg.join("git/config"), "[user]\n\tname = FromXdg\n");
    write_file(&xdg.join("git/ignore"), "ignored\n");
    write_file(&xdg.join("git/attributes"), "*.bin binary\n");
    write_file(
        &xdg.join("git/credentials"),
        "https://u:s3cret@example.com\n",
    );
    let run = |command: &str| {
        let mut spec = ProcessSpec::new("bash", &project).args(["-c", command]);
        spec.env.extend([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), home.display().to_string()),
            ("XDG_CONFIG_HOME".to_string(), xdg.display().to_string()),
        ]);
        let confined = PlatformSandbox.confine(
            spec,
            &SandboxPolicy::default().strict([project.clone()], None),
        );
        assert_eq!(confined.enforcement, Enforcement::Full, "{confined:?}");
        std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .env_clear()
            .envs(confined.spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&project)
            .output()
            .unwrap()
    };

    let name = run("git config --global --get user.name");
    assert_eq!(
        String::from_utf8_lossy(&name.stdout).trim(),
        "FromXdg",
        "{name:?}"
    );
    for file in ["ignore", "attributes"] {
        let out = run(&format!("cat \"$XDG_CONFIG_HOME/git/{file}\""));
        assert!(out.status.success(), "{file}: {out:?}");
    }
    let creds = run("cat \"$XDG_CONFIG_HOME/git/credentials\"");
    assert!(
        !creds.status.success() && !String::from_utf8_lossy(&creds.stdout).contains("s3cret"),
        "{creds:?}"
    );
}

/// `~/.config/git` is read file by file as well: its `credentials` stay
/// unreadable under a policy whose `deny_read` does not name them.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn the_strict_profile_reads_the_git_files_under_dot_config_but_not_their_credentials() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let git = home.join(".config/git");
    write_file(&git.join("config"), "[user]\n\tname = FromDotConfig\n");
    write_file(&git.join("ignore"), "ignored\n");
    write_file(&git.join("attributes"), "*.bin binary\n");
    write_file(&git.join("credentials"), "https://u:s3cret@example.com\n");
    let policy = SandboxPolicy {
        deny_read: Vec::new(),
        ..Default::default()
    }
    .strict([project.clone()], None);
    let run = |command: &str| {
        let mut spec = ProcessSpec::new("bash", &project).args(["-c", command]);
        spec.env.extend([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), home.display().to_string()),
        ]);
        let confined = PlatformSandbox.confine(spec, &policy);
        assert_eq!(confined.enforcement, Enforcement::Full, "{confined:?}");
        std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .env_clear()
            .envs(confined.spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&project)
            .output()
            .unwrap()
    };

    let name = run("git config --global --get user.name");
    assert_eq!(
        String::from_utf8_lossy(&name.stdout).trim(),
        "FromDotConfig",
        "{name:?}"
    );
    for file in ["ignore", "attributes"] {
        let out = run(&format!("cat \"$HOME/.config/git/{file}\""));
        assert!(out.status.success(), "{file}: {out:?}");
    }
    let creds = run("cat \"$HOME/.config/git/credentials\"");
    assert!(
        !creds.status.success() && !String::from_utf8_lossy(&creds.stdout).contains("s3cret"),
        "{creds:?}"
    );
}

/// The strict profile's minimal environment does not pass the daemon's
/// `XDG_CONFIG_HOME` on, so the command's git reads `~/.config/git` and
/// the probe looks there too, not at a configuration git never opens.
/// Run again in a child whose own environment sets the variable.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn the_strict_profile_ignores_a_daemon_xdg_config_home_its_command_does_not_inherit() {
    const ROOT: &str = "AGIX_TEST_STRICT_XDG_ROOT";
    if !a_backend_runs() {
        return;
    }
    let Some(root) = std::env::var_os(ROOT).map(PathBuf::from) else {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_file(
            &root.join("xdg/git/config"),
            "[include]\n\tpath = ../../dotfiles/gitconfig\n",
        );
        write_file(&root.join("dotfiles/gitconfig"), "");
        write_file(
            &root.join("home/.config/git/config"),
            "[user]\n\tname = FromDotConfig\n",
        );
        std::fs::create_dir_all(root.join("project")).unwrap();
        let test = format!(
            "{}::the_strict_profile_ignores_a_daemon_xdg_config_home_its_command_does_not_inherit",
            module_path!().split_once("::").unwrap().1
        );
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test.as_str(), "--exact", "--test-threads=1"])
            .env(ROOT, &root)
            .env("XDG_CONFIG_HOME", root.join("xdg"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    };
    let home = root.join("home");
    let project = root.join("project");
    let mut spec =
        ProcessSpec::new("bash", &project).args(["-c", "git config --global --get user.name"]);
    spec.env.extend([
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("HOME".to_string(), home.display().to_string()),
    ]);

    let confined = PlatformSandbox.confine(
        spec,
        &SandboxPolicy::default().strict([project.clone()], None),
    );
    assert_eq!(confined.enforcement, Enforcement::Full, "{confined:?}");
    let ran = std::process::Command::new(&confined.spec.program)
        .args(&confined.spec.args)
        .env_clear()
        .envs(confined.spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&project)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&ran.stdout).trim(),
        "FromDotConfig",
        "{ran:?}"
    );
}
