// @ts-check
// `notes [help] | list | peek | read-guarded | add <title> | pair <a> <b> | escape | stamp`.

const USAGE = "usage: notes help | list | add <title> (body in input.body) | escape";

async function main(argv, input, ctx) {
  const [sub, ...rest] = argv;
  switch (sub) {
    case undefined:
    case "help":
      return USAGE;
    case "list": {
      const found = await tools.Glob({ pattern: "notes/*.md", path: ctx.projectRoot });
      return { readOnly: ctx.readOnly, found: typeof found === "string" ? found : JSON.stringify(found) };
    }
    case "add": {
      const title = rest[0];
      if (!title) throw new Error(USAGE);
      const body = (input && input.body) || "(empty)";
      await tools.Write({ file_path: `${ctx.projectRoot}/notes/${title}.md`, content: body });
      log(`wrote notes/${title}.md`);
      return { added: title };
    }
    case "pair": {
      const outcomes = [];
      for (const title of rest) {
        try {
          await tools.Write({ file_path: `${ctx.projectRoot}/notes/${title}.md`, content: title });
          outcomes.push(`${title}: written`);
        } catch (e) {
          outcomes.push(`${title}: ${e && e.message ? e.message : e}`);
        }
      }
      return outcomes.join("\n");
    }
    case "stamp":
      await tools.Bash({ command: `mkdir -p ${ctx.projectRoot}/notes && touch ${ctx.projectRoot}/notes/stamped.md` });
      return "stamped";
    case "escape":
      await tools.Write({ file_path: `${ctx.projectRoot}/outside.txt`, content: "escaped" });
      return "wrote outside.txt";
    case "peek":
      await tools.Write({ file_path: `${ctx.projectRoot}/notes/peeked.md`, content: "x" });
      return "peeked";
    case "read-guarded": {
      const read = await tools.Read({ file_path: `${ctx.projectRoot}/guarded/secret.txt` });
      return typeof read === "string" ? read : JSON.stringify(read);
    }
    default:
      throw new Error(USAGE);
  }
}
