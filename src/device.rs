// SPDX-License-Identifier: GPL-2.0-only
//! Discovery of Intel GPUs via sysfs (no privileges or xpu-smi required).

use std::fs;
use std::path::{Path, PathBuf};

use crate::util::{read_str, read_u64};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    Xe,
    I915,
}

impl Driver {
    pub fn name(self) -> &'static str {
        match self {
            Driver::Xe => "xe",
            Driver::I915 => "i915",
        }
    }
}

/// The primary GT runs render/compute/copy engines; the media GT (Meteor Lake and
/// newer) runs the video decode/enhance engines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GtKind {
    Primary,
    Media,
}

impl GtKind {
    pub fn label(self) -> &'static str {
        match self {
            GtKind::Primary => "Render",
            GtKind::Media => "Media",
        }
    }
}

/// Paths to one GT's frequency, idle and throttle attributes. Each is optional
/// because availability differs by driver, kernel and platform.
#[derive(Clone, Debug, Default)]
pub struct GtFiles {
    pub act_freq: Option<PathBuf>,
    pub cur_freq: Option<PathBuf>,
    pub min_freq: Option<PathBuf>,
    pub max_freq: Option<PathBuf>,
    pub rp0_freq: Option<PathBuf>,
    pub rpa_freq: Option<PathBuf>,
    pub rpe_freq: Option<PathBuf>,
    pub rpn_freq: Option<PathBuf>,
    /// Cumulative milliseconds the GT spent power-gated (C6/RC6).
    pub idle_ms: Option<PathBuf>,
    pub idle_status: Option<PathBuf>,
    /// xe: space separated reason list ("none" when unthrottled).
    pub throttle_reasons: Option<PathBuf>,
    pub throttle_status: Option<PathBuf>,
    /// Individual reason_* flags (name, path).
    pub throttle_flags: Vec<(String, PathBuf)>,
    pub power_profile: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Gt {
    pub id: u32,
    pub tile: u32,
    pub kind: GtKind,
    pub engines: Vec<String>,
    pub files: GtFiles,
}

#[derive(Clone, Debug)]
pub struct Device {
    pub card: String,
    pub bdf: String,
    pub driver: Driver,
    pub dev_dir: PathBuf,
    pub hwmon: Option<PathBuf>,
    pub vendor_id: u16,
    pub device_id: u16,
    pub revision: u8,
    pub subsystem: Option<String>,
    pub render_node: Option<String>,
    pub gts: Vec<Gt>,
    pub name: String,
    /// pci.ids description, e.g. "Battlemage G31 [Intel Graphics]".
    pub codename: Option<String>,
}

pub fn discover() -> Vec<Device> {
    let Ok(rd) = fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut cards: Vec<(u32, String)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let n = name.strip_prefix("card")?.parse::<u32>().ok()?;
            Some((n, name))
        })
        .collect();
    cards.sort();
    cards
        .into_iter()
        .filter_map(|(_, card)| probe(&card))
        .collect()
}

fn hex_u16(p: impl AsRef<Path>) -> Option<u16> {
    let s = read_str(p)?;
    u16::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

fn probe(card: &str) -> Option<Device> {
    let card_dir = Path::new("/sys/class/drm").join(card);
    let dev_dir = fs::canonicalize(card_dir.join("device")).ok()?;
    let driver_name = fs::read_link(dev_dir.join("driver"))
        .ok()?
        .file_name()?
        .to_string_lossy()
        .into_owned();
    let driver = match driver_name.as_str() {
        "xe" => Driver::Xe,
        "i915" => Driver::I915,
        _ => return None,
    };
    let vendor_id = hex_u16(dev_dir.join("vendor"))?;
    if vendor_id != 0x8086 {
        return None;
    }
    let device_id = hex_u16(dev_dir.join("device")).unwrap_or(0);
    let revision = hex_u16(dev_dir.join("revision")).unwrap_or(0) as u8;
    let subsystem = match (
        hex_u16(dev_dir.join("subsystem_vendor")),
        hex_u16(dev_dir.join("subsystem_device")),
    ) {
        (Some(v), Some(d)) => Some(format!("{v:04x}:{d:04x}")),
        _ => None,
    };
    let bdf = dev_dir.file_name()?.to_string_lossy().into_owned();

    let hwmon = fs::read_dir(dev_dir.join("hwmon"))
        .ok()
        .and_then(|rd| rd.flatten().map(|e| e.path()).min());

    let render_node = fs::read_dir(dev_dir.join("drm")).ok().and_then(|rd| {
        rd.flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .find(|n| n.starts_with("renderD"))
    });

    let gts = match driver {
        Driver::Xe => xe_gts(&dev_dir),
        Driver::I915 => i915_gts(&card_dir),
    };

    let codename = pci_ids_name(vendor_id, device_id);
    let name = known_name(device_id)
        .map(str::to_string)
        .or_else(|| codename.clone())
        .unwrap_or_else(|| format!("Intel GPU [{vendor_id:04x}:{device_id:04x}]"));

    Some(Device {
        card: card.to_string(),
        bdf,
        driver,
        dev_dir,
        hwmon,
        vendor_id,
        device_id,
        revision,
        subsystem,
        render_node,
        gts,
        name,
        codename,
    })
}

/// Marketing names for parts whose pci.ids entry only carries the codename.
fn known_name(device_id: u16) -> Option<&'static str> {
    match device_id {
        0xe223 => Some("Intel Arc Pro B70 Graphics"),
        _ => None,
    }
}

