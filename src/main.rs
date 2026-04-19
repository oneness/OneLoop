mod app;
mod agent;
mod auth;
mod config;
mod ext;
mod output;
mod providers;
mod tools;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "oneloop")]
#[command(about = "A tiny, extensible coding agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg()]
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Login { provider: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Some(Command::Login { provider }) => login(&provider),
        None => {
            let prompt = if cli.prompt.is_empty() {
                None
            } else {
                Some(cli.prompt.join(" "))
            };
            let app = app::App::new(config::Config::default());
            app.run(prompt).await
        }
    }
}

fn login(provider: &str) -> Result<()> {
    match provider {
        "anthropic" => {
            println!("Anthropic login for oneloop");
            println!();
            println!("Note: oneloop uses Anthropic API-key authentication.");
            println!("It does not implement claude.ai subscription login.");
            println!();
            let key = rpassword::prompt_password("Enter ANTHROPIC_API_KEY: ")?;
            if key.trim().is_empty() {
                bail!("empty API key")
            }
            let path = auth::store_anthropic_api_key(key)?;
            println!("Stored Anthropic credentials at {}", path.display());
            Ok(())
        }
        "zai" => {
            println!("Z.AI login for oneloop");
            println!();
            let key = rpassword::prompt_password("Enter ZAI_API_KEY: ")?;
            if key.trim().is_empty() {
                bail!("empty API key")
            }
            let path = auth::store_zai_api_key(key)?;
            println!("Stored Z.AI credentials at {}", path.display());
            Ok(())
        }
        other => bail!("unsupported provider login: {other}"),
    }
}
