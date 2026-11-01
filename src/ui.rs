// SPDX-License-Identifier: GPL-2.0-only
//! View: renders the model. Pure function of `App` apart from table state.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Cell, Chart, Clear, Dataset, GraphType, HighlightSpacing,
    LegendPosition, Paragraph, Row, Sparkline, SparklineBar, Table, Wrap,
};

use crate::app::{App, DeviceView, Mode, Series, SortKey, Tab};
use crate::device::GtKind;
use crate::fdinfo::{ProcGpu, engine_label};
use crate::sysfs::{GtSample, TempSensor};
use crate::theme::{self, Gradient, bar, pill, *};
use crate::util::{fmt_bytes, fmt_duration, fmt_energy, fmt_kib};

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    draw_header(f, header, app);
    match app.tab {
        Tab::Overview => draw_overview(f, body, app),
        Tab::Sensors => draw_sensors(f, body, app),
        Tab::Device => draw_device(f, body, app),
    }
    draw_footer(f, footer, app);
    if app.show_help {
        draw_help(f);
    }
    if let Mode::ConfirmKill { pid, name, signal } = &app.mode {
        draw_confirm(f, *pid, name, *signal);
    }
}

// ───────────────────────────── shared pieces ─────────────────────────────

fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(title.to_string(), bold(PINK)),
            Span::raw(" "),
        ]))
}

fn right_title(spans: Vec<Span<'static>>) -> Line<'static> {
    let mut v = vec![Span::raw(" ")];
    v.extend(spans);
    v.push(Span::raw(" "));
    Line::from(v).right_aligned()
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        s.chars().take(w).collect()
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

fn rpad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(w - n))
    }
}

/// `label ▕gradient bar▏ value` sized to `width`.
#[allow(clippy::too_many_arguments)]
fn meter(
    label: &str,
    label_style: Style,
    lw: usize,
    ratio: f64,
    grad: &Gradient,
    value: Span<'static>,
    vw: usize,
    width: u16,
) -> Line<'static> {
    let bw = (width as usize).saturating_sub(lw + vw + 2);
    let mut spans = vec![Span::styled(pad(label, lw), label_style), Span::raw(" ")];
    spans.extend(bar(bw, ratio, grad));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(rpad(&value.content, vw), value.style));
    Line::from(spans)
}

fn kv(name: &str, lw: usize, value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::styled(pad(name, lw), label())];
    spans.extend(value);
    Line::from(spans)
}

fn pretty_sensor(l: &str) -> String {
    match l {
        "pkg" => "Package".into(),
        "vram" => "VRAM".into(),
        "mctrl" => "Mem ctrl".into(),
        "pcie" => "PCIe".into(),
        "card" => "Card".into(),
        other => match other.strip_prefix("vram_ch_") {
            Some(n) => format!("VRAM ch{n}"),
            None => {
                let mut c = other.chars();
                c.next()
                    .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                    .unwrap_or_default()
            }
        },
    }
}

fn pct_span(p: Option<f64>) -> Span<'static> {
    match p {
        Some(p) => Span::styled(format!("{p:.1}%"), Style::new().fg(pct_color(p))),
        None => Span::styled("—", dim()),
    }
}

fn temp_span(c: f64) -> Span<'static> {
    Span::styled(format!("{c:.0}°C"), bold(temp_color(c)))
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn ip_name(major: u16) -> &'static str {
    match major {
        12 => "Xe",
        20 => "Xe2",
        30 => "Xe3",
        35 => "Xe3P",
        _ => "",
    }
}

// ───────────────────────────── header / footer ─────────────────────────────

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let v = app.view();
    let mut right: Vec<Span> = Vec::new();
    for (i, t) in Tab::ALL.iter().enumerate() {
        if *t == app.tab {
            right.push(pill(format!("{} {}", i + 1, t.title()), PINK));
        } else {
            right.push(Span::styled(format!(" {} {} ", i + 1, t.title()), muted()));
        }
    }
    if app.paused {
        right.push(Span::raw(" "));
        right.push(pill("PAUSED", ORANGE));
    }
    right.push(Span::styled(format!("  {} ", crate::util::clock()), text()));
    let rw = right
        .iter()
        .map(|s| s.content.chars().count())
        .sum::<usize>() as u16;

    let mut left = vec![
        pill("xtop", PURPLE),
        Span::raw(" "),
        Span::styled(v.dev.name.clone(), bold(CREAM)),
    ];
    left.push(Span::styled(
        format!("  {} · {} · {}", v.dev.bdf, v.dev.driver.name(), v.dev.card),
        muted(),
    ));
    if app.views.len() > 1 {
        left.push(Span::styled(
            format!("  [{}/{}]", app.sel + 1, app.views.len()),
            bold(CYAN),
        ));
    }
    let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(rw)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(left)), l);
    f.render_widget(Paragraph::new(Line::from(right)), r);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let line = if matches!(app.mode, Mode::Filter) {
        Line::from(vec![
            pill("FILTER", PINK),
            Span::raw(" "),
            Span::styled(app.filter.clone(), bold(CREAM)),
            Span::styled("█", Style::new().fg(PINK)),
            Span::styled("   enter apply • esc clear", dim()),
        ])
    } else if let Some((msg, at)) = app
        .status
        .as_ref()
        .filter(|(_, at)| at.elapsed() < Duration::from_secs(3))
    {
        let _ = at;
        Line::from(vec![
            Span::styled(" ● ", bold(GREEN)),
            Span::styled(msg.clone(), text()),
        ])
    } else {
        let interval = format!("{}ms", app.interval());
        let mut keys: Vec<(&str, &str)> = vec![
            ("q", "quit"),
            ("↑↓", "select"),
            ("s", "sort"),
            ("r", "reverse"),
            ("/", "filter"),
            ("k", "kill"),
            ("p", "pause"),
            ("+/-", interval.as_str()),
            ("tab", "view"),
        ];
        if app.views.len() > 1 {
            keys.push(("d", "device"));
        }
        keys.push(("?", "help"));
        // Drop the least important hints until the line fits.
        let filter_w = if app.filter.is_empty() {
            0
        } else {
            11 + app.filter.chars().count()
        };
        let width = |ks: &[(&str, &str)]| -> usize {
            1 + ks
                .iter()
                .map(|(k, d)| k.chars().count() + d.chars().count() + 4)
                .sum::<usize>()
                + filter_w
        };
        for drop in ["r", "p", "+/-", "↑↓", "k", "tab", "s", "/"] {
            if width(&keys) <= area.width as usize {
                break;
            }
            keys.retain(|(k, _)| *k != drop);
        }
        let mut spans = vec![Span::raw(" ")];
        for (i, (k, d)) in keys.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" • ", Style::new().fg(TRACK)));
            }
            spans.push(Span::styled(k.to_string(), bold(MUTED)));
            spans.push(Span::styled(format!(" {d}"), dim()));
        }
        if !app.filter.is_empty() {
            spans.push(Span::styled("   filter: ", dim()));
            spans.push(Span::styled(app.filter.clone(), bold(PINK)));
        }
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(line), area);
}

