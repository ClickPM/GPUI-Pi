<!-- 本文件由 CLAUDE.md 镜像而来，改动请同步两份。 -->

# CLAUDE.md — GPUI-Pi 开发约定

> 权威设计文档是 [`docs/立项文档.md`](docs/立项文档.md)。本文件是**每轮必守的操作约定**，与它冲突时以立项文档为准。
> `AGENTS.md` 是本文件的镜像，改动请同时更新两份。

## 这是什么

pi 编程智能体的**原生桌面客户端**：GPUI + gpui-component 画界面，官方 pi 独立二进制（`pi --mode rpc`）当内核。
没有 Electron、没有 Chromium、没有 Next.js、没有内置 Node / Python。

## 六条红线

1. **不引入 web 技术栈** —— 不加 WebView（含 `gpui-wry`）、不嵌 HTML 页面。Mermaid 按立项文档 § 一 直出源码。
2. **不动上游钉版本** —— `Cargo.lock`、`PINNED_PI_VERSION` 与 `vendor/upstream/` 下两份上游源码是钉死点，版本号与 commit 身份以「上游源码在哪」一表为准；`cargo update` 或源码身份漂移会被 `scripts/check-pins.ps1` 判红（`.sh` 版是历史遗留，不作为判红依据）。要改版本先改立项文档 § 二。
3. **不跨轮次改动** —— 发现前面轮次的问题写进 `rounds/BACKLOG.md`，不当场顺手改。
4. **不放宽验收** —— 同一验收项经针对性整改后连续 2 次 validation 仍不过，写 `rounds/round-NN/BLOCKED.md` 停下呼人，禁止改标准让自己通过；日常迭代中的其他失败按流程图「不过就改，重跑」处理，不触发此条。
5. **不写 `~/.pi` 的破坏性操作** —— 指用户主目录下的 pi 数据目录（与仓库根 gitignored 的 `.pi/` 日志目录同名异物，勿混淆），它与终端 pi、pi-web-desktop 共享。能只读就只读，必须写走「临时文件 + rename」。
6. **不把共享目录链接进 worktree** —— 从创建 worktree 起，禁止在其中建立任何指向主 checkout、其他 worktree 或外部共享目录的 Junction、目录 symlink 或其他 reparse point，尤其禁止链接 `vendor/`、`target/`、`.venv/`、仓库 `.pi/` 与用户数据目录 `~/.pi`。执行 `git worktree remove` 前必须检查 reparse point；发现任何目录链接就立即停止并呼人，禁止假设删除 worktree 只会删除链接本身——Git for Windows 可能沿 Junction 递归删除共享目标内容。目录如何独立准备见「新 round 启动门禁」。

## 新 round 启动门禁

`git worktree add` 完成后，**第一项操作就是在新 worktree 内独立准备并验证完整 `vendor`**；此门禁必须早于读取轮次任务卡、搜索上游对照源码、启动子代理或修改任何代码：

```powershell
.\scripts\fetch-pi.ps1
.\scripts\fetch-pi-source.ps1
.\scripts\fetch-pi-web.ps1
.\scripts\check-pins.ps1
```

开始实现前必须同时确认 `vendor/pi/pi.exe`、`vendor/upstream/pi-0.84.2/`、`vendor/upstream/pi-web-0.8.9/` 均存在，且 `check-pins` 全绿。每个 worktree 的本地目录必须独立准备，上游参考源码走本 worktree 自己的 fetch 脚本。任何一项缺失或准备失败都立即停止并呼人；禁止先寻找替代源码路径、边开发边补、创建共享目录链接（红线 6），或读取主 checkout / 其他 worktree 的 `vendor/` 顶替。临时只读参考可以直接读外部绝对路径，但不得挂载进 worktree，且这一口径只适用于 `vendor/` 之外的资源（如 gpui-component clone）——上游对照在门禁通过前后都只走本 worktree 自己的 `vendor/`。这样每轮从第一分钟起就有完整运行时和钉死对照源码，不再把实现时间浪费在寻找缺失的 `vendor`。

