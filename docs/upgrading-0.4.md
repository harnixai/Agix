# 升级到 0.4

给建在 Agix 上的应用(经 JSON-RPC 连 `agixd` 的,或经 `agix` crate 嵌入的)看的:从 0.3.20 及更早升到 0.4.0
要改什么,末尾有 0.4.2、0.4.3 的新增。0.3.21 重排了设置文件,0.4.0 改了会话选项、协议与预算;两版都不做兼容,旧写法不再生效。

逐个类型、逐个函数的变化在 [`agix-sdk.md`](agix-sdk.md)「迁移说明」的 0.3.21、0.4.0 两条;协议的完整说明
在 [`daemon_rpc_protocol.md`](daemon_rpc_protocol.md)。

## 先做的检查

1. **握手看协议版本。** 0.4.0 的 `protocol_version` 是 **5**(0.3.x 是 4)。宿主应在 `daemon.auth` 这一步就
   拒绝它不认识的版本。0.4.0 新增两个能力位:`session_settings`(`options.settings`,`session.setModel` /
   `session.setPrompt` 的 `clamped_options`)与 `config_explain`(`config.explain`);用到这些的应用在 `daemon.ping`
   / `daemon.auth` 的 `capabilities` 里查它们,不要靠版本号推。
2. **跑一次 `daemon.doctor`。** `settings_tiers` 的每一层报出 `renamed_keys`(写了旧键的,连同取代它的新键)与
   `ignored_values`(放不进的值、不合格的 provider id 等)。旧键一律不读:管权限、沙箱、信任、MCP 开关的旧键还在时
   检查报 `failing`,其余报 `degraded`。加载时的告警也会点出新键名。
3. **值不是预期的,调 `config.explain {key}`。** 它逐层列出这个键在每一层写了什么、结局如何(`applied` 生效、`overridden` 被盖过、`merged`、`not_set`、
   `withheld_untrusted` 未受信项目被扣下、`dropped_not_global` 只认全局层被丢掉、`ignored` 值放不进、
   `managed_overrode` 被 managed 文件改掉、`held_tighter_below` managed 的值比下层松、下层更严的留下;
   加 `session_id` 时还有会话那一层:`clamped` 被夹住、`not_session_scope` 这个键不按会话定),能力位 `config_explain`。迁移后某个值没生效,多半是旧键还在、写错了层,或被信任门禁扣下,这里一眼可见。

## 坏设置怎么处理

谁在写、谁在场,就给谁报错;文件只告警;安全文件按失败即关闭处理。设置文件里不认识的键、旧键、放不进的值,告警并
忽略(`daemon.doctor`、`config.explain` 报出);`agixd --set` 与 `EmbedConfig.settings` 里的让启动失败;
`options.settings` 与其他 RPC 参数里的让这个请求以 `INVALID_PARAMS` 失败;managed 文件里的一个值都不套、
拒绝每一次工具调用。所以升级时旧键留在文件里不会让 daemon 起不来,留在 `--set`、`EmbedConfig.settings`、
`options.settings` 里则会。

## 设置文件(0.3.21 起)

设置按块重排,旧键不读。旧键 → 新键(代码里的对照表是 `settings::renamed::RENAMED_KEYS`):

| 旧 | 新 |
|---|---|
| `permission_mode` | `permissions.mode`(不再认 `auto`、`yolo` 别名) |
| `permission_rules[{tool,action,match}]` | `permissions.allow` / `ask` / `deny` 的字符串规则(如 `"Bash(git status:*)"`);按字面比对的写进 `permissions.allow_exact` |
| `allow_client_permission_override` | `permissions.client_may_loosen` |
| `disable_bypass_permissions_mode`(managed) | `permissions.disable_bypass` |
| `execution.permission_prompt_timeout_secs` | `permissions.prompt_timeout_secs` |
| `execution.max_api_calls_per_turn`、`max_budget_tokens`、`max_parallelism` | `turn.max_model_calls`、`turn.token_budget`、`turn.parallel_tools` |
| `execution.max_agent_depth`、`max_concurrent_subagents` | `agents.max_depth`、`agents.max_concurrent` |
| `sandbox.additional_directories` | `files.extra_dirs` |
| `sandbox.read_directories` + `read_anywhere` | `files.read_dirs`(写 `"/"` 即任意处可读) |
| `sandbox.allow_write`、`deny_write` | `files.allow_write`、`files.deny_write` |
| `sandbox.dangerously_disable_sandbox` | `sandbox.enabled`(取反) |
| `sandbox.require_enforcement` | `sandbox.require` |
| `sandbox.network_mode`、`egress_proxy` | `sandbox.network.mode`、`sandbox.network.proxy` |
| `sandbox.subprocess_env` | `sandbox.env` |
| `sandbox.cache_directories` | `sandbox.cache_dirs` |
| `sandbox.reap_process_groups` | `sandbox.reap_on_close` |
| `sandbox.unattended_requires_network_policy` | `sandbox.unattended_needs_network_policy` |
| `sandbox.allowed_domains` | `web.allowed_domains` |
| `network.use_system_proxy` | `app.system_proxy` |
| `project_trust.trusted`、`project_trust.default` | `app.trusted_projects`、`app.trust_all_projects`(`default: "trusted"` 即 `true`) |
| `history.location` | `app.history` |
| `worktree_dir` | `app.worktree_dir`(场景层不再能写) |
| `team.max_persistent_members`、`member_idle_timeout_secs` | `team.max_members`、`team.member_idle_secs` |
| `compaction.cached_microcompact`、`tool_result_inline_chars` | `compaction.clear_on_cache_expiry`、`compaction.inline_result_chars` |
| `prompt_append`、`prompt_override`、`instruction_file`、`language`、`path_style` | `prompt.append`、`prompt.override`、`prompt.instruction_files`(列表)、`prompt.language`、`prompt.path_style`(`workspace_relative` 改叫 `relative`) |
| `hooks_config` | `hooks` |
| `mcp_servers` + `mcp_enabled` | `mcp.servers.<名>`;开关写在条目的 `enabled` 里 |
| `disable_skill_shell_execution` | `skills.allow_shell`(取反) |
| `memory_enabled` | `memory.enabled` |
| `telemetry_enabled`、`telemetry_url` | `telemetry.enabled`、`telemetry.otlp_endpoint` |
| `providers.<id>`(对象) | `providers[]`(列表,每项带 `id`);写成对象的整段不读,`ignored_values` 说明 |
| `providers[].api_type`(`openai_compatible` 等) | `providers[].api`(`anthropic`、`openai_chat`、`openai_responses`) |
| `providers[].thinking_toggle`、`reasoning_passback` | 由 `providers[].vendor` 推出 |
| `providers[].api_key` | 删除:只认 `api_key_env` 或宿主给的凭据 |
| `default_provider`、`providers[].default_model`、`model.model_name` | `model.order` 排 provider;每个 provider 的 `tiers.{strong,balanced,fast}` 给三档模型 |
| `task_models.<t>` | `model.tasks.<t>`,值是档位 |
| `model.api_type`、`model.base_url` | 写在 provider 条目里 |
| `model.max_output_tokens` | `providers[].models[].max_output` |
| `model.fallback_model` | 删除;`model.order` 里排在后面的 provider 就是回退 |
| `model.thinking_mode` | `model.thinking` |

