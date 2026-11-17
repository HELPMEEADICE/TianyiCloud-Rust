//! Linux FUSE 后端（只读挂载）
//!
//! 与 Windows 侧共用 [crate::fs::CloudFs]：路径解析、目录缓存、严格 Range 读取全在那边，
//! 这里只把 FUSE 请求翻译成同一次调用。本文件在 Windows 构建里不参与编译（lib.rs 用 cfg
//! 关掉），因此 Windows 提交可以完全不依赖 libfuse。
//!
//! # 挂载时序（为什么 start 是同步返回的）
//!
//! ```text
//! start()
//!   ├─ 挂载点体检：不存在就 mkdir -p，非空目录直接报错
//!   ├─ Session::new(fs, mountpoint, options)     ← 真正的 mount() 在这里发生
//!   │    成功：mount 已生效；失败：io::Error 原样转成挂载错误返回给上层
//!   ├─ Session::unmount_callable() → SessionUnmounter（卸载闭包用它）
//!   ├─ 后台线程：Session::run()                  ← 请求分发循环
//!   └─ 主线程：等 init() 回调发来的 ready（15 秒超时）
//! ```
//!
//! 关键点是「已挂载」不等于「已经开始工作」：`mount()` 只把 FUSE 挂上，内核要等
//! INIT 请求被答复后才会让挂载点可用。`Filesystem::init` 正好是这个时刻，所以
//! ready 信号从 init 里发出，主线程拿到它才算挂载成功——这样 `start` 返回 Ok 的那一刻
//! 挂载点就已经可以被 `ls` 了，而不是让调用方撞上一个会卡住的目录。
//!
//! `mount()` 本身放在主线程同步执行：它是本地系统调用（外加一次 fusermount 子进程），
//! 失败原因（没有 /dev/fuse、没有权限、目录被占用）原样返回，比通过 channel 传一句
//! 字符串更精确。
//!
//! # 只读约定
//!
//! 所有写语义（write / create / mknod / mkdir / unlink / rmdir / rename / symlink /
//! link / setattr / setxattr / removexattr / fallocate / copy_file_range）一律回
//! **EROFS**（统一走 `deny_write`）。选 EROFS 而不是 EACCES：Linux 上 EROFS 就是
//! 「文件系统只读」的语义，用户看到的是 "Read-only file system"；EACCES 会被误读成
//! 「这个文件的权限不够」。mount option 也用 `ro`，内核自己先拦一层，用户态这层是兜底。
//!
//! # inode 方案
//!
//! 一张表两向索引，**ino 由本模块自己分配**（从 2 递增，1 恒为根）：
//!
//! * `by_ino: HashMap<u64, InodeRecord>` —— FUSE 的 `ino` → 条目 + 它的路径段 + 父 ino；
//! * `by_id: HashMap<String, u64>` —— 云端对象 id → `ino`（同一个对象无论从哪条路径
//!   命中都是同一个 ino，这是内核判断「是不是同一个文件」的依据）。
//!
//! 不用「按 id 哈希直接当 ino」的理由：哈希撞了就会让两个不同文件共享一个 ino，读出来的
//! 内容属于谁全看运气。自增 ino 天生不撞，代价只是多一张反查表（每个被访问过的对象
//! 一条，几十字节）。
//!
//! 生命周期按 FUSE 协议走：`lookup` 加一次引用，`forget(n)` 减 n 次，引用归零才允许
//! 淘汰。同时给表加硬上限 `MAX_INODES`，超了就淘汰「引用为 0」的条目——它们是内核
//! 已经不再引用的，丢掉不会产生悬空句柄。引用数大于 0 的条目一概不动（所以上限是
//! 「被内核持有的对象数 + MAX_INODES」，不是绝对值）。
//!
//! # 内核缓存与 direct_io 的取舍
//!
//! `open` 返回 **FOPEN_DIRECT_IO**：内核不为该文件保留页缓存，每次 `read(2)` 都直接
//! 变成一个 FUSE READ 请求。这是「零磁盘文件缓存」在 Linux 上唯一能由本模块决定的部分
//! ——否则内核会把读过的页留在内存里，文件被远端改过还照样返回旧内容。
//!
//! 代价（必须说清）：
//!
//! * **mmap 不可用**：direct_io 的文件在内核里没有页缓存，`mmap` 会失败，因此
//!   「把云盘里的可执行文件直接跑起来」、以及部分用 mmap 读文件的编辑器会受限；
//! * 每次读都要一次网络 Range，顺序读的放大倍数取决于上层 `read(2)` 的块大小
//!   （`cat` / `cp` 通常是 128 KiB 级别，与 FUSE 的 max_read 一致）；
//! * 没有预读，随机小读的延迟就是一次 HTTP 往返。
//!
//! 如果以后更需要 mmap 兼容性，把 `OPEN_FLAGS` 换成 `FOPEN_KEEP_CACHE` 即可：那时内核
//! 会缓存页（内存），仍然不落盘，但远端改动不会立刻可见——这是一次显式的语义交换，
//! 不应该悄悄发生。
//!
//! # 已知限制
//!
//! * fuser 的会话循环是**单线程串行派发**：一个慢请求会占住整条队列（包括别的读）。
//!   这里靠 `OP_TIMEOUT` 给每个云端请求封顶 30 秒，避免永久阻塞，但不做请求级并发；
//! * 元数据只有 `ENTRY_TTL`（1 秒）的寿命：`getattr` 不联网，`lookup` 才重新解析，
//!   所以远端改名的可见延迟是「TTL + 下一次 lookup」；
//! * 进程被 SIGKILL 后挂载点会留下一个死挂载（内核报 Transport endpoint is not
//!   connected），需要手动 `fusermount3 -u <挂载点>`。没有启用 `auto_unmount`：它要求
//!   `allow_other`，在没配 /etc/fuse.conf 的机器上会让挂载直接失败，代价大于收益。

