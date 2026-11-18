// RunExt 命令里的全局（docs/runext.md §4）：入口函数的 ctx、tools.* 与 callTool、log、stopRequested。
// 复制到自己的插件里，入口文件写 `// @ts-check` 并 `/// <reference path="types/runext.d.ts" />`。

/// <reference path="agix.d.ts" />

interface RunExtContext {
  /** The session's working directory: the project, or the worktree the session is in. */
  readonly projectRoot: string;
  readonly cwd: string;
  /** The command's name, as the model writes it. */
  readonly command: string;
  readonly plugin: { readonly name: string; readonly version: string };
  /** The command line matched a `read_only` prefix: any write in this run fails. */
  readonly readOnly: boolean;
}

interface ReadResult {
  /** The text as it is, without line numbers. */
  content: string;
  totalLines: number;
  /** More lines follow the window that was read. */
  truncated: boolean;
}

interface GlobResult {
  /** Newest first. */
  paths: string[];
  /** How many `max_results` cut. */
  more: number;
}

interface BashResult {
  stdout: string;
  stderr: string;
  exit_code: number;
}

interface CodeTools {
  /** A missing file rejects with `(no such file) <path>`. */
  Read(input: { file_path: string; offset?: number; limit?: number }): Promise<ReadResult>;
  /** Replacing an existing file needs a Read of it earlier in the same run. */
  Write(input: { file_path: string; content: string }): Promise<string>;
  Edit(input: { file_path: string; old_string: string; new_string: string; replace_all?: boolean }): Promise<string>;
  Glob(input: { pattern: string; path?: string; respect_gitignore?: boolean; max_results?: number }): Promise<GlobResult>;
  Grep(input: { pattern: string; path?: string; glob?: string; output_mode?: "files_with_matches" }): Promise<{ files: string[] }>;
  Grep(input: { pattern: string; path?: string; glob?: string; output_mode: "count" }): Promise<{ counts: Record<string, number> }>;
  Grep(input: { pattern: string; path?: string; glob?: string; output_mode: "content" }): Promise<string>;
  /** Rejects when the exit code is not zero. */
  Bash(input: { command: string; description?: string; timeout?: number }): Promise<BashResult>;
  /** Any other tool the session lets code call: its structured result, or its text. */
  [name: string]: (input: any) => Promise<any>;
}

/** The session's tools, as code calls them. Each call goes through the session's permissions. */
declare const tools: CodeTools;
/** A tool by name, for MCP tools (`mcp__<server>__<tool>`); resolves to its structured result or its text. */
declare function callTool(name: string, input?: object): Promise<any>;
/** A progress line: the host sees it as the run goes, the model gets the last 4,000 characters. */
declare function log(...parts: unknown[]): void;
declare const console: { log: typeof log; info: typeof log; warn: typeof log; error: typeof log; debug: typeof log };
/** The user interrupted the turn or stopped the task: wrap up. */
declare function stopRequested(): boolean;
