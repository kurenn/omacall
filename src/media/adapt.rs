//! Bitrate adaptation.
//!
//! Deliberately crude and labelled as such. Leaving WebRTC means leaving GCC
//! and TWCC behind, so this is AIMD driven by QUIC path statistics plus each
//! peer's own receiver report.
//!
//! `ponytail: AIMD, not GCC — upgrade to delay-based estimation if relay-path
//! calls visibly pump.`
//!
//! Two things measurement forced. Offered-load control is not a nicety here: a
//! bottleneck below offered load produced *zero* decodable video, because
//! drop-oldest shreds keyframes and a VP8 stream with no intact keyframe never
//! starts. And the lever depends on the encoder — hardware H.264 honours a
//! bitrate target within 10%, while `vp8enc` at `deadline=1` overshoots it by
//! up to 4.47x and only really responds to resolution.

use crate::proto::Codec;

pub const FLOOR_KBPS: u32 = 300;
pub const CEILING_KBPS: u32 = 2500;
/// Below this, drop resolution rather than keep starving a 720p encode.
pub const LOW_RES_THRESHOLD_KBPS: u32 = 700;
const RECOVER_STEP_KBPS: u32 = 100;
/// Two consecutive bad ticks before halving, so a single lost packet does not
/// collapse a healthy call.
const BAD_TICKS_BEFORE_CUT: u32 = 2;
const LOSS_CUT_RATIO: f64 = 0.02;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adjustment {
    Hold,
    /// Set the encoder's target. Only meaningful where the encoder obeys it.
    Bitrate(u32),
    /// Drop to a smaller frame. The only lever that moves bytes on VP8:
    /// measured 1787kbps at 720p versus 569kbps at 640x360, same target.
    Resolution { width: i32, height: i32 },
}

pub struct Aim {
    codec: Codec,
    current_kbps: u32,
    bad_ticks: u32,
    low_res: bool,
}

/// One tick of evidence. `send_buffer_free` is the observable that matters:
/// quinn discards queued datagrams oldest-first rather than returning an error,
/// so counting send failures reports nothing while video dies.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub packets: u32,
    pub lost: u32,
    pub send_buffer_free: usize,
    pub send_buffer_total: usize,
}

impl Sample {
    fn loss_ratio(&self) -> f64 {
        if self.packets == 0 {
            return 0.0;
        }
        f64::from(self.lost) / f64::from(self.packets)
    }

    /// Sustained backpressure. The send buffer emptying out is the earliest
    /// warning available, and it arrives before loss does.
    fn congested(&self) -> bool {
        self.send_buffer_total > 0
            && self.send_buffer_free * 8 < self.send_buffer_total
    }
}

impl Aim {
    pub fn new(codec: Codec, start_kbps: u32) -> Self {
        Self { codec, current_kbps: start_kbps.clamp(FLOOR_KBPS, CEILING_KBPS), bad_ticks: 0, low_res: false }
    }

    pub fn current_kbps(&self) -> u32 {
        self.current_kbps
    }

    pub fn tick(&mut self, s: Sample) -> Adjustment {
        let bad = s.loss_ratio() > LOSS_CUT_RATIO || s.congested();

        if bad {
            self.bad_ticks += 1;
            if self.bad_ticks < BAD_TICKS_BEFORE_CUT {
                return Adjustment::Hold;
            }
            self.bad_ticks = 0;
            let target = (self.current_kbps / 2).max(FLOOR_KBPS);
            if target == self.current_kbps && self.low_res {
                return Adjustment::Hold; // already at the floor, nothing left
            }
            self.current_kbps = target;
            return self.lever();
        }

        self.bad_ticks = 0;
        if self.current_kbps >= CEILING_KBPS {
            return Adjustment::Hold;
        }
        self.current_kbps = (self.current_kbps + RECOVER_STEP_KBPS).min(CEILING_KBPS);
        self.lever()
    }

