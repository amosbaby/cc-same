#!/bin/bash
# 一键安装并锁定配置 cc-same 命令行工具（macOS；Linux 也可用）。
#
#   ./scripts/secure-setup.sh                                    # 从列表里选组织
#   ./scripts/secure-setup.sh --org ACCT/ORG --org ACCT/ORG      # 直接指定（完整 ID 或唯一前缀）
#   ./scripts/secure-setup.sh --org ACCT/ORG=名字 --org ACCT/ORG=名字 --yes   # 全程不提问
#
# 按顺序做这些事：
#   1. 用锁定的依赖版本从本仓库编译安装 `cc-same`（不含 GUI、自动更新和账号切换）；
#   2. 列出本机所有账号/组织，带名字、会话数和最近的会话标题；
#   3. 只让你选的组织参与同步（`config --only`），其他账号和组织（包括以后新登录的）一律不碰；
#   4. 等 Claude 退出，展示同步计划，确认后首次同步（之前会先做一份永久保留的基线快照），
#      然后启动后台同步。
#
# 随时可以撤销：`cc-same uninstall`（每个组织都保留自己的完整会话副本）。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ORGS=()
YES=0
while [ $# -gt 0 ]; do
    case "$1" in
        --org) ORGS+=("$2"); shift 2 ;;
        --yes|-y) YES=1; shift ;;
        -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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

# ---------------------------------------------------------------- 1. 编译安装
command -v cargo >/dev/null 2>&1 || die "没有安装 Rust。请先安装（https://rustup.rs，或 brew install rustup && rustup-init），再重新运行。"
say "编译 cc-same（$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo '当前目录')，锁定依赖版本）"
if [ -n "$(git -C "$ROOT" status --porcelain 2>/dev/null)" ]; then
    echo "提示：这个目录有本地改动，会按改动后的代码编译。"
fi
cargo install --locked --force --path "$ROOT/crates/cli" --bin cc-same
BIN="$(command -v cc-same || true)"
[ -n "$BIN" ] || BIN="${CARGO_HOME:-$HOME/.cargo}/bin/cc-same"
[ -x "$BIN" ] || die "cc-same 已编译，但不在 PATH 里；请把 ${CARGO_HOME:-$HOME/.cargo}/bin 加入 PATH"
"$BIN" --version
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
$("$BIN" orgs --tsv)
EOF
}

say "本机的账号/组织"
show_orgs
[ "${#IDS[@]}" -ge 2 ] || die "找到的账号/组织少于两个。请打开 Claude，在每个组织里各用一次（随便开一个会话），然后重新运行。"

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
"$BIN" sync --yes
"$BIN" install --yes
say "完成"
"$BIN" status
cat <<EOF

重新打开 Claude，照常在 Claude 里切换组织即可，后台会保持两边的会话列表一致。
  查看状态：cc-same doctor        cc-same orgs
  快照：    cc-same snapshots     cc-same restore <id>   （先退出 Claude）
  改名字：  cc-same config --name <账号>/<组织>=<名字>
  停止同步：cc-same uninstall     （每个组织都保留完整副本）
EOF
