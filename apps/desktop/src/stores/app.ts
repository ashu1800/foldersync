// 全局状态（zustand）。
//
// 只放「多页面共享」的数据：概览、设备、文件夹、设置。
// 活动与冲突是列表页自己的事，留在页面内，避免 store 变成大杂烩。

import { create } from "zustand";
import type {
  ConflictDto,
  DeviceDto,
  FolderDto,
  OverviewDto,
  PendingPairingDto,
  SettingsDto,
} from "../lib/bindings";
import * as api from "../lib/tauri";

interface State {
  overview: OverviewDto | null;
  devices: DeviceDto[];
  folders: FolderDto[];
  settings: SettingsDto | null;
  pendingPairings: PendingPairingDto[];
  conflicts: ConflictDto[];

  loading: boolean;
  overviewLoaded: boolean;
  devicesLoaded: boolean;
  foldersLoaded: boolean;
  conflictsLoaded: boolean;
  overviewError: string | null;
  devicesError: string | null;
  foldersError: string | null;
  settingsError: string | null;
  conflictsError: string | null;

  refreshOverview: () => Promise<void>;
  refreshDevices: () => Promise<void>;
  refreshFolders: () => Promise<void>;
  refreshSettings: () => Promise<void>;
  refreshPendingPairings: () => Promise<void>;
  refreshConflicts: () => Promise<void>;
  refreshAll: () => Promise<void>;

  setDevice: (d: DeviceDto) => void;
  setFolder: (f: FolderDto) => void;
  addPendingPairing: (p: PendingPairingDto) => void;
  removePendingPairing: (id: string) => void;
  addConflict: (c: ConflictDto) => void;
}

export const useStore = create<State>((set, get) => ({
  overview: null,
  devices: [],
  folders: [],
  settings: null,
  pendingPairings: [],
  conflicts: [],
  loading: false,
  overviewLoaded: false,
  devicesLoaded: false,
  foldersLoaded: false,
  conflictsLoaded: false,
  overviewError: null,
  devicesError: null,
  foldersError: null,
  settingsError: null,
  conflictsError: null,

  refreshOverview: async () => {
    try {
      set({
        overview: await api.getOverview(),
        overviewLoaded: true,
        overviewError: null,
      });
    } catch (e) {
      set({ overviewError: api.asCmdError(e).message, overviewLoaded: true });
    }
  },

  refreshDevices: async () => {
    try {
      set({
        devices: await api.listDevices(),
        devicesLoaded: true,
        devicesError: null,
      });
    } catch (e) {
      set({ devicesError: api.asCmdError(e).message, devicesLoaded: true });
    }
  },

  refreshFolders: async () => {
    try {
      set({
        folders: await api.listFolders(),
        foldersLoaded: true,
        foldersError: null,
      });
    } catch (e) {
      set({ foldersError: api.asCmdError(e).message, foldersLoaded: true });
    }
  },

  refreshSettings: async () => {
    try {
      set({ settings: await api.getSettings(), settingsError: null });
    } catch (e) {
      set({ settingsError: api.asCmdError(e).message });
    }
  },

  refreshPendingPairings: async () => {
    try {
      set({ pendingPairings: await api.listPendingPairings() });
    } catch {
      // 配对请求列表失败不值得打扰用户
    }
  },

  refreshConflicts: async () => {
    try {
      set({
        conflicts: await api.listConflicts(),
        conflictsLoaded: true,
        conflictsError: null,
      });
    } catch (e) {
      set({ conflictsError: api.asCmdError(e).message });
    }
  },

  refreshAll: async () => {
    if (get().loading) return;
    set({ loading: true });
    await Promise.all([
      get().refreshOverview(),
      get().refreshDevices(),
      get().refreshFolders(),
      get().refreshSettings(),
      get().refreshPendingPairings(),
      get().refreshConflicts(),
    ]);
    set({ loading: false });
  },

  // 事件推送用的局部更新，避免每次事件都全量拉一遍
  setDevice: (d) =>
    set((s) => {
      const i = s.devices.findIndex((x) => x.id === d.id);
      if (i < 0) return { devices: [...s.devices, d] };
      const next = [...s.devices];
      next[i] = d;
      return { devices: next };
    }),

  setFolder: (f) =>
    set((s) => {
      const i = s.folders.findIndex((x) => x.id === f.id);
      if (i < 0) return { folders: [...s.folders, f] };
      const next = [...s.folders];
      next[i] = f;
      return { folders: next };
    }),

  addPendingPairing: (p) =>
    set((s) =>
      s.pendingPairings.some((x) => x.request_id === p.request_id)
        ? s
        : { pendingPairings: [...s.pendingPairings, p] },
    ),

  removePendingPairing: (id) =>
    set((s) => ({
      pendingPairings: s.pendingPairings.filter((x) => x.request_id !== id),
    })),

  addConflict: (c) =>
    set((s) =>
      s.conflicts.some((x) => x.id === c.id) ? s : { conflicts: [c, ...s.conflicts] },
    ),
}));