use crate::error::Error;
use crate::fs::CloudFs;
use crate::meta::Entry;
use crate::mount::{MountOptions, MountTarget, PlatformMount};
use crate::paths;
use crate::runtime;
use fuser::{
    consts::FOPEN_DIRECT_IO, FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, Session, SessionUnmounter,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// 根目录的 inode 号（FUSE 协议固定：内核认为 1 就是挂载点本身）
const ROOT_INO: u64 = 1;

/// 目录项属性的缓存时间
///
/// 取 1 秒而不是 30 秒：元数据不联网（`getattr` 直接用快照），只有 `lookup` 会刷新，
/// TTL 越短远端改动越早被看见，代价是多几次 `lookup`（命中目录缓存，不花网络）。
const ENTRY_TTL: Duration = Duration::from_secs(1);

/// 单个云端请求的硬上限
const OP_TIMEOUT: Duration = Duration::from_secs(30);

/// 等待「内核 INIT 完成」的上限
const READY_TIMEOUT: Duration = Duration::from_secs(15);

/// `statfs` 的块大小（同时作为 `st_blksize` 报出去）
const IO_BLOCK: u64 = 4096;

/// `st_blocks` 的单位：POSIX 规定是 512 字节
const BLOCK_UNIT: u64 = 512;

/// inode 表上限
const MAX_INODES: usize = 65_536;

/// `open` 返回的 FOPEN_* 标志（取舍见模块文档）
const OPEN_FLAGS: u32 = FOPEN_DIRECT_IO;

/// /proc/mounts 里显示的名字
const FS_NAME: &str = "tianyi-cloud";
const FS_SUBTYPE: &str = "tianyi";

/// 后台线程名
const THREAD_NAME: &str = "tianyi-mount-fuse";

/// 交给 [crate::mount::MountStatus] 的后端标识（与 Windows 的 "winfsp" 对称）
const BACKEND: &str = "fuse";

/// POSIX 常量
///
/// 这里刻意不引入 libc：Linux 的 errno / fcntl 编号在 asm-generic 系架构
/// （x86 / x86_64 / arm / aarch64 / riscv / ppc / s390x）上完全一致，取需要的几个即可。
/// 编号取自内核 `include/uapi/asm-generic/errno{,-base}.h` 与 `fcntl.h`，并逐条与
/// libc crate 的 `unix/linux_like/linux_l4re_shared.rs`（EPERM..ERANGE 段）和
/// `unix/linux_like/linux/mod.rs`（ENAMETOOLONG..ESTALE 段）核对过。
/// mips / sparc 等使用另一套编号的架构不在本模块的支持范围内。
mod posix {
    use std::os::raw::c_int;

    /// 对象不存在
    pub const ENOENT: c_int = 2;
    /// 未分类的 IO 失败（云端错误、Range 不合规等一律折到这里）
    pub const EIO: c_int = 5;
    /// 文件句柄无效
    pub const EBADF: c_int = 9;
    /// 对文件做目录操作
    pub const ENOTDIR: c_int = 20;
    /// 对目录做文件读取
    pub const EISDIR: c_int = 21;
    /// 名字不合法（路径里出现上级引用）
    pub const EINVAL: c_int = 22;
    /// 只读文件系统
    pub const EROFS: c_int = 30;
    /// 请求超时
    pub const ETIMEDOUT: c_int = 110;

    /// `access(2)` 的「要写」位
    pub const W_OK: c_int = 2;
    /// `open(2)` 的访问模式掩码
    pub const O_ACCMODE: c_int = 0o3;
}

/// ready 通道：`Ok(())` = 内核 INIT 已答复，挂载点可用
type ReadySender = Sender<std::result::Result<(), String>>;
type ReadySlot = Arc<Mutex<Option<ReadySender>>>;

/// 启动 Linux FUSE 只读挂载
///
/// 返回前保证：挂载点已建立、`mount()` 已生效、内核 INIT 已答复（挂载点真的能用）。
/// 任何一步失败都返回明确错误，绝不返回 Ok。
pub fn start(
    cloud: Arc<CloudFs>,
    options: &MountOptions,
    target: &MountTarget,
) -> crate::error::Result<(Arc<PlatformMount>, &'static str)> {
    let dir = match target {
        MountTarget::Directory(dir) => dir.clone(),
        MountTarget::Drive(letter) => {
            return Err(Error::InvalidMountPoint(format!(
                "Linux 只支持目录挂载，收到的是盘符 {}:",
                *letter as char
            )))
        }
    };
    let dir = prepare_mount_dir(&dir)?;
    if !options.read_only {
        // 写路径根本没有实现：与其让用户以为能编辑，不如挂载前就说清楚
        return Err(Error::InvalidMountPoint(
            "本实现只支持只读挂载（read_only 必须为 true）".into(),
        ));
    }

    let mount_point = dir.display().to_string();
    let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();
    let ready: ReadySlot = Arc::new(Mutex::new(Some(ready_tx)));
    let filesystem = CloudFuse::create(cloud, ready.clone());

    let mut session = Session::new(filesystem, &dir, &mount_options()).map_err(|e| {
        Error::Other(format!(
            "FUSE 挂载 {mount_point} 失败：{e}（确认已加载 fuse 模块、当前用户在 fuse 组里、\
             且有 fusermount3 或 root 权限）"
        ))
    })?;
    let unmounter: Arc<Mutex<SessionUnmounter>> = Arc::new(Mutex::new(session.unmount_callable()));

    let log_dir = mount_point.clone();
    let thread = std::thread::Builder::new()
        .name(THREAD_NAME.into())
        .spawn(move || {
            let mut session = session;
            let outcome = session.run();
            // 走到这里说明请求循环结束了。如果 init 从未被调用（ready 还留在槽里），
            // 那这次挂载从来没有真正可用过，必须让主线程知道，而不是让它干等到超时。
            if let Some(tx) = lock_ready(&ready).take() {
                let msg = match &outcome {
                    Ok(()) => format!("FUSE 会话在就绪前就结束了（{log_dir}）"),
                    Err(e) => format!("FUSE 会话在就绪前失败：{e}（{log_dir}）"),
                };
                let _ = tx.send(Err(msg));
            }
            match outcome {
                Ok(()) => log::info!("FUSE 会话已结束（{log_dir}）"),
                Err(e) => log::warn!("FUSE 会话异常结束（{log_dir}）：{e}"),
            }
        })
        .map_err(|e| Error::Other(format!("创建 FUSE 会话线程失败：{e}")))?;

    match ready_rx.recv_timeout(READY_TIMEOUT) {
        Ok(Ok(())) => {
            log::info!(
                "FUSE 只读挂载已就绪：{mount_point}（空间 {}）",
                options.space.as_str()
            );
        }
        Ok(Err(msg)) => {
            unmount_session(&unmounter);
            let _ = thread.join();
            return Err(Error::NotReady(msg));
        }
        Err(_) => {
            unmount_session(&unmounter);
            let _ = thread.join();
            return Err(Error::NotReady(format!(
                "FUSE 在 {READY_TIMEOUT:?} 内没有完成挂载初始化（{mount_point}）"
            )));
        }
    }

    let unmount = {
        let unmounter = unmounter.clone();
        Box::new(move || unmount_session(&unmounter))
    };
    let platform = PlatformMount::new(unmount, thread, mount_point);
    Ok((Arc::new(platform), BACKEND))
}

/// 幂等的卸载动作
///
/// [crate::mount::PlatformMount::stop] 已经保证只调一次，这里再幂等一次：
/// `SessionUnmounter::unmount` 的实现是 `drop(mem::take(&mut self.mount))`，第二次调用
/// 拿到 None，什么也不做。
fn unmount_session(unmounter: &Mutex<SessionUnmounter>) {
    let mut guard = unmounter.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = guard.unmount() {
        log::warn!("FUSE 卸载失败：{e}");
    }
}

/// FUSE 挂载参数
///
/// * `RO` —— 只读（内核层面就先拦写）；
/// * `NoDev` / `NoSuid` —— 云端对象不可信，绝不能出现设备节点或 setuid 位；
/// * `DefaultPermissions` —— 让内核按我们报的 0o444/0o555 做权限检查，省掉每个
///   `access()` 一次用户态往返，也让「只读」在权限位上就自洽；
/// * `FSName` / `Subtype` —— 固定 ASCII，/proc/mounts 与 findmnt 里能认出来。
///
/// 刻意不加 `AutoUnmount`（需要 `allow_other`，没配 `user_allow_other` 的机器会直接
/// 挂不上，见模块文档），也刻意不加 `CUSTOM("direct_io")`：`direct_io` 不是内核 mount
/// 参数，传给 `mount(2)` 会被内核以 EINVAL 拒绝；它只能通过 `open` 返回
/// FOPEN_DIRECT_IO 来表达。
fn mount_options() -> Vec<MountOption> {
    vec![
        MountOption::RO,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::DefaultPermissions,
        MountOption::FSName(FS_NAME.to_string()),
        MountOption::Subtype(FS_SUBTYPE.to_string()),
    ]
}

/// 挂载点体检：不存在就建，存在但非空就报错
///
/// 返回规范化后的绝对路径（作为 [crate::mount::PlatformMount::actual_mount_point] 的值）。
fn prepare_mount_dir(dir: &Path) -> crate::error::Result<PathBuf> {
    match std::fs::metadata(dir) {
        Ok(md) if md.is_dir() => {}
        Ok(_) => {
            return Err(Error::InvalidMountPoint(format!(
                "挂载点不是目录：{}",
                dir.display()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir).map_err(|e| {
                Error::InvalidMountPoint(format!("无法创建挂载点 {}：{e}", dir.display()))
            })?;
        }
        Err(e) => {
            return Err(Error::InvalidMountPoint(format!(
                "无法访问挂载点 {}：{e}。如果这是上次异常退出留下的死挂载点，\
                 请先执行 fusermount3 -u {}",
                dir.display(),
                dir.display()
            )))
        }
    }

    let mut entries = std::fs::read_dir(dir).map_err(|e| {
        Error::MountPointBusy(format!(
            "无法读取挂载点 {}：{e}。如果这是上次异常退出留下的死挂载点\
             （Transport endpoint is not connected），请先执行 fusermount3 -u {}",
            dir.display(),
            dir.display()
        ))
    })?;
    if entries.next().is_some() {
        return Err(Error::MountPointBusy(format!(
            "挂载点 {} 不是空目录：FUSE 会盖住目录里已有的内容，请换一个空目录",
            dir.display()
        )));
    }
    Ok(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
}

/// 把云端错误折成 errno
///
/// 分类原则：只有「确实不存在」才给 ENOENT（调用方会据此清缓存/报路径不存在），超时给
/// ETIMEDOUT，其余（网络、Range 不合规、服务端行为异常、只读、容量不可用）一律 EIO
/// ——它们对用户都是「读失败了」，细分信息留在日志与 [crate::stats] 里。
fn errno_for(err: &Error) -> c_int {
    match err {
        Error::NotFound(_) => posix::ENOENT,
        Error::Cloud(tianyi_core::Error::NotFound(_)) => posix::ENOENT,
        Error::Timeout(_) => posix::ETIMEDOUT,
        Error::InvalidPath(_) => posix::EINVAL,
        Error::ReadOnly(_) => posix::EROFS,
        _ => posix::EIO,
    }
}

/// 写操作的统一答复
///
/// 只读挂载里所有写语义都走这里，保证「永远是同一个 errno」，也让以后要改成 EACCES
/// 时只改一处。
fn deny_write(op: &str) -> c_int {
    log::debug!("只读挂载，拒绝 {op}");
    posix::EROFS
}

/// 同步等待一个 future，超过 [crate::runtime::block_on_timeout] 的时限就放弃
///
/// 用 [crate::runtime::block_on_timeout] 桥接：FUSE 的请求线程不是 Tokio 线程，
/// 这里绝不能自己 `Runtime::new()`（每个请求建一个运行时会拖垮内存与 fd）。
fn deadline<T>(fut: impl std::future::Future<Output = T>) -> std::result::Result<T, c_int> {
    match runtime::block_on_timeout(OP_TIMEOUT, fut) {
        Ok(value) => Ok(value),
        Err(_) => {
            log::warn!("云端请求在 {OP_TIMEOUT:?} 内没有完成，已放弃");
            Err(posix::ETIMEDOUT)
        }
    }
}

/// 等待并把云端错误映射成 errno 的版本
fn cloud_op<T>(
    fut: impl std::future::Future<Output = crate::error::Result<T>>,
) -> std::result::Result<T, c_int> {
    match deadline(fut) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => {
            log::warn!("云端请求失败：{}", e.brief());
            Err(errno_for(&e))
        }
        Err(code) => Err(code),
    }
}

/// 属性：只读、无设备、时间全部取云端时间
///
/// uid/gid 取请求方。看起来与「取当前进程」不同，但本挂载**不用** `allow_other` /
/// `allow_root`，fuser 的会话 ACL（`SessionACL::Owner`）会把非属主请求直接拒掉，
/// 所以到达这里的 uid/gid 必然就是挂载进程的 uid/gid（同一个用户的不同进程 uid 相同）。
fn attr_of(ino: u64, entry: &Entry, uid: u32, gid: u32) -> FileAttr {
    FileAttr {
        ino,
        size: entry.size,
        blocks: entry.size.div_ceil(BLOCK_UNIT),
        atime: entry.modified,
        mtime: entry.modified,
        ctime: entry.modified,
        crtime: entry.created,
        kind: kind_of(entry),
        perm: if entry.is_dir { 0o555 } else { 0o444 },
        nlink: if entry.is_dir { 2 } else { 1 },
        uid,
        gid,
        rdev: 0,
        blksize: IO_BLOCK as u32,
        flags: 0,
    }
}

fn kind_of(entry: &Entry) -> FileType {
    if entry.is_dir {
        FileType::Directory
    } else {
        FileType::RegularFile
    }
}

fn lock_ready(slot: &ReadySlot) -> MutexGuard<'_, Option<ReadySender>> {
    slot.lock().unwrap_or_else(|e| e.into_inner())
}

/// 一个 inode 的全部信息
#[derive(Clone, Debug)]
struct InodeRecord {
    /// 云端条目快照（大小/时间用于属性与读取边界）
    entry: Entry,
    /// 到该对象的路径段（readdir / list_dir / resolve 都按 `&[String]` 工作）
    segments: Vec<String>,
    /// 父目录的 ino（`..` 用）
    parent: u64,
    /// 内核还没 forget 掉的引用数（0 = 可淘汰）
    lookups: u64,
}

/// inode 表（两向索引，见模块文档）
#[derive(Debug)]
struct InodeTable {
    by_ino: HashMap<u64, InodeRecord>,
    by_id: HashMap<String, u64>,
    next_ino: u64,
}

impl InodeTable {
    fn with_root(root: Entry) -> Self {
        let mut by_id = HashMap::new();
        let mut by_ino = HashMap::new();
        by_id.insert(root.id.clone(), ROOT_INO);
        by_ino.insert(
            ROOT_INO,
            InodeRecord {
                entry: root,
                segments: Vec::new(),
                parent: ROOT_INO,
                lookups: 1,
            },
        );
        Self {
            by_ino,
            by_id,
            next_ino: ROOT_INO + 1,
        }
    }

    /// 登记（或刷新）一个对象，返回它的 ino
    ///
    /// `looked_up` 区分两种来源：`lookup` 要加一次引用，`readdir` 只是「顺便建表」
    /// （内核还没有引用它，随时可以淘汰）。
    fn upsert(&mut self, entry: Entry, segments: Vec<String>, parent: u64, looked_up: bool) -> u64 {
        if let Some(&ino) = self.by_id.get(&entry.id) {
            if let Some(rec) = self.by_ino.get_mut(&ino) {
                if looked_up {
                    rec.lookups = rec.lookups.saturating_add(1);
                }
                // 路径与属性总是以最新一次解析为准：远端改名/改大小后，同一个 id 的
                // ino 保持不变，但路径段与大小要跟着更新。
                rec.segments = segments;
                rec.parent = parent;
                rec.entry = entry;
            }
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino = self.next_ino.saturating_add(1);
        self.by_id.insert(entry.id.clone(), ino);
        self.by_ino.insert(
            ino,
            InodeRecord {
                entry,
                segments,
                parent,
                lookups: u64::from(looked_up),
            },
        );
        ino
    }

    /// 引用归零就忘掉它（根永远留着）
    fn forget(&mut self, ino: u64, nlookup: u64) {
        if ino == ROOT_INO {
            return;
        }
        let remove = match self.by_ino.get_mut(&ino) {
            Some(rec) => {
                rec.lookups = rec.lookups.saturating_sub(nlookup);
                rec.lookups == 0
            }
            None => false,
        };
        if remove {
            if let Some(rec) = self.by_ino.remove(&ino) {
                self.by_id.remove(&rec.entry.id);
            }
        }
    }

    /// 超上限时按「引用为 0」淘汰，直到回到一半
    ///
    /// 只动引用为 0 的条目：内核还引用着的 ino 一旦消失，之后的 `getattr` 就只能报
    /// ENOENT，用户态会看到 ESTALE。引用数大于 0 的条目全部保留，所以表的实际规模受
    /// 「内核持有的对象数」支配，而不是本表的硬上限。
    fn evict_if_needed(&mut self) {
        if self.by_ino.len() <= MAX_INODES {
            return;
        }
        let excess = self.by_ino.len().saturating_sub(MAX_INODES / 2);
        let victims: Vec<u64> = self
            .by_ino
            .iter()
            .filter(|(ino, rec)| **ino != ROOT_INO && rec.lookups == 0)
            .map(|(ino, _)| *ino)
            .take(excess)
            .collect();
        for ino in victims {
            if let Some(rec) = self.by_ino.remove(&ino) {
                self.by_id.remove(&rec.entry.id);
            }
        }
    }
}

/// FUSE 文件系统实现
struct CloudFuse {
    cloud: Arc<CloudFs>,
    inodes: Mutex<InodeTable>,
    /// 打开的句柄：fh → ino（只读，所以句柄里不需要偏移量）
    handles: Mutex<HashMap<u64, u64>>,
    next_fh: AtomicU64,
    /// 内核 INIT 完成时发 ready（只发一次）
    ready: ReadySlot,
}

impl CloudFuse {
    fn create(cloud: Arc<CloudFs>, ready: ReadySlot) -> Self {
        let root = cloud.root_entry();
        Self {
            cloud,
            inodes: Mutex::new(InodeTable::with_root(root)),
            handles: Mutex::new(HashMap::new()),
            // 句柄号从 1 开始：0 留给「不是文件句柄」的场合（readdir 的 fh 就是 0）
            next_fh: AtomicU64::new(1),
            ready,
        }
    }

    fn lock_inodes(&self) -> MutexGuard<'_, InodeTable> {
        self.inodes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_handles(&self) -> MutexGuard<'_, HashMap<u64, u64>> {
        self.handles.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, ino: u64) -> Option<InodeRecord> {
        self.lock_inodes().by_ino.get(&ino).cloned()
    }

    fn ensure(&self, entry: Entry, segments: Vec<String>, parent: u64, looked_up: bool) -> u64 {
        let mut table = self.lock_inodes();
        let ino = table.upsert(entry, segments, parent, looked_up);
        table.evict_if_needed();
        ino
    }

    fn forget_inode(&self, ino: u64, nlookup: u64) {
        self.lock_inodes().forget(ino, nlookup);
    }

    fn new_handle(&self, ino: u64) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.lock_handles().insert(fh, ino);
        fh
    }

    fn close_handle(&self, fh: u64) {
        self.lock_handles().remove(&fh);
    }

    /// 句柄是否属于这个 ino（防止程序拿错 fh 读到别人的内容）
    fn handle_matches(&self, fh: u64, ino: u64) -> bool {
        self.lock_handles().get(&fh) == Some(&ino)
    }

    /// 把内核给的名字变成一段合法的云端路径段
    ///
    /// FUSE 的 `name` 理论上不含 `/`，但仍然过一遍 [crate::paths::split_remote_path]：
    /// 它会把 `..` 拒掉、把空段与 `.` 丢掉，于是「内核传来的名字」永远不可能把解析带出
    /// 父目录之外。返回 None 表示这个名字不可能对应云端对象。
    fn child_segments(parent: &[String], name: &str) -> Option<Vec<String>> {
        let mut segments = parent.to_vec();
        let mut parts = paths::split_remote_path(name).ok()?;
        if parts.len() != 1 {
            return None;
        }
        segments.append(&mut parts);
        Some(segments)
    }
}

impl Filesystem for CloudFuse {
    /// 内核 INIT 已答复：这一刻挂载点才真正可用
    fn init(
        &mut self,
        _req: &Request<'_>,
        _config: &mut KernelConfig,
    ) -> std::result::Result<(), c_int> {
        log::info!("FUSE 内核 INIT 完成，挂载点已可用");
        if let Some(tx) = lock_ready(&self.ready).take() {
            let _ = tx.send(Ok(()));
        }
        Ok(())
    }

    fn destroy(&mut self) {
        log::debug!("FUSE 会话销毁");
    }

    fn lookup(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_rec) = self.record(parent) else {
            reply.error(posix::ENOENT);
            return;
        };
        if !parent_rec.entry.is_dir {
            reply.error(posix::ENOTDIR);
            return;
        }
        let Some(name) = name.to_str() else {
            // 云端名字来自 JSON，一定是 UTF-8；不是 UTF-8 的名字不可能存在
            reply.error(posix::ENOENT);
            return;
        };
        if name == "." {
            reply.entry(
                &ENTRY_TTL,
                &attr_of(parent, &parent_rec.entry, req.uid(), req.gid()),
                0,
            );
            return;
        }
        if name == ".." {
            let Some(up) = self.record(parent_rec.parent) else {
                reply.error(posix::ENOENT);
                return;
            };
            reply.entry(
                &ENTRY_TTL,
                &attr_of(parent_rec.parent, &up.entry, req.uid(), req.gid()),
                0,
            );
            return;
        }
        let Some(segments) = Self::child_segments(&parent_rec.segments, name) else {
            reply.error(posix::ENOENT);
            return;
        };
        match cloud_op(self.cloud.resolve(&segments)) {
            Ok(entry) => {
                let ino = self.ensure(entry.clone(), segments, parent, true);
                reply.entry(&ENTRY_TTL, &attr_of(ino, &entry, req.uid(), req.gid()), 0);
            }
            Err(code) => reply.error(code),
        }
    }

    fn forget(&mut self, _req: &Request<'_>, ino: u64, nlookup: u64) {
        self.forget_inode(ino, nlookup);
    }

    /// 属性直接返回内存里的快照，不联网
    ///
    /// 内核会非常频繁地 `getattr`（`ls -l`、复制进度、`stat` 循环），在这里做网络请求
    /// 会把整条串行队列拖死。快照的新鲜度由 TTL + `lookup` 保证。
    fn getattr(&mut self, req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.record(ino) {
            Some(rec) => reply.attr(&ENTRY_TTL, &attr_of(ino, &rec.entry, req.uid(), req.gid())),
            None => reply.error(posix::ENOENT),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        reply.error(deny_write("setattr"));
    }

    fn mknod(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(deny_write("mknod"));
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(deny_write("mkdir"));
    }

    fn unlink(&mut self, _req: &Request<'_>, _parent: u64, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(deny_write("unlink"));
    }

    fn rmdir(&mut self, _req: &Request<'_>, _parent: u64, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(deny_write("rmdir"));
    }

    fn symlink(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _link_name: &OsStr,
        _target: &Path,
        reply: ReplyEntry,
    ) {
        reply.error(deny_write("symlink"));
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _newparent: u64,
        _newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(deny_write("rename"));
    }

    fn link(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _newparent: u64,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(deny_write("link"));
    }

    /// 打开文件：登记一个句柄并声明 direct_io
    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        if (flags & posix::O_ACCMODE) != 0 {
            // `ro` 挂载选项本来就会拦住，这里是第二道闸
            reply.error(deny_write("open(O_WRONLY/O_RDWR)"));
            return;
        }
        let Some(rec) = self.record(ino) else {
            reply.error(posix::ENOENT);
            return;
        };
        if rec.entry.is_dir {
            reply.error(posix::EISDIR);
            return;
        }
        let fh = self.new_handle(ino);
        reply.opened(fh, OPEN_FLAGS);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            reply.error(posix::EINVAL);
            return;
        }
        let Some(rec) = self.record(ino) else {
            reply.error(posix::ENOENT);
            return;
        };
        if rec.entry.is_dir {
            reply.error(posix::EISDIR);
            return;
        }
        if !self.handle_matches(fh, ino) {
            reply.error(posix::EBADF);
            return;
        }
        // 越界与零长度读取直接回空：省一次网络，也天然满足 FUSE 的 EOF 语义
        if size == 0 || offset as u64 >= rec.entry.size {
            reply.data(&[]);
            return;
        }
        match cloud_op(self.cloud.read(&rec.entry, offset as u64, size)) {
            Ok(data) => reply.data(&data),
            Err(code) => reply.error(code),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.close_handle(fh);
        reply.ok();
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        // 目录句柄没有状态（readdir 每次按 offset 重算），直接放行
        reply.ok();
    }

    /// 列目录：`.` / `..` 加云端子项，按 offset 分页
    ///
    /// offset 的约定：第 i 项（0 起）递交时告诉内核「下一次从 i+1 开始」，内核下次就把
    /// i+1 原样传回来。这样即使一屏装不下（`reply.add` 返回 true），下一次调用也能精确
    /// 续上，不漏项也不重复。
    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(rec) = self.record(ino) else {
            reply.error(posix::ENOENT);
            return;
        };
        if !rec.entry.is_dir {
            reply.error(posix::ENOTDIR);
            return;
        }
        let listing = match cloud_op(self.cloud.list_dir(&rec.segments)) {
            Ok(listing) => listing,
            Err(code) => {
                reply.error(code);
                return;
            }
        };

        let mut index = offset.max(0) as usize;
        loop {
            let (child_ino, kind, name, next) = match index {
                0 => (ino, FileType::Directory, ".".to_string(), 1usize),
                1 => (rec.parent, FileType::Directory, "..".to_string(), 2usize),
                n => match listing.entries.get(n - 2) {
                    Some(child) => match Self::child_segments(&rec.segments, &child.name) {
                        Some(segments) => {
                            let found = self.ensure(child.clone(), segments, ino, false);
                            (found, kind_of(child), child.name.clone(), n + 1)
                        }
                        // 云端名字脏到不能当路径段：跳过它，而不是让整个目录失败
                        None => {
                            index = n + 1;
                            continue;
                        }
                    },
                    None => break,
                },
            };
            if reply.add(child_ino, next as i64, kind, name.as_str()) {
                // 缓冲区满了：内核会带着 offset 再来一次
                break;
            }
            index = next;
        }
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        // capacity() 自己就把失败折成 (0, 0) 并记进日志，这里只管换算成块
        let (total, free) = deadline(self.cloud.capacity()).unwrap_or((0, 0));
        let blocks = total / IO_BLOCK;
        let bfree = free / IO_BLOCK;
        reply.statfs(
            blocks,
            bfree,
            bfree,
            0,
            0,
            IO_BLOCK as u32,
            255,
            IO_BLOCK as u32,
        );
    }

    fn setxattr(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(deny_write("setxattr"));
    }

    fn removexattr(&mut self, _req: &Request<'_>, _ino: u64, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(deny_write("removexattr"));
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _offset: i64,
        _data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        reply.error(deny_write("write"));
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        reply.error(deny_write("create"));
    }

    fn fallocate(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _offset: i64,
        _length: i64,
        _mode: i32,
        reply: ReplyEmpty,
    ) {
        reply.error(deny_write("fallocate"));
    }

    fn copy_file_range(
        &mut self,
        _req: &Request<'_>,
        _ino_in: u64,
        _fh_in: u64,
        _offset_in: i64,
        _ino_out: u64,
        _fh_out: u64,
        _offset_out: i64,
        _len: u64,
        _flags: u32,
        reply: ReplyWrite,
    ) {
        reply.error(deny_write("copy_file_range"));
    }

    /// 只读文件系统上「flush / fsync 无事可做」就是成功
    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsyncdir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// `access(2)`：写请求明确拒成 EROFS（`default_permissions` 生效时内核不会问我们）
    fn access(&mut self, _req: &Request<'_>, ino: u64, mask: i32, reply: ReplyEmpty) {
        if self.record(ino).is_none() {
            reply.error(posix::ENOENT);
            return;
        }
        if (mask & posix::W_OK) != 0 {
            reply.error(deny_write("access(W_OK)"));
            return;
        }
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn entry(id: &str, name: &str, is_dir: bool, size: u64) -> Entry {
        Entry {
            id: id.to_string(),
            name: name.to_string(),
            raw_name: name.to_string(),
            is_dir,
            size,
            created: SystemTime::UNIX_EPOCH,
            modified: SystemTime::UNIX_EPOCH,
        }
    }

    fn table() -> InodeTable {
        InodeTable::with_root(entry("-11", "", true, 0))
    }

    #[test]
    fn root_is_inode_one_pointing_at_itself() {
        let t = table();
        let root = t.by_ino.get(&ROOT_INO).expect("根必须在表里");
        assert!(root.segments.is_empty());
        assert_eq!(root.parent, ROOT_INO, ".. 指向自己");
        assert_eq!(t.by_id.get("-11"), Some(&ROOT_INO));
    }

    #[test]
    fn same_cloud_id_keeps_one_ino_and_refreshes_its_path() {
        let mut t = table();
        let a = t.upsert(
            entry("f1", "a.txt", false, 4),
            vec!["a.txt".into()],
            ROOT_INO,
            true,
        );
        assert_ne!(a, ROOT_INO, "根保留 1，其它对象必须往上排");
        let b = t.upsert(
            entry("f1", "a.txt", false, 9),
            vec!["dir".into(), "a.txt".into()],
            ROOT_INO,
            true,
        );
        assert_eq!(a, b, "同一个云端 id 必须复用同一个 ino");
        let rec = t.by_ino.get(&a).expect("还在表里");
        assert_eq!(rec.segments, vec!["dir".to_string(), "a.txt".to_string()]);
        assert_eq!(rec.entry.size, 9, "属性以最新一次解析为准");
        assert_eq!(rec.lookups, 2, "两次 lookup 就是两次引用");
    }

    #[test]
    fn readdir_registration_takes_no_reference() {
        let mut t = table();
        let ino = t.upsert(entry("d1", "d", true, 0), vec!["d".into()], ROOT_INO, false);
        assert_eq!(t.by_ino.get(&ino).expect("在表里").lookups, 0);
    }

    #[test]
    fn forget_removes_the_entry_once_the_last_reference_is_gone() {
        let mut t = table();
        let ino = t.upsert(
            entry("f2", "b.txt", false, 1),
            vec!["b.txt".into()],
            ROOT_INO,
            true,
        );
        t.forget(ino, 1);
        assert!(!t.by_ino.contains_key(&ino), "引用归零必须移除");
        assert!(!t.by_id.contains_key("f2"), "反查表也要清掉");
    }

    #[test]
    fn forget_keeps_the_entry_while_references_remain() {
        let mut t = table();
        let ino = t.upsert(
            entry("f3", "c.txt", false, 1),
            vec!["c.txt".into()],
            ROOT_INO,
            true,
        );
        t.upsert(
            entry("f3", "c.txt", false, 1),
            vec!["c.txt".into()],
            ROOT_INO,
            true,
        );
        t.forget(ino, 1);
        assert_eq!(t.by_ino.get(&ino).expect("还在").lookups, 1);
        // 多减也不能 panic / 下溢
        t.forget(ino, 99);
        assert!(!t.by_ino.contains_key(&ino));
    }

    #[test]
    fn root_is_never_forgotten() {
        let mut t = table();
        t.forget(ROOT_INO, 5);
        assert!(t.by_ino.contains_key(&ROOT_INO), "根不参与引用计数淘汰");
    }

    #[test]
    fn eviction_never_drops_referenced_inodes() {
        let mut t = table();
        let hot = t.upsert(
            entry("hot", "hot", false, 1),
            vec!["hot".into()],
            ROOT_INO,
            true,
        );
        for i in 0..(MAX_INODES + 32) {
            let name = format!("c{i}");
            t.upsert(
                entry(&name, &name, false, 0),
                vec![name.clone()],
                ROOT_INO,
                false,
            );
        }
        t.evict_if_needed();
        assert!(t.by_ino.contains_key(&hot), "被内核引用的 ino 不能被淘汰");
        assert!(t.by_ino.contains_key(&ROOT_INO));
        assert!(t.by_ino.len() <= MAX_INODES, "淘汰后必须回到上限内");
    }

    #[test]
    fn eviction_is_a_no_op_below_the_cap() {
        let mut t = table();
        t.upsert(
            entry("f9", "z", false, 0),
            vec!["z".into()],
            ROOT_INO,
            false,
        );
        let before = t.by_ino.len();
        t.evict_if_needed();
        assert_eq!(t.by_ino.len(), before);
    }

    #[test]
    fn files_are_read_only_and_blocks_follow_512_bytes() {
        let file = entry("f", "f", false, 513);
        let a = attr_of(7, &file, 1000, 1000);
        assert_eq!(a.ino, 7);
        assert_eq!(a.size, 513);
        assert_eq!(a.perm, 0o444, "文件必须只读、不可执行");
        assert_eq!(a.nlink, 1);
        assert_eq!(a.kind, FileType::RegularFile);
        assert_eq!(a.blocks, 2, "513 字节向上取整到两个 512 字节块");
        assert_eq!((a.uid, a.gid), (1000, 1000));
        assert_eq!(a.rdev, 0, "云端对象不能变成设备节点");
    }

    #[test]
    fn directories_are_0555_and_zero_byte_files_have_no_blocks() {
        let dir = entry("d", "d", true, 0);
        let b = attr_of(8, &dir, 0, 0);
        assert_eq!(b.perm, 0o555);
        assert_eq!(b.nlink, 2);
        assert_eq!(b.kind, FileType::Directory);
        assert_eq!(b.blocks, 0);

        let empty = entry("z", "z", false, 0);
        let c = attr_of(9, &empty, 0, 0);
        assert_eq!(c.blocks, 0);
        assert_eq!(c.size, 0);
        assert_eq!(c.perm, 0o444);
    }
}
