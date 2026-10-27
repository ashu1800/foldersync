// 设备页（提示词 5.2.3）：已配对列表 + 添加设备（生成码 / 输入码两个标签页）。

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { DeviceDto, PairingCodeDto } from "../lib/bindings";
import * as api from "../lib/tauri";
import { useStore } from "../stores/app";
import {
  ConnBadge,
  Empty,
  ErrorBanner,
  Loading,
  Modal,
  copyText,
} from "../components/ui";

export default function Devices() {
  const { t } = useTranslation();
  const {
    devices,
    pendingPairings,
    loading,
    refreshDevices,
    refreshPendingPairings,
    removePendingPairing,
  } = useStore();

  const [showAdd, setShowAdd] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

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
      if (!window.confirm(t("devices.removeConfirm"))) return;
      setBusy(true);
      try {
        await api.removeDevice(d.id);
        await refreshDevices();
      } catch (e) {
        setError(api.asCmdError(e).message);
      } finally {
        setBusy(false);
      }
    },
    [refreshDevices, t],
  );

  const handleRename = useCallback(
    async (d: DeviceDto) => {
      const name = window.prompt(t("devices.rename"), d.name);
      if (!name || name === d.name) return;
      setBusy(true);
      try {
        await api.renameDevice(d.id, name);
        await refreshDevices();
      } catch (e) {
        setError(api.asCmdError(e).message);
      } finally {
        setBusy(false);
      }
    },
    [refreshDevices, t],
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

      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

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

      {loading && devices.length === 0 ? (
        <Loading />
      ) : devices.length === 0 ? (
        <div className="card">
          <Empty
            icon="◍"
            title={t("devices.empty")}
            action={
              <button className="btn btn-primary" onClick={() => setShowAdd(true)}>
                {t("devices.add")}
              </button>
            }
          />
        </div>
      ) : (
        <div className="card" style={{ padding: 0 }}>
          <table className="table">
            <thead>
              <tr>
                <th>{t("devices.title")}</th>
                <th>{t("devices.fingerprint")}</th>
                <th>{t("devices.online")}</th>
                <th style={{ width: 200 }} />
              </tr>
            </thead>
            <tbody>
              {devices.map((d) => (
                <tr key={d.id}>
                  <td>
                    <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                      <span
                        className={`dot ${d.online ? "dot-success" : "dot-muted"}`}
                        aria-hidden
                      />
                      {d.name}
                    </div>
                  </td>
                  <td>
                    <span className="mono">{d.fingerprint}</span>
                  </td>
                  <td>
                    <ConnBadge conn={d.conn} rtt={d.rtt_ms} />
                  </td>
                  <td>
                    <div className="btn-row">
                      <button
                        className="btn btn-sm"
                        onClick={() => void handleRename(d)}
                        disabled={busy}
                      >
                        {t("devices.rename")}
                      </button>
                      <button
                        className="btn btn-sm btn-danger"
                        onClick={() => void handleRemove(d)}
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
  const [tab, setTab] = useState<"generate" | "input">("generate");
  const [code, setCode] = useState<PairingCodeDto | null>(null);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

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
      await api.acceptPairingCode(input.trim());
      onDone();
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
          {tab === "generate" ? (
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
      <div className="tabs">
        <button
          className={`tab ${tab === "generate" ? "active" : ""}`}
          onClick={() => setTab("generate")}
        >
          {t("devices.generateCode")}
        </button>
        <button
          className={`tab ${tab === "input" ? "active" : ""}`}
          onClick={() => setTab("input")}
        >
          {t("devices.inputCode")}
        </button>
      </div>

      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

      {tab === "generate" ? (
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
              在另一台设备上打开 FolderSync → 设备 → 输入配对码，粘贴上面这串字符。
            </p>
          </>
        ) : (
          <p className="hint">点击下方按钮生成一次性配对码，5 分钟内有效。</p>
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
    </Modal>
  );
}

function formatDuration(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}
