# 设置手册

`settings.json` 的每一个键:缺省值、在哪一层决定、未受信项目能对它做什么、改了何时生效。
生成的 [JSON Schema](schemas/settings.schema.json) 给编辑器补全与校验;本文给规则。
最常用的键在 [README](README.zh-CN.md) 里也有摘要。

一个最小可用的例子(每个键都可省略,省略即取缺省):

```json
{
  "providers": [
    {
      "id": "deepseek",
      "vendor": "deepseek",
      "api": "openai_chat",
      "base_url": "https://api.deepseek.com",
      "api_key_env": "DEEPSEEK_API_KEY",
      "models": ["deepseek-chat"],
      "tiers": { "strong": "deepseek-chat" },
      "quota": { "day": 5000000 }
    }
  ],
  "model": { "order": ["deepseek"], "effort": "high", "tasks": { "subagent": "fast" } },
  "permissions": {
    "mode": "acceptEdits",
    "allow": ["Bash(git status:*)"],
    "deny": ["Read(~/.ssh/**)"]
  },
  "sandbox": { "network": { "mode": "deny_all" } },
  "turn": { "token_budget": 400000 },
  "agents": { "max_depth": 2 },
  "web": { "allowed_domains": ["docs.rs"] },
  "mcp": { "servers": { "gh": { "enabled": false } } },
  "app": { "history": "project" }
}
```

## 设置从哪来

文件层:`~/.agix/settings.json`(全局)、`~/.agix/scenes/<scene>/settings.json`(场景)、
`<项目>/.agix/settings.json`(项目),每个都可以再带一个 `settings.local.json`,优先于它旁边的文件。
文件之上是 `cli` 层(`agixd --set`,或嵌入方的 `EmbedConfig.settings`)、宿主在内存里改的值、
会话自带的 `options.settings`,最上面是部署方的 managed 文件。

值可能从哪来,优先级从低到高:

| 入口 | 谁写 | 作用到 | 约束 |
|---|---|---|---|
| 内建默认 | 引擎 | 全部 | — |
| `~/.agix/settings.json`(+ `.local`) | 用户 | 所有项目、所有场景 | 文件层里只有它能写 `app` |
| `~/.agix/scenes/<scene>/settings.json`(+ `.local`) | 用户 | 这个场景 | — |
| `<项目>/.agix/settings.json`(+ `.local`) | 项目 | 这个项目 | 用户信任之前只能收紧 |
| `cli` 层:`agixd --set`、`EmbedConfig.settings` | 启动 daemon 的人 | 所有项目、所有场景 | 不受项目信任限制 |
| 宿主在内存里改的值(`HostOverrides`) | 嵌入方 | 守护进程自己那一对照原样;别的对只带宿主新加的 | — |
| `session.create` 的 `options.settings` | RPC 客户端 | 这个会话,连同它的子 Agent、团队成员 | 只收会话作用域的键;不开 `permissions.client_may_loosen` 只能收紧 |
| managed 文件 | 部署方 | 全部 | 压过以上各层;有松紧次序的键下层更严时留下层的;读不了就拒绝所有工具调用 |

## 层与层怎么合

- 对象逐键合并;高层写的标量或列表替换低层的(项目里的 `model.order` 替换全局的)。
- `providers` 是定义,各层相加;同一个 id 先读到的为准,后面重复的忽略并告警。
- `permissions` 的规则列表(`allow`、`allow_exact`、`ask`、`deny`)不合并:各层规则并列生效,deny 压过任何 allow。
- `hooks` 按事件合并;`mcp.servers` 按名字、按字段合并。
- `app` 只认全局层、`cli` 层与 managed 文件,scene、项目里写了被忽略并告警。
- `cli` 层按全局层的规矩合并,不受项目信任限制;`config.reload` 重读的是它下面的文件。
- managed 文件能写任何键(外加 `permissions.disable_bypass`),压过各层;它的规则列表与各层并列,
  deny、ask 在 `bypassPermissions` 下照样生效。
- 用户没信任的项目只能收紧:下面每张表的「未受信项目」一列说明每个键遇到未受信项目时怎么处理。
  信任怎么给、`project.trust` 怎么写见 [ARCHITECTURE.md](ARCHITECTURE.md) 的项目信任一节。

## 写错了怎么办

坏设置按谁写的分别对待:谁在写、谁在场,就给谁报错;文件只告警;安全文件按失败即关闭处理。

