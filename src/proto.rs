//! Wire protocol and the call state machine.
//!
//! `CallState` is deliberately pure: it owns no sockets, spawns nothing, and
//! never blocks. It takes an [`Event`] and returns [`Action`]s for the caller to
//! perform. Every transition in PLAN.md §2 is therefore a unit test with no
//! network, which is the whole reason the protocol rework was affordable.

use std::collections::HashMap;

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

/// Datagram channel tags. The first byte of every datagram.
pub const TAG_VIDEO: u8 = 0;
pub const TAG_AUDIO: u8 = 1;
pub const TAG_FEEDBACK: u8 = 2;

/// Full mesh, so every participant sends to every other. The ceiling is
/// upstream bandwidth: four people at 1.5Mbps is 4.5Mbps up, which is where
/// ordinary home connections start to hurt.
pub const MAX_PARTICIPANTS: usize = 4;

/// How long a ring waits for a human. v1's value.
pub const RING_TIMEOUT_SECS: u64 = 45;

/// How long an invited peer has to actually connect before members forget it.
pub const JOIN_GRACE_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Vp8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    pub id: EndpointId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Msg {
    /// A human invite. Always rings someone.
    Ring {
        call_id: u64,
        name: String,
        /// Preference order. Vp8 is always present.
        video: Vec<Codec>,
        roster: Vec<Peer>,
    },
    /// A silent mesh join, only honoured for a peer already announced by
    /// `PeerJoining`. Knowing `call_id` is deliberately not sufficient.
    Join {
        call_id: u64,
        name: String,
        video: Codec,
    },
    /// Sent by an inviter to every existing member when its invitee accepts.
    /// This is what authorises the silent join, and what tells members a new
    /// tile is about to appear.
    PeerJoining { id: EndpointId, name: String },
    Accept {
        name: String,
        video: Codec,
    },
    Decline,
    Busy,
    /// The dialer gave up before the callee answered.
    Cancel,
    Bye,
    Ping {
        t_us: u64,
    },
    Pong {
        t_us: u64,
    },
    /// Feedback channel only.
    KeyframeReq,
    /// Feedback channel only. Once per second, per peer.
    Stats {
        pkts: u32,
        lost: u32,
        jitter_ms: u16,
    },
}

/// Length-prefixed postcard framing for the control stream.
pub mod frame {
    use super::Msg;

    #[derive(Debug, thiserror::Error)]
    pub enum FrameError {
        #[error("message too large to frame: {0} bytes")]
        TooLarge(usize),
        #[error("postcard: {0}")]
        Postcard(#[from] postcard::Error),
    }

    pub const MAX_FRAME: usize = u16::MAX as usize;

