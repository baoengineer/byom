use anyhow::Result;
use clap::{Parser, Subcommand};

/// Bring your own model: run Claude Code with every model you can sign in to.
#[derive(Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Print the guide Claude loads to use byom's models, then exit.
    #[arg(long)]
    skill: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Launch Claude Code with every signed-in model available (the default command).
    ///
    /// `byom run [model] [claude arguments]`: a first argument that is not a flag names the
    /// main model, such as openai/gpt-5.6-sol; the rest go to claude, as in
    /// `byom run --resume <id>`. Plain `byom --resume <id>` works too.
    Run {
        /// Optional model, then arguments for claude.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "MODEL] [CLAUDE ARGS"
        )]
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
    /// Stop the running bridge; refuses while sessions are using it, unless --force.
    Stop {
        /// Stop even though open sessions would lose their connection.
        #[arg(long)]
        force: bool,
    },
    /// Replace the running bridge with this version's, as after an upgrade.
    Restart,
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

/// `byom --resume <id>` and other claude flags mean `byom run --resume <id>`.
fn claude_flags_run(mut argv: Vec<String>) -> Vec<String> {
    let own = ["-h", "--help", "-V", "--version", "--skill"];
    if argv
        .get(1)
        .is_some_and(|a| a.starts_with('-') && !own.contains(&a.as_str()))
    {
        argv.insert(1, "run".into());
    }
    argv
}

/// A leading argument that is not a flag is the model; a leading `--` is dropped.
fn split_run(mut args: Vec<String>) -> (Option<String>, Vec<String>) {
    let model = match args.first() {
        Some(first) if !first.starts_with('-') => Some(args.remove(0)),
        _ => None,
    };
    if args.first().is_some_and(|a| a == "--") {
        args.remove(0);
    }
    (model, args)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse_from(claude_flags_run(std::env::args().collect()));
    byom::store::migrate()?;
    if cli.skill {
        print!("{}", byom::skill::GUIDE);
        return Ok(());
    }
    match cli.command.unwrap_or(Command::Run { args: Vec::new() }) {
        Command::Run { args } => {
            let (model, args) = split_run(args);
            byom::launch::run(model, args).await
        }
        Command::Login { provider } => byom::accounts::login(provider).await,
        Command::Logout { provider } => byom::accounts::logout(provider).await,
        Command::Auth | Command::AuthStatus => byom::accounts::print_status(),
        Command::Models {
            json,
            provider,
            refresh,
            all,
        } => byom::roster::print(json, provider, refresh, all).await,
        Command::Config { action: None } => {
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || byom::tui::run(handle)).await?
        }
        Command::Config {
            action: Some(action),
        } => match action {
            ConfigAction::Get { key } => byom::config::cli_get(&key),
            ConfigAction::Set { key, value } => byom::config::cli_set(&key, Some(&value)),
            ConfigAction::Unset { key } => byom::config::cli_set(&key, None),
            ConfigAction::Path => {
                println!("{}", byom::store::config_path()?.display());
                Ok(())
            }
        },
        Command::Doctor => byom::doctor::run().await,
        Command::Status => byom::launch::status().await,
        Command::Stop { force } => byom::launch::stop(force).await,
        Command::Restart => byom::launch::restart().await,
        Command::Logs { lines } => byom::roster::print_logs(lines),
        Command::Bridge { port } => byom::bridge::serve(port).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn claude_flags_start_a_session() {
        assert_eq!(
            claude_flags_run(argv(&["byom", "--resume", "x"])),
            argv(&["byom", "run", "--resume", "x"])
        );
        assert_eq!(
            claude_flags_run(argv(&["byom", "-c"])),
            argv(&["byom", "run", "-c"])
        );
        assert_eq!(
            claude_flags_run(argv(&["byom", "--version"])),
            argv(&["byom", "--version"])
        );
        assert_eq!(
            claude_flags_run(argv(&["byom", "models"])),
            argv(&["byom", "models"])
        );
    }

    #[test]
    fn run_takes_an_optional_model_then_claude_arguments() {
        assert_eq!(
            split_run(argv(&["kimi/k3", "--resume", "x"])),
            (Some("kimi/k3".into()), argv(&["--resume", "x"]))
        );
        assert_eq!(
            split_run(argv(&["--resume", "x"])),
            (None, argv(&["--resume", "x"]))
        );
        assert_eq!(
            split_run(argv(&["--", "--continue"])),
            (None, argv(&["--continue"]))
        );
        assert_eq!(
            split_run(argv(&["kimi/k3", "--", "-p", "hi"])),
            (Some("kimi/k3".into()), argv(&["-p", "hi"]))
        );
        assert_eq!(split_run(Vec::new()), (None, Vec::new()));
    }

    #[test]
    fn run_accepts_claude_flags_without_a_separator() {
        let cli = Cli::parse_from(argv(&[
            "byom",
            "run",
            "openai/gpt-5.6-sol",
            "--resume",
            "x",
            "-p",
        ]));
        let Some(Command::Run { args }) = cli.command else {
            panic!("expected run")
        };
        assert_eq!(args, argv(&["openai/gpt-5.6-sol", "--resume", "x", "-p"]));
    }
}
