// @ts-check
/// <reference path="../types/agix.d.ts" />
// tool.around（docs/forge.md「护栏」）：forge/ 下的数据只经 forge 命令读写。模型与 RunCode 程序
// 对 forge/ 的 Read、Write、Edit、Glob、Grep，以及参数里写到 forge/ 下路径的 Bash，一律拒绝，
// 理由里写该用哪条命令；forge 命令自己的子调用（via 为 RunExt:forge）与用户敲的命令放行。

const UNDER_FORGE = ["forge", "forge/**", "**/forge", "**/forge/**"];
const FORGE_PATH_WORD = ["forge/**", "**/forge/**", "/**/forge"];

const HINT = {
  read: "use the forge command through RunExt instead: forge req get <R-id>, forge spec get <S-id>, " +
    "forge flow show <C-id> (forge help lists them all)",
  write: "change it with the forge command through RunExt instead: forge req add/update, " +
    "forge flow delta/plan/check/verify/advance/archive (forge help lists them all)",
};

/** @param {{tool: string, input: any, via: string}} call */
function touchesForge(call) {
  const input = call.input || {};
  if (call.tool === "Bash") {
    const words = String(input.command || "").split(/[\s"'=;|&<>()]+/);
    return words.some((w) => w && agix.glob.test(FORGE_PATH_WORD, w.replace(/^\.\//, "")));
  }
  if (!["Read", "Write", "Edit", "Glob", "Grep"].includes(call.tool)) return false;
  const paths = [input.file_path, input.path, call.tool === "Glob" ? input.pattern : input.glob];
  return paths.some((p) => typeof p === "string" && agix.glob.test(UNDER_FORGE, p.replace(/^\.\//, "")));
}

/** @param {{tool: string, input: any, via: string}} call */
function onAround(call) {
  if (call.via !== "model" && call.via !== "RunCode") return null;
  if (!touchesForge(call)) return null;
  const how = call.tool === "Write" || call.tool === "Edit" ? HINT.write : HINT.read;
  return { action: "deny", reason: `forge/ is kept by the forge command; ${how}.` };
}