0.4.0 再加的:

- **进程参数进 `app`。** `app.session_cap`(缺省 32)、`app.session_idle_timeout_secs`(缺省 3600)、
  `app.worker_threads`。`app` 只认全局层、`cli` 层与 managed 层。
- **自带 provider。** 引擎自带的 `anthropic` 只在没有定义任何 `providers` 时存在,模型是 `--model`;要给它配额或
  档位,在 `providers` 里自己定义(宿主在内存里对自带 provider 的改动,重载后不保留)。
- **新块。** `session.token_budget`(会话从创建起的累计上限,跨重启)、`goal.max_rounds`(模型自己设的目标的轮数
  上限,缺省 10000)、`schedule.max_model_jobs`(缺省 20)与 `schedule.min_model_interval_minutes`(缺省 1):只限
  模型经 `CronCreate` 建的定时任务,宿主的 `schedule.create` 不受限。
- **provider id 只能是小写字母、数字、`-`、`_`。** 别的 id 整项忽略,`daemon.doctor` 的 `ignored_values` 说明。
- **managed 文件能写任何键,** 压过所有层(见 README「配置」的优先级表);有松紧次序的键下层更严时留下层的值。
  managed 写了 `permissions.mode` 时,它是会话模式的上限,`permissions.client_may_loosen` 与 `session.setPermissionMode`
  也越不过。

## 两个缺省值(0.4.0)

0.3.x 说过"后续版本改"的两个缺省在 0.4.0 改了,应用要跟着改:

- **`plugins.enable_on_install` 缺省 `false`。** `plugin.install` 装好并返回披露,新包保持禁用,结果的 `enabled`
  是 `false`。宿主读过披露后调 `plugin.enable`,包才往会话里贡献东西。要沿用装完即启用,在全局设置里写
  `plugins.enable_on_install: true`。已有启用记录的包(包括升级同名包)不受影响。
- **`sandbox.unattended_needs_network_policy` 缺省 `true`。** 无人值守的会话(场景不交互,或
  `options.attended: false`)里,没有 allow 规则的 `Bash` / `Monitor` 命令要沙箱完全兜得住、并且
  `sandbox.network.mode` 不是 `unrestricted` 才执行,否则按无人可问拒绝。缺省的 `unrestricted` 下,这类命令在 0.3.x
  能跑、在 0.4.0 被拒。三种改法任选:把 `sandbox.network.mode` 设成 `deny_all` / `allowlist`;给要跑的命令写
  `permissions.allow` 规则;或者接受开着的网络,把这个开关设为 `false`(`daemon.doctor` 的
  `sandbox.unattended_network` 这时报 `degraded`)。

## agixd 命令行

- `agixd --set KEY=VALUE`(可重复):值按 JSON 读,读不成 JSON 就当字符串;不认识的键、旧键、类型不对时启动失败。
  它们合成 `cli` 层,叠在每个项目、每个场景的设置文件之上,`config.reload` 之后仍在。
- `--max-tokens`、`--permission-prompt-timeout`、`--session-cap`、`--session-idle-timeout`、`--worker-threads` 是
  `--set` 的简写。`--max-tokens` 现在压过设置文件里写的 `model.max_tokens`,不再只是没写时的兜底。

## 协议(0.4.0)

### `session.create` 的 `options`

`options` 里与模型、设置有关的字段分两类(`recorder`、`clock`、`telemetry`、`output_schema`、`prompt_types`、
`host_tools`、`attended`、`auto_name`、`skill_dirs` 不变):

- **选这个会话的对话用哪个模型:** `model`、`provider`。
- **`options.settings`:** 形状同 settings.json,作用于整个会话,连同它的子 Agent 与团队成员。推理强度、思考开关
  与系统提示词也在这里:`model.effort`、`model.thinking`、`prompt.append`、`prompt.override`,所以子 Agent 与团队成员
  用的也是会话的这几项。`session.setModel` 的 `effort` / `thinking` 与 `session.setPrompt` 改的是同一层,判法相同。
  - 只收按会话决定的键。按 (项目, 场景) 或按进程决定的(`providers`、`model.order`、`model.tasks`、`hooks`、
    `mcp`、`app`……)让请求以 `INVALID_PARAMS` 失败,`error.data.invalid_options` 逐项列出。
  - 缺省只能收紧。比设置宽的值被夹住,报在结果的 `clamped_options`(`{field, requested, applied}`);
    带 allow 的规则列表被扣下。设置开了 `permissions.client_may_loosen` 才能放宽。managed 层写了的键压过它。
  - 能力位 `session_settings`。

