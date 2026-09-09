//! Spike: RTP over iroh QUIC datagrams.
//!
//! Throwaway. Answers the go/no-go questions in PLAN.md §0 before any production
//! code exists, and is a rough draft of Stage 1's tunnel so little is wasted.
//!
//!   spike listen          bind, print our endpoint id, accept one connection
//!   spike dial <id>       connect to that endpoint id
//!
//! Either side bridges the same way, so v1's bash pipelines work unmodified apart
//! from ports:
//!
//!   local gst  --udp 5000/5002-->  spike  --tagged datagrams-->  peer
//!   local gst  <--udp 5004/5006--  spike  <--tagged datagrams--  peer
//!
//! Run the media by hand, e.g. on one side:
//!   gst-launch-1.0 v4l2src ! image/jpeg,width=1280,height=720,framerate=30/1 \
//!     ! jpegdec ! videoconvert ! vp8enc deadline=1 error-resilient=1 \
//!     ! rtpvp8pay pt=96 mtu=1120 ! udpsink host=127.0.0.1 port=5000
//! and on the other:
//!   gst-launch-1.0 udpsrc port=5004 caps="application/x-rtp,media=(string)video,\
//!     encoding-name=(string)VP8,payload=(int)96" ! rtpjitterbuffer latency=120 \
//!     ! rtpvp8depay ! vp8dec ! videoconvert ! autovideosink