本机缓存：fetch 脚本默认走 `D:\tmp\gpui-pi-cache`（环境变量 `GPUI_PI_CACHE` 可改路径，设 `OFF` 禁用；CI 已禁用）——缓存命中时只做本地校验与拷贝，不联网；缓存缺失或校验失败才联网拉取，并在缓存目录内覆盖更新。缓存目录位于仓库之外，`vendor` 里始终是每 worktree 独立的真实拷贝，不创建任何链接（红线 6 不受影响）。

## 每轮怎么跑

```
新 worktree 独立准备完整 vendor + check-pins
                  ↓ 全绿
读 rounds/round-NN/round-NN.md  →  实现  →  跑 validate  →  不过就改，重跑
                                                 ↓ 全绿
        代码审查（按「代码审查工具路由」）→ 涉及 UI 则视觉审查：
              主代理出截图请求清单并记录 30 分钟窗口
              ├─ 截止前完整回传 → SCREENSHOT review，须 PASS
              └─ 用户拒绝或超时 → CODE_ONLY review，须 CODE_ONLY_PASS
                                                 ↓ 任一路径通过并回填 round 标记
                    commit + 更新仓库根 ROUNDS.md + 回填任务卡「本轮实测」
```

```powershell
.\scripts\validate.ps1              # 全量（含 GPUI 编译，慢）
.\scripts\validate.ps1 -Logic       # 当前只跑 pi-rpc / pi-data / pi-render；R21 引入 pi-runtime 时同步纳入
```

**任何一次迭代结束都必须跑 validate**，不许"看起来对了就下一步"。

## 轮次目录

- `rounds/` 根目录只放全局文件：`README.md`、`TEMPLATE.md`、`BACKLOG.md`；轮次总览 `ROUNDS.md` 在仓库根目录而非 `rounds/` 下，每轮收口时更新；
- 每轮建立独立目录 `rounds/round-NN/`，任务卡固定为 `rounds/round-NN/round-NN.md`；
- 属于该轮的管理产出（实测记录、阻塞报告、复盘、可提交的验收附件说明等）全部放在该目录下；阻塞报告固定命名为 `BLOCKED.md`；
- 源码、脚本、fixture、构建产物仍放各自的标准目录，不为了“归档”复制进 `rounds/`；本地大日志与截图继续放仓库根 gitignored 的 `.pi/`（与红线 5 的用户数据目录 `~/.pi` 无关），在任务卡中记录结论或引用路径；
- 新增或引用轮次文件时禁止恢复 `rounds/round-NN.md`、`rounds/BLOCKED-NN.md` 这类扁平路径。

## 代码审查工具路由

- **本节只约束已有代码改动的独立审查通道**：不规定实现阶段由主会话还是子代理承担，也不得据此改变模型基于任务、能力与当前上下文自主选择的任务拆解、委派策略或 writer。无论调用 `claude-code-review` 还是 codex 插件的审查命令，都不代表主会话必须充当 writer，也不降低 worker、scout 等子代理在实现阶段的可用性或优先级。
- 当已有代码改动需要独立代码审查时，按开发 harness 选择审查通道：
  - 使用 **pi harness** 时：优先调用 `claude-code-review`（独立 Claude Code 子进程，只读审查，默认 `scope=working`）；若 `claude-code-review` 不可用（未登记、鉴权/限流不可用、启动失败等），降级派发 pi-subagents 的 `reviewer` 子代理，走下方「pi-subagents 审查路径」；
  - 使用 **Claude Code harness** 时：优先走 **codex 插件**（`codex@openai-codex`）做独立代码审查，走下方「codex 插件审查路径」；只有该路径确实不可用时才降级到 Claude Code 自带的 `/code-review`，并在轮次记录写明降级原因；
  - 使用 **codex harness** 时：审查者与 writer 同源，本节路由不适用，代码审查按 codex 自身流程执行。
- 审查器始终只读，并与当前 writer 隔离。主会话负责判断是否接受审查结论；需要修改代码时，保持审查前已经形成的 writer 归属：此前由主会话实现则由主会话修复，此前由 worker 实现则将 findings 交回同一 worker。审查工具的选择本身不得触发 writer 身份切换。

### codex 插件审查路径（Claude Code harness 用）

