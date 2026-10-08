# Winter 测试策略

## 1. 自动化门槛

与 CI 五个并行作业（fmt / lint / test / build / MSRV）保持一致，全部使用 `--locked`：

```powershell
cargo --locked fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo test --doc --locked
$env:RUSTDOCFLAGS = '-D warnings'; cargo doc --no-deps --locked
cargo build --release --bins --locked
```

`cargo test --all-targets` 不跑 doc test，所以必须单独执行 `cargo test --doc --locked`；MSRV 作业另在 Rust 1.88 上运行 `cargo check --all-targets --locked`。

## 2. Prefix 单元测试

- 非 Windows Terminal 前台的 Ctrl+B 完全透传。
- Ctrl+B 激活 Prefix，并吞掉 B down/repeat/up。
- Arrow、Shift+Arrow、Ctrl+Arrow 映射正确。
- C/N/P/0–9/X/Z/逗号映射正确。
- Escape、未知键、系统切换键符合取消规则。
- B 释放后再次按 B 只发送一次 literal。
- 截止时刻采用 `now >= deadline`。
- 前台 WindowIdentity 变化后取消且不转投。
- injected 输入永远透传。
- 已吞 keydown 的 repeat 和 keyup 在状态重置后仍被吞掉。
- 自定义 Prefix 与动作 chord 能驱动同一个状态机，不存在只改配置未改运行时的双实现。
- chord parser 覆盖大小写、空格、方向键、标点、F1–F12、重复 modifier 和非法多键。
- 任意左键交互会取消待完成 Prefix，但仍平衡已吞掉的 keyup。

## 3. Shortcut 配置测试

- 旧 schema 1 三字段 TOML 自动获得默认 Prefix、30 个动作（`registry.rs` 派生的 `ACTION_COUNT`）和鼠标配置，在内存中迁移到 schema 2，且不改写原文件。
- Partial `[shortcuts]` 只覆盖指定动作，其他动作回落默认值。
- 自定义 Prefix 和动作能编译为不可变运行时映射。
- 未知动作名、重复 chord、普通无修饰 Prefix、系统保留键必须失败。
- 可选动作可设为 `disabled`，`shutdown` 不可禁用。
- `winter config` 的完整 TOML 可以再次解析并保持相同语义。
- 旧配置缺少 `[mouse_resize]` 时获得安全默认值；命中扩展和刷新周期越界时拒绝启动。

## 4. 窗格拖动模型测试

- 从相邻 `TermControl` 矩形推导横向、纵向及嵌套分隔线。
- 分隔线可命中区域包含原生间隙与有限可配置扩展，不误命中远离边界的正文。
- 指针位移按局部父分栏的 5% 换算为离散 `resizePane` 动作，并保留不足一步的残差。
- 单个 mouse move 的动作数量有上限，不能绕过有界队列。
- 非法、重叠或距离过远的矩形不会生成伪分隔线。

## 5. Bridge 测试

- prefix 快捷键规格（30 条）与托管桥接绑定（29 条）均由 `registry.rs` 注册表在编译期派生；`const` 断言拒绝重复动作/ID/chord、不完整的 `SPEC_ORDER` 置换和多余 shutdown，测试验证派生表与注册表逐条一致。
- 所有 TerminalAction 都有唯一 ID、command 和 chord。
- ID 均以 `User.WinTerminalP.` 开头。
- 托管 chord 只使用 F13、F14、F15、F18–F24 和约定 modifier，F16/F17 必须被测试阻止进入托管表。
- fragment 包含 actions 且不包含 keys/keybindings。
- 用户 keybinding 引用的 ID 全部存在于 fragment。
- direction、tab index 和 `splitMode: duplicate` 序列化正确。

## 6. JSONC/事务测试

Fixture 至少覆盖：

- 空对象、无 keybindings、已有空数组。
- `//` 与 `/* */` 注释。
- CRLF/LF、UTF-8 BOM、尾逗号、不同缩进。
- 已安装配置的幂等执行。
- 同 ID 异义、同 chord 异义、用户 unbound。
- 安装计划阶段零写入。
- preHash 变化导致 CAS 中止。
- 写后重新解析和托管项验证。
- 当前配置未变化时恢复原始字节。
- 当前配置已变化时只删除仍匹配 manifest 的项。
- 用户修改的托管项保留。

