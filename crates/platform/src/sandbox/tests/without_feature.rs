use super::super::*;

fn opts(command: &'static str) -> SandboxOptions<'static> {
    SandboxOptions {
        command,
        cwd: Path::new("/tmp/work"),
        additional_writable: &[],
        policy: SandboxPolicy::default(),
        disable: false,
    }
}

#[test]
fn a_build_without_the_backend_says_so_and_claims_nothing() {
    let c = wrap(opts("echo hi"));
    assert_eq!(c.mode, SandboxMode::Unavailable);
    assert_eq!(c.enforcement, Enforcement::None);
    assert!(
        c.unmet.iter().any(|u| u.contains("--features sandbox")),
        "the shortfall must name what is missing, not just that something is: {:?}",
        c.unmet
    );
}

#[test]
fn the_command_still_runs_unchanged() {
    let c = wrap(opts("echo hi"));
    assert_eq!(c.spec.program, "bash");
    assert_eq!(c.spec.args, vec!["-c".to_string(), "echo hi".to_string()]);
}

#[test]
fn status_names_the_feature_rather_than_the_platform() {
    assert!(
        sandbox_status().contains("--features sandbox"),
        "{}",
        sandbox_status()
    );
}
