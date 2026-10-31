// 文件夹页（提示词 5.2.2）：列表 + 添加向导 + 详情（文件树 / 忽略规则）。

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import type { FolderDto, IgnoreRulesDto, TreeNodeDto } from "../lib/bindings";

/// 文件树一次最多渲染的行数。超过就截断并提示——上万行 DOM 会明显卡顿。
const TREE_RENDER_LIMIT = 1000;
import * as api from "../lib/tauri";
import { useStore } from "../stores/app";
import {
  ConfirmDialog,
  Empty,
  ErrorBanner,
  FolderBadge,
  Icon,
  Loading,
  Modal,
  formatBytes,
  formatTime,
} from "../components/ui";

export default function Folders() {
  const { t } = useTranslation();
  const { folders, foldersLoaded, refreshFolders, foldersError } = useStore();
  const [showAdd, setShowAdd] = useState(false);
  const [detail, setDetail] = useState<FolderDto | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmRemove, setConfirmRemove] = useState<FolderDto | null>(null);

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
      setBusy(true);
      try {
        await api.removeFolder(f.id);
        setConfirmRemove(null);
        await refreshFolders();
      } catch (e) {
        setError(api.asCmdError(e).message);
      } finally {
        setBusy(false);
      }
    },
    [refreshFolders],
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

      {(error ?? foldersError) && (
        <ErrorBanner
          message={error ?? foldersError ?? ""}
          onRetry={() => void refreshFolders()}
          onDismiss={() => setError(null)}
        />
      )}

      {!foldersLoaded && folders.length === 0 ? (
        <Loading />
      ) : folders.length === 0 ? (
        <div className="card">
          <Empty
            icon={<Icon name="folder" size={30} />}
            title={t("folders.empty")}
            action={
              <button className="btn btn-primary" onClick={() => setShowAdd(true)}>
                {t("folders.add")}
              </button>
            }
          />
        </div>
      ) : (
        <div className="card table-scroll" style={{ padding: 0 }}>
          <table className="table">
            <thead>
              <tr>
                <th>{t("folders.colName")}</th>
                <th style={{ width: 72 }}>{t("folders.files")}</th>
                <th style={{ width: 132 }}>{t("folders.lastSync")}</th>
                <th style={{ width: 72 }}>{t("folders.pending")}</th>
                <th style={{ width: 218 }}>{t("common.actions")}</th>
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
                    {f.last_error && (
                      <div className="card-meta" style={{ color: "var(--danger)" }}>
                        {f.last_error}
                        {f.last_error.includes("不存在") && (
                          <button
                            className="btn btn-sm"
                            style={{ marginLeft: 8 }}
                            onClick={async () => {
                              const selected = await openDialog({
                                directory: true,
                                multiple: false,
                              });
                              if (typeof selected !== "string") return;
                              try {
                                await api.relocateFolder(f.id, selected);
                                await refreshFolders();
                              } catch (e) {
                                setError(api.asCmdError(e).message);
                              }
                            }}
                          >
                            {t("folders.relocate")}
                          </button>
                        )}
                      </div>
                    )}
                  </td>
                  <td>{f.file_count}</td>
                  <td className="card-meta">{formatTime(f.last_sync_at)}</td>
                  <td>{f.pending}</td>
                  <td>
                    <div className="btn-row">
                      <button className="btn btn-sm" onClick={() => setDetail(f)}>
                        {t("folders.details")}
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
                        onClick={() => setConfirmRemove(f)}
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

      {detail && <FolderDetail folder={detail} onClose={() => setDetail(null)} />}

      {confirmRemove && (
        <ConfirmDialog
          title={t("folders.remove")}
          message={t("folders.removeConfirm")}
          confirmLabel={t("folders.remove")}
          danger
          busy={busy}
          onConfirm={() => void remove(confirmRemove)}
          onCancel={() => setConfirmRemove(null)}
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
      dismissible={step === 0 && !path}
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
      <div className="steps" aria-label={t("folders.add")}>
        {[0, 1, 2].map((i) => (
          <div
            key={i}
            className={`step ${i === step ? "active" : i < step ? "done" : ""}`}
            aria-current={i === step ? "step" : undefined}
          />
        ))}
      </div>

      {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

      {step === 0 && (
        <div className="field">
          <label className="label">{t("folders.chooseFolder")}</label>
          <div className="btn-row" style={{ marginBottom: 8 }}>
            <button className="btn" onClick={() => void pick()}>
              <Icon name="folder" size={16} /> {t("folders.chooseFolder")}
            </button>
          </div>
          {path && <div className="pairing-code">{path}</div>}
        </div>
      )}

      {step === 1 && (
        <div className="field">
          <label className="label">{t("folders.chooseDevices")}</label>
          {devices.length === 0 ? (
            <p className="hint">{t("folders.noDevicesHint")}</p>
          ) : (
            <>
              {devices.map((d) => (
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
              ))}
              <p className="hint">{t("folders.shareHint")}</p>
            </>
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
function FolderDetail({ folder, onClose }: { folder: FolderDto; onClose: () => void }) {
  const { t } = useTranslation();
  const { devices, refreshFolders } = useStore();
  const [treeAll, setTreeAll] = useState<TreeNodeDto[]>([]);
  const [treeLimit, setTreeLimit] = useState(TREE_RENDER_LIMIT);
  const [rules, setRules] = useState<IgnoreRulesDto | null>(null);
  const [tab, setTab] = useState<"files" | "ignore" | "sharing">("files");
  const [subpath, setSubpath] = useState("");
  const [shared, setShared] = useState<string[]>(() =>
    folder.shared_with.length > 0 &&
    devices.length > 0 &&
    devices.every((d) => folder.shared_with.includes(d.id))
      ? []
      : folder.shared_with,
  );
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [rulesDirty, setRulesDirty] = useState(false);
  const [sharingDirty, setSharingDirty] = useState(false);
  const [savingRules, setSavingRules] = useState(false);
  const [savingSharing, setSavingSharing] = useState(false);
  const [confirmClose, setConfirmClose] = useState(false);

  const dirty = rulesDirty || sharingDirty;
  const requestClose = () => {
    if (dirty) setConfirmClose(true);
    else onClose();
  };

  useEffect(() => {
    (async () => {
      setLoading(true);
      try {
        const [tr, rl] = await Promise.all([
          api.getFolderTree(folder.id, subpath || undefined),
          api.getIgnoreRules(folder.id),
        ]);
        // 大文件夹全量渲染上万行会卡；默认只渲染前 1000 行，用户可继续加载。
        setTreeAll(tr);
        setTreeLimit(TREE_RENDER_LIMIT);
        setRules(rl);
      } catch (e) {
        setError(api.asCmdError(e).message);
      } finally {
        setLoading(false);
      }
    })();
  }, [folder.id, subpath]);

  const crumbs = subpath ? subpath.split("/") : [];
  const parentPath = crumbs.slice(0, -1).join("/");
  const tree = treeAll.slice(0, treeLimit);
  const treeTruncated = Math.max(0, treeAll.length - treeLimit);

  const saveRules = async () => {
    if (!rules) return;
    setSavingRules(true);
    try {
      await api.setIgnoreRules(folder.id, rules);
      setRulesDirty(false);
      setSaved(true);
      window.setTimeout(() => setSaved(false), 1500);
    } catch (e) {
      setError(api.asCmdError(e).message);
    } finally {
      setSavingRules(false);
    }
  };

  const saveSharing = async () => {
    setSavingSharing(true);
    try {
      await api.setFolderSharing(folder.id, shared);
      setSharingDirty(false);
      await refreshFolders();
      setSaved(true);
      window.setTimeout(() => setSaved(false), 1500);
    } catch (e) {
      setError(api.asCmdError(e).message);
    } finally {
      setSavingSharing(false);
    }
  };

  return (
    <>
      <Modal
        title={folder.name}
        onClose={requestClose}
        width={680}
        dismissible={!confirmClose}
        footer={
          <>
            {saved && <span className="card-meta">{t("settings.saved")}</span>}
            <button
              className="btn"
              onClick={() =>
                void api.openFolder(folder.id).catch((e) => {
                  setError(api.asCmdError(e).message);
                })
              }
            >
              {t("folders.openFolder")}
            </button>
            <button className="btn" onClick={requestClose}>
              {t("common.close")}
            </button>
            {tab === "ignore" && (
              <button
                className="btn btn-primary"
                disabled={savingRules}
                onClick={() => void saveRules()}
              >
                {t("settings.save")}
              </button>
            )}
            {tab === "sharing" && (
              <button
                className="btn btn-primary"
                disabled={savingSharing}
                onClick={() => void saveSharing()}
              >
                {t("settings.save")}
              </button>
            )}
          </>
        }
      >
        <div
          className="tabs"
          role="tablist"
          aria-label={folder.name}
          onKeyDown={(e) => {
            const keys = ["files", "ignore", "sharing"] as const;
            if (e.key !== "ArrowRight" && e.key !== "ArrowLeft") return;
            e.preventDefault();
            const i = keys.indexOf(tab);
            const next =
              keys[(i + (e.key === "ArrowRight" ? 1 : keys.length - 1)) % keys.length];
            setTab(next);
            document.getElementById(`folder-tab-${next}`)?.focus();
          }}
        >
          <button
            id="folder-tab-files"
            role="tab"
            aria-selected={tab === "files"}
            tabIndex={tab === "files" ? 0 : -1}
            aria-controls="folder-panel"
            className={`tab ${tab === "files" ? "active" : ""}`}
            onClick={() => setTab("files")}
          >
            {t("folders.filesTab")}
          </button>
          <button
            id="folder-tab-ignore"
            role="tab"
            aria-selected={tab === "ignore"}
            tabIndex={tab === "ignore" ? 0 : -1}
            aria-controls="folder-panel"
            className={`tab ${tab === "ignore" ? "active" : ""}`}
            onClick={() => setTab("ignore")}
          >
            {t("folders.ignoreRules")}
          </button>
          <button
            id="folder-tab-sharing"
            role="tab"
            aria-selected={tab === "sharing"}
            tabIndex={tab === "sharing" ? 0 : -1}
            aria-controls="folder-panel"
            className={`tab ${tab === "sharing" ? "active" : ""}`}
            onClick={() => setTab("sharing")}
          >
            {t("folders.sharingTab")}
          </button>
        </div>

        {error && <ErrorBanner message={error} onDismiss={() => setError(null)} />}

        <div id="folder-panel" role="tabpanel" aria-labelledby={`folder-tab-${tab}`}>
          {tab === "files" ? (
            <>
              <div className="tree-bar">
                <button
                  className="btn btn-sm btn-ghost"
                  disabled={!subpath}
                  onClick={() => setSubpath(parentPath)}
                >
                  ← {t("folders.up")}
                </button>
                <div className="breadcrumb">
                  <button
                    className="breadcrumb-link"
                    onClick={() => setSubpath("")}
                    title={folder.path}
                  >
                    {folder.name}
                  </button>
                  {crumbs.map((c, i) => (
                    <span key={`${c}-${i}`}>
                      <span className="breadcrumb-sep">/</span>
                      <button
                        className="breadcrumb-link"
                        onClick={() => setSubpath(crumbs.slice(0, i + 1).join("/"))}
                      >
                        {c}
                      </button>
                    </span>
                  ))}
                </div>
              </div>
              {loading ? (
                <Loading />
              ) : tree.length === 0 ? (
                <Empty title={t("folders.emptyDir")} hint={t("folders.emptyDirHint")} />
              ) : (
                <div style={{ maxHeight: "min(380px, 42vh)", overflowY: "auto" }}>
                  <table className="table">
                    <thead>
                      <tr>
                        <th>{t("folders.colName")}</th>
                        <th style={{ width: 90 }}>{t("folders.colSize")}</th>
                        <th style={{ width: 150 }}>{t("folders.colMtime")}</th>
                      </tr>
                    </thead>
                    <tbody>
                      {treeTruncated > 0 && (
                        <tr>
                          <td colSpan={3} className="hint" style={{ padding: 10 }}>
                            <span>
                              {t("folders.treeTruncated", {
                                shown: tree.length,
                                rest: treeTruncated,
                              })}
                            </span>{" "}
                            <button
                              className="btn btn-sm"
                              onClick={() => setTreeLimit((v) => v + TREE_RENDER_LIMIT)}
                            >
                              {t("folders.treeLoadMore")}
                            </button>
                          </td>
                        </tr>
                      )}
                      {tree.map((n) => (
                        <tr key={n.path}>
                          <td>
                            {n.is_dir ? (
                              <button
                                className="tree-dir"
                                onClick={() => setSubpath(n.path)}
                                title={n.path}
                              >
                                <Icon name="chevron" size={13} /> {n.name}
                              </button>
                            ) : (
                              <span className="tree-file" title={n.path}>
                                <Icon name="file" size={14} />
                                <span className="truncate">{n.name}</span>
                              </span>
                            )}
                            {n.is_conflict && (
                              <span
                                className="badge badge-warn"
                                style={{ marginLeft: 8 }}
                              >
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
              )}
            </>
          ) : tab === "ignore" ? (
            <>
              {rules && (
                <div className="checkbox-row">
                  <input
                    type="checkbox"
                    id="rules-defaults"
                    checked={rules.use_defaults}
                    disabled={savingRules}
                    onChange={(e) => {
                      setRules({ ...rules, use_defaults: e.target.checked });
                      setRulesDirty(true);
                    }}
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
                  disabled={savingRules}
                  onChange={(e) => {
                    if (!rules) return;
                    setRules({ ...rules, custom: e.target.value.split("\n") });
                    setRulesDirty(true);
                  }}
                  placeholder={"*.tmp\nbuild/\nnode_modules/"}
                />
                <p className="hint">{t("folders.ignoreSyntax")}</p>
              </div>
            </>
          ) : (
            <>
              <p className="hint" style={{ marginBottom: 12 }}>
                {t("folders.shareHint")}
              </p>
              {devices.length === 0 ? (
                <Empty title={t("devices.empty")} hint={t("folders.noDevicesHint")} />
              ) : (
                devices.map((d) => (
                  <div className="checkbox-row" key={d.id}>
                    <input
                      type="checkbox"
                      id={`share-${d.id}`}
                      checked={shared.length === 0 || shared.includes(d.id)}
                      disabled={savingSharing}
                      onChange={(e) => {
                        const allIds = devices.map((x) => x.id);
                        const current = shared.length === 0 ? allIds : shared;
                        let next = e.target.checked
                          ? [...current, d.id]
                          : current.filter((x) => x !== d.id);
                        next = [...new Set(next)];
                        // 全选用空数组表示“所有已配对设备”，以后新配对设备也会共享
                        if (
                          allIds.length > 0 &&
                          allIds.every((id) => next.includes(id))
                        ) {
                          next = [];
                        }
                        setShared(next);
                        setSharingDirty(true);
                      }}
                    />
                    <label htmlFor={`share-${d.id}`}>
                      {d.name} <span className="mono card-meta">{d.fingerprint}</span>
                    </label>
                  </div>
                ))
              )}
            </>
          )}
        </div>
      </Modal>

      {confirmClose && (
        <ConfirmDialog
          title={t("folders.unsavedTitle")}
          message={t("folders.unsavedMessage")}
          confirmLabel={t("folders.discard")}
          danger
          onConfirm={onClose}
          onCancel={() => setConfirmClose(false)}
        />
      )}
    </>
  );
}
