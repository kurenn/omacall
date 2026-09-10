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
        Some(Cmd::Status) => send(ipc::Request::Status).await,
        None => picker().await,
    }
}

/// Bare `omacall`: pick someone and call them.
///
/// The empty case matters as much as the populated one -- a first run with no
/// contacts should say what to do next, not print an empty list.
async fn picker() -> Result<()> {
    let path = identity::config_dir().join("contacts.toml");
    let contacts = omacall::contacts::Contacts::load(&path)?;

    if contacts.peers.is_empty() {
        println!("No contacts yet.\n");
        match ipc::request(&ipc::socket_path(), &ipc::Request::Status).await {
            Ok(ipc::Response::Status(s)) => {
                println!("Your id:\n  {}\n", s.endpoint_id);
            }
            _ => {
                let key = identity::load_or_create(&identity::key_path())?;
                println!("Your id:\n  {}\n", key.public());
                println!("(start the daemon for a ticket that can be dialled straight away)\n");
            }
        }
        println!("Send that to someone, get theirs, and save it:");
        println!("  omacall add NAME TICKET");
        return Ok(());
    }

    let names: Vec<&str> = contacts.peers.keys().map(String::as_str).collect();
    let chosen = choose(&names)?;
    let Some(name) = chosen else { return Ok(()) };
    println!("Calling {name}...");
    send(ipc::Request::Dial { name }).await
}

/// Offer a list. Uses `gum` to match the rest of Omarchy, and falls back to a
/// numbered prompt so the daemon stays usable over ssh and in a container.
fn choose(names: &[&str]) -> Result<Option<String>> {
    use std::io::{BufRead, Write};

    if which_gum() {
        let out = std::process::Command::new("gum")
            .args(["choose", "--header", "Call which machine?"])
            .args(names)
            .output()?;
        let pick = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return Ok((!pick.is_empty()).then_some(pick));
    }

    for (i, n) in names.iter().enumerate() {
        println!("  {}) {n}", i + 1);
    }
    print!("call which? ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let idx: usize = match line.trim().parse() {
        Ok(n) => n,
        Err(_) => return Ok(None),
    };
    Ok(names.get(idx.wrapping_sub(1)).map(|s| s.to_string()))
}

fn which_gum() -> bool {
    std::process::Command::new("gum")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
