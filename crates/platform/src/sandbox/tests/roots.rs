use super::*;

#[cfg(feature = "sandbox")]
#[test]
fn a_parent_step_after_a_missing_directory_resolves_to_the_real_project() {
    let dir = tempfile::tempdir().unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    let cwd = dir.path().join("missing/../proj");
    let dirs = project_agix_dirs(&cwd);
    assert!(dirs.contains(&real.join("proj/.agix")), "{dirs:?}");
}

/// A submodule's git directory, `modules/<name>` under the git directory
/// of the repository that holds it, as `git submodule` lays it out.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(clippy::unwrap_used)]
fn plant_git_dir(dir: &Path) {
    std::fs::create_dir_all(dir.join("hooks")).unwrap();
    std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(dir.join("config"), "[core]\n\tbare = false\n").unwrap();
}

/// A submodule's configuration and hooks are git's as much as the
/// superproject's: `core.fsmonitor` planted in one runs the next time
/// the user's git, outside the sandbox, looks at the submodule. Nested
/// submodules have git directories of their own under `modules/`, and
/// the name a submodule is filed under may itself contain a slash.
/// Removing a submodule's `HEAD` first does not unguard it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn a_sandboxed_command_cannot_rewrite_a_submodules_configuration_or_hooks() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let state = dir.path().join("state");
    let elsewhere = dir.path().join("elsewhere");
    for d in [&project, &state, &elsewhere] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::create_dir_all(project.join(".agix")).unwrap();
    let git = project.join(".git");
    plant_git_dir(&git);
    let modules = git.join("modules");
    for module in ["sub", "a", "a/modules/b", "libs/c"] {
        plant_git_dir(&modules.join(module));
    }
    let policy = SandboxPolicy {
        state_root: Some(state.clone()),
        ..Default::default()
    };
    let runs = |command: &str| {
        run_confined(&project, &policy, &[], command)
            .status
            .success()
    };

    let fsmonitor = "printf '[core]\\n\\tfsmonitor = touch pwned\\n' >>";
    runs("rm .git/modules/sub/HEAD");
    let elsewhere = elsewhere.display();
    let mut refused = vec![
        format!("{fsmonitor} .git/modules/sub/config"),
        format!("{fsmonitor} .git/modules/a/modules/b/config"),
        format!("{fsmonitor} .git/modules/libs/c/config"),
        "echo 'touch pwned' > .git/modules/sub/hooks/post-checkout".to_string(),
        "echo 'touch pwned' > .git/modules/a/modules/b/hooks/x".to_string(),
        "mv .git/modules/sub .git/modules/gone".to_string(),
        "mv .git/modules/a/modules .git/modules/a/gone".to_string(),
        format!("mv .git/modules/libs '{elsewhere}/libs'"),
    ];
    // bwrap cannot hold a file that is not there yet (the module header's
    // escape path 3); a submodule's git directory has no `commondir`.
    let absent_files_held = cfg!(target_os = "macos");
    if absent_files_held {
        refused.push("echo /tmp > .git/modules/sub/commondir".to_string());
    }
    for command in refused {
        assert!(!runs(&command), "`{command}` was allowed");
    }
    for module in ["sub", "a/modules/b", "libs/c"] {
        let module = modules.join(module);
        assert!(
            module.symlink_metadata().unwrap().is_dir(),
            "{} was replaced",
            module.display()
        );
        let config = std::fs::read_to_string(module.join("config")).unwrap();
        assert!(!config.contains("fsmonitor"), "{config}");
        assert!(!absent_files_held || !module.join("commondir").exists());
    }
    assert!(!modules.join("a/modules/b/hooks/x").exists());

    assert!(
        runs("echo x > .git/modules/sub/index && echo x > .git/modules/a/modules/b/FETCH_HEAD"),
        "a submodule's own bookkeeping must stay writable"
    );
}

/// A project that lives in the state root lifts the fence around it,
/// and the state root is then guarded as the configuration root it is:
/// its skills, hooks and agents, and its settings files, stay
/// read-only. The project itself stays writable.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[allow(clippy::unwrap_used)]
fn a_project_in_the_state_root_cannot_rewrite_the_state_roots_configuration() {
    if !a_backend_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().canonicalize().unwrap().join("state");
    write_file(&state.join("skills/s/SKILL.md"), "fine");
    write_file(&state.join("hooks/h.sh"), "fine");
    write_file(&state.join("settings.local.json"), "{}");
    let policy = SandboxPolicy {
        state_root: Some(state.clone()),
        ..Default::default()
    };
    let runs = |command: &str| run_confined(&state, &policy, &[], command).status.success();

    assert!(
        runs("echo x > notes.md"),
        "the project itself stays writable"
    );
    for command in [
        "echo x > skills/s/SKILL.md",
        "mkdir -p skills/planted && echo x > skills/planted/SKILL.md",
        "echo x >> hooks/h.sh",
        "mkdir -p agents && echo x > agents/a.md",
        "echo '{}' > settings.local.json",
    ] {
        assert!(!runs(command), "`{command}` was allowed");
    }
    assert_eq!(
        std::fs::read_to_string(state.join("skills/s/SKILL.md")).unwrap(),
        "fine"
    );
}
