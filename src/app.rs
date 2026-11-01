// SPDX-License-Identifier: GPL-2.0-only
//! Application model and update function.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;

use crate::collector::{Control, DeviceSample, Msg};
use crate::device::{Device, Driver, GtKind, PcieLink};
use crate::fdinfo::{DeviceProcs, ProcGpu};
use crate::sysfs::HwSample;
use crate::xequery::{XeInfo, XeQuery};

pub const HISTORY: usize = 900;

#[derive(Clone, Debug, Default)]
pub struct Series {
    data: VecDeque<f64>,
}

impl Series {
    pub fn push(&mut self, v: f64) {
        if self.data.len() == HISTORY {
            self.data.pop_front();
        }
        self.data.push_back(v);
    }

    pub fn last(&self) -> Option<f64> {
        self.data.back().copied()
    }

    /// The most recent `n` points, oldest first.
    pub fn tail(&self, n: usize) -> impl Iterator<Item = f64> + '_ {
        self.data
            .iter()
            .skip(self.data.len().saturating_sub(n))
            .copied()
    }

    pub fn max_tail(&self, n: usize) -> f64 {
        self.tail(n).fold(0.0, f64::max)
    }

    pub fn min_tail(&self, n: usize) -> f64 {
        self.tail(n).fold(f64::INFINITY, f64::min)
    }

    pub fn avg_tail(&self, n: usize) -> f64 {
        let (s, c) = self.tail(n).fold((0.0, 0usize), |(s, c), v| (s + v, c + 1));
        if c == 0 { 0.0 } else { s / c as f64 }
    }
}

#[derive(Default)]
pub struct History {
    pub gpu: Series,
    pub power: Series,
    pub temp: Series,
    pub freq: Series,
    pub vram_pct: Series,
    pub engines: BTreeMap<String, Series>,
    pub temps: BTreeMap<String, Series>,
}

pub struct DeviceView {
    pub dev: Device,
    pub link: PcieLink,
    pub info: Option<XeInfo>,
    pub hw: Option<HwSample>,
    pub procs: DeviceProcs,
    pub hist: History,
    pub samples: u64,
    /// Peak engine/VRAM usage seen this session, for the Device tab.
    pub peak_power: f64,
    pub peak_temp: f64,
}

impl DeviceView {
    fn new(dev: Device) -> Self {
        let link = upstream_link(&dev);
        let info = match (&dev.driver, &dev.render_node) {
            (Driver::Xe, Some(node)) => XeQuery::open(node).map(|q| q.info()),
            _ => None,
        };
        Self {
            dev,
            link,
            info,
            hw: None,
            procs: DeviceProcs::default(),
            hist: History::default(),
            samples: 0,
            peak_power: 0.0,
            peak_temp: 0.0,
        }
    }

    /// Headline GPU utilization: the busiest engine class across all visible
    /// DRM clients (the same definition nvtop and qmassa use).
    pub fn gpu_util(&self) -> Option<f64> {
        if self.samples < 2 {
            return None;
        }
        Some(
            self.procs
                .engine_totals
                .values()
                .cloned()
                .fold(0.0, f64::max),
        )
    }

    /// Engine classes to display: those reported by fdinfo, else the GT sysfs list.
    pub fn engine_names(&self) -> Vec<String> {
        let mut names = self.procs.engine_names.clone();
        for gt in &self.dev.gts {
            for e in &gt.engines {
                if !names.contains(e) {
                    names.push(e.clone());
                }
            }
        }
        names.sort_by_key(|n| (crate::fdinfo::engine_rank(n).0, n.clone()));
        names
    }

    /// (used bytes, total bytes if known, device-wide?) — the xe query covers
    /// every process; the fdinfo fallback only sees processes we can inspect.
    pub fn vram(&self) -> Option<(f64, Option<f64>, bool)> {
        if let Some((used, total, _)) = self.hw.as_ref().and_then(HwSample::vram) {
            return Some((used as f64, Some(total as f64), true));
        }
        if self.procs.clients > 0 {
            let used: f64 = self
                .procs
                .procs
                .iter()
                .map(|p| p.vram_kib as f64 * 1024.0)
                .sum();
            return Some((used, None, false));
        }
        None
    }

