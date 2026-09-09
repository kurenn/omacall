//! The resident daemon: identity, endpoint, call state, and everything the CLI
//! and plugin ask it to do.
//!
//! Stage 1 scope: 1:1 calls, with v1's bash pipeline still doing the media over
//! a loopback tunnel. `CallState` already handles the mesh; the daemon grows
//! into it in Stage 2.

use std::{collections::HashMap, time::Duration};

use anyhow::{Context, Result};
use iroh::{
    endpoint::{presets, Connection, QuicTransportConfig, RecvStream, SendStream},
    Endpoint, EndpointId,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::{mpsc, oneshot},
};

use crate::{
    contacts::Contacts,
    identity, ipc,
    proto::{self, Action, CallState, Codec, Event, Msg, RING_TIMEOUT_SECS},
    ring::{self, Ring},
    tunnel::Tunnel,
};

pub const ALPN: &[u8] = b"omacall/0";

/// See PLAN.md R1: with quinn's default-sized buffer, congestion shows up as
/// seconds of stale video before it shows up as drops. Small buffer, drop
/// oldest, which is what unreliable media wants.
const DATAGRAM_SEND_BUFFER: usize = 32 * 1024;

enum Ev {
    Ipc(ipc::Request, oneshot::Sender<ipc::Response>),
    Incoming(Connection, SendStream, RecvStream),
    Control { from: EndpointId, msg: Msg },
    Gone(EndpointId),
    Verdict(bool),
    RingTimeout,
}

struct PeerConn {
    conn: Connection,
    send: SendStream,
    tunnel: Option<Tunnel>,
    /// The bash media pipeline. Owned, in its own process group, killed on
    /// drop -- v1 orphaned these and they transmitted forever.
    child: Option<Child>,
}

pub async fn run(port_base: u16) -> Result<()> {
    let key_path = identity::key_path();
    let secret = identity::load_or_create(&key_path)?;
    if let Err(e) = identity::check_permissions(&key_path) {
        tracing::warn!("{e}");
    }
    let me = secret.public();

    let contacts_path = identity::config_dir().join("contacts.toml");
    let contacts = Contacts::load(&contacts_path)?;

    if let Err(e) = ring::has_display() {
        tracing::warn!("{e}");
    }

    let transport = QuicTransportConfig::builder()
        .datagram_send_buffer_size(DATAGRAM_SEND_BUFFER)
        .keep_alive_interval(Duration::from_secs(5))
        .build();

    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec()])
        .transport_config(transport)
        .bind()
        .await
        .context("binding the iroh endpoint")?;

    println!("omacall daemon");
    println!("  id:     {me}");
    println!("  ticket: {}", identity::encode_ticket(&endpoint.addr())?);

    let (ev_tx, mut ev_rx) = mpsc::channel::<Ev>(64);

    let sock = ipc::socket_path();
    let listener = ipc::bind(&sock).await?;
    println!("  socket: {}", sock.display());
    {
        let (itx, mut irx) = mpsc::channel::<ipc::Envelope>(16);
        tokio::spawn(ipc::serve(listener, itx));
        let ev_tx = ev_tx.clone();
        tokio::spawn(async move {
            while let Some((req, reply)) = irx.recv().await {
                if ev_tx.send(Ev::Ipc(req, reply)).await.is_err() {
                    return;
                }
            }
        });
    }

    {
        let ep = endpoint.clone();
        let ev_tx = ev_tx.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let ev_tx = ev_tx.clone();
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    // The dialer opens the control stream, so the accepting
                    // side waits for it rather than racing to open its own.
                    let Ok((send, recv)) = conn.accept_bi().await else { return };
                    let _ = ev_tx.send(Ev::Incoming(conn, send, recv)).await;
                });
            }
        });
    }

    let mut state = CallState::new(me, hostname(), vec![Codec::Vp8]);
    state.ring_unknown = true; // Stage 1: LAN-first, tighten in Stage 3
    let mut peers: HashMap<EndpointId, PeerConn> = HashMap::new();
    let mut ring: Option<Ring> = None;
    // Reloaded whenever it matters: `omacall add` writes the file directly, so
    // a daemon holding a startup snapshot would never see a new contact.
    let mut contacts = contacts;

    while let Some(ev) = ev_rx.recv().await {
        let actions = match ev {
            Ev::Ipc(ipc::Request::Dial { name }, reply) => {
                if let Ok(fresh) = Contacts::load(&contacts_path) {
                    contacts = fresh;
                }
                match dial(&endpoint, &contacts, &name, &mut peers, &ev_tx).await {
                    Ok(id) => {
                        let _ = reply.send(ipc::Response::Ok);
                        state.handle(Event::Dial { id, name })
                    }
                    Err(e) => {
                        let _ = reply.send(ipc::Response::Error { message: e.to_string() });
                        vec![]
                    }
                }
            }
            Ev::Ipc(req, reply) => {
                let (resp, evs) = handle_ipc(req, &state, &peers, &contacts, me).await;
                let _ = reply.send(resp);
                let mut acc = Vec::new();
                for e in evs {
                    acc.extend(state.handle(e));
                }
                acc
            }
            Ev::Incoming(conn, send, mut recv) => {
                if let Ok(fresh) = Contacts::load(&contacts_path) {
                    contacts = fresh;
                }
                let from = conn.remote_id();
                let msg = match read_msg(&mut recv).await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!("no opening message from {from}: {e}");
                        continue;
                    }
                };
                peers.insert(from, PeerConn { conn: conn.clone(), send, tunnel: None, child: None });
                spawn_reader(from, recv, conn, ev_tx.clone());
                state.handle(Event::Rx { from, msg })
            }
            Ev::Control { from, msg } => state.handle(Event::Rx { from, msg }),
            Ev::Gone(id) => {
                peers.remove(&id);
                state.handle(Event::ConnLost(id))
            }
            Ev::Verdict(answered) => {
                ring = None;
                state.handle(Event::UserVerdict(answered))
            }
            Ev::RingTimeout => {
                ring = None;
                state.handle(Event::RingTimeout)
            }
        };

        for action in actions {
            apply(action, &mut peers, &mut ring, &ev_tx, port_base, &contacts).await;
        }
    }
    Ok(())
}

