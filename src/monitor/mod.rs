//! Resource monitoring (CPU, RAM, disks, NVIDIA GPUs) and the load gate that
//! delays new jobs under high system load.
//!
//! GPU utilisation can only be *observed* (nvidia-smi); Framewise does not
//! offer a GPU usage limit because there is no reliable way to enforce one.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sysinfo::{Disks, System};

use crate::core::config::ResourceSettings;
use crate::core::tools::run_capture;
use crate::trf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskInfo {
    pub mount_point: PathBuf,
    pub total: u64,
    pub available: u64,
    pub read_bps: u64,
    pub write_bps: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuSample {
    pub name: String,
    pub utilization: Option<f32>,
    pub encoder_utilization: Option<f32>,
    pub decoder_utilization: Option<f32>,
    pub memory_used_mb: Option<u64>,
    pub memory_total_mb: Option<u64>,
    pub temperature_c: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResourceSnapshot {
    pub cpu_brand: String,
    pub cpu_percent: f32,
    pub per_core: Vec<f32>,
    pub load_avg: Option<[f64; 3]>,
    pub mem_total: u64,
    pub mem_available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub disks: Vec<DiskInfo>,
    pub gpus: Vec<GpuSample>,
}

impl ResourceSnapshot {
    /// Disk containing `path` (longest mount point prefix).
    pub fn disk_for(&self, path: &Path) -> Option<&DiskInfo> {
        self.disks
            .iter()
            .filter(|d| path.starts_with(&d.mount_point))
            .max_by_key(|d| d.mount_point.as_os_str().len())
    }
}

/// Why new jobs are currently held back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GateBlock {
    CpuHigh {
        percent: f32,
        limit: f32,
    },
    RamLow {
        available_mb: u64,
        min_mb: u64,
    },
    DiskLow {
        path: PathBuf,
        available_gb: u64,
        min_gb: u64,
    },
}

impl GateBlock {
    pub fn describe(&self) -> String {
        match self {
            GateBlock::CpuHigh { percent, limit } => trf!(
                "CPU load {} % above target {} %",
                format!("{percent:.0}"),
                format!("{limit:.0}")
            ),
            GateBlock::RamLow {
                available_mb,
                min_mb,
            } => trf!("Only {} MB RAM free (minimum {} MB)", available_mb, min_mb),
            GateBlock::DiskLow {
                path,
                available_gb,
                min_gb,
            } => trf!(
                "Only {} GB free at {} (minimum {} GB)",
                available_gb,
                path.display(),
                min_gb
            ),
        }
    }
}

/// Decide whether another job may start. `own_jobs` running jobs are expected
/// to cause load themselves, so the CPU check only applies when at least one
/// job is already running (otherwise the queue could never start on a busy box
/// it is meant to share).
pub fn check_gate(
    snapshot: &ResourceSnapshot,
    settings: &ResourceSettings,
    paths: &[PathBuf],
    running_jobs: usize,
) -> Option<GateBlock> {
    if running_jobs > 0 && snapshot.cpu_percent > settings.cpu_target_percent {
        return Some(GateBlock::CpuHigh {
            percent: snapshot.cpu_percent,
            limit: settings.cpu_target_percent,
        });
    }
    let available_mb = snapshot.mem_available / 1024 / 1024;
    if snapshot.mem_total > 0 && available_mb < settings.min_free_ram_mb {
        return Some(GateBlock::RamLow {
            available_mb,
            min_mb: settings.min_free_ram_mb,
        });
    }
    for path in paths {
        if let Some(disk) = snapshot.disk_for(path) {
            let available_gb = disk.available / 1_000_000_000;
            if available_gb < settings.min_free_disk_gb {
                return Some(GateBlock::DiskLow {
                    path: path.clone(),
                    available_gb,
                    min_gb: settings.min_free_disk_gb,
                });
            }
        }
    }
    None
}

pub struct Monitor {
    system: Option<System>,
    disks: Option<Disks>,
    last_disk_refresh: Instant,
    nvidia_smi: bool,
}

impl Monitor {
    pub fn new() -> Self {
        let mut system = System::new().ok();
        if let Some(s) = system.as_mut() {
            s.refresh_cpu_all();
            s.refresh_memory();
        }
        Self {
            system,
            disks: Disks::new_with_refreshed_list().ok(),
            last_disk_refresh: Instant::now(),
            nvidia_smi: true,
        }
    }

    pub async fn sample(&mut self) -> ResourceSnapshot {
        let mut snap = ResourceSnapshot::default();
        if let Some(sys) = self.system.as_mut() {
            sys.refresh_cpu_usage();
            sys.refresh_memory();
            snap.cpu_percent = sys.global_cpu_usage();
            snap.per_core = sys.cpus().iter().map(|c| c.usage()).collect();
            snap.cpu_brand = sys
                .cpus()
                .first()
                .map(|c| c.brand().trim().to_string())
                .unwrap_or_default();
            snap.mem_total = sys.total_memory();
            snap.mem_available = sys.available_memory();
            snap.swap_total = sys.total_swap();
            snap.swap_used = sys.used_swap();
        }
        snap.load_avg = System::load_average()
            .ok()
            .map(|l| [l.one, l.five, l.fifteen])
            .filter(|l| l.iter().any(|v| *v > 0.0));

        let elapsed = self.last_disk_refresh.elapsed().as_secs_f64().max(0.001);
        self.last_disk_refresh = Instant::now();
        if let Some(disks) = self.disks.as_mut() {
            disks.refresh(true);
            snap.disks = disks
                .iter()
                .map(|d| {
                    let usage = d.usage();
                    DiskInfo {
                        mount_point: d.mount_point().to_path_buf(),
                        total: d.total_space(),
                        available: d.available_space(),
                        read_bps: (usage.read_bytes as f64 / elapsed) as u64,
                        write_bps: (usage.written_bytes as f64 / elapsed) as u64,
                    }
                })
                .collect();
        }

        if self.nvidia_smi {
            match sample_nvidia().await {
                Some(gpus) => snap.gpus = gpus,
                None => self.nvidia_smi = false,
            }
        }
        snap
    }
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_opt_f32(v: &str) -> Option<f32> {
    v.trim().parse().ok()
}

/// Parse `nvidia-smi --query-gpu=... --format=csv,noheader,nounits`.
pub fn parse_nvidia_csv(text: &str) -> Vec<GpuSample> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let cols: Vec<&str> = line.split(',').map(str::trim).collect();
            if cols.len() < 7 {
                return None;
            }
            Some(GpuSample {
                name: cols[0].to_string(),
                utilization: parse_opt_f32(cols[1]),
                encoder_utilization: parse_opt_f32(cols[2]),
                decoder_utilization: parse_opt_f32(cols[3]),
                memory_used_mb: cols[4].parse().ok(),
                memory_total_mb: cols[5].parse().ok(),
                temperature_c: parse_opt_f32(cols[6]),
            })
        })
        .collect()
}

