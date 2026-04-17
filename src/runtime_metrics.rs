//! Runtime-level performance observability.
//!
//! Two samplers running on a 1 Hz cadence inside one loop:
//!
//! * Cgroup `cpu.stat` — detects CFS throttling. `throttled_usec` is the
//!   single most important signal for diagnosing `cpu_throttled` vs any
//!   other bottleneck class. Handles cgroup v1 and v2, with or without
//!   cgroup namespaces, and any mount layout — the path to the process's
//!   own cgroup is resolved via `/proc/self/cgroup` + `/proc/self/mountinfo`.
//! * Tokio runtime — per-worker busy duration / park count / local queue
//!   depth via `tokio::runtime::RuntimeMetrics`. Requires the
//!   `tokio_unstable` cfg (set in `.cargo/config.toml`).
//!
//! The loop is intended to be wrapped in `supervised_background_loop` at
//! the call site so a panic restarts it after 5 s instead of silently
//! killing observability.
//!
//! Cost: ~0.05% CPU at 1 Hz, <1 KB RSS.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio::time::{interval, MissedTickBehavior};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Cgroup path resolution — one source of truth for both worker sizing (main)
// and the sampler (this module).
// ---------------------------------------------------------------------------

/// Directory on disk where this process's CPU cgroup files live
/// (`cpu.max` / `cpu.stat` in v2, `cpu.cfs_quota_us` / `cpu.cfs_period_us` /
/// `cpu.stat` in v1). `None` when no cgroup applies (dev box, BSD, unusual
/// layout).
///
/// Algorithm mirrors Rust stdlib's `available_parallelism` cgroup logic:
/// enumerate every cpu-relevant line in `/proc/self/cgroup` (unified v2
/// line and any v1 `cpu` controller lines), resolve each against
/// `/proc/self/mountinfo`, and return the first resolved dir whose cpu
/// quota file actually exists. This handles:
///
/// * pure v2 K8s (cgroup namespace on): one v2 line, resolves to the
///   pod's cgroup dir with `cpu.max`.
/// * pure v2 K8s (cgroup namespace off) / non-`/sys/fs/cgroup` mounts:
///   resolved via `/proc/self/mountinfo`.
/// * pure v1 legacy: v1 line only, resolves to cpu-hierarchy dir.
/// * systemd hybrid (v2 + v1 coexist): v2 line may resolve to a dir
///   where `cpu.max` doesn't exist because the cpu controller lives
///   in v1. Falling through to the v1 candidate keeps us honoring the
///   real limit instead of silently dropping back to
///   `available_parallelism`.
pub fn process_cgroup_cpu_dir() -> Option<PathBuf> {
    let cgroup = fs::read_to_string("/proc/self/cgroup").ok()?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok()?;
    pick_cgroup_cpu_dir(&cgroup, &mountinfo, |p| p.exists())
}

/// Pure selector: iterate cpu-relevant lines in /proc/self/cgroup, resolve
/// each against mountinfo, and return the first dir whose cpu quota file
/// satisfies `probe_exists`. Extracted so the hybrid-cgroup fallback path
/// is unit-testable without real filesystem fixtures.
fn pick_cgroup_cpu_dir<F>(cgroup: &str, mountinfo: &str, probe_exists: F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    for (is_v2, group_path) in cpu_cgroup_lines(cgroup) {
        let Some(dir) = resolve_mount(mountinfo, is_v2, &group_path) else {
            continue;
        };
        let probe = dir.join(if is_v2 { "cpu.max" } else { "cpu.cfs_quota_us" });
        if probe_exists(&probe) {
            return Some(dir);
        }
    }
    None
}

/// Enumerate `(is_v2, group_path)` for every cpu-relevant line in
/// `/proc/self/cgroup`. Order follows file order: on hybrid systems the
/// v2 line appears first, so pure-v2 deployments keep the current
/// behavior and hybrids fall through to v1 only when v2 lacks cpu files.
fn cpu_cgroup_lines(cgroup: &str) -> impl Iterator<Item = (bool, PathBuf)> + '_ {
    cgroup.lines().filter_map(|line| {
        let mut parts = line.splitn(3, ':');
        let _hier = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;
        if controllers.is_empty() {
            Some((true, PathBuf::from(path)))
        } else if controllers.split(',').any(|c| c == "cpu") {
            Some((false, PathBuf::from(path)))
        } else {
            None
        }
    })
}

