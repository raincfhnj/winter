# Winter 开发指南

## 环境

- Windows 10/11 x64。
- Rust stable，最低 Rust 1.88，项目当前使用 2024 Edition。
- Windows Terminal 1.21+。
- MSVC C++ Build Tools。

项目根目录是纯 Rust 控制器，不包含旧 Tauri/xterm 原型。

终端用户只需执行一次 `.\install.ps1`：它构建 Release、把 `winter` 安装到 PATH，并执行 `winter install`。之后在任意 shell 输入 `winter` 即可启动，重启后依然有效；若集成缺失，`winter` 会在首次启动时自动重新安装。

## 常用命令

质量门与 CI 完全一致（CI 在 `windows-latest` 上拆成 fmt / lint / test / build / MSRV 五个并行作业，全部使用 `--locked`）：

```powershell
cargo --locked fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo test --doc --locked
$env:RUSTDOCFLAGS = '-D warnings'; cargo doc --no-deps --locked
cargo build --release --bins --locked
```

MSRV 作业另在 Rust 1.88 工具链上运行 `cargo check --all-targets --locked`。

开发期以前台模式运行控制器：

```powershell
cargo run --bin winter -- run --no-launch
```

安装计划必须先只读查看：

```powershell
cargo run --bin winter -- plan
cargo run --bin winter -- doctor
```

确认无冲突后安装：

```powershell
cargo run --bin winter -- install
```

Release 隐藏控制器：

```powershell
cargo build --release --bins
target\release\winterd.exe
```

`winterd.exe` 可直接双击：集成已安装时，它在后台启动控制器并打开原生 Windows Terminal，不创建 Winter 自己的界面。`winter.exe` 是推荐的短命令入口，`winterminalp.exe` 保留为 Winter 的兼容别名：

```powershell
target\release\winter.exe
target\release\winter.exe doctor
target\release\winter.exe install
target\release\winter.exe uninstall
```

若通过 `cargo install --path . --bin winter` 安装到已经加入 `PATH` 的 Cargo bin 目录，可在任意目录直接运行 `winter`。

查看或编辑用户快捷键：

```powershell
winter config
winter config --path
winter config --edit
```

Shortcut 字段采用 `action_name = "modifier+key"`；缺失项继承默认值。配置解析、重复键与系统保留键校验必须留在 `config`/`prefix`，不能放进 Hook callback。

## 代码边界

| 模块 | 职责 |
|---|---|
| `model` | 方向、动作、Terminal 通道和窗口身份 |
| `config` | 应用 TOML schema、兼容默认值与 Shortcut 校验 |
| `registry` | 唯一动作注册表：编译期派生 prefix 快捷键规格与 29 条托管桥接绑定，`const` 断言查重查数 |
| `keys` | 规范键名表：配置 parse/display 与虚拟键正/反向映射的单一来源 |
| `keymap` | `ManagedBinding`/`BridgeChord` 类型与 registry 派生的桥接视图（`MANAGED_BINDINGS`） |
| `prefix` | KeyChord 契约和不依赖 Win32 的纯 Prefix 状态机；动作表由 registry 派生 |
| `pane_layout` | 纯几何分隔线推导、命中测试和拖动步长状态 |
| `platform/windows` | 统一输入 Hook、UI Automation、前台识别、SendInput、单实例和启动 |
| `integration` | 按职责拆分的 settings 事务层：discovery/targets/fragment/jsonc/transaction/rollback/manifest/helpers/shell/types |
| `controller/keyboard` | Win32 虚拟键与物理 modifier 的无 IO 规范化 |
| `controller/desktop` | 前台身份与可丢弃窗格几何快照，不拥有布局 |
| `controller` | Prefix/拖动 reducer、有界工作队列和 dispatcher 编排 |
| `bin` | CLI 与隐藏 daemon 入口 |

## Rust 规范

- 使用 `rustfmt` 默认格式。
- Clippy 以 `-D warnings` 运行。
- 可恢复错误返回 `Result`，不在生产输入上 `unwrap` 或 `expect`。
- 所有 HANDLE、Hook 和 Mutex 必须使用 RAII。
- `unsafe` 块必须小且带安全不变量说明。
- Hook callback 禁止 IO、阻塞和进程启动。
- UI Automation 只能在 observer/worker 线程调用，不能进入 Hook callback。
- 不保存或自行修改 pane tree；`PaneLayout` 只能由最新原生矩形重新推导。
- 测试 fixture 可以使用 `expect`，生产路径不使用。

## 配置安全

- 开发测试优先使用临时目录中的 JSONC fixture。
- 不直接格式化或重写真实 `settings.json`。
- 真实安装前记录目标绝对路径、原始哈希和备份路径。
- 不修改 `defaults.json` 或 `state.json`。
- 测试卸载时仅操作 `User.WinTerminalP.*` 命名空间。
