import { useEffect, useMemo, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { Cloud, RefreshCw, RotateCcw, Square } from "lucide-react";
import { toast } from "sonner";
import { useQueryClient } from "@tanstack/react-query";
import { cloudflareApi, type CloudflareRouteConfig, type CloudflareRouteView } from "@/lib/api/cloudflare";
import type { AppId } from "@/lib/api/types";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Button } from "@/components/ui/button";

interface CloudflareScanProgressEvent {
  providerId: string;
  appType: string;
  stage: string;
  message?: string;
  elapsedSeconds?: number;
}

interface Props {
  appId: AppId;
  providerId?: string;
  // ProviderForm keeps this field as the JSON editor's string value while the
  // provider API stores it as a parsed object. Accept both representations so
  // the panel is available while editing and after loading an existing provider.
  settingsConfig: Record<string, unknown> | string;
}

function parseSettingsConfig(
  settings: Record<string, unknown> | string,
): Record<string, unknown> {
  if (typeof settings !== "string") return settings;
  try {
    const parsed: unknown = JSON.parse(settings);
    return typeof parsed === "object" && parsed !== null
      ? (parsed as Record<string, unknown>)
      : {};
  } catch {
    return {};
  }
}

function findHttpsBaseUrl(settingsInput: Record<string, unknown> | string): string | undefined {
  const settings = parseSettingsConfig(settingsInput);
  const env = settings.env;
  const config = settings.config;
  const candidates: unknown[] = [
    settings.baseUrl,
    settings.baseURL,
    settings.base_url,
    settings.apiUrl,
    settings.api_url,
    typeof env === "object" && env !== null
      ? (env as Record<string, unknown>).ANTHROPIC_BASE_URL
      : undefined,
    typeof env === "object" && env !== null
      ? (env as Record<string, unknown>).GOOGLE_GEMINI_BASE_URL
      : undefined,
  ];

  // Some generic/OpenAI-compatible provider configs keep the endpoint in a
  // nested JSON `config` object rather than at the top level.
  if (typeof config === "object" && config !== null) {
    const nestedConfig = config as Record<string, unknown>;
    candidates.push(
      nestedConfig.baseUrl,
      nestedConfig.baseURL,
      nestedConfig.base_url,
      nestedConfig.apiUrl,
      nestedConfig.api_url,
    );
  }

  // OpenCode stores its endpoint in `options.baseURL`.
  if (typeof settings.options === "object" && settings.options !== null) {
    const options = settings.options as Record<string, unknown>;
    candidates.push(options.baseUrl, options.baseURL, options.base_url);
  }

  // Codex stores its endpoint in the TOML text under settings.config.
  if (typeof config === "string") {
    const match = config.match(/^\s*base_url\s*=\s*["']([^"']+)["']/m);
    if (match?.[1]) candidates.push(match[1]);
  }

  return candidates.find((value): value is string => {
    if (typeof value !== "string" || !value.startsWith("https://")) return false;
    try { return Boolean(new URL(value).hostname); } catch { return false; }
  });
}

function scanLabel(status?: string | null): string {
  switch (status) {
    case "completed_switched": return "已切换到首选 IP";
    case "completed_not_switched": return "首选 IP 未变化";
    case "invalid_first_result": return "首条结果不合格";
    case "no_result": return "没有扫描结果";
    case "cancelled": return "已取消";
    case "process_failed": return "进程失败";
    default: return status ?? "尚未扫描";
  }
}

