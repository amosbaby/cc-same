#!/bin/bash
# 一键安装并锁定配置 cc-same 命令行工具（macOS；Linux 也可用）。
#
#   ./scripts/secure-setup.sh                                    # 从列表里选组织
#   ./scripts/secure-setup.sh --org ACCT/ORG --org ACCT/ORG      # 直接指定（完整 ID 或唯一前缀）
#   ./scripts/secure-setup.sh --org ACCT/ORG=名字 --org ACCT/ORG=名字 --yes   # 全程不提问
#   ./scripts/secure-setup.sh                                    # 已装过：默认保持设置，只更新程序
#
# 按顺序做这些事：
#   1. 用锁定的依赖版本从本仓库编译安装 `cc-same`（不含 GUI、自动更新和账号切换）；
#   2. 列出本机所有账号/组织，带名字、会话数和最近的会话标题；
#   3. 只让你选的组织参与同步（`config --only`），其他账号和组织（包括以后新登录的）一律不碰；
#   4. 等 Claude 退出，展示同步计划，确认后首次同步（之前会先做一份永久保留的基线快照），
#      然后启动后台同步。
#
# 随时可以撤销：`cc-same rollback` 回到安装前；`cc-same uninstall` 只停掉同步（每个组织都保留自己的完整会话副本）。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ORGS=()
YES=0
while [ $# -gt 0 ]; do
    case "$1" in
        --org) ORGS+=("$2"); shift 2 ;;
        --yes|-y) YES=1; shift ;;
        -h|--help) sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "未知参数：$1" >&2; exit 2 ;;
    esac
done

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
die() { printf '\033[31m错误：\033[0m%s\n' "$*" >&2; exit 1; }
ask() {
    [ "$YES" = 1 ] && return 0
    local reply
    read -r -p "$1 [y/N] " reply </dev/tty
    case "$reply" in y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
}
# 默认"是"的提问
ask_yes() {
    [ "$YES" = 1 ] && return 0
    local reply
    read -r -p "$1 [Y/n] " reply </dev/tty
    case "$reply" in n|N|no|NO) return 1 ;; *) return 0 ;; esac
}

# ---------------------------------------------------------------- 1. 编译安装
command -v cargo >/dev/null 2>&1 || die "没有安装 Rust。请先安装（https://rustup.rs，或 brew install rustup && rustup-init），再重新运行。"
say "编译 cc-same（$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo '当前目录')，锁定依赖版本）"
if [ -n "$(git -C "$ROOT" status --porcelain 2>/dev/null)" ]; then
    echo "提示：这个目录有本地改动，会按改动后的代码编译。"
fi
# 在仓库根目录里编译，rust-toolchain.toml 才会生效，每次都用同一个编译器版本。
(cd "$ROOT" && cargo install --locked --force --root "${CARGO_HOME:-$HOME/.cargo}" --path crates/cli --bin cc-same)
# 直接用刚编译出来的这个，不按 PATH 找：PATH 里可能先排着别的 cc-same（比如上游版）。
BIN="${CARGO_HOME:-$HOME/.cargo}/bin/cc-same"
[ -x "$BIN" ] || die "编译完成，但没有在 $BIN 找到 cc-same"
"$BIN" --version
ON_PATH="$(command -v cc-same || true)"
if [ -z "$ON_PATH" ]; then
    echo "提示：${BIN%/*} 不在 PATH 里。以后手动运行时请用完整路径 $BIN，或把它加入 PATH。"
elif [ "$ON_PATH" != "$BIN" ]; then
    echo "注意：PATH 里的 cc-same 是 $ON_PATH，不是刚编译的这个。以后手动运行时请用 $BIN，或调整 PATH。"
fi
if "$BIN" --help | grep -qE '^  (switch|add) '; then
    die "这个 cc-same 编译进了账号切换功能；请去掉 --features account-switching 重新编译"
fi

# ---------------------------------------------------------------- 2. 选择组织
IDS=() NAMES=() RECENTS=()
show_orgs() {
    IDS=() NAMES=() RECENTS=()
    local n=0 id name email sessions syncs open recent
    while IFS=$'\t' read -r id name email sessions syncs open recent; do
        case "$id" in */*) ;; *) continue ;; esac
        n=$((n + 1))
        IDS+=("$id")
        NAMES+=("$name")
        RECENTS+=("$recent")
        if [ "$name" = "-" ]; then name="（未命名）"; fi
        if [ "$sessions" = "-" ]; then sessions="还没有文件夹"; else sessions="$sessions 个会话"; fi
        if [ "$syncs" = 1 ]; then syncs="同步"; else syncs="不同步"; fi
        if [ "$open" = 1 ]; then open="  ← Claude 当前打开"; else open=""; fi
        printf '%2d. \033[1m%s\033[0m  %s  %s  [%s]%s\n' "$n" "$name" "$email" "$sessions" "$syncs" "$open"
        printf '    ID：%s\n' "$id"
        if [ "$recent" != "-" ]; then printf '    最近会话：%s\n' "$recent"; fi
    done <<EOF
$TSV
EOF
}
# 先单独取列表：命令失败时如实报错，而不是当成"没有组织"。
list_orgs() {
    TSV="$("$BIN" orgs --tsv)" || die "列出账号/组织失败（错误见上方）。可以运行 cc-same doctor 查看原因。"
}
count_orgs() {
    local n=0 id
    while IFS=$'\t' read -r id _; do
        case "$id" in */*) n=$((n + 1)) ;; esac
    done <<EOF
$TSV
EOF
    echo "$n"
}