// ───────────────────────────── overview ─────────────────────────────

fn draw_overview(f: &mut Frame, area: Rect, app: &mut App) {
    let v = app.view();
    let engines = v.engine_names();
    let hw = v.hw.as_ref();
    let main_temps = hw
        .map(|h| h.temps.iter().filter(|t| !t.is_vram_channel()).count())
        .unwrap_or(0);
    let has_ch = hw.is_some_and(|h| h.temps.iter().any(TempSensor::is_vram_channel));
    let fans = hw.map(|h| h.fans.len()).unwrap_or(0);
    let rails = hw.map(|h| h.rails.len()).unwrap_or(0);

    let gpu_rows = 2 + engines.len();
    let therm_rows = main_temps + has_ch as usize + fans;
    let power_rows = rails + v.dev.gts.len() + 2;
    let top_h = (gpu_rows.max(therm_rows).max(power_rows).clamp(4, 12) + 2) as u16;
    let rest = area.height.saturating_sub(top_h);
    let hist_h = match rest {
        r if r >= 26 => 9,
        r if r >= 18 => 7,
        r if r >= 13 => 5,
        _ => 0,
    };
    // Give space the process table doesn't need to the history graphs.
    let procs_need = (v.procs.procs.len() as u16 + 4).max(8);
    let hist_h = if hist_h > 0 {
        hist_h.max(rest.saturating_sub(procs_need).min(18))
    } else {
        0
    };
    let [top, hist, procs] = Layout::vertical([
        Constraint::Length(top_h),
        Constraint::Length(hist_h),
        Constraint::Fill(1),
    ])
    .areas(area);

    let [a, b, c] = Layout::horizontal([
        Constraint::Percentage(36),
        Constraint::Percentage(30),
        Constraint::Percentage(34),
    ])
    .areas(top);
    draw_gpu_panel(f, a, v, &engines);
    draw_thermal_panel(f, b, v);
    draw_power_panel(f, c, v);
    if hist_h > 0 {
        draw_history(f, hist, v);
    }
    draw_procs(f, procs, app);
}

