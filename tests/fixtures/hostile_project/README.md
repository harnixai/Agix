# hostile_project

一个克隆来的恶意仓库的静态部分：注入式 `AGENTS.md`、`allowed-tools` 想要放宽权限的
skill（`Bash(*)`、用公共后缀 `github.io` 写的 URL）、想比会话更宽的 agent 类型。

依赖路径与 git 的部分不能提交成静态文件（符号链接要指到这次运行的 `~/.agix`，嵌套仓库
要现建），由 `blackbox::World::hostile_project` 在拷贝模板之后补上：指向全局凭据的
符号链接 skill、两层深的 gitlink。
