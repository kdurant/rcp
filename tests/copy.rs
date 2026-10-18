//! rcp 的行为回归测试。
//!
//! 刻意不依赖系统 `cp`，断言的是"GNU cp 语义"本身。

use assert_cmd::Command;
use std::fs;
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

fn rcp() -> Command {
    let mut c = Command::cargo_bin("rcp").expect("binary");
    // 测试环境里 stderr 不是终端，进度条默认关闭；统一关掉交互性差异
    c.env("LC_ALL", "C");
    c
}

fn write(p: impl AsRef<Path>, data: &[u8]) {
    let p = p.as_ref();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, data).unwrap();
}

fn read(p: impl AsRef<Path>) -> Vec<u8> {
    fs::read(p.as_ref()).unwrap()
}

fn mode(p: impl AsRef<Path>) -> u32 {
    fs::symlink_metadata(p.as_ref()).unwrap().mode() & 0o7777
}

fn set_mode(p: impl AsRef<Path>, m: u32) {
    fs::set_permissions(p.as_ref(), fs::Permissions::from_mode(m)).unwrap();
}

// ---------------------------------------------------------------- 基本

#[test]
fn copies_file_content() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"hello\n");
    rcp()
        .args(["a", "b"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("b")), b"hello\n");
}

#[test]
fn overwrites_existing_destination() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"new\n");
    write(d.path().join("b"), b"this is a longer old content\n");
    rcp()
        .args(["a", "b"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("b")), b"new\n");
    // 不保留权限时沿用目标原有权限
    set_mode(d.path().join("b"), 0o600);
    write(d.path().join("a"), b"newer\n");
    rcp()
        .args(["a", "b"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(mode(d.path().join("b")), 0o600);
}

#[test]
fn multichunk_large_file_is_intact() {
    let d = TempDir::new().unwrap();
    // 9 MiB，跨过 4 MiB 的 copy_file_range 分块边界
    let data: Vec<u8> = (0..9 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    fs::write(d.path().join("big"), &data).unwrap();
    rcp()
        .args(["big", "copy"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("copy")), data);
}

#[test]
fn copies_multiple_sources_into_directory() {
    let d = TempDir::new().unwrap();
    fs::create_dir(d.path().join("dst")).unwrap();
    write(d.path().join("a"), b"1");
    write(d.path().join("b"), b"2");
    rcp()
        .args(["a", "b", "dst"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("dst/a")), b"1");
    assert_eq!(read(d.path().join("dst/b")), b"2");
}

// ---------------------------------------------------------------- 目录

#[test]
fn recursive_creates_tree() {
    let d = TempDir::new().unwrap();
    write(d.path().join("s/sub/deep/f"), b"deep");
    write(d.path().join("s/g"), b"g");
    rcp()
        .args(["-r", "s", "d"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("d/sub/deep/f")), b"deep");
    assert_eq!(read(d.path().join("d/g")), b"g");
}

#[test]
fn recursive_into_existing_dir_merges() {
    let d = TempDir::new().unwrap();
    write(d.path().join("s/f"), b"x");
    fs::create_dir(d.path().join("d")).unwrap();
    rcp()
        .args(["-r", "s", "d"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("d/s/f")), b"x");
}

#[test]
fn directory_without_r_is_an_error() {
    let d = TempDir::new().unwrap();
    fs::create_dir(d.path().join("s")).unwrap();
    rcp()
        .args(["s", "d"])
        .current_dir(d.path())
        .assert()
        .code(1)
        .stderr(predicates::str::contains("omitting directory"));
    assert!(!d.path().join("d").exists());
}

#[test]
fn dir_mode_follows_umask_without_p() {
    let d = TempDir::new().unwrap();
    fs::create_dir(d.path().join("s")).unwrap();
    set_mode(d.path().join("s"), 0o777);
    write(d.path().join("s/f"), b"x");
    rcp()
        .args(["-r", "s", "d"])
        .current_dir(d.path())
        .assert()
        .success();
    // 022 & ~umask(022) = 755
    assert_eq!(mode(d.path().join("d")), 0o755);
}

#[test]
fn readonly_source_dir_is_still_populated() {
    let d = TempDir::new().unwrap();
    write(d.path().join("s/f"), b"x");
    set_mode(d.path().join("s"), 0o555);
    rcp()
        .args(["-r", "s", "d"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("d/f")), b"x");
    set_mode(d.path().join("d"), 0o755); // 便于 TempDir 清理
}

#[test]
fn dir_into_itself_is_refused_without_side_effects() {
    let d = TempDir::new().unwrap();
    write(d.path().join("s/f"), b"x");
    rcp()
        .args(["-r", "s", "s/sub"])
        .current_dir(d.path())
        .assert()
        .code(1)
        .stderr(predicates::str::contains("into itself"));
    assert!(!d.path().join("s/sub").exists());
}

#[test]
fn existing_file_cannot_be_replaced_by_directory() {
    let d = TempDir::new().unwrap();
    write(d.path().join("s/f"), b"x");
    write(d.path().join("dst"), b"file");
    rcp()
        .args(["-r", "s", "dst"])
        .current_dir(d.path())
        .assert()
        .code(1)
        .stderr(predicates::str::contains("cannot overwrite non-directory"));
    // 子树里的条目不应再刷屏报错
    assert_eq!(read(d.path().join("dst")), b"file");
}

// ---------------------------------------------------------------- 属性

#[test]
fn archive_preserves_mode_and_mtime() {
    let d = TempDir::new().unwrap();
    write(d.path().join("s/f"), b"x");
    set_mode(d.path().join("s"), 0o750);
    set_mode(d.path().join("s/f"), 0o640);
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let ft = fs::File::open(d.path().join("s/f")).unwrap();
    ft.set_modified(t).unwrap();
    drop(ft);

    rcp()
        .args(["-a", "s", "d"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(mode(d.path().join("d")), 0o750);
    assert_eq!(mode(d.path().join("d/f")), 0o640);
    let mt = fs::metadata(d.path().join("d/f"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(mt, t);
}

#[test]
fn without_p_mtime_is_not_preserved() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"x");
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    fs::File::open(d.path().join("a"))
        .unwrap()
        .set_modified(t)
        .unwrap();
    rcp()
        .args(["a", "b"])
        .current_dir(d.path())
        .assert()
        .success();
    let mt = fs::metadata(d.path().join("b"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(mt > t + Duration::from_secs(86400));
}

#[test]
fn file_mode_is_masked_by_umask_without_p() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"x");
    set_mode(d.path().join("a"), 0o666);
    rcp()
        .args(["a", "b"])
        .current_dir(d.path())
        .assert()
        .success();
    // 0666 & ~022 = 0644
    assert_eq!(mode(d.path().join("b")), 0o644);
}

#[test]
fn readonly_source_file_is_copied() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"data");
    set_mode(d.path().join("a"), 0o444);
    rcp()
        .args(["a", "b"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("b")), b"data");
    assert_eq!(mode(d.path().join("b")), 0o444);
}

// ---------------------------------------------------------------- 符号链接

#[test]
fn symlink_is_dereferenced_by_default() {
    let d = TempDir::new().unwrap();
    write(d.path().join("t"), b"content");
    symlink("t", d.path().join("l")).unwrap();
    rcp()
        .args(["l", "out"])
        .current_dir(d.path())
        .assert()
        .success();
    assert!(!fs::symlink_metadata(d.path().join("out"))
        .unwrap()
        .is_symlink());
    assert_eq!(read(d.path().join("out")), b"content");
}

#[test]
fn symlink_is_copied_as_link_when_recursive_or_p() {
    let d = TempDir::new().unwrap();
    write(d.path().join("t"), b"content");
    symlink("t", d.path().join("l")).unwrap();

    rcp()
        .args(["-P", "l", "out1"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(
        fs::read_link(d.path().join("out1")).unwrap(),
        PathBuf::from("t")
    );

    rcp()
        .args(["-r", "l", "out2"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(
        fs::read_link(d.path().join("out2")).unwrap(),
        PathBuf::from("t")
    );
}

#[test]
fn dangling_symlink_source_fails_without_p() {
    let d = TempDir::new().unwrap();
    symlink("nowhere", d.path().join("l")).unwrap();
    rcp()
        .args(["l", "out"])
        .current_dir(d.path())
        .assert()
        .code(1)
        .stderr(predicates::str::contains("cannot stat"));
}

#[test]
fn dangling_symlink_is_copied_as_link_with_p() {
    let d = TempDir::new().unwrap();
    symlink("nowhere", d.path().join("l")).unwrap();
    rcp()
        .args(["-P", "l", "out"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(
        fs::read_link(d.path().join("out")).unwrap(),
        PathBuf::from("nowhere")
    );
}

#[test]
fn refuses_to_write_through_dangling_dest_symlink() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"x");
    symlink("nowhere", d.path().join("dst")).unwrap();
    rcp()
        .args(["a", "dst"])
        .current_dir(d.path())
        .assert()
        .code(1)
        .stderr(predicates::str::contains("dangling symlink"));
}

// ---------------------------------------------------------------- 链接模式

#[test]
fn hard_link_mode_shares_inode() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"x");
    rcp()
        .args(["-l", "a", "b"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(
        fs::metadata(d.path().join("a")).unwrap().ino(),
        fs::metadata(d.path().join("b")).unwrap().ino()
    );
}

#[test]
fn hard_link_mode_with_existing_dest_needs_force() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"new");
    write(d.path().join("b"), b"old");
    rcp()
        .args(["-l", "a", "b"])
        .current_dir(d.path())
        .assert()
        .code(1);
    rcp()
        .args(["-lf", "a", "b"])
        .current_dir(d.path())
        .assert()
        .success();
    // 硬链接不搬内容：b 现在就是 a 的另一个名字
    assert_eq!(read(d.path().join("b")), b"new");
    assert_eq!(
        fs::metadata(d.path().join("a")).unwrap().ino(),
        fs::metadata(d.path().join("b")).unwrap().ino()
    );
}

#[test]
fn symbolic_link_mode_creates_link_to_source_path() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"x");
    rcp()
        .args(["-s", "a", "b"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(
        fs::read_link(d.path().join("b")).unwrap(),
        PathBuf::from("a")
    );
}

#[test]
fn archive_preserves_hard_links_in_tree() {
    let d = TempDir::new().unwrap();
    write(d.path().join("s/a"), b"x");
    fs::hard_link(d.path().join("s/a"), d.path().join("s/b")).unwrap();
    rcp()
        .args(["-a", "s", "d"])
        .current_dir(d.path())
        .assert()
        .success();
    let a = fs::metadata(d.path().join("d/a")).unwrap();
    let b = fs::metadata(d.path().join("d/b")).unwrap();
    assert_eq!(a.ino(), b.ino());
    assert_eq!(a.nlink(), 2);
}

#[test]
fn recursive_without_a_does_not_link() {
    let d = TempDir::new().unwrap();
    write(d.path().join("s/a"), b"x");
    fs::hard_link(d.path().join("s/a"), d.path().join("s/b")).unwrap();
    rcp()
        .args(["-r", "s", "d"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_ne!(
        fs::metadata(d.path().join("d/a")).unwrap().ino(),
        fs::metadata(d.path().join("d/b")).unwrap().ino()
    );
}

// ---------------------------------------------------------------- 覆盖策略

#[test]
fn no_clobber_keeps_destination() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"new");
    write(d.path().join("b"), b"old");
    rcp()
        .args(["-n", "a", "b"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("b")), b"old");
}

#[test]
fn update_skips_when_destination_is_newer() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"src");
    write(d.path().join("b"), b"dst");
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    fs::File::open(d.path().join("a"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    rcp()
        .args(["-u", "a", "b"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("b")), b"dst");
}

#[test]
fn interactive_decline_exits_1_and_keeps_destination() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"new");
    write(d.path().join("b"), b"old");
    rcp()
        .args(["-i", "a", "b"])
        .write_stdin("n\n")
        .current_dir(d.path())
        .assert()
        .code(1);
    assert_eq!(read(d.path().join("b")), b"old");
}

#[test]
fn interactive_accept_overwrites() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"new");
    write(d.path().join("b"), b"old");
    rcp()
        .args(["-i", "a", "b"])
        .write_stdin("y\n")
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("b")), b"new");
}

