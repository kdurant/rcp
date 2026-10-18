//! 复制执行引擎：单条目级别的复制、覆盖决策、元数据回写。

use crate::cli::{Config, Deref, Overwrite};
use crate::meta;
use crate::progress::Progress;
use crate::scan::{Item, Kind};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// `copy_file_range` 的分块大小：够大以摊薄系统调用，够小以更新进度。
const CFR_CHUNK: usize = 4 * 1024 * 1024;
/// read/write 回退路径的分块大小。
///
/// 128 KiB 与 GNU coreutils 的 `IO_BUFSIZE` 一致；实测更大的分块（≥256 KiB）
/// 在 tmpfs 上反而更慢（数据超出缓存友好区间），所以不取更激进的块大小。
const RW_CHUNK: usize = 64 * 1024;

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub files: u64,
    pub dirs: u64,
    pub links: u64,
    pub specials: u64,
    pub skipped: u64,
    pub declined: u64,
    pub failed: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Skipped,
    Declined,
}

/// 单条目的覆盖决策结果。
enum Decision {
    Copy(Option<Metadata>),
    Skip,
    Declined,
}

pub struct Copier<'a> {
    cfg: &'a Config,
    pb: &'a Progress,
    /// (dev, ino) -> 已复制的第一个目标路径，用于 `--preserve=links`
    hardlinks: HashMap<(u64, u64), PathBuf>,
    /// 本次由我们创建的目录（决定收尾时是否要改回真正权限）
    created_dirs: HashSet<PathBuf>,
    /// 创建失败的目录：其下的条目直接跳过，避免刷一屏连带错误
    failed_dirs: HashSet<PathBuf>,
    pub stats: Stats,
}

impl<'a> Copier<'a> {
    pub fn new(cfg: &'a Config, pb: &'a Progress) -> Self {
        Copier {
            cfg,
            pb,
            hardlinks: HashMap::new(),
            created_dirs: HashSet::new(),
            failed_dirs: HashSet::new(),
            stats: Stats::default(),
        }
    }

    /// 目标的某个祖先目录创建失败 → 本条目不做了。
    fn under_failed_dir(&self, dst: &Path) -> bool {
        if self.failed_dirs.is_empty() {
            return false;
        }
        let mut cur = dst.parent();
        while let Some(p) = cur {
            if self.failed_dirs.contains(p) {
                return true;
            }
            cur = p.parent();
        }
        false
    }

    pub fn handle(&mut self, item: &Item) {
        if self.under_failed_dir(&item.dst) {
            return;
        }
        // 当前条目的进度展示
        match &item.kind {
            Kind::File { size } => self.pb.begin_bytes(*size),
            _ => self.pb.begin_plain(item.describe()),
        }

        let outcome = match &item.kind {
            Kind::Dir => self.dir(item),
            Kind::Symlink { target } => self.symlink_item(item, target),
            Kind::File { size } => self.file(item, *size),
            Kind::Special { mode, rdev } => self.special(item, *mode, *rdev),
        };

        match outcome {
            Ok(Outcome::Done) => {
                if self.cfg.verbose {
                    self.pb.println(format!(
                        "'{}' -> '{}'",
                        item.src.display(),
                        item.dst.display()
                    ));
                }
            }
            Ok(Outcome::Skipped) => self.stats.skipped += 1,
            Ok(Outcome::Declined) => self.stats.declined += 1,
            Err(msg) => {
                self.stats.failed += 1;
                if matches!(item.kind, Kind::Dir) {
                    self.failed_dirs.insert(item.dst.clone());
                }
                self.pb.println(format!("rcp: {msg}"));
            }
        }
    }

    /// 目录收尾（自底向上调用）：写回真正权限 / 属主 / 时间戳。
    pub fn finalize_dir(&mut self, item: &Item) {
        if !matches!(item.kind, Kind::Dir) {
            return;
        }
        let Some(sm) = &item.meta else {
            return;
        };
        let created = self.created_dirs.contains(&item.dst);
        if !created && (!item.dst.is_dir() || !meta::needs_attr_writeback(&self.cfg.preserve)) {
            return;
        }
        let dst = item.dst.clone();
        let final_mode = if self.cfg.preserve.mode {
            meta::perm(sm.mode())
        } else {
            meta::default_mode(sm.mode(), self.cfg.umask)
        };
        if self.cfg.preserve.ownership {
            for w in meta::set_owner_path(&dst, sm) {
                self.pb.println(format!("rcp: {w}"));
            }
        }
        if created || self.cfg.preserve.mode {
            if let Err(e) = meta::chmod_path(&dst, final_mode) {
                self.pb.println(format!(
                    "rcp: failed to set permissions of '{}': {e}",
                    dst.display()
                ));
            }
        }
        if self.cfg.preserve.timestamps {
            if let Err(e) = meta::utimens_path_nofollow(&dst, sm) {
                self.pb.println(format!(
                    "rcp: failed to preserve times for '{}': {e}",
                    dst.display()
                ));
            }
        }
    }