fn draw_gpu_panel(f: &mut Frame, area: Rect, v: &DeviceView, engines: &[String]) {
    let block = panel("GPU").title(right_title(vec![Span::styled(
        format!(
            "{} client{}",
            v.procs.clients,
            if v.procs.clients == 1 { "" } else { "s" }
        ),
        muted(),
    )]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width;
    let mut lines = Vec::new();
    let util = v.gpu_util();
    lines.push(meter(
        "GPU",
        bold(CREAM),
        8,
        util.unwrap_or(0.0) / 100.0,
        &UTIL,
        Span::styled(
            util.map(|u| format!("{u:.1}%")).unwrap_or("—".into()),
            bold(CREAM),
        ),
        11,
        w,
    ));
    for e in engines {
        let p = v.procs.engine_totals.get(e).copied().unwrap_or(0.0);
        lines.push(meter(
            &engine_label(e),
            label(),
            8,
            p / 100.0,
            &UTIL,
            Span::styled(format!("{p:.1}%"), Style::new().fg(pct_color(p))),
            11,
            w,
        ));
    }
    match v.vram() {
        Some((used, Some(total), _)) => lines.push(meter(
            "VRAM",
            bold(CREAM),
            8,
            used / total,
            &MEM,
            Span::styled(
                format!("{}/{}", fmt_bytes(used), fmt_bytes(total)),
                bold(CYAN),
            ),
            11,
            w,
        )),
        Some((used, None, _)) => lines.push(kv(
            "VRAM",
            9,
            vec![
                Span::styled(fmt_bytes(used), bold(CYAN)),
                Span::styled("  (visible processes)", dim()),
            ],
        )),
        None => lines.push(kv("VRAM", 9, vec![Span::styled("—", dim())])),
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_thermal_panel(f: &mut Frame, area: Rect, v: &DeviceView) {
    let block = panel("Thermals");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(hw) = v.hw.as_ref() else {
        f.render_widget(
            Paragraph::new(Span::styled("waiting for sample…", dim())),
            inner,
        );
        return;
    };
    let w = inner.width;
    let mut lines = Vec::new();
    for t in hw.temps.iter().filter(|t| !t.is_vram_channel()) {
        let limit = t.crit.unwrap_or(100.0).max(1.0);
        lines.push(meter(
            &pretty_sensor(&t.label),
            label(),
            8,
            t.celsius / limit,
            &HEAT,
            temp_span(t.celsius),
            5,
            w,
        ));
    }
    let ch: Vec<&TempSensor> = hw.temps.iter().filter(|t| t.is_vram_channel()).collect();
    if !ch.is_empty() {
        let lo = ch.iter().map(|t| t.celsius).fold(f64::INFINITY, f64::min);
        let hi = ch
            .iter()
            .map(|t| t.celsius)
            .fold(f64::NEG_INFINITY, f64::max);
        let range = format!("{lo:.0}–{hi:.0}°C");
        let avail = (w as usize).saturating_sub(9 + range.chars().count() + 1);
        let cell = if avail >= ch.len() * 2 {
            "▆▆"
        } else {
            "▆"
        };
        let mut spans = vec![Span::styled(pad("VRAM ch", 8), label()), Span::raw(" ")];
        let shown = (avail / cell.chars().count()).min(ch.len());
        for t in ch.iter().take(shown) {
            spans.push(Span::styled(cell, Style::new().fg(temp_color(t.celsius))));
        }
        let used: usize = 9 + shown * cell.chars().count();
        spans.push(Span::raw(
            " ".repeat((w as usize).saturating_sub(used + range.chars().count())),
        ));
        spans.push(Span::styled(range, Style::new().fg(temp_color(hi))));
        lines.push(Line::from(spans));
    }
    for (name, rpm) in &hw.fans {
        let label_text = if hw.fans.len() > 1 {
            pretty_sensor(name)
        } else {
            "Fan".into()
        };
        lines.push(kv(
            &label_text,
            9,
            vec![
                Span::styled(format!("{rpm}"), bold(CYAN)),
                Span::styled(" RPM", muted()),
            ],
        ));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn freq_line(g: &GtSample, w: u16) -> Line<'static> {
    let top = g.rp0_mhz.or(g.max_mhz).unwrap_or(1).max(1) as f64;
    let act = g.act_mhz.unwrap_or(0);
    let value = if act == 0 {
        Span::styled("idle", dim())
    } else {
        Span::styled(format!("{act} MHz"), bold(CREAM))
    };
    meter(
        g.kind.label(),
        label(),
        8,
        act as f64 / top,
        &FREQ,
        value,
        9,
        w,
    )
}

fn draw_power_panel(f: &mut Frame, area: Rect, v: &DeviceView) {
    let block = panel("Power · Clocks");
    let profile =
        v.hw.as_ref()
            .and_then(|h| h.gts.iter().find_map(|g| g.power_profile.clone()));
    let block = match profile {
        Some(p) => block.title(right_title(vec![
            Span::styled("profile ", dim()),
            Span::styled(p, muted()),
        ])),
        None => block,
    };
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(hw) = v.hw.as_ref() else { return };
    let w = inner.width;
    let mut lines = Vec::new();
    let card_cap = hw.rail("card").and_then(|r| r.cap_w);
    for r in &hw.rails {
        let cap = r.cap_w.or(card_cap).unwrap_or(300.0).max(1.0);
        let watts = r.watts.unwrap_or(0.0);
        let value = match r.watts {
            Some(w) => Span::styled(format!("{w:.1} W"), bold(CREAM)),
            None => Span::styled("…", dim()),
        };
        lines.push(meter(
            &pretty_sensor(&r.label),
            label(),
            8,
            watts / cap,
            &HEAT,
            value,
            9,
            w,
        ));
    }
    for g in &hw.gts {
        lines.push(freq_line(g, w));
    }
    let mut act = vec![];
    for g in &hw.gts {
        if !act.is_empty() {
            act.push(Span::styled("  ", dim()));
        }
        act.push(Span::styled(
            format!("{} ", g.kind.label().to_lowercase()),
            muted(),
        ));
        act.push(pct_span(g.awake_pct));
    }
    lines.push(kv("Awake", 9, act));
    let reasons: Vec<String> = hw
        .gts
        .iter()
        .flat_map(|g| g.throttle_reasons.clone())
        .collect();
    if reasons.is_empty() {
        lines.push(kv("Throttle", 9, vec![Span::styled("none", bold(GREEN))]));
    } else {
        let mut uniq = reasons;
        uniq.sort();
        uniq.dedup();
        lines.push(kv(
            "Throttle",
            9,
            vec![Span::styled(uniq.join(" "), bold(RED))],
        ));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

#[allow(clippy::too_many_arguments)]
fn spark(
    f: &mut Frame,
    area: Rect,
    title: &str,
    now: String,
    sub: String,
    s: &Series,
    max: f64,
    grad: &Gradient,
) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(title.to_string(), bold(PINK)),
            Span::raw(" "),
            Span::styled(now, bold(CREAM)),
            Span::raw(" "),
        ]))
        .title_bottom(right_title(vec![Span::styled(sub, dim())]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let n = inner.width as usize;
    let max = max.max(1e-9);
    let pts: Vec<f64> = s.tail(n).collect();
    let mut bars: Vec<SparklineBar> =
        std::iter::repeat_n(SparklineBar::from(None::<u64>), n - pts.len()).collect();
    bars.extend(pts.iter().map(|v| {
        SparklineBar::from((v.max(0.0) * 100.0) as u64).style(Style::new().fg(grad.at(v / max)))
    }));
    f.render_widget(
        Sparkline::default().data(bars).max((max * 100.0) as u64),
        inner,
    );
}

fn draw_history(f: &mut Frame, area: Rect, v: &DeviceView) {
    let cols = Layout::horizontal([Constraint::Fill(1); 4]).split(area);
    let n = cols[0].width.saturating_sub(2) as usize;
    let h = &v.hist;

    spark(
        f,
        cols[0],
        "GPU",
        h.gpu.last().map(|x| format!("{x:.0}%")).unwrap_or_default(),
        format!(
            "avg {:.0}% · max {:.0}%",
            h.gpu.avg_tail(n),
            h.gpu.max_tail(n)
        ),
        &h.gpu,
        100.0,
        &UTIL,
    );
    let cap =
        v.hw.as_ref()
            .and_then(|hw| hw.board_power())
            .and_then(|r| r.cap_w);
    let pmax = (h.power.max_tail(n) * 1.15).max(cap.map(|c| c * 0.25).unwrap_or(25.0));
    spark(
        f,
        cols[1],
        "Power",
        h.power
            .last()
            .map(|x| format!("{x:.0} W"))
            .unwrap_or_default(),
        format!(
            "avg {:.0} · max {:.0} W",
            h.power.avg_tail(n),
            h.power.max_tail(n)
        ),
        &h.power,
        pmax,
        &HEAT,
    );
    let tcrit =
        v.hw.as_ref()
            .and_then(|hw| hw.primary_temp())
            .and_then(|t| t.crit)
            .unwrap_or(100.0);
    spark(
        f,
        cols[2],
        "Temp",
        h.temp
            .last()
            .map(|x| format!("{x:.0}°C"))
            .unwrap_or_default(),
        format!(
            "min {:.0} · max {:.0}°C",
            h.temp.min_tail(n).min(999.0),
            h.temp.max_tail(n)
        ),
        &h.temp,
        tcrit,
        &HEAT,
    );
    let fmax =
        v.hw.as_ref()
            .and_then(|hw| hw.gt(GtKind::Primary))
            .and_then(|g| g.rp0_mhz.or(g.max_mhz))
            .unwrap_or(3000) as f64;
    spark(
        f,
        cols[3],
        "Clock",
        h.freq
            .last()
            .map(|x| format!("{x:.0} MHz"))
            .unwrap_or_default(),
        format!(
            "avg {:.0} · max {:.0}",
            h.freq.avg_tail(n),
            h.freq.max_tail(n)
        ),
        &h.freq,
        fmax,
        &FREQ,
    );
}

fn draw_procs(f: &mut Frame, area: Rect, app: &mut App) {
    let rows_data: Vec<ProcGpu> = app.rows().into_iter().cloned().collect();
    let v = app.view();
    let engines = v.engine_names();
    let vram_total = v.vram().and_then(|(_, t, _)| t);
    let inner_w = area.width.saturating_sub(4) as usize;

    let show_engines = inner_w >= 104;
    let show_extra = inner_w >= 72;

    let arrow = if app.sort_desc { "▼" } else { "▲" };
    let sort = app.sort;
    let hdr = |title: &str, key: Option<SortKey>| -> Cell<'static> {
        if key == Some(sort) {
            Cell::from(Span::styled(format!("{title}{arrow}"), bold(PINK)))
        } else {
            Cell::from(Span::styled(title.to_string(), bold(MUTED)))
        }
    };

    let mut header = vec![
        hdr("PID", Some(SortKey::Pid)),
        hdr("USER", Some(SortKey::User)),
        hdr("GPU%", Some(SortKey::Gpu)),
    ];
    let mut widths = vec![
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(6),
    ];
    if show_engines {
        for e in &engines {
            let short = match engine_label(e).as_str() {
                "Compute" => "COMP".to_string(),
                "Render" => "REND".to_string(),
                "Copy" => "COPY".to_string(),
                "Video" => "VID".to_string(),
                "VidEnh" => "VENH".to_string(),
                other => other.to_uppercase().chars().take(5).collect(),
            };
            header.push(hdr(&short, None));
            widths.push(Constraint::Length(6));
        }
    }
    header.push(hdr("VRAM", Some(SortKey::Vram)));
    widths.push(Constraint::Length(8));
    if show_extra {
        header.push(hdr("GTT", Some(SortKey::Gtt)));
        widths.push(Constraint::Length(8));
    }
    header.push(hdr("CPU%", Some(SortKey::Cpu)));
    widths.push(Constraint::Length(6));
    if show_extra {
        header.push(hdr("RES", Some(SortKey::Rss)));
        widths.push(Constraint::Length(7));
    }
    header.push(hdr("COMMAND", Some(SortKey::Name)));
    widths.push(Constraint::Fill(1));

    let me = crate::util::is_root();
    let rows: Vec<Row> = rows_data
        .iter()
        .map(|p| {
            let mut cells = vec![
                Cell::from(Span::styled(p.pid.to_string(), text())),
                Cell::from(Span::styled(pad(&p.user, 9), Style::new().fg(CYAN))),
                Cell::from(pct_cell(p.gpu_pct(), true)),
            ];
            if show_engines {
                for e in &engines {
                    cells.push(Cell::from(pct_cell(
                        p.engines.get(e).copied().unwrap_or(0.0),
                        false,
                    )));
                }
            }
            let vram_color = match vram_total {
                Some(t) if t > 0.0 => MEM.at(p.vram_kib as f64 * 1024.0 / t * 2.0),
                _ => CYAN,
            };
            cells.push(Cell::from(Span::styled(
                rpad(&fmt_kib(p.vram_kib), 7),
                Style::new().fg(if p.vram_kib == 0 { DIM } else { vram_color }),
            )));
            if show_extra {
                cells.push(Cell::from(Span::styled(
                    rpad(&fmt_kib(p.gtt_kib), 7),
                    Style::new().fg(if p.gtt_kib == 0 { DIM } else { TEXT }),
                )));
            }
            let cpu = p.cpu_pct.unwrap_or(0.0);
            cells.push(Cell::from(Span::styled(
                rpad(&format!("{cpu:.1}"), 5),
                Style::new().fg(if cpu < 0.5 { DIM } else { HEAT.at(cpu / 400.0) }),
            )));
            if show_extra {
                cells.push(Cell::from(Span::styled(
                    rpad(&fmt_kib(p.rss_kib), 6),
                    text(),
                )));
            }
            cells.push(Cell::from(command_line(p)));
            Row::new(cells)
        })
        .collect();

    // Keep the selection on the same pid across refreshes and re-sorts.
    let pids: Vec<u32> = rows_data.iter().map(|p| p.pid).collect();
    let sel_idx = app
        .selected_pid
        .and_then(|p| pids.iter().position(|x| *x == p));
    let sel_idx = match (sel_idx, pids.is_empty()) {
        (Some(i), _) => Some(i),
        (None, false) => {
            app.selected_pid = Some(pids[0]);
            Some(0)
        }
        (None, true) => None,
    };
    app.table.select(sel_idx);
    app.visible_pids = pids;
    app.page_rows = area.height.saturating_sub(4) as usize;

    let v = app.view();
    let total = v.procs.procs.len();
    let mut rt = vec![
        Span::styled("sort ", dim()),
        Span::styled(format!("{} {arrow}", sort.title()), bold(PINK)),
    ];
    rt.push(Span::styled(
        if rows_data.len() == total {
            format!(" · {total} procs")
        } else {
            format!(" · {}/{total} procs", rows_data.len())
        },
        muted(),
    ));
    let mut block = panel("Processes").title(right_title(rt));
    if !me {
        block = block.title_bottom(
            Line::from(Span::styled(
                " own processes only — run as root to see every user's GPU clients ",
                dim(),
            ))
            .right_aligned(),
        );
    }

    if rows.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let msg = if app.filter.is_empty() {
            "No GPU clients"
        } else {
            "No processes match the filter"
        };
        let y = inner.y + inner.height / 2;
        f.render_widget(
            Paragraph::new(Span::styled(msg, muted())).centered(),
            Rect::new(
                inner.x,
                y.min(inner.bottom().saturating_sub(1)),
                inner.width,
                1,
            ),
        );
        return;
    }

    let table = Table::new(rows, widths)
        .header(
            Row::new(header)
                .style(Style::new().bg(SURFACE))
                .bottom_margin(0),
        )
        .block(block)
        .column_spacing(1)
        .row_highlight_style(Style::new().bg(SELECT_BG).add_modifier(Modifier::BOLD))
        .highlight_symbol(Line::from(Span::styled("▌", Style::new().fg(PINK))))
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(table, area, &mut app.table);
}

fn pct_cell(p: f64, strong: bool) -> Span<'static> {
    if p < 0.05 {
        Span::styled(rpad("·", 5), dim())
    } else {
        let st = Style::new().fg(pct_color(p));
        Span::styled(
            rpad(&format!("{p:.0}%"), 5),
            if strong {
                st.add_modifier(Modifier::BOLD)
            } else {
                st
            },
        )
    }
}

/// htop-style command: dim directory, bold basename, normal arguments.
fn command_line(p: &ProcGpu) -> Line<'static> {
    let (argv0, rest) = match p.cmdline.split_once(' ') {
        Some((a, r)) => (a, r),
        None => (p.cmdline.as_str(), ""),
    };
    let (dir, base) = match argv0.rfind('/') {
        Some(i) => (&argv0[..=i], &argv0[i + 1..]),
        None => ("", argv0),
    };
    let mut spans = vec![];
    if !dir.is_empty() {
        spans.push(Span::styled(dir.to_string(), dim()));
    }
    spans.push(Span::styled(base.to_string(), bold(CREAM)));
    if !rest.is_empty() {
        spans.push(Span::styled(format!(" {rest}"), muted()));
    }
    Line::from(spans)
}

// ───────────────────────────── sensors ─────────────────────────────

fn draw_sensors(f: &mut Frame, area: Rect, app: &App) {
    let v = app.view();
    let Some(hw) = v.hw.as_ref() else {
        f.render_widget(
            Paragraph::new(Span::styled(" waiting for first sample…", dim())),
            area,
        );
        return;
    };
    let main: Vec<&TempSensor> = hw.temps.iter().filter(|t| !t.is_vram_channel()).collect();
    let ch: Vec<&TempSensor> = hw.temps.iter().filter(|t| t.is_vram_channel()).collect();
    // Header + one row per GT, a gap, then throttle flags (which may wrap).
    let clocks_h = 2 + hw.gts.len() * 3;
    let top_h = ((main.len() + 1).max(clocks_h) + 2) as u16;
    let ch_rows = ch.len().div_ceil(4);
    let mid_h = (ch_rows.max(hw.rails.len() + hw.fans.len() + 3) + 2) as u16;

    let [top, mid, bottom] = Layout::vertical([
        Constraint::Length(top_h),
        Constraint::Length(mid_h),
        Constraint::Min(6),
    ])
    .areas(area);
    let [tl, tr] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(top);
    let [ml, mr] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(mid);

    // Temperatures table.
    let w = tl.width.saturating_sub(2);
    let bar_w = w.saturating_sub(11 + 6 + 7 + 7 + 7 + 5).max(4) as usize;
    let mut rows: Vec<Row> = Vec::new();
    for t in &main {
        let limit = t.crit.unwrap_or(100.0).max(1.0);
        let peak = v
            .hist
            .temps
            .get(&t.label)
            .map(|s| s.max_tail(usize::MAX))
            .unwrap_or(t.celsius);
        rows.push(Row::new(vec![
            Cell::from(Span::styled(pretty_sensor(&t.label), text())),
            Cell::from(temp_span(t.celsius)),
            Cell::from(Line::from(bar(bar_w, t.celsius / limit, &HEAT))),
            Cell::from(Span::styled(
                format!("{peak:.0}°C"),
                Style::new().fg(temp_color(peak)),
            )),
            Cell::from(Span::styled(
                t.crit.map(|c| format!("{c:.0}°C")).unwrap_or("—".into()),
                muted(),
            )),
            Cell::from(Span::styled(
                t.emergency
                    .map(|c| format!("{c:.0}°C"))
                    .unwrap_or("—".into()),
                dim(),
            )),
        ]));
    }
    let header = Row::new(
        ["Sensor", "Now", "", "Peak", "Crit", "Shutdn"]
            .map(|h| Cell::from(Span::styled(h, bold(MUTED)))),
    );
    f.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Length(5),
                Constraint::Length(bar_w as u16),
                Constraint::Length(6),
                Constraint::Length(6),
                Constraint::Length(6),
            ],
        )
        .header(header)
        .block(panel("Temperatures")),
        tl,
    );

    // Clocks per GT plus throttle flags; the RP* detail columns need width.
    let wide = tr.width >= 88;
    let mhz = |m: Option<u64>| m.map(|v| v.to_string()).unwrap_or("—".into());
    let mut rows: Vec<Row> = Vec::new();
    for g in &hw.gts {
        let act = match g.act_mhz {
            Some(0) => "C6".into(),
            m => mhz(m),
        };
        let mut cells = vec![
            Cell::from(Span::styled(
                format!("gt{} {}", g.id, g.kind.label()),
                text(),
            )),
            Cell::from(Span::styled(act, bold(CREAM))),
            Cell::from(Span::styled(mhz(g.cur_mhz), text())),
            Cell::from(Span::styled(
                format!("{}–{}", mhz(g.min_mhz), mhz(g.max_mhz)),
                muted(),
            )),
        ];
        if wide {
            cells.push(Cell::from(Span::styled(mhz(g.rpn_mhz), dim())));
            cells.push(Cell::from(Span::styled(mhz(g.rpe_mhz), dim())));
            cells.push(Cell::from(Span::styled(mhz(g.rpa_mhz), dim())));
        }
        cells.push(Cell::from(Span::styled(mhz(g.rp0_mhz), muted())));
        cells.push(Cell::from(pct_span(g.awake_pct)));
        cells.push(Cell::from(Span::styled(
            g.idle_status.clone().unwrap_or("—".into()),
            muted(),
        )));
        rows.push(Row::new(cells));
    }
    let mut titles = vec!["GT", "Act", "Req", "Min–Max"];
    let mut widths = vec![
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Length(10),
    ];
    if wide {
        titles.extend(["RPn", "RPe", "RPa"]);
        widths.extend([Constraint::Length(5); 3]);
    }
    titles.extend(["RP0", "Awake", "State"]);
    widths.extend([
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Fill(1),
    ]);
    let header = Row::new(
        titles
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, bold(MUTED)))),
    );
    let block = panel("Clocks (MHz)").title(right_title(vec![Span::styled(
        "throttle ● active ○ clear",
        dim(),
    )]));
    let inner = block.inner(tr);
    f.render_widget(block, tr);
    let [table_area, _, flags_area] = Layout::vertical([
        Constraint::Length(1 + hw.gts.len() as u16),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);
    f.render_widget(Table::new(rows, widths).header(header), table_area);
    let flags: Vec<Line> = hw
        .gts
        .iter()
        .map(|g| {
            let mut spans = vec![Span::styled(format!("gt{} throttle", g.id), muted())];
            for (name, on) in &g.throttle_flags {
                spans.push(if *on {
                    Span::styled(format!(" ● {name}"), bold(RED))
                } else {
                    Span::styled(format!(" ○ {name}"), dim())
                });
            }
            if g.throttle_flags.is_empty() {
                spans.push(Span::styled(
                    if g.throttled { " throttled" } else { " none" },
                    text(),
                ));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(flags).wrap(Wrap { trim: true }), flags_area);

    // VRAM channel grid.
    let block = panel("VRAM channels");
    let inner = block.inner(ml);
    f.render_widget(block, ml);
    if ch.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled("no per-channel sensors", dim())),
            inner,
        );
    } else {
        let cell_w = (inner.width / 4).max(8) as usize;
        let lines: Vec<Line> = ch
            .chunks(4)
            .map(|row| {
                Line::from(
                    row.iter()
                        .flat_map(|t| {
                            let n = t.label.trim_start_matches("vram_ch_");
                            let c = temp_color(t.celsius);
                            let txt = format!("ch{n:<2} {:.0}°", t.celsius);
                            vec![
                                Span::styled("▐", Style::new().fg(c)),
                                Span::styled(
                                    pad(&txt, cell_w.saturating_sub(2)),
                                    Style::new().fg(c),
                                ),
                                Span::raw(" "),
                            ]
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    // Power, fans and memory.
    let block = panel("Power · Fan · Memory");
    let inner = block.inner(mr);
    f.render_widget(block, mr);
    let mut lines = Vec::new();
    for r in &hw.rails {
        let mut s = vec![Span::styled(
            r.watts
                .map(|w| format!("{w:>6.1} W"))
                .unwrap_or("     … W".into()),
            bold(CREAM),
        )];
        if let Some(c) = r.cap_w {
            s.push(Span::styled(format!("   cap {c:.0} W"), muted()));
        }
        if let Some(c) = r.crit_w {
            s.push(Span::styled(format!("   crit {c:.0} W"), dim()));
        }
        if let Some(e) = r.energy_j {
            s.push(Span::styled(format!("   Σ {}", fmt_energy(e)), dim()));
        }
        lines.push(kv(&pretty_sensor(&r.label), 10, s));
    }
    for (name, rpm) in &hw.fans {
        lines.push(kv(
            &pretty_sensor(name),
            10,
            vec![Span::styled(format!("{rpm:>6} RPM"), bold(CYAN))],
        ));
    }
    if let Some((used, total, vis)) = hw.vram() {
        lines.push(kv(
            "VRAM",
            10,
            vec![
                Span::styled(
                    format!("{} / {}", fmt_bytes(used as f64), fmt_bytes(total as f64)),
                    bold(CYAN),
                ),
                Span::styled(
                    format!("   ({:.1}%)", used as f64 / total.max(1) as f64 * 100.0),
                    muted(),
                ),
            ],
        ));
        let bar_state = if vis >= total {
            ("full (ReBAR)", GREEN)
        } else {
            ("limited", ORANGE)
        };
        lines.push(kv(
            "CPU BAR",
            10,
            vec![
                Span::styled(fmt_bytes(vis as f64), text()),
                Span::styled(format!("   {}", bar_state.0), Style::new().fg(bar_state.1)),
            ],
        ));
    }
    for r in hw.mem_regions.iter().filter(|r| !r.vram) {
        lines.push(kv(
            "System",
            10,
            vec![Span::styled(
                format!("{} GPU-mapped", fmt_bytes(r.used as f64)),
                text(),
            )],
        ));
    }
    f.render_widget(Paragraph::new(lines), inner);

    draw_temp_chart(f, bottom, v, app.interval());
}

/// (legend name, line colour, (x, y) points)
type TempSeries = (String, Color, Vec<(f64, f64)>);

fn draw_temp_chart(f: &mut Frame, area: Rect, v: &DeviceView, interval_ms: u64) {
    let palette: [Color; 6] = [PINK, CYAN, YELLOW, GREEN, PURPLE, ORANGE];
    let n = area.width.saturating_sub(10) as usize * 2;
    let series: Vec<TempSeries> = v
        .hist
        .temps
        .iter()
        .enumerate()
        .map(|(i, (name, s))| {
            let pts: Vec<f64> = s.tail(n).collect();
            let off = n.saturating_sub(pts.len()) as f64;
            (
                pretty_sensor(name),
                palette[i % palette.len()],
                pts.iter()
                    .enumerate()
                    .map(|(x, y)| (off + x as f64, *y))
                    .collect(),
            )
        })
        .collect();
    let (lo, hi) = series
        .iter()
        .flat_map(|s| s.2.iter().map(|p| p.1))
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), y| {
            (a.min(y), b.max(y))
        });
    let (lo, hi) = if lo.is_finite() {
        ((lo - 3.0).floor().max(0.0), (hi + 3.0).ceil())
    } else {
        (20.0, 100.0)
    };
    let datasets: Vec<Dataset> = series
        .iter()
        .map(|(name, c, pts)| {
            Dataset::default()
                .name(name.clone())
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(*c))
                .data(pts)
        })
        .collect();
    let span_s = n as u64 * interval_ms / 1000;
    let chart = Chart::new(datasets)
        .block(panel("Temperature history"))
        .legend_position(Some(LegendPosition::TopLeft))
        .x_axis(
            Axis::default()
                .bounds([0.0, n.max(1) as f64])
                .labels([
                    Line::from(format!("-{}", fmt_duration(span_s))),
                    Line::from("now"),
                ])
                .style(dim()),
        )
        .y_axis(
            Axis::default()
                .bounds([lo, hi])
                .labels([
                    Line::from(format!("{lo:.0}°")),
                    Line::from(format!("{:.0}°", (lo + hi) / 2.0)),
                    Line::from(format!("{hi:.0}°")),
                ])
                .style(dim()),
        );
    f.render_widget(chart, area);
}

// ───────────────────────────── device ─────────────────────────────

fn draw_device(f: &mut Frame, area: Rect, app: &App) {
    let v = app.view();
    let [l, r] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
    let lw = 14;
    let mut lines: Vec<Line> = Vec::new();
    let t = |s: String| vec![Span::styled(s, text())];
    let d = &v.dev;
    lines.push(kv(
        "Name",
        lw,
        vec![Span::styled(d.name.clone(), bold(CREAM))],
    ));
    if let Some(c) = &d.codename {
        lines.push(kv("Codename", lw, t(c.clone())));
    }
    let mut pci = format!("{}  rev {:02x}", d.pci_id(), d.revision);
    if let Some(s) = &d.subsystem {
        pci.push_str(&format!("  subsys {s}"));
    }
    lines.push(kv("PCI ID", lw, t(pci)));
    lines.push(kv(
        "Bus",
        lw,
        t(format!(
            "{}  {}  {}",
            d.bdf,
            d.card,
            d.render_node.clone().unwrap_or_default()
        )),
    ));
    lines.push(kv(
        "Driver",
        lw,
        t(format!(
            "{}  kernel {}",
            d.driver.name(),
            crate::util::kernel_release().unwrap_or_default()
        )),
    ));
    let link_style = if v.link.degraded() {
        bold(ORANGE)
    } else {
        bold(GREEN)
    };
    let mut link = vec![
        Span::styled(v.link.current(), link_style),
        Span::styled(
            format!("  {}", v.link.cur_speed.clone().unwrap_or_default()),
            dim(),
        ),
    ];
    if v.link.slot_limited() {
        link.push(Span::styled(
            format!(
                "  slot max {} · card {}",
                v.link.effective_max_desc(),
                v.link.max()
            ),
            muted(),
        ));
    } else {
        link.push(Span::styled(format!("  max {}", v.link.max()), muted()));
    }
    if v.link.degraded() {
        link.push(Span::styled("  (downtrained)", Style::new().fg(ORANGE)));
    }
    lines.push(kv("PCIe link", lw, link));
    if let Some(info) = &v.info {
        let mut arch = Vec::new();
        for g in &info.gts {
            let name = ip_name(g.ip.0);
            arch.push(format!(
                "{} {}{}.{:02}",
                if g.media { "media" } else { "graphics" },
                if name.is_empty() {
                    String::new()
                } else {
                    format!("{name} ")
                },
                g.ip.0,
                g.ip.1
            ));
        }
        if !arch.is_empty() {
            lines.push(kv("IP version", lw, t(arch.join("  ·  "))));
        }
        let mut topo = Vec::new();
        if let Some(c) = info.xe_cores {
            topo.push(format!("{c} Xe-cores"));
        }
        if let Some(e) = info.eus {
            topo.push(format!("{e} EUs"));
        }
        if let Some(b) = info.l3_banks {
            topo.push(format!("{b} L3 banks"));
        }
        if !topo.is_empty() {
            lines.push(kv("Topology", lw, t(topo.join("  ·  "))));
        }
        let engines: Vec<String> = info
            .engine_counts()
            .iter()
            .map(|(n, c)| format!("{} ×{c}", engine_label(n)))
            .collect();
        if !engines.is_empty() {
            lines.push(kv("Engines", lw, t(engines.join("  "))));
        }
        let mut fw = Vec::new();
        if let Some(g) = &info.guc {
            fw.push(format!("GuC {g}"));
        }
        if let Some(h) = &info.huc {
            fw.push(format!("HuC {h}"));
        }
        if !fw.is_empty() {
            lines.push(kv("Firmware", lw, t(fw.join("  ·  "))));
        }
        if let Some(b) = info.va_bits {
            lines.push(kv("GPU VA", lw, t(format!("{b}-bit"))));
        }
    }
    if let Some(hw) = &v.hw {
        if let Some((_, total, vis)) = hw.vram() {
            lines.push(kv(
                "VRAM",
                lw,
                vec![
                    Span::styled(fmt_bytes(total as f64), bold(CYAN)),
                    Span::styled(format!("  CPU-visible {}", fmt_bytes(vis as f64)), muted()),
                ],
            ));
        }
        for g in &hw.gts {
            lines.push(kv(
                &format!("{} clock", g.kind.label()),
                lw,
                t(format!(
                    "{}–{} MHz  (efficient {} MHz)",
                    g.rpn_mhz.unwrap_or(0),
                    g.rp0_mhz.unwrap_or(0),
                    g.rpe_mhz.unwrap_or(0)
                )),
            ));
        }
        for r in hw
            .rails
            .iter()
            .filter(|r| r.cap_w.is_some() || r.crit_w.is_some())
        {
            let mut s = Vec::new();
            if let Some(c) = r.cap_w {
                s.push(format!("cap {c:.0} W"));
            }
            if let Some(c) = r.crit_w {
                s.push(format!("crit {c:.0} W"));
            }
            lines.push(kv(
                &format!("{} power", pretty_sensor(&r.label)),
                lw,
                t(s.join("  ·  ")),
            ));
        }
    }
    lines.push(Line::raw(""));
    lines.push(kv(
        "Session",
        lw,
        vec![
            Span::styled(fmt_duration(app.started.elapsed().as_secs()), text()),
            Span::styled(
                format!("   peak {:.0} W · {:.0}°C", v.peak_power, v.peak_temp),
                muted(),
            ),
        ],
    ));
    if let Some(up) = crate::util::system_uptime() {
        lines.push(kv("System up", lw, t(fmt_duration(up))));
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(panel("Hardware"))
            .wrap(Wrap { trim: false }),
        l,
    );

    // Data sources.
    let ok = |b: bool| {
        if b {
            Span::styled("● ", bold(GREEN))
        } else {
            Span::styled("○ ", bold(RED))
        }
    };
    let mut s: Vec<Line> = Vec::new();
    let hw = v.hw.as_ref();
    let n_sensors = hw
        .map(|h| h.temps.len() + h.fans.len() + h.rails.len())
        .unwrap_or(0);
    s.push(Line::from(vec![
        ok(d.hwmon.is_some()),
        Span::styled("hwmon ", bold(CREAM)),
        Span::styled(format!("{n_sensors} sensors"), muted()),
    ]));
    let hwmon_name = d
        .hwmon
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| format!("  {}/{}", d.card, n.to_string_lossy()))
        .unwrap_or("  not found".into());
    s.push(Line::from(Span::styled(hwmon_name, dim())));
    s.push(Line::from(vec![
        ok(!d.gts.is_empty()),
        Span::styled("GT sysfs ", bold(CREAM)),
        Span::styled(
            format!("{} GTs · freq, C6 residency, throttle", d.gts.len()),
            muted(),
        ),
    ]));
    let has_q = hw.is_some_and(|h| !h.mem_regions.is_empty());
    s.push(Line::from(vec![
        ok(has_q),
        Span::styled("xe query ioctl ", bold(CREAM)),
        Span::styled("VRAM, topology, firmware", muted()),
    ]));
    let root = crate::util::is_root();
    s.push(Line::from(vec![
        ok(true),
        Span::styled("DRM fdinfo ", bold(CREAM)),
        Span::styled(
            format!(
                "{} clients in {} processes",
                v.procs.clients,
                v.procs.procs.len()
            ),
            muted(),
        ),
    ]));
    s.push(Line::from(Span::styled(
        if root {
            "  root: all users' clients are visible"
        } else {
            "  own processes only; run as root for all users"
        },
        dim(),
    )));
    s.push(Line::raw(""));
    s.push(Line::from(Span::styled("Not used", bold(MUTED))));
    let paranoid =
        crate::util::read_str("/proc/sys/kernel/perf_event_paranoid").unwrap_or("?".into());
    s.push(Line::from(vec![
        Span::styled("○ ", dim()),
        Span::styled("xe PMU ", text()),
        Span::styled(
            format!("holds forcewake while open (paranoid={paranoid})"),
            dim(),
        ),
    ]));
    s.push(Line::from(vec![
        Span::styled("○ ", dim()),
        Span::styled("PMT telemetry ", text()),
        Span::styled("root-only, GUID-specific layout", dim()),
    ]));
    s.push(Line::raw(""));
    s.push(Line::from(Span::styled("Notes", bold(MUTED))));
    for note in [
        "GPU% is the busiest engine class summed over visible DRM clients.",
        "Awake = time out of C6; monitoring itself briefly wakes the GT.",
        "Requested clock is read only on the Sensors tab (it takes forcewake).",
    ] {
        s.push(Line::from(Span::styled(format!("· {note}"), dim())));
    }
    f.render_widget(
        Paragraph::new(s)
            .block(panel("Telemetry sources"))
            .wrap(Wrap { trim: false }),
        r,
    );
}

// ───────────────────────────── overlays ─────────────────────────────

fn draw_help(f: &mut Frame) {
    let area = centered(f.area(), 64, 24);
    f.render_widget(Clear, area);
    let keys: &[(&str, &str)] = &[
        ("↑ ↓  PgUp PgDn  Home End", "move selection"),
        ("tab  shift-tab  1 2 3", "switch view"),
        ("s  >  F6  /  <", "next / previous sort column"),
        ("r  I", "reverse sort order"),
        ("g  m  c", "sort by GPU% / VRAM / CPU%"),
        ("/  F3", "filter processes"),
        ("esc", "clear filter / close"),
        ("k  F9", "send SIGTERM to selected"),
        ("K", "send SIGKILL to selected"),
        ("p  space", "pause / resume"),
        ("+  -", "refresh interval"),
        ("d  [  ]", "next / previous GPU"),
        ("?  h  F1", "this help"),
        ("q  F10  ctrl-c", "quit"),
    ];
    let mut lines = vec![Line::raw("")];
    for (k, d) in keys {
        lines.push(Line::from(vec![
            Span::styled(format!("  {}", pad(k, 24)), bold(PINK)),
            Span::styled(format!("  {d}"), text()),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  Reads hwmon, GT sysfs, DRM fdinfo and the xe query ioctl",
        dim(),
    )));
    lines.push(Line::from(Span::styled(
        "  directly — no xpu-smi, Level Zero or root required.",
        dim(),
    )));
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER_HI))
        .title(Line::from(vec![
            Span::raw(" "),
            pill("Help", PURPLE),
            Span::raw(" "),
        ]))
        .title_bottom(right_title(vec![Span::styled("any key to close", dim())]));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_confirm(f: &mut Frame, pid: u32, name: &str, signal: i32) {
    let area = centered(f.area(), 52, 7);
    f.render_widget(Clear, area);
    let sig = if signal == libc::SIGKILL {
        "SIGKILL"
    } else {
        "SIGTERM"
    };
    let lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled("  Send ", text()),
            Span::styled(sig, bold(RED)),
            Span::styled(" to ", text()),
            Span::styled(pid.to_string(), bold(CREAM)),
            Span::styled(format!(" ({name})?"), text()),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            pill("y", RED),
            Span::styled(" confirm   ", muted()),
            pill("n", theme::SURFACE),
            Span::styled(" cancel", muted()),
        ]),
    ];
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(RED))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled("Kill process", bold(RED)),
            Span::raw(" "),
        ]));
    f.render_widget(Paragraph::new(lines).block(block), area);
}
