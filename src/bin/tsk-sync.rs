//! `tsk-sync`: the shared-workspace server.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use tsk_tui::sync::tokens::{self, Scope};
use tsk_tui::sync::{authority::Authority, config::Config, db, server::SyncServer};

const USAGE: &str = "\
usage: tsk-sync migrate <config.toml>
       tsk-sync serve <config.toml>
       tsk-sync token add <config.toml> <actor> [--read]
       tsk-sync token list <config.toml>
       tsk-sync token revoke <config.toml> <actor>";

/// One parsed command line.
enum Command {
    Migrate,
    Serve,
    TokenAdd { actor: String, scope: Scope },
    TokenList,
    TokenRevoke { actor: String },
}

fn parse(args: &[String]) -> Option<(Command, PathBuf)> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let (command, config) = match words.as_slice() {
        ["migrate", config] => (Command::Migrate, config),
        ["serve", config] => (Command::Serve, config),
        ["token", "add", config, actor] => (
            Command::TokenAdd {
                actor: actor.to_string(),
                scope: Scope::Write,
            },
            config,
        ),
        ["token", "add", config, actor, "--read"] => (
            Command::TokenAdd {
                actor: actor.to_string(),
                scope: Scope::Read,
            },
            config,
        ),
        ["token", "list", config] => (Command::TokenList, config),
        ["token", "revoke", config, actor] => (
            Command::TokenRevoke {
                actor: actor.to_string(),
            },
            config,
        ),
        _ => return None,
    };
    Some((command, PathBuf::from(config)))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, config)) = parse(&args) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match run(command, &config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tsk-sync: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command, config: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(config)?;
    let connection = db::open(&config.database)?;
    match command {
        Command::Migrate => println!(
            "tsk-sync: {} at schema {}",
            config.database.display(),
            db::SCHEMA_VERSION
        ),
        Command::Serve => {
            let mut server = SyncServer::bind(config.listen, Authority::load(connection)?)?;
            println!("tsk-sync: listening on {}", server.local_addr());
            server.serve()?;
        }
        Command::TokenAdd { actor, scope } => {
            let token = tokens::add(&connection, &actor, scope)?;
            eprintln!("tsk-sync: a {scope} token for {actor}; it is shown only now");
            println!("{token}");
        }
        Command::TokenList => {
            for (actor, scope, count) in tokens::list(&connection)? {
                println!("{actor}\t{scope}\t{count}");
            }
        }
        Command::TokenRevoke { actor } => {
            let revoked = tokens::revoke(&connection, &actor)?;
            if revoked == 0 {
                return Err(format!("{actor} has no tokens").into());
            }
            println!("tsk-sync: revoked {revoked} token(s) for {actor}");
        }
    }
    Ok(())
}
