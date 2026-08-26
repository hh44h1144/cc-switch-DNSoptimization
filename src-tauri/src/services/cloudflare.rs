//! Cloudflare 优选 IP 路由。
//!
//! CloudflareSpeedTest (CFST) 只负责低频选优；日常质量判断由 CC Switch 自己的
//! TCP/TLS/HTTP 边缘探测负责。请求 URL、Host、TLS SNI 和证书校验始终使用原域名。

use crate::database::Database;
use crate::error::AppError;
use chrono::Utc;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const CFST_VERSION: &str = "1da0c02";
const CFST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MONITOR_SAMPLES: u32 = 3;
const DEGRADED_SCAN_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudflareRouteConfig {
    pub provider_id: String,
    pub app_type: String,
    pub target_host: String,
    pub target_port: u16,
    pub enabled: bool,
    pub monitor_interval: u64,
    pub tcp_latency_threshold: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CloudflareMonitorMetrics {
    pub samples: u32,
    pub successes: u32,
    pub failures: u32,
    pub failure_rate: f64,
    pub average_tcp_latency: Option<u64>,
    pub tls_ok: Option<bool>,
    pub http_ok: Option<bool>,
    pub http_status: Option<u16>,
    pub http_path: Option<String>,
    pub last_error: Option<String>,
    pub checked_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CloudflareScanResult {
    pub started_at: i64,
    pub finished_at: i64,
    pub duration: u64,
    pub cfst_version: String,
    pub status: String,
    pub first_ip: Option<String>,
    pub first_loss_rate: Option<f64>,
    pub first_latency: Option<f64>,
    pub first_download_speed: Option<f64>,
    pub first_colo: Option<String>,
    pub switched: bool,
    pub reject_reason: Option<String>,
    pub process_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CloudflareRouteState {
    pub active_ip: Option<String>,
    pub route_status: String,
    pub last_monitor_metrics: Option<CloudflareMonitorMetrics>,
    pub consecutive_degraded: u32,
    pub last_scan_at: Option<i64>,
    pub last_scan_status: Option<String>,
    pub last_scan_result: Option<CloudflareScanResult>,
    pub last_scan_error: Option<String>,
    pub last_switch_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudflareRouteView {
    pub config: CloudflareRouteConfig,
    pub state: CloudflareRouteState,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanProgressEvent {
    provider_id: String,
    app_type: String,
    stage: String,
    message: String,
    elapsed_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IpSwitchedEvent {
    provider_id: String,
    app_type: String,
    previous_ip: Option<String>,
    active_ip: String,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct RouteKey {
    provider_id: String,
    app_type: String,
    host: String,
    port: u16,
}

static ACTIVE_ROUTES: Lazy<RwLock<HashMap<RouteKey, IpAddr>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static SCAN_ABORTS: Lazy<RwLock<HashMap<String, Arc<AtomicBool>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

struct ScanRegistration(String);

impl Drop for ScanRegistration {
    fn drop(&mut self) {
        if let Ok(mut scans) = SCAN_ABORTS.write() {
            scans.remove(&self.0);
        }
    }
}

fn scan_key(config: &CloudflareRouteConfig) -> String {
    format!("{}:{}", config.app_type, config.provider_id)
}

fn emit<T: Serialize + Clone>(app: Option<&tauri::AppHandle>, event: &str, payload: T) {
    if let Some(app) = app {
        if let Err(error) = app.emit(event, payload) {
            log::warn!("发送 {event} 事件失败: {error}");
        }
    }
}

fn emit_scan_progress(
    app: Option<&tauri::AppHandle>,
    config: &CloudflareRouteConfig,
    stage: &str,
    message: impl Into<String>,
    instant: Instant,
) {
    emit(
        app,
        "cloudflare-scan-progress",
        ScanProgressEvent {
            provider_id: config.provider_id.clone(),
            app_type: config.app_type.clone(),
            stage: stage.into(),
            message: message.into(),
            elapsed_seconds: instant.elapsed().as_secs(),
        },
    );
}

pub fn cancel_scan(provider_id: &str, app_type: &str) -> bool {
    SCAN_ABORTS
        .read()
        .ok()
        .and_then(|scans| scans.get(&format!("{app_type}:{provider_id}")).cloned())
        .map(|flag| {
            flag.store(true, Ordering::SeqCst);
            true
        })
        .unwrap_or(false)
}

pub fn active_ip(provider_id: &str, app_type: &str, host: &str, port: u16) -> Option<IpAddr> {
    ACTIVE_ROUTES.read().ok().and_then(|routes| {
        routes
            .get(&RouteKey {
                provider_id: provider_id.to_string(),
                app_type: app_type.to_string(),
                host: host.to_ascii_lowercase(),
                port,
            })
            .copied()
    })
}

pub fn remove_provider_routes(provider_id: &str, app_type: &str) {
    if let Ok(mut routes) = ACTIVE_ROUTES.write() {
        routes.retain(|key, _| !(key.provider_id == provider_id && key.app_type == app_type));
    }
}

fn clear_active_route(config: &CloudflareRouteConfig) {
    if let Ok(mut routes) = ACTIVE_ROUTES.write() {
        routes.remove(&RouteKey {
            provider_id: config.provider_id.clone(),
            app_type: config.app_type.clone(),
            host: config.target_host.to_ascii_lowercase(),
            port: config.target_port,
        });
    }
    crate::proxy::http_client::evict_resolved_target(&config.target_host, config.target_port);
}

fn set_active_route(config: &CloudflareRouteConfig, ip: Option<IpAddr>) {
    clear_active_route(config);
    if let Some(ip) = ip {
        if let Ok(mut routes) = ACTIVE_ROUTES.write() {
            routes.insert(
                RouteKey {
                    provider_id: config.provider_id.clone(),
                    app_type: config.app_type.clone(),
                    host: config.target_host.to_ascii_lowercase(),
                    port: config.target_port,
                },
                ip,
            );
        }
    }
}

pub fn restore_active_route(config: &CloudflareRouteConfig, state: &CloudflareRouteState) {
    if config.enabled {
        if let Some(ip) = state
            .active_ip
            .as_deref()
            .and_then(|value| value.parse().ok())
        {
            set_active_route(config, Some(ip));
        }
    }
}

fn validate_config(config: &CloudflareRouteConfig) -> Result<(), AppError> {
    if config.provider_id.trim().is_empty() || config.app_type.trim().is_empty() {
        return Err(AppError::InvalidInput("供应商标识不能为空".into()));
    }
    if config.target_host.trim().is_empty() || config.target_port == 0 {
        return Err(AppError::InvalidInput(
            "Cloudflare 目标主机和端口不能为空".into(),
        ));
    }
    if config.monitor_interval < 30 {
        return Err(AppError::InvalidInput("巡检间隔不能小于 30 秒".into()));
    }
    if config.tcp_latency_threshold == 0 {
        return Err(AppError::InvalidInput("TCP 延迟阈值必须大于 0".into()));
    }
    Ok(())
}

pub fn save_route(
    db: &Database,
    config: CloudflareRouteConfig,
    mut state: CloudflareRouteState,
) -> Result<CloudflareRouteView, AppError> {
    validate_config(&config)?;
    if let Some((old_config, _)) = db.get_cloudflare_route(&config.provider_id, &config.app_type)? {
        if old_config.target_host != config.target_host
            || old_config.target_port != config.target_port
        {
            clear_active_route(&old_config);
            // An IP selected for a different hostname/port must never be reused
            // for the new route target.
            state.active_ip = None;
            state.route_status = if config.enabled { "disabled" } else { "dns" }.into();
            state.last_monitor_metrics = None;
            state.consecutive_degraded = 0;
        }
    }
    if config.enabled {
        let ip = state
            .active_ip
            .as_deref()
            .and_then(|value| value.parse().ok());
        set_active_route(&config, ip);
    } else {
        clear_active_route(&config);
    }
    db.save_cloudflare_route(&config, &state)?;
    Ok(CloudflareRouteView { config, state })
}

pub fn get_route(
    db: &Database,
    provider_id: &str,
    app_type: &str,
) -> Result<Option<CloudflareRouteView>, AppError> {
    Ok(db
        .get_cloudflare_route(provider_id, app_type)?
        .map(|(config, state)| {
            restore_active_route(&config, &state);
            CloudflareRouteView { config, state }
        }))
}

pub fn restore_all_routes(db: &Database) -> Result<usize, AppError> {
    let routes = db.list_cloudflare_routes()?;
    let count = routes.len();
    for (config, state) in routes {
        restore_active_route(&config, &state);
    }
    Ok(count)
}

fn executable_path(app: Option<&tauri::AppHandle>) -> Result<PathBuf, AppError> {
    if let Ok(path) = std::env::var("CC_SWITCH_CFST_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }
    let binary_name = if cfg!(windows) { "cfst.exe" } else { "cfst" };
    let platform_dir = if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        "darwin-arm64"
    } else if cfg!(target_os = "macos") {
        "darwin-amd64"
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
        "linux-arm64"
    } else if cfg!(target_os = "linux") {
        "linux-amd64"
    } else if cfg!(target_os = "windows") && cfg!(target_arch = "aarch64") {
        "windows-arm64"
    } else {
        "windows-amd64"
    };
    let mut candidates = Vec::new();
    if let Some(app) = app {
        if let Ok(resources) = app.path().resource_dir() {
            candidates.push(resources.join("cfst").join(platform_dir).join(binary_name));
            candidates.push(resources.join("cfst").join(binary_name));
            candidates.push(
                resources
                    .join("resources/cfst")
                    .join(platform_dir)
                    .join(binary_name),
            );
            candidates.push(resources.join("resources/cfst").join(binary_name));
            candidates.push(resources.join(binary_name));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(
                parent
                    .join("resources/cfst")
                    .join(platform_dir)
                    .join(binary_name),
            );
            candidates.push(parent.join("resources/cfst").join(binary_name));
            candidates.push(parent.join(binary_name));
            // macOS app bundle: Contents/MacOS/<app> -> Contents/Resources/…
            if cfg!(target_os = "macos") {
                if let Some(contents) = parent.parent() {
                    let resources = contents.join("Resources");
                    candidates.push(
                        resources
                            .join("resources/cfst")
                            .join(platform_dir)
                            .join(binary_name),
                    );
                    candidates.push(resources.join("resources/cfst").join(binary_name));
                    candidates.push(resources.join("cfst").join(platform_dir).join(binary_name));
                    candidates.push(resources.join("cfst").join(binary_name));
                    candidates.push(resources.join(binary_name));
                }
            }
        }
    }
    candidates.push(
        PathBuf::from("resources/cfst")
            .join(platform_dir)
            .join(binary_name),
    );
    candidates.push(PathBuf::from("resources/cfst").join(binary_name));
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            let searched = candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("; ");
            AppError::Config(format!(
                "未找到随应用分发的 CloudflareSpeedTest，已搜索: {searched}"
            ))
        })
}

fn ip_file_path(binary: &Path) -> Result<PathBuf, AppError> {
    if let Ok(path) = std::env::var("CC_SWITCH_CFST_IP_FILE") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }
    binary
        .parent()
        .map(|parent| parent.join("ip.txt"))
        .filter(|path| path.is_file())
        .or_else(|| {
            binary
                .parent()
                .and_then(|parent| parent.parent())
                .map(|parent| parent.join("ip.txt"))
                .filter(|path| path.is_file())
        })
        .ok_or_else(|| AppError::Config("未找到随 CFST 分发的 ip.txt".into()))
}

fn parse_first_result(path: &Path) -> Result<Option<(String, f64, f64, f64, String)>, AppError> {
    let content = std::fs::read_to_string(path).map_err(|error| AppError::io(path, error))?;
    let first = match content.lines().skip(1).find(|line| !line.trim().is_empty()) {
        Some(line) => line,
        None => return Ok(None),
    };
    let fields: Vec<_> = first.split(',').map(str::trim).collect();
    if fields.len() < 7 || fields[0].is_empty() {
        return Err(AppError::Config("CFST 首条 CSV 记录格式错误".into()));
    }
    let parse = |index: usize, name: &str| {
        fields[index]
            .parse::<f64>()
            .map_err(|error| AppError::Config(format!("CFST {name}解析失败: {error}")))
    };
    Ok(Some((
        fields[0].to_string(),
        parse(3, "丢包率")?,
        parse(4, "延迟")?,
        parse(5, "下载速度")?,
        fields[6].to_string(),
    )))
}

fn failed_scan(
    started: i64,
    instant: Instant,
    status: &str,
    error: String,
) -> CloudflareScanResult {
    CloudflareScanResult {
        started_at: started,
        finished_at: Utc::now().timestamp(),
        duration: instant.elapsed().as_secs(),
        cfst_version: CFST_VERSION.into(),
        status: status.into(),
        process_error: Some(error),
        ..Default::default()
    }
}

async fn execute_cfst(
    config: &CloudflareRouteConfig,
    app: Option<&tauri::AppHandle>,
    abort: &AtomicBool,
    started: i64,
    instant: Instant,
) -> CloudflareScanResult {
    emit_scan_progress(
        app,
        config,
        "checking_binary",
        "正在检查随包 CFST 程序",
        instant,
    );
    let binary = match executable_path(app) {
        Ok(path) => path,
        Err(error) => return failed_scan(started, instant, "process_failed", error.to_string()),
    };
    emit_scan_progress(
        app,
        config,
        "loading_ip_list",
        "正在加载随包 IP 列表",
        instant,
    );
    let ip_file = match ip_file_path(&binary) {
        Ok(path) => path,
        Err(error) => return failed_scan(started, instant, "process_failed", error.to_string()),
    };
    let workdir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(error) => return failed_scan(started, instant, "process_failed", error.to_string()),
    };
    if let Err(error) = std::fs::copy(&ip_file, workdir.path().join("ip.txt")) {
        return failed_scan(started, instant, "process_failed", error.to_string());
    }

    emit_scan_progress(app, config, "running", "CFST 正在测速，请稍候", instant);

    // 不传任何参数：保留 CFST 默认 TCP、丢包及下载测速和默认排序。
    let mut command = Command::new(binary);
    command
        .current_dir(workdir.path())
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ] {
        command.env_remove(key);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return failed_scan(started, instant, "process_failed", error.to_string()),
    };
    let stdout_task = child.stdout.take().map(|mut output| {
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = output.read_to_end(&mut bytes).await;
            String::from_utf8_lossy(&bytes).trim().to_string()
        })
    });
    let stderr_task = child.stderr.take().map(|mut output| {
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = output.read_to_end(&mut bytes).await;
            String::from_utf8_lossy(&bytes).trim().to_string()
        })
    });
    let mut last_progress = Instant::now();
    let status = loop {
        if abort.load(Ordering::SeqCst) {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return failed_scan(started, instant, "cancelled", "CFST 扫描已取消".into());
        }
        if instant.elapsed() >= CFST_TIMEOUT {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return failed_scan(started, instant, "process_failed", "CFST 扫描超时".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if last_progress.elapsed() >= Duration::from_secs(5) {
                    emit_scan_progress(app, config, "running", "CFST 正在测速，请稍候", instant);
                    last_progress = Instant::now();
                }
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            Err(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return failed_scan(started, instant, "process_failed", error.to_string());
            }
        }
    };
    let stdout = match stdout_task {
        Some(task) => task.await.unwrap_or_default(),
        None => String::new(),
    };
    let stderr = match stderr_task {
        Some(task) => task.await.unwrap_or_default(),
        None => String::new(),
    };
    if !stdout.is_empty() {
        log::debug!(
            "CFST stdout: {}",
            stdout
                .lines()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    if !stderr.is_empty() {
        log::debug!(
            "CFST stderr: {}",
            stderr
                .lines()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    if !status.success() {
        return failed_scan(
            started,
            instant,
            "process_failed",
            if stderr.is_empty() {
                format!("CFST 退出码 {:?}", status.code())
            } else {
                format!("CFST 退出码 {:?}: {}", status.code(), stderr)
            },
        );
    }

    emit_scan_progress(
        app,
        config,
        "parsing_result",
        "正在解析 CFST 测速结果",
        instant,
    );
    match parse_first_result(&workdir.path().join("result.csv")) {
        Ok(Some((ip, loss, latency, speed, colo))) => {
            let valid_ip = ip.parse::<IpAddr>().is_ok();
            let valid = valid_ip && loss == 0.0 && speed > 0.0;
            CloudflareScanResult {
                started_at: started,
                finished_at: Utc::now().timestamp(),
                duration: instant.elapsed().as_secs(),
                cfst_version: CFST_VERSION.into(),
                status: if valid {
                    "completed_not_switched".into()
                } else {
                    "invalid_first_result".into()
                },
                first_ip: Some(ip),
                first_loss_rate: Some(loss),
                first_latency: Some(latency),
                first_download_speed: Some(speed),
                first_colo: Some(colo),
                reject_reason: (!valid).then(|| {
                    if !valid_ip {
                        "首条结果 IP 无效".into()
                    } else if loss != 0.0 {
                        "首条结果存在丢包".into()
                    } else {
                        "首条结果下载速度为 0".into()
                    }
                }),
                ..Default::default()
            }
        }
        Ok(None) => CloudflareScanResult {
            started_at: started,
            finished_at: Utc::now().timestamp(),
            duration: instant.elapsed().as_secs(),
            cfst_version: CFST_VERSION.into(),
            status: "no_result".into(),
            reject_reason: Some("CFST 未生成结果".into()),
            ..Default::default()
        },
        Err(error) => failed_scan(started, instant, "no_result", error.to_string()),
    }
}

pub async fn run_scan(
    db: &Database,
    config: &CloudflareRouteConfig,
    app: Option<&tauri::AppHandle>,
) -> Result<CloudflareRouteView, AppError> {
    validate_config(config)?;
    log::info!(
        "开始 Cloudflare CFST 扫描: provider={}, app={}, target={}:{}",
        config.provider_id,
        config.app_type,
        config.target_host,
        config.target_port
    );
    let key = scan_key(config);
    let abort = Arc::new(AtomicBool::new(false));
    {
        let mut scans = SCAN_ABORTS.write()?;
        if scans.contains_key(&key) {
            return Err(AppError::Conflict("该供应商已有 CFST 扫描正在运行".into()));
        }
        scans.insert(key.clone(), abort.clone());
    }
    let _registration = ScanRegistration(key);
    let started = Utc::now().timestamp();
    let instant = Instant::now();
    let existing = db.get_cloudflare_route(&config.provider_id, &config.app_type)?;
    let target_changed = existing.as_ref().is_some_and(|(old_config, _)| {
        old_config.target_host != config.target_host || old_config.target_port != config.target_port
    });
    if let Some((old_config, _)) = existing.as_ref().filter(|_| target_changed) {
        clear_active_route(old_config);
    }
    let mut previous = existing.map(|(_, state)| state).unwrap_or_default();
    if target_changed {
        previous.active_ip = None;
        previous.route_status = if config.enabled { "disabled" } else { "dns" }.into();
        previous.last_monitor_metrics = None;
        previous.consecutive_degraded = 0;
    }

    emit_scan_progress(
        app,
        config,
        "preparing",
        "正在准备 Cloudflare 优选扫描",
        instant,
    );

    let mut result = if crate::proxy::http_client::get_current_proxy_url().is_some() {
        failed_scan(
            started,
            instant,
            "process_failed",
            "已启用全局出站代理，CFST 直连扫描已暂停".into(),
        )
    } else {
        execute_cfst(config, app, &abort, started, instant).await
    };

    let mut state = CloudflareRouteState {
        last_scan_at: Some(result.finished_at),
        last_scan_status: Some(result.status.clone()),
        last_scan_error: result
            .process_error
            .clone()
            .or_else(|| result.reject_reason.clone()),
        last_scan_result: Some(result.clone()),
        ..previous
    };

    let previous_ip = state.active_ip.clone();
    if result.status == "completed_not_switched" {
        if let Some(ip) = result
            .first_ip
            .as_deref()
            .and_then(|value| value.parse::<IpAddr>().ok())
        {
            let candidate = ip.to_string();
            emit_scan_progress(app, config, "switching_route", "正在应用优选 IP", instant);
            result.switched = state.active_ip.as_deref() != Some(candidate.as_str());
            result.status = if result.switched {
                "completed_switched".into()
            } else {
                "completed_not_switched".into()
            };
            if result.switched {
                state.last_switch_at = Some(result.finished_at);
            }
            state.active_ip = Some(candidate.clone());
            state.route_status = if config.enabled {
                "active".into()
            } else {
                "disabled".into()
            };
            set_active_route(config, config.enabled.then_some(ip));
            if result.switched {
                emit(
                    app,
                    "cloudflare-ip-switched",
                    IpSwitchedEvent {
                        provider_id: config.provider_id.clone(),
                        app_type: config.app_type.clone(),
                        previous_ip,
                        active_ip: candidate,
                    },
                );
            }
        }
    }
    state.last_scan_status = Some(result.status.clone());
    state.last_scan_result = Some(result.clone());
    db.save_cloudflare_route(config, &state)?;
    let view = CloudflareRouteView {
        config: config.clone(),
        state,
    };
    emit_scan_progress(
        app,
        config,
        if view.state.last_scan_status.as_deref() == Some("cancelled") {
            "cancelled"
        } else if view.state.last_scan_status.as_deref() == Some("process_failed")
            || view.state.last_scan_status.as_deref() == Some("no_result")
            || view.state.last_scan_status.as_deref() == Some("invalid_first_result")
        {
            "failed"
        } else {
            "completed"
        },
        view.state
            .last_scan_error
            .as_deref()
            .unwrap_or("CFST 扫描已完成"),
        instant,
    );
    log::info!(
        "Cloudflare CFST 扫描结束: provider={}, status={:?}, active_ip={:?}",
        config.provider_id,
        view.state.last_scan_status,
        view.state.active_ip
    );
    emit(app, "cloudflare-scan-finished", view.clone());
    Ok(view)
}

struct HttpProbe {
    tls_ok: bool,
    http_ok: bool,
    status: Option<u16>,
    path: String,
    body: Vec<u8>,
    error: Option<String>,
}

fn probe_tls_connector() -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let native = rustls_native_certs::load_native_certs();
    roots.add_parsable_certificates(native.certs);
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

async fn probe_http_path(config: &CloudflareRouteConfig, ip: IpAddr, path: &str) -> HttpProbe {
    let timeout = Duration::from_secs(10);
    let operation = async {
        let tcp = tokio::net::TcpStream::connect(SocketAddr::new(ip, config.target_port))
            .await
            .map_err(|error| std::io::Error::other(format!("TCP 连接失败: {error}")))?;
        let server_name = rustls::pki_types::ServerName::try_from(config.target_host.clone())
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let mut tls = probe_tls_connector()
            .connect(server_name, tcp)
            .await
            .map_err(|error| std::io::Error::other(format!("TLS 握手失败: {error}")))?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: cc-switch-cloudflare-monitor/1\r\nAccept: text/plain,*/*\r\nConnection: close\r\n\r\n",
            config.target_host
        );
        tls.write_all(request.as_bytes()).await?;
        tls.flush().await?;
        let mut response = Vec::with_capacity(4096);
        let mut buffer = [0u8; 2048];
        let header_end = loop {
            let read = tls.read(&mut buffer).await?;
            if read == 0 {
                return Err(std::io::Error::other("HTTP 响应头不完整"));
            }
            response.extend_from_slice(&buffer[..read]);
            if let Some(position) = response.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            if response.len() >= 64 * 1024 {
                return Err(std::io::Error::other("HTTP 响应头过大"));
            }
        };
        let status_line = response[..header_end]
            .split(|byte| *byte == b'\n')
            .next()
            .ok_or_else(|| std::io::Error::other("HTTP 状态行缺失"))?;
        let status_line = String::from_utf8_lossy(status_line);
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or_else(|| std::io::Error::other("HTTP 状态码无效"))?;
        Ok::<_, std::io::Error>((status, response[header_end..].to_vec()))
    };
    match tokio::time::timeout(timeout, operation).await {
        Ok(Ok((status, body))) => HttpProbe {
            tls_ok: true,
            http_ok: true,
            status: Some(status),
            path: path.into(),
            body,
            error: None,
        },
        Ok(Err(error)) => HttpProbe {
            tls_ok: !error.to_string().contains("TCP 连接失败")
                && !error.to_string().contains("TLS 握手失败"),
            http_ok: false,
            status: None,
            path: path.into(),
            body: Vec::new(),
            error: Some(error.to_string()),
        },
        Err(_) => HttpProbe {
            tls_ok: false,
            http_ok: false,
            status: None,
            path: path.into(),
            body: Vec::new(),
            error: Some("TLS/HTTP 探测超时".into()),
        },
    }
}

async fn probe_http(config: &CloudflareRouteConfig, ip: IpAddr) -> HttpProbe {
    let trace = probe_http_path(config, ip, "/cdn-cgi/trace").await;
    let trace_supported = trace.http_ok
        && (trace.body.windows(3).any(|part| part == b"fl=")
            || trace.body.windows(5).any(|part| part == b"colo="));
    if trace_supported {
        trace
    } else if trace.tls_ok {
        probe_http_path(config, ip, "/").await
    } else {
        trace
    }
}

pub async fn monitor_once(
    config: &CloudflareRouteConfig,
    state: &mut CloudflareRouteState,
) -> Result<CloudflareMonitorMetrics, AppError> {
    validate_config(config)?;
    let ip = state
        .active_ip
        .as_deref()
        .ok_or_else(|| AppError::Config("当前没有 active IP".into()))?
        .parse::<IpAddr>()
        .map_err(|error| AppError::Config(format!("active IP 无效: {error}")))?;
    let mut successes = 0;
    let mut latencies = Vec::new();
    let mut last_error = None;
    for _ in 0..MONITOR_SAMPLES {
        let started = Instant::now();
        let connect = tokio::net::TcpStream::connect(SocketAddr::new(ip, config.target_port));
        match tokio::time::timeout(
            Duration::from_millis(config.tcp_latency_threshold.max(1000)),
            connect,
        )
        .await
        {
            Ok(Ok(stream)) => {
                successes += 1;
                latencies.push(started.elapsed().as_millis() as u64);
                drop(stream);
            }
            Ok(Err(error)) => last_error = Some(error.to_string()),
            Err(_) => last_error = Some("TCP 建联超时".into()),
        }
    }

    let http = if successes > 0 {
        Some(probe_http(config, ip).await)
    } else {
        None
    };
    if let Some(error) = http.as_ref().and_then(|probe| probe.error.clone()) {
        last_error = Some(error);
    }
    let failures = MONITOR_SAMPLES - successes;
    let average_tcp_latency =
        (!latencies.is_empty()).then(|| latencies.iter().sum::<u64>() / latencies.len() as u64);
    let metrics = CloudflareMonitorMetrics {
        samples: MONITOR_SAMPLES,
        successes,
        failures,
        failure_rate: failures as f64 / MONITOR_SAMPLES as f64,
        average_tcp_latency,
        tls_ok: http.as_ref().map(|probe| probe.tls_ok),
        http_ok: http.as_ref().map(|probe| probe.http_ok),
        http_status: http.as_ref().and_then(|probe| probe.status),
        http_path: http.as_ref().map(|probe| probe.path.clone()),
        last_error,
        checked_at: Utc::now().timestamp(),
    };
    let degraded = metrics.failures > 0
        || metrics.average_tcp_latency.unwrap_or(u64::MAX) > config.tcp_latency_threshold
        || metrics.tls_ok == Some(false)
        || metrics.http_ok == Some(false);
    state.consecutive_degraded = if degraded {
        state.consecutive_degraded.saturating_add(1)
    } else {
        0
    };
    state.last_monitor_metrics = Some(metrics.clone());
    state.route_status = if degraded { "degraded" } else { "active" }.into();
    Ok(metrics)
}

pub fn start_monitor_worker(db: Arc<Database>, app: tauri::AppHandle) {
    // Tauri's setup hook is not guaranteed to run inside a Tokio runtime on
    // every platform. Use the runtime owned by Tauri so startup cannot panic
    // with "there is no reactor running".
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            ticker.tick().await;
            if crate::proxy::http_client::get_current_proxy_url().is_some() {
                continue;
            }
            let routes = match db.list_cloudflare_routes() {
                Ok(routes) => routes,
                Err(error) => {
                    log::warn!("读取 Cloudflare 路由失败: {error}");
                    continue;
                }
            };
            let now = Utc::now().timestamp();
            for (config, mut state) in routes {
                if !config.enabled || state.active_ip.is_none() {
                    continue;
                }
                if state.last_monitor_metrics.as_ref().is_some_and(|metrics| {
                    now - metrics.checked_at < config.monitor_interval as i64
                }) {
                    continue;
                }
                match monitor_once(&config, &mut state).await {
                    Ok(metrics) => {
                        if state.consecutive_degraded > 0 {
                            emit(
                                Some(&app),
                                "cloudflare-monitor-failed",
                                CloudflareRouteView {
                                    config: config.clone(),
                                    state: state.clone(),
                                },
                            );
                        }
                        let should_scan = state.consecutive_degraded >= DEGRADED_SCAN_THRESHOLD;
                        if should_scan {
                            state.consecutive_degraded = 0;
                        }
                        if let Err(error) = db.save_cloudflare_route(&config, &state) {
                            log::warn!("保存 Cloudflare 巡检结果失败: {error}");
                            continue;
                        }
                        if should_scan {
                            match run_scan(&db, &config, Some(&app)).await {
                                Ok(_) => log::info!(
                                    "Cloudflare 路由连续质量恶化，已重新执行 CFST: provider={}",
                                    config.provider_id
                                ),
                                Err(error) => {
                                    log::warn!("Cloudflare 重扫失败，保留旧 active IP: {error}")
                                }
                            }
                        }
                        let _ = metrics;
                    }
                    Err(error) => log::warn!("Cloudflare TCP 巡检失败: {error}"),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_the_first_cfst_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result.csv");
        std::fs::write(&path, "IP 地址,已发送,已接收,丢包率,平均延迟,下载速度(MB/s),地区码\n1.1.1.1,4,4,0.00,20.10,2.50,HKG\n2.2.2.2,4,4,0.00,10.00,9.00,NRT\n").unwrap();
        let first = parse_first_result(&path).unwrap().unwrap();
        assert_eq!(first.0, "1.1.1.1");
        assert_eq!(first.1, 0.0);
        assert_eq!(first.3, 2.5);
    }

    #[test]
    fn malformed_first_row_does_not_fall_through_to_second_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result.csv");
        std::fs::write(&path, "header\ninvalid\n2.2.2.2,4,4,0.00,10.00,9.00,NRT\n").unwrap();
        assert!(parse_first_result(&path).is_err());
    }

    #[test]
    fn zero_speed_or_nonzero_loss_is_rejected_by_the_scan_rule() {
        let is_valid = |loss: f64, speed: f64| loss == 0.0 && speed > 0.0;
        assert!(!is_valid(0.25, 10.0));
        assert!(!is_valid(0.0, 0.0));
        assert!(is_valid(0.0, 0.01));
    }
}
