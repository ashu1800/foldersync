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
    ///
    /// **必须用 `TS::export_to_string`**，不能自己拿 `TS::decl` 拼：
    /// ts-rs 的文件是「文件头 + docs + imports + export type ...」四段拼起来的，
    /// `decl()` 只有最后一段。早期自己拼导致 rustdoc 全部丢失，而且不同环境
    /// （本地有增量缓存 / CI 干净构建）拼出来的结果还不一样，CI 的
    /// `git diff --exit-code` 永远失败。官方函数负责完整拼装，且结果稳定。
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
                    let content = <$t as TS>::export_to_string(&cfg)
                        .unwrap_or_else(|e| panic!("生成 {name} 失败: {e}"));
                    let path = out.join(format!("{name}.ts"));
                    std::fs::write(&path, content)
                        .unwrap_or_else(|e| panic!("写 {} 失败: {e}", path.display()));
                })*
            };
        }

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

        // index.ts 汇总，前端可以 `import { DeviceDto } from "../lib/bindings"`
        let mut index = String::new();
        for t in [
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
            index.push_str(&format!("export type {{ {t} }} from \"./{t}\";\n"));
        }
        std::fs::write(out.join("index.ts"), index).expect("写 index.ts 失败");
    }
}
