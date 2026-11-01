// SPDX-License-Identifier: GPL-2.0-only
use std::fs;
use std::path::Path;

pub fn read_str(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

pub fn read_u64(p: impl AsRef<Path>) -> Option<u64> {
    read_str(p)?.parse().ok()
}

pub fn read_i64(p: impl AsRef<Path>) -> Option<i64> {
    read_str(p)?.parse().ok()
}

/// Human-readable binary size from a byte count.
pub fn fmt_bytes(bytes: f64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut v = bytes.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 || v >= 100.0 {
        format!("{v:.0}{}", UNITS[i])
    } else if v >= 10.0 {
        format!("{v:.1}{}", UNITS[i])
    } else {
        format!("{v:.2}{}", UNITS[i])
    }
}

pub fn fmt_kib(kib: u64) -> String {
    fmt_bytes(kib as f64 * 1024.0)
}

pub fn fmt_energy(joules: f64) -> String {
    if joules >= 3.6e6 {
        format!("{:.2} kWh", joules / 3.6e6)
    } else if joules >= 3600.0 {
        format!("{:.1} Wh", joules / 3600.0)
    } else {
        format!("{joules:.0} J")
    }
}

pub fn fmt_duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        format!("{d}d {h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// Local wall-clock time as HH:MM:SS.
pub fn clock() -> String {
    // SAFETY: time/localtime_r are thread-safe with a caller-owned tm.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

pub fn system_uptime() -> Option<u64> {
    let s = read_str("/proc/uptime")?;
    s.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .map(|v| v as u64)
}

pub fn kernel_release() -> Option<String> {
    read_str("/proc/sys/kernel/osrelease")
}

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(fmt_bytes(0.0), "0B");
        assert_eq!(fmt_bytes(1536.0), "1.50K");
        assert_eq!(fmt_bytes(34242297856.0), "31.9G");
        assert_eq!(fmt_bytes(200.0 * 1024.0 * 1024.0), "200M");
        assert_eq!(fmt_kib(22371816), "21.3G");
    }

    #[test]
    fn formats_durations_and_energy() {
        assert_eq!(fmt_duration(59), "00:00:59");
        assert_eq!(fmt_duration(2 * 86400 + 3661), "2d 01:01:01");
        assert_eq!(fmt_energy(500.0), "500 J");
        assert_eq!(fmt_energy(7200.0), "2.0 Wh");
        assert_eq!(fmt_energy(7.2e6), "2.00 kWh");
    }
}