| 写在哪 | 不认识的键、改过名的键、放不进的值 |
|---|---|
| 设置文件 | 告警并忽略,`daemon.doctor` 与 `config.explain` 能看到;改名的键,告警会指出新名字 |
| `agixd --set`、`EmbedConfig.settings` | 启动失败 |
| `options.settings` 与其他 RPC 参数 | 这个请求以 `INVALID_PARAMS` 失败 |
| managed 文件 | 一个都不套,拒绝所有工具调用 |

值按字面取:`$VAR` / `${VAR}` / `${VAR:-默认}` 只在 MCP server 的 `command`、`args`、`env`、`url`、`headers` 里展开。

## 为什么是这个值

- `config.explain {key}`(也可以给一个块,如 `turn`;加 `session_id` 看会话的)逐层列出这一层写了什么、结局如何:
  生效、被更高层盖过、未受信项目被扣下、只认全局层而被丢掉、值放不进、被 managed 文件改掉,或在会话上被夹住。
- `config.get` 读各层(`tier`)或合并后的值;`daemon.doctor` 汇总加载时的告警、被忽略的键与各层的来源。
  三者的参数与结果见 [daemon_rpc_protocol.md](daemon_rpc_protocol.md) 的 `config.*` 与 `daemon.doctor`。
- `session.create` 的 `model`、`provider` 不是设置,只选这个会话的对话用哪个模型;推理强度、思考开关与提示词是设置
  (`options.settings` 的 `model.effort`、`model.thinking`、`prompt.append` / `prompt.override`)。

## 怎么读下面的表

- **缺省**:内建默认值;「无」是没有值。写成一个反引号字面量的,有测试对着内建默认核对。
- **作用域**:进程——启动时或建池时读一次;项目与场景——绑定一对(项目,场景)时读,对其上所有会话生效;
  会话——建会话的引擎时读,`options.settings` 只能设这一档的键。
- **未受信项目**:照用——未受信项目写的照常生效;只能收紧——只有不比下层宽松的值才保留(括号里是「更紧」的含义);
  信任之后才生效——未受信项目写的整个不用;只认全局文件层、cli 与 managed——别的层写了丢掉并告警。
- **改后**:立即——`config.reload` 后(会话作用域的键在新会话或引擎重建时);要重启——daemon 启动时读一次,
  `config.reload` 不改它。

## 各块

### `model`

`model` 选哪个 provider 回答、每类调用要哪一档、思考与推理强度。provider 本身在 `providers` 里定义。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `model.order` | 无(第一个定义的 provider) | 项目与场景 | 只能收紧(只能列下层已定义的 provider) | 立即 | 模型调用依次尝试的 provider id;首个是缺省,前一个额度用尽、没有 key、拒绝 key 或重试耗尽仍连不上时换下一个,每次调用都从首个起。每个 id 都必须已定义 |
| `model.tasks.main` | `strong` | 项目与场景 | 照用 | 立即 | 对话本身要的档位(`strong`、`balanced`、`fast`) |
| `model.tasks.subagent` | `balanced` | 项目与场景 | 照用 | 立即 | 没有自带模型的子 Agent 要的档位 |
| `model.tasks.compact` | `fast` | 项目与场景 | 照用 | 立即 | 压缩摘要要的档位 |
| `model.tasks.memory` | `fast` | 项目与场景 | 照用 | 立即 | 提取记忆要的档位 |
| `model.tasks.title` | `fast` | 项目与场景 | 照用 | 立即 | 给会话起名要的档位 |
| `model.tasks.web_search` | `fast` | 项目与场景 | 照用 | 立即 | `WebSearch` 要的档位 |
| `model.tasks.web_fetch` | 无 | 项目与场景 | 照用 | 立即 | `WebFetch` 读页面时要的档位;不设则不用模型,直接返回页面文本 |
| `model.effort` | `high` | 会话 | 照用 | 立即 | 推理强度:`low`、`medium`、`high`、`xhigh`、`max`,作为 `output_config.effort` 发出 |
| `model.thinking` | `on` | 会话 | 照用 | 立即 | 思考开关:`auto`、`on`、`off` |
| `model.max_tokens` | `32000` | 会话 | 照用 | 立即 | 一次调用的 `max_tokens`;受 `providers[].models[].max_output` 封顶 |

### `providers`

