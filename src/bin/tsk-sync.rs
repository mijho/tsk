use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let config_path = match config_path() {
        Ok(path) => path,
        Err(message) => {
            eprintln!("tsk-sync: {message}");
            std::process::exit(2);
        }
    };

    if let Err(error) = tsk_tui::sync::run(&config_path).await {
        eprintln!("tsk-sync: {error}");
        std::process::exit(1);
    }
}

fn config_path() -> Result<PathBuf, String> {
    let mut args = std::env::args_os().skip(1);
    match (args.next(), args.next()) {
        (Some(path), None) => Ok(path.into()),
        (None, None) => std::env::var_os("TSK_SYNC_CONFIG")
            .map(PathBuf::from)
            .ok_or_else(|| "usage: tsk-sync CONFIG.json (or set TSK_SYNC_CONFIG)".to_string()),
        _ => Err("usage: tsk-sync CONFIG.json (or set TSK_SYNC_CONFIG)".to_string()),
    }
}