`options` 里任何读不出的字段(包括不认识的)都以 `INVALID_PARAMS` 失败,`invalid_options` 逐项列出。0.3 的下列选项已删,报错里给出新写法:

| 0.3 | 0.4 |
|---|---|
| `options.turn.{token_budget, max_model_calls}` | `options.settings.turn.{token_budget, max_model_calls}` |
| `options.permission_mode` | `options.settings.permissions.mode` |
| `options.permission_rules` | `options.settings.permissions.{allow, allow_exact, ask, deny}` 的规则串 |
| `options.effort` | `options.settings.model.effort` |
| `options.thinking_mode` | `options.settings.model.thinking` |
| `options.prompt.{append, override}` | `options.settings.prompt.{append, override}` |

会话日志里记下的旧选项,以及 `session.setModel` / `session.setPrompt` 记下的推理强度、思考开关与提示词,恢复时按新形状读进
会话的设置,限制不会因恢复变松。

### 改名

- `session.setModel` 的参数与结果:`thinking_mode` → `thinking`;再传 `thinking_mode` 以 `INVALID_PARAMS` 失败。
  `effort` / `thinking` 写进会话自己那层设置(`model.effort` / `model.thinking`),会话随之原地重建;`session.setPrompt`
  写 `prompt.append` / `prompt.override`。两者都写进会话日志,重建、驱逐后恢复与重启都保留;值给 `null` 就把这个键从
  会话层去掉,回到项目与场景设置的值。两者判法同 `options.settings`,值被 managed 层压住时结果多一个
  `clamped_options`(形状同 `session.create` 的)。只改 `effort` / `thinking` 的 `session.setModel` 也要重建,所以
  与 `session.setPrompt` 一样:一轮在跑或会话在重建时返回 `SESSION_BUSY`,有后台任务在跑时 `SESSION_BUSY` 的
  `reason` 是 `background_tasks`。推理强度、思考开关与提示词对子 Agent 与团队成员同样生效。
- `session.get`、`session.inspectContext` 的 `reasoning.{thinking_mode, thinking_mode_source}` →
  `{thinking, thinking_source}`;`session.call` 的 `request.thinking_mode` → `thinking`。
- `stop_reason` 的 `max_turns_reached` → `max_turns`(引擎实际发的一直是后者)。

### 用量与预算

- **口径统一。** 所有预算与 provider 配额都按「未命中缓存的输入 + 缓存写入 + 输出」计,缓存命中不计。
- **`turn_complete.usage` 是这一轮的全部:** 自己、子 Agent、团队成员,以及旁路调用(压缩、记忆、
  `WebSearch`、`WebFetch` 抽取、prompt 型与 agent 型 hook);拆分在 `usage_breakdown.{own, side, delegates}`。
- **预算是一条链。** 由近及远:子 Agent 的轮 → 根轮(`turn.token_budget`)→ 目标 → 会话(`session.token_budget`)。
  任何一层用完都以 `budget_exceeded` 结束这一轮,`turn_complete.message` 写明是谁的预算。每次调用前先从剩余里
  扣掉这次请求预计会计入的输入,付不起就不发。
- **`session.get` 多一个 `usage`:** 会话从创建起的累计,含子 Agent(后台的也算)与两轮之间的调用(如 `session.compact`
  的摘要、两轮之间触发的 hook),跨重启。
- **目标:** `session.setGoal` / `updateGoal` 可带 `token_budget`;目标状态多 `token_budget`、`tokens_used`。
  用完时目标转 `blocked`,`blocked_reason` 以 `token budget` 开头。模型的 `GoalUpdate` 只能调低目标的预算,调高要宿主
  调 `session.updateGoal`。
- **定时任务:** `schedule.list` 的任务多一个 `by_model`。

### 其他

- `daemon.doctor` 的 `settings.managed` 的 `details.permissions` 换成 `details.settings`(文件的形状);
  `config.get` 的 `tier` 多一个 `cli`,`effective` 回答多 `managed` 字段;`daemon.doctor` 的 `settings_tiers` 多一项
  `tier: "cli"`。
- `/cost` 与 telemetry `session_end` 的 token 合计含子 Agent。
- `config.reload`、`config.set`、`config.setProvider` 的结果多一个 `restart_required`:只在启动时读的键里值变了的
  (`app.session_cap` 等),managed 文件变了时还有 `managed`。这些改动要重启 daemon 才生效。
- 0.3.21 起:`provider_switched` 事件(换了 provider 及原因)与 `providers_unavailable` 错误码
  (`model.order` 里的 provider 都接不了)。

## 构建

- **RPC schema 生成不进发布二进制。** `docs/schemas/rpc.schema.json` 由测试或 `assembly` 的 `rpc-schema` feature 生成
  (`cargo test -p agix-assembly write_rpc_schema -- --ignored`),agixd 与嵌入方不用管。

## 嵌入(`agix` crate)

- `EmbedConfig.settings: Option<serde_json::Value>` 就是 `cli` 层,与 `agixd --set` 同一条路径。
  `EmbedConfig` 的 `max_tokens`、`permission_prompt_timeout`、`history`、`session_cap`、`session_idle_timeout`
  删掉,写进 `settings`:

  ```rust
  config.settings = Some(serde_json::json!({
      "model": { "max_tokens": 32000 },
      "permissions": { "prompt_timeout_secs": 300 },
      "app": { "history": "project", "session_cap": 8 }
  }));
  ```

