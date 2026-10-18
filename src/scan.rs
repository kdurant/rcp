//! 复制计划扫描：先完整遍历源，算出条目列表与总字节数，再交给 copier 执行。
//!
//! 之所以先扫描：进度条的"总量 + ETA"必须是确定的，否则只能退化成不确定的百分比。

use crate::cli::{Config, Deref};
use crate::progress::ScanProgress;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
pub enum Kind {
    Dir,
    File { size: u64 },
    Symlink { target: PathBuf },
    Special { mode: u32, rdev: u64 },
}

#[derive(Debug)]
pub struct Item {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub kind: Kind,
    /// 源的元数据；`--parents` 合成的中间目录可能没有对应的源目录
    pub meta: Option<Metadata>,
    /// (dev, ino)，用于 `--preserve=links` 的硬链接去重
    pub link_key: Option<(u64, u64)>,
}

impl Item {
    pub fn size(&self) -> u64 {
        match self.kind {
            Kind::File { size } => size,
            _ => 0,
        }
    }

    /// 进度条上"当前条目"的说明文字。
    pub fn describe(&self) -> String {
        match &self.kind {
            Kind::Dir => "directory".to_string(),
            Kind::File { .. } => "file".to_string(),
            Kind::Symlink { target } => format!("symbolic link -> {}", target.display()),
            Kind::Special { mode, .. } => {
                let t = mode & libc::S_IFMT;
                let name = if t == libc::S_IFIFO {
                    "fifo"
                } else if t == libc::S_IFCHR {
                    "character special"
                } else if t == libc::S_IFBLK {
                    "block special"
                } else {
                    "special"
                };
                name.to_string()
            }
        }
    }
}

pub struct Plan {
    pub items: Vec<Item>,
    pub total_bytes: u64,
    pub errors: usize,
}

/// 把路径拆成"可拼到目标目录下"的若干段（`--parents` 用）。
fn rel_parts(p: &Path) -> Vec<OsString> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_os_string()),
            Component::ParentDir => Some(OsString::from("..")),
            _ => None,
        })
        .collect()
}

/// 计算单个源的最终目标路径。
pub fn dest_for(cfg: &Config, src: &Path) -> PathBuf {
    if cfg.no_target_dir {
        return cfg.target.clone();
    }
    if cfg.parents {
        let parts = rel_parts(src);
        if parts.is_empty() {
            return cfg.target.clone();
        }
        let mut p = cfg.target.clone();
        for part in parts {
            p.push(part);
        }
        return p;
    }
    if cfg.target_is_dir {
        match src.file_name() {
            Some(name) => cfg.target.join(name),
            None => match src.components().next_back() {
                Some(Component::ParentDir) => cfg.target.join(".."),
                _ => cfg.target.clone(),
            },
        }
    } else {
        cfg.target.clone()
    }
}

/// 词法绝对化 + 归一化（不解析符号链接），用于"目录复制到自身内部"的判断。
fn lexical_abs(p: &Path) -> PathBuf {
    let base = if p.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    };
    let mut out = PathBuf::new();
    for c in base.join(p).components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
            Component::Prefix(_) => {}
        }
    }
    out
}

pub struct Scanner<'a> {
    cfg: &'a Config,
    scan: &'a ScanProgress,
    items: Vec<Item>,
    visited: HashSet<(u64, u64)>,
    errors: usize,
    bytes: u64,
    root_dev: u64,
}

impl<'a> Scanner<'a> {
    pub fn new(cfg: &'a Config, scan: &'a ScanProgress) -> Self {
        Scanner {
            cfg,
            scan,
            items: Vec::new(),
            visited: HashSet::new(),
            errors: 0,
            bytes: 0,
            root_dev: 0,
        }
    }

    pub fn run(mut self) -> Plan {
        let sources = self.cfg.sources.clone();
        for src in sources {
            let dst = dest_for(self.cfg, &src);
            if let Err(msg) = self.scan_root(&src, &dst) {
                self.report(&msg);
            }
        }
        self.scan.finish();
        Plan {
            items: self.items,
            total_bytes: self.bytes,
            errors: self.errors,
        }
    }

    fn report(&mut self, msg: &str) {
        self.errors += 1;
        eprintln!("rcp: {msg}");
    }

    /// 取源元数据：按 `-L/-P/-H` 决定是否跟随符号链接。
    fn stat(&self, p: &Path, depth: usize) -> Result<Metadata, String> {
        let follow = match self.cfg.deref {
            Deref::Always => true,
            Deref::CmdLine => depth == 0,
            Deref::Never => false,
        };
        let r = if follow {
            fs::metadata(p)
        } else {
            fs::symlink_metadata(p)
        };
        r.map_err(|e| format!("cannot stat '{}': {e}", p.display()))
    }

