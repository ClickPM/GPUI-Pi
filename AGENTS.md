<!-- 本文件由 CLAUDE.md 镜像而来，改动请同步两份。 -->

# CLAUDE.md — GPUI-Pi 开发约定

> 权威设计文档是 [`docs/立项文档.md`](docs/立项文档.md)。本文件是**每轮必守的操作约定**，与它冲突时以立项文档为准。
> `AGENTS.md` 是本文件的镜像，改动请同时更新两份。

## 这是什么

pi 编程智能体的**原生桌面客户端**：GPUI + gpui-component 画界面，官方 pi 独立二进制（`pi --mode rpc`）当内核。
没有 Electron、没有 Chromium、没有 Next.js、没有内置 Node / Python。

## 六条红线

1. **不引入 web 技术栈** —— 不加 WebView（含 `gpui-wry`）、不嵌 HTML 页面。Mermaid 按立项文档 § 一 直出源码。
2. **不动上游钉版本** —— `Cargo.lock`、`PINNED_PI_VERSION` 与 `vendor/upstream/pi-0.84.2/`（`.gpui-pi-source-pin` marker + `pins/pi-0.84.2.manifest` 全量基线）是钉死点，`cargo update` 或源码身份漂移会被 `scripts/check-pins.*` 判红。要改版本先改立项文档 § 二。
3. **不跨轮次改动** —— 发现前面轮次的问题写进 `rounds/BACKLOG.md`，不当场顺手改。
4. **不放宽验收** —— 连续 2 次 validation 不过，写 `rounds/round-NN/BLOCKED.md` 停下呼人，禁止改标准让自己通过。
5. **不写 `~/.pi` 的破坏性操作** —— 数据目录与终端 pi、pi-web-desktop 共享。能只读就只读，必须写走「临时文件 + rename」。
6. **不把共享目录链接进 worktree** —— 从创建 worktree 起，禁止在其中建立任何指向主 checkout、其他 worktree 或外部共享目录的 Junction、目录 symlink 或其他 reparse point，尤其禁止链接 `vendor/`、`target/`、`.venv/`、`.pi/`。每个 worktree 的本地目录必须独立准备；上游参考源码走该 worktree 自己的 fetch 脚本，临时只读参考可直接读外部绝对路径，但不得挂载进 worktree。执行 `git worktree remove` 前必须检查 reparse point；发现任何目录链接就立即停止并呼人，禁止假设删除 worktree 只会删除链接本身——Git for Windows 可能沿 Junction 递归删除共享目标内容。

## 每轮怎么跑

```
读 rounds/round-NN/round-NN.md  →  实现  →  跑 validate  →  不过就改，重跑
                                                 ↓ 全绿
                              commit + 更新 ROUNDS.md + 回填任务卡「本轮实测」
```

```bash
./scripts/validate.sh            # 全量（含 GPUI 编译，慢）
./scripts/validate.sh --logic    # 只跑 pi-rpc / pi-data / pi-render（快）
```

```powershell
.\scripts\validate.ps1
.\scripts\validate.ps1 -Logic
```

**任何一次迭代结束都必须跑 validate**，不许"看起来对了就下一步"。

## 轮次目录

- `rounds/` 根目录只放全局文件：`README.md`、`TEMPLATE.md`、`BACKLOG.md`；
- 每轮建立独立目录 `rounds/round-NN/`，任务卡固定为 `rounds/round-NN/round-NN.md`；
- 属于该轮的管理产出（实测记录、阻塞报告、复盘、可提交的验收附件说明等）全部放在该目录下；阻塞报告固定命名为 `BLOCKED.md`；
- 源码、脚本、fixture、构建产物仍放各自的标准目录，不为了“归档”复制进 `rounds/`；本地大日志与截图继续放 gitignored 的 `.pi/`，在任务卡中记录结论或引用路径；
- 新增或引用轮次文件时禁止恢复 `rounds/round-NN.md`、`rounds/BLOCKED-NN.md` 这类扁平路径。

## 子代理代码审查

- 启动 `reviewer` 前先用 `subagent action=models`（或 `/subagents-models reviewer`）核对当前运行时映射；
- 代码审查优先显式指定 **DeepSeek provider** 的 `deepseek/deepseek-v4-pro`；这里的 provider 必须是 `deepseek`，不得改用 `variflight-ticket`、OpenRouter 等其他 provider 的同名/路由模型；
- 若 `deepseek/deepseek-v4-pro` 未登记、鉴权/限流不可用或启动失败，立即以**不传 `model`** 的方式重试 reviewer，使其继承主会话模型；禁止拿 DeepSeek V4 Flash 或其他 DeepSeek 型号冒充 V4 Pro；
- **DSV4 Pro 的 reviewer 任务必须从第一个字符开始用简体中文**，并在任务首部明确写“严禁英文前导语，调用工具前只能写中文”。它有时会先输出 `I'll review` / `Let me`，触发桌面端 `language-guard` 中断；此时工具层常只显示误导性的 `Operation aborted`；
- DSV4 Pro 出现 `Operation aborted` 时，先查子代理 transcript 是否含 `[language-guard-restart]`。若存在，这是语言守卫中断，**不算模型/provider 启动失败，不得直接 fallback**；应保留 `deepseek/deepseek-v4-pro`，补强中文首字符约束后以 `context: fresh`、显式目标 `cwd` 重新运行，直到拿到完整审查结论；只有 transcript 无该标记且明确显示模型未登记、鉴权/限流或启动错误时，才按上一条 fallback；
- 模型优先级不改变审查隔离要求：审查默认 `context: fresh`、只读、不与 writer 共用写权限，最终判断与修复仍由主会话负责。

