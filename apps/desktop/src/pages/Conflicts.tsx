// 冲突页（提示词 5.2.5）：列出冲突副本 + 解决操作。

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { ConflictDto, ConflictResolution } from "../lib/bindings";
import * as api from "../lib/tauri";
import { useStore } from "../stores/app";
import {
  Empty,
  ErrorBanner,
  Loading,
  formatBytes,
  formatTime,
} from "../components/ui";

export default function Conflicts() {
  const { t } = useTranslation();
  const { conflicts, refreshConflicts } = useStore();
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  useEffect(() => {
    (async () => {
      setLoading(true);
      await refreshConflicts();
      setLoading(false);
    })();
  }, [refreshConflicts]);

  // 新冲突事件：直接刷列表（冲突不频繁，不用做局部更新）
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void api.on("conflict://new", () => {
      void refreshConflicts();
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, [refreshConflicts]);

  const resolve = useCallback(
    async (c: ConflictDto, resolution: ConflictResolution) => {
      const label = t(
        resolution === "keep_conflict"
          ? "conflicts.keepConflict"
          : resolution === "keep_original"
            ? "conflicts.keepOriginal"
            : "conflicts.keepBoth",
      );
      if (!window.confirm(`${label}？`)) return;
      setBusy(c.id);
      try {
        await api.resolveConflict(c.id, resolution);
        await refreshConflicts();
      } catch (e) {
        setError(api.asCmdError(e).message);
      } finally {
        setBusy(null);
      }
    },
    [refreshConflicts, t],
  );

  return (
    <>
      <div className="page-header">
        <h1 className="page-title">{t("conflicts.title")}</h1>
      </div>

      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

      {loading && conflicts.length === 0 ? (
        <Loading />
      ) : conflicts.length === 0 ? (
        <div className="card">
          <Empty icon="✓" title={t("conflicts.empty")} hint="冲突文件会自动保留副本，不会丢失任何一版内容" />
        </div>
      ) : (
        <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
          {conflicts.map((c) => (
            <div className="card" key={c.id}>
              <div
                style={{
                  display: "flex",
                  alignItems: "flex-start",
                  justifyContent: "space-between",
                  gap: 12,
                }}
              >
                <div style={{ minWidth: 0 }}>
                  <div className="card-title truncate" title={c.original_path}>
                    {c.original_path}
                  </div>
                  <div className="card-meta" style={{ marginTop: 4 }}>
                    {t("conflicts.source")}: <strong>{c.source_device || "—"}</strong>
                  </div>
                </div>
                <span className="badge badge-warn">⚠ {t("activity.conflict")}</span>
              </div>

              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "1fr 1fr",
                  gap: 12,
                  marginTop: 14,
                }}
              >
                <div>
                  <div className="label" style={{ marginBottom: 6 }}>
                    {t("conflicts.original")}
                  </div>
                  <div className="card-meta">
                    {t("conflicts.size")}: {c.original_size != null ? formatBytes(c.original_size) : "—"}
                    <br />
                    {t("conflicts.mtime")}: {formatTime(c.original_mtime)}
                  </div>
                </div>
                <div>
                  <div className="label" style={{ marginBottom: 6 }}>
                    {t("conflicts.conflictCopy")}
                  </div>
                  <div className="card-meta">
                    {t("conflicts.size")}: {formatBytes(c.conflict_size)}
                    <br />
                    {t("conflicts.mtime")}: {formatTime(c.conflict_mtime)}
                  </div>
                </div>
              </div>

              <div className="btn-row" style={{ marginTop: 14 }}>
                <button
                  className="btn btn-sm"
                  onClick={() => void resolve(c, "keep_conflict")}
                  disabled={busy === c.id}
                >
                  {t("conflicts.keepConflict")}
                </button>
                <button
                  className="btn btn-sm"
                  onClick={() => void resolve(c, "keep_original")}
                  disabled={busy === c.id}
                >
                  {t("conflicts.keepOriginal")}
                </button>
                <button
                  className="btn btn-sm"
                  onClick={() => void resolve(c, "keep_both")}
                  disabled={busy === c.id}
                >
                  {t("conflicts.keepBoth")}
                </button>
              </div>
            </div>
          ))}
        </div>
      )}
    </>
  );
}
