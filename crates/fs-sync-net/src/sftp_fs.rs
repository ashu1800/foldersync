//! 沙箱化的 SFTP 文件系统处理器。
//!
//! 只把 `root` 目录暴露给 SSH 对端；所有路径都做穿越校验，禁止绝对路径与 `..`。
//! daemon 的“对端拉 blob”和 relay 的“房间文件空间”共用这一份实现。

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use russh_sftp::server::{Handler, StatusReply};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// 一个 SFTP 会话的文件句柄。
enum HandleState {
    File(tokio::fs::File),
    Dir(Vec<File>),
}

/// 沙箱 SFTP 处理器。
pub struct FsSftpHandler {
    root: PathBuf,
    handles: HashMap<String, HandleState>,
    next_handle: u64,
}

impl FsSftpHandler {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        Ok(Self {
            root,
            handles: HashMap::new(),
            next_handle: 1,
        })
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, StatusReply> {
        let rel = path.trim().replace('\\', "/");
        let rel = rel.trim_start_matches('/');
        let candidate = Path::new(rel);
        for comp in candidate.components() {
            match comp {
                Component::Normal(_) | Component::CurDir => {}
                _ => {
                    return Err(StatusReply::new(StatusCode::PermissionDenied)
                        .with_message("路径不允许逃逸沙箱"));
                }
            }
        }
        let full = self.root.join(candidate);
        // 目标可能不存在：校验最近的已存在祖先
        let mut probe = full.as_path();
        while !probe.exists() {
            probe = probe.parent().unwrap_or(&self.root);
        }
        let real = probe.canonicalize().unwrap_or_else(|_| probe.to_path_buf());
        if !real.starts_with(&self.root) {
            return Err(
                StatusReply::new(StatusCode::PermissionDenied).with_message("路径不允许逃逸沙箱")
            );
        }
        Ok(full)
    }

    fn alloc_handle(&mut self, state: HandleState) -> String {
        let id = format!("h{}", self.next_handle);
        self.next_handle += 1;
        self.handles.insert(id.clone(), state);
        id
    }

    fn ok(&self, id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        }
    }
}

fn io_err(e: std::io::Error) -> StatusReply {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        std::io::ErrorKind::AlreadyExists => StatusCode::Failure,
        _ => StatusCode::Failure,
    };
    StatusReply::new(code).with_message(e.to_string())
}

impl Handler for FsSftpHandler {
    type Error = StatusReply;

