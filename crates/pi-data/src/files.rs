//! R11 项目文件能力：受根目录约束的浏览、读取、模糊索引与上传。
//!
//! 所有公开操作都从一个已 canonicalize 的项目根开始，只接受相对路径。目录链接与
//! Windows reparse point 一律不跟随；写入使用同目录临时文件发布，避免覆盖失败时丢旧文件。

use std::{
    collections::{HashSet, VecDeque},
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, BufRead, BufReader},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

use thiserror::Error;

use crate::{
    composer::{FileIndex, build_entries_from_files},
    fs_util::{hide_console_window, is_link_like, write_atomic_with},
};

pub const TEXT_PREVIEW_MAX_BYTES: u64 = 256 * 1024;
pub const IMAGE_PREVIEW_MAX_BYTES: u64 = 10 * 1024 * 1024;
pub const IMAGE_PREVIEW_MAX_PIXELS: u64 = 40_000_000;
pub const IMAGE_PREVIEW_MAX_SIDE: u32 = 16_384;
pub const IMAGE_PREVIEW_MAX_GIF_FRAMES: u64 = 200;
pub const MAX_UPLOAD_FILE_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_UPLOAD_TOTAL_BYTES: u64 = 100 * 1024 * 1024;
pub const FILE_INDEX_HARD_LIMIT: usize = 50_000;
pub const FILE_INDEX_MAX_DEPTH: usize = 8;
pub const FILE_SEARCH_RESULT_LIMIT: usize = 20;
pub const FILE_TREE_LIMIT: usize = 20_000;

const IGNORED_NAMES: &[&str] = &[
    "node_modules",
    ".git",
    ".next",
    "dist",
    "build",
    "__pycache__",
    ".turbo",
    ".cache",
    "coverage",
    ".pytest_cache",
    ".mypy_cache",
    "target",
    "vendor",
    ".DS_Store",
];