    pub fn encode(msg: &Msg) -> Result<Vec<u8>, FrameError> {
        let body = postcard::to_allocvec(msg)?;
        if body.len() > MAX_FRAME {
            return Err(FrameError::TooLarge(body.len()));
        }
        let mut out = Vec::with_capacity(body.len() + 2);
        out.extend_from_slice(&(body.len() as u16).to_le_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub fn decode(body: &[u8]) -> Result<Msg, FrameError> {
        Ok(postcard::from_bytes(body)?)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The local user placed a call.
    Dial { id: EndpointId, name: String },
    /// A control message arrived from a peer.
    Rx { from: EndpointId, msg: Msg },
    /// The answer/decline prompt came back.
    UserVerdict(bool),
    /// Nobody answered within `RING_TIMEOUT_SECS`.
    RingTimeout,
    /// QUIC says a peer is gone.
    ConnLost(EndpointId),
    /// The call window was closed, or SIGINT.
    Hangup,
    /// An announced joiner never connected within `JOIN_GRACE_SECS`.
    JoinGraceExpired(EndpointId),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Send { to: EndpointId, msg: Msg },
    /// Ring the human. The daemon renders this via the plugin overlay, or the
    /// floating terminal when no plugin is installed.
    StartRing { from: String },
    StopRing,
    StartMedia { peer: EndpointId, codec: Codec },
    StopMedia { peer: EndpointId },
    /// A desktop notification that is not a ring: missed calls, joins, aborts.
    Notify { title: String, body: String },
    /// Begin dialing the rest of the roster after accepting an invite.
    DialRoster { peers: Vec<Peer>, call_id: u64 },
    CallEnded,
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    Idle,
    RingingOut { id: EndpointId, name: String, call_id: u64 },
    RingingIn { call_id: u64, from: Vec<EndpointId>, name: String, roster: Vec<Peer> },
    InCall { call_id: u64, codec: Codec },
}

pub struct CallState {
    me: EndpointId,
    my_name: String,
    state: State,
    /// Peers currently in the call, by id.
    roster: HashMap<EndpointId, String>,
    /// Peers a member told us are joining. Only these may `Join` silently.
    pending_joiners: HashMap<EndpointId, String>,
    /// Codecs this machine can encode, in preference order. Always ends in Vp8.
    my_codecs: Vec<Codec>,
    /// Ring strangers, or decline them silently.
    pub ring_unknown: bool,
    next_call_id: u64,
}

impl CallState {
    pub fn new(me: EndpointId, my_name: impl Into<String>, my_codecs: Vec<Codec>) -> Self {
        debug_assert!(my_codecs.contains(&Codec::Vp8), "Vp8 is the universal fallback");
        Self {
            me,
            my_name: my_name.into(),
            state: State::Idle,
            roster: HashMap::new(),
            pending_joiners: HashMap::new(),
            my_codecs,
            ring_unknown: false,
            next_call_id: 1,
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle)
    }

    pub fn roster_len(&self) -> usize {
        self.roster.len()
    }

    /// Test seam: real callers mint 64 random bits.
    pub fn set_next_call_id(&mut self, id: u64) {
        self.next_call_id = id;
    }

    fn pick_codec(&self, offered: &[Codec]) -> Option<Codec> {
        offered.iter().copied().find(|c| self.my_codecs.contains(c))
    }

    fn end_call(&mut self, out: &mut Vec<Action>) {
        for peer in self.roster.keys().copied().collect::<Vec<_>>() {
            out.push(Action::Send { to: peer, msg: Msg::Bye });
            out.push(Action::StopMedia { peer });
        }
        self.roster.clear();
        self.pending_joiners.clear();
        self.state = State::Idle;
        out.push(Action::CallEnded);
    }

    pub fn handle(&mut self, ev: Event) -> Vec<Action> {
        let mut out = Vec::new();
        match (&self.state.clone(), ev) {
            // ---------- placing a call ----------
            (State::Idle, Event::Dial { id, name }) => {
                let call_id = self.next_call_id;
                self.state = State::RingingOut { id, name: name.clone(), call_id };
                out.push(Action::Send {
                    to: id,
                    msg: Msg::Ring {
                        call_id,
                        name: self.my_name.clone(),
                        video: self.my_codecs.clone(),
                        roster: vec![],
                    },
                });
            }

            // ---------- glare: both sides dialled at once ----------
            // Both users already expressed intent, so neither should be
            // prompted. Deterministic tiebreak: the lower id's call wins.
            (State::RingingOut { id, call_id, .. }, Event::Rx { from, msg: Msg::Ring { call_id: their_id, video, .. } })
                if from == *id =>
            {
                let winner = if self.me.as_bytes() < from.as_bytes() { *call_id } else { their_id };
                let codec = self.pick_codec(&video).unwrap_or(Codec::Vp8);
                self.roster.insert(from, String::new());
                self.state = State::InCall { call_id: winner, codec };
                out.push(Action::Send {
                    to: from,
                    msg: Msg::Accept { name: self.my_name.clone(), video: codec },
                });
                out.push(Action::StartMedia { peer: from, codec });
            }

            (State::RingingOut { .. }, Event::Rx { from, msg: Msg::Accept { name, video } }) => {
                self.roster.insert(from, name);
                self.state = State::InCall { call_id: self.next_call_id, codec: video };
                out.push(Action::StartMedia { peer: from, codec: video });
            }

            (State::RingingOut { name, .. }, Event::Rx { msg: Msg::Decline, .. })
            | (State::RingingOut { name, .. }, Event::Rx { msg: Msg::Busy, .. }) => {
                out.push(Action::Notify {
                    title: format!("{name} is not available"),
                    body: String::new(),
                });
                self.state = State::Idle;
                out.push(Action::CallEnded);
            }

            (State::RingingOut { id, .. }, Event::RingTimeout) => {
                out.push(Action::Send { to: *id, msg: Msg::Cancel });
                self.state = State::Idle;
                out.push(Action::CallEnded);
            }

            // ---------- receiving a call ----------
            (State::Idle, Event::Rx { from, msg: Msg::Ring { call_id, name, video, roster } }) => {
                if self.pick_codec(&video).is_none() {
                    out.push(Action::Send { to: from, msg: Msg::Decline });
                } else if !self.ring_unknown && !self.roster.contains_key(&from) {
                    // Unknown caller from the internet: decline without ringing,
                    // but say so, or missed calls become invisible.
                    out.push(Action::Send { to: from, msg: Msg::Decline });
                    out.push(Action::Notify {
                        title: "Declined unknown caller".into(),
                        body: name,
                    });
                } else {
                    self.state = State::RingingIn {
                        call_id,
                        from: vec![from],
                        name: name.clone(),
                        roster,
                    };
                    out.push(Action::StartRing { from: name });
                }
            }

            // Two members invited the same person at once: coalesce, so one
            // Answer accepts to both rather than leaving a dangling inviter.
            (State::RingingIn { call_id, from, .. }, Event::Rx { from: f2, msg: Msg::Ring { call_id: c2, .. } })
                if c2 == *call_id && !from.contains(&f2) =>
            {
                if let State::RingingIn { from, .. } = &mut self.state {
                    from.push(f2);
                }
            }

            (State::RingingIn { .. }, Event::Rx { from, msg: Msg::Ring { .. } }) => {
                out.push(Action::Send { to: from, msg: Msg::Busy });
            }

            (State::RingingIn { from, call_id, roster, .. }, Event::UserVerdict(true)) => {
                let codec = Codec::Vp8;
                out.push(Action::StopRing);
                for peer in from {
                    self.roster.insert(*peer, String::new());
                    out.push(Action::Send {
                        to: *peer,
                        msg: Msg::Accept { name: self.my_name.clone(), video: codec },
                    });
                    out.push(Action::StartMedia { peer: *peer, codec });
                }
                // Reach the rest of the mesh ourselves; a roster member we
                // cannot reach must abort the join rather than sit divergent.
                let others: Vec<Peer> = roster.iter().filter(|p| p.id != self.me && !self.roster.contains_key(&p.id)).cloned().collect();
                if !others.is_empty() {
                    out.push(Action::DialRoster { peers: others, call_id: *call_id });
                }
                self.state = State::InCall { call_id: *call_id, codec };
            }

            (State::RingingIn { from, .. }, Event::UserVerdict(false)) => {
                out.push(Action::StopRing);
                for peer in from {
                    out.push(Action::Send { to: *peer, msg: Msg::Decline });
                }
                self.state = State::Idle;
                out.push(Action::CallEnded);
            }

            (State::RingingIn { from, .. }, Event::RingTimeout) => {
                out.push(Action::StopRing);
                for peer in from {
                    out.push(Action::Send { to: *peer, msg: Msg::Decline });
                }
                self.state = State::Idle;
                out.push(Action::CallEnded);
            }

            // The caller gave up while our prompt was still on screen. Without
            // this the dialog outlives the call and a later Answer lands on a
            // peer that is already Idle.
            (State::RingingIn { from, .. }, Event::Rx { from: f, msg: Msg::Cancel }) if from.contains(&f) => {
                out.push(Action::StopRing);
                self.state = State::Idle;
                out.push(Action::Notify { title: "Missed call".into(), body: String::new() });
                out.push(Action::CallEnded);
            }

            // ---------- in a call ----------
            (State::InCall { call_id, codec }, Event::Rx { from, msg: Msg::Join { call_id: c, name, video } }) => {
                let announced = self.pending_joiners.contains_key(&from);
                if c != *call_id || !announced {
                    // Knowing the call_id is not authorisation. A declined
                    // invitee holds one and must still be turned away.
                    out.push(Action::Send { to: from, msg: Msg::Busy });
                } else if self.roster.len() + 1 >= MAX_PARTICIPANTS {
                    out.push(Action::Send { to: from, msg: Msg::Busy });
                } else if video != *codec {
                    out.push(Action::Send { to: from, msg: Msg::Decline });
                } else {
                    self.pending_joiners.remove(&from);
                    self.roster.insert(from, name.clone());
                    out.push(Action::Send {
                        to: from,
                        msg: Msg::Accept { name: self.my_name.clone(), video: *codec },
                    });
                    out.push(Action::StartMedia { peer: from, codec: *codec });
                    out.push(Action::Notify { title: format!("{name} joined"), body: String::new() });
                }
            }

            (State::InCall { .. }, Event::Rx { msg: Msg::PeerJoining { id, name }, .. }) => {
                out.push(Action::Notify {
                    title: format!("{name} is joining"),
                    body: String::new(),
                });
                self.pending_joiners.insert(id, name);
            }

            (State::InCall { .. }, Event::JoinGraceExpired(id)) => {
                if let Some(name) = self.pending_joiners.remove(&id) {
                    out.push(Action::Notify {
                        title: format!("{name} could not join"),
                        body: String::new(),
                    });
                }
            }

            // Someone dialling a call we are not in.
            (State::InCall { .. }, Event::Rx { from, msg: Msg::Ring { name, .. } }) => {
                out.push(Action::Send { to: from, msg: Msg::Busy });
                out.push(Action::Notify { title: format!("Missed call from {name}"), body: String::new() });
            }

            (State::InCall { .. }, Event::Rx { from, msg: Msg::Bye })
            | (State::InCall { .. }, Event::ConnLost(from)) => {
                if self.roster.remove(&from).is_some() {
                    out.push(Action::StopMedia { peer: from });
                }
                if self.roster.is_empty() {
                    self.state = State::Idle;
                    self.pending_joiners.clear();
                    out.push(Action::CallEnded);
                }
            }

            (State::InCall { .. }, Event::Hangup) => self.end_call(&mut out),

            // ---------- catch-alls ----------
            // A stray Accept or Bye arriving after we gave up. Answer Bye and
            // drop it, rather than leaving the sender waiting forever.
            (State::Idle, Event::Rx { from, msg }) if !matches!(msg, Msg::Ring { .. }) => {
                if !matches!(msg, Msg::Bye | Msg::Decline | Msg::Busy) {
                    out.push(Action::Send { to: from, msg: Msg::Bye });
                }
            }

            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // An ed25519 public key must be a valid curve point, so these are derived
    // from deterministic secret keys rather than invented byte patterns.
    fn id(b: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[b; 32]).public()
    }

    fn state(me: u8) -> CallState {
        CallState::new(id(me), "me", vec![Codec::Vp8])
    }

    fn ring(call_id: u64) -> Msg {
        Msg::Ring { call_id, name: "them".into(), video: vec![Codec::Vp8], roster: vec![] }
    }

    #[test]
    fn frame_roundtrip() {
        let m = ring(7);
        let bytes = frame::encode(&m).unwrap();
        let len = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
        assert_eq!(len, bytes.len() - 2);
        assert_eq!(frame::decode(&bytes[2..]).unwrap(), m);
    }

    #[test]
    fn frame_rejects_truncated_body() {
        let bytes = frame::encode(&ring(1)).unwrap();
        assert!(frame::decode(&bytes[2..bytes.len() - 1]).is_err());
    }

    #[test]
    fn dial_then_accept_starts_media() {
        let mut s = state(1);
        s.handle(Event::Dial { id: id(2), name: "them".into() });
        let out = s.handle(Event::Rx {
            from: id(2),
            msg: Msg::Accept { name: "them".into(), video: Codec::Vp8 },
        });
        assert!(out.iter().any(|a| matches!(a, Action::StartMedia { .. })));
        assert_eq!(s.roster_len(), 1);
    }

    #[test]
    fn ring_timeout_cancels_and_returns_to_idle() {
        let mut s = state(1);
        s.handle(Event::Dial { id: id(2), name: "them".into() });
        let out = s.handle(Event::RingTimeout);
        assert!(out.contains(&Action::Send { to: id(2), msg: Msg::Cancel }));
        assert!(s.is_idle());
    }

    #[test]
    fn unknown_caller_is_declined_but_still_reported() {
        let mut s = state(1);
        let out = s.handle(Event::Rx { from: id(9), msg: ring(1) });
        assert!(out.contains(&Action::Send { to: id(9), msg: Msg::Decline }));
        assert!(out.iter().any(|a| matches!(a, Action::Notify { .. })));
        assert!(s.is_idle());
    }

    #[test]
    fn known_caller_rings_and_answering_starts_media() {
        let mut s = state(1);
        s.ring_unknown = true;
        let out = s.handle(Event::Rx { from: id(2), msg: ring(5) });
        assert!(out.iter().any(|a| matches!(a, Action::StartRing { .. })));
        let out = s.handle(Event::UserVerdict(true));
        assert!(out.iter().any(|a| matches!(a, Action::StartMedia { .. })));
    }

    #[test]
    fn caller_cancelling_tears_down_the_prompt() {
        let mut s = state(1);
        s.ring_unknown = true;
        s.handle(Event::Rx { from: id(2), msg: ring(5) });
        let out = s.handle(Event::Rx { from: id(2), msg: Msg::Cancel });
        assert!(out.contains(&Action::StopRing));
        assert!(s.is_idle(), "a cancelled ring must not leave a live dialog");
    }

    #[test]
    fn simultaneous_invites_coalesce_into_one_answer() {
        let mut s = state(1);
        s.ring_unknown = true;
        s.handle(Event::Rx { from: id(2), msg: ring(5) });
        s.handle(Event::Rx { from: id(3), msg: ring(5) });
        let out = s.handle(Event::UserVerdict(true));
        let accepts = out
            .iter()
            .filter(|a| matches!(a, Action::Send { msg: Msg::Accept { .. }, .. }))
            .count();
        assert_eq!(accepts, 2, "one Answer must accept to both inviters");
    }

    #[test]
    fn a_different_call_while_ringing_gets_busy() {
        let mut s = state(1);
        s.ring_unknown = true;
        s.handle(Event::Rx { from: id(2), msg: ring(5) });
        let out = s.handle(Event::Rx { from: id(4), msg: ring(99) });
        assert!(out.contains(&Action::Send { to: id(4), msg: Msg::Busy }));
    }

    #[test]
    fn glare_resolves_without_prompting_either_side() {
        let mut s = state(1);
        s.handle(Event::Dial { id: id(2), name: "them".into() });
        let out = s.handle(Event::Rx { from: id(2), msg: ring(77) });
        assert!(out.iter().any(|a| matches!(a, Action::StartMedia { .. })));
        assert!(
            !out.iter().any(|a| matches!(a, Action::StartRing { .. })),
            "both users already expressed intent; neither should be rung"
        );
    }

    #[test]
    fn join_without_being_announced_is_refused() {
        let mut s = state(1);
        s.ring_unknown = true;
        s.handle(Event::Rx { from: id(2), msg: ring(5) });
        s.handle(Event::UserVerdict(true));
        // id(3) knows call_id 5 -- e.g. it was invited and declined -- but no
        // member announced it.
        let out = s.handle(Event::Rx {
            from: id(3),
            msg: Msg::Join { call_id: 5, name: "gatecrasher".into(), video: Codec::Vp8 },
        });
        assert!(out.contains(&Action::Send { to: id(3), msg: Msg::Busy }));
        assert_eq!(s.roster_len(), 1, "knowing the call_id must not admit anyone");
    }

    #[test]
    fn announced_join_is_admitted() {
        let mut s = state(1);
        s.ring_unknown = true;
        s.handle(Event::Rx { from: id(2), msg: ring(5) });
        s.handle(Event::UserVerdict(true));
        s.handle(Event::Rx {
            from: id(2),
            msg: Msg::PeerJoining { id: id(3), name: "carlos".into() },
        });
        let out = s.handle(Event::Rx {
            from: id(3),
            msg: Msg::Join { call_id: 5, name: "carlos".into(), video: Codec::Vp8 },
        });
        assert!(out.iter().any(|a| matches!(a, Action::StartMedia { .. })));
        assert_eq!(s.roster_len(), 2);
    }

    #[test]
    fn announcement_expires_so_a_stale_invite_cannot_be_replayed() {
        let mut s = state(1);
        s.ring_unknown = true;
        s.handle(Event::Rx { from: id(2), msg: ring(5) });
        s.handle(Event::UserVerdict(true));
        s.handle(Event::Rx { from: id(2), msg: Msg::PeerJoining { id: id(3), name: "carlos".into() } });
        s.handle(Event::JoinGraceExpired(id(3)));
        let out = s.handle(Event::Rx {
            from: id(3),
            msg: Msg::Join { call_id: 5, name: "carlos".into(), video: Codec::Vp8 },
        });
        assert!(out.contains(&Action::Send { to: id(3), msg: Msg::Busy }));
    }

    #[test]
    fn roster_is_capped() {
        let mut s = state(1);
        s.ring_unknown = true;
        s.handle(Event::Rx { from: id(2), msg: ring(5) });
        s.handle(Event::UserVerdict(true));
        for n in 3..=4u8 {
            s.handle(Event::Rx { from: id(2), msg: Msg::PeerJoining { id: id(n), name: "x".into() } });
            s.handle(Event::Rx {
                from: id(n),
                msg: Msg::Join { call_id: 5, name: "x".into(), video: Codec::Vp8 },
            });
        }
        assert_eq!(s.roster_len(), 3);
        s.handle(Event::Rx { from: id(2), msg: Msg::PeerJoining { id: id(5), name: "fifth".into() } });
        let out = s.handle(Event::Rx {
            from: id(5),
            msg: Msg::Join { call_id: 5, name: "fifth".into(), video: Codec::Vp8 },
        });
        assert!(out.contains(&Action::Send { to: id(5), msg: Msg::Busy }));
        assert_eq!(s.roster_len(), 3, "a fifth participant must be refused");
    }

    #[test]
    fn last_peer_leaving_ends_the_call() {
        let mut s = state(1);
        s.ring_unknown = true;
        s.handle(Event::Rx { from: id(2), msg: ring(5) });
        s.handle(Event::UserVerdict(true));
        let out = s.handle(Event::Rx { from: id(2), msg: Msg::Bye });
        assert!(out.contains(&Action::CallEnded));
        assert!(s.is_idle());
    }

    #[test]
    fn one_peer_leaving_a_group_does_not_end_it() {
        let mut s = state(1);
        s.ring_unknown = true;
        s.handle(Event::Rx { from: id(2), msg: ring(5) });
        s.handle(Event::UserVerdict(true));
        s.handle(Event::Rx { from: id(2), msg: Msg::PeerJoining { id: id(3), name: "c".into() } });
        s.handle(Event::Rx {
            from: id(3),
            msg: Msg::Join { call_id: 5, name: "c".into(), video: Codec::Vp8 },
        });
        let out = s.handle(Event::ConnLost(id(3)));
        assert!(out.contains(&Action::StopMedia { peer: id(3) }));
        assert!(!out.contains(&Action::CallEnded));
        assert_eq!(s.roster_len(), 1);
    }

    #[test]
    fn hangup_says_goodbye_to_everyone() {
        let mut s = state(1);
        s.ring_unknown = true;
        s.handle(Event::Rx { from: id(2), msg: ring(5) });
        s.handle(Event::UserVerdict(true));
        let out = s.handle(Event::Hangup);
        assert!(out.contains(&Action::Send { to: id(2), msg: Msg::Bye }));
        assert!(out.contains(&Action::StopMedia { peer: id(2) }));
        assert!(s.is_idle());
    }

    #[test]
    fn ringing_someone_already_in_a_call_gets_busy_and_a_missed_call() {
        let mut s = state(1);
        s.ring_unknown = true;
        s.handle(Event::Rx { from: id(2), msg: ring(5) });
        s.handle(Event::UserVerdict(true));
        let out = s.handle(Event::Rx { from: id(7), msg: ring(42) });
        assert!(out.contains(&Action::Send { to: id(7), msg: Msg::Busy }));
        assert!(out.iter().any(|a| matches!(a, Action::Notify { .. })));
    }

    #[test]
    fn stray_accept_in_idle_is_answered_with_bye() {
        let mut s = state(1);
        let out = s.handle(Event::Rx {
            from: id(2),
            msg: Msg::Accept { name: "late".into(), video: Codec::Vp8 },
        });
        assert!(out.contains(&Action::Send { to: id(2), msg: Msg::Bye }));
    }

    #[test]
    fn a_caller_with_no_common_codec_is_declined() {
        let mut s = CallState::new(id(1), "me", vec![Codec::Vp8]);
        s.ring_unknown = true;
        let out = s.handle(Event::Rx {
            from: id(2),
            msg: Msg::Ring { call_id: 1, name: "them".into(), video: vec![Codec::H264], roster: vec![] },
        });
        assert!(out.contains(&Action::Send { to: id(2), msg: Msg::Decline }));
    }
}
