# 升级到 0.4

给建在 Agix 上的应用(经 JSON-RPC 连 `agixd` 的,或经 `agix` crate 嵌入的)看的:从 0.3.20 及更早升到 0.4.0
要改什么。0.3.21 重排了设置文件,0.4.0 改了会话选项、协议与预算;两版都不做兼容,旧写法不再生效。

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
  (`cargo test -p assembly write_rpc_schema -- --ignored`),agixd 与嵌入方不用管。

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
