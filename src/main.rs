//! omacall — peer-to-peer video calls between machines.
//!
//! Every subcommand except `daemon` is a thin client over the control socket.
//! The daemon owns the identity key, the endpoint and the media ports, so
//! nothing else can act on its own.

use anyhow::Result;
use clap::{Parser, Subcommand};
use omacall::{daemon, identity, ipc};

#[derive(Parser)]
#[command(name = "omacall", about = "Peer-to-peer video calls. No account, no server.")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the resident daemon. Normally started by omacall.service.
    Daemon {
        /// Base UDP port for the loopback media tunnel.
        #[arg(long, env = "OMACALL_PORT_BASE", default_value_t = 5000)]
        port_base: u16,
    },
    /// Print this machine's ticket, to send to someone who wants to call you.
    Id,
    /// Save someone's ticket under a name.
    Add { name: String, ticket: String },
    /// Call a saved contact.
    Call { name: String },
    /// End the current call.
    Hangup,
    /// What the daemon is doing, as JSON.
    Status,
    /// Check this machine for the things that fail silently.
    Doctor,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("OMACALL_LOG")
                .unwrap_or_else(|_| "omacall=info".into()),
        )
        .init();

    match Cli::parse().command {
        Some(Cmd::Daemon { port_base }) => daemon::run(port_base).await,
        Some(Cmd::Id) => {
            // Reads the key directly: printing your own ticket should work
            // whether or not the daemon happens to be running.
            let key = identity::load_or_create(&identity::key_path())?;
            println!("{}", key.public());
            eprintln!("\nThe id above is stable. For a ticket that can be dialled immediately,");
            eprintln!("run this while the daemon is up:  omacall status");
            Ok(())
        }
        Some(Cmd::Add { name, ticket }) => {
            let addr = identity::decode_ticket(&ticket)?;
            let path = identity::config_dir().join("contacts.toml");
            let mut contacts = omacall::contacts::Contacts::load(&path)?;
            contacts.add(&name, &addr);
            contacts.save(&path)?;
            println!("saved {name} ({})", addr.id);
            Ok(())
        }
        Some(Cmd::Call { name }) => send(ipc::Request::Dial { name }).await,
        Some(Cmd::Hangup) => send(ipc::Request::Hangup).await,
        Some(Cmd::Doctor) => {
            let report = omacall::doctor::run().await;
            let bad = report.iter().filter(|h| h.is_bad()).count();
            for line in &report {
                println!("{line}");
            }
            if bad > 0 {
                anyhow::bail!("{bad} problem(s) would stop a call working");
            }
            Ok(())
        }
        Some(Cmd::Status) | None => send(ipc::Request::Status).await,
    }
}

async fn send(req: ipc::Request) -> Result<()> {
    match ipc::request(&ipc::socket_path(), &req).await? {
        ipc::Response::Ok => Ok(()),
        ipc::Response::Status(s) => {
            println!("{}", serde_json::to_string_pretty(&s)?);
            Ok(())
        }
        ipc::Response::Error { message } => anyhow::bail!(message),
    }
}
