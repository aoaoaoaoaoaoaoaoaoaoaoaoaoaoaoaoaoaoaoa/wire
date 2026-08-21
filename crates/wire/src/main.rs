mod api;
mod server;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(author, version, about = "Mattermost chat for Codex agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the MCP transport.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// Run the ordinary stdio MCP server.
    Serve,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wire: {error}");
            ExitCode::from(70)
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Mcp {
            command: McpCommand::Serve,
        } => server::serve(api::Mattermost::load()?).await?,
    }
    Ok(())
}
