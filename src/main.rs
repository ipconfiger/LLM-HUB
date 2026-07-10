//! llm-hub — a local LLM proxy that routes requests to multiple backends by model name.
//!
//! Usage:
//!   llm-hub --serve          Start the HTTP proxy server (agents point at http://127.0.0.1:3000/v1)
//!   llm-hub --admin          Start the TUI config editor
//!   llm-hub --serve --bind 127.0.0.1:4000

mod admin;
mod config;
mod error;
mod proxy;
mod server;
mod worker;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Command-line flags.
#[derive(Parser, Debug)]
#[command(
    name = "llm-hub",
    version,
    about = "A local LLM proxy that routes requests to multiple backends by model name"
)]
struct Cli {
    /// Start the HTTP proxy server.
    #[arg(long, conflicts_with = "admin")]
    serve: bool,

    /// Start the TUI admin interface to edit configuration.
    #[arg(long, conflicts_with = "serve")]
    admin: bool,

    /// Address to bind the proxy server to (only meaningful with --serve).
    #[arg(long, default_value = "127.0.0.1:3000", requires = "serve")]
    bind: String,

    /// Initialize the config file with a sample backend and exit.
    #[arg(long)]
    init: bool,
}

fn main() -> ExitCode {
    // Initialize structured logging. Respect RUST_LOG if set, default to "info".
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    if cli.init {
        return run_init();
    }

    // Default behavior when no mode flag is given: show help.
    if !(cli.serve || cli.admin) {
        print_help();
        return ExitCode::SUCCESS;
    }

    // A tokio runtime is required for both modes.
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = if cli.serve {
        rt.block_on(serve(cli.bind))
    } else {
        rt.block_on(admin::run())
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parse the bind address and start the proxy server.
async fn serve(bind: String) -> error::Result<()> {
    let addr: std::net::SocketAddr = bind
        .parse()
        .map_err(|e| error::Error::Config(format!("invalid --bind address '{bind}': {e}")))?;
    server::serve(addr).await
}

/// Seed the config file with a sample backend if it does not already exist.
fn run_init() -> ExitCode {
    match config::Config::path() {
        Some(path) => {
            if path.exists() {
                println!("config already exists at {}", path.display());
                return ExitCode::SUCCESS;
            }
            let cfg = config::Config::sample();
            match cfg.save() {
                Ok(()) => {
                    println!("wrote sample config to {}", path.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("failed to write config: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        None => {
            eprintln!("could not determine the configuration directory for this platform");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    eprintln!(
        "llm-hub — a local LLM proxy\n\n\
         Usage:\n  \
         llm-hub --serve [--bind 127.0.0.1:3000]   Start the proxy server\n  \
         llm-hub --admin                            Edit configuration in the TUI\n  \
         llm-hub --init                             Write a sample config and exit\n\n\
         Agents should use http://127.0.0.1:3000/v1 as their base URL.\n"
    );
}
