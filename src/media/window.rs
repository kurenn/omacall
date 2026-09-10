//! One window, every participant.
//!
//! A single `compositor` mixes the local camera and every remote stream into
//! one frame, so a call is one surface rather than a pile of windows. v1 opened
//! two per side; a three-way call would have opened six.
//!
//! Peers join and leave a *running* pipeline, which means requesting and
//! releasing compositor pads live. That is the part `gst-launch` cannot express
//! at all, and the clearest single argument for building this in Rust.
//!
//! For now the peer branches read from the Stage 1 tunnel's UDP ports. Stage 2
//! proper replaces `udpsrc` with `appsrc` fed directly from iroh datagrams; the
//! pad choreography here does not change when that happens.

use std::collections::HashMap;

use anyhow::{Context, Result};
use gstreamer as gst;
use gst::prelude::*;

use super::layout::{self, Tile, CANVAS_H, CANVAS_W};

/// Aggregators compute their latency from the branches present when they start.
/// Every remote branch carries a 120ms jitterbuffer and is always added later,
/// so without this they deliver late and get dropped -- which looks exactly like
/// a network fault and would be debugged as one for days.
const MIN_UPSTREAM_LATENCY_NS: u64 = 250_000_000;

const VCAPS: &str = "application/x-rtp,media=(string)video,encoding-name=(string)VP8,payload=(int)96";
const ACAPS: &str = "application/x-rtp,media=(string)audio,encoding-name=(string)OPUS,payload=(int)97,clock-rate=(int)48000";

struct Branch {
    bin: gst::Bin,
    video_pad: gst::Pad,
    audio_bin: Option<gst::Bin>,
    audio_pad: Option<gst::Pad>,
}

pub struct CallWindow {
    pipeline: gst::Pipeline,
    compositor: gst::Element,
    audiomixer: gst::Element,
    /// Insertion order decides tile assignment, so this is a Vec, not a set.
    order: Vec<String>,
    peers: HashMap<String, Branch>,
    self_pad: gst::Pad,
}

