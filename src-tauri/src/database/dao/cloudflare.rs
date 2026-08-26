use super::super::{lock_conn, Database};
use crate::error::AppError;
use crate::services::cloudflare::{CloudflareRouteConfig, CloudflareRouteState};

impl Database {
    pub fn list_cloudflare_routes(
        &self,
    ) -> Result<Vec<(CloudflareRouteConfig, CloudflareRouteState)>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn.prepare(
            "SELECT provider_id, app_type, target_host, target_port, enabled, monitor_interval,
                    tcp_latency_threshold, active_ip, route_status, last_monitor_metrics, consecutive_degraded, last_scan_at,
                    last_scan_status, last_scan_result, last_scan_error, last_switch_at
             FROM cloudflare_routes",
        ).map_err(|e| AppError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                let config = CloudflareRouteConfig {
                    provider_id: row.get(0)?,
                    app_type: row.get(1)?,
                    target_host: row.get(2)?,
                    target_port: row.get::<_, i64>(3)? as u16,
                    enabled: row.get::<_, i64>(4)? != 0,
                    monitor_interval: row.get::<_, i64>(5)? as u64,
                    tcp_latency_threshold: row.get::<_, i64>(6)? as u64,
                };
                let state = CloudflareRouteState {
                    active_ip: row.get(7)?,
                    route_status: row.get(8)?,
                    last_monitor_metrics: row
                        .get::<_, Option<String>>(9)?
                        .and_then(|v| serde_json::from_str(&v).ok()),
                    consecutive_degraded: row.get::<_, i64>(10)? as u32,
                    last_scan_at: row.get(11)?,
                    last_scan_status: row.get(12)?,
                    last_scan_result: row
                        .get::<_, Option<String>>(13)?
                        .and_then(|v| serde_json::from_str(&v).ok()),
                    last_scan_error: row.get(14)?,
                    last_switch_at: row.get(15)?,
                };
                Ok((config, state))
            })
            .map_err(|e| AppError::Database(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn get_cloudflare_route(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Result<Option<(CloudflareRouteConfig, CloudflareRouteState)>, AppError> {
        let conn = lock_conn!(self.conn);
        let result = conn.query_row(
            "SELECT target_host, target_port, enabled, monitor_interval, tcp_latency_threshold,
                    active_ip, route_status, last_monitor_metrics, consecutive_degraded, last_scan_at, last_scan_status,
                    last_scan_result, last_scan_error, last_switch_at
             FROM cloudflare_routes WHERE provider_id = ?1 AND app_type = ?2",
            rusqlite::params![provider_id, app_type],
            |row| {
                let config = CloudflareRouteConfig {
                    provider_id: provider_id.to_string(),
                    app_type: app_type.to_string(),
                    target_host: row.get(0)?,
                    target_port: row.get::<_, i64>(1)? as u16,
                    enabled: row.get::<_, i64>(2)? != 0,
                    monitor_interval: row.get::<_, i64>(3)? as u64,
                    tcp_latency_threshold: row.get::<_, i64>(4)? as u64,
                };
                let state = CloudflareRouteState {
                    active_ip: row.get(5)?,
                    route_status: row.get(6)?,
                    last_monitor_metrics: row
                        .get::<_, Option<String>>(7)?
                        .and_then(|v| serde_json::from_str(&v).ok()),
                    consecutive_degraded: row.get::<_, i64>(8)? as u32,
                    last_scan_at: row.get(9)?,
                    last_scan_status: row.get(10)?,
                    last_scan_result: row
                        .get::<_, Option<String>>(11)?
                        .and_then(|v| serde_json::from_str(&v).ok()),
                    last_scan_error: row.get(12)?,
                    last_switch_at: row.get(13)?,
                };
                Ok((config, state))
            },
        );
        match result {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(AppError::Database(error.to_string())),
        }
    }

    pub fn save_cloudflare_route(
        &self,
        config: &CloudflareRouteConfig,
        state: &CloudflareRouteState,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        let metrics = state
            .last_monitor_metrics
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| AppError::Database(e.to_string()))?;
        let scan = state
            .last_scan_result
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| AppError::Database(e.to_string()))?;
        conn.execute(
            "INSERT INTO cloudflare_routes
             (provider_id, app_type, target_host, target_port, enabled, monitor_interval,
              tcp_latency_threshold, active_ip, route_status, last_monitor_metrics, consecutive_degraded, last_scan_at,
              last_scan_status, last_scan_result, last_scan_error, last_switch_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(provider_id, app_type) DO UPDATE SET
              target_host=excluded.target_host, target_port=excluded.target_port,
              enabled=excluded.enabled, monitor_interval=excluded.monitor_interval,
              tcp_latency_threshold=excluded.tcp_latency_threshold, active_ip=excluded.active_ip,
              route_status=excluded.route_status, last_monitor_metrics=excluded.last_monitor_metrics,
              consecutive_degraded=excluded.consecutive_degraded,
              last_scan_at=excluded.last_scan_at, last_scan_status=excluded.last_scan_status,
              last_scan_result=excluded.last_scan_result, last_scan_error=excluded.last_scan_error,
              last_switch_at=excluded.last_switch_at",
            rusqlite::params![
                config.provider_id,
                config.app_type,
                config.target_host,
                config.target_port as i64,
                config.enabled as i64,
                config.monitor_interval as i64,
                config.tcp_latency_threshold as i64,
                state.active_ip,
                state.route_status,
                metrics,
                state.consecutive_degraded as i64,
                state.last_scan_at,
                state.last_scan_status,
                scan,
                state.last_scan_error,
                state.last_switch_at,
            ],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn delete_cloudflare_route(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "DELETE FROM cloudflare_routes WHERE provider_id = ?1 AND app_type = ?2",
            rusqlite::params![provider_id, app_type],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }
}
