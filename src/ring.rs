//! Ringing the human.
//!
//! Keeps v1's approach wholesale, because it works: a critical notification, a
//! ringtone, and Omarchy's floating terminal running `gum confirm`. What
//! disappears is everything around it -- the daemon already lives in the
//! graphical session, so v1's `systemctl --user show-environment` import is
//! gone along with sshd, key auth and the login account on the callee.
//!
//! Two things the review caught are fixed here. `pw-play` has no `--loop`
//! flag, so the ringtone needs a respawn loop. And a cancelled ring has to tear
//! down both the tone and the dialog: otherwise the prompt outlives the call,
//! and a later Answer arrives at a peer that has already gone Idle.
//!
//! Stage 3 replaces the terminal prompt with the plugin's QML overlay. This
//! stays as the fallback for a machine with the binary but no plugin, which is
//! also what makes the daemon usable headless.

use std::{path::PathBuf, process::Stdio, time::Duration};

use anyhow::Result;
use tokio::{process::Command, sync::oneshot, task::JoinSet};

const RINGTONE: &str = "/usr/share/sounds/freedesktop/stereo/phone-incoming-call.oga";

/// A ring in progress. Dropping it stops the tone and closes the prompt.
pub struct Ring {
    tasks: JoinSet<()>,
    verdict_file: PathBuf,
}

impl Ring {
    /// Ring for `caller`, resolving to true if the user answered.
    ///
    /// The prompt runs in a detached terminal, so the answer comes back through
    /// a file rather than an exit code.
    pub fn start(caller: &str, timeout: Duration) -> (Self, oneshot::Receiver<bool>) {
        let (tx, rx) = oneshot::channel();
        let mut tasks = JoinSet::new();

        // Unit tests must not pop terminals onto the user's desktop or play
        // a ringtone; the verdict-file mechanism is what they exercise.
        let quiet = cfg!(test) || std::env::var("OMACALL_RING_SILENT").is_ok();

        if !quiet {
            notify_critical(caller);
        }

        // pw-play exits after one play; loop it until the ring is dropped.
        tasks.spawn(async move {
            if quiet {
                return;
            }
            loop {
                let played = Command::new("pw-play")
                    .arg(RINGTONE)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
                if played.is_err() {
                    return; // no pw-play: silence, not a dead ring
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });

        let verdict_file = std::env::temp_dir().join(format!("omacall-verdict-{}", std::process::id()));
        let _ = std::fs::remove_file(&verdict_file);

        let prompt = format!(
            "gum confirm 'Incoming call from {}. Answer?' && echo yes > {} || echo no > {}",
            caller.replace('\'', ""),
            verdict_file.display(),
            verdict_file.display()
        );
        tasks.spawn(async move {
            if quiet {
                return;
            }
            let _ = Command::new("omarchy-launch-floating-terminal-with-presentation")
                .arg(prompt)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        });

        let watch = verdict_file.clone();
        tasks.spawn(async move {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                if let Ok(s) = tokio::fs::read_to_string(&watch).await {
                    if !s.trim().is_empty() {
                        let _ = tx.send(s.trim() == "yes");
                        return;
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    let _ = tx.send(false);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });

        (Self { tasks, verdict_file }, rx)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        self.tasks.abort_all();
        let _ = std::fs::remove_file(&self.verdict_file);
        if cfg!(test) || std::env::var("OMACALL_RING_SILENT").is_ok() {
            return;
        }
        // The floating terminal is detached, so aborting our task does not
        // close it. Writing a verdict makes the gum prompt's own shell exit.
        close_stale_prompt();
    }
}

fn close_stale_prompt() {
    // Best effort and deliberately narrow: match the exact prompt text rather
    // than anything broader. v1 killed its own shell twice with `pkill -f`.
    let _ = std::process::Command::new("pkill")
        .args(["-f", "gum confirm 'Incoming call from"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn notify_critical(caller: &str) {
    spawn_notify("-u", "critical", "Incoming call", &format!("{caller} is calling"));
}

/// A notification that is not a ring: missed calls, joins, aborts.
pub fn notify(title: &str, body: &str) {
    spawn_notify("-u", "normal", title, body);
}

fn spawn_notify(flag: &str, urgency: &str, title: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args([flag, urgency, "-a", "omacall", title, body])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Is there a graphical session to ring into?
///
/// The daemon runs as a user unit ordered after `graphical-session.target`, but
/// a hand-started one may have no display at all -- in which case the ring UI
/// would fail silently, which is the failure mode this whole project keeps
/// hitting.
pub fn has_display() -> Result<()> {
    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        anyhow::bail!(
            "no WAYLAND_DISPLAY or DISPLAY: the daemon cannot ring anyone. \
             Start it from your graphical session, or via the omacall.service user unit."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_display_check_actually_checks() {
        // This process has one in a normal session; the point is that the
        // function reads the environment rather than always returning Ok.
        let result = has_display();
        let expected = std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_ok();
        assert_eq!(result.is_ok(), expected);
    }

    #[tokio::test]
    async fn a_ring_that_nobody_answers_resolves_false() {
        let (_ring, rx) = Ring::start("nobody", Duration::from_millis(300));
        let answered = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("ring must resolve, not hang")
            .expect("sender must not be dropped");
        assert!(!answered, "an unanswered ring is a decline");
    }

    #[tokio::test]
    async fn dropping_a_ring_removes_its_verdict_file() {
        let (ring, _rx) = Ring::start("someone", Duration::from_secs(30));
        let path = ring.verdict_file.clone();
        drop(ring);
        assert!(!path.exists(), "a cancelled ring must not leave state behind");
    }
}