- 默认命令是 `/codex:review`（Codex 原生只读审查），可带 `--base <ref>`、`--scope auto|working-tree|branch`；需要质疑实现思路、设计取舍与假设，而不只是挑实现缺陷时，改用 `/codex:adversarial-review [focus...]`。两者都是 review-only、不改代码，天然满足「审查器只读」。
- **这两个命令标了 `disable-model-invocation`，主会话无法自行触发**：主代理必须先把本轮 diff 收敛稳定（validate 已全绿），再在正文给出待运行的完整命令（含 scope 与 base）请用户手动输入。用户运行前不得声称代码审查已完成，也不得拿主会话自审冒充独立审查。
- 改动超过 1–2 个文件时，交给用户的命令直接带 `--background`；随后用 `/codex:status` 看进度、`/codex:result <job-id>` 取完整结论。findings 必须按原文对齐处理，不得只留一句摘要就判通过。
- 判定 codex 路径不可用只认硬失败：`/codex:setup` 报 Codex CLI 未安装/未登录、命令启动失败，或用户明确拒绝使用 codex 审查。此时才降级 `/code-review`；「等得久」「改动小」都不是降级理由。
- `/codex:setup --enable-review-gate` 打开的 stop 时挑战式复查属于**补充信号**，不替代本节的独立代码审查门禁，也不能单独作为「代码审查通过」的依据。

### pi-subagents 审查路径（pi harness 降级用）

- 启动 `reviewer` 前先用 `subagent action=models`（或 `/subagents-models reviewer`）核对当前运行时映射；
- 代码审查优先显式指定 **DeepSeek provider** 的 `deepseek/deepseek-v4-pro`；这里的 provider 必须是 `deepseek`，不得改用 `variflight-ticket`、OpenRouter 等其他 provider 的同名/路由模型；
- 若 `deepseek/deepseek-v4-pro` 未登记、鉴权/限流不可用或启动失败，立即以**不传 `model`** 的方式重试 reviewer，使其继承主会话模型；禁止拿 DeepSeek V4 Flash 或其他 DeepSeek 型号冒充 V4 Pro；
- **DSV4 Pro 的 reviewer 任务必须从第一个字符开始用简体中文**，并在任务首部明确写“严禁英文前导语，调用工具前只能写中文”。它有时会先输出 `I'll review` / `Let me`，触发桌面端 `language-guard` 中断；此时工具层常只显示误导性的 `Operation aborted`；
- DSV4 Pro 出现 `Operation aborted` 时，先查子代理 transcript 是否含 `[language-guard-restart]`。若存在，这是语言守卫中断，**不算模型/provider 启动失败，不得直接 fallback**；应保留 `deepseek/deepseek-v4-pro`，补强中文首字符约束后以 `context: fresh`、显式目标 `cwd` 重新运行，直到拿到完整审查结论；只有 transcript 无该标记且明确显示模型未登记、鉴权/限流或启动错误时，才按上一条 fallback；
- 模型优先级不改变审查隔离要求：审查默认 `context: fresh`、只读、不与 writer 共用写权限。

## 视觉还原度审查

