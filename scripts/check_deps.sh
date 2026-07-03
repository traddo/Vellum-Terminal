#!/usr/bin/env bash
# 铁律 1 验收：release 二进制的 ldd 输出不得出现 libfreetype / libfontconfig。
# 这是 CI 级验收项——渲染路径必须零系统文本渲染依赖。
set -euo pipefail

cd "$(dirname "$0")/.."

echo "==> 构建 release 二进制"
cargo build --release --bins

FAIL=0
for BIN in target/release/vlt target/release/snapshot; do
    if [[ ! -x "$BIN" ]]; then
        echo "跳过（不存在）：$BIN"
        continue
    fi
    echo ""
    echo "==> ldd $BIN"
    LDD_OUT="$(ldd "$BIN" || true)"
    echo "$LDD_OUT"

    if echo "$LDD_OUT" | grep -qiE 'libfreetype|libfontconfig'; then
        echo ""
        echo "❌ 失败：$BIN 链接到了 libfreetype / libfontconfig（违背铁律 1）"
        FAIL=1
    else
        echo "✅ 通过：$BIN 无 libfreetype / libfontconfig"
    fi
done

echo ""
if [[ "$FAIL" -ne 0 ]]; then
    echo "==> 验收失败"
    exit 1
fi
echo "==> 验收通过：渲染路径零系统文本渲染依赖"
