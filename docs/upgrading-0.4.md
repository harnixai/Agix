# 升级到 0.4

给建在 Agix 上的应用(经 JSON-RPC 连 `agixd` 的,或经 `agix` crate 嵌入的)看的:从 0.3.20 及更早升到 0.4.0
要改什么。0.3.21 重排了设置文件,0.4.0 改了会话选项、协议与预算;两版都不做兼容,旧写法不再生效。

逐个类型、逐个函数的变化在 [`agix-sdk.md`](agix-sdk.md)「迁移说明」的 0.3.21、0.4.0 两条;协议的完整说明
在 [`daemon_rpc_protocol.md`](daemon_rpc_protocol.md)。

## 先做的检查

1. **握手看协议版本。** 0.4.0 的 `protocol_version` 是 **5**(0.3.x 是 4)。宿主应在 `daemon.auth` 这一步就
   拒绝它不认识的版本。
2. **跑一次 `daemon.doctor`。** `settings_tiers` 的每一层报出 `renamed_keys`(写了旧键的,连同取代它的新键)与
   `ignored_values`(放不进的值、不合格的 provider id 等)。旧键一律不读:管权限、沙箱、信任、MCP 开关的旧键还在时
   检查报 `failing`,其余报 `degraded`。加载时的告警也会点出新键名。

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
| `providers.<id>`(对象) | `providers[]`(列表,每项带 `id`) |
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
- **新块。** `session.token_budget`(会话从创建起的累计上限,跨重启)、`goal.max_rounds`(模型自己设的目标的轮数
  上限,缺省 10000)、`schedule.max_model_jobs`(缺省 20)与 `schedule.min_model_interval_minutes`(缺省 1):只限
  模型经 `CronCreate` 建的定时任务,宿主的 `schedule.create` 不受限。
- **provider id 只能是小写字母、数字、`-`、`_`。** 别的 id 整项忽略,`daemon.doctor` 的 `ignored_values` 说明。
- **managed 文件能写任何键,** 压过所有层(见 README「配置」的优先级表);有松紧次序的键下层更严时留下层的值。

## agixd 命令行

- `agixd --set KEY=VALUE`(可重复):值按 JSON 读,读不成 JSON 就当字符串;不认识的键、旧键、类型不对时启动失败。
  它们合成 `cli` 层,叠在每个项目、每个场景的设置文件之上,`config.reload` 之后仍在。
- `--max-tokens`、`--permission-prompt-timeout`、`--session-cap`、`--session-idle-timeout`、`--worker-threads` 是
  `--set` 的简写。`--max-tokens` 现在压过设置文件里写的 `model.max_tokens`,不再只是没写时的兜底。

## 协议(0.4.0)

### `session.create` 的 `options`

`options` 分两类:

- **只管这个会话自己的对话:** `model`、`provider`、`effort`、`thinking`(原 `thinking_mode`)、`prompt`。
  子 Agent 不继承。
- **`options.settings`:** 形状同 settings.json,作用于整个会话,连同它的子 Agent 与团队成员。
  - 只收按会话决定的键。按 (项目, 场景) 或按进程决定的(`providers`、`model.order`、`model.tasks`、`hooks`、
    `mcp`、`app`……)让请求以 `INVALID_PARAMS` 失败,`error.data.invalid_options` 逐项列出。
  - 缺省只能收紧。比设置宽的值被夹住,报在结果的 `clamped_options`(`{field, requested, applied}`);
    带 allow 的规则列表被扣下。设置开了 `permissions.client_may_loosen` 才能放宽。managed 层写了的键压过它。
  - 能力位 `session_settings`。

删掉的选项,传了以 `INVALID_PARAMS` 失败,报错里给出新写法:

| 0.3 | 0.4 |
|---|---|
| `options.turn.{token_budget, max_model_calls}` | `options.settings.turn.{token_budget, max_model_calls}` |
| `options.permission_mode` | `options.settings.permissions.mode` |
| `options.permission_rules` | `options.settings.permissions.{allow, allow_exact, ask, deny}` 的规则串 |
| `options.thinking_mode` | `options.thinking` |

0.3 的会话日志里记下的旧选项,恢复时按新形状读,限制不会因恢复变松。

### 改名

- `session.setModel` 的参数与结果:`thinking_mode` → `thinking`。
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
- **`session.get` 多一个 `usage`:** 会话从创建起的累计,含子 Agent(后台的也算),跨重启。
- **目标:** `session.setGoal` / `updateGoal` 可带 `token_budget`;目标状态多 `token_budget`、`tokens_used`。
  用完时目标转 `blocked`,`blocked_reason` 以 `token budget` 开头。
- **定时任务:** `schedule.list` 的任务多一个 `by_model`。

### 其他

- `config.reload`、`config.set`、`config.setProvider` 的结果多一个 `restart_required`:只在启动时读的键里值变了的
  (`app.session_cap` 等),managed 文件变了时还有 `managed`。这些改动要重启 daemon 才生效。
- 0.3.21 起:`provider_switched` 事件(换了 provider 及原因)与 `providers_unavailable` 错误码
  (`model.order` 里的 provider 都接不了)。

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
- `SessionOptions` 的变化同上面的 `options`:`settings` 字段,删掉 `permission_mode`、`permission_rules`、`turn`,
  `thinking_mode` 改名 `thinking`。不经 RPC 直接建会话时,会话不能给的键被略去并告警;要先检查,调
  `SessionOptions::settings_problems()`。
- provider 路由的实现从 `base` 挪进 `model::routing`(`ProviderSet`、`SessionRouter`),runtime 只见
  `base::provider::TaskRouting`;对 `ModelEvent` 穷举 `match` 的要补 `Served`。其余类型与函数的变化见
  [`agix-sdk.md`](agix-sdk.md)「迁移说明」。
