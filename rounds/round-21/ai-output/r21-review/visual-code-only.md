## Review Mode

CODE_ONLY

## Verdict

**CODE_ONLY_PASS**

未发现代码层视觉阻断项。R21 的 UI 相关改动主要是 Runtime 状态所有权迁移、`SessionHandle` 接入及字段收敛；静态代码显示既有组件结构、Theme token、布局约束和可见状态规则均被保留。

**截图验证：未提供（SCREENSHOT_NOT_PROVIDED）**

仅完成纯代码层视觉审查，未验证真实渲染；不阻塞 PR。

## Compared Evidence

### CODE_ONLY 兜底契约

- fallback reason：`USER_DECLINED`
- `requested_at`：`2026-08-23T12:06:15+08:00`
- `deadline`：`2026-08-23T12:36:15+08:00`
- 用户虽回传三张图片，但明确拒绝按截图请求清单补图；图片均为 2880×1716、浅色主题，且第二张有通知遮挡，不符合请求的 1280×820 / 100% / 深色状态，不能计为合格 SCREENSHOT 证据。

### 已读取的代码与文档证据

- 当前完整 UI diff：`D:/variFlight_work/GPUI-Pi-R21/.pi/visual-review/round-21/code-only.diff`
- 变更文件清单：`D:/variFlight_work/GPUI-Pi-R21/.pi/visual-review/round-21/changed-files.txt`
- 任务卡及目标基线：`D:/variFlight_work/GPUI-Pi-R21/rounds/round-21/round-21.md`
  - `rounds/round-21/round-21.md:7`：不改变既有单会话外部行为。
  - `rounds/round-21/round-21.md:37`：本轮不设计视觉变化。
- UI 规范：`D:/variFlight_work/GPUI-Pi-R21/docs/UI设计规范.md`
- 代码审查第二轮 PASS：`D:/variFlight_work/GPUI-Pi-R21/ai-output/r21-review/fix-review.md`
- 相关当前源码与测试：
  - `crates/app/src/panels.rs`
  - `crates/app/src/live_session.rs`
  - `crates/app/src/main.rs`
  - `crates/app/src/main_panel.rs`
  - `crates/app/src/session_sidebar.rs`
  - `crates/app/src/workspace.rs`
  - `crates/pi-runtime/src/lib.rs`

### 辅助图片旁证

- Manifest：`D:/variFlight_work/GPUI-Pi-R21/.pi/visual-review/round-21/evidence/manifest-088acca8cc21ed9b.json`
  - 读取成功；记录三张 PNG，`actualImageCount == 3`。
- `D:/variFlight_work/GPUI-Pi-R21/.pi/visual-review/round-21/evidence/0fbf40a0-01.png`
  - 角色：辅助旁证，历史静态状态。
  - 读取成功，2880×1716，浅色主题。
  - 元数据不合格，不作为 SCREENSHOT 验收证据。
- `D:/variFlight_work/GPUI-Pi-R21/.pi/visual-review/round-21/evidence/0fbf40a0-02.png`
  - 角色：辅助旁证，活会话 Idle 状态。
  - 读取成功，2880×1716，浅色主题，右上通知遮挡。
  - 元数据和遮挡状态不合格，不作为 SCREENSHOT 验收证据。
- `D:/variFlight_work/GPUI-Pi-R21/.pi/visual-review/round-21/evidence/0fbf40a0-03.png`
  - 角色：辅助旁证，文件工作区状态。
  - 读取成功，2880×1716，浅色主题。
  - 元数据不合格，不作为 SCREENSHOT 验收证据。

**截图验证未提供；上述图片仅用于排查明显回归，不支持真实像素几何、深色主题、目标尺寸或交互最终画面的结论。**

## Findings

### Blocker findings

无。

### 代码层审查结论

1. **未引入硬编码颜色或字体**
   - R21 的 UI diff 未增加硬编码 RGB、十六进制颜色或组件内字体族。
   - 现有可见状态继续使用 `cx.theme()` token，例如 composer 输入壳使用 `background`、`border`、`ring`，状态反馈使用 `success`、`warning`、`danger`。
   - 位置：`crates/app/src/panels.rs:85-95`、`crates/app/src/panels.rs:2873-2915`
   - 规范依据：UI 规范红线 1、§2、§3.5。

2. **`SessionUiState` 收敛未改变组件或样式结构**
   - 原先直接存储在 `ChatPanel` 的可见会话字段被原值迁移至单一 `SessionUiState`；默认值与迁移前一致。
   - `Deref` / `DerefMut` 仅将原字段引用投影至 `self.session`，既有渲染代码仍读取同名字段，没有新增包装层、边框、背景、间距或滚动容器。
   - 位置：`crates/app/src/panels.rs:133-223`
   - 对应目标：`rounds/round-21/round-21.md:22`、`rounds/round-21/round-21.md:74`。

