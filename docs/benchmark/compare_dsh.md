# 与 DeepSeek Harness 的对比测试

Agix 拿同一组工程任务、同一个模型，和另一个开源 agent 框架 DeepSeek Harness（DSH）各跑一遍，比
花了多少 token、多久跑完、占了多少 CPU 和内存，以及做出来的东西对不对。用例在
`tests/blackbox/tests/compare_dsh.rs`，固定的项目素材在 `tests/fixtures/compare_dsh/`。README
「为什么选 Agix」里引用的数字就出自这个测试。

DSH 不在本仓库里，也不是任何 crate 的依赖：跑之前要自己用 npm 装一份，用环境变量把路径告诉
测试。这份文档讲怎么装、怎么跑、量的是什么、怎么读结果。

## 1. 准备

| 要什么 | 说明 |
|---|---|
| Node.js ≥ 24.9 | DSH 的 `engines` 要求 |
| DSH | `npm i --prefix <目录> @deepseek-ai/dsh@<版本>`，启动器在 `<目录>/node_modules/.bin/dsh`。要和已有结果对照，就装同一个版本；README 的数字用的是 `0.2.2-rc.1` |
| 模型凭据 | 和 live 测试共用工作区的 `.env`（`ANTHROPIC_BASE_URL`、`ANTHROPIC_AUTH_TOKEN`、`ANTHROPIC_MODEL` 等，见 `tests/README.md`「真实模型」）。只能是 Flash：配置里出现 Pro 模型名会直接报错 |
| release 版 `agixd` | `cargo build --release -p daemon --bin agixd`。不指定时测的是测试旁边的 debug 版，CPU 和内存读数没有意义 |
| Python 3 | 验证产物要用 |

DSH 装在仓库外面，比如一个临时目录。它的会话和配置写在每次运行自己的临时 `DSH_HOME` 里，
不会碰到你机器上原有的 DSH 配置。

## 2. 运行

```sh
npm i --prefix /some/dir @deepseek-ai/dsh@0.2.2-rc.1
cargo build --release -p daemon --bin agixd

AGIX_DSH_BIN=/some/dir/node_modules/.bin/dsh \
AGIX_TEST_DAEMON_BIN=$PWD/target/release/agixd \
AGIX_COMPARE_REPEAT=3 \
  cargo test -p blackbox --features real-model --test compare_dsh -- --nocapture
```

| 变量 | 作用 |
|---|---|
| `AGIX_DSH_BIN` | 必填，`dsh` 启动器的路径 |
| `AGIX_TEST_DAEMON_BIN` | 要测的 `agixd`，指向 release 构建 |
| `AGIX_COMPARE_REPEAT` | 每个用例跑几轮，表里报中位数，缺省 1。单次读数两边都能差 2–3 倍，下结论至少跑 3 轮 |
| `AGIX_COMPARE_CASES` | 只跑其中几项，逗号分隔，如 `stats,biglog` |
| `AGIX_LIVE_TURN_TIMEOUT_SECS` | 一轮的上限，两边共用，缺省 900 |

它不在 `real_model` 的 `live_*` 里，是一个单独的测试程序。六项跑三轮大约半小时。

## 3. 用例

每个用例先在一个空的临时项目里铺好素材，再把同样的用户消息交给两边。做完以后，用模型没见过的
输入实际跑一遍产物来判定对错。

| 用例 | 形态 | 任务 | 怎么判定 |
|---|---|---|---|
| `stats` | 短任务 | 照一份精确的规格写一个 Python 小程序 `stats.py`（计数、求和、中位数、错误行） | 用随机生成的输入文件逐条比对 stdout、stderr 和退出码 |
| `inventory` | 长流程 | 修好一个包里的三个缺陷，让测试脚本通过，再加一个 CSV 命令行入口 | 先还原测试脚本（改它没用），再跑测试；用随机操作序列核对命令行的输出 |
| `biglog` | 长输入 | 回答一个约 1.4 MB 访问日志里的问题，写成 `answer.json` | 和从同一个随机种子算出的答案比对；把整个文件读进对话的一方会在 token 上吃亏 |
| `shop` | 日常用法，同一会话两轮 | 在一个陌生的多模块项目里，先按缺陷报告定位并修复问题，再加一个优惠码功能 | 跑项目自己的测试，再用独立的参考实现核对随机购物车 |
| `audit` | workflow | 四个包、二十四个模块，其中六个违背了自己的 docstring；要求按包分给四个子 Agent 并行审查，修好后汇总成 `findings.json` | 用项目之外的契约套件逐个校验；没坏的模块必须一字未动；清单必须恰好是那六个 |
| `migrate` | 批量改动 | 三十个模块里 58 处调用从 `fetch_json(url, timeout_ms)` 迁到 `get_json(url, timeout_s=…)`，调用写法有四种 | 用新接口的桩把每个服务重放一遍，核对 URL 和换算后的秒数 |

