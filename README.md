# No Sleep Till Done

No Sleep Till Done is a macOS menu bar app that keeps a MacBook awake while long-running work finishes, including when the lid is closed.

The app ensures its privileged controller is installed and running, keeps the Mac awake when the lid closes, immediately asks macOS to sleep the display, then puts the computer to sleep only if the lid remains closed for the configured delay. The menu bar item shows charge digits, a vertical battery tick bar, and state marker as one compact icon.

## Safety

This intentionally overrides normal lid-close sleep. Keep the delay short, especially before putting the MacBook in a bag.

The app owns `SleepDisabled` only while it is enabled. The menu bar app itself does not prompt for a password at normal startup. The resident background service watches the app's heartbeat and disables battery lid-close sleep only while the heartbeat is fresh and enabled. Disabling or quitting restores `pmset -b disablesleep 0` without another password prompt. The app does not set the normal battery idle sleep timer to `0`, so deliberate Apple-menu Sleep should remain available while the app is running.

Process-aware waiting only applies after the lid-close delay expires. While the app heartbeat is active, the controller still keeps lid-close sleep disabled even when the lid is open.

## Components

| Component | Purpose |
| --- | --- |
| `No Sleep Till Done.app` | Normal macOS menu bar app, launched from Spotlight or Finder. |
| `no-sleep-till-done` | Privileged delayed lid-sleep controller, normally run by LaunchDaemon. |
| `no-sleep-till-done-watchdog` | Privileged safety watchdog that restores normal battery lid-close sleep when the controller stays absent. |

## Menu Bar State

| Marker | Meaning |
| --- | --- |
| Green | Ready: lid open and sleep override is active. |
| Amber | Lid closed; delay timer is active. |
| Blue | Lid closed; waiting for configured processes or their exit grace timer. |
| Red | Sleep override appears off or unsafe. |
| Gray | State could not be read. |

## Requirements

- macOS 13 or newer
- Rust toolchain when building from source

## Build From Source

```bash
cargo build --release --bins
scripts/build-app-bundle.sh
```

## Dry Run

```bash
./target/release/no-sleep-till-done --dry-run --once
./target/release/no-sleep-till-done --dry-run --delay-seconds 60
```

## Install And Run

The app you run is `No Sleep Till Done.app`. It does not need arguments, `sudo`, or environment variables.

```bash
open "target/release/No Sleep Till Done.app"
```

Install it in your user Applications folder so Spotlight can find it:

```bash
scripts/build-app-bundle.sh "$HOME/Applications"
open "$HOME/Applications/No Sleep Till Done.app"
```

`No Sleep Till Done Enabled` controls the sleep override without quitting the menu app. The setting persists in the config file and does not require Touch ID or a password. `Reload Configuration` validates the file and tells the resident background service to apply controller settings without restarting it.

The menu includes `Start at Login: On/Off`. That toggle writes or removes the current user's LaunchAgent and does not require Touch ID or a password. It changes future login behavior without relaunching or stopping the currently running menu app.

The menu checks the background service and safety watchdog. On the first install, choose `Install Background Service...` and approve the one administrator prompt. The maintenance item is hidden while both services are healthy. It reappears as `Repair Background Service...` or `Update Background Service...` only when action is required. Normal starts, quits, enable changes, configuration reloads, and start-at-login changes do not ask for a password.

The menu's `Quit` item removes the app heartbeat and asks the resident controller to restore normal lid-close sleep. If the controller is unavailable, Quit prompts for administrator approval and resets `SleepDisabled` directly as a fallback.

## Manual Controller Run

The controller binary changes `pmset` settings, so a real manual run must use sudo. Without sudo, use `--dry-run` only.

```bash
sudo ./target/release/no-sleep-till-done
```

The controller normally runs from the LaunchDaemon, not from your shell.

## Config

The menu bar app creates this file if it is missing:

```text
~/.config/no-sleep-till-done/config.toml
```

Use `Open Configuration...` to open it in the default text editor. Menu-only values such as colors and refresh frequency update during the normal menu refresh. When controller values change, the status reports `configuration changed; reload required`; choose `Reload Configuration` to apply them.

The controller reads this file on startup and whenever the menu requests a reload. A successful reload does not restart the controller. Invalid TOML is rejected and the controller keeps its last valid settings. CLI flags still override TOML values during manual runs.

