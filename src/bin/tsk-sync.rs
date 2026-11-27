//! `tsk-sync`: the shared-workspace server. `tsk-sync migrate <config>` creates or upgrades
//! the database; `tsk-sync serve <config>` also listens.

use std::path::Path;
use std::process::ExitCode;

use tsk_tui::sync::{config::Config, db, server::SyncServer};

const USAGE: &str = "usage: tsk-sync migrate <config.toml>\n       tsk-sync serve <config.toml>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [command, config] = args.as_slice() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if !matches!(command.as_str(), "migrate" | "serve") {
        eprintln!("tsk-sync: unknown command `{command}`\n{USAGE}");
        return ExitCode::from(2);
    }
    match run(command, Path::new(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tsk-sync: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: &str, config: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(config)?;
    let connection = db::open(&config.database)?;
    match command {
        "migrate" => {
            println!(
                "tsk-sync: {} at schema {}",
                config.database.display(),
                db::SCHEMA_VERSION
            );
            Ok(())
        }
        "serve" => {
            let server = SyncServer::bind(config.listen, connection)?;
            println!("tsk-sync: listening on {}", server.local_addr());
            server.serve();
            Ok(())
        }
        _ => unreachable!("main checked the command"),
    }
}