    fn scan_root(&mut self, src: &Path, dst: &Path) -> Result<(), String> {
        let meta = self.stat(src, 0)?;
        self.root_dev = meta.dev();

        if meta.is_dir() {
            let a = lexical_abs(src);
            let b = lexical_abs(dst);
            if b != a && b.starts_with(&a) {
                return Err(format!(
                    "cannot copy a directory, '{}', into itself, '{}'",
                    src.display(),
                    dst.display()
                ));
            }
        }
        if self.cfg.parents {
            self.push_parent_dirs(src);
        }
        self.scan_entry(src, dst, 0, Some(meta))
    }

    /// `--parents`: 为目标路径缺失的中间目录补上计划项。
    fn push_parent_dirs(&mut self, src: &Path) {
        let mut parts = rel_parts(src);
        if parts.pop().is_none() {
            return;
        }
        let mut cur_src = if src.is_absolute() {
            PathBuf::from("/")
        } else {
            PathBuf::new()
        };
        let mut cur_dst = self.cfg.target.clone();
        for part in parts {
            cur_src.push(&part);
            cur_dst.push(&part);
            if cur_dst.is_dir() {
                continue;
            }
            let meta = fs::symlink_metadata(&cur_src).ok().filter(|m| m.is_dir());
            self.items.push(Item {
                src: cur_src.clone(),
                dst: cur_dst.clone(),
                kind: Kind::Dir,
                meta,
                link_key: None,
            });
            self.scan.tick(self.items.len() as u64, self.bytes);
        }
    }

    fn scan_entry(
        &mut self,
        src: &Path,
        dst: &Path,
        depth: usize,
        hint: Option<Metadata>,
    ) -> Result<(), String> {
        let meta = match hint {
            Some(m) => m,
            None => self.stat(src, depth)?,
        };

        // -x：不跨文件系统（入口源所在设备为基准）
        if depth > 0 && self.cfg.one_fs && meta.dev() != self.root_dev {
            return Ok(());
        }

        let ft = meta.file_type();
        if ft.is_symlink() {
            let target = fs::read_link(src)
                .map_err(|e| format!("cannot read symbolic link '{}': {e}", src.display()))?;
            self.push(Item {
                src: src.to_path_buf(),
                dst: dst.to_path_buf(),
                kind: Kind::Symlink { target },
                meta: Some(meta),
                link_key: None,
            });
            return Ok(());
        }

        if ft.is_dir() {
            if !self.cfg.recursive {
                return Err(format!(
                    "-r not specified; omitting directory '{}'",
                    src.display()
                ));
            }
            let key = (meta.dev(), meta.ino());
            if !self.visited.insert(key) {
                return Err(format!(
                    "cannot copy cyclic symbolic link '{}'",
                    src.display()
                ));
            }
            self.push(Item {
                src: src.to_path_buf(),
                dst: dst.to_path_buf(),
                kind: Kind::Dir,
                meta: Some(meta),
                link_key: None,
            });
            return self.scan_children(src, dst, depth);
        }

        if ft.is_file() {
            let size = meta.len();
            let link_key = if self.cfg.preserve.links && meta.nlink() > 1 {
                Some((meta.dev(), meta.ino()))
            } else {
                None
            };
            self.push(Item {
                src: src.to_path_buf(),
                dst: dst.to_path_buf(),
                kind: Kind::File { size },
                meta: Some(meta),
                link_key,
            });
            return Ok(());
        }

        if ft.is_fifo() || ft.is_char_device() || ft.is_block_device() || ft.is_socket() {
            self.push(Item {
                src: src.to_path_buf(),
                dst: dst.to_path_buf(),
                kind: Kind::Special {
                    mode: meta.mode(),
                    rdev: meta.rdev(),
                },
                meta: Some(meta),
                link_key: None,
            });
            return Ok(());
        }

        Err(format!(
            "cannot copy '{}': unsupported file type",
            src.display()
        ))
    }

    fn scan_children(&mut self, src: &Path, dst: &Path, depth: usize) -> Result<(), String> {
        let rd = match fs::read_dir(src) {
            Ok(rd) => rd,
            Err(e) => {
                self.report(&format!("cannot read directory '{}': {e}", src.display()));
                return Ok(());
            }
        };
        let mut entries: Vec<PathBuf> = Vec::new();
        for e in rd {
            match e {
                Ok(e) => entries.push(e.path()),
                Err(e) => self.report(&format!("cannot read directory '{}': {e}", src.display())),
            }
        }
        entries.sort();
        for path in entries {
            let Some(name) = path.file_name() else {
                continue;
            };
            let child_dst = dst.join(name);
            if let Err(msg) = self.scan_entry(&path, &child_dst, depth + 1, None) {
                self.report(&msg);
            }
        }
        Ok(())
    }

    fn push(&mut self, item: Item) {
        self.bytes += item.size();
        self.items.push(item);
        let n = self.items.len() as u64;
        if n <= 64 || n.is_multiple_of(64) {
            self.scan.tick(n, self.bytes);
        }
    }
}