Upgrading from the former **LidSleep Delay** name is automatic. If the new file is absent, the app copies `~/.config/lidsleep-delay/config.toml` to the new location, preserves its settings, and migrates an existing start-at-login item. The old privileged service remains active until you choose `Update Background Service...`, which replaces it after one administrator approval.

Existing config files are also updated in place when new sections or keys are added.

When the controller is run with sudo or as a root LaunchDaemon, it still resolves this config to the invoking or active console user's home by default. `NO_SLEEP_TILL_DONE_CONFIG` is only needed for tests or unusual setups.

To keep the Mac awake for long-running commands after the lid-close delay expires, enable process waiting and add full-command substrings:

```toml
enabled = true

[process_wait]
enabled = true
command_substrings = [
  "codex",
  "python /path/to/agent-daemon.py",
]
exit_grace_seconds = 300
```

The controller matches these values case-sensitively against `/bin/ps -axo pid=,command=` output. When all matches disappear, it waits `exit_grace_seconds`, then sleeps if the lid is still closed.

For AI-agent guards, use the exact marker substring `nosleeptilldone`. The application binaries deliberately use the hyphenated name `no-sleep-till-done`, so this marker cannot match the app itself.

After editing controller settings, choose `Reload Configuration`. You do not need to quit or restart the app.

## Advanced Manual LaunchDaemon Install

The app normally installs these privileged services through the menu. Use this manual flow only for debugging or deployment without the menu app.

```bash
sudo install -m 755 target/release/no-sleep-till-done /usr/local/sbin/no-sleep-till-done
sudo install -m 755 target/release/no-sleep-till-done-watchdog /usr/local/sbin/no-sleep-till-done-watchdog
sudo install -m 644 launchd/com.evren.nosleeptilldone.plist /Library/LaunchDaemons/com.evren.nosleeptilldone.plist
sudo install -m 644 launchd/com.evren.nosleeptilldone.watchdog.plist /Library/LaunchDaemons/com.evren.nosleeptilldone.watchdog.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.evren.nosleeptilldone.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.evren.nosleeptilldone.watchdog.plist
sudo launchctl enable system/com.evren.nosleeptilldone
sudo launchctl enable system/com.evren.nosleeptilldone.watchdog
```

The controller LaunchDaemon restarts only unsuccessful exits. The watchdog checks the controller's `launchd` state every 5 seconds. If `com.evren.nosleeptilldone` is not running for 20 continuous seconds, it runs `pmset -b disablesleep 0`.

## Install Menu Bar LaunchAgent

```bash
scripts/build-app-bundle.sh /Applications
cp launchd/com.evren.nosleeptilldone.menubar.plist ~/Library/LaunchAgents/com.evren.nosleeptilldone.menubar.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.evren.nosleeptilldone.menubar.plist
launchctl enable gui/$(id -u)/com.evren.nosleeptilldone.menubar
launchctl kickstart -k gui/$(id -u)/com.evren.nosleeptilldone.menubar
```

## Uninstall

```bash
sudo launchctl bootout system /Library/LaunchDaemons/com.evren.nosleeptilldone.watchdog.plist
sudo launchctl bootout system /Library/LaunchDaemons/com.evren.nosleeptilldone.plist
sudo rm -f /Library/LaunchDaemons/com.evren.nosleeptilldone.watchdog.plist
sudo rm -f /Library/LaunchDaemons/com.evren.nosleeptilldone.plist
sudo rm -f /usr/local/sbin/no-sleep-till-done-watchdog
sudo rm -f /usr/local/sbin/no-sleep-till-done
launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/com.evren.nosleeptilldone.menubar.plist
rm -f ~/Library/LaunchAgents/com.evren.nosleeptilldone.menubar.plist
rm -rf "/Applications/No Sleep Till Done.app" "$HOME/Applications/No Sleep Till Done.app"
```

## Options

```text
--delay-seconds N          Seconds lid must stay closed before sleep
--poll-seconds N           Lid polling interval in seconds
--dry-run                  Log commands instead of changing power settings
--once                     Read lid state once, then exit
--keep-disabled-on-exit    Leave battery SleepDisabled enabled when daemon exits
```

Watchdog options:

```text
--interval-seconds N      Seconds between controller checks
--grace-seconds N         Seconds controller may be absent before repair
--dry-run                 Log repair commands instead of changing power settings
--once                    Check once, then exit
```

## License

Apache License 2.0. See [LICENSE](LICENSE).
