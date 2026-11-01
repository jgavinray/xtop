// SPDX-License-Identifier: GPL-2.0-only
//! Background producers. Everything the UI reacts to arrives as a [`Msg`] on a
//! single channel, Elm/Bubble Tea style: input events and hardware samples.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event};

use crate::device::Device;
use crate::fdinfo::{DeviceProcs, FdinfoScanner};
use crate::sysfs::{HwSample, SysfsSampler};

pub struct DeviceSample {
    pub hw: HwSample,
    pub procs: DeviceProcs,
}

pub enum Msg {
    Input(Event),
    /// One sample per discovered device, in discovery order.
    Sample(Vec<DeviceSample>),
}

pub enum Control {
    /// Sample immediately (e.g. after an interval change).
    Refresh,
}

pub fn spawn_input(tx: Sender<Msg>) {
    thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            while let Ok(ev) = event::read() {
                if tx.send(Msg::Input(ev)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn input thread");
}

pub fn spawn_sampler(
    devices: Vec<Device>,
    interval_ms: Arc<AtomicU64>,
    detail: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    tx: Sender<Msg>,
    ctl: Receiver<Control>,
) {
    thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            let bdfs: Vec<String> = devices.iter().map(|d| d.bdf.clone()).collect();
            let mut samplers: Vec<SysfsSampler> = devices
                .into_iter()
                .map(|d| SysfsSampler::new(d, detail.clone()))
                .collect();
            let mut scanner = FdinfoScanner::new();
            while !stop.load(Ordering::Relaxed) {
                let started = Instant::now();
                let mut procs: HashMap<String, DeviceProcs> = scanner.scan(&bdfs);
                let samples = samplers
                    .iter_mut()
                    .zip(&bdfs)
                    .map(|(s, bdf)| DeviceSample {
                        hw: s.sample(),
                        procs: procs.remove(bdf).unwrap_or_default(),
                    })
                    .collect();
                if tx.send(Msg::Sample(samples)).is_err() {
                    break;
                }
                let wait = Duration::from_millis(interval_ms.load(Ordering::Relaxed))
                    .saturating_sub(started.elapsed());
                match ctl.recv_timeout(wait) {
                    Ok(Control::Refresh) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .expect("spawn sampler thread");
}