- 设置写错时 `Embedded::start` 返回 `EmbedError::InvalidSetting { key, reason }`。
- `SessionOptions` 的变化同上面的 `options`:`settings` 字段,删掉 `permission_mode`、`permission_rules`、`turn`、
  `thinking_mode`、`effort`、`prompt`,这些都写进 `settings`。`SessionPool::set_session_prompt` 收
  `PromptChange`。不经 RPC 直接建会话时,会话不能给的键被略去并告警;要先检查,调
  `SessionOptions::settings_problems()`。
- provider 路由的实现从 `base` 挪进 `model::routing`(`ProviderSet`、`SessionRouter`),runtime 只见
  `base::provider::TaskRouting`;对 `ModelEvent` 穷举 `match` 的要补 `Served`。其余类型与函数的变化见
  [`agix-sdk.md`](agix-sdk.md)「迁移说明」。
- `SessionCreated`、`ModelChoice`、`PromptChoice` 多 `clamped_options: Vec<ClampedOption>`;
  `SessionPool::set_session_model` 收 `ReasoningChange { effort, thinking }`;新增
  `SessionPool::explain_setting(key, ExplainTarget, include_secrets)`(即 `config.explain`)。
  对 `base::interface::config_source::Tier` 穷举 `match` 的要补 `Cli` 与 `Session`。
- 库式用法的 `load_daemon_config` 第二个参数是 `cli_settings: Option<serde_json::Value>`(原 `max_tokens: u32`),
  `Assembly::permission_prompt_timeout` 删掉,写进 `cli_settings`。

## 0.4.2

0.4.2 没有协议版本变化(仍是 5),新增四个能力位:`host_tool_permit`、`subagent_provider`、`task_model`、`session_plugins`
(旧引擎把 `options.settings.plugins` 以 `INVALID_PARAMS` 退回)。用到的应用先查
`daemon.ping` 的 `capabilities`。

### 宿主工具自己放行(`permit`,#99)

0.3 里宿主在 `session.create` 里给一条按工具名的 allow 规则,无人值守的会话就不再为自己的宿主工具发询问。0.4 起会话层
只能收紧,`options.settings.permissions.allow` 被扣下(除非设置开了 `permissions.client_may_loosen`),这条路断了。
现在宿主在工具条目上自己说:

```json
{"options": {"host_tools": [{"name": "HostEcho", "description": "...", "input_schema": {...}, "permit": "allow"}]}}
```

- `permit` 是 `"ask"`(缺省,按设置判)或 `"allow"`;只放行这一个工具,不放宽会话模式,不受 `client_may_loosen` 管。
  `daemon.setHostTools` 的条目同样带。
- 不认这个字段的旧引擎把整个 `options` 以 `INVALID_PARAMS` 退回:先查能力位 `host_tool_permit`。
- 它算一条命中的 allow:`dontAsk` 与无人值守下免问。压过它的照旧:任何一层(含 managed)的 deny 与 ask 规则、宿主自带权限
  策略、`PreToolUse` hook 的拦截、`plan` 模式只放行 `read_only` 的。遥测 `permission_decision` 的 `basis` 是
  `host_declared`。
- `daemon.setHostTools` 里的 `permit` 作用于这个 daemon 里此后建起的每个会话,不论哪条连接建的;已建好的引擎保留建起时
  那份,直到下一次重建(`setPrompt`、`setModel` 的重建也算)。改回 `ask` 或撤回同样只对此后建起的引擎生效。
- `options.host_tools` 的 `permit` 随 `options` 记进会话日志,恢复时取记下的那份。

### 插件按会话收窄(#100,能力位 `session_plugins`)

`options.settings.plugins.allow` 与 `plugins.enabled` 的作用域由进程改为会话(`plugins.enable_on_install` 仍是进程级),
所以 0.4.1 里会让 `options.settings` 以 `INVALID_PARAMS` 失败的这两个键现在可以写。规矩是只能收紧:

- 会话的集合 = 会话的 `allow` 与池里**实际启用**的包取交集,再从同一个池集合里补上这些包的依赖。名单不写或写 `[]` 是
  池里的全部;`enabled: false` 是这个会话没有插件,宿主自带的包(`source: host`)也不例外;`enabled: true` 放不开池里的
  `false`。
- 名单里池没有启用的名字不放开,报在 `clamped_options`(`settings.plugins.allow`,`applied` 是留下的)。名字全不在时
  `applied` 是 `[]`:会话没有插件,不是「空表示全部」。
- 不在集合里的包对这个会话、它的子 Agent 与团队成员什么都不贡献:脚本不跑,命令、技能、agent 类型不列,函数不可见,
  它的 MCP server 不交给会话,它的场景进不去。包占用的同名 agent 类型(如覆盖内置 `explore`)回落到内置或下层的那个。
- `session.inspectContext` 多 `plugins`:会话实际用的包 `[{name, version, required_by?}]`,因依赖补进来的带 `required_by`。
- `plugin.functions {session_id}` 只列会话集合里的包的函数;`plugin.call` 指定会话、函数属于集合外的包时以
  `INVALID_PARAMS` 失败。不带 `session_id` 的照旧是池级。
- 场景 `plugin:<name>`:包不在会话集合里时 `session.create`、恢复与重建以 `INVALID_PARAMS` 拒绝
  (`invalid_options` 字段 `settings.plugins`);agent 类型的 `scene:` 指向这样的场景时,子 Agent 的派生报错,不退回父场景。
- MCP:被排除的包声明的 server 仍由共享的 hub 连着(连接按 (项目, 场景) 共用),只是不交给这个会话。包与 settings 写了
  同一个 MCP 键时,连上的是包的那个,所以排除这个包的会话两者都拿不到。这种同名会报警告(`mcp.status` 的 `warnings`,连接时另写日志)。
