"""管理面鉴权原语：PBKDF2 密码哈希、会话/CSRF 派生、登录限速。"""
from __future__ import annotations

import base64
import hashlib
import hmac
import os
import secrets
import time
from collections import defaultdict, deque

PBKDF2_ITERATIONS = 120_000


def hash_password(pw: str) -> str:
    salt = os.urandom(16)
    dk = hashlib.pbkdf2_hmac("sha256", pw.encode(), salt, PBKDF2_ITERATIONS)
    return (
        "pbkdf2$"
        + base64.b64encode(salt).decode()
        + "$"
        + base64.b64encode(dk).decode()
    )


def verify_password(pw: str, stored: str) -> bool:
    try:
        _, salt_b64, dk_b64 = stored.split("$")
        dk = hashlib.pbkdf2_hmac(
            "sha256", pw.encode(), base64.b64decode(salt_b64), PBKDF2_ITERATIONS
        )
        return hmac.compare_digest(dk, base64.b64decode(dk_b64))
    except Exception:
        return False


def session_cookie(secret: str) -> str:
    return hmac.new(secret.encode(), b"admin-session", hashlib.sha256).hexdigest()


def csrf_token(secret: str) -> str:
    return hmac.new(secret.encode(), b"admin-csrf", hashlib.sha256).hexdigest()


def new_session_secret() -> str:
    return secrets.token_urlsafe(32)


class LoginRateLimiter:
    """每 IP 登录限速：窗口内最多 max_attempts 次。"""

    def __init__(self, max_attempts: int = 10, window: float = 300.0):
        self.max_attempts = max_attempts
        self.window = window
        self._hits: dict[str, deque] = defaultdict(deque)

    def _prune(self, ip: str, now: float) -> None:
        q = self._hits.get(ip)
        if q is not None:
            while q and now - q[0] > self.window:
                q.popleft()

    def retry_after(self, ip: str) -> int:
        self._prune(ip, time.time())
        q = self._hits.get(ip)
        if not q or len(q) < self.max_attempts:
            return 0
        return max(1, int(self.window - (time.time() - q[0])))

    def record(self, ip: str) -> None:
        # 追踪表上限，防止海量假 IP 撑爆内存
        if len(self._hits) > 4096:
            now = time.time()
            for k in [k for k, q in self._hits.items() if not q or now - q[-1] > self.window]:
                self._hits.pop(k, None)
            if len(self._hits) > 4096:
                self._hits.clear()
        self._hits[ip].append(time.time())

    def clear(self, ip: str) -> None:
        self._hits.pop(ip, None)
