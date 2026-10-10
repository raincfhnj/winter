# Winter

**tmux-style keyboard control for Windows Terminal — without replacing it.**

Winter adds a tmux-like two-stage prefix (`Ctrl+B`, then a second key) to the Windows
Terminal you already use. It never draws a window, never hosts a terminal, and never
manages a PTY: Windows Terminal stays the only UI, renderer, pane tree, tab manager, and
shell owner, and your profiles, themes, fonts, shells, and existing keybindings are
untouched.

![Winter driving a real Windows Terminal workspace](assets/demo.gif)

## Install

```powershell
git clone https://github.com/raincfhnj/winter.git
cd winter
.\install.ps1
```

One run of `install.ps1` builds the release binaries, puts `winter` on your `PATH`,
installs the Windows Terminal integration, and registers a per-user logon task. It needs
administrator approval exactly once, for that task.

Afterwards the prefix is available in every session and comes back by itself after a
restart: there is nothing to run again. To inspect or change that:

```powershell
winter autostart status     # read-only report, no UAC
winter autostart enable     # re-register after moving the binaries
winter autostart disable    # stop starting at sign-in
```

## Keys

Shortcuts work while Windows Terminal is in the foreground: press and release `Ctrl+B`,
then press the second key.

| Second key | Action |
|---|---|
| `←` / `→` / `↑` / `↓` | Focus the pane in that direction |
| `Shift` + arrow | Split a new pane in that direction |
| `Ctrl` + arrow | Resize the active pane in that direction |
| `C` | New tab |
| `N` / `P` | Next / previous tab |
| `0`–`9` | Activate the zero-based tab index |
| `X` | Close the active pane |
| `Z` | Toggle pane zoom |
| `,` | Rename the current tab |
| `B` | Send a literal prefix (`Ctrl+B` to the shell) |
| `Q` | Stop the controller (does not close Windows Terminal) |
| `Escape` | Cancel the prefix |

The prefix expires after 1500 ms and any unbound second key cancels it. Move the pointer
over a native pane divider and drag to resize; sizes follow Windows Terminal's native
~5% steps.

Windows Terminal exposes no pane tree, so Winter proves from the observed rectangles
which separator a drag would move, and only then takes the mouse. In a layout where that
cannot be proven from the rectangles alone — some nested arrangements where the separator
under the pointer has more than one possible owner — the drag is passed straight through
to Windows Terminal instead of moving a separator you did not grab. Terminal keeps the
window's own divider cursor either way.

## Usage

| Command | What it does |
|---|---|
| `winter` | Start the controller and open Windows Terminal |
| `winter ui` | Live session manager in this pane: controller status, tabs, panes |
| `winter config` | Print the effective configuration (`--path`, `--edit`) |
| `winter plan` / `install` / `uninstall` | Preview, apply, or remove the integration |
| `winter doctor` | Machine-readable health report (JSON) |
| `winter autostart status\|enable\|disable` | Manage the logon task |

Rebind anything in `%LOCALAPPDATA%\Winter\config.toml`. Only the actions you want to
change need to be listed, and shortcut edits never require re-running `winter install`.

```toml
prefix = "ctrl+a"

[shortcuts]
focus_left = "h"
focus_down = "j"
focus_up = "k"
focus_right = "l"
new_tab = "t"
shutdown = "q"
```

## Uninstall

```powershell
winter uninstall
```

Removes the logon task, the action fragment, the hidden keybindings, and the shell
block — only entries Winter still owns are touched, and anything you edited yourself is
preserved and reported.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
