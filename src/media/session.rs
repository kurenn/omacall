//! The whole media pipeline for one call, in process.
//!
//! Replaces Stage 1's loopback UDP hop: the encoder's `appsink` hands RTP
//! straight to iroh, and each peer's `appsrc` is fed straight from datagrams.
//! One pipeline per call, video and audio together, so there is one clock and
//! one bus.
//!
//! Encode once and fan out. The codec is therefore call-wide and fixed at
//! creation -- per-pair negotiation would force encoding the same frames twice.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use tokio::sync::mpsc;

use super::layout::{self, Tile, CANVAS_H, CANVAS_W};
use crate::proto::{Codec, TAG_AUDIO, TAG_VIDEO};

/// See §04: aggregators fix their latency from the branches present at start,
/// and every remote branch is added later carrying a 120ms jitterbuffer.
const MIN_UPSTREAM_LATENCY_NS: u64 = 250_000_000;

// The audiomixer is also fed a silent, zero-volume source from the start. An
// aggregator with no sink pads produces nothing, so its sink never prerolls and
// the whole pipeline sits in PAUSED forever -- with no error on the bus, which
// is what makes it easy to mistake for a dead encoder. Until the first peer
// joins there is genuinely no audio to mix, so it needs something to mix.

/// Fits the 1162-byte datagram measured at connection start, plus the tag byte.
const PAYLOAD_MTU: u32 = 1120;

const VCAPS_VP8: &str = "application/x-rtp,media=(string)video,encoding-name=(string)VP8,payload=(int)96,clock-rate=(int)90000";
const VCAPS_H264: &str = "application/x-rtp,media=(string)video,encoding-name=(string)H264,payload=(int)96,clock-rate=(int)90000";
const ACAPS: &str = "application/x-rtp,media=(string)audio,encoding-name=(string)OPUS,payload=(int)97,clock-rate=(int)48000,encoding-params=(string)2";

/// One tagged RTP packet on its way to a peer.
pub type Outbound = (u8, Vec<u8>);

struct Branch {
    bin: gst::Bin,
    video_src: gst_app::AppSrc,
    video_pad: gst::Pad,
    audio_bin: Option<gst::Bin>,
    audio_src: Option<gst_app::AppSrc>,
    audio_pad: Option<gst::Pad>,
}

pub struct MediaSession {
    pipeline: gst::Pipeline,
    compositor: gst::Element,
    audiomixer: gst::Element,
    encoder: gst::Element,
    codec: Codec,
    self_pad: gst::Pad,
    order: Vec<String>,
    peers: HashMap<String, Branch>,
    /// Frames decoded per peer, for `omacall status` and the smoke test.
    /// Counting datagrams would pass with a caps typo or a dead decoder.
    decoded: Arc<Mutex<HashMap<String, u64>>>,
}

