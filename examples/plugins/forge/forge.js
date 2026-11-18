// @ts-check
/// <reference path="types/runext.d.ts" />
/// <reference path="types/agix.d.ts" />
// forge 命令（docs/forge.md）：需求、规格与变更流程。模型经 RunExt 调，用户可以直接 /forge …。
// 依次是：frontmatter 与 input 的 schema、命令分发与用法、forge/ 下的文件、需求与规格的两种后端
// （本地 Markdown、经 callTool 调的 MCP 服务）、规格条目的解析与改写、变更流程。

const DIR = "forge";
const STAGES = ["propose", "spec", "plan", "apply", "verify", "archive", "done"];
const REQ_ID = "^R-[0-9]{3,}$";
const SPEC_ID = "^S-[a-z0-9][a-z0-9-]*-[0-9]{3,}$";
const CHANGE_ID = "^C-[0-9]{3,}$";
const CAPABILITY = "^[a-z0-9][a-z0-9-]*$";
const SERVER = "^[A-Za-z0-9_-]+$";

const str = (pattern) => (pattern ? { type: "string", pattern } : { type: "string" });
const text = str("\\S");
const arrayOf = (items, minItems = 0) => ({ type: "array", items, minItems });
const ids = (pattern) => arrayOf(str(pattern));
/** @param {object} properties @param {string[]} required */
const closed = (properties, required = []) => ({ type: "object", properties, required, additionalProperties: false });
const loose = (properties, required = []) => ({ type: "object", properties, required });

const PRIORITY = { enum: ["high", "medium", "low"] };
const backend = {
  ...closed({ backend: { enum: ["local", "mcp"] }, server: str(SERVER) }, ["backend"]),
  if: { properties: { backend: { const: "mcp" } } },
  then: { required: ["server"] },
};
const RESULT = { spec: str(SPEC_ID), criterion: str("^A[0-9]+$"), pass: { type: "boolean" }, note: str() };

const SCHEMA = {
  config: closed({ reqs: backend, specs: backend, flow: closed({ auto: { type: "boolean" }, stop_before: arrayOf({ enum: STAGES }) }) }),
  req: closed(
    {
      id: str(REQ_ID), title: text, status: { enum: ["proposed", "accepted", "rejected", "done"] }, priority: PRIORITY,
      specs: ids(SPEC_ID), changes: ids(CHANGE_ID), body: str(),
    },
    ["id", "title", "status", "priority"],
  ),
  specFile: closed({ capability: str(CAPABILITY) }, ["capability"]),
  entry: loose(
    {
      id: str(SPEC_ID), capability: str(CAPABILITY), title: text, reqs: ids(REQ_ID), body: str(),
      criteria: arrayOf(loose({ id: str("^A[0-9]+$"), text }, ["id", "text"])), removed: { type: "boolean" },
    },
    ["id", "capability", "title", "reqs", "body", "criteria"],
  ),
  change: closed(
    {
      id: str(CHANGE_ID), title: text, stage: { enum: STAGES }, reqs: ids(REQ_ID),
      history: arrayOf(loose({ stage: { enum: STAGES }, at: str() }, ["stage", "at"])),
      plan: arrayOf(closed({ id: str("^P[0-9]+$"), text: str(), done: { type: "boolean" } }, ["id", "text", "done"])),
      verification: arrayOf(closed({ ...RESULT, at: str() }, ["spec", "criterion", "pass"])),
    },
    ["id", "title", "stage", "reqs", "history", "plan", "verification"],
  ),
  archived: loose({ id: str(CHANGE_ID), archived: { const: true } }, ["id", "archived"]),
  delta: closed(
    {
      change: str(CHANGE_ID), add: ids(SPEC_ID), modify: ids(SPEC_ID), remove: ids(SPEC_ID),
      base: { type: "object", propertyNames: str(SPEC_ID), additionalProperties: str("^[0-9a-f]{64}$") },
    },
    ["change", "add", "modify", "remove", "base"],
  ),
};

const reqInput = closed({ title: text, priority: PRIORITY, body: str() }, ["title", "body"]);
const entryInput = { title: text, body: str(), criteria: arrayOf(text, 1), reqs: ids(REQ_ID) };
const INPUT = {
  reqAdd: reqInput,
  reqAddMany: closed({ items: arrayOf(reqInput, 1) }, ["items"]),
  flowNew: closed({ title: text, reqs: ids(REQ_ID), body: str() }, ["title", "body"]),
  delta: closed({
    add: arrayOf(closed({ capability: str(CAPABILITY), ...entryInput }, ["capability", "title", "body", "criteria"])),
    modify: arrayOf(closed({ id: str(SPEC_ID), ...entryInput }, ["id"])),
    remove: ids(SPEC_ID),
  }),
  plan: closed({ items: arrayOf(text, 1), design: str() }, ["items"]),
  verify: closed({ results: arrayOf(closed(RESULT, ["spec", "criterion", "pass"]), 1) }, ["results"]),
};