- 项目专用视觉 reviewer 为 `.agents/visual-reviewer.md`（运行时名称 `visual-reviewer`）。它是**能力层只读** agent，只允许 `read` / `grep` / `find` / `ls`，禁止 `bash`、`edit`、`write`、子代理和任何文件修改；不得用通用代码 reviewer 替代视觉 reviewer。
- **触发条件**：本轮 diff 只要触及 `crates/ui/**`，或触及 `crates/app/**` 中的视图/组件/布局/样式渲染代码、Theme token、图标及其他视觉资源，就视为涉及 UI 层；即使改动位于其他路径，只要会改变用户可见的 UI 表现，也必须触发视觉 review。不得仅凭“没有改 `crates/ui`”而跳过。
- 视觉 review 是代码 review 之后的独立门禁：普通代码 review 的阻断 findings 已关闭、主会话判定代码 review 通过后，才运行 `visual-reviewer`；视觉 review 不能替代代码 review，也不得提前与尚未稳定的实现并行给出终审结论。
- 发起视觉 review 前，主会话或当前 writer 必须准备目标截图/设计稿或明确规范基线、对应窗口尺寸与缩放、主题、交互状态、当前 diff、任务卡，以及 `docs/UI设计规范.md`。来自主会话聊天附件的所有图片证据都**不得假设子代理能够继承**，`context: fork` 也不能替代图片落盘。当前实现截图一律走用户回传：主代理无法直接操作 GUI 应用（GPUI 桌面客户端）导航与截图，禁止自行臆造或从旧轮次/上游实现借用；主代理先产出「截图请求清单」，逐页给出页面路由/导航路径（启动方式、窗口布局、操作序列、所需前置状态）、窗口尺寸与缩放、主题、交互状态及对应目标基线引用。
- **30 分钟截图窗口**：主代理发出完整截图请求清单时，必须记录含时区的 ISO-8601 `requested_at`，并计算 `deadline = requested_at + 30 分钟`。用户明确拒绝提供截图时立即进入 `CODE_ONLY`，无需等待截止；用户未明确拒绝时不得忙等或反复催促，可结束当前 turn，待会话在截止后继续时执行兜底。截止前收到的截图只有在请求清单所需页面/状态全部齐全时才算“完整回传”；缺图、缺状态或尺寸/主题不符均按未完整回传处理。
- **`SCREENSHOT` 模式（首选）**：用户在截止前完整回传截图后，使用 **pi harness** 的主代理运行 `.\scripts\export-visual-evidence.ps1 -Round round-NN -ExpectedImageCount N`（只读 `$env:PI_SESSION_FILE`）；分多条消息回传时传精确的 `-EntryId id1,id2`，或以 `requested_at` 运行 `-AllSince -Since <ISO-8601>`。主代理必须核对 manifest 的 `actualImageCount == ExpectedImageCount` 且 `selectedEntries` 确属本次回传，禁止修改 `~/.pi` 下的会话 JSONL。使用 **Claude Code / codex 等非 pi harness** 时不得假设存在 `$env:PI_SESSION_FILE`，应请用户把图片保存到 `.pi/visual-review/round-NN/evidence/`，或使用该 harness 能提供的本地附件路径。随后将每张图片绝对路径及其证据角色、页面路由、窗口尺寸与缩放、主题、交互状态和对应目标基线显式传给 `visual-reviewer`，以 `SCREENSHOT` 模式审查；结论必须为 `PASS`。本模式中缺少可读路径、证据元数据或图片输入模型时只能给出 `INSUFFICIENT_EVIDENCE`，不得冒充通过。
- **`CODE_ONLY` 兜底模式（不阻塞 PR）**：仅在用户明确拒绝（`USER_DECLINED`），或截止时间已过且完整截图仍未提供（`TIMEOUT_30M`）时启用。主代理必须把兜底原因、`requested_at`、`deadline`、当前 diff（正文或可由 `read` 读取的 diff 文件绝对路径）、变更文件清单、任务卡、目标基线以及 `docs/UI设计规范.md` 显式传给 `visual-reviewer`，要求其只从代码层审查 Theme token、硬编码颜色/字体、组件与样式结构、布局约束、溢出/截断/滚动、可见状态分支及相关 UI 测试。该模式不要求图片，结论只能为 `CODE_ONLY_PASS` 或 `CODE_ONLY_FAIL`；`CODE_ONLY_PASS` 仅表示未发现代码层视觉阻断项，不等于截图还原度 `PASS`，但允许继续创建/推进 PR；`CODE_ONLY_FAIL` 仍阻塞，修复后必须重跑同模式审查。
- 走 `CODE_ONLY_PASS` 时，`rounds/round-NN/round-NN.md` 必须固定记录：`视觉审查模式：CODE_ONLY`、`视觉审查结论：CODE_ONLY_PASS`、`截图验证：未提供（SCREENSHOT_NOT_PROVIDED）`、`兜底原因：TIMEOUT_30M | USER_DECLINED`、`requested_at`、`deadline`，并注明“仅完成纯代码层视觉审查，未验证真实渲染；不阻塞 PR”。若截图后来补齐，可追加 `SCREENSHOT` review 并将结论升级为 `PASS`，但不得回写成此前已经完成截图验证。
- `SCREENSHOT` 模式判断实际还原度与规范符合度，包括几何与面板比例、对齐、间距、字体层级、颜色/对比度、组件形态、信息密度、状态表现、溢出/裁切/滚动及不同窗口尺寸下的稳定性；`CODE_ONLY` 模式只能判断上述问题在静态代码中可证实的部分，不得推断真实像素结果。两种模式都不得把个人审美当作目标、扩展产品范围或审查业务逻辑。
- 主会话负责接受或驳回视觉 findings；需要修复时保持视觉 review 前已经形成的 writer 归属。**视觉 review 之后产生的修复增量只允许修改 UI 表现代码**：限 `crates/ui/**`、`crates/app/**` 中纯视图/布局/样式渲染片段，以及直接对应的 UI 测试、视觉 fixture 和视觉资源；禁止修改 `crates/pi-rpc/**`、`crates/pi-data/**`、`crates/pi-render/**`，也禁止改 RPC、进程/会话控制、状态机、数据模型、持久化、协议及其他业务逻辑。
- 若某条视觉 finding 必须依赖业务代码才能解决，立即停止该项视觉修复，将其标记为“非 UI 依赖”，另行进入普通开发与代码 review 流程；禁止借视觉修复顺手改业务代码。视觉修复完成后，先确认相对视觉 review 基线的增量没有越界，再对该 UI 增量完成必要代码复核并以原审查模式重新运行 `visual-reviewer`。最终门禁结论必须为 `PASS`，或在上述兜底条件成立且 round 标记完整时为 `CODE_ONLY_PASS`；二者均可继续 PR 流程，但只有 `PASS` 可以宣称完成截图视觉验证。

