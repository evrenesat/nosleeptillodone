pub mod config;
pub mod system;

pub const APP_ACTIVE_LEASE_PATH: &str = "/tmp/com.evren.nosleeptilldone.active";
pub const CONTROLLER_RESET_REQUEST_PATH: &str = "/tmp/com.evren.nosleeptilldone.reset";
pub const LEASE_TIMEOUT_SECONDS: u64 = 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeaseRecord {
    pub enabled: bool,
    pub reload_generation: u64,
}

impl LeaseRecord {
    pub fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()?.trim() != "active" {
            return None;
        }

        let mut record = Self {
            enabled: true,
            reload_generation: 0,
        };
        for line in lines {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "enabled" => record.enabled = value.trim() == "true",
                "reload_generation" => {
                    record.reload_generation = value.trim().parse().ok()?;
                }
                _ => {}
            }
        }
        Some(record)
    }

    pub fn serialize(self) -> String {
        format!(
            "active\nenabled={}\nreload_generation={}\n",
            self.enabled, self.reload_generation
        )
    }
}

#[cfg(test)]
mod tests {
    use super::LeaseRecord;

    #[test]
    fn legacy_lease_remains_enabled() {
        assert_eq!(
            LeaseRecord::parse("active\n"),
            Some(LeaseRecord {
                enabled: true,
                reload_generation: 0,
            })
        );
    }

    #[test]
    fn lease_round_trip_preserves_control_state() {
        let record = LeaseRecord {
            enabled: false,
            reload_generation: 42,
        };
        assert_eq!(LeaseRecord::parse(&record.serialize()), Some(record));
    }
}
