use libc::{c_char, c_uchar, c_void};
use no_sleep_till_done::config::{load_or_create_config, AppConfig, ProcessWaitConfig};
use no_sleep_till_done::system::matching_processes;
use no_sleep_till_done::{
    LeaseRecord, APP_ACTIVE_LEASE_PATH, CONTROLLER_RESET_REQUEST_PATH, LEASE_TIMEOUT_SECONDS,
};
use std::env;
use std::ffi::CString;
use std::fmt;
use std::fs;
use std::io;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

const DEFAULT_DELAY_SECONDS: u64 = 60;
const DEFAULT_POLL_SECONDS: u64 = 1;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

type KernReturn = i32;
type MachPort = u32;
type IoObject = u32;
type IoRegistryEntry = IoObject;
type CfAllocatorRef = *const c_void;
type CfDictionaryRef = *const c_void;
type CfStringRef = *const c_void;
type CfTypeRef = *const c_void;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CfDictionaryRef;
    fn IOServiceGetMatchingService(
        masterPort: MachPort,
        matching: CfDictionaryRef,
    ) -> IoRegistryEntry;
    fn IORegistryEntryCreateCFProperty(
        entry: IoRegistryEntry,
        key: CfStringRef,
        allocator: CfAllocatorRef,
        options: u32,
    ) -> CfTypeRef;
    fn IOObjectRelease(object: IoObject) -> KernReturn;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(
        alloc: CfAllocatorRef,
        cStr: *const c_char,
        encoding: u32,
    ) -> CfStringRef;
    fn CFBooleanGetValue(boolean: CfTypeRef) -> c_uchar;
    fn CFRelease(cf: CfTypeRef);
}

#[derive(Clone, Debug)]
struct Config {
    enabled: bool,
    delay: Duration,
    poll: Duration,
    process_wait: ProcessWait,
    delay_override: Option<Duration>,
    poll_override: Option<Duration>,
    dry_run: bool,
    once: bool,
    keep_disabled_on_exit: bool,
}

#[derive(Clone, Debug)]
struct RuntimeState {
    active: bool,
    lid: LidState,
    closed_sleep: Option<ClosedSleepState>,
    reload_generation: Option<u64>,
}

#[derive(Clone, Debug)]
struct ProcessWait {
    enabled: bool,
    command_substrings: Vec<String>,
    exit_grace: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LidState {
    Open,
    Closed,
}

#[derive(Clone, Debug)]
enum ClosedSleepState {
    Delay(Instant),
    WaitingForProcesses { last_count: usize },
    ProcessExitGrace(Instant),
}

#[derive(Debug)]
enum AppError {
    Cli(String),
    RootRequired,
    CString(std::ffi::NulError),
    Iokit(String),
    Config(String),
    CommandFailed {
        program: String,
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },
    Io(std::io::Error),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Cli(message) => write!(f, "{message}"),
            AppError::RootRequired => {
                write!(
                    f,
                    "no-sleep-till-done is the privileged controller and must run with sudo unless --dry-run is used; run no-sleep-till-done-menubar as the normal user app"
                )
            }
            AppError::CString(error) => write!(f, "invalid C string: {error}"),
            AppError::Iokit(message) => write!(f, "IOKit error: {message}"),
            AppError::Config(message) => write!(f, "configuration error: {message}"),
            AppError::CommandFailed {
                program,
                args,
                code,
                stderr,
            } => write!(
                f,
                "{program} {} failed with {:?}: {}",
                args.join(" "),
                code,
                stderr.trim()
            ),
            AppError::Io(error) => write!(f, "{error}"),
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        AppError::Io(error)
    }
}

impl From<std::ffi::NulError> for AppError {
    fn from(error: std::ffi::NulError) -> Self {
        AppError::CString(error)
    }
}

struct PowerGuard {
    dry_run: bool,
    keep_disabled_on_exit: bool,
}

impl PowerGuard {
    fn new(dry_run: bool, keep_disabled_on_exit: bool) -> Self {
        Self {
            dry_run,
            keep_disabled_on_exit,
        }
    }

    fn enable_lid_awake(&self) -> Result<(), AppError> {
        run_command("/usr/bin/pmset", &["-b", "disablesleep", "1"], self.dry_run)
    }

