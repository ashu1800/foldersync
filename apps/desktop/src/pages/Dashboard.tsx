// 概览页（提示词 5.2.1）：总状态 + 设备卡片 + 文件夹卡片。

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import type { EnvStatusDto } from "../lib/bindings";
import * as api from "../lib/tauri";
import { useStore } from "../stores/app";
import {
  ConnBadge,
  Empty,
  ErrorBanner,
  FolderBadge,
  Icon,
  Loading,
  OnlineBadge,
  OverallBadge,
  RelayGuideModal,
  formatTime,
  shortFingerprint,
} from "../components/ui";

export default function Dashboard() {
  const { t, i18n } = useTranslation();
  const {
    overview,
    overviewLoaded,
    devices,
    devicesLoaded,
    folders,
    foldersLoaded,
    overviewError,
    devicesError,
    foldersError,
    refreshAll,
  } = useStore();
  const [env, setEnv] = useState<EnvStatusDto | null>(null);
  const [envLoading, setEnvLoading] = useState(false);
  const [envError, setEnvError] = useState<string | null>(null);
  const [showRelayGuide, setShowRelayGuide] = useState(false);

  const refreshEnv = useCallback(async () => {
    try {
      const next = await api.getEnvironmentStatus();
      setEnv(next);
      setEnvError(null);
    } catch (e) {
      setEnvError(api.asCmdError(e).message);
    }
  }, []);

  const checkEnv = async () => {
    setEnvLoading(true);
    await refreshEnv();
    setEnvLoading(false);
  };

  useEffect(() => {
    void refreshAll();
  }, [refreshAll]);

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

  const installTailscale = async () => {
    try {
      await api.openTailscaleDownload();
      setEnvError(null);
    } catch (e) {
      setEnvError(api.asCmdError(e).message);
    }
  };

  if (!overview && !overviewLoaded) return <Loading />;

  return (
    <>
      <div className="page-header">
        <div>
          <h1 className="page-title">{t("overview.title")}</h1>
          <div className="page-subtitle truncate" title={overview?.fingerprint}>
            {overview?.device_name} ·{" "}
            <span className="mono">
              {overview?.fingerprint ? shortFingerprint(overview.fingerprint) : ""}
            </span>
          </div>
        </div>
        {overview && <OverallBadge status={overview.status} />}
      </div>

      {(overviewError || devicesError || foldersError) && (
        <ErrorBanner
          message={overviewError ?? devicesError ?? foldersError ?? ""}
          onRetry={() => void refreshAll()}
        />
      )}

      {overview?.status === "syncing" && (
        <div
          className="sync-progress"
          role="progressbar"
          aria-label={t("overview.syncingProgress", {
            syncing: overview.syncing_files,
            remaining: overview.remaining_files,
          })}
        >
          <span />
        </div>
      )}

      {env && !env.ready && (
        <div className="notice" role="status" style={{ flexWrap: "wrap" }}>
          <div className="notice-content">
            <strong>{t("overview.envTitle")}</strong>
            <div className="card-meta" style={{ marginTop: 4 }}>
              {t("overview.envHint")}
            </div>
            {i18n.language.startsWith("zh") &&
              env.notes.slice(0, 2).map((n) => (
                <div className="card-meta" key={n}>
                  {n}
                </div>
              ))}
          </div>
          <div className="btn-row">
            {!env.tailscale_installed && (
              <button
                className="btn btn-primary"
                onClick={() => void installTailscale()}
                aria-label={t("overview.envInstallTailscale")}
              >
                {t("overview.envInstallTailscale")}
              </button>
            )}
            <button
              className="btn"
              onClick={() => setShowRelayGuide(true)}
              aria-label={t("overview.envSetupRelay")}
            >
              {t("overview.envSetupRelay")}
            </button>
            <button
              className="btn btn-sm btn-ghost"
              onClick={() => void checkEnv()}
              disabled={envLoading}
              aria-label={t("overview.envRefresh")}
            >
              {envLoading ? t("overview.envChecking") : t("overview.envRefresh")}
            </button>
          </div>
        </div>
      )}
      {envError && (
        <ErrorBanner message={envError} onDismiss={() => setEnvError(null)} />
      )}
      {showRelayGuide && <RelayGuideModal onClose={() => setShowRelayGuide(false)} />}

      <div className="stat-row">
        <div className="stat">
          <div className="stat-value">{overview?.online_devices ?? 0}</div>
          <div className="stat-label">{t("overview.onlineDevices")}</div>
        </div>
        <div className="stat">
          <div className="stat-value">{folders.length}</div>
          <div className="stat-label">{t("overview.folders")}</div>
        </div>
        <div className="stat">
          <div className="stat-value">{overview?.remaining_files ?? 0}</div>
          <div className="stat-label">{t("overview.remaining")}</div>
        </div>
        {overview?.tailscale && (
          <div className="stat">
            <div className="stat-value" style={{ fontSize: 13, paddingTop: 6 }}>
              {overview.tailscale}
            </div>
            <div className="stat-label">{t("overview.tailscale")}</div>
          </div>
        )}
      </div>

      <h2 className="card-title" style={{ fontSize: 15, marginBottom: 10 }}>
        {t("overview.devices")}
      </h2>
      {devicesLoaded && devices.length === 0 ? (
        <div className="card">
          <Empty
            icon={<Icon name="device" size={30} />}
            title={t("overview.noDevices")}
            action={
              <Link className="btn btn-primary" to="/devices">
                {t("devices.add")}
              </Link>
            }
          />
        </div>
      ) : !devicesLoaded ? (
        <Loading />
      ) : (
        <div className="card-grid">
          {devices.map((d) => (
            <div className="card" key={d.id}>
              <div
                className="card-title"
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: 8,
                }}
              >
                <span className="truncate">{d.name}</span>
                <OnlineBadge online={d.online} />
              </div>
              <div
                className="card-meta truncate"
                style={{ marginBottom: 8 }}
                title={d.fingerprint}
              >
                <span className="mono">{shortFingerprint(d.fingerprint)}</span>
              </div>
              <ConnBadge conn={d.conn} rtt={d.rtt_ms} />
            </div>
          ))}
        </div>
      )}

      <h2 className="card-title" style={{ fontSize: 15, margin: "24px 0 10px" }}>
        {t("overview.folders")}
      </h2>
      {foldersLoaded && folders.length === 0 ? (
        <div className="card">
          <Empty
            icon={<Icon name="folder" size={30} />}
            title={t("overview.noFolders")}
            action={
              <Link className="btn btn-primary" to="/folders">
                {t("folders.add")}
              </Link>
            }
          />
        </div>
      ) : !foldersLoaded ? (
        <Loading />
      ) : (
        <div className="card-grid">
          {folders.map((f) => (
            <div className="card" key={f.id}>
              <div
                className="card-title"
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: 8,
                }}
              >
                <span className="truncate">{f.name}</span>
                <FolderBadge status={f.status} />
              </div>
              <div className="card-meta truncate" title={f.path}>
                {f.path}
              </div>
              <div className="card-meta" style={{ marginTop: 8 }}>
                {t("folders.files")}: {f.file_count} · {t("folders.lastSync")}:{" "}
                {formatTime(f.last_sync_at)}
              </div>
            </div>
          ))}
        </div>
      )}
    </>
  );
}
