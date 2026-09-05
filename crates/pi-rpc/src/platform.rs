//! 平台进程治理原语：Windows Job Object 与内存采样。
//!
//! # 为什么单独一个模块
//!
//! 全局约定是禁止 `unsafe`；R25 的 Job Object 与进程内存查询在 Rust 里只能经 Win32 FFI
//! 表达，项目所有者据此批准了**限定豁免**：本轮新增的 `unsafe` 全部集中在本文件，
//! 模块外一律安全 Rust。因此本文件对外只暴露安全 API，每一处 `unsafe` 都写明为什么
//! 满足对应 API 的安全前提。
//!
//! # 为什么要 Job Object 而不是 `taskkill /T`
//!
//! `taskkill /T` 沿 PPID 链走：pi 的扩展进程若先 spawn 孙进程再自己退出，孙进程就会被
//! 系统重挂到别的父下，整树终止随即漏掉它。Job Object 是内核对象，进程一旦被纳管就
//! **无法逃逸**（本模块显式不开 breakaway），因此：
//!
//! - `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`：句柄一关整树即终止，不依赖上层记得清理；
//! - `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` / `JOB_OBJECT_LIMIT_JOB_MEMORY`：内核级硬上限，
//!   对扩展自行 spawn 的子代理同样有效；
//! - `QueryInformationJobObject`：整树进程数与内存的权威口径。

/// Job Object 的硬上限配置。
///
/// 两项都是 `None` 时只保留「句柄关闭即整树终止」，不设配额 —— 库层不替上层定策略，
/// 具体水位由 `pi-runtime` 按实测配置。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobLimits {
    /// 整棵进程树的活跃进程数上限；超限时新进程创建直接失败。
    pub max_active_processes: Option<u32>,
    /// 整棵进程树的提交内存上限（字节）；超限时分配失败。
    pub job_memory_bytes: Option<u64>,
}

/// 整棵进程树的一次采样。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobStats {
    /// 当前活跃进程数（含 pi 自身与全部子孙进程）。
    pub active_processes: u32,
    /// 该 job 历史上纳管过的进程总数。
    pub total_processes: u32,
    /// 被 job 终止过的进程数。
    pub total_terminated_processes: u32,
    /// 内核记录的 job 提交内存峰值。
    pub peak_job_memory_bytes: u64,
    /// 当前整树私有提交内存之和；逐进程采样，取不到的进程按 0 计。
    pub private_bytes: u64,
    /// `private_bytes` 实际覆盖到的进程数；小于 `active_processes` 说明有进程在采样
    /// 途中退出或拒绝打开，调用方据此判断样本完整度。
    pub sampled_processes: u32,
}

/// 一次系统级内存采样。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemMemory {
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// 系统报告的内存占用百分比（0–100）。
    pub load_percent: u32,
}

impl SystemMemory {
    /// 已用物理内存字节数。
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }
}

/// 本平台是否支持 Job Object 级进程树治理。
pub const fn job_objects_supported() -> bool {
    cfg!(windows)
}