    fn restore_lid_awake(&self) -> Result<(), AppError> {
        if self.keep_disabled_on_exit {
            log("leaving battery SleepDisabled enabled because --keep-disabled-on-exit was set");
            return Ok(());
        }

        run_command("/usr/bin/pmset", &["-b", "disablesleep", "0"], self.dry_run)
    }

    fn force_sleep(&self) -> Result<(), AppError> {
        run_command("/usr/bin/pmset", &["-b", "disablesleep", "0"], self.dry_run)?;
        run_command("/usr/bin/pmset", &["sleepnow"], self.dry_run)?;
        thread::sleep(Duration::from_secs(5));
        self.enable_lid_awake()
    }

    fn display_sleep_now(&self) -> Result<(), AppError> {
        run_command("/usr/bin/pmset", &["displaysleepnow"], self.dry_run)
    }

    fn wake_display(&self) -> Result<(), AppError> {
        run_command("/usr/bin/caffeinate", &["-u", "-t", "2"], self.dry_run)
    }
}

impl Drop for PowerGuard {
    fn drop(&mut self) {
        if let Err(error) = self.restore_lid_awake() {
            eprintln!("no-sleep-till-done: failed to restore SleepDisabled state: {error}");
        }
    }
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("no-sleep-till-done: {error}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<(), AppError> {
    let mut config = parse_args(env::args().skip(1))?;
    install_signal_handlers();

    if !config.dry_run && unsafe { libc::geteuid() } != 0 {
        return Err(AppError::RootRequired);
    }

    let current_lid = read_lid_state()?;

    log(&format!(
        "starting; enabled={}; lid={current_lid:?}; delay={}s; poll={}s; dry_run={}",
        config.enabled,
        config.delay.as_secs(),
        config.poll.as_secs(),
        config.dry_run
    ));

    if config.once {
        return Ok(());
    }

    let power = PowerGuard::new(config.dry_run, config.keep_disabled_on_exit);
    let mut runtime = RuntimeState {
        active: false,
        lid: current_lid,
        closed_sleep: None,
        reload_generation: None,
    };

    while !SHUTDOWN.load(Ordering::SeqCst) {
        thread::sleep(config.poll);

        let requested_reset = reset_requested()?;
        if requested_reset {
            clear_reset_request()?;
        }

        let lease = app_lease()?;
        if let Some(lease) = lease {
            if runtime.reload_generation != Some(lease.reload_generation) {
                runtime.reload_generation = Some(lease.reload_generation);
                match reload_config(&config) {
                    Ok(reloaded) => {
                        config = reloaded;
                        log(&format!(
                            "configuration reloaded; enabled={}; delay={}s; poll={}s; process_wait={}; exit_grace={}s",
                            config.enabled,
                            config.delay.as_secs(),
                            config.poll.as_secs(),
                            config.process_wait.is_active(),
                            config.process_wait.exit_grace.as_secs()
                        ));
                        if runtime.active && runtime.lid == LidState::Closed {
                            log("configuration changed while lid closed; restarting delay timer");
                            runtime.closed_sleep = Some(ClosedSleepState::Delay(Instant::now()));
                        }
                    }
                    Err(error) => {
                        eprintln!(
                            "no-sleep-till-done: reload rejected; keeping previous configuration: {error}"
                        );
                    }
                }
            }
        }

        let active = lease.is_some_and(|lease| lease.enabled && config.enabled) && !requested_reset;
        if active != runtime.active {
            runtime.active = active;
            runtime.closed_sleep = None;
            if active {
                log("app lease active; enabling delayed lid-close control");
                power.enable_lid_awake()?;
                if runtime.lid == LidState::Closed {
                    power.display_sleep_now()?;
                    runtime.closed_sleep = Some(ClosedSleepState::Delay(Instant::now()));
                }
            } else {
                log("app lease inactive; restoring normal lid-close sleep");
                power.restore_lid_awake()?;
            }
        }

        if !runtime.active {
            continue;
        }

        let next_lid = match read_lid_state() {
            Ok(state) => state,
            Err(error) => {
                eprintln!("no-sleep-till-done: failed to read lid state: {error}");
                continue;
            }
        };

        if next_lid != runtime.lid {
            match next_lid {
                LidState::Closed => {
                    log("lid closed; sleeping display and starting delay timer");
                    if let Err(error) = power.display_sleep_now() {
                        eprintln!("no-sleep-till-done: failed to sleep display: {error}");
                    }
                    runtime.closed_sleep = Some(ClosedSleepState::Delay(Instant::now()));
                }
                LidState::Open => {
                    log("lid opened; canceling delay timer and process wait state");
                    runtime.closed_sleep = None;
                    if let Err(error) = power.wake_display() {
                        eprintln!("no-sleep-till-done: failed to wake display: {error}");
                    }
                }
            }

            runtime.lid = next_lid;
        }

        if runtime.lid == LidState::Closed {
            if let Some(state) = runtime.closed_sleep.take() {
                runtime.closed_sleep = handle_closed_sleep_state(state, &config, &power)?;
            }
        }
    }

    log("shutting down");
    Ok(())
}

fn parse_args<I>(args: I) -> Result<Config, AppError>
where
    I: IntoIterator<Item = String>,
{
    let file_config = load_or_create_config()
        .map_err(|error| AppError::Config(error.to_string()))?
        .0;
    let mut config = Config {
        enabled: file_config.enabled,
        delay: Duration::from_secs(file_config.delay_seconds),
        poll: Duration::from_secs(file_config.poll_seconds),
        process_wait: ProcessWait::from_config(file_config.process_wait),
        delay_override: None,
        poll_override: None,
        dry_run: false,
        once: false,
        keep_disabled_on_exit: false,
    };

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--delay-seconds" => {
                let value = args
                    .next()
                    .ok_or_else(|| AppError::Cli("--delay-seconds requires a value".into()))?;
                let seconds = parse_positive_u64("--delay-seconds", &value)?;
                config.delay = Duration::from_secs(seconds);
                config.delay_override = Some(config.delay);
            }
            "--poll-seconds" => {
                let value = args
                    .next()
                    .ok_or_else(|| AppError::Cli("--poll-seconds requires a value".into()))?;
                let seconds = parse_positive_u64("--poll-seconds", &value)?;
                config.poll = Duration::from_secs(seconds);
                config.poll_override = Some(config.poll);
            }
            "--dry-run" => config.dry_run = true,
            "--once" => config.once = true,
            "--keep-disabled-on-exit" => config.keep_disabled_on_exit = true,
            "-h" | "--help" => return Err(AppError::Cli(usage())),
            other => {
                return Err(AppError::Cli(format!(
                    "unknown argument: {other}\n\n{}",
                    usage()
                )))
            }
        }
    }

    Ok(config)
}

