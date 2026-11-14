//! Which shell variables, set in front of a command, change what it runs or
//! reads.

/// Variables whose assignment changes how a later word runs or where a read
/// lands: program lookup and loading, the configuration a reader takes its
/// options from (git's `GIT_CONFIG_*`, `RIPGREP_CONFIG_PATH`, `MANOPT`, a
/// `XDG_CONFIG_HOME` with a `git/config` in it), a pager, browser, editor or
/// askpass helper it shells out to (`HOMEBREW_BROWSER`, `SSH_ASKPASS`), and
/// the directories `cd` and `~` resolve against.
const DANGEROUS_VARIABLE_NAMES: &[&str] = &[
    "PATH",
    "BASH_ENV",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PROMPT_COMMAND",
    "IFS",
    "LESSOPEN",
    "LESSCLOSE",
    "MANOPT",
    "MANROFFOPT",
    "MANROFFSEQ",
    "VISUAL",
    "KUBECONFIG",
    "ACKRC",
    "CDPATH",
    "OLDPWD",
    "HOME",
    "PWD",
];

const DANGEROUS_VARIABLE_PREFIXES: &[&str] = &[
    "GIT_",
    "SSH_",
    "LD_",
    "DYLD_",
    "XDG_",
    "GROFF_",
    "DOCKER_",
    "HOMEBREW_",
    "NPM_CONFIG_",
    "PYTHON",
    "PERL",
    "RUBY",
    "NODE_",
    "BAT_",
    "ACK_",
];

const DANGEROUS_VARIABLE_SUFFIXES: &[&str] =
    &["PAGER", "BROWSER", "EDITOR", "_CONFIG_PATH", "_CONFIG_FILE"];

/// Whether setting `name` in front of a command can make it run another
/// program or read somewhere other than it says ([`DANGEROUS_VARIABLE_NAMES`]
/// and the prefixes and suffixes beside it). Case is ignored.
pub fn is_dangerous_variable(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    DANGEROUS_VARIABLE_NAMES.contains(&name.as_str())
        || DANGEROUS_VARIABLE_PREFIXES
            .iter()
            .any(|p| name.starts_with(p))
        || DANGEROUS_VARIABLE_SUFFIXES
            .iter()
            .any(|s| name.ends_with(s))
}
