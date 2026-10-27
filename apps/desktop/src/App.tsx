// 应用外壳：左侧导航 + 右侧内容区（提示词 5.1）。

import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { NavLink, Navigate, Route, Routes, useLocation } from "react-router-dom";
import * as api from "./lib/tauri";
import { useStore } from "./stores/app";
import { OverallBadge } from "./components/ui";
import Dashboard from "./pages/Dashboard";
import Devices from "./pages/Devices";
import Folders from "./pages/Folders";
import Activity from "./pages/Activity";
import Conflicts from "./pages/Conflicts";
import Settings from "./pages/Settings";

const NAV = [
  { to: "/", key: "overview", icon: "◱", end: true },
  { to: "/folders", key: "folders", icon: "▤" },
  { to: "/devices", key: "devices", icon: "◍" },
  { to: "/activity", key: "activity", icon: "◷" },
  { to: "/conflicts", key: "conflicts", icon: "⚠" },
  { to: "/settings", key: "settings", icon: "⚙" },
] as const;

export default function App() {
  const { t } = useTranslation();
  const location = useLocation();
  const {
    overview,
    conflicts,
    pendingPairings,
    refreshOverview,
    refreshPendingPairings,
    addPendingPairing,
    addConflict,
    setDevice,
    setFolder,
  } = useStore();

  // 事件桥接：后端推送 → store 更新（提示词 5.3）
  useEffect(() => {
    const unlisteners: Array<() => void> = [];
    let cancelled = false;

    const wire = async () => {
      const subs: Array<Promise<() => void>> = [
        api.on("device://status", (d) => {
          setDevice(d);
          void refreshOverview();
        }),
        api.on("folder://status", (f) => setFolder(f)),
        api.on("pairing://request", (p) => {
          addPendingPairing(p);
        }),
        api.on("conflict://new", (c) => {
          addConflict(c);
        }),
      ];
      for (const p of subs) {
        const fn = await p;
        if (cancelled) {
          fn();
        } else {
          unlisteners.push(fn);
        }
      }
    };

    void wire();
    return () => {
      cancelled = true;
      for (const fn of unlisteners) fn();
    };
  }, [addConflict, addPendingPairing, refreshOverview, setDevice, setFolder]);

  // 周期性拉概览：RTT / 在线状态靠事件推不全，低频兜底
  useEffect(() => {
    const id = window.setInterval(() => {
      void refreshOverview();
      void refreshPendingPairings();
    }, 5000);
    return () => window.clearInterval(id);
  }, [refreshOverview, refreshPendingPairings]);

  const conflictCount = conflicts.length;
  const pendingCount = pendingPairings.length;

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="sidebar-brand">
          <span aria-hidden>🔄</span>
          {t("app.name")}
        </div>

        {overview && (
          <div className="sidebar-status">
            <OverallBadge status={overview.status} />
            <div className="card-meta" style={{ marginTop: 8 }}>
              {overview.device_name}
            </div>
          </div>
        )}

        <nav className="sidebar-nav">
          {NAV.map((n) => (
            <NavLink
              key={n.to}
              to={n.to}
              end={"end" in n ? n.end : false}
              className={({ isActive }) => `nav-item ${isActive ? "active" : ""}`}
            >
              <span aria-hidden>{n.icon}</span>
              {t(`nav.${n.key}`)}
              {n.key === "conflicts" && conflictCount > 0 && (
                <span className="nav-badge">{conflictCount}</span>
              )}
              {n.key === "devices" && pendingCount > 0 && (
                <span className="nav-badge">{pendingCount}</span>
              )}
            </NavLink>
          ))}
        </nav>
      </aside>

      <main className="main" key={location.pathname}>
        <Routes>
          <Route path="/" element={<Dashboard />} />
          <Route path="/folders" element={<Folders />} />
          <Route path="/devices" element={<Devices />} />
          <Route path="/activity" element={<Activity />} />
          <Route path="/conflicts" element={<Conflicts />} />
          <Route path="/settings" element={<Settings />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </main>
    </div>
  );
}
