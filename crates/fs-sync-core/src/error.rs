//! 错误类型。

use thiserror::Error;

pub type Result<T> = std::result::Result<T, PathError>;

/// 路径校验错误：对端发来的路径一律先过 `validate_path`。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PathError {
    #[error("路径为空")]
    Empty,
    #[error("路径必须是相对路径: {0}")]
    Absolute(String),
    #[error("路径包含非法组件（.. 或空）: {0}")]
    BadComponent(String),
    #[error("路径包含 Windows 保留名或非法字符: {0}")]
    ReservedOrIllegal(String),
    #[error("路径过长: {0}")]
    TooLong(String),
    #[error("组件不是合法 UTF-8")]
    NotUtf8,
}
