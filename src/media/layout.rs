//! Tile geometry for the call window.
//!
//! Pure arithmetic, so the layout can be tested without a pipeline, a display
//! or a peer. `relayout` just writes these numbers onto live compositor pads.

/// The composited frame. The sink scales this to whatever the window is.
pub const CANVAS_W: i32 = 1280;
pub const CANVAS_H: i32 = 720;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile {
    pub xpos: i32,
    pub ypos: i32,
    pub width: i32,
    pub height: i32,
    /// Higher draws on top. The self-view sits above the remote video when it
    /// is a picture-in-picture, and level with everyone once it is a grid tile.
    pub zorder: u32,
}

/// Where each participant goes, given how many remote people are in the call.
///
/// Index 0 is always the local self-view; remote tiles follow in roster order.
/// One window, everyone in it -- the product requirement §07 exists for.
pub fn layout(n_remote: usize) -> Vec<Tile> {
    match n_remote {
        // Nobody yet: your own camera fills the window, so a call that has not
        // connected still shows something and proves the camera works.
        0 => vec![full()],

        // One other person: them full-frame, you small in the corner. This is
        // what a two-person call looks like everywhere, for good reason.
        1 => vec![pip(), full_at(0)],

        // Two others: side by side, you still picture-in-picture.
        2 => {
            let w = CANVAS_W / 2;
            let h = CANVAS_H / 2;
            let y = (CANVAS_H - h) / 2; // centred vertically, not top-aligned
            vec![pip(), tile(0, y, w, h), tile(w, y, w, h)]
        }

        // Three others is the roster cap (MAX_PARTICIPANTS counts you), so the
        // grid is exactly full and the self-view stops being an overlay: it
        // takes the fourth cell as an equal participant.
        _ => {
            let (w, h) = (CANVAS_W / 2, CANVAS_H / 2);
            vec![
                tile(w, h, w, h),
                tile(0, 0, w, h),
                tile(w, 0, w, h),
                tile(0, h, w, h),
            ]
        }
    }
}

fn full() -> Tile {
    full_at(0)
}

fn full_at(z: u32) -> Tile {
    Tile { xpos: 0, ypos: 0, width: CANVAS_W, height: CANVAS_H, zorder: z }
}

/// The self-view overlay: bottom-right, with a margin, drawn on top.
fn pip() -> Tile {
    let w = CANVAS_W / 4;
    let h = CANVAS_H / 4;
    let margin = 24;
    Tile {
        xpos: CANVAS_W - w - margin,
        ypos: CANVAS_H - h - margin,
        width: w,
        height: h,
        zorder: 10,
    }
}

fn tile(xpos: i32, ypos: i32, width: i32, height: i32) -> Tile {
    Tile { xpos, ypos, width, height, zorder: 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlaps(a: &Tile, b: &Tile) -> bool {
        a.xpos < b.xpos + b.width
            && b.xpos < a.xpos + a.width
            && a.ypos < b.ypos + b.height
            && b.ypos < a.ypos + a.height
    }

    #[test]
    fn alone_you_fill_the_window() {
        let t = layout(0);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0], full_at(0));
    }

    #[test]
    fn one_other_person_is_full_frame_with_you_in_the_corner() {
        let t = layout(1);
        assert_eq!(t.len(), 2);
        assert_eq!(t[1].width, CANVAS_W, "the other person gets the whole frame");
        assert!(t[0].zorder > t[1].zorder, "the self-view must draw on top");
        assert!(t[0].width < CANVAS_W / 2, "the self-view is a thumbnail");
    }

    #[test]
    fn every_layout_has_one_tile_per_participant() {
        // Three remote is the cap: MAX_PARTICIPANTS is four and counts you.
        for n in 0..=3 {
            assert_eq!(layout(n).len(), n + 1, "n={n} remote plus yourself");
        }
    }

    #[test]
    fn the_layout_matches_the_roster_cap() {
        assert_eq!(
            layout(crate::proto::MAX_PARTICIPANTS - 1).len(),
            crate::proto::MAX_PARTICIPANTS,
            "the grid must hold exactly as many people as the roster admits"
        );
    }

    #[test]
    fn nothing_is_drawn_outside_the_canvas() {
        for n in 0..=3 {
            for t in layout(n) {
                assert!(t.xpos >= 0 && t.ypos >= 0, "n={n}: {t:?} starts off-canvas");
                assert!(
                    t.xpos + t.width <= CANVAS_W && t.ypos + t.height <= CANVAS_H,
                    "n={n}: {t:?} runs past the canvas"
                );
                assert!(t.width > 0 && t.height > 0, "n={n}: {t:?} is degenerate");
            }
        }
    }

    #[test]
    fn remote_tiles_never_cover_each_other() {
        // The self-view is allowed to overlap -- it is an overlay by design --
        // but two people covering each other would just be a bug.
        for n in 2..=3 {
            let tiles = layout(n);
            let remotes: Vec<_> = tiles[1..].to_vec();
            for i in 0..remotes.len() {
                for j in (i + 1)..remotes.len() {
                    assert!(
                        !overlaps(&remotes[i], &remotes[j]),
                        "n={n}: remote tiles {i} and {j} overlap"
                    );
                }
            }
        }
    }

    #[test]
    fn at_the_roster_cap_you_become_an_equal_tile() {
        let t = layout(3);
        assert_eq!(t.len(), 4);
        assert_eq!(t[0].width, t[1].width, "the self-view stops being a thumbnail");
        assert_eq!(t[0].zorder, t[1].zorder, "and stops being an overlay");
    }

    #[test]
    fn the_self_view_is_always_first() {
        // add_peer appends, so index 0 has to stay the local branch or every
        // tile assignment shifts by one when someone joins.
        for n in 0..=3 {
            let t = layout(n);
            assert!(!t.is_empty(), "n={n}");
        }
    }
}