#[cfg(windows)]
mod imp {
    use super::{JobLimits, JobStats, SystemMemory};
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_MORE_DATA, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_PROCESS_ID_LIST,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
        JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenProcess, OpenThread, PROCESS_QUERY_INFORMATION,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ, ResumeThread, THREAD_SUSPEND_RESUME,
    };

    /// pid 列表查询的初始容量（进程数）。一棵 pi 进程树远小于这个数，正常一次到位。
    const INITIAL_PID_CAPACITY: usize = 64;
    /// pid 列表重查上限：每次按内核报告的实际进程数翻倍扩容，三轮足以吸收查询间隙。
    const PID_QUERY_ATTEMPTS: usize = 3;
    /// 变长 pid 数组在结构体内的起始槽位（以 `usize` 计）。
    const PID_HEADER_SLOTS: usize =
        std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList)
            / std::mem::size_of::<usize>();

    /// 自动关闭的 Win32 句柄。
    ///
    /// 只在本模块内构造，构造点保证传入的是成功返回、尚未关闭的句柄。
    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // 安全性：`self.0` 由本模块的构造点保证有效且未关闭，且本类型不可 Clone /
            // Copy，因此不会重复关闭。
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// 拥有一个 Job Object 句柄。
    ///
    /// 句柄关闭即整树终止（`KILL_ON_JOB_CLOSE`），所以「Runtime 结束时不泄漏工具子进程」
    /// 不依赖任何上层调用顺序，Drop 就是兜底。
    pub struct JobObject {
        handle: OwnedHandle,
    }

    // 安全性：Win32 句柄是进程范围内的内核对象引用，不绑定创建线程；本类型对句柄的全部
    // 操作（Query / Set / Terminate / Close）都由内核串行化，且 `handle` 私有、无内部
    // 可变性，因此跨线程发送与共享都不会产生数据竞争。
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        pub fn create(limits: JobLimits) -> io::Result<Self> {
            // 安全性：两个参数都传空 —— 匿名（无名字）且使用默认安全属性，因此句柄
            // **不可被子进程继承**，`KILL_ON_JOB_CLOSE` 的「最后一个句柄」就是我们手里这个。
            let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if raw.is_null() {
                return Err(last_error("CreateJobObjectW"));
            }
            let job = Self {
                handle: OwnedHandle(raw),
            };
            job.apply_limits(limits)?;
            Ok(job)
        }

        fn apply_limits(&self, limits: JobLimits) -> io::Result<()> {
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            // KILL_ON_JOB_CLOSE 永远打开：它才是「关掉 Runtime 不留孙进程」的兜底。
            // 这里**不设** BREAKAWAY_OK / SILENT_BREAKAWAY_OK —— 不设即禁止逃逸，
            // 扩展 spawn 的子代理无法脱离 job。
            let mut flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Some(max_active) = limits.max_active_processes.filter(|value| *value > 0) {
                flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
                info.BasicLimitInformation.ActiveProcessLimit = max_active;
            }
            if let Some(memory) = limits.job_memory_bytes.filter(|value| *value > 0) {
                // 转换失败只可能出现在 32 位目标上配了超 4GiB 的上限：那种配置本就无意义，
                // 收敛到 usize::MAX 等价于「不设上限」，比直接启动失败温和。
                flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                info.JobMemoryLimit = usize::try_from(memory).unwrap_or(usize::MAX);
            }
            info.BasicLimitInformation.LimitFlags = flags;

            // 安全性：`info` 是本地 `#[repr(C)]` 值，指针在调用期间有效；长度取自同一类型的
            // `size_of`，与 `JobObjectExtendedLimitInformation` 要求的结构完全匹配。
            let ok = unsafe {
                SetInformationJobObject(
                    self.handle.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    size_as_u32::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
                )
            };
            if ok == 0 {
                return Err(last_error("SetInformationJobObject"));
            }
            Ok(())
        }

        /// 把子进程纳入 job。
        ///
        /// **必须在子进程仍处于挂起状态时调用**，否则存在「纳管前它已经 spawn 出孙进程」
        /// 的竞态 —— 那个孙进程会永远留在 job 之外，硬限制与整树终止都管不到它。
        pub fn assign(&self, child: &Child) -> io::Result<()> {
            let process = child.as_raw_handle() as HANDLE;
            // 安全性：`process` 是 std 仍然持有的子进程句柄（`child` 的借用保证调用期间
            // 不会被关闭），`self.handle.0` 是本类型拥有的有效 job 句柄。
            let ok = unsafe { AssignProcessToJobObject(self.handle.0, process) };
            if ok == 0 {
                return Err(last_error("AssignProcessToJobObject"));
            }
            Ok(())
        }

        /// 整树终止。对已经全部退出的 job 是幂等的。
        pub fn terminate(&self) -> io::Result<()> {
            // 安全性：仅传本类型拥有的有效 job 句柄与一个退出码常量。
            let ok = unsafe { TerminateJobObject(self.handle.0, 1) };
            if ok == 0 {
                return Err(last_error("TerminateJobObject"));
            }
            Ok(())
        }

        pub fn stats(&self) -> io::Result<JobStats> {
            let accounting = self.accounting()?;
            let extended = self.extended_limits()?;
            let (private_bytes, sampled_processes) = self.sample_private_bytes()?;
            Ok(JobStats {
                active_processes: accounting.ActiveProcesses,
                total_processes: accounting.TotalProcesses,
                total_terminated_processes: accounting.TotalTerminatedProcesses,
                peak_job_memory_bytes: extended.PeakJobMemoryUsed as u64,
                private_bytes,
                sampled_processes,
            })
        }

        fn accounting(&self) -> io::Result<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION> {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // 安全性：输出缓冲是本地 `#[repr(C)]` 值，长度与信息类别匹配；不关心返回长度，
            // 传 null 是该 API 允许的用法。
            let ok = unsafe {
                QueryInformationJobObject(
                    self.handle.0,
                    JobObjectBasicAccountingInformation,
                    std::ptr::from_mut(&mut info).cast(),
                    size_as_u32::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>(),
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(last_error("QueryInformationJobObject(BasicAccounting)"));
            }
            Ok(info)
        }

        fn extended_limits(&self) -> io::Result<JOBOBJECT_EXTENDED_LIMIT_INFORMATION> {
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            // 安全性：同 `accounting`，缓冲类型与信息类别一一对应。
            let ok = unsafe {
                QueryInformationJobObject(
                    self.handle.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_mut(&mut info).cast(),
                    size_as_u32::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(last_error("QueryInformationJobObject(ExtendedLimit)"));
            }
            Ok(info)
        }

        /// 枚举 job 内的 pid，逐个累加私有提交内存。
        ///
        /// 采样与进程退出天然竞态：打不开的进程直接跳过而不是让整次采样失败，否则水位策略
        /// 会在进程正常退出的瞬间失去输入。跳过了多少由 `sampled_processes` 反映。
        fn sample_private_bytes(&self) -> io::Result<(u64, u32)> {
            let mut total = 0_u64;
            let mut sampled = 0_u32;
            for pid in self.process_ids()? {
                if let Some(bytes) = process_private_bytes(pid) {
                    total = total.saturating_add(bytes);
                    sampled = sampled.saturating_add(1);
                }
            }
            Ok((total, sampled))
        }

        /// job 内当前全部进程的 pid（含 pi 自身与全部子孙进程）。
        ///
        /// 「关闭 Runtime 不泄漏工具子进程」这条验收需要的是**具体是谁**，不只是有几个：
        /// 只比数字的话，一个进程退出、另一个新起会互相抵消掉。
        pub fn process_ids(&self) -> io::Result<Vec<u32>> {
            // `JOBOBJECT_BASIC_PROCESS_ID_LIST` 是变长结构：两个 u32 头部之后跟 pid 数组。
            // 用 `Vec<usize>` 做后备存储 —— 元素类型与 pid 数组一致，对齐天然满足，
            // 头部占多少槽由 `offset_of!` 精确算出，不靠平台假设。
            let mut capacity = INITIAL_PID_CAPACITY;
            for _ in 0..PID_QUERY_ATTEMPTS {
                let slots = PID_HEADER_SLOTS + capacity;
                let mut buffer = vec![0_usize; slots];
                let bytes = u32::try_from(slots * std::mem::size_of::<usize>())
                    .expect("pid 缓冲尺寸恒小于 u32::MAX");
                // 安全性：缓冲由本地 `Vec<usize>` 拥有并已全量初始化，长度与传入的 `bytes`
                // 一致，对齐满足 `JOBOBJECT_BASIC_PROCESS_ID_LIST`（其最宽字段即 usize）。
                let ok = unsafe {
                    QueryInformationJobObject(
                        self.handle.0,
                        JobObjectBasicProcessIdList,
                        buffer.as_mut_ptr().cast(),
                        bytes,
                        std::ptr::null_mut(),
                    )
                };
                // 安全性：无论查询成败缓冲都已初始化，且 `slots >= PID_HEADER_SLOTS + 1`，
                // 足以容纳整个头部；该类型是 `Copy` 且全零位模式合法，`read` 不会构造出
                // 非法值。
                let header = unsafe {
                    buffer
                        .as_ptr()
                        .cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()
                        .read()
                };
                if ok == 0 {
                    // 安全性：紧接失败调用读取本线程最后错误码，无指针操作。
                    let code = unsafe { GetLastError() };
                    let assigned = header.NumberOfAssignedProcesses as usize;
                    if code == ERROR_MORE_DATA && assigned > capacity {
                        // 按内核报告的实际进程数翻倍扩容，多留的余量用来吸收两次查询之间
                        // 新起的进程。
                        capacity = assigned.saturating_mul(2);
                        continue;
                    }
                    return Err(last_error("QueryInformationJobObject(BasicProcessIdList)"));
                }
                let listed = (header.NumberOfProcessIdsInList as usize).min(capacity);
                let pids = buffer
                    .iter()
                    .skip(PID_HEADER_SLOTS)
                    .take(listed)
                    .filter_map(|slot| u32::try_from(*slot).ok())
                    .collect();
                return Ok(pids);
            }
            Err(io::Error::other(
                "QueryInformationJobObject(BasicProcessIdList) 连续扩容后仍报 ERROR_MORE_DATA",
            ))
        }
    }

    /// 单个进程的私有提交内存；打不开或查询失败返回 `None`（进程多半刚退出）。
    fn process_private_bytes(pid: u32) -> Option<u64> {
        let process = open_for_memory_query(pid)?;
        let size = size_as_u32::<PROCESS_MEMORY_COUNTERS_EX>();
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: size,
            ..PROCESS_MEMORY_COUNTERS_EX::default()
        };
        // 安全性：`PROCESS_MEMORY_COUNTERS_EX` 以 `PROCESS_MEMORY_COUNTERS` 为前缀布局，
        // 按 Win32 约定用 `cb` 声明实际缓冲长度后即可用前者的指针类型接收；缓冲是本地值，
        // 指针在调用期间有效，且 `size` 与它的真实长度一致。
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                process.0,
                std::ptr::from_mut(&mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
                size,
            )
        };
        if ok == 0 {
            return None;
        }
        Some(counters.PrivateUsage as u64)
    }

    fn open_for_memory_query(pid: u32) -> Option<OwnedHandle> {
        // 先要最小权限；个别环境下内存查询需要完整的 QUERY_INFORMATION，因此失败后再退一步。
        // 两者都拿不到就是进程已退出或权限不足，交由调用方跳过。
        for access in [
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
        ] {
            // 安全性：只传访问掩码与 pid，无指针；返回句柄立即判空并交给 `OwnedHandle` 托管。
            let raw = unsafe { OpenProcess(access, 0, pid) };
            if !raw.is_null() {
                return Some(OwnedHandle(raw));
            }
        }
        None
    }

    /// 恢复以 `CREATE_SUSPENDED` 创建的进程。
    ///
    /// 挂起创建的进程有且仅有初始线程，因此恢复它名下的全部线程等价于恢复初始线程。
    /// 一个线程都找不到说明进程已经消失，作为错误返回，调用方据此放弃这次启动。
    pub fn resume_process(pid: u32) -> io::Result<()> {
        // 安全性：只传标志位与 pid；返回值立即校验并交给 `OwnedHandle` 托管。
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            return Err(last_error("CreateToolhelp32Snapshot"));
        }
        let snapshot = OwnedHandle(raw);

        let mut entry = THREADENTRY32 {
            dwSize: size_as_u32::<THREADENTRY32>(),
            ..THREADENTRY32::default()
        };
        // 安全性：`entry` 是本地 `#[repr(C)]` 值且已按 API 要求填好 `dwSize`；快照句柄有效。
        let mut has_entry =
            unsafe { Thread32First(snapshot.0, std::ptr::from_mut(&mut entry)) } != 0;
        let mut resumed = 0_usize;
        while has_entry {
            if entry.th32OwnerProcessID == pid {
                resume_thread(entry.th32ThreadID)?;
                resumed += 1;
            }
            // 安全性：同上，循环内 `entry` 始终是同一个有效本地值。
            has_entry = unsafe { Thread32Next(snapshot.0, std::ptr::from_mut(&mut entry)) } != 0;
        }
        if resumed == 0 {
            return Err(io::Error::other(format!(
                "挂起的子进程 {pid} 没有可恢复的线程，多半已经退出"
            )));
        }
        Ok(())
    }

    fn resume_thread(tid: u32) -> io::Result<()> {
        // 安全性：只传访问掩码与线程 id；返回句柄立即判空并交给 `OwnedHandle` 托管。
        let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, tid) };
        if raw.is_null() {
            return Err(last_error("OpenThread"));
        }
        let thread = OwnedHandle(raw);
        // 安全性：仅传本地拥有的有效线程句柄。
        let previous = unsafe { ResumeThread(thread.0) };
        if previous == u32::MAX {
            return Err(last_error("ResumeThread"));
        }
        Ok(())
    }

    pub fn system_memory() -> io::Result<SystemMemory> {
        let mut status = MEMORYSTATUSEX {
            dwLength: size_as_u32::<MEMORYSTATUSEX>(),
            ..MEMORYSTATUSEX::default()
        };
        // 安全性：`status` 是本地 `#[repr(C)]` 值且已按 API 要求填好 `dwLength`。
        let ok = unsafe { GlobalMemoryStatusEx(std::ptr::from_mut(&mut status)) };
        if ok == 0 {
            return Err(last_error("GlobalMemoryStatusEx"));
        }
        Ok(SystemMemory {
            total_bytes: status.ullTotalPhys,
            available_bytes: status.ullAvailPhys,
            load_percent: status.dwMemoryLoad,
        })
    }

    /// 控制台子进程不分配可见窗口。
    ///
    /// 父进程若是 `windows` 子系统（打包后的 `gpui-pi.exe`），不带这个标志时每个
    /// `git` / `pi` / `taskkill` 都会各开一个空终端（Win11 默认看起来像 PowerShell）。
    pub const fn no_window_creation_flags() -> u32 {
        CREATE_NO_WINDOW
    }

    /// 子进程创建标志：挂起创建，等纳入 job 后再放行；同时隐藏控制台窗口。
    pub const fn suspended_creation_flags() -> u32 {
        CREATE_SUSPENDED | CREATE_NO_WINDOW
    }

    /// Win32 的长度字段一律是 `u32`；这些结构体尺寸都是编译期常量且远小于 4GiB。
    fn size_as_u32<T>() -> u32 {
        u32::try_from(std::mem::size_of::<T>()).expect("Win32 结构体尺寸恒小于 u32::MAX")
    }

    fn last_error(api: &str) -> io::Error {
        // 安全性：读取本线程的最后错误码，无指针操作。
        let code = unsafe { GetLastError() };
        io::Error::other(format!("{api} 失败（Win32 错误 {code}）"))
    }
}

