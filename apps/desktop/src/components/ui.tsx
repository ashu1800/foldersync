// 共享 UI 组件：小、无依赖、各页面复用。

import { useEffect, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import type { ConnKind, FolderStatus, OverallStatus } from "../lib/bindings";
import * as api from "../lib/tauri";

// ---------- 状态占位 ----------

export function Loading({ text }: { text?: string }) {
  const { t } = useTranslation();
  return (
    <div className="loading">
      <div className="spinner" />
      <span>{text ?? t("app.loading")}</span>
    </div>
  );
}

export function Empty({
  icon = "○",
  title,
  hint,
  action,
}: {
  icon?: string;
  title: string;
  hint?: string;
  action?: ReactNode;
}) {
  return (
    <div className="empty">
      <div className="empty-icon">{icon}</div>
      <div className="empty-title">{title}</div>
      {hint && <div>{hint}</div>}
      {action && <div style={{ marginTop: 16 }}>{action}</div>}
    </div>
  );
}

export function ErrorBanner({
  message,
  onRetry,
  onDismiss,
}: {
  message: string;
  onRetry?: () => void;
  onDismiss?: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="error-banner" role="alert">
      <span>{message}</span>
      <span className="btn-row">
        {onRetry && (
          <button className="btn btn-sm" onClick={onRetry}>
            {t("common.retry")}
          </button>
        )}
        {onDismiss && (
          <button className="btn btn-sm btn-ghost" onClick={onDismiss}>
            {t("common.close")}
          </button>
        )}
      </span>
    </div>
  );
}

// ---------- 对话框 ----------

export function Modal({
  title,
  children,
  footer,
  onClose,
  width,
}: {
  title: string;
  children: ReactNode;
  footer?: ReactNode;
  onClose: () => void;
  width?: number;
}) {
  // Esc 关闭：键盘用户的基本可访问性
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="modal-backdrop" onClick={onClose} role="presentation">
      <div
        className="modal"
        style={width ? { maxWidth: width } : undefined}
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label={title}
      >
        <div className="modal-header">
          <h2 className="modal-title">{title}</h2>
          <button className="btn btn-sm btn-ghost" onClick={onClose} aria-label="close">
            ✕
          </button>
        </div>
        <div className="modal-body">{children}</div>
        {footer && <div className="modal-footer">{footer}</div>}
      </div>
    </div>
  );
}

// ---------- 状态徽标 ----------

export function OverallBadge({ status }: { status: OverallStatus }) {
  const { t } = useTranslation();
  const map: Record<OverallStatus, { cls: string; dot: string }> = {
    synced: { cls: "badge-success", dot: "dot-success" },
    syncing: { cls: "badge-muted", dot: "dot-warn" },
    error: { cls: "badge-danger", dot: "dot-danger" },
    paused: { cls: "badge-warn", dot: "dot-muted" },
  };
  const m = map[status];
  return (
    <span className={`badge ${m.cls}`}>
      <span className={`dot ${m.dot}`} />
      {t(`status.${status}`)}
    </span>
  );
}

export function FolderBadge({ status }: { status: FolderStatus }) {
  const { t } = useTranslation();
  const map: Record<FolderStatus, string> = {
    idle: "badge-success",
    scanning: "badge-muted",
    syncing: "badge-muted",
    paused: "badge-warn",
    error: "badge-danger",
  };
  return <span className={`badge ${map[status]}`}>{t(`status.${status}`)}</span>;
}

export function ConnBadge({ conn, rtt }: { conn: ConnKind; rtt?: number | null }) {
  const { t } = useTranslation();
  const label = t(`devices.${conn}`);
  const cls = conn === "tailscale" ? "badge-success" : "badge-muted";
  return (
    <span className={`badge ${cls}`}>
      {label}
      {rtt != null && ` · ${rtt}ms`}
    </span>
  );
}

/** 中继服务器搭建步骤（Markdown 原文按等宽文本展示）。 */
export function RelayGuideModal({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const [guide, setGuide] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const text = await api.getRelayGuide();
        if (!cancelled) setGuide(text);
      } catch (e) {
        if (!cancelled) setError(api.asCmdError(e).message);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <Modal
      title={t("settings.relayGuideTitle")}
      onClose={onClose}
      width={640}
      footer={
        <button className="btn" onClick={onClose}>
          {t("common.close")}
        </button>
      }
    >
      {error && <ErrorBanner message={error} />}
      {guide ? (
        <pre className="guide-pre">{guide}</pre>
      ) : (
        !error && <Loading text={t("app.loading")} />
      )}
    </Modal>
  );
}

// ---------- 格式化 ----------

/** 字节数 → 人类可读。 */
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

/** Unix 秒 → 本地时间字符串。 */
export function formatTime(secs: number | null | undefined): string {
  if (!secs) return "—";
  const d = new Date(secs * 1000);
  return d.toLocaleString(undefined, {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** 相对时间（刚刚 / 3 分钟前 / …）。 */
export function formatRelative(
  secs: number | null | undefined,
  t: (k: string) => string,
): string {
  if (!secs) return "—";
  const diff = Math.floor(Date.now() / 1000) - secs;
  if (diff < 60) return t("common.none") === t("common.none") ? "just now" : "刚刚";
  if (diff < 3600) return `${Math.floor(diff / 60)} min`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} h`;
  return formatTime(secs);
}

/** 复制到剪贴板（带回退）。 */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}
