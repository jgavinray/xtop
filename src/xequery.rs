// SPDX-License-Identifier: GPL-2.0-only
//! `DRM_IOCTL_XE_DEVICE_QUERY` on the render node: device-wide VRAM total/used,
//! topology and firmware versions without root (only `render` group access).
//! Struct layouts follow include/uapi/drm/xe_drm.h.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;

#[repr(C)]
#[derive(Default)]
struct DeviceQuery {
    extensions: u64,
    query: u32,
    size: u32,
    data: u64,
    reserved: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawMemRegion {
    mem_class: u16,
    instance: u16,
    min_page_size: u32,
    total_size: u64,
    used: u64,
    cpu_visible_size: u64,
    cpu_visible_used: u64,
    reserved: [u64; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawGt {
    gt_type: u16,
    tile_id: u16,
    gt_id: u16,
    pad: [u16; 3],
    reference_clock: u32,
    near_mem_regions: u64,
    far_mem_regions: u64,
    ip_ver_major: u16,
    ip_ver_minor: u16,
    ip_ver_rev: u16,
    pad2: u16,
    reserved: [u64; 7],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawEngine {
    engine_class: u16,
    engine_instance: u16,
    gt_id: u16,
    pad: u16,
    reserved: [u64; 3],
}

#[repr(C)]
#[derive(Default)]
struct RawUcFwVersion {
    uc_type: u16,
    pad: u16,
    branch_ver: u32,
    major_ver: u32,
    minor_ver: u32,
    patch_ver: u32,
    pad2: u32,
    reserved: u64,
}

const DRM_COMMAND_BASE: u64 = 0x40;
const DRM_XE_DEVICE_QUERY: u64 = 0x00;
const QUERY_ENGINES: u32 = 0;
const QUERY_MEM_REGIONS: u32 = 1;
const QUERY_CONFIG: u32 = 2;
const QUERY_GT_LIST: u32 = 3;
const QUERY_GT_TOPOLOGY: u32 = 5;
const QUERY_UC_FW_VERSION: u32 = 7;
const MEM_CLASS_VRAM: u16 = 1;
const TOPO_DSS_GEOMETRY: u16 = 1;
const TOPO_DSS_COMPUTE: u16 = 2;
const TOPO_L3_BANK: u16 = 3;
const TOPO_EU_PER_DSS: u16 = 4;
const TOPO_SIMD16_EU_PER_DSS: u16 = 5;

/// _IOWR('d', DRM_COMMAND_BASE + DRM_XE_DEVICE_QUERY, struct drm_xe_device_query)
const DRM_IOCTL_XE_DEVICE_QUERY: u64 = (3 << 30)
    | ((std::mem::size_of::<DeviceQuery>() as u64) << 16)
    | ((b'd' as u64) << 8)
    | (DRM_COMMAND_BASE + DRM_XE_DEVICE_QUERY);

#[derive(Clone, Debug)]
pub struct MemRegion {
    pub vram: bool,
    pub total: u64,
    pub used: u64,
    pub cpu_visible_size: u64,
}

#[derive(Clone, Debug)]
pub struct XeGt {
    pub media: bool,
    pub ip: (u16, u16, u16),
}

#[derive(Clone, Debug, Default)]
pub struct XeInfo {
    pub gts: Vec<XeGt>,
    /// (engine class, instance, gt) as reported by the driver.
    pub engines: Vec<(u16, u16, u16)>,
    pub va_bits: Option<u64>,
    pub xe_cores: Option<u32>,
    pub eus: Option<u32>,
    pub l3_banks: Option<u32>,
    pub guc: Option<String>,
    pub huc: Option<String>,
}

impl XeInfo {
    /// Engine instance counts per class, e.g. [("ccs", 4), ("vcs", 2)].
    pub fn engine_counts(&self) -> Vec<(&'static str, usize)> {
        let names = ["rcs", "bcs", "vcs", "vecs", "ccs"];
        let mut out: Vec<(&'static str, usize)> = names
            .iter()
            .enumerate()
            .map(|(class, n)| {
                (
                    *n,
                    self.engines
                        .iter()
                        .filter(|e| e.0 as usize == class)
                        .count(),
                )
            })
            .filter(|(_, c)| *c > 0)
            .collect();
        out.sort_by_key(|(n, _)| crate::fdinfo::engine_rank(n).0);
        out
    }
}

pub struct XeQuery {
    file: File,
}

impl XeQuery {
    pub fn open(render_node: &str) -> Option<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/dev/dri/{render_node}"))
            .ok()?;
        let q = Self { file };
        q.mem_regions().map(|_| q)
    }

    fn ioctl(&self, q: &mut DeviceQuery) -> bool {
        // SAFETY: q points to a correctly laid out drm_xe_device_query and, when
        // q.data is non-null, to a buffer of at least q.size bytes.
        unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                DRM_IOCTL_XE_DEVICE_QUERY as _,
                q as *mut DeviceQuery,
            ) == 0
        }
    }

    /// Two-step query: ask for the size, then fetch into an 8-byte aligned buffer.
    fn query(&self, kind: u32) -> Option<(Vec<u64>, usize)> {
        let mut q = DeviceQuery {
            query: kind,
            ..Default::default()
        };
        if !self.ioctl(&mut q) || q.size < 8 {
            return None;
        }
        let mut buf = vec![0u64; (q.size as usize).div_ceil(8)];
        q.data = buf.as_mut_ptr() as u64;
        self.ioctl(&mut q).then_some((buf, q.size as usize))
    }

    /// Items of type T following a `{ u32 count; u32 pad; }` header.
    fn query_array<T: Copy>(&self, kind: u32) -> Option<Vec<T>> {
        let (buf, size) = self.query(kind)?;
        let count = (buf[0] & 0xFFFF_FFFF) as usize;
        let avail = (size - 8) / std::mem::size_of::<T>();
        // SAFETY: the kernel wrote `count` elements directly after the 8-byte
        // header; the buffer is u64-aligned, which satisfies every T used here.
        let items = unsafe {
            std::slice::from_raw_parts(buf.as_ptr().add(1) as *const T, count.min(avail))
        };
        Some(items.to_vec())
    }

    fn uc_version(&self, uc_type: u16) -> Option<String> {
        let mut v = RawUcFwVersion {
            uc_type,
            ..Default::default()
        };
        let mut q = DeviceQuery {
            query: QUERY_UC_FW_VERSION,
            size: std::mem::size_of::<RawUcFwVersion>() as u32,
            data: &mut v as *mut RawUcFwVersion as u64,
            ..Default::default()
        };
        if !self.ioctl(&mut q) || (v.major_ver == 0 && v.minor_ver == 0) {
            return None;
        }
        Some(format!("{}.{}.{}", v.major_ver, v.minor_ver, v.patch_ver))
    }

    /// Static device description: GTs, engines, topology and firmware.
    pub fn info(&self) -> XeInfo {
        let gts = self
            .query_array::<RawGt>(QUERY_GT_LIST)
            .unwrap_or_default()
            .iter()
            .map(|g| XeGt {
                media: g.gt_type == 1,
                ip: (g.ip_ver_major, g.ip_ver_minor, g.ip_ver_rev),
            })
            .collect();
        let engines = self
            .query_array::<RawEngine>(QUERY_ENGINES)
            .unwrap_or_default()
            .iter()
            .map(|e| (e.engine_class, e.engine_instance, e.gt_id))
            .collect();
        let config = self.query_array::<u64>(QUERY_CONFIG).unwrap_or_default();

        // Topology is a packed list of { u16 gt; u16 type; u32 len; u8 mask[len] }.
        let mut xe_cores = 0u32;
        let mut eu_per_core = 0u32;
        let mut l3_banks = 0u32;
        if let Some((buf, size)) = self.query(QUERY_GT_TOPOLOGY) {
            // SAFETY: reinterpreting our own u64 buffer as bytes, bounded by `size`.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, size) };
            let mut off = 0;
            let mut dss: Vec<u8> = Vec::new();
            while off + 8 <= size {
                let ty = u16::from_ne_bytes([bytes[off + 2], bytes[off + 3]]);
                let n = u32::from_ne_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
                let Some(mask) = bytes.get(off + 8..off + 8 + n) else {
                    break;
                };
                let pop = mask.iter().map(|b| b.count_ones()).sum::<u32>();
                match ty {
                    TOPO_DSS_GEOMETRY | TOPO_DSS_COMPUTE => {
                        dss.resize(dss.len().max(n), 0);
                        for (d, m) in dss.iter_mut().zip(mask) {
                            *d |= m;
                        }
                    }
                    TOPO_EU_PER_DSS | TOPO_SIMD16_EU_PER_DSS => eu_per_core = eu_per_core.max(pop),
                    TOPO_L3_BANK => l3_banks = l3_banks.max(pop),
                    _ => {}
                }
                off += 8 + n;
            }
            xe_cores = dss.iter().map(|b| b.count_ones()).sum();
        }

        XeInfo {
            gts,
            engines,
            va_bits: config.get(3).copied(),
            xe_cores: (xe_cores > 0).then_some(xe_cores),
            eus: (xe_cores > 0 && eu_per_core > 0).then_some(xe_cores * eu_per_core),
            l3_banks: (l3_banks > 0).then_some(l3_banks),
            guc: self.uc_version(0),
            huc: self.uc_version(1),
        }
    }

    pub fn mem_regions(&self) -> Option<Vec<MemRegion>> {
        Some(
            self.query_array::<RawMemRegion>(QUERY_MEM_REGIONS)?
                .iter()
                .map(|r| MemRegion {
                    vram: r.mem_class == MEM_CLASS_VRAM,
                    total: r.total_size,
                    used: r.used,
                    cpu_visible_size: r.cpu_visible_size,
                })
                .collect(),
        )
    }
}
