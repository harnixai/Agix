//! Who made a tool call — the model, a program, a plugin command, a skill's
//! inline command, the user typing a plugin command, or the host outside any
//! turn — and what a plugin command's or function's own calls are held to.

use std::sync::Arc;

/// Who made a call: what can be asked about it, whether the host hears its
/// progress, and how its result is fenced follow from this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Caller {
    Model,
    /// A `RunCode` program — see `ToolContext::from_code`.
    Code,
    /// A plugin command run through `RunExt`, or a plugin function the host
    /// called, whose calls are held to what it was run as.
    Command(Arc<CommandCalls>),
    /// A skill's `` !`cmd` ``, expanded by a `Skill` call or a `/name` command.
    InlineCommand,
    /// A plugin command the user typed as `/name args`: typing it is their
    /// approval of it.
    Person,
    /// The host, calling outside any turn: calling it is its approval of it.
    Host,
}

impl Caller {
    /// Code rather than the model made the call: its result goes back to
    /// that code.
    pub(crate) fn is_code(&self) -> bool {
        matches!(self, Self::Code | Self::Command(_))
    }

    /// Making the call is the approval of it: what the session would ask
    /// about runs.
    pub(crate) fn approves_its_call(&self) -> bool {
        matches!(self, Self::Person | Self::Host)
    }

    /// Whether making the call answers the question the session put about
    /// it: one only a person may answer only a person's call does.
    pub(crate) fn answers(&self, answerable: base::interface::permission::Answerable) -> bool {
        use base::interface::permission::Answerable;
        match self {
            Self::Person => true,
            Self::Host => answerable == Answerable::HookOrPerson,
            _ => false,
        }
    }

    /// Who made the call, as `tool.around` is told.
    pub(crate) fn via(&self) -> String {
        match self {
            Self::Model => "model".into(),
            Self::Code => "RunCode".into(),
            Self::Command(calls) if calls.by_host => format!("PluginCall:{}", calls.command),
            Self::Command(calls) => format!("RunExt:{}", calls.command),
            Self::InlineCommand => "skill".into(),
            Self::Person => "user".into(),
            Self::Host => "host".into(),
        }
    }

    /// The package whose command made the call, as `tool.around` is told.
    pub(crate) fn plugin(&self) -> Option<String> {
        match self {
            Self::Command(calls) => Some(calls.plugin.clone()),
            _ => None,
        }
    }
}

/// What one run of a plugin command's calls are held to: a read-only run
/// writes nothing; a write run writes inside `writes` only, where its
/// invocation stands in for the question the mode would put. A function the
/// host called (`by_host`) is held the same way.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CommandCalls {
    pub command: String,
    pub plugin: String,
    pub read_only: bool,
    /// The globs as the package declares them, for what a refusal says.
    pub declared: Vec<String>,
    pub writes: permissions::write_scope::WriteScope,
    pub grants: Vec<base::permission::PermissionRule>,
    pub by_host: bool,
}

impl CommandCalls {
    pub(crate) fn new(
        scope: base::interface::tool_dispatch::CommandScope,
        root: &std::path::Path,
        by_host: bool,
    ) -> Self {
        let (writes, dropped) = permissions::write_scope::WriteScope::new(&scope.writes, root);
        if !dropped.is_empty() {
            tracing::warn!(
                command = %scope.command,
                dropped = ?dropped,
                "ignoring `writes` globs that reach beyond the project; scope them to a \
                 directory inside it"
            );
        }
        Self {
            grants: writes.grants(),
            command: scope.command,
            plugin: scope.plugin,
            read_only: scope.read_only,
            declared: scope.writes,
            writes,
            by_host,
        }
    }

    fn kind(&self) -> &'static str {
        if self.by_host {
            "function"
        } else {
            "command"
        }
    }

    /// Why a call is refused before the session's permission is asked: a
    /// read-only run that would change something, a write of configuration,
    /// or a write outside `writes`.
    pub(crate) fn refusal(
        &self,
        name: &str,
        facts: &base::interface::permission::ToolFacts,
        tool_ctx: &base::tool::ToolContext,
    ) -> Option<String> {
        use base::interface::permission::Subject;
        let writes_a_file = matches!(facts.subject, Subject::WritePath(_));
        let runs_a_writer = facts.runs.is_some() && !facts.traits.read_only;
        if self.read_only && (writes_a_file || runs_a_writer) {
            return Some(if self.by_host {
                format!(
                    "the host called `{}` without letting it write, and {name} would change \
                     something",
                    self.command
                )
            } else {
                format!(
                    "`{}` ran as a read-only subcommand, and {name} would change something; a \
                     read-only subcommand that writes is a defect of the plugin",
                    self.command
                )
            });
        }
        let Subject::WritePath(path) = &facts.subject else {
            return None;
        };
        let cwd = &tool_ctx.cwd;
        if permissions::write_scope::WriteScope::reaches_configuration(
            path,
            cwd,
            &tool_ctx.config.sandbox.paths,
        ) {
            return Some(format!(
                "{path} is configuration, and configuration paths are never writable by a \
                 plugin {}, whatever its `writes` say; `{}` cannot write it",
                self.kind(),
                self.command
            ));
        }
        if self.writes.covers(path, cwd) {
            return None;
        }
        let scope = if self.declared.is_empty() {
            "writes no files".to_string()
        } else {
            format!("may write only {}", self.declared.join(", "))
        };
        Some(format!(
            "the {} `{}` {scope}, and {path} is outside that",
            self.kind(),
            self.command
        ))
    }
}
