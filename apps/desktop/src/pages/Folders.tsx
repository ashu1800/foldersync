// 文件夹页（提示词 5.2.2）：列表 + 添加向导 + 详情（文件树 / 忽略规则）。

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import type { FolderDto, IgnoreRulesDto, TreeNodeDto } from "../lib/bindings";
import * as api from "../lib/tauri";
import { useStore } from "../stores/app";
import {
  Empty,
  ErrorBanner,
  FolderBadge,
  Loading,
  Modal,
  formatBytes,
  formatTime,
} from "../components/ui";

export default function Folders() {
  const { t } = useTranslation();
  const { folders, loading, refreshFolders } = useStore();
  const [showAdd, setShowAdd] = useState(false);
  const [detail, setDetail] = useState<FolderDto | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    void refreshFolders();
  }, [refreshFolders]);

  const togglePause = useCallback(
    async (f: FolderDto) => {
      setBusy(true);
      try {
        if (f.paused) {
          await api.resumeFolder(f.id);
        } else {
          await api.pauseFolder(f.id);
        }
        await refreshFolders();
      } catch (e) {
        setError(api.asCmdError(e).message);
      } finally {
        setBusy(false);
      }
    },
    [refreshFolders],
  );

  const remove = useCallback(
    async (f: FolderDto) => {
      if (!window.confirm(t("folders.removeConfirm"))) return;
      setBusy(true);
      try {
        await api.removeFolder(f.id);
        await refreshFolders();
      } catch (e) {
        setError(api.asCmdError(e).message);
      } finally {
        setBusy(false);
      }
    },
    [refreshFolders, t],
  );

  return (
    <>
      <div className="page-header">
        <h1 className="page-title">{t("folders.title")}</h1>
        <button
          className="btn btn-primary"
          onClick={() => setShowAdd(true)}
          disabled={busy}
        >
          {t("folders.add")}
        </button>
      </div>

      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

      {loading && folders.length === 0 ? (
        <Loading />
      ) : folders.length === 0 ? (
        <div className="card">
          <Empty
            icon="▤"
            title={t("folders.empty")}
            action={
              <button
                className="btn btn-primary"
                onClick={() => setShowAdd(true)}
              >
                {t("folders.add")}
              </button>
            }
          />
        </div>
      ) : (
        <div className="card" style={{ padding: 0 }}>
          <table className="table">
            <thead>
              <tr>
                <th>{t("folders.title")}</th>
                <th>{t("folders.files")}</th>
                <th>{t("folders.lastSync")}</th>
                <th>{t("folders.pending")}</th>
                <th style={{ width: 260 }} />
              </tr>
            </thead>
            <tbody>
              {folders.map((f) => (
                <tr key={f.id}>
                  <td>
                    <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                      <strong>{f.name}</strong>
                      <FolderBadge status={f.status} />
                    </div>
                    <div className="card-meta truncate mono" title={f.path}>
                      {f.path}
                    </div>
                  </td>
                  <td>{f.file_count}</td>
                  <td className="card-meta">{formatTime(f.last_sync_at)}</td>
                  <td>{f.pending}</td>
                  <td>
                    <div className="btn-row">
                      <button
                        className="btn btn-sm"
                        onClick={() => setDetail(f)}
                      >
                        {t("folders.ignoreRules")}
                      </button>
                      <button
                        className="btn btn-sm"
                        onClick={() => void togglePause(f)}
                        disabled={busy}
                      >
                        {f.paused ? t("folders.resume") : t("folders.pause")}
                      </button>
                      <button
                        className="btn btn-sm btn-danger"
                        onClick={() => void remove(f)}
                        disabled={busy}
                      >
                        {t("folders.remove")}
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
        <AddFolderWizard
          onClose={() => setShowAdd(false)}
          onDone={() => {
            setShowAdd(false);
            void refreshFolders();
          }}
        />
      )}

      {detail && (
        <FolderDetail
          folder={detail}
          onClose={() => setDetail(null)}
        />
      )}
    </>
  );
}

/** 添加文件夹向导：选目录 → 选设备 → 配置忽略 → 完成。 */
function AddFolderWizard({
  onClose,
  onDone,
}: {
  onClose: () => void;
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const { devices } = useStore();
  const [step, setStep] = useState(0);
  const [path, setPath] = useState("");
  const [selected, setSelected] = useState<string[]>([]);
  const [customRules, setCustomRules] = useState("");
  const [useDefaults, setUseDefaults] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const pick = async () => {
    try {
      const picked = await openDialog({ directory: true, multiple: false });
      if (typeof picked === "string") setPath(picked);
    } catch (e) {
      setError(String(e));
    }
  };

  const finish = async () => {
    setBusy(true);
    setError(null);
    try {
      const ignore: IgnoreRulesDto = {
        use_defaults: useDefaults,
        custom: customRules
          .split("\n")
          .map((s) => s.trim())
          .filter(Boolean),
        defaults: [],
      };
      await api.addFolder({ path, device_ids: selected, ignore });
      onDone();
    } catch (e) {
      setError(api.asCmdError(e).message);
    } finally {
      setBusy(false);
    }
  };

  const canNext = step === 0 ? path.length > 0 : true;

  return (
    <Modal
      title={t("folders.add")}
      onClose={onClose}
      footer={
        <>
          {step > 0 && (
            <button className="btn" onClick={() => setStep(step - 1)}>
              {t("folders.back")}
            </button>
          )}
          <button className="btn" onClick={onClose}>
            {t("common.cancel")}
          </button>
          {step < 2 ? (
            <button
              className="btn btn-primary"
              onClick={() => setStep(step + 1)}
              disabled={!canNext}
            >
              {t("folders.next")}
            </button>
          ) : (
            <button
              className="btn btn-primary"
              onClick={() => void finish()}
              disabled={busy}
            >
              {t("folders.finish")}
            </button>
          )}
        </>
      }
    >
      <div className="steps">
        <div className={`step ${step >= 0 ? "done" : ""}`} />
        <div className={`step ${step >= 1 ? "done" : ""}`} />
        <div className={`step ${step >= 2 ? "active" : ""}`} />
      </div>

      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

      {step === 0 && (
        <div className="field">
          <label className="label">{t("folders.chooseFolder")}</label>
          <div className="btn-row" style={{ marginBottom: 8 }}>
            <button className="btn" onClick={() => void pick()}>
              📁 {t("folders.chooseFolder")}
            </button>
          </div>
          {path && <div className="pairing-code">{path}</div>}
        </div>
      )}

      {step === 1 && (
        <div className="field">
          <label className="label">{t("folders.chooseDevices")}</label>
          {devices.length === 0 ? (
            <p className="hint">
              还没有配对设备。可以先添加文件夹，之后在「设备」页配对。
            </p>
          ) : (
            devices.map((d) => (
              <div className="checkbox-row" key={d.id}>
                <input
                  type="checkbox"
                  id={`dev-${d.id}`}
                  checked={selected.includes(d.id)}
                  onChange={(e) =>
                    setSelected(
                      e.target.checked
                        ? [...selected, d.id]
                        : selected.filter((x) => x !== d.id),
                    )
                  }
                />
                <label htmlFor={`dev-${d.id}`}>
                  {d.name} <span className="mono card-meta">{d.fingerprint}</span>
                </label>
              </div>
            ))
          )}
        </div>
      )}

      {step === 2 && (
        <>
          <div className="checkbox-row">
            <input
              type="checkbox"
              id="use-defaults"
              checked={useDefaults}
              onChange={(e) => setUseDefaults(e.target.checked)}
            />
            <label htmlFor="use-defaults">{t("folders.useDefaults")}</label>
          </div>
          <div className="field">
            <label className="label" htmlFor="rules">
              {t("folders.customRules")}
            </label>
            <textarea
              id="rules"
              className="textarea"
              value={customRules}
              onChange={(e) => setCustomRules(e.target.value)}
              placeholder={"*.tmp\nbuild/\n.DS_Store"}
            />
          </div>
        </>
      )}
    </Modal>
  );
}

/** 文件夹详情：只读文件树 + 忽略规则编辑。 */
function FolderDetail({
  folder,
  onClose,
}: {
  folder: FolderDto;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [tree, setTree] = useState<TreeNodeDto[]>([]);
  const [rules, setRules] = useState<IgnoreRulesDto | null>(null);
  const [tab, setTab] = useState<"files" | "ignore">("files");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    (async () => {
      setLoading(true);
      try {
        const [tr, rl] = await Promise.all([
          api.getFolderTree(folder.id),
          api.getIgnoreRules(folder.id),
        ]);
        setTree(tr);
        setRules(rl);
      } catch (e) {
        setError(api.asCmdError(e).message);
      } finally {
        setLoading(false);
      }
    })();
  }, [folder.id]);

  const saveRules = async () => {
    if (!rules) return;
    try {
      await api.setIgnoreRules(folder.id, rules);
      setSaved(true);
      window.setTimeout(() => setSaved(false), 1500);
    } catch (e) {
      setError(api.asCmdError(e).message);
    }
  };

  return (
    <Modal
      title={folder.name}
      onClose={onClose}
      width={680}
      footer={
        <>
          {saved && <span className="card-meta">{t("settings.saved")}</span>}
          <button className="btn" onClick={onClose}>
            {t("common.close")}
          </button>
          {tab === "ignore" && (
            <button className="btn btn-primary" onClick={() => void saveRules()}>
              {t("settings.save")}
            </button>
          )}
        </>
      }
    >
      <div className="tabs">
        <button
          className={`tab ${tab === "files" ? "active" : ""}`}
          onClick={() => setTab("files")}
        >
          文件
        </button>
        <button
          className={`tab ${tab === "ignore" ? "active" : ""}`}
          onClick={() => setTab("ignore")}
        >
          {t("folders.ignoreRules")}
        </button>
      </div>

      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

      {tab === "files" ? (
        loading ? (
          <Loading />
        ) : tree.length === 0 ? (
          <Empty title="文件夹为空" hint="同步开始后这里会显示文件列表" />
        ) : (
          <div style={{ maxHeight: 380, overflowY: "auto" }}>
            <table className="table">
              <thead>
                <tr>
                  <th>文件</th>
                  <th style={{ width: 90 }}>大小</th>
                  <th style={{ width: 150 }}>修改时间</th>
                </tr>
              </thead>
              <tbody>
                {tree.map((n) => (
                  <tr key={n.path}>
                    <td>
                      <span className="truncate" title={n.path}>
                        {n.is_dir ? "📁 " : "📄 "}
                        {n.name}
                      </span>
                      {n.is_conflict && (
                        <span className="badge badge-warn" style={{ marginLeft: 8 }}>
                          {t("folders.conflictBadge")}
                        </span>
                      )}
                    </td>
                    <td className="card-meta">
                      {n.is_dir ? "—" : formatBytes(n.size)}
                    </td>
                    <td className="card-meta">{formatTime(n.mtime)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )
      ) : (
        <>
          {rules && (
            <div className="checkbox-row">
              <input
                type="checkbox"
                id="rules-defaults"
                checked={rules.use_defaults}
                onChange={(e) =>
                  setRules({ ...rules, use_defaults: e.target.checked })
                }
              />
              <label htmlFor="rules-defaults">{t("folders.useDefaults")}</label>
            </div>
          )}
          <div className="field">
            <label className="label" htmlFor="rules-custom">
              {t("folders.customRules")}
            </label>
            <textarea
              id="rules-custom"
              className="textarea"
              value={rules?.custom.join("\n") ?? ""}
              onChange={(e) =>
                rules &&
                setRules({
                  ...rules,
                  custom: e.target.value.split("\n"),
                })
              }
              placeholder={"*.tmp\nbuild/\nnode_modules/"}
            />
            <p className="hint">语法同 .gitignore：每行一条，`/` 结尾表示目录。</p>
          </div>
        </>
      )}
    </Modal>
  );
}
