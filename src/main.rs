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
        eprintln!(
            "{}",
            serde_json::json!({"error": {"kind": "invalid_input", "message": format!("{error:#}")}})
        );
        std::process::exit(1);
    }
}
