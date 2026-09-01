use no_sleep_till_done::config::{
    load_or_create_config, set_enabled, AppConfig, ProcessWaitConfig,
};
use no_sleep_till_done::system::{
    filter_processes, process_table, read_battery_state, read_lid_state, read_sleep_disabled,
    BatteryState, LidState, PowerSource, ProcessInfo, SystemError,
};
use no_sleep_till_done::{LeaseRecord, APP_ACTIVE_LEASE_PATH, CONTROLLER_RESET_REQUEST_PATH};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::window::WindowId;

#[cfg(target_os = "macos")]
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};

const OPEN_CONFIG_ID: &str = "open-config";
const ENABLED_ID: &str = "enabled";
const RELOAD_CONFIG_ID: &str = "reload-config";
const BACKGROUND_SERVICE_ID: &str = "background-service";
const START_AT_LOGIN_ID: &str = "start-at-login";
const QUIT_ID: &str = "quit";
const CONTROLLER_LABEL: &str = "com.evren.nosleeptilldone";
const LAUNCH_AGENT_LABEL: &str = "com.evren.nosleeptilldone.menubar";
const WATCHDOG_LABEL: &str = "com.evren.nosleeptilldone.watchdog";
const LEGACY_CONTROLLER_LABEL: &str = "com.evren.lidsleep-delay";
const LEGACY_LAUNCH_AGENT_LABEL: &str = "com.evren.lidsleep-delay-menubar";
const LEGACY_WATCHDOG_LABEL: &str = "com.evren.lidsleep-delay-watchdog";
const LEGACY_APP_ACTIVE_LEASE_PATH: &str = "/tmp/com.evren.lidsleep-delay.active";
const LEGACY_CONTROLLER_RESET_REQUEST_PATH: &str = "/tmp/com.evren.lidsleep-delay.reset";
const HEARTBEAT_SECONDS: u64 = 5;
const SERVICE_CHECK_SECONDS: u64 = 15;
const PROCESS_WAIT_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const PROCESS_LABEL_MAX_CHARS: usize = 100;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("no-sleep-till-done-menubar: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = acquire_single_instance_lock()?;
    if _lock.is_none() {
        return Ok(());
    }

    let (config, config_path) = load_or_create_config()?;
    migrate_legacy_start_at_login()?;
    let reload_generation = next_reload_generation();
    refresh_active_lease(config.enabled, reload_generation)?;

    #[cfg(target_os = "macos")]
    let event_loop = {
        let mut builder = winit::event_loop::EventLoop::builder();
        builder.with_activation_policy(ActivationPolicy::Accessory);
        builder.with_default_menu(false);
        builder.build()?
    };

    #[cfg(not(target_os = "macos"))]
    let event_loop = winit::event_loop::EventLoop::new()?;

    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = MenuBarApp::new(config, config_path, reload_generation);
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(unix)]
fn acquire_single_instance_lock() -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open("/tmp/com.evren.nosleeptilldone.menubar.lock")?;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(Some(file));
    }

    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        return Ok(None);
    }

    Err(error)
}

#[cfg(not(unix))]
fn acquire_single_instance_lock() -> io::Result<Option<File>> {
    Ok(None)
}

fn start_at_login_text() -> String {
    if is_start_at_login_enabled() {
        "Start at Login: On".into()
    } else {
        "Start at Login: Off".into()
    }
}

fn is_start_at_login_enabled() -> bool {
    launch_agent_path().is_file()
}

fn toggle_start_at_login() -> io::Result<()> {
    if is_start_at_login_enabled() {
        disable_start_at_login()
    } else {
        enable_start_at_login()
    }
}

fn enable_start_at_login() -> io::Result<()> {
    let path = launch_agent_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let executable = std::env::current_exe()?;
    fs::write(&path, launch_agent_plist(&executable))?;

    let target = launchctl_target();
    let service = format!("{target}/{LAUNCH_AGENT_LABEL}");
    let _ = Command::new("/bin/launchctl")
        .args(["enable", &service])
        .status();

    Ok(())
}

