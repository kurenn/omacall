// Visual check of §07: one window, tiles appearing and disappearing live.
use omacall::media::window::CallWindow;
use std::{thread::sleep, time::Duration};

fn main() -> anyhow::Result<()> {
    let mut w = CallWindow::new("videotestsrc is-live=true pattern=smpte", "waylandsink")?;
    w.start()?;
    println!("self only  -> full frame");
    sleep(Duration::from_secs(3));

    for (i, (name, v, a)) in [("carlos", 16000u16, 16002u16), ("ana", 16004, 16006)].iter().enumerate() {
        w.add_peer(name, *v, *a)?;
        println!("+{name}      -> {} remote, relayout", i + 1);
        sleep(Duration::from_secs(3));
    }
    w.remove_peer("carlos")?;
    println!("-carlos     -> back to 1 remote");
    sleep(Duration::from_secs(3));
    w.stop()?;
    println!("closed cleanly, peers left: {}", w.peer_count());
    Ok(())
}
