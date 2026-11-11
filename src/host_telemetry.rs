//! Host telemetry for the machine view, read in-process.
//!
//! The load balancer samples the box it runs on: CPU, memory, pressure,
//! block-device and network rates, filesystem usage, RAPL package power, and
//! NVIDIA GPUs. Every source is independent and best-effort. A source this
//! process cannot read is reported as absent, never as zero, so a container
//! granted less access simply shows less.
//!
//! Inside a container, `/proc`'s CPU, memory, pressure, and block-device
//! counters are already host-wide. Three sources need more:
//!
//! * network rates are read from the host init process's namespace, so they
//!   need the host's procfs: the host root bind-mounted read-only at
//!   `RJ_MACHINEVIEW_HOST_ROOT`, or a host PID namespace;
//! * RAPL energy counters are root-only and Docker masks them under `/sys`, so
//!   they also come through the host root;
//! * GPUs come from `nvidia-smi`, which the NVIDIA runtime injects when the
//!   container is granted the `utility` driver capability. A query creates no
//!   CUDA context and reserves no device memory.
//!
//! Rates are deltas between consecutive samples, so the first sample after
//! start reports them as absent. Collection is observation-only: it never
//! influences routing, upstream health, or request handling.

#![allow(clippy::cast_precision_loss)]