`providers` 是定义列表:各层相加,同一个 id 先读到的为准,后面重复的忽略并告警。没有定义任何 provider 时引擎自带 `anthropic`(Anthropic 官方 API,key 取自 `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` 或宿主)。路由与档位的用法见 README 的「模型路由」。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `providers` | 无(内建 `anthropic`) | 项目与场景 | 信任之后才生效 | 立即 | provider 列表,每项的字段见下 |
| `providers[].id` | 必填 | — | — | — | 唯一,只含 ASCII 字母、数字、`-`、`_` |
| `providers[].vendor` | `generic` | — | — | — | `anthropic`、`openai`、`deepseek`、`qwen`、`vllm`、`sglang`、`generic`;决定思考怎么开、推理内容是否回传、强度怎么写 |
| `providers[].api` | `anthropic` | — | — | — | 协议:`anthropic`、`openai_chat`、`openai_responses`,或宿主注册的协议 |
| `providers[].base_url` | 无 | — | — | — | 端点;`openai_chat` 必填,`anthropic` 不写走官方 API,`openai_responses` 走 OpenAI |
| `providers[].api_key_env` | 无 | — | — | — | 放 key 的环境变量;不写则向宿主的凭据来源要。设置文件点名的变量不会被子进程继承,除非 `sandbox.env.allow` 列了它 |
| `providers[].models` | `[]` | — | — | — | 这个 provider 提供的模型,每项是 id 或带 `context_window`、`max_output`、`reasoning_levels`、`input_types` 的对象;空表示任何 id 都接受 |
| `providers[].models[].max_output` | 按模型名查 | — | — | — | 调用这个模型时 `max_tokens` 最多发多少,回复在上限处被截断后重试也最多抬到这么多 |
| `providers[].tiers` | `{}` | — | — | — | `strong`、`balanced`、`fast` 各由哪个模型担任;`strong` 必填,`balanced` 缺省取 `strong` 的,`fast` 缺省取 `balanced` 的 |
| `providers[].quota` | 无(不限) | — | — | — | `day`、`week`、`month` 三个周期的 token 上限(未命中缓存的输入 + 缓存写入 + 输出,缓存命中不计,按本地时间的 0 点、周一、1 号起算);用尽的 provider 到下个周期前被跳过 |

### `permissions`

规则语法与模式的语义见 [ARCHITECTURE.md](ARCHITECTURE.md) 与 [daemon_rpc_protocol.md](daemon_rpc_protocol.md) 的权限一节。规则列表不按层合并:各层规则并列生效,deny 压过任何 allow;未受信项目的 allow 规则(`allow`、`allow_exact`)被加载器丢掉。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `permissions.mode` | `default` | 会话 | 只能收紧(不得比下层的模式更宽) | 立即 | 权限模式:`default`、`acceptEdits`、`plan`、`dontAsk`、`bypassPermissions` |
| `permissions.allow` | 无 | 会话 | 照用 | 立即 | 不问就放行的规则,如 `Bash(git status:*)`、`Read(~/notes/**)`、`mcp__server__tool` |
| `permissions.allow_exact` | 无 | 会话 | 照用 | 立即 | 内容按字面比对、不当模式读的 allow 规则;"每次都允许"记在这里 |
| `permissions.ask` | 无 | 会话 | 照用 | 立即 | 先问再执行的规则,即使 allow 规则也覆盖它 |
| `permissions.deny` | 无 | 会话 | 照用 | 立即 | 拒绝的规则,压过任何 allow 与 ask |
| `permissions.prompt_timeout_secs` | `300` | 会话 | 只能收紧(只能更小(0 表示不限,最宽)) | 立即 | 权限提问没人答多久后拒绝;`0` 一直等(只有取消会话能打断) |
| `permissions.client_may_loosen` | `false` | 项目与场景 | 只能收紧(只能设为 false) | 立即 | RPC 客户端能否经 `options.settings` 要求比这里更宽的设置;`false` 时更宽的值被夹到这里的值,并列在 `clamped_options` 里 |