const USAGE = {
  req: `forge req list [--status proposed|accepted|rejected|done] [--priority high|medium|low]
forge req get <R-id>
forge req add                     input: {title, priority?, body} or {items: [{title, priority?, body}, ...]}
forge req update <R-id> [--status s] [--priority p] [--title t]    input.body, if given, replaces the body`,
  spec: `forge spec list [--capability c]
forge spec get <S-id>
forge spec coverage               accepted reqs without spec entries, entries without reqs, criteria never verified as passing`,
  flow: `forge flow new                    input: {title, reqs?: [R-id], body}; opens a change at stage propose
forge flow list
forge flow show <C-id>            stage, plan, delta (with criteria), verification
forge flow next <C-id>            the stage to work on now, its skill, whether auto mode goes on
forge flow delta <C-id>           input: {add: [{capability, title, body, criteria: [text], reqs?}],
                                          modify: [{id, title?, body?, criteria?, reqs?}], remove: [S-id]}
                                  replaces the change's whole delta; ids are assigned here
forge flow plan <C-id>            input: {items: [text], design?}; replaces the plan
forge flow check <C-id> <P#>...   marks plan items done
forge flow verify <C-id>          input: {results: [{spec, criterion, pass, note}]}
forge flow advance <C-id>         moves to the next stage
forge flow archive <C-id>         merges the delta into the specs, marks the reqs done, moves the change to archive/`,
};

/** @param {string} [group] */
function help(group) {
  if (group && Object.hasOwn(USAGE, group)) return USAGE[group];
  return `forge keeps this project's requirements, specs and changes under forge/.\n\n${Object.values(USAGE).join("\n\n")}\n\nforge help <req|spec|flow> for one group.`;
}

class UsageError extends Error {}

/** @type {string} */
let ROOT = "";

/** @param {string[]} argv @param {any} input @param {RunExtContext} ctx */
async function main(argv, input, ctx) {
  ROOT = ctx.projectRoot;
  const [group, verb, ...rest] = argv;
  if (!group || group === "help" || group === "--help") return help(verb);
  if (argv.includes("--help")) return help(group);
  const verbs = Object.hasOwn(COMMANDS, group) ? COMMANDS[group] : {};
  if (!Object.hasOwn(verbs, verb)) throw new Error(`unknown command: forge ${argv.join(" ")}\n\n${help(group)}`);
  try {
    return await verbs[verb](rest, inputObject(input));
  } catch (e) {
    const cli = e && typeof e.code === "string" && e.code.startsWith("cli.");
    if (cli || e instanceof UsageError) throw new Error(`${e.message}\n\n${USAGE[group]}`);
    throw e;
  }
}

/** `input` as an object; models often send it as JSON or YAML text. @param {unknown} input */
function inputObject(input) {
  if (input === null || input === undefined) return {};
  let value = input;
  if (typeof input === "string") value = parseText(input);
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("input must be an object, such as {\"title\": \"…\", \"body\": \"…\"}");
  }
  return value;
}

/** @param {string} input */
function parseText(input) {
  try {
    return JSON.parse(input);
  } catch {
    try {
      return agix.yaml.parse(input);
    } catch (e) {
      throw new Error(`input must be an object (JSON); the text given does not parse: ${e.message}`);
    }
  }
}

/**
 * @param {string[]} rest @param {string[]} names
 * @param {number} positionals how many; negative: at least that many
 * @returns {{ids: string[], [option: string]: any}}
 */
function args(rest, names, positionals) {
  /** @type {Record<string, {type: "string"}>} */
  const options = {};
  for (const n of names) options[n] = { type: "string" };
  const parsed = agix.cli.parseArgs(rest, { options, allowPositionals: positionals !== 0 });
  const got = parsed.positionals.length;
  const enough = positionals < 0 ? got >= -positionals : got === positionals;
  if (!enough) {
    const want = positionals < 0 ? `at least ${-positionals}` : String(positionals);
    throw new UsageError(`expected ${want} argument${positionals === 1 ? "" : "s"}, got ${got}`);
  }
  return { ...parsed.values, ids: parsed.positionals };
}

/** @param {object} schema @param {unknown} value @param {string} where */
function check(schema, value, where) {
  const { valid, errors } = agix.json.validate(schema, value);
  if (!valid) {
    throw new Error(`${where}: ${errors.map((e) => `${e.instancePath || "(top)"} ${e.message}`).join("; ")}`);
  }
}

/** @param {string} id @param {string} pattern @param {string} what */
function need(id, pattern, what) {
  if (!new RegExp(pattern).test(String(id))) throw new Error(`${id} is not ${what}`);
  return id;
}

/** @param {string[]} list */
const repeated = (list) => [...new Set(list.filter((x, i) => list.indexOf(x) !== i))];

const now = () => new Date().toISOString().replace(/\.\d+Z$/, "Z");
const pad = (n) => String(n).padStart(3, "0");
/** @param {string[]} taken @param {string} prefix */
const nextNumber = (taken, prefix) =>
  taken.reduce((max, id) => {
    const n = id.startsWith(prefix) ? Number(id.slice(prefix.length)) : NaN;
    return Number.isInteger(n) && n > max ? n : max;
  }, 0) + 1;

const abs = (rel) => agix.path.join(ROOT, DIR, rel);

/** @type {Set<string> | null} */
let existing = null;
/** @type {Set<string>} */
const readThisRun = new Set();

/**
 * The files under forge/, relative to it, from one Glob per run. Reads go only to files listed
 * here: a Read of a path whose directory does not exist yet can be judged outside the project.
 */
async function forgeFiles() {
  if (!existing) {
    const found = await tools.Glob({ pattern: `${DIR}/**`, path: ROOT, respect_gitignore: false, max_results: 100000 });
    if (found.more) throw new Error(`forge/ holds more than ${found.paths.length} files, more than forge reads`);
    const top = agix.path.join(ROOT, DIR);
    const relative = found.paths.map((p) => agix.path.relative(top, agix.path.isAbsolute(p) ? p : agix.path.join(ROOT, p)));
    existing = new Set(relative.filter((p) => !p.startsWith("..")));
  }
  return existing;
}

