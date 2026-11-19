// The base library (docs/agix_js.md), a global named for the engine: the
// namespaces, argument checks and the parts that read better in JavaScript,
// over the Rust functions `native` carries. Compiled to bytecode once per
// process; the default export runs once per runtime.

const { parse: jsonParse, stringify: jsonStringify } = JSON;
const { freeze, isFrozen, defineProperty, getOwnPropertyNames, create } = Object;
const isArray = Array.isArray;

class AgixError extends Error {
  constructor(code, message, extra) {
    super(message);
    this.code = code;
    if (extra && typeof extra.line === "number") {
      this.line = extra.line;
      this.column = extra.column;
    }
  }
}

const fail = (code, message, extra) => {
  throw new AgixError(code, message, extra);
};

const lift = (e) =>
  e !== null && typeof e === "object" && e.__agix === true
    ? new AgixError(e.code, e.message, e)
    : e;

const guarded = (f) => (...args) => {
  try {
    return f(...args);
  } catch (e) {
    throw lift(e);
  }
};

const typeName = (v) => (v === null ? "null" : isArray(v) ? "array" : typeof v);

const needString = (what, v) => {
  if (typeof v !== "string") throw new TypeError(`${what} must be a string, got ${typeName(v)}`);
  return v;
};

const deepFreeze = (o) => {
  freeze(o);
  for (const name of getOwnPropertyNames(o)) {
    const v = o[name];
    if (((typeof v === "object" && v !== null) || typeof v === "function") && !isFrozen(v)) deepFreeze(v);
  }
  return o;
};

const unrepresentable = (what) =>
  fail("yaml.unrepresentable", `${what} has no YAML value; only JSON values can be written`);

const jsonText = (value, onBad) => {
  const stop = {};
  let bad;
  try {
    return jsonStringify(value, function (key, v) {
      const t = typeof v;
      if (v === undefined) bad = key === "" ? "undefined" : `undefined (at key ${jsonStringify(key)})`;
      else if (t === "function" || t === "symbol" || t === "bigint") bad = `a ${t}`;
      else if (t === "number" && !Number.isFinite(v)) bad = String(v);
      if (bad !== undefined) throw stop;
      return v;
    });
  } catch (e) {
    if (e !== stop) bad = `a value that cannot be written as JSON (${e && e.message})`;
  }
  return onBad(bad);
};

function splitFrontmatter(text) {
  const lineEnd = (from) => {
    const nl = text.indexOf("\n", from);
    return nl === -1 ? text.length : nl;
  };
  const isFence = (from, end) => {
    const line = text.slice(from, end);
    return line === "---" || line === "---\r";
  };
  const start = text.charCodeAt(0) === 0xfeff ? 1 : 0;
  let end = lineEnd(start);
  if (!isFence(start, end)) return null;
  if (end === text.length) return { unclosed: true };
  const yamlStart = end + 1;
  for (let at = yamlStart; at < text.length; at = end + 1) {
    end = lineEnd(at);
    if (isFence(at, end)) return { yaml: text.slice(yamlStart, at), body: text.slice(end + 1) };
  }
  return { unclosed: true };
}

function makeYaml(native) {
  const parse = guarded((text) => jsonParse(native.yamlParse(needString("text", text))));
  const stringify = guarded((value) => native.yamlStringify(jsonText(value, unrepresentable)));
  const parseFrontmatter = guarded((text) => {
    needString("text", text);
    const split = splitFrontmatter(text);
    if (split === null) return { data: null, body: text };
    if (split.unclosed) {
      fail("yaml.syntax", "the frontmatter opened on line 1 has no closing --- line", { line: 1, column: 1 });
    }
    let data;
    try {
      data = jsonParse(native.yamlParse(split.yaml));
    } catch (e) {
      const lifted = lift(e);
      if (lifted instanceof AgixError && lifted.line !== undefined) lifted.line += 1;
      throw lifted;
    }
    return { data: data === null ? {} : data, body: split.body };
  });
  const stringifyFrontmatter = guarded((data, body) => {
    needString("body", body);
    if (data === null) {
      if (splitFrontmatter(body) !== null) {
        fail(
          "yaml.unrepresentable",
          "the body starts with a --- line, which would read back as frontmatter; pass data to keep them apart",
        );
      }
      return body;
    }
    return "---\n" + native.yamlStringify(jsonText(data, unrepresentable)) + "---\n" + body;
  });
  return { parse, stringify, parseFrontmatter, stringifyFrontmatter };
}

