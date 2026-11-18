// Refuses reads under the project's guarded/ unless this package's own
// `notes` command makes them: a command of the same name from another
// package is not it.
function onAround(call) {
  const path = String((call.input && call.input.file_path) || "");
  const guarded = call.projectRoot !== null && path.startsWith(`${call.projectRoot}/guarded/`);
  const ours = call.via === "RunExt:notes" && call.plugin === "notes-plugin";
  if (call.tool === "Read" && guarded && !ours) {
    return { action: "deny", reason: `guarded/ is read through the notes command, not via ${call.via}` };
  }
  return null;
}
