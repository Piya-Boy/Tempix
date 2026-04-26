//! Stat collection. All values cached; consumers just read fields.
//!
//! Polled once per tick (default 1000 ms). Network deltas are computed
//! against a high-resolution timestamp so the reported B/s figures are
//! accurate even if the timer drifts.

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sysinfo::{Components, CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};
use wmi::WMIConnection;

const WMI_TEMP_POLL: Duration = Duration::from_secs(5);
const WMI_RETRY: Duration = Duration::from_secs(15);
const HELPER_POLL: Duration = Duration::from_secs(10);
const HELPER_WARMUP: Duration = Duration::from_secs(3);
const GPU_POLL: Duration = Duration::from_secs(4);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct Stats {
    sys: System,
    nets: Networks,
    components: Option<Components>,
    cpu_temp_helper: CpuTempHelper,
    cpu_temp_acpi: CpuTempAcpi,
    cpu_temp_wmi: CpuTempWmi,
    nvml: Option<nvml_wrapper::Nvml>,

    last_net_sample: Instant,
    last_gpu_poll: Instant,

    pub cpu_pct: f32,
    pub cpu_temp_c: Option<f32>,
    pub gpu_pct: Option<u32>,
    pub gpu_temp_c: Option<u32>,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub net_down_bps: u64,
    pub net_up_bps: u64,
}

impl Stats {
    pub fn new() -> Self {
        let refresh = RefreshKind::new()
            .with_cpu(CpuRefreshKind::new().with_cpu_usage())
            .with_memory(MemoryRefreshKind::new().with_ram());

        let mut sys = System::new_with_specifics(refresh);
        // Prime the CPU sample so the first reading is meaningful.
        sys.refresh_cpu_usage();

        let nets = Networks::new_with_refreshed_list();
        let cpu_temp_helper = CpuTempHelper::new();
        let cpu_temp_acpi = CpuTempAcpi::new();
        let cpu_temp_wmi = CpuTempWmi::new();

        // NVML init is fallible (no NVIDIA GPU / driver). Failure is fine.
        let nvml = nvml_wrapper::Nvml::init().ok();

        let mem_total_mb = sys.total_memory() / 1024 / 1024;

        Self {
            sys,
            nets,
            components: None,
            cpu_temp_helper,
            cpu_temp_acpi,
            cpu_temp_wmi,
            nvml,
            last_net_sample: Instant::now(),
            last_gpu_poll: Instant::now() - GPU_POLL,
            cpu_pct: 0.0,
            cpu_temp_c: None,
            gpu_pct: None,
            gpu_temp_c: None,
            mem_used_mb: 0,
            mem_total_mb,
            net_down_bps: 0,
            net_up_bps: 0,
        }
    }

    /// Refresh all metrics. Cheap: only the cpu_usage / memory / network
    /// kinds we asked for are touched.
    pub fn refresh(&mut self) {
        // CPU
        self.sys.refresh_cpu_usage();
        // Aggregate cores -> single global %.
        let cpus = self.sys.cpus();
        if !cpus.is_empty() {
            let mut sum = 0.0f32;
            for c in cpus {
                sum += c.cpu_usage();
            }
            self.cpu_pct = sum / cpus.len() as f32;
        }

        // RAM
        self.sys.refresh_memory();
        self.mem_used_mb = self.sys.used_memory() / 1024 / 1024;

        // CPU temp. Try the helper first; the fallback probes are slower and
        // only run when the helper has had time to produce a value and failed.
        let helper_temp = self.cpu_temp_helper.refresh();
        let use_fallbacks = helper_temp.is_none() && !self.cpu_temp_helper.is_warming_up();
        let acpi_temp = if use_fallbacks {
            self.cpu_temp_acpi.refresh()
        } else {
            None
        };
        let hardware_monitor_temp = if use_fallbacks && acpi_temp.is_none() {
            self.cpu_temp_wmi.refresh()
        } else {
            None
        };
        let sysinfo_temp =
            if use_fallbacks && acpi_temp.is_none() && hardware_monitor_temp.is_none() {
                let components = self.components.get_or_insert_with(Components::new);
                components.refresh_list();
                components.refresh();
                components
                    .iter()
                    .find(|c| {
                        let l = c.label().to_ascii_lowercase();
                        l.contains("cpu") || l.contains("package") || l.contains("tctl")
                    })
                    .map(|c| c.temperature())
            } else {
                None
            };
        self.cpu_temp_c = helper_temp
            .or(acpi_temp)
            .or(hardware_monitor_temp)
            .or(sysinfo_temp);

        // Network: refresh, then sum deltas / elapsed time.
        self.nets.refresh();
        let now = Instant::now();
        let dt = now.duration_since(self.last_net_sample).as_secs_f64();
        self.last_net_sample = now;
        if dt > 0.0 {
            let mut down = 0u64;
            let mut up = 0u64;
            for (_iface, data) in self.nets.iter() {
                down += data.received();
                up += data.transmitted();
            }
            // sysinfo's `received`/`transmitted` already return *delta since
            // last refresh*, so no need to subtract a prior value.
            self.net_down_bps = (down as f64 / dt) as u64;
            self.net_up_bps = (up as f64 / dt) as u64;
        }

        // GPU (NVIDIA only).
        if self.last_gpu_poll.elapsed() >= GPU_POLL {
            self.last_gpu_poll = Instant::now();
            self.refresh_gpu();
        }
    }