## 平台归属

| 范围 | 执行方 |
|---|---|
| R0 工程骨架 | 130 Arch（历史；已迁移） |
| **R1 起全部实现、测试、打包** | **Windows** |
| CI | `windows-latest` 阻断（唯一 job） |

**项目已迁移为 Windows solo**：不再维护 Linux 构建、不再跑 Linux CI，`ubuntu-latest` 相关 job 已从 `.github/workflows/ci.yml` 移除。所有 `.sh` 脚本与 Linux 分支说明仅为历史遗留，不再维护更新。从 Linux 交叉编译 Windows 的 GPUI 目标**不可行**（DirectX + COM + MSVC 链接器）。

## crate 分层

| crate | 依赖 GPUI | 放什么 |
|---|---|---|
| `pi-rpc` | ❌ | 子进程、JSONL 协议、命令/事件类型 |
| `pi-data` | ❌ | `~/.pi/agent` 文件层 |
| `pi-render` | ❌ | 消息 → 可渲染中间模型（Markdown 分块 / ANSI / diff / 工具卡片）。**app / ui 与 pi-runtime 共享的类型层**，不是 pi-runtime 的下游：`crates/ui` 对它的直接依赖必须保留 |
| `pi-runtime`（R21 引入） | ❌ | 应用级有界 RuntimeManager、Actor、Snapshot、调度、资源预算与内建子代理任务；所有 app 内建 `pi --mode rpc` 的唯一生产创建入口 |
| `gpui-pi-ui` (`crates/ui`) | ✅ | 跨面板复用的组件封装 |
| `gpui-pi` (`crates/app`) | ✅ | 窗口、面板、轻量状态投影、入口；不得直接拥有 `pi_rpc::Client` |

**能放进前四个就不要放进后两个** —— 四个纯逻辑 crate 不需要窗口和 GPU，可脱离 GPUI 完整单测。`pi-rpc` 只负责单进程监督，跨 Session 的调度、回收和预算统一属于 `pi-runtime`。

## Post-v1 有界运行时（R21–R27）