### `files`

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `files.extra_dirs` | 无 | 会话 | 只能收紧(只能是下层的子集) | 立即 | 项目之外、工具可以像项目一样读写的目录;实例自己的状态目录始终可读,但只有其中的 `scratchpad` 与 `memory` 能这样写 |
| `files.read_dirs` | 无 | 会话 | 只能收紧(只能是下层的子集) | 立即 | 项目之外、文件工具与只读 shell 命令不问就能读的目录;`"/"` 即任意处可读。其余路径要人批准,无人值守的会话直接拒绝 |
| `files.allow_write` | 无 | 会话 | 只能收紧(只能是下层的子集) | 立即 | 在内建的写入拒绝名单之上重新放开的路径,最具体的赢;为带 `.env.example` 或私有仓库 `.npmrc` 这类正常流程留口子 |
| `files.deny_write` | 无 | 会话 | 只能收紧(只能是下层的超集) | 立即 | 在内建凭据名单(`.env`、`id_rsa`、`.ssh`、`.npmrc` 等)之上追加的、工具绝不能写的文件名片段;路径任一分量相同或以它加点开头即拒,沙箱里的命令在可写根内同样受限 |

### `sandbox`

`Bash` / `Monitor` 命令运行所在的 OS 沙箱。`sandbox.network.mode` 的取值与限制见 README。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `sandbox.enabled` | `true` | 会话 | 只能收紧(只能设为 true) | 立即 | `false` 时命令不进 OS 沙箱;没有沙箱后端的构建里它不起作用 |
| `sandbox.require` | `false` | 会话 | 只能收紧(只能设为 true) | 立即 | 平台没法真正强制策略(没编进后端,或 Linux 没装 `bwrap`)时拒绝执行命令,而不是不受约束地执行 |
| `sandbox.allow_read` | 无 | 会话 | 只能收紧(只能是下层的子集) | 立即 | 在内建凭据拒读名单(`~/.ssh`、`~/.aws`、`~/.npmrc` 等)之上重新放开的路径,只放开点名的那一项;不能放开被 `Read(...)` deny 规则点名的路径 |
| `sandbox.deny_read` | 无 | 会话 | 只能收紧(只能是下层的超集) | 立即 | 沙箱里命令不得读的路径,追加到内建凭据名单上;点名文件或目录的 `Read(...)` deny 规则会一并加入,别的 glob 不会 |
| `sandbox.network.mode` | `unrestricted` | 会话 | 只能收紧(deny_all 最严,其次 allowlist 与 proxy(两者不能互换),unrestricted 最宽) | 立即 | 沙箱里命令的出网:`unrestricted`、`deny_all`、`allowlist`(读 `web.allowed_domains`,但后端不按域名放行)、`proxy`;引擎自己的 `WebFetch` / `Ping` 不受它约束 |
| `sandbox.network.proxy` | 无 | 会话 | 信任之后才生效 | 立即 | `proxy` 模式下命令唯一能连的地址:`"127.0.0.1:3128"`,或 `{"http", "socks", "no_proxy"}`;经 `HTTP_PROXY` 等变量告诉命令。代理放在本机;不在本机时 macOS 只能按端口限,Linux 要 `socat`,没有就整个断网 |
| `sandbox.env.mode` | `inherit` | 会话 | 只能收紧(allowlist 比 inherit 严) | 立即 | 命令的环境变量:`inherit` 继承宿主的、去掉引擎自己的凭据;`allowlist` 只给 shell 能工作所需的加上 `allow` |
| `sandbox.env.allow` | 无 | 会话 | 只能收紧(只能是下层的子集) | 立即 | 无论模式都放行的变量:`allowlist` 下是多保留的名字,`inherit` 下是有意把引擎自己的凭据(provider 的 `api_key_env`、MCP 配置读的变量)交给每个子进程的办法 |
| `sandbox.cache_dirs.<工具链>` | 无 | 会话 | 只能收紧(只能是下层的子集) | 立即 | 工具链名 → 缓存路径列表,让沙箱里的构建工具能写自己的缓存 |
| `sandbox.reap_on_close` | `false` | 会话 | 只能收紧(只能设为 true) | 立即 | 会话结束(关闭、被逐出、daemon 关停)时停掉它的命令起的、仍在跑的所有进程组,包括有意留在后台的服务;关着时那些服务比会话活得久 |
| `sandbox.unattended_needs_network_policy` | `true` | 会话 | 只能收紧(只能设为 true) | 立即 | 无人值守的会话里,没有 allow 规则的 `Bash` / `Monitor` 命令要沙箱完全兜得住、并且 `network.mode` 不是 `unrestricted` 才执行;设为 `false` 去掉网络这一条,出网不受限时沙箱兜得住的命令照样执行 |

