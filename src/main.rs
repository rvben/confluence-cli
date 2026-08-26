mod cli;
mod config;
mod markdown;
mod model;
mod output;
mod provider;
mod schema;
mod sync;

use std::io::IsTerminal;

#[tokio::main]
async fn main() {
    if let Err(error) = cli::run().await {
        let machine = output::machine_readable_errors(
            std::env::args().skip(1),
            std::io::stdout().is_terminal(),
        );
        std::process::exit(output::render_anyhow(&error, machine));
    }
}
