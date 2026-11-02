// 冲突页（提示词 5.2.5）：列出冲突副本 + 解决操作。

import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type {
  ConflictDto,
  ConflictPreviewDto,
  ConflictResolution,
} from "../lib/bindings";
import * as api from "../lib/tauri";
import { useStore } from "../stores/app";
import {
  ConfirmDialog,
  Empty,
  ErrorBanner,
  Icon,
  Loading,
  Modal,
  formatBytes,
  formatTime,
} from "../components/ui";

export default function Conflicts() {
  const { t } = useTranslation();
  const { conflicts, folders, refreshConflicts, conflictsError } = useStore();
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [preview, setPreview] = useState<ConflictPreviewDto | null>(null);
  const [previewSource, setPreviewSource] = useState<ConflictDto | null>(null);
  const [previewLabel, setPreviewLabel] = useState("");
  const [previewLoading, setPreviewLoading] = useState<string | null>(null);
  const [previewLoadingMore, setPreviewLoadingMore] = useState(false);
  const previewGeneration = useRef(0);
  const [confirmAction, setConfirmAction] = useState<{
    c: ConflictDto;
    resolution: ConflictResolution;
    label: string;
  } | null>(null);

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
    void api
      .on("conflict://new", () => {
        void refreshConflicts();
      })
      .then((fn) => {
        unlisten = fn;
      });
    return () => unlisten?.();
  }, [refreshConflicts]);

  const openPreview = useCallback(async (c: ConflictDto) => {
    const gen = ++previewGeneration.current;
    setPreviewLoading(c.id);
    setPreviewLabel(c.original_path);
    setPreviewSource(c);
    try {
      const data = await api.getConflictPreview(
        c.folder_id,
        c.original_path,
        c.conflict_path,
      );
      if (gen !== previewGeneration.current) return;
      setPreview(data);
    } catch (e) {
      if (gen === previewGeneration.current) setError(e);
    } finally {
      if (gen === previewGeneration.current) setPreviewLoading(null);
    }
  }, []);

  const closePreview = () => {
    previewGeneration.current += 1;
    setPreview(null);
    setPreviewSource(null);
    setPreviewLoading(null);
    setPreviewLoadingMore(false);
  };

  const loadMorePreview = useCallback(async () => {
    if (!preview || !previewSource || !preview.truncated || previewLoadingMore) return;
    const gen = previewGeneration.current;
    setPreviewLoadingMore(true);
    try {
      const data = await api.getConflictPreviewPage(
        previewSource.folder_id,
        previewSource.original_path,
        previewSource.conflict_path,
        preview.next_offset,
      );
      if (gen !== previewGeneration.current) return;
      setPreview((current) =>
        current
          ? {
              ...current,
              original:
                data.original == null ? null : (current.original ?? "") + data.original,
              conflict: current.conflict + data.conflict,
              truncated: data.truncated,
              binary: data.binary,
              next_offset: data.next_offset,
            }
          : data,
      );
    } catch (e) {
      if (gen === previewGeneration.current) setError(e);
    } finally {
      if (gen === previewGeneration.current) setPreviewLoadingMore(false);
    }
  }, [preview, previewSource, previewLoadingMore]);

  const resolve = useCallback(
    async (c: ConflictDto, resolution: ConflictResolution) => {
      setBusy(c.id);
      try {
        await api.resolveConflict(c.id, resolution, c.folder_id);
        setConfirmAction(null);
        await refreshConflicts();
      } catch (e) {
        setError(e);
      } finally {
        setBusy(null);
      }
    },
    [refreshConflicts],
  );

  return (
    <>
      <div className="page-header">
        <h1 className="page-title">{t("conflicts.title")}</h1>
      </div>

      {(error ?? conflictsError) && (
        <ErrorBanner
          error={error ?? conflictsError}
          onRetry={() => void refreshConflicts()}
          onDismiss={() => setError(null)}
        />
      )}

      {loading && conflicts.length === 0 ? (
        <Loading />
      ) : conflicts.length === 0 ? (
        <div className="card">
          <Empty
            icon={<Icon name="check" size={30} />}
            title={t("conflicts.empty")}
            hint={t("conflicts.emptyHint")}
          />
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
                    {t("conflicts.folder")}:{" "}
                    <strong>
                      {folders.find((f) => f.id === c.folder_id)?.name ?? c.folder_id}
                    </strong>
                    {" · "}
                    {t("conflicts.source")}: <strong>{c.source_device || "—"}</strong>
                  </div>
                </div>
                <span className="badge badge-warn">
                  <Icon name="conflict" size={13} /> {t("activity.conflict")}
                </span>
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
                    {t("conflicts.size")}:{" "}
                    {c.original_size != null ? formatBytes(c.original_size) : "—"}
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
                  className="btn btn-sm btn-ghost"
                  disabled={previewLoading === c.id}
                  onClick={() => void openPreview(c)}
                >
                  {previewLoading === c.id
                    ? t("conflicts.loadingPreview")
                    : t("conflicts.preview")}
                </button>
                <button
                  className="btn btn-sm btn-ghost"
                  disabled={c.original_size == null}
                  onClick={() =>
                    void api
                      .openFile(c.folder_id, c.original_path)
                      .catch((e) => setError(e))
                  }
                >
                  {t("conflicts.openOriginal")}
                </button>
                <button
                  className="btn btn-sm btn-ghost"
                  onClick={() =>
                    void api
                      .openFile(c.folder_id, c.conflict_path)
                      .catch((e) => setError(e))
                  }
                >
                  {t("conflicts.openCopy")}
                </button>
                <button
                  className="btn btn-sm btn-ghost"
                  onClick={() =>
                    void api
                      .openFolderItem(c.folder_id, c.conflict_path)
                      .catch((e) => setError(e))
                  }
                >
                  {t("conflicts.openDir")}
                </button>
              </div>
              <div className="btn-row" style={{ marginTop: 8 }}>
                <button
                  className="btn btn-sm"
                  title={t("conflicts.keepConflictHint")}
                  onClick={() =>
                    setConfirmAction({
                      c,
                      resolution: "keep_conflict",
                      label: t("conflicts.keepConflict"),
                    })
                  }
                  disabled={busy === c.id}
                >
                  {t("conflicts.keepConflict")}
                </button>
                <button
                  className="btn btn-sm"
                  title={t("conflicts.keepOriginalHint")}
                  onClick={() =>
                    setConfirmAction({
                      c,
                      resolution: "keep_original",
                      label: t("conflicts.keepOriginal"),
                    })
                  }
                  disabled={busy === c.id}
                >
                  {t("conflicts.keepOriginal")}
                </button>
                <button
                  className="btn btn-sm"
                  title={t("conflicts.keepBothHint")}
                  onClick={() =>
                    setConfirmAction({
                      c,
                      resolution: "keep_both",
                      label: t("conflicts.keepBoth"),
                    })
                  }
                  disabled={busy === c.id}
                >
                  {t("conflicts.keepBoth")}
                </button>
              </div>
            </div>
          ))}
        </div>
      )}

      {preview && (
        <Modal
          title={t("conflicts.previewTitle")}
          width={920}
          onClose={closePreview}
          footer={
            <div className="btn-row">
              {preview.truncated && !preview.binary && (
                <button
                  className="btn"
                  disabled={previewLoadingMore}
                  onClick={() => void loadMorePreview()}
                >
                  {previewLoadingMore
                    ? t("conflicts.loadingPreview")
                    : t("conflicts.loadMorePreview")}
                </button>
              )}
              <button className="btn" onClick={closePreview}>
                {t("common.close")}
              </button>
            </div>
          }
        >
          <div className="card-meta" style={{ marginBottom: 10 }}>
            {previewLabel}
          </div>
          <div className="conflict-preview">
            {preview.binary ? (
              <div className="notice" style={{ gridColumn: "1 / -1" }}>
                {t("conflicts.binaryPreview")}
              </div>
            ) : (
              <>
                <div>
                  <div className="label" style={{ marginBottom: 6 }}>
                    {t("conflicts.original")}
                  </div>
                  <pre>{preview.original ?? t("conflicts.originalMissing")}</pre>
                </div>
                <div>
                  <div className="label" style={{ marginBottom: 6 }}>
                    {t("conflicts.conflictCopy")}
                  </div>
                  <pre>{preview.conflict}</pre>
                </div>
              </>
            )}
          </div>
          {preview.truncated && (
            <div className="card-meta" style={{ marginTop: 10 }}>
              {t("conflicts.previewTruncated")}
            </div>
          )}
        </Modal>
      )}

      {confirmAction && (
        <ConfirmDialog
          title={confirmAction.label}
          message={t(
            confirmAction.resolution === "keep_conflict"
              ? "conflicts.keepConflictHint"
              : confirmAction.resolution === "keep_original"
                ? "conflicts.keepOriginalHint"
                : "conflicts.keepBothHint",
          )}
          confirmLabel={confirmAction.label}
          danger={confirmAction.resolution !== "keep_both"}
          busy={busy === confirmAction.c.id}
          onConfirm={() => void resolve(confirmAction.c, confirmAction.resolution)}
          onCancel={() => setConfirmAction(null)}
        />
      )}
    </>
  );
}
