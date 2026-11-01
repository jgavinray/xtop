// SPDX-License-Identifier: GPL-2.0-only
//! Per-process GPU usage from DRM fdinfo (/proc/<pid>/fdinfo/<fd>).
//!
//! The kernel exposes per-client engine busyness and memory for every open DRM
//! file. xe reports `drm-cycles-<engine>` against `drm-total-cycles-<engine>`
//! (both in GPU timestamp ticks); i915 reports `drm-engine-<engine>` in ns.
//! Several fds can share one client (dup/fork), so clients are deduplicated by
//! (pdev, drm-client-id). Other users' processes are only visible to root.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::Instant;

use crate::util::read_str;

#[derive(Clone, Copy, Debug)]
enum Counter {
    Cycles { busy: u64, total: u64 },
    Nanos(u64),
}

struct ClientPrev {
    engines: HashMap<String, Counter>,
    at: Instant,
}

#[derive(Clone, Debug, Default)]
pub struct ProcGpu {
    pub pid: u32,
    pub name: String,
    pub cmdline: String,
    pub user: String,
    pub clients: usize,
    pub vram_kib: u64,
    pub gtt_kib: u64,
    pub sys_kib: u64,
    /// Engine class -> utilization percent (None until a second sample exists).
    pub engines: HashMap<String, f64>,
    pub cpu_pct: Option<f64>,
    pub rss_kib: u64,
}

impl ProcGpu {
    pub fn gpu_pct(&self) -> f64 {
        self.engines.values().cloned().fold(0.0, f64::max)
    }
}

#[derive(Clone, Debug, Default)]
pub struct DeviceProcs {
    pub procs: Vec<ProcGpu>,
    /// Sum of all visible clients' utilization per engine class.
    pub engine_totals: HashMap<String, f64>,
    /// Engine classes this device reports, in display order.
    pub engine_names: Vec<String>,
    pub visible_vram_kib: u64,
    pub clients: usize,
}

pub struct FdinfoScanner {
    prev: HashMap<(String, u64), ClientPrev>,
    prev_cpu: HashMap<u32, (u64, Instant)>,
    users: HashMap<u32, String>,
    clk_tck: f64,
    page_kib: u64,
}

/// Canonical engine order and short labels for xe and i915 names.
pub fn engine_rank(name: &str) -> (u8, &'static str) {
    match name {
        "ccs" | "compute" => (0, "Compute"),
        "rcs" | "render" => (1, "Render"),
        "bcs" | "copy" => (2, "Copy"),
        "vcs" | "video" => (3, "Video"),
        "vecs" | "video-enhance" => (4, "VidEnh"),
        _ => (9, "Other"),
    }
}

pub fn engine_label(name: &str) -> String {
    match engine_rank(name) {
        (9, _) => name.to_string(),
        (_, l) => l.to_string(),
    }
}

impl FdinfoScanner {
    pub fn new() -> Self {
        // SAFETY: sysconf has no preconditions.
        let (clk, page) = unsafe {
            (
                libc::sysconf(libc::_SC_CLK_TCK),
                libc::sysconf(libc::_SC_PAGESIZE),
            )
        };
        Self {
            prev: HashMap::new(),
            prev_cpu: HashMap::new(),
            users: load_users(),
            clk_tck: if clk > 0 { clk as f64 } else { 100.0 },
            page_kib: if page > 0 { page as u64 / 1024 } else { 4 },
        }
    }

