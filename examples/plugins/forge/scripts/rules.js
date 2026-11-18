// @ts-check
// prompt.block（docs/forge.md「护栏」）：告诉模型这个项目的需求、规格、变更都在 forge 里。

function onBlock() {
  return {
    name: "forge.rules",
    order: 450,
    content:
      "This project keeps its requirements, specs and changes in forge. Read and change them only " +
      "with the `forge` command through the RunExt tool (`forge help` prints its usage); never open, " +
      "list or edit files under forge/ directly. The forge-* skills walk a change from propose to archive.",
  };
}