#[test]
fn n_after_i_wins() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"new");
    write(d.path().join("b"), b"old");
    rcp()
        .args(["-i", "-n", "a", "b"])
        .write_stdin("y\n")
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("b")), b"old");
}

#[test]
fn i_after_n_wins() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"new");
    write(d.path().join("b"), b"old");
    rcp()
        .args(["-n", "-i", "a", "b"])
        .write_stdin("y\n")
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("b")), b"new");
}

#[test]
fn force_replaces_readonly_destination_with_new_file_mode() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"new");
    write(d.path().join("b"), b"old");
    set_mode(d.path().join("b"), 0o400);
    rcp()
        .args(["-f", "a", "b"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("b")), b"new");
    assert_eq!(mode(d.path().join("b")), 0o644);
}

// ---------------------------------------------------------------- 目标形式

#[test]
fn parents_creates_intermediate_directories() {
    let d = TempDir::new().unwrap();
    write(d.path().join("s/sub/f"), b"x");
    fs::create_dir(d.path().join("out")).unwrap();
    rcp()
        .args(["--parents", "s/sub/f", "out"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("out/s/sub/f")), b"x");
}

#[test]
fn parents_without_directory_target_is_usage_error() {
    let d = TempDir::new().unwrap();
    write(d.path().join("s/f"), b"x");
    rcp()
        .args(["--parents", "s/f", "nodir"])
        .current_dir(d.path())
        .assert()
        .code(1)
        .stderr(predicates::str::contains("--parents"));
}

