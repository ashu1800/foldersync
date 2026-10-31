// 共享 UI 组件：小、无依赖、各页面复用。

import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import type { ConnKind, FolderStatus, OverallStatus } from "../lib/bindings";
import * as api from "../lib/tauri";

// ---------- 图标（内联 SVG，统一描边，不引入图标库） ----------

type IconName =
  | "overview"
  | "folder"
  | "device"
  | "activity"
  | "conflict"
  | "settings"
  | "file"
  | "chevron"
  | "check"
  | "close"
  | "plus"
  | "refresh"
  | "alert"
  | "info"
  | "clock"
  | "copy"
  | "trash"
  | "sync";

const ICON_PATHS: Record<IconName, ReactNode> = {
  overview: (
    <>
      <rect x="3" y="3" width="7" height="7" rx="1.5" />
      <rect x="14" y="3" width="7" height="7" rx="1.5" />
      <rect x="3" y="14" width="7" height="7" rx="1.5" />
      <rect x="14" y="14" width="7" height="7" rx="1.5" />
    </>
  ),
  folder: (
    <path d="M3 6a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
  ),
  device: (
    <>
      <rect x="3" y="4" width="18" height="12" rx="2" />
      <path d="M8 20h8M12 16v4" />
    </>
  ),
  activity: <path d="M3 12h4l2-7 4 14 2-7h6" />,
  conflict: (
    <>
      <path d="M12 3 2.8 19.2A1.5 1.5 0 0 0 4.1 21h15.8a1.5 1.5 0 0 0 1.3-1.8z" />
      <path d="M12 9v5M12 17.2v.1" />
    </>
  ),
  settings: (
    <>
      <circle cx="12" cy="12" r="3" />
      <path d="M19.4 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06A1.7 1.7 0 0 0 15 19.4a1.7 1.7 0 0 0-1 1.55V21a2 2 0 1 1-4 0v-.09A1.7 1.7 0 0 0 9 19.4a1.7 1.7 0 0 0-1.87.34l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.7 1.7 0 0 0 4.6 15a1.7 1.7 0 0 0-1.55-1H3a2 2 0 1 1 0-4h.09A1.7 1.7 0 0 0 4.6 9a1.7 1.7 0 0 0-.34-1.87l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.7 1.7 0 0 0 9 4.6a1.7 1.7 0 0 0 1-1.55V3a2 2 0 1 1 4 0v.09A1.7 1.7 0 0 0 15 4.6a1.7 1.7 0 0 0 1.87-.34l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.7 1.7 0 0 0 19.4 9v.09A1.7 1.7 0 0 0 21 10h.09a2 2 0 1 1 0 4H21a1.7 1.7 0 0 0-1.6 1z" />
    </>
  ),
  file: <path d="M6 2h8l5 5v15H6zM14 2v6h5" />,
  chevron: <path d="m9 6 6 6-6 6" />,
  check: <path d="m4 12 5 5L20 6" />,
  close: <path d="M6 6l12 12M18 6 6 18" />,
  plus: <path d="M12 5v14M5 12h14" />,
  refresh: (
    <>
      <path d="M20 11a8 8 0 1 0-2.3 5.7" />
      <path d="M20 4v7h-7" />
    </>
  ),
  alert: (
    <>
      <circle cx="12" cy="12" r="9" />
      <path d="M12 8v5M12 16.5v.1" />
    </>
  ),
  info: (
    <>
      <circle cx="12" cy="12" r="9" />
      <path d="M12 11v5M12 7.5v.1" />
    </>
  ),
  clock: (
    <>
      <circle cx="12" cy="12" r="9" />
      <path d="M12 7v5l3 2" />
    </>
  ),
  copy: (
    <>
      <rect x="9" y="9" width="12" height="12" rx="2" />
      <path d="M5 15V5a2 2 0 0 1 2-2h10" />
    </>
  ),
  trash: <path d="M4 7h16M9 7V4h6v3M6 7l1 14h10l1-14M10 11v6M14 11v6" />,
  sync: (
    <>
      <path d="M20 7h-9a4 4 0 0 0-4 4v1" />
      <path d="M17 4l3 3-3 3" />
      <path d="M4 17h9a4 4 0 0 0 4-4v-1" />
      <path d="M7 20l-3-3 3-3" />
    </>
  ),
};

export function Icon({
  name,
  size = 18,
  className,
}: {
  name: IconName;
  size?: number;
  className?: string;
}) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.5}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {ICON_PATHS[name]}
    </svg>
  );
}

// ---------- 状态占位 ----------

export function Loading({ text }: { text?: string }) {
  const { t } = useTranslation();
  return (
    <div className="loading" role="status" aria-live="polite">
      <div className="spinner" aria-hidden="true" />
      <span>{text ?? t("app.loading")}</span>
    </div>
  );
}

