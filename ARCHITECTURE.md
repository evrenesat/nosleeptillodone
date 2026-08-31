# Architecture

No Sleep Till Done has three processes: a root-run controller, a root-run safety watchdog, and a user-session menu bar app.

| Area | Design |
| --- | --- |
| Menu bar | `No Sleep Till Done.app`, an `LSUIElement` macOS app wrapping `no-sleep-till-done-menubar`; it owns user controls, configuration validation, and the active lease. |
| Controller | `no-sleep-till-done`, installed as a resident root LaunchDaemon by the app; it stays running and activates power control only while the app heartbeat is fresh. |
| Watchdog | `no-sleep-till-done-watchdog`, installed as a root LaunchDaemon by the app; if the controller is not `launchd`-running past its grace window, it runs `pmset -b disablesleep 0`. |
| Lid state | Reads `AppleClamshellState` from `IOPMrootDomain` through IOKit/CoreFoundation FFI. |
| Sleep override | Uses `pmset -b disablesleep 1` while the app heartbeat is active, without changing the normal battery idle sleep timer. |
| Display behavior | Runs `pmset displaysleepnow` on the lid-close edge. |
| Wake display | Runs `caffeinate -u -t 2` on the lid-open edge. |
| Delayed sleep | If the lid remains closed past the configured delay, temporarily disables `SleepDisabled` and runs `pmset sleepnow`. |
| Process wait | Optional `[process_wait]` config matches case-sensitive full-command substrings from `/bin/ps`; after matches exit, a grace timer runs before lid-closed sleep. |
| Battery display | The menu bar app reads `pmset -g batt` and renders charge digits, a vertical tick bar, and state marker as one compact icon without separate title spacing. |
| Config | `~/.config/no-sleep-till-done/config.toml`, created with defaults and opened via `/usr/bin/open -t`; reload generations in the lease tell the controller to apply validated changes without restarting. |
| Enable state | The persisted top-level `enabled` value and lease state allow the menu app to restore or activate the sleep override without quitting or using administrator approval. |
| Service health | The menu checks both launchd jobs and compares installed binaries/plists with bundled resources. Maintenance actions appear only for missing, unhealthy, or outdated services. |
| Start at login | The menu bar app writes/removes `~/Library/LaunchAgents/com.evren.nosleeptilldone.menubar.plist` using its current app executable path and enables/disables it with user-scoped `launchctl`. The toggle does not spawn or stop the current app process. |
| Shutdown | Runs `pmset -b disablesleep 0` when the app heartbeat disappears, expires, or `/tmp/com.evren.nosleeptilldone.reset` appears; menu Quit removes the heartbeat and writes the reset request. |
| Rename migration | On first renamed launch, the menu app copies the legacy config if needed, replaces the legacy user LaunchAgent, and refreshes both heartbeat paths so protection remains active. The conditional background-service action replaces legacy root services after administrator approval. |

## State Machine

```text
lid open
  -> lid closes
  -> sleep display, start timer
  -> lid opens before delay: cancel timer, wake display
  -> delay expires while closed and no process matches: enable normal sleep, sleep now, restore override after wake
  -> delay expires while closed and process matches exist: wait for matches
  -> matches exit: start process exit grace timer
  -> grace expires while closed: enable normal sleep, sleep now, restore override after wake
  -> lid opens during any closed-lid state: cancel timer/process wait/grace, wake display
```

## Scope

The daemon manages only the battery lid-close sleep override. It is meant for moving a MacBook briefly while background work continues, not for long-term closed-lid operation in confined spaces. Process-aware waiting applies only after lid-close delay expiry; lid-open idle sleep remains governed by macOS because the daemon does not set `pmset -b sleep 0`. When the app heartbeat is absent or stale, the daemon returns battery lid-close sleep to macOS with `pmset -b disablesleep 0` and remains resident for the next app launch.

The menu bar app delegates power changes to the privileged controller. Its heartbeat contains enabled state and a reload generation. The controller reloads the active user's config when that generation changes, keeps its previous settings if parsing fails, and restarts the closed-lid delay when a reload occurs with the lid closed. Installation, repair, and update actions are conditional and run on a background thread; only those maintenance actions require administrator approval. Quit removes the heartbeat and writes a reset marker that the controller consumes.

The watchdog is intentionally narrower than the controller. It does not inspect lid state or timers, and it does not enable `SleepDisabled`; it only repairs toward normal battery lid-close sleep when the controller remains absent.

The unhyphenated process name `nosleeptilldone` is reserved for task markers. Product executables remain hyphenated so a configured marker substring never matches the controller, watchdog, or menu app.
