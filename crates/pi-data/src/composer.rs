//! R8 composer 的纯逻辑：补全、图片附件与进程内草稿。

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use thiserror::Error;

use crate::fs_util::hide_console_window;

pub const AT_RESULT_LIMIT: usize = 20;
pub const FILE_INDEX_LIMIT: usize = 5_000;
pub const FILE_WALK_LIMIT: usize = 20_000;
pub const FILE_WALK_DEPTH: usize = 24;
pub const MAX_ATTACHED_IMAGES: usize = 10;
pub const MAX_ATTACHED_IMAGE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtQuery {
    pub start: usize,
    pub query: String,
    pub quoted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileIndexEntry {
    pub path: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtInsertion {
    pub text: String,
    pub cursor_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileIndex {
    pub entries: Vec<FileIndexEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftImage {
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComposerDraft {
    pub text: String,
    pub images: Vec<DraftImage>,
}

#[derive(Debug, Default)]
pub struct DraftStore {
    drafts: HashMap<String, ComposerDraft>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ImageValidationError {
    #[error("最多只能附加 {MAX_ATTACHED_IMAGES} 张图片")]
    TooMany,
    #[error("图片超过 10 MiB 上限")]
    TooLarge,
    #[error("不支持的图片格式；仅接受 PNG、JPEG、GIF 或 WebP")]
    Unsupported,
    #[error("图片数据为空或已损坏")]
    Invalid,
    #[error("不支持的剪贴板图片格式：SVG；请改用 PNG、JPEG、GIF 或 WebP")]
    UnsupportedSvg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl SupportedImageFormat {
    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

pub fn extract_at_query(text_before_cursor: &str) -> Option<AtQuery> {
    let line_start = text_before_cursor.rfind('\n').map_or(0, |index| index + 1);
    let mut candidates = text_before_cursor[line_start..].match_indices('@');
    let (relative, _) = candidates.next_back()?;
    let start = line_start + relative;
    if start > 0 {
        let previous = text_before_cursor[..start].chars().next_back()?;
        if !previous.is_whitespace() {
            return None;
        }
    }
    let tail = &text_before_cursor[start + 1..];
    if let Some(query) = tail.strip_prefix('"') {
        if query.contains(['"', '\n']) {
            return None;
        }
        return Some(AtQuery {
            start,
            query: query.to_owned(),
            quoted: true,
        });
    }
    if tail.chars().any(|ch| ch.is_whitespace() || ch == '"') {
        return None;
    }
    Some(AtQuery {
        start,
        query: tail.to_owned(),
        quoted: false,
    })
}

pub fn build_entries_from_files(files: impl IntoIterator<Item = String>) -> Vec<FileIndexEntry> {
    let mut dirs = HashSet::new();
    let mut normalized_files = Vec::new();
    for path in files {
        let path = path.replace('\\', "/");
        if path.is_empty() {
            continue;
        }
        let mut offset = 0;
        while let Some(index) = path[offset..].find('/') {
            let end = offset + index;
            if end > 0 {
                dirs.insert(path[..end].to_owned());
            }
            offset = end + 1;
        }
        normalized_files.push(path);
    }
    let mut entries = dirs
        .into_iter()
        .map(|path| FileIndexEntry { path, is_dir: true })
        .chain(normalized_files.into_iter().map(|path| FileIndexEntry {
            path,
            is_dir: false,
        }))
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| {
        path_depth(&a.path)
            .cmp(&path_depth(&b.path))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| b.is_dir.cmp(&a.is_dir))
    });
    entries.dedup();
    entries
}

pub fn filter_file_entries(
    entries: &[FileIndexEntry],
    query: &str,
    limit: usize,
) -> Vec<FileIndexEntry> {
    if query.is_empty() {
        return entries.iter().take(limit).cloned().collect();
    }
    let query = query.to_lowercase();
    let mut scored = entries
        .iter()
        .filter_map(|entry| {
            let score = score_entry(entry, &query);
            (score > 0).then_some((score, entry))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| path_depth(&left.path).cmp(&path_depth(&right.path)))
            .then_with(|| left.path.cmp(&right.path))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, entry)| entry.clone())
        .collect()
}

pub fn build_at_insertion(path: &str, is_dir: bool, force_quotes: bool) -> AtInsertion {
    let path = if is_dir {
        format!("{path}/")
    } else {
        path.to_owned()
    };
    let quotes = force_quotes || path.contains(' ');
    if is_dir {
        let text = if quotes {
            format!("@\"{path}\"")
        } else {
            format!("@{path}")
        };
        let cursor_offset = if quotes { text.len() - 1 } else { text.len() };
        AtInsertion {
            text,
            cursor_offset,
        }
    } else {
        let text = if quotes {
            format!("@\"{path}\" ")
        } else {
            format!("@{path} ")
        };
        let cursor_offset = text.len();
        AtInsertion {
            text,
            cursor_offset,
        }
    }
}

pub fn apply_at_insertion(
    value: &str,
    cursor: usize,
    query: &AtQuery,
    entry: &FileIndexEntry,
) -> (String, usize) {
    let mut cursor = cursor.min(value.len());
    while !value.is_char_boundary(cursor) {
        cursor = cursor.saturating_sub(1);
    }
    let mut after = &value[cursor..];
    if query.quoted && after.starts_with('"') {
        after = &after[1..];
    }
    let insertion = build_at_insertion(&entry.path, entry.is_dir, query.quoted);
    let mut next = String::with_capacity(value.len() + insertion.text.len());
    next.push_str(&value[..query.start]);
    next.push_str(&insertion.text);
    let next_cursor = next.len() - insertion.text.len() + insertion.cursor_offset;
    next.push_str(after);
    (next, next_cursor)
}

pub fn build_file_index(cwd: &Path) -> FileIndex {
    git_file_index(cwd).unwrap_or_else(|| walk_file_index(cwd))
}

fn git_file_index(cwd: &Path) -> Option<FileIndex> {
    let mut child = hide_console_window(&mut Command::new("git"))
        .arg("-C")
        .arg(cwd)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (index, stopped_early) = collect_git_file_index(BufReader::new(stdout)).ok()?;
    if stopped_early {
        let _ = child.kill();
    }
    let status = child.wait().ok()?;
    if !stopped_early && !status.success() {
        return None;
    }
    Some(index)
}

fn collect_git_file_index(mut reader: impl BufRead) -> std::io::Result<(FileIndex, bool)> {
    let mut files = Vec::with_capacity(FILE_INDEX_LIMIT);
    let mut seen = HashSet::with_capacity(FILE_INDEX_LIMIT);
    let mut path = Vec::new();
    let mut records = 0_usize;
    let mut truncated = false;

    loop {
        path.clear();
        let bytes_read = reader.read_until(0, &mut path)?;
        if bytes_read == 0 {
            break;
        }
        records += 1;
        if path.last() == Some(&0) {
            path.pop();
        }
        if !path.is_empty() {
            let path = String::from_utf8_lossy(&path).replace('\\', "/");
            if seen.insert(path.clone()) {
                if files.len() == FILE_INDEX_LIMIT {
                    truncated = true;
                    break;
                }
                files.push(path);
            }
        }
        if records >= FILE_WALK_LIMIT {
            // `fill_buf` 只探测是否还有输出，不继续收集，既保持硬上限也避免恰好到上限时误报。
            truncated = !reader.fill_buf()?.is_empty();
            break;
        }
    }

    files.sort();
    Ok((
        FileIndex {
            entries: build_entries_from_files(files),
            truncated,
        },
        truncated,
    ))
}

fn walk_file_index(cwd: &Path) -> FileIndex {
    let mut queue = VecDeque::from([(cwd.to_path_buf(), 0_usize)]);
    let mut files = Vec::new();
    let mut visited = 0_usize;
    let mut truncated = false;
    while let Some((directory, depth)) = queue.pop_front() {
        if depth > FILE_WALK_DEPTH || visited >= FILE_WALK_LIMIT {
            truncated = true;
            continue;
        }
        let Ok(read_dir) = fs::read_dir(&directory) else {
            continue;
        };
        let mut children = read_dir.flatten().collect::<Vec<_>>();
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            visited += 1;
            if visited > FILE_WALK_LIMIT {
                truncated = true;
                break;
            }
            let path = child.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if crate::fs_util::is_link_like(&metadata)
                || crate::fs_util::is_any_reparse_point(&metadata)
            {
                continue;
            }
            if metadata.is_dir() {
                if depth < FILE_WALK_DEPTH {
                    queue.push_back((path, depth + 1));
                } else {
                    truncated = true;
                }
            } else if metadata.is_file()
                && let Ok(relative) = path.strip_prefix(cwd)
            {
                files.push(relative.to_string_lossy().replace('\\', "/"));
                if files.len() >= FILE_INDEX_LIMIT {
                    truncated = true;
                    break;
                }
            }
        }
        if files.len() >= FILE_INDEX_LIMIT {
            break;
        }
    }
    FileIndex {
        entries: build_entries_from_files(files),
        truncated,
    }
}

pub fn detect_image_format(bytes: &[u8]) -> Result<SupportedImageFormat, ImageValidationError> {
    if bytes.is_empty() {
        return Err(ImageValidationError::Invalid);
    }
    if bytes.len() > MAX_ATTACHED_IMAGE_BYTES {
        return Err(ImageValidationError::TooLarge);
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(SupportedImageFormat::Png);
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Ok(SupportedImageFormat::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Ok(SupportedImageFormat::Gif);
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Ok(SupportedImageFormat::Webp);
    }
    Err(ImageValidationError::Unsupported)
}

pub fn image_from_bytes(bytes: Vec<u8>) -> Result<DraftImage, ImageValidationError> {
    let format = detect_image_format(&bytes)?;
    Ok(DraftImage {
        data: STANDARD.encode(bytes),
        mime_type: format.mime_type().to_owned(),
    })
}

/// 校验 GPUI 剪贴板给出的图片编码，并转换为 RPC 草稿。
///
/// Windows 的 CF_DIB 会由 GPUI 包装成 BMP；钉版本范围内没有公开的重编码 API，
/// 因此本轮明确返回错误而不是静默丢弃或修改依赖钉点。
pub fn image_from_clipboard_bytes(
    mime_type: &str,
    bytes: Vec<u8>,
) -> Result<DraftImage, ImageValidationError> {
    if bytes.is_empty() {
        return Err(ImageValidationError::Invalid);
    }
    if bytes.len() > MAX_ATTACHED_IMAGE_BYTES {
        return Err(ImageValidationError::TooLarge);
    }
    match mime_type {
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" => image_from_bytes(bytes),
        "image/svg+xml" => Err(ImageValidationError::UnsupportedSvg),
        // GPUI 没有公开的编码 API；在不新增依赖、不改 Cargo.lock 的前提下，
        // Windows CF_DIB/BMP 必须明确报错，不能继续静默丢弃。
        "image/bmp" => Err(ImageValidationError::Unsupported),
        _ => Err(ImageValidationError::Unsupported),
    }
}

pub fn validate_image_batch(
    existing: usize,
    incoming: &[DraftImage],
) -> Result<(), ImageValidationError> {
    if existing.saturating_add(incoming.len()) > MAX_ATTACHED_IMAGES {
        return Err(ImageValidationError::TooMany);
    }
    for image in incoming {
        let bytes = STANDARD
            .decode(&image.data)
            .map_err(|_| ImageValidationError::Invalid)?;
        let actual = detect_image_format(&bytes)?;
        if actual.mime_type() != image.mime_type {
            return Err(ImageValidationError::Invalid);
        }
    }
    Ok(())
}

impl DraftStore {
    pub fn get(&self, key: &str) -> ComposerDraft {
        self.drafts.get(key).cloned().unwrap_or_default()
    }

    pub fn set(&mut self, key: impl Into<String>, draft: ComposerDraft) {
        let key = key.into();
        if draft.text.is_empty() && draft.images.is_empty() {
            self.drafts.remove(&key);
        } else {
            self.drafts.insert(key, draft);
        }
    }

    pub fn clear(&mut self, key: &str) {
        self.drafts.remove(key);
    }

    pub fn restore_submission(&mut self, key: &str, submission: ComposerDraft) -> ComposerDraft {
        let current = self.get(key);
        let restored = merge_restored_submission(submission, current);
        self.set(key.to_owned(), restored.clone());
        restored
    }
}

pub fn merge_restored_submission(
    submitted: ComposerDraft,
    current: ComposerDraft,
) -> ComposerDraft {
    let text = match (
        submitted.text.trim().is_empty(),
        current.text.trim().is_empty(),
    ) {
        (true, _) => current.text,
        (_, true) => submitted.text,
        (false, false) => format!("{}\n\n{}", submitted.text, current.text),
    };
    let images = submitted
        .images
        .into_iter()
        .chain(current.images)
        .take(MAX_ATTACHED_IMAGES)
        .collect();
    ComposerDraft { text, images }
}

fn path_depth(path: &str) -> usize {
    path.bytes().filter(|byte| *byte == b'/').count()
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut needle = needle.chars();
    let mut next = needle.next();
    for character in haystack.chars() {
        if next == Some(character) {
            next = needle.next();
            if next.is_none() {
                return true;
            }
        }
    }
    next.is_none()
}

fn score_entry(entry: &FileIndexEntry, query: &str) -> u8 {
    let path = entry.path.to_lowercase();
    let candidate = if query.contains('/') {
        path.as_str()
    } else {
        path.rsplit('/').next().unwrap_or(&path)
    };
    let mut score = if candidate == query {
        100
    } else if candidate.starts_with(query) {
        80
    } else if candidate.contains(query) {
        50
    } else if !query.contains('/') && path.contains(query) {
        30
    } else if is_subsequence(query, &path) {
        10
    } else {
        0
    };
    if entry.is_dir && score > 0 {
        score += 10;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png() -> Vec<u8> {
        b"\x89PNG\r\n\x1a\nfixture".to_vec()
    }

    #[test]
    fn at_token_requires_start_or_whitespace_and_supports_quotes() {
        assert_eq!(extract_at_query("hello foo@bar"), None);
        assert_eq!(
            extract_at_query("hello @src/ma"),
            Some(AtQuery {
                start: 6,
                query: "src/ma".into(),
                quoted: false,
            })
        );
        assert_eq!(
            extract_at_query("@\"path with spaces/fi"),
            Some(AtQuery {
                start: 0,
                query: "path with spaces/fi".into(),
                quoted: true,
            })
        );
    }

    #[test]
    fn fuzzy_ranking_and_directory_drill_down_match_upstream() {
        let entries = build_entries_from_files([
            "components/ChatInput.tsx".to_owned(),
            "src/main.rs".to_owned(),
            "src/model.rs".to_owned(),
        ]);
        let fuzzy = filter_file_entries(&entries, "chinp", AT_RESULT_LIMIT);
        assert_eq!(fuzzy[0].path, "components/ChatInput.tsx");
        let drill = filter_file_entries(&entries, "src/", AT_RESULT_LIMIT);
        assert!(drill.iter().all(|entry| entry.path.starts_with("src/")));
        let query = extract_at_query("@src/").unwrap();
        let (value, cursor) = apply_at_insertion("@src/", 5, &query, &drill[0]);
        assert_eq!(cursor, value.len());
    }

    #[test]
    fn insertion_quotes_spaces_and_files_end_with_space() {
        assert_eq!(
            build_at_insertion("my dir/file.rs", false, false).text,
            "@\"my dir/file.rs\" "
        );
        let directory = build_at_insertion("my dir", true, false);
        assert_eq!(directory.text, "@\"my dir/\"");
        assert_eq!(directory.cursor_offset, directory.text.len() - 1);
    }

    #[test]
    fn insertion_at_utf8_cursor_preserves_text_after_cursor() {
        let value = "前缀 @sr 后续";
        let cursor = "前缀 @sr".len();
        let query = extract_at_query(&value[..cursor]).unwrap();
        let entry = FileIndexEntry {
            path: "src".into(),
            is_dir: true,
        };
        let (next, next_cursor) = apply_at_insertion(value, cursor, &query, &entry);
        assert_eq!(next, "前缀 @src/ 后续");
        assert_eq!(next_cursor, "前缀 @src/".len());
    }

    #[test]
    fn git_index_collection_stops_at_bound_and_marks_truncated() {
        let mut output = Vec::new();
        for index in 0..=FILE_INDEX_LIMIT {
            output.extend_from_slice(format!("file-{index:05}.rs\0").as_bytes());
        }
        let (index, stopped_early) = collect_git_file_index(output.as_slice()).unwrap();
        let file_count = index.entries.iter().filter(|entry| !entry.is_dir).count();
        assert_eq!(file_count, FILE_INDEX_LIMIT);
        assert!(index.truncated);
        assert!(stopped_early);

        let exact = (0..FILE_INDEX_LIMIT)
            .flat_map(|index| format!("exact-{index:05}.rs\0").into_bytes())
            .collect::<Vec<_>>();
        let (index, stopped_early) = collect_git_file_index(exact.as_slice()).unwrap();
        assert!(!index.truncated);
        assert!(!stopped_early);
    }

    #[test]
    fn walk_index_is_bounded_and_derives_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        fs::write(dir.path().join("src/nested/main.rs"), "fn main() {}").unwrap();
        let index = walk_file_index(dir.path());
        assert!(index.entries.contains(&FileIndexEntry {
            path: "src".into(),
            is_dir: true
        }));
        assert!(index.entries.contains(&FileIndexEntry {
            path: "src/nested/main.rs".into(),
            is_dir: false
        }));
        assert!(index.entries.len() <= FILE_INDEX_LIMIT + FILE_WALK_DEPTH);
    }

    #[test]
    fn image_magic_size_and_count_are_enforced() {
        let image = image_from_bytes(tiny_png()).unwrap();
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(
            detect_image_format(b"not an image"),
            Err(ImageValidationError::Unsupported)
        );
        assert_eq!(
            detect_image_format(&vec![0; MAX_ATTACHED_IMAGE_BYTES + 1]),
            Err(ImageValidationError::TooLarge)
        );
        assert_eq!(
            validate_image_batch(MAX_ATTACHED_IMAGES, &[image]),
            Err(ImageValidationError::TooMany)
        );
    }

    #[test]
    fn clipboard_bmp_and_svg_are_explicitly_rejected() {
        assert_eq!(
            image_from_clipboard_bytes("image/bmp", b"BMfixture".to_vec()),
            Err(ImageValidationError::Unsupported)
        );
        assert_eq!(
            image_from_clipboard_bytes("image/svg+xml", b"<svg/>".to_vec()),
            Err(ImageValidationError::UnsupportedSvg)
        );
    }

    #[test]
    fn draft_store_isolated_keys_and_rejected_submission_prepends() {
        let image = image_from_bytes(tiny_png()).unwrap();
        let mut store = DraftStore::default();
        store.set(
            "one",
            ComposerDraft {
                text: "new typing".into(),
                images: vec![],
            },
        );
        store.set(
            "two",
            ComposerDraft {
                text: "other".into(),
                images: vec![],
            },
        );
        let restored = store.restore_submission(
            "one",
            ComposerDraft {
                text: "rejected".into(),
                images: vec![image],
            },
        );
        assert_eq!(restored.text, "rejected\n\nnew typing");
        assert_eq!(restored.images.len(), 1);
        assert_eq!(store.get("two").text, "other");
        store.clear("one");
        assert_eq!(store.get("one"), ComposerDraft::default());
    }
}
