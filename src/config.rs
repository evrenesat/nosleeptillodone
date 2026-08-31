use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const APP_NAME: &str = "no-sleep-till-done";
const LEGACY_APP_NAME: &str = "lidsleep-delay";
const LEGACY_CONFIG_ENV: &str = "LIDSLEEP_DELAY_CONFIG";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct AppConfig {
    pub enabled: bool,
    pub delay_seconds: u64,
    pub poll_seconds: u64,
    pub menu_refresh_seconds: u64,
    pub process_wait: ProcessWaitConfig,
    pub colors: ColorConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ProcessWaitConfig {
    pub enabled: bool,
    pub command_substrings: Vec<String>,
    pub exit_grace_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ColorConfig {
    #[serde(alias = "armed")]
    pub ready: String,
    pub timer: String,
    pub process_wait: String,
    pub error: String,
    pub unknown: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            delay_seconds: 60,
            poll_seconds: 1,
            menu_refresh_seconds: 5,
            process_wait: ProcessWaitConfig::default(),
            colors: ColorConfig::default(),
        }
    }
}

impl Default for ProcessWaitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command_substrings: Vec::new(),
            exit_grace_seconds: 300,
        }
    }
}

impl Default for ColorConfig {
    fn default() -> Self {
        Self {
            ready: "green".into(),
            timer: "orange".into(),
            process_wait: "blue".into(),
            error: "red".into(),
            unknown: "gray".into(),
        }
    }
}

pub fn config_path() -> PathBuf {
    if let Ok(value) = env::var("NO_SLEEP_TILL_DONE_CONFIG") {
        return PathBuf::from(value);
    }
    if let Ok(value) = env::var(LEGACY_CONFIG_ENV) {
        return PathBuf::from(value);
    }

    let home = default_config_home();

    home.join(".config").join(APP_NAME).join("config.toml")
}

fn default_config_home() -> PathBuf {
    if unsafe { libc::geteuid() } == 0 {
        if let Some(home) = sudo_user_home().or_else(console_user_home) {
            return home;
        }
    }

    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn sudo_user_home() -> Option<PathBuf> {
    let user = env::var("SUDO_USER").ok()?;
    user_home(&user)
}

#[cfg(target_os = "macos")]
fn console_user_home() -> Option<PathBuf> {
    let output = Command::new("/usr/bin/stat")
        .args(["-f", "%Su", "/dev/console"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let user = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if user.is_empty() || user == "root" || user == "loginwindow" {
        return None;
    }

    user_home(&user)
}

#[cfg(not(target_os = "macos"))]
fn console_user_home() -> Option<PathBuf> {
    None
}

fn user_home(user: &str) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/dscl")
            .args([".", "-read", &format!("/Users/{user}"), "NFSHomeDirectory"])
            .output()
            .ok()?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(home) = stdout
                .lines()
                .find_map(|line| line.strip_prefix("NFSHomeDirectory: "))
            {
                return Some(PathBuf::from(home.trim()));
            }
        }
    }

    Some(PathBuf::from("/Users").join(user))
}

pub fn load_or_create_config() -> Result<(AppConfig, PathBuf), ConfigError> {
    let path = config_path();
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !copy_legacy_config(&path, &default_config_home())? {
            fs::write(&path, default_config_text())?;
        }
    }

    let text = fs::read_to_string(&path)?;
    let text = migrate_config_text(&path, text)?;
    let config = toml::from_str::<AppConfig>(&text)?;
    Ok((config.normalized(), path))
}

fn copy_legacy_config(path: &Path, home: &Path) -> io::Result<bool> {
    let legacy = home
        .join(".config")
        .join(LEGACY_APP_NAME)
        .join("config.toml");
    if legacy == path || !legacy.is_file() {
        return Ok(false);
    }

    fs::copy(legacy, path)?;
    Ok(true)
}

fn migrate_config_text(path: &PathBuf, text: String) -> Result<String, ConfigError> {
    let mut migrated = text;
    let mut changed = false;

    if !has_toml_table(&migrated, "process_wait") {
        if !migrated.ends_with('\n') {
            migrated.push('\n');
        }
        migrated.push_str(
            r#"
[process_wait]
# When enabled, sleep is delayed after delay_seconds until all matching
# command lines disappear, then exit_grace_seconds is counted before sleep.
enabled = false
command_substrings = []
exit_grace_seconds = 300
"#,
        );
        changed = true;
    }

    if has_toml_table(&migrated, "colors")
        && !table_contains_key(&migrated, "colors", "process_wait")
    {
        migrated = insert_key_in_table(&migrated, "colors", "process_wait = \"blue\"");
        changed = true;
    }

    let mut document = migrated
        .parse::<toml_edit::Document>()
        .map_err(ConfigError::TomlEdit)?;
    if !document.as_table().contains_key("enabled") {
        document["enabled"] = toml_edit::value(true);
        migrated = document.to_string();
        changed = true;
    }

    if changed {
        fs::write(path, &migrated)?;
    }

    Ok(migrated)
}

pub fn set_enabled(path: &Path, enabled: bool) -> Result<(), ConfigError> {
    let text = fs::read_to_string(path)?;
    let mut document = text
        .parse::<toml_edit::Document>()
        .map_err(ConfigError::TomlEdit)?;
    document["enabled"] = toml_edit::value(enabled);
    fs::write(path, document.to_string())?;
    Ok(())
}

