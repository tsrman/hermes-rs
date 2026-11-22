//! Scheduled-job primitives: schedule parsing, next-run computation and the job store.

mod expr;
pub mod manage;
pub mod scan;
pub mod schedule;
pub mod store;
pub mod tick;

use std::path::Path;

pub use chrono::DateTime;
use chrono::Utc;
pub use chrono_tz::Tz;

/// The profile's zone: `HERMES_TIMEZONE`, then `timezone` in `config.yaml`, then the host zone.
/// An unknown name falls back to the next source, as the reference does.
pub fn profile_timezone(home: Option<&Path>) -> Tz {
    let configured = std::env::var("HERMES_TIMEZONE")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            let config = crate::config::read_config(&home?.join("config.yaml")).ok()?;
            let name = config.get("timezone")?.as_str()?.trim().to_owned();
            (!name.is_empty()).then_some(name)
        });
    configured
        .and_then(|name| name.parse().ok())
        .or_else(|| iana_time_zone::get_timezone().ok()?.parse().ok())
        .unwrap_or(Tz::UTC)
}

pub fn now_in(zone: Tz) -> DateTime<Tz> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    DateTime::<Utc>::from_timestamp(elapsed.as_secs() as i64, elapsed.subsec_nanos())
        .unwrap_or_default()
        .with_timezone(&zone)
}

/// The `cron` settings of the profile at `home`.
pub fn load_tick_settings(home: &Path) -> Result<tick::TickSettings, String> {
    let config = crate::config::read_config(&home.join("config.yaml"))?;
    Ok(tick::TickSettings::from_config(&config))
}