/// 非 Windows 平台的占位实现。
///
/// 项目已迁移为 Windows solo，这里只保证四个纯逻辑 crate 仍能在其他平台编译与单测：
/// 创建/纳管是 no-op，统计与整树终止明确报 `Unsupported`，由调用方回退到
/// [`crate::kill_process_tree`]，绝不假装成功。
#[cfg(not(windows))]
mod imp {
    use super::{JobLimits, JobStats, SystemMemory};
    use std::io;
    use std::process::Child;

    fn unsupported<T>(what: &str) -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{what} 仅在 Windows 上可用"),
        ))
    }

    pub struct JobObject;

    impl JobObject {
        pub fn create(_limits: JobLimits) -> io::Result<Self> {
            Ok(Self)
        }

        pub fn assign(&self, _child: &Child) -> io::Result<()> {
            Ok(())
        }

        pub fn terminate(&self) -> io::Result<()> {
            unsupported("Job Object 整树终止")
        }

        pub fn stats(&self) -> io::Result<JobStats> {
            unsupported("Job Object 进程树统计")
        }

        pub fn process_ids(&self) -> io::Result<Vec<u32>> {
            unsupported("Job Object 进程树 pid 枚举")
        }
    }

    pub fn resume_process(_pid: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn system_memory() -> io::Result<SystemMemory> {
        unsupported("系统内存采样")
    }

    pub const fn no_window_creation_flags() -> u32 {
        0
    }

    pub const fn suspended_creation_flags() -> u32 {
        0
    }
}

