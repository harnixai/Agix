//! The table of commands that run another command named in their arguments
//! (`env`, `sudo`, `timeout`, `xargs`, ...) and how to find where it starts.

use crate::shell::Word;

/// A command that runs another command named in its arguments.
pub(super) struct Wrapper {
    pub(super) names: &'static [&'static str],
    /// Short options followed by a value, in the same word or the next.
    short_values: &'static str,
    short_flags: &'static str,
    /// Short options whose value, if any, is attached (`xargs -i{}`).
    short_optional: &'static str,
    long_values: &'static [&'static str],
    long_flags: &'static [&'static str],
    /// Options under which nothing is run, only looked up (`command -v`).
    lookups: &'static str,
    long_lookups: &'static [&'static str],
    /// Options whose value is itself a command line this does not read.
    opaque: &'static str,
    long_opaque: &'static [&'static str],
    /// `-N` is an option (`nice -5`).
    numeric_flags: bool,
    /// Words between the options and the command (`timeout`'s duration).
    operands: usize,
    /// `NAME=value` words may come before the command (`env`).
    pub(super) assignments: bool,
}

const NO_WRAPPER: Wrapper = Wrapper {
    names: &[],
    short_values: "",
    short_flags: "",
    short_optional: "",
    long_values: &[],
    long_flags: &[],
    lookups: "",
    long_lookups: &[],
    opaque: "",
    long_opaque: &[],
    numeric_flags: false,
    operands: 0,
    assignments: false,
};

pub(super) const WRAPPERS: &[Wrapper] = &[
    Wrapper {
        names: &["command"],
        short_flags: "p",
        lookups: "vV",
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["builtin", "nohup", "coproc", "busybox", "arch"],
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["pkexec"],
        long_values: &["user"],
        long_flags: &["disable-internal-agent", "keep-cwd", "version", "help"],
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["caffeinate"],
        short_values: "tw",
        short_flags: "dimsu",
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["ionice"],
        short_values: "cnp",
        short_flags: "t",
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["chrt"],
        short_values: "pTPD",
        short_flags: "ofrbidRmav",
        long_flags: &["reset-on-fork"],
        operands: 1,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["taskset"],
        short_values: "p",
        short_flags: "ac",
        operands: 1,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["flock"],
        short_values: "wE",
        short_flags: "sxunoF",
        operands: 1,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["exec"],
        short_values: "a",
        short_flags: "cl",
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["env"],
        short_values: "uCP",
        short_flags: "i0v",
        long_values: &["unset", "chdir"],
        long_flags: &[
            "ignore-environment",
            "null",
            "debug",
            "default-signal",
            "ignore-signal",
            "block-signal",
            "list-signal-handling",
        ],
        opaque: "S",
        long_opaque: &["split-string"],
        assignments: true,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["nice"],
        short_values: "n",
        long_values: &["adjustment"],
        numeric_flags: true,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["time"],
        short_values: "fo",
        short_flags: "pavq",
        long_values: &["format", "output"],
        long_flags: &["append", "verbose", "portability", "quiet"],
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["sudo"],
        short_values: "ugCDhprtTU",
        short_flags: "AbEHiKknPSsBN",
        long_values: &[
            "user",
            "group",
            "close-from",
            "chdir",
            "host",
            "prompt",
            "role",
            "type",
            "command-timeout",
            "other-user",
        ],
        long_flags: &[
            "askpass",
            "background",
            "bell",
            "preserve-env",
            "preserve-groups",
            "set-home",
            "login",
            "remove-timestamp",
            "reset-timestamp",
            "non-interactive",
            "stdin",
            "shell",
        ],
        lookups: "elvV",
        long_lookups: &["edit", "list", "validate", "version", "help"],
        assignments: true,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["doas"],
        short_values: "uC",
        short_flags: "ns",
        lookups: "L",
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["timeout", "gtimeout"],
        short_values: "sk",
        short_flags: "v",
        long_values: &["signal", "kill-after"],
        long_flags: &["preserve-status", "foreground", "verbose"],
        operands: 1,
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["stdbuf"],
        short_values: "ioe",
        long_values: &["input", "output", "error"],
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["xargs", "gxargs"],
        short_values: "IaELnPsd",
        short_flags: "0prtx",
        short_optional: "iel",
        long_values: &[
            "arg-file",
            "delimiter",
            "max-lines",
            "max-args",
            "max-procs",
            "max-chars",
            "process-slot-var",
        ],
        long_flags: &[
            "null",
            "no-run-if-empty",
            "verbose",
            "interactive",
            "exit",
            "open-tty",
            "show-limits",
            "replace",
            "eof",
        ],
        ..NO_WRAPPER
    },
    Wrapper {
        names: &["setsid"],
        short_flags: "cfw",
        long_flags: &["ctty", "fork", "wait"],
        ..NO_WRAPPER
    },
];

/// A word `env` / `sudo` take as `NAME=value` rather than as the command:
/// any word with an `=` in it, quoted or not, whether or not the shell would
/// call `NAME` a variable name.
pub(super) fn passes_on_assignment(word: &str) -> bool {
    word.contains('=') && !word.starts_with('-')
}

pub(super) enum Wrapped {
    /// The wrapped command starts at this index of the wrapper's arguments.
    At(usize),
    /// The wrapper runs nothing else.
    Nothing,
    /// An option this cannot read: where the command starts is unknown.
    Unreadable,
}

impl Wrapper {
    pub(super) fn command_in(&self, args: &[Word]) -> Wrapped {
        let mut i = 0;
        while let Some(w) = args.get(i) {
            let t = w.text.as_str();
            if t == "--" {
                i += 1;
                break;
            }
            if let Some(long) = t.strip_prefix("--") {
                let (name, inline_value) = match long.split_once('=') {
                    Some((name, _)) => (name, true),
                    None => (long, false),
                };
                if self.long_opaque.contains(&name) {
                    return Wrapped::Unreadable;
                }
                if self.long_lookups.contains(&name) {
                    return Wrapped::Nothing;
                }
                if self.long_values.contains(&name) {
                    i += if inline_value { 1 } else { 2 };
                } else if self.long_flags.contains(&name) {
                    i += 1;
                } else {
                    return Wrapped::Unreadable;
                }
                continue;
            }
            if t == "-" && self.assignments {
                i += 1;
                continue;
            }
            let Some(cluster) = t.strip_prefix('-').filter(|c| !c.is_empty()) else {
                break;
            };
            if self.numeric_flags && cluster.chars().all(|c| c.is_ascii_digit()) {
                i += 1;
                continue;
            }
            let mut value_in_next = false;
            for (k, c) in cluster.char_indices() {
                if self.opaque.contains(c) {
                    return Wrapped::Unreadable;
                }
                if self.lookups.contains(c) {
                    return Wrapped::Nothing;
                }
                if self.short_optional.contains(c) {
                    break;
                }
                if self.short_values.contains(c) {
                    value_in_next = k + c.len_utf8() == cluster.len();
                    break;
                }
                if !self.short_flags.contains(c) {
                    return Wrapped::Unreadable;
                }
            }
            i += if value_in_next { 2 } else { 1 };
        }
        i += self.operands;
        if self.assignments {
            while args.get(i).is_some_and(|w| passes_on_assignment(&w.text)) {
                i += 1;
            }
        }
        if i < args.len() {
            Wrapped::At(i)
        } else {
            Wrapped::Nothing
        }
    }
}