/// Resolve a single `(is_v2, group_path)` candidate against mountinfo,
/// returning the real on-disk directory. Pure — unit-tested against
/// synthetic mountinfo bodies.
fn resolve_mount(mountinfo: &str, is_v2: bool, group_path: &Path) -> Option<PathBuf> {
    // /proc/self/mountinfo: fields 0-5 fixed, then optional tagged fields,
    // then "-", then fstype, source, super_opts. See proc_pid_mountinfo(5).
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(dash) = fields.iter().position(|&f| f == "-") else {
            continue;
        };
        if dash < 6 || fields.len() < dash + 4 {
            continue;
        }
        // proc(5) escapes whitespace/backslash as octal \ooo; decode so
        // paths with spaces survive as real filesystem paths.
        let root = unescape_mountinfo(fields[3]);
        let mount_point = unescape_mountinfo(fields[4]);
        let fstype = fields[dash + 1];
        let super_opts = fields[dash + 3];

        let matches = if is_v2 {
            fstype == "cgroup2"
        } else {
            fstype == "cgroup" && super_opts.split(',').any(|o| o == "cpu")
        };
        if !matches {
            continue;
        }

        // `group_path` is absolute; `root` is the slice of the source FS
        // the mount exposes. Use `Path::strip_prefix` (not `str::` —
        // that one matches byte prefixes and would incorrectly map
        // root=`/a`, group=`/ab/c` to `<mp>/b/c`).
        //
        // On hosts with multiple cgroup2 mounts (systemd-nspawn,
        // bind-mounted cgroup subtrees, hybrid-migration hosts) the
        // first matching fstype entry may have a root that is *not*
        // a prefix of our group path. `continue` to keep searching
        // instead of giving up on the whole mountinfo.
        let Ok(tail) = group_path.strip_prefix(&root) else {
            continue;
        };
        return Some(PathBuf::from(mount_point).join(tail));
    }
    None
}

/// Test-only convenience: first cpu-relevant candidate, resolved against
/// mountinfo. Pre-existing test inputs have only one cpu line, so their
/// behavior is unchanged. The prod path (`process_cgroup_cpu_dir`) adds
/// an existence check on top to handle hybrid systems.
#[cfg(test)]
fn resolve_cgroup_cpu_dir(cgroup: &str, mountinfo: &str) -> Option<PathBuf> {
    cpu_cgroup_lines(cgroup).find_map(|(is_v2, path)| resolve_mount(mountinfo, is_v2, &path))
}

