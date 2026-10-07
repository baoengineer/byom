use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in with ChatGPT to use your Plus/Pro plan (no API key).
    Login {
        #[arg(default_value = "openai")]
        provider: String,
    },
    /// List models available to the signed-in ChatGPT account.
    Models,
    /// Show local ChatGPT authentication status without a network request.
    AuthStatus,
    /// Launch Claude Code with a locally configured provider route.
    Run {
        model: Option<String>,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Report local bridge readiness.
    Status,
    /// Stop the running bridge.
    Stop,
    /// Internal loopback bridge process.
    #[command(hide = true)]
    Bridge {
        #[arg(long, default_value_t = 47391)]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Login { provider } => {
            anyhow::ensure!(
                provider == "openai" || provider == "chatgpt",
                "Supported login provider: openai"
            );
            byoclaude::auth::login().await
        }
        Command::Models => byoclaude::catalog::print().await,
        Command::AuthStatus => byoclaude::auth::status(),
        Command::Run { model, args } => byoclaude::launch::run(model, args).await,
        Command::Status => byoclaude::launch::status().await,
        Command::Stop => byoclaude::launch::stop().await,
        Command::Bridge { port } => byoclaude::bridge::serve(port).await,
    }
}