impl CallWindow {
    /// Build the window with just the local camera in it.
    ///
    /// `video_src` is a gstreamer launch fragment so tests can substitute
    /// `videotestsrc` -- V4L2 allows one opener, so a test that grabbed the real
    /// camera would fight any call in progress.
    pub fn new(video_src: &str, sink: &str) -> Result<Self> {
        gst::init().context("initialising gstreamer")?;

        let desc = format!(
            "compositor name=mix background=black min-upstream-latency={MIN_UPSTREAM_LATENCY_NS} \
               ! video/x-raw,width={CANVAS_W},height={CANVAS_H} ! videoconvert ! {sink} name=sink \
             audiomixer name=amix min-upstream-latency={MIN_UPSTREAM_LATENCY_NS} \
               ! audioconvert ! audioresample ! autoaudiosink sync=false \
             {video_src} ! videoconvert ! videoscale ! video/x-raw,width={CANVAS_W},height={CANVAS_H} \
               ! queue ! mix."
        );

        let pipeline = gst::parse::launch(&desc)
            .context("building the call window pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("not a pipeline"))?;

        let compositor = pipeline.by_name("mix").context("no compositor")?;
        let audiomixer = pipeline.by_name("amix").context("no audiomixer")?;
        // parse::launch names the first requested pad sink_0, but do not rely
        // on that: fall back to whatever sink pad actually exists.
        let self_pad = compositor
            .static_pad("sink_0")
            .or_else(|| compositor.iterate_sink_pads().into_iter().flatten().next())
            .context("the self-view never got a compositor pad")?;

        let mut w = Self {
            pipeline,
            compositor,
            audiomixer,
            order: Vec::new(),
            peers: HashMap::new(),
            self_pad,
        };
        w.relayout();
        Ok(w)
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

    /// Add a participant to the running pipeline and give them a tile.
    pub fn add_peer(&mut self, name: &str, video_port: u16, audio_port: u16) -> Result<()> {
        if self.peers.contains_key(name) {
            return Ok(());
        }

        let vdesc = format!(
            "udpsrc port={video_port} caps=\"{VCAPS}\" ! rtpjitterbuffer latency=120 do-lost=true \
             ! rtpvp8depay ! vp8dec ! videoconvert ! videoscale ! queue"
        );
        let bin = gst::parse::bin_from_description(&vdesc, true)
            .context("building the peer's video branch")?;
        self.pipeline.add(&bin)?;

        let pad = self
            .compositor
            .request_pad_simple("sink_%u")
            .context("compositor refused a new pad")?;
        bin.static_pad("src")
            .context("peer branch has no src pad")?
            .link(&pad)?;

        let adesc = format!(
            "udpsrc port={audio_port} caps=\"{ACAPS}\" ! rtpjitterbuffer latency=120 \
             ! rtpopusdepay ! opusdec ! audioconvert ! audioresample \
             ! audio/x-raw,rate=48000,channels=2 ! queue"
        );
        // audiomixer does not convert, so a second joiner with a different
        // channel layout would simply refuse to link without the chain above.
        let (audio_bin, audio_pad) = match gst::parse::bin_from_description(&adesc, true) {
            Ok(abin) => {
                self.pipeline.add(&abin)?;
                let apad = self.audiomixer.request_pad_simple("sink_%u");
                if let (Some(apad), Some(src)) = (apad.clone(), abin.static_pad("src")) {
                    src.link(&apad)?;
                }
                (Some(abin), apad)
            }
            Err(e) => {
                tracing::warn!("no audio branch for {name}: {e}");
                (None, None)
            }
        };

        bin.sync_state_with_parent()?;
        if let Some(a) = &audio_bin {
            a.sync_state_with_parent()?;
        }

        self.order.push(name.to_string());
        self.peers.insert(
            name.to_string(),
            Branch { bin, video_pad: pad, audio_bin, audio_pad },
        );
        self.relayout();
        Ok(())
    }

    /// Remove a participant from the running pipeline.
    ///
    /// The ordering matters and is easy to get backwards: send EOS *and then*
    /// wait for it on the compositor pad. A block probe added after EOS has
    /// already passed never fires, because nothing else will ever traverse that
    /// pad -- the tile then freezes forever and the bin is never released.
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

        self.relayout();
        Ok(())
    }

    /// Write the current geometry onto the live compositor pads.
    fn relayout(&mut self) {
        let tiles = layout::layout(self.peers.len());
        apply(&self.self_pad, &tiles[0]);
        for (i, name) in self.order.iter().enumerate() {
            if let (Some(branch), Some(tile)) = (self.peers.get(name), tiles.get(i + 1)) {
                apply(&branch.video_pad, tile);
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

impl Drop for CallWindow {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Headless, and never touches the camera: V4L2 allows one opener, so a
    /// test that grabbed it would fight a call in progress.
    fn window() -> Result<CallWindow> {
        CallWindow::new("videotestsrc is-live=true pattern=black", "fakesink sync=false")
    }

    #[test]
    fn builds_with_only_the_local_camera() {
        let w = window().expect("pipeline should build");
        assert_eq!(w.peer_count(), 0);
    }

    #[test]
    fn peers_can_join_and_leave_a_running_pipeline() {
        let mut w = window().unwrap();
        w.start().unwrap();

        w.add_peer("carlos", 15000, 15002).unwrap();
        assert_eq!(w.peer_count(), 1);
        w.add_peer("ana", 15004, 15006).unwrap();
        assert_eq!(w.peer_count(), 2);

        w.remove_peer("carlos").unwrap();
        assert_eq!(w.peer_count(), 1);
        w.remove_peer("ana").unwrap();
        assert_eq!(w.peer_count(), 0);

        w.stop().unwrap();
    }

    #[test]
    fn repeated_join_and_leave_neither_leaks_nor_deadlocks() {
        // The churn test the plan asks for: this is where a wrong pad-removal
        // recipe hangs on the first iteration.
        let mut w = window().unwrap();
        w.start().unwrap();
        for i in 0..12 {
            w.add_peer("churn", 15100, 15102).unwrap();
            assert_eq!(w.peer_count(), 1, "iteration {i}");
            w.remove_peer("churn").unwrap();
            assert_eq!(w.peer_count(), 0, "iteration {i}");
        }
        w.stop().unwrap();
    }

    #[test]
    fn adding_the_same_peer_twice_is_harmless() {
        let mut w = window().unwrap();
        w.start().unwrap();
        w.add_peer("carlos", 15200, 15202).unwrap();
        w.add_peer("carlos", 15200, 15202).unwrap();
        assert_eq!(w.peer_count(), 1, "a duplicate join must not double the tile");
        w.stop().unwrap();
    }

    #[test]
    fn removing_someone_who_is_not_there_is_harmless() {
        let mut w = window().unwrap();
        w.remove_peer("nobody").expect("a stray Bye must not be fatal");
    }
}