- 收窄的会话在建引擎时定下集合:之后才启用的包要等重建(`session.setModel`、`session.setPrompt` 等)才进来;没有
  设 `plugins` 的会话继续跟着池走。
- 日志里最后一条会话规格读不出来时,会话不再退回更早的规格而放宽到池的全部插件;`plugins` 段读不出就是没有插件。

### 子 Agent 的 provider(#101)与任务里的模型(#98)

- `session.startAgent`、`Agent` 工具的输入、agent 类型 frontmatter 收可选的 `provider`(请求的压过类型的)。子 Agent
  用一条自己的路由:那家排第一,其余照父会话的路由(`model.order`,父会话指名了 provider 的连同它),健康与配额与父会话共用。没给 `provider` 但模型 id 有确定的归属
  (某家的 `models` 列着它或把它作档位;只是「什么都收」的那家不算),且父会话的调用不会先去那里时,也从它的归属起;
  这样 `model.order: ["ds","cc"]` 下点名 `cc` 的模型 id 真的会到 `cc`,不再换成 `ds` 的同档模型。档位名、`inherit`、
  `opus` / `sonnet` 这类别名在没给 `provider` 时照旧按会话的路由解析。
- 只有宿主(`startAgent`)可以点名这一对 (项目, 场景) 定义了的任何一家;模型(`Agent` 工具)与 agent 类型(项目文件、
  插件、用户目录的都算)点名的必须在父会话的路由里 —— `model.order`,加上父会话自己指名的 provider。定义了但不在里面的:
  `Agent` 工具是这次调用的错误(说它不在这个会话的 `model.order` 里);agent 类型的记一条告警、当没写。有确定归属的模型
  id 也只在路由里找归属。
- 未受信项目的 agent 类型写的 `provider` 在项目受信前不生效,这样的类型列在 `project.status` 的 `requires_trust`
  (`agents/<name>`)。
- `provider` 不在这一对的定义里:`startAgent` 以 `INVALID_PARAMS` 失败且不登记任务;`Agent` 工具是这次调用的错误;
  agent 类型写错的记一条告警、当没写。能力位 `subagent_provider`。
- 给活着的团队常驻成员发消息时点名的 provider 与它在跑的那家不同:这次 `Agent` 调用的错误 —— 活着的成员保持它起来时的
  provider;要换,先停掉它(`TaskStop` / `TeamDelete`)再发。
- 有自己路由的子 Agent 的 `provider_switched` 在它自己的事件里(经 `subagent_progress` 转出),不再算到父会话头上。
- 宿主画任务树时:`session.tasks` 里 agent 与 teammate 任务的 `detail` 带 `model`、`provider`。`task_started` 里是预测,
  第一次调用后可能被 `task_updated` 改写(路由回退后也会);`step_end` 带 `served: {provider, model}`(没经路由的调用
  没有);侧链会话的 `session.get` 带 `model`、`provider`(父会话不在内存里、或已不记那件任务时没有)。能力位 `task_model`。

### 其他

- Anthropic 请求的 `cache_control` 不再带 `scope`(官方端点对 `{"type":"ephemeral","ttl":"1h","scope":"global"}` 回 400)。
  只有 `type` 与 `ttl`;场景侧「静态、缓存一小时」的含义不变。
- Anthropic 请求里的 `cache_control` 总数不超过四个(工具、系统、消息合计;官方端点对第五个回 400)。系统块标得
  多时只留静态前缀(`Global` 块)末尾与最后一个缓存块,另两个给工具前缀和对话滚动点。

### 嵌入(`agix` crate)

- `base::provider::TaskRouting` 多四个**必须实现**的方法:`starting_at(provider) -> Result<Arc<dyn TaskRouting>, String>`、
  `in_order(provider) -> bool`、`home_of(model) -> Option<String>`、`destination(task, model) -> Destination`;自己实现路由的宿主要补上。新类型
  `base::provider::Destination { provider, model }`。
- `ModelEvent::Served` 多 `model`(`#[serde(default)]`);`AgentEvent::StepEnd` 与 `StreamResult` 多 `served:
  Option<Destination>`,对它们穷举 `match` 或按字段构造的要补。
- `AgentTypeDefinition.provider`、`DelegateRequest.provider`:`Option<String>`。
- `AgentTypeSource::Plugin` 由单元变体改为 `Plugin { plugin: String }`,类型不再是 `Copy`;`may_loosen`、`may_shadow`
  取引用。
- `SceneRegistry::narrowed(keep)` / `withholds(id)`,`McpManager::share_where(keep)`:会话收窄用的,自己装会话的宿主可以
  照用。
- `HostToolOption.permit: HostToolPermit`(`Ask` 缺省 / `Allow`),`agix::host_tools::HostToolPermit` 导出;构造
  `HostToolOption` 的补 `permit`。

## 0.4.3

### 不认识的 agent 类型报错

以前点名一个会话没有的 agent 类型,会悄悄起一个通用子 Agent;现在是错误,不起子 Agent。「会话有的」是内置、用户目录、
项目与插件的类型,按会话的 `plugins.allow` / `enabled` 收窄之后 —— 所以被会话排除的包带来的类型同样报错(0.4.2 里是
悄悄起通用的)。错误写明这个名字并列出会话有的类型名(按名字排序,最多 30 个)。

- `session.startAgent` 的 `subagent_type`:`INVALID_PARAMS`,不登记任务。
- 模型的 `Agent` 工具(含给团队常驻成员发消息)的 `subagent_type`、`TeamCreate` 成员的 `agent_type`:这次工具调用的错误,
  模型可以换个名字重试;`TeamCreate` 在任何成员起来之前就失败。`context: fork` 的 skill 的 `agent` 同样。