fn disable_start_at_login() -> io::Result<()> {
    let target = launchctl_target();
    let service = format!("{target}/{LAUNCH_AGENT_LABEL}");
    let _ = Command::new("/bin/launchctl")
        .args(["disable", &service])
        .status();

    let path = launch_agent_path();
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn launch_agent_path() -> PathBuf {
    launch_agent_path_for(LAUNCH_AGENT_LABEL)
}

fn launch_agent_path_for(label: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{label}.plist"))
}

fn migrate_legacy_start_at_login() -> io::Result<()> {
    let legacy_path = launch_agent_path_for(LEGACY_LAUNCH_AGENT_LABEL);
    if !legacy_path.is_file() || launch_agent_path().is_file() {
        return Ok(());
    }

    enable_start_at_login()?;
    let target = launchctl_target();
    let legacy_service = format!("{target}/{LEGACY_LAUNCH_AGENT_LABEL}");
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &legacy_service])
        .status();
    let _ = Command::new("/bin/launchctl")
        .args(["disable", &legacy_service])
        .status();
    match fs::remove_file(legacy_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn launchctl_target() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launch_agent_plist(executable: &std::path::Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCH_AGENT_LABEL}</string>

  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
  </array>

  <key>RunAtLoad</key>
  <true/>

  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>

  <key>StandardOutPath</key>
  <string>/tmp/no-sleep-till-done-menubar.log</string>

  <key>StandardErrorPath</key>
  <string>/tmp/no-sleep-till-done-menubar.log</string>
</dict>
</plist>
"#,
        xml_escape(&executable.display().to_string())
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

struct MenuBarApp {
    config: AppConfig,
    applied_config: AppConfig,
    enabled: bool,
    config_path: PathBuf,
    reload_generation: u64,
    config_notice: Option<String>,
    config_notice_until: Option<Instant>,
    tray: Option<TrayIcon>,
    status_item: Option<MenuItem>,
    enabled_item: Option<CheckMenuItem>,
    background_service_item: Option<MenuItem>,
    start_at_login_item: Option<MenuItem>,
    open_config_id: MenuId,
    enabled_id: MenuId,
    reload_config_id: MenuId,
    background_service_id: MenuId,
    start_at_login_id: MenuId,
    quit_id: MenuId,
    service_installing: bool,
    service_status: ServiceStatus,
    service_tx: Sender<ServiceStatus>,
    service_rx: Receiver<ServiceStatus>,
    last_lid: Option<LidState>,
    lid_closed_since: Option<Instant>,
    saw_process_wait_matches: bool,
    process_grace_started: Option<Instant>,
    process_display_state: ProcessDisplayState,
    last_process_display_state: ProcessDisplayState,
    process_submenu: Option<Submenu>,
    next_lease_refresh: Instant,
    next_service_check: Instant,
    next_refresh: Instant,
}

impl MenuBarApp {
    fn new(config: AppConfig, config_path: PathBuf, reload_generation: u64) -> Self {
        let (service_tx, service_rx) = mpsc::channel();
        Self {
            applied_config: config.clone(),
            enabled: config.enabled,
            config,
            config_path,
            reload_generation,
            config_notice: None,
            config_notice_until: None,
            tray: None,
            status_item: None,
            enabled_item: None,
            background_service_item: None,
            start_at_login_item: None,
            open_config_id: MenuId::new(OPEN_CONFIG_ID),
            enabled_id: MenuId::new(ENABLED_ID),
            reload_config_id: MenuId::new(RELOAD_CONFIG_ID),
            background_service_id: MenuId::new(BACKGROUND_SERVICE_ID),
            start_at_login_id: MenuId::new(START_AT_LOGIN_ID),
            quit_id: MenuId::new(QUIT_ID),
            service_installing: false,
            service_status: background_service_status(),
            service_tx,
            service_rx,
            last_lid: None,
            lid_closed_since: None,
            saw_process_wait_matches: false,
            process_grace_started: None,
            process_display_state: ProcessDisplayState::Hidden,
            last_process_display_state: ProcessDisplayState::Hidden,
            process_submenu: None,
            next_lease_refresh: Instant::now() + Duration::from_secs(HEARTBEAT_SECONDS),
            next_service_check: Instant::now() + Duration::from_secs(SERVICE_CHECK_SECONDS),
            next_refresh: Instant::now(),
        }
    }

    fn create_tray(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.tray.is_some() {
            return Ok(());
        }

        let snapshot = self.snapshot();
        let menu = self.build_menu()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(render_icon(&snapshot, &self.config)?)
            .with_icon_as_template(false)
            .with_title("")
            .with_tooltip(snapshot.tooltip_text(&self.applied_config, &self.config_path))
            .build()?;

        self.tray = Some(tray);
        self.apply_snapshot(snapshot)?;
        Ok(())
    }

    fn build_menu(&mut self) -> Result<Menu, Box<dyn std::error::Error>> {
        let status_item = MenuItem::new("Loading status", false, None);
        let process_submenu = match &self.process_display_state {
            ProcessDisplayState::Waiting(tree) => Some(build_process_submenu(tree)?),
            ProcessDisplayState::Hidden | ProcessDisplayState::Unavailable => None,
        };
        let process_unavailable = matches!(
            &self.process_display_state,
            ProcessDisplayState::Unavailable
        );
        let enabled_item = CheckMenuItem::with_id(
            self.enabled_id.clone(),
            "No Sleep Till Done Enabled",
            true,
            self.enabled,
            None,
        );
        let reload_config = MenuItem::with_id(
            self.reload_config_id.clone(),
            "Reload Configuration",
            true,
            None,
        );
        let open_config = MenuItem::with_id(
            self.open_config_id.clone(),
            "Open Configuration...",
            true,
            None,
        );
        let background_service_item =
            background_service_action_text(&self.service_status).map(|text| {
                MenuItem::with_id(
                    self.background_service_id.clone(),
                    text,
                    !matches!(self.service_status, ServiceStatus::Installing),
                    None,
                )
            });
        let start_at_login = MenuItem::with_id(
            self.start_at_login_id.clone(),
            start_at_login_text(),
            true,
            None,
        );
        let quit = MenuItem::with_id(self.quit_id.clone(), "Quit", true, None);
        let separator = PredefinedMenuItem::separator();
        let separator_two = PredefinedMenuItem::separator();

        let menu = Menu::new();
        menu.append(&status_item)?;
        if let Some(item) = &process_submenu {
            menu.append(item)?;
        } else if process_unavailable {
            let item = MenuItem::new("Waiting Processes: unavailable", false, None);
            menu.append(&item)?;
        }
        menu.append(&separator)?;
        menu.append(&enabled_item)?;
        menu.append(&reload_config)?;
        menu.append(&open_config)?;
        if let Some(item) = &background_service_item {
            menu.append(item)?;
        }
        menu.append(&start_at_login)?;
        menu.append(&separator_two)?;
        menu.append(&quit)?;

        self.status_item = Some(status_item);
        self.enabled_item = Some(enabled_item);
        self.background_service_item = background_service_item;
        self.start_at_login_item = Some(start_at_login);
        self.process_submenu = process_submenu;
        self.last_process_display_state = self.process_display_state.clone();
        Ok(menu)
    }

    fn rebuild_menu(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let menu = self.build_menu()?;
        if let Some(tray) = &self.tray {
            tray.set_menu(Some(Box::new(menu)));
        }
        Ok(())
    }

    fn sync_process_menu(&mut self, rebuild_menu: bool) -> Result<(), Box<dyn std::error::Error>> {
        if rebuild_menu
            || process_menu_variant(&self.last_process_display_state)
                != process_menu_variant(&self.process_display_state)
        {
            self.rebuild_menu()?;
            return Ok(());
        }

        let tree_changed = match (
            &self.last_process_display_state,
            &self.process_display_state,
        ) {
            (ProcessDisplayState::Waiting(previous), ProcessDisplayState::Waiting(current)) => {
                process_tree_signature(previous) != process_tree_signature(current)
            }
            _ => false,
        };

        if tree_changed {
            let tree = match &self.process_display_state {
                ProcessDisplayState::Waiting(tree) => tree.clone(),
                ProcessDisplayState::Hidden | ProcessDisplayState::Unavailable => unreachable!(),
            };
            self.update_process_submenu(&tree)?;
        }

        self.last_process_display_state = self.process_display_state.clone();
        Ok(())
    }

    fn update_process_submenu(
        &mut self,
        tree: &ProcessTree,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(submenu) = self.process_submenu.as_ref().cloned() else {
            return self.rebuild_menu();
        };

        submenu.set_text(waiting_processes_title(tree.match_count));
        while submenu.remove_at(0).is_some() {}
        append_process_tree_nodes(&submenu, &tree.roots)?;
        Ok(())
    }

    fn refresh(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self
            .config_notice_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.config_notice = None;
            self.config_notice_until = None;
        }

        let mut rebuild_menu = false;
        while let Ok(status) = self.service_rx.try_recv() {
            self.service_installing = false;
            if status != self.service_status {
                self.service_status = status;
                rebuild_menu = true;
            }
        }

        if Instant::now() >= self.next_lease_refresh {
            refresh_active_lease(self.enabled, self.reload_generation)?;
            self.next_lease_refresh = Instant::now() + Duration::from_secs(HEARTBEAT_SECONDS);
        }

        match load_or_create_config() {
            Ok((config, path)) => {
                self.config = config;
                self.config_path = path;
                if controller_config_changed(&self.config, &self.applied_config) {
                    self.config_notice = Some("configuration changed; reload required".into());
                    self.config_notice_until = None;
                }
            }
            Err(error) => {
                self.config_notice = Some(format!("configuration error: {error}"));
                self.config_notice_until = None;
            }
        }

        if !self.service_installing && Instant::now() >= self.next_service_check {
            let status = background_service_status();
            if status != self.service_status {
                self.service_status = status;
                rebuild_menu = true;
            }
            self.next_service_check = Instant::now() + Duration::from_secs(SERVICE_CHECK_SECONDS);
        }

        let snapshot = self.snapshot();
        self.sync_process_menu(rebuild_menu)?;
        self.apply_snapshot(snapshot)?;
        let next_refresh_started = Instant::now();
        let delay_remaining = self.process_wait_delay_remaining(next_refresh_started);
        self.next_refresh = next_refresh_started
            + process_wait_refresh_interval(
                self.process_wait_refresh_relevant(delay_remaining),
                delay_remaining,
                self.config.menu_refresh_seconds,
            );
        Ok(())
    }

    fn snapshot(&mut self) -> StatusSnapshot {
        let now = Instant::now();
        let lid = read_lid_state().ok();

        if lid != self.last_lid {
            match lid {
                Some(LidState::Closed) => self.lid_closed_since = Some(now),
                Some(LidState::Open) => {
                    self.lid_closed_since = None;
                    self.saw_process_wait_matches = false;
                    self.process_grace_started = None;
                    self.process_display_state = ProcessDisplayState::Hidden;
                }
                None => {}
            }
            self.last_lid = lid;
        }

        let battery = read_battery_state().ok();
        let sleep_disabled = read_sleep_disabled().ok();
        let remaining = match (lid, self.lid_closed_since) {
            (Some(LidState::Closed), Some(started)) => {
                let delay = Duration::from_secs(self.applied_config.delay_seconds);
                Some(delay.saturating_sub(started.elapsed()))
            }
            _ => None,
        };
        let process_wait = self.process_wait_status(lid, now);

        StatusSnapshot {
            battery,
            lid,
            sleep_disabled,
            remaining,
            process_wait,
            enabled: self.enabled,
            service_status: self.service_status.clone(),
            config_notice: self.config_notice.clone(),
        }
    }

    fn apply_snapshot(&self, snapshot: StatusSnapshot) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(tray) = &self.tray {
            tray.set_icon(Some(render_icon(&snapshot, &self.config)?))?;
            tray.set_title(Some(""));
            tray.set_tooltip(Some(
                snapshot.tooltip_text(&self.applied_config, &self.config_path),
            ))?;
        }

        if let Some(item) = &self.status_item {
            item.set_text(snapshot.menu_status_text(&self.applied_config));
        }

        if let Some(item) = &self.enabled_item {
            item.set_checked(self.enabled);
        }

        if let Some(item) = &self.start_at_login_item {
            item.set_text(start_at_login_text());
        }

        Ok(())
    }

    fn process_wait_status(
        &mut self,
        lid: Option<LidState>,
        now: Instant,
    ) -> Option<ProcessWaitStatus> {
        self.process_wait_status_with_scan(lid, now, process_table)
    }

    fn process_wait_status_with_scan(
        &mut self,
        lid: Option<LidState>,
        now: Instant,
        scan: impl FnOnce() -> Result<Vec<ProcessInfo>, SystemError>,
    ) -> Option<ProcessWaitStatus> {
        if !self.enabled
            || lid != Some(LidState::Closed)
            || !self.applied_config.process_wait.enabled
        {
            self.saw_process_wait_matches = false;
            self.process_grace_started = None;
            self.process_display_state = ProcessDisplayState::Hidden;
            return None;
        }

        let closed_since = self.lid_closed_since?;
        let delay = Duration::from_secs(self.applied_config.delay_seconds);
        if closed_since.elapsed() < delay
            || !self
                .applied_config
                .process_wait
                .command_substrings
                .iter()
                .any(|value| !value.is_empty())
        {
            self.process_display_state = ProcessDisplayState::Hidden;
            return None;
        }

        self.process_wait_status_from_scan(scan(), now)
    }

    fn process_wait_status_from_scan(
        &mut self,
        scan: Result<Vec<ProcessInfo>, SystemError>,
        now: Instant,
    ) -> Option<ProcessWaitStatus> {
        let table = match scan {
            Ok(table) => table,
            Err(_) => {
                self.process_display_state = ProcessDisplayState::Unavailable;
                return Some(ProcessWaitStatus::Unavailable);
            }
        };
        let matches = filter_processes(
            &table,
            &self.applied_config.process_wait.command_substrings,
            std::process::id(),
        );

        if matches.is_empty() {
            self.process_display_state = ProcessDisplayState::Hidden;
            if !self.saw_process_wait_matches {
                return None;
            }

            let started = *self.process_grace_started.get_or_insert(now);
            let grace = Duration::from_secs(self.applied_config.process_wait.exit_grace_seconds);
            return Some(ProcessWaitStatus::Grace {
                remaining: grace.saturating_sub(started.elapsed()),
            });
        }

        let tree = build_process_tree(&table, &matches);
        self.saw_process_wait_matches = true;
        self.process_grace_started = None;
        self.process_display_state = ProcessDisplayState::Waiting(tree.clone());
        Some(ProcessWaitStatus::Waiting { tree })
    }

    fn process_wait_delay_remaining(&self, now: Instant) -> Option<Duration> {
        if !self.enabled
            || self.last_lid != Some(LidState::Closed)
            || !self.applied_config.process_wait.enabled
            || !self
                .applied_config
                .process_wait
                .command_substrings
                .iter()
                .any(|value| !value.is_empty())
        {
            return None;
        }

        let closed_since = self.lid_closed_since?;
        let elapsed = now.saturating_duration_since(closed_since);
        Some(Duration::from_secs(self.applied_config.delay_seconds).saturating_sub(elapsed))
    }

    fn process_wait_refresh_relevant(&self, delay_remaining: Option<Duration>) -> bool {
        process_wait_refresh_relevant(
            self.enabled,
            self.last_lid,
            delay_remaining.is_some_and(|remaining| remaining.is_zero()),
            &self.applied_config.process_wait,
        )
    }

    fn handle_menu_event(&mut self, event_loop: &ActiveEventLoop, event: MenuEvent) {
        if event.id == self.open_config_id {
            if let Err(error) = Command::new("/usr/bin/open")
                .arg("-t")
                .arg(&self.config_path)
                .spawn()
            {
                eprintln!("no-sleep-till-done-menubar: failed to open config: {error}");
            }
        } else if event.id == self.enabled_id {
            if let Err(error) = self.set_app_enabled(!self.enabled) {
                self.config_notice = Some(format!("failed to change enabled state: {error}"));
                self.config_notice_until = None;
            }
            if let Some(item) = &self.enabled_item {
                item.set_checked(self.enabled);
            }
        } else if event.id == self.reload_config_id {
            if let Err(error) = self.reload_configuration() {
                self.config_notice = Some(format!("configuration error: {error}"));
                self.config_notice_until = None;
            }
        } else if event.id == self.background_service_id {
            self.start_background_service_setup();
            if let Err(error) = self.rebuild_menu() {
                eprintln!("no-sleep-till-done-menubar: failed to update menu: {error}");
            }
        } else if event.id == self.start_at_login_id {
            if let Err(error) = toggle_start_at_login() {
                eprintln!("no-sleep-till-done-menubar: failed to toggle start at login: {error}");
            }
            if let Some(item) = &self.start_at_login_item {
                item.set_text(start_at_login_text());
            }
        } else if event.id == self.quit_id {
            if let Err(error) = request_controller_reset() {
                eprintln!(
                    "no-sleep-till-done-menubar: failed to request controller reset: {error}"
                );
            }
            if let Err(error) = ensure_sleep_restored_after_quit() {
                eprintln!("no-sleep-till-done-menubar: failed to restore sleep on quit: {error}");
            }
            event_loop.exit();
        }
    }

    fn set_app_enabled(&mut self, enabled: bool) -> Result<(), Box<dyn std::error::Error>> {
        set_enabled(&self.config_path, enabled)?;
        self.reload_configuration()?;
        self.set_temporary_notice(if enabled {
            "No Sleep Till Done enabled"
        } else {
            "No Sleep Till Done disabled"
        });
        Ok(())
    }

    fn reload_configuration(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let (config, path) = load_or_create_config()?;
        self.enabled = config.enabled;
        self.config = config.clone();
        self.applied_config = config;
        self.config_path = path;
        self.reload_generation = self.reload_generation.wrapping_add(1);
        refresh_active_lease(self.enabled, self.reload_generation)?;
        self.next_lease_refresh = Instant::now() + Duration::from_secs(HEARTBEAT_SECONDS);
        self.set_temporary_notice("configuration reload requested");
        Ok(())
    }

    fn set_temporary_notice(&mut self, message: &str) {
        self.config_notice = Some(message.into());
        self.config_notice_until = Some(Instant::now() + Duration::from_secs(10));
    }

    fn start_background_service_setup(&mut self) {
        if self.service_installing {
            return;
        }

        self.service_installing = true;
        self.service_status = ServiceStatus::Installing;
        let tx = self.service_tx.clone();
        thread::spawn(move || {
            let status = match ensure_privileged_services() {
                Ok(()) => background_service_status(),
                Err(error) => ServiceStatus::Error(error.to_string()),
            };
            let _ = tx.send(status);
        });
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProcessDisplayState {
    Hidden,
    Waiting(ProcessTree),
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProcessTree {
    match_count: usize,
    roots: Vec<ProcessTreeNode>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProcessTreeNode {
    process: ProcessInfo,
    is_match: bool,
    children: Vec<ProcessTreeNode>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessMenuVariant {
    Hidden,
    Waiting,
    Unavailable,
}

fn process_menu_variant(state: &ProcessDisplayState) -> ProcessMenuVariant {
    match state {
        ProcessDisplayState::Hidden => ProcessMenuVariant::Hidden,
        ProcessDisplayState::Waiting(_) => ProcessMenuVariant::Waiting,
        ProcessDisplayState::Unavailable => ProcessMenuVariant::Unavailable,
    }
}

fn process_wait_refresh_relevant(
    enabled: bool,
    lid: Option<LidState>,
    delay_elapsed: bool,
    config: &ProcessWaitConfig,
) -> bool {
    enabled
        && lid == Some(LidState::Closed)
        && delay_elapsed
        && config.enabled
        && config
            .command_substrings
            .iter()
            .any(|value| !value.is_empty())
}

fn process_wait_refresh_interval(
    process_wait_relevant: bool,
    delay_remaining: Option<Duration>,
    menu_refresh_seconds: u64,
) -> Duration {
    let configured_interval = Duration::from_secs(menu_refresh_seconds.max(1));
    if process_wait_relevant {
        PROCESS_WAIT_REFRESH_INTERVAL
    } else if let Some(remaining) = delay_remaining {
        remaining.min(configured_interval)
    } else {
        configured_interval
    }
}

fn build_process_tree(table: &[ProcessInfo], matches: &[ProcessInfo]) -> ProcessTree {
    let processes: HashMap<u32, ProcessInfo> = table
        .iter()
        .cloned()
        .map(|process| (process.pid, process))
        .collect();
    let matching_pids: BTreeSet<u32> = matches.iter().map(|process| process.pid).collect();
    let mut relevant_pids = BTreeSet::new();

    for matching in matches {
        if !processes.contains_key(&matching.pid) {
            continue;
        }

        let mut current_pid = matching.pid;
        let mut path_pids = HashSet::new();
        loop {
            if !path_pids.insert(current_pid) || !relevant_pids.insert(current_pid) {
                break;
            }

            let Some(process) = processes.get(&current_pid) else {
                break;
            };
            if process.ppid <= 1 || !processes.contains_key(&process.ppid) {
                break;
            }
            current_pid = process.ppid;
        }
    }

    let mut parent_by_pid = HashMap::new();
    for pid in relevant_pids.iter().copied() {
        let parent = processes.get(&pid).and_then(|process| {
            (process.ppid > 1 && relevant_pids.contains(&process.ppid)).then_some(process.ppid)
        });
        parent_by_pid.insert(pid, parent);
    }
    break_parent_cycles(&mut parent_by_pid);

    let mut children_by_parent: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut roots = Vec::new();
    for pid in relevant_pids.iter().copied() {
        match parent_by_pid.get(&pid).copied().flatten() {
            Some(parent) => children_by_parent.entry(parent).or_default().push(pid),
            None => roots.push(pid),
        }
    }
    for children in children_by_parent.values_mut() {
        children.sort_unstable();
    }

    let roots = roots
        .into_iter()
        .map(|pid| {
            build_process_tree_node(
                pid,
                &processes,
                &matching_pids,
                &children_by_parent,
                &mut HashSet::new(),
            )
        })
        .collect();

    ProcessTree {
        match_count: matches.len(),
        roots,
    }
}

fn break_parent_cycles(parent_by_pid: &mut HashMap<u32, Option<u32>>) {
    let starts: Vec<u32> = {
        let mut starts: Vec<_> = parent_by_pid.keys().copied().collect();
        starts.sort_unstable();
        starts
    };

    for start in starts {
        let mut positions = HashMap::new();
        let mut path = Vec::new();
        let mut current = start;

        loop {
            if let Some(&cycle_start) = positions.get(&current) {
                if let Some(break_pid) = path[cycle_start..].iter().copied().max() {
                    parent_by_pid.insert(break_pid, None);
                }
                break;
            }

            positions.insert(current, path.len());
            path.push(current);
            let Some(parent) = parent_by_pid.get(&current).copied().flatten() else {
                break;
            };
            current = parent;
        }
    }
}

fn build_process_tree_node(
    pid: u32,
    processes: &HashMap<u32, ProcessInfo>,
    matching_pids: &BTreeSet<u32>,
    children_by_parent: &BTreeMap<u32, Vec<u32>>,
    path: &mut HashSet<u32>,
) -> ProcessTreeNode {
    let process = processes
        .get(&pid)
        .expect("process tree node must exist in the process table")
        .clone();
    if !path.insert(pid) {
        return ProcessTreeNode {
            process,
            is_match: matching_pids.contains(&pid),
            children: Vec::new(),
        };
    }

    let child_ids = children_by_parent.get(&pid).cloned().unwrap_or_default();
    let mut children = Vec::new();
    for child in child_ids {
        if !path.contains(&child) {
            children.push(build_process_tree_node(
                child,
                processes,
                matching_pids,
                children_by_parent,
                path,
            ));
        }
    }
    path.remove(&pid);

    ProcessTreeNode {
        process,
        is_match: matching_pids.contains(&pid),
        children,
    }
}

type ProcessTreeSignature = Vec<(u32, u32, String, bool)>;

fn process_tree_signature(tree: &ProcessTree) -> ProcessTreeSignature {
    fn append_node_signature(node: &ProcessTreeNode, signature: &mut ProcessTreeSignature) {
        signature.push((
            node.process.pid,
            node.process.ppid,
            node.process.command.clone(),
            node.is_match,
        ));
        for child in &node.children {
            append_node_signature(child, signature);
        }
    }

    let mut signature = Vec::new();
    for root in &tree.roots {
        append_node_signature(root, &mut signature);
    }
    signature
}

fn waiting_processes_title(count: usize) -> String {
    format!("Waiting Processes ({count})")
}

fn waiting_processes_status(count: usize) -> String {
    if count == 1 {
        "waiting for 1 process".into()
    } else {
        format!("waiting for {count} processes")
    }
}

fn truncate_label(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.into();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let mut result: String = value.chars().take(max_chars - 3).collect();
    result.push_str("...");
    result
}

fn process_menu_label(node: &ProcessTreeNode) -> String {
    let prefix = if node.is_match { "* " } else { "" };
    truncate_label(
        &format!("{prefix}[{}] {}", node.process.pid, node.process.command),
        PROCESS_LABEL_MAX_CHARS,
    )
}

fn build_process_submenu(tree: &ProcessTree) -> Result<Submenu, Box<dyn std::error::Error>> {
    let submenu = Submenu::new(waiting_processes_title(tree.match_count), true);
    append_process_tree_nodes(&submenu, &tree.roots)?;
    Ok(submenu)
}

fn append_process_tree_nodes(
    parent: &Submenu,
    nodes: &[ProcessTreeNode],
) -> Result<(), Box<dyn std::error::Error>> {
    for node in nodes {
        let label = process_menu_label(node);
        if node.children.is_empty() {
            let item = MenuItem::new(label, false, None);
            parent.append(&item)?;
        } else {
            let submenu = Submenu::new(label, true);
            append_process_tree_nodes(&submenu, &node.children)?;
            parent.append(&submenu)?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ServiceStatus {
    Missing,
    RepairRequired(String),
    UpdateRequired,
    Installing,
    Ready,
    Error(String),
}

fn background_service_action_text(status: &ServiceStatus) -> Option<&'static str> {
    match status {
        ServiceStatus::Missing => Some("Install Background Service..."),
        ServiceStatus::RepairRequired(_) | ServiceStatus::Error(_) => {
            Some("Repair Background Service...")
        }
        ServiceStatus::UpdateRequired => Some("Update Background Service..."),
        ServiceStatus::Installing => Some("Installing Background Service..."),
        ServiceStatus::Ready => None,
    }
}

fn background_service_status() -> ServiceStatus {
    let installed = [
        PathBuf::from("/usr/local/sbin/no-sleep-till-done"),
        PathBuf::from("/usr/local/sbin/no-sleep-till-done-watchdog"),
        PathBuf::from("/Library/LaunchDaemons/com.evren.nosleeptilldone.plist"),
        PathBuf::from("/Library/LaunchDaemons/com.evren.nosleeptilldone.watchdog.plist"),
    ];
    let installed_count = installed.iter().filter(|path| path.is_file()).count();
    if installed_count == 0 {
        let legacy_installed = [
            "/usr/local/sbin/lidsleep-delay",
            "/usr/local/sbin/lidsleep-delay-watchdog",
            "/Library/LaunchDaemons/com.evren.lidsleep-delay.plist",
            "/Library/LaunchDaemons/com.evren.lidsleep-delay-watchdog.plist",
        ]
        .iter()
        .any(|path| Path::new(path).is_file());
        if legacy_installed
            || service_is_running(LEGACY_CONTROLLER_LABEL)
            || service_is_running(LEGACY_WATCHDOG_LABEL)
        {
            return ServiceStatus::UpdateRequired;
        }
        return ServiceStatus::Missing;
    }
    if installed_count != installed.len() {
        return ServiceStatus::RepairRequired("installed files are incomplete".into());
    }

    let resources = match bundled_resources_dir() {
        Ok(resources) => resources,
        Err(error) => return ServiceStatus::Error(error.to_string()),
    };
    let bundled = [
        resources.join("no-sleep-till-done"),
        resources.join("no-sleep-till-done-watchdog"),
        resources
            .join("launchd")
            .join("com.evren.nosleeptilldone.plist"),
        resources
            .join("launchd")
            .join("com.evren.nosleeptilldone.watchdog.plist"),
    ];

    for (bundled, installed) in bundled.iter().zip(installed.iter()) {
        match files_equal(bundled, installed) {
            Ok(true) => {}
            Ok(false) => return ServiceStatus::UpdateRequired,
            Err(error) => return ServiceStatus::Error(error.to_string()),
        }
    }

    if !service_is_running(CONTROLLER_LABEL) {
        return ServiceStatus::RepairRequired("controller is not running".into());
    }
    if !service_is_running(WATCHDOG_LABEL) {
        return ServiceStatus::RepairRequired("safety watchdog is not running".into());
    }

    ServiceStatus::Ready
}

fn files_equal(left: &Path, right: &Path) -> io::Result<bool> {
    let left_metadata = fs::metadata(left)?;
    let right_metadata = fs::metadata(right)?;
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }
    Ok(fs::read(left)? == fs::read(right)?)
}

fn controller_config_changed(current: &AppConfig, applied: &AppConfig) -> bool {
    current.enabled != applied.enabled
        || current.delay_seconds != applied.delay_seconds
        || current.poll_seconds != applied.poll_seconds
        || current.process_wait != applied.process_wait
}

fn next_reload_generation() -> u64 {
    fs::read_to_string(APP_ACTIVE_LEASE_PATH)
        .ok()
        .and_then(|text| LeaseRecord::parse(&text))
        .map_or(1, |record| record.reload_generation.wrapping_add(1))
}

fn refresh_active_lease(enabled: bool, reload_generation: u64) -> io::Result<()> {
    let lease = LeaseRecord {
        enabled,
        reload_generation,
    }
    .serialize();
    fs::write(APP_ACTIVE_LEASE_PATH, &lease)?;

    // Keep an installed pre-rename controller active until the user approves
    // the one-time background-service update.
    let _ = fs::write(LEGACY_APP_ACTIVE_LEASE_PATH, lease);
    Ok(())
}

fn request_controller_reset() -> io::Result<()> {
    match fs::remove_file(APP_ACTIVE_LEASE_PATH) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    match fs::remove_file(LEGACY_APP_ACTIVE_LEASE_PATH) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => {}
    }

    fs::write(CONTROLLER_RESET_REQUEST_PATH, b"reset\n")?;
    let _ = fs::write(LEGACY_CONTROLLER_RESET_REQUEST_PATH, b"reset\n");
    Ok(())
}

fn ensure_sleep_restored_after_quit() -> io::Result<()> {
    for _ in 0..10 {
        thread::sleep(Duration::from_millis(500));
        if matches!(read_sleep_disabled(), Ok(false)) {
            return Ok(());
        }
    }

    run_privileged_shell("pmset -b disablesleep 0")
}

fn ensure_privileged_services() -> io::Result<()> {
    let resources = bundled_resources_dir()?;
    let controller = resources.join("no-sleep-till-done");
    let watchdog = resources.join("no-sleep-till-done-watchdog");
    let controller_plist = resources
        .join("launchd")
        .join("com.evren.nosleeptilldone.plist");
    let watchdog_plist = resources
        .join("launchd")
        .join("com.evren.nosleeptilldone.watchdog.plist");

    for path in [&controller, &watchdog, &controller_plist, &watchdog_plist] {
        if !path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "bundled background service resource missing: {}",
                    path.display()
                ),
            ));
        }
    }

    let script = format!(
        "\
install -m 755 {controller} /usr/local/sbin/no-sleep-till-done
install -m 755 {watchdog} /usr/local/sbin/no-sleep-till-done-watchdog
install -m 644 {controller_plist} /Library/LaunchDaemons/com.evren.nosleeptilldone.plist
install -m 644 {watchdog_plist} /Library/LaunchDaemons/com.evren.nosleeptilldone.watchdog.plist
launchctl bootout system /Library/LaunchDaemons/com.evren.nosleeptilldone.plist >/dev/null 2>&1 || true
launchctl bootout system /Library/LaunchDaemons/com.evren.nosleeptilldone.watchdog.plist >/dev/null 2>&1 || true
bounded_launchctl 10 bootstrap system /Library/LaunchDaemons/com.evren.nosleeptilldone.plist
bounded_launchctl 10 bootstrap system /Library/LaunchDaemons/com.evren.nosleeptilldone.watchdog.plist
launchctl enable system/{controller_label}
launchctl enable system/{watchdog_label}
launchctl bootout system /Library/LaunchDaemons/com.evren.lidsleep-delay.plist >/dev/null 2>&1 || true
launchctl bootout system /Library/LaunchDaemons/com.evren.lidsleep-delay-watchdog.plist >/dev/null 2>&1 || true
rm -f /Library/LaunchDaemons/com.evren.lidsleep-delay.plist
rm -f /Library/LaunchDaemons/com.evren.lidsleep-delay-watchdog.plist
rm -f /usr/local/sbin/lidsleep-delay
rm -f /usr/local/sbin/lidsleep-delay-watchdog
sleep 1
",
        controller = shell_quote_path(&controller),
        watchdog = shell_quote_path(&watchdog),
        controller_plist = shell_quote_path(&controller_plist),
        watchdog_plist = shell_quote_path(&watchdog_plist),
        controller_label = CONTROLLER_LABEL,
        watchdog_label = WATCHDOG_LABEL,
    );

    run_privileged_shell(&script)
}