export function CloudflareRoutePanel({ appId, providerId, settingsConfig }: Props) {
  const queryClient = useQueryClient();
  const [view, setView] = useState<CloudflareRouteView | null>(null);
  const [busy, setBusy] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [scanStage, setScanStage] = useState<string | null>(null);
  const [scanMessage, setScanMessage] = useState<string | null>(null);
  const [scanElapsed, setScanElapsed] = useState(0);
  const [scanLogs, setScanLogs] = useState<string[]>([]);
  const [scanInteracted, setScanInteracted] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const baseUrl = useMemo(() => findHttpsBaseUrl(settingsConfig), [settingsConfig]);

  useEffect(() => {
    if (!providerId) return;
    let cancelled = false;
    void cloudflareApi.get(providerId, appId)
      .then((next) => { if (!cancelled) setView(next); })
      .catch((reason: unknown) => { if (!cancelled) setError(String(reason)); });
    const listeners = ["cloudflare-scan-finished", "cloudflare-monitor-failed", "cloudflare-ip-switched"]
      .map((event) => listen<CloudflareRouteView>(event, (payload) => {
        if (payload.payload?.config?.providerId === providerId && payload.payload?.config?.appType === appId) {
          setView(payload.payload);
          if (event === "cloudflare-scan-finished") {
            setScanning(false);
            setScanStage(null);
            setScanMessage(null);
          }
        }
      }));
    const progressListener = listen<CloudflareScanProgressEvent>("cloudflare-scan-progress", (payload) => {
      if (payload.payload?.providerId === providerId && payload.payload?.appType === appId) {
        setScanning(true);
        setScanStage(payload.payload.stage);
        setScanMessage(payload.payload.message ?? null);
        setScanElapsed(payload.payload.elapsedSeconds ?? 0);
        setScanInteracted(true);
        if (payload.payload.message) {
          setScanLogs((current) => [...current, payload.payload.message!].slice(-5));
        }
      }
    });
    return () => {
      cancelled = true;
      void Promise.all([...listeners, progressListener]).then((unlisteners) => unlisteners.forEach((unlisten) => unlisten()));
    };
  }, [providerId, appId]);

  if (!providerId || !baseUrl) return null;
  let parsed: URL;
  try { parsed = new URL(baseUrl); } catch { return null; }
  const config = view?.config ?? {
    providerId,
    appType: appId,
    targetHost: parsed.hostname,
    targetPort: Number(parsed.port || 443),
    enabled: false,
    monitorInterval: 60,
    tcpLatencyThreshold: 800,
  } satisfies CloudflareRouteConfig;
  const state = view?.state;
  const activeIp = config.enabled ? state?.activeIp : undefined;
  const update = (next: Partial<CloudflareRouteConfig>) => setView({ config: { ...config, ...next }, state: state ?? { routeStatus: "disabled" } });
  const run = async (operation: () => Promise<CloudflareRouteView>, success: string) => {
    setBusy(true); setError(null);
    try {
      setView(await operation());
      await queryClient.invalidateQueries({
        queryKey: ["cloudflare-route", appId, providerId],
      });
      toast.success(success);
    }
    catch (reason: unknown) { const message = String(reason); setError(message); toast.error(message); }
    finally { setBusy(false); }
  };
  const persist = () => run(() => cloudflareApi.save(config, state), "Cloudflare 路由已保存");
  const scan = () => {
    setScanInteracted(true);
    setScanning(true);
    setScanStage("starting");
    setScanMessage("正在准备 Cloudflare 优选扫描");
    setScanElapsed(0);
    setScanLogs([]);
    return run(() => cloudflareApi.scan(config), "CFST 扫描已完成");
  };
  const restoreDns = async () => {
    setBusy(true); setError(null);
    try {
      await cloudflareApi.restoreDns(providerId, appId);
      setView((current) => current ? { ...current, state: { ...current.state, activeIp: null, routeStatus: "dns" } } : current);
      await queryClient.invalidateQueries({
        queryKey: ["cloudflare-route", appId, providerId],
      });
      toast.success("已恢复系统 DNS");
    }
    catch (reason: unknown) { const message = String(reason); setError(message); toast.error(message); }
    finally { setBusy(false); }
  };
  const metrics = state?.lastMonitorMetrics;
  const result = state?.lastScanResult;
  return <section className="space-y-4 rounded-lg border border-border/50 bg-muted/20 p-4" aria-label="Cloudflare 优选 IP">
    <div className="flex items-center justify-between gap-3">
      <div className="flex min-w-0 items-center gap-2"><Cloud className="h-4 w-4 shrink-0 text-sky-500" /><div className="min-w-0"><h3 className="font-medium">Cloudflare 优选 IP</h3><p className="text-xs text-muted-foreground">CFST 只负责选优；日常巡检只统计 TCP、TLS 和 HTTP 边缘质量。</p></div></div>
      <Switch checked={config.enabled} onCheckedChange={(enabled) => update({ enabled })} aria-label="启用 Cloudflare 优选路由" />
    </div>
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
      <div className="min-w-0 space-y-1"><Label htmlFor="cloudflare-target-host">目标域名</Label><Input id="cloudflare-target-host" value={config.targetHost} onChange={(event) => update({ targetHost: event.target.value.trim() })} /></div>
      <div className="min-w-0 space-y-1"><Label htmlFor="cloudflare-target-port">目标端口</Label><Input id="cloudflare-target-port" type="number" min={1} max={65535} value={config.targetPort} onChange={(event) => update({ targetPort: Math.min(65535, Math.max(1, Number(event.target.value) || 443)) })} /></div>
      <div className="min-w-0 space-y-1"><Label htmlFor="cloudflare-monitor-interval">巡检间隔 (秒)</Label><Input id="cloudflare-monitor-interval" type="number" min={30} value={config.monitorInterval} onChange={(event) => update({ monitorInterval: Math.max(30, Number(event.target.value) || 60) })} /></div>
      <div className="min-w-0 space-y-1"><Label htmlFor="cloudflare-threshold">TCP 延迟阈值 (ms)</Label><Input id="cloudflare-threshold" type="number" min={1} value={config.tcpLatencyThreshold} onChange={(event) => update({ tcpLatencyThreshold: Math.max(1, Number(event.target.value) || 800) })} /></div>
    </div>
    <div className="flex flex-wrap gap-2">
      <Button size="sm" variant="outline" onClick={() => void persist()} disabled={busy}>保存路由</Button>
      <Button size="sm" variant="outline" onClick={() => void run(() => cloudflareApi.monitor(config), "连通性检测完成")} disabled={busy || !config.enabled || !activeIp}>立即检测</Button>
      <Button size="sm" onClick={() => void scan()} disabled={busy || scanning}><RefreshCw className="mr-1 h-3.5 w-3.5" />{scanning ? `扫描中（${scanStage ?? "运行"}）` : "手动扫描 CFST"}</Button>
      <Button size="sm" variant="ghost" onClick={() => void cloudflareApi.cancel(providerId, appId).catch((reason: unknown) => toast.error(String(reason)))} disabled={!scanning}><Square className="mr-1 h-3.5 w-3.5" />停止扫描</Button>
      <Button size="sm" variant="ghost" onClick={() => void restoreDns()} disabled={busy}><RotateCcw className="mr-1 h-3.5 w-3.5" />恢复系统 DNS</Button>
    </div>
    <div className="grid min-w-0 grid-cols-[minmax(0,1fr)_minmax(0,1.5fr)] gap-x-4 gap-y-1 text-xs">
      <span>当前实际使用 IP</span><span className="truncate font-mono" title={activeIp ?? undefined}>{activeIp ?? "系统 DNS"}</span>
      <span>最近扫描首选 IP</span><span className="truncate font-mono" title={result?.firstIp ?? undefined}>{result?.firstIp ?? "暂无"}</span>
      <span>扫描状态</span><span>{scanLabel(state?.lastScanStatus)}</span>
      <span>首条丢包 / 延迟</span><span>{result?.firstLossRate ?? "—"} / {result?.firstLatency ?? "—"} ms</span>
      <span>首条下载 / 节点</span><span>{result?.firstDownloadSpeed ?? "—"} MB/s / {result?.firstColo ?? "—"}</span>
      <span>扫描耗时</span><span>{result ? `${result.duration} 秒` : "—"}</span>
      <span>TCP 延迟 / 失败率</span><span>{metrics?.averageTcpLatency ?? "—"} ms / {metrics ? `${(metrics.failureRate * 100).toFixed(1)}%` : "—"}</span>
      <span>边缘探测</span><span>{metrics ? `${metrics.tlsOk === false ? "TLS 失败" : "TLS 正常"}，${metrics.httpOk === false ? "HTTP 失败" : `HTTP ${metrics.httpStatus ?? "正常"} ${metrics.httpPath ?? ""}`}` : "—"}</span>
    </div>
    {scanning && <p className="flex items-center gap-2 text-xs text-sky-700 dark:text-sky-300"><RefreshCw className="h-3.5 w-3.5 animate-spin" />{scanMessage ?? `扫描中（${scanStage ?? "运行"}）`} · 已用时 {scanElapsed} 秒</p>}
    {scanInteracted && scanLogs.length > 0 && <div className="space-y-0.5 rounded-md bg-background/60 p-2 text-[11px] text-muted-foreground"><div className="font-medium text-foreground">扫描日志</div>{scanLogs.map((message, index) => <div key={`${message}-${index}`}>· {message}</div>)}</div>}
    {(error || (scanInteracted && (state?.lastScanError || result?.rejectReason))) && <p className="break-words text-xs text-amber-600">{error ?? state?.lastScanError ?? result?.rejectReason}</p>}
  </section>;
}