3. **Runtime snapshot 对文档和可见状态的投影完整**
   - Dirty 通知触发后，UI 拉取 `SessionSnapshot`，按 epoch/revision 过滤过期状态，再投影 effects、document、列表内容和 `ChatStatus::Ready`。
   - 文档仍通过既有 `sync_list_document` 进入消息虚拟列表，没有改变消息组件、最大列宽、截断、展开或滚动结构。
   - 位置：`crates/app/src/panels.rs:692-770`
   - Snapshot 包含 UI 所需的 `document`、`phase`、steer/follow-up 队列长度及 effect 流。
   - 位置：`crates/pi-runtime/src/lib.rs:697-745`、`crates/pi-runtime/src/lib.rs:868-880`

4. **`active`、`phase` 和 snapshot 仍正确控制 composer 与按钮可见状态**
   - `phase` 现在从 `active.snapshot().phase` 获取；`running`、`stopping`、`live_started` 的派生语义未变。
   - 位置：`crates/app/src/panels.rs:2562-2565`
   - 模型、Thinking、导出等控制项仅在 Idle 且非 busy 时启用。
   - 位置：`crates/app/src/panels.rs:2589-2593`、`crates/app/src/panels.rs:3437-3439`
   - 工具预设在 Running、Stopping 或 busy 时禁用。
   - 位置：`crates/app/src/panels.rs:2590-2593`、`crates/app/src/panels.rs:3200-3204`
   - 停止按钮仍只在 Running/Stopping 分支进入渲染树，Stopping 时显示“正在停止…”并禁用。
   - 位置：`crates/app/src/panels.rs:3272-3286`
   - 发送按钮仍为唯一常驻 primary 操作；未启动、Stopping、控制操作中、只读分支预览或 compaction 时禁用，Running 时显示“加入队列”。
   - 位置：`crates/app/src/panels.rs:3289-3305`
   - 规范依据：`docs/UI设计规范.md:351`、`docs/UI设计规范.md:643-645`。

5. **可见成功、错误、重试、压缩和终止状态分支仍保留**
   - Runtime effects 继续投影 Extension UI、请求失败、控制结果、工具重启、诊断和终止状态。
   - 位置：`crates/app/src/panels.rs:773-926`
   - 错误反馈继续压制过期成功反馈，避免同屏出现相互冲突的红绿结果。
   - 位置：`crates/app/src/panels.rs:2585-2588`
   - 终止或工具重启失败会清除 active handle，使 composer 控件回到非活会话启禁状态，同时保留可见错误文案。
   - 位置：`crates/app/src/panels.rs:890-924`

6. **布局、溢出和滚动结构未发生视觉回归性改动**
   - Composer 仍使用 `TextareaState::auto_grow(1, 8)`。
   - 位置：`crates/app/src/panels.rs:297-309`
   - Composer 内容列继续使用 820px 最大宽度、16px 外留白，并与消息面板几何对齐。
   - 位置：`crates/app/src/panels.rs:2928-2944`
   - 输入壳继续使用 `rounded_xl`、单层边框和唯一允许的 `shadow_sm`。
   - 位置：`crates/app/src/panels.rs:3042-3067`
   - 本轮没有在消息虚拟列表表项中新增内部滚动容器，也没有改变消息截断、换行或 minimap 结构。
   - 规范依据：S-2、S-13、S-22、§5.6。

7. **相关 UI 测试仍覆盖主要静态视觉约束**
   - Steer / Follow-up 单选及点击切换：`crates/app/src/panels.rs:5229-5284`
   - 会话控制选择器存在性：`crates/app/src/panels.rs:5287-5326`
   - 工具重启失败清 busy 与错误状态：`crates/app/src/panels.rs:5371-5394`
   - 停止按钮空闲态不占布局：`crates/app/src/panels.rs:5432-5437`
   - Composer 单边框、主题 token 与阴影：`crates/app/src/panels.rs:5439-5462`
   - 最小窗口 composer 可见性及消息列对齐：`crates/app/src/panels.rs:5464-5513`
   - 现有附件、popup、状态栏、列表滚动和展开重测相关视觉测试仍保留。

## Non-UI Dependencies

无。

未发现必须修改 RPC、Runtime 状态机、数据模型、持久化或其他业务代码才能处理的视觉问题。

## Matches

以下仅表示代码层符合规范，不表示实际画面已完成截图还原：

- Theme token 使用方式未回归，未新增硬编码颜色或字体。
- 既有 composer 组件层级、820px 内容列、输入壳圆角/边框/阴影保持不变。
- `SessionUiState` 字段收敛保持原默认状态与原渲染字段访问方式。
- Running、Stopping、Idle、无 active、busy 等状态继续控制停止按钮、发送按钮、模型/Thinking/工具选择器的显示与启禁。
- 成功、错误、重试、压缩、Extension UI 和终止反馈的可见分支仍存在。
- 未新增消息表项内滚动、异常固定高度、硬编码字号、颜色或视觉表面层级。
- 三张辅助图片中未发现足以推翻上述静态结论的明显整体结构回归，但它们不是合格截图证据。

**最终结论：CODE_ONLY_PASS。仅完成纯代码层视觉审查，未验证真实渲染；不阻塞 PR。**