/** @param {string} rel @returns {Promise<string | null>} */
async function readText(rel) {
  if (!(await forgeFiles()).has(rel)) return null;
  const read = await tools.Read({ file_path: abs(rel) });
  if (read.truncated) throw new Error(`forge/${rel} is longer than one read returns`);
  readThisRun.add(rel);
  return read.content;
}

/** Write replaces a file only once this run has read it. @param {string} rel @param {string} content */
async function writeText(rel, content) {
  if ((await forgeFiles()).has(rel) && !readThisRun.has(rel)) await readText(rel);
  await tools.Write({ file_path: abs(rel), content });
  (await forgeFiles()).add(rel);
  readThisRun.add(rel);
}

/** Files under forge/ matching `pattern`, relative to forge/. @param {string} pattern */
async function list(pattern) {
  return agix.glob.filter(pattern, [...(await forgeFiles())]).sort();
}

/** @param {string} rel @param {object} schema */
async function readDoc(rel, schema) {
  const raw = await readText(rel);
  if (raw === null) return null;
  let doc;
  try {
    doc = agix.yaml.parseFrontmatter(raw);
  } catch (e) {
    throw new Error(`forge/${rel}: ${e.message}${e.line ? ` (line ${e.line})` : ""}`);
  }
  if (doc.data === null) throw new Error(`forge/${rel}: no YAML frontmatter`);
  check(schema, doc.data, `forge/${rel}`);
  return { data: /** @type {any} */ (doc.data), body: doc.body };
}

/** @param {string} rel @param {object} schema @param {object} data @param {string} body */
async function writeDoc(rel, schema, data, body) {
  check(schema, data, `forge/${rel}`);
  await writeText(rel, agix.yaml.stringifyFrontmatter(data, body));
}

async function config() {
  const raw = await readText("config.yaml");
  let data = {};
  if (raw !== null) {
    try {
      data = agix.yaml.parse(raw) || {};
    } catch (e) {
      throw new Error(`forge/config.yaml: ${e.message}`);
    }
  }
  check(SCHEMA.config, data, "forge/config.yaml");
  const cfg = /** @type {any} */ (data);
  return {
    reqs: cfg.reqs || { backend: "local" },
    specs: cfg.specs || { backend: "local" },
    flow: { auto: Boolean(cfg.flow && cfg.flow.auto), stopBefore: (cfg.flow && cfg.flow.stop_before) || [] },
  };
}

/**
 * @typedef {{ id?: string, title: string, status: string, priority: string, specs: string[], changes: string[], body: string }} Req
 * @typedef {{ id: string, capability: string, title: string, reqs: string[], body: string, criteria: {id: string, text: string}[], removed?: boolean }} Entry
 * @typedef {{ all(): Promise<any[]>, get(id: string): Promise<any>, put(item: any): Promise<any> }} Store
 */

/** @param {{backend: string, server?: string}} where @param {"req" | "spec"} kind @returns {Store} */
function store(where, kind) {
  if (where.backend === "mcp") return remoteStore(String(where.server), kind);
  return kind === "req" ? localReqs : localSpecs;
}

/**
 * callTool gives an MCP tool's structured result; for a tool that has none, its text, which may
 * be fenced as external content.
 * @param {unknown} answer
 */
function toolValue(answer) {
  if (typeof answer !== "string") return answer;
  const fenced = /^<external-content[^>]*>\n([\s\S]*?)\n<\/external-content>/.exec(answer);
  try {
    return JSON.parse(fenced ? fenced[1] : answer);
  } catch {
    return undefined;
  }
}

/** @param {string} server @param {"req" | "spec"} kind @returns {Store} */
function remoteStore(server, kind) {
  const label = `${kind} backend \`${server}\``;
  const call = async (tool, payload) => {
    let answer;
    try {
      answer = await callTool(`mcp__${server}__items_${tool}`, { kind, ...payload });
    } catch (e) {
      throw new Error(`${label} (items_${tool}): ${e.message}`);
    }
    const value = toolValue(answer);
    if (typeof value !== "object" || value === null || Array.isArray(value)) {
      throw new Error(`${label} (items_${tool}) answered with no JSON object: ${String(answer).slice(0, 200)}`);
    }
    return /** @type {any} */ (value);
  };
  const valid = (item) => {
    check(kind === "req" ? SCHEMA.req : SCHEMA.entry, item, `${kind} ${item && item.id} from \`${server}\``);
    return item;
  };
  return {
    async all() {
      const items = [];
      const cursors = new Set();
      let cursor;
      do {
        const page = await call("query", cursor ? { cursor } : {});
        if (!Array.isArray(page.items)) throw new Error(`${label} (items_query) answered without an items list`);
        items.push(...page.items);
        cursor = page.next_cursor;
        if (cursor && cursors.has(cursor)) throw new Error(`${label} (items_query) gave the cursor ${cursor} twice`);
        cursors.add(cursor);
      } while (cursor);
      return items.map(valid);
    },
    async get(id) {
      need(id, kind === "req" ? REQ_ID : SPEC_ID, kind === "req" ? "a requirement id (R-001)" : "a spec entry id (S-report-001)");
      const item = valid((await call("get", { id })).item);
      if (item.id !== id) throw new Error(`${label} answered ${item.id} when asked for ${id}`);
      return item;
    },
    async put(item) {
      return valid((await call("put", { item })).item);
    },
  };
}

const reqFiles = async () => (await list("reqs/*.md")).filter((f) => new RegExp(REQ_ID).test(agix.path.basename(f, ".md")));

