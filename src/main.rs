mod agent;
mod app;
mod auth;
mod config;
mod directives;
mod output;
mod providers;
mod sanitize;
mod tools;

use std::io::{self, IsTerminal, Read};

use anyhow::{Result, bail};
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
            let stdin = if !io::stdin().is_terminal() {
                let mut buf = String::new();
                io::stdin().read_to_string(&mut buf)?;
                if buf.trim().is_empty() {
                    None
                } else {
                    Some(buf)
                }
            } else {
                None
            };
            let args = if cli.prompt.is_empty() {
                None
            } else {
                Some(cli.prompt.join(" "))
            };
            let prompt = match (stdin, args) {
                (None, None) => None,
                (None, Some(a)) => Some(a),
                (Some(s), None) => Some(s),
                (Some(s), Some(a)) => Some(format!("{s}\n\n{a}")),
            };
            let app = app::App::new(config::Config::default());
            app.run(prompt).await
        }
    }
}

fn login(provider: &str) -> Result<()> {
    let Some(provider) = auth::AuthProvider::from_name(provider) else {
        bail!("unsupported provider login: {provider}");
    };

    println!("{} login for OneLoop", provider.display_name());
    println!();
    if provider == auth::AuthProvider::Anthropic {
        println!("Note: OneLoop uses Anthropic API-key authentication.");
        println!("It does not implement claude.ai subscription login.");
        println!();
    }

    let key = rpassword::prompt_password(format!("Enter {}: ", provider.env_var()))?;
    if key.trim().is_empty() {
        bail!("empty API key")
    }
    let path = auth::store_api_key(provider, key)?;
    println!(
        "Stored {} credentials at {}",
        provider.display_name(),
        path.display()
    );
    Ok(())
}
