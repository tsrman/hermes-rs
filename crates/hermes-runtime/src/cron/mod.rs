//! Scheduled-job primitives: schedule parsing, next-run computation and the job store.

pub mod cancel;
pub mod doctor;
pub mod executions;
mod expr;
pub mod incidents;
pub mod manage;
pub mod monitor;
pub mod notepad;
pub mod scan;
pub mod schedule;
pub mod script;
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

/// Whether `cron.allow_agent_scheduling` is enabled in `config.yaml` (default false).
pub fn allow_agent_scheduling(home: &Path) -> bool {
    crate::config::read_config(&home.join("config.yaml"))
        .ok()
        .and_then(|cfg| {
            cfg.get("cron")
                .and_then(|c| c.get("allow_agent_scheduling"))
                .and_then(|v| v.as_bool())
        })
        .unwrap_or(false)
}

/// Load cron job skills into prompt parts matching `cron/scheduler_prompt.py::_load_cron_skill_parts`.
pub fn load_cron_skill_parts(home: &Path, cwd: &Path, skill_names: &[String]) -> Vec<String> {
    let settings = crate::config::read_config(&home.join("config.yaml"))
        .map(|cfg| crate::skills::SkillSettings::from_config(&cfg))
        .unwrap_or_default();
    let catalog = crate::skills::SkillCatalog::load(Some(home), cwd, &settings, false, true);
    let mut parts = Vec::new();
    let mut skipped = Vec::new();
    for raw in skill_names {
        let name = raw.trim().trim_start_matches('/');
        if name.is_empty() {
            continue;
        }
        let args = serde_json::json!({"name": name}).to_string();
        let loaded = catalog.view(&args, Some(home));
        if loaded["success"].as_bool() == Some(true) {
            if !parts.is_empty() {
                parts.push(String::new());
            }
            parts.push(format!(
                "[IMPORTANT: The user has invoked the \"{name}\" skill, indicating they want you to follow its instructions. The full skill content is loaded below.]"
            ));
            parts.push(String::new());
            parts.push(
                loaded["content"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            );
        } else {
            skipped.push(raw.trim().to_owned());
        }
    }
    if !skipped.is_empty() {
        let joined = skipped.join(", ");
        parts.insert(
            0,
            format!(
                "[IMPORTANT: The following skill(s) were listed for this job but could not be found and were skipped: {joined}. Start your response with a brief notice so the user is aware, e.g.: '⚠️ Skill(s) not found and skipped: {joined}']"
            ),
        );
    }
    parts
}
