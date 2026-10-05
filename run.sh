#!/usr/bin/env bash
# 直接以系统 python3 运行（screen 友好）：
#   交互:   screen -S zcm 然后运行 ./run.sh（Ctrl+A D 脱离）
#   后台:   screen -dmS zcm ./run.sh
# 依赖先装好: python3 -m pip install -r requirements.txt
cd "$(dirname "$0")"
exec python3 -m zcm