async fn handle_ipc(
    req: ipc::Request,
    state: &CallState,
    peers: &HashMap<EndpointId, PeerConn>,
    contacts: &Contacts,
    me: EndpointId,
) -> (ipc::Response, Vec<Event>) {
    match req {
        ipc::Request::Status => {
            let mut status = ipc::Status {
                state: state.state_name().into(),
                endpoint_id: me.to_string(),
                ..Default::default()
            };
            for (id, p) in peers {
                for path in p.conn.paths().iter() {
                    if path.is_relay() {
                        status.paths_relay += 1;
                    } else if path.is_ip() {
                        status.paths_direct += 1;
                    }
                }
                status.peers.push(ipc::PeerStatus {
                    name: contacts.name_of(id).unwrap_or("unknown").to_string(),
                    id: id.to_string(),
                    ..Default::default()
                });
            }
            (ipc::Response::Status(status), vec![])
        }
        ipc::Request::Hangup => (ipc::Response::Ok, vec![Event::Hangup]),
        ipc::Request::Invite { .. } => (
            ipc::Response::Error { message: "invite arrives in Stage 2".into() },
            vec![],
        ),
        // Dial is handled in the main loop: it has to register the peer.
        ipc::Request::Dial { .. } => (
            ipc::Response::Error { message: "unreachable".into() },
            vec![],
        ),
    }
}

/// Connect, open the control stream, and register the peer.
///
/// The dialer opens the stream; the accepting side waits for it. Both then read
/// from it, so there is exactly one control channel per pair.
async fn dial(
    endpoint: &Endpoint,
    contacts: &Contacts,
    name: &str,
    peers: &mut HashMap<EndpointId, PeerConn>,
    ev_tx: &mpsc::Sender<Ev>,
) -> Result<EndpointId> {
    let contact = contacts.lookup(name).with_context(|| {
        format!("no contact called {name:?}. Add one with: omacall add {name} <ticket>")
    })?;
    // Dial the cached addresses alongside the id: discovery takes ~45s to
    // publish after a daemon starts, so a bare-id dial fails until then.
    let conn = endpoint
        .connect(contact.addr(), ALPN)
        .await
        .with_context(|| format!("could not reach {name}"))?;
    let (send, recv) = conn.open_bi().await.context("opening the control stream")?;
    let id = conn.remote_id();
    peers.insert(id, PeerConn { conn: conn.clone(), send, tunnel: None, child: None });
    spawn_reader(id, recv, conn, ev_tx.clone());
    Ok(id)
}

