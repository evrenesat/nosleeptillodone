# Devlog

| Date | Change |
| --- | --- |
| 2026-08-31 | Renamed the product to No Sleep Till Done, including binaries, bundle identifiers, launchd services, configuration paths, and documentation. Added compatibility migration for the previous LidSleep Delay config, login item, heartbeat, and privileged services. |
| 2026-08-31 | Added persistent enable/disable and live configuration reload controls that do not restart the controller or require administrator approval. |
| 2026-08-31 | Replaced user-visible helper terminology with background service, added controller/watchdog and installed-file health checks, and hid maintenance actions while healthy. |
| 2026-05-03 | Removed automatic privileged setup from normal app startup; background service install/repair is now an explicit menu action so startup does not ask for a password. |
| 2026-05-03 | Switched from stopping the privileged controller on menu Quit to a resident controller with an app heartbeat/reset file, so password prompts should be one-time install/update prompts instead of normal start/quit prompts. |
| 2026-05-03 | Moved privileged background service setup behind tray creation, bounded launchctl operations, and removed blocking kickstart calls so setup cannot prevent the menu bar icon from appearing. |
| 2026-05-01 | Made the app bundle install/kickstart its privileged controller/watchdog with administrator approval when missing, and made Quit fall back to an administrator-approved `pmset -b disablesleep 0` reset if controller cleanup is unavailable. |
| 2026-04-29 | Made menu Quit request a clean privileged controller shutdown, changed controller and menu launchd jobs to restart only unsuccessful exits, and stopped forcing the battery idle sleep timer to `0`. |
| 2026-04-26 | Added a root `no-sleep-till-done-watchdog` LaunchDaemon that restores `pmset -b disablesleep 0` if the controller remains absent beyond a grace period. |
| 2026-04-26 | Changed controller shutdown to restore normal battery lid-close sleep by default with `pmset -b disablesleep 0` instead of restoring the startup override state. |
| 2026-04-26 | Replaced the horizontal battery glyph with an icon-only compact charge number, vertical tick bar, and state dot. |
| 2026-04-26 | Collapsed the menu bar battery percentage into the tray icon itself so macOS no longer adds separate title spacing. |
| 2026-04-26 | Added in-place config migration so existing user configs receive newly added sections such as `[process_wait]` and `colors.process_wait`. |
| 2026-04-26 | Added `No Sleep Till Done.app` bundle generation and a menu bar Start at Login toggle backed by a user LaunchAgent. |
| 2026-04-26 | Clarified normal menu app versus privileged controller usage and made root controller config lookup prefer the invoking or active console user's config. |
| 2026-04-26 | Added optional process-aware lid sleep waiting with command-line substring matching, exit grace timing, and a blue menu bar process-wait state. |
| 2026-04-26 | Tightened menu bar icon width, aligned the status dot with the battery glyph, renamed the ready-state color key from `armed` to `ready`, and added CSS/HTML color-name parsing. |
| 2026-04-26 | Added user-session menu bar binary with battery/status display, colored lid-mode marker, shared TOML config creation/opening, Quit menu item, and LaunchAgent template. The controller now reads the same TOML defaults on startup. |
| 2026-04-25 | Created initial Rust daemon with IOKit lid-state polling, battery `pmset` sleep override, display sleep on lid close, delayed forced sleep, and LaunchDaemon template. |