    pub fn scan(&mut self, bdfs: &[String]) -> HashMap<String, DeviceProcs> {
        let now = Instant::now();
        let mut out: HashMap<String, DeviceProcs> = bdfs
            .iter()
            .map(|b| (b.clone(), DeviceProcs::default()))
            .collect();
        let mut procs: HashMap<(String, u32), ProcGpu> = HashMap::new();
        let mut seen: HashSet<(String, u64)> = HashSet::new();
        let mut next_prev: HashMap<(String, u64), ClientPrev> = HashMap::new();
        let mut engines_seen: HashMap<String, HashSet<String>> = HashMap::new();

        let Ok(proc_dir) = fs::read_dir("/proc") else {
            return out;
        };
        let mut pids: Vec<u32> = proc_dir
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect();
        pids.sort_unstable();
        // Our own render-node handle (for the VRAM query) is not a workload.
        let me = std::process::id();

        for pid in pids.into_iter().filter(|p| *p != me) {
            let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                let Ok(target) = fs::read_link(fd.path()) else {
                    continue;
                };
                if !target
                    .as_os_str()
                    .as_encoded_bytes()
                    .starts_with(b"/dev/dri/")
                {
                    continue;
                }
                let fdn = fd.file_name();
                let Ok(info) =
                    fs::read_to_string(format!("/proc/{pid}/fdinfo/{}", fdn.to_string_lossy()))
                else {
                    continue;
                };
                let Some(client) = parse_fdinfo(&info) else {
                    continue;
                };
                let Some(dev) = out.get_mut(&client.pdev) else {
                    continue;
                };
                let key = (client.pdev.clone(), client.id);
                if !seen.insert(key.clone()) {
                    continue;
                }
                dev.clients += 1;

                let prev = self.prev.get(&key);
                let entry = procs
                    .entry((client.pdev.clone(), pid))
                    .or_insert_with(|| ProcGpu {
                        pid,
                        ..Default::default()
                    });
                entry.clients += 1;
                entry.vram_kib += client.vram_kib;
                entry.gtt_kib += client.gtt_kib;
                entry.sys_kib += client.sys_kib;
                dev.visible_vram_kib += client.vram_kib;

                let seen_names = engines_seen.entry(client.pdev.clone()).or_default();
                for (name, cur) in &client.engines {
                    seen_names.insert(name.clone());
                    let cap = client.capacity.get(name).copied().unwrap_or(1).max(1) as f64;
                    let pct = prev.and_then(|p| {
                        let old = p.engines.get(name)?;
                        match (*old, *cur) {
                            (
                                Counter::Cycles {
                                    busy: b0,
                                    total: t0,
                                },
                                Counter::Cycles {
                                    busy: b1,
                                    total: t1,
                                },
                            ) => (t1 > t0 && b1 >= b0)
                                .then(|| (b1 - b0) as f64 / (t1 - t0) as f64 * 100.0 / cap),
                            (Counter::Nanos(n0), Counter::Nanos(n1)) => {
                                let dt = now.duration_since(p.at).as_nanos() as f64;
                                (dt > 0.0 && n1 >= n0).then(|| (n1 - n0) as f64 / dt * 100.0 / cap)
                            }
                            _ => None,
                        }
                    });
                    if let Some(pct) = pct {
                        let pct = pct.clamp(0.0, 100.0);
                        *entry.engines.entry(name.clone()).or_insert(0.0) += pct;
                        *dev.engine_totals.entry(name.clone()).or_insert(0.0) += pct;
                    }
                }
                // Per the DRM usage-stats spec, a counter that goes backwards
                // keeps its previous (larger) value until it catches up.
                let mut engines = client.engines;
                if let Some(p) = prev {
                    for (name, cur) in engines.iter_mut() {
                        if let (
                            Some(Counter::Cycles { busy: b0, .. }),
                            Counter::Cycles { busy: b1, .. },
                        ) = (p.engines.get(name), *cur)
                            && b1 < *b0
                        {
                            *cur = p.engines[name];
                        }
                    }
                }
                next_prev.insert(key, ClientPrev { engines, at: now });
            }
        }
        self.prev = next_prev;

        let mut live_pids = HashSet::new();
        for ((pdev, pid), mut p) in procs {
            live_pids.insert(pid);
            self.fill_process(&mut p, now);
            for v in p.engines.values_mut() {
                *v = v.min(100.0);
            }
            if let Some(d) = out.get_mut(&pdev) {
                d.procs.push(p);
            }
        }
        self.prev_cpu.retain(|pid, _| live_pids.contains(pid));

