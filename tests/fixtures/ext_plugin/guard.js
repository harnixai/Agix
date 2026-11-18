// Refuses reads under guarded/ unless a plugin command makes them.
function onAround(call) {
  const path = String((call.input && call.input.file_path) || "");
  if (call.tool === "Read" && path.includes("guarded/") && !String(call.via).startsWith("RunExt:")) {
    return { action: "deny", reason: `guarded/ is read through the notes command, not via ${call.via}` };
  }
  return null;
}