### `web`

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `web.allowed_domains` | `[]` | 会话 | 只能收紧(只能是下层的子集) | 立即 | `WebFetch`、`Ping` 能访问的主机(域名也覆盖子域);也是 `sandbox.network.mode: "allowlist"` 读的名单。名单里的主机即使解析到本机或内网地址也放行——写 `127.0.0.1` 才能让 `WebFetch` 访问本地服务 |

### `turn`

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `turn.max_model_calls` | `25` | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 一轮里模型往返的上限,超出以 `max_turns` 结束这一轮;是防失控的闸,不是任务预算 |
| `turn.token_budget` | 无(不限) | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 一轮能花的 token,口径、估计与超限行为见 README;会话只能经 `options.settings.turn` 调低它 |
| `turn.parallel_tools` | `10` | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 模型一次回复里的工具调用同时跑几个 |

### `session`

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `session.token_budget` | 无(不限) | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 一个会话从创建起能花的 token,口径同 `turn.token_budget`,跨 daemon 重启累计;花完后每一轮都以 `budget_exceeded` 结束,不再调模型 |

### `goal`

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `goal.max_rounds` | `10000` | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 模型设或改的目标(`GoalCreate`、`GoalUpdate`)最多几轮,给多了压到它;宿主设的目标不受限 |

### `schedule`

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `schedule.max_model_jobs` | `20` | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 一个会话里模型自己经 `CronCreate` 建的、还在的定时任务最多几个;`0` 即模型不能建。宿主经 `schedule.create` 建的不计 |
| `schedule.min_model_interval_minutes` | `1` | 会话 | 只能收紧(只能更大) | 立即 | 模型建的周期任务两次触发至少隔几分钟;一次性任务不受它限制 |

### `agents`

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `agents.max_depth` | `3` | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 子 Agent 链最多委派几层:宿主建的会话是第 0 层,到这一层的 Agent 拿不到 `Agent` 工具;团队成员同样适用 |
| `agents.max_concurrent` | `6` | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 一个会话下同时在跑的子 Agent 上限(前台、后台、恢复的,各层深度合计);超出的派生被拒。团队成员另受 `team.*` 限制,不计入 |

### `team`

团队工具(`TeamCreate` / `TeamList` / `TeamDelete`)只注册给场景支持团队的会话(`code`、`harnix`)。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `team.enabled` | `false` | 会话 | 只能收紧(只能设为 false) | 立即 | 是否给会话注册团队工具;关着时它们的提示词与 `Agent` 工具里的 `team` / `name` 字段也不出现 |
| `team.stage_concurrency` | `6` | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 一个阶段里 `TeamCreate` 的子 Agent 同时派几个 |
| `team.max_members` | `20` | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 一个会话里常驻团队成员的总数上限,超出就停掉最久空闲的(不停活跃的);`0` 即不允许常驻成员 |
| `team.member_idle_secs` | `1800` | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 常驻成员空闲这么久(自上一轮结束起没有消息)就停掉 |

### `compaction`

未写的字段取场景自己的值。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `compaction.threshold_tokens` | 无(场景定:150000) | 会话 | 照用 | 立即 | 请求的上下文超过多少 token 就压缩;`0` 关闭自动压缩 |
| `compaction.keep_recent` | 无(场景定:`code` 20、`harnix` 15、`mini` 10) | 会话 | 照用 | 立即 | 压缩时原样保留最近几轮 |
| `compaction.reactive` | `false` | 会话 | 照用 | 立即 | 在一轮中途提前清旧工具结果:离阈值不足 5 万 token,或上下文涨得三次模型调用内就到阈值时,把早于最近 `keep_recent` 轮(至少 5 轮)的文件、shell、搜索类工具结果清空(`MicroCompact`);连续三次没释放出东西就在本会话停用 |
| `compaction.clear_on_cache_expiry` | `false` | 会话 | 照用 | 立即 | 每过一小时(provider 提示缓存的寿命)清空除最新 20 条外的工具结果,并把 `cache_edits` 发给仍持有它们的 provider |
| `compaction.inline_result_chars` | 无(`30000`) | 会话 | 信任之后才生效 | 立即 | 工具结果超过这么多字符就存到项目 `state/tool-results/` 下的文件,模型只拿到头、尾与路径;`0` 全部留在对话里。`Read`、`WebFetch`、`TaskOutput` 自己分页,从不转存 |