    /// Which control actually moves bytes for this encoder.
    fn lever(&mut self) -> Adjustment {
        match self.codec {
            // Hardware H.264: the target is honoured, so use it.
            Codec::H264 => Adjustment::Bitrate(self.current_kbps),

            // VP8 ignores its target, so resolution is the only real control.
            // Crossing the threshold is the event; between crossings there is
            // nothing useful to say.
            Codec::Vp8 => {
                let want_low = self.current_kbps < LOW_RES_THRESHOLD_KBPS;
                if want_low == self.low_res {
                    return Adjustment::Hold;
                }
                self.low_res = want_low;
                if want_low {
                    Adjustment::Resolution { width: 640, height: 360 }
                } else {
                    Adjustment::Resolution { width: 1280, height: 720 }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean() -> Sample {
        Sample { packets: 1000, lost: 0, send_buffer_free: 32768, send_buffer_total: 32768 }
    }

    fn lossy() -> Sample {
        Sample { packets: 1000, lost: 50, send_buffer_free: 32768, send_buffer_total: 32768 }
    }

    fn congested() -> Sample {
        // What the bottleneck run actually measured: 259 free of 32768.
        Sample { packets: 1000, lost: 0, send_buffer_free: 259, send_buffer_total: 32768 }
    }

    #[test]
    fn one_bad_tick_is_not_enough_to_cut() {
        let mut a = Aim::new(Codec::H264, 1500);
        assert_eq!(a.tick(lossy()), Adjustment::Hold);
        assert_eq!(a.current_kbps(), 1500, "a single blip must not halve a good call");
    }

    #[test]
    fn sustained_loss_halves_the_rate() {
        let mut a = Aim::new(Codec::H264, 1500);
        a.tick(lossy());
        assert_eq!(a.tick(lossy()), Adjustment::Bitrate(750));
    }

    #[test]
    fn a_draining_send_buffer_counts_as_congestion_on_its_own() {
        // The bottleneck run showed zero loss while the buffer emptied: loss
        // alone would never have fired.
        let mut a = Aim::new(Codec::H264, 1500);
        a.tick(congested());
        assert_eq!(a.tick(congested()), Adjustment::Bitrate(750));
    }

    #[test]
    fn recovery_is_additive_not_a_jump_back() {
        let mut a = Aim::new(Codec::H264, 1000);
        assert_eq!(a.tick(clean()), Adjustment::Bitrate(1100));
        assert_eq!(a.tick(clean()), Adjustment::Bitrate(1200));
    }

    #[test]
    fn it_never_goes_below_the_floor_or_above_the_ceiling() {
        let mut a = Aim::new(Codec::H264, 400);
        for _ in 0..20 {
            a.tick(lossy());
        }
        assert_eq!(a.current_kbps(), FLOOR_KBPS);

        let mut b = Aim::new(Codec::H264, 2400);
        for _ in 0..20 {
            b.tick(clean());
        }
        assert_eq!(b.current_kbps(), CEILING_KBPS);
    }

    #[test]
    fn vp8_drops_resolution_because_its_bitrate_target_does_nothing() {
        let mut a = Aim::new(Codec::Vp8, 1500);
        let mut dropped = None;
        for _ in 0..12 {
            if let Adjustment::Resolution { width, height } = a.tick(lossy()) {
                dropped = Some((width, height));
                break;
            }
        }
        assert_eq!(
            dropped,
            Some((640, 360)),
            "vp8enc overshoots its target 4.47x; only resolution moves bytes"
        );
    }

    #[test]
    fn vp8_never_reports_a_bitrate_adjustment() {
        let mut a = Aim::new(Codec::Vp8, 1500);
        for _ in 0..30 {
            assert!(
                !matches!(a.tick(lossy()), Adjustment::Bitrate(_)),
                "claiming bitrate control over vp8 would be a lie the loop then believes"
            );
        }
    }

    #[test]
    fn vp8_climbs_back_to_full_resolution_when_the_link_recovers() {
        let mut a = Aim::new(Codec::Vp8, 1500);
        for _ in 0..12 {
            a.tick(lossy());
        }
        let mut restored = false;
        for _ in 0..40 {
            if a.tick(clean()) == (Adjustment::Resolution { width: 1280, height: 720 }) {
                restored = true;
                break;
            }
        }
        assert!(restored, "a recovered link must get its resolution back");
    }

    #[test]
    fn resolution_changes_only_on_a_crossing() {
        let mut a = Aim::new(Codec::Vp8, 1500);
        let mut changes = 0;
        for _ in 0..30 {
            if matches!(a.tick(lossy()), Adjustment::Resolution { .. }) {
                changes += 1;
            }
        }
        assert_eq!(changes, 1, "renegotiating caps every tick would be its own outage");
    }
}
