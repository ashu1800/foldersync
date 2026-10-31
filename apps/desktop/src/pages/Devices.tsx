// 设备页（提示词 5.2.3）：已配对列表 + 添加设备（生成码 / 输入码两个标签页）。

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { DeviceDto, PairingCodeDto } from "../lib/bindings";
import * as api from "../lib/tauri";
import { useStore } from "../stores/app";
import {
  ConfirmDialog,
  ConnBadge,
  Empty,
  ErrorBanner,
  Icon,
  Loading,
  Modal,
  OnlineBadge,
  copyText,
  shortFingerprint,
} from "../components/ui";

export default function Devices() {
  const { t } = useTranslation();
  const {
    devices,
    devicesLoaded,
    pendingPairings,
    refreshDevices,
    refreshPendingPairings,
    removePendingPairing,
    devicesError,
  } = useStore();

  const [showAdd, setShowAdd] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmRemove, setConfirmRemove] = useState<DeviceDto | null>(null);
  const [renameTarget, setRenameTarget] = useState<DeviceDto | null>(null);
  const [renameValue, setRenameValue] = useState("");

  useEffect(() => {
    void refreshDevices();
    void refreshPendingPairings();
  }, [refreshDevices, refreshPendingPairings]);

  const handleConfirm = useCallback(
    async (requestId: string, accept: boolean) => {
      try {
        await api.confirmPairing(requestId, accept);
        removePendingPairing(requestId);
        await refreshDevices();
      } catch (e) {
        setError(api.asCmdError(e).message);
      }
    },
    [refreshDevices, removePendingPairing],
  );

  const handleRemove = useCallback(
    async (d: DeviceDto) => {
      setBusy(true);
      try {
        await api.removeDevice(d.id);
        setConfirmRemove(null);
        await refreshDevices();
      } catch (e) {
        setError(api.asCmdError(e).message);
      } finally {
        setBusy(false);
      }
    },
    [refreshDevices],
  );

  const handleRename = useCallback(
    async (d: DeviceDto) => {
      const name = renameValue.trim();
      if (!name || name === d.name) {
        setRenameTarget(null);
        return;
      }
      setBusy(true);
      try {
        await api.renameDevice(d.id, name);
        setRenameTarget(null);
        await refreshDevices();
      } catch (e) {
        setError(api.asCmdError(e).message);
      } finally {
        setBusy(false);
      }
    },
    [refreshDevices, renameValue],
  );

  return (
    <>
      <div className="page-header">
        <h1 className="page-title">{t("devices.title")}</h1>
        <button
          className="btn btn-primary"
          onClick={() => setShowAdd(true)}
          disabled={busy}
        >
          {t("devices.add")}
        </button>
      </div>

      {(error ?? devicesError) && (
        <ErrorBanner
          message={error ?? devicesError ?? ""}
          onRetry={() => void refreshDevices()}
          onDismiss={() => setError(null)}
        />
      )}

      {/* 待确认的配对请求：这是安全关键路径，必须显式展示指纹供核对 */}
      {pendingPairings.map((p) => (
        <div className="notice" key={p.request_id}>
          <div className="notice-content">
            <strong>{t("devices.incomingRequest")}</strong>
            <div className="card-meta">
              {p.device_name} · <span className="mono">{p.fingerprint}</span>
            </div>
          </div>
          <button
            className="btn btn-sm btn-primary"
            onClick={() => void handleConfirm(p.request_id, true)}
          >
            {t("devices.accept")}
          </button>
          <button
            className="btn btn-sm"
            onClick={() => void handleConfirm(p.request_id, false)}
          >
            {t("devices.reject")}
          </button>
        </div>
      ))}

      {!devicesLoaded && devices.length === 0 ? (
        <Loading />
      ) : devices.length === 0 ? (
        <div className="card">
          <Empty
            icon={<Icon name="device" size={30} />}
            title={t("devices.empty")}
            action={
              <button className="btn btn-primary" onClick={() => setShowAdd(true)}>
                {t("devices.add")}
              </button>
            }
          />
        </div>
      ) : (
        <div className="card table-scroll" style={{ padding: 0 }}>
          <table className="table">
            <thead>
              <tr>
                <th>{t("devices.name")}</th>
                <th style={{ width: 150 }}>{t("devices.fingerprint")}</th>
                <th style={{ width: 90 }}>{t("devices.online")}</th>
                <th style={{ width: 110 }}>{t("devices.connection")}</th>
                <th style={{ width: 156 }}>{t("common.actions")}</th>
              </tr>
            </thead>
            <tbody>
              {devices.map((d) => (
                <tr key={d.id}>
                  <td>
                    <strong>{d.name}</strong>
                  </td>
                  <td>
                    <button
                      className="fp-copy mono"
                      title={`${d.fingerprint}\n${t("devices.copy")}`}
                      onClick={() => void copyText(d.fingerprint)}
                    >
                      {shortFingerprint(d.fingerprint)}
                    </button>
                  </td>
                  <td>
                    <OnlineBadge online={d.online} />
                  </td>
                  <td>
                    <ConnBadge conn={d.conn} rtt={d.rtt_ms} />
                  </td>
                  <td>
                    <div className="btn-row">
                      <button
                        className="btn btn-sm"
                        onClick={() => {
                          setRenameTarget(d);
                          setRenameValue(d.name);
                        }}
                        disabled={busy}
                      >
                        {t("devices.rename")}
                      </button>
                      <button
                        className="btn btn-sm btn-danger"
                        onClick={() => setConfirmRemove(d)}
                        disabled={busy}
                      >
                        {t("devices.remove")}
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {showAdd && (
        <AddDeviceModal
          onClose={() => setShowAdd(false)}
          onDone={() => {
            setShowAdd(false);
            void refreshDevices();
          }}
        />
      )}

      {confirmRemove && (
        <ConfirmDialog
          title={t("devices.remove")}
          message={t("devices.removeConfirm")}
          confirmLabel={t("devices.remove")}
          danger
          busy={busy}
          onConfirm={() => void handleRemove(confirmRemove)}
          onCancel={() => setConfirmRemove(null)}
        />
      )}

      {renameTarget && (
        <Modal
          title={t("devices.rename")}
          onClose={() => setRenameTarget(null)}
          footer={
            <>
              <button className="btn" onClick={() => setRenameTarget(null)}>
                {t("common.cancel")}
              </button>
              <button
                className="btn btn-primary"
                disabled={busy || !renameValue.trim()}
                onClick={() => void handleRename(renameTarget)}
              >
                {t("settings.save")}
              </button>
            </>
          }
        >
          <div className="field">
            <label className="label" htmlFor="rename-device">
              {t("devices.name")}
            </label>
            <input
              id="rename-device"
              className="input"
              value={renameValue}
              maxLength={64}
              onChange={(e) => setRenameValue(e.target.value)}
            />
          </div>
        </Modal>
      )}
    </>
  );
}

/** 添加设备对话框：生成码 / 输入码。 */
function AddDeviceModal({
  onClose,
  onDone,
}: {
  onClose: () => void;
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const pendingCount = useStore((s) => s.pendingPairings.length);
  const refreshPendingPairings = useStore((s) => s.refreshPendingPairings);
  const [tab, setTab] = useState<"generate" | "input">("generate");
  const [code, setCode] = useState<PairingCodeDto | null>(null);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [paired, setPaired] = useState<DeviceDto | null>(null);

  // 有设备请求配对时关闭弹窗，让用户看到页面上的指纹核对/接受按钮
  useEffect(() => {
    if (pendingCount > 0) onClose();
  }, [pendingCount, onClose]);

  useEffect(() => {
    const id = window.setInterval(() => void refreshPendingPairings(), 1500);
    return () => window.clearInterval(id);
  }, [refreshPendingPairings]);

  // 倒计时：让用户知道还有多久过期
  const [remaining, setRemaining] = useState(0);
  useEffect(() => {
    if (!code) return;
    const tick = () => {
      const left = Math.max(0, code.expires_at - Math.floor(Date.now() / 1000));
      setRemaining(left);
    };
    tick();
    const id = window.setInterval(tick, 1000);
    return () => window.clearInterval(id);
  }, [code]);

  const generate = async () => {
    setBusy(true);
    setError(null);
    try {
      setCode(await api.createPairingCode());
    } catch (e) {
      setError(api.asCmdError(e).message);
    } finally {
      setBusy(false);
    }
  };

  const submit = async () => {
    if (!input.trim()) return;
    setBusy(true);
    setError(null);
    try {
      setPaired(await api.acceptPairingCode(input.trim()));
    } catch (e) {
      setError(api.asCmdError(e).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title={t("devices.add")}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            {t("common.close")}
          </button>
          {paired ? (
            <button className="btn btn-primary" onClick={onDone}>
              {t("common.confirm")}
            </button>
          ) : tab === "generate" ? (
            <button
              className="btn btn-primary"
              onClick={() => void generate()}
              disabled={busy}
            >
              {code ? t("common.retry") : t("devices.generateCode")}
            </button>
          ) : (
            <button
              className="btn btn-primary"
              onClick={() => void submit()}
              disabled={busy || !input.trim()}
            >
              {busy ? t("devices.pairing") : t("devices.submit")}
            </button>
          )}
        </>
      }
    >
      <div
        className="tabs"
        role="tablist"
        aria-label={t("devices.add")}
        onKeyDown={(e) => {
          if (e.key !== "ArrowRight" && e.key !== "ArrowLeft") return;
          e.preventDefault();
          const next = tab === "generate" ? "input" : "generate";
          setTab(next);
          document.getElementById(`device-tab-${next}`)?.focus();
        }}
      >
        <button
          id="device-tab-generate"
          role="tab"
          aria-selected={tab === "generate"}
          aria-controls="device-panel"
          tabIndex={tab === "generate" ? 0 : -1}
          className={`tab ${tab === "generate" ? "active" : ""}`}
          onClick={() => setTab("generate")}
        >
          {t("devices.generateCode")}
        </button>
        <button
          id="device-tab-input"
          role="tab"
          aria-selected={tab === "input"}
          aria-controls="device-panel"
          tabIndex={tab === "input" ? 0 : -1}
          className={`tab ${tab === "input" ? "active" : ""}`}
          onClick={() => setTab("input")}
        >
          {t("devices.inputCode")}
        </button>
      </div>

      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

      <div role="tabpanel" id="device-panel" aria-labelledby={`device-tab-${tab}`}>
        {paired ? (
          <div
            className="notice"
            style={{ flexDirection: "column", alignItems: "stretch" }}
          >
            <strong>{t("devices.pairedTitle")}</strong>
            <div className="card-meta" style={{ marginTop: 4 }}>
              {t("devices.pairedHint")}
            </div>
            <div className="pairing-code mono" style={{ marginTop: 10 }}>
              {paired.fingerprint}
            </div>
          </div>
        ) : tab === "generate" ? (
          code ? (
            <>
              <div className="pairing-code">{code.code}</div>
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  marginTop: 12,
                }}
              >
                <span className="hint">
                  {t("devices.codeExpires")}: {formatDuration(remaining)}
                </span>
                <button
                  className="btn btn-sm"
                  onClick={async () => {
                    const ok = await copyText(code.code);
                    setCopied(ok);
                    window.setTimeout(() => setCopied(false), 1500);
                  }}
                >
                  {copied ? t("devices.copied") : t("devices.copy")}
                </button>
              </div>
              <p className="hint" style={{ marginTop: 12 }}>
                {t("devices.pairingOtherHint")}
              </p>
            </>
          ) : (
            <p className="hint">{t("devices.generateHint")}</p>
          )
        ) : (
          <div className="field">
            <label className="label" htmlFor="pairing-input">
              {t("devices.inputCode")}
            </label>
            <textarea
              id="pairing-input"
              className="textarea"
              placeholder={t("devices.codePlaceholder")}
              value={input}
              onChange={(e) => setInput(e.target.value)}
              autoFocus
            />
          </div>
        )}
      </div>
    </Modal>
  );
}

function formatDuration(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}