function patterns(pattern) {
  if (typeof pattern === "string") return [pattern];
  if (isArray(pattern) && pattern.every((p) => typeof p === "string")) return pattern;
  throw new TypeError(`pattern must be a string or an array of strings, got ${typeName(pattern)}`);
}

const GLOB_SLICE = 1000;

function makeGlob(native) {
  const test = guarded((pattern, path) => native.globTest(patterns(pattern), needString("path", path)));
  const filter = guarded((pattern, paths) => {
    if (!isArray(paths) || !paths.every((p) => typeof p === "string")) {
      throw new TypeError(`paths must be an array of strings, got ${typeName(paths)}`);
    }
    // In slices, so a script time limit can stop a long list between them:
    // it does not reach into a native call.
    const list = patterns(pattern);
    const out = [];
    for (let at = 0; at < paths.length; at += GLOB_SLICE) {
      const slice = paths.slice(at, at + GLOB_SLICE);
      const hits = native.globMatches(list, slice);
      for (let i = 0; i < slice.length; i++) if (hits[i]) out.push(slice[i]);
    }
    return out;
  });
  return { test, filter };
}

function normalizeSegments(path, absolute) {
  const out = [];
  for (const seg of path.split("/")) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") {
      if (out.length > 0 && out[out.length - 1] !== "..") out.pop();
      else if (!absolute) out.push("..");
    } else {
      out.push(seg);
    }
  }
  return out;
}