    // ---------------------------------------------------------------- 决策

    /// 目标元数据：跟随符号链接与否取决于 `-P/-L`。
    fn dst_meta(&self, p: &Path) -> io::Result<Metadata> {
        if self.cfg.deref == Deref::Never {
            fs::symlink_metadata(p)
        } else {
            fs::metadata(p)
        }
    }

    fn decide(&mut self, item: &Item) -> Result<Decision, String> {
        let dst = &item.dst;
        let dm = match self.dst_meta(dst) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Decision::Copy(None)),
            Err(e) => return Err(format!("cannot stat '{}': {e}", dst.display())),
        };

        if dm.is_dir() {
            return Err(format!(
                "cannot overwrite directory '{}' with non-directory",
                dst.display()
            ));
        }
        if let Some(sm) = &item.meta {
            if sm.dev() == dm.dev() && sm.ino() == dm.ino() {
                return Err(format!(
                    "'{}' and '{}' are the same file",
                    item.src.display(),
                    dst.display()
                ));
            }
        }

        match self.cfg.overwrite {
            Overwrite::Never => return Ok(Decision::Skip),
            Overwrite::Ask => {
                let yes = self.pb.suspend(|| prompt_overwrite(dst));
                if !yes {
                    return Ok(Decision::Declined);
                }
            }
            Overwrite::Default => {}
        }

        if self.cfg.update {
            if let Some(sm) = &item.meta {
                let src_newer = (sm.mtime(), sm.mtime_nsec()) > (dm.mtime(), dm.mtime_nsec());
                if !src_newer {
                    return Ok(Decision::Skip);
                }
            }
        }

        Ok(Decision::Copy(Some(dm)))
    }

    /// 需要覆盖时先删掉旧目标（GNU 的默认行为）。
    fn replace_if_exists(&self, item: &Item, dm: &Option<Metadata>) -> Result<(), String> {
        if dm.is_some() {
            match fs::remove_file(&item.dst) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!("cannot remove '{}': {e}", item.dst.display()));
                }
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- 目录

    fn dir(&mut self, item: &Item) -> Result<Outcome, String> {
        let dst = &item.dst;
        match fs::symlink_metadata(dst) {
            Ok(m) => {
                if !m.is_dir() {
                    return Err(format!(
                        "cannot overwrite non-directory '{}' with directory '{}'",
                        dst.display(),
                        item.src.display()
                    ));
                }
                // 已存在：属性回写交给 finalize_dir
                return Ok(Outcome::Skipped);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot stat '{}': {e}", dst.display())),
        }

        let src_mode = item.meta.as_ref().map(|m| m.mode()).unwrap_or(0o755);
        let final_mode = if self.cfg.preserve.mode {
            meta::perm(src_mode)
        } else {
            meta::default_mode(src_mode, self.cfg.umask)
        };
        // 先带 u+wx 创建，收尾时再改回真正权限，否则只读目录里写不进子项
        meta::mkdir_path(dst, final_mode | 0o700)
            .map_err(|e| format!("cannot create directory '{}': {e}", dst.display()))?;
        self.created_dirs.insert(dst.clone());
        self.stats.dirs += 1;
        Ok(Outcome::Done)
    }

    // ---------------------------------------------------------------- 符号链接

    fn symlink_item(&mut self, item: &Item, target: &Path) -> Result<Outcome, String> {
        let dm = match self.decide(item)? {
            Decision::Copy(dm) => dm,
            Decision::Skip => return Ok(Outcome::Skipped),
            Decision::Declined => return Ok(Outcome::Declined),
        };
        // -s 时链接指向"源路径"本身，否则复制原链接的目标
        let link_target = if self.cfg.symbolic_link {
            item.src.clone()
        } else {
            target.to_path_buf()
        };
        self.create_symlink(item, link_target, &dm)
    }

    fn create_symlink(
        &mut self,
        item: &Item,
        target: PathBuf,
        dm: &Option<Metadata>,
    ) -> Result<Outcome, String> {
        self.pb.begin_plain("symbolic link");
        self.replace_if_exists(item, dm)?;
        std::os::unix::fs::symlink(&target, &item.dst)
            .map_err(|e| format!("cannot create symbolic link '{}': {e}", item.dst.display()))?;
        self.stats.links += 1;
        if self.cfg.preserve.ownership {
            if let Some(sm) = &item.meta {
                for w in meta::set_owner_path(&item.dst, sm) {
                    self.pb.println(format!("rcp: {w}"));
                }
            }
        }
        if self.cfg.preserve.timestamps {
            if let Some(sm) = &item.meta {
                if let Err(e) = meta::utimens_path_nofollow(&item.dst, sm) {
                    self.pb.println(format!(
                        "rcp: failed to preserve times for '{}': {e}",
                        item.dst.display()
                    ));
                }
            }
        }
        Ok(Outcome::Done)
    }

    // ---------------------------------------------------------------- 常规文件

    fn file(&mut self, item: &Item, size: u64) -> Result<Outcome, String> {
        let dm = match self.decide(item)? {
            Decision::Copy(dm) => dm,
            Decision::Skip => {
                self.pb.skip_bytes(size);
                return Ok(Outcome::Skipped);
            }
            Decision::Declined => {
                self.pb.skip_bytes(size);
                return Ok(Outcome::Declined);
            }
        };

        if self.cfg.link {
            return self.link_existing(item, item.src.clone(), Some(size));
        }
        if self.cfg.symbolic_link {
            return self.create_symlink(item, item.src.clone(), &dm);
        }

        // --preserve=links：同一 (dev,ino) 的后续文件改成硬链接
        if self.cfg.preserve.links {
            if let Some(key) = item.link_key {
                if let Some(first) = self.hardlinks.get(&key).cloned() {
                    return self.link_existing(item, first, Some(size));
                }
            }
        }

        let copied = self.copy_contents(item, &dm, size)?;
        self.stats.files += 1;
        self.stats.bytes += copied;
        if self.cfg.preserve.links {
            if let Some(key) = item.link_key {
                self.hardlinks.insert(key, item.dst.clone());
            }
        }
        Ok(Outcome::Done)
    }

    /// `-l` / `--preserve=links` 用的硬链接创建（覆盖决策已在外层做过）。
    ///
    /// 目标已存在时 GNU 的行为：无 `-f` 直接报错；有 `-f` 才先删掉再链。
    fn link_existing(
        &mut self,
        item: &Item,
        from: PathBuf,
        size_hint: Option<u64>,
    ) -> Result<Outcome, String> {
        self.pb.begin_plain("hard link");
        let mut result = fs::hard_link(&from, &item.dst);
        if self.cfg.force {
            if let Err(e) = &result {
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EEXIST) | Some(libc::EACCES) | Some(libc::EPERM)
                ) {
                    let _ = fs::remove_file(&item.dst);
                    result = fs::hard_link(&from, &item.dst);
                }
            }
        }
        result.map_err(|e| {
            format!(
                "cannot create hard link '{}' => '{}': {e}",
                item.dst.display(),
                from.display()
            )
        })?;
        self.stats.links += 1;
        // 没有实际搬运字节，把总量相应缩小
        if let Some(s) = size_hint {
            self.pb.skip_bytes(s);
        }
        Ok(Outcome::Done)
    }

    fn copy_contents(
        &mut self,
        item: &Item,
        dm: &Option<Metadata>,
        size: u64,
    ) -> Result<u64, String> {
        let dst = &item.dst;
        let src_meta = item.meta.clone();

        // 不写穿悬空符号链接（GNU 行为）
        if self.cfg.deref != Deref::Never {
            if let Ok(lm) = fs::symlink_metadata(dst) {
                if lm.file_type().is_symlink() && fs::metadata(dst).is_err() {
                    self.pb.skip_bytes(size);
                    return Err(format!(
                        "not writing through dangling symlink '{}'",
                        dst.display()
                    ));
                }
            }
        }

        let src_mode = src_meta.as_ref().map(|m| m.mode()).unwrap_or(0o644);
        let mut created = dm.is_none();
        let mut final_mode = if self.cfg.preserve.mode {
            meta::perm(src_mode)
        } else {
            match dm {
                Some(dm) => meta::perm(dm.mode()),
                None => meta::default_mode(src_mode, self.cfg.umask),
            }
        };

        let fin = File::open(&item.src)
            .map_err(|e| format!("cannot open '{}' for reading: {e}", item.src.display()))?;
        let fout = match self.open_dst(dst, meta::temporary_mode(final_mode)) {
            Ok(f) => f,
            // `-f`：打不开就先删掉再来一次，此时等于新建文件，权限按新文件算
            Err(OpenFail::Retry) => {
                fs::remove_file(dst)
                    .map_err(|e| format!("cannot remove '{}': {e}", dst.display()))?;
                created = true;
                final_mode = if self.cfg.preserve.mode {
                    meta::perm(src_mode)
                } else {
                    meta::default_mode(src_mode, self.cfg.umask)
                };
                self.open_dst(dst, meta::temporary_mode(final_mode))
                    .map_err(|e| e.into_message(dst))?
            }
            Err(other) => return Err(other.into_message(dst)),
        };

        let is_reg = src_meta.as_ref().map(|m| m.is_file()).unwrap_or(false);
        let copied = self.pump(&fin, &fout, is_reg, size, item)?;

        let fd = fout.as_raw_fd();
        if self.cfg.preserve.ownership {
            if let Some(sm) = &src_meta {
                for w in meta::set_owner_fd(fd, sm) {
                    self.pb.println(format!("rcp: {w}"));
                }
            }
        }
        if created || self.cfg.preserve.mode {
            if let Err(e) = meta::fchmod(fd, final_mode) {
                self.pb.println(format!(
                    "rcp: failed to set permissions of '{}': {e}",
                    dst.display()
                ));
            }
        }
        if self.cfg.preserve.timestamps {
            if let Some(sm) = &src_meta {
                if let Err(e) = meta::futimens(fd, sm) {
                    self.pb.println(format!(
                        "rcp: failed to preserve times for '{}': {e}",
                        dst.display()
                    ));
                }
            }
        }
        drop(fout);
        Ok(copied)
    }

    /// 打开目标文件准备写入。
    ///
    /// `Err(OpenFail::Retry)` 表示带 `-f` 时可以先删掉再重试一次。
    fn open_dst(&self, dst: &Path, mode: u32) -> Result<File, OpenFail> {
        let mut oo = OpenOptions::new();
        oo.write(true).create(true).truncate(true).mode(mode);
        oo.open(dst).map_err(|e| {
            if self.cfg.force
                && matches!(
                    e.raw_os_error(),
                    Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::ETXTBSY)
                )
            {
                OpenFail::Retry
            } else {
                OpenFail::Fatal(e)
            }
        })
    }

    /// 字节搬运：优先 `copy_file_range`，不支持时退到 read/write。
    fn pump(
        &self,
        fin: &File,
        fout: &File,
        is_regular: bool,
        expected: u64,
        item: &Item,
    ) -> Result<u64, String> {
        let mut copied: u64 = 0;
        let mut via_cfr: u64 = 0;
        let mut via_rw: u64 = 0;

        #[cfg(target_os = "linux")]
        if is_regular {
            let fd_in = fin.as_raw_fd();
            let fd_out = fout.as_raw_fd();
            loop {
                let n = unsafe {
                    libc::copy_file_range(
                        fd_in,
                        std::ptr::null_mut(),
                        fd_out,
                        std::ptr::null_mut(),
                        CFR_CHUNK,
                        0,
                    )
                };
                if n > 0 {
                    copied += n as u64;
                    via_cfr += n as u64;
                    self.pb.inc(n as u64);
                    continue;
                }
                if n == 0 {
                    // EOF：内核已经拷完（或源在过程中变小了）
                    self.pb.end_bytes();
                    debug_path(item, via_cfr, 0);
                    return Ok(copied);
                }
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    // 跨设备 / 文件系统不支持：退回 read/write
                    Some(libc::EXDEV)
                    | Some(libc::EINVAL)
                    | Some(libc::ENOSYS)
                    | Some(libc::EOPNOTSUPP)
                    | Some(libc::EPERM) => {
                        if debug_enabled() {
                            eprintln!("rcp: debug: copy_file_range unavailable: {err}");
                        }
                        break;
                    }
                    _ => {
                        self.pb.skip_bytes(expected.saturating_sub(copied));
                        return Err(format!("error copying '{}': {err}", item.src.display()));
                    }
                }
            }
        }
        let _ = is_regular;

        let mut buf = vec![0u8; RW_CHUNK];
        let mut r: &File = fin;
        let mut w: &File = fout;
        loop {
            let n = match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    self.pb.skip_bytes(expected.saturating_sub(copied));
                    return Err(format!("error reading '{}': {e}", item.src.display()));
                }
            };
            if let Err(e) = w.write_all(&buf[..n]) {
                self.pb.skip_bytes(expected.saturating_sub(copied));
                return Err(format!("error writing '{}': {e}", item.dst.display()));
            }
            copied += n as u64;
            via_rw += n as u64;
            self.pb.inc(n as u64);
        }
        self.pb.end_bytes();
        debug_path(item, via_cfr, via_rw);
        Ok(copied)
    }

    // ---------------------------------------------------------------- 特殊文件

    fn special(&mut self, item: &Item, mode: u32, rdev: u64) -> Result<Outcome, String> {
        let ft = item.meta.as_ref().map(|m| m.file_type());
        // 没有 -R/-l/-s：按内容读取（例如 /dev/null → 空普通文件）
        if !self.cfg.recursive && !self.cfg.link && !self.cfg.symbolic_link {
            let size = item.meta.as_ref().map(|m| m.len()).unwrap_or(0);
            return self.file(item, size);
        }

        let hint = item.meta.as_ref().map(|m| m.len());
        let dm = match self.decide(item)? {
            Decision::Copy(dm) => dm,
            Decision::Skip => return Ok(Outcome::Skipped),
            Decision::Declined => return Ok(Outcome::Declined),
        };
        if self.cfg.link {
            return self.link_existing(item, item.src.clone(), hint);
        }
        if self.cfg.symbolic_link {
            return self.create_symlink(item, item.src.clone(), &dm);
        }

        self.pb.begin_plain(item.describe());
        self.replace_if_exists(item, &dm)?;

        if matches!(ft, Some(t) if t.is_fifo()) {
            meta::mkfifo_path(&item.dst, meta::perm(mode))
                .map_err(|e| format!("cannot create fifo '{}': {e}", item.dst.display()))?;
        } else {
            meta::mknod_path(&item.dst, mode, rdev)
                .map_err(|e| format!("cannot create special file '{}': {e}", item.dst.display()))?;
        }
        self.stats.specials += 1;

        if self.cfg.preserve.ownership {
            if let Some(sm) = &item.meta {
                for w in meta::set_owner_path(&item.dst, sm) {
                    self.pb.println(format!("rcp: {w}"));
                }
            }
        }
        if self.cfg.preserve.timestamps {
            if let Some(sm) = &item.meta {
                if let Err(e) = meta::utimens_path_nofollow(&item.dst, sm) {
                    self.pb.println(format!(
                        "rcp: failed to preserve times for '{}': {e}",
                        item.dst.display()
                    ));
                }
            }
        }
        Ok(Outcome::Done)
    }
}