- 不写类型(或 `null`)仍是通用子 Agent;`general-purpose` 是内置类型,照常可写。名字先按原样找;找不到时,
  忽略大小写只对上一个类型的就用它(Claude Code 的 `Explore` / `Plan` 落到 `explore` / `plan`),对上多个仍报错。

嵌入:`base::interface::delegate::Delegator` 多一个有缺省实现的 `check_agent_type(name) -> Result<(), DelegateError>`,
包装别的 `Delegator` 的要转发它。

### 发给 Anthropic 官方 API 的请求

- 只做搜索的那次调用(`WebSearch`)不再对 Opus 5.5 起、Sonnet 5.5 起、Fable / Mythos 5.1 起发强制 `tool_choice`
  (这些型号回 400),改发 `auto` 并在 system 里点名 `web_search`;更早的 Claude 型号与非 Claude 模型照旧强制。
  模型没搜就作答时,`WebSearch` 返回工具错误 "the provider answered without searching the web",不再把那段话当成结果。
- `WebSearch` 的结果不再以 "REMINDER: You MUST include the sources…" 结尾;列出来源的要求只在工具说明里。

### 压缩或修剪之后再读文件

压缩、按时间或缓存过期清工具结果、工具结果预算、提前修剪拿走或截短了某次 `Read` 的结果之后,再 `Read` 同一文件返回内容,
不再答 "[file unchanged]"。压缩后原样放回的最近文件(5000 字符以内)仍按"未变"作答。`Read` 自己没给全(默认行数先到、
有行被截长)时,重复同样的读仍答"未变",但写明上次没显示全,要用 `offset` / `limit` 读剩下的。

## 0.4.4

协议版本不变(仍是 5)。新增能力位 `compact_events`、`prompt_task`、`agent_token_budget`、`plugin_call_long`、
`prompt_append_blocks`、`session_rerun`、`plugin_call_hooks`;用到的应用先查 `daemon.ping` 的 `capabilities`。

### 压缩的首尾两帧(#103,`compact_events`)

每次改写历史的压缩(会触发 `PreCompact` / `PostCompact` 的那种)动手前先发 `compact_started {trigger, tokens_before,
strategy?}`,`trigger` 是 `threshold`、`manual`(`/compact`、`session.compact`)或 `overflow`(请求因体积被拒后压缩重发)。
压成了照旧以 `compact` 收尾;没压成以 `compact_ended {outcome, reason?}` 收尾,`outcome` 是 `declined`(`PreCompact` hook
拒绝)、`nothing_to_compact`、`failed`(`reason` 写原因)或 `cancelled`。开始了就一定有且只有一个结束帧。`strategy` 只在
指定了 `at` 的手动压缩里有(总是 `FullCompact`),其余要压完才知道。修剪(`trim: true`)不发新帧。子 Agent 侧链里的
压缩经 `subagent_progress` 转出。
压缩换掉历史之后才到的中断(或 future 被丢掉)以 `compact` 结尾,不是 `compact_ended{cancelled}`;`compact` 帧可能先于
压缩记录写进会话日志,此后的 `step_end` / `turn_complete(persisted)` 时日志里一定有它。

### 子 Agent 的询问带任务(#104,`prompt_task`)

`prompt` 帧的三种 `prompt_type` 都带可选的 `task_id` 与 `agent_session_id`:询问来自子 Agent 时是它的任务与侧链会话
(宿主 `session.startAgent` 起的,等于返回的那两个值);子 Agent 里的权限询问也带 `tool_use_id`(子会话里那次调用的)。不再需要解析 `message` 的
`[sub-agent <id>]` 前缀(它不是契约)。子 Agent 再起的子 Agent 发问时,`agent_session_id` 与 `tool_use_id` 指最里层发问的
那个会话和它的调用(`session.get` / `session.history` 查得到),`task_id` 仍是宿主会话里最外层子 Agent 的任务;常驻团队
成员发问不带 `task_id`。

### 宿主起的子 Agent 的预算(#105,`agent_token_budget`)

- `session.startAgent` 收可选的 `token_budget`(口径同 `turn.token_budget`;`0` 以 `INVALID_PARAMS` 拒绝,不登记任务)。
  花费照常计入会话,下一次调用受链上剩得最少的那个限制,所以只能比会话的余额紧。
- 用完(或剩下的放不下下一次调用)时子 Agent 在下一次调用前停下,`task_finished.task`(与 `session.tasks`)是
  `status: "failed"`、`error: "budget_exceeded: <那一轮的 turn_complete.message>"`,如
  `budget_exceeded: the task's token budget of N is used up` 或 `… has L left, too little for the next call's input of
  about I tokens and its reply`;是谁的写在开头:`the task's`、`this session's`、`this turn's`。停之前说过的话留在任务输出里。
  父会话与别的子 Agent 不受影响。模型用 `Agent` 工具起的子 Agent 仍以 `done` 结束,输出末尾的说明同样写明是谁的预算。
- **行为变化**:宿主起的子 Agent 被任何预算截停(包括会话的预算)都以 `failed` 结束,`error` 写明是谁的预算;0.4.3 里被
  会话预算截停的是 `done`。模型用 `Agent` 工具起的子 Agent 不变。

### `plugin.call` 可以跑十分钟(#106,`plugin_call_long`)

宿主调 `plugin.call` 的 `timeout_ms` 缺省仍是 120000,上限由 120000 放到 600000(同 `Bash`),这次运行的时限就是给的值。
模型调的插件命令仍是两分钟。能力位只在带插件函数的构建里报。

### `prompt.append` 可以分块(#107,`prompt_append_blocks`)