/** @type {Store} */
const localReqs = {
  async all() {
    const found = await reqFiles();
    return Promise.all(found.map((f) => localReqs.get(agix.path.basename(f, ".md"))));
  },
  async get(id) {
    const rel = `reqs/${need(id, REQ_ID, "a requirement id (R-001)")}.md`;
    const doc = await readDoc(rel, SCHEMA.req);
    if (!doc) throw new Error(`no requirement ${id}`);
    if (doc.data.id !== id) throw new Error(`forge/${rel}: its id is ${doc.data.id}, the file name says ${id}`);
    return { specs: [], changes: [], ...doc.data, body: doc.body };
  },
  async put(req) {
    if (!req.id) {
      const taken = (await reqFiles()).map((f) => agix.path.basename(f, ".md"));
      req = { ...req, id: `R-${pad(nextNumber(taken, "R-"))}` };
    }
    const { body, ...data } = req;
    await writeDoc(`reqs/${req.id}.md`, SCHEMA.req, data, body);
    return req;
  },
};

/** @type {Store} */
const localSpecs = {
  async all() {
    const found = (await list("specs/*.md")).map((f) => agix.path.basename(f, ".md"));
    const docs = await Promise.all(found.filter((c) => new RegExp(CAPABILITY).test(c)).map(specFile));
    return docs.flatMap((d) => d.entries);
  },
  async get(id) {
    const found = (await specFile(capabilityOf(need(id, SPEC_ID, "a spec entry id (S-report-001)")))).entries.find((e) => e.id === id);
    if (!found) throw new Error(`no spec entry ${id}`);
    return found;
  },
  async put(entry) {
    const file = await specFile(entry.capability);
    const at = file.entries.findIndex((e) => e.id === entry.id);
    const entries = file.entries.filter((e) => e.id !== entry.id);
    if (!entry.removed) entries.splice(at < 0 ? entries.length : at, 0, entry);
    const body = [file.preamble, ...entries.map(renderEntry)].filter(Boolean).join("\n\n") + "\n";
    await writeDoc(`specs/${entry.capability}.md`, SCHEMA.specFile, { capability: entry.capability }, body);
    return entry;
  },
};

/** @param {string} capability */
async function specFile(capability) {
  const rel = `specs/${capability}.md`;
  const doc = await readDoc(rel, SCHEMA.specFile);
  if (!doc) return { preamble: "", entries: /** @type {Entry[]} */ ([]) };
  const where = `forge/${rel}`;
  if (doc.data.capability !== capability) throw new Error(`${where}: capability is ${doc.data.capability}, the file name says ${capability}`);
  const parsed = parseEntries(doc.body, where);
  const stray = parsed.entries.find((e) => e.capability !== capability);
  if (stray) throw new Error(`${where}: ${stray.id} belongs in specs/${stray.capability}.md`);
  return parsed;
}

const reqsOf = async () => store((await config()).reqs, "req");
const specsOf = async () => store((await config()).specs, "spec");
/** @param {Store} specs */
const liveEntries = async (specs) => (await specs.all()).filter((e) => !e.removed);

const ENTRY_HEAD = /^## (S-[a-z0-9][a-z0-9-]*-[0-9]{3,}) (.+)$/;
const CRITERION = /^- \[(A[0-9]+)\] (.+)$/;
const CRITERIA_HEAD = "### 验收";

/** @param {string} id */
const capabilityOf = (id) => id.slice(2, id.lastIndexOf("-"));

/** @param {string} body @param {string} where */
function parseEntries(body, where) {
  const preamble = [];
  /** @type {(Entry & {lines: string[]})[]} */
  const entries = [];
  let current = null;
  let section = "body";
  for (const line of body.split("\n")) {
    const head = ENTRY_HEAD.exec(line);
    if (head) {
      current = { id: head[1], capability: capabilityOf(head[1]), title: head[2].trim(), reqs: [], body: "", criteria: [], lines: [] };
      entries.push(current);
      section = "reqs";
      continue;
    }
    if (!current) {
      preamble.push(line);
      continue;
    }
    if (section === "reqs") {
      section = "body";
      const reqs = /^reqs:(.*)$/.exec(line);
      if (reqs) {
        current.reqs = reqs[1].split(/[\s,]+/).filter(Boolean);
        continue;
      }
    }
    if (line.trim() === CRITERIA_HEAD) {
      section = "criteria";
      continue;
    }
    if (section === "criteria") {
      const c = CRITERION.exec(line);
      if (c) current.criteria.push({ id: c[1], text: c[2].trim() });
      else if (line.trim()) throw new Error(`${where}: under ${current.id}'s ${CRITERIA_HEAD}, "${line}" is not "- [A<n>] text"`);
      continue;
    }
    current.lines.push(line);
  }
  const seen = new Set();
  const parsed = entries.map(({ lines, ...e }) => {
    if (seen.has(e.id)) throw new Error(`${where}: ${e.id} appears twice`);
    seen.add(e.id);
    const entry = { ...e, body: lines.join("\n").trim() };
    check(SCHEMA.entry, entry, `${where} ${e.id}`);
    return entry;
  });
  return { preamble: preamble.join("\n").trim(), entries: parsed };
}

/** @param {Entry} e */
function renderEntry(e) {
  const criteria = e.criteria.map((c) => `- [${c.id}] ${c.text}`).join("\n");
  return `## ${e.id} ${e.title}\nreqs: ${e.reqs.join(", ")}\n\n${e.body ? e.body + "\n\n" : ""}${CRITERIA_HEAD}\n${criteria}`;
}

/** What a delta records of the entry it modifies or removes, to see at archive whether it changed since. @param {Entry} e */
const fingerprint = (e) => agix.hash.sha256(renderEntry(e));