#[derive(Debug, Error)]
pub enum FileAccessError {
    #[error("项目目录不存在或不可访问: {path}")]
    InvalidRoot { path: PathBuf },
    #[error("路径必须是项目根目录内的相对路径: {path}")]
    InvalidRelativePath { path: PathBuf },
    #[error("路径包含符号链接或 reparse point，已拒绝: {path}")]
    LinkedPath { path: PathBuf },
    #[error("路径越过项目根目录，已拒绝: {path}")]
    OutsideRoot { path: PathBuf },
    #[error("目标不存在: {path}")]
    NotFound { path: PathBuf },
    #[error("目标不是目录: {path}")]
    NotDirectory { path: PathBuf },
    #[error("目标不是普通文件: {path}")]
    NotFile { path: PathBuf },
    #[error("文件超过预览上限（{limit} 字节）: {path}")]
    TooLarge { path: PathBuf, limit: u64 },
    #[error("文件不是可预览的 UTF-8 文本: {path}")]
    BinaryText { path: PathBuf },
    #[error("不支持的图片格式: {path}")]
    UnsupportedImage { path: PathBuf },
    #[error("图片尺寸超过安全预览上限（{width}×{height}）: {path}")]
    ImageDimensionsTooLarge {
        path: PathBuf,
        width: u32,
        height: u32,
    },
    #[error("未选择上传文件")]
    EmptyUploadSelection,
    #[error("文件名无效: {name}")]
    InvalidUploadName { name: String },
    #[error("同一批上传中存在重复文件名: {name}")]
    DuplicateUploadName { name: String },
    #[error("单个上传文件超过 25 MiB: {path}")]
    UploadFileTooLarge { path: PathBuf },
    #[error("一批上传文件总计超过 100 MiB")]
    UploadBatchTooLarge,
    #[error("目标文件已存在: {name}")]
    UploadConflict { name: String },
    #[error("不能覆盖目录、符号链接或 reparse point: {path}")]
    NonReplaceable { path: PathBuf },
    #[error("文件系统操作 {operation} 失败（{path}）: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileNodeKind {
    Directory,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNode {
    pub name: String,
    pub relative_path: PathBuf,
    pub kind: FileNodeKind,
    pub children: Vec<FileNode>,
}

impl FileNode {
    pub fn is_dir(&self) -> bool {
        self.kind == FileNodeKind::Directory
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileTreeSnapshot {
    pub nodes: Vec<FileNode>,
    pub truncated: bool,
    pub skipped_links: usize,
    pub skipped_unreadable: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
    Ico,
}

impl ImageKind {
    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Bmp => "image/bmp",
            Self::Ico => "image/x-icon",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFileContent {
    pub text: String,
    pub language: &'static str,
    pub size: u64,
    pub lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageFileContent {
    pub bytes: Vec<u8>,
    pub kind: ImageKind,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    Text(TextFileContent),
    Image(ImageFileContent),
    Unsupported { size: u64, reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadConflictStrategy {
    Error,
    Overwrite,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadCandidate {
    pub source: PathBuf,
    pub name: OsString,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UploadInspection {
    pub candidates: Vec<UploadCandidate>,
    pub conflicts: Vec<String>,
    pub non_replaceable: Vec<String>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadItemError {
    pub name: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UploadReport {
    pub uploaded: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<UploadItemError>,
}

#[derive(Debug, Clone)]
pub struct ProjectFiles {
    root: PathBuf,
}

impl ProjectFiles {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, FileAccessError> {
        let requested = root.as_ref().to_path_buf();
        let metadata = fs::metadata(&requested).map_err(|source| {
            FileAccessError::InvalidRoot {
                path: requested.clone(),
            }
            .with_source("读取项目目录", source)
        })?;
        if !metadata.is_dir() {
            return Err(FileAccessError::InvalidRoot { path: requested });
        }
        let root = dunce::canonicalize(&requested)
            .map_err(|_| FileAccessError::InvalidRoot { path: requested })?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn scan_tree(&self) -> Result<FileTreeSnapshot, FileAccessError> {
        let mut snapshot = FileTreeSnapshot::default();
        snapshot.nodes =
            self.scan_directory_recursive(Path::new(""), 0, &mut snapshot, &mut 0_usize)?;
        Ok(snapshot)
    }

    pub fn build_index(&self) -> Result<FileIndex, FileAccessError> {
        if let Some(index) = self.git_index()? {
            return Ok(index);
        }
        self.walk_index()
    }

    pub fn read(&self, relative: impl AsRef<Path>) -> Result<FileContent, FileAccessError> {
        let (path, metadata) = self.resolve_existing(relative.as_ref(), false)?;
        if !metadata.is_file() {
            return Err(FileAccessError::NotFile { path });
        }
        let size = metadata.len();
        if let Some(kind) = image_kind_from_path(&path) {
            if size > IMAGE_PREVIEW_MAX_BYTES {
                return Err(FileAccessError::TooLarge {
                    path,
                    limit: IMAGE_PREVIEW_MAX_BYTES,
                });
            }
            let bytes = fs::read(&path).map_err(|source| FileAccessError::Io {
                operation: "读取图片",
                path: path.clone(),
                source,
            })?;
            if !image_magic_matches(kind, &bytes) {
                return Err(FileAccessError::UnsupportedImage { path });
            }
            let (width, height) = image_dimensions(kind, &bytes)
                .ok_or_else(|| FileAccessError::UnsupportedImage { path: path.clone() })?;
            let frames = if kind == ImageKind::Gif {
                gif_frame_count(&bytes)
                    .ok_or_else(|| FileAccessError::UnsupportedImage { path: path.clone() })?
            } else {
                1
            };
            if width > IMAGE_PREVIEW_MAX_SIDE
                || height > IMAGE_PREVIEW_MAX_SIDE
                || frames > IMAGE_PREVIEW_MAX_GIF_FRAMES
                || u64::from(width)
                    .saturating_mul(u64::from(height))
                    .saturating_mul(frames)
                    > IMAGE_PREVIEW_MAX_PIXELS
            {
                return Err(FileAccessError::ImageDimensionsTooLarge {
                    path,
                    width,
                    height,
                });
            }
            return Ok(FileContent::Image(ImageFileContent { bytes, kind, size }));
        }
        if is_active_or_unsupported_extension(&path) {
            return Ok(FileContent::Unsupported {
                size,
                reason: "该格式不在原生安全预览范围内".to_owned(),
            });
        }
        if size > TEXT_PREVIEW_MAX_BYTES {
            return Err(FileAccessError::TooLarge {
                path,
                limit: TEXT_PREVIEW_MAX_BYTES,
            });
        }
        let bytes = fs::read(&path).map_err(|source| FileAccessError::Io {
            operation: "读取文本",
            path: path.clone(),
            source,
        })?;
        if bytes.contains(&0) {
            return Err(FileAccessError::BinaryText { path });
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| FileAccessError::BinaryText { path: path.clone() })?;
        let lines = text.lines().count().max(1);
        Ok(FileContent::Text(TextFileContent {
            language: language_for_path(&path),
            size,
            lines,
            text,
        }))
    }

    pub fn inspect_upload(
        &self,
        sources: impl IntoIterator<Item = PathBuf>,
        target_dir: impl AsRef<Path>,
    ) -> Result<UploadInspection, FileAccessError> {
        let (directory, metadata) = self.resolve_existing(target_dir.as_ref(), true)?;
        if !metadata.is_dir() {
            return Err(FileAccessError::NotDirectory { path: directory });
        }
        let mut seen = HashSet::new();
        let mut inspection = UploadInspection::default();
        for source in sources {
            let metadata =
                fs::symlink_metadata(&source).map_err(|source_error| FileAccessError::Io {
                    operation: "读取上传源文件",
                    path: source.clone(),
                    source: source_error,
                })?;
            if !metadata.is_file() || is_link_like(&metadata) {
                return Err(FileAccessError::NotFile { path: source });
            }
            if metadata.len() > MAX_UPLOAD_FILE_BYTES {
                return Err(FileAccessError::UploadFileTooLarge { path: source });
            }
            let name = source
                .file_name()
                .ok_or_else(|| FileAccessError::InvalidUploadName {
                    name: source.display().to_string(),
                })?
                .to_os_string();
            validate_upload_name(&name)?;
            let identity = upload_name_identity(&name);
            if !seen.insert(identity) {
                return Err(FileAccessError::DuplicateUploadName {
                    name: name.to_string_lossy().into_owned(),
                });
            }
            inspection.total_bytes = inspection.total_bytes.saturating_add(metadata.len());
            if inspection.total_bytes > MAX_UPLOAD_TOTAL_BYTES {
                return Err(FileAccessError::UploadBatchTooLarge);
            }
            let destination = directory.join(&name);
            if let Ok(target_metadata) = fs::symlink_metadata(&destination) {
                let name_string = name.to_string_lossy().into_owned();
                inspection.conflicts.push(name_string.clone());
                if !target_metadata.is_file() || is_link_like(&target_metadata) {
                    inspection.non_replaceable.push(name_string);
                }
            }
            inspection.candidates.push(UploadCandidate {
                source,
                name,
                size: metadata.len(),
            });
        }
        if inspection.candidates.is_empty() {
            return Err(FileAccessError::EmptyUploadSelection);
        }
        Ok(inspection)
    }

    pub fn upload(
        &self,
        inspection: &UploadInspection,
        target_dir: impl AsRef<Path>,
        strategy: UploadConflictStrategy,
    ) -> Result<UploadReport, FileAccessError> {
        let (directory, metadata) = self.resolve_existing(target_dir.as_ref(), true)?;
        if !metadata.is_dir() {
            return Err(FileAccessError::NotDirectory { path: directory });
        }
        if strategy == UploadConflictStrategy::Error {
            for candidate in &inspection.candidates {
                let destination = directory.join(&candidate.name);
                if fs::symlink_metadata(&destination).is_ok() {
                    return Err(FileAccessError::UploadConflict {
                        name: candidate.name.to_string_lossy().into_owned(),
                    });
                }
            }
        }

        let mut report = UploadReport::default();
        for candidate in &inspection.candidates {
            let name = candidate.name.to_string_lossy().into_owned();
            let destination = directory.join(&candidate.name);
            let current_target = fs::symlink_metadata(&destination).ok();
            if current_target.is_some() && strategy == UploadConflictStrategy::Skip {
                report.skipped.push(name);
                continue;
            }
            if current_target
                .as_ref()
                .is_some_and(|metadata| !metadata.is_file() || is_link_like(metadata))
            {
                report.errors.push(UploadItemError {
                    name,
                    error: "不能覆盖目录、符号链接或 reparse point".to_owned(),
                });
                continue;
            }
            match publish_source_atomic(&candidate.source, &destination) {
                Ok(()) => report.uploaded.push(name),
                Err(error) => report.errors.push(UploadItemError {
                    name,
                    error: error.to_string(),
                }),
            }
        }
        Ok(report)
    }

    fn scan_directory_recursive(
        &self,
        relative: &Path,
        depth: usize,
        snapshot: &mut FileTreeSnapshot,
        visited: &mut usize,
    ) -> Result<Vec<FileNode>, FileAccessError> {
        if *visited >= FILE_TREE_LIMIT {
            snapshot.truncated = true;
            return Ok(Vec::new());
        }
        let resolved = self.resolve_existing(relative, true);
        let (directory, metadata) = match resolved {
            Ok(resolved) => resolved,
            Err(FileAccessError::LinkedPath { .. }) if !relative.as_os_str().is_empty() => {
                snapshot.skipped_links += 1;
                return Ok(Vec::new());
            }
            Err(_) if !relative.as_os_str().is_empty() => {
                snapshot.skipped_unreadable += 1;
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        if !metadata.is_dir() {
            return Err(FileAccessError::NotDirectory { path: directory });
        }
        let read_dir = match fs::read_dir(&directory) {
            Ok(read_dir) => read_dir,
            Err(_) => {
                snapshot.skipped_unreadable += 1;
                return Ok(Vec::new());
            }
        };
        let mut entries = Vec::new();
        for entry in read_dir {
            if *visited >= FILE_TREE_LIMIT {
                snapshot.truncated = true;
                break;
            }
            let Ok(entry) = entry else {
                snapshot.skipped_unreadable += 1;
                continue;
            };
            let name = entry.file_name();
            if is_ignored_name(&name) {
                continue;
            }
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                snapshot.skipped_unreadable += 1;
                continue;
            };
            if is_link_like(&metadata) {
                snapshot.skipped_links += 1;
                continue;
            }
            *visited += 1;
            let child_relative = relative.join(&name);
            if metadata.is_dir() {
                let children = if depth < FILE_INDEX_MAX_DEPTH {
                    self.scan_directory_recursive(&child_relative, depth + 1, snapshot, visited)?
                } else {
                    snapshot.truncated = true;
                    Vec::new()
                };
                entries.push(FileNode {
                    name: name.to_string_lossy().into_owned(),
                    relative_path: child_relative,
                    kind: FileNodeKind::Directory,
                    children,
                });
            } else if metadata.is_file() {
                entries.push(FileNode {
                    name: name.to_string_lossy().into_owned(),
                    relative_path: child_relative,
                    kind: FileNodeKind::File,
                    children: Vec::new(),
                });
            }
        }
        entries.sort_by(|left, right| {
            right
                .is_dir()
                .cmp(&left.is_dir())
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(entries)
    }

    fn git_index(&self) -> Result<Option<FileIndex>, FileAccessError> {
        let mut child = match hide_console_window(&mut Command::new("git"))
            .arg("-C")
            .arg(&self.root)
            .args([
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ])
            .env("LC_ALL", "C")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return Ok(None),
        };
        let Some(stdout) = child.stdout.take() else {
            return Ok(None);
        };
        let mut files = Vec::new();
        let mut records = 0_usize;
        let mut truncated = false;
        let mut reader = BufReader::new(stdout);
        loop {
            let mut record = Vec::new();
            match reader.read_until(0, &mut record) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(None);
                }
            }
            if record.last() == Some(&0) {
                record.pop();
            }
            if record.is_empty() {
                continue;
            }
            records += 1;
            if records > FILE_INDEX_HARD_LIMIT {
                truncated = true;
                let _ = child.kill();
                break;
            }
            let relative = PathBuf::from(String::from_utf8_lossy(&record).into_owned());
            if validate_relative_path(&relative).is_ok()
                && !path_contains_ignored_component(&relative)
            {
                files.push(relative.to_string_lossy().replace('\\', "/"));
                if files.len() >= crate::composer::FILE_INDEX_LIMIT {
                    truncated = true;
                    let _ = child.kill();
                    break;
                }
            }
        }
        drop(reader);
        let status = child.wait().ok();
        if !truncated && !status.is_some_and(|status| status.success()) {
            return Ok(None);
        }
        files.sort();
        files.dedup();
        Ok(Some(FileIndex {
            entries: build_entries_from_files(files),
            truncated,
        }))
    }

    fn walk_index(&self) -> Result<FileIndex, FileAccessError> {
        let mut queue = VecDeque::from([(PathBuf::new(), 0_usize)]);
        let mut files = Vec::new();
        let mut visited = 0_usize;
        let mut truncated = false;
        while let Some((relative, depth)) = queue.pop_front() {
            let (directory, metadata) = match self.resolve_existing(&relative, true) {
                Ok(resolved) => resolved,
                Err(_) if !relative.as_os_str().is_empty() => continue,
                Err(error) => return Err(error),
            };
            if !metadata.is_dir() {
                continue;
            }
            let Ok(read_dir) = fs::read_dir(&directory) else {
                continue;
            };
            let mut children = read_dir.flatten().collect::<Vec<_>>();
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                if visited >= FILE_INDEX_HARD_LIMIT {
                    truncated = true;
                    break;
                }
                let name = child.file_name();
                if is_ignored_name(&name) {
                    continue;
                }
                let path = child.path();
                let Ok(metadata) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if is_link_like(&metadata) {
                    continue;
                }
                visited += 1;
                let child_relative = relative.join(&name);
                if metadata.is_dir() {
                    if depth < FILE_INDEX_MAX_DEPTH {
                        queue.push_back((child_relative, depth + 1));
                    } else {
                        truncated = true;
                    }
                } else if metadata.is_file() {
                    files.push(child_relative.to_string_lossy().replace('\\', "/"));
                    if files.len() >= crate::composer::FILE_INDEX_LIMIT {
                        truncated = true;
                        break;
                    }
                }
            }
            if files.len() >= crate::composer::FILE_INDEX_LIMIT || visited >= FILE_INDEX_HARD_LIMIT
            {
                break;
            }
        }
        files.sort();
        files.dedup();
        Ok(FileIndex {
            entries: build_entries_from_files(files),
            truncated,
        })
    }

    fn resolve_existing(
        &self,
        relative: &Path,
        directory_ok: bool,
    ) -> Result<(PathBuf, fs::Metadata), FileAccessError> {
        validate_relative_path(relative)?;
        let mut current = self.root.clone();
        for component in relative.components() {
            let Component::Normal(part) = component else {
                return Err(FileAccessError::InvalidRelativePath {
                    path: relative.to_path_buf(),
                });
            };
            current.push(part);
            let metadata = fs::symlink_metadata(&current).map_err(|source| {
                if source.kind() == io::ErrorKind::NotFound {
                    FileAccessError::NotFound {
                        path: current.clone(),
                    }
                } else {
                    FileAccessError::Io {
                        operation: "读取路径元数据",
                        path: current.clone(),
                        source,
                    }
                }
            })?;
            if is_link_like(&metadata) {
                return Err(FileAccessError::LinkedPath {
                    path: current.clone(),
                });
            }
        }
        let metadata = fs::symlink_metadata(&current).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                FileAccessError::NotFound {
                    path: current.clone(),
                }
            } else {
                FileAccessError::Io {
                    operation: "读取路径元数据",
                    path: current.clone(),
                    source,
                }
            }
        })?;
        if !directory_ok && !metadata.is_file() {
            return Err(FileAccessError::NotFile { path: current });
        }
        let canonical = dunce::canonicalize(&current).map_err(|source| FileAccessError::Io {
            operation: "解析真实路径",
            path: current.clone(),
            source,
        })?;
        if !path_is_within_root(&canonical, &self.root) {
            return Err(FileAccessError::OutsideRoot { path: canonical });
        }
        Ok((canonical, metadata))
    }
}

impl FileAccessError {
    fn with_source(self, operation: &'static str, source: io::Error) -> Self {
        match self {
            Self::InvalidRoot { path } => Self::Io {
                operation,
                path,
                source,
            },
            other => other,
        }
    }
}

pub fn validate_upload_name(name: &OsStr) -> Result<(), FileAccessError> {
    let text = name.to_string_lossy();
    if text.is_empty()
        || text == "."
        || text == ".."
        || text.contains('\0')
        || text.contains('/')
        || text.contains('\\')
        || Path::new(name).file_name() != Some(name)
        || is_windows_reserved_name(&text)
        || text.ends_with(['.', ' '])
        || (cfg!(windows) && text.contains(':'))
    {
        return Err(FileAccessError::InvalidUploadName {
            name: text.into_owned(),
        });
    }
    Ok(())
}

pub fn language_for_path(path: &Path) -> &'static str {
    let base = path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if base == "dockerfile" || base.starts_with("dockerfile.") {
        return "text";
    }
    if base == ".env" || base.starts_with(".env.") || base == ".gitignore" {
        return "bash";
    }
    match path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .as_deref()
    {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("js" | "jsx" | "mjs" | "cjs") => "javascript",
        Some("ts") => "typescript",
        Some("tsx") => "typescript",
        Some("json" | "jsonl") => "json",
        Some("yaml" | "yml") => "yaml",
        Some("sh" | "bash" | "zsh") => "bash",
        Some("toml" | "md" | "mdx" | "html" | "htm" | "css" | "xml" | "txt") => "text",
        _ => "text",
    }
}

fn validate_relative_path(path: &Path) -> Result<(), FileAccessError> {
    if path.is_absolute() {
        return Err(FileAccessError::InvalidRelativePath {
            path: path.to_path_buf(),
        });
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(FileAccessError::InvalidRelativePath {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(())
}

fn is_ignored_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    IGNORED_NAMES.iter().any(|ignored| name == *ignored) || name.ends_with(".pyc")
}

fn path_contains_ignored_component(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => is_ignored_name(name),
        _ => true,
    })
}

fn image_kind_from_path(path: &Path) -> Option<ImageKind> {
    match path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .as_deref()
    {
        Some("png") => Some(ImageKind::Png),
        Some("jpg" | "jpeg") => Some(ImageKind::Jpeg),
        Some("gif") => Some(ImageKind::Gif),
        Some("webp") => Some(ImageKind::Webp),
        Some("bmp") => Some(ImageKind::Bmp),
        Some("ico") => Some(ImageKind::Ico),
        _ => None,
    }
}

fn image_magic_matches(kind: ImageKind, bytes: &[u8]) -> bool {
    match kind {
        ImageKind::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        ImageKind::Jpeg => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        ImageKind::Gif => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        ImageKind::Webp => {
            bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP"
        }
        ImageKind::Bmp => bytes.starts_with(b"BM"),
        ImageKind::Ico => bytes.starts_with(&[0, 0, 1, 0]),
    }
}

fn image_dimensions(kind: ImageKind, bytes: &[u8]) -> Option<(u32, u32)> {
    match kind {
        ImageKind::Png if bytes.len() >= 24 => Some((
            u32::from_be_bytes(bytes[16..20].try_into().ok()?),
            u32::from_be_bytes(bytes[20..24].try_into().ok()?),
        )),
        ImageKind::Gif if bytes.len() >= 10 => Some((
            u32::from(u16::from_le_bytes(bytes[6..8].try_into().ok()?)),
            u32::from(u16::from_le_bytes(bytes[8..10].try_into().ok()?)),
        )),
        ImageKind::Bmp if bytes.len() >= 26 => {
            let dib_size = u32::from_le_bytes(bytes[14..18].try_into().ok()?);
            if dib_size == 12 {
                Some((
                    u32::from(u16::from_le_bytes(bytes[18..20].try_into().ok()?)),
                    u32::from(u16::from_le_bytes(bytes[20..22].try_into().ok()?)),
                ))
            } else {
                let width = i32::from_le_bytes(bytes[18..22].try_into().ok()?);
                let height = i32::from_le_bytes(bytes[22..26].try_into().ok()?);
                Some((width.unsigned_abs(), height.unsigned_abs()))
            }
        }
        ImageKind::Ico if bytes.len() >= 8 => Some((
            if bytes[6] == 0 {
                256
            } else {
                u32::from(bytes[6])
            },
            if bytes[7] == 0 {
                256
            } else {
                u32::from(bytes[7])
            },
        )),
        ImageKind::Webp => webp_dimensions(bytes),
        ImageKind::Jpeg => jpeg_dimensions(bytes),
        _ => None,
    }
    .filter(|(width, height)| *width > 0 && *height > 0)
}

fn gif_frame_count(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 13 {
        return None;
    }
    let packed = bytes[10];
    let mut offset = 13_usize;
    if packed & 0x80 != 0 {
        let table_bytes = 3_usize.checked_mul(1_usize << (usize::from(packed & 0x07) + 1))?;
        offset = offset.checked_add(table_bytes)?;
    }
    let mut frames = 0_u64;
    while offset < bytes.len() {
        match bytes[offset] {
            0x3b => return (frames > 0).then_some(frames),
            0x21 => {
                offset += 2;
                skip_gif_sub_blocks(bytes, &mut offset)?;
            }
            0x2c => {
                frames = frames.saturating_add(1);
                if frames > IMAGE_PREVIEW_MAX_GIF_FRAMES {
                    return Some(frames);
                }
                let descriptor = bytes.get(offset + 1..offset + 10)?;
                let local_packed = descriptor[8];
                offset += 10;
                if local_packed & 0x80 != 0 {
                    let table_bytes =
                        3_usize.checked_mul(1_usize << (usize::from(local_packed & 0x07) + 1))?;
                    offset = offset.checked_add(table_bytes)?;
                }
                offset = offset.checked_add(1)?;
                skip_gif_sub_blocks(bytes, &mut offset)?;
            }
            _ => return None,
        }
    }
    None
}

fn skip_gif_sub_blocks(bytes: &[u8], offset: &mut usize) -> Option<()> {
    loop {
        let length = usize::from(*bytes.get(*offset)?);
        *offset = offset.checked_add(1)?;
        if length == 0 {
            return Some(());
        }
        *offset = offset.checked_add(length)?;
        if *offset > bytes.len() {
            return None;
        }
    }
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let chunk = bytes.get(12..16)?;
    match chunk {
        b"VP8X" if bytes.len() >= 30 => Some((
            1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]),
            1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]),
        )),
        b"VP8 " if bytes.len() >= 30 && bytes[23..26] == [0x9d, 0x01, 0x2a] => Some((
            u32::from(u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff),
            u32::from(u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff),
        )),
        b"VP8L" if bytes.len() >= 25 && bytes[20] == 0x2f => {
            let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        _ => None,
    }
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut offset = 2_usize;
    while offset + 4 <= bytes.len() {
        while offset < bytes.len() && bytes[offset] != 0xff {
            offset += 1;
        }
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length = usize::from(u16::from_be_bytes(
            bytes.get(offset..offset + 2)?.try_into().ok()?,
        ));
        if length < 2 || offset + length > bytes.len() {
            return None;
        }
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            if length < 7 {
                return None;
            }
            let height = u32::from(u16::from_be_bytes(
                bytes.get(offset + 3..offset + 5)?.try_into().ok()?,
            ));
            let width = u32::from(u16::from_be_bytes(
                bytes.get(offset + 5..offset + 7)?.try_into().ok()?,
            ));
            return Some((width, height));
        }
        offset += length;
    }
    None
}

fn is_active_or_unsupported_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .map(|extension| extension.to_string_lossy().to_lowercase())
            .as_deref(),
        Some("svg" | "pdf" | "docx" | "html" | "htm")
    )
}

fn upload_name_identity(name: &OsStr) -> String {
    let text = name.to_string_lossy();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.into_owned()
    }
}

