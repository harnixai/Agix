// @ts-check
/// <reference path="../types/agix.d.ts" />
// tool.around（docs/forge.md「护栏」）：提醒合作的模型，项目 forge/ 下的数据经 forge 命令读写。模型、
// RunCode 程序与别的插件命令对 forge/ 的 Read、Write、Edit、NotebookEdit、Glob、Grep，以及命令行里
// 明摆着写到 forge/ 的 Bash、Monitor，拒绝并说该用哪条命令；forge 自己的命令、用户敲的命令与 skill
// 的内联命令放行。它是提醒，不是边界：换个写法的 shell 命令照样过得去。

const FORGE = "forge";
const OWN_COMMAND = `RunExt:${FORGE}`;

const HINT = {
  read: "use the forge command through RunExt instead: forge req get <R-id>, forge spec get <S-id>, " +
    "forge flow show <C-id> (forge help lists them all)",
  write: "change it with the forge command through RunExt instead: forge req add/update, " +
    "forge flow delta/plan/check/verify/advance/archive (forge help lists them all)",
};

/**
 * @typedef {{tool: string, input: any, via: string, plugin?: string | null, projectRoot?: string | null}} Call
 */

/** `plugin` and `projectRoot` come from engines that pass them; without them the guard guesses. @param {Call} call */
function trusted(call) {
  if (call.via === "user" || call.via === "skill") return true;
  return call.via === OWN_COMMAND && (call.plugin == null || call.plugin === FORGE);
}

/**
 * Whether `p` is the project's forge/ or under it. Compared lowercased, since the file systems
 * forge mostly runs on ignore case. Without the project root, a relative path counts from it and
 * an absolute one counts when any directory on it is called forge.
 * @param {unknown} p @param {string | null} root
 */
function underForge(p, root) {
  if (typeof p !== "string" || !p) return false;
  const path = agix.path.normalize(p).toLowerCase();
  if (path.includes("/plugins/cache/")) return false;
  if (root) {
    const dir = agix.path.join(root, FORGE).toLowerCase();
    const full = agix.path.isAbsolute(path) ? path : agix.path.join(root.toLowerCase(), path);
    return full === dir || full.startsWith(`${dir}/`);
  }
  if (agix.path.isAbsolute(path)) return path.endsWith(`/${FORGE}`) || path.includes(`/${FORGE}/`);
  return path === FORGE || path.startsWith(`${FORGE}/`);
}

/** A Glob pattern or Grep filter, as a path from the directory it searches. @param {unknown} dir @param {unknown} pattern */
function within(dir, pattern) {
  if (typeof pattern !== "string") return undefined;
  if (agix.path.isAbsolute(pattern)) return pattern;
  return agix.path.join(typeof dir === "string" && dir ? dir : ".", pattern);
}

/**
 * The words of a command line that are arguments: not a command, not a `NAME=value` before one.
 * Only the plain cases: quoting, variables and substitutions are not followed.
 * @param {string} command
 */
function shellArguments(command) {
  const found = [];
  for (const part of command.split(/[;&|()`\n]+/)) {
    const words = part.trim().split(/\s+/).filter(Boolean);
    let first = 0;
    while (first < words.length && /^[A-Za-z_][A-Za-z0-9_]*=/.test(words[first])) first += 1;
    for (const word of words.slice(first + 1)) {
      for (const piece of word.split("=")) found.push(piece.replace(/^['"]+|['"]+$/g, ""));
    }
  }
  return found;
}

/** @param {Call} call */
function touchesForge(call) {
  const input = call.input || {};
  const root = typeof call.projectRoot === "string" && call.projectRoot ? call.projectRoot : null;
  /** @param {unknown} p */
  const under = (p) => underForge(p, root);
  switch (call.tool) {
    case "Bash":
    case "Monitor":
      return shellArguments(String(input.command || "")).some(under);
    case "Glob":
      return under(input.path) || under(within(input.path, input.pattern));
    case "Grep":
      return under(input.path) || under(within(input.path, input.glob));
    case "Read":
    case "Write":
    case "Edit":
      return under(input.file_path);
    case "NotebookEdit":
      return under(input.notebook_path);
    default:
      return false;
  }
}

/** @param {Call} call */
function onAround(call) {
  if (trusted(call) || !touchesForge(call)) return null;
  const writes = ["Write", "Edit", "NotebookEdit"].includes(call.tool);
  return { action: "deny", reason: `forge/ is kept by the forge command; ${writes ? HINT.write : HINT.read}.` };
}