fn service_is_running(label: &str) -> bool {
    let service = format!("system/{label}");
    let output = Command::new("/bin/launchctl")
        .args(["print", &service])
        .output();

    let Ok(output) = output else {
        return false;
    };

    if !output.status.success() {
        return false;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim() == "state = running")
}

fn bundled_resources_dir() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let macos_dir = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "current executable has no parent directory",
        )
    })?;
    let contents_dir = macos_dir.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "app Contents directory not found")
    })?;
    Ok(contents_dir.join("Resources"))
}

fn run_privileged_shell(script: &str) -> io::Result<()> {
    let script = format!(
        "\
bounded_launchctl() {{
  seconds=\"$1\"
  shift
  launchctl \"$@\" &
  pid=\"$!\"
  elapsed=0
  while kill -0 \"$pid\" >/dev/null 2>&1; do
    if [ \"$elapsed\" -ge \"$seconds\" ]; then
      kill -TERM \"$pid\" >/dev/null 2>&1 || true
      sleep 1
      kill -KILL \"$pid\" >/dev/null 2>&1 || true
      wait \"$pid\" >/dev/null 2>&1 || true
      echo \"launchctl $* timed out\" >&2
      return 124
    fi
    sleep 1
    elapsed=$((elapsed + 1))
  done
  wait \"$pid\"
}}
{script}"
    );

    let status = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(format!(
            "do shell script {} with administrator privileges",
            applescript_string(&script)
        ))
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("administrator command failed with {status}"),
        ))
    }
}

