use libc::{c_char, c_uchar, c_void};
use std::ffi::CString;
use std::process::Command;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LidState {
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerSource {
    Battery,
    AC,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatteryState {
    pub percent: Option<u8>,
    pub source: PowerSource,
    pub charging: bool,
    pub raw: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub command: String,
}

#[derive(Debug)]
pub enum SystemError {
    CString(std::ffi::NulError),
    Iokit(String),
    Command(std::io::Error),
    CommandFailed(String),
}

impl std::fmt::Display for SystemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SystemError::CString(error) => write!(f, "{error}"),
            SystemError::Iokit(message) => write!(f, "{message}"),
            SystemError::Command(error) => write!(f, "{error}"),
            SystemError::CommandFailed(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for SystemError {}

impl From<std::ffi::NulError> for SystemError {
    fn from(error: std::ffi::NulError) -> Self {
        SystemError::CString(error)
    }
}

impl From<std::io::Error> for SystemError {
    fn from(error: std::io::Error) -> Self {
        SystemError::Command(error)
    }
}

pub fn read_lid_state() -> Result<LidState, SystemError> {
    read_root_domain_bool("AppleClamshellState").map(|closed| {
        if closed {
            LidState::Closed
        } else {
            LidState::Open
        }
    })
}

pub fn read_sleep_disabled() -> Result<bool, SystemError> {
    read_root_domain_bool("SleepDisabled")
}

pub fn read_battery_state() -> Result<BatteryState, SystemError> {
    let output = Command::new("/usr/bin/pmset")
        .args(["-g", "batt"])
        .output()?;
    if !output.status.success() {
        return Err(SystemError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }

    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let source = if text.contains("'Battery Power'") {
        PowerSource::Battery
    } else if text.contains("'AC Power'") {
        PowerSource::AC
    } else {
        PowerSource::Unknown
    };

    let percent = text
        .split_whitespace()
        .find_map(|part| part.strip_suffix("%;"))
        .and_then(|value| value.parse::<u8>().ok())
        .or_else(|| {
            text.split('%')
                .next()
                .and_then(|prefix| prefix.split_whitespace().last())
                .and_then(|value| value.parse::<u8>().ok())
        });

    let charging = text.contains("; charging;") || text.contains("; charged;");

    Ok(BatteryState {
        percent,
        source,
        charging,
        raw: text,
    })
}

pub fn process_table() -> Result<Vec<ProcessInfo>, SystemError> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()?;
    if !output.status.success() {
        return Err(SystemError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_ps_line)
        .collect())
}

pub fn matching_processes(command_substrings: &[String]) -> Result<Vec<ProcessInfo>, SystemError> {
    if command_substrings.iter().all(String::is_empty) {
        return Ok(Vec::new());
    }

    let table = process_table()?;
    Ok(filter_processes(
        &table,
        command_substrings,
        std::process::id(),
    ))
}

pub fn filter_processes(
    table: &[ProcessInfo],
    command_substrings: &[String],
    excluded_pid: u32,
) -> Vec<ProcessInfo> {
    let needles: Vec<&str> = command_substrings
        .iter()
        .map(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .collect();

    if needles.is_empty() {
        return Vec::new();
    }

    table
        .iter()
        .filter(|process| process.pid != excluded_pid)
        .filter(|process| {
            needles
                .iter()
                .any(|needle| process.command.contains(needle))
        })
        .cloned()
        .collect()
}

fn parse_ps_line(line: &str) -> Option<ProcessInfo> {
    let line = line.trim_start();
    let pid_end = line.find(char::is_whitespace)?;
    let pid = line[..pid_end].parse::<u32>().ok()?;

    let ppid_field = line[pid_end..].trim_start();
    let ppid_end = ppid_field.find(char::is_whitespace)?;
    let ppid = ppid_field[..ppid_end].parse::<u32>().ok()?;

    let command = ppid_field[ppid_end..].trim_start();
    if command.is_empty() {
        return None;
    }

    Some(ProcessInfo {
        pid,
        ppid,
        command: command.to_string(),
    })
}

fn read_root_domain_bool(property: &str) -> Result<bool, SystemError> {
    let service_name = CString::new("IOPMrootDomain")?;
    let key_name = CString::new(property)?;

    unsafe {
        let matching = IOServiceMatching(service_name.as_ptr());
        if matching.is_null() {
            return Err(SystemError::Iokit(
                "failed to create IOPMrootDomain matcher".into(),
            ));
        }

        let entry = IOServiceGetMatchingService(0, matching);
        if entry == 0 {
            return Err(SystemError::Iokit("IOPMrootDomain not found".into()));
        }

        let key = CFStringCreateWithCString(
            std::ptr::null(),
            key_name.as_ptr(),
            K_CF_STRING_ENCODING_UTF8,
        );
        if key.is_null() {
            let _ = IOObjectRelease(entry);
            return Err(SystemError::Iokit(format!(
                "failed to create {property} key"
            )));
        }

        let value = IORegistryEntryCreateCFProperty(entry, key, std::ptr::null(), 0);
        CFRelease(key);
        let _ = IOObjectRelease(entry);

        if value.is_null() {
            return Err(SystemError::Iokit(format!("{property} property not found")));
        }

        let result = CFBooleanGetValue(value) != 0;
        CFRelease(value);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::{filter_processes, parse_ps_line, ProcessInfo};

    fn process(pid: u32, ppid: u32, command: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            command: command.into(),
        }
    }

    #[test]
    fn parses_pid_ppid_and_command_with_leading_whitespace() {
        assert_eq!(
            parse_ps_line("   123   45 /usr/bin/python script with spaces"),
            Some(process(123, 45, "/usr/bin/python script with spaces"))
        );
    }

    #[test]
    fn rejects_incomplete_or_invalid_process_rows() {
        for line in [
            "",
            "123",
            "123 45",
            "not-a-pid 45 command",
            "123 not-a-ppid command",
            "123 45   ",
        ] {
            assert_eq!(
                parse_ps_line(line),
                None,
                "row should be rejected: {line:?}"
            );
        }
    }

    #[test]
    fn filters_case_sensitive_substrings_and_excludes_the_requested_pid() {
        let table = vec![
            process(10, 1, "/usr/bin/codex agent"),
            process(11, 1, "/usr/bin/Codex agent"),
            process(12, 1, "/usr/bin/python worker"),
        ];

        assert_eq!(
            filter_processes(&table, &["".into(), "agent".into()], 10),
            vec![process(11, 1, "/usr/bin/Codex agent")]
        );
        assert!(filter_processes(&table, &["".into()], 0).is_empty());
    }
}