### `prompt`

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `prompt.append` | 无 | 会话 | 照用 | 立即 | 追加到系统提示词末尾 |
| `prompt.override` | 无 | 会话 | 信任之后才生效 | 立即 | 整个替换系统提示词 |
| `prompt.instruction_files` | 无 | 会话 | 信任之后才生效 | 立即 | 再加几份指令文件(如项目外的 `CLAUDE.md`),会话构建时按序读入,与发现的 `AGENTS.md` 一样交给模型;根会话带 `FileChanged` hook 时会监视它们 |
| `prompt.language` | 无 | 会话 | 照用 | 立即 | 用户语言偏好,如 `zh-CN`、`ja` |
| `prompt.path_style` | 无(场景定) | 会话 | 照用 | 立即 | 文件工具向模型写路径的方式:`absolute`,或 `relative`(工具描述说明路径相对工作目录,Grep / Glob / Write / Edit 的输出也按相对路径写,录下的会话能在别的工作目录重放) |

### `hooks`

生命周期 hook:事件名 → hook 列表,每个 hook 的 `type` 是 `command`、`prompt`、`http` 或 `agent`。各层按事件合并——某一层列了某个事件,就替换低层对它的列表;不认识的事件名跳过并告警。协议、配置与事件见 [hooks.md](hooks.md)。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `hooks` | 无 | 项目与场景 | 信任之后才生效 | 立即 | 事件名 → hook 列表 |

### `scripts`

把脚本绑到扩展点,写法与可用的扩展点见 [extending_quickjs.md](extending_quickjs.md) 与 [extension_points.md](extension_points.md)。一条绑定有 `path`、`point`、`entry`,以及可选的 `timeout_ms`、`calls_per_turn`。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `scripts` | `[]` | 项目与场景 | 信任之后才生效 | 立即 | 脚本绑定列表;没有脚本引擎的构建里不起作用。绑了不存在或不允许脚本用的扩展点,启动时拒绝并说明原因 |

### `plugins`

插件包的格式、安装与信任见 [plugin_packages.md](plugin_packages.md)。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `plugins.enabled` | `true` | 进程 | 只能收紧(只能设为 false) | 立即 | 是否加载已安装的插件 |
| `plugins.allow` | 无 | 进程 | 只能收紧(只能是下层的子集(空表示全部)) | 立即 | 非空时只加载列出名字的插件(白名单,不在其上的被拒) |
| `plugins.enable_on_install` | `false` | 进程 | 只能收紧(只能设为 false) | 立即 | `plugin.install` 装好后是否直接启用;`false` 时宿主读完安装返回的披露,再自己调 `plugin.enable`;`true` 时装完即启用 |

### `mcp`

MCP server 的配置字段(`command`、`args`、`env`、`url`、`headers`、认证等)与 OAuth 见 [mcp_auth.md](mcp_auth.md)。`mcp.servers` 按名字、按字段合并;只写 `enabled` 的条目是对别处声明的 server 的开关。`$VAR` / `${VAR}` / `${VAR:-默认}` 只在 `command`、`args`、`env`、`url`、`headers` 里展开。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `mcp.servers.<名>.enabled` | 无(即开) | 项目与场景 | 只能收紧(只能设为 false) | 立即 | 开关;`null` 是坏值而不是未设 |
| `mcp.servers.<名>.<字段>` | 无 | 项目与场景 | 信任之后才生效 | 立即 | server 自己的配置字段,由 MCP 模块读取 |