/// `RCP_DEBUG=1` 时打印每个文件走的拷贝路径，便于诊断性能问题。
fn debug_path(item: &Item, via_cfr: u64, via_rw: u64) {
    if !debug_enabled() {
        return;
    }
    let path = match (via_cfr > 0, via_rw > 0) {
        (true, true) => "copy_file_range+read/write",
        (true, false) => "copy_file_range",
        (false, true) => "read/write",
        (false, false) => "none",
    };
    eprintln!(
        "rcp: debug: '{}' via {path} (cfr={via_cfr} B, rw={via_rw} B)",
        item.src.display()
    );
}

fn debug_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RCP_DEBUG").is_some())
}

/// 打开目标失败的两种情形。
enum OpenFail {
    /// `-f`：先删掉目标再重试
    Retry,
    Fatal(io::Error),
}

impl OpenFail {
    fn into_message(self, dst: &Path) -> String {
        match self {
            OpenFail::Retry => format!("cannot create regular file '{}'", dst.display()),
            OpenFail::Fatal(e) => {
                format!("cannot create regular file '{}': {e}", dst.display())
            }
        }
    }
}

/// `-i` 的询问。与 GNU 一致：stdout 是终端时打到 stdout，否则打到 stderr，
/// 免得提示混进被重定向的数据流里。
fn prompt_overwrite(dst: &Path) -> bool {
    use std::io::IsTerminal;
    let prompt = format!("rcp: overwrite '{}'? ", dst.display());
    if io::stdout().is_terminal() {
        print!("{prompt}");
        let _ = io::stdout().flush();
    } else {
        eprint!("{prompt}");
        let _ = io::stderr().flush();
    }
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) => false,
        Ok(_) => matches!(line.trim_start().chars().next(), Some('y') | Some('Y')),
        Err(_) => false,
    }
}
