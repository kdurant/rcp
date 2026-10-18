//! 命令行解析与配置解析。
//!
//! 有意对齐 GNU coreutils `cp` 的常用语义：
//!   * `-i` / `-n` 谁在后面谁生效（用 clap 记录的参数下标判断）
//!   * `-L` / `-P` / `-H` / `-d` 同理；默认值按 `recursive || preserve.links ? Never : Always`
//!   * `-a` = `-dR --preserve=all`，`-p` = `--preserve=mode,ownership,timestamps`
//!   * 末位参数为目标；多源时目标必须是已存在目录

use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};
use std::ffi::OsString;
use std::path::PathBuf;

/// 进度条的显示时机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProgressWhen {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Parser)]
#[command(
    name = "rcp",
    version,
    about = "cp with progress bar and transfer rate",
    after_help = "ATTR_LIST for --preserve: mode, ownership, timestamps, links, all\n\
                  --progress: auto (only when stderr is a terminal), always, never"
)]
pub struct Cli {
    /// Source files and the destination (the last argument)
    #[arg(value_name = "SOURCE", num_args = 0..)]
    pub paths: Vec<PathBuf>,

    /// Copy directories recursively
    #[arg(short = 'r', long = "recursive", visible_short_alias = 'R')]
    pub recursive: bool,

    /// Same as -dR --preserve=all
    #[arg(short = 'a', long = "archive")]
    pub archive: bool,

    /// Same as --no-dereference --preserve=links
    #[arg(short = 'd')]
    pub deref_preserve_links: bool,

    /// Never follow symbolic links in SOURCE
    #[arg(short = 'P', long = "no-dereference")]
    pub no_dereference: bool,

    /// Always follow symbolic links in SOURCE
    #[arg(short = 'L', long = "dereference")]
    pub dereference: bool,

    /// Follow command-line symbolic links in SOURCE
    #[arg(short = 'H')]
    pub dereference_cmdline: bool,

    /// Remove an existing destination file if it cannot be opened for writing
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Prompt before overwrite (overrides an earlier -f, but -n overrides -i if later)
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Do not overwrite an existing file
    #[arg(short = 'n', long = "no-clobber")]
    pub no_clobber: bool,

    /// Copy only when SOURCE is newer than the destination
    #[arg(short = 'u', long = "update")]
    pub update: bool,

    /// Same as --preserve=mode,ownership,timestamps
    #[arg(short = 'p')]
    pub preserve_p: bool,

    /// Preserve the specified attributes (default: mode,ownership,timestamps)
    #[arg(
        long = "preserve",
        value_name = "ATTR_LIST",
        num_args = 0..=1,
        default_missing_value = "mode,ownership,timestamps"
    )]
    pub preserve: Option<String>,

    /// Explain what is being done
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    /// Copy all SOURCE arguments into DIRECTORY
    #[arg(short = 't', long = "target-directory", value_name = "DIRECTORY")]
    pub target_directory: Option<PathBuf>,

    /// Treat DEST as a normal file
    #[arg(short = 'T', long = "no-target-directory")]
    pub no_target_directory: bool,

    /// Hard-link files instead of copying
    #[arg(short = 'l', long = "link")]
    pub link: bool,

    /// Make symbolic links instead of copying
    #[arg(short = 's', long = "symbolic-link")]
    pub symbolic_link: bool,

    /// Stay on this file system
    #[arg(short = 'x', long = "one-file-system")]
    pub one_file_system: bool,

    /// Use the full source file name under DIRECTORY
    #[arg(long = "parents")]
    pub parents: bool,

    /// When to render the progress bar
    #[arg(long = "progress", value_enum, value_name = "WHEN", default_value_t = ProgressWhen::Auto)]
    pub progress: ProgressWhen,

    /// Suppress the progress bar and the final summary
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
}

/// 保留属性的集合。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Preserve {
    pub mode: bool,
    pub ownership: bool,
    pub timestamps: bool,
    pub links: bool,
}

impl Preserve {
    pub const ALL: Preserve = Preserve {
        mode: true,
        ownership: true,
        timestamps: true,
        links: true,
    };
}

/// 源符号链接的处理方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deref {
    /// 不跟随（`-P`）
    Never,
    /// 始终跟随（`-L`，非递归时的默认）
    Always,
    /// 只跟随命令行上给出的源（`-H`）
    CmdLine,
}