fn shell_quote_path(path: &Path) -> String {
    shell_quote(&path.display().to_string())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

impl ApplicationHandler for MenuBarApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.create_tray() {
            eprintln!("no-sleep-till-done-menubar: failed to create menu bar item: {error}");
            event_loop.exit();
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, _event: WindowEvent) {}

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            self.handle_menu_event(event_loop, event);
        }

        if Instant::now() >= self.next_refresh {
            if let Err(error) = self.refresh() {
                eprintln!("no-sleep-till-done-menubar: refresh failed: {error}");
            }
        }

        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_refresh));
    }
}

#[derive(Clone, Debug)]
struct StatusSnapshot {
    battery: Option<BatteryState>,
    lid: Option<LidState>,
    sleep_disabled: Option<bool>,
    remaining: Option<Duration>,
    process_wait: Option<ProcessWaitStatus>,
    enabled: bool,
    service_status: ServiceStatus,
    config_notice: Option<String>,
}

impl StatusSnapshot {
    fn battery_percent_text(&self) -> String {
        self.battery
            .as_ref()
            .and_then(|battery| battery.percent)
            .map(|percent| percent.to_string())
            .unwrap_or_else(|| "--".into())
    }

    fn tooltip_text(&self, config: &AppConfig, config_path: &Path) -> String {
        format!(
            "{}\nConfig: {}",
            self.menu_status_text(config),
            config_path.display()
        )
    }

