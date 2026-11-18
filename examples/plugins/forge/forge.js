// @ts-check
/// <reference path="types/runext.d.ts" />
/// <reference path="types/agix.d.ts" />
// forge 命令（docs/forge.md）：需求、规格与变更流程。模型经 RunExt 调，用户可以直接 /forge …。
// 依次是：frontmatter 的 schema、命令分发与用法、forge/ 下的文件、需求与规格的两种后端
// （本地 Markdown、经 callTool 调的 MCP 服务）、规格条目的解析与改写、变更流程。

const DIR = "forge";
const STAGES = ["propose", "spec", "plan", "apply", "verify", "archive", "done"];
const REQ_ID = "^R-[0-9]{3,}$";
const SPEC_ID = "^S-[a-z0-9][a-z0-9-]*-[0-9]{3,}$";
const CHANGE_ID = "^C-[0-9]{3,}$";
const CAPABILITY = "^[a-z0-9][a-z0-9-]*$";

const str = (pattern) => (pattern ? { type: "string", pattern } : { type: "string" });
const text = { type: "string", minLength: 1 };
const arrayOf = (items, minItems = 0) => ({ type: "array", items, minItems });
const ids = (pattern) => arrayOf(str(pattern));
/** @param {object} properties @param {string[]} required */
const closed = (properties, required = []) => ({ type: "object", properties, required, additionalProperties: false });
const loose = (properties, required = []) => ({ type: "object", properties, required });