fn is_windows_reserved_name(name: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(['.', ' '])
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .and_then(|suffix| suffix.parse::<u8>().ok())
            .is_some_and(|number| (1..=9).contains(&number))
        || stem
            .strip_prefix("LPT")
            .and_then(|suffix| suffix.parse::<u8>().ok())
            .is_some_and(|number| (1..=9).contains(&number))
}

fn path_is_within_root(path: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        crate::project_identity_key(path).starts_with(&format!(
            "{}\\",
            crate::project_identity_key(root).trim_end_matches('\\')
        )) || crate::project_identity_key(path) == crate::project_identity_key(root)
    } else {
        path == root || path.starts_with(root)
    }
}

fn publish_source_atomic(source: &Path, destination: &Path) -> Result<(), FileAccessError> {
    let source_metadata =
        fs::symlink_metadata(source).map_err(|source_error| FileAccessError::Io {
            operation: "读取上传源文件",
            path: source.to_path_buf(),
            source: source_error,
        })?;
    if !source_metadata.is_file() || is_link_like(&source_metadata) {
        return Err(FileAccessError::NotFile {
            path: source.to_path_buf(),
        });
    }
    if source_metadata.len() > MAX_UPLOAD_FILE_BYTES {
        return Err(FileAccessError::UploadFileTooLarge {
            path: source.to_path_buf(),
        });
    }
    ensure_replaceable(destination)?;
    write_atomic_with(
        destination,
        |temp, temp_path| {
            let mut input = File::open(source).map_err(|source_error| FileAccessError::Io {
                operation: "打开上传源文件",
                path: source.to_path_buf(),
                source: source_error,
            })?;
            io::copy(&mut input, temp).map_err(|source_error| FileAccessError::Io {
                operation: "复制上传文件",
                path: temp_path.to_path_buf(),
                source: source_error,
            })?;
            Ok(())
        },
        || ensure_replaceable(destination),
        |operation, path, source| FileAccessError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        },
    )
}

