# Winter v0.2 架构

## 1. 架构结论

Winter 是 Windows Terminal 的无界面输入增强层，不是终端模拟器。

- Windows Terminal：唯一 UI、终端渲染、窗格树、标签页和 Shell 生命周期权威。
- Rust Prefix Core：两段式快捷键状态权威。
- Rust Integration：产品配置、Action fragment 和用户 keybinding 事务权威。
- UI Automation Observer：只读、可丢弃的原生 `TermControl` 几何快照。
- Win32 Adapter：键盘/鼠标 Hook、前台身份、输入注入和单实例边界。
- Pointer Reducer：纯拖拽捕获状态与移动距离换算；不直接访问 Hook、UIA 或队列。
- Action Worker：串行执行键盘动作和已合并的拖拽 resize 批次。

不存在 WebView、前端状态副本、Tauri IPC、PTY 管理器或应用自有布局树。鼠标拖动只从 UI Automation 矩形推导当前分隔线，快照不持久化、不参与 split/close 决策，也不是第二份布局权威。

## 2. 运行结构

    Physical keyboard
          │
          ▼
    WH_KEYBOARD_LL hook thread
          │ raw event
          ▼
    PrefixMachine
      ├── PassThrough ───────────────► current application
      └── Consume + Dispatch
                     │ bounded queue
                     ▼
                action worker
                     │ revalidate HWND/PID/start marker
                     ▼
                SendInput hidden chord
                     │
                     ▼
             Windows Terminal Action
                     │
                     ▼
       native tab / pane / renderer / shell

    Physical mouse
          │
          ▼
    WH_MOUSE_LL hook ──► PointerDragState
          │                       │ idle / external drag / pass through
          │ resize batch          ▼
          └──────────────► bounded action queue
                                  │
                                  ▼
                       UIA focus native TermControl once
                                  │
                                  ▼
                   native resizePane action × batch steps

### 模块结构

    src/
    ├── model.rs / config.rs / pane_layout.rs   纯数据结构、TOML schema 与纯几何状态机
    ├── registry.rs    唯一动作注册表：30 条动作（含 1 条 shutdown）。prefix 快捷键
    │                  规格与 29 条托管桥接绑定都在编译期由它派生，const 断言拒绝
    │                  重复动作/默认 chord/Action ID、不完整 SPEC_ORDER 与多余 shutdown
    ├── keys.rs        规范键名表：配置 parse/display 与虚拟键正/反向映射的单一来源
    ├── prefix.rs      KeyChord 契约与 Prefix 状态机（SHORTCUT_SPECS 由 registry 派生）
    ├── keymap.rs      ManagedBinding/BridgeChord 类型与 registry 派生的桥接视图
    │                  （MANAGED_BINDINGS）
    ├── controller/    keyboard / desktop / pointer / action_worker 与编排入口
    ├── platform/windows/ 低级 Hook、UI Automation、前台身份、SendInput、提权与单实例
    ├── integration/   settings 事务层，按职责拆分（不再是单文件模块）：
    │   ├── mod.rs          plan / install / uninstall / doctor 编排
    │   ├── discovery.rs    Windows Terminal 通道发现
    │   ├── targets.rs      逐目标计划、预检与执行
    │   ├── fragment.rs     fragment 安装与卸载
    │   ├── jsonc.rs        无损 JSONC CST 编辑与 keybindings 合并
    │   ├── transaction.rs  同目录临时文件 + CAS 原子替换
    │   ├── rollback.rs     内存逆操作栈回滚
    │   ├── manifest.rs     托管 manifest 读写与语义比对
    │   ├── helpers.rs      期望 keybindings 推导与错误上下文
    │   ├── shell.rs        PowerShell 受管 Profile 包装
    │   └── types.rs        报告与状态类型
    └── bin/            winter / winterd / winterminalp CLI 入口

## 3. 线程与性能

- Hook 线程同时拥有低级键盘/鼠标 Hook、Windows 消息循环和 Hook 生命周期。
- Hook 回调只更新内存状态并执行 `try_send`，不做 IO、进程查询或等待。
- Desktop Observer 提供前台 Terminal 身份，并按独立低频周期读取原生窗格矩形；COM/UI Automation 不进入 Hook 回调。
- 分隔线命中、拖动残差和 5% 原生步长换算位于纯 Rust `pane_layout`，不依赖 Win32。
- `controller/pointer` 只决定是否捕获、何时取消以及一个拖拽批次的净 resize 意图；普通悬停和从 Explorer 开始的外部拖放永远透传。
- 控制器以 per-monitor DPI 感知启动，使低级鼠标 Hook 的物理像素坐标与 UI Automation 返回的矩形处于同一坐标系；几何枚举在 provider 侧按 `TermControl` 类名过滤，单个失效元素不会丢弃整份快照。
- Action worker 串行派发，避免多个合成 chord 交错；一个拖拽批次只做一次 UIA 聚焦，再发送对应数量的原生 resize chord，避免把同次移动拆成多个昂贵任务。
- 队列固定容量；队列满时动作失败但键盘系统不被阻塞。
- Hook 使用 RAII 在消息循环退出时调用 `UnhookWindowsHookEx`。

