mod agent;
mod app;
mod auth;
mod catalog;
mod config;
mod models;
mod output;
mod providers;
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
        Some(Command::Login { provider }) => login(&provider).await,
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

async fn login(provider: &str) -> Result<()> {
    // A subscription has no key to type: it is signed in to, in a browser.
    if provider == auth::codex::PROVIDER_NAME {
        println!("ChatGPT (Plus/Pro) login for OneLoop");
        println!();
        let grant = auth::codex::login().await?;
        let path = auth::store(provider, &auth::Credential::Oauth(grant))?;
        println!("Stored ChatGPT credentials at {}", path.display());
        return Ok(());
    }

    let Some(login) = auth::api_key_login(provider) else {
        bail!("unsupported provider login: {provider}");
    };

    println!("{} login for OneLoop", login.display_name);
    println!();

    let key = rpassword::prompt_password(format!("Enter {}: ", login.env_var))?;
    if key.trim().is_empty() {
        bail!("empty API key")
    }
    let path = auth::store(login.provider, &auth::Credential::ApiKey { key })?;
    println!(
        "Stored {} credentials at {}",
        login.display_name,
        path.display()
    );
    Ok(())
}