fn has_toml_table(text: &str, table: &str) -> bool {
    let header = format!("[{table}]");
    text.lines().any(|line| line.trim() == header)
}

fn table_contains_key(text: &str, table: &str, key: &str) -> bool {
    table_lines(text, table).any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with(key)
            && trimmed
                .get(key.len()..)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

fn table_lines<'a>(text: &'a str, table: &str) -> impl Iterator<Item = &'a str> {
    let header = format!("[{table}]");
    let mut in_table = false;
    text.lines().filter(move |line| {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_table = trimmed == header;
            return false;
        }

        in_table
    })
}

fn insert_key_in_table(text: &str, table: &str, entry: &str) -> String {
    let header = format!("[{table}]");
    let mut result = String::new();
    let mut in_table = false;
    let mut inserted = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if in_table && trimmed.starts_with('[') && trimmed.ends_with(']') && !inserted {
            result.push_str(entry);
            result.push('\n');
            inserted = true;
        }

        result.push_str(line);
        result.push('\n');

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_table = trimmed == header;
        }
    }

    if in_table && !inserted {
        result.push_str(entry);
        result.push('\n');
    }

    result
}

pub fn default_config_text() -> String {
    r##"# no-sleep-till-done configuration

# Keep the background service's sleep override active while the menu app runs.
enabled = true

# Seconds the lid must remain closed before the controller sleeps the Mac.
delay_seconds = 60

# Seconds between lid-state checks in the controller.
poll_seconds = 1

# Seconds between menu bar refreshes.
menu_refresh_seconds = 5

[process_wait]
# When enabled, sleep is delayed after delay_seconds until all matching
# command lines disappear, then exit_grace_seconds is counted before sleep.
enabled = false
command_substrings = []
exit_grace_seconds = 300

[colors]
# Color for the small menu bar state dot.
# Supports #rrggbb hex colors and standard HTML/CSS color names.
ready = "green"
timer = "orange"
process_wait = "blue"
error = "red"
unknown = "gray"
"##
    .to_string()
}

impl AppConfig {
    fn normalized(mut self) -> Self {
        self.delay_seconds = self.delay_seconds.max(1);
        self.poll_seconds = self.poll_seconds.max(1);
        self.menu_refresh_seconds = self.menu_refresh_seconds.max(1);
        self.process_wait.exit_grace_seconds = self.process_wait.exit_grace_seconds.max(1);
        self.process_wait
            .command_substrings
            .retain(|value| !value.is_empty());
        self
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Toml(toml::de::Error),
    TomlEdit(toml_edit::TomlError),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(error) => write!(f, "{error}"),
            ConfigError::Toml(error) => write!(f, "{error}"),
            ConfigError::TomlEdit(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<io::Error> for ConfigError {
    fn from(error: io::Error) -> Self {
        ConfigError::Io(error)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(error: toml::de::Error) -> Self {
        ConfigError::Toml(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{copy_legacy_config, migrate_config_text, set_enabled, AppConfig};
    use std::fs;

    fn temp_config(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "no-sleep-till-done-{name}-{}-{}.toml",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    #[test]
    fn migration_adds_enabled_without_changing_existing_values() {
        let path = temp_config("migration");
        let text = "delay_seconds = 7\npoll_seconds = 2\nmenu_refresh_seconds = 3\n\n[process_wait]\nenabled = false\ncommand_substrings = []\nexit_grace_seconds = 4\n\n[colors]\nready = \"green\"\ntimer = \"orange\"\nprocess_wait = \"blue\"\nerror = \"red\"\nunknown = \"gray\"\n";
        let migrated = migrate_config_text(&path, text.into()).expect("migration should succeed");
        let config: AppConfig = toml::from_str(&migrated).expect("migrated TOML should parse");
        assert!(config.enabled);
        assert_eq!(config.delay_seconds, 7);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn enabled_writer_preserves_comments_and_other_settings() {
        let path = temp_config("enabled");
        fs::write(
            &path,
            "# keep this comment\nenabled = true\ndelay_seconds = 99\n",
        )
        .expect("fixture should be written");
        set_enabled(&path, false).expect("enabled should be updated");
        let updated = fs::read_to_string(&path).expect("fixture should be readable");
        assert!(updated.contains("# keep this comment"));
        assert!(updated.contains("enabled = false"));
        assert!(updated.contains("delay_seconds = 99"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rename_migration_copies_the_legacy_config() {
        let root = std::env::temp_dir().join(format!(
            "no-sleep-till-done-legacy-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let legacy = root.join(".config/lidsleep-delay/config.toml");
        let current = root.join(".config/no-sleep-till-done/config.toml");
        fs::create_dir_all(legacy.parent().expect("legacy parent should exist"))
            .expect("legacy directory should be created");
        fs::create_dir_all(current.parent().expect("current parent should exist"))
            .expect("current directory should be created");
        fs::write(&legacy, "delay_seconds = 42\n").expect("legacy config should be written");

        assert!(copy_legacy_config(&current, &root).expect("copy should succeed"));
        assert_eq!(
            fs::read_to_string(&current).expect("current config should be readable"),
            "delay_seconds = 42\n"
        );

        let _ = fs::remove_dir_all(root);
    }
}