function makePath() {
  const isAbsolute = (p) => needString("path", p).startsWith("/");
  const normalize = (p) => {
    needString("path", p);
    if (p === "") return ".";
    const absolute = p.startsWith("/");
    const trailing = p.endsWith("/");
    let out = normalizeSegments(p, absolute).join("/");
    if (out === "" && !absolute) out = ".";
    if (out !== "" && trailing) out += "/";
    return absolute ? "/" + out : out;
  };
  const join = (...parts) => {
    const kept = parts.map((p) => needString("path", p)).filter((p) => p !== "");
    return kept.length === 0 ? "." : normalize(kept.join("/"));
  };
  const relative = (from, to) => {
    needString("from", from);
    needString("to", to);
    if (isAbsolute(from) !== isAbsolute(to)) {
      fail("path.relative", "one path is absolute and the other relative; that answer needs a current directory. Make both absolute (ctx.projectRoot)");
    }
    const absolute = isAbsolute(from);
    const a = normalizeSegments(from, absolute);
    const b = normalizeSegments(to, absolute);
    if (!absolute && (a[0] === ".." || b[0] === "..")) {
      fail("path.relative", "a relative path climbs out with '..'; that answer needs a current directory. Make both absolute (ctx.projectRoot)");
    }
    let i = 0;
    while (i < a.length && i < b.length && a[i] === b[i]) i++;
    return [...a.slice(i).map(() => ".."), ...b.slice(i)].join("/");
  };
  const dirname = (p) => {
    needString("path", p);
    if (p.length === 0) return ".";
    const rooted = p[0] === "/";
    let end = -1;
    let matchedSlash = true;
    for (let i = p.length - 1; i >= 1; --i) {
      if (p[i] === "/") {
        if (!matchedSlash) {
          end = i;
          break;
        }
      } else {
        matchedSlash = false;
      }
    }
    if (end === -1) return rooted ? "/" : ".";
    if (rooted && end === 1) return "//";
    return p.slice(0, end);
  };
  const basename = (p, suffix) => {
    needString("path", p);
    if (suffix !== undefined) needString("suffix", suffix);
    let start = 0;
    let end = -1;
    let matchedSlash = true;
    if (suffix !== undefined && suffix.length > 0 && suffix.length <= p.length) {
      if (suffix === p) return "";
      let suffixAt = suffix.length - 1;
      let firstNonSlashEnd = -1;
      for (let i = p.length - 1; i >= 0; --i) {
        if (p[i] === "/") {
          if (!matchedSlash) {
            start = i + 1;
            break;
          }
        } else {
          if (firstNonSlashEnd === -1) {
            matchedSlash = false;
            firstNonSlashEnd = i + 1;
          }
          if (suffixAt >= 0) {
            if (p[i] === suffix[suffixAt]) {
              if (--suffixAt === -1) end = i;
            } else {
              suffixAt = -1;
              end = firstNonSlashEnd;
            }
          }
        }
      }
      if (start === end) end = firstNonSlashEnd;
      else if (end === -1) end = p.length;
      return p.slice(start, end);
    }
    for (let i = p.length - 1; i >= 0; --i) {
      if (p[i] === "/") {
        if (!matchedSlash) {
          start = i + 1;
          break;
        }
      } else if (end === -1) {
        matchedSlash = false;
        end = i + 1;
      }
    }
    return end === -1 ? "" : p.slice(start, end);
  };
  const extname = (p) => {
    needString("path", p);
    let startDot = -1;
    let startPart = 0;
    let end = -1;
    let matchedSlash = true;
    let preDotState = 0;
    for (let i = p.length - 1; i >= 0; --i) {
      if (p[i] === "/") {
        if (!matchedSlash) {
          startPart = i + 1;
          break;
        }
        continue;
      }
      if (end === -1) {
        matchedSlash = false;
        end = i + 1;
      }
      if (p[i] === ".") {
        if (startDot === -1) startDot = i;
        else if (preDotState !== 1) preDotState = 1;
      } else if (startDot !== -1) {
        preDotState = -1;
      }
    }
    if (
      startDot === -1 ||
      end === -1 ||
      preDotState === 0 ||
      (preDotState === 1 && startDot === end - 1 && startDot === startPart + 1)
    ) {
      return "";
    }
    return p.slice(startDot, end);
  };
  return { join, normalize, relative, dirname, basename, extname, isAbsolute };
}

function makeJson(native) {
  const toText = (what, v) => {
    const text = jsonStringify(v);
    if (text === undefined) throw new TypeError(`${what} is not a JSON value, got ${typeName(v)}`);
    return text;
  };
  const validate = guarded((schema, value) =>
    jsonParse(native.jsonValidate(toText("schema", schema), toText("value", value))),
  );
  return { validate };
}

function makeHash(native) {
  const sha256 = guarded((input) => {
    if (typeof input === "string") return native.sha256Text(input);
    if (input instanceof Uint8Array) return native.sha256Bytes(input);
    throw new TypeError(`input must be a string or a Uint8Array, got ${typeName(input)}`);
  });
  return { sha256 };
}

function makeUuid(native) {
  return { v4: () => native.uuidV4(), v7: () => native.uuidV7() };
}

