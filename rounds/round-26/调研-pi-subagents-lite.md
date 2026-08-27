# R26 前置调研 — `pi-subagents-lite` 能否充当内建子代理内核

> 调研日期：2026-08-27 · 调研方：Windows · 状态：结论已出，方向待所有者裁定
> 调研对象：`npm:pi-subagents-lite@1.13.0`（https://pi.dev/packages/pi-subagents-lite）

## 一、包身份（已核实）

| 项 | 值 |
|---|---|
| 版本 | `1.13.0`（latest，发布于 2026-08-20） |
| 许可 | MIT |
| 体积 | 59 文件 / 504,757 B unpacked |
| tarball sha1 | `f62f32622ff03c7825d48535952027e26908dac2`（与 npm registry `dist.shasum` 一致） |
| integrity | `sha512-g6kyfhC762ju1Esr7/6O3WQKrSXPtEsACC+O9UK1jLQ2d5gFTZU1F39mB8AkpuyOMHgVemZVqr4rXbmycr1fIw==` |
| 运行时依赖 | `@sinclair/typebox ^0.34.52`（唯一一条） |
| peerDependencies | `@earendil-works/pi-{ai,coding-agent,tui} >= 0.82.0` —— 钉死的 pi 0.84.2 满足 |
| 入口 | `pi.extensions: ["./src/index.ts"]`（发布 TS 源码，由 pi 直接加载） |

包体是 TS 源码 + 一条 typebox 依赖，**不是 web 技术栈**，与红线 1 无冲突；加载方式与 R15 已有的 `project-command-environment.ts`（经 `pi -e` 注入）同构。

## 二、RPC 模式实测（`vendor/pi/pi.exe --mode rpc`）

```
./vendor/pi/pi.exe --mode rpc --no-extensions -e npm:pi-subagents-lite@1.13.0
  <<< {"type":"get_state"}
  <<< {"type":"get_commands"}
```

- 退出码 0，**stderr 全空**，无加载错误；
- `get_commands` 返回中出现 `{"name":"agents","source":"extension","sourceInfo":{"scope":"temporary","source":"cli"}}` —— 扩展在 RPC 模式下确实加载成功并注册了命令；
- 副作用：`pi -e npm:<spec>` 把包解到 **`~/.pi/agent/tmp/extensions/npm/<hash>/`**。产品化时必须改为 vendor 本地路径注入，避免每次启动写用户数据目录（红线 5）。

## 三、决定性结论 —— 三条与 R26 验收正面冲突

立项文档 § 三「会话与进程模型」为 R26/R27 钉死了三条增量价值：**有会话文件（可恢复、可回看）、有进度与结果 UI、mutating 任务有 worktree 隔离**，并写明「若这三点都不做，就不该自建第二套子代理」。逐条核对源码：

### 3.1 子代理是**进程内**执行，不是独立 `pi` 子进程

`src/agents/agent-runner.ts` 使用 `createAgentSession()`（来自 `@earendil-works/pi-coding-agent`）在**当前 pi 进程内**开会话，而不是像官方 `subagent` 示例那样 spawn `pi --mode json -p --no-session`。

影响：
- Manager 侧看不到任何新进程，R25 的 **per-Runtime 进程数配额对它无效**；
- 所有子代理的上下文与并发内存都涨在**父会话那一个 pi 进程**里，一旦触发 R25 的内存硬限，**父会话会被一起打死**；
- 立项文档 § 三表格里「pi 内核 / extension 自行 spawn 的子代理」一列的描述（"Runtime 的子进程"）对本扩展**不成立** —— 它连子进程都没有，比表格里的情况更贴近父进程本身。

### 3.2 子代理会话**不落盘**

`agent-runner.ts:520`：

```ts
sessionManager: SessionManager.inMemory(cwd),
```

全仓库只有这一处 `SessionManager` 构造，没有任何 `newSession({ parentSession })` / `--session` 调用。

影响：
- **没有 pi 会话文件**，`new_session { parentSession }` 的父子落盘链路不存在；
- 应用崩溃或重启后子代理会话**不可恢复**；
- 唯一的留痕是可选的 `output_transcript` 文本日志（`/tmp/pi-agent-outputs/<agentId>.log`），可用于"回看"，但不是可 resume 的会话。

→ 直接推翻 R26 验收项「子代理会话经 `new_session { parentSession }` 建立并落盘」。

### 3.3 它的进度 / 结果 UI 在 RPC 模式下**全部失效**

扩展的富交互（`/agents` 菜单、conversation viewer、spawn wizard、运行中 steering）一律走 `ctx.ui.custom()`；实时进度块走 `setWidget(KEY, (tui, theme) => ...)` 传**组件工厂**。而钉死的 `docs/rpc.md`：

- 第 1167 行：RPC 模式下 `custom()` **返回 `undefined`**；
- 第 1295 行：`setWidget` 在 RPC 模式下 **只支持字符串数组，组件工厂被忽略**。

源码中 `grep 'mode === "tui"'` **零命中** —— 扩展没有为 RPC 模式准备任何降级路径。实测输出里也确实只看到 `{"method":"setWidget","widgetKey":"subagent-async"}` 这类**不带 `widgetLines` 的空请求**。

RPC 模式下实际还能拿到的只有：
- `Agent` / `StopAgent` / `AgentStatus` 三个工具的**调用与结果事件流**（与普通工具无异，GPUI-Pi 现在就会渲染成通用工具卡片）；
- `setStatus` 的**纯文本**状态行。

→ 「进度与结果 UI」这一条，**无论选哪个方案，GPUI 侧都必须自己实现**；扩展在这里提供不了现成能力。

### 3.4 配额归属

扩展自带 per-model / per-provider 并发槽与排队，配置在 `~/.pi/agent/subagents-lite.json`（项目级 `.pi/subagents-lite.json` 可覆盖 model 与 concurrency）。这是**扩展进程内**的配额，`RuntimeManager` 既看不到也调度不了；只能通过**写配置**间接设上限，无法做 R26 验收要求的「Manager 派发 / 排队」。

## 四、可以复用的部分（不应从 0 重写）

即便不把它当执行内核，下列资产仍有复用价值：

- `.agents/*.md` / `.pi/agents/*.md` 的 **agent 定义格式**（YAML frontmatter：`name` / `description` / `tools` / `exclude_tools` / `model` / `thinking` / `max_turns` / `max_tokens`）—— 与官方 `subagent` 示例同源，本仓库 `.agents/visual-reviewer.md` 已在用；
- 三种 **system prompt 组装模式**（`replace` / `inherit` / `custom`）；
- **watchdog** 口径（`toolTimeoutMinutes` / `idleTimeoutMinutes`，默认各 45 分钟）；
- 内建 `general-purpose` / `Explore` 两个 agent 的职责划分。

## 五、附：本次调研的环境副作用

- 首次 spike 未加 `--no-extensions`，被用户全局 `~/.pi` 扩展污染（MCP 桥接、language-guard、cross-agent-memory 等），且其中的 `python-workdir-guard` 在本 worktree 根创建了 `.venv/`。该目录由 `.git/info/exclude` 排除，不进版本库，未做删除处理。
- 用户机器上另装有 `npm:pi-subagents`（非 lite 版，`~/.pi/agent/npm/node_modules/pi-subagents`），是 CLAUDE.md「pi-subagents 审查路径」所指的那一套，与本次调研对象是不同的包，勿混淆。
