// 活动页（提示词 5.2.4）：实时事件流 + 过滤 + 暂停滚动。

import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { ActivityDto, ActivityKind } from "../lib/bindings";
import * as api from "../lib/tauri";
import { useStore } from "../stores/app";
import {
  Empty,
  ErrorBanner,
  Icon,
  Loading,
  formatBytes,
  formatTime,
} from "../components/ui";

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
  const [buffered, setBuffered] = useState<ActivityDto[]>([]);
  const [hidden, setHidden] = useState(false);
  const [hasMore, setHasMore] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const listRef = useRef<HTMLDivElement>(null);
  const itemsRef = useRef<ActivityDto[]>(items);
  const filterGeneration = useRef(0);
  useEffect(() => {
    itemsRef.current = items;
  }, [items]);

  const mergeRows = useCallback((prev: ActivityDto[], latest: ActivityDto[]) => {
    const byId = new Map<number, ActivityDto>();
    for (const a of [...latest, ...prev]) {
      if (!byId.has(a.id)) byId.set(a.id, a);
    }
    return [...byId.values()].sort((a, b) => b.id - a.id).slice(0, 1000);
  }, []);

  const loadMore = useCallback(async () => {
    const gen = filterGeneration.current;
    const oldest = items[items.length - 1]?.id;
    if (oldest == null || loadingMore) return;
    setLoadingMore(true);
    try {
      const rows = await api.listActivity({
        folder_id: folderFilter || undefined,
        kind: kindFilter || undefined,
        limit: 300,
        before_id: oldest,
      });
      if (gen !== filterGeneration.current) return;
      setItems((prev) => mergeRows(prev, rows));
      setHasMore(rows.length >= 300);
    } catch (e) {
      if (gen === filterGeneration.current) setError(api.asCmdError(e).message);
    } finally {
      if (gen === filterGeneration.current) setLoadingMore(false);
    }
  }, [items, loadingMore, folderFilter, kindFilter, mergeRows]);

  const load = useCallback(
    async (showSpinner = false) => {
      const gen = filterGeneration.current;
      if (showSpinner) setLoading(true);
      try {
        const rows = await api.listActivity({
          folder_id: folderFilter || undefined,
          kind: kindFilter || undefined,
          limit: 300,
        });
        if (gen !== filterGeneration.current) return;
        setItems(rows);
        setError(null);
      } catch (e) {
        if (gen === filterGeneration.current) setError(api.asCmdError(e).message);
      } finally {
        if (gen === filterGeneration.current) setLoading(false);
      }
    },
    [folderFilter, kindFilter],
  );

  // 首次加载与切换过滤条件。
  // 请求写在 effect 内部而不是调用上面的 load()：一来避免在自己身上触发
  // setState 级联渲染告警，二来 cancelled 标记能防住"旧请求后到、覆盖新结果"。
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const rows = await api.listActivity({
          folder_id: folderFilter || undefined,
          kind: kindFilter || undefined,
          limit: 300,
        });
        if (!cancelled) {
          setItems(rows);
          setHasMore(rows.length >= 300);
          setError(null);
        }
      } catch (e) {
        if (!cancelled) setError(api.asCmdError(e).message);
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [folderFilter, kindFilter]);

  // 后台轮询：后端目前只持久化活动（没有推送事件），2.5 秒拉一次足够实时，
  // 页面不可见时暂停请求，回来立刻补一次。
  useEffect(() => {
    const tick = async () => {
      if (document.hidden) return;
      const gen = filterGeneration.current;
      try {
        const rows = await api.listActivity({
          folder_id: folderFilter || undefined,
          kind: kindFilter || undefined,
          limit: 300,
        });
        if (gen !== filterGeneration.current) return;
        if (pausedScroll) {
          // 暂停滚动时只攒比当前顶部更新的事件，不改变用户当前阅读位置
          const newestShown = itemsRef.current[0]?.id ?? 0;
          const fresh = rows.filter((a) => a.id > newestShown);
          if (fresh.length > 0) {
            setBuffered((prev) => mergeRows(prev, fresh));
          }
          return;
        }
        setItems((prev) => mergeRows(prev, rows));
        setError(null);
      } catch {
        // 轮询失败不打断阅读，下一次自动重试
      }
    };
    const id = window.setInterval(() => void tick(), 2500);
    const onVisible = () => {
      if (!document.hidden) void tick();
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      window.clearInterval(id);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [folderFilter, kindFilter, mergeRows, pausedScroll]);

  const togglePauseScroll = () => {
    if (!pausedScroll) {
      setPausedScroll(true);
      return;
    }
    if (buffered.length > 0) {
      setItems((prev) => mergeRows(prev, buffered));
      setBuffered([]);
    }
    setPausedScroll(false);
    window.setTimeout(() => listRef.current?.scrollTo({ top: 0 }), 0);
  };

  // 新活动事件：追加到顶部（暂停滚动时不打断用户位置）
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void api
      .on("sync://activity", (a) => {
        if (pausedScroll) {
          setBuffered((prev) => mergeRows(prev, [a]));
          return;
        }
        setItems((prev) => [a, ...prev].slice(0, 500));
        if (listRef.current) {
          listRef.current.scrollTop = 0;
        }
      })
      .then((fn) => {
        unlisten = fn;
      });
    return () => unlisten?.();
  }, [pausedScroll, mergeRows]);

  const visible = hidden ? [] : items;

  return (
    <>
      <div className="page-header">
        <h1 className="page-title">{t("activity.title")}</h1>
        <div className="btn-row">
          <button className="btn btn-sm" onClick={togglePauseScroll}>
            {pausedScroll && buffered.length > 0
              ? t("activity.newWhilePaused", { count: buffered.length })
              : pausedScroll
                ? t("activity.resumeScroll")
                : t("activity.pauseScroll")}
          </button>
          <button className="btn btn-sm" onClick={() => setHidden(true)}>
            {t("activity.clear")}
          </button>
        </div>
      </div>

      {error && <ErrorBanner message={error} onRetry={() => void load()} />}

      <div className="btn-row" style={{ marginBottom: 14 }}>
        <label
          className="card-meta"
          style={{ display: "flex", alignItems: "center", gap: 6 }}
        >
          <span>{t("activity.filterFolder")}</span>
          <select
            className="select"
            style={{ width: 200 }}
            value={folderFilter}
            onChange={(e) => {
              filterGeneration.current += 1;
              setLoadingMore(false);
              setFolderFilter(e.target.value);
              setBuffered([]);
            }}
            aria-label={t("activity.filterFolder")}
          >
            <option value="">{t("activity.filterAll")}</option>
            {folders.map((f) => (
              <option key={f.id} value={f.id}>
                {f.name}
              </option>
            ))}
          </select>
        </label>
        <label
          className="card-meta"
          style={{ display: "flex", alignItems: "center", gap: 6 }}
        >
          <span>{t("activity.filterKind")}</span>
          <select
            className="select"
            style={{ width: 160 }}
            value={kindFilter}
            onChange={(e) => {
              filterGeneration.current += 1;
              setLoadingMore(false);
              setKindFilter(e.target.value as ActivityKind | "");
              setBuffered([]);
            }}
            aria-label={t("activity.filterKind")}
          >
            <option value="">{t("activity.filterAll")}</option>
            {KINDS.map((k) => (
              <option key={k} value={k}>
                {t(`activity.${k}`)}
              </option>
            ))}
          </select>
        </label>
      </div>

      {loading && items.length === 0 ? (
        <Loading />
      ) : visible.length === 0 ? (
        <div className="card">
          <Empty
            icon={<Icon name="clock" size={30} />}
            title={t("activity.empty")}
            hint={hidden ? t("activity.hiddenHint") : undefined}
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
          className="card table-scroll"
          style={{ padding: 0, maxHeight: "calc(100vh - 240px)", overflowY: "auto" }}
          ref={listRef}
        >
          <table className="table">
            <thead>
              <tr>
                <th style={{ width: 132 }}>{t("activity.colTime")}</th>
                <th style={{ width: 66 }}>{t("activity.colKind")}</th>
                <th>{t("activity.colFile")}</th>
                <th style={{ width: 72 }}>{t("activity.colSize")}</th>
                <th style={{ width: 150 }}>{t("activity.colMessage")}</th>
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
                  <td className="card-meta">
                    {a.size > 0 ? formatBytes(a.size) : "—"}
                  </td>
                  <td className="card-meta truncate" title={a.message}>
                    {a.message}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {hasMore && (
            <div className="load-more">
              <button
                className="btn btn-sm"
                disabled={loadingMore}
                onClick={() => void loadMore()}
              >
                {loadingMore ? t("activity.loadingMore") : t("activity.loadMore")}
              </button>
            </div>
          )}
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