    fn menu_status_text(&self, config: &AppConfig) -> String {
        let battery = match &self.battery {
            Some(value) => {
                let percent = value
                    .percent
                    .map(|percent| percent.to_string())
                    .unwrap_or_else(|| "--".into());
                let source = match value.source {
                    PowerSource::Battery => "battery",
                    PowerSource::AC => "AC",
                    PowerSource::Unknown => "power unknown",
                };
                let charging = if value.charging { ", charging" } else { "" };
                format!("{percent} on {source}{charging}")
            }
            None => "battery unknown".into(),
        };

        let lid = if !self.enabled {
            "No Sleep Till Done disabled".into()
        } else {
            match self.lid {
                Some(LidState::Open) => "lid open".into(),
                Some(LidState::Closed) => match &self.process_wait {
                    Some(ProcessWaitStatus::Waiting { tree }) => {
                        format!("lid closed, {}", waiting_processes_status(tree.match_count))
                    }
                    Some(ProcessWaitStatus::Unavailable) => {
                        "lid closed, process status unavailable".into()
                    }
                    Some(ProcessWaitStatus::Grace { remaining }) => {
                        format!("lid closed, process grace {}s", remaining.as_secs())
                    }
                    None => match self.remaining {
                        Some(remaining) => format!("lid closed, sleep in {}s", remaining.as_secs()),
                        None => format!("lid closed, sleep in {}s", config.delay_seconds),
                    },
                },
                None => "lid unknown".into(),
            }
        };

        let sleep = match (self.enabled, self.sleep_disabled) {
            (false, Some(false)) => "normal sleep restored",
            (false, Some(true)) => "disabling sleep override",
            (false, None) => "sleep override unknown",
            (true, Some(true)) => "sleep override on",
            (true, Some(false)) => "sleep override off",
            (true, None) => "sleep override unknown",
        };

        let mut status = format!("{battery}; {lid}; {sleep}; {}", self.service_status_text());
        if let Some(notice) = &self.config_notice {
            status.push_str("; ");
            status.push_str(notice);
        }
        status
    }