`prompt.append`(`options.settings`、`session.setPrompt`、设置文件)除字符串外也收 `[{"name": "...", "text": "..."}]`:
每项一个系统块 `host.<name>`,次序照数组,整组在 `config.prompt_append` 的位置,缓存断点不变。`session.inspectContext`
里 `origin` 为 `{"host": "<name>"}`、`tokens` 各算各的;`session.call` 的 `request.system[]` 里 `source` 为
`host.<name>`、`origin` 为 `host:<name>`。

- `name` 非空、只含 `[A-Za-z0-9_.:-]`、组内不重复;`text` 非空;`[]` 什么都不加。不合的在 `session.create` 以
  `INVALID_PARAMS` 失败(`invalid_options` 字段 `settings.prompt.append`),在 `setPrompt` 同样;设置文件里的告警并忽略。
- 块名按给出这个数组的层:会话层(`options.settings`、`session.setPrompt`)与宿主/命令行层(`agixd --set`、
  `EmbedConfig.settings`、嵌入方在内存里改的设置)是 `host.<name>` / `{"host": "<name>"}` / `host:<name>`;设置文件
  (全局、场景、项目,未受信的项目也一样)与 managed 层是 `config.<name>` / `{"config": "<name>"}` / `config:<name>`。
  条目名不能是 `prompt_append`、`prompt_override`。
- 给字符串的照旧是一块 `config.prompt_append`。分层照旧是上层整体替换下层,不按项合并;managed 层也可以写数组。
- 脚本贡献的提示块不能再用 `host.` 或 `config.` 开头的名字。

### 经 daemon 重跑录像里的一次调用(#108,`session_rerun`)

新方法 `session.rerunCall {session_id, index, generation?, edits?, provider?, model?}`:取会话录像里第 `index` 次调用的请求,
套上 `edits`(`system: [{index | source, text | null}]` 换或删系统块,`drop_messages: [n]` 删消息;位置都按录下的请求算)
发给真实 provider,答 `{session_id, index, generation, provider, model, response, diff}`。`response` 同 `session.call`
的形状;`diff` 分 `text`、`tool_uses`、`stop_reason`、`usage` 报与录下回答的差别。

- 只读录像:不写录像、不改会话、不计入会话的花费;会话不在内存里也能跑。经会话自己的 provider 路由(从 `provider`
  起,缺省是录下那次的 provider),健康与配额照常记。
- 一个录像目录只有一代;`generation` 是用来确认录像在这之后没有被重写的,对不上时以 `INVALID_PARAMS` 失败并在
  `data.generation` 给出现在的值(同 `session.call`)。
- 没有录像、编辑指不到(越界、重名或不存在的 `source`、`index` 与 `source` 同给或都不给、同一块编辑两次、删光所有消息)、
  录像里的 provider 已不在设置里且没给 `provider`:`INVALID_PARAMS`,什么都不发。`edits.system[].text` 必须给(`null` 即删除)。provider 报错时照常返回,
  `response.status` 为 `"error"`。

### 不带会话的 `plugin.call` 与连接登记的 hook(#109,`plugin_call_hooks`)

0.4.3 里连接用 `daemon.setHooks` 登记了 hook 之后,不带 `session_id` 的 `plugin.call` 会一直等到自己的超时:临时引擎照样问
hook,询问却发不到任何人、也答不回去。现在宿主登记的 hook 对这种调用照样生效(否则任何一条连接都能借它绕开别的宿主设的
`PreToolUse`):

- 登记者收到 `hook_call`,`session_id` 与 `input.session_id` 是这次调用的临时会话 id,多一个
  `plugin_call: {function, run_id?}`;照常用 `session.respondHook {session_id, call_id, …}` 作答,只有登记它的连接能答。
  带会话的调用的帧不变。
- 调用结束后临时会话 id 即失效,迟到的回答得到 `SESSION_NOT_FOUND`;临时会话不进会话列表。
- 登记者在调用中途断开,或询问根本送不到时,立刻按 hook 的 `on_error` 了结(`block` 即拒绝、`allow` 即放行),不再等满时限。

### `session.close` 不再删侧链(#110,`Agix-v0.4.4-2026112405` 起)

`session.close` 只把会话移出内存、停下它的引擎,不再级联删除侧链。父会话 close 之后,宿主 `session.startAgent` 起的
与模型 `Agent` 工具起的子 Agent 的侧链照样在磁盘上:`session.list {parent_session_id}` 列得出,`session.get` /
`session.history` 读得到。要连侧链一起删用 `session.delete`(级联照旧)。`session.close` 结果里的
`sidechains_deleted` 恒为 `0`。靠 `close` 清理磁盘的宿主改调 `delete`。

`session.list {parent_session_id}` 的每一条带真的 `message_count`、`created_at`(第一行的写入时刻)与
`last_active`(最后一行的),从那个子会话的日志读出;其余列表照旧不读日志、这几个字段为空。

### 宿主不经会话调 MCP 工具(#111,`mcp_call`,`Agix-v0.4.4-2026112501` 起)

三个新方法,用 Agix 已有的 MCP 连接、设置、凭据、MCP 策略与项目信任,不开会话:

- `mcp.tools {server?, scene?, project_root? | session_id?}`:每台 server 的状态与工具(`input_schema`、
  `output_schema`、`annotations`、规则与 hook 认的 `tool_name` 即 `mcp__<server>__<tool>`)。带 `session_id` 时
  正好列出那个会话调得了的。
