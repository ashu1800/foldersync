// 设置页（提示词 5.2.6）。

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type {
  EnvStatusDto,
  NetworkMode,
  SettingsDto,
  SettingsPatch,
  UpdateDto,
} from "../lib/bindings";
import * as api from "../lib/tauri";
import { setLanguage } from "../i18n";
import { ErrorBanner, Loading, RelayGuideModal } from "../components/ui";
import { UpdateDetails, useUpdateInstall } from "../components/UpdatePrompt";

const LOG_LEVELS = ["error", "warn", "info", "debug", "trace"];

export default function Settings() {
  const { t, i18n } = useTranslation();
  const [s, setS] = useState<SettingsDto | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [version, setVersion] = useState("");
  const [deviceId, setDeviceId] = useState("");
  const [diagPath, setDiagPath] = useState<string | null>(null);
  const [env, setEnv] = useState<EnvStatusDto | null>(null);
  const [envBusy, setEnvBusy] = useState(false);
  const [envError, setEnvError] = useState<string | null>(null);
  const [showRelayGuide, setShowRelayGuide] = useState(false);
  // 手动检查更新（Basic 版本更新服务）；下载安装与弹窗共用 useUpdateInstall
  const [updateState, setUpdateState] = useState<
    "idle" | "checking" | "found" | "latest" | "error"
  >("idle");
  const [update, setUpdate] = useState<UpdateDto | null>(null);
  const [checkError, setCheckError] = useState<string | null>(null);
  const installer = useUpdateInstall();

  const refreshEnv = useCallback(async () => {
    try {
      const next = await api.getEnvironmentStatus();
      setEnv(next);
      setEnvError(null);
    } catch (e) {
      setEnvError(api.asCmdError(e).message);
    }
  }, []);

  useEffect(() => {
    (async () => {
      try {
        const [settings, info] = await Promise.all([
          api.getSettings(),
          api.getLocalInfo(),
        ]);
        setS(settings);
        setVersion(String(info.version ?? ""));
        setDeviceId(String(info.device_id ?? ""));
      } catch (e) {
        setError(api.asCmdError(e).message);
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const next = await api.getEnvironmentStatus();
        if (!cancelled) setEnv(next);
      } catch (e) {
        if (!cancelled) setEnvError(api.asCmdError(e).message);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const save = async (patch: SettingsPatch) => {
    if (!s) return;
    // 乐观更新：先改界面，失败再回滚
    const prev = s;
    setS({ ...s, ...patchToPreview(patch) } as SettingsDto);
    try {
      const next = await api.updateSettings(patch);
      setS(next);
      setSaved(true);
      window.setTimeout(() => setSaved(false), 1500);
      if (patch.ssh_port !== undefined || patch.relay_url !== undefined) {
        void refreshEnv();
      }
    } catch (e) {
      setS(prev);
      setError(api.asCmdError(e).message);
    }
  };

  const repairSsh = async () => {
    setEnvBusy(true);
    setEnvError(null);
    try {
      setEnv(await api.repairSshEnvironment());
    } catch (e) {
      setEnvError(api.asCmdError(e).message);
    } finally {
      setEnvBusy(false);
    }
  };

  const installTailscale = async () => {
    try {
      await api.openTailscaleDownload();
    } catch (e) {
      setEnvError(api.asCmdError(e).message);
    }
  };

  if (loading) return <Loading />;
  if (!s) {
    return <ErrorBanner message={error ?? t("common.loadFailed")} />;
  }

  return (
    <>
      <div className="page-header">
        <h1 className="page-title">{t("settings.title")}</h1>
        {saved && <span className="badge badge-success">{t("settings.saved")}</span>}
      </div>

      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

      {/* 基本信息 */}
      <div className="card">
        <h2 className="card-title" style={{ marginBottom: 14 }}>
          {t("settings.deviceName")}
        </h2>
        <div className="field">
          <input
            className="input"
            value={s.device_name}
            onChange={(e) => setS({ ...s, device_name: e.target.value })}
            onBlur={() => void save({ device_name: s.device_name })}
            aria-label={t("settings.deviceName")}
          />
        </div>
        <div className="card-meta mono truncate" title={deviceId}>
          {deviceId}
        </div>
      </div>

      {/* 界面语言 */}
      <div className="card">
        <h2 className="card-title" style={{ marginBottom: 14 }}>
          界面语言 / Language
        </h2>
        <div className="btn-row">
          <button
            className={`btn ${i18n.language === "zh" ? "btn-primary" : ""}`}
            onClick={() => setLanguage("zh")}
          >
            中文
          </button>
          <button
            className={`btn ${i18n.language === "en" ? "btn-primary" : ""}`}
            onClick={() => setLanguage("en")}
          >
            English
          </button>
        </div>
      </div>

      {/* 行为 */}
      <div className="card">
        <h2 className="card-title" style={{ marginBottom: 14 }}>
          行为
        </h2>
        <div className="checkbox-row">
          <input
            type="checkbox"
            id="auto-start"
            checked={s.auto_start}
            onChange={(e) => void save({ auto_start: e.target.checked })}
          />
          <label htmlFor="auto-start">{t("settings.autoStart")}</label>
        </div>
        <div className="checkbox-row">
          <input
            type="checkbox"
            id="close-to-tray"
            checked={s.close_to_tray}
            onChange={(e) => void save({ close_to_tray: e.target.checked })}
          />
          <label htmlFor="close-to-tray">{t("settings.closeToTray")}</label>
        </div>
      </div>

      {/* 限速 */}
      <div className="card">
        <h2 className="card-title" style={{ marginBottom: 14 }}>
          带宽
        </h2>
        <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 16 }}>
          <div className="field">
            <label className="label" htmlFor="upload-limit">
              {t("settings.uploadLimit")}
            </label>
            <input
              id="upload-limit"
              className="input"
              type="number"
              min={0}
              value={s.upload_limit_kbps}
              onChange={(e) =>
                setS({ ...s, upload_limit_kbps: Number(e.target.value) })
              }
              onBlur={() => void save({ upload_limit_kbps: s.upload_limit_kbps })}
            />
          </div>
          <div className="field">
            <label className="label" htmlFor="download-limit">
              {t("settings.downloadLimit")}
            </label>
            <input
              id="download-limit"
              className="input"
              type="number"
              min={0}
              value={s.download_limit_kbps}
              onChange={(e) =>
                setS({ ...s, download_limit_kbps: Number(e.target.value) })
              }
              onBlur={() => void save({ download_limit_kbps: s.download_limit_kbps })}
            />
          </div>
        </div>
        <p className="hint">{t("settings.unlimitedHint")}</p>
      </div>

      {/* 网络 */}
      <div className="card">
        <h2 className="card-title" style={{ marginBottom: 14 }}>
          {t("settings.networkTitle")}
        </h2>

        {envError && (
          <p className="hint" style={{ color: "var(--danger)", marginBottom: 10 }}>
            {envError}
          </p>
        )}

        <div className="list" style={{ marginBottom: 16 }}>
          <div className="list-item">
            <div className="list-main">
              <div className="list-title">{t("settings.envSsh")}</div>
              <div className="list-sub">
                {env
                  ? `${t("settings.envPort")} ${env.ssh_port} · ${
                      env.firewall_ok
                        ? t("settings.envFirewallOk")
                        : t("settings.envFirewallBad")
                    }`
                  : t("settings.envUnknown")}
              </div>
            </div>
            <span
              className={`badge ${
                !env ? "badge-muted" : env.ssh_ready ? "badge-success" : "badge-warn"
              }`}
            >
              {!env
                ? t("settings.envUnknown")
                : env.ssh_ready
                  ? t("settings.envSshReady")
                  : t("settings.envSshNotReady")}
            </span>
          </div>

          <div className="list-item">
            <div className="list-main">
              <div className="list-title">{t("settings.envTailscale")}</div>
              <div className="list-sub">
                {!env
                  ? t("settings.envUnknown")
                  : !env.tailscale_installed
                    ? t("settings.envTailscaleMissing")
                    : !env.tailscale_running
                      ? t("settings.envTailscaleStopped")
                      : [
                          ...(env.tailscale_hostname ? [env.tailscale_hostname] : []),
                          ...env.tailscale_ips,
                        ].join(" · ") || "—"}
              </div>
            </div>
            <span
              className={`badge ${
                !env
                  ? "badge-muted"
                  : env.tailscale_running
                    ? "badge-success"
                    : env.tailscale_installed
                      ? "badge-warn"
                      : "badge-muted"
              }`}
            >
              {!env
                ? t("settings.envUnknown")
                : env.tailscale_running
                  ? t("settings.envTailscaleConnected")
                  : env.tailscale_installed
                    ? t("settings.envTailscaleStopped")
                    : t("settings.envTailscaleMissing")}
            </span>
          </div>

          <div className="list-item">
            <div className="list-main">
              <div className="list-title">{t("settings.envRelay")}</div>
              <div className="list-sub">
                {!env
                  ? t("settings.envUnknown")
                  : !env.relay_configured
                    ? t("settings.envRelayMissing")
                    : env.relay_reachable
                      ? t("settings.envRelayReachable")
                      : t("settings.envRelayUnreachable")}
              </div>
            </div>
            <span
              className={`badge ${
                env?.relay_reachable
                  ? "badge-success"
                  : env?.relay_configured
                    ? "badge-danger"
                    : "badge-muted"
              }`}
            >
              {!env
                ? t("settings.envUnknown")
                : env.relay_reachable
                  ? t("settings.envRelayReachable")
                  : env.relay_configured
                    ? t("settings.envRelayUnreachable")
                    : t("settings.envRelayMissing")}
            </span>
          </div>
        </div>

        <div className="btn-row" style={{ marginBottom: 16 }}>
          <button
            className="btn"
            onClick={() => void repairSsh()}
            disabled={envBusy}
            aria-label={t("settings.repairSsh")}
          >
            {envBusy ? t("settings.repairing") : t("settings.repairSsh")}
          </button>
          <button
            className="btn"
            onClick={() => void installTailscale()}
            aria-label={t("settings.installTailscale")}
          >
            {t("settings.installTailscale")}
          </button>
          <button
            className="btn"
            onClick={() => setShowRelayGuide(true)}
            aria-label={t("settings.relayGuide")}
          >
            {t("settings.relayGuide")}
          </button>
        </div>

        <div className="field">
          <label className="label" htmlFor="network-mode">
            {t("settings.networkMode")}
          </label>
          <select
            id="network-mode"
            className="select"
            value={s.network_mode}
            onChange={(e) => void save({ network_mode: e.target.value as NetworkMode })}
            aria-label={t("settings.networkMode")}
          >
            <option value="auto">{t("settings.modeAuto")}</option>
            <option value="tailscale">{t("settings.modeTailscale")}</option>
            <option value="relay">{t("settings.modeRelay")}</option>
          </select>
        </div>

        <div className="field">
          <label className="label" htmlFor="relay">
            {t("settings.relay")}
          </label>
          <input
            id="relay"
            className="input"
            placeholder={t("settings.relayPlaceholder")}
            value={s.relay_url ?? ""}
            onChange={(e) => setS({ ...s, relay_url: e.target.value || null })}
            onBlur={() => void save({ relay_url: s.relay_url })}
          />
          <p className="hint">{t("settings.relayHint")}</p>
        </div>

        <div className="field" style={{ marginBottom: 0 }}>
          <label className="label" htmlFor="ssh-port">
            {t("settings.sshPort")}
          </label>
          <input
            id="ssh-port"
            className="input"
            type="number"
            min={1}
            max={65535}
            value={s.ssh_port}
            onChange={(e) =>
              setS({
                ...s,
                ssh_port: e.target.value ? Number(e.target.value) : 0,
              })
            }
            onBlur={() => {
              if (s.ssh_port >= 1 && s.ssh_port <= 65535) {
                void save({ ssh_port: s.ssh_port });
              }
            }}
          />
          <p className="hint">{t("settings.sshPortHint")}</p>
        </div>
      </div>

      {showRelayGuide && <RelayGuideModal onClose={() => setShowRelayGuide(false)} />}

      {/* 诊断 */}
      <div className="card">
        <h2 className="card-title" style={{ marginBottom: 14 }}>
          诊断
        </h2>
        <div className="field">
          <label className="label" htmlFor="log-level">
            {t("settings.logLevel")}
          </label>
          <select
            id="log-level"
            className="select"
            value={s.log_level}
            onChange={(e) => void save({ log_level: e.target.value })}
          >
            {LOG_LEVELS.map((l) => (
              <option key={l} value={l}>
                {l}
              </option>
            ))}
          </select>
        </div>
        <div className="btn-row">
          <button
            className="btn"
            onClick={async () => {
              try {
                await api.openLogDir();
                setError(null);
              } catch (e) {
                setError(api.asCmdError(e).message);
              }
            }}
          >
            {t("settings.openLogDir")}
          </button>
          <button
            className="btn"
            onClick={async () => {
              try {
                const path = await api.exportDiagnostics();
                setDiagPath(path);
                await navigator.clipboard?.writeText(path).catch(() => {});
              } catch (e) {
                setError(api.asCmdError(e).message);
              }
            }}
          >
            导出诊断包
          </button>
        </div>
        {/* 检查更新（Basic 版本更新服务；后台每 6 小时也会自动检测） */}
        <div className="field" style={{ marginTop: 18 }}>
          <label className="label">检查更新</label>
          <div className="btn-row">
            <button
              className="btn"
              disabled={updateState === "checking" || installer.installing}
              onClick={async () => {
                setUpdateState("checking");
                setCheckError(null);
                setUpdate(null);
                try {
                  const u = await api.checkUpdate();
                  setUpdate(u);
                  setUpdateState(u.available ? "found" : "latest");
                } catch (e) {
                  setCheckError(api.asCmdError(e).message);
                  setUpdateState("error");
                }
              }}
            >
              {updateState === "checking" ? "检查中…" : "检查更新"}
            </button>
            {updateState === "found" && (
              <button
                className="btn btn-primary"
                disabled={installer.installing}
                onClick={installer.install}
              >
                {installer.installing ? "更新中…" : "下载并安装"}
              </button>
            )}
          </div>
          {updateState === "latest" && (
            <p className="hint" style={{ marginTop: 8 }}>
              {update?.disabled
                ? `最新版本 ${update.latest_version ?? ""} 已被发布方停用，暂无可用更新。`
                : `已是最新版本（当前 ${version}）。`}
            </p>
          )}
          {updateState === "found" && update && (
            <UpdateDetails
              update={update}
              installing={installer.installing}
              progress={installer.progress}
              error={installer.error}
            />
          )}
          {updateState === "error" && (
            <p
              className="hint"
              style={{ marginTop: 8, color: "var(--danger, #e5484d)" }}
            >
              检查更新失败：{checkError}
            </p>
          )}
        </div>
        {diagPath && (
          <p className="hint" style={{ marginTop: 10 }}>
            已导出（路径已复制到剪贴板）：<span className="mono">{diagPath}</span>
          </p>
        )}
        <p className="card-meta" style={{ marginTop: 12 }}>
          {t("settings.version")}: {version}
        </p>
      </div>
    </>
  );
}

/** 补丁 → 本地预览状态（乐观更新用）。 */
function patchToPreview(p: SettingsPatch): Partial<SettingsDto> {
  const out: Partial<SettingsDto> = {};
  if (p.device_name !== undefined) out.device_name = p.device_name;
  if (p.auto_start !== undefined) out.auto_start = p.auto_start;
  if (p.close_to_tray !== undefined) out.close_to_tray = p.close_to_tray;
  if (p.upload_limit_kbps !== undefined) out.upload_limit_kbps = p.upload_limit_kbps;
  if (p.download_limit_kbps !== undefined)
    out.download_limit_kbps = p.download_limit_kbps;
  if (p.network_mode !== undefined) out.network_mode = p.network_mode;
  if (p.log_level !== undefined) out.log_level = p.log_level;
  if (p.relay_url !== undefined) out.relay_url = p.relay_url;
  if (p.ssh_port !== undefined) out.ssh_port = p.ssh_port;
  return out;
}