/** @param {Entry} e */
const sameFields = (e) => JSON.stringify([e.id, e.capability, e.title, e.reqs, e.body, e.criteria.map((c) => [c.id, c.text])]);

/**
 * An entry is kept as Markdown, so one whose text would read back as something else (a body line
 * that looks like an entry heading or the criteria heading) is refused before it is written.
 * @param {Entry} e
 */
function storable(e) {
  check(SCHEMA.entry, e, e.id);
  let back = [];
  try {
    back = parseEntries(renderEntry(e), e.id).entries;
  } catch {
    back = [];
  }
  if (back.length !== 1 || sameFields(back[0]) !== sameFields(e)) {
    throw new Error(
      `${e.id} would not read back as written: a body line that starts with "## S-" or reads "${CRITERIA_HEAD}" ` +
        "is taken for an entry heading or the criteria; reword those lines (put criteria in `criteria`).",
    );
  }
  return e;
}

/** @param {string} t */
const oneLine = (t) => t.replace(/\s+/g, " ").trim();
/** @param {string[]} texts */
const numbered = (texts) => texts.map((t, i) => ({ id: `A${i + 1}`, text: oneLine(t) }));

/** @param {Req} r */
const shortReq = (r) => ({ id: r.id, title: r.title, status: r.status, priority: r.priority });

/** @param {any} input */
async function reqAdd(input) {
  check(input.items ? INPUT.reqAddMany : INPUT.reqAdd, input, "input");
  const reqs = await reqsOf();
  const items = input.items || [input];
  const added = [];
  for (const item of items) {
    const req = await reqs.put({ title: item.title, status: "proposed", priority: item.priority || "medium", specs: [], changes: [], body: item.body });
    added.push(shortReq(req));
  }
  return { added };
}

/** @param {string[]} rest @param {any} input */
async function reqUpdate(rest, input) {
  const { status, priority, title, ids: [id] } = args(rest, ["status", "priority", "title"], 1);
  const reqs = await reqsOf();
  const req = { ...(await reqs.get(id)) };
  if (status !== undefined) req.status = status;
  if (priority !== undefined) req.priority = priority;
  if (title !== undefined) req.title = title;
  if (typeof input.body === "string") req.body = input.body;
  check(SCHEMA.req, req, id);
  return { updated: shortReq(await reqs.put(req)) };
}

/** When a change was archived: the time it entered `done`. @param {any} change */
const archivedAt = (change) => (change.history.findLast((h) => h.stage === "done") || { at: "" }).at;

async function specCoverage() {
  const reqs = await (await reqsOf()).all();
  const entries = await liveEntries(await specsOf());
  const archived = (await allChanges()).filter((c) => c.where === "archive").map((c) => c.change);
  archived.sort((a, b) => archivedAt(a).localeCompare(archivedAt(b)) || a.id.localeCompare(b.id));
  const cited = new Set(entries.flatMap((e) => e.reqs));
  const passed = new Set();
  for (const c of archived) {
    for (const v of c.verification) {
      const key = `${v.spec} ${v.criterion}`;
      if (v.pass) passed.add(key);
      else passed.delete(key);
    }
  }
  return {
    accepted_without_specs: reqs.filter((r) => r.status === "accepted" && !cited.has(r.id) && !(r.specs || []).length).map(shortReq),
    entries_without_reqs: entries.filter((e) => !e.reqs.length).map((e) => e.id),
    criteria_not_passed: entries.flatMap((e) => e.criteria.filter((c) => !passed.has(`${e.id} ${c.id}`)).map((c) => `${e.id} ${c.id}`)),
  };
}

/**
 * An open change, or with `archived` also one under archive/.
 * @param {string} id
 */
async function loadChange(id, archived = false) {
  need(id, CHANGE_ID, "a change id (C-001)");
  const open = await readDoc(`changes/${id}/change.md`, { type: "object" });
  if (open && !open.data.archived) {
    check(SCHEMA.change, open.data, `forge/changes/${id}/change.md`);
    return { change: open.data, body: open.body, where: "changes" };
  }
  const done = await readDoc(`archive/${id}/change.md`, SCHEMA.change);
  if (done && archived) return { change: done.data, body: done.body, where: "archive" };
  if (done) throw new Error(`${id} is archived (forge/archive/${id}); it no longer changes`);
  throw new Error(`no change ${id}; forge flow list shows them`);
}

/** @param {any} change @param {string} body */
const saveChange = (change, body) => writeDoc(`changes/${change.id}/change.md`, SCHEMA.change, change, body);

/** Every change, open (`where: "changes"`) or archived (`where: "archive"`). */
async function allChanges() {
  const found = [...(await list("changes/*/change.md")), ...(await list("archive/*/change.md"))];
  const docs = await Promise.all(found.map((f) => readDoc(f, { type: "object" })));
  const changes = [];
  for (const [i, f] of found.entries()) {
    const data = docs[i] && docs[i].data;
    if (!data || data.archived) continue;
    check(SCHEMA.change, data, `forge/${f}`);
    changes.push({ change: data, where: f.split("/")[0] });
  }
  return changes;
}

/** @param {string} id */
async function loadDelta(id, where = "changes") {
  const doc = await readDoc(`${where}/${id}/delta.md`, SCHEMA.delta);
  if (!doc) return { add: [], modify: [], remove: [], base: {} };
  const { entries } = parseEntries(doc.body, `forge/${where}/${id}/delta.md`);
  const pick = (sids) => sids.map((sid) => {
    const e = entries.find((x) => x.id === sid);
    if (!e) throw new Error(`forge/${where}/${id}/delta.md lists ${sid} but has no entry for it`);
    return e;
  });
  return { add: pick(doc.data.add), modify: pick(doc.data.modify), remove: doc.data.remove, base: doc.data.base };
}

