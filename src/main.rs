use std::env::args;

use brc20_command_minter::{Config, reset, start};

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_target(false)
        .with_max_level(match std::env::var("RUST_LOG") {
            Ok(level) => level.parse().unwrap_or(tracing::Level::INFO),
            Err(_) => tracing::Level::INFO,
        })
        .init();
    if args().any(|arg| arg == "--reset") {
        reset(&Config::default()).await;
    } else {
        start(&Config::default()).await;
    }
}
