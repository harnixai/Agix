//! The shell that runs bwrap outside the sandbox: the command's stdio
//! passed through, bwrap's own messages kept apart, and the mount points
//! made for absent configuration directories removed afterwards.

use std::path::{Path, PathBuf};

/// Runs bwrap (`"$@"`) with the command's stdout and stderr on fds 3 and 4,
/// bwrap's own stdout on `/dev/null` and its stderr on an unlinked scratch
/// file printed to stderr once bwrap exits; stdin (fd 0) passes through to
/// bwrap and the command untouched. bwrap's PID 1 keeps fds 0–2 for as long
/// as anything the command left in the background runs, which would hold the
/// command's output open; it closes every other fd.
///
/// Once the command's namespace is gone, each of `cleanup.made` that is
/// still an empty directory is removed, deepest first — what bwrap made as a
/// mount point, or covered, to shut a configuration directory that was not
/// there — unless another sandboxed command is running in the same project:
/// every wrapper holds a shared `flock` on the project directory
/// `cleanup.project` for as long as its namespace lives, and removes only
/// once it gets the lock alone. A directory a live namespace holds is a
/// mount point there; removing it on the host would take that mount away and
/// let what runs there make the directory. That fd is closed for bwrap —
/// opened outside the sandbox, a path looked up from it would see none of the
/// sandbox's mounts. Without `flock` in the system directories nothing is
/// removed.
///
/// The namespace outlives bwrap: bwrap exits with the command, while its
/// PID 1 (whose host pid `--info-fd` gives) stays until what the command left
/// in the background is gone, and a `SIGTERM` to the group stops bwrap and
/// the wrapper but not that PID 1 (`pid_namespaces(7)`), only the `SIGKILL`
/// after it. So where PID 1 still has children when bwrap is done, the
/// wrapper hands the lock to a waiter in a session of its own, which outlasts
/// that `SIGKILL`, polls PID 1 and removes after it; where it has none, PID 1
/// is about to exit and the wrapper waits and removes itself, so that the
/// next command is confined against what is left. The wrapper traps
/// `SIGTERM` so as to get that far. A PID 1 the host has not reaped yet
/// counts as gone.
///
/// The scratch file is made in the host's `/tmp`, not the session `TMPDIR`
/// that `mktemp` would otherwise pick up from the environment: the session
/// `TMPDIR` is bound writable into the sandbox, so a process an earlier
/// command left running there could swap the file for a symlink between
/// `mktemp` and the `exec` that opens it, and this wrapper runs outside the
/// sandbox. The sandbox always mounts its own tmpfs over `/tmp`, so a file in
/// the host's `/tmp` is on no path a confined command can reach. `mktemp`,
/// `rm` and `cat` are addressed absolutely so a writable directory earlier on
/// `PATH` cannot stand one of them in.
pub(in crate::sandbox) fn bwrap_stdio_outside(cleanup: &Cleanup) -> String {
    let mktemp = trusted_bin("mktemp");
    let rm = trusted_bin("rm");
    let cat = trusted_bin("cat");
    let (lock, run, remove) = match (find_trusted_bin("flock"), cleanup.made.is_empty()) {
        (Some(flock), false) => {
            let mut made = cleanup.made.clone();
            made.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
            let made: Vec<String> = made.iter().map(|p| sh_quoted(p)).collect();
            let alive = format!(
                "alive() {{ kill -0 \"$1\" 2>/dev/null && ! {grep} -qs '^State:[[:space:]]*Z' \"/proc/$1/status\"; }}",
                grep = trusted_bin("grep")
            );
            let remove_now = format!(
                "{flock} -x -n 7 && {rmdir} -- {made} 2>/dev/null",
                rmdir = trusted_bin("rmdir"),
                made = made.join(" ")
            );
            let sleep = trusted_bin("sleep");
            let waiter = format!(
                "trap : TERM; {alive}; [ -n \"$1\" ] || exit; \
                 while alive \"$1\"; do {sleep} 0.1; done; {remove_now}"
            );
            (
                format!(
                    "locked=; command exec 7<{} && {flock} -s 7 && locked=1 && trap : TERM\n\
                     info=$({mktemp} -p /tmp 2>/dev/null) && exec 8>\"$info\" 9<\"$info\" && {rm} -f \"$info\" || exec 8>/dev/null 9</dev/null\n\
                     bwrap=$1; shift\n",
                    sh_quoted(&cleanup.project)
                ),
                "\"$bwrap\" --info-fd 8 \"$@\"",
                format!(
                    "exec 8>&-\n\
                     pid1=$({sed} -n 's/.*\"child-pid\": *\\([0-9][0-9]*\\).*/\\1/p' <&9)\n\
                     {alive}\n\
                     while [ -n \"$pid1\" ] && alive \"$pid1\" && [ -z \"$({cat} /proc/$pid1/task/$pid1/children 2>/dev/null || echo unknown)\" ]; do {sleep} 0.01; done\n\
                     if [ -z \"$locked\" ]; then :\n\
                     elif [ -n \"$pid1\" ] && ! alive \"$pid1\"; then {remove_now}\n\
                     else {setsid} {sh} -c {waiter} sh \"$pid1\" </dev/null >/dev/null 2>&1 5>&- 6<&- 9<&- &\n\
                     fi\n",
                    sed = trusted_bin("sed"),
                    setsid = find_trusted_bin("setsid").unwrap_or_default(),
                    sh = trusted_bin("sh"),
                    waiter = sh_quoted(Path::new(&waiter)),
                ),
            )
        }
        _ => (String::new(), "\"$@\"", String::new()),
    };
    format!(
        "{lock}err=$({mktemp} -p /tmp 2>/dev/null) && exec 5>\"$err\" 6<\"$err\" && {rm} -f \"$err\" || exec 5>/dev/null 6</dev/null\n\
         {run} 3>&1 4>&2 >/dev/null 2>&5 5>&- 6<&- 7<&- 9<&-\n\
         status=$?\n\
         {remove}\
         {cat} <&6 >&2\n\
         exit \"$status\""
    )
}

/// What [`bwrap_stdio_outside`] removes once bwrap is done, and the project
/// directory it locks to do so.
#[derive(Default)]
pub(in crate::sandbox) struct Cleanup {
    pub(in crate::sandbox) project: PathBuf,
    pub(in crate::sandbox) made: Vec<PathBuf>,
}

fn sh_quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// The absolute path of a core utility `name`, looked up in the system
/// directories rather than `PATH` (which a command could have planted a
/// writable entry on). Falls back to the bare name where none is found, so a
/// deployment with its utilities elsewhere still runs, unhardened.
pub(in crate::sandbox) fn trusted_bin(name: &str) -> String {
    find_trusted_bin(name).unwrap_or_else(|| name.to_string())
}

/// [`trusted_bin`] without the fallback.
pub(in crate::sandbox) fn find_trusted_bin(name: &str) -> Option<String> {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.exists())
        .map(|path| path.display().to_string())
}

/// The inside half of [`bwrap_stdio_outside`]: fds 3 and 4 back to 1 and 2,
/// then the command.
pub(in crate::sandbox) const BWRAP_STDIO_INSIDE: &str = r#"exec 1>&3 2>&4 3>&- 4>&- && exec "$@""#;