/**
 * Spec ids no new entry may take: every entry the backend has (removed ones too) and every id
 * any other change's delta names, open or archived, so a removed id is never handed out again.
 * @param {string} own @param {Store} specs
 */
async function takenSpecIds(own, specs) {
  const taken = (await specs.all()).map((e) => e.id);
  for (const { change, where } of await allChanges()) {
    if (change.id === own) continue;
    const delta = await readDoc(`${where}/${change.id}/delta.md`, SCHEMA.delta);
    if (delta) taken.push(...delta.data.add, ...delta.data.modify, ...delta.data.remove);
  }
  return taken;
}

/** @param {any} change @param {{flow: {auto: boolean, stopBefore: string[]}}} cfg */
function nextStep(change, cfg) {
  const { id, stage } = change;
  if (stage === "done") return { change: id, stage, skill: null, continue: false, say: `${id} is done and archived.` };
  const skill = `forge-${stage}`;
  const go = cfg.flow.auto && !cfg.flow.stopBefore.includes(stage);
  let say = `Stop here and tell the user ${id} is at the ${stage} stage; /forge-next ${id} continues.`;
  if (go) say = `Auto mode: go on now — call the Skill tool with skill "${skill}" and args "${id}".`;
  else if (cfg.flow.auto) say = `Auto mode stops before ${stage}. Stop here and tell the user ${id} is ready for the ${stage} stage and waits for them (/forge-next ${id}).`;
  return { change: id, stage, skill, auto: cfg.flow.auto, continue: go, say };
}

/** @param {any} change @param {string} stage */
function enter(change, stage) {
  change.stage = stage;
  change.history.push({ stage, at: now() });
}

/** @param {any} input */
async function flowNew(input) {
  check(INPUT.flowNew, input, "input");
  const reqs = await reqsOf();
  const linked = await Promise.all((input.reqs || []).map((r) => reqs.get(r)));
  const taken = [...(await list("changes/*/change.md")), ...(await list("archive/*/change.md"))].map((f) => f.split("/")[1]);
  const id = `C-${pad(nextNumber(taken, "C-"))}`;
  const change = { id, title: input.title, stage: "propose", reqs: linked.map((r) => r.id), history: [{ stage: "propose", at: now() }], plan: [], verification: [] };
  await saveChange(change, input.body);
  for (const req of linked) {
    await reqs.put({ ...req, status: req.status === "proposed" ? "accepted" : req.status, changes: [...new Set([...(req.changes || []), id])] });
  }
  return { id, stage: "propose", reqs: change.reqs };
}

/** @param {string} id */
async function flowShow(id) {
  const { change, body, where } = await loadChange(id, true);
  const delta = await loadDelta(id, where);
  const design = await readText(`${where}/${id}/design.md`);
  const result = (spec, criterion) => change.verification.find((v) => v.spec === spec && v.criterion === criterion);
  const withResults = (e) => ({ ...e, criteria: e.criteria.map((c) => ({ ...c, verified: result(e.id, c.id) || null })) });
  return {
    ...change,
    body,
    delta: { add: delta.add.map(withResults), modify: delta.modify.map(withResults), remove: delta.remove },
    design,
  };
}

/** @param {string} id @param {any} input */
async function flowDelta(id, input) {
  check(INPUT.delta, input, "input");
  const modifyIds = (input.modify || []).map((m) => m.id);
  const removeIds = input.remove || [];
  const twice = repeated([...modifyIds, ...removeIds]);
  if (twice.length) throw new Error(`a delta modifies or removes an entry once; listed more than once: ${twice.join(", ")}`);
  if (!(input.add || []).length && !modifyIds.length && !removeIds.length) throw new Error("the delta is empty: give add, modify or remove");
  const { change, body } = await loadChange(id);
  if (change.stage === "propose") throw new Error(`${id} is at propose; the delta is written from the spec stage on (forge flow advance ${id})`);

  const specs = await specsOf();
  const current = await liveEntries(specs);
  const taken = await takenSpecIds(id, specs);
  /** @param {string} sid @returns {Entry} */
  const byId = (sid) => {
    const e = current.find((x) => x.id === sid);
    if (!e) throw new Error(`no spec entry ${sid} to modify or remove; forge spec list shows them`);
    return e;
  };

  const add = (input.add || []).map((a) => {
    const prefix = `S-${a.capability}-`;
    const sid = `${prefix}${pad(nextNumber(taken, prefix))}`;
    taken.push(sid);
    return storable({ id: sid, capability: a.capability, title: oneLine(a.title), reqs: a.reqs || change.reqs, body: a.body.trim(), criteria: numbered(a.criteria) });
  });
  /** @type {Record<string, string>} */
  const base = {};
  const modify = (input.modify || []).map((m) => {
    const old = byId(m.id);
    base[old.id] = fingerprint(old);
    return storable({
      id: old.id,
      capability: old.capability,
      title: oneLine(m.title ?? old.title),
      reqs: m.reqs ?? old.reqs,
      body: (m.body ?? old.body).trim(),
      criteria: m.criteria ? numbered(m.criteria) : old.criteria.map((c) => ({ id: c.id, text: oneLine(c.text) })),
    });
  });
  for (const sid of removeIds) base[sid] = fingerprint(byId(sid));

  const before = await loadDelta(id);
  const written = new Map([...before.add, ...before.modify].map((e) => [e.id, renderEntry(e)]));
  const unchanged = new Set([...add, ...modify].filter((e) => written.get(e.id) === renderEntry(e)).map((e) => e.id));
  const stale = [...new Set(change.verification.map((v) => v.spec).filter((spec) => !unchanged.has(spec)))];

  const data = { change: id, add: add.map((e) => e.id), modify: modify.map((e) => e.id), remove: removeIds, base };
  await writeDoc(`changes/${id}/delta.md`, SCHEMA.delta, data, [...add, ...modify].map(renderEntry).join("\n\n") + "\n");
  const result = { change: id, stage: change.stage, add: data.add, modify: data.modify, remove: removeIds, criteria: [...add, ...modify].map((e) => ({ spec: e.id, criteria: e.criteria.map((c) => c.id) })) };
  if (!stale.length) return result;

  change.verification = change.verification.filter((v) => unchanged.has(v.spec));
  if (change.stage === "archive") enter(change, "verify");
  await saveChange(change, body);
  const back = change.stage === "verify" ? `; ${id} is at verify` : "";
  return { ...result, stage: change.stage, say: `${stale.join(", ")} changed, so their verification results were dropped${back}. Verify them again.` };
}