use std::{
    collections::HashMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

use crate::machineview::{DiskSample, GpuSample, HostSample};

const NVIDIA_SMI_FIELDS: &str = "index,name,utilization.gpu,memory.used,memory.total,\
power.draw,temperature.gpu,clocks.sm";
/// Adds memory-controller activity, clocks, power limit, fan, P-state, memory
/// temperature, and the four throttle reasons the dashboard plots. Older
/// drivers reject some of these names; collection then falls back to
/// [`NVIDIA_SMI_FIELDS`] for the life of the process.
const NVIDIA_SMI_EXTENDED_FIELDS: &str = "index,name,utilization.gpu,memory.used,memory.total,\
power.draw,temperature.gpu,clocks.sm,utilization.memory,clocks.mem,power.limit,fan.speed,\
pstate,temperature.memory,clocks_throttle_reasons.sw_power_cap,\
clocks_throttle_reasons.sw_thermal_slowdown,clocks_throttle_reasons.hw_thermal_slowdown,\
clocks_throttle_reasons.hw_slowdown";
/// node06 (8 busy GPUs) measured 1.7-3.0s per query, so a 3s bound dropped
/// about one sample in six. A slow query only stretches that sampler tick.
const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RAPL_DOMAINS: usize = 8;
/// `/proc/self/ns/pid` of a process in the initial PID namespace. The kernel
/// fixes this inode (`PROC_PID_INIT_INO`).
const INITIAL_PID_NAMESPACE: &str = "pid:[4026531836]";
const MIB: f64 = 1024.0 * 1024.0;

/// Stateful host sampler. Holds previous counter values to derive rates.
pub struct HostTelemetry {
    sources: Arc<Sources>,
    counters: Arc<Mutex<Counters>>,
    nvidia_smi: Option<PathBuf>,
    extended_gpu_fields: bool,
}

/// Where each source is read from; fixed at startup.
struct Sources {
    proc_dir: PathBuf,
    /// `None` when the visible procfs is a container's own, whose network
    /// counters would describe the container rather than the host.
    net_dev: Option<PathBuf>,
    powercap_dir: PathBuf,
    /// `(host mount point, path to stat through the host root)`.
    disks: Vec<(String, PathBuf)>,
}

#[derive(Default)]
struct Counters {
    rates: HashMap<String, (Instant, f64)>,
    cpu: Option<CpuTimes>,
    io_ticks: Option<(Instant, HashMap<String, u64>)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CpuTimes {
    busy: u64,
    total: u64,
    iowait: u64,
}

enum GpuQueryError {
    /// `nvidia-smi` ran and exited non-zero: a field name the driver rejects.
    Rejected,
    /// Spawn failure or timeout; says nothing about the field list.
    Unavailable,
}

impl HostTelemetry {
    /// Resolves every source under `host_root` and logs which are readable.
    #[must_use]
    pub fn new(host_root: &Path, disk_mounts: &[String]) -> Self {
        let proc_dir = host_root.join("proc");
        let sources = Sources {
            net_dev: host_net_dev(&proc_dir),
            powercap_dir: host_root.join("sys/class/powercap"),
            disks: disk_mounts
                .iter()
                .map(|mount| (mount.clone(), host_root.join(mount.trim_start_matches('/'))))
                .collect(),
            proc_dir,
        };
        let nvidia_smi = find_on_path("nvidia-smi");
        tracing::info!(
            host_root = %host_root.display(),
            network = sources.net_dev.as_ref().is_some_and(|path| path.is_file()),
            rapl = rapl_readable(&sources.powercap_dir),
            disks = sources.disks.iter().filter(|(_, path)| rustix::fs::statvfs(path).is_ok()).count(),
            gpus = nvidia_smi.is_some(),
            "machineview host telemetry sources"
        );
        Self {
            sources: Arc::new(sources),
            counters: Arc::new(Mutex::new(Counters::default())),
            nvidia_smi,
            extended_gpu_fields: true,
        }
    }

    /// Samples the host and GPUs concurrently. Filesystem reads run on the
    /// blocking pool because `statvfs` on a stalled mount can block.
    pub async fn sample(&mut self) -> (Option<HostSample>, Vec<GpuSample>) {
        let sources = self.sources.clone();
        let counters = self.counters.clone();
        let host = tokio::task::spawn_blocking(move || {
            sample_host(&sources, &mut counters.lock(), Instant::now())
        });
        let (host, gpus) = tokio::join!(host, self.sample_gpus());
        (host.ok().flatten(), gpus)
    }

    async fn sample_gpus(&mut self) -> Vec<GpuSample> {
        let Some(binary) = &self.nvidia_smi else {
            return Vec::new();
        };
        if self.extended_gpu_fields {
            match query_gpus(binary, NVIDIA_SMI_EXTENDED_FIELDS).await {
                Ok(gpus) => return gpus,
                Err(GpuQueryError::Unavailable) => return Vec::new(),
                Err(GpuQueryError::Rejected) => {
                    tracing::info!(
                        "nvidia-smi rejected the extended field list; using the base list"
                    );
                    self.extended_gpu_fields = false;
                }
            }
        }
        query_gpus(binary, NVIDIA_SMI_FIELDS)
            .await
            .unwrap_or_default()
    }
}

async fn query_gpus(binary: &Path, fields: &str) -> Result<Vec<GpuSample>, GpuQueryError> {
    let output = tokio::process::Command::new(binary)
        .arg(format!("--query-gpu={fields}"))
        .arg("--format=csv,noheader,nounits")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(NVIDIA_SMI_TIMEOUT, output).await {
        Ok(Ok(output)) if output.status.success() => Ok(parse_nvidia_smi_csv(
            &String::from_utf8_lossy(&output.stdout),
        )),
        Ok(Ok(_)) => Err(GpuQueryError::Rejected),
        Ok(Err(_)) | Err(_) => Err(GpuQueryError::Unavailable),
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// Picks the network counters of the host init process, or `None` when the
/// visible procfs belongs to a container's private PID namespace.
///
/// The procfs numbers this process as it sees it. If that matches our own PID,
/// the procfs is our namespace's, which is the host's only when our namespace
/// is the initial one (a native process, or a container sharing the host PID
/// namespace). A different number means a bind-mounted procfs from an
/// enclosing namespace: the host's.
fn host_net_dev(proc_dir: &Path) -> Option<PathBuf> {
    let seen = fs::read_link(proc_dir.join("self")).ok()?;
    let seen_is_own =
        seen.to_str().and_then(|pid| pid.parse::<u32>().ok()) == Some(std::process::id());
    let own_namespace_is_initial = fs::read_link("/proc/self/ns/pid")
        .is_ok_and(|link| link.as_os_str() == INITIAL_PID_NAMESPACE);
    (!seen_is_own || own_namespace_is_initial).then(|| proc_dir.join("1/net/dev"))
}

fn rapl_readable(powercap_dir: &Path) -> bool {
    rapl_package_domains(powercap_dir)
        .first()
        .is_some_and(|domain| fs::read_to_string(domain.join("energy_uj")).is_ok())
}

/// Package-level RAPL domains only; sub-domains (`intel-rapl:N:M`) would
/// double-count their package.
fn rapl_package_domains(powercap_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(powercap_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("intel-rapl:") && name.matches(':').count() == 1)
        .collect();
    names.sort();
    names.truncate(MAX_RAPL_DOMAINS);
    names
        .into_iter()
        .map(|name| powercap_dir.join(name))
        .collect()
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn sample_host(sources: &Sources, counters: &mut Counters, now: Instant) -> Option<HostSample> {
    let proc = &sources.proc_dir;
    let stat = read(&proc.join("stat")).and_then(|text| parse_proc_stat_cpu(&text));
    let memory = read(&proc.join("meminfo")).and_then(|text| parse_meminfo(&text));
    let load1 = read(&proc.join("loadavg")).and_then(|text| parse_loadavg(&text));
    if stat.is_none() && memory.is_none() && load1.is_none() {
        return None;
    }
    let (cpu_pct, iowait_pct) = counters.cpu_shares(stat);
    let net = sources
        .net_dev
        .as_ref()
        .and_then(|path| read(path))
        .and_then(|text| parse_net_dev(&text));
    let disk = read(&proc.join("diskstats")).and_then(|text| parse_diskstats(&text));
    let mut host = HostSample {
        cpu_pct,
        iowait_pct,
        load1,
        net_rx_bps: counters.rate("net.rx", net.map(|(rx, _)| rx as f64), now),
        net_tx_bps: counters.rate("net.tx", net.map(|(_, tx)| tx as f64), now),
        disk_read_bps: counters.rate("disk.read", disk.as_ref().map(|d| d.read_bytes as f64), now),
        disk_write_bps: counters.rate(
            "disk.write",
            disk.as_ref().map(|d| d.write_bytes as f64),
            now,
        ),
        disk_read_iops: counters.rate("disk.reads", disk.as_ref().map(|d| d.reads as f64), now),
        disk_write_iops: counters.rate("disk.writes", disk.as_ref().map(|d| d.writes as f64), now),
        disk_inflight: disk.as_ref().map(|d| d.inflight as f64),
        disk_util_pct: counters.disk_util_pct(disk.map(|d| d.io_ticks), now),
        io_pressure_pct: read(&proc.join("pressure/io")).and_then(|text| parse_pressure(&text)),
        mem_pressure_pct: read(&proc.join("pressure/memory"))
            .and_then(|text| parse_pressure(&text)),
        cpu_watts: counters.rapl_watts(&sources.powercap_dir, now),
        disks: sources
            .disks
            .iter()
            .filter_map(|(mount, path)| disk_usage(mount, path))
            .collect(),
        ..HostSample::default()
    };
    if let Some(memory) = memory {
        host.mem_total_bytes = Some(memory.total);
        host.mem_used_bytes = memory.used;
        host.mem_cached_bytes = memory.cached;
        host.swap_total_bytes = memory.swap_total;
        host.swap_used_bytes = memory.swap_used;
        host.dirty_bytes = memory.dirty;
        host.writeback_bytes = memory.writeback;
    }
    Some(host)
}

fn disk_usage(mount: &str, path: &Path) -> Option<DiskSample> {
    let stats = rustix::fs::statvfs(path).ok()?;
    let total = stats.f_frsize.saturating_mul(stats.f_blocks);
    if total == 0 {
        return None;
    }
    let free = stats.f_frsize.saturating_mul(stats.f_bavail);
    let (inodes_total, inodes_used) = if stats.f_files > 0 {
        (
            Some(stats.f_files as f64),
            Some(stats.f_files.saturating_sub(stats.f_favail) as f64),
        )
    } else {
        (None, None)
    };
    Some(DiskSample {
        mount: mount.to_owned(),
        total_bytes: total as f64,
        used_bytes: total.saturating_sub(free) as f64,
        inodes_total,
        inodes_used,
    })
}

impl Counters {
    /// Per-second delta against the previous observation of `key`. A missing
    /// value forgets the key; a counter that went backwards (reset) yields
    /// `None` rather than a negative rate.
    fn rate(&mut self, key: &str, value: Option<f64>, now: Instant) -> Option<f64> {
        let Some(value) = value else {
            self.rates.remove(key);
            return None;
        };
        let (prior_t, prior_value) = self.rates.insert(key.to_owned(), (now, value))?;
        let elapsed = now.checked_duration_since(prior_t)?.as_secs_f64();
        (elapsed > 0.0 && value >= prior_value).then(|| (value - prior_value) / elapsed)
    }

    /// `(cpu_pct, iowait_pct)` between this and the previous `/proc/stat`.
    fn cpu_shares(&mut self, current: Option<CpuTimes>) -> (Option<f64>, Option<f64>) {
        let prior = std::mem::replace(&mut self.cpu, current);
        let (Some(prior), Some(current)) = (prior, current) else {
            return (None, None);
        };
        let total = current
            .total
            .checked_sub(prior.total)
            .filter(|delta| *delta > 0);
        let busy = current.busy.checked_sub(prior.busy);
        let iowait = current.iowait.checked_sub(prior.iowait);
        let (Some(total), Some(busy), Some(iowait)) = (total, busy, iowait) else {
            return (None, None);
        };
        let share = |part: u64| (part as f64 * 100.0 / total as f64).clamp(0.0, 100.0);
        (Some(share(busy)), Some(share(iowait)))
    }

    /// Busiest whole disk: its `io_ticks` delta as a percent of wall time.
    fn disk_util_pct(
        &mut self,
        io_ticks: Option<HashMap<String, u64>>,
        now: Instant,
    ) -> Option<f64> {
        let prior = std::mem::replace(&mut self.io_ticks, io_ticks.map(|ticks| (now, ticks)));
        let (prior_t, prior_ticks) = prior?;
        let (_, ticks) = self.io_ticks.as_ref()?;
        let elapsed_ms = now.checked_duration_since(prior_t)?.as_secs_f64() * 1_000.0;
        if elapsed_ms <= 0.0 {
            return None;
        }
        ticks
            .iter()
            .filter_map(|(name, ticks)| {
                let delta = ticks.checked_sub(*prior_ticks.get(name)?)?;
                Some((delta as f64 * 100.0 / elapsed_ms).min(100.0))
            })
            .reduce(f64::max)
    }

    fn rapl_watts(&mut self, powercap_dir: &Path, now: Instant) -> Option<f64> {
        let mut total = None;
        for domain in rapl_package_domains(powercap_dir) {
            let microjoules =
                read(&domain.join("energy_uj")).and_then(|text| text.trim().parse::<u64>().ok());
            let key = format!("rapl.{}", domain.display());
            if let Some(watts) = self.rate(&key, microjoules.map(|uj| uj as f64 / 1_000_000.0), now)
            {
                *total.get_or_insert(0.0) += watts;
            }
        }
        total
    }
}

// --- Parsers ----------------------------------------------------------------

/// Parses `nvidia-smi --query-gpu=... --format=csv,noheader,nounits` in either
/// the 8-column base shape or the 18-column extended shape.
fn parse_nvidia_smi_csv(output: &str) -> Vec<GpuSample> {
    let mut gpus = Vec::new();
    for line in output.lines() {
        let cells: Vec<&str> = line.split(',').map(str::trim).collect();
        if cells.len() != 8 && cells.len() != 18 {
            continue;
        }
        let Some(index) = cells[0].parse::<u32>().ok() else {
            continue;
        };
        let mut gpu = GpuSample {
            index,
            name: cells[1].chars().take(80).collect(),
            util_pct: smi_float(cells[2]),
            mem_used_bytes: smi_float(cells[3]).map(|mib| mib * MIB),
            mem_total_bytes: smi_float(cells[4]).map(|mib| mib * MIB),
            power_watts: smi_float(cells[5]),
            temp_c: smi_float(cells[6]),
            sm_mhz: smi_float(cells[7]),
            ..GpuSample::default()
        };
        if cells.len() == 18 {
            gpu.mem_util_pct = smi_float(cells[8]);
            gpu.mem_clock_mhz = smi_float(cells[9]);
            gpu.power_limit_watts = smi_float(cells[10]);
            gpu.fan_pct = smi_float(cells[11]);
            gpu.pstate = smi_pstate(cells[12]);
            gpu.temp_mem_c = smi_float(cells[13]);
            gpu.throttle_sw_power = smi_flag(cells[14]);
            gpu.throttle_sw_thermal = smi_flag(cells[15]);
            gpu.throttle_hw_thermal = smi_flag(cells[16]);
            gpu.throttle_hw = smi_flag(cells[17]);
        }
        gpus.push(gpu);
    }
    gpus
}

/// One numeric cell; `[N/A]`, `[Not Supported]`, and non-finite values are absent.
fn smi_float(cell: &str) -> Option<f64> {
    cell.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn smi_flag(cell: &str) -> Option<f64> {
    match cell.to_ascii_lowercase().as_str() {
        "active" => Some(1.0),
        "not active" => Some(0.0),
        _ => None,
    }
}

fn smi_pstate(cell: &str) -> Option<f64> {
    let number = cell.strip_prefix('P').or_else(|| cell.strip_prefix('p'))?;
    number.parse::<u8>().ok().map(f64::from)
}

/// Busy, total, and iowait jiffies from the aggregate `cpu` line. Idle
/// includes iowait, the usual CPU-busy definition.
fn parse_proc_stat_cpu(text: &str) -> Option<CpuTimes> {
    let line = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some("cpu"))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if values.len() < 4 {
        return None;
    }
    let total: u64 = values.iter().sum();
    let iowait = values.get(4).copied().unwrap_or(0);
    let idle = values[3] + iowait;
    Some(CpuTimes {
        busy: total.saturating_sub(idle),
        total,
        iowait,
    })
}

#[derive(Debug, PartialEq)]
struct Memory {
    total: f64,
    used: Option<f64>,
    cached: Option<f64>,
    swap_total: Option<f64>,
    swap_used: Option<f64>,
    dirty: Option<f64>,
    writeback: Option<f64>,
}

fn parse_meminfo(text: &str) -> Option<Memory> {
    let fields: HashMap<&str, u64> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let key = parts.next()?.strip_suffix(':')?;
            Some((key, parts.next()?.parse::<u64>().ok()? * 1024))
        })
        .collect();
    let get = |key: &str| fields.get(key).copied();
    let total = get("MemTotal")?;
    Some(Memory {
        total: total as f64,
        used: get("MemAvailable").map(|available| total.saturating_sub(available) as f64),
        cached: get("Cached").map(|cached| (cached + get("Buffers").unwrap_or(0)) as f64),
        swap_total: get("SwapTotal").map(|bytes| bytes as f64),
        swap_used: get("SwapTotal")
            .zip(get("SwapFree"))
            .map(|(total, free)| total.saturating_sub(free) as f64),
        dirty: get("Dirty").map(|bytes| bytes as f64),
        writeback: get("Writeback").map(|bytes| bytes as f64),
    })
}

/// Sums rx/tx bytes across physical-looking interfaces.
fn parse_net_dev(text: &str) -> Option<(u64, u64)> {
    let mut totals = None;
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name == "lo"
            || ["docker", "veth", "br-", "virbr"]
                .iter()
                .any(|p| name.starts_with(p))
        {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 16 {
            continue;
        }
        let (Ok(rx), Ok(tx)) = (fields[0].parse::<u64>(), fields[8].parse::<u64>()) else {
            continue;
        };
        let (rx_total, tx_total) = totals.get_or_insert((0u64, 0u64));
        *rx_total += rx;
        *tx_total += tx;
    }
    totals
}

#[derive(Debug, Default, PartialEq)]
struct DiskCounters {
    read_bytes: u64,
    write_bytes: u64,
    reads: u64,
    writes: u64,
    inflight: u64,
    io_ticks: HashMap<String, u64>,
}

/// Sums whole-disk counters, skipping partitions. `None` if no disk is seen.
fn parse_diskstats(text: &str) -> Option<DiskCounters> {
    let mut counters = DiskCounters::default();
    let mut seen = false;
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 14 || !is_whole_disk(fields[2]) {
            continue;
        }
        let parsed: Option<Vec<u64>> = [3, 5, 7, 9, 11, 12]
            .iter()
            .map(|&i| fields[i].parse::<u64>().ok())
            .collect();
        let Some(
            [
                reads,
                read_sectors,
                writes,
                written_sectors,
                inflight,
                io_ticks,
            ],
        ) = parsed.and_then(|values| <[u64; 6]>::try_from(values).ok())
        else {
            continue;
        };
        counters.reads += reads;
        counters.read_bytes += read_sectors * 512;
        counters.writes += writes;
        counters.write_bytes += written_sectors * 512;
        counters.inflight += inflight;
        counters.io_ticks.insert(fields[2].to_owned(), io_ticks);
        seen = true;
    }
    seen.then_some(counters)
}

/// `sdX`, `vdX`, `xvdX`, `nvmeXnY`, and `mmcblkX`; not their partitions.
fn is_whole_disk(name: &str) -> bool {
    if ["sd", "vd", "xvd"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return !name.ends_with(|c: char| c.is_ascii_digit());
    }
    if let Some(rest) = name.strip_prefix("nvme") {
        return !rest.contains('p');
    }
    if let Some(rest) = name.strip_prefix("mmcblk") {
        return !rest.contains('p');
    }
    false
}

/// `some avg10` (0-100) from a `/proc/pressure/{io,memory}` file.
fn parse_pressure(text: &str) -> Option<f64> {
    let line = text.lines().find(|line| line.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 100.0))
}

fn parse_loadavg(text: &str) -> Option<f64> {
    text.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_smi_csv_parses_base_and_extended_rows() {
        let gpus = parse_nvidia_smi_csv(
            "0, NVIDIA RTX PRO 6000, 93, 1024, 97887, 332.25, 61, 2430\n\
             1, NVIDIA RTX PRO 6000, [N/A], 0, 97887, 31.0, 30, 180, 4, 405, 600.00, [N/A], P8, [N/A], Active, Not Active, Not Active, odd\n\
             garbage line\n\
             x, bad index, 1, 1, 1, 1, 1, 1\n",
        );
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[0].util_pct, Some(93.0));
        assert_eq!(gpus[0].mem_used_bytes, Some(1024.0 * MIB));
        assert_eq!(gpus[0].power_watts, Some(332.25));
        assert_eq!(gpus[0].pstate, None);
        assert_eq!(gpus[1].util_pct, None);
        assert_eq!(gpus[1].power_limit_watts, Some(600.0));
        assert_eq!(gpus[1].fan_pct, None);
        assert_eq!(gpus[1].pstate, Some(8.0));
        assert_eq!(gpus[1].throttle_sw_power, Some(1.0));
        assert_eq!(gpus[1].throttle_sw_thermal, Some(0.0));
        assert_eq!(gpus[1].throttle_hw, None);
    }

    #[test]
    fn proc_stat_counts_iowait_as_idle() {
        let cpu =
            parse_proc_stat_cpu("cpu  100 0 50 800 40 0 10 0 0 0\ncpu0 1 2 3 4\n").expect("cpu");
        assert_eq!(
            cpu,
            CpuTimes {
                busy: 160,
                total: 1000,
                iowait: 40
            }
        );
        assert_eq!(parse_proc_stat_cpu("cpu0 1 2 3 4\n"), None);
    }

    #[test]
    fn cpu_shares_need_two_samples_and_reject_resets() {
        let mut counters = Counters::default();
        let first = CpuTimes {
            busy: 100,
            total: 1_000,
            iowait: 10,
        };
        assert_eq!(counters.cpu_shares(Some(first)), (None, None));
        let second = CpuTimes {
            busy: 150,
            total: 1_100,
            iowait: 20,
        };
        assert_eq!(counters.cpu_shares(Some(second)), (Some(50.0), Some(10.0)));
        assert_eq!(counters.cpu_shares(Some(first)), (None, None));
    }

    #[test]
    fn meminfo_derives_used_from_available() {
        let memory = parse_meminfo(
            "MemTotal: 1000 kB\nMemAvailable: 400 kB\nCached: 100 kB\nBuffers: 20 kB\n\
             SwapTotal: 50 kB\nSwapFree: 30 kB\nDirty: 5 kB\nnot a line\n",
        )
        .expect("memory");
        assert_eq!(memory.total, 1_024_000.0);
        assert_eq!(memory.used, Some(614_400.0));
        assert_eq!(memory.cached, Some(122_880.0));
        assert_eq!(memory.swap_used, Some(20_480.0));
        assert_eq!(memory.writeback, None);
        assert_eq!(parse_meminfo("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn net_dev_skips_virtual_interfaces() {
        let text = "Inter-|   Receive\n face |bytes\n\
            lo: 9 1 0 0 0 0 0 0 9 1 0 0 0 0 0 0\n\
            eno1: 100 1 0 0 0 0 0 0 200 1 0 0 0 0 0 0\n\
            veth12: 5000 1 0 0 0 0 0 0 5000 1 0 0 0 0 0 0\n\
            br-0ec2: 5000 1 0 0 0 0 0 0 5000 1 0 0 0 0 0 0\n\
            tailscale0: 10 1 0 0 0 0 0 0 20 1 0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(text), Some((110, 220)));
        assert_eq!(parse_net_dev("lo: 1 1 0 0 0 0 0 0 1 1 0 0 0 0 0 0\n"), None);
    }

    #[test]
    fn diskstats_sums_whole_disks_only() {
        let text = "   8       0 sda 10 0 100 0 20 0 200 0 1 50 0\n\
                       8       1 sda1 99 0 999 0 99 0 999 0 9 99 0\n\
                     259       0 nvme0n1 5 0 10 0 5 0 10 0 2 70 0\n\
                     259       1 nvme0n1p1 99 0 999 0 99 0 999 0 9 99 0\n\
                       9       0 md0 99 0 999 0 99 0 999 0 9 99 0\n";
        let disk = parse_diskstats(text).expect("disks");
        assert_eq!(disk.reads, 15);
        assert_eq!(disk.read_bytes, 110 * 512);
        assert_eq!(disk.writes, 25);
        assert_eq!(disk.write_bytes, 210 * 512);
        assert_eq!(disk.inflight, 3);
        assert_eq!(disk.io_ticks.len(), 2);
        assert_eq!(parse_diskstats("9 0 md0 1 0 1 0 1 0 1 0 1 1 0\n"), None);
    }

    #[test]
    fn rates_need_a_prior_value_and_drop_resets() {
        let mut counters = Counters::default();
        let start = Instant::now();
        assert_eq!(counters.rate("k", Some(100.0), start), None);
        let later = start + Duration::from_secs(2);
        assert_eq!(counters.rate("k", Some(300.0), later), Some(100.0));
        let reset = later + Duration::from_secs(1);
        assert_eq!(counters.rate("k", Some(5.0), reset), None);
        assert_eq!(counters.rate("k", None, reset), None);
        assert_eq!(
            counters.rate("k", Some(50.0), reset + Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn disk_util_reports_the_busiest_disk() {
        let mut counters = Counters::default();
        let start = Instant::now();
        let ticks = |a: u64, b: u64| {
            Some(HashMap::from([
                ("sda".to_owned(), a),
                ("nvme0n1".to_owned(), b),
            ]))
        };
        assert_eq!(counters.disk_util_pct(ticks(0, 0), start), None);
        let util = counters.disk_util_pct(ticks(100, 600), start + Duration::from_secs(1));
        assert_eq!(util, Some(60.0));
    }

    #[test]
    fn pressure_and_loadavg_parse() {
        let pressure = "some avg10=12.50 avg60=1.00 avg300=0.00 total=1\nfull avg10=99.00 avg60=0 avg300=0 total=1\n";
        assert_eq!(parse_pressure(pressure), Some(12.5));
        assert_eq!(parse_pressure("full avg10=1.0\n"), None);
        assert_eq!(parse_loadavg("3.25 2.00 1.00 1/100 42\n"), Some(3.25));
        assert_eq!(parse_loadavg(""), None);
    }

    #[test]
    fn whole_disk_names() {
        for name in ["sda", "vdb", "xvdc", "nvme0n1", "mmcblk0"] {
            assert!(is_whole_disk(name), "{name}");
        }
        for name in [
            "sda1",
            "nvme0n1p2",
            "mmcblk0p1",
            "md0",
            "dm-0",
            "loop0",
            "zd0",
        ] {
            assert!(!is_whole_disk(name), "{name}");
        }
    }

    #[test]
    fn host_sample_reads_a_fake_host_root() {
        let root = std::env::temp_dir().join(format!("rj-host-{}", std::process::id()));
        let proc = root.join("proc");
        fs::create_dir_all(proc.join("pressure")).expect("proc");
        fs::write(proc.join("stat"), "cpu 1 0 1 8 0 0 0 0\n").expect("stat");
        fs::write(
            proc.join("meminfo"),
            "MemTotal: 10 kB\nMemAvailable: 4 kB\n",
        )
        .expect("meminfo");
        fs::write(proc.join("loadavg"), "0.50 0 0 1/1 1\n").expect("loadavg");
        fs::write(
            proc.join("pressure/io"),
            "some avg10=3.00 avg60=0 avg300=0 total=0\n",
        )
        .expect("psi");
        let telemetry = HostTelemetry::new(&root, &["/".to_owned(), "/missing".to_owned()]);
        let host = sample_host(&telemetry.sources, &mut Counters::default(), Instant::now())
            .expect("host");
        assert_eq!(host.load1, Some(0.5));
        assert_eq!(host.mem_used_bytes, Some(6_144.0));
        assert_eq!(host.io_pressure_pct, Some(3.0));
        assert_eq!(host.cpu_pct, None);
        // The fake root has no procfs `self` link, so network stays absent.
        assert!(telemetry.sources.net_dev.is_none());
        assert_eq!(host.net_rx_bps, None);
        assert_eq!(host.cpu_watts, None);
        assert_eq!(host.disks.len(), 1);
        assert_eq!(host.disks[0].mount, "/");
        fs::remove_dir_all(&root).expect("cleanup");

        let empty = std::env::temp_dir().join(format!("rj-host-empty-{}", std::process::id()));
        fs::create_dir_all(&empty).expect("empty");
        let telemetry = HostTelemetry::new(&empty, &[]);
        assert!(
            sample_host(&telemetry.sources, &mut Counters::default(), Instant::now()).is_none()
        );
        fs::remove_dir_all(&empty).expect("cleanup");
    }
}