/// 覆盖已存在目标的策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overwrite {
    /// 直接覆盖
    Default,
    /// `-i`，覆盖前询问
    Ask,
    /// `-n`，静默跳过
    Never,
}

#[derive(Debug)]
pub struct Config {
    pub sources: Vec<PathBuf>,
    pub target: PathBuf,
    /// 目标解析时目标是否已存在且为目录
    pub target_is_dir: bool,
    pub no_target_dir: bool,
    pub parents: bool,
    pub recursive: bool,
    pub deref: Deref,
    pub preserve: Preserve,
    pub overwrite: Overwrite,
    pub update: bool,
    pub force: bool,
    pub verbose: bool,
    pub quiet: bool,
    pub link: bool,
    pub symbolic_link: bool,
    pub one_fs: bool,
    pub progress: ProgressWhen,
    pub umask: u32,
}

/// 从原始 argv 中扫出"互斥开关"的出现顺序（值是 argv 下标）。
///
/// 不用 clap 的 `indices_of`：对 `SetTrue` 开关，即使没出现在命令行上它也会返回下标，
/// 因此无法区分 `-i -n` 与 `-n -i`（GNU 是"后者胜出"）。
fn flag_order(args: &[OsString]) -> Vec<(&'static str, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut positional_only = false;
    while i < args.len() {
        let raw = args[i].to_string_lossy().into_owned();
        i += 1;
        if positional_only {
            continue;
        }
        if raw == "--" {
            positional_only = true;
            continue;
        }
        if let Some(long) = raw.strip_prefix("--") {
            let (name, has_value) = match long.split_once('=') {
                Some((n, _)) => (n, true),
                None => (long, false),
            };
            match name {
                "interactive" => out.push(("interactive", i)),
                "no-clobber" => out.push(("no_clobber", i)),
                "dereference" => out.push(("dereference", i)),
                "no-dereference" => out.push(("no_dereference", i)),
                // 这些长选项会吃掉一个参数
                "target-directory" | "preserve" | "progress" if !has_value => i += 1,
                _ => {}
            }
            continue;
        }
        if let Some(cluster) = raw.strip_prefix('-') {
            for c in cluster.chars() {
                match c {
                    'i' => out.push(("interactive", i)),
                    'n' => out.push(("no_clobber", i)),
                    'L' => out.push(("dereference", i)),
                    'P' => out.push(("no_dereference", i)),
                    'H' => out.push(("dereference_cmdline", i)),
                    'd' => out.push(("deref_preserve_links", i)),
                    // -t 的值是下一个 argv，簇里后面的字符不再当开关
                    't' => {
                        i += 1;
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

fn last_pos(order: &[(&'static str, usize)], id: &str) -> Option<usize> {
    order
        .iter()
        .filter(|(k, _)| *k == id)
        .map(|(_, i)| *i)
        .max()
}

fn parse_preserve(list: &str) -> Result<Preserve, String> {
    let mut p = Preserve::default();
    if list.is_empty() {
        return Ok(p);
    }
    for attr in list.split(',') {
        match attr {
            "mode" => p.mode = true,
            "ownership" => p.ownership = true,
            "timestamps" => p.timestamps = true,
            "links" => p.links = true,
            "all" => p = Preserve::ALL,
            // GNU 支持但本实现不处理：显式要求时给出提示，xattr 保留不可用不影响复制本身
            "xattr" | "context" => {
                eprintln!("rcp: warning: --preserve={attr} is not supported; ignoring")
            }
            other => {
                return Err(format!(
                    "invalid argument '{other}' for '--preserve'\n\
                     valid arguments are: mode, ownership, timestamps, links, all"
                ))
            }
        }
    }
    Ok(p)
}

/// 解析阶段的错误。`usage` 决定是否额外打印 "Try --help"（对齐 GNU：只有用法类错误才给提示）。
#[derive(Debug)]
pub struct ResolveError {
    pub msg: String,
    pub usage: bool,
}

fn usage_err(msg: impl Into<String>) -> ResolveError {
    ResolveError {
        msg: msg.into(),
        usage: true,
    }
}

fn runtime_err(msg: impl Into<String>) -> ResolveError {
    ResolveError {
        msg: msg.into(),
        usage: false,
    }
}

impl Config {
    fn resolve(cli: Cli, order: &[(&'static str, usize)]) -> Result<Config, ResolveError> {
        // ---- 属性保留 ----
        let mut preserve = Preserve::default();
        let mut recursive = cli.recursive;
        if cli.archive {
            recursive = true;
            preserve = Preserve::ALL;
        }
        if cli.deref_preserve_links {
            preserve.links = true;
        }
        if cli.preserve_p {
            preserve.mode = true;
            preserve.ownership = true;
            preserve.timestamps = true;
        }
        if let Some(list) = &cli.preserve {
            let extra = parse_preserve(list).map_err(usage_err)?;
            preserve.mode |= extra.mode;
            preserve.ownership |= extra.ownership;
            preserve.timestamps |= extra.timestamps;
            preserve.links |= extra.links;
        }

        // ---- 符号链接策略：显式开关后者胜出，否则按 GNU 的默认推导 ----
        let explicit = [
            ("no_dereference", Deref::Never),
            ("dereference", Deref::Always),
            ("dereference_cmdline", Deref::CmdLine),
            ("deref_preserve_links", Deref::Never),
        ]
        .iter()
        .filter_map(|(id, d)| last_pos(order, id).map(|i| (i, *d)))
        .max_by_key(|(i, _)| *i)
        .map(|(_, d)| d);
        let deref = explicit.unwrap_or(if recursive || preserve.links {
            Deref::Never
        } else {
            Deref::Always
        });

        // ---- 覆盖策略：-i / -n 后者胜出（-f 不参与） ----
        let overwrite = match (
            last_pos(order, "interactive"),
            last_pos(order, "no_clobber"),
        ) {
            (Some(i), Some(n)) if n > i => Overwrite::Never,
            (Some(_), Some(_)) => Overwrite::Ask,
            (Some(_), None) => Overwrite::Ask,
            (None, Some(_)) => Overwrite::Never,
            (None, None) => Overwrite::Default,
        };

        // ---- 源与目标 ----
        let mut paths = cli.paths.clone();
        let (sources, target) = if let Some(dir) = cli.target_directory.clone() {
            (paths, dir)
        } else {
            match paths.pop() {
                Some(target) => (paths, target),
                None => return Err(usage_err("missing file operand")),
            }
        };
        if sources.is_empty() {
            return Err(usage_err("missing file operand"));
        }
        if target.as_os_str().is_empty() {
            // `cp src `` / `cp src ""`
            return Err(usage_err(format!(
                "missing destination file operand after '{}'",
                sources[0].display()
            )));
        }
        if cli.no_target_directory && sources.len() > 1 {
            return Err(usage_err(format!(
                "extra operand '{}'",
                sources[1].display()
            )));
        }
        let target_is_dir = target.is_dir();
        if cli.target_directory.is_some() && !target_is_dir {
            return Err(runtime_err(format!(
                "target directory '{}': No such file or directory",
                target.display()
            )));
        }
        if cli.parents && !target_is_dir {
            return Err(usage_err(
                "with --parents, the destination must be a directory",
            ));
        }
        if sources.len() > 1 && !target_is_dir {
            return Err(runtime_err(format!(
                "target '{}': No such file or directory",
                target.display()
            )));
        }

        Ok(Config {
            sources,
            target,
            target_is_dir,
            no_target_dir: cli.no_target_directory,
            parents: cli.parents,
            recursive,
            deref,
            preserve,
            overwrite,
            update: cli.update,
            force: cli.force,
            verbose: cli.verbose,
            quiet: cli.quiet,
            link: cli.link,
            symbolic_link: cli.symbolic_link,
            one_fs: cli.one_file_system,
            progress: cli.progress,
            umask: crate::meta::read_umask(),
        })
    }
}

/// 解析命令行；clap 的 `--help` / `--version` / 参数错误会在这里直接退出。
pub fn config_from_env() -> Config {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    let order = flag_order(&argv);
    let matches = match Cli::command().try_get_matches() {
        Ok(m) => m,
        Err(e) => {
            let _ = e.print();
            // 与 GNU cp 一致：用法错误返回 1，--help/--version 返回 0
            std::process::exit(if e.use_stderr() { 1 } else { 0 });
        }
    };
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    match Config::resolve(cli, &order) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("rcp: {}", e.msg);
            if e.usage {
                eprintln!("Try 'rcp --help' for more information.");
            }
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_us(args: &[&str]) -> Config {
        let mut argv = vec!["rcp".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let os: Vec<OsString> = argv[1..].iter().map(OsString::from).collect();
        let order = flag_order(&os);
        let matches = Cli::command().get_matches_from(&argv);
        let cli = Cli::from_arg_matches(&matches).expect("parse");
        Config::resolve(cli, &order).expect("resolve")
    }

    #[test]
    fn flag_order_scan() {
        let scan = |v: &[&str]| -> Vec<&'static str> {
            let os: Vec<OsString> = v.iter().map(OsString::from).collect();
            flag_order(&os).into_iter().map(|(k, _)| k).collect()
        };
        assert_eq!(scan(&["a", "b"]), Vec::<&str>::new());
        assert_eq!(scan(&["-in", "a", "b"]), vec!["interactive", "no_clobber"]);
        assert_eq!(scan(&["-ni", "a", "b"]), vec!["no_clobber", "interactive"]);
        assert_eq!(
            scan(&["--no-clobber", "-i", "a"]),
            vec!["no_clobber", "interactive"]
        );
        assert_eq!(scan(&["-t", "-i", "a"]), Vec::<&str>::new());
        assert_eq!(scan(&["--target-directory", "-n", "a"]), Vec::<&str>::new());
        assert_eq!(scan(&["--target-directory=-i", "a"]), Vec::<&str>::new());
        assert_eq!(scan(&["--", "-i"]), Vec::<&str>::new());
        assert_eq!(scan(&["-riL", "a"]), vec!["interactive", "dereference"]);
    }

    #[test]
    fn overwrite_last_wins() {
        assert_eq!(resolve_us(&["a", "b"]).overwrite, Overwrite::Default);
        assert_eq!(resolve_us(&["-i", "a", "b"]).overwrite, Overwrite::Ask);
        assert_eq!(resolve_us(&["-n", "a", "b"]).overwrite, Overwrite::Never);
        assert_eq!(
            resolve_us(&["-i", "-n", "a", "b"]).overwrite,
            Overwrite::Never
        );
        assert_eq!(
            resolve_us(&["-n", "-i", "a", "b"]).overwrite,
            Overwrite::Ask
        );
        assert_eq!(
            resolve_us(&["-f", "-i", "a", "b"]).overwrite,
            Overwrite::Ask
        );
        assert_eq!(
            resolve_us(&["-i", "-f", "a", "b"]).overwrite,
            Overwrite::Ask
        );
    }

    #[test]
    fn deref_defaults() {
        assert_eq!(resolve_us(&["a", "b"]).deref, Deref::Always);
        assert_eq!(resolve_us(&["-r", "a", "b"]).deref, Deref::Never);
        assert_eq!(resolve_us(&["-P", "a", "b"]).deref, Deref::Never);
        assert_eq!(resolve_us(&["-d", "a", "b"]).deref, Deref::Never);
        assert_eq!(resolve_us(&["-a", "a", "b"]).deref, Deref::Never);
        assert_eq!(resolve_us(&["-a", "-L", "a", "b"]).deref, Deref::Always);
        assert_eq!(resolve_us(&["-H", "-r", "a", "b"]).deref, Deref::CmdLine);
        assert_eq!(resolve_us(&["-L", "-P", "a", "b"]).deref, Deref::Never);
        assert_eq!(resolve_us(&["-P", "-L", "a", "b"]).deref, Deref::Always);
    }

    #[test]
    fn preserve_sets() {
        let p = resolve_us(&["-a", "a", "b"]).preserve;
        assert!(p.mode && p.ownership && p.timestamps && p.links);
        let p = resolve_us(&["-p", "a", "b"]).preserve;
        assert!(p.mode && p.ownership && p.timestamps && !p.links);
        let p = resolve_us(&["-d", "a", "b"]).preserve;
        assert!(p.links && !p.mode);
        let p = resolve_us(&["--preserve=mode", "a", "b"]).preserve;
        assert!(p.mode && !p.ownership);
    }

    #[test]
    fn operands() {
        let dir = std::env::temp_dir();
        let dir_s = dir.to_str().unwrap();
        let c = resolve_us(&["-t", dir_s, "a", "b"]);
        assert_eq!(c.target, dir);
        assert_eq!(c.sources, vec![PathBuf::from("a"), PathBuf::from("b")]);
        let c = resolve_us(&["a", "b", dir_s]);
        assert_eq!(c.target, dir);
        assert_eq!(c.sources.len(), 2);
    }
}
