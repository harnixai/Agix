// 全局 `agix`（docs/agix_js.md §3）：纯计算的基础库，RunExt 命令、RunCode 与扩展点脚本里都有；
// 改了引擎名的嵌入方里全局叫引擎名。出错抛 AgixError（带 code，如 `yaml.syntax`；name 随引擎名，
// 如 FooError，判断错误按 code）；参数类型不对抛 TypeError。

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

interface AgixError extends Error {
  readonly name: string;
  /** `<namespace>.<what>`, e.g. `yaml.syntax`, `cli.unknown_option`. */
  readonly code: string;
  /** For syntax errors, from 1; frontmatter errors count lines of the whole text. */
  readonly line?: number;
  readonly column?: number;
}

interface AgixOption {
  type: "string" | "boolean";
  short?: string;
  multiple?: boolean;
  default?: string | boolean | string[] | boolean[];
}

declare const agix: {
  /** The engine's `x.y.z`. */
  readonly version: string;
  readonly yaml: {
    /** One YAML 1.2 (core schema) document; dates stay strings. */
    parse(text: string): JsonValue;
    /** Block style, indent 2, keys in insertion order, ends with a newline. */
    stringify(value: unknown): string;
    /** `data` is null when the text has no frontmatter, `{}` when it is empty; `body` is kept byte for byte. */
    parseFrontmatter(text: string): { data: { [key: string]: JsonValue } | null; body: string };
    stringifyFrontmatter(data: object | null, body: string): string;
  };
  readonly glob: {
    /** Same matching as the Glob tool and permission rules: `*` and `?` stop at `/`, `**` crosses. */
    test(pattern: string | string[], path: string): boolean;
    filter(pattern: string | string[], paths: string[]): string[];
  };
  /** Node's `path.posix`, without `resolve`. */
  readonly path: {
    join(...parts: string[]): string;
    normalize(p: string): string;
    /** Throws `path.relative` when the answer would need a current directory. */
    relative(from: string, to: string): string;
    dirname(p: string): string;
    basename(p: string, ext?: string): string;
    extname(p: string): string;
    isAbsolute(p: string): boolean;
  };
  readonly json: {
    /** Draft 2020-12 unless the schema names another; at most 100 errors. */
    validate(
      schema: object | boolean,
      value: unknown,
    ): { valid: boolean; errors: { instancePath: string; schemaPath: string; message: string }[] };
  };
  readonly hash: {
    /** Lowercase hex; a string is hashed as UTF-8. */
    sha256(input: string | Uint8Array): string;
  };
  readonly uuid: {
    v4(): string;
    /** Time-ordered. */
    v7(): string;
  };
  readonly cli: {
    /** A subset of Node's `util.parseArgs`, always strict. */
    parseArgs(
      argv: string[],
      config?: { options?: { [name: string]: AgixOption }; allowPositionals?: boolean },
    ): { values: { [name: string]: string | boolean | string[] | boolean[] | undefined }; positionals: string[] };
  };
};
