// 收到的文件夹共享邀请：全局弹窗确认（接受并选择存储路径 / 拒绝）。
//
// 数据来源：folder://invite 事件实时推送 + 5 秒轮询兜底（App.tsx）。
// 接受时用系统目录对话框选路径，默认建议 `<同步根>/<文件夹名>`。

import { useCallback, useState } from "react";
import { useTranslation } from "react-i18next";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import type { InviteDto } from "../lib/bindings";
import * as api from "../lib/tauri";
import { useStore } from "../stores/app";
import { Modal } from "./ui";

export default function InvitePrompt() {
  const pendingInvites = useStore((s) => s.pendingInvites);
  const removePendingInvite = useStore((s) => s.removePendingInvite);
  const refreshFolders = useStore((s) => s.refreshFolders);

  // 一次只处理一条，避免多个弹窗叠罗汉；
  // key = folder_id：换下一条邀请时 InviteForm 的 path 状态整体重置。
  const current: InviteDto | null = pendingInvites[0] ?? null;
  if (!current) return null;
  return (
    <InviteForm
      key={current.folder_id}
      invite={current}
      onDone={(folderId) => {
        removePendingInvite(folderId);
        void refreshFolders();
      }}
    />
  );
}

function InviteForm({
  invite,
  onDone,
}: {
  invite: InviteDto;
  onDone: (folderId: string) => void;
}) {
  const { t } = useTranslation();
  const [path, setPath] = useState(invite.suggested_path);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const choosePath = useCallback(async () => {
    const selected = await openDialog({ directory: true, multiple: false });
    if (typeof selected === "string") setPath(selected);
  }, []);

  const accept = useCallback(async () => {
    if (!path.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await api.acceptPendingInvite(invite.folder_id, path.trim());
      onDone(invite.folder_id);
    } catch (e) {
      setError(api.localizedError(e, t));
      setBusy(false);
    }
  }, [invite.folder_id, onDone, path, t]);

  const decline = useCallback(async () => {
    setBusy(true);
    try {
      await api.declinePendingInvite(invite.folder_id);
      onDone(invite.folder_id);
    } catch (e) {
      setError(api.localizedError(e, t));
      setBusy(false);
    }
  }, [invite.folder_id, onDone, t]);

  return (
    <Modal
      title={t("invites.title")}
      onClose={() => {}}
      dismissible={false}
      hideClose
      footer={
        <>
          <button className="btn" onClick={() => void decline()} disabled={busy}>
            {t("invites.decline")}
          </button>
          <button
            className="btn btn-primary"
            onClick={() => void accept()}
            disabled={busy || !path.trim()}
          >
            {busy ? t("common.saving") : t("invites.accept")}
          </button>
        </>
      }
    >
      <p style={{ marginTop: 0 }}>
        {t("invites.message", {
          device: invite.from_device,
          name: invite.folder_name,
        })}
      </p>
      <div className="field">
        <label className="label" htmlFor="invite-path">
          {t("invites.pathLabel")}
        </label>
        <div style={{ display: "flex", gap: 8 }}>
          <input
            id="invite-path"
            className="input mono"
            value={path}
            onChange={(e) => setPath(e.target.value)}
            spellCheck={false}
          />
          <button className="btn" onClick={() => void choosePath()} disabled={busy}>
            {t("invites.browse")}
          </button>
        </div>
      </div>
      {error && (
        <div className="notice" style={{ color: "var(--danger)" }}>
          {error}
        </div>
      )}
    </Modal>
  );
}
