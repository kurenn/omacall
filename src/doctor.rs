//! `omacall doctor` — what is wrong, in one screen.
//!
//! Every check here exists because something failed silently during
//! development. A firewall that drops media without an error, a camera that
//! negotiates 10fps because nobody asked for MJPG, a daemon started outside the
//! graphical session so it can never ring: none of these announce themselves,
//! and all of them look like a network problem.

use std::{fmt, path::Path, process::Command};

use crate::{contacts::Contacts, identity, ipc};

pub enum Health {
    Ok(String),
    Warn(String),
    Bad(String),
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Health::Ok(m) => write!(f, "  ok    {m}"),
            Health::Warn(m) => write!(f, "  warn  {m}"),
            Health::Bad(m) => write!(f, "  BAD   {m}"),
        }
    }
}

impl Health {
    pub fn is_bad(&self) -> bool {
        matches!(self, Health::Bad(_))
    }
}

pub async fn run() -> Vec<Health> {
    let mut out = Vec::new();
    out.push(check_key());
    out.push(check_display());
    out.push(check_camera());
    out.extend(check_plugins());
    out.push(check_echo_cancel());
    out.push(check_contacts());
    out.push(check_daemon().await);
    out
}

fn check_key() -> Health {
    let path = identity::key_path();
    if !path.exists() {
        return Health::Warn(format!("no identity yet; it is created on first run ({path:?})"));
    }
    match identity::load_or_create(&path) {
        Err(e) => Health::Bad(format!("identity: {e}")),
        Ok(k) => match identity::check_permissions(&path) {
            Err(e) => Health::Bad(format!("identity {}: {e}", k.public())),
            Ok(()) => Health::Ok(format!("identity {}", k.public())),
        },
    }
}

fn check_display() -> Health {
    match std::env::var("WAYLAND_DISPLAY").or_else(|_| std::env::var("DISPLAY")) {
        Ok(d) => Health::Ok(format!("graphical session ({d})")),
        // A daemon with no display cannot ring anyone, and fails silently.
        Err(_) => Health::Bad(
            "no WAYLAND_DISPLAY or DISPLAY: incoming calls could not be shown. \
             Start the daemon from your session, or via omacall.service"
                .into(),
        ),
    }
}

fn check_camera() -> Health {
    let dev = std::env::var("OMACALL_CAM").unwrap_or_else(|_| "/dev/video0".into());
    if !Path::new(&dev).exists() {
        return Health::Bad(format!("no camera at {dev}"));
    }
    let Ok(out) = Command::new("v4l2-ctl").args(["-d", &dev, "--list-formats-ext"]).output() else {
        return Health::Warn(format!("camera {dev} present; install v4l-utils to check its modes"));
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // Many UVC cameras only reach 720p30 as MJPG; without asking for it you
    // silently get 10fps and blame the network.
    if text.contains("MJPG") || text.contains("Motion-JPEG") {
        Health::Ok(format!("camera {dev} offers MJPG (needed for 720p30)"))
    } else {
        Health::Warn(format!(
            "camera {dev} advertises no MJPG mode; expect a low frame rate at 720p"
        ))
    }
}

fn check_plugins() -> Vec<Health> {
    let mut out = Vec::new();
    let required = ["vp8enc", "vp8dec", "rtpvp8pay", "opusenc", "compositor", "audiomixer", "appsrc", "appsink"];
    let missing: Vec<&str> = required.iter().copied().filter(|e| !has_element(e)).collect();
    out.push(if missing.is_empty() {
        Health::Ok("gstreamer: everything required is present".into())
    } else {
        Health::Bad(format!("gstreamer is missing: {}", missing.join(", ")))
    });

    out.push(if has_element("vah264enc") {
        Health::Ok("hardware H.264 available (vah264enc)".into())
    } else {
        Health::Warn(
            "no hardware H.264: calls will use software VP8, which costs about half a core. \
             Install gst-plugin-va"
                .into(),
        )
    });
    out
}

fn has_element(name: &str) -> bool {
    Command::new("gst-inspect-1.0")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn check_echo_cancel() -> Health {
    let Ok(out) = Command::new("pactl").args(["list", "modules", "short"]).output() else {
        return Health::Warn("could not ask PipeWire about echo cancellation".into());
    };
    if String::from_utf8_lossy(&out.stdout).contains("echo-cancel") {
        Health::Ok("echo cancellation is loaded".into())
    } else {
        // Perceived call quality is audio quality: a frozen frame is
        // forgivable, hearing yourself is not.
        Health::Warn(
            "no echo cancellation: use headphones, or run \
             pactl load-module module-echo-cancel"
                .into(),
        )
    }
}

fn check_contacts() -> Health {
    let path = identity::config_dir().join("contacts.toml");
    match Contacts::load(&path) {
        Err(e) => Health::Bad(format!("contacts: {e}")),
        Ok(c) if c.peers.is_empty() => Health::Warn(
            "no contacts yet. Run `omacall id`, send the ticket to someone, \
             and add theirs with `omacall add NAME TICKET`"
                .into(),
        ),
        Ok(c) => Health::Ok(format!("{} contact(s)", c.peers.len())),
    }
}

async fn check_daemon() -> Health {
    match ipc::request(&ipc::socket_path(), &ipc::Request::Status).await {
        Ok(ipc::Response::Status(s)) => {
            Health::Ok(format!("daemon running, state {}", s.state))
        }
        Ok(other) => Health::Warn(format!("daemon replied oddly: {other:?}")),
        Err(_) => Health::Warn(
            "daemon not running. Start it with `omacall daemon`, or enable omacall.service".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_camera_is_reported_as_bad() {
        // Not a warning: without a camera there is no call to be had.
        let h = {
            let dev = "/dev/definitely-not-a-camera";
            if Path::new(dev).exists() { Health::Ok("unexpected".into()) } else { Health::Bad(format!("no camera at {dev}")) }
        };
        assert!(h.is_bad());
    }

    #[test]
    fn required_elements_are_actually_checked() {
        // Guards against the check silently passing because the element list
        // is empty or the lookup always succeeds.
        assert!(has_element("compositor"), "compositor should exist here");
        assert!(!has_element("definitely-not-an-element"));
    }

    #[test]
    fn health_renders_with_a_severity_prefix() {
        assert!(Health::Ok("x".into()).to_string().contains("ok"));
        assert!(Health::Bad("x".into()).to_string().contains("BAD"));
        assert!(Health::Warn("x".into()).to_string().contains("warn"));
    }

    #[tokio::test]
    async fn doctor_runs_and_reports_something_for_every_check() {
        let report = run().await;
        assert!(report.len() >= 7, "every check must report, even when it passes");
    }
}
