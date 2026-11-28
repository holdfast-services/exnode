//! `exnode` — inference exchange node agent.
//!
//!   exnode init --config node.toml --token qd_reg_…   create this node's key (mode 0600) and enroll it
//!                                                     (--token - reads the token from stdin)
//!   exnode init --key-out node.key                    only create the key; enroll on the first run
//!                                                     with EXNODE_ENROLL_TOKEN
//!   exnode run --config node.toml                     check the backend, register and serve
//!   exnode doctor --config node.toml                  print the checks, the key's mode, hub, backend, models
//!   exnode setup                                      ask the setup questions, write /etc/exnode/node.toml,
//!                                                     enroll and start the service (re-runnable; --yes and
//!                                                     flags for a scripted install)
//!
//! `run` and `init --token` refuse a vulnerable or exposed backend
//! ([`exnode::checks`]) and then exit with status 78.

use std::process::ExitCode;

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Parser, Subcommand};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use ex_common::identity::{generate_keypair, load_signing_key, node_id_for};
use exnode::{
    agent::Agent,
    checks::{Refused, EXIT_REFUSED},
    config::{display_url, NodeConfig},
    doctor,
    duty::{self, Control},
    enroll, keyfile, setup,
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
    /// Show when this machine serves the exchange, and how much of it, over the next seven days
    Schedule {
        #[arg(long, short, default_value = "node.toml")]
        config: String,
    },
    /// Stop taking jobs now (a job already running finishes); `exnode resume` ends the pause
    Pause {
        #[arg(long, short, default_value = "node.toml")]
        config: String,
        /// How long to pause, such as 30m, 2h or 1d [default: until `exnode resume`]
        #[arg(long = "for")]
        span: Option<String>,
    },
    /// End a pause: the node serves again as its schedule says
    Resume {
        #[arg(long, short, default_value = "node.toml")]
        config: String,
    },
    /// Configure this node: hub, engine, models, level of service, capacity and hours; then
    /// enroll and start it.  Asks on a terminal; with --yes it asks nothing and uses the flags.
    Setup(setup::SetupArgs),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cmd = Cli::parse().cmd;
    // setup talks to a person: only warnings and errors from the library's logs
    let quiet = matches!(cmd, Commands::Setup(_));
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| if quiet { "exnode=warn".into() } else { "exnode=info".into() }))
        .with(tracing_subscriber::fmt::layer())
        .init();

    match cli(cmd).await {
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
            let cfg = cfg.take().expect("--token loads a config");
            let done = enroll::enroll(cfg, &key, &token).await?;
            drop(token);
            println!("node_id:  {}", done.node_id);
            println!("status:   {} (tier {})", done.status, done.tier);
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
        Commands::Schedule { config } => {
            let cfg = load(&config)?;
            let schedule = cfg.duty_schedule()?;
            let now = chrono::Utc::now();
            print!("{}", duty::week_report(&schedule, cfg.max_concurrency, now));
            let control = Control::load(&Control::path_for(&cfg.key_file));
            let now_duty = duty::evaluate(&schedule, cfg.max_concurrency, &control, true, now);
            println!("\nNow: {}", now_duty.sentence(schedule.zone()));
        }
        Commands::Pause { config, span } => {
            let cfg = load(&config)?;
            let until = span.as_deref().map(duty::parse_span).transpose().map_err(anyhow::Error::msg)?
                .map(|d| ex_common::identity::unix_now() + d.as_secs());
            let path = Control::path_for(&cfg.key_file);
            Control { paused: true, until }.save(&path)
                .map_err(|e| anyhow::anyhow!("could not write {}: {e} (run this as root or as the exnode user)", path.display()))?;
            match span.as_deref() {
                Some(s) => println!("Paused for {s}. The agent notices within seconds; a job already running finishes."),
                None    => println!("Paused until `exnode resume`. The agent notices within seconds; a job already running finishes."),
            }
        }
        Commands::Resume { config } => {
            let cfg = load(&config)?;
            let path = Control::path_for(&cfg.key_file);
            Control::default().save(&path)
                .map_err(|e| anyhow::anyhow!("could not write {}: {e} (run this as root or as the exnode user)", path.display()))?;
            println!("Resumed. The agent serves as its schedule says within seconds.");
        }
        Commands::Doctor { config } => {
            return Ok(if doctor::run(&config).await? { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
        Commands::Setup(args) => {
            setup::run(args).await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}
