// 概览页（提示词 5.2.1）：总状态 + 设备卡片 + 文件夹卡片。

import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import { useStore } from "../stores/app";
import {
  ConnBadge,
  Empty,
  ErrorBanner,
  FolderBadge,
  Loading,
  OverallBadge,
  formatTime,
} from "../components/ui";

export default function Dashboard() {
  const { t } = useTranslation();
  const {
    overview,
    devices,
    folders,
    loading,
    error,
    refreshAll,
  } = useStore();

  useEffect(() => {
    void refreshAll();
  }, [refreshAll]);

  if (!overview && loading) return <Loading />;

  return (
    <>
      <div className="page-header">
        <div>
          <h1 className="page-title">{t("overview.title")}</h1>
          <div className="page-subtitle">
            {overview?.device_name} · <span className="mono">{overview?.fingerprint}</span>
          </div>
        </div>
        {overview && <OverallBadge status={overview.status} />}
      </div>

      {error && (
        <ErrorBanner message={error} onRetry={() => void refreshAll()} />
      )}

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
      {devices.length === 0 ? (
        <div className="card">
          <Empty
            icon="◍"
            title={t("overview.noDevices")}
            action={
              <Link className="btn btn-primary" to="/devices">
                {t("devices.add")}
              </Link>
            }
          />
        </div>
      ) : (
        <div className="card-grid">
          {devices.map((d) => (
            <div className="card" key={d.id}>
              <div className="card-title" style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <span
                  className={`dot ${d.online ? "dot-success" : "dot-muted"}`}
                  aria-hidden
                />
                {d.name}
              </div>
              <div className="card-meta" style={{ marginBottom: 8 }}>
                <span className="mono">{d.fingerprint}</span>
              </div>
              <ConnBadge conn={d.conn} rtt={d.rtt_ms} />
            </div>
          ))}
        </div>
      )}

      <h2
        className="card-title"
        style={{ fontSize: 15, margin: "24px 0 10px" }}
      >
        {t("overview.folders")}
      </h2>
      {folders.length === 0 ? (
        <div className="card">
          <Empty
            icon="▤"
            title={t("overview.noFolders")}
            action={
              <Link className="btn btn-primary" to="/folders">
                {t("folders.add")}
              </Link>
            }
          />
        </div>
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