# ---------------------------------------------------------------- 已装过：默认只更新程序
CONFIG="$("$BIN" config)" || die "读取现有配置失败（错误见上方）"
if [ "${#ORGS[@]}" -eq 0 ] && printf '%s' "$CONFIG" | tr -d '\n' | grep -q '"only": \[ *"'; then
    list_orgs
    say "已经装过，现在只同步这些组织："
    show_orgs
    if ask_yes "保持现有设置，只更新程序和后台同步吗？（选 n 重新选择组织）"; then
        "$BIN" install --yes || die "更新后台同步失败（错误见上方）。如果同步处于暂停状态，先运行 cc-same resume。"
        say "已更新"
        "$BIN" status
        exit 0
    fi
fi

say "本机的账号/组织"
list_orgs
show_orgs
if [ "$(count_orgs)" -lt 2 ]; then
    say "只找到 $(count_orgs) 个组织。请在 Claude 里切换到另一个组织（如果还是没出现，就在那个组织里随便开一个会话）；正在等待（按 Ctrl+C 中止）"
    while [ "$(count_orgs)" -lt 2 ]; do
        sleep 3
        list_orgs
    done
    show_orgs
fi

if [ "${#ORGS[@]}" -eq 0 ]; then
    [ "$YES" = 0 ] || die "使用 --yes 时，需要用 --org 指定每个要同步的组织"
    echo
    echo "要让哪几个组织共用同一份会话列表？输入编号，用空格分隔（至少两个）。"
    read -r -p "> " picks </dev/tty
    for n in $picks; do
        case "$n" in ''|*[!0-9]*) die "不是编号：$n" ;; esac
        [ "$n" -ge 1 ] && [ "$n" -le "${#IDS[@]}" ] || die "没有第 $n 项"
        ORGS+=("${IDS[$((n - 1))]}")
    done
fi
[ "${#ORGS[@]}" -ge 2 ] || die "至少要选两个组织"

# --org 可以写成 ID=名字
PICKED=() NAMEARGS=()
for o in "${ORGS[@]}"; do
    id="${o%%=*}"
    # 只写账号 ID 会把这个账号下所有组织（包括以后新加的）都带进来。
    case "$id" in */*) ;; *) die "--org 需要写成 <账号>/<组织>（各自写唯一前缀即可）：$id" ;; esac
    PICKED+=("$id")
    if [ "$o" != "$id" ]; then NAMEARGS+=(--name "$o"); fi
done

# ---------------------------------------------------------------- 3. 锁定配置
if [ "$YES" = 0 ]; then
    say "给选中的组织起个名字（比如 Aftership-2），之后的列表和同步计划都会显示它；直接回车就保持不变"
    for id in "${PICKED[@]}"; do
        current="-" recent="-" label="$id"
        i=0
        while [ $i -lt "${#IDS[@]}" ]; do
            if [ "${IDS[$i]}" = "$id" ]; then
                current="${NAMES[$i]}" recent="${RECENTS[$i]}" label="第 $((i + 1)) 项"
            fi
            i=$((i + 1))
        done
        hint=""
        if [ "$current" != "-" ]; then hint="，现在叫 $current"; fi
        if [ "$recent" != "-" ]; then hint="$hint，最近会话：${recent%% | *}"; fi
        read -r -p "  ${label}${hint:+（${hint#，}）}：" label </dev/tty
        if [ -n "$label" ]; then NAMEARGS+=(--name "$id=$label"); fi
    done
fi

say "只同步选中的组织，其他账号和组织一律不碰"
ARGS=(config --clear-only --auto-join off --notify on --language zh-CN)
for o in "${PICKED[@]}"; do ARGS+=(--only "$o"); done
if [ "${#NAMEARGS[@]}" -gt 0 ]; then ARGS+=("${NAMEARGS[@]}"); fi
"$BIN" "${ARGS[@]}" >/dev/null
show_orgs

# ---------------------------------------------------------------- 4. 首次同步并启动后台同步
claude_running() {
    if [ "$(uname)" = Darwin ]; then pgrep -xq Claude; else pgrep -x claude-desktop >/dev/null 2>&1 || pgrep -x Claude >/dev/null 2>&1; fi
}
if claude_running; then
    say "请等 Claude 里正在运行的任务结束后，按 Cmd+Q 退出 Claude；正在等它关闭（按 Ctrl+C 中止）"
    while claude_running; do sleep 2; done
fi

say "首次同步会做的改动"
"$BIN" plan
ask "现在同步，并在后台持续保持这些组织一致吗？" || { echo "已停止，没有同步任何内容；上面的设置已保存。"; exit 0; }
"$BIN" sync --yes || die "首次同步没有完成（错误见上方）。设置已保存，后台同步还没开启。
  查看原因：cc-same doctor
  撤销已做的部分：cc-same undo --dry-run，然后 cc-same undo
  修好后重新运行本脚本即可。"
"$BIN" install --yes || die "首次同步已完成，但后台同步没有开启（错误见上方）。修好后运行 cc-same install 即可。"
say "完成"
"$BIN" status
cat <<EOF

重新打开 Claude，照常在 Claude 里切换组织即可，后台会保持两边的会话列表一致。
  查看状态：cc-same doctor        cc-same orgs
  出问题时：cc-same rollback --dry-run，然后 cc-same rollback   （回到安装前，并停止同步）
  撤销：    cc-same undo --dry-run   cc-same undo   （只撤销最近一次同步）
  快照：    cc-same snapshots     cc-same restore 1   （先退出 Claude；1 是最新的快照）
  暂停：    cc-same pause         cc-same resume
  改名字：  cc-same config --name <账号>/<组织>=<名字>
  停止同步：cc-same uninstall     （每个组织都保留完整副本；加 --purge 连同 cc-same 的数据一起删除）
  更新：    git pull && ./scripts/secure-setup.sh   （默认保持现有设置）
EOF