export function Empty({
  icon,
  title,
  hint,
  action,
}: {
  icon?: ReactNode;
  title: string;
  hint?: string;
  action?: ReactNode;
}) {
  return (
    <div className="empty">
      <div className="empty-icon" aria-hidden="true">
        {icon ?? <Icon name="info" size={30} />}
      </div>
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
  dismissible = true,
  hideClose = false,
}: {
  title: string;
  children: ReactNode;
  footer?: ReactNode;
  onClose: () => void;
  width?: number;
  /** false：强制流程，屏蔽 Esc/遮罩/✕ */
  dismissible?: boolean;
  hideClose?: boolean;
}) {
  const { t } = useTranslation();
  const panelRef = useRef<HTMLDivElement>(null);
  const titleId = useId();
  const onCloseRef = useRef(onClose);
  const dismissibleRef = useRef(dismissible);

  useEffect(() => {
    onCloseRef.current = onClose;
    dismissibleRef.current = dismissible;
  });

  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    const panel = panelRef.current;
    const focusables = () =>
      panel
        ? Array.from(
            panel.querySelectorAll<HTMLElement>(
              'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
            ),
          ).filter((el) => el.offsetParent !== null)
        : [];
    // 初始焦点：优先第一个输入框，其次第一个按钮
    const first =
      panel?.querySelector<HTMLElement>("input, select, textarea") ?? focusables()[0];
    first?.focus();

    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && dismissibleRef.current) {
        e.preventDefault();
        onCloseRef.current();
        return;
      }
      if (e.key !== "Tab") return;
      const items = focusables();
      if (items.length === 0) return;
      const head = items[0];
      const tail = items[items.length - 1];
      if (e.shiftKey && document.activeElement === head) {
        e.preventDefault();
        tail.focus();
      } else if (!e.shiftKey && document.activeElement === tail) {
        e.preventDefault();
        head.focus();
      }
    };
    document.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("keydown", onKey, true);
      previous?.focus?.();
    };
  }, []);

  return (
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (dismissible && e.target === e.currentTarget) onClose();
      }}
      role="presentation"
    >
      <div
        ref={panelRef}
        className="modal"
        style={width ? { maxWidth: width } : undefined}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
      >
        <div className="modal-header">
          <h2 className="modal-title" id={titleId}>
            {title}
          </h2>
          {dismissible && !hideClose && (
            <button
              className="modal-close"
              onClick={onClose}
              aria-label={t("common.close")}
            >
              <Icon name="close" size={16} />
            </button>
          )}
        </div>
        <div className="modal-body">{children}</div>
        {footer && <div className="modal-footer">{footer}</div>}
      </div>
    </div>
  );
}

export function ConfirmDialog({
  title,
  message,
  confirmLabel,
  danger,
  busy,
  onConfirm,
  onCancel,
}: {
  title: string;
  message: string;
  confirmLabel: string;
  danger?: boolean;
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  return (
    <Modal
      title={title}
      onClose={onCancel}
      dismissible={!busy}
      footer={
        <>
          <button className="btn" onClick={onCancel} disabled={busy}>
            {t("common.cancel")}
          </button>
          <button
            className={`btn ${danger ? "btn-danger" : "btn-primary"}`}
            onClick={onConfirm}
            disabled={busy}
          >
            {busy ? t("common.saving") : confirmLabel}
          </button>
        </>
      }
    >
      <p style={{ margin: 0 }}>{message}</p>
    </Modal>
  );
}

// ---------- 状态徽标 ----------

export function OverallBadge({ status }: { status: OverallStatus }) {
  const { t } = useTranslation();
  const map: Record<OverallStatus, { cls: string; dot: string }> = {
    not_configured: { cls: "badge-muted", dot: "dot-muted" },
    synced: { cls: "badge-success", dot: "dot-success" },
    syncing: { cls: "badge-primary", dot: "dot-primary" },
    error: { cls: "badge-danger", dot: "dot-danger" },
    paused: { cls: "badge-warn", dot: "dot-warn" },
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
    idle: "badge-muted",
    scanning: "badge-primary",
    syncing: "badge-primary",
    paused: "badge-warn",
    error: "badge-danger",
  };
  return <span className={`badge ${map[status]}`}>{t(`status.${status}`)}</span>;
}

export function ConnBadge({ conn, rtt }: { conn: ConnKind; rtt?: number | null }) {
  const { t } = useTranslation();
  const label = t(`devices.${conn}`);
  const cls = "badge-muted";
  return (
    <span className={`badge ${cls}`}>
      {label}
      {rtt != null && ` · ${rtt}ms`}
    </span>
  );
}

/** 在线/离线：文字 + 图标，不能只靠颜色。 */
export function OnlineBadge({ online }: { online: boolean }) {
  const { t } = useTranslation();
  return (
    <span className={`badge ${online ? "badge-success" : "badge-muted"}`}>
      <span
        className={`dot ${online ? "dot-success" : "dot-muted"}`}
        aria-hidden="true"
      />
      {t(online ? "devices.online" : "devices.offline")}
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
export function shortFingerprint(fp: string): string {
  if (fp.length <= 22) return fp;
  return `${fp.slice(0, 10)}…${fp.slice(-6)}`;
}

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

/** Unix 秒 → 本地时间字符串（跟随应用语言）。 */
export function formatTime(secs: number | null | undefined): string {
  if (!secs) return "—";
  const d = new Date(secs * 1000);
  const locale = document.documentElement.lang || undefined;
  return d.toLocaleString(locale, {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
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
