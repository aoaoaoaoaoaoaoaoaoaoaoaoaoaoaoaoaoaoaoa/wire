mod api;
mod appserver;
mod config;
mod identity;
mod relay;
mod server;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use uuid::Uuid;

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
    /// Relay live Mattermost advisories into live Codex sessions.
    Relay,
    /// Maintain public Codex session identities.
    Identity {
        #[command(subcommand)]
        command: IdentityCommand,
    },
    /// Operate the shared Codex app server.
    Codex {
        #[command(subcommand)]
        command: CodexCommand,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// Run the ordinary stdio MCP server.
    Serve,
}

#[derive(Subcommand)]
enum IdentityCommand {
    /// Consume one Codex `PostCompact` hook event from standard input.
    UpdateHook,
}

#[derive(Subcommand)]
enum CodexCommand {
    /// Print the MCP inventory attached to a Codex thread.
    McpStatus {
        #[arg(long)]
        session: Uuid,
    },
    /// Resume an idle thread in a fresh turn after reloading MCP servers.
    Handoff {
        #[arg(long)]
        session: Uuid,
        #[arg(long)]
        message: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match Box::pin(run()).await {
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
        Command::Relay => Box::pin(relay::serve(api::Mattermost::load()?)).await?,
        Command::Identity {
            command: IdentityCommand::UpdateHook,
        } => identity::update_hook(&api::Mattermost::load()?).await?,
        Command::Codex {
            command: CodexCommand::Handoff { session, message },
        } => appserver::handoff(session, &message).await?,
        Command::Codex {
            command: CodexCommand::McpStatus { session },
        } => println!(
            "{}",
            serde_json::to_string_pretty(&appserver::mcp_status(session).await?)?
        ),
    }
    Ok(())
}
