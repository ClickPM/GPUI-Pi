use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) fn write_atomic_with<E>(
    path: &Path,
    mut copy: impl FnMut(&mut File, &Path) -> Result<(), E>,
    verify_before_replace: impl FnOnce() -> Result<(), E>,
    map_io: impl Fn(&'static str, &Path, io::Error) -> E + Copy,
) -> Result<(), E> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| map_io("创建目标目录", parent, error))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let mut last_collision = None;
    let mut verify_before_replace = Some(verify_before_replace);

    for _ in 0..100 {
        let temp_path = parent.join(format!(".{file_name}-{:016x}.tmp", next_temp_nonce()));
        let mut file = match open_private_temp(&temp_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
                continue;
            }
            Err(error) => return Err(map_io("创建临时文件", &temp_path, error)),
        };
        let result = (|| {
            copy(&mut file, &temp_path)?;
            file.flush()
                .map_err(|error| map_io("刷新临时文件", &temp_path, error))?;
            file.sync_all()
                .map_err(|error| map_io("同步临时文件", &temp_path, error))?;
            drop(file);
            verify_before_replace
                .take()
                .expect("replace verifier is called once")()?;
            replace_file(&temp_path, path).map_err(|error| map_io("发布文件", path, error))?;
            sync_directory(parent);
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        return result;
    }
    Err(map_io(
        "创建唯一临时文件",
        path,
        last_collision.unwrap_or_else(|| io::Error::other("临时文件名冲突")),
    ))
}

pub(crate) fn write_bytes_atomic_if<E>(
    path: &Path,
    bytes: &[u8],
    verify_before_replace: impl FnOnce() -> Result<(), E>,
) -> Result<(), E>
where
    E: From<io::Error>,
{
    write_atomic_with(
        path,
        |file, _| file.write_all(bytes).map_err(E::from),
        verify_before_replace,
        |_, _, error| E::from(error),
    )
}

pub(crate) fn is_link_like(metadata: &fs::Metadata) -> bool {
    // Windows 的 FileType::is_symlink 同时覆盖 symlink 与 junction（name-surrogate
    // reparse point），但不会误拒 OneDrive 占位符、去重等非路径跳转 reparse point。
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
pub(crate) fn is_any_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
}

#[cfg(not(windows))]
pub(crate) fn is_any_reparse_point(_: &fs::Metadata) -> bool {
    false
}

/// GUI 子系统父进程下，控制台子进程必须显式隐藏窗口，否则每个 git/cmd 都会弹空终端。
pub(crate) fn hide_console_window(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn open_private_temp(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

fn next_temp_nonce() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    now ^ u64::from(std::process::id()) ^ COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: 两个 UTF-16 缓冲都以 NUL 结尾，并在调用期间保持有效。
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        // SAFETY: 紧跟失败的 Win32 调用读取线程局部错误码。
        let code = unsafe { GetLastError() };
        Err(io::Error::from_raw_os_error(code as i32))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_directory(_: &Path) {}
