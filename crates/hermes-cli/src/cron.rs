use std::io::{self, Write};
use std::path::Path;

use clap::Subcommand;
use hermes_runtime::cron::schedule::{compute_next_run, parse_schedule};
use hermes_runtime::cron::{now_in, profile_timezone, store};
use serde_json::json;

#[derive(Subcommand)]
pub enum Command {
    /// Print the profile's scheduled jobs as JSON without changing the store.
    List {
        /// Include paused and finished jobs.
        #[arg(long)]
        all: bool,
    },
    /// Print the next run times of a schedule as JSON.
    Preview {
        schedule: String,
        /// Number of runs to compute.
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=50))]
        count: u32,
        /// Pin an IANA zone instead of the profile zone.
        #[arg(long, value_name = "ZONE")]
        timezone: Option<String>,
    },
}

pub fn run(home: Option<&Path>, command: Command) -> Result<(), String> {
    let mut output = io::stdout().lock();
    let zone = profile_timezone(home);
    let value = match command {
        Command::List { all } => {
            let jobs = store::read_jobs(home.ok_or("profile root is unavailable")?)?;
            let shown: Vec<_> = jobs
                .into_iter()
                .filter(|job| all || (job.enabled && job.state != "completed"))
                .collect();
            serde_json::to_value(shown).map_err(|error| error.to_string())?
        }
        Command::Preview {
            schedule,
            count,
            timezone,
        } => {
            let pinned = timezone
                .map(|name| {
                    name.parse()
                        .map_err(|_| format!("Unknown timezone {name:?}."))
                })
                .transpose()?;
            let now = now_in(zone);
            let parsed = parse_schedule(&schedule, now)?;
            let mut runs = Vec::new();
            let mut last: Option<String> = None;
            for _ in 0..count {
                let Some(next) = compute_next_run(&parsed, last.as_deref(), pinned, now) else {
                    break;
                };
                // One-shot schedules have a single run.
                let repeats = parsed.kind != "once";
                last = Some(next.clone());
                runs.push(next);
                if !repeats {
                    break;
                }
            }
            json!({"schedule": parsed, "timezone": zone.name(), "next_runs": runs})
        }
    };
    writeln!(output, "{value}").map_err(|error| error.to_string())
}