    fn refresh_gpu(&mut self) {
        let Some(nvml) = &self.nvml else {
            return;
        };

        if let Ok(dev) = nvml.device_by_index(0) {
            self.gpu_pct = dev.utilization_rates().ok().map(|u| u.gpu);
            self.gpu_temp_c = dev
                .temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
                .ok();
        }
    }
}

struct CpuTempHelper {
    latest: Arc<Mutex<Option<f32>>>,
    running: Arc<AtomicBool>,
    last_start: Instant,
}

impl CpuTempHelper {
    fn new() -> Self {
        let mut helper = Self {
            latest: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(false)),
            last_start: Instant::now() - HELPER_POLL,
        };
        helper.start();
        helper
    }

    fn refresh(&mut self) -> Option<f32> {
        if !self.running.load(Ordering::Acquire) && self.last_start.elapsed() >= HELPER_POLL {
            self.start();
        }

        self.latest.lock().ok().and_then(|latest| *latest)
    }

    fn is_warming_up(&self) -> bool {
        self.running.load(Ordering::Acquire) && self.last_start.elapsed() < HELPER_WARMUP
    }

    fn start(&mut self) {
        if self.running.swap(true, Ordering::AcqRel) {
            return;
        }
        self.last_start = Instant::now();
        let Some(path) = sensor_helper_path() else {
            self.running.store(false, Ordering::Release);
            return;
        };

        let latest = Arc::clone(&self.latest);
        let running = Arc::clone(&self.running);
        thread::spawn(move || {
            let value = Command::new(path)
                .arg("--once")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .ok()
                .and_then(|output| {
                    String::from_utf8(output.stdout)
                        .ok()
                        .and_then(|stdout| parse_helper_temp(stdout.lines().next().unwrap_or("")))
                });

            if let Ok(mut latest) = latest.lock() {
                *latest = value;
            }
            running.store(false, Ordering::Release);
        });
    }
}

fn parse_helper_temp(line: &str) -> Option<f32> {
    line.trim()
        .parse::<f32>()
        .ok()
        .filter(|v| (1.0..=125.0).contains(v))
}

fn sensor_helper_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let path = dir.join("tempix-sensors.exe");
    path.exists().then_some(path)
}

struct CpuTempAcpi {
    connection: Option<WMIConnection>,
    last_poll: Instant,
    last_retry: Instant,
    cached: Option<f32>,
}

impl CpuTempAcpi {
    fn new() -> Self {
        Self {
            connection: None,
            last_poll: Instant::now() - WMI_TEMP_POLL,
            last_retry: Instant::now() - WMI_RETRY,
            cached: None,
        }
    }

