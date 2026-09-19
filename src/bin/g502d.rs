//! g502d: listens on the G502's F13/F14 endpoint and keeps the shared DPI of
//! the configured profiles in sync through ratbagd. Replaces g502-dpid + g502-dpi.

use g502_linux_control::{
    config::Config,
    dpi::{apply_batch, next_batch},
    error, info, input,
    ratbag::{Controller, lock_writes},
    warn,
};
use std::{process::exit, sync::mpsc, thread};

fn main() {
    let (cfg, path) = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            error!("config error: {e:#}");
            exit(1);
        }
    };
    match &path {
        Some(p) => info!("config: {}", p.display()),
        None => info!("config: built-in defaults (no config file)"),
    }
    info!(
        "stages {:?}, shared slot {}, synced profiles {:?}",
        cfg.dpi.values, cfg.dpi.shared_slot, cfg.dpi.profiles
    );

    let (tx, rx) = mpsc::channel();
    let (vendor, product) = (cfg.device.vendor, cfg.device.product);
    thread::spawn(move || input::run(vendor, product, tx));

    // ratbagd may be down or the mouse absent at start; connect lazily and
    // reconnect after any error.
    let mut ctl: Option<Controller> = None;
    while let Some(steps) = next_batch(&rx) {
        if ctl.is_none() {
            match Controller::connect(&cfg) {
                Ok(c) => {
                    info!("connected to {}", c.device_name());
                    ctl = Some(c);
                }
                Err(e) => {
                    warn!("cannot use device, dropping {} press(es): {e:#}", steps.len());
                    continue;
                }
            }
        }
        let c = ctl.as_mut().unwrap();

        let result = lock_writes().and_then(|_guard| apply_batch(c, &cfg.dpi.values, &steps));
        match result {
            Ok(Some((from, to))) => info!("dpi {from} -> {to} ({} press(es))", steps.len()),
            Ok(None) => info!("dpi unchanged ({} press(es), at boundary)", steps.len()),
            Err(e) => {
                error!("dpi update failed: {e:#}");
                ctl = None;
            }
        }
    }
}
