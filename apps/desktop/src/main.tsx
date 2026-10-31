import React from "react";
import ReactDOM from "react-dom/client";
import { HashRouter } from "react-router-dom";
import App from "./App";
import "./i18n";
import "./styles.css";

// HashRouter：Tauri 打包后是 file:// 协议，BrowserRouter 的 history 路由会 404

class ErrorBoundary extends React.Component<
  { children: React.ReactNode },
  { error: Error | null }
> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  render() {
    if (!this.state.error) return this.props.children;
    const en = document.documentElement.lang === "en";
    return (
      <div style={{ padding: 32 }}>
        <div className="error-banner" role="alert">
          <div>
            <strong>{en ? "Something went wrong" : "界面出现异常"}</strong>
            <div className="card-meta" style={{ marginTop: 6 }}>
              {this.state.error.message}
            </div>
          </div>
          <button className="btn" onClick={() => window.location.reload()}>
            {en ? "Reload" : "重新加载"}
          </button>
        </div>
      </div>
    );
  }
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ErrorBoundary>
      <HashRouter>
        <App />
      </HashRouter>
    </ErrorBoundary>
  </React.StrictMode>,
);