Agix 跑的是 `harnix` 场景：无人值守，缺省权限策略，不绕过检查。DSH 跑 `--profile headless
--json`；同一用例的第二轮用 `--session-id` 接上第一轮的会话。两边都开 thinking、effort high，
表后每一行的 `request` 字段里能看到实际发出的请求参数。

## 4. 量什么、怎么量

两边各接一个录制代理（`LiveProvider`）。harness 只知道代理的地址，代理再把请求转给真实端点，
并把每个请求和响应记下来。

| 指标 | 来源 |
|---|---|
| prompt / 输出 token | DeepSeek 在每个响应里报的 `usage`，子 Agent 的调用也算在内；prompt = 未命中 + 命中缓存 + 写入缓存 |
| 模型调用、工具调用 | 代理记下的 `/v1/messages` 请求数；响应里的 `tool_use` 块数 |
| 首个带工具请求 | 第一个带工具表的请求的 prompt token，也就是固定前缀（系统提示 + 工具表）的大小 |
| 最大上下文 | 所有请求里 prompt token 最大的那一个 |
| 墙钟时间 | 从第一轮发出到最后一轮结束。DSH 的含每轮的进程启动；Agix 的 daemon 这时已经起好了 |
| CPU、内存峰值 | 内核计数：测试自己用 `wait4` 回收 harness 进程，取 rusage |
| harness 自身 CPU、进程树内存 | 每 100 ms 用 `ps` 采一次 |

两边**轮流跑、不同时跑**，谁先跑按轮次交替，免得一边的负载算到另一边头上。

**CPU 和内存。**

- **内核计数**是这两项的主要依据。rusage 里是这个进程加上它等待过的所有子进程的 user+sys
  CPU，工具命令跑得再短也算在内；内存峰值取其中最大的单个进程。
  - Agix：以单独的 `agixd` 进程跑，最后一轮结束后正常关掉再回收，所以统计的是整个生命期，
    启动在内。
  - DSH：每轮一个 node 进程，各轮相加，进程启动同样在内。
- **采样**作补充，给出两样内核计数给不了的东西：
  - 不含工具的 harness 自身 CPU；Agix 这一项从建会话开始算。
  - 某一时刻整棵进程树的内存总和。

  活不到一次采样的进程会漏掉，所以采样值是下限。

## 5. 结果怎么读

汇总表打在 stderr，每个指标是各轮的中位数；表后是每一次运行的明细。完整数据写进
`tests/output/compare_dsh/<seed>.json`（不入库），每个用例、每一边的请求原样存在旁边：

- `<seed>-<case>-<harness>.request.json`：第一个带工具的请求，用来对比系统提示和工具表。
- `<seed>-<case>-<harness>-<第几轮>.last.json`：最长的那个请求，主对话的整个过程都在里面。

断言只有两条：两边都没用到 Pro，Agix 的产物全部通过。DSH 的结果只记录，不断言。

读数时注意：

- **token 主要由调用次数决定。** 每次调用都要把整段对话重读一遍，多一次调用就多一份上下文。
  比较 token 时把调用次数放在一起看，并且至少看三轮的中位数。DeepSeek 一侧两次测试之间，
  同一用例的中位数也能差 40%–70%。
- **资源读数很稳。** CPU 和内存在三轮之间波动很小，可以直接比。
- **测量环境要写明。** 沙箱包装、虚拟机、机器上的其他负载都会影响墙钟和 CPU。发布数字时写明
  机器、构建和两边的版本。

## 6. 记录

每次对外引用的测试单独记一份，写明两边的版本、环境、方法和每一轮的读数：

| 日期 | 两边版本 | 记录 |
|---|---|---|
| 2026-11-15 | Agix 0.3.15（`e46e7804`）对 DSH 0.2.2-rc.1 | [2026-11-15-agix-0.3.15-vs-dsh-0.2.2-rc.1.md](2026-11-15-agix-0.3.15-vs-dsh-0.2.2-rc.1.md) |