async fn apply(
    action: Action,
    peers: &mut HashMap<EndpointId, PeerConn>,
    ring: &mut Option<Ring>,
    ev_tx: &mpsc::Sender<Ev>,
    port_base: u16,
    contacts: &Contacts,
) {
    match action {
        Action::Send { to, msg } => {
            if let Some(p) = peers.get_mut(&to) {
                if let Err(e) = write_msg(&mut p.send, &msg).await {
                    tracing::debug!("sending to {to} failed: {e}");
                }
            }
        }
        Action::StartRing { from } => {
            let (r, rx) = Ring::start(&from, Duration::from_secs(RING_TIMEOUT_SECS));
            *ring = Some(r);
            let tx = ev_tx.clone();
            tokio::spawn(async move {
                match rx.await {
                    Ok(answered) => {
                        let _ = tx.send(Ev::Verdict(answered)).await;
                    }
                    Err(_) => {
                        let _ = tx.send(Ev::RingTimeout).await;
                    }
                }
            });
        }
        Action::StopRing => *ring = None,
        Action::StartMedia { peer, .. } => {
            let Some(p) = peers.get_mut(&peer) else { return };
            match Tunnel::start(p.conn.clone(), port_base).await {
                Ok(t) => p.tunnel = Some(t),
                Err(e) => {
                    tracing::error!("tunnel failed: {e}");
                    return;
                }
            }
            let name = contacts.name_of(&peer).unwrap_or("peer").to_string();
            p.child = spawn_media(&name);
            ring::notify("In call", &format!("with {name}"));
        }
        Action::StopMedia { peer } => {
            if let Some(p) = peers.get_mut(&peer) {
                p.tunnel = None;
                if let Some(mut c) = p.child.take() {
                    let _ = c.kill().await;
                }
            }
        }
        Action::Notify { title, body } => ring::notify(&title, &body),
        Action::DialRoster { .. } => tracing::debug!("mesh join arrives in Stage 2"),
        Action::CallEnded => {
            for (_, p) in peers.iter_mut() {
                p.tunnel = None;
                if let Some(mut c) = p.child.take() {
                    let _ = c.kill().await;
                }
            }
            peers.clear();
        }
    }
}

/// Start the media pipeline. Stage 1 shells out to v1's bash script; Stage 2
/// replaces this with in-process gstreamer.
///
/// Unset `OMACALL_MEDIA_CMD` means the daemon runs the call without media, so
/// signalling can be exercised on its own -- which is how the first end-to-end
/// test is run.
fn spawn_media(peer_name: &str) -> Option<Child> {
    let cmd = std::env::var("OMACALL_MEDIA_CMD").ok()?;
    Command::new("sh")
        .arg("-c")
        .arg(format!("{cmd} {peer_name}"))
        .kill_on_drop(true)
        .process_group(0) // its own group, so hangup kills the whole pipeline
        .spawn()
        .map_err(|e| tracing::error!("media command failed to start: {e}"))
        .ok()
}

fn spawn_reader(from: EndpointId, mut recv: RecvStream, conn: Connection, tx: mpsc::Sender<Ev>) {
    tokio::spawn(async move {
        loop {
            match read_msg(&mut recv).await {
                Ok(msg) => {
                    if tx.send(Ev::Control { from, msg }).await.is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = conn.closed().await;
                    let _ = tx.send(Ev::Gone(from)).await;
                    return;
                }
            }
        }
    });
}

async fn read_msg(recv: &mut RecvStream) -> Result<Msg> {
    let mut len = [0u8; 2];
    recv.read_exact(&mut len).await?;
    let n = u16::from_le_bytes(len) as usize;
    let mut body = vec![0u8; n];
    recv.read_exact(&mut body).await?;
    Ok(proto::frame::decode(&body)?)
}

async fn write_msg(send: &mut SendStream, msg: &Msg) -> Result<()> {
    let bytes = proto::frame::encode(msg)?;
    send.write_all(&bytes).await?;
    Ok(())
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "omarchy".into())
}