    fn refresh(&mut self) -> Option<f32> {
        let now = Instant::now();

        if self.connection.is_none() && now.duration_since(self.last_retry) >= WMI_RETRY {
            self.connection = WMIConnection::with_namespace_path("ROOT\\WMI").ok();
            self.last_retry = now;
        }

        if now.duration_since(self.last_poll) < WMI_TEMP_POLL {
            return self.cached;
        }
        self.last_poll = now;

        if let Some(connection) = &self.connection {
            if let Some(temp) = query_acpi_cpu_temp(connection) {
                self.cached = Some(temp);
                return self.cached;
            }
        }

        self.cached
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AcpiThermalZone {
    current_temperature: Option<u32>,
    instance_name: Option<String>,
}

fn query_acpi_cpu_temp(connection: &WMIConnection) -> Option<f32> {
    let zones: Vec<AcpiThermalZone> = connection
        .raw_query("SELECT CurrentTemperature, InstanceName FROM MSAcpi_ThermalZoneTemperature")
        .ok()?;

    zones
        .into_iter()
        .filter_map(|zone| {
            let raw = zone.current_temperature?;
            let temp_c = raw as f32 / 10.0 - 273.15;
            if (1.0..=125.0).contains(&temp_c) {
                Some((acpi_zone_score(zone.instance_name.as_deref()), temp_c))
            } else {
                None
            }
        })
        .max_by(|(left_score, left_temp), (right_score, right_temp)| {
            left_score
                .cmp(right_score)
                .then_with(|| left_temp.total_cmp(right_temp))
        })
        .map(|(_, temp)| temp)
}

fn acpi_zone_score(name: Option<&str>) -> i32 {
    let Some(name) = name else { return 1 };
    let name = name.to_ascii_lowercase();
    if name.contains("cpu") || name.contains("pkg") || name.contains("thermalzone") {
        10
    } else {
        1
    }
}

struct CpuTempWmi {
    connections: Vec<WMIConnection>,
    last_poll: Instant,
    last_retry: Instant,
    cached: Option<f32>,
}

impl CpuTempWmi {
    fn new() -> Self {
        Self {
            connections: Vec::new(),
            last_poll: Instant::now() - WMI_TEMP_POLL,
            last_retry: Instant::now() - WMI_RETRY,
            cached: None,
        }
    }

    fn refresh(&mut self) -> Option<f32> {
        let now = Instant::now();

        if self.connections.is_empty() && now.duration_since(self.last_retry) >= WMI_RETRY {
            self.connections = open_wmi_connections();
            self.last_retry = now;
        }

        if now.duration_since(self.last_poll) < WMI_TEMP_POLL {
            return self.cached;
        }
        self.last_poll = now;

        for connection in &self.connections {
            if let Some(temp) = query_wmi_cpu_temp(connection) {
                self.cached = Some(temp);
                return self.cached;
            }
        }

        self.cached
    }
}

fn open_wmi_connections() -> Vec<WMIConnection> {
    ["ROOT\\LibreHardwareMonitor", "ROOT\\OpenHardwareMonitor"]
        .into_iter()
        .filter_map(|namespace| WMIConnection::with_namespace_path(namespace).ok())
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WmiSensor {
    name: Option<String>,
    sensor_type: Option<String>,
    value: Option<f32>,
    identifier: Option<String>,
    parent: Option<String>,
}

fn query_wmi_cpu_temp(connection: &WMIConnection) -> Option<f32> {
    let sensors: Vec<WmiSensor> = connection.raw_query("SELECT * FROM Sensor").ok()?;
    let mut best_score = 0;
    let mut best_temp = None;

    for sensor in sensors {
        if !sensor
            .sensor_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("temperature"))
        {
            continue;
        }

        let Some(temp) = sensor.value else { continue };
        if !(1.0..=125.0).contains(&temp) {
            continue;
        }

        let score = cpu_sensor_score(&sensor);
        if score > best_score || (score == best_score && best_temp.is_some_and(|t| temp > t)) {
            best_score = score;
            best_temp = Some(temp);
        }
    }

    best_temp
}

fn cpu_sensor_score(sensor: &WmiSensor) -> i32 {
    let mut text = String::new();
    append_lower(&mut text, sensor.name.as_deref());
    append_lower(&mut text, sensor.identifier.as_deref());
    append_lower(&mut text, sensor.parent.as_deref());

    if text.contains("gpu") || text.contains("nvidia") || text.contains("radeon") {
        return 0;
    }
    if text.contains("package") {
        return 100;
    }
    if text.contains("tctl") || text.contains("tdie") {
        return 95;
    }
    if text.contains("ccd") {
        return 90;
    }
    if text.contains("cpu") {
        return 80;
    }
    if text.contains("core") {
        return 70;
    }
    if text.contains("intel") || text.contains("amd") || text.contains("ryzen") {
        return 60;
    }
    0
}

fn append_lower(out: &mut String, text: Option<&str>) {
    if let Some(text) = text {
        out.push(' ');
        out.extend(text.chars().flat_map(char::to_lowercase));
    }
}
