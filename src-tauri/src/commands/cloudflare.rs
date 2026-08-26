use crate::services::cloudflare::{
    self, CloudflareRouteConfig, CloudflareRouteState, CloudflareRouteView,
};
use crate::store::AppState;
use tauri::Emitter;

#[tauri::command]
pub fn get_cloudflare_route(
    state: tauri::State<'_, AppState>,
    provider_id: String,
    app_type: String,
) -> Result<Option<CloudflareRouteView>, String> {
    cloudflare::get_route(&state.db, &provider_id, &app_type).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_cloudflare_route(
    state: tauri::State<'_, AppState>,
    config: CloudflareRouteConfig,
    route_state: Option<CloudflareRouteState>,
) -> Result<CloudflareRouteView, String> {
    let existing = cloudflare::get_route(&state.db, &config.provider_id, &config.app_type)
        .map_err(|e| e.to_string())?;
    let route_state = route_state
        .or_else(|| existing.map(|view| view.state))
        .unwrap_or_else(|| CloudflareRouteState {
            route_status: "disabled".into(),
            ..Default::default()
        });
    cloudflare::save_route(&state.db, config, route_state).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn run_cloudflare_scan(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    config: CloudflareRouteConfig,
) -> Result<CloudflareRouteView, String> {
    cloudflare::run_scan(&state.db, &config, Some(&app_handle))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn cancel_cloudflare_scan(provider_id: String, app_type: String) -> Result<(), String> {
    if cloudflare::cancel_scan(&provider_id, &app_type) {
        Ok(())
    } else {
        Err("当前没有运行中的 CFST 扫描".into())
    }
}

#[tauri::command]
pub async fn monitor_cloudflare_route(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    config: CloudflareRouteConfig,
) -> Result<CloudflareRouteView, String> {
    let mut view = cloudflare::get_route(&state.db, &config.provider_id, &config.app_type)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Cloudflare 路由尚未保存".to_string())?;
    cloudflare::monitor_once(&config, &mut view.state)
        .await
        .map_err(|e| e.to_string())?;
    state
        .db
        .save_cloudflare_route(&config, &view.state)
        .map_err(|e| e.to_string())?;
    if view
        .state
        .last_monitor_metrics
        .as_ref()
        .is_some_and(|metrics| {
            metrics.failures > 0
                || metrics.average_tcp_latency.unwrap_or(u64::MAX) > config.tcp_latency_threshold
                || metrics.tls_ok == Some(false)
                || metrics.http_ok == Some(false)
        })
    {
        let _ = app_handle.emit("cloudflare-monitor-failed", view.clone());
    }
    Ok(view)
}

#[tauri::command]
pub fn restore_cloudflare_dns(
    state: tauri::State<'_, AppState>,
    provider_id: String,
    app_type: String,
) -> Result<(), String> {
    let view = cloudflare::get_route(&state.db, &provider_id, &app_type)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Cloudflare 路由尚未保存".to_string())?;
    let mut route_state = view.state;
    route_state.active_ip = None;
    route_state.route_status = "dns".into();
    route_state.last_switch_at = None;
    route_state.last_scan_error = None;
    cloudflare::save_route(&state.db, view.config, route_state).map_err(|e| e.to_string())?;
    Ok(())
}