    fn unimplemented(&self) -> Self::Error {
        StatusReply::new(StatusCode::OpUnsupported)
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = self.resolve(&filename)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(io_err)?;
        }
        let mut opts = tokio::fs::OpenOptions::new();
        opts.read(pflags.contains(OpenFlags::READ))
            .write(pflags.contains(OpenFlags::WRITE))
            .append(pflags.contains(OpenFlags::APPEND))
            .create(pflags.contains(OpenFlags::CREATE))
            .truncate(pflags.contains(OpenFlags::TRUNCATE));
        let file = opts.open(&path).await.map_err(io_err)?;
        let handle = self.alloc_handle(HandleState::File(file));
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        if let Some(HandleState::File(mut f)) = self.handles.remove(&handle) {
            let _ = f.flush().await;
        }
        Ok(self.ok(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let Some(HandleState::File(file)) = self.handles.get_mut(&handle) else {
            return Err(StatusReply::new(StatusCode::Failure).with_message("句柄无效"));
        };
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(io_err)?;
        let mut buf = vec![0u8; len as usize];
        let n = file.read(&mut buf).await.map_err(io_err)?;
        if n == 0 {
            return Err(StatusReply::new(StatusCode::Eof));
        }
        buf.truncate(n);
        Ok(Data { id, data: buf })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let Some(HandleState::File(file)) = self.handles.get_mut(&handle) else {
            return Err(StatusReply::new(StatusCode::Failure).with_message("句柄无效"));
        };
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(io_err)?;
        file.write_all(&data).await.map_err(io_err)?;
        Ok(self.ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let path = self.resolve(&path)?;
        let meta = tokio::fs::metadata(&path).await.map_err(io_err)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let Some(HandleState::File(file)) = self.handles.get(&handle) else {
            return Err(StatusReply::new(StatusCode::Failure).with_message("句柄无效"));
        };
        let meta = file.metadata().await.map_err(io_err)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = self.resolve(&path)?;
        if let Some(size) = attrs.size {
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .await
                .map_err(io_err)?;
            file.set_len(size).await.map_err(io_err)?;
        }
        if let Some(perms) = attrs.permissions {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(perms)).await;
            }
            #[cfg(not(unix))]
            {
                let _ = perms;
            }
        }
        Ok(self.ok(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        _handle: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        Ok(self.ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let path = self.resolve(&path)?;
        let mut rd = tokio::fs::read_dir(&path).await.map_err(io_err)?;
        let mut files = Vec::new();
        while let Some(entry) = rd.next_entry().await.map_err(io_err)? {
            let meta = entry.metadata().await.map_err(io_err)?;
            files.push(File::new(
                entry.file_name().to_string_lossy(),
                FileAttributes::from(&meta),
            ));
        }
        let handle = self.alloc_handle(HandleState::Dir(files));
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let Some(HandleState::Dir(files)) = self.handles.get_mut(&handle) else {
            return Err(StatusReply::new(StatusCode::Failure).with_message("目录句柄无效"));
        };
        if files.is_empty() {
            return Err(StatusReply::new(StatusCode::Eof));
        }
        let files = std::mem::take(files);
        Ok(Name { id, files })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let path = self.resolve(&filename)?;
        tokio::fs::remove_file(&path).await.map_err(io_err)?;
        Ok(self.ok(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = self.resolve(&path)?;
        tokio::fs::create_dir_all(&path).await.map_err(io_err)?;
        Ok(self.ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let path = self.resolve(&path)?;
        tokio::fs::remove_dir(&path).await.map_err(io_err)?;
        Ok(self.ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let path = self.resolve(&path)?;
        let shown = path
            .strip_prefix(&self.root)
            .map(|p| format!("/{}", p.to_string_lossy().replace('\\', "/")))
            .unwrap_or_else(|_| "/".into());
        Ok(Name {
            id,
            files: vec![File::dummy(shown)],
        })
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let old = self.resolve(&oldpath)?;
        let new = self.resolve(&newpath)?;
        tokio::fs::rename(&old, &new).await.map_err(io_err)?;
        Ok(self.ok(id))
    }

    async fn readlink(&mut self, _id: u32, _path: String) -> Result<Name, Self::Error> {
        Err(StatusReply::new(StatusCode::OpUnsupported).with_message("不支持符号链接"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_and_reads_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let mut h = FsSftpHandler::new(tmp.path().join("root")).unwrap();
        let handle = h
            .open(
                1,
                "blobs/abc".into(),
                OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::READ,
                FileAttributes::empty(),
            )
            .await
            .unwrap();
        h.write(2, handle.handle.clone(), 0, b"hello".to_vec())
            .await
            .unwrap();
        let data = h.read(3, handle.handle.clone(), 0, 16).await.unwrap();
        assert_eq!(data.data, b"hello");
        let attrs = h.stat(4, "blobs/abc".into()).await.unwrap();
        assert_eq!(attrs.attrs.size, Some(5));
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut h = FsSftpHandler::new(tmp.path().join("root")).unwrap();
        assert!(
            h.open(
                1,
                "../escape".into(),
                OpenFlags::CREATE | OpenFlags::WRITE,
                FileAttributes::empty()
            )
            .await
            .is_err()
        );
        assert!(h.stat(2, "C:/Windows/evil".into()).await.is_err());
    }

    #[tokio::test]
    async fn directory_listing_returns_entries_then_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("blobs")).unwrap();
        std::fs::write(root.join("blobs/a"), b"x").unwrap();
        let mut h = FsSftpHandler::new(&root).unwrap();
        let dir = h.opendir(1, "blobs".into()).await.unwrap();
        let listing = h.readdir(2, dir.handle.clone()).await.unwrap();
        assert_eq!(listing.files.len(), 1);
        assert_eq!(listing.files[0].filename, "a");
        assert!(h.readdir(3, dir.handle).await.is_err(), "读完后应返回 EOF");
    }
}