/** @param {string} id @param {any} input */
async function flowPlan(id, input) {
  check(INPUT.plan, input, "input");
  const { change, body } = await loadChange(id);
  const before = new Map(change.plan.map((p) => [p.text, p.done]));
  change.plan = input.items.map((t, i) => ({ id: `P${i + 1}`, text: t.trim(), done: before.get(t.trim()) === true }));
  await saveChange(change, body);
  if (typeof input.design === "string") await writeText(`changes/${id}/design.md`, input.design);
  return { change: id, plan: change.plan };
}

/** @param {string} id @param {string[]} items */
async function flowCheck(id, items) {
  const { change, body } = await loadChange(id);
  for (const item of items) {
    const p = change.plan.find((x) => x.id === item);
    if (!p) throw new Error(`${id} has no plan item ${item}; its plan is ${change.plan.map((x) => x.id).join(", ") || "empty"}`);
    p.done = true;
  }
  await saveChange(change, body);
  const open = change.plan.filter((x) => !x.done).map((x) => x.id);
  return { change: id, checked: items, open };
}

/** @param {string} id @param {any} input */
async function flowVerify(id, input) {
  check(INPUT.verify, input, "input");
  const keys = input.results.map((r) => `${r.spec} ${r.criterion}`);
  const twice = repeated(keys);
  if (twice.length) throw new Error(`one result per criterion; given more than once: ${twice.join(", ")}`);
  const { change, body } = await loadChange(id);
  if (change.stage !== "verify") throw new Error(`${id} is at ${change.stage}; results are recorded at the verify stage`);
  const delta = await loadDelta(id);
  const criteria = [...delta.add, ...delta.modify].flatMap((e) => e.criteria.map((c) => `${e.id} ${c.id}`));
  for (const key of keys) {
    if (!criteria.includes(key)) throw new Error(`${key} is not a criterion of ${id}; they are: ${criteria.join(", ")}`);
  }
  const at = now();
  const kept = change.verification.filter((v) => !keys.includes(`${v.spec} ${v.criterion}`));
  change.verification = [...kept, ...input.results.map((r) => ({ ...r, at }))];
  const latest = (key) => change.verification.find((v) => `${v.spec} ${v.criterion}` === key);
  const failed = criteria.filter((k) => latest(k) && !latest(k).pass);
  const missing = criteria.filter((k) => !latest(k));
  let say = `Every criterion passed: forge flow advance ${id}.`;
  if (missing.length) say = `Still unverified: ${missing.join(", ")}.`;
  if (failed.length) {
    enter(change, "apply");
    say = `${failed.length} criteria failed (${failed.join(", ")}); ${id} is back at apply. Fix them, then advance to verify again.`;
  }
  await saveChange(change, body);
  return { change: id, stage: change.stage, failed, missing, say };
}

/** @param {string} id */
async function flowAdvance(id) {
  const { change, body } = await loadChange(id);
  const from = change.stage;
  const refuse = (why) => {
    throw new Error(`${id} stays at ${from}: ${why}`);
  };
  if (from === "archive") refuse(`run forge flow archive ${id}`);
  if (from === "done") refuse("it is done");
  if (from === "spec") {
    const d = await loadDelta(id);
    if (!d.add.length && !d.modify.length && !d.remove.length) refuse(`no spec delta yet (forge flow delta ${id})`);
  }
  if (from === "plan" && !change.plan.length) refuse(`no plan yet (forge flow plan ${id})`);
  if (from === "apply") {
    const open = change.plan.filter((p) => !p.done).map((p) => p.id);
    if (open.length) refuse(`plan items not checked: ${open.join(", ")} (forge flow check ${id} <P#>)`);
  }
  if (from === "verify") {
    const d = await loadDelta(id);
    const keys = [...d.add, ...d.modify].flatMap((e) => e.criteria.map((c) => `${e.id} ${c.id}`));
    const ok = (k) => change.verification.some((v) => `${v.spec} ${v.criterion}` === k && v.pass);
    const left = keys.filter((k) => !ok(k));
    if (left.length) refuse(`criteria without a passing record: ${left.join(", ")} (forge flow verify ${id})`);
  }
  enter(change, STAGES[STAGES.indexOf(from) + 1]);
  await saveChange(change, body);
  return { change: id, from, ...nextStep(change, await config()) };
}

