use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::Subcommand;
use hermes_runtime::storage::{migrate_session_copy, SessionStore, TranscriptOptions};

#[derive(Subcommand)]
pub enum Command {
    /// List recent sessions, newest first.
    List {
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
    },
    /// Print a stored transcript as JSON.
    Show {
        id: String,
        /// Include archived and rewound rows.
        #[arg(long)]
        all: bool,
        /// Show the last segment on the compression chain.
        #[arg(long)]
        follow_compression: bool,
    },
    /// Find stored messages with an SQLite FTS5 query and print matches as JSON.
    Search {
        query: String,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
    },
    /// Migrate a copy of the profile's schema 32 database with FTS layout 3.
    Migrate {
        /// New database file in an existing directory.
        #[arg(long, value_name = "PATH")]
        output: PathBuf,
    },
}

pub fn run(home: &Path, command: Command) -> Result<(), String> {
    let mut output = io::stdout().lock();
    match command {
        Command::Show {
            id,
            all,
            follow_compression,
        } => {
            let store = SessionStore::open_read_only(home).map_err(|error| error.to_string())?;
            let transcript = store
                .transcript(
                    &id,
                    TranscriptOptions {
                        include_inactive: all,
                        follow_compression,
                    },
                )
                .map_err(|error| error.to_string())?;
            serde_json::to_writer(&mut output, &transcript)
        }
        Command::List { limit } => {
            let store = SessionStore::open_read_only(home).map_err(|error| error.to_string())?;
            for session in store
                .recent(limit, None)
                .map_err(|error| error.to_string())?
            {
                let text = session
                    .title
                    .or(session.preview)
                    .unwrap_or_default()
                    .replace('\n', " ");
                writeln!(
                    output,
                    "{}  {:>4} msgs  {}  {}",
                    session.id,
                    session.message_count,
                    session.model.unwrap_or_default(),
                    text
                )
                .map_err(|error| error.to_string())?;
            }
            return Ok(());
        }
        Command::Search { query, limit } => {
            let store = SessionStore::open_read_only(home).map_err(|error| error.to_string())?;
            let hits = store
                .search(&query, limit)
                .map_err(|error| error.to_string())?;
            serde_json::to_writer(&mut output, &hits)
        }
        Command::Migrate {
            output: destination,
        } => {
            migrate_session_copy(home, &destination).map_err(|error| error.to_string())?;
            return writeln!(
                output,
                "Migrated session database to {}",
                destination.display()
            )
            .map_err(|error| error.to_string());
        }
    }
    .map_err(|error| error.to_string())?;
    writeln!(output).map_err(|error| error.to_string())
}