    fn absorb(&mut self, s: DeviceSample) {
        self.samples += 1;
        self.procs = s.procs;
        self.hw = Some(s.hw);

        let gpu = self.gpu_util().unwrap_or(0.0);
        self.hist.gpu.push(gpu);
        let vram = self.vram();
        let hw = self.hw.as_ref().unwrap();
        if let Some(w) = hw.board_power().and_then(|r| r.watts) {
            self.hist.power.push(w);
            self.peak_power = self.peak_power.max(w);
        }
        if let Some(t) = hw.primary_temp() {
            self.hist.temp.push(t.celsius);
            self.peak_temp = self.peak_temp.max(t.celsius);
        }
        for t in hw.temps.iter().filter(|t| !t.is_vram_channel()) {
            self.hist
                .temps
                .entry(t.label.clone())
                .or_default()
                .push(t.celsius);
        }
        if let Some(g) = hw.gt(GtKind::Primary) {
            self.hist.freq.push(g.act_mhz.unwrap_or(0) as f64);
        }
        if let Some((used, Some(total), _)) = vram
            && total > 0.0
        {
            self.hist.vram_pct.push(used / total * 100.0);
        }
        for name in self.engine_names() {
            let v = self.procs.engine_totals.get(&name).copied().unwrap_or(0.0);
            self.hist.engines.entry(name).or_default().push(v);
        }
    }
}

/// Discrete Arc/Battlemage cards sit behind an on-board PCIe switch whose
/// downstream port reports a fake 2.5 GT/s x1 link; walk up to the switch's
/// upstream port to report the real slot link.
fn upstream_link(dev: &Device) -> PcieLink {
    let mut best = dev.link();
    let mut dir = dev.dev_dir.clone();
    for _ in 0..4 {
        let Some(parent) = dir.parent().map(|p| p.to_path_buf()) else {
            break;
        };
        if !parent.join("current_link_speed").exists() {
            break;
        }
        let is_intel = crate::util::read_str(parent.join("vendor")).as_deref() == Some("0x8086");
        if !is_intel {
            // First non-card bridge: the slot's root/downstream port.
            best.slot_speed = crate::util::read_str(parent.join("max_link_speed"));
            best.slot_width = crate::util::read_u64(parent.join("max_link_width"));
            break;
        }
        best = Device {
            dev_dir: parent.clone(),
            ..dev.clone()
        }
        .link();
        dir = parent;
    }
    best
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Sensors,
    Device,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Overview, Tab::Sensors, Tab::Device];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Sensors => "Sensors",
            Tab::Device => "Device",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Gpu,
    Vram,
    Gtt,
    Cpu,
    Rss,
    Pid,
    User,
    Name,
}

impl SortKey {
    const ORDER: [SortKey; 8] = [
        SortKey::Gpu,
        SortKey::Vram,
        SortKey::Gtt,
        SortKey::Cpu,
        SortKey::Rss,
        SortKey::Pid,
        SortKey::User,
        SortKey::Name,
    ];

    pub fn title(self) -> &'static str {
        match self {
            SortKey::Gpu => "GPU%",
            SortKey::Vram => "VRAM",
            SortKey::Gtt => "GTT",
            SortKey::Cpu => "CPU%",
            SortKey::Rss => "RES",
            SortKey::Pid => "PID",
            SortKey::User => "USER",
            SortKey::Name => "COMMAND",
        }
    }

    fn step(self, d: isize) -> SortKey {
        let i = Self::ORDER.iter().position(|k| *k == self).unwrap_or(0) as isize;
        Self::ORDER[(i + d).rem_euclid(Self::ORDER.len() as isize) as usize]
    }

    fn default_desc(self) -> bool {
        !matches!(self, SortKey::Pid | SortKey::User | SortKey::Name)
    }
}

pub enum Mode {
    Normal,
    Filter,
    ConfirmKill { pid: u32, name: String, signal: i32 },
}

pub struct App {
    pub views: Vec<DeviceView>,
    pub sel: usize,
    pub tab: Tab,
    pub sort: SortKey,
    pub sort_desc: bool,
    pub table: TableState,
    pub selected_pid: Option<u32>,
    pub paused: bool,
    pub mode: Mode,
    pub filter: String,
    pub show_help: bool,
    pub interval_ms: Arc<AtomicU64>,
    pub status: Option<(String, Instant)>,
    pub quit: bool,
    pub started: Instant,
    /// Row pids from the last frame, used for selection movement.
    pub visible_pids: Vec<u32>,
    pub page_rows: usize,
    detail: Arc<AtomicBool>,
    ctl: Sender<Control>,
}

