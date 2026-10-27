#!/bin/bash
# 双击进入实时面板。关掉终端窗口即退出。

cd "$(dirname "$0")" || exit 1
BIN="./target/release/rmcooler"

if [ ! -x "$BIN" ]; then
  echo "找不到可执行文件 $BIN"
  echo "先编译：cargo build --release"
  read -r -p "按回车关闭…" _
  exit 1
fi

exec "$BIN"