fn ensure_replaceable(destination: &Path) -> Result<(), FileAccessError> {
    if let Ok(metadata) = fs::symlink_metadata(destination)
        && (!metadata.is_file() || is_link_like(&metadata))
    {
        return Err(FileAccessError::NonReplaceable {
            path: destination.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn tree_sorting_ignores_build_dirs_and_reports_links() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("z-dir")).unwrap();
        fs::create_dir(root.path().join("a-dir")).unwrap();
        fs::create_dir(root.path().join("target")).unwrap();
        write(&root.path().join("b.txt"), b"b");
        write(&root.path().join("A.txt"), b"a");
        let files = ProjectFiles::open(root.path()).unwrap();
        let tree = files.scan_tree().unwrap();
        assert_eq!(
            tree.nodes
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a-dir", "z-dir", "A.txt", "b.txt"]
        );
        assert!(!tree.nodes.iter().any(|node| node.name == "target"));
    }

    #[test]
    fn read_rejects_traversal_binary_active_content_and_bad_image_magic() {
        let root = tempdir().unwrap();
        write(&root.path().join("src/main.rs"), b"fn main() {}\n");
        write(&root.path().join("binary.dat"), b"a\0b");
        write(&root.path().join("page.html"), b"<script>alert(1)</script>");
        write(&root.path().join("fake.png"), b"not png");
        let files = ProjectFiles::open(root.path()).unwrap();
        assert!(matches!(
            files.read("src/main.rs").unwrap(),
            FileContent::Text(TextFileContent {
                language: "rust",
                ..
            })
        ));
        assert!(matches!(
            files.read("../outside"),
            Err(FileAccessError::InvalidRelativePath { .. })
        ));
        assert!(matches!(
            files.read("binary.dat"),
            Err(FileAccessError::BinaryText { .. })
        ));
        assert!(matches!(
            files.read("page.html").unwrap(),
            FileContent::Unsupported { .. }
        ));
        assert!(matches!(
            files.read("fake.png"),
            Err(FileAccessError::UnsupportedImage { .. })
        ));
    }

    #[test]
    fn image_dimensions_reject_decode_bombs_before_gpui() {
        let root = tempdir().unwrap();
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&30_000_u32.to_be_bytes());
        png.extend_from_slice(&30_000_u32.to_be_bytes());
        write(&root.path().join("huge.png"), &png);
        let files = ProjectFiles::open(root.path()).unwrap();
        assert!(matches!(
            files.read("huge.png"),
            Err(FileAccessError::ImageDimensionsTooLarge {
                width: 30_000,
                height: 30_000,
                ..
            })
        ));
        assert_eq!(
            image_dimensions(ImageKind::Gif, b"GIF89a\x02\0\x03\0"),
            Some((2, 3))
        );
    }

    #[test]
    fn gif_frame_budget_rejects_animation_decode_bombs() {
        let mut gif = b"GIF89a\x01\0\x01\0\0\0\0".to_vec();
        for _ in 0..=IMAGE_PREVIEW_MAX_GIF_FRAMES {
            gif.extend_from_slice(&[0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x4c, 1, 0]);
        }
        gif.push(0x3b);
        assert!(gif_frame_count(&gif).unwrap() > IMAGE_PREVIEW_MAX_GIF_FRAMES);
        let root = tempdir().unwrap();
        write(&root.path().join("many.gif"), &gif);
        let files = ProjectFiles::open(root.path()).unwrap();
        assert!(matches!(
            files.read("many.gif"),
            Err(FileAccessError::ImageDimensionsTooLarge { .. })
        ));
    }

    #[test]
    fn empty_upload_selection_has_a_specific_error() {
        let root = tempdir().unwrap();
        let files = ProjectFiles::open(root.path()).unwrap();
        assert!(matches!(
            files.inspect_upload(Vec::new(), ""),
            Err(FileAccessError::EmptyUploadSelection)
        ));
    }

    #[test]
    fn index_uses_ignore_rules_and_fuzzy_ranking() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        write(&root.path().join("components/ChatInput.tsx"), b"x");
        write(&root.path().join("components/ChatWindow.tsx"), b"x");
        write(&root.path().join("target/hidden.rs"), b"x");
        let files = ProjectFiles::open(root.path()).unwrap();
        let index = files.walk_index().unwrap();
        assert!(
            !index
                .entries
                .iter()
                .any(|entry| entry.path.contains("target"))
        );
        let matches =
            crate::composer::filter_file_entries(&index.entries, "chinp", FILE_SEARCH_RESULT_LIMIT);
        assert_eq!(matches[0].path, "components/ChatInput.tsx");
    }

    #[test]
    fn upload_preflight_and_strategies_are_atomic_and_partial() {
        let root = tempdir().unwrap();
        let source = tempdir().unwrap();
        write(&source.path().join("keep.txt"), b"new keep");
        write(&source.path().join("fresh.txt"), b"fresh");
        write(&root.path().join("keep.txt"), b"old keep");
        fs::create_dir(root.path().join("fresh.txt")).unwrap();
        let files = ProjectFiles::open(root.path()).unwrap();
        let inspection = files
            .inspect_upload(
                [
                    source.path().join("keep.txt"),
                    source.path().join("fresh.txt"),
                ],
                "",
            )
            .unwrap();
        assert_eq!(inspection.conflicts, vec!["keep.txt", "fresh.txt"]);
        assert_eq!(inspection.non_replaceable, vec!["fresh.txt"]);
        assert!(matches!(
            files.upload(&inspection, "", UploadConflictStrategy::Error),
            Err(FileAccessError::UploadConflict { .. })
        ));
        assert_eq!(fs::read(root.path().join("keep.txt")).unwrap(), b"old keep");

        let skipped = files
            .upload(&inspection, "", UploadConflictStrategy::Skip)
            .unwrap();
        assert_eq!(skipped.skipped, vec!["keep.txt", "fresh.txt"]);

        let overwritten = files
            .upload(&inspection, "", UploadConflictStrategy::Overwrite)
            .unwrap();
        assert_eq!(overwritten.uploaded, vec!["keep.txt"]);
        assert_eq!(overwritten.errors.len(), 1);
        assert_eq!(fs::read(root.path().join("keep.txt")).unwrap(), b"new keep");
        assert!(root.path().join("fresh.txt").is_dir());
        assert_eq!(
            root.path()
                .read_dir()
                .unwrap()
                .flatten()
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with('.') && name.ends_with(".tmp")
                })
                .count(),
            0
        );
    }

    #[test]
    fn upload_names_and_limits_are_checked_before_writes() {
        assert!(validate_upload_name(OsStr::new("ok.txt")).is_ok());
        for invalid in ["", ".", "..", "a/b", "a\\b", "bad.", "bad "] {
            assert!(
                validate_upload_name(OsStr::new(invalid)).is_err(),
                "{invalid}"
            );
        }
        if cfg!(windows) {
            for invalid in ["CON", "con.txt", "LPT1.log", "bad:name.txt"] {
                assert!(
                    validate_upload_name(OsStr::new(invalid)).is_err(),
                    "{invalid}"
                );
            }
        }
        let root = tempdir().unwrap();
        let source = tempdir().unwrap();
        write(&source.path().join("same.txt"), b"a");
        write(&source.path().join("SAME.txt"), b"b");
        let files = ProjectFiles::open(root.path()).unwrap();
        let result = files.inspect_upload(
            [
                source.path().join("same.txt"),
                source.path().join("SAME.txt"),
            ],
            "",
        );
        if cfg!(windows) {
            assert!(matches!(
                result,
                Err(FileAccessError::DuplicateUploadName { .. })
            ));
        }
    }

    #[test]
    fn upload_rechecks_targets_after_preflight() {
        let root = tempdir().unwrap();
        let source = tempdir().unwrap();
        write(&source.path().join("late.txt"), b"new");
        let files = ProjectFiles::open(root.path()).unwrap();
        let inspection = files
            .inspect_upload([source.path().join("late.txt")], "")
            .unwrap();
        write(&root.path().join("late.txt"), b"racer");
        assert!(matches!(
            files.upload(&inspection, "", UploadConflictStrategy::Error),
            Err(FileAccessError::UploadConflict { .. })
        ));
        let skipped = files
            .upload(&inspection, "", UploadConflictStrategy::Skip)
            .unwrap();
        assert_eq!(skipped.skipped, vec!["late.txt"]);
        assert_eq!(fs::read(root.path().join("late.txt")).unwrap(), b"racer");
    }

    #[test]
    fn upload_size_boundaries_are_enforced() {
        let root = tempdir().unwrap();
        let source = tempdir().unwrap();
        let path = source.path().join("too-large.bin");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_UPLOAD_FILE_BYTES + 1).unwrap();
        let files = ProjectFiles::open(root.path()).unwrap();
        assert!(matches!(
            files.inspect_upload([path], ""),
            Err(FileAccessError::UploadFileTooLarge { .. })
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_symlink_escape_is_rejected_when_creation_is_permitted() {
        use std::os::windows::fs::symlink_dir;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let link = root.path().join("escape");
        if symlink_dir(outside.path(), &link).is_err() {
            let status = hide_console_window(&mut Command::new("cmd"))
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(outside.path())
                .status()
                .expect("启动 mklink /J");
            assert!(status.success(), "无法创建测试 junction");
        }
        write(&outside.path().join("secret.txt"), b"secret");
        let files = ProjectFiles::open(root.path()).unwrap();
        let tree = files.scan_tree().unwrap();
        assert!(tree.nodes.is_empty());
        assert_eq!(tree.skipped_links, 1);
        assert!(matches!(
            files.read("escape/secret.txt"),
            Err(FileAccessError::LinkedPath { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected_for_read_scan_and_upload_target() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        write(&outside.path().join("secret.txt"), b"secret");
        symlink(outside.path(), root.path().join("escape")).unwrap();
        let files = ProjectFiles::open(root.path()).unwrap();
        let tree = files.scan_tree().unwrap();
        assert!(tree.nodes.is_empty());
        assert_eq!(tree.skipped_links, 1);
        assert!(matches!(
            files.read("escape/secret.txt"),
            Err(FileAccessError::LinkedPath { .. })
        ));
        assert!(
            files
                .inspect_upload([outside.path().join("secret.txt")], "escape")
                .is_err()
        );
    }
}
