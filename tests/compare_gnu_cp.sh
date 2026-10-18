#!/bin/bash
# rcp 与系统 cp 的行为对拍脚本（开发用，不参与 cargo test）。
#
#   ./tests/compare_gnu_cp.sh            # 用 target/debug/rcp
#   RCP=target/release/rcp ./tests/compare_gnu_cp.sh
#
# 每个用例在宿主机上建两个同样的沙箱，分别跑系统 cp 和 rcp，
# 然后比较目录树快照（类型/权限/大小/符号链接目标/硬链接数，times 用例还比 mtime）、
# 退出码以及 stderr 行数。
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RCP=${RCP:-$ROOT/target/debug/rcp}
case "$RCP" in /*) ;; *) RCP="$ROOT/$RCP" ;; esac
PASS=0
FAIL=0
FAILED_NAMES=()

snapshot() { # $1=dir $2=withmtime
  local dir="$1" with="${2:-}"
  ( cd "$dir" || exit 1
    if [ "$with" = "times" ]; then
      find . -printf '%P|%y|%m|%s|%l|%n|%T@\n' | LC_ALL=C sort
    else
      find . -printf '%P|%y|%m|%s|%l|%n\n' | LC_ALL=C sort
    fi
    find . -type f -exec sha256sum {} + 2>/dev/null | LC_ALL=C sort
  )
}

# case_check <name> <setup-cmd> <cmd-with-CMD-placeholder> [stdin] [times]
case_check() {
  local name="$1" setup="$2" cmd="$3" stdin="${4:-}" withtime="${5:-}"
  local d
  d=$(mktemp -d)
  mkdir -p "$d/ref" "$d/new"
  ( cd "$d/ref" && eval "$setup" >/dev/null 2>&1 )
  ( cd "$d/new" && eval "$setup" >/dev/null 2>&1 )

  if [ "$withtime" = "times" ]; then
    ( cd "$d/ref" && find . -exec touch -h -d @1000000000 {} + >/dev/null 2>&1 )
    ( cd "$d/new" && find . -exec touch -h -d @1000000000 {} + >/dev/null 2>&1 )
  fi
  ( cd "$d/ref" && printf '%s' "$stdin" | eval "${cmd//CMD/cp}" ) >"$d/ref.out" 2>"$d/ref.err"
  local rc_ref=$?
  ( cd "$d/new" && printf '%s' "$stdin" | eval "${cmd//CMD/$RCP}" ) >"$d/new.out" 2>"$d/new.err"
  local rc_new=$?

  local problems=""
  [ "$rc_ref" != "$rc_new" ] && problems="$problems exit($rc_ref!=$rc_new)"
  local err_ref err_new
  err_ref=$(grep -c . "$d/ref.err")
  err_new=$(grep -c . "$d/new.err")
  [ "$err_ref" != "$err_new" ] && problems="$problems stderr-lines($err_ref!=$err_new)"

  snapshot "$d/ref" "$withtime" > "$d/snap.ref"
  snapshot "$d/new" "$withtime" > "$d/snap.new"
  if ! diff -q "$d/snap.ref" "$d/snap.new" >/dev/null; then
    problems="$problems tree"
  fi

  if [ -z "$problems" ]; then
    PASS=$((PASS+1))
  else
    FAIL=$((FAIL+1))
    FAILED_NAMES+=("$name")
    echo "FAIL [$name]:$problems"
    echo "  cmd: ${cmd//CMD/rcp}   (rc ref=$rc_ref new=$rc_new)"
    diff -u "$d/snap.ref" "$d/snap.new" | head -20 | sed 's/^/    /'
    echo "    --- ref stderr:"; sed 's/^/    /' "$d/ref.err" | head -5
    echo "    --- new stderr:"; sed 's/^/    /' "$d/new.err" | head -5
  fi
  chmod -R u+rwX "$d" 2>/dev/null; rm -rf "$d"
}

umask 022

# ---------- 基本文件 ----------
case_check "file-basic"      "echo hi > a"                       "CMD a b"
case_check "file-content"    "yes abcdefghij0123456789 | head -c 100000 > a"   "CMD a b"
case_check "file-empty"      ": > a"                             "CMD a b"
case_check "file-multi-src"  "mkdir d; echo 1>a; echo 2>b"       "CMD a b d"
case_check "file-into-dir"   "mkdir d; echo 1>a"                 "CMD a d"
case_check "file-init"       "echo old>b; echo new>a"            "CMD a b"
case_check "file-666"        "echo x>a; chmod 666 a"             "CMD a b"
case_check "file-666-p"      "echo x>a; chmod 666 a"             "CMD -p a b"
case_check "file-400-p"      "echo x>a; chmod 400 a"             "CMD -p a b"
case_check "file-444"        "echo x>a; chmod 444 a"             "CMD a b"
case_check "file-existing-600"  "echo old>b; chmod 600 b; echo new>a" "CMD a b"
case_check "file-existing-600-p" "echo old>b; chmod 600 b; echo new>a" "CMD -p a b"
case_check "file-umask077"   "echo x>a; chmod 666 a"             "(umask 077; CMD a b)"
case_check "file-large-9m"   "yes abcdefghij0123456789 | head -c 9437184 > a"  "CMD a b"
case_check "file-big-40m"    "yes abcdefghij0123456789 | head -c 41943040 > a" "CMD a b"

# ---------- 目录 ----------
case_check "dir-new"         "mkdir -p s/sub; echo x>s/f; echo y>s/sub/g"      "CMD -r s d"
case_check "dir-existing"    "mkdir -p s/sub d; echo x>s/f"                    "CMD -r s d"
case_check "dir-twice"       "mkdir -p s/sub d; echo x>s/f"                    "CMD -r s d && CMD -r s d"
case_check "dir-trailing"    "mkdir -p s/sub; echo x>s/f"                     "CMD -r s/ d"
case_check "dir-777"         "mkdir -p s; echo x>s/f; chmod 777 s"            "CMD -r s d"
case_check "dir-777-p"       "mkdir -p s; echo x>s/f; chmod 777 s"            "CMD -rp s d"
case_check "dir-555"         "mkdir -p s; echo x>s/f; chmod 755 s; echo y>s/g; chmod 555 s" "CMD -r s d"
case_check "dir-555-a"       "mkdir -p s; echo x>s/f; chmod 755 s; echo y>s/g; chmod 555 s" "CMD -a s d"
case_check "dir-500"         "mkdir -m 500 s"                                  "CMD -r s d"
case_check "dir-empty"       "mkdir s"                                         "CMD -r s d"
case_check "dir-umask077"    "mkdir -p s; echo x>s/f; chmod 777 s"            "(umask 077; CMD -r s d)"
case_check "dir-deep"        "mkdir -p s/a/b/c/d/e; echo x>s/a/b/c/d/e/f"      "CMD -r s d"
case_check "dir-onto-file"   "mkdir s; echo x>s/f; echo f>dst"                 "CMD -r s dst"
case_check "dir-src-slash-dst" "mkdir -p s/sub; echo x>s/f"                    "CMD -r s/ d/"
case_check "dir-no-r"        "mkdir s"                                         "CMD s d"

# ---------- 符号链接 ----------
case_check "symlink-default"  "echo x>t; ln -s t l"          "CMD l out"
case_check "symlink-P"        "echo x>t; ln -s t l"          "CMD -P l out"
case_check "symlink-P-existing" "echo x>t; ln -s t l; echo old>out" "CMD -P l out"
case_check "symlink-r-single" "echo x>t; ln -s t l"          "CMD -r l out"
case_check "symlink-in-tree"  "mkdir s; echo x>s/t; ln -s t s/l" "CMD -r s d"
case_check "symlink-in-tree-a" "mkdir s; echo x>s/t; ln -s t s/l; ln -s /etc/hostname s/abs" "CMD -a s d"
case_check "symlink-L-tree"   "mkdir s; echo x>s/t; ln -s t s/l" "CMD -rL s d"
case_check "symlink-dangling-r" "ln -s nowhere l"            "CMD -r l out"
case_check "symlink-dangling" "ln -s nowhere l"              "CMD l out"
case_check "symlink-dangling-L" "ln -s nowhere l"            "CMD -L l out"
case_check "symlink-dst-dangling" "echo x>a; ln -s nowhere dst" "CMD a dst"
case_check "symlink-dst-through" "echo x>a; echo old>t; ln -s t dst" "CMD a dst"
case_check "symlink-dst-through-a" "echo x>a; echo old>t; ln -s t dst" "CMD -a a dst"
case_check "symlink-self"     "echo x>t; ln -s t l"          "CMD l l"
case_check "same-file"        "echo x>a"                     "CMD a a"

# ---------- 归档 / 属性 ----------
case_check "archive-tree" "mkdir -p s/sub; echo x>s/f; yes abcdefghij0123456789 | head -c 4096 > s/sub/g; ln -s f s/l; chmod 750 s/sub" "CMD -a s d" times
case_check "archive-tree-p" "mkdir -p s/sub; echo x>s/f; chmod 741 s; chmod 640 s/f" "CMD -a s d" times
case_check "preserve-all-file" "echo x>a; chmod 604 a" "CMD --preserve=all a b"
case_check "preserve-times" "echo x>a; touch -d '2020-01-02 03:04:05' a" "CMD -p a b" times
case_check "preserve-links" "mkdir s; echo x>s/a; ln s/a s/b" "CMD -a s d"
case_check "no-preserve-links" "mkdir s; echo x>s/a; ln s/a s/b" "CMD -r s d"
case_check "dir-times-a" "mkdir -p s/sub; echo x>s/sub/f; touch -d '2019-05-06 07:08:09' s/sub s" "CMD -a s d" times
case_check "dir-times-r" "mkdir -p s/sub; echo x>s/sub/f; touch -d '2019-05-06 07:08:09' s/sub s" "CMD -r s d"
case_check "preserve-links-deep" "mkdir -p s/x; echo x>s/x/a; ln s/x/a s/x/b; ln s/x/b s/c" "CMD -a s d"

# ---------- 覆盖策略 ----------
case_check "opt-n"        "echo old>b; echo new>a"   "CMD -n a b"
case_check "opt-n-missing" "echo new>a"              "CMD -n a b"
case_check "opt-i-yes"    "echo old>b; echo new>a"   "CMD -i a b" "y
"
case_check "opt-i-no"     "echo old>b; echo new>a"   "CMD -i a b" "n
"
case_check "opt-i-n-order" "echo old>b; echo new>a"  "CMD -i -n a b" "y
"
case_check "opt-n-i-order" "echo old>b; echo new>a"  "CMD -n -i a b" "y
"
case_check "opt-u-older"  "echo old>b; echo new>a; touch -d '2000-01-01' a" "CMD -u a b"
case_check "opt-u-newer"  "echo old>b; echo new>a"   "CMD -u a b"
case_check "opt-f"        "echo old>b; echo new>a"   "CMD -f a b"
case_check "opt-f-readonly-dst" "echo new>a; echo old>b; chmod 400 b" "CMD -f a b"

# ---------- 硬链接 / 符号链接模式 ----------
case_check "opt-l"        "echo x>a"                 "CMD -l a b"
case_check "opt-l-dir"    "mkdir s; echo x>s/f"      "CMD -lr s d"
case_check "opt-l-existing" "echo old>b; echo x>a"   "CMD -l a b"
case_check "opt-s-rel"    "echo x>a"                 "CMD -s a b"

# ---------- 目标形式 ----------
case_check "opt-t"        "mkdir d; echo 1>a; echo 2>b"  "CMD -t d a b"
case_check "opt-t-long"   "mkdir d; echo 1>a"            "CMD --target-directory=d a"
case_check "opt-T"        "echo 1>a"                     "CMD -T a b"
case_check "opt-T-multi"  "echo 1>a; echo 2>b"           "CMD -T a b"
case_check "target-not-dir" "echo 1>a; echo 2>b"          "CMD a b nodir"
case_check "missing-src"  "echo 1>a"                     "CMD nosuch b"
case_check "partial-fail" "mkdir d; echo 1>a"            "CMD nosuch a d"
case_check "missing-dst-operand" "echo 1>a"              "CMD a"
case_check "no-operand"   ":"                            "CMD"

# ---------- --parents ----------
case_check "parents-rel"  "mkdir -p s/sub; echo x>s/sub/f"  "CMD -r --parents s/sub/f d"
case_check "parents-dot"  "mkdir -p s; echo x>s/f"          "CMD --parents ./s/f d"
case_check "parents-tree" "mkdir -p s/sub; echo x>s/sub/f"  "CMD -r --parents s d"

# ---------- 特殊文件 ----------
case_check "fifo-a"   "mkfifo p"                     "CMD -a p q"
case_check "fifo-r"   "mkfifo p"                     "CMD -r p q"
case_check "devnull"  ":"                            "CMD /dev/null out"
case_check "socket-a" "python3 -c \"import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind('sk')\"" "CMD -a sk out"
case_check "opt-x"    "mkdir -p s/sub; echo x>s/sub/f"  "CMD -rx s d"

echo
echo "==== PASS=$PASS FAIL=$FAIL ===="
if [ ${#FAILED_NAMES[@]} -gt 0 ]; then
  printf 'failed: %s\n' "${FAILED_NAMES[*]}"
fi
