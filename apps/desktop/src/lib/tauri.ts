// 类型化 Tauri 调用封装（提示词 5.4：src/lib/tauri.ts）。
//
// 所有后端调用都从这里走，页面里不出现裸 invoke 字符串，
// 这样改命令名只改一处，TypeScript 也能在参数/返回值上把关。

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  ActivityDto,
  ActivityFilter,
  AddFolderRequest,
  ConflictDto,
  ConflictResolution,
  DeviceDto,
  ErrorDto,
  FolderDto,
  IgnoreRulesDto,
  OverviewDto,
  PairingCodeDto,
  PendingPairingDto,
  SettingsDto,
  SettingsPatch,
  TreeNodeDto,
} from "./bindings";

/** 后端错误统一形状（命令返回的 reject 值）。 */
export type CmdError = ErrorDto;

/** 把 unknown 收窄成 CmdError，便于界面展示。 */
export function asCmdError(e: unknown): CmdError {
  if (typeof e === "object" && e !== null && "code" in e && "message" in e) {
    return e as CmdError;
  }
  return { code: "unknown", message: String(e) };
}

/** 命令调用：失败时抛出 CmdError。 */
async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    throw asCmdError(e);
  }
}

// ---------- 概览 ----------

export const getOverview = () => call<OverviewDto>("get_overview");

// ---------- 设备 ----------

export const listDevices = () => call<DeviceDto[]>("list_devices");
export const createPairingCode = () => call<PairingCodeDto>("create_pairing_code");
export const acceptPairingCode = (code: string) =>
  call<DeviceDto>("accept_pairing_code", { code });
export const confirmPairing = (requestId: string, accept: boolean) =>
  call<void>("confirm_pairing", { requestId, accept });
export const listPendingPairings = () =>
  call<PendingPairingDto[]>("list_pending_pairings");
export const renameDevice = (id: string, name: string) =>
  call<void>("rename_device", { id, name });
export const removeDevice = (id: string) => call<void>("remove_device", { id });

// ---------- 文件夹 ----------

export const listFolders = () => call<FolderDto[]>("list_folders");
export const addFolder = (req: AddFolderRequest) =>
  call<FolderDto>("add_folder", { req });
export const removeFolder = (id: string) => call<void>("remove_folder", { id });
export const pauseFolder = (id: string) => call<void>("pause_folder", { id });
export const resumeFolder = (id: string) => call<void>("resume_folder", { id });
export const pauseAll = () => call<void>("pause_all");
export const resumeAll = () => call<void>("resume_all");
export const getFolderTree = (folderId: string, subpath?: string) =>
  call<TreeNodeDto[]>("get_folder_tree", { folderId, subpath: subpath ?? null });
export const getIgnoreRules = (folderId: string) =>
  call<IgnoreRulesDto>("get_ignore_rules", { folderId });
export const setIgnoreRules = (folderId: string, rules: IgnoreRulesDto) =>
  call<void>("set_ignore_rules", { folderId, rules });

// ---------- 活动 ----------

export const listActivity = (filter: ActivityFilter = {}) =>
  call<ActivityDto[]>("list_activity", { filter });

// ---------- 冲突 ----------

export const listConflicts = (folderId?: string) =>
  call<ConflictDto[]>("list_conflicts", { folderId: folderId ?? null });
export const resolveConflict = (conflictId: string, resolution: ConflictResolution) =>
  call<void>("resolve_conflict", { conflictId, resolution });

// ---------- 设置 ----------

export const getSettings = () => call<SettingsDto>("get_settings");
export const updateSettings = (patch: SettingsPatch) =>
  call<SettingsDto>("update_settings", { patch });
export const openLogDir = () => call<string>("open_log_dir");
export const getLogDir = () => call<string>("get_log_dir");
export const exportDiagnostics = (outDir?: string) =>
  call<string>("export_diagnostics", { outDir: outDir ?? null });
export const getLocalInfo = () => call<Record<string, unknown>>("get_local_info");

// ---------- 事件 ----------

/** 后端事件名与载荷类型（提示词 5.3）。 */
export interface EventMap {
  "device://status": DeviceDto;
  "folder://status": FolderDto;
  "sync://activity": ActivityDto;
  "pairing://request": PendingPairingDto;
  "conflict://new": ConflictDto;
  "sync://progress": {
    folder_id: string;
    path: string;
    done: number;
    total: number;
    direction: "upload" | "download";
  };
}

/** 订阅一个后端事件，返回取消订阅函数。 */
export function on<K extends keyof EventMap>(
  event: K,
  handler: (payload: EventMap[K]) => void,
): Promise<UnlistenFn> {
  return listen<EventMap[K]>(event, (e) => handler(e.payload));
}
