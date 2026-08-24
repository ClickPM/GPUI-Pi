# 轮次进度

拆解与验收标准见 [`docs/立项文档.md`](docs/立项文档.md) § 七；目录规则见 [`rounds/README.md`](rounds/README.md)，每轮任务卡位于 `rounds/round-NN/round-NN.md`。
每轮完成后由执行方更新本表 —— 状态、PR、完成日期三列都要填。R21–R27 为 Issue [#27](https://github.com/cking000bigdemon/GPUI-Pi/issues/27) 的 post-v1 架构扩展；当前仅提交方案评审，未开始代码实现，各轮任务卡由执行方在开工时按 [`rounds/README.md`](rounds/README.md) 建立。

| 轮 | 内容 | 执行方 | 状态 | PR | 完成 |
|---|---|---|---|---|---|
| **R0** | 工程骨架 · CI · 脚本 · 约定文件 | 130 Arch（历史） | ✅ 已完成 | #1 | 2026-08-16 |
| **R1** | ⚠️ 风险门禁 spike（IME / 流式 / 选中 / 冷启） | Windows | ✅ 已完成 | #3 | 2026-08-16 |
| **R2** | `pi-rpc`：子进程 + JSONL 协议 | Windows | ✅ 已完成 | #5 | 2026-08-16 |
| **R3** | `pi-data`：`~/.pi/agent` 文件层 | Windows | ✅ 已完成 | [#6](https://github.com/cking000bigdemon/GPUI-Pi/pull/6) | 2026-08-16 |
| **R4** | 主界面框架（Dock / 侧栏 / 标题栏 / 主题） | Windows | ✅ 已完成 | [#7](https://github.com/cking000bigdemon/GPUI-Pi/pull/7) | 2026-08-16 |
| **R5** | 会话列表（只读） | Windows | ✅ 已完成 | [#9](https://github.com/cking000bigdemon/GPUI-Pi/pull/9) | 2026-08-17 |
| **R6** | 历史消息渲染（静态） | Windows | ✅ 已完成 | [#10](https://github.com/cking000bigdemon/GPUI-Pi/pull/10) | 2026-08-17 |
| **R7** | 活会话流式 | Windows | ✅ 已完成 | [#13](https://github.com/cking000bigdemon/GPUI-Pi/pull/13) | 2026-08-17 |
| **R8** | 输入框 / 附件 / slash 面板 | Windows | ✅ 已完成 | [#14](https://github.com/cking000bigdemon/GPUI-Pi/pull/14) | 2026-08-17 |
| **R9** | 前端视觉打磨（依据 [`docs/UI设计规范.md`](docs/UI设计规范.md)，源自 Zed Agent Panel 设计语言调研；规范演进至 v2.2） | Windows | ✅ 已完成 | [#16](https://github.com/cking000bigdemon/GPUI-Pi/pull/16) | 2026-08-18 |
| **R10** | 模型 · 思考级别 · 工具预设 | Windows | ✅ | [#17](https://github.com/cking000bigdemon/GPUI-Pi/pull/17) | 2026-08-18 |
| **R11** | 文件浏览器 + 查看器 | Windows | ✅ 已完成 | [#18](https://github.com/cking000bigdemon/GPUI-Pi/pull/18) | 2026-08-19 |
| **R12** | git diff / worktree / 本轮改动文件 | Windows | ✅ 已完成 | [#22](https://github.com/cking000bigdemon/GPUI-Pi/pull/22) | 2026-08-20 |
| **R13** | 分支树 · compaction · retry · 导出 | Windows | ✅ 已完成 | [#23](https://github.com/cking000bigdemon/GPUI-Pi/pull/23) | 2026-08-20 |
| **R14** | Extension UI Protocol | Windows | ✅ 已完成 | [#26](https://github.com/cking000bigdemon/GPUI-Pi/pull/26) | 2026-08-21 |
| **R15** | 项目命令环境 bash 扩展（`.ts`） | Windows | ✅ 已完成 | [#24](https://github.com/cking000bigdemon/GPUI-Pi/pull/24) | 2026-08-20 |
| **R16** | 模型配置面板 + 登录 | Windows | ✅ 已完成 | [#25](https://github.com/cking000bigdemon/GPUI-Pi/pull/25) | 2026-08-20 |
| **R17** | 打包分发 | Windows | ⬜ | — | — |
| **R18** | 1:1 验收 + 文档定稿 | Windows | ⬜ | — | — |
| **R19** | Windows 应用图标（独立维护） | Windows | ✅ 已完成 | [#21](https://github.com/cking000bigdemon/GPUI-Pi/pull/21) | 2026-08-19 |
| **R20** | Release 试用第一批问题修复（UI-001–UI-008） | Windows | ✅ 已完成 | [#28](https://github.com/cking000bigdemon/GPUI-Pi/pull/28) | 2026-08-22 |
| **R21** | 权威设计同步 + 单会话 Runtime 集中化 + 会话态收敛 | Windows | ✅ 已完成（BLOCKED 已解除：fresh 不落盘是钉死 pi 权威契约，resume 接缝走 `--session` 空文件分支零 token 验证，见 [`rounds/round-21/BLOCKED.md`](rounds/round-21/BLOCKED.md)） | [#31](https://github.com/cking000bigdemon/GPUI-Pi/pull/31) | 2026-08-24 |
| **R22** | 单 Runtime 有界 Actor 与事件背压 | Windows | ✅ 已完成（代码审查 PASS；视觉审查 `CODE_ONLY_PASS`，未验证真实渲染，见 [`rounds/round-22/round-22.md`](rounds/round-22/round-22.md)） | —（待创建） | 2026-08-24 |
| **R23** | Scheduler / 状态机 / Park-Resume / Idle TTL | Windows | ⬜ | — | — |
| **R24** | 有界多用户 Session UI 接线 | Windows | ⬜ | — | — |
| **R25** | Windows Job Object + 进程树与内存治理 | Windows | ⬜ | — | — |
| **R26** | 内建只读子代理任务与配额调度 | Windows | ⬜ | — | — |
| **R27** | mutating 子代理 + worktree writer 隔离 | Windows | ⬜ | — | — |

## 里程碑

| | 覆盖 | 含义 | 止损 |
|---|---|---|---|
| **M0** | R0–R1 | 骨架 + 风险门禁 | R1 四条任一不过 → **停项** |
| **M1** | R2–R3 | 两个纯逻辑 crate 打通真实 pi | — |
| **M2** | R4–R8 | **可日用** | 到此为止也是稳定终态 |
| **M3** | R10–R15 | 功能追平 | — |
| **M4** | R16–R18 | 交付 | 附录 A 不全绿不发版 |
| **M5** | R21–R27 | 有界多会话 + 内存治理 + 内建子代理 | **已选口径 B，M5 先于 M4 收口**；R22 背压不过不得开放 R24；R25 整树清理不过不得启用 mutating 子代理 |

> **项目所有者于 2026-08-23 选定口径 B**：先实施阶段 E（R21–R27），M4 顺延到 M5 之后；附录 A 验收基线须在 R24 多会话接线完成后重新建立，不得沿用改造前记录。