- 权威拆分见 `docs/立项文档.md` § 七 阶段 E；对应 GitHub Issue 为 #27。项目所有者已于 2026-08-23 选择**口径 B**：先实施阶段 E，M4 顺延到 M5 之后，附录 A 验收基线须在 R24 后重建。
- 实施顺序固定为：R21 集中化 → R22 Actor/背压 → R23 Scheduler/Park → R24 多会话 UI → R25 Windows Job Object/内存 → R26 只读子代理 → R27 mutating writer/worktree 隔离。**R22 背压未通过前禁止开放多会话**。
- `RuntimeManager` 是应用级共享服务，跨窗口、Workspace、用户 Session、内建子代理和 maintenance job 共用总预算；禁止在 `ChatPanel` 或单个窗口内各建一套 Manager。
- app 生产代码创建用户 Session、内建子代理或 maintenance `pi --mode rpc`（包括历史 HTML 导出）必须经过 `RuntimeManager`；`pi-rpc` 自身隔离测试可直接创建 `Client`。Provider 登录等一次性非 RPC CLI 不计入 Session Runtime。Maintenance job 走独立小配额，**不占用户会话运行槽**。
- **重启决策权归 Manager**：经 `RuntimeManager` 创建的 Runtime 一律把 `pi-rpc` 的 `max_restarts` 设为 0，崩溃恢复走 Manager 状态机。确需保留底层自动重启的场景，必须订阅 `LifecycleEvent::Restarting / Restarted` 并把该次重启计入运行槽与内存预算——绝不允许底层在 Manager 判 `Failed`、释放运行槽之后自行拉起进程。
- **Park/Resume 优先复用 `switch_session`**：`IdleWarm` 是可被任意 Session 复用的热进程池，不是某个 Session 的预热副本；冷启动只作为无空闲 Runtime 时的回退。
- `SessionHandle` 不得暴露 raw `Client`、可变 reducer、无界 receiver 或 app 专用 `PumpMessage`；UI 通过稳定 `RuntimeId`、Snapshot revision 与 Dirty 通知消费状态。Pi `session_id` 可后置校准，不作为不可变 Handle 主键。
- **两类子代理，边界不许含糊**：Scheduler 的并发与深度配额只覆盖 **Manager 派发**的子代理；pi 内核与 extension 自行 spawn 的子代理绕过配额（官方 `subagent` 示例扩展对每次调用 spawn 一个 `pi --mode json -p --no-session`，且支持 `tasks` 数组并行）。因此 R26 验收不得表述为"所有子代理"。反过来也不得说这类进程"无法强制控制"——它们在 Runtime 的进程树 / Job Object 内，R25 的进程数与内存硬限制对其有效；真正做不到的只是**按任务语义限流**。
- R26/R27 自建子代理的立项理由必须成立且写明：有会话文件（可恢复、可回看）、有进度与结果 UI、mutating 任务有 worktree 隔离。子代理会话用钉死协议已有的 `new_session { parentSession }` 把父子关系落盘，不得只存在 Manager 内存里。
- R21 的「`ChatPanel` 会话态收敛为 `SessionUiState`」属本轮授权范围，**不受红线 3 约束**——R21 已要改遍 `active` / `reducer` 的全部引用点，留到 R24 等于同一批代码改两遍。
- R25 的内存与进程数采样必须经可注入的 `ResourceProbe` 抽象，水位策略用 fake probe 做确定性测试（与 R23 的 fake clock 同构），避免只能手测而撞红线 4。Job Object 所需 API 在已有的 `windows-sys` 内，加 feature 即可，**不改 `Cargo.lock`、不触红线 2**。
- Issue 中并发数、Warm 数量、Idle TTL 与内存水位是可配置初始值，后续按 Windows 实测调整；不得把建议值硬编码成不可变产品契约。

## 编码约定

| 项 | 约定 |
|---|---|
| 换行 | 一律 LF（`.gitattributes` 已强制）。判断"文件是否被改过"的哈希必须忽略换行差异 |
| 路径 | 只用 `std::path::Path`，禁止手拼 `/` 或 `\`；长路径用 `\\?\` 或 `dunce` 规范化 |
| home 目录 | 走 `dirs` crate，别自己读 `HOME`（Windows 上是 `USERPROFILE`） |
| 子进程终止 | 统一走 `pi_rpc` 的封装：Windows `taskkill /T /PID`，其余 `kill(-pgid)`。只杀父进程会留僵尸 |
| 文件替换 | Windows 上被打开的文件不能删/改名 —— 写临时文件 + rename，rename 前确认句柄已关 |
| 颜色/字体 | 一律走 `cx.theme()` token（gpui-component `Theme`），组件内禁止硬编码颜色值和字体名；字体族按平台条件编译，唯一注入点是 `crates/ui/src/theme.rs` 的 theme 初始化，改字体只改那里 |
| 注释 | 中文。写**为什么**，不复述代码在做什么 |
| clippy | `-D warnings`，不许 `#[allow]` 糊过去；确有必要时必须写明理由 |
| PowerShell 脚本 | 改完**必须验证**，不许"照 sh 版翻译完就提交"（R0 因此红过一次 CI）：能跑的直接跑，跑不了的至少过一遍语法解析（`[Parser]::ParseFile`） |
| `$LASTEXITCODE` | PowerShell 只有跑过**外部程序**才写它 —— 调 `.ps1` 且它正常结束时该变量保持旧值、首次调用时是空的。判断成败前先 `$global:LASTEXITCODE = 0`，且被调脚本成功时要显式 `exit 0` |