/**
 * What archiving still has to write. Every entry the delta modifies or removes must still be the
 * one the delta was written against; one already written as the delta has it was merged by an
 * earlier archive of this change that stopped part way.
 * @param {string} id @param {Store} specs @param {Awaited<ReturnType<typeof loadDelta>>} delta
 */
async function stillToMerge(id, specs, delta) {
  const live = new Map((await liveEntries(specs)).map((e) => [e.id, e]));
  const put = [...delta.add];
  const remove = [];
  const stale = [];
  for (const e of delta.modify) {
    const current = live.get(e.id);
    const seen = current && fingerprint(current);
    if (seen === fingerprint(e)) continue;
    if (seen === delta.base[e.id]) put.push(e);
    else stale.push(e.id);
  }
  for (const sid of delta.remove) {
    const current = live.get(sid);
    if (!current) continue;
    if (fingerprint(current) === delta.base[sid]) remove.push(current);
    else stale.push(sid);
  }
  if (stale.length) {
    throw new Error(
      `${id} stays at archive: ${stale.join(", ")} changed after its delta was written (another change was archived). ` +
        `Read the current entries (forge spec get <S-id>) and send the delta again (forge flow delta ${id}); ` +
        "entries whose text changes go back to verify.",
    );
  }
  return { put, remove };
}

/** @param {string} id */
async function flowArchive(id) {
  const { change, body } = await loadChange(id);
  if (change.stage !== "archive") throw new Error(`${id} is at ${change.stage}; it is archived after verify (forge flow advance ${id})`);
  const delta = await loadDelta(id);
  const specs = await specsOf();
  const reqs = await reqsOf();
  const merge = await stillToMerge(id, specs, delta);
  for (const e of merge.put) await specs.put(e);
  for (const e of merge.remove) await specs.put({ ...e, removed: true });

  const merged = [...delta.add, ...delta.modify];
  for (const rid of change.reqs) {
    const req = await reqs.get(rid);
    const cited = merged.filter((e) => e.reqs.includes(rid)).map((e) => e.id);
    await reqs.put({ ...req, status: "done", specs: [...new Set([...(req.specs || []), ...cited])], changes: [...new Set([...(req.changes || []), id])] });
  }

  enter(change, "done");
  const extras = ["delta.md", "design.md"];
  const copies = [];
  for (const f of extras) copies.push(await readText(`changes/${id}/${f}`));
  for (const [i, f] of extras.entries()) {
    if (copies[i] !== null) await writeText(`archive/${id}/${f}`, copies[i]);
  }
  await writeDoc(`archive/${id}/change.md`, SCHEMA.change, change, body);
  const moved = `Moved to forge/archive/${id}/. forge cannot delete files; remove this directory with git rm -r.\n`;
  await writeDoc(`changes/${id}/change.md`, SCHEMA.archived, { id, archived: true }, moved);
  for (const [i, f] of extras.entries()) {
    if (copies[i] !== null) await writeText(`changes/${id}/${f}`, moved);
  }
  return { change: id, stage: "done", specs: { added: delta.add.map((e) => e.id), modified: delta.modify.map((e) => e.id), removed: delta.remove }, reqs_done: change.reqs, archived_to: `forge/archive/${id}` };
}

/** @type {Record<string, Record<string, (rest: string[], input: any) => Promise<unknown>>>} */
const COMMANDS = {
  req: {
    async list(rest) {
      const { status, priority } = args(rest, ["status", "priority"], 0);
      const all = await (await reqsOf()).all();
      const items = all.filter((r) => (!status || r.status === status) && (!priority || r.priority === priority)).map(shortReq);
      return { items };
    },
    async get(rest) {
      const [id] = args(rest, [], 1).ids;
      return (await reqsOf()).get(id);
    },
    async add(rest, input) {
      args(rest, [], 0);
      return reqAdd(input);
    },
    update: reqUpdate,
  },
  spec: {
    async list(rest) {
      const { capability } = args(rest, ["capability"], 0);
      const entries = await liveEntries(await specsOf());
      const items = entries.filter((e) => !capability || e.capability === capability).map((e) => ({ id: e.id, title: e.title, reqs: e.reqs, criteria: e.criteria.length }));
      return { items };
    },
    async get(rest) {
      const [id] = args(rest, [], 1).ids;
      const entry = await (await specsOf()).get(id);
      if (entry.removed) throw new Error(`${entry.id} was removed`);
      return entry;
    },
    async coverage(rest) {
      args(rest, [], 0);
      return specCoverage();
    },
  },
  flow: {
    async new(rest, input) {
      args(rest, [], 0);
      return flowNew(input);
    },
    async list(rest) {
      args(rest, [], 0);
      return { items: (await allChanges()).map(({ change }) => ({ id: change.id, title: change.title, stage: change.stage })) };
    },
    async show(rest) {
      return flowShow(args(rest, [], 1).ids[0]);
    },
    async next(rest) {
      const { change } = await loadChange(args(rest, [], 1).ids[0], true);
      return nextStep(change, await config());
    },
    async delta(rest, input) {
      return flowDelta(args(rest, [], 1).ids[0], input);
    },
    async plan(rest, input) {
      return flowPlan(args(rest, [], 1).ids[0], input);
    },
    async check(rest) {
      const [id, ...items] = args(rest, [], -2).ids;
      return flowCheck(id, items);
    },
    async verify(rest, input) {
      return flowVerify(args(rest, [], 1).ids[0], input);
    },
    async advance(rest) {
      return flowAdvance(args(rest, [], 1).ids[0]);
    },
    async archive(rest) {
      return flowArchive(args(rest, [], 1).ids[0]);
    },
  },
};