fn reload_config(current: &Config) -> Result<Config, AppError> {
    let app_config = load_or_create_config()
        .map_err(|error| AppError::Config(error.to_string()))?
        .0;
    Ok(current.with_app_config(app_config))
}

impl Config {
    fn with_app_config(&self, app_config: AppConfig) -> Self {
        let mut reloaded = self.clone();
        reloaded.enabled = app_config.enabled;
        reloaded.delay = self
            .delay_override
            .unwrap_or_else(|| Duration::from_secs(app_config.delay_seconds));
        reloaded.poll = self
            .poll_override
            .unwrap_or_else(|| Duration::from_secs(app_config.poll_seconds));
        reloaded.process_wait = ProcessWait::from_config(app_config.process_wait);
        reloaded
    }
}

impl ProcessWait {
    fn from_config(config: ProcessWaitConfig) -> Self {
        Self {
            enabled: config.enabled,
            command_substrings: config.command_substrings,
            exit_grace: Duration::from_secs(config.exit_grace_seconds),
        }
    }

    fn is_active(&self) -> bool {
        self.enabled && !self.command_substrings.is_empty()
    }
}

fn handle_closed_sleep_state(
    state: ClosedSleepState,
    config: &Config,
    power: &PowerGuard,
) -> Result<Option<ClosedSleepState>, AppError> {
    match state {
        ClosedSleepState::Delay(started) => {
            if started.elapsed() < config.delay {
                return Ok(Some(ClosedSleepState::Delay(started)));
            }

            if !config.process_wait.is_active() {
                log("delay elapsed while lid remained closed; forcing system sleep");
                power.force_sleep()?;
                return Ok(Some(ClosedSleepState::Delay(Instant::now())));
            }

            match matching_processes(&config.process_wait.command_substrings) {
                Ok(matches) if matches.is_empty() => {
                    log("delay elapsed; no matching processes; forcing system sleep");
                    power.force_sleep()?;
                    Ok(Some(ClosedSleepState::Delay(Instant::now())))
                }
                Ok(matches) => {
                    log(&format!(
                        "delay elapsed; waiting for matching processes ({})",
                        matches.len()
                    ));
                    log_process_matches(&matches);
                    Ok(Some(ClosedSleepState::WaitingForProcesses {
                        last_count: matches.len(),
                    }))
                }
                Err(error) => {
                    eprintln!("no-sleep-till-done: failed to inspect processes: {error}");
                    Ok(Some(ClosedSleepState::Delay(started)))
                }
            }
        }
        ClosedSleepState::WaitingForProcesses { last_count } => {
            match matching_processes(&config.process_wait.command_substrings) {
                Ok(matches) if matches.is_empty() => {
                    log("matching processes exited; starting process exit grace timer");
                    Ok(Some(ClosedSleepState::ProcessExitGrace(Instant::now())))
                }
                Ok(matches) => {
                    if matches.len() != last_count {
                        log(&format!(
                            "still waiting for matching processes ({})",
                            matches.len()
                        ));
                        log_process_matches(&matches);
                    }
                    Ok(Some(ClosedSleepState::WaitingForProcesses {
                        last_count: matches.len(),
                    }))
                }
                Err(error) => {
                    eprintln!("no-sleep-till-done: failed to inspect processes: {error}");
                    Ok(Some(ClosedSleepState::WaitingForProcesses { last_count }))
                }
            }
        }
        ClosedSleepState::ProcessExitGrace(started) => {
            if started.elapsed() < config.process_wait.exit_grace {
                return Ok(Some(ClosedSleepState::ProcessExitGrace(started)));
            }

            match matching_processes(&config.process_wait.command_substrings) {
                Ok(matches) if matches.is_empty() => {
                    log("process exit grace elapsed; forcing system sleep");
                    power.force_sleep()?;
                    Ok(Some(ClosedSleepState::Delay(Instant::now())))
                }
                Ok(matches) => {
                    log(&format!(
                        "matching processes restarted during grace; waiting again ({})",
                        matches.len()
                    ));
                    log_process_matches(&matches);
                    Ok(Some(ClosedSleepState::WaitingForProcesses {
                        last_count: matches.len(),
                    }))
                }
                Err(error) => {
                    eprintln!("no-sleep-till-done: failed to inspect processes: {error}");
                    Ok(Some(ClosedSleepState::ProcessExitGrace(started)))
                }
            }
        }
    }
}