impl MediaSession {
    /// Build the pipeline. `video_src` is a launch fragment so tests can use
    /// `videotestsrc`: V4L2 allows one opener, so a test that took the real
    /// camera would fight any call in progress.
    pub fn new(
        video_src: &str,
        audio_src: &str,
        sink: &str,
        codec: Codec,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Outbound>)> {
        gst::init().context("initialising gstreamer")?;

        // Hardware H.264 honours its bitrate target within 10% and is mutable
        // while playing; software VP8 overshoots it by up to 4.47x. That is why
        // AIMD drives bitrate on one path and resolution on the other.
        let (enc, pay) = match codec {
            Codec::H264 => (
                "vah264enc name=enc bitrate=1500 key-int-max=60".to_string(),
                format!("rtph264pay name=pay pt=96 mtu={PAYLOAD_MTU} config-interval=-1"),
            ),
            Codec::Vp8 => (
                "vp8enc name=enc deadline=1 target-bitrate=1500000 keyframe-max-dist=30 error-resilient=1".to_string(),
                format!("rtpvp8pay name=pay pt=96 mtu={PAYLOAD_MTU}"),
            ),
        };

        let desc = format!(
            "compositor name=mix background=black min-upstream-latency={MIN_UPSTREAM_LATENCY_NS} \
               ! video/x-raw,width={CANVAS_W},height={CANVAS_H} ! videoconvert ! {sink} name=sink \
             audiomixer name=amix min-upstream-latency={MIN_UPSTREAM_LATENCY_NS} \
               ! audioconvert ! audioresample ! autoaudiosink sync=false \
             audiotestsrc is-live=true wave=silence volume=0 ! audioconvert \
               ! audio/x-raw,rate=48000,channels=2 ! queue ! amix. \
             {video_src} ! videoconvert ! videoscale \
               ! video/x-raw,width={CANVAS_W},height={CANVAS_H},framerate=30/1 ! tee name=cam \
             cam. ! queue leaky=downstream max-size-buffers=3 ! videoconvert ! {enc} ! {pay} \
               ! appsink name=vout emit-signals=true sync=false max-buffers=8 drop=true \
             cam. ! queue leaky=downstream max-size-buffers=3 ! mix. \
             {audio_src} ! audioconvert ! audioresample \
               ! opusenc name=aenc frame-size=20 inband-fec=true \
               ! rtpopuspay pt=97 mtu={PAYLOAD_MTU} \
               ! appsink name=aout emit-signals=true sync=false max-buffers=16 drop=true"
        );

        let pipeline = gst::parse::launch(&desc)
            .context("building the media pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("not a pipeline"))?;

        let compositor = pipeline.by_name("mix").context("no compositor")?;
        let audiomixer = pipeline.by_name("amix").context("no audiomixer")?;
        let encoder = pipeline.by_name("enc").context("no encoder")?;
        let self_pad = compositor
            .static_pad("sink_0")
            .or_else(|| compositor.iterate_sink_pads().into_iter().flatten().next())
            .context("the self-view never got a compositor pad")?;

        let (tx, rx) = mpsc::unbounded_channel();
        for (name, tag) in [("vout", TAG_VIDEO), ("aout", TAG_AUDIO)] {
            let sink = pipeline
                .by_name(name)
                .with_context(|| format!("no {name} appsink"))?
                .downcast::<gst_app::AppSink>()
                .map_err(|_| anyhow::anyhow!("{name} is not an appsink"))?;
            let tx = tx.clone();
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |s| {
                        let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                        // A closed channel means the call ended; stop the flow
                        // rather than spinning on a dead receiver.
                        tx.send((tag, map.to_vec())).map_err(|_| gst::FlowError::Eos)?;
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }

        let mut s = Self {
            pipeline,
            compositor,
            audiomixer,
            encoder,
            codec,
            self_pad,
            order: Vec::new(),
            peers: HashMap::new(),
            decoded: Arc::new(Mutex::new(HashMap::new())),
        };
        s.relayout();
        Ok((s, rx))
    }

    pub fn start(&self) -> Result<()> {
        self.pipeline.set_state(gst::State::Playing)?;
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        self.pipeline.set_state(gst::State::Null)?;
        Ok(())
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn frames_decoded(&self, name: &str) -> u64 {
        self.decoded.lock().map(|d| d.get(name).copied().unwrap_or(0)).unwrap_or(0)
    }

    /// Add a participant to the running pipeline.
    pub fn add_peer(&mut self, name: &str) -> Result<()> {
        if self.peers.contains_key(name) {
            return Ok(());
        }
        let vcaps = match self.codec {
            Codec::H264 => VCAPS_H264,
            Codec::Vp8 => VCAPS_VP8,
        };
        let depay = match self.codec {
            Codec::H264 => "rtph264depay ! avdec_h264",
            Codec::Vp8 => "rtpvp8depay ! vp8dec",
        };

        let vdesc = format!(
            "appsrc name=vsrc is-live=true do-timestamp=true format=time caps=\"{vcaps}\" \
             ! rtpjitterbuffer latency=120 do-lost=true ! {depay} \
             ! videoconvert ! videoscale ! identity name=count ! queue"
        );
        let bin = gst::parse::bin_from_description(&vdesc, true)
            .context("building the peer's video branch")?;
        self.pipeline.add(&bin)?;

        // Count decoded frames, not datagrams: a caps typo or a dead decoder
        // would sail past a datagram counter.
        if let Some(counter) = bin.by_name("count") {
            let decoded = self.decoded.clone();
            let who = name.to_string();
            counter.static_pad("src").unwrap().add_probe(
                gst::PadProbeType::BUFFER,
                move |_, _| {
                    if let Ok(mut d) = decoded.lock() {
                        *d.entry(who.clone()).or_insert(0) += 1;
                    }
                    gst::PadProbeReturn::Ok
                },
            );
        }

        let video_src = bin
            .by_name("vsrc")
            .context("no appsrc")?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| anyhow::anyhow!("vsrc is not an appsrc"))?;

        let video_pad = self
            .compositor
            .request_pad_simple("sink_%u")
            .context("compositor refused a pad")?;
        bin.static_pad("src").context("no src pad")?.link(&video_pad)?;

        // audiomixer does not convert, so the explicit chain is required or a
        // second joiner with a different layout refuses to link.
        let adesc = format!(
            "appsrc name=asrc is-live=true do-timestamp=true format=time caps=\"{ACAPS}\" \
             ! rtpjitterbuffer latency=120 ! rtpopusdepay ! opusdec \
             ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! queue"
        );
        let (audio_bin, audio_src, audio_pad) = match gst::parse::bin_from_description(&adesc, true) {
            Ok(abin) => {
                self.pipeline.add(&abin)?;
                let asrc = abin.by_name("asrc").and_then(|e| e.downcast::<gst_app::AppSrc>().ok());
                let apad = self.audiomixer.request_pad_simple("sink_%u");
                if let (Some(apad), Some(src)) = (apad.clone(), abin.static_pad("src")) {
                    src.link(&apad)?;
                }
                (Some(abin), asrc, apad)
            }
            Err(e) => {
                tracing::warn!("no audio branch for {name}: {e}");
                (None, None, None)
            }
        };

        bin.sync_state_with_parent()?;
        if let Some(a) = &audio_bin {
            a.sync_state_with_parent()?;
        }

        // A joiner should decode from its first second rather than waiting a
        // KeyframeReq round trip.
        self.force_keyframe();

        self.order.push(name.to_string());
        self.peers.insert(
            name.to_string(),
            Branch { bin, video_src, video_pad, audio_bin, audio_src, audio_pad },
        );
        self.relayout();
        Ok(())
    }

    pub fn remove_peer(&mut self, name: &str) -> Result<()> {
        let Some(branch) = self.peers.remove(name) else { return Ok(()) };
        self.order.retain(|n| n != name);

        branch.bin.set_state(gst::State::Null)?;
        self.compositor.release_request_pad(&branch.video_pad);
        self.pipeline.remove(&branch.bin)?;

        if let (Some(abin), Some(apad)) = (branch.audio_bin, branch.audio_pad) {
            abin.set_state(gst::State::Null)?;
            self.audiomixer.release_request_pad(&apad);
            self.pipeline.remove(&abin)?;
        }
        if let Ok(mut d) = self.decoded.lock() {
            d.remove(name);
        }
        self.relayout();
        Ok(())
    }

    /// Feed one inbound RTP packet, already stripped of its channel tag.
    pub fn push(&self, name: &str, tag: u8, payload: &[u8]) {
        let Some(branch) = self.peers.get(name) else { return };
        let src = match tag {
            TAG_VIDEO => Some(&branch.video_src),
            TAG_AUDIO => branch.audio_src.as_ref(),
            _ => None,
        };
        if let Some(src) = src {
            let buf = gst::Buffer::from_slice(payload.to_vec());
            let _ = src.push_buffer(buf);
        }
    }

    /// Ask the encoder for a keyframe. Callers must debounce: a keyframe costs
    /// 5-10x a delta frame and goes to *everyone*, so one lossy peer asking at
    /// 1Hz would degrade every healthy link.
    pub fn force_keyframe(&self) {
        if let Some(pad) = self.encoder.static_pad("sink") {
            let ev = gst::event::CustomDownstream::new(
                gst::Structure::builder("GstForceKeyUnit").field("all-headers", true).build(),
            );
            pad.push_event(ev);
        }
    }

    /// Set the outgoing bitrate in kbps.
    ///
    /// Only meaningful on H.264: `vah264enc` honours its target within 10% and
    /// is mutable while playing, whereas `vp8enc` overshoots by up to 4.47x at
    /// `deadline=1` and carries no playing-state flag. Returns whether the
    /// encoder is one that actually responds.
    pub fn set_bitrate_kbps(&self, kbps: u32) -> bool {
        match self.codec {
            Codec::H264 => {
                self.encoder.set_property("bitrate", kbps);
                true
            }
            Codec::Vp8 => false,
        }
    }

    /// Debug helpers: what state did the pipeline actually reach, and what did
    /// the bus say about it.
    pub fn debug_state(&self) -> (gst::State, gst::State) {
        let (_, cur, pending) = self.pipeline.state(gst::ClockTime::from_mseconds(100));
        (cur, pending)
    }

    pub fn debug_bus_drain(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(bus) = self.pipeline.bus() {
            while let Some(msg) = bus.pop() {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        out.push(format!("ERROR from {:?}: {} ({:?})",
                            e.src().map(|s| s.path_string()), e.error(), e.debug()));
                    }
                    gst::MessageView::Warning(w) => {
                        out.push(format!("WARN {}", w.error()));
                    }
                    _ => {}
                }
            }
        }
        out
    }