impl App {
    pub fn new(
        devices: Vec<Device>,
        initial: usize,
        interval_ms: Arc<AtomicU64>,
        detail: Arc<AtomicBool>,
        ctl: Sender<Control>,
    ) -> Self {
        Self {
            views: devices.into_iter().map(DeviceView::new).collect(),
            sel: initial,
            tab: Tab::Overview,
            sort: SortKey::Vram,
            sort_desc: true,
            table: TableState::default(),
            selected_pid: None,
            paused: false,
            mode: Mode::Normal,
            filter: String::new(),
            show_help: false,
            interval_ms,
            status: None,
            quit: false,
            started: Instant::now(),
            visible_pids: Vec::new(),
            page_rows: 10,
            detail,
            ctl,
        }
    }

    pub fn view(&self) -> &DeviceView {
        &self.views[self.sel]
    }

    pub fn interval(&self) -> u64 {
        self.interval_ms.load(Ordering::Relaxed)
    }

    fn flash(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
    }

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => self.on_key(k),
            Msg::Input(_) => {}
            Msg::Sample(samples) => {
                if self.paused {
                    return;
                }
                for (v, s) in self.views.iter_mut().zip(samples) {
                    v.absorb(s);
                }
            }
        }
    }

    /// Processes for the selected device after filtering and sorting.
    pub fn rows(&self) -> Vec<&ProcGpu> {
        let f = self.filter.to_lowercase();
        let mut rows: Vec<&ProcGpu> = self
            .view()
            .procs
            .procs
            .iter()
            .filter(|p| {
                f.is_empty()
                    || p.name.to_lowercase().contains(&f)
                    || p.cmdline.to_lowercase().contains(&f)
                    || p.user.to_lowercase().contains(&f)
                    || p.pid.to_string().contains(&f)
            })
            .collect();
        let key = self.sort;
        rows.sort_by(|a, b| {
            let o = match key {
                SortKey::Gpu => a.gpu_pct().total_cmp(&b.gpu_pct()),
                SortKey::Vram => a.vram_kib.cmp(&b.vram_kib),
                SortKey::Gtt => a.gtt_kib.cmp(&b.gtt_kib),
                SortKey::Cpu => a
                    .cpu_pct
                    .unwrap_or(0.0)
                    .total_cmp(&b.cpu_pct.unwrap_or(0.0)),
                SortKey::Rss => a.rss_kib.cmp(&b.rss_kib),
                SortKey::Pid => a.pid.cmp(&b.pid),
                SortKey::User => a.user.cmp(&b.user),
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            };
            let o = if self.sort_desc { o.reverse() } else { o };
            o.then(a.pid.cmp(&b.pid))
        });
        rows
    }

    fn move_sel(&mut self, delta: isize) {
        if self.visible_pids.is_empty() {
            return;
        }
        let cur = self
            .selected_pid
            .and_then(|p| self.visible_pids.iter().position(|x| *x == p))
            .unwrap_or(0) as isize;
        let n = self.visible_pids.len() as isize;
        let next = cur.saturating_add(delta).clamp(0, n - 1) as usize;
        self.selected_pid = Some(self.visible_pids[next]);
    }

    fn selected_proc(&self) -> Option<&ProcGpu> {
        let pid = self.selected_pid?;
        self.view().procs.procs.iter().find(|p| p.pid == pid)
    }

    fn set_interval(&mut self, ms: u64) {
        let ms = ms.clamp(100, 10_000);
        self.interval_ms.store(ms, Ordering::Relaxed);
        let _ = self.ctl.send(Control::Refresh);
        self.flash(format!("Refresh interval {ms} ms"));
    }

    fn set_tab(&mut self, t: Tab) {
        self.tab = t;
        self.detail.store(t == Tab::Sensors, Ordering::Relaxed);
        if t == Tab::Sensors {
            let _ = self.ctl.send(Control::Refresh);
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match &self.mode {
            Mode::Filter => {
                match k.code {
                    KeyCode::Esc => {
                        self.filter.clear();
                        self.mode = Mode::Normal;
                    }
                    KeyCode::Enter => self.mode = Mode::Normal,
                    KeyCode::Backspace => {
                        self.filter.pop();
                    }
                    KeyCode::Char(c) => self.filter.push(c),
                    _ => {}
                }
                return;
            }
            Mode::ConfirmKill { pid, name, signal } => {
                if matches!(
                    k.code,
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
                ) {
                    let (pid, name, signal) = (*pid, name.clone(), *signal);
                    // SAFETY: kill(2) with a concrete pid and signal number.
                    let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
                    let sig = if signal == libc::SIGKILL {
                        "SIGKILL"
                    } else {
                        "SIGTERM"
                    };
                    if rc == 0 {
                        self.flash(format!("Sent {sig} to {pid} ({name})"));
                    } else {
                        let err = std::io::Error::last_os_error();
                        self.flash(format!("kill {pid} failed: {err}"));
                    }
                }
                self.mode = Mode::Normal;
                return;
            }
            Mode::Normal => {}
        }

        if self.show_help {
            self.show_help = false;
            if k.code != KeyCode::Char('q') {
                return;
            }
        }

        match k.code {
            KeyCode::Char('q') | KeyCode::F(10) => self.quit = true,
            KeyCode::Esc => self.filter.clear(),
            KeyCode::Char('?') | KeyCode::Char('h') | KeyCode::F(1) => self.show_help = true,
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-(self.page_rows.max(1) as isize)),
            KeyCode::PageDown => self.move_sel(self.page_rows.max(1) as isize),
            KeyCode::Home => self.move_sel(isize::MIN),
            KeyCode::End => self.move_sel(isize::MAX),
            KeyCode::Tab => self.step_tab(1),
            KeyCode::BackTab => self.step_tab(-1),
            KeyCode::Char('1') => self.set_tab(Tab::Overview),
            KeyCode::Char('2') => self.set_tab(Tab::Sensors),
            KeyCode::Char('3') => self.set_tab(Tab::Device),
            KeyCode::Char('s') | KeyCode::Char('>') | KeyCode::Char('.') | KeyCode::F(6) => {
                self.sort = self.sort.step(1);
                self.sort_desc = self.sort.default_desc();
            }
            KeyCode::Char('<') | KeyCode::Char(',') => {
                self.sort = self.sort.step(-1);
                self.sort_desc = self.sort.default_desc();
            }
            KeyCode::Char('r') | KeyCode::Char('I') => self.sort_desc = !self.sort_desc,
            KeyCode::Char('g') => (self.sort, self.sort_desc) = (SortKey::Gpu, true),
            KeyCode::Char('m') => (self.sort, self.sort_desc) = (SortKey::Vram, true),
            KeyCode::Char('c') => (self.sort, self.sort_desc) = (SortKey::Cpu, true),
            KeyCode::Char('/') | KeyCode::F(3) | KeyCode::F(4) => self.mode = Mode::Filter,
            KeyCode::Char('p') | KeyCode::Char(' ') => {
                self.paused = !self.paused;
                self.flash(if self.paused { "Paused" } else { "Resumed" });
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                let i = self.interval();
                self.set_interval(if i >= 1000 { i + 500 } else { i + 100 });
            }
            KeyCode::Char('-') | KeyCode::Char('_') => {
                let i = self.interval();
                self.set_interval(if i > 1000 {
                    i - 500
                } else {
                    i.saturating_sub(100)
                });
            }
            KeyCode::Char('d') | KeyCode::Char(']') if self.views.len() > 1 => {
                self.sel = (self.sel + 1) % self.views.len();
                self.selected_pid = None;
            }
            KeyCode::Char('[') if self.views.len() > 1 => {
                self.sel = (self.sel + self.views.len() - 1) % self.views.len();
                self.selected_pid = None;
            }
            KeyCode::Char('k') | KeyCode::Char('K') | KeyCode::F(9) => {
                let signal = if k.code == KeyCode::Char('K') {
                    libc::SIGKILL
                } else {
                    libc::SIGTERM
                };
                if let Some(p) = self.selected_proc() {
                    self.mode = Mode::ConfirmKill {
                        pid: p.pid,
                        name: p.name.clone(),
                        signal,
                    };
                }
            }
            _ => {}
        }
    }

    fn step_tab(&mut self, d: isize) {
        let i = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0) as isize;
        self.set_tab(Tab::ALL[(i + d).rem_euclid(Tab::ALL.len() as isize) as usize]);
    }
}
