//! Stage 1 only: bridges the bash pipeline's UDP to iroh datagrams.
//!
//! Local gst sends RTP to `127.0.0.1:base` and `base+2`; we tag each packet and
//! send it as a QUIC datagram. Inbound datagrams are untagged and delivered to
//! `base+4` and `base+6`, where the receiving pipeline listens. All loopback, so
//! the firewall never sees any of it.
//!
//! Stage 2 deletes this file: appsink and appsrc replace the localhost hop.

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use bytes::Bytes;
use iroh::endpoint::Connection;
use tokio::{net::UdpSocket, task::JoinSet};

use crate::proto::{TAG_AUDIO, TAG_VIDEO};

/// Payloader MTU that fits from the first packet.
///
/// Measured during the spike: `max_datagram_size()` is 1162 at connection start
/// and only rises to ~1414 after MTU discovery. v1's default of 1400 (+1 tag
/// byte) therefore does not fit at call start, and every keyframe fragment is
/// dropped -- which looks exactly like a codec bug.
pub const PAYLOAD_MTU: u16 = 1120;

pub struct Tunnel {
    tasks: JoinSet<()>,
}

impl Tunnel {
    /// Start bridging. Tasks stop when the returned `Tunnel` is dropped.
    pub async fn start(conn: Connection, port_base: u16) -> Result<Self> {
        let vid_in = UdpSocket::bind(local(port_base)).await
            .with_context(|| format!("binding udp/{port_base} for outbound video"))?;
        let aud_in = UdpSocket::bind(local(port_base + 2)).await
            .with_context(|| format!("binding udp/{} for outbound audio", port_base + 2))?;
        let out = Arc::new(UdpSocket::bind(local(0)).await?);

        let mut tasks = JoinSet::new();

        for (sock, tag) in [(vid_in, TAG_VIDEO), (aud_in, TAG_AUDIO)] {
            let conn = conn.clone();
            tasks.spawn(async move {
                let mut buf = vec![0u8; 2048];
                loop {
                    let Ok(n) = sock.recv(&mut buf).await else { return };
                    let mut framed = Vec::with_capacity(n + 1);
                    framed.push(tag);
                    framed.extend_from_slice(&buf[..n]);
                    // A failed datagram is a dropped frame, never a dropped
                    // call: this is unreliable media by design.
                    let _ = conn.send_datagram(Bytes::from(framed));
                }
            });
        }

        let vid_out = local(port_base + 4);
        let aud_out = local(port_base + 6);
        tasks.spawn(async move {
            loop {
                let Ok(dg) = conn.read_datagram().await else { return };
                let Some((&tag, body)) = dg.split_first() else { continue };
                let dest = match tag {
                    TAG_VIDEO => vid_out,
                    TAG_AUDIO => aud_out,
                    _ => continue, // feedback is the call actor's business
                };
                let _ = out.send_to(body, dest).await;
            }
        });

        Ok(Self { tasks })
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // v1's defining bug was a killed wrapper leaving gst transmitting
        // forever. Here the tasks are owned, so hanging up really stops them.
        self.tasks.abort_all();
    }
}

fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Ports the bash pipeline must use, given a base. Kept next to the tunnel so
/// the two cannot drift apart.
pub fn ports(base: u16) -> Ports {
    Ports {
        send_video_to: base,
        send_audio_to: base + 2,
        recv_video_on: base + 4,
        recv_audio_on: base + 6,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ports {
    pub send_video_to: u16,
    pub send_audio_to: u16,
    pub recv_video_on: u16,
    pub recv_audio_on: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_layout_never_collides() {
        let p = ports(5000);
        assert_eq!(p.send_video_to, 5000);
        assert_eq!(p.recv_video_on, 5004);
        // The send and receive ports must differ: the tunnel owns the send
        // side, so a pipeline listening on the same port would never bind.
        let mut all = vec![p.send_video_to, p.send_audio_to, p.recv_video_on, p.recv_audio_on];
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 4, "every port must be distinct");
    }

    #[test]
    fn two_daemons_on_one_machine_do_not_overlap() {
        let a = ports(5000);
        let b = ports(5100);
        let mut all = vec![
            a.send_video_to, a.send_audio_to, a.recv_video_on, a.recv_audio_on,
            b.send_video_to, b.send_audio_to, b.recv_video_on, b.recv_audio_on,
        ];
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 8, "OMACALL_PORT_BASE must isolate test daemons");
    }

    #[test]
    fn payload_mtu_fits_the_smallest_measured_datagram() {
        // 1162 was measured at connection start on both loopback and a real
        // cross-machine path, +1 for the channel tag.
        assert!(PAYLOAD_MTU + 1 < 1162, "must fit before MTU discovery raises it");
    }
}
