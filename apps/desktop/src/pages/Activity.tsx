// 活动页（提示词 5.2.4）：实时事件流 + 过滤 + 暂停滚动。

import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { ActivityDto, ActivityKind } from "../lib/bindings";
import * as api from "../lib/tauri";
import { useStore } from "../stores/app";
import { Empty, ErrorBanner, Loading, formatTime } from "../components/ui";

const KINDS: ActivityKind[] = [
  "upload",
  "download",
  "delete",
  "conflict",
  "error",
  "info",
];

export default function Activity() {
  const { t } = useTranslation();
  const { folders } = useStore();

  const [items, setItems] = useState<ActivityDto[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [folderFilter, setFolderFilter] = useState<string>("");
  const [kindFilter, setKindFilter] = useState<ActivityKind | "">("");
  const [pausedScroll, setPausedScroll] = useState(false);
  const [hidden, setHidden] = useState(false);
  const listRef = useRef<HTMLDivElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const rows = await api.listActivity({
        folder_id: folderFilter || undefined,
        kind: kindFilter || undefined,
        limit: 300,
      });
      setItems(rows);
      setError(null);
    } catch (e) {
      setError(api.asCmdError(e).message);
    } finally {
      setLoading(false);
    }
  }, [folderFilter, kindFilter]);

  useEffect(() => {
    void load();
  }, [load]);

  // 新活动事件：追加到顶部（暂停滚动时不打断用户位置）
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void api
      .on("sync://activity", (a) => {
        setItems((prev) => [a, ...prev].slice(0, 500));
        if (!pausedScroll && listRef.current) {
          listRef.current.scrollTop = 0;
        }
      })
      .then((fn) => {
        unlisten = fn;
      });
    return () => unlisten?.();
  }, [pausedScroll]);

  const visible = hidden ? [] : items;

  return (
    <>
      <div className="page-header">
        <h1 className="page-title">{t("activity.title")}</h1>
        <div className="btn-row">
          <button
            className="btn btn-sm"
            onClick={() => setPausedScroll((v) => !v)}
          >
            {pausedScroll ? t("activity.resumeScroll") : t("activity.pauseScroll")}
          </button>
          <button className="btn btn-sm" onClick={() => setHidden(true)}>
            {t("activity.clear")}
          </button>
        </div>
      </div>

      {error && <ErrorBanner message={error} onRetry={() => void load()} />}

      <div className="btn-row" style={{ marginBottom: 14 }}>
        <select
          className="select"
          style={{ width: 200 }}
          value={folderFilter}
          onChange={(e) => setFolderFilter(e.target.value)}
          aria-label={t("activity.filterFolder")}
        >
          <option value="">{t("activity.filterAll")}</option>
          {folders.map((f) => (
            <option key={f.id} value={f.id}>
              {f.name}
            </option>
          ))}
        </select>
        <select
          className="select"
          style={{ width: 160 }}
          value={kindFilter}
          onChange={(e) => setKindFilter(e.target.value as ActivityKind | "")}
          aria-label={t("activity.filterKind")}
        >
          <option value="">{t("activity.filterAll")}</option>
          {KINDS.map((k) => (
            <option key={k} value={k}>
              {t(`activity.${k}`)}
            </option>
          ))}
        </select>
      </div>

      {loading && items.length === 0 ? (
        <Loading />
      ) : visible.length === 0 ? (
        <div className="card">
          <Empty
            icon="◷"
            title={t("activity.empty")}
            hint={hidden ? "已清空显示，刷新页面可恢复从数据库读取的记录" : undefined}
            action={
              hidden ? (
                <button
                  className="btn"
                  onClick={() => {
                    setHidden(false);
                    void load();
                  }}
                >
                  {t("common.retry")}
                </button>
              ) : undefined
            }
          />
        </div>
      ) : (
        <div
          className="card"
          style={{ padding: 0, maxHeight: "calc(100vh - 240px)", overflowY: "auto" }}
          ref={listRef}
        >
          <table className="table">
            <thead>
              <tr>
                <th style={{ width: 160 }}>时间</th>
                <th style={{ width: 80 }}>类型</th>
                <th>文件</th>
                <th style={{ width: 90 }}>大小</th>
                <th style={{ width: 200 }}>说明</th>
              </tr>
            </thead>
            <tbody>
              {visible.map((a) => (
                <tr key={a.id}>
                  <td className="card-meta">{formatTime(a.at)}</td>
                  <td>
                    <KindBadge kind={a.kind} label={t(`activity.${a.kind}`)} />
                  </td>
                  <td>
                    <span className="mono truncate" title={a.path}>
                      {a.path || "—"}
                    </span>
                  </td>
                  <td className="card-meta">{a.size || "—"}</td>
                  <td className="card-meta truncate" title={a.message}>
                    {a.message}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

function KindBadge({ kind, label }: { kind: ActivityKind; label: string }) {
  const cls: Record<ActivityKind, string> = {
    upload: "badge-muted",
    download: "badge-muted",
    delete: "badge-warn",
    conflict: "badge-warn",
    error: "badge-danger",
    info: "badge-muted",
  };
  return <span className={`badge ${cls[kind]}`}>{label}</span>;
}