/// Decode proc(5) octal escapes `\ooo` in a mountinfo field.
/// Only whitespace and backslash are escaped in practice; we decode any
/// three-digit octal for completeness. Invalid sequences are passed through.
fn unescape_mountinfo(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Compute in u16 to avoid u8 overflow on `\7xx` (7 * 64 = 448),
        // then accept only values that fit in a byte. proc(5) emits at
        // most `\377`, but malicious / corrupt mountinfo must not crash
        // the sampler.
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && (b'0'..=b'7').contains(&bytes[i + 1])
            && (b'0'..=b'7').contains(&bytes[i + 2])
            && (b'0'..=b'7').contains(&bytes[i + 3])
        {
            let v = (bytes[i + 1] - b'0') as u16 * 64
                + (bytes[i + 2] - b'0') as u16 * 8
                + (bytes[i + 3] - b'0') as u16;
            if let Ok(b) = u8::try_from(v) {
                out.push(b);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Worker count implied by this process's cgroup CPU quota, computed as
/// `ceil(quota / period)`. Returns `None` when no finite quota applies
/// (unbounded, no cgroup, or parse failure) — caller should fall back to
/// `available_parallelism`.
pub fn cgroup_worker_count() -> Option<usize> {
    let dir = process_cgroup_cpu_dir()?;
    let (quota_us, period_us) = read_cpu_quota(&dir)?;
    compute_worker_count(quota_us, period_us)
}

/// Pure arithmetic core of `cgroup_worker_count`, extracted for direct
/// unit-test coverage. `quota=None` (unbounded) and `period=0` both
/// resolve to `None` so the caller falls back to `available_parallelism`.
fn compute_worker_count(quota_us: Option<u64>, period_us: u64) -> Option<usize> {
    let quota = quota_us?;
    if period_us == 0 {
        return None;
    }
    let n = quota.div_ceil(period_us) as usize;
    if n == 0 {
        None
    } else {
        Some(n)
    }
}

/// Returns `(quota, period)` in microseconds. `quota=None` means the
/// cgroup is unbounded (`"max"` on v2, `-1` on v1).
fn read_cpu_quota(dir: &Path) -> Option<(Option<u64>, u64)> {
    // v2: single file "cpu.max" with "<quota|max> <period>".
    if let Ok(s) = fs::read_to_string(dir.join("cpu.max")) {
        let mut it = s.split_ascii_whitespace();
        let q = it.next()?;
        let p: u64 = it.next()?.parse().ok()?;
        let quota = if q == "max" { None } else { q.parse().ok() };
        return Some((quota, p));
    }
    // v1: separate files; quota=-1 means unbounded.
    let q: i64 = fs::read_to_string(dir.join("cpu.cfs_quota_us"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let p: u64 = fs::read_to_string(dir.join("cpu.cfs_period_us"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some((if q < 0 { None } else { Some(q as u64) }, p))
}

// ---------------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------------

/// Runs the 1 Hz sampler forever. Wrap in `supervised_background_loop` so
/// a panic restarts it instead of permanently killing observability.
pub async fn sampler_loop() {
    let mut ticker = interval(SAMPLE_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let handle = Handle::current();
    let num_workers = handle.metrics().num_workers();
    let mut prev_busy_nanos: Vec<u64> = vec![0; num_workers];
    let mut prev_park_count: Vec<u64> = vec![0; num_workers];

    // Snapshot initial counters so the first tick measures a real
    // interval instead of process-lifetime-to-now.
    let rm = handle.metrics();
    for w in 0..num_workers {
        prev_busy_nanos[w] = rm.worker_total_busy_duration(w).as_nanos() as u64;
        prev_park_count[w] = rm.worker_park_count(w);
    }

    // Resolve the cgroup dir up-front; layout is stable for the
    // process lifetime in every deployment we target. If early-startup
    // read of /proc fails (rare but possible), retry each tick for a
    // short window so a transient failure doesn't permanently disable
    // cgroup sampling, then give up — on hosts that genuinely have no
    // cgroup (dev box, BSD, exotic layouts) we don't want to re-read
    // /proc forever.
    const CGROUP_RETRY_TICKS: u32 = 30;
    let mut cgroup_dir = process_cgroup_cpu_dir();
    let mut cgroup_retries_left: u32 = if cgroup_dir.is_some() {
        0
    } else {
        CGROUP_RETRY_TICKS
    };

    // `tokio::time::interval` fires its first tick immediately; that
    // sample would record a near-zero elapsed and a near-zero busy
    // delta, producing a meaningless first point. Burn it and use the
    // real second tick as the baseline.
    ticker.tick().await;
    let mut prev_tick = Instant::now();

    // `MissedTickBehavior::Delay` lets tick intervals exceed
    // SAMPLE_INTERVAL exactly when the runtime is CPU-starved — which
    // is when `busy_ratio` matters most. Track real wall-clock elapsed
    // per tick so the ratio denominator reflects actual interval, and
    // expose the overshoot as a cumulative sampler-lag counter.
    loop {
        ticker.tick().await;
        let now = Instant::now();
        let elapsed = now.duration_since(prev_tick);
        prev_tick = now;

        // Counter (not gauge) so one burst of CFS-throttle lag survives
        // even if Prometheus's scrape interval skips the moment the
        // sampler recovered — `rate(…[5m])` still sees it.
        let lag_micros = elapsed
            .checked_sub(SAMPLE_INTERVAL)
            .unwrap_or_default()
            .as_micros() as u64;
        if lag_micros > 0 {
            ::metrics::counter!("db9_tokio_sampler_lag_microseconds_total").increment(lag_micros);
        }

        if cgroup_dir.is_none() && cgroup_retries_left > 0 {
            cgroup_retries_left -= 1;
            cgroup_dir = process_cgroup_cpu_dir();
        }
        if let Some(dir) = cgroup_dir.as_deref() {
            sample_cgroup(dir);
        }
        sample_tokio_runtime(
            &handle,
            num_workers,
            elapsed,
            &mut prev_busy_nanos,
            &mut prev_park_count,
        );
    }
}

fn sample_tokio_runtime(
    handle: &Handle,
    num_workers: usize,
    elapsed: Duration,
    prev_busy_nanos: &mut [u64],
    prev_park_count: &mut [u64],
) {
    let rm = handle.metrics();
    // `elapsed` is the real wall-clock interval between this and the
    // previous tick; under CPU starvation it's > SAMPLE_INTERVAL and
    // that's exactly when we need the real value.
    let interval_nanos = (elapsed.as_nanos() as u64).max(1);

    for w in 0..num_workers {
        let busy_nanos = rm.worker_total_busy_duration(w).as_nanos() as u64;
        let park = rm.worker_park_count(w);
        let local_q = rm.worker_local_queue_depth(w);

        let busy_delta = busy_nanos.saturating_sub(prev_busy_nanos[w]);
        let park_delta = park.saturating_sub(prev_park_count[w]);
        prev_busy_nanos[w] = busy_nanos;
        prev_park_count[w] = park;

        let busy_ratio = (busy_delta as f64 / interval_nanos as f64).min(1.0);

        let worker_label = worker_label(w);
        ::metrics::gauge!("db9_tokio_worker_busy_ratio", "worker" => worker_label.clone())
            .set(busy_ratio);
        ::metrics::counter!("db9_tokio_worker_park_total", "worker" => worker_label.clone())
            .increment(park_delta);
        ::metrics::gauge!("db9_tokio_worker_local_queue_depth", "worker" => worker_label)
            .set(local_q as f64);
    }

    ::metrics::gauge!("db9_tokio_global_queue_depth").set(rm.global_queue_depth() as f64);
    ::metrics::gauge!("db9_tokio_blocking_queue_depth").set(rm.blocking_queue_depth() as f64);
    ::metrics::gauge!("db9_tokio_num_blocking_threads").set(rm.num_blocking_threads() as f64);
    ::metrics::gauge!("db9_tokio_num_idle_blocking_threads")
        .set(rm.num_idle_blocking_threads() as f64);
}

fn worker_label(w: usize) -> String {
    w.to_string()
}

fn sample_cgroup(dir: &Path) {
    if let Ok(raw) = fs::read_to_string(dir.join("cpu.stat")) {
        let stat = parse_cpu_stat(&raw);
        if let Some(v) = stat.usage_usec {
            ::metrics::counter!("db9_cgroup_cpu_usage_usec_total").absolute(v);
        }
        if let Some(v) = stat.throttled_usec {
            ::metrics::counter!("db9_cgroup_cpu_throttled_usec_total").absolute(v);
        }
        if let Some(v) = stat.nr_throttled {
            ::metrics::counter!("db9_cgroup_cpu_nr_throttled_total").absolute(v);
        }
        if let Some(v) = stat.nr_periods {
            ::metrics::counter!("db9_cgroup_cpu_nr_periods_total").absolute(v);
        }
    }
    match read_cpu_quota(dir) {
        Some((Some(q), _)) => ::metrics::gauge!("db9_cgroup_cpu_quota_usec").set(q as f64),
        _ => ::metrics::gauge!("db9_cgroup_cpu_quota_usec").set(-1.0),
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct CgroupCpuStat {
    usage_usec: Option<u64>,
    throttled_usec: Option<u64>,
    nr_throttled: Option<u64>,
    nr_periods: Option<u64>,
}

fn parse_cpu_stat(raw: &str) -> CgroupCpuStat {
    let mut stat = CgroupCpuStat::default();
    for line in raw.lines() {
        let mut parts = line.split_ascii_whitespace();
        let Some(key) = parts.next() else { continue };
        let Some(val) = parts.next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        match key {
            "usage_usec" => stat.usage_usec = Some(val),
            "throttled_usec" => stat.throttled_usec = Some(val),
            // cgroup v1 reports throttled time in nanoseconds under
            // `throttled_time`; convert to microseconds. v2's
            // `throttled_usec` wins if both are present.
            "throttled_time" if stat.throttled_usec.is_none() => {
                stat.throttled_usec = Some(val / 1_000);
            }
            "nr_throttled" => stat.nr_throttled = Some(val),
            "nr_periods" => stat.nr_periods = Some(val),
            _ => {}
        }
    }
    stat
}

// ---------------------------------------------------------------------------
// Metric descriptions
// ---------------------------------------------------------------------------

/// Register Prometheus HELP/TYPE for every metric emitted by this module
/// (plus the startup gauges in `main.rs`). Must be called before the
/// first emit so the initial scrape carries metadata.
pub fn describe_metrics() {
    use ::metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

    // Startup gauges (emitted from main / metrics::install_recorder).
    describe_gauge!(
        "db9_server_build_info",
        "Server build info; value is always 1, labels carry version / git_hash"
    );
    describe_gauge!(
        "db9_tokio_worker_threads",
        "Tokio multi-thread runtime worker count actually configured at startup"
    );
    describe_gauge!(
        "db9_server_start_time_seconds",
        Unit::Seconds,
        "Process start time as seconds since the Unix epoch"
    );

    // Cgroup.
    describe_counter!(
        "db9_cgroup_cpu_usage_usec_total",
        Unit::Microseconds,
        "Total CPU usage of this cgroup (from cpu.stat)"
    );
    describe_counter!(
        "db9_cgroup_cpu_throttled_usec_total",
        Unit::Microseconds,
        "Time this cgroup spent throttled by CFS (non-zero ⇒ cpu_limit too low)"
    );
    describe_counter!(
        "db9_cgroup_cpu_nr_throttled_total",
        Unit::Count,
        "Number of CFS periods this cgroup was throttled"
    );
    describe_counter!(
        "db9_cgroup_cpu_nr_periods_total",
        Unit::Count,
        "Number of CFS periods elapsed"
    );
    describe_gauge!(
        "db9_cgroup_cpu_quota_usec",
        Unit::Microseconds,
        "CFS quota from cpu.max / cpu.cfs_quota_us (-1 if unbounded)"
    );

    // Tokio.
    describe_gauge!(
        "db9_tokio_worker_busy_ratio",
        "Tokio worker busy time / sample interval, per-worker (0..=1)"
    );
    describe_counter!(
        "db9_tokio_worker_park_total",
        "Tokio worker park events, per-worker (high delta ⇒ idle worker)"
    );
    describe_gauge!(
        "db9_tokio_worker_local_queue_depth",
        "Tokio worker local queue depth, per-worker"
    );
    describe_gauge!(
        "db9_tokio_global_queue_depth",
        "Tokio global (injection) queue depth"
    );
    describe_gauge!(
        "db9_tokio_blocking_queue_depth",
        "Tokio spawn_blocking queue depth"
    );
    describe_gauge!(
        "db9_tokio_num_blocking_threads",
        "Tokio spawn_blocking pool size"
    );
    describe_gauge!(
        "db9_tokio_num_idle_blocking_threads",
        "Tokio spawn_blocking idle thread count"
    );
    describe_counter!(
        "db9_tokio_sampler_lag_microseconds_total",
        Unit::Microseconds,
        "Cumulative overshoot of the 1 Hz sampler tick vs SAMPLE_INTERVAL (non-zero ⇒ sampler itself was CPU-starved; rate preserves the signal across scrape intervals)"
    );

    // Upload hot-path.
    describe_histogram!(
        "db9_upload_sha256_seconds",
        Unit::Seconds,
        "Per-chunk SHA-256 update() time on the WS upload path"
    );
    describe_histogram!(
        "db9_upload_mpsc_send_seconds",
        Unit::Seconds,
        "Latency of mpsc.send() into the fs9 WriteParts stream (proxies backpressure)"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- cgroup path resolution ---------------------------------------

    fn mi_line(root: &str, mp: &str, fstype: &str, super_opts: &str) -> String {
        format!(
            "29 24 0:26 {root} {mp} rw,nosuid,nodev,noexec,relatime shared:5 - {fstype} cgroup {super_opts}\n"
        )
    }

    #[test]
    fn resolves_v2_with_cgroup_namespace() {
        // Modern K8s pod: namespace on, process sees group_path "/" and
        // mount root "/"; result is the mount point itself.
        let cgroup = "0::/\n";
        let mountinfo = mi_line("/", "/sys/fs/cgroup", "cgroup2", "rw,nsdelegate");
        assert_eq!(
            resolve_cgroup_cpu_dir(cgroup, &mountinfo),
            Some(PathBuf::from("/sys/fs/cgroup"))
        );
    }

    #[test]
    fn resolves_v2_without_cgroup_namespace() {
        // Pod without cgroup ns: /proc/self/cgroup shows the full path.
        let cgroup = "0::/kubepods.slice/pod-abc/container-42\n";
        let mountinfo = mi_line("/", "/sys/fs/cgroup", "cgroup2", "rw");
        assert_eq!(
            resolve_cgroup_cpu_dir(cgroup, &mountinfo),
            Some(PathBuf::from(
                "/sys/fs/cgroup/kubepods.slice/pod-abc/container-42"
            ))
        );
    }

    #[test]
    fn resolves_v1_cpu_cpuacct_hierarchy() {
        let cgroup = "\
11:memory:/kubepods/pod-x\n\
9:cpu,cpuacct:/kubepods/pod-x/container-y\n\
1:name=systemd:/init.scope\n";
        let mountinfo = mi_line(
            "/",
            "/sys/fs/cgroup/cpu,cpuacct",
            "cgroup",
            "rw,cpu,cpuacct",
        );
        assert_eq!(
            resolve_cgroup_cpu_dir(cgroup, &mountinfo),
            Some(PathBuf::from(
                "/sys/fs/cgroup/cpu,cpuacct/kubepods/pod-x/container-y"
            ))
        );
    }

    #[test]
    fn resolves_v1_plain_cpu_hierarchy() {
        // Older v1 layout where `cpu` is mounted standalone.
        let cgroup = "7:cpu:/slice/task\n";
        let mountinfo = mi_line("/", "/sys/fs/cgroup/cpu", "cgroup", "rw,cpu");
        assert_eq!(
            resolve_cgroup_cpu_dir(cgroup, &mountinfo),
            Some(PathBuf::from("/sys/fs/cgroup/cpu/slice/task"))
        );
    }

    #[test]
    fn no_cpu_controller_line_returns_none() {
        let cgroup = "10:memory:/foo\n11:pids:/bar\n";
        let mountinfo = mi_line("/", "/sys/fs/cgroup", "cgroup2", "rw");
        assert_eq!(resolve_cgroup_cpu_dir(cgroup, &mountinfo), None);
    }

    #[test]
    fn no_matching_mount_returns_none() {
        let cgroup = "0::/\n";
        let mountinfo =
            "30 1 0:27 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw\n".to_string();
        assert_eq!(resolve_cgroup_cpu_dir(cgroup, &mountinfo), None);
    }

    #[test]
    fn skips_mount_whose_root_is_not_a_prefix() {
        // cgroup is nested under /a/b, but the available mount exposes
        // only /c — we must not emit a bogus path.
        let cgroup = "0::/a/b\n";
        let mountinfo = mi_line("/c", "/sys/fs/cgroup", "cgroup2", "rw");
        assert_eq!(resolve_cgroup_cpu_dir(cgroup, &mountinfo), None);
    }

    #[test]
    fn pick_cgroup_cpu_dir_prefers_v2_when_cpu_max_exists() {
        let cgroup = "\
0::/user.slice/svc.service\n\
9:cpu,cpuacct:/user.slice/svc.service\n";
        let mut mountinfo = mi_line("/", "/sys/fs/cgroup", "cgroup2", "rw");
        mountinfo.push_str(&mi_line(
            "/",
            "/sys/fs/cgroup/cpu,cpuacct",
            "cgroup",
            "rw,cpu,cpuacct",
        ));
        // v2 probe exists — it wins, v1 is never consulted.
        let picked = pick_cgroup_cpu_dir(cgroup, &mountinfo, |p| {
            p == Path::new("/sys/fs/cgroup/user.slice/svc.service/cpu.max")
        });
        assert_eq!(
            picked,
            Some(PathBuf::from("/sys/fs/cgroup/user.slice/svc.service"))
        );
    }

    #[test]
    fn pick_cgroup_cpu_dir_falls_back_to_v1_when_v2_cpu_max_missing() {
        // This is the hybrid-systemd case: v2 line resolves but the
        // cpu controller isn't delegated to v2, so `cpu.max` doesn't
        // exist. v1 line has cfs_quota_us.
        let cgroup = "\
0::/user.slice/svc.service\n\
9:cpu,cpuacct:/user.slice/svc.service\n";
        let mut mountinfo = mi_line("/", "/sys/fs/cgroup", "cgroup2", "rw");
        mountinfo.push_str(&mi_line(
            "/",
            "/sys/fs/cgroup/cpu,cpuacct",
            "cgroup",
            "rw,cpu,cpuacct",
        ));
        let picked = pick_cgroup_cpu_dir(cgroup, &mountinfo, |p| {
            p == Path::new("/sys/fs/cgroup/cpu,cpuacct/user.slice/svc.service/cpu.cfs_quota_us")
        });
        assert_eq!(
            picked,
            Some(PathBuf::from(
                "/sys/fs/cgroup/cpu,cpuacct/user.slice/svc.service"
            ))
        );
    }

    #[test]
    fn pick_cgroup_cpu_dir_returns_none_when_no_candidate_has_probe() {
        // Hybrid with cpu controller on neither — caller falls back to
        // available_parallelism instead of silently picking a dir with
        // no quota file.
        let cgroup = "\
0::/user.slice/svc.service\n\
9:cpu,cpuacct:/user.slice/svc.service\n";
        let mut mountinfo = mi_line("/", "/sys/fs/cgroup", "cgroup2", "rw");
        mountinfo.push_str(&mi_line(
            "/",
            "/sys/fs/cgroup/cpu,cpuacct",
            "cgroup",
            "rw,cpu,cpuacct",
        ));
        let picked = pick_cgroup_cpu_dir(cgroup, &mountinfo, |_| false);
        assert_eq!(picked, None);
    }

    #[test]
    fn cpu_cgroup_lines_enumerates_v2_before_v1_on_hybrid() {
        // Hybrid systemd: /proc/self/cgroup has both a v2 unified line
        // and a v1 cpu line. `process_cgroup_cpu_dir` tries them in
        // order; the first whose cpu files exist wins. Here we assert
        // the iteration order itself.
        let cgroup = "\
0::/user.slice/svc.service\n\
9:cpu,cpuacct:/user.slice/svc.service\n\
4:memory:/user.slice/svc.service\n";
        let candidates: Vec<_> = cpu_cgroup_lines(cgroup).collect();
        assert_eq!(
            candidates,
            vec![
                (true, PathBuf::from("/user.slice/svc.service")),
                (false, PathBuf::from("/user.slice/svc.service")),
            ]
        );
    }

    #[test]
    fn continues_past_non_matching_mount_to_find_later_matching_mount() {
        // systemd-nspawn / bind-mount case: two cgroup2 mounts. The
        // first is a bind of a subtree (root=/other) that doesn't
        // cover our group_path; the second is the real root mount.
        // The resolver must skip the first and find the second.
        let cgroup = "0::/kubepods/pod-abc\n";
        let mut mountinfo = mi_line("/other", "/host/subtree", "cgroup2", "rw");
        mountinfo.push_str(&mi_line("/", "/sys/fs/cgroup", "cgroup2", "rw"));
        assert_eq!(
            resolve_cgroup_cpu_dir(cgroup, &mountinfo),
            Some(PathBuf::from("/sys/fs/cgroup/kubepods/pod-abc"))
        );
    }

    #[test]
    fn rejects_string_prefix_that_is_not_a_path_component() {
        // Regression guard: str::strip_prefix("/a") on "/ab/c" returns
        // Some("b/c"); the resolver must use Path-component matching
        // and reject this case outright.
        let cgroup = "0::/ab/c\n";
        let mountinfo = mi_line("/a", "/sys/fs/cgroup", "cgroup2", "rw");
        assert_eq!(resolve_cgroup_cpu_dir(cgroup, &mountinfo), None);
    }

    #[test]
    fn resolves_mount_point_with_octal_escaped_space() {
        // proc(5) escapes spaces in mount paths as `\040`. The resolver
        // must decode them so fs::read_to_string hits the real path.
        let cgroup = "0::/\n";
        let mountinfo = mi_line("/", "/sys/fs/my\\040cgroup", "cgroup2", "rw");
        assert_eq!(
            resolve_cgroup_cpu_dir(cgroup, &mountinfo),
            Some(PathBuf::from("/sys/fs/my cgroup"))
        );
    }

    #[test]
    fn unescape_mountinfo_passes_through_when_no_backslash() {
        assert_eq!(unescape_mountinfo("/sys/fs/cgroup"), "/sys/fs/cgroup");
    }

    #[test]
    fn unescape_mountinfo_decodes_space_tab_newline_backslash() {
        assert_eq!(unescape_mountinfo("a\\040b"), "a b");
        assert_eq!(unescape_mountinfo("a\\011b"), "a\tb");
        assert_eq!(unescape_mountinfo("a\\012b"), "a\nb");
        assert_eq!(unescape_mountinfo("a\\134b"), "a\\b");
    }

    #[test]
    fn unescape_mountinfo_passes_invalid_sequences_through() {
        // Not three octal digits → not an escape, leave it alone.
        assert_eq!(unescape_mountinfo("a\\b"), "a\\b");
        assert_eq!(unescape_mountinfo("a\\99"), "a\\99");
    }

    #[test]
    fn unescape_mountinfo_survives_oversize_octal_without_panic() {
        // `\7xx` passes the ASCII-octal bounds check but 7*64 overflows
        // u8 — debug builds used to panic here. Treat oversize values
        // as "not a valid escape" and pass the backslash through.
        assert_eq!(unescape_mountinfo("a\\777b"), "a\\777b");
        assert_eq!(unescape_mountinfo("\\400"), "\\400");
    }

    // --- cpu.stat parsing ---------------------------------------------

    #[test]
    fn cpu_stat_parses_v2_keys() {
        let raw = "usage_usec 1234567\nuser_usec 900000\nsystem_usec 334567\nnr_periods 42\nnr_throttled 3\nthrottled_usec 5000\n";
        let stat = parse_cpu_stat(raw);
        assert_eq!(stat.usage_usec, Some(1234567));
        assert_eq!(stat.throttled_usec, Some(5000));
        assert_eq!(stat.nr_throttled, Some(3));
        assert_eq!(stat.nr_periods, Some(42));
    }

    #[test]
    fn cpu_stat_converts_v1_throttled_time_ns_to_us() {
        let raw = "nr_periods 100\nnr_throttled 7\nthrottled_time 5000000\n";
        let stat = parse_cpu_stat(raw);
        assert_eq!(stat.nr_periods, Some(100));
        assert_eq!(stat.nr_throttled, Some(7));
        assert_eq!(stat.throttled_usec, Some(5000));
    }

    #[test]
    fn cpu_stat_prefers_v2_throttled_usec_over_v1_throttled_time() {
        let raw = "throttled_usec 42\nthrottled_time 9999000\n";
        assert_eq!(parse_cpu_stat(raw).throttled_usec, Some(42));
    }

    #[test]
    fn cpu_stat_skips_malformed_lines_without_aborting() {
        let raw = "usage_usec garbage\nthrottled_usec 7\n\nnr_throttled 2\n";
        let stat = parse_cpu_stat(raw);
        assert_eq!(stat.usage_usec, None);
        assert_eq!(stat.throttled_usec, Some(7));
        assert_eq!(stat.nr_throttled, Some(2));
    }

    // --- worker-count arithmetic --------------------------------------

    #[test]
    fn worker_count_fractional_vcpu_rounds_up() {
        // 500m pod: 50000 / 100000 period = 0.5 vCPU → 1 worker.
        assert_eq!(compute_worker_count(Some(50_000), 100_000), Some(1));
        // 1 vCPU pod.
        assert_eq!(compute_worker_count(Some(100_000), 100_000), Some(1));
        // 1.5 vCPU → 2.
        assert_eq!(compute_worker_count(Some(150_000), 100_000), Some(2));
        // 4 vCPU.
        assert_eq!(compute_worker_count(Some(400_000), 100_000), Some(4));
    }

    #[test]
    fn worker_count_is_none_when_unbounded() {
        // Signals the caller to fall back to available_parallelism.
        assert_eq!(compute_worker_count(None, 100_000), None);
    }

    #[test]
    fn worker_count_is_none_when_period_is_zero() {
        assert_eq!(compute_worker_count(Some(100_000), 0), None);
    }
}
