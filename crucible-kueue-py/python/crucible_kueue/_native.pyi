from typing import Awaitable, Optional

class NativeError(Exception): ...

class NativeClient:
    def __init__(
        self,
        url: str,
        *,
        token: Optional[str] = None,
        token_file: Optional[str] = None,
    ) -> None: ...
    @staticmethod
    def from_env(server: str) -> "NativeClient": ...
    def call(self, tool: str, arguments: str) -> Awaitable[str]: ...
    def call_blocking(self, tool: str, arguments: str) -> str: ...
    def wait(
        self,
        kind: str,
        name: str,
        until: str,
        namespace: Optional[str],
        timeout_seconds: float,
    ) -> Awaitable[str]: ...
    def wait_blocking(
        self,
        kind: str,
        name: str,
        until: str,
        namespace: Optional[str],
        timeout_seconds: float,
    ) -> str: ...