## 平台归属

| 范围 | 执行方 |
|---|---|
| R0 工程骨架 | 130 Arch |
| **R1–R17 全部实现、测试、打包** | **Windows** |
| CI | `windows-latest` 阻断；`ubuntu-latest` 只跑纯逻辑 crate 且非阻断 |

从 Linux 交叉编译 Windows 的 GPUI 目标**不可行**（DirectX + COM + MSVC 链接器）。R1 起 130 端只做只读工作。

## crate 分层

| crate | 依赖 GPUI | 放什么 |
|---|---|---|
| `pi-rpc` | ❌ | 子进程、JSONL 协议、命令/事件类型 |
| `pi-data` | ❌ | `~/.pi/agent` 文件层 |
| `pi-render` | ❌ | 消息 → 可渲染中间模型（Markdown 分块 / ANSI / diff / 工具卡片） |
| `gpui-pi-ui` (`crates/ui`) | ✅ | 跨面板复用的组件封装 |
| `gpui-pi` (`crates/app`) | ✅ | 窗口、面板、状态编排、入口 |

**能放进前三个就不要放进后两个** —— 那三个不需要窗口和 GPU 就能全量单测，是自动化验收的全部基础。

## 编码约定

| 项 | 约定 |
|---|---|
| 换行 | 一律 LF（`.gitattributes` 已强制）。判断"文件是否被改过"的哈希必须忽略换行差异 |
| 路径 | 只用 `std::path::Path`，禁止手拼 `/` 或 `\`；长路径用 `\\?\` 或 `dunce` 规范化 |
| home 目录 | 走 `dirs` crate，别自己读 `HOME`（Windows 上是 `USERPROFILE`） |
| 子进程终止 | 统一走 `pi_rpc` 的封装：Windows `taskkill /T /PID`，其余 `kill(-pgid)`。只杀父进程会留僵尸 |
| 文件替换 | Windows 上被打开的文件不能删/改名 —— 写临时文件 + rename，rename 前确认句柄已关 |
| 颜色/字体 | 颜色一律走 gpui-component 的 `Theme` 变量，禁止硬编码；字体族按平台条件编译（Windows 微软雅黑/Segoe UI，其余 Noto Sans CJK SC） |
| 注释 | 中文。写**为什么**，不复述代码在做什么 |
| clippy | `-D warnings`，不许 `#[allow]` 糊过去；确有必要时必须写明理由 |
| PowerShell 脚本 | 改完**必须验证**，不许"照 sh 版翻译完就提交"（R0 因此红过一次 CI）。130 上已装 pwsh 7.6.5（`~/.local/bin/pwsh`）：能跑的直接跑，跑不了的至少过一遍语法解析（`[Parser]::ParseFile`） |
| `$LASTEXITCODE` | PowerShell 只有跑过**外部程序**才写它 —— 调 `.ps1` 且它正常结束时该变量保持旧值、首次调用时是空的。判断成败前先 `$global:LASTEXITCODE = 0`，且被调脚本成功时要显式 `exit 0` |

## 上游源码在哪（只读参考）

| 用途 | 位置 |
|---|---|
| 功能对照基线 pi-web 0.8.9 | 固定 `vendor/upstream/pi-web-0.8.9/`；运行 `./scripts/fetch-pi-web.sh` 或 `.\\scripts\\fetch-pi-web.ps1` 准备，身份钉 `v0.8.9` / `2a6e53710f6409e0cceb3de839a62f8cdf3ca3ca`（`pins/pi-web-0.8.9.manifest` 全量校验） |
| pi 0.84.2 源码（协议、trust 等实现权威参考） | 固定使用 `vendor/upstream/pi-0.84.2/`；运行 `./scripts/fetch-pi-source.sh` 或 `.\\scripts\\fetch-pi-source.ps1` 准备，身份钉 `v0.84.2` / `914cf1472e715297caa30db4b9535d534a9eb718`；禁止引用会自动更新的 Pi Agent 安装目录 |
| RPC 协议权威文档 | `vendor/upstream/pi-0.84.2/packages/coding-agent/docs/rpc.md`，或 pi 发布包内 `vendor/pi/docs/rpc.md` |
| 会话文件格式 | `vendor/upstream/pi-0.84.2/packages/coding-agent/docs/session-format.md`，或 pi 发布包内 `vendor/pi/docs/session-format.md` |
| 组件库用法 | `git clone --depth 1 https://github.com/longbridge/gpui-component`，看 `crates/story/src/stories/` |

## 协作

- worktree + PR，分支 `WinClaude/round-NN`（130 端用 `ArchLinuxClaude/round-NN`）；
- `main` 只接 PR，不直推；
- PR 描述必须贴 validation 的实际回显，不贴不审；
- **合并后必须验证 main 真的包含那些 commit**（`git log main --oneline` 或 `git branch --contains <sha>`）。R0 合并时踩过：最后一个 commit 已经 push 成功（remote-tracking reflog 有记录），但 `gh pr merge` 用的是 GitHub 侧尚未刷新的 PR head，那个 commit 被静默漏掉，而分支随即被 `--delete-branch` 删了。