- `mcp.callTool {server, tool, arguments?, scene?/project_root? | session_id?, timeout_ms?, run_id?}`:
  `{ok: true, is_error, content, text, structured?}`,`content` 是 MCP `CallToolResult.content` **原样**(camelCase,
  不转成 Agix 的块),工具自己报错是 `is_error: true` 仍 `ok: true`;没有结果时 `{ok: false, reached_server, error:
  {code}}`,`code` 与 `plugin.call` 同一套(`refused`、`answered_by_middleware`、`timeout`、`cancelled`、`failed`,
  开放集合)。`reached_server: true` 时 server 可能已经照做了,不幂等的工具不要盲目重调。时限缺省 120 秒、
  至多 600 秒,server 自己的 `tool_timeout_ms` 更短时以它为准。
- `mcp.cancel {run_id}`:与 `plugin.cancel` 共用这条连接上的 `run_id`,两者都能取消任何一种宿主调用。

规矩与会话里调这个工具一样:deny 规则(`mcp__feishu__x`、`mcp__feishu`)、plan 模式、managed 的只能本人答的
ask 拦下(`refused`),会话要问的视同宿主批准;`PreToolUse` / `PostToolUse`、`daemon.setHooks` 的 hook 照问
(不带会话时 `hook_call` 带 `mcp_call: {server, tool, run_id?}`);未受信项目的 hook 不跑、自己的 server 不启动。
场景须是现在建得了会话的。server 发起的提问一律答 `cancel`。每次调用都问到 server,不用工具输出缓存。
server 不可调用时是新错误码 `MCP_SERVER_UNAVAILABLE`(-32029),`data: {server, state, error?}`,`state` 取
`mcp.status` 的状态(开放集合)。每次有结果的调用向 `daemon.subscribeEvents` 发 `mcp_tool_called`(带
`outcome`、`reached_server`、`duration_ms`,从不带参数与结果)。

**`plugin.call` 的一处变化**:是不是中间件作答改用与 `mcp.callTool` 同一套判定(这一次调用里 `tool.around`
交回的、`tool.result` 是否改了文本、图片或错误标记),判得更准;以前漏判、被改写过的结果照样当函数结果交回的,
现在是 `answered_by_middleware`。

### 嵌入(`agix` crate)

- `AgentEvent` 多 `CompactStarted { turn_id, trigger: CompactCause, tokens_before, strategy }` 与
  `CompactEnded { turn_id, outcome: CompactOutcome, reason }`;`PermissionPrompt`、`QuestionPrompt`、
  `McpElicitationPrompt` 多 `agent_session_id: Option<String>`。对它们穷举 `match` 或按字段构造的要补。
- `base::interface::budget_policy::Scope::Task` 与 `SpendNode::task(limit, session)`;`SessionPool::start_agent` 多第三个
  参数 `token_budget: Option<u64>`。
- `DelegateEnd::CutShort` 多第二个字段:`CutShort(&'static str, Option<String>)`,是那一轮的停止原因;匹配 `CutShort(_)` 的改成
  `CutShort(..)`。
- `ptc::PluginCallTool::time_limit()` 是宿主调用的上限(600 秒),新增 `default_time()`(120 秒)。
- 提示词:`settings::PromptAppend` / `AppendBlock`(`agix::contracts::prompt` 导出);`PromptChange.append` 是
  `Option<Option<PromptAppend>>`,`PromptChoice.append` 是 `Option<PromptAppend>`;自己实现装配器的,
  `AssemblyRequest.prompt_append` 与 `managed_append` 由 `Option<&str>` 改为拼好的 `&[PromptBlock]`;`BlockOrigin` 多
  `Host`(算本地来源)与 `ConfigEntry { config }`(设置文件与 managed 层给的数组项),`names::CONFIG_PREFIX`;
  `PromptAppend::blocks(by: Tier)` 取给出它的层,`PromptSettings::append_blocks` 换成 `LoadedSettings::prompt_append_blocks`
  与 `prompt_append_tier`。`SystemPromptSettings` 删掉没人读的 `append` 与 `override_text`。
- `SessionPool::rerun_call`;`telemetry::recorder::rerun::rerun_one` 总是返回一份结果(收到的事件、实际的 provider、
  错误、首个事件的时间、时长与差异),中途出错时不再丢掉已收到的事件;新增 `request_in`。
- 直接依赖 `base` 构造 `ElicitRequest` 的补 `agent_session_id: None`;`session::transcript::Transcript::replace_messages_after_compact`
  改为同步,写盘另由 `write_compact_marker().await` 做。
- `PluginCall.run_id: Option<String>`(`PluginCall::new` 给 `None`,按字段构造的要补);`SessionEvent::FromPluginCall(Arc<PluginCallOrigin>,
  AgentEvent)` 与 `agix::session::PluginCallOrigin { function, run_id }`;`runtime::agent::Agent::serve_host_answers()`;
  `SessionPool::respond_hook` / `respond_tool_call` / `report_tool_progress` 在调用进行时也认临时会话 id。
- `SessionPool::shutdown_session` 返回 `Result<(), PoolError>`(以前是删掉的侧链数),不再删侧链。
- #111:`HostToolRunner` 多一个必须实现的 `tool(name) -> Option<Arc<dyn Tool>>`,`run` 改为返回
  `HostCallOutcome { outcome, by_middleware, stopped }`;`SessionEvent::FromPluginCall` 改名 `FromHostCall(Arc<HostCallOrigin>,
  AgentEvent)`,`HostCallOrigin { Plugin, Mcp }` 与 `McpCallOrigin` 由 `agix` 导出;`mcp` crate 的 `McpToolMeta.output_schema`、
  `McpToolHints.open_world`、`McpCallResult.raw_content` 是新字段(按字段构造的要补),`McpManager::tool_adapters()` 返回
  `Vec<Arc<dyn Tool>>`;新增 `SessionPool::mcp_tools` / `call_mcp_tool`、`PoolError::McpServerUnavailable`、
  `DaemonEvent::McpToolCalled`。
