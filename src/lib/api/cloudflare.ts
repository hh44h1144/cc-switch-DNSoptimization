import { invoke } from "@tauri-apps/api/core";

export interface CloudflareRouteConfig {
  providerId: string;
  appType: string;
  targetHost: string;
  targetPort: number;
  enabled: boolean;
  monitorInterval: number;
  tcpLatencyThreshold: number;
}

export interface CloudflareMonitorMetrics {
  samples: number;
  successes: number;
  failures: number;
  failureRate: number;
  averageTcpLatency?: number | null;
  tlsOk?: boolean | null;
  httpOk?: boolean | null;
  httpStatus?: number | null;
  httpPath?: string | null;
  lastError?: string | null;
  checkedAt: number;
}

export interface CloudflareScanResult {
  startedAt: number;
  finishedAt: number;
  duration: number;
  cfstVersion: string;
  status: string;
  firstIp?: string | null;
  firstLossRate?: number | null;
  firstLatency?: number | null;
  firstDownloadSpeed?: number | null;
  firstColo?: string | null;
  switched: boolean;
  rejectReason?: string | null;
  processError?: string | null;
}

export interface CloudflareRouteState {
  activeIp?: string | null;
  routeStatus: string;
  consecutiveDegraded?: number;
  lastMonitorMetrics?: CloudflareMonitorMetrics | null;
  lastScanAt?: number | null;
  lastScanStatus?: string | null;
  lastScanResult?: CloudflareScanResult | null;
  lastScanError?: string | null;
  lastSwitchAt?: number | null;
}

export interface CloudflareRouteView {
  config: CloudflareRouteConfig;
  state: CloudflareRouteState;
}

export const cloudflareApi = {
  get(providerId: string, appType: string) {
    return invoke<CloudflareRouteView | null>("get_cloudflare_route", {
      providerId,
      appType,
    });
  },
  save(config: CloudflareRouteConfig, routeState?: CloudflareRouteState) {
    return invoke<CloudflareRouteView>("save_cloudflare_route", {
      config,
      routeState,
    });
  },
  scan(config: CloudflareRouteConfig) {
    return invoke<CloudflareRouteView>("run_cloudflare_scan", { config });
  },
  monitor(config: CloudflareRouteConfig) {
    return invoke<CloudflareRouteView>("monitor_cloudflare_route", { config });
  },
  cancel(providerId: string, appType: string) {
    return invoke<void>("cancel_cloudflare_scan", { providerId, appType });
  },
  restoreDns(providerId: string, appType: string) {
    return invoke<void>("restore_cloudflare_dns", { providerId, appType });
  },
};
