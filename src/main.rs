use anyhow::Result;
use clap::{Parser, Subcommand};

/// Bring your own Claude: run Claude Code with every model you can sign in to.
#[derive(Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Print the guide Claude loads to use byoclaude's models, then exit.
    #[arg(long)]
    skill: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Launch Claude Code with every signed-in model available (the default command).
    Run {
        /// Main model for this session, such as openai/gpt-5.6-sol or claude-opus-5-5.
        model: Option<String>,
        /// Arguments passed to claude, after `--`.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Sign in to a provider: ChatGPT, or save an API key.
    Login {
        /// Provider ID; omit to choose from a list.
        provider: Option<String>,
    },
    /// Remove a provider's saved sign-in or key.
    Logout { provider: String },
    /// Show each provider's sign-in status.
    Auth,
    /// List available models.
    Models {
        /// Machine-readable roster with status, for scripts and Claude.
        #[arg(long)]
        json: bool,
        /// Only this provider's models.
        #[arg(long)]
        provider: Option<String>,
        /// Re-fetch model lists from providers first.
        #[arg(long)]
        refresh: bool,
        /// Include models hidden from /model.
        #[arg(long)]
        all: bool,
    },
    /// Edit settings in a terminal UI, or get/set them from scripts.
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Check Claude Code, sign-ins, the relay, the bridge and each provider.
    Doctor,
    /// Report whether the bridge is running.
    Status,
    /// Stop the running bridge.
    Stop,
    /// Show recent requests from the bridge log.
    Logs {
        /// Number of requests to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
    #[command(hide = true, name = "auth-status")]
    AuthStatus,
    /// Internal loopback bridge process.
    #[command(hide = true)]
    Bridge {
        #[arg(long, default_value_t = 47391)]
        port: u16,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print a setting, such as `model` or `providers.zai.models`.
    Get { key: String },
    /// Set a setting; lists take comma-separated values.
    Set { key: String, value: String },
    /// Remove a setting, returning it to its default.
    Unset { key: String },
    /// Print the config file path.
    Path,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    byoclaude::store::migrate()?;
    if cli.skill {
        print!("{}", byoclaude::skill::GUIDE);
        return Ok(());
    }
    match cli.command.unwrap_or(Command::Run {
        model: None,
        args: Vec::new(),
    }) {
        Command::Run { model, args } => byoclaude::launch::run(model, args).await,
        Command::Login { provider } => byoclaude::accounts::login(provider).await,
        Command::Logout { provider } => byoclaude::accounts::logout(provider).await,
        Command::Auth | Command::AuthStatus => byoclaude::accounts::print_status(),
        Command::Models {
            json,
            provider,
            refresh,
            all,
        } => byoclaude::roster::print(json, provider, refresh, all).await,
        Command::Config { action: None } => {
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || byoclaude::tui::run(handle)).await?
        }
        Command::Config {
            action: Some(action),
        } => match action {
            ConfigAction::Get { key } => byoclaude::config::cli_get(&key),
            ConfigAction::Set { key, value } => byoclaude::config::cli_set(&key, Some(&value)),
            ConfigAction::Unset { key } => byoclaude::config::cli_set(&key, None),
            ConfigAction::Path => {
                println!("{}", byoclaude::store::config_path()?.display());
                Ok(())
            }
        },
        Command::Doctor => byoclaude::doctor::run().await,
        Command::Status => byoclaude::launch::status().await,
        Command::Stop => byoclaude::launch::stop().await,
        Command::Logs { lines } => byoclaude::roster::print_logs(lines),
        Command::Bridge { port } => byoclaude::bridge::serve(port).await,
    }
}
