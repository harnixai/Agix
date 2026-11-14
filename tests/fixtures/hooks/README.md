# hook 脚本库

`tests/blackbox` 的用例里要的「行为各异的 command hook」，一个脚本一种行为。用例通过
`blackbox::hook_fixture("<文件名>")` 拿到绝对路径，直接当 `command` 配进 `hooks_config`。
需要往 ledger 里写、或带用例私有路径的脚本（`ledger.sh`、写 pid 的 hang、改写到某个路径的
rewrite）依赖用例的临时目录，仍在用例里现写。

| 脚本 | 行为 |
|---|---|
| `refuse_exit2.sh` / `refuse_guarded.sh` | stderr 写原因，退出码 2 |
| `approve.sh` | `{"decision":"approve"}` |
| `garbage.sh` | 打印不是 JSON 的东西，退出码 0 |
| `failing.sh` / `exit3_silent.sh` | 非 0 退出，stdout 或 stderr 有一行 |
| `hang.sh` | 读完 stdin 后 `sleep 30`，等 hook 超时被杀 |
| `block_bash.sh` / `withhold_result.sh` / `end_turn.sh` | 各自的 `block` / `continue:false` 回复 |
| `screen_prompt.sh` | 提示里有 FORBIDDEN 就拒收，否则改写 |
| `huge_stdout.sh` | 向 stdout 倒 64 MiB |
| `fork_background.sh` | 派生一个后台进程后给出 block 并退出 |
| `half_stdin.sh` | 只读 16 字节 stdin 就答 `{}` 退出 |
