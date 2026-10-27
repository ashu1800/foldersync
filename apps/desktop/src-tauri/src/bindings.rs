//! TypeScript 类型导出（提示词 5.3：避免前后端手写类型漂移）。
//!
//! 运行 `cargo test -p foldersync-desktop export_bindings` 生成到
//! `apps/desktop/src/lib/bindings/`，前端直接 import，不再手写 interface。

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ts_rs::TS;

    /// 把所有 DTO 导出成 .ts 文件。
    ///
    /// 用 `cargo test` 而不是独立 bin：CI 里的 `cargo test` 就能顺带校验
    /// 类型定义仍可生成，不会出现「改了 Rust 忘了重新生成」的漂移。
    #[test]
    fn export_bindings() {
        let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../src/lib/bindings");
        std::fs::create_dir_all(&out).expect("创建 bindings 目录失败");

        // JSON 传输下 u64 就是普通 number，不是 bigint（TS 里 bigint 无法直接 JSON.parse）
        let mut cfg = ts_rs::Config::default();
        cfg = cfg.with_large_int("number");

        macro_rules! export {
            ($($t:ty),* $(,)?) => {
                $({
                    let name = <$t as TS>::name(&cfg);
                    let decl = <$t as TS>::decl(&cfg);
                    // ts-rs 的枚举声明不带 `export` 前缀（因为它假设按需内联），
                    // 但我们要每个类型独立成模块，必须显式导出。
                    let content = if decl.starts_with("export ") {
                        decl
                    } else {
                        format!("export {decl}")
                    };
                    let path = out.join(format!("{name}.ts"));
                    std::fs::write(&path, content)
                        .unwrap_or_else(|e| panic!("写 {} 失败: {e}", path.display()));
                })*
            };
        }

        // index.ts 单独在最后写——宏调用之后必须由此处继续执行

        export!(
            crate::dto::ErrorDto,
            crate::dto::OverallStatus,
            crate::dto::ConnKind,
            crate::dto::DeviceDto,
            crate::dto::FolderStatus,
            crate::dto::FolderDto,
            crate::dto::OverviewDto,
            crate::dto::ActivityKind,
            crate::dto::ActivityDto,
            crate::dto::ActivityFilter,
            crate::dto::ConflictDto,
            crate::dto::ConflictResolution,
            crate::dto::TreeNodeDto,
            crate::dto::FileState,
            crate::dto::PairingCodeDto,
            crate::dto::PendingPairingDto,
            crate::dto::InviteDto,
            crate::dto::IgnoreRulesDto,
            crate::dto::NetworkMode,
            crate::dto::SettingsDto,
            crate::dto::SettingsPatch,
            crate::dto::AddFolderRequest,
            crate::dto::ProgressDto,
            crate::dto::Direction,
            crate::dto::DeviceStatusEvent,
            crate::dto::FolderStatusEvent,
        );

        // index.ts 统一再导出，前端只 import 一个入口
        let mut index = String::from("// 本文件由 cargo test 自动生成，请勿手改。\n\n");
        for name in [
            "ErrorDto",
            "OverallStatus",
            "ConnKind",
            "DeviceDto",
            "FolderStatus",
            "FolderDto",
            "OverviewDto",
            "ActivityKind",
            "ActivityDto",
            "ActivityFilter",
            "ConflictDto",
            "ConflictResolution",
            "TreeNodeDto",
            "FileState",
            "PairingCodeDto",
            "PendingPairingDto",
            "InviteDto",
            "IgnoreRulesDto",
            "NetworkMode",
            "SettingsDto",
            "SettingsPatch",
            "AddFolderRequest",
            "ProgressDto",
            "Direction",
            "DeviceStatusEvent",
            "FolderStatusEvent",
        ] {
            index.push_str(&format!("export type {{ {name} }} from \"./{name}\";\n"));
        }
        std::fs::write(out.join("index.ts"), index).expect("写 index.ts 失败");
    }
}