## 7. Win32 测试

- 前台非 Terminal 返回 None。
- Stable/Preview/Canary 路径识别。
- HWND、PID 或启动标记变化时拒绝派发。
- SendInput 插入数量不足返回错误。
- 自身注入的键盘/鼠标事件凭系统注入标志一律透传（不再依赖 extra-info 标记）。
- 单实例 mutex 第二次获取返回 already-running。
- Hook guard drop 时卸载 Hook。
- 键盘与鼠标 Hook 在同一线程安装、停止，panic 后统一 fail-open。
- injected 鼠标事件与 injected 键盘事件一样始终透传。
- UI Automation 只返回有效 `TermControl` 矩形，并在聚焦前重验前台 WindowIdentity。
- 控制器核心在访问令牌未提权时拒绝启动；`winter run`、`winter launch` 和 `winterd` 会把原始运行参数经 UAC 自重启，避免低完整性实例悄然安装 Hook 后对管理员 Terminal 注入失败。

## 8. 真实 Windows Terminal 验证

真实验证必须使用专用测试窗口，不能操作用户正在工作的 Terminal：

1. 记录 Stable settings 原始 SHA-256 和备份路径。
2. 运行 `plan`，确认只增加 `User.WinTerminalP.*`。
3. 运行 `install`，重新打开专用 Windows Terminal 窗口。
4. 启动前台控制器。
5. 验证四向 split、focus、键盘 resize，并在横向、纵向、嵌套分栏上拖动原生边界。
6. 验证 close、zoom、new/next/prev/switch/rename tab。
7. 同时打开两个 Terminal 窗口，确认只操作捕获 Prefix 的窗口。
8. 切换到记事本验证 Ctrl+B 未受影响。
9. 运行 uninstall，确认用户配置和 fragment 恢复。
10. 对照初始哈希；若 Terminal 自身重写过配置，执行语义 diff 并保留备份证据。

仓库提供四个默认忽略的真实桌面探针，必须只对专用测试窗口显式运行：

```powershell
$env:WINTERMINAL_E2E_HWND = '<专用窗口十进制 HWND>'
$env:WINTERMINAL_E2E_ACTION = 'focus-left'
$env:WINTERMINAL_E2E_EXPECTED_TITLE = '预期标题片段'
cargo test --test live_bridge dispatch_bridge_action_from_environment -- --ignored --exact --nocapture
cargo test --test live_bridge managed_bridge_chords_are_available_as_global_hotkeys -- --ignored --exact --nocapture
$env:WINTERMINAL_E2E_EXPECTED_PANES = '3'
cargo test --test live_bridge native_pane_geometry_from_environment -- --ignored --exact --nocapture
cargo test --test live_bridge combined_input_hooks_install_and_stop -- --ignored --exact --nocapture
```

控制器会主动忽略 injected 键盘和鼠标事件以防递归与自动化误操作，因此不能用 `SendInput` 自动伪造完整的物理 `Ctrl+B` Prefix 或真实边界拖动验收。

Explorer 开始的文件/目录拖放也必须保持原生透传：从 Terminal 分隔线外开始拖动，经过分隔线并释放时，控制器不得消费 down/move/up。控制器不创建覆盖窗口或自定义 OLE drop target；Windows 的 UAC 边界不允许普通 Explorer 向管理员 Terminal 直接 OLE 投放，此项不能由控制器绕过。将文件投到某个 Shell 后如何处理属于 Windows Terminal 与该 Shell 的原生契约。

真实 Explorer → Terminal 验收仍须手工验证：拖入一个含空格、中文和单引号的测试文件，确认鼠标不是禁止符号，并由 Windows Terminal 直接将路径放入 PowerShell 的输入缓冲；控制器不得消费该拖放的 down/move/up。

## 9. 不可由单元测试证明的边界

- UIPI 对管理员 Terminal 的真实阻止行为。
- Windows 在高负载时对低级 Hook 的调度和静默移除。
- 重验 HWND 与 SendInput 之间极短 TOCTOU 窗口。
- Windows Terminal 内部 Action 是否实际改变 pane tree。
- 物理鼠标按下、拖动、释放时的最终可视手感和光标反馈。
- Shell Integration 的当前目录继承。

这些项目必须在目标 Windows 版本和 Terminal 通道上手工或端到端验证，不能仅凭编译通过宣称完成。
