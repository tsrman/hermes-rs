use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::Subcommand;
use hermes_runtime::profile::{check_profile, migrate_profile};

#[derive(Subcommand)]
pub enum Command {
    /// Print a profile inventory and migration readiness as JSON.
    Check,
    /// Prepare a verified copy of the profile and print the migration report.
    Migrate {
        /// New profile directory outside the source profile.
        #[arg(long, value_name = "PATH")]
        output: PathBuf,
    },
}

pub fn run(home: &Path, command: Command) -> Result<u8, String> {
    let report = match command {
        Command::Check => check_profile(home)?,
        Command::Migrate { output } => migrate_profile(home, &output)?,
    };
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &report).map_err(|error| error.to_string())?;
    writeln!(stdout).map_err(|error| error.to_string())?;
    Ok(u8::from(!report.ready))
}
