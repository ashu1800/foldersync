//! 设备身份：SecretKey 生成与持久化（0600 权限）。

use anyhow::{Context, Result};
use iroh::SecretKey;
use std::path::{Path, PathBuf};

/// 读取或首次生成设备密钥，存于 `<data_dir>/secret.key`（32 字节原始密钥）。
pub fn load_or_create_secret(data_dir: &Path) -> Result<SecretKey> {
    std::fs::create_dir_all(data_dir).context("创建数据目录失败")?;
    let file = secret_path(data_dir);
    if file.exists() {
        let bytes = std::fs::read(&file).context("读取密钥文件失败")?;
        anyhow::ensure!(bytes.len() == 32, "密钥文件损坏");
        let arr: [u8; 32] = bytes.try_into().unwrap();
        Ok(SecretKey::from_bytes(&arr))
    } else {
        let sk = SecretKey::generate();
        write_secret(&file, &sk)?;
        Ok(sk)
    }
}

/// 保存密钥（覆盖写），并收紧权限。
pub fn write_secret(file: &Path, sk: &SecretKey) -> Result<()> {
    std::fs::write(file, sk.to_bytes()).context("写入密钥失败")?;
    restrict_permissions(file);
    Ok(())
}

pub fn secret_path(data_dir: &Path) -> PathBuf {
    data_dir.join("secret.key")
}

/// 文件权限收紧：Unix 0600。Windows 上用户 profile 目录的默认 ACL 已限定为本人。
fn restrict_permissions(file: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_persists_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let sk1 = load_or_create_secret(tmp.path()).unwrap();
        let sk2 = load_or_create_secret(tmp.path()).unwrap();
        assert_eq!(sk1.to_bytes(), sk2.to_bytes());
        assert_eq!(sk1.public(), sk2.public());
    }

    #[cfg(unix)]
    #[test]
    fn secret_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        load_or_create_secret(tmp.path()).unwrap();
        let meta = std::fs::metadata(secret_path(tmp.path())).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }
}
