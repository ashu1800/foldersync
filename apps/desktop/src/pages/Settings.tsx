// 设置页（提示词 5.2.6）。

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { NetworkMode, SettingsDto, SettingsPatch } from "../lib/bindings";
import * as api from "../lib/tauri";
import { setLanguage } from "../i18n";
import { ErrorBanner, Loading } from "../components/ui";

const LOG_LEVELS = ["error", "warn", "info", "debug", "trace"];

export default function Settings() {
  const { t, i18n } = useTranslation();
  const [s, setS] = useState<SettingsDto | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [version, setVersion] = useState("");
  const [endpointId, setEndpointId] = useState("");
  const [diagPath, setDiagPath] = useState<string | null>(null);

  useEffect(() => {
    (async () => {
      try {
        const [settings, info] = await Promise.all([
          api.getSettings(),
          api.getLocalInfo(),
        ]);
        setS(settings);
        setVersion(String(info.version ?? ""));
        setEndpointId(String(info.endpoint_id ?? ""));
      } catch (e) {
        setError(api.asCmdError(e).message);
      } finally {
        setLoading(false);
      }
    })();
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
    } catch (e) {
      setS(prev);
      setError(api.asCmdError(e).message);
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
        <div className="card-meta mono truncate" title={endpointId}>
          {endpointId}
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
          {t("settings.networkMode")}
        </h2>
        <div className="field">
          <select
            className="select"
            value={s.network_mode}
            onChange={(e) => void save({ network_mode: e.target.value as NetworkMode })}
            aria-label={t("settings.networkMode")}
          >
            <option value="auto">{t("settings.modeAuto")}</option>
            <option value="prefer_tailscale">
              {t("settings.modePreferTailscale")}
            </option>
            <option value="tailscale_only">{t("settings.modeTailscaleOnly")}</option>
          </select>
        </div>

        <div className="field">
          <label className="label" htmlFor="relay">
            {t("settings.relay")}
          </label>
          <input
            id="relay"
            className="input"
            placeholder={t("settings.relayDefault")}
            value={s.relay_url ?? ""}
            onChange={(e) => setS({ ...s, relay_url: e.target.value || null })}
            onBlur={() => void save({ relay_url: s.relay_url })}
          />
          <p className="hint">留空使用默认中继（自动选择最近的节点）。</p>
        </div>

        <div className="field">
          <label className="label" htmlFor="port">
            {t("settings.listenPort")}
          </label>
          <input
            id="port"
            className="input"
            type="number"
            min={1}
            max={65535}
            placeholder={t("settings.listenPortHint")}
            value={s.listen_port ?? ""}
            onChange={(e) =>
              setS({
                ...s,
                listen_port: e.target.value ? Number(e.target.value) : null,
              })
            }
            onBlur={() => void save({ listen_port: s.listen_port })}
          />
        </div>
      </div>

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
                const dir = await api.getLogDir();
                await navigator.clipboard?.writeText(dir).catch(() => {});
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
  if (p.listen_port !== undefined) out.listen_port = p.listen_port;
  return out;
}