fn numbered(dir: &Path, prefix: &str) -> Vec<(u32, PathBuf)> {
    let mut v: Vec<(u32, PathBuf)> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().into_string().ok()?;
                    Some((n.strip_prefix(prefix)?.parse().ok()?, e.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn existing(p: PathBuf) -> Option<PathBuf> {
    p.exists().then_some(p)
}

fn xe_gts(dev_dir: &Path) -> Vec<Gt> {
    let mut gts = Vec::new();
    for (tile, tile_dir) in numbered(dev_dir, "tile") {
        for (id, gt_dir) in numbered(&tile_dir, "gt") {
            let engines: Vec<String> = fs::read_dir(gt_dir.join("engines"))
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| e.file_name().into_string().ok())
                        .collect()
                })
                .unwrap_or_default();
            let idle_name = read_str(gt_dir.join("gtidle/name")).unwrap_or_default();
            let media_engines = engines.iter().any(|e| e == "vcs" || e == "vecs");
            let primary_engines = engines.iter().any(|e| e == "rcs" || e == "ccs");
            let kind = if idle_name.ends_with("-mc") || (media_engines && !primary_engines) {
                GtKind::Media
            } else {
                GtKind::Primary
            };
            let f = gt_dir.join("freq0");
            let t = f.join("throttle");
            let mut throttle_flags: Vec<(String, PathBuf)> = fs::read_dir(&t)
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| {
                            let n = e.file_name().into_string().ok()?;
                            Some((n.strip_prefix("reason_")?.to_string(), e.path()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            throttle_flags.sort();
            let files = GtFiles {
                act_freq: existing(f.join("act_freq")),
                cur_freq: existing(f.join("cur_freq")),
                min_freq: existing(f.join("min_freq")),
                max_freq: existing(f.join("max_freq")),
                rp0_freq: existing(f.join("rp0_freq")),
                rpa_freq: existing(f.join("rpa_freq")),
                rpe_freq: existing(f.join("rpe_freq")),
                rpn_freq: existing(f.join("rpn_freq")),
                idle_ms: existing(gt_dir.join("gtidle/idle_residency_ms")),
                idle_status: existing(gt_dir.join("gtidle/idle_status")),
                throttle_reasons: existing(t.join("reasons")),
                throttle_status: existing(t.join("status")),
                throttle_flags,
                power_profile: existing(f.join("power_profile")),
            };
            gts.push(Gt {
                id,
                tile,
                kind,
                engines,
                files,
            });
        }
    }
    gts
}

fn i915_gts(card_dir: &Path) -> Vec<Gt> {
    let mut gts = Vec::new();
    for (id, g) in numbered(&card_dir.join("gt"), "gt") {
        let mut throttle_flags: Vec<(String, PathBuf)> = fs::read_dir(&g)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let n = e.file_name().into_string().ok()?;
                        let r = n.strip_prefix("throttle_reason_")?;
                        (r != "status").then(|| (r.to_string(), e.path()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        throttle_flags.sort();
        let files = GtFiles {
            act_freq: existing(g.join("rps_act_freq_mhz")),
            cur_freq: existing(g.join("rps_cur_freq_mhz")),
            min_freq: existing(g.join("rps_min_freq_mhz")),
            max_freq: existing(g.join("rps_max_freq_mhz")),
            rp0_freq: existing(g.join("rps_RP0_freq_mhz")),
            rpa_freq: None,
            rpe_freq: existing(g.join("rps_RP1_freq_mhz")),
            rpn_freq: existing(g.join("rps_RPn_freq_mhz")),
            idle_ms: existing(g.join("rc6_residency_ms")),
            idle_status: None,
            throttle_reasons: None,
            throttle_status: existing(g.join("throttle_reason_status")),
            throttle_flags,
            power_profile: None,
        };
        gts.push(Gt {
            id,
            tile: 0,
            kind: if id == 0 {
                GtKind::Primary
            } else {
                GtKind::Media
            },
            engines: Vec::new(),
            files,
        });
    }
    gts
}

/// Look up the marketing/codename string in the system pci.ids database.
fn pci_ids_name(vendor: u16, device: u16) -> Option<String> {
    let db = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"]
        .iter()
        .find_map(|p| fs::read_to_string(p).ok())?;
    let vendor_tag = format!("{vendor:04x}  ");
    let device_tag = format!("\t{device:04x}  ");
    let mut in_vendor = false;
    for line in db.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if !line.starts_with('\t') {
            if in_vendor {
                break;
            }
            in_vendor = line.starts_with(&vendor_tag);
        } else if in_vendor && line.starts_with(&device_tag) {
            return Some(line[device_tag.len()..].trim().to_string());
        }
    }
    None
}

impl Device {
    pub fn pci_id(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor_id, self.device_id)
    }

    pub fn link(&self) -> PcieLink {
        let r = |n: &str| read_str(self.dev_dir.join(n));
        PcieLink {
            cur_speed: r("current_link_speed"),
            cur_width: read_u64(self.dev_dir.join("current_link_width")),
            max_speed: r("max_link_speed"),
            max_width: read_u64(self.dev_dir.join("max_link_width")),
            slot_speed: None,
            slot_width: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PcieLink {
    pub cur_speed: Option<String>,
    pub cur_width: Option<u64>,
    pub max_speed: Option<String>,
    pub max_width: Option<u64>,
    /// Capability of the port on the other end (root port / host bridge).
    pub slot_speed: Option<String>,
    pub slot_width: Option<u64>,
}

impl PcieLink {
    pub fn describe(speed: &Option<String>, width: Option<u64>) -> String {
        let g = speed.as_deref().map(pcie_gen).unwrap_or("?");
        match width {
            Some(w) => format!("Gen{g} x{w}"),
            None => format!("Gen{g}"),
        }
    }

    pub fn current(&self) -> String {
        Self::describe(&self.cur_speed, self.cur_width)
    }

    pub fn max(&self) -> String {
        Self::describe(&self.max_speed, self.max_width)
    }

    /// The best link both ends support.
    fn effective_max(&self) -> (f64, u64) {
        let card = (gts(&self.max_speed), self.max_width.unwrap_or(0));
        match (&self.slot_speed, self.slot_width) {
            (Some(_), Some(w)) => (card.0.min(gts(&self.slot_speed)), card.1.min(w)),
            _ => card,
        }
    }

    pub fn slot_limited(&self) -> bool {
        let (s, w) = self.effective_max();
        s < gts(&self.max_speed) || w < self.max_width.unwrap_or(0)
    }

    pub fn effective_max_desc(&self) -> String {
        let (s, w) = self.effective_max();
        let speed = Some(format!("{s} GT/s"));
        Self::describe(&speed, Some(w))
    }

    /// Running below what both the card and the slot can do (often just idle
    /// link power management).
    pub fn degraded(&self) -> bool {
        let (s, w) = self.effective_max();
        gts(&self.cur_speed) < s || self.cur_width.unwrap_or(0) < w
    }
}

fn gts(speed: &Option<String>) -> f64 {
    speed
        .as_deref()
        .and_then(|s| s.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

fn pcie_gen(speed: &str) -> &'static str {
    let gts: f64 = speed
        .split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    match gts {
        x if x >= 64.0 => "6",
        x if x >= 32.0 => "5",
        x if x >= 16.0 => "4",
        x if x >= 8.0 => "3",
        x if x >= 5.0 => "2",
        x if x > 0.0 => "1",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(cur: &str, max: &str, slot: Option<&str>) -> PcieLink {
        PcieLink {
            cur_speed: Some(cur.into()),
            cur_width: Some(16),
            max_speed: Some(max.into()),
            max_width: Some(16),
            slot_speed: slot.map(Into::into),
            slot_width: slot.map(|_| 16),
        }
    }

    #[test]
    fn gen5_card_in_gen3_slot_is_not_degraded() {
        let l = link("8.0 GT/s PCIe", "32.0 GT/s PCIe", Some("8.0 GT/s PCIe"));
        assert_eq!(l.current(), "Gen3 x16");
        assert_eq!(l.max(), "Gen5 x16");
        assert_eq!(l.effective_max_desc(), "Gen3 x16");
        assert!(l.slot_limited());
        assert!(!l.degraded());
    }

    #[test]
    fn idle_downtrain_is_degraded() {
        let l = link("2.5 GT/s PCIe", "16.0 GT/s PCIe", Some("16.0 GT/s PCIe"));
        assert!(l.degraded());
        assert!(!l.slot_limited());
    }

    #[test]
    fn maps_speeds_to_generations() {
        for (speed, generation) in [
            ("2.5", "1"),
            ("5.0", "2"),
            ("8.0", "3"),
            ("16.0", "4"),
            ("32.0", "5"),
            ("64.0", "6"),
        ] {
            assert_eq!(pcie_gen(&format!("{speed} GT/s PCIe")), generation);
        }
        assert_eq!(pcie_gen("Unknown"), "?");
    }
}
