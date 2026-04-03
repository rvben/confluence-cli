mod cli;
mod config;
mod markdown;
mod model;
mod output;
mod provider;
mod schema;
mod sync;

#[tokio::main]
async fn main() {
    if let Err(error) = cli::run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
