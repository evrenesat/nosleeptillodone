use std::env;
use std::fmt;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

const CONTROLLER_LABEL: &str = "com.evren.nosleeptilldone";
const DEFAULT_INTERVAL_SECONDS: u64 = 5;
const DEFAULT_GRACE_SECONDS: u64 = 20;

#[derive(Clone, Debug)]
struct Config {
    interval: Duration,
    grace: Duration,
    dry_run: bool,
    once: bool,
}

#[derive(Debug)]
enum AppError {
    Cli(String),
    RootRequired,
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
            AppError::RootRequired => write!(
                f,
                "no-sleep-till-done-watchdog must run as root unless --dry-run is used"
            ),
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

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        AppError::Io(error)
    }
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("no-sleep-till-done-watchdog: {error}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<(), AppError> {
    let config = parse_args(env::args().skip(1))?;
    install_signal_handlers();

    if !config.dry_run && unsafe { libc::geteuid() } != 0 {
        return Err(AppError::RootRequired);
    }

    log(&format!(
        "starting; interval={}s; grace={}s; dry_run={}",
        config.interval.as_secs(),
        config.grace.as_secs(),
        config.dry_run
    ));

    let mut missing_since: Option<Instant> = None;

    loop {
        let running = controller_running()?;
        if running {
            missing_since = None;
            log("controller is running");
        } else {
            let first_missing = *missing_since.get_or_insert_with(Instant::now);
            let missing_for = first_missing.elapsed();
            log(&format!(
                "controller is not running; missing for {}s",
                missing_for.as_secs()
            ));

            if missing_for >= config.grace {
                restore_lid_sleep(config.dry_run)?;
            }
        }

        if config.once || SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        thread::sleep(config.interval);
    }

    log("shutting down");
    Ok(())
}

fn controller_running() -> Result<bool, AppError> {
    let service = format!("system/{CONTROLLER_LABEL}");
    let output = Command::new("/bin/launchctl")
        .args(["print", &service])
        .output()?;

    if !output.status.success() {
        return Ok(false);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().any(|line| line.trim() == "state = running"))
}

fn restore_lid_sleep(dry_run: bool) -> Result<(), AppError> {
    run_command("/usr/bin/pmset", &["-b", "disablesleep", "0"], dry_run)
}

fn run_command(program: &str, args: &[&str], dry_run: bool) -> Result<(), AppError> {
    if dry_run {
        log(&format!("dry-run: {program} {}", args.join(" ")));
        return Ok(());
    }

    let output = Command::new(program).args(args).output()?;
    if output.status.success() {
        log(&format!("{program} {} succeeded", args.join(" ")));
        return Ok(());
    }

    Err(AppError::CommandFailed {
        program: program.into(),
        args: args.iter().map(|arg| (*arg).into()).collect(),
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn parse_args<I>(args: I) -> Result<Config, AppError>
where
    I: IntoIterator<Item = String>,
{
    let mut config = Config {
        interval: Duration::from_secs(DEFAULT_INTERVAL_SECONDS),
        grace: Duration::from_secs(DEFAULT_GRACE_SECONDS),
        dry_run: false,
        once: false,
    };

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--interval-seconds" => {
                let value = args
                    .next()
                    .ok_or_else(|| AppError::Cli("--interval-seconds requires a value".into()))?;
                config.interval =
                    Duration::from_secs(parse_positive_u64("--interval-seconds", &value)?);
            }
            "--grace-seconds" => {
                let value = args
                    .next()
                    .ok_or_else(|| AppError::Cli("--grace-seconds requires a value".into()))?;
                config.grace = Duration::from_secs(parse_positive_u64("--grace-seconds", &value)?);
            }
            "--dry-run" => config.dry_run = true,
            "--once" => config.once = true,
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
Usage: no-sleep-till-done-watchdog [OPTIONS]

Options:
  --interval-seconds N      Seconds between controller checks [{DEFAULT_INTERVAL_SECONDS}]
  --grace-seconds N         Seconds controller may be absent before repair [{DEFAULT_GRACE_SECONDS}]
  --dry-run                 Log repair commands instead of changing power settings
  --once                    Check once, then exit
  -h, --help                Show this help
"
    )
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
    eprintln!("no-sleep-till-done-watchdog: {message}");
}
