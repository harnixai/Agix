use super::super::*;

fn opts<'a>(cmd: &'a str, cwd: &'a std::path::Path, disable: bool) -> SandboxOptions<'a> {
    SandboxOptions {
        command: cmd,
        cwd,
        additional_writable: &[],
        disable,
        policy: SandboxPolicy::default(),
    }
}

/// An explicit opt-out is unconstrained and says so. This is the case
/// where `Enforcement::None` is the honest answer *and* nothing is wrong.
#[test]
fn disabling_the_sandbox_reports_no_enforcement() {
    let dir = std::env::temp_dir();
    let w = wrap(opts("echo hi", &dir, true));
    assert_eq!(w.mode, SandboxMode::Disabled);
    assert_eq!(w.enforcement, Enforcement::None);
}

/// The invariant the field exists for: `enforcement` and `mode` never
/// disagree about whether anything is actually wrapping the command.
/// A backend that reported `Full` while running bare `bash` would make
/// every consumer's refusal check useless.
#[test]
fn enforcement_agrees_with_the_selected_backend() {
    let dir = std::env::temp_dir();
    let w = wrap(opts("echo hi", &dir, false));
    match w.mode {
        SandboxMode::Disabled | SandboxMode::Unavailable => {
            assert_eq!(w.enforcement, Enforcement::None);
            assert_eq!(
                w.spec.program, "bash",
                "an unenforced command must be plain bash"
            );
        }
        SandboxMode::MacOSSandboxExec => {
            assert_eq!(w.enforcement, Enforcement::Full);
            assert_eq!(w.spec.program, "/usr/bin/sandbox-exec");
        }
        SandboxMode::LinuxBwrap => {
            assert_eq!(w.enforcement, Enforcement::Full);
            assert_eq!(Path::new(&w.spec.program).file_name().unwrap(), "sh");
            assert_eq!(Path::new(&w.spec.args[3]).file_name().unwrap(), "bwrap");
        }
    }
}
