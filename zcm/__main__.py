"""python -m zcm 启动服务；--reset-password 重置管理密码。"""
from __future__ import annotations

import logging
import sys

from .auth import hash_password


def _reset_password(argv: list[str]) -> int:
    if len(argv) < 2 or not argv[1].strip():
        print("用法: python3 -m zcm --reset-password '新密码'", file=sys.stderr)
        return 2
    from .config import Registry, Settings

    s = Settings.from_env()
    reg = Registry(s.accounts_path, s)
    reg.data["admin_password_hash"] = hash_password(argv[1].strip())
    reg.save()
    print(f"已重置 {s.accounts_path} 的管理密码（账号 {reg.data.get('admin_username', 'admin')}）")
    return 0


def main() -> None:
    argv = sys.argv[1:]
    if argv and argv[0] == "--reset-password":
        sys.exit(_reset_password(argv))

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s [%(name)s] %(message)s",
        stream=sys.stdout,
    )
    from .config import Settings
    from .web import create_app

    s = Settings.from_env()
    host, _, port = s.addr.rpartition(":")
    app = create_app(s)
    import uvicorn

    uvicorn.run(
        app,
        host=host or "0.0.0.0",
        port=int(port or 9000),
        log_level="info",
        # 单进程内存态，workers 固定 1
        workers=1,
    )


if __name__ == "__main__":
    main()