#[test]
fn target_directory_option() {
    let d = TempDir::new().unwrap();
    fs::create_dir(d.path().join("out")).unwrap();
    write(d.path().join("a"), b"1");
    rcp()
        .args(["-t", "out", "a"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("out/a")), b"1");
}

#[test]
fn no_target_directory_option() {
    let d = TempDir::new().unwrap();
    fs::create_dir(d.path().join("d")).unwrap();
    write(d.path().join("a"), b"1");
    rcp()
        .args(["-T", "a", "d/name"])
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(read(d.path().join("d/name")), b"1");
}

#[test]
fn multiple_sources_need_directory_target() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"1");
    write(d.path().join("b"), b"2");
    rcp()
        .args(["a", "b", "nodir"])
        .current_dir(d.path())
        .assert()
        .code(1)
        .stderr(predicates::str::contains("target"));
}

#[test]
fn same_file_is_refused() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"1");
    rcp()
        .args(["a", "a"])
        .current_dir(d.path())
        .assert()
        .code(1)
        .stderr(predicates::str::contains("same file"));
}

#[test]
fn missing_operand_is_usage_error() {
    let d = TempDir::new().unwrap();
    rcp().current_dir(d.path()).assert().code(1);
    rcp().arg("only-one").current_dir(d.path()).assert().code(1);
}

