//! `exnode` — inference exchange node agent.
//!
//!   exnode init --config node.toml --token qd_reg_…   create this node's key (mode 0600) and enroll it
//!                                                     (--token - reads the token from stdin)
//!   exnode init --key-out node.key                    only create the key; enroll on the first run
//!                                                     with EXNODE_ENROLL_TOKEN
//!   exnode run --config node.toml                     check the backend, register and serve
//!   exnode doctor --config node.toml                  print the checks, the key's mode, hub, backend, models
//!
//! `run` and `init --token` refuse a vulnerable or exposed backend
//! ([`exnode::checks`]) and then exit with status 78.

use std::process::ExitCode;

use anyhow::{bail, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Parser, Subcommand};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use ex_common::identity::{generate_keypair, load_signing_key, node_id_for};
use exnode::{
    agent::Agent,
    checks::{Refused, EXIT_REFUSED},
    config::{display_url, NodeConfig},
    doctor, keyfile,
};

#[derive(Parser)]
#[command(name = "exnode", version, about = "Inference Exchange Node Agent")]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the node agent
    Run {
        #[arg(long, short, default_value = "node.toml")]
        config: String,
    },
    /// Create this node's Ed25519 key (mode 0600) and, with --token, enroll it with the hub
    Init {
        /// Where to write the key [default: key_file from --config, else node.key]
        #[arg(long)]
        key_out: Option<String>,
        /// The node config to enroll with [default with --token: node.toml]
        #[arg(long, short)]
        config: Option<String>,
        /// One-time enrollment token (qd_reg_…) from the console.  Enrolls now; the token is
        /// never written to disk.  `-` reads it from stdin, keeping it out of the process list.
        #[arg(long, env = "EXNODE_ENROLL_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// Print the safety checks, the key file's mode, the hub, the backend and the models' hashes
    Doctor {
        #[arg(long, short, default_value = "node.toml")]
        config: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "exnode=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    match cli(Cli::parse().cmd).await {
        Ok(code) => code,
        Err(e) if e.downcast_ref::<Refused>().is_some() => {
            eprintln!("exnode: {e}");
            ExitCode::from(EXIT_REFUSED)
        }
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}

fn load(path: &str) -> Result<NodeConfig> {
    NodeConfig::load(path).map_err(|e| anyhow::anyhow!("config error: {e}"))
}

async fn cli(cmd: Commands) -> Result<ExitCode> {
    match cmd {
        Commands::Init { key_out, config, token } => {
            let config = config.or_else(|| token.as_ref().map(|_| "node.toml".to_string()));
            let mut cfg = config.as_deref().map(load).transpose()?;
            let key = key_out.or_else(|| cfg.as_ref().map(|c| c.key_file.clone())).unwrap_or_else(|| "node.key".into());

            let Some(token) = token else {
                let (priv_b64, pub_b64) = generate_keypair();
                keyfile::write_key_file(&key, &priv_b64)?;
                println!("node_id:    {}", node_id_for(&pub_b64)?);
                println!("public_key: {pub_b64}");
                println!("key file:   {key} (mode 0600; never copy it off this machine)");
                return Ok(ExitCode::SUCCESS);
            };
            let token = if token == "-" {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line
            } else {
                token
            };
            let token = token.trim().to_string();
            if !token.starts_with("qd_reg_") {
                bail!("that is not an enrollment token (they start with qd_reg_)");
            }
            let mut cfg = cfg.take().expect("--token loads a config");
            cfg.key_file = key.clone();
            // the same key again if enrollment is being retried
            if !std::path::Path::new(&key).exists() {
                keyfile::write_key_file(&key, &generate_keypair().0)?;
            }
            let sk      = load_signing_key(&keyfile::read_key_file(&key)?)?;
            let pub_b64 = STANDARD.encode(sk.verifying_key().as_bytes());
            let node_id = node_id_for(&pub_b64)?;

            let agent = Agent::new(cfg, node_id.clone(), sk).await?;
            agent.preflight().await?; // never enroll a node that would refuse to serve
            let resp = agent.register_with(&pub_b64, Some(&token)).await?;
            println!("node_id:  {node_id}");
            println!("status:   {} (tier {})", resp["status"].as_str().unwrap_or("?"), resp["tier"].as_str().unwrap_or("?"));
            println!("key file: {key} (mode 0600; never copy it off this machine)");
            println!("enrolled. Start the agent: sudo systemctl start exnode   (or: exnode run --config {})",
                     config.as_deref().unwrap_or("node.toml"));
        }
        Commands::Run { config } => {
            let cfg     = load(&config)?;
            let sk      = load_signing_key(&keyfile::read_key_file(&cfg.key_file)?)?;
            let pub_b64 = STANDARD.encode(sk.verifying_key().as_bytes());
            let node_id = node_id_for(&pub_b64)?;
            tracing::info!(%node_id, hub = %display_url(&cfg.hub_url), backend = %cfg.backend,
                           version = env!("CARGO_PKG_VERSION"), "starting");
            Agent::new(cfg, node_id, sk).await?.with_safety_checks().run(pub_b64).await?;
        }
        Commands::Doctor { config } => {
            return Ok(if doctor::run(&config).await? { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
    }
    Ok(ExitCode::SUCCESS)
}