### `skills`

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `skills.allow_shell` | `true` | 会话 | 只能收紧(只能设为 false) | 立即 | `false` 时 skill 正文里的 `` !`cmd` `` 与 ```` ```! ```` 块换成一行说明,不执行 |

### `memory`

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `memory.enabled` | `true` | 会话 | 照用 | 立即 | 文件记忆(`MEMORY.md` 加每条记忆一个 `.md`):`false` 时没有记忆提示、每轮提醒里的索引与召回 |

### `telemetry`

文件格式与事件见 [telemetry.md](telemetry.md)。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `telemetry.enabled` | `true` | 会话 | 只能收紧(只能设为 true) | 立即 | 每个会话在项目 `.agix/state/telemetry/` 下写一份遥测 JSONL(无项目时在状态目录的 `telemetry/` 下) |
| `telemetry.otlp_endpoint` | 无 | 进程 | 信任之后才生效 | 立即 | OTLP 端点;不设则看 `OTEL_EXPORTER_OTLP_ENDPOINT`,都没有就不导出。只有带 `otel` feature 的构建才导出 |

### `checkpoints`

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `checkpoints.enabled` | `true` | 会话 | 只能收紧(只能设为 false) | 立即 | `false` 时文件工具写入前不记录被覆盖的内容,会话也不能回退文件 |
| `checkpoints.max_bytes_per_file` | `16777216` | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 大于它的文件只记哈希,不留旧内容 |
| `checkpoints.max_total_bytes` | `536870912` | 会话 | 只能收紧(只能更小(null 表示不限,最宽)) | 立即 | 一个会话保留的副本总量超过它就删最旧的,并在会话记录里写一行 `pruned` |

### `recorder`

录制与重放模型调用,给测试与复现用。没有这个块时模型包装器是直通的。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `recorder.mode` | 无(写 `recorder` 时必填) | 项目与场景 | 信任之后才生效 | 立即 | `record`、`replay`,或 `rerun`(把录下的请求重新发给真实 provider) |
| `recorder.root` | 无(写 `recorder` 时必填) | 项目与场景 | 信任之后才生效 | 立即 | 录制放在哪;每份录制是它下面一个自成一体的目录,删掉目录即作废 |
| `recorder.name` | 无(用会话 id) | 项目与场景 | 信任之后才生效 | 立即 | 这份录制在 `root` 下的目录名;测试用它指定名字以便重放时找到 |
| `recorder.on_divergence` | `strict` | 项目与场景 | 信任之后才生效 | 立即 | 重放时实时请求与录下的不一致怎么办:`strict` 报错并指出差异字段,`warn` 报告差异后照放录下的回答 |

### `app`

`app` 只认全局文件层(`~/.agix/settings.json` 与它的 `settings.local.json`)、`cli` 层与 managed 文件;scene、项目里写了会被忽略并告警。

| 键 | 缺省 | 作用域 | 未受信项目 | 改后 | 说明 |
|---|---|---|---|---|---|
| `app.trusted_projects` | 无 | 进程 | 只认全局文件层、cli 与 managed | 立即 | 其自带配置照写的值生效的项目:绝对路径,或 glob(`*` 不跨路径分量,`**` 跨);开头的 `~/` 是家目录。全局 `settings.json` 与 `settings.local.json` 的列表相加;`project.trust` 写进后者 |
| `app.trust_all_projects` | `false` | 进程 | 只认全局文件层、cli 与 managed | 立即 | 信任所有没被 `trusted_projects` 点名的项目 |
| `app.history` | `global` | 进程 | 只认全局文件层、cli 与 managed | 要重启 | 会话放在哪:`global` 在状态目录的 `projects/` 与 `sessions/`;`project` 让受信项目的会话放进它自己的 `.agix/state/history/<id>/`,`sessions/<id>/` 只留索引 |
| `app.system_proxy` | `true` | 进程 | 只认全局文件层、cli 与 managed | 要重启 | 引擎自己的 HTTP(模型调用、`WebFetch` / `Ping`、MCP over HTTP、OAuth)是否跟随 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`;访问本机始终直连。与沙箱命令的出网无关 |
| `app.worktree_dir` | 无(`.wkts`) | 进程 | 只认全局文件层、cli 与 managed | 立即 | `EnterWorktree` 与 `Agent` 的 `worktree` 把 git worktree 建在仓库根的这个目录下,仓库的 `.gitignore` 也只加这一行;一个以 `.` 开头的路径分量,不能以 `.git` 开头,也不能是配置目录或别的工具的目录,不合规则的被忽略并告警 |
| `app.session_cap` | `32` | 进程 | 只认全局文件层、cli 与 managed | 要重启 | 内存里留几个会话;再多一个就把最久未活动的空闲会话移出内存,下次用到时从磁盘恢复 |
| `app.session_idle_timeout_secs` | `3600` | 进程 | 只认全局文件层、cli 与 managed | 要重启 | 会话没有活动多少秒后移出内存 |
| `app.worker_threads` | 无(每个 CPU 核一个) | 进程 | 只认全局文件层、cli 与 managed | 要重启 | `agixd` 的异步工作线程数,至少 1;阻塞活(文件 I/O、搜索、脚本)另有线程池。嵌入的引擎跑在宿主的运行时上,不读它 |
