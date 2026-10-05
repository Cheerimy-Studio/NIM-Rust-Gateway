"""docker / docker compose CLI 封装。

只依赖退出码与行文本，不解析 compose 的结构化输出；变更操作共用
op_lock 串行，exec/日志/查询可并发；所有命令带超时，超时即 kill。
"""
from __future__ import annotations

import asyncio
import os
import time
from typing import Callable, Iterable

from .config import Settings


class DockerUnavailable(RuntimeError):
    pass


class DockerCtl:
    def __init__(self, s: Settings):
        self.s = s
        self.bin = os.environ.get("ZCM_DOCKER") or "docker"
        # 惰性建锁：Python 3.8 在事件循环外创建 Lock 会绑到临时 loop，
        # 之后在 uvicorn 的 loop 里争用时抛 attached to a different loop
        self._op_lock: asyncio.Lock | None = None
        self._ok_until = 0.0
        self._ok = False

    @property
    def op_lock(self) -> asyncio.Lock:
        if self._op_lock is None:
            self._op_lock = asyncio.Lock()
        return self._op_lock

    # ---- 基础 -------------------------------------------------------------
    def compose_args(self) -> list[str]:
        return [
            self.bin,
            "compose",
            "--project-name",
            self.s.project,
            "--project-directory",
            str(self.s.runtime_dir),
            "-f",
            str(self.s.compose_file),
        ]

    async def _run(
        self,
        cmd: list[str],
        timeout: float = 120.0,
        on_line: Callable[[str], None] | None = None,
    ) -> tuple[int, list[str]]:
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
            )
        except FileNotFoundError as e:
            raise DockerUnavailable(f"找不到 {self.bin} 命令") from e
        lines: list[str] = []

        async def _pump() -> None:
            assert proc.stdout is not None
            while True:
                raw = await proc.stdout.readline()
                if not raw:
                    break
                line = raw.decode("utf-8", "replace").rstrip()
                lines.append(line)
                if on_line:
                    try:
                        on_line(line)
                    except Exception:
                        pass

        pump = asyncio.create_task(_pump())
        try:
            await asyncio.wait_for(proc.wait(), timeout)
        except asyncio.TimeoutError:
            proc.kill()
            await proc.wait()
            pump.cancel()
            raise TimeoutError("命令超时（已终止）: " + " ".join(cmd[:6]) + " ...")
        await pump
        return proc.returncode or 0, lines

    async def available(self) -> bool:
        """守护进程可达性，结果缓存 60 秒。"""
        if time.time() < self._ok_until:
            return self._ok
        try:
            rc, _ = await self._run(
                [self.bin, "version", "--format", "{{.Server.Version}}"], timeout=10
            )
            self._ok = rc == 0
        except Exception:
            self._ok = False
        # 可用与否都缓存：docker 挂掉时避免每个周期都空转子进程
        self._ok_until = time.time() + (60 if self._ok else 15)
        return self._ok

    # ---- 网络 -------------------------------------------------------------
    async def ensure_network(self) -> None:
        rc, _ = await self._run([self.bin, "network", "inspect", self.s.mesh], timeout=15)
        if rc != 0:
            rc, out = await self._run([self.bin, "network", "create", self.s.mesh], timeout=30)
            if rc != 0:
                raise RuntimeError("创建网络失败: " + "\n".join(out[-5:]))

    # ---- 生命周期（变更类，走 op_lock） ------------------------------------
    async def up(self) -> tuple[int, list[str]]:
        """调用方需持有 op_lock。收敛到 compose 文件的期望状态。"""
        return await self._run(
            self.compose_args() + ["up", "-d", "--remove-orphans"], timeout=900
        )

    async def pull(self) -> tuple[int, list[str]]:
        """调用方需持有 op_lock。"""
        return await self._run(self.compose_args() + ["pull"], timeout=1800)

    async def up_service(self, name: str) -> tuple[int, list[str]]:
        return await self._run(
            self.compose_args() + ["up", "-d", "--no-deps", name], timeout=600
        )

    async def stop_service(self, name: str) -> tuple[int, list[str]]:
        return await self._run(self.compose_args() + ["rm", "-sf", name], timeout=120)

    async def restart_service(self, name: str) -> tuple[int, list[str]]:
        return await self._run(self.compose_args() + ["restart", name], timeout=180)

    async def remove_volumes(self, names: Iterable[str]) -> tuple[int, list[str]]:
        names = [n for n in names if n]
        if not names:
            return 0, []
        return await self._run([self.bin, "volume", "rm", "-f", *names], timeout=60)

    # ---- 查询类 -----------------------------------------------------------
    async def ps_states(self) -> dict[str, str]:
        """{服务名: 状态}，仅本 compose 项目。"""
        rc, out = await self._run(
            [
                self.bin,
                "ps",
                "-a",
                "--filter",
                f"label=com.docker.compose.project={self.s.project}",
                "--format",
                '{{.Label "com.docker.compose.service"}}\t{{.State}}',
            ],
            timeout=30,
        )
        states: dict[str, str] = {}
        if rc == 0:
            for ln in out:
                if "\t" in ln:
                    name, st = ln.split("\t", 1)
                    if name:
                        states[name] = st.strip()
        return states

    async def logs(self, service: str, tail: int = 300) -> str:
        rc, out = await self._run(
            self.compose_args() + ["logs", "--no-color", "--tail", str(tail), service],
            timeout=120,
        )
        text = "\n".join(out)
        if rc == 0 and text.strip():
            return text
        # 兜底：compose logs 失败或为空时，按服务标签找容器直读 docker logs
        rc2, names = await self._run(
            [
                self.bin, "ps", "-a",
                "--filter", f"label=com.docker.compose.project={self.s.project}",
                "--filter", f"label=com.docker.compose.service={service}",
                "--format", "{{.Names}}",
            ],
            timeout=30,
        )
        if rc2 == 0 and names:
            rc3, dlog = await self._run(
                [self.bin, "logs", "--tail", str(tail), names[0].strip()], timeout=120
            )
            if rc3 == 0 and dlog:
                return "\n".join(dlog)
        if text.strip():
            return text
        return "（暂无日志）" + (("\n" + "\n".join(out[-10:])) if out else "")

    async def exec_service(
        self,
        service: str,
        cmd: list[str],
        timeout: float = 600.0,
        on_line: Callable[[str], None] | None = None,
    ) -> tuple[int, list[str]]:
        return await self._run(
            self.compose_args() + ["exec", "-T", service, *cmd], timeout=timeout, on_line=on_line
        )

    async def run_service(
        self,
        service: str,
        cmd: list[str],
        timeout: float = 600.0,
        on_line: Callable[[str], None] | None = None,
        env: dict[str, str] | None = None,
    ) -> tuple[int, list[str]]:
        """一次性容器里跑命令。

        未登录时 zc 容器会退出并进入重启循环，exec 进不去；
        run 共享服务的卷与环境，从镜像新起一个容器执行，不受主容器状态影响。
        env 可按需覆盖服务环境变量（如清空代理走直连）。
        """
        extra: list[str] = []
        for k, v in (env or {}).items():
            extra += ["-e", f"{k}={v}"]
        return await self._run(
            self.compose_args() + ["run", "--rm", "-T", "--no-deps", *extra, service, *cmd],
            timeout=timeout,
            on_line=on_line,
        )

    async def recreate_service(self, name: str) -> tuple[int, list[str]]:
        return await self._run(
            self.compose_args() + ["up", "-d", "--no-deps", "--force-recreate", name],
            timeout=600,
        )

    async def fix_volume_owner(self, volume: str, image: str) -> tuple[int, list[str]]:
        """把命名卷属主改为镜像内的 bun 用户。

        命名卷新建时根目录属主是 root:root，容器以 bun 运行写不进去，会 EACCES。
        用同一镜像以 root 起一次性容器做 chown，无需额外拉取镜像；幂等。
        """
        return await self._run(
            [
                self.bin, "run", "--rm", "--user", "root",
                "--entrypoint", "chown",
                "-v", f"{volume}:/target",
                image, "-R", "bun:bun", "/target",
            ],
            timeout=180,
        )