// ---------------------------------------------------------------- 特殊文件

#[test]
fn fifo_is_recreated_with_r() {
    let d = TempDir::new().unwrap();
    let fifo = d.path().join("p");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    rcp()
        .args(["-a", "p", "q"])
        .current_dir(d.path())
        .assert()
        .success();
    let ft = fs::symlink_metadata(d.path().join("q"))
        .unwrap()
        .file_type();
    assert!(ft.is_fifo());
}

#[test]
fn fifo_without_r_is_read_as_data() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"");
    // /dev/null 是字符设备：不带 -R 时按内容读取 → 普通空文件
    rcp()
        .args(["/dev/null", "out"])
        .current_dir(d.path())
        .assert()
        .success();
    let md = fs::symlink_metadata(d.path().join("out")).unwrap();
    assert!(md.file_type().is_file());
    assert_eq!(md.len(), 0);
}

// ---------------------------------------------------------------- 进度输出

#[test]
fn progress_never_pollutes_stdout() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), &vec![b'x'; 1024]);
    rcp()
        .args(["--progress=always", "a", "b"])
        .current_dir(d.path())
        .assert()
        .success()
        .stdout("");
}

#[test]
fn quiet_suppresses_progress_but_keeps_verbose() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), &vec![b'x'; 1024]);
    let out = rcp()
        .args(["--progress=always", "-v", "-q", "a", "b"])
        .current_dir(d.path())
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // -v 的逐条输出保留，进度/汇总被 -q 关掉
    assert!(stderr.contains("'a' -> 'b'"), "stderr={stderr}");
    assert!(!stderr.contains('%'), "stderr={stderr}");
}

#[test]
fn verbose_reports_source_and_destination() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), b"x");
    rcp()
        .args(["-v", "a", "b"])
        .current_dir(d.path())
        .assert()
        .success()
        .stderr(predicates::str::contains("'a' -> 'b'"));
}

#[test]
fn lines_mode_reports_snapshot_on_stderr() {
    let d = TempDir::new().unwrap();
    write(d.path().join("a"), &vec![b'y'; 4096]);
    rcp()
        .args(["--progress=always", "a", "b"])
        .current_dir(d.path())
        .assert()
        .success()
        .stderr(predicates::str::contains("100%"));
}

// ---------------------------------------------------------------- 跨设备

#[test]
fn cross_device_copy_falls_back_and_is_intact() {
    let d = TempDir::new().unwrap();
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() {
        return;
    }
    let dev_a = fs::metadata(d.path()).unwrap().dev();
    let dev_b = fs::metadata(shm).unwrap().dev();
    if dev_a == dev_b {
        return; // 同一个文件系统，这条路径就不测了
    }
    let data: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 253) as u8).collect();
    fs::write(d.path().join("big"), &data).unwrap();
    let tmp_out = shm.join(format!("rcp-test-{}", std::process::id()));
    let _ = fs::remove_file(&tmp_out);
    rcp()
        .arg("big")
        .arg(&tmp_out)
        .current_dir(d.path())
        .assert()
        .success();
    assert_eq!(fs::read(&tmp_out).unwrap(), data);
    fs::remove_file(&tmp_out).unwrap();
}

#[test]
fn hard_link_mode_across_devices_reports_error() {
    let d = TempDir::new().unwrap();
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() || fs::metadata(shm).unwrap().dev() == fs::metadata(d.path()).unwrap().dev() {
        return;
    }
    write(d.path().join("a"), b"x");
    let tmp_out = shm.join(format!("rcp-test-link-{}", std::process::id()));
    let _ = fs::remove_file(&tmp_out);
    rcp()
        .args(["-l", "a"])
        .arg(&tmp_out)
        .current_dir(d.path())
        .assert()
        .code(1)
        .stderr(predicates::str::contains("hard link"));
}
