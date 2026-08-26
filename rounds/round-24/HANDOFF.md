# R24 交接提示词（新 session 用）

> 复制下面「提示词正文」整段发给新 session 即可。本文件本身也留在 worktree 里，
> 新 session 可以直接读它确认上下文。

---

## 提示词正文

继续开发 R24，**就在当前 worktree 里改**，不要新建 worktree、不要重新准备 vendor：

```
D:\variFlight_work\GPUI-Pi\.claude\worktrees\r24-dev-e5b193
分支 claude/r24-dev-e5b193
```

先读 `rounds/round-24/round-24.md`（任务卡，含十轮审查的完整账目）和
`rounds/round-24/HANDOFF.md`（本文件），再动手。`CLAUDE.md` / `AGENTS.md` 照常必守。

### 当前状态

R24「有界多用户 Session UI 接线」**功能已完成并全部验证通过**，卡在收口前的最后一步：

- 18 个 commit，`main...HEAD` 共 14 文件 +5379/−1706，工作区干净；
- `.\scripts\validate.ps1` 全量 `VALIDATE OK`；`gpui-pi` 149 passed、`gpui-pi-ui` 32 passed、
  `pi-runtime --lib` 80 passed、真实 pi 零 token 集成测试 4 passed；
- 视觉审查 **`PASS`**（用户两轮回传共 15 张截图，证据在 `.pi/visual-review/round-24/`）；
- 独立代码审查已跑 **10 轮 codex**，**21 条 findings 全部成立、全部整改并补了回归测试**；
- `ROUNDS.md` 里 R24 行已标 ✅，PR 列写的是「待创建」。

### 唯一待办：第十一轮 codex 审查

第十轮出了 **2 项 P1**（严重度较第 8–9 轮回升），而且**它的整改本身还没过审**。
用户已明确选择「再跑一轮」。所以新 session 的第一件事是：

```bash
/codex:review --scope branch --base main --background
```

（codex 插件 1.0.6 起 `disable-model-invocation: false`，主会话可以自己调；
`CLAUDE.md` 里那段说明已在本轮修正过。改动超过 1–2 个文件所以带 `--background`，
随后 `/codex:status` 看进度、`/codex:result <job-id>` 取结论。）

拿到结论后按前十轮同样的规矩处理：**逐条核对源码判断是否成立**（不要照单全收，
也不要照单驳回），成立的就整改 + 补回归测试，然后跑全量 `validate`，
再把该轮的 findings 与整改**追加**进任务卡的「第 N 轮独立代码审查与整改」小节。

### 收口条件与止损

- 若第十一轮**归零或只剩 P3**：直接收口——回填任务卡、更新 `ROUNDS.md`，
  然后**问用户是否推分支并开 PR**（`git push` 与建 PR 必须先问，不要自作主张）。
- 若还有 P1/P2：修完再报，但**不要自动跑第十二轮**，把「还要不要继续磨」的判断交给用户。
  用户上一轮就是这么要求的。

### 十轮下来最该带走的三条判断（写在任务卡里，别丢）

1. **findings 驱动的整改必须重新过审**，而且要专门检查「这条修复的同类入口是不是都覆盖了」——
   第 3~6 轮有一半 findings 是「同一条修复漏了某个入口」。
2. **反复在同一处出 findings，往往说明那里有个没拆开的概念**，不是又一个疏忽。
   `draft_key` 同时担着「pi 会话身份」和「草稿存放位置」，补了四轮补丁才拆开。
3. **判据式修复的成本随字段数线性增长且永远漏一个；构造式修复是常数成本。**
   第七轮不再往 `is_focused_tab_pristine` 加条件，改成复用时原地重建整个槽。

另外记住两类多会话问题是**并列**的，不是一条线：
「状态该归谁」（每标签一份 vs 整窗口一份，第 1~9 轮）与
「资源被同时争抢」（同一份会话文件、同一个会话的启动参数，第 10 轮才开始碰）。
第十一轮如果还出 findings，很可能仍在第二类上。

### 几个容易踩的坑（已经踩过，别再踩）

- **`cargo check` / `cargo test` 不 deny warnings，替代不了 `validate`**：
  第二轮整改就是这样漏了 `unused_variables`，被 `clippy -D warnings` 拦下。
- **测试里不要调 `start_new_session` 这类会真的 `request_run` 的路径**：
  它会 spawn `vendor/pi/pi.exe` 并落到真实 `~/.pi`（红线 5）。
  需要会话时用 `register_parked_session`（`create_session` 是纯内存操作，不起进程）。
- **GPUI 测试调度器禁止其他线程唤醒任务**：调度器桥接线程因此做成「只在真的登记了会话之后才起」，
  别改回构造时无条件起。
- **`model_service::tests::timeout_oversize_and_malformed_json_are_bounded` 是已知 flake**
  （BACKLOG #27，三段子用例共用 50ms 超时预算）。整轮 validate 里偶发判红，单跑必过，
  **属 R16 代码，红线 3 不许在 R24 顺手修**。撞上就重跑，并在轮次记录里如实写明。
- **用 heredoc 往 Python 里传含反斜杠的字符串会被吃掉转义**（`\\n` 变成真换行、
  `\\s` 触发 SyntaxWarning）。要写 `\x89PNG` 这类字面量就用 `chr(92)` 拼，或改用 Write 工具。

### 本轮已登记的 BACKLOG（不要在 R24 里顺手修）

`#24` 侧栏选中态不跟随标签切换 · `#25` 退出时不显式注销会话（留给 R25）·
`#26` 后台标签的 Extension UI 对话框会超时 · `#27` model_service flake ·
`#28` 两条标签条上下堆叠。`#18 / #21 / #22` 已由本轮关闭。