        for (pdev, d) in out.iter_mut() {
            for v in d.engine_totals.values_mut() {
                *v = v.min(100.0);
            }
            let mut names: Vec<String> = engines_seen
                .remove(pdev)
                .map(|s| s.into_iter().collect())
                .unwrap_or_default();
            names.sort_by_key(|n| (engine_rank(n).0, n.clone()));
            d.engine_names = names;
        }
        out
    }

    fn fill_process(&mut self, p: &mut ProcGpu, now: Instant) {
        let pid = p.pid;
        p.name = read_str(format!("/proc/{pid}/comm")).unwrap_or_else(|| "?".into());
        p.cmdline = fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .split('\0')
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        if p.cmdline.is_empty() {
            p.cmdline = format!("[{}]", p.name);
        }
        p.user = proc_uid(pid)
            .map(|uid| {
                self.users
                    .get(&uid)
                    .cloned()
                    .unwrap_or_else(|| uid.to_string())
            })
            .unwrap_or_else(|| "?".into());
        if let Some((ticks, rss_pages)) = proc_stat(pid) {
            p.rss_kib = rss_pages * self.page_kib;
            p.cpu_pct = self.prev_cpu.get(&pid).and_then(|&(t0, at)| {
                let dt = now.duration_since(at).as_secs_f64();
                (dt > 0.0 && ticks >= t0).then(|| (ticks - t0) as f64 / self.clk_tck / dt * 100.0)
            });
            self.prev_cpu.insert(pid, (ticks, now));
        }
    }
}

struct Client {
    pdev: String,
    id: u64,
    engines: HashMap<String, Counter>,
    capacity: HashMap<String, u64>,
    vram_kib: u64,
    gtt_kib: u64,
    sys_kib: u64,
}

fn parse_kib(v: &str) -> u64 {
    let mut it = v.split_whitespace();
    let n: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    match it.next() {
        Some("KiB") => n,
        Some("MiB") => n * 1024,
        Some("GiB") => n * 1024 * 1024,
        _ => n / 1024, // bare value is bytes
    }
}

fn parse_fdinfo(text: &str) -> Option<Client> {
    let mut pdev = None;
    let mut id = None;
    let mut busy: HashMap<String, u64> = HashMap::new();
    let mut total: HashMap<String, u64> = HashMap::new();
    let mut nanos: HashMap<String, u64> = HashMap::new();
    let mut capacity: HashMap<String, u64> = HashMap::new();
    let mut total_mem: HashMap<String, u64> = HashMap::new();
    let mut resident_mem: HashMap<String, u64> = HashMap::new();

    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        let Some(k) = k.strip_prefix("drm-") else {
            continue;
        };
        let num = v
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok());
        if k == "pdev" {
            pdev = Some(v.to_string());
        } else if k == "client-id" {
            id = num;
        } else if let Some(e) = k.strip_prefix("total-cycles-") {
            total.extend(num.map(|n| (e.to_string(), n)));
        } else if let Some(e) = k.strip_prefix("cycles-") {
            busy.extend(num.map(|n| (e.to_string(), n)));
        } else if let Some(e) = k.strip_prefix("engine-capacity-") {
            capacity.extend(num.map(|n| (e.to_string(), n)));
        } else if let Some(e) = k.strip_prefix("engine-") {
            nanos.extend(num.map(|n| (e.to_string(), n)));
        } else if let Some(r) = k.strip_prefix("total-") {
            total_mem.insert(r.into(), parse_kib(v));
        } else if let Some(r) = k.strip_prefix("resident-") {
            resident_mem.insert(r.into(), parse_kib(v));
        }
    }

    let mut engines: HashMap<String, Counter> = HashMap::new();
    for (e, b) in busy {
        if let Some(&t) = total.get(&e) {
            engines.insert(e, Counter::Cycles { busy: b, total: t });
        }
    }
    for (e, n) in nanos {
        engines.entry(e).or_insert(Counter::Nanos(n));
    }

    let mut vram = 0;
    let mut gtt = 0;
    let mut sys = 0;
    let regions: HashSet<&String> = total_mem.keys().chain(resident_mem.keys()).collect();
    for r in regions {
        let kib = resident_mem
            .get(r)
            .or_else(|| total_mem.get(r))
            .copied()
            .unwrap_or(0);
        if r.starts_with("vram") || r.starts_with("local") {
            vram += kib;
        } else if r == "gtt" {
            gtt += kib;
        } else if r.starts_with("system") {
            sys += kib;
        }
    }

    Some(Client {
        pdev: pdev?,
        id: id?,
        engines,
        capacity,
        vram_kib: vram,
        gtt_kib: gtt,
        sys_kib: sys,
    })
}