## UI 设计规范（每轮必守）

- 权威文档：[`docs/UI设计规范.md`](docs/UI设计规范.md)——由 Zed Agent Panel 设计语言调研映射 gpui-component 形成（R9 前置产出），是 UI 视觉的唯一判据；与它冲突时以规范文档为准，与立项文档冲突时以立项文档为准。
- **R9 负责把现有 UI 打磨到符合规范**；R10 起每一轮新增/修改 UI 都必须遵守规范，不得回归旧风格。
- 核心条款（详细条目见规范文档）：
  - 颜色/字体一律走 `cx.theme()` token，禁止硬编码；
  - 层级用背景 + 留白表达，边框只用于 hover / 选中 / 错误态；过程性内容（工具/思考/计划）用弱边框卡片 + header 状态点；
  - 工具输出用左竖线 + 缩进表达从属关系，不嵌套新卡片；
  - 操作三级可见：主操作常驻（primary）、次操作 hover 显隐、低频操作收菜单；
  - 一行信息不超过 3 个片段，次要信息进 tooltip；
  - 状态色只点不铺（小圆点 / 左侧竖条 / 图标色），不铺满卡片边框或背景。
- UI 改动若与规范冲突：先改规范文档（走评审）再改代码，不许绕过规范硬编码。

## 上游源码在哪（只读参考）

| 用途 | 位置 |
|---|---|
| 功能对照基线 pi-web 0.8.9 | 固定 `vendor/upstream/pi-web-0.8.9/`；运行 `.\\scripts\\fetch-pi-web.ps1` 准备，身份钉 `v0.8.9` / `2a6e53710f6409e0cceb3de839a62f8cdf3ca3ca`（`pins/pi-web-0.8.9.manifest` 全量校验） |
| pi 0.84.2 源码（协议、trust 等实现权威参考） | 固定使用 `vendor/upstream/pi-0.84.2/`；运行 `.\\scripts\\fetch-pi-source.ps1` 准备，身份钉 `v0.84.2` / `914cf1472e715297caa30db4b9535d534a9eb718`（`.gpui-pi-source-pin` marker + `pins/pi-0.84.2.manifest` 全量校验）；禁止引用会自动更新的 Pi Agent 安装目录 |
| RPC 协议权威文档 | `vendor/upstream/pi-0.84.2/packages/coding-agent/docs/rpc.md`，或 pi 发布包内 `vendor/pi/docs/rpc.md` |
| 会话文件格式 | `vendor/upstream/pi-0.84.2/packages/coding-agent/docs/session-format.md`，或 pi 发布包内 `vendor/pi/docs/session-format.md` |
| 组件库用法 | `git clone https://github.com/longbridge/gpui-component` 后 checkout 到 `Cargo.lock` 中 `gpui-component` 钉定的 rev（以 lock 文件为准），看 `crates/story/src/stories/`；禁止按 main HEAD 参考 |

## 协作

- worktree + PR，分支 `WinClaude/round-NN`；
- `main` 只接 PR，不直推；
- PR 描述必须贴 validation 的实际回显，不贴不审；
- **合并后必须验证 main 真的包含那些 commit**（`git log main --oneline` 或 `git branch --contains <sha>`）。R0 合并时踩过：最后一个 commit 已经 push 成功（remote-tracking reflog 有记录），但 `gh pr merge` 用的是 GitHub 侧尚未刷新的 PR head，那个 commit 被静默漏掉，而分支随即被 `--delete-branch` 删了。