fn app_lease() -> Result<Option<LeaseRecord>, AppError> {
    let metadata = match fs::metadata(APP_ACTIVE_LEASE_PATH) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::Io(error)),
    };

    let modified = metadata.modified()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_else(|_| Duration::from_secs(0));
    if age > Duration::from_secs(LEASE_TIMEOUT_SECONDS) {
        return Ok(None);
    }

    let text = fs::read_to_string(APP_ACTIVE_LEASE_PATH)?;
    Ok(LeaseRecord::parse(&text))
}

fn reset_requested() -> Result<bool, AppError> {
    match fs::metadata(CONTROLLER_RESET_REQUEST_PATH) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::Io(error)),
    }
}

fn clear_reset_request() -> Result<(), AppError> {
    match fs::remove_file(CONTROLLER_RESET_REQUEST_PATH) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Io(error)),
    }
}

fn log_process_matches(matches: &[no_sleep_till_done::system::ProcessMatch]) {
    for process in matches.iter().take(5) {
        log(&format!(
            "process match: pid={} {}",
            process.pid, process.command
        ));
    }

    if matches.len() > 5 {
        log(&format!("process match: ... {} more", matches.len() - 5));
    }
}

fn parse_positive_u64(name: &str, value: &str) -> Result<u64, AppError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| AppError::Cli(format!("{name} must be a positive integer")))?;

    if parsed == 0 {
        return Err(AppError::Cli(format!("{name} must be greater than zero")));
    }

    Ok(parsed)
}

