use super::*;

#[cfg(target_os = "macos")]
#[test]
fn macos_wraps_with_sandbox_exec_and_profile_includes_cwd() {
    let cmd = wrap(opts("ls", Path::new("/tmp/work")));
    assert_eq!(cmd.spec.program, "/usr/bin/sandbox-exec");
    assert_eq!(cmd.spec.args[0], "-p");
    assert!(cmd.spec.args[1].contains("(deny file-write*)"));
    assert!(cmd.spec.args[1].contains("(subpath \"/private/tmp/work\")"));
    assert!(!cmd.spec.args[1].contains("(subpath \"/private/tmp\")"));
    assert!(!cmd.spec.args[1].contains("(subpath \"/private/var/folders\")"));
    assert_eq!(cmd.spec.args[2], "bash");
    assert_eq!(cmd.spec.args[3], "-c");
    assert_eq!(cmd.spec.args[4], "ls");
    assert_eq!(cmd.mode, SandboxMode::MacOSSandboxExec);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_additional_writable_dirs_added_to_profile() {
    let extras = vec![
        PathBuf::from("/Users/me/scratch"),
        PathBuf::from("/Users/me/another"),
    ];
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.additional_writable = &extras;
    let cmd = wrap(o);
    assert!(cmd.spec.args[1].contains("(subpath \"/Users/me/scratch\")"));
    assert!(cmd.spec.args[1].contains("(subpath \"/Users/me/another\")"));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_path_with_quotes_is_escaped() {
    let cmd = wrap(opts("ls", Path::new("/tmp/with\"quote")));
    // 反斜杠转义后 sandbox-exec 仍然能 parse
    assert!(cmd.spec.args[1].contains("with\\\"quote"));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_profile_emits_deny_read_for_default_policy() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy = SandboxPolicy {
        allow_read: Vec::new(),
        deny_read: default_deny_read(),
        network_mode: NetworkMode::Unrestricted,
        allowed_domains: Vec::new(),
        state_root: None,
        additional_writable: Vec::new(),
        only_additional_writable: false,
        deny_ipc_escapes: false,
        ..Default::default()
    };
    let cmd = wrap(o);
    let profile = &cmd.spec.args[1];
    // Should at least mention .ssh in the deny-read list
    assert!(profile.contains("(deny file-read*"));
    assert!(profile.contains(".ssh"));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_allow_read_overrides_deny_read() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy = SandboxPolicy {
        allow_read: vec![PathBuf::from("/tmp/some-secret")],
        deny_read: vec![PathBuf::from("/tmp/some-secret")],
        network_mode: NetworkMode::Unrestricted,
        allowed_domains: vec![],
        state_root: None,
        additional_writable: Vec::new(),
        only_additional_writable: false,
        deny_ipc_escapes: false,
        ..Default::default()
    };
    let cmd = wrap(o);
    let profile = &cmd.spec.args[1];
    // Both rules emitted; allow comes after deny so it wins per
    // sandbox-exec evaluation order.
    let deny_idx = profile
        .find("(deny file-read* (subpath \"/tmp/some-secret\"))")
        .unwrap();
    let allow_idx = profile
        .find("(allow file-read* (subpath \"/tmp/some-secret\"))")
        .unwrap();
    assert!(allow_idx > deny_idx);
}

/// The profile without the rules that fence off the daemon itself, which
/// every network mode has.
#[cfg(target_os = "macos")]
fn network_mode_rules(profile: &str) -> String {
    profile
        .lines()
        .filter(|l| !l.starts_with("(deny network-outbound"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(target_os = "macos")]
#[test]
fn macos_unrestricted_network_emits_no_network_rules() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy.network_mode = NetworkMode::Unrestricted;
    let cmd = wrap(o);
    let profile = network_mode_rules(&cmd.spec.args[1]);
    assert!(
        !profile.contains("network"),
        "Unrestricted must not add any network rule, profile:\n{profile}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_deny_all_network_emits_deny_network_rule() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy.network_mode = NetworkMode::DenyAll;
    let cmd = wrap(o);
    let profile = network_mode_rules(&cmd.spec.args[1]);
    assert!(profile.contains("(deny network*)"), "profile:\n{profile}");
    assert!(
        !profile.contains("network-outbound"),
        "DenyAll must not also emit an allowlist rule, profile:\n{profile}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_allowlist_domains_stay_out_of_the_profile_and_are_reported_unmet() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy.network_mode = NetworkMode::Allowlist;
    o.policy.allowed_domains = vec!["api.example.com".into(), "evil\".com".into()];
    let cmd = wrap(o);
    let profile = &cmd.spec.args[1];
    assert!(profile.contains("(deny network*)"), "profile:\n{profile}");
    assert!(
        !network_mode_rules(profile).contains("network-outbound"),
        "profile:\n{profile}"
    );
    assert!(!profile.contains("example.com"), "profile:\n{profile}");
    assert!(!profile.contains("evil"), "profile:\n{profile}");
    assert_eq!(cmd.enforcement, Enforcement::Partial);
    let unmet = cmd.unmet.join("; ");
    assert!(
        unmet.contains("api.example.com") && unmet.contains("evil\".com"),
        "{unmet}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_allowlist_naming_localhost_lets_the_loopback_through() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy.network_mode = NetworkMode::Allowlist;
    o.policy.allowed_domains = vec!["127.0.0.1".into()];
    let cmd = wrap(o);
    let profile = &cmd.spec.args[1];
    let deny_idx = profile.find("(deny network*)").unwrap();
    let allow_idx = profile
        .find("(allow network-outbound (remote ip \"localhost:*\"))")
        .unwrap_or_else(|| panic!("profile:\n{profile}"));
    assert!(allow_idx > deny_idx);
    assert_eq!(cmd.enforcement, Enforcement::Full, "{:?}", cmd.unmet);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_allowlist_with_no_domains_still_denies_network() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy.network_mode = NetworkMode::Allowlist;
    o.policy.allowed_domains = vec![];
    let cmd = wrap(o);
    let profile = network_mode_rules(&cmd.spec.args[1]);
    assert!(profile.contains("(deny network*)"), "profile:\n{profile}");
    assert!(!profile.contains("network-outbound"), "profile:\n{profile}");
    assert_eq!(cmd.enforcement, Enforcement::Full, "{:?}", cmd.unmet);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_network_rules_keep_profile_parens_balanced() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy.network_mode = NetworkMode::Allowlist;
    o.policy.allowed_domains = vec!["a.example.com".into(), "b.example.com".into()];
    let cmd = wrap(o);
    let profile = &cmd.spec.args[1];
    assert_eq!(profile.matches('(').count(), profile.matches(')').count());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_profile_has_balanced_parentheses() {
    // Structural sanity check: the TinyScheme profile must have matching
    // open/close paren counts so sandbox-exec(1) can parse it.
    let o = SandboxOptions {
        command: "echo hi",
        cwd: Path::new("/tmp/work"),
        additional_writable: &[PathBuf::from("/Users/me/scratch")],
        disable: false,
        policy: SandboxPolicy {
            allow_read: Vec::new(),
            deny_read: default_deny_read(),
            network_mode: NetworkMode::Unrestricted,
            allowed_domains: Vec::new(),
            state_root: None,
            additional_writable: Vec::new(),
            only_additional_writable: false,
            deny_ipc_escapes: false,
            ..Default::default()
        },
    };
    let cmd = wrap(o);
    let profile = &cmd.spec.args[1];
    let opens = profile.matches('(').count();
    let closes = profile.matches(')').count();
    assert_eq!(
        opens, closes,
        "macOS sandbox profile must have balanced parens"
    );
    // Must start with (version 1) per TinyScheme sandbox-exec convention
    assert!(
        profile.starts_with("(version 1)"),
        "profile must start with (version 1)"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_profile_denies_settings_json_even_without_deny_read() {
    // Settings.json write denial must be emitted unconditionally,
    // regardless of whether credential deny-read paths are configured —
    // in the instance's own project directory and in the foreign ones.
    for name in [".agix", ".codix"] {
        let o = SandboxOptions {
            command: "ls",
            cwd: Path::new("/tmp/work"),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy {
                allow_read: vec![],
                deny_read: vec![],
                network_mode: NetworkMode::Unrestricted,
                allowed_domains: vec![],
                state_root: None,
                additional_writable: Vec::new(),
                only_additional_writable: false,
                deny_ipc_escapes: false,
                layout: base::paths::Layout::new(name).unwrap(),
                ..Default::default()
            },
        };
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        for dir in [name, ".agix", ".claude"] {
            assert!(
                profile.contains(&format!(
                    "(deny file-write* (literal \"/private/tmp/work/{dir}/settings.json\"))"
                )),
                "{name}: {dir}/settings.json denial must appear unconditionally"
            );
        }
    }
}

/// The regular expressions of the profile's `(deny <operation> (regex …))`
/// rules.
#[cfg(target_os = "macos")]
#[allow(clippy::unwrap_used)]
fn denied_patterns(profile: &str, operation: &str) -> Vec<regex::Regex> {
    let prefix = format!("(deny {operation} (regex \"");
    profile
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix)?.strip_suffix("\"))"))
        .map(|escaped| {
            let mut pattern = String::new();
            let mut chars = escaped.chars();
            while let Some(c) = chars.next() {
                pattern.push(if c == '\\' { chars.next().unwrap() } else { c });
            }
            regex::Regex::new(&pattern).unwrap()
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn denies(patterns: &[regex::Regex], path: &Path) -> bool {
    let path = path.display().to_string();
    patterns.iter().any(|p| p.is_match(&path))
}

/// A declarative scene is defined by the files in its directory, so every
/// scene directory is protected — not just the names the engine knows,
/// and one created after the profile was written as well — and its
/// definition along with its settings.
#[cfg(target_os = "macos")]
#[test]
fn macos_profile_protects_the_scene_files_of_the_root_and_every_scene() {
    let policy = SandboxPolicy {
        state_root: Some(PathBuf::from("/srv/agix-state")),
        ..Default::default()
    };
    let profile = build_macos_profile(
        Path::new("/work/project"),
        &[],
        &policy,
        &read_scope::UserDirs::of(INHERITED),
    );
    let writes = denied_patterns(&profile, "file-write*");
    for p in [
        "/srv/agix-state/settings.json",
        "/srv/agix-state/scenes/code/settings.json",
        "/srv/agix-state/scenes/triage/scene.json",
        "/srv/agix-state/scenes/added-later/prompt.md",
    ] {
        assert!(denies(&writes, Path::new(p)), "{p}\n{profile}");
    }
    for p in [
        "/srv/agix-state/skills/x/settings.json",
        "/srv/agix-state/scenes/code/skills/x/prompt.md",
        "/srv/agix-state/memory/settings.json",
    ] {
        assert!(!denies(&writes, Path::new(p)), "{p}");
    }
}

/// A scene directory linked in from elsewhere is written through its
/// real path, which the pattern over the state root does not match.
#[cfg(target_os = "macos")]
#[test]
fn macos_profile_protects_a_linked_scenes_files_under_their_real_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("state");
    let elsewhere = dir.path().canonicalize().unwrap().join("dotfiles/scene");
    std::fs::create_dir_all(root.join("scenes")).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join("scenes/linked")).unwrap();
    let policy = SandboxPolicy {
        state_root: Some(root.clone()),
        ..Default::default()
    };
    let profile = build_macos_profile(
        Path::new("/work/project"),
        &[],
        &policy,
        &read_scope::UserDirs::of(INHERITED),
    );
    let writes = denied_patterns(&profile, "file-write*");
    for name in ["settings.json", "scene.json", "prompt.md"] {
        let path = elsewhere.join(name);
        assert!(denies(&writes, &path), "{}\n{profile}", path.display());
    }
    assert!(!denies(&writes, &elsewhere.join("skills/x/prompt.md")));
    assert!(!denies(
        &writes,
        &elsewhere.parent().unwrap().join("settings.json")
    ));
}

/// With a root it protects that one and no other: a redirected instance
/// whose profile protected `$HOME/.agix` would be guarding a file nobody
/// uses while leaving its real settings.json — where the permission rules
/// live — writable. Without one it guesses none.
#[cfg(target_os = "macos")]
#[test]
fn macos_profile_protects_the_instance_root_it_was_given() {
    let Some(home) = base::paths::home_dir() else {
        return;
    };
    let home_settings = base::path::resolve_physically(&home.join(".agix/settings.json"));
    let unnamed = build_macos_profile(
        Path::new("/work/project"),
        &[],
        &Default::default(),
        &read_scope::UserDirs::of(INHERITED),
    );
    assert!(!denies(
        &denied_patterns(&unnamed, "file-write*"),
        &home_settings
    ));

    let policy = SandboxPolicy {
        state_root: Some(PathBuf::from("/srv/agix-state")),
        ..Default::default()
    };
    let profile = build_macos_profile(
        Path::new("/work/project"),
        &[],
        &policy,
        &read_scope::UserDirs::of(INHERITED),
    );
    let writes = denied_patterns(&profile, "file-write*");
    assert!(denies(&writes, Path::new("/srv/agix-state/settings.json")));
    assert!(
        !denies(&writes, &home_settings),
        "a redirected instance is still protecting the home-relative path"
    );
}

/// Every credential of `base::paths` but those `deny_read` covers has a
/// read denial in the profile, a scene created after it was written
/// included.
#[cfg(target_os = "macos")]
#[test]
fn every_credential_is_unreadable_under_the_macos_profile() {
    for name in [".agix", ".codix"] {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = state.path().canonicalize().unwrap();
        let cwd = project.path().canonicalize().unwrap();
        let policy = SandboxPolicy {
            state_root: Some(root.clone()),
            layout: base::paths::Layout::new(name).unwrap(),
            ..Default::default()
        };
        let profile = build_macos_profile(&cwd, &[], &policy, &read_scope::UserDirs::of(INHERITED));
        let reads = denied_patterns(&profile, "file-read*");
        for dir in [name, ".agix"] {
            let examples = mask::credential_examples(&root, &cwd.join(dir));
            assert!(!examples.is_empty());
            for (credential, path) in &examples {
                assert!(
                    denies(&reads, path),
                    "{name}: {credential:?}: {} is readable\n{profile}",
                    path.display()
                );
            }
        }
        for p in [
            "skills/x/SKILL.md",
            "scenes/code/scene.json",
            "mcp/servers.json",
        ] {
            assert!(!denies(&reads, &root.join(p)), "{p}");
        }
        assert!(!denies(&reads, &cwd.join(".claude/settings.local.json")));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_profile_empty_deny_read_denies_only_the_engines_own_secrets() {
    let o = SandboxOptions {
        command: "ls",
        cwd: Path::new("/tmp/work"),
        additional_writable: &[],
        disable: false,
        policy: SandboxPolicy {
            allow_read: vec![],
            deny_read: vec![],
            network_mode: NetworkMode::Unrestricted,
            allowed_domains: vec![],
            state_root: None,
            additional_writable: Vec::new(),
            only_additional_writable: false,
            deny_ipc_escapes: false,
            ..Default::default()
        },
    };
    let cmd = wrap(o);
    let profile = &cmd.spec.args[1];
    // Other cases in this process may hold a fenced secret meanwhile.
    let reads: Vec<&str> = profile
        .lines()
        .filter(|l| l.contains("file-read*") && !l.contains("(literal "))
        .collect();
    assert_eq!(reads.len(), 1, "{reads:?}");
    assert!(
        reads
            .iter()
            .all(|l| l.starts_with("(deny file-read* (regex \"^") && l.contains("/\\\\.agix/")),
        "{reads:?}"
    );
}

#[cfg(target_os = "macos")]
#[test]
#[allow(clippy::unwrap_used)]
fn macos_a_proxy_is_let_through_before_the_daemon_fences_and_named_to_the_command() {
    let served = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let _fence = fence_port(served.local_addr().unwrap());
    let policy = SandboxPolicy {
        network_mode: NetworkMode::Proxy,
        egress_proxy: Some(EgressProxy {
            http: "127.0.0.1:3128".parse().unwrap(),
            socks: None,
            no_proxy: Vec::new(),
        }),
        ..Default::default()
    };
    let mut spec = ProcessSpec::new("bash", Path::new("/tmp/work")).args(["-c", "true"]);
    spec.env
        .push(("http_proxy".into(), "http://inherited.example:1".into()));
    let confined = PlatformSandbox.confine(spec, &policy);
    assert_eq!(
        confined.enforcement,
        Enforcement::Full,
        "{:?}",
        confined.unmet
    );
    let profile = &confined.spec.args[1];
    let at = |rule: &str| {
        profile
            .find(rule)
            .unwrap_or_else(|| panic!("{rule} not in:\n{profile}"))
    };
    let deny = at("(deny network*)");
    let allow = at("(allow network-outbound (remote tcp \"localhost:3128\"))");
    let fence = at(&format!(
        "(deny network-outbound (remote tcp \"localhost:{}\"))",
        served.local_addr().unwrap().port()
    ));
    assert!(deny < allow && allow < fence, "{profile}");
    assert_eq!(profile.matches('(').count(), profile.matches(')').count());
    let last = |name: &str| {
        confined
            .spec
            .env
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(last("http_proxy").as_deref(), Some("http://127.0.0.1:3128"));
    assert_eq!(last("NO_PROXY").as_deref(), Some(""));
    assert_eq!(confined.spec.env.len(), 9);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_proxy_mode_without_a_proxy_cuts_the_network_and_says_so() {
    let mut o = opts("ls", Path::new("/tmp/work"));
    o.policy.network_mode = NetworkMode::Proxy;
    let cmd = wrap(o);
    let profile = network_mode_rules(&cmd.spec.args[1]);
    assert!(profile.contains("(deny network*)"), "profile:\n{profile}");
    assert!(!profile.contains("network-outbound"), "profile:\n{profile}");
    assert_eq!(cmd.enforcement, Enforcement::Partial);
    assert!(cmd.spec.env.is_empty());
}