async fn sample_nvidia() -> Option<Vec<GpuSample>> {
    let out = run_capture(
        "nvidia-smi",
        Path::new("nvidia-smi"),
        [
            "--query-gpu=name,utilization.gpu,utilization.encoder,utilization.decoder,memory.used,memory.total,temperature.gpu",
            "--format=csv,noheader,nounits",
        ],
        Duration::from_secs(5),
        "GPU sample",
    )
    .await
    .ok()?;
    Some(parse_nvidia_csv(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> ResourceSnapshot {
        ResourceSnapshot {
            cpu_percent: 50.0,
            mem_total: 16 << 30,
            mem_available: 8 << 30,
            disks: vec![
                DiskInfo {
                    mount_point: "/".into(),
                    total: 500_000_000_000,
                    available: 100_000_000_000,
                    read_bps: 0,
                    write_bps: 0,
                },
                DiskInfo {
                    mount_point: "/media".into(),
                    total: 4_000_000_000_000,
                    available: 2_000_000_000,
                    read_bps: 0,
                    write_bps: 0,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn gate() {
        let s = ResourceSettings::default();
        assert_eq!(check_gate(&snap(), &s, &["/home/x".into()], 0), None);
        let mut busy = snap();
        busy.cpu_percent = 99.0;
        assert_eq!(
            check_gate(&busy, &s, &[], 0),
            None,
            "first job always allowed"
        );
        assert!(matches!(
            check_gate(&busy, &s, &[], 1),
            Some(GateBlock::CpuHigh { .. })
        ));
        assert!(matches!(
            check_gate(&snap(), &s, &["/media/optimized".into()], 0),
            Some(GateBlock::DiskLow {
                available_gb: 2,
                ..
            })
        ));
        let mut low = snap();
        low.mem_available = 100 << 20;
        assert!(matches!(
            check_gate(&low, &s, &[], 0),
            Some(GateBlock::RamLow { .. })
        ));
    }

    #[test]
    fn nvidia_csv() {
        let g = parse_nvidia_csv("NVIDIA GeForce RTX 3060, 12, 35, 0, 1024, 12288, 55\n");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].encoder_utilization, Some(35.0));
        assert_eq!(g[0].memory_total_mb, Some(12288));
        let g = parse_nvidia_csv("Tesla, [N/A], [N/A], [N/A], 1, 2, 30\n");
        assert_eq!(g[0].utilization, None);
    }
}