const PRIORITY = { enum: ["high", "medium", "low"] };
const backend = {
  ...closed({ backend: { enum: ["local", "mcp"] }, server: text }, ["backend"]),
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
  delta: closed({ change: str(CHANGE_ID), add: ids(SPEC_ID), modify: ids(SPEC_ID), remove: ids(SPEC_ID) }, ["change", "add", "modify", "remove"]),
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

const help = (group) =>
  group && USAGE[group]
    ? USAGE[group]
    : `forge keeps this project's requirements, specs and changes under forge/.\n\n${Object.values(USAGE).join("\n\n")}\n\nforge help <req|spec|flow> for one group.`;

/** @type {string} */
let ROOT = "";

/** @param {string[]} argv @param {any} input @param {RunExtContext} ctx */
async function main(argv, input, ctx) {
  ROOT = ctx.projectRoot;
  const [group, verb, ...rest] = argv;
  if (!group || group === "help") return help(verb);
  if (argv.includes("--help") || argv.includes("-h")) return help(group);
  const handler = COMMANDS[group] && COMMANDS[group][verb];
  if (!handler) throw new Error(`unknown command: forge ${argv.join(" ")}\n\n${help(group)}`);
  try {
    return await handler(rest, inputObject(input));
  } catch (e) {
    if (e && e.name === "AgixError" && String(e.code).startsWith("cli.")) {
      throw new Error(`${e.message}\n\n${USAGE[group]}`);
    }
    throw e;
  }
}

/** `input` as an object; models often send it as JSON or YAML text. @param {unknown} input */
function inputObject(input) {
  if (input === null || input === undefined) return {};
  let value = input;
  if (typeof input === "string") {
    try {
      value = agix.yaml.parse(input);
    } catch (e) {
      throw new Error(`input must be an object (JSON); the text given does not parse: ${e.message}`);
    }
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("input must be an object, such as {\"title\": \"…\", \"body\": \"…\"}");
  }
  return value;
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
  const many = positionals < 0 && parsed.positionals.length >= -positionals;
  if (!many && parsed.positionals.length !== positionals) {
    const want = positionals < 0 ? `at least ${-positionals}` : String(positionals);
    throw new Error(`expected ${want} argument${positionals === 1 ? "" : "s"}, got ${parsed.positionals.length}`);
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

/**
 * The files under forge/, relative to it, from one Glob per run. Reads go only to files listed
 * here: a Read of a path whose directory does not exist yet can be judged outside the project.
 */
async function files() {
  if (!existing) {
    const found = await tools.Glob({ pattern: `${DIR}/**`, path: ROOT, respect_gitignore: false, max_results: 100000 });
    const marker = `${DIR}/`;
    existing = new Set(found.paths.map((p) => p.slice(p.lastIndexOf(marker) + marker.length)));
  }
  return existing;
}

/** @param {string} rel @returns {Promise<string | null>} */
async function readText(rel) {
  if (!(await files()).has(rel)) return null;
  const read = await tools.Read({ file_path: abs(rel) });
  if (read.truncated) throw new Error(`forge/${rel} is longer than one read returns`);
  return read.content;
}

/** @param {string} rel @param {string} content */
async function writeText(rel, content) {
  await tools.Write({ file_path: abs(rel), content });
  (await files()).add(rel);
}

/** Files under forge/ matching `pattern`, relative to forge/. @param {string} pattern */
async function list(pattern) {
  return agix.glob.filter(pattern, [...(await files())]).sort();
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

/** @param {string} server @param {"req" | "spec"} kind @returns {Store} */
function remoteStore(server, kind) {
  const call = async (tool, payload) => {
    let answer;
    try {
      answer = await callTool(`mcp__${server}__items_${tool}`, { kind, ...payload });
    } catch (e) {
      throw new Error(`${kind} backend \`${server}\` (items_${tool}): ${e.message}`);
    }
    // MCP results reach code as their text today; the JSON is in it, maybe fenced.
    if (typeof answer !== "string") return answer;
    const fenced = /^<external-content[^>]*>\n([\s\S]*?)\n<\/external-content>/.exec(answer);
    const json = fenced ? fenced[1] : answer;
    try {
      return JSON.parse(json);
    } catch {
      throw new Error(`${kind} backend \`${server}\` (items_${tool}) answered with no JSON: ${answer.slice(0, 200)}`);
    }
  };
  const valid = (item) => {
    check(kind === "req" ? SCHEMA.req : SCHEMA.entry, item, `${kind} ${item && item.id} from \`${server}\``);
    return item;
  };
  return {
    async all() {
      const items = [];
      let cursor;
      do {
        const page = await call("query", cursor ? { cursor } : {});
        items.push(...page.items);
        cursor = page.next_cursor;
      } while (cursor);
      return items.map(valid);
    },
    async get(id) {
      return valid((await call("get", { id })).item);
    },
    async put(item) {
      return valid((await call("put", { item })).item);
    },
  };
}

/** @type {Store} */
const localReqs = {
  async all() {
    const files = await list("reqs/*.md");
    return Promise.all(files.map((f) => localReqs.get(agix.path.basename(f, ".md"))));
  },
  async get(id) {
    const doc = await readDoc(`reqs/${need(id, REQ_ID, "a requirement id (R-001)")}.md`, SCHEMA.req);
    if (!doc) throw new Error(`no requirement ${id}`);
    return { specs: [], changes: [], ...doc.data, body: doc.body };
  },
  async put(req) {
    if (!req.id) {
      const taken = (await list("reqs/*.md")).map((f) => agix.path.basename(f, ".md"));
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
    const files = await list("specs/*.md");
    const docs = await Promise.all(files.map((f) => specFile(agix.path.basename(f, ".md"))));
    return docs.flatMap((d) => d.entries);
  },
  async get(id) {
    const found = (await specFile(capabilityOf(need(id, SPEC_ID, "a spec entry id (S-report-001)")))).entries.find((e) => e.id === id);
    if (!found) throw new Error(`no spec entry ${id}`);
    return found;
  },
  async put(entry) {
    const file = await specFile(entry.capability);
    const rest = file.entries.filter((e) => e.id !== entry.id);
    const entries = entry.removed ? rest : file.entries.some((e) => e.id === entry.id)
      ? file.entries.map((e) => (e.id === entry.id ? entry : e))
      : [...rest, entry];
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
  return parseEntries(doc.body, `forge/${rel}`);
}

const reqsOf = async () => store((await config()).reqs, "req");
const specsOf = async () => store((await config()).specs, "spec");
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
  return { updated: shortReq(await reqs.put(req)) };
}

async function coverage() {
  const [reqs, entries, changes] = await Promise.all([(await reqsOf()).all(), liveEntries(await specsOf()), allChanges()]);
  const cited = new Set(entries.flatMap((e) => e.reqs));
  const passed = new Set();
  for (const c of changes.sort((a, b) => a.id.localeCompare(b.id))) {
    for (const v of c.verification || []) {
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

async function allChanges() {
  const files = [...(await list("changes/*/change.md")), ...(await list("archive/*/change.md"))];
  const docs = await Promise.all(files.map((f) => readDoc(f, { type: "object" })));
  return files
    .map((f, i) => ({ f, data: docs[i] && docs[i].data }))
    .filter(({ data }) => data && !data.archived)
    .map(({ f, data }) => (check(SCHEMA.change, data, `forge/${f}`), data));
}

/** @param {string} id */
async function loadDelta(id, where = "changes") {
  const doc = await readDoc(`${where}/${id}/delta.md`, SCHEMA.delta);
  if (!doc) return { add: [], modify: [], remove: [], body: "" };
  const { entries } = parseEntries(doc.body, `forge/${where}/${id}/delta.md`);
  const pick = (list) => list.map((sid) => {
    const e = entries.find((x) => x.id === sid);
    if (!e) throw new Error(`forge/${where}/${id}/delta.md lists ${sid} but has no entry for it`);
    return e;
  });
  return { add: pick(doc.data.add), modify: pick(doc.data.modify), remove: doc.data.remove, body: doc.body };
}

/** @param {any} change @param {{flow: {auto: boolean, stopBefore: string[]}}} cfg */
function nextStep(change, cfg) {
  const { id, stage } = change;
  if (stage === "done") return { change: id, stage, skill: null, continue: false, say: `${id} is done and archived.` };
  const skill = `forge-${stage}`;
  const go = cfg.flow.auto && !cfg.flow.stopBefore.includes(stage);
  const say = go
    ? `Auto mode: go on now — call the Skill tool with skill "${skill}" and args "${id}".`
    : cfg.flow.auto
      ? `Auto mode stops before ${stage}. Stop here and tell the user ${id} is ready for the ${stage} stage and waits for them (/forge-next ${id}).`
      : `Stop here and tell the user ${id} is at the ${stage} stage; /forge-next ${id} continues.`;
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
  const { change } = await loadChange(id);
  if (change.stage === "propose" || change.stage === "archive") {
    throw new Error(`${id} is at ${change.stage}; the delta is written from the spec stage until verify`);
  }
  const specs = await specsOf();
  const current = await liveEntries(specs);
  const others = (await allChanges()).filter((c) => c.id !== id && c.stage !== "done");
  const pending = (await Promise.all(others.map((c) => loadDelta(c.id)))).flatMap((d) => d.add.map((e) => e.id));
  const taken = [...current.map((e) => e.id), ...pending];
  const byId = (sid) => {
    const e = current.find((x) => x.id === sid);
    if (!e) throw new Error(`no spec entry ${sid} to modify or remove; forge spec list shows them`);
    return e;
  };

  const add = (input.add || []).map((a) => {
    const prefix = `S-${a.capability}-`;
    const sid = `${prefix}${pad(nextNumber(taken, prefix))}`;
    taken.push(sid);
    return { id: sid, capability: a.capability, title: oneLine(a.title), reqs: a.reqs || change.reqs, body: a.body.trim(), criteria: numbered(a.criteria) };
  });
  const modify = (input.modify || []).map((m) => {
    const old = byId(m.id);
    return { ...old, title: oneLine(m.title || old.title), reqs: m.reqs || old.reqs, body: (m.body !== undefined ? m.body : old.body).trim(), criteria: m.criteria ? numbered(m.criteria) : old.criteria };
  });
  const remove = (input.remove || []).map((sid) => byId(sid).id);
  if (!add.length && !modify.length && !remove.length) throw new Error("the delta is empty: give add, modify or remove");

  const body = [...add, ...modify].map(renderEntry).join("\n\n") + "\n";
  const data = { change: id, add: add.map((e) => e.id), modify: modify.map((e) => e.id), remove };
  // Write refuses to replace a file this run has not read.
  await readText(`changes/${id}/delta.md`);
  await writeDoc(`changes/${id}/delta.md`, SCHEMA.delta, data, body);
  return { change: id, add: data.add, modify: data.modify, remove, criteria: [...add, ...modify].map((e) => ({ spec: e.id, criteria: e.criteria.map((c) => c.id) })) };
}

/** @param {string} id @param {any} input */
async function flowPlan(id, input) {
  check(INPUT.plan, input, "input");
  const { change, body } = await loadChange(id);
  const before = new Map(change.plan.map((p) => [p.text, p.done]));
  change.plan = input.items.map((t, i) => ({ id: `P${i + 1}`, text: t.trim(), done: before.get(t.trim()) === true }));
  await saveChange(change, body);
  if (typeof input.design === "string") {
    // Write refuses to replace a file this run has not read.
    await readText(`changes/${id}/design.md`);
    await writeText(`changes/${id}/design.md`, input.design);
  }
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
  const { change, body } = await loadChange(id);
  if (change.stage !== "verify") throw new Error(`${id} is at ${change.stage}; results are recorded at the verify stage`);
  const delta = await loadDelta(id);
  const criteria = [...delta.add, ...delta.modify].flatMap((e) => e.criteria.map((c) => `${e.id} ${c.id}`));
  for (const r of input.results) {
    if (!criteria.includes(`${r.spec} ${r.criterion}`)) throw new Error(`${r.spec} ${r.criterion} is not a criterion of ${id}; they are: ${criteria.join(", ")}`);
  }
  const at = now();
  const kept = change.verification.filter((v) => !input.results.some((r) => r.spec === v.spec && r.criterion === v.criterion));
  change.verification = [...kept, ...input.results.map((r) => ({ ...r, at }))];
  const latest = (key) => change.verification.find((v) => `${v.spec} ${v.criterion}` === key);
  const failed = criteria.filter((k) => latest(k) && !latest(k).pass);
  const missing = criteria.filter((k) => !latest(k));
  let say = missing.length ? `Still unverified: ${missing.join(", ")}.` : `Every criterion passed: forge flow advance ${id}.`;
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

/** @param {string} id */
async function flowArchive(id) {
  const { change, body } = await loadChange(id);
  if (change.stage !== "archive") throw new Error(`${id} is at ${change.stage}; it is archived after verify (forge flow advance ${id})`);
  const delta = await loadDelta(id);
  const [specs, reqs] = [await specsOf(), await reqsOf()];
  for (const e of [...delta.add, ...delta.modify]) await specs.put(e);
  for (const sid of delta.remove) await specs.put({ ...(await specs.get(sid)), removed: true });

  const done = [];
  for (const rid of change.reqs) {
    const req = await reqs.get(rid);
    const cited = [...delta.add, ...delta.modify].filter((e) => e.reqs.includes(rid)).map((e) => e.id);
    await reqs.put({ ...req, status: "done", specs: [...new Set([...(req.specs || []), ...cited])], changes: [...new Set([...(req.changes || []), id])] });
    done.push(rid);
  }

  enter(change, "done");
  const files = ["delta.md", "design.md"];
  const copies = await Promise.all(files.map((f) => readText(`changes/${id}/${f}`)));
  await writeDoc(`archive/${id}/change.md`, SCHEMA.change, change, body);
  const moved = `Moved to forge/archive/${id}/. forge cannot delete files; remove this directory with git rm -r.\n`;
  for (const [i, f] of files.entries()) {
    if (copies[i] === null) continue;
    await writeText(`archive/${id}/${f}`, copies[i]);
    await writeText(`changes/${id}/${f}`, moved);
  }
  await writeDoc(`changes/${id}/change.md`, SCHEMA.archived, { id, archived: true }, moved);
  return { change: id, stage: "done", specs: { added: delta.add.map((e) => e.id), modified: delta.modify.map((e) => e.id), removed: delta.remove }, reqs_done: done, archived_to: `forge/archive/${id}` };
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
      return (await reqsOf()).get(args(rest, [], 1).ids[0]);
    },
    add: (rest, input) => (args(rest, [], 0), reqAdd(input)),
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
      const entry = await (await specsOf()).get(args(rest, [], 1).ids[0]);
      if (entry.removed) throw new Error(`${entry.id} was removed`);
      return entry;
    },
    coverage: (rest) => (args(rest, [], 0), coverage()),
  },
  flow: {
    new: (rest, input) => (args(rest, [], 0), flowNew(input)),
    async list(rest) {
      args(rest, [], 0);
      return { items: (await allChanges()).map((c) => ({ id: c.id, title: c.title, stage: c.stage })) };
    },
    show: (rest) => flowShow(args(rest, [], 1).ids[0]),
    async next(rest) {
      const { change } = await loadChange(args(rest, [], 1).ids[0], true);
      return nextStep(change, await config());
    },
    delta: (rest, input) => flowDelta(args(rest, [], 1).ids[0], input),
    plan: (rest, input) => flowPlan(args(rest, [], 1).ids[0], input),
    check: (rest) => {
      const [id, ...items] = args(rest, [], -2).ids;
      return flowCheck(id, items);
    },
    verify: (rest, input) => flowVerify(args(rest, [], 1).ids[0], input),
    advance: (rest) => flowAdvance(args(rest, [], 1).ids[0]),
    archive: (rest) => flowArchive(args(rest, [], 1).ids[0]),
  },
};