function makeCli() {
  const optionLike = (s) => s.length > 1 && s[0] === "-";
  const unknown = (name) =>
    fail(
      "cli.unknown_option",
      `Unknown option '${name}'. To pass an argument that starts with '-', put it after '--', as in '-- "${name}"'`,
    );

  const parseArgs = (argv, config = {}) => {
    if (!isArray(argv) || !argv.every((a) => typeof a === "string")) {
      throw new TypeError(`argv must be an array of strings, got ${typeName(argv)}`);
    }
    if (config === null || typeof config !== "object") throw new TypeError("config must be an object");
    const options = config.options === undefined ? {} : config.options;
    if (options === null || typeof options !== "object") throw new TypeError("config.options must be an object");
    const allowPositionals = config.allowPositionals === undefined ? true : config.allowPositionals;
    const byShort = create(null);
    const names = getOwnPropertyNames(options);
    for (const name of names) {
      const o = options[name];
      if (o === null || typeof o !== "object" || (o.type !== "string" && o.type !== "boolean")) {
        fail("cli.config", `option '${name}' needs type "string" or "boolean"`);
      }
      if (o.short !== undefined) {
        if (typeof o.short !== "string" || o.short.length !== 1) {
          fail("cli.config", `option '${name}': short must be a single character`);
        }
        byShort[o.short] = name;
      }
      if (o.default !== undefined) {
        const ok = (d) => typeof d === o.type;
        if (o.multiple ? !(isArray(o.default) && o.default.every(ok)) : !ok(o.default)) {
          fail("cli.config", `option '${name}': default must be ${o.multiple ? "an array of " : "a "}${o.type}`);
        }
      }
    }
    const known = (name) => names.includes(name);
    const values = create(null);
    const positionals = [];
    const store = (name, value) => {
      if (options[name].multiple) (values[name] ??= []).push(value);
      else values[name] = value;
    };
    const takeValue = (shown, name, inline, rest) => {
      const o = options[name];
      if (o.type === "boolean") {
        if (inline !== undefined) fail("cli.unexpected_value", `Option '${shown}' does not take a value`);
        return true;
      }
      if (inline !== undefined) return inline;
      if (rest.length === 0) fail("cli.missing_value", `Option '${shown} <value>' needs a value`);
      if (optionLike(rest[0])) {
        fail(
          "cli.missing_value",
          `Option '${shown} <value>' needs a value, and '${rest[0]}' looks like an option. ` +
            `To give a value that starts with '-', write '--${name}=${rest[0]}'`,
        );
      }
      return rest.shift();
    };

    const rest = argv.slice();
    while (rest.length > 0) {
      const arg = rest.shift();
      if (arg === "--") {
        positionals.push(...rest);
        break;
      }
      if (arg.startsWith("--")) {
        const eq = arg.indexOf("=");
        const name = eq === -1 ? arg.slice(2) : arg.slice(2, eq);
        const inline = eq === -1 ? undefined : arg.slice(eq + 1);
        if (!known(name)) unknown(`--${name}`);
        store(name, takeValue(`--${name}`, name, inline, rest));
        continue;
      }
      if (optionLike(arg)) {
        for (let i = 1; i < arg.length; i++) {
          const short = arg[i];
          const name = byShort[short];
          if (name === undefined) unknown(`-${short}`);
          if (options[name].type === "string") {
            const attached = i + 1 < arg.length ? arg.slice(i + 1) : undefined;
            store(name, takeValue(`-${short}`, name, attached, rest));
            break;
          }
          store(name, true);
        }
        continue;
      }
      if (!allowPositionals) {
        fail("cli.unexpected_positional", `Unexpected argument '${arg}': this command takes no positional arguments`);
      }
      positionals.push(arg);
    }
    for (const name of names) {
      const d = options[name].default;
      if (d !== undefined && values[name] === undefined) values[name] = isArray(d) ? d.slice() : d;
    }
    return { values, positionals };
  };
  return { parseArgs };
}

let installed = false;

export default function install(native, version, name) {
  if (installed) fail("library.installed", "the base library is already installed in this runtime");
  if (name in globalThis) fail("library.global", `${name} is already a global`);
  installed = true;
  const errorName = `${name[0].toUpperCase()}${name.slice(1)}Error`;
  defineProperty(AgixError, "name", { value: errorName });
  defineProperty(AgixError.prototype, "name", { value: errorName });
  const agix = {
    version,
    yaml: makeYaml(native),
    glob: makeGlob(native),
    path: makePath(),
    json: makeJson(native),
    hash: makeHash(native),
    uuid: makeUuid(native),
    cli: makeCli(),
  };
  deepFreeze(AgixError);
  defineProperty(globalThis, name, {
    value: deepFreeze(agix),
    writable: false,
    enumerable: false,
    configurable: false,
  });
}
