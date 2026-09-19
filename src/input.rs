//! The G502's keyboard-like evdev endpoint: find it, grab it, turn F13/F14
//! into DPI steps.
//!
//! The profile-1 side buttons send KEY_F13 / KEY_F14 as macros (see README for
//! why). We grab the endpoint exclusively so those keys never reach the
//! desktop. Only one process may do this per node.

use crate::dpi::Step;
use evdev::{Device, EventType, KeyCode};
use std::{path::PathBuf, sync::mpsc::Sender, thread::sleep, time::Duration};

/// Find the endpoint with our vendor/product that can emit F13 and F14.
/// The plain mouse node shares the IDs but has no such keys.
pub fn find_endpoint(vendor: u16, product: u16) -> Option<(PathBuf, Device)> {
    evdev::enumerate().find(|(_, dev)| {
        let id = dev.input_id();
        id.vendor() == vendor
            && id.product() == product
            && dev.supported_keys().is_some_and(|k| {
                k.contains(KeyCode::KEY_F13) && k.contains(KeyCode::KEY_F14)
            })
    })
}

fn step_for(code: u16) -> Option<Step> {
    match KeyCode::new(code) {
        KeyCode::KEY_F13 => Some(Step::Down),
        KeyCode::KEY_F14 => Some(Step::Up),
        _ => None,
    }
}

/// Never returns. Reconnects when the mouse disappears or the grab fails.
/// Returns (by ending the loop) only if the receiver is gone.
pub fn run(vendor: u16, product: u16, tx: Sender<Step>) {
    // Log each failure once, not on every retry.
    let mut waiting_logged = false;
    let mut grab_logged = false;
    loop {
        let Some((path, mut dev)) = find_endpoint(vendor, product) else {
            if !waiting_logged {
                crate::warn!("G502 keyboard endpoint not found or not readable; waiting");
                waiting_logged = true;
            }
            sleep(Duration::from_secs(2));
            continue;
        };
        waiting_logged = false;

        if let Err(e) = dev.grab() {
            if !grab_logged {
                crate::error!("cannot grab {} ({e}); is another daemon (g502-dpid?) running? retrying every 5s", path.display());
                grab_logged = true;
            }
            sleep(Duration::from_secs(5));
            continue;
        }
        grab_logged = false;
        crate::info!("listening on {} ({})", path.display(), dev.name().unwrap_or("?"));

        loop {
            let events = match dev.fetch_events() {
                Ok(events) => events,
                Err(e) => {
                    crate::warn!("{} went away: {e}", path.display());
                    break;
                }
            };
            for ev in events {
                // value 1 = key down; ignore release (0) and autorepeat (2).
                if ev.event_type() != EventType::KEY || ev.value() != 1 {
                    continue;
                }
                if let Some(step) = step_for(ev.code()) {
                    if tx.send(step).is_err() {
                        return;
                    }
                }
            }
        }
        // Dropping `dev` closes the fd, which releases the grab.
        sleep(Duration::from_secs(1));
    }
}