    fn service_status_text(&self) -> String {
        match &self.service_status {
            ServiceStatus::Missing => "background service not installed".into(),
            ServiceStatus::RepairRequired(reason) => {
                format!("background service needs repair: {reason}")
            }
            ServiceStatus::UpdateRequired => "background service update available".into(),
            ServiceStatus::Installing => "installing background service".into(),
            ServiceStatus::Ready => "background service ready".into(),
            ServiceStatus::Error(error) => {
                format!("background service status failed: {error}")
            }
        }
    }

    fn marker_mode(&self) -> MarkerMode {
        if !self.enabled {
            return MarkerMode::Unknown;
        }
        match (self.lid, self.sleep_disabled) {
            (_, Some(false)) => MarkerMode::Error,
            (Some(LidState::Closed), _) if self.process_wait.is_some() => MarkerMode::ProcessWait,
            (Some(LidState::Closed), _) => MarkerMode::Timer,
            (Some(LidState::Open), Some(true)) => MarkerMode::Ready,
            _ => MarkerMode::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MarkerMode {
    Ready,
    Timer,
    ProcessWait,
    Error,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProcessWaitStatus {
    Waiting { tree: ProcessTree },
    Unavailable,
    Grace { remaining: Duration },
}

fn render_icon(
    snapshot: &StatusSnapshot,
    config: &AppConfig,
) -> Result<Icon, Box<dyn std::error::Error>> {
    let height = 14;
    let percent_text = snapshot.battery_percent_text();
    let text_width = compact_text_width(&percent_text);
    let bar_x = text_width + 2;
    let dot_x = bar_x + 7;
    let width = dot_x + 3;
    let mut rgba = vec![0; width * height * 4];

    let percent = snapshot
        .battery
        .as_ref()
        .and_then(|battery| battery.percent)
        .unwrap_or(0);

    let outline = [42, 42, 46, 255];
    let fill = if percent <= 10 {
        [239, 68, 68, 255]
    } else if percent <= 25 {
        [245, 158, 11, 255]
    } else {
        [80, 80, 86, 255]
    };

    draw_compact_text(&mut rgba, width, 0, 2, &percent_text, outline);

    draw_rect(&mut rgba, width, bar_x, 2, 4, 10, outline);
    draw_rect(&mut rgba, width, bar_x + 1, 3, 2, 8, [0, 0, 0, 0]);

    let fill_height = ((percent.min(100) as usize * 8) / 100).max(1);
    draw_rect(
        &mut rgba,
        width,
        bar_x + 1,
        11 - fill_height,
        2,
        fill_height,
        fill,
    );

    let dot = match snapshot.marker_mode() {
        MarkerMode::Ready => parse_color(&config.colors.ready).unwrap_or([0, 128, 0, 255]),
        MarkerMode::Timer => parse_color(&config.colors.timer).unwrap_or([255, 165, 0, 255]),
        MarkerMode::ProcessWait => {
            parse_color(&config.colors.process_wait).unwrap_or([0, 0, 255, 255])
        }
        MarkerMode::Error => parse_color(&config.colors.error).unwrap_or([255, 0, 0, 255]),
        MarkerMode::Unknown => parse_color(&config.colors.unknown).unwrap_or([128, 128, 128, 255]),
    };
    draw_circle(&mut rgba, width, dot_x as isize, 7, 2, dot);

    Ok(Icon::from_rgba(rgba, width as u32, height as u32)?)
}

fn compact_text_width(text: &str) -> usize {
    let glyph_count = text.chars().count();
    if glyph_count == 0 {
        0
    } else {
        glyph_count * 4 + glyph_count.saturating_sub(1)
    }
}

fn draw_compact_text(
    rgba: &mut [u8],
    width: usize,
    x: usize,
    y: usize,
    text: &str,
    color: [u8; 4],
) {
    let mut cursor = x;
    for ch in text.chars() {
        draw_compact_glyph(rgba, width, cursor, y, ch, color);
        cursor += 5;
    }
}

fn draw_compact_glyph(rgba: &mut [u8], width: usize, x: usize, y: usize, ch: char, color: [u8; 4]) {
    let glyph = match ch {
        '0' => ["0110", "1001", "1001", "1001", "1001", "1001", "0110"],
        '1' => ["0010", "0110", "0010", "0010", "0010", "0010", "0111"],
        '2' => ["1110", "0001", "0001", "0110", "1000", "1000", "1111"],
        '3' => ["1110", "0001", "0001", "0110", "0001", "0001", "1110"],
        '4' => ["1001", "1001", "1001", "1111", "0001", "0001", "0001"],
        '5' => ["1111", "1000", "1000", "1110", "0001", "0001", "1110"],
        '6' => ["0111", "1000", "1000", "1110", "1001", "1001", "0110"],
        '7' => ["1111", "0001", "0010", "0010", "0100", "0100", "0100"],
        '8' => ["0110", "1001", "1001", "0110", "1001", "1001", "0110"],
        '9' => ["0110", "1001", "1001", "0111", "0001", "0001", "1110"],
        '-' => ["0000", "0000", "0000", "1110", "0000", "0000", "0000"],
        _ => ["0000", "0000", "0000", "0000", "0000", "0000", "0000"],
    };

    for (row, pattern) in glyph.iter().enumerate() {
        for (col, pixel) in pattern.bytes().enumerate() {
            if pixel == b'1' {
                set_pixel(rgba, width, x + col, y + row, color);
            }
        }
    }
}

fn parse_color(value: &str) -> Option<[u8; 4]> {
    let value = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if value.len() == 6 {
        let red = u8::from_str_radix(&value[0..2], 16).ok()?;
        let green = u8::from_str_radix(&value[2..4], 16).ok()?;
        let blue = u8::from_str_radix(&value[4..6], 16).ok()?;
        return Some([red, green, blue, 255]);
    }

    css_named_color(value)
}

fn css_named_color(value: &str) -> Option<[u8; 4]> {
    let rgb = match value.trim().to_ascii_lowercase().as_str() {
        "aliceblue" => [240, 248, 255],
        "antiquewhite" => [250, 235, 215],
        "aquamarine" => [127, 255, 212],
        "black" => [0, 0, 0],
        "blanchedalmond" => [255, 235, 205],
        "blue" => [0, 0, 255],
        "blueviolet" => [138, 43, 226],
        "brown" => [165, 42, 42],
        "burlywood" => [222, 184, 135],
        "cadetblue" => [95, 158, 160],
        "chartreuse" => [127, 255, 0],
        "chocolate" => [210, 105, 30],
        "coral" => [255, 127, 80],
        "cornflowerblue" => [100, 149, 237],
        "cornsilk" => [255, 248, 220],
        "crimson" => [220, 20, 60],
        "cyan" | "aqua" => [0, 255, 255],
        "darkblue" => [0, 0, 139],
        "darkcyan" => [0, 139, 139],
        "darkgoldenrod" => [184, 134, 11],
        "darkgray" | "darkgrey" => [169, 169, 169],
        "darkgreen" => [0, 100, 0],
        "darkkhaki" => [189, 183, 107],
        "darkmagenta" => [139, 0, 139],
        "darkolivegreen" => [85, 107, 47],
        "darkorange" => [255, 140, 0],
        "darkorchid" => [153, 50, 204],
        "darkred" => [139, 0, 0],
        "darksalmon" => [233, 150, 122],
        "darkseagreen" => [143, 188, 143],
        "darkslateblue" => [72, 61, 139],
        "darkslategray" | "darkslategrey" => [47, 79, 79],
        "darkturquoise" => [0, 206, 209],
        "darkviolet" => [148, 0, 211],
        "deeppink" => [255, 20, 147],
        "deepskyblue" => [0, 191, 255],
        "dimgray" | "dimgrey" => [105, 105, 105],
        "dodgerblue" => [30, 144, 255],
        "firebrick" => [178, 34, 34],
        "floralwhite" => [255, 250, 240],
        "forestgreen" => [34, 139, 34],
        "fuchsia" | "magenta" => [255, 0, 255],
        "gainsboro" => [220, 220, 220],
        "ghostwhite" => [248, 248, 255],
        "gold" => [255, 215, 0],
        "goldenrod" => [218, 165, 32],
        "gray" | "grey" => [128, 128, 128],
        "green" => [0, 128, 0],
        "greenyellow" => [173, 255, 47],
        "honeydew" => [240, 255, 240],
        "hotpink" => [255, 105, 180],
        "indianred" => [205, 92, 92],
        "indigo" => [75, 0, 130],
        "ivory" => [255, 255, 240],
        "khaki" => [240, 230, 140],
        "lavender" => [230, 230, 250],
        "lavenderblush" => [255, 240, 245],
        "lawngreen" => [124, 252, 0],
        "lemonchiffon" => [255, 250, 205],
        "lightblue" => [173, 216, 230],
        "lightcoral" => [240, 128, 128],
        "lightcyan" => [224, 255, 255],
        "lightgoldenrodyellow" => [250, 250, 210],
        "lightgray" | "lightgrey" => [211, 211, 211],
        "lightgreen" => [144, 238, 144],
        "lightpink" => [255, 182, 193],
        "lightsalmon" => [255, 160, 122],
        "lightseagreen" => [32, 178, 170],
        "lightskyblue" => [135, 206, 250],
        "lightslategray" | "lightslategrey" => [119, 136, 153],
        "lightsteelblue" => [176, 196, 222],
        "lightyellow" => [255, 255, 224],
        "lime" => [0, 255, 0],
        "limegreen" => [50, 205, 50],
        "linen" => [250, 240, 230],
        "maroon" => [128, 0, 0],
        "mediumaquamarine" => [102, 205, 170],
        "mediumblue" => [0, 0, 205],
        "mediumorchid" => [186, 85, 211],
        "mediumpurple" => [147, 112, 219],
        "mediumseagreen" => [60, 179, 113],
        "mediumslateblue" => [123, 104, 238],
        "mediumspringgreen" => [0, 250, 154],
        "mediumturquoise" => [72, 209, 204],
        "mediumvioletred" => [199, 21, 133],
        "midnightblue" => [25, 25, 112],
        "mintcream" => [245, 255, 250],
        "mistyrose" => [255, 228, 225],
        "moccasin" => [255, 228, 181],
        "navajowhite" => [255, 222, 173],
        "navy" => [0, 0, 128],
        "oldlace" => [253, 245, 230],
        "olive" => [128, 128, 0],
        "olivedrab" => [107, 142, 35],
        "orange" => [255, 165, 0],
        "orangered" => [255, 69, 0],
        "orchid" => [218, 112, 214],
        "palegoldenrod" => [238, 232, 170],
        "palegreen" => [152, 251, 152],
        "paleturquoise" => [175, 238, 238],
        "palevioletred" => [219, 112, 147],
        "papayawhip" => [255, 239, 213],
        "peachpuff" => [255, 218, 185],
        "peru" => [205, 133, 63],
        "pink" => [255, 192, 203],
        "plum" => [221, 160, 221],
        "powderblue" => [176, 224, 230],
        "purple" => [128, 0, 128],
        "rebeccapurple" => [102, 51, 153],
        "red" => [255, 0, 0],
        "rosybrown" => [188, 143, 143],
        "royalblue" => [65, 105, 225],
        "saddlebrown" => [139, 69, 19],
        "salmon" => [250, 128, 114],
        "sandybrown" => [244, 164, 96],
        "seagreen" => [46, 139, 87],
        "seashell" => [255, 245, 238],
        "sienna" => [160, 82, 45],
        "silver" => [192, 192, 192],
        "skyblue" => [135, 206, 235],
        "slateblue" => [106, 90, 205],
        "slategray" | "slategrey" => [112, 128, 144],
        "snow" => [255, 250, 250],
        "springgreen" => [0, 255, 127],
        "steelblue" => [70, 130, 180],
        "tan" => [210, 180, 140],
        "teal" => [0, 128, 128],
        "thistle" => [216, 191, 216],
        "tomato" => [255, 99, 71],
        "transparent" => return Some([0, 0, 0, 0]),
        "turquoise" => [64, 224, 208],
        "violet" => [238, 130, 238],
        "wheat" => [245, 222, 179],
        "white" => [255, 255, 255],
        "whitesmoke" => [245, 245, 245],
        "yellow" => [255, 255, 0],
        "yellowgreen" => [154, 205, 50],
        _ => return None,
    };

    Some([rgb[0], rgb[1], rgb[2], 255])
}

fn draw_rect(
    rgba: &mut [u8],
    width: usize,
    x: usize,
    y: usize,
    rect_width: usize,
    rect_height: usize,
    color: [u8; 4],
) {
    for yy in y..(y + rect_height) {
        for xx in x..(x + rect_width) {
            set_pixel(rgba, width, xx, yy, color);
        }
    }
}

fn draw_circle(rgba: &mut [u8], width: usize, cx: isize, cy: isize, radius: isize, color: [u8; 4]) {
    for yy in (cy - radius)..=(cy + radius) {
        for xx in (cx - radius)..=(cx + radius) {
            let dx = xx - cx;
            let dy = yy - cy;
            if dx * dx + dy * dy <= radius * radius {
                set_pixel(rgba, width, xx as usize, yy as usize, color);
            }
        }
    }
}

fn set_pixel(rgba: &mut [u8], width: usize, x: usize, y: usize, color: [u8; 4]) {
    let index = (y * width + x) * 4;
    if index + 3 < rgba.len() {
        rgba[index..index + 4].copy_from_slice(&color);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        background_service_action_text, build_process_tree, controller_config_changed,
        process_menu_label, process_tree_signature, process_wait_refresh_interval,
        process_wait_refresh_relevant, waiting_processes_status, AppConfig, MenuBarApp,
        ProcessDisplayState, ProcessInfo, ProcessTree, ProcessTreeNode, ProcessWaitStatus,
        ServiceStatus, StatusSnapshot, SystemError, PROCESS_WAIT_REFRESH_INTERVAL,
    };
    use no_sleep_till_done::config::ProcessWaitConfig;
    use no_sleep_till_done::system::LidState;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn process(pid: u32, ppid: u32, command: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            command: command.into(),
        }
    }

    fn pids(nodes: &[ProcessTreeNode]) -> Vec<u32> {
        nodes.iter().map(|node| node.process.pid).collect()
    }

    fn process_wait_config(command_substrings: &[&str]) -> ProcessWaitConfig {
        ProcessWaitConfig {
            enabled: true,
            command_substrings: command_substrings
                .iter()
                .map(|value| (*value).into())
                .collect(),
            exit_grace_seconds: 300,
        }
    }

    fn test_app() -> MenuBarApp {
        let config = AppConfig {
            process_wait: process_wait_config(&["agent"]),
            ..AppConfig::default()
        };
        MenuBarApp::new(
            config,
            PathBuf::from("/tmp/no-sleep-till-done-test.toml"),
            1,
        )
    }

    #[test]
    fn healthy_service_has_no_maintenance_action() {
        assert_eq!(background_service_action_text(&ServiceStatus::Ready), None);
        assert_eq!(
            background_service_action_text(&ServiceStatus::Missing),
            Some("Install Background Service...")
        );
    }

    #[test]
    fn menu_only_config_changes_do_not_require_controller_reload() {
        let applied = AppConfig::default();
        let mut current = applied.clone();
        current.colors.ready = "lime".into();
        current.menu_refresh_seconds = 9;
        assert!(!controller_config_changed(&current, &applied));

        current.delay_seconds = 30;
        assert!(controller_config_changed(&current, &applied));
    }

    #[test]
    fn process_tree_deduplicates_shared_ancestors_and_sorts_nodes() {
        let table = vec![
            process(40, 20, "worker-b"),
            process(10, 1, "shell"),
            process(30, 20, "worker-a"),
            process(20, 10, "runner"),
        ];
        let matches = vec![table[0].clone(), table[2].clone()];

        let tree = build_process_tree(&table, &matches);

        assert_eq!(tree.match_count, 2);
        assert_eq!(pids(&tree.roots), vec![10]);
        assert!(!tree.roots[0].is_match);
        assert_eq!(pids(&tree.roots[0].children), vec![20]);
        assert_eq!(pids(&tree.roots[0].children[0].children), vec![30, 40]);
        assert!(tree.roots[0].children[0]
            .children
            .iter()
            .all(|node| node.is_match));
    }

    #[test]
    fn process_tree_uses_highest_available_roots_and_omits_unrelated_descendants() {
        let table = vec![
            process(60, 1, "direct-b"),
            process(30, 20, "direct-a"),
            process(20, 10, "ancestor"),
            process(10, 1, "root"),
            process(40, 20, "unrelated-child"),
            process(50, 30, "unrelated-grandchild"),
        ];
        let matches = vec![table[0].clone(), table[1].clone()];

        let tree = build_process_tree(&table, &matches);

        assert_eq!(pids(&tree.roots), vec![10, 60]);
        assert_eq!(pids(&tree.roots[0].children), vec![20]);
        assert_eq!(pids(&tree.roots[0].children[0].children), vec![30]);
        assert!(!process_tree_signature(&tree)
            .iter()
            .any(|(pid, _, _, _)| *pid == 40 || *pid == 50));
    }

    #[test]
    fn process_tree_breaks_parent_cycles_without_recursing_forever() {
        let table = vec![process(20, 30, "direct"), process(30, 20, "cycle")];
        let tree = build_process_tree(&table, &[table[0].clone()]);

        assert_eq!(pids(&tree.roots), vec![30]);
        assert_eq!(pids(&tree.roots[0].children), vec![20]);
        assert!(tree.roots[0].children[0].children.is_empty());
        assert!(tree.roots[0].children[0].is_match);
    }

    #[test]
    fn process_menu_labels_mark_matches_and_truncate_unicode_safely() {
        let node = ProcessTreeNode {
            process: process(123, 1, &"é".repeat(120)),
            is_match: true,
            children: Vec::new(),
        };

        let label = process_menu_label(&node);

        assert_eq!(label.chars().count(), 100);
        assert!(label.starts_with("* [123] "));
        assert!(label.ends_with("..."));
    }

    #[test]
    fn process_wait_refresh_is_one_second_only_in_relevant_closed_lid_state() {
        let config = process_wait_config(&["agent"]);
        assert!(!process_wait_refresh_relevant(
            false,
            Some(LidState::Closed),
            true,
            &config
        ));
        assert!(!process_wait_refresh_relevant(
            true,
            Some(LidState::Open),
            true,
            &config
        ));
        assert!(!process_wait_refresh_relevant(
            true,
            Some(LidState::Closed),
            false,
            &config
        ));
        assert!(process_wait_refresh_relevant(
            true,
            Some(LidState::Closed),
            true,
            &config
        ));
        assert_eq!(
            process_wait_refresh_interval(true, Some(Duration::ZERO), 9),
            PROCESS_WAIT_REFRESH_INTERVAL
        );
        assert_eq!(
            process_wait_refresh_interval(false, None, 9),
            Duration::from_secs(9)
        );
        assert_eq!(
            process_wait_refresh_interval(false, Some(Duration::from_millis(250)), 5),
            Duration::from_millis(250)
        );
        assert_eq!(
            process_wait_refresh_interval(false, Some(Duration::from_secs(9)), 5),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn waiting_status_uses_singular_and_plural_wording() {
        assert_eq!(waiting_processes_status(1), "waiting for 1 process");
        assert_eq!(waiting_processes_status(2), "waiting for 2 processes");
    }

    #[test]
    fn failed_process_scan_is_unavailable_and_does_not_start_grace() {
        let mut app = test_app();
        let now = Instant::now();
        let result = app.process_wait_status_from_scan(
            Err(SystemError::CommandFailed("ps failed".into())),
            now,
        );

        assert_eq!(result, Some(ProcessWaitStatus::Unavailable));
        assert_eq!(app.process_display_state, ProcessDisplayState::Unavailable);
        assert!(!app.saw_process_wait_matches);
        assert!(app.process_grace_started.is_none());
    }

    #[test]
    fn failed_process_scan_clears_old_display_but_preserves_existing_grace() {
        let mut app = test_app();
        let started = Instant::now() - Duration::from_secs(10);
        app.saw_process_wait_matches = true;
        app.process_grace_started = Some(started);
        app.process_display_state = ProcessDisplayState::Waiting(ProcessTree {
            match_count: 1,
            roots: vec![ProcessTreeNode {
                process: process(123, 1, "agent"),
                is_match: true,
                children: Vec::new(),
            }],
        });

        let result = app.process_wait_status_from_scan(
            Err(SystemError::CommandFailed("ps failed".into())),
            Instant::now(),
        );

        assert_eq!(result, Some(ProcessWaitStatus::Unavailable));
        assert_eq!(app.process_display_state, ProcessDisplayState::Unavailable);
        assert_eq!(app.process_grace_started, Some(started));
    }

    #[test]
    fn matching_process_reappearing_during_grace_restores_waiting_tree() {
        let mut app = test_app();
        app.last_lid = Some(LidState::Closed);
        app.lid_closed_since =
            Some(Instant::now() - Duration::from_secs(app.applied_config.delay_seconds + 1));
        app.saw_process_wait_matches = true;
        app.process_grace_started = Some(Instant::now() - Duration::from_secs(1));

        let result =
            app.process_wait_status_with_scan(Some(LidState::Closed), Instant::now(), || {
                Ok(vec![process(4242, 1, "agent --reappeared")])
            });

        let Some(ProcessWaitStatus::Waiting { tree }) = result else {
            panic!("a matching process during grace should restore waiting");
        };
        assert_eq!(tree.match_count, 1);
        assert_eq!(pids(&tree.roots), vec![4242]);
        assert_eq!(
            app.process_display_state,
            ProcessDisplayState::Waiting(tree)
        );
        assert!(app.process_grace_started.is_none());
    }

    #[test]
    fn unavailable_status_does_not_fall_back_to_a_stale_count() {
        let snapshot = StatusSnapshot {
            battery: None,
            lid: Some(LidState::Closed),
            sleep_disabled: Some(true),
            remaining: None,
            process_wait: Some(ProcessWaitStatus::Unavailable),
            enabled: true,
            service_status: ServiceStatus::Ready,
            config_notice: None,
        };

        let status = snapshot.menu_status_text(&AppConfig::default());

        assert!(status.contains("process status unavailable"));
        assert!(!status.contains("waiting for"));
    }
}