fn usage() -> String {
    format!(
        "\
Usage: no-sleep-till-done [OPTIONS]

Options:
  --delay-seconds N          Seconds lid must stay closed before sleep [{DEFAULT_DELAY_SECONDS}]
  --poll-seconds N           Lid polling interval in seconds [{DEFAULT_POLL_SECONDS}]
  --dry-run                  Log commands instead of changing power settings
  --once                     Read lid state once, then exit
  --keep-disabled-on-exit    Leave battery SleepDisabled enabled when daemon exits
  -h, --help                 Show this help
"
    )
}

fn read_lid_state() -> Result<LidState, AppError> {
    let service_name = CString::new("IOPMrootDomain")?;
    let key_name = CString::new("AppleClamshellState")?;

    unsafe {
        let matching = IOServiceMatching(service_name.as_ptr());
        if matching.is_null() {
            return Err(AppError::Iokit(
                "failed to create IOPMrootDomain matcher".into(),
            ));
        }

        let entry = IOServiceGetMatchingService(0, matching);
        if entry == 0 {
            return Err(AppError::Iokit("IOPMrootDomain not found".into()));
        }

        let key = CFStringCreateWithCString(
            std::ptr::null(),
            key_name.as_ptr(),
            K_CF_STRING_ENCODING_UTF8,
        );
        if key.is_null() {
            let _ = IOObjectRelease(entry);
            return Err(AppError::Iokit(
                "failed to create AppleClamshellState key".into(),
            ));
        }

        let value = IORegistryEntryCreateCFProperty(entry, key, std::ptr::null(), 0);
        CFRelease(key);
        let _ = IOObjectRelease(entry);

        if value.is_null() {
            return Err(AppError::Iokit(
                "AppleClamshellState property not found".into(),
            ));
        }

        let closed = CFBooleanGetValue(value) != 0;
        CFRelease(value);

        if closed {
            Ok(LidState::Closed)
        } else {
            Ok(LidState::Open)
        }
    }
}

fn run_command(program: &str, args: &[&str], dry_run: bool) -> Result<(), AppError> {
    if dry_run {
        log(&format!("dry-run: {program} {}", args.join(" ")));
        return Ok(());
    }

    let output = Command::new(program).args(args).output()?;
    if output.status.success() {
        return Ok(());
    }

    Err(AppError::CommandFailed {
        program: program.into(),
        args: args.iter().map(|arg| (*arg).into()).collect(),
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn install_signal_handlers() {
    unsafe extern "C" fn handle_signal(_: i32) {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }

    unsafe {
        libc::signal(libc::SIGINT, handle_signal as *const () as usize);
        libc::signal(libc::SIGTERM, handle_signal as *const () as usize);
    }
}

fn log(message: &str) {
    eprintln!("no-sleep-till-done: {message}");
}

#[cfg(test)]
mod tests {
    use super::{Config, ProcessWait};
    use no_sleep_till_done::config::{AppConfig, ProcessWaitConfig};
    use std::time::Duration;

    #[test]
    fn reload_preserves_cli_overrides_and_updates_toml_values() {
        let current = Config {
            enabled: true,
            delay: Duration::from_secs(10),
            poll: Duration::from_secs(2),
            process_wait: ProcessWait::from_config(Default::default()),
            delay_override: Some(Duration::from_secs(10)),
            poll_override: None,
            dry_run: true,
            once: false,
            keep_disabled_on_exit: false,
        };
        let app_config = AppConfig {
            enabled: false,
            delay_seconds: 99,
            poll_seconds: 7,
            process_wait: ProcessWaitConfig {
                enabled: true,
                command_substrings: vec!["nosleeptilldone".into()],
                ..Default::default()
            },
            ..Default::default()
        };

        let reloaded = current.with_app_config(app_config);

        assert!(!reloaded.enabled);
        assert_eq!(reloaded.delay, Duration::from_secs(10));
        assert_eq!(reloaded.poll, Duration::from_secs(7));
        assert!(reloaded.process_wait.is_active());
    }
}