fn proc_uid(pid: u32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// (utime + stime ticks, rss pages) from /proc/<pid>/stat.
fn proc_stat(pid: u32) -> Option<(u64, u64)> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // Fields after comm start at index 3 (state); utime=14, stime=15, rss=24 (1-based).
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    let rss: u64 = f.get(21)?.parse().ok()?;
    Some((utime + stime, rss))
}

fn load_users() -> HashMap<u32, String> {
    fs::read_to_string("/etc/passwd")
        .map(|s| {
            s.lines()
                .filter_map(|l| {
                    let mut f = l.split(':');
                    let name = f.next()?;
                    let uid = f.nth(1)?.parse().ok()?;
                    Some((uid, name.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from an xe client on an Arc Pro B70 (kernel 7.0).
    const XE: &str = "pos:\t0\nflags:\t02100002\nmnt_id:\t30\nino:\t1259\n\
drm-driver:\txe\ndrm-client-id:\t117\ndrm-pdev:\t0000:08:00.0\n\
drm-total-system:\t390820 KiB\ndrm-resident-system:\t0\n\
drm-total-gtt:\t4767364 KiB\ndrm-resident-gtt:\t4767364 KiB\n\
drm-total-vram0:\t22371816 KiB\ndrm-resident-vram0:\t22371816 KiB\n\
drm-cycles-rcs:\t0\ndrm-total-cycles-rcs:\t3322682612913\n\
drm-cycles-vcs:\t0\ndrm-total-cycles-vcs:\t3322682612913\ndrm-engine-capacity-vcs:\t2\n\
drm-cycles-ccs:\t1244432140\ndrm-total-cycles-ccs:\t3322682612913\n";

    const I915: &str = "drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t7\n\
drm-engine-render:\t25662044495 ns\ndrm-engine-copy:\t0 ns\n\
drm-engine-video:\t0 ns\ndrm-engine-capacity-video:\t2\n\
drm-total-local0:\t4194304 KiB\ndrm-total-system0:\t1 MiB\n";

    #[test]
    fn parses_xe_client() {
        let c = parse_fdinfo(XE).unwrap();
        assert_eq!(c.pdev, "0000:08:00.0");
        assert_eq!(c.id, 117);
        assert_eq!(c.vram_kib, 22371816);
        assert_eq!(c.gtt_kib, 4767364);
        assert_eq!(c.sys_kib, 0, "resident is preferred over total");
        assert_eq!(c.capacity.get("vcs"), Some(&2));
        assert!(matches!(
            c.engines.get("ccs"),
            Some(Counter::Cycles {
                busy: 1244432140,
                total: 3322682612913
            })
        ));
        assert_eq!(c.engines.len(), 3);
    }

    #[test]
    fn parses_i915_client() {
        let c = parse_fdinfo(I915).unwrap();
        assert!(matches!(
            c.engines.get("render"),
            Some(Counter::Nanos(25662044495))
        ));
        assert!(!c.engines.contains_key("capacity-video"));
        assert_eq!(c.capacity.get("video"), Some(&2));
        assert_eq!(c.vram_kib, 4194304);
        assert_eq!(c.sys_kib, 1024);
    }

    #[test]
    fn rejects_non_drm_fdinfo() {
        assert!(parse_fdinfo("pos:\t0\nflags:\t02\n").is_none());
    }

    #[test]
    fn parses_size_units() {
        assert_eq!(parse_kib("2048"), 2);
        assert_eq!(parse_kib("512 KiB"), 512);
        assert_eq!(parse_kib("4 MiB"), 4096);
        assert_eq!(parse_kib("2 GiB"), 2 * 1024 * 1024);
    }

    #[test]
    fn orders_engines() {
        let mut v = vec!["vecs", "bcs", "rcs", "ccs", "vcs"];
        v.sort_by_key(|n| engine_rank(n).0);
        assert_eq!(v, ["ccs", "rcs", "bcs", "vcs", "vecs"]);
        assert_eq!(engine_label("video-enhance"), "VidEnh");
        assert_eq!(engine_label("mystery"), "mystery");
    }
}