Windows 可能静默移除超时的低级 Hook，因此所有慢操作都必须移出回调。[LowLevelKeyboardProc](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc)

## 4. Prefix 状态

    Idle
      │ WT foreground + Ctrl+B down / consume
      ▼
    Armed(target, deadline)
      ├── mapped key / consume + action ──► Idle
      ├── B / consume + literal Ctrl+B ──► Idle
      ├── Escape / consume ──────────────► Idle
      ├── unknown / consume ─────────────► Idle
      ├── timeout ───────────────────────► Idle
      ├── foreground changed ────────────► Idle
      └── system switch key / pass ──────► Idle

状态机记录已吞按键，确保其 repeat 和 keyup 不会泄漏到 Shell。

## 5. Action Bridge

Windows Terminal keybinding 只能表达“多个 modifier + 一个非 modifier 键”，不能表达两阶段 Prefix。`multipleActions` 也只是一次按键执行多个 Action。因此 Rust 负责 Prefix，Terminal 只接收隐藏单 chord。[Windows Terminal Actions](https://learn.microsoft.com/en-us/windows/terminal/customize-settings/actions)

Bridge 分两层：

1. `%LOCALAPPDATA%\Microsoft\Windows Terminal\Fragments\Winter\actions.json` 提供命名 Action。
2. 每个 Terminal 通道的用户 `settings.json` 只绑定产品隐藏 chord 到这些 Action ID。

> 说明：配置路径、Action ID、fragment 目录与 Shell 标记均使用 Winter 品牌命名；
> 历史上的 `WinTerminalP` 安装由 `winter install` 自动迁移。

桥接表使用 F13、F14、F15、F18–F24 的三组修饰键组合。F16/F17 已在 Stable 1.24 真实输入链路中判定为不可靠并由单元测试禁止重新进入托管表。`send_prefix_literal` 是唯一例外：控制器按当前配置的 Prefix 直接注入对应 chord，不经过静态桥接，因此自定义 Prefix 无需重新安装桥接。

两张表不再靠手工同步：`src/registry.rs` 的 `ACTIONS` 是唯一动作注册表，`keymap::MANAGED_BINDINGS`（29 条托管绑定，保持注册表顺序以保证已安装 fragment/keybindings 字节稳定）与 `prefix::SHORTCUT_SPECS`（30 条快捷键规格，按 `SPEC_ORDER` 置换为用户可见顺序）都在编译期由注册表派生；模块底部的 `const` 断言在编译期拒绝重复或不完整的注册表。引入新的 `TerminalAction` 只需在注册表加一条记录。

官方自 1.21 起允许 fragment 提供 Action，但明确禁止 fragment 注入 keys；这是防止第三方静默劫持快捷键的安全边界。[Action fragment implementation](https://github.com/microsoft/terminal/pull/16185)

## 6. 前台身份与派发

捕获 Prefix 时保存：

- 顶层 HWND。
- `WindowsTerminal.exe` PID。
- 进程启动时间标记。
- Stable、Preview、Canary、Unpackaged 或 Portable 通道。

派发前重新读取前台窗口并逐字段比较。任何字段变化都返回 `ForegroundChanged`，不调用 `SendInput`。

`wt.exe --window 0` 只表示最近使用窗口，不能替代该校验。[Windows Terminal command line arguments](https://learn.microsoft.com/en-us/windows/terminal/command-line-arguments)

## 7. 配置持久化

### 应用配置

`%LOCALAPPDATA%\Winter\config.toml`

同一目录下还持久化其余运行状态：集成状态与备份（`integration\`，内含 `manifest.json`
与 `backups\`）、控制器发布的仪表盘状态 `dashboard.json`（`winter ui` 的数据源），以及
`winterd` 的诊断日志 `last-error.log`。

```toml
schema_version = 2
prefix_timeout_ms = 1500
launch_terminal_on_start = true
prefix = "ctrl+b"

[shortcuts]
focus_left = "left"
split_right = "shift+right"
resize_up = "ctrl+up"
new_tab = "c"
shutdown = "q"

[mouse_resize]
enabled = true
divider_hit_slop_px = 8
geometry_poll_interval_ms = 100

```

创建配置时使用 create-new；已有配置解析失败时保留原文件，不自动覆盖。`shortcuts` 是按动作名合并的 override 表，缺失项使用 Rust 中唯一的默认 `ShortcutSpec`。旧 schema 1 文件缺少 `prefix`、`shortcuts` 或 `[mouse_resize]` 时仍能加载默认值，并只在内存中迁移为 schema 2；不会隐式改写用户文件。

启动控制器前，Config 层把字符串解析为规范化 `KeyChord`，验证未知动作、重复 chord、系统保留组合和必需退出键，再一次性构造不可变的 `PrefixConfig`。Hook 回调只查内存表，不读取 TOML；修改配置需要重启控制器。

### Terminal 配置

Terminal settings 是 JSONC，允许注释和尾逗号。Integration 使用 CST 只编辑根 `keybindings`，并保存：

- 原字节备份。
- pre/post SHA-256。
- 目标通道和规范路径。
- 托管 ID、chord 和语义哈希。
- committed manifest 中的托管语义与前后哈希。

写入使用同目录临时文件、CAS 和原子替换。

运行期失败会通过内存逆操作栈恢复已完成的写入；每一步回滚前再次做 CAS，因此不会覆盖并发用户修改。当前版本没有跨进程持久化 prepared journal，强杀或断电后的恢复依赖原字节备份和 `doctor` 诊断。

### PowerShell Shell Integration

Windows Terminal 只有在 Shell 通过 `OSC 9;9` 报告 CWD 时，才会让 `splitMode: duplicate` 的窗格继承当前目录（WT 1.18–1.25 的 `_HandleSplitPane` 把聚焦 Tab 交给 `_MakeTerminalPane`，后者读取活动控件的 `WorkingDirectory`，只要该值非空就覆盖 Profile 的 `startingDirectory`，否则回落到默认的 `%USERPROFILE%`；`main`/1.26 起改为要求该值是一个存在的目录）。因此 `winter install` 会向 `Documents\WindowsPowerShell\Microsoft.PowerShell_profile.ps1` 和 `Documents\PowerShell\Microsoft.PowerShell_profile.ps1` 追加一段带 `# >>> Winter shell integration >>>` / `# <<< Winter shell integration <<<` 标记的提示符包装：它保存原 `prompt`，先输出 `OSC 9;9`，再调用原提示符。安装使用与 Terminal 配置相同的快照、备份和 CAS 写入；重复安装幂等，卸载只移除内容未被用户修改的受管块，用户改动会被保留并报告。写入保持原 Profile 编码（UTF-8 / UTF-16LE），无法识别的编码只报告为冲突、不修改文件。该步骤不参与 `bridge_is_ready` 判定，任何 Profile 失败都只体现在报告里，不影响控制器启动，只会失去目录继承。

PowerShell 7 自己不会创建 `Documents\PowerShell`：只要没有任何工具为它生成过 `$PROFILE`，该目录就不存在，而 `pwsh` 正是用它。旧实现把“目录不存在”一律当作“该 Shell 未安装”而跳过，结果是最常见的 pwsh 场景完全没有包装、分屏永远回落到 `%USERPROFILE%`。现在 `shell::ShellPresence` 会先探测宿主可执行文件（`PATH` 上的 `pwsh.exe`——覆盖 Microsoft Store 的应用执行别名——以及 MSI 安装根目录 `%ProgramFiles%\PowerShell\7[-preview]`；`powershell.exe` 同理探测 `System32`），只有确认宿主也不存在时才保持 `Skipped`；宿主存在而目录缺失时报告 `Create` 并由原子写入顺带创建目录。该探测结果按目标注入，`plan_with` / `install_with` 使单元测试不依赖运行测试的机器。

提示符包装只在文件系统位置上报目录：`$loc.Provider.Name -eq 'FileSystem'` 时才输出 `$loc.ProviderPath`。1.25 及更早版本对上报表只做非空判断，把 `HKLM:\` 这类 PSDrive 路径原样交给 `CreateProcess` 会让下一次分屏直接失败（WT #20373）；非文件系统位置保持静默后，WT 会继续使用构造时预置的 Profile 目录，分屏至少能正常打开。

## 8. 安全边界

- Hook 只判断有限快捷键和原生分隔线附近的左键拖动，不记录字符流或鼠标轨迹。
- 非 Terminal 前台全部透传。
- 非分隔线区域的鼠标输入全部透传；拖动期间消费左键 down/up、透传 move。move 必须透传，因为消费低级鼠标移动会冻结硬件光标，使 `pt` 不再累积，拖动残差无法推进；down 已被消费，目标窗口不会因此进入文本选择。
- 控制器不注册或替换 Windows Terminal 的 OLE `IDropTarget`。从 Explorer 开始的文件/目录拖放不会被控制器截获，仍由 Windows Terminal 或目标 Shell 按其原生规则处理。
- injected 输入不能激活 Prefix；递归防护依据系统注入标志 `LLKHF_INJECTED`，自身注入仍附带固定 extra-info 标记，但 Hook 不再读取该标记。
- Action ID 和 fragment 目录均使用产品命名空间。
- 用户冲突默认阻断，不自动抢占。
- 控制器运行入口会先检测访问令牌；未提权时通过 UAC 以相同命令自重启，只有提权实例可安装 Hook、UI Automation 与输入桥接。因此管理员启动的 Windows Terminal 可使用全部 Prefix 与鼠标 resize 功能。`SendInput` 仍只向相同或更低完整性目标注入。[SendInput](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)
- `winter run`、`winter launch` 和 `winterd.exe` 只要进入控制器运行态就必须提权；`config`、`plan`、`install`、`uninstall`、`doctor` 仍是无副作用或用户配置命令，不触发 UAC。
- 控制器不开放网络端口、不读取终端缓冲区。

## 9. 失败语义

| 失败 | 行为 |
|---|---|
| 非 Terminal 前台 | Prefix 透传 |
| 前台在派发前变化 | 丢弃动作 |
| Bridge 未安装 | `winter`/`winter launch` 首次启动自动执行安装；检测到真实冲突时仍报错并提示 `winter doctor` |
| Hook 安装失败 | 控制器退出并记录错误 |
| 队列满 | 丢弃本次动作，不阻塞 Hook |
| SendInput 部分成功/失败 | 返回结构化错误并释放本次修饰键 |
| UI Automation 初始化失败 | 启用鼠标拖动时控制器启动失败并记录明确错误；可在配置中关闭该功能 |
| 几何快照瞬时读取失败 | 清空本次快照、禁用命中并累计诊断，不猜测旧分隔线 |
| 拖动期间前台变化 | 终止拖动，不向新窗口派发 resize |
| JSONC 无法解析 | 不写文件 |
| 安装时文件并发变化 | CAS 失败并重新规划 |
| 卸载时用户已修改 | 只保留用户修改并报告 |
| Hook 回调 panic | `HookHealth` 原子计数并禁用 handler，后续事件 fail-open 透传；控制器的 `recv_timeout` 看门狗轮询发现后返回错误，而不是永久阻塞在等待上 |
| Hook 线程关闭超时 | 有界 join（3 秒、20 ms 轮询）后返回 `HookShutdownTimeout` 并分离线程，不无限等待 |
| 关闭握手超时 | `Q` 的按下与释放必须在 3 秒窗口内完成；超时或期间出现其他键则丢弃握手，不会稍后误关控制器 |

错误模型按可恢复性区分三类：`SettingsConflict` 表示磁盘内容与操作预期分歧（含 CAS 检出的并发用户修改），文件保留给用户处置；`OperationIncomplete` 表示工具侧未能完成（回滚未全部还原、持久化失败或不可能的中途状态），系统可能处于部分修改状态，由原字节备份与 manifest 描述残余；`Platform` 包装 Windows API 失败并保留 `PlatformError` 的 `source()` 链（可附加诊断 context），调用方能沿链定位底层原因，而不是只得到扁平化字符串。

## 10. 架构限制

- Windows Terminal 没有公开的远程 Action API或 pane tree 查询 API。
- UI Automation 只暴露可见 `TermControl` 的屏幕矩形，不暴露真实 pane tree；因此拖动以可见相邻矩形推导分隔线，并使用 Terminal 原生约 5% 父区域步长，而不是像素级重排。
- 由于没有 pane tree，Winter 无法直接指定"移动哪条分隔线"，只能先证明"指针下这条线就是 `resizePane` 会移动的那条"。`pane_layout` 的做法是选择一个可证的聚焦目标——贴布局前缘（或镜像的贴后缘）且一边正好落在该线上的窗格；焦点叶向上最近的同轴祖先节点的分隔线必然就是这条线，且该论证与具体树形无关。无法证明时（例如一排四个窗格的中间线，矩形在两种一致树形下给出不同归属）拖动**不捕获**、左键透传，由 Windows Terminal 自行处理：这是刻意的 fail-closed 取舍，宁可拖不动也不移动错的分隔线。真机探针 `pointer_drag_moves_only_the_dragged_divider` 断言"其他分隔线坐标不动"。
- SendInput 在重验目标和实际输入之间存在不可完全消除的微小竞争窗口。
- 控制器只能提供 tmux 风格操作，不提供 tmux 的后台 session server。
- 当前目录复制由 Windows Terminal 的 `duplicate` 语义加 Shell Integration 提供；Winter 只负责安装受管的 `OSC 9;9` 提示符包装。包装只对新启动的 Shell 生效（已运行的会话需重新打开标签页或重新加载 Profile），cmd、WSL 等非 PowerShell Shell 不在包装范围内；Shell 不加载 Profile 时仍回退到 Windows Terminal 默认目录。
- Windows Terminal 不公开 Action 执行回执或 pane tree 查询；`SendInput` 成功只证明事件已插入，不能单独证明布局已改变。