pub use imp::{
    JobObject, no_window_creation_flags, resume_process, suspended_creation_flags, system_memory,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_matches_platform() {
        assert_eq!(job_objects_supported(), cfg!(windows));
    }

    #[test]
    fn used_bytes_never_underflows() {
        let memory = SystemMemory {
            total_bytes: 4,
            available_bytes: 9,
            load_percent: 0,
        };
        assert_eq!(memory.used_bytes(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn system_memory_is_plausible() {
        let memory = system_memory().expect("Windows 上系统内存采样必须成功");
        assert!(memory.total_bytes > 0, "总内存不应为 0");
        assert!(
            memory.available_bytes <= memory.total_bytes,
            "可用内存 {} 不应超过总内存 {}",
            memory.available_bytes,
            memory.total_bytes
        );
        assert!(memory.load_percent <= 100, "占用百分比应在 0–100");
    }

    #[cfg(windows)]
    #[test]
    fn spawn_flags_hide_console_and_start_suspended() {
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        assert_eq!(no_window_creation_flags(), CREATE_NO_WINDOW);
        assert_eq!(
            suspended_creation_flags(),
            CREATE_SUSPENDED | CREATE_NO_WINDOW
        );
    }

    #[cfg(windows)]
    #[test]
    fn empty_job_reports_no_processes() {
        let job = JobObject::create(JobLimits::default()).expect("创建 job 应成功");
        let stats = job.stats().expect("空 job 也应可统计");
        assert_eq!(stats.active_processes, 0);
        assert_eq!(stats.total_processes, 0);
        assert_eq!(stats.private_bytes, 0);
        assert_eq!(stats.sampled_processes, 0);
    }
}
