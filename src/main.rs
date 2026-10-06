mod cli;
mod config;
mod credentials;
mod markdown;
mod model;
mod output;
mod provider;
mod schema;
mod storage;
mod sync;
mod terminal;
mod tui;

use std::io::IsTerminal;

#[tokio::main]
async fn main() {
    if let Err(error) = cli::run().await {
        let machine = output::machine_readable_errors(
            std::env::args_os()
                .skip(1)
                .map(|arg| arg.to_string_lossy().into_owned()),
            std::io::stdout().is_terminal(),
        );
        std::process::exit(output::render_anyhow(&error, machine));
    }
}
