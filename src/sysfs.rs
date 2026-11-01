// SPDX-License-Identifier: GPL-2.0-only
//! Unprivileged hardware telemetry from hwmon and the GT sysfs tree.

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::device::{Device, Driver, GtKind};
use crate::util::{read_i64, read_str, read_u64};
use crate::xequery::{MemRegion, XeQuery};

#[derive(Clone, Debug)]
pub struct TempSensor {
    pub label: String,
    pub celsius: f64,
    pub crit: Option<f64>,
    pub emergency: Option<f64>,
}

impl TempSensor {
    pub fn is_vram_channel(&self) -> bool {
        self.label.starts_with("vram_ch_")
    }
}

#[derive(Clone, Debug)]
pub struct PowerRail {
    pub label: String,
    /// Average power over the last interval, from the energy counter delta.
    pub watts: Option<f64>,
    pub energy_j: Option<f64>,
    pub cap_w: Option<f64>,
    pub crit_w: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct GtSample {
    pub id: u32,
    pub kind: GtKind,
    pub act_mhz: Option<u64>,
    pub cur_mhz: Option<u64>,
    pub min_mhz: Option<u64>,
    pub max_mhz: Option<u64>,
    pub rp0_mhz: Option<u64>,
    pub rpa_mhz: Option<u64>,
    pub rpe_mhz: Option<u64>,
    pub rpn_mhz: Option<u64>,
    /// Percent of the interval the GT was out of C6 (powered up).
    pub awake_pct: Option<f64>,
    pub idle_status: Option<String>,
    pub throttled: bool,
    pub throttle_reasons: Vec<String>,
    pub throttle_flags: Vec<(String, bool)>,
    pub power_profile: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct HwSample {
    pub temps: Vec<TempSensor>,
    pub fans: Vec<(String, u64)>,
    pub rails: Vec<PowerRail>,
    pub voltages: Vec<(String, f64)>,
    pub gts: Vec<GtSample>,
    pub mem_regions: Vec<MemRegion>,
}

impl HwSample {
    pub fn temp(&self, label: &str) -> Option<&TempSensor> {
        self.temps.iter().find(|t| t.label == label)
    }

    pub fn rail(&self, label: &str) -> Option<&PowerRail> {
        self.rails.iter().find(|r| r.label == label)
    }

    /// The hottest "headline" temperature: package if present, else any non-channel sensor.
    pub fn primary_temp(&self) -> Option<&TempSensor> {
        self.temp("pkg")
            .or_else(|| self.temp("gpu"))
            .or_else(|| self.temps.iter().find(|t| !t.is_vram_channel()))
    }

    /// Board power: the card rail when present, otherwise package.
    pub fn board_power(&self) -> Option<&PowerRail> {
        self.rail("card")
            .filter(|r| r.watts.is_some())
            .or_else(|| self.rail("pkg"))
            .or_else(|| self.rails.first())
    }

    pub fn gt(&self, kind: GtKind) -> Option<&GtSample> {
        self.gts.iter().find(|g| g.kind == kind)
    }

    /// Device-wide VRAM (used, total, cpu-visible size) summed over VRAM regions.
    pub fn vram(&self) -> Option<(u64, u64, u64)> {
        let v: Vec<&MemRegion> = self.mem_regions.iter().filter(|r| r.vram).collect();
        if v.is_empty() {
            return None;
        }
        Some((
            v.iter().map(|r| r.used).sum(),
            v.iter().map(|r| r.total).sum(),
            v.iter().map(|r| r.cpu_visible_size).sum(),
        ))
    }
}

pub struct SysfsSampler {
    dev: Device,
    query: Option<XeQuery>,
    /// Read requested frequency too. On xe that read takes GT forcewake and
    /// briefly blocks C6, so it is only done while the Sensors view is open.
    detail: Arc<AtomicBool>,
    prev_energy: HashMap<u32, (u64, Instant)>,
    prev_idle: HashMap<(u32, u32), (u64, Instant)>,
    prev_fan: HashMap<u32, u64>,
}

impl SysfsSampler {
    pub fn new(dev: Device, detail: Arc<AtomicBool>) -> Self {
        let query = match (&dev.driver, &dev.render_node) {
            (Driver::Xe, Some(node)) => XeQuery::open(node),
            _ => None,
        };
        Self {
            dev,
            query,
            detail,
            prev_energy: HashMap::new(),
            prev_idle: HashMap::new(),
            prev_fan: HashMap::new(),
        }
    }

    pub fn sample(&mut self) -> HwSample {
        let mut s = HwSample::default();
        if let Some(h) = self.dev.hwmon.clone() {
            self.read_hwmon(&h, &mut s);
        }
        self.read_gts(&mut s);
        if let Some(q) = &self.query {
            s.mem_regions = q.mem_regions().unwrap_or_default();
        }
        s
    }

    fn read_hwmon(&mut self, h: &std::path::Path, s: &mut HwSample) {
        let now = Instant::now();
        let mut temp_ids = Vec::new();
        let mut fan_ids = Vec::new();
        let mut energy_ids = Vec::new();
        let mut power_ids = Vec::new();
        let mut in_ids = Vec::new();
        for e in fs::read_dir(h).into_iter().flatten().flatten() {
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            let Some(stem) = name
                .strip_suffix("_input")
                .or_else(|| name.strip_suffix("_cap"))
            else {
                continue;
            };
            let split = stem
                .find(|c: char| c.is_ascii_digit())
                .unwrap_or(stem.len());
            let (kind, num) = stem.split_at(split);
            let Ok(n) = num.parse::<u32>() else { continue };
            match kind {
                "temp" => temp_ids.push(n),
                "fan" => fan_ids.push(n),
                "energy" => energy_ids.push(n),
                "power" => power_ids.push(n),
                "in" => in_ids.push(n),
                _ => {}
            }
        }
        for v in [
            &mut temp_ids,
            &mut fan_ids,
            &mut energy_ids,
            &mut power_ids,
            &mut in_ids,
        ] {
            v.sort();
            v.dedup();
        }

        let milli = |p: std::path::PathBuf| read_i64(p).map(|v| v as f64 / 1000.0);
        let label = |kind: &str, n: u32| {
            read_str(h.join(format!("{kind}{n}_label"))).unwrap_or_else(|| format!("{kind}{n}"))
        };

        for n in temp_ids {
            let Some(c) = milli(h.join(format!("temp{n}_input"))) else {
                continue;
            };
            s.temps.push(TempSensor {
                label: label("temp", n),
                celsius: c,
                crit: milli(h.join(format!("temp{n}_crit"))),
                emergency: milli(h.join(format!("temp{n}_emergency"))),
            });
        }
        // xe returns -EAGAIN for two fan reads in the same millisecond (from
        // any process); keep the previous value rather than dropping the fan.
        for n in fan_ids {
            let rpm = read_u64(h.join(format!("fan{n}_input"))).or(self.prev_fan.get(&n).copied());
            if let Some(rpm) = rpm {
                self.prev_fan.insert(n, rpm);
                s.fans.push((label("fan", n), rpm));
            }
        }
        for n in in_ids {
            if let Some(v) = milli(h.join(format!("in{n}_input"))) {
                s.voltages.push((label("in", n), v));
            }
        }

        // Power rails are keyed by label so energyN and powerN line up ("card", "pkg").
        let mut rails: Vec<PowerRail> = Vec::new();
        for n in energy_ids {
            let Some(uj) = read_u64(h.join(format!("energy{n}_input"))) else {
                continue;
            };
            let watts = self.prev_energy.get(&n).and_then(|&(prev, at)| {
                let dt = now.duration_since(at).as_secs_f64();
                (dt > 0.0 && uj >= prev).then(|| (uj - prev) as f64 / 1e6 / dt)
            });
            self.prev_energy.insert(n, (uj, now));
            rails.push(PowerRail {
                label: label("energy", n),
                watts,
                energy_j: Some(uj as f64 / 1e6),
                cap_w: None,
                crit_w: None,
            });
        }
        let micro = |p: std::path::PathBuf| read_u64(p).map(|v| v as f64 / 1e6);
        for n in power_ids {
            let l = label("power", n);
            let cap = micro(h.join(format!("power{n}_cap")))
                .or_else(|| micro(h.join(format!("power{n}_max"))));
            let crit = micro(h.join(format!("power{n}_crit")));
            let inst = micro(h.join(format!("power{n}_input")))
                .or_else(|| micro(h.join(format!("power{n}_average"))));
            match rails.iter_mut().find(|r| r.label == l) {
                Some(r) => {
                    r.cap_w = cap;
                    r.crit_w = crit;
                    if r.watts.is_none() {
                        r.watts = inst;
                    }
                }
                None => rails.push(PowerRail {
                    label: l,
                    watts: inst,
                    energy_j: None,
                    cap_w: cap,
                    crit_w: crit,
                }),
            }
        }
        s.rails = rails;
    }

    fn read_gts(&mut self, s: &mut HwSample) {
        let now = Instant::now();
        let detail = self.detail.load(Ordering::Relaxed) || self.dev.driver == Driver::I915;
        for gt in &self.dev.gts {
            let f = &gt.files;
            let mhz = |p: &Option<std::path::PathBuf>| p.as_ref().and_then(read_u64);
            let awake_pct = f.idle_ms.as_ref().and_then(read_u64).and_then(|idle| {
                let key = (gt.tile, gt.id);
                let pct = self.prev_idle.get(&key).and_then(|&(prev, at)| {
                    let dt_ms = now.duration_since(at).as_secs_f64() * 1000.0;
                    (dt_ms > 0.0 && idle >= prev)
                        .then(|| (100.0 - (idle - prev) as f64 / dt_ms * 100.0).clamp(0.0, 100.0))
                });
                self.prev_idle.insert(key, (idle, now));
                pct
            });
            let throttle_flags: Vec<(String, bool)> = f
                .throttle_flags
                .iter()
                .map(|(n, p)| (n.clone(), read_u64(p).unwrap_or(0) != 0))
                .collect();
            let mut throttle_reasons: Vec<String> = match &f.throttle_reasons {
                Some(p) => read_str(p)
                    .unwrap_or_default()
                    .split_whitespace()
                    .filter(|r| *r != "none")
                    .map(str::to_string)
                    .collect(),
                None => Vec::new(),
            };
            if throttle_reasons.is_empty() {
                throttle_reasons = throttle_flags
                    .iter()
                    .filter(|(_, on)| *on)
                    .map(|(n, _)| n.clone())
                    .collect();
            }
            let throttled = f
                .throttle_status
                .as_ref()
                .and_then(read_u64)
                .map(|v| v != 0)
                .unwrap_or(false)
                || !throttle_reasons.is_empty();
            let power_profile = f.power_profile.as_ref().and_then(read_str).and_then(|p| {
                // "[base]    power_saving" -> "base"
                p.split_whitespace()
                    .find(|t| t.starts_with('['))
                    .map(|t| t.trim_matches(['[', ']']).to_string())
            });
            s.gts.push(GtSample {
                id: gt.id,
                kind: gt.kind,
                act_mhz: mhz(&f.act_freq),
                cur_mhz: if detail { mhz(&f.cur_freq) } else { None },
                min_mhz: mhz(&f.min_freq),
                max_mhz: mhz(&f.max_freq),
                rp0_mhz: mhz(&f.rp0_freq),
                rpa_mhz: mhz(&f.rpa_freq),
                rpe_mhz: mhz(&f.rpe_freq),
                rpn_mhz: mhz(&f.rpn_freq),
                awake_pct,
                idle_status: f.idle_status.as_ref().and_then(read_str),
                throttled,
                throttle_reasons,
                throttle_flags,
                power_profile,
            });
        }
    }
}