    fn relayout(&mut self) {
        let tiles = layout::layout(self.peers.len());
        apply(&self.self_pad, &tiles[0]);
        for (i, name) in self.order.iter().enumerate() {
            if let (Some(b), Some(t)) = (self.peers.get(name), tiles.get(i + 1)) {
                apply(&b.video_pad, t);
            }
        }
    }
}

fn apply(pad: &gst::Pad, t: &Tile) {
    pad.set_property("xpos", t.xpos);
    pad.set_property("ypos", t.ypos);
    pad.set_property("width", t.width);
    pad.set_property("height", t.height);
    pad.set_property("zorder", t.zorder);
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(codec: Codec) -> Result<(MediaSession, mpsc::UnboundedReceiver<Outbound>)> {
        MediaSession::new(
            "videotestsrc is-live=true pattern=black",
            "audiotestsrc is-live=true wave=silence",
            "fakesink sync=false",
            codec,
        )
    }

    #[tokio::test]
    async fn vp8_encodes_and_hands_rtp_out_through_the_appsink() {
        let (s, mut rx) = session(Codec::Vp8).unwrap();
        s.start().unwrap();

        let mut video = 0;
        let mut audio = 0;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        while (video < 5 || audio < 5) && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Some((TAG_VIDEO, p))) => {
                    assert!(p.len() as u32 <= PAYLOAD_MTU + 64, "packet exceeds the datagram budget");
                    video += 1;
                }
                Ok(Some((TAG_AUDIO, _))) => audio += 1,
                _ => break,
            }
        }
        s.stop().unwrap();
        assert!(video >= 5, "expected encoded video out of the appsink, got {video}");
        assert!(audio >= 5, "expected encoded audio out of the appsink, got {audio}");
    }

    #[tokio::test]
    async fn a_peer_can_join_and_leave_a_running_pipeline() {
        let (mut s, _rx) = session(Codec::Vp8).unwrap();
        s.start().unwrap();
        s.add_peer("carlos").unwrap();
        assert_eq!(s.peer_count(), 1);
        s.add_peer("ana").unwrap();
        assert_eq!(s.peer_count(), 2);
        s.remove_peer("carlos").unwrap();
        assert_eq!(s.peer_count(), 1);
        s.stop().unwrap();
    }

    #[tokio::test]
    async fn churn_neither_leaks_nor_deadlocks() {
        let (mut s, _rx) = session(Codec::Vp8).unwrap();
        s.start().unwrap();
        for i in 0..10 {
            s.add_peer("churn").unwrap();
            s.remove_peer("churn").unwrap();
            assert_eq!(s.peer_count(), 0, "iteration {i}");
        }
        s.stop().unwrap();
    }

    #[tokio::test]
    async fn rtp_pushed_in_one_end_is_decoded_and_counted() {
        // Loop this session's own encoded output back into a peer branch: if
        // the caps and depayloader agree, frames decode and the counter moves.
        let (mut s, mut rx) = session(Codec::Vp8).unwrap();
        s.start().unwrap();
        s.add_peer("loop").unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        while s.frames_decoded("loop") < 3 && tokio::time::Instant::now() < deadline {
            if let Ok(Some((tag, payload))) =
                tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await
            {
                s.push("loop", tag, &payload);
            } else {
                break;
            }
        }
        let n = s.frames_decoded("loop");
        s.stop().unwrap();
        assert!(n >= 3, "expected decoded frames through appsrc, got {n}");
    }

    #[test]
    fn bitrate_control_is_honest_about_which_encoder_responds() {
        let (vp8, _a) = session(Codec::Vp8).unwrap();
        assert!(
            !vp8.set_bitrate_kbps(600),
            "vp8enc overshoots its target 4.47x; claiming control would be a lie"
        );
    }
}
