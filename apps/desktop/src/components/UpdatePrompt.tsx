// 自动更新提示：后端定时检测到新版本时推 `update://available`，这里弹窗。
//
// - 普通更新：可以「稍后」；本次运行内不再打扰同一个版本。
// - 强制更新（force）：不能关闭，只能升级。
// - 更新说明（notice）必须展示给用户。

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import * as api from "../lib/tauri";
import type { UpdateDto } from "../lib/bindings";
import { Modal } from "./ui";

/** 下载 + 安装的共享状态（弹窗和设置页都用）。 */
export function useUpdateInstall() {
  const [installing, setInstalling] = useState(false);
  const [progress, setProgress] = useState(0);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    void api
      .on("update://progress", (p) => {
        if (p.total > 0)
          setProgress(Math.min(100, Math.round((p.downloaded / p.total) * 100)));
      })
      .then((fn) => (cancelled ? fn() : (unlisten = fn)));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  const install = useCallback(async () => {
    setInstalling(true);
    setProgress(0);
    setError(null);
    try {
      // 成功时后端启动安装器并退出进程，这里通常等不到返回
      await api.installUpdate();
    } catch (e) {
      setError(api.asCmdError(e).message);
      setInstalling(false);
    }
  }, []);

  return { installing, progress, error, install };
}

/** 更新说明 + 进度 + 错误，弹窗与设置页共用的展示块。 */
export function UpdateDetails({
  update,
  installing,
  progress,
  error,
}: {
  update: UpdateDto;
  installing: boolean;
  progress: number;
  error: string | null;
}) {
  const { t } = useTranslation();
  return (
    <div className="hint" style={{ marginTop: 8 }}>
      <p>
        {t("update.found", {
          latest: update.latest_version,
          current: update.current_version,
        })}
        {update.force && (
          <b style={{ color: "var(--danger)" }}> · {t("update.forceBadge")}</b>
        )}
      </p>
      <pre
        style={{
          whiteSpace: "pre-wrap",
          maxHeight: 200,
          overflow: "auto",
          fontSize: 12,
          marginTop: 6,
        }}
      >
        {update.notice || t("update.notesEmpty")}
      </pre>
      {installing && (
        <>
          <p style={{ marginTop: 8 }}>{t("update.installingProgress", { progress })}</p>
          <div
            className="update-progress"
            role="progressbar"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={progress}
          >
            <span style={{ width: `${progress}%` }} />
          </div>
        </>
      )}
      {error && (
        <p style={{ marginTop: 8, color: "var(--danger)" }}>
          {t("update.installFailed", { error })}
        </p>
      )}
    </div>
  );
}

export default function UpdatePrompt() {
  const { t } = useTranslation();
  const [update, setUpdate] = useState<UpdateDto | null>(null);
  // 用户点过「稍后」的版本：本次运行内不再弹（强制更新除外）
  const [dismissed, setDismissed] = useState<string | null>(null);
  const { installing, progress, error, install } = useUpdateInstall();

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    void api
      .on("update://available", (u) => {
        if (u.available) setUpdate(u);
      })
      .then((fn) => (cancelled ? fn() : (unlisten = fn)));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  if (!update) return null;
  if (!update.force && dismissed === update.latest_version) return null;

  // 强制更新：Esc / 点背景都不能关
  const close = update.force ? () => {} : () => setDismissed(update.latest_version);

  return (
    <Modal
      title={update.force ? t("update.requiredTitle") : t("update.availableTitle")}
      onClose={close}
      dismissible={!update.force}
      footer={
        <div className="btn-row">
          {!update.force && (
            <button className="btn" disabled={installing} onClick={close}>
              {t("update.later")}
            </button>
          )}
          <button className="btn btn-primary" disabled={installing} onClick={install}>
            {installing
              ? t("update.installing")
              : error
                ? t("update.retry")
                : t("update.installNow")}
          </button>
        </div>
      }
    >
      <UpdateDetails
        update={update}
        installing={installing}
        progress={progress}
        error={error}
      />
      {update.force && (
        <p className="hint" style={{ marginTop: 8 }}>
          {t("update.requiredHint")}
        </p>
      )}
    </Modal>
  );
}