use std::{
    env,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use bytes::Bytes;
use iroh::{
    endpoint::{presets, QuicTransportConfig},
    Endpoint, EndpointAddr,
};
use tokio::net::UdpSocket;

const ALPN: &[u8] = b"omacall/0";

const TAG_VIDEO: u8 = 0;
const TAG_AUDIO: u8 = 1;

/// PLAN.md R1: with quinn's default-sized datagram buffer, congestion first shows
/// up as seconds of buffered stale video and only then as silent drops. A small
/// buffer turns that into drop-oldest, which is what unreliable media wants.
const DATAGRAM_SEND_BUFFER: usize = 32 * 1024;

#[derive(Default)]
struct Counters {
    dg_out: AtomicU64,
    dg_in: AtomicU64,
    bytes_out: AtomicU64,
    bytes_in: AtomicU64,
    send_err_size: AtomicU64,
    send_err_other: AtomicU64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let port_base: u16 = env::var("OMACALL_PORT_BASE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000);

    // qlog gives per-connection congestion and loss traces for free; set
    // OMACALL_QLOG to a directory to collect them during the bottleneck runs.
    let transport = {
        let b = QuicTransportConfig::builder()
            .datagram_send_buffer_size(DATAGRAM_SEND_BUFFER)
            .keep_alive_interval(Duration::from_secs(5));
        let b = match env::var("OMACALL_QLOG") {
            Ok(dir) if !dir.is_empty() => b.qlog_from_path(dir, "spike"),
            _ => b,
        };
        b.build()
    };

    let mut builder = Endpoint::builder(presets::N0)
        .alpns(vec![ALPN.to_vec()])
        .transport_config(transport);

    // OMACALL_RELAY_ONLY=1 disables direct paths, which is how the matrix's
    // relay row is run without needing a genuinely hostile NAT: it forces the
    // same fallback a symmetric-NAT pair would land on.
    if env::var("OMACALL_RELAY_ONLY").is_ok_and(|v| v == "1") {
        println!("relay-only mode: direct IP transports cleared");
        builder = builder.clear_ip_transports();
    }

    let endpoint = builder.bind().await?;

    let conn = match args.get(1).map(String::as_str) {
        Some("listen") => {
            // Dialing a bare endpoint id needs pkarr/DNS discovery to have
            // published AND propagated, which fails outright on a cold start.
            // The ticket carries the addresses, so it works immediately --
            // this is why PLAN.md specifies tickets rather than bare ids.
            let mut addr = endpoint.addr();
            for _ in 0..20 {
                if !addr.addrs.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                addr = endpoint.addr();
            }
            println!("endpoint id: {}", endpoint.id());
            println!("addresses:   {}", addr.addrs.len());
            println!("TICKET {}", serde_json::to_string(&addr)?);
            println!("waiting for a connection...");
            let incoming = endpoint
                .accept()
                .await
                .ok_or_else(|| anyhow::anyhow!("endpoint closed while accepting"))?;
            incoming.await?
        }
        Some("dial") => {
            let raw = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("usage: spike dial <ticket-json>"))?;
            let addr: EndpointAddr = serde_json::from_str(raw)?;
            println!("dialing {} via {} address(es)...", addr.id, addr.addrs.len());
            endpoint.connect(addr, ALPN).await?
        }
        _ => {
            eprintln!("usage: spike listen | spike dial '<ticket-json>'");
            std::process::exit(2);
        }
    };

    println!("connected to {}", conn.remote_id());
    println!(
        "max_datagram_size at start: {:?}",
        conn.max_datagram_size()
    );

    let counters = Arc::new(Counters::default());

    // Local gst sends to these; we forward them out as tagged datagrams.
    let vid_in = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], port_base))).await?;
    let aud_in = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], port_base + 2))).await?;
    // Inbound datagrams are delivered to local gst here.
    let out = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let vid_out = SocketAddr::from(([127, 0, 0, 1], port_base + 4));
    let aud_out = SocketAddr::from(([127, 0, 0, 1], port_base + 6));

    println!(
        "bridging: udp/{} -> tag0, udp/{} -> tag1, inbound -> udp/{} and udp/{}",
        port_base,
        port_base + 2,
        port_base + 4,
        port_base + 6
    );

    let mut tasks = tokio::task::JoinSet::new();

    for (sock, tag) in [(vid_in, TAG_VIDEO), (aud_in, TAG_AUDIO)] {
        let conn = conn.clone();
        let c = counters.clone();
        tasks.spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let n = match sock.recv(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("udp recv: {e}");
                        return;
                    }
                };
                let mut framed = Vec::with_capacity(n + 1);
                framed.push(tag);
                framed.extend_from_slice(&buf[..n]);
                let len = framed.len() as u64;
                match conn.send_datagram(Bytes::from(framed)) {
                    Ok(()) => {
                        c.dg_out.fetch_add(1, Ordering::Relaxed);
                        c.bytes_out.fetch_add(len, Ordering::Relaxed);
                    }
                    Err(e) => {
                        // Distinguish "too big for this path" from everything else:
                        // the first is an mtu problem, the second is not.
                        if format!("{e}").contains("too large") {
                            c.send_err_size.fetch_add(1, Ordering::Relaxed);
                        } else {
                            c.send_err_other.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        });
    }

    {
        let conn = conn.clone();
        let c = counters.clone();
        tasks.spawn(async move {
            loop {
                let dg = match conn.read_datagram().await {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("connection closed: {e}");
                        return;
                    }
                };
                if dg.is_empty() {
                    continue;
                }
                let dest = match dg[0] {
                    TAG_VIDEO => vid_out,
                    TAG_AUDIO => aud_out,
                    _ => continue,
                };
                let _ = out.send_to(&dg[1..], dest).await;
                c.dg_in.fetch_add(1, Ordering::Relaxed);
                c.bytes_in.fetch_add(dg.len() as u64, Ordering::Relaxed);
            }
        });
    }

    // 1Hz instrument panel. Everything the pass/fail matrix needs to read.
    {
        let conn = conn.clone();
        let c = counters.clone();
        tasks.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let (mut last_out, mut last_in) = (0u64, 0u64);
            let (mut last_bo, mut last_bi) = (0u64, 0u64);
            loop {
                tick.tick().await;
                let out_n = c.dg_out.load(Ordering::Relaxed);
                let in_n = c.dg_in.load(Ordering::Relaxed);
                let bo = c.bytes_out.load(Ordering::Relaxed);
                let bi = c.bytes_in.load(Ordering::Relaxed);
                let stats = conn.stats();

                // Direct vs relay is what the go/no-go matrix actually reads.
                let paths = conn.paths();
                let (n_ip, n_relay) = paths.iter().fold((0, 0), |(i, r), p| {
                    if p.is_relay() {
                        (i, r + 1)
                    } else if p.is_ip() {
                        (i + 1, r)
                    } else {
                        (i, r)
                    }
                });

                println!(
                    "out {:>5}/s  in {:>5}/s  kbps_out {:>6}  kbps_in {:>6}  mtu {:?}  \
                     send_buf_free {:>6}  paths ip:{} relay:{}  lost {:>6}  \
                     size_err {}  other_err {}",
                    out_n - last_out,
                    in_n - last_in,
                    ((bo - last_bo) * 8) / 1000,
                    ((bi - last_bi) * 8) / 1000,
                    conn.max_datagram_size(),
                    conn.datagram_send_buffer_space(),
                    n_ip,
                    n_relay,
                    stats.lost_packets,
                    c.send_err_size.load(Ordering::Relaxed),
                    c.send_err_other.load(Ordering::Relaxed),
                );
                (last_out, last_in, last_bo, last_bi) = (out_n, in_n, bo, bi);
            }
        });
    }

    // Path changes are the direct-vs-relay answer, and iroh 1.x is multipath, so
    // this can flip mid-call. max_datagram_size can change with it.
    {
        let conn = conn.clone();
        tasks.spawn(async move {
            let mut events = conn.path_events();
            while let Some(ev) = futures_lite::StreamExt::next(&mut events).await {
                println!("PATH EVENT: {ev:?}  mtu now {:?}", conn.max_datagram_size());
            }
        });
    }

    tokio::signal::ctrl_c().await?;
    println!("\nshutting down");
    tasks.shutdown().await;
    Ok(())
}
