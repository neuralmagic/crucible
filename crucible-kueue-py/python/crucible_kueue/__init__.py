"""Client for crucible-kueue, for a crucible run's command and evaluate tasks.

A pack that lists ``kueue`` in ``[workflow] servers`` gets the server's URL and a
token file in ``CRUCIBLE_TASK_SERVERS``. ``AsyncClient()`` reads both; every call
is checked by the server against the run's own Kubernetes RBAC.

``AsyncClient`` is for asyncio. ``Client`` has the same methods and blocks.
"""

from __future__ import annotations

import contextlib
import json
from typing import (
    Any,
    AsyncIterator,
    Awaitable,
    Generic,
    Iterator,
    Literal,
    Optional,
    TypeVar,
)

from crucible_kueue._native import NativeClient, NativeError

__all__ = [
    "AsyncClient",
    "Client",
    "KueueError",
    "ToolError",
    "WorkloadFailed",
    "WorkloadStuck",
    "WaitTimeout",
    "Kind",
    "Until",
]

Kind = Literal[
    "Job",
    "RayJob",
    "LeaderWorkerSet",
    "Deployment",
    "StatefulSet",
    "JobSet",
    "PyTorchJob",
]
Until = Literal["admitted", "running", "ready", "finished"]
Detail = Literal["summary", "full"]
ListFilter = Literal["mine", "managed", "all"]
Json = dict[str, Any]


class KueueError(Exception):
    """A call that did not answer ``ok``.

    ``kind`` is ``tool``, ``failed``, ``stuck``, ``wait_timeout``, ``refused``,
    ``rpc``, ``not_granted``, ``token``, ``transport`` or ``protocol``.
    """

    def __init__(self, message: str, detail: Json) -> None:
        super().__init__(message)
        self.kind: str = detail.get("kind", "")
        self.detail = detail


class ToolError(KueueError):
    """The server answered a status other than ``ok``."""

    def __init__(self, message: str, detail: Json) -> None:
        super().__init__(message, detail)
        self.status: str = detail["status"]
        self.retryable: bool = detail["retryable"]
        self.body: Json = detail["body"]


class WorkloadFailed(KueueError):
    """The workload finished without succeeding, or ended before the point waited for.

    ``reason`` is Kueue's, such as ``BackoffLimitExceeded``; ``body`` is the last
    status report, whose ``problem_pods`` carry exit codes and messages.
    """

    def __init__(self, message: str, detail: Json) -> None:
        super().__init__(message, detail)
        self.reason: str = detail["reason"]
        self.body: Json = detail["body"]


class WorkloadStuck(KueueError):
    """A pod cannot start: its image cannot be pulled or its container cannot run.

    ``body`` is the last status report; ``problem_pods`` say why.
    """

    def __init__(self, message: str, detail: Json) -> None:
        super().__init__(message, detail)
        self.body: Json = detail["body"]


class WaitTimeout(KueueError):
    """The workload did not reach the point waited for in time."""

    def __init__(self, message: str, detail: Json) -> None:
        super().__init__(message, detail)
        self.body: Json = detail["body"]


_ERRORS: dict[str, type[KueueError]] = {
    "tool": ToolError,
    "failed": WorkloadFailed,
    "stuck": WorkloadStuck,
    "wait_timeout": WaitTimeout,
}


def _raised(error: NativeError) -> KueueError:
    message, detail = error.args
    parsed: Json = json.loads(detail)
    return _ERRORS.get(parsed.get("kind", ""), KueueError)(message, parsed)


def _given(**arguments: Any) -> Json:
    return {key: value for key, value in arguments.items() if value is not None}


R = TypeVar("R")

_EPOCH = "1970-01-01T00:00:00Z"
_MAX_LOG_BYTES = 1024 * 1024


def _native(
    server: str, url: Optional[str], token: Optional[str], token_file: Optional[str]
) -> NativeClient:
    try:
        if url is None:
            if token is not None or token_file is not None:
                raise ValueError("token and token_file need url")
            return NativeClient.from_env(server)
        return NativeClient(url, token=token, token_file=token_file)
    except NativeError as error:
        raise _raised(error) from None


class _Tools(Generic[R]):
    """The server's tools, each a call that answers ``R``."""

    def call(self, tool: str, **arguments: Any) -> R:
        """Call ``tool`` and answer its result. Arguments left ``None`` are not sent."""
        raise NotImplementedError

    def whoami(self, namespace: Optional[str] = None) -> R:
        return self.call("whoami", namespace=namespace)

    def queues_list(self, namespace: Optional[str] = None) -> R:
        return self.call("queues_list", namespace=namespace)

    def queue_describe(self, queue: str, namespace: Optional[str] = None) -> R:
        return self.call("queue_describe", queue=queue, namespace=namespace)

    def hardware_describe(self, queue: str, namespace: Optional[str] = None) -> R:
        return self.call("hardware_describe", queue=queue, namespace=namespace)

    def workload_plan(
        self,
        queue: str,
        manifest: Json,
        namespace: Optional[str] = None,
        expose: Optional[list[int]] = None,
    ) -> R:
        return self.call(
            "workload_plan",
            queue=queue,
            manifest=manifest,
            namespace=namespace,
            expose=expose,
        )

    def workload_submit(
        self,
        queue: str,
        manifest: Json,
        plan_hash: str,
        namespace: Optional[str] = None,
        expose: Optional[list[int]] = None,
    ) -> R:
        return self.call(
            "workload_submit",
            queue=queue,
            manifest=manifest,
            plan_hash=plan_hash,
            namespace=namespace,
            expose=expose,
        )

    def workload_status(
        self,
        kind: Kind,
        name: str,
        *,
        namespace: Optional[str] = None,
        detail: Optional[Detail] = None,
        until: Optional[Until] = None,
        timeout_seconds: Optional[int] = None,
    ) -> R:
        return self.call(
            "workload_status",
            kind=kind,
            name=name,
            namespace=namespace,
            detail=detail,
            until=until,
            timeout_seconds=timeout_seconds,
        )

    def workload_list(
        self, namespace: Optional[str] = None, filter: Optional[ListFilter] = None
    ) -> R:
        return self.call("workload_list", namespace=namespace, filter=filter)

    def workload_logs(
        self,
        kind: Kind,
        name: str,
        *,
        namespace: Optional[str] = None,
        pod: Optional[str] = None,
        container: Optional[str] = None,
        tail_lines: Optional[int] = None,
        since_seconds: Optional[int] = None,
        grep: Optional[str] = None,
        max_bytes: Optional[int] = None,
        previous: Optional[bool] = None,
        follow_seconds: Optional[int] = None,
        since_time: Optional[str] = None,
        timestamps: Optional[bool] = None,
    ) -> R:
        return self.call(
            "workload_logs",
            kind=kind,
            name=name,
            namespace=namespace,
            pod=pod,
            container=container,
            tail_lines=tail_lines,
            since_seconds=since_seconds,
            grep=grep,
            max_bytes=max_bytes,
            previous=previous,
            follow_seconds=follow_seconds,
            since_time=since_time,
            timestamps=timestamps,
        )

    def workload_scale(
        self, kind: Kind, name: str, replicas: int, namespace: Optional[str] = None
    ) -> R:
        return self.call(
            "workload_scale",
            kind=kind,
            name=name,
            replicas=replicas,
            namespace=namespace,
        )

    def workload_stop(
        self, kind: Kind, name: str, namespace: Optional[str] = None
    ) -> R:
        return self.call("workload_stop", kind=kind, name=name, namespace=namespace)

    def workload_resume(
        self, kind: Kind, name: str, namespace: Optional[str] = None
    ) -> R:
        return self.call("workload_resume", kind=kind, name=name, namespace=namespace)

    def workload_delete(
        self, kind: Kind, name: str, namespace: Optional[str] = None
    ) -> R:
        return self.call("workload_delete", kind=kind, name=name, namespace=namespace)


class AsyncClient(_Tools[Awaitable[Json]]):
    """crucible-kueue as this task reaches it, for asyncio.

    With no arguments, the server granted under ``server`` in
    ``CRUCIBLE_TASK_SERVERS``. Outside a run, pass ``url`` and one of ``token``
    and ``token_file``. Every method answers an awaitable.
    """

    def __init__(
        self,
        server: str = "kueue",
        *,
        url: Optional[str] = None,
        token: Optional[str] = None,
        token_file: Optional[str] = None,
    ) -> None:
        self._native = _native(server, url, token, token_file)

    def call(self, tool: str, **arguments: Any) -> Awaitable[Json]:
        return self._call(tool, _given(**arguments))

    async def _call(self, tool: str, arguments: Json) -> Json:
        try:
            return json.loads(await self._native.call(tool, json.dumps(arguments)))
        except NativeError as error:
            raise _raised(error) from None

    async def submit(
        self,
        queue: str,
        manifest: Json,
        namespace: Optional[str] = None,
        expose: Optional[list[int]] = None,
    ) -> Json:
        """Plan ``manifest`` and submit it under the hash the plan answered.

        ``expose`` lists ports of a Deployment, StatefulSet or LeaderWorkerSet to
        reach in the cluster. The reply's ``service`` then holds ``host`` and
        ``ports``; the Service is deleted with the workload.
        """
        plan = await self.workload_plan(queue, manifest, namespace, expose)
        return await self.workload_submit(
            queue, manifest, plan["plan_hash"], namespace, expose
        )

    async def wait(
        self,
        kind: Kind,
        name: str,
        until: Until = "finished",
        *,
        namespace: Optional[str] = None,
        timeout: float = 3600.0,
    ) -> Json:
        """Wait up to ``timeout`` seconds for the workload to reach ``until``.

        Raises ``WorkloadFailed`` when it finishes without succeeding or ends
        short of ``until``, ``WorkloadStuck`` when a pod cannot start, and
        ``WaitTimeout`` when time runs out. Cancelling the wait leaves the
        workload as it is.
        """
        try:
            return json.loads(
                await self._native.wait(kind, name, until, namespace, timeout)
            )
        except NativeError as error:
            raise _raised(error) from None


    async def run(
        self,
        queue: str,
        manifest: Json,
        *,
        namespace: Optional[str] = None,
        timeout: float = 3600.0,
        grep: Optional[str] = None,
        max_bytes: int = _MAX_LOG_BYTES,
    ) -> str:
        """Run a Job (or other batch workload) to its end and answer its log.

        Raises ``WorkloadFailed`` when it fails, ``WorkloadStuck`` when it cannot
        start and ``WaitTimeout`` when ``timeout`` passes; each carries the last
        status in ``body``.
        """
        job = await self.submit(queue, manifest, namespace)
        kind, name = manifest["kind"], job["name"]
        await self.wait(kind, name, "finished", namespace=namespace, timeout=timeout)
        logs = await self.workload_logs(
            kind, name, namespace=namespace, grep=grep, max_bytes=max_bytes
        )
        return "\n".join(entry.get("log", "") for entry in logs["logs"])

    @contextlib.asynccontextmanager
    async def serving(
        self,
        queue: str,
        manifest: Json,
        port: int,
        *,
        namespace: Optional[str] = None,
        timeout: float = 3600.0,
    ) -> AsyncIterator[str]:
        """Serve a Deployment, StatefulSet or LeaderWorkerSet and yield its URL.

        ``async with kueue.serving(queue, manifest, 8000) as url:`` waits until
        the workload is ready, yields ``http://<host>:<port>`` reachable from
        tasks and from other workloads, and deletes the workload and its
        Service on the way out, whatever happened inside.
        """
        served = await self.submit(queue, manifest, namespace, expose=[port])
        kind, name = manifest["kind"], served["name"]
        try:
            await self.wait(kind, name, "ready", namespace=namespace, timeout=timeout)
            yield f"http://{served['service']['host']}:{port}"
        finally:
            await self.workload_delete(kind, name, namespace)

    async def follow_logs(
        self,
        kind: Kind,
        name: str,
        *,
        namespace: Optional[str] = None,
        container: Optional[str] = None,
        window_seconds: int = 30,
    ) -> AsyncIterator[tuple[str, str]]:
        """Yield ``(pod, line)`` for each line the workload's pods write, once each.

        It reads from the start of each pod's log and keeps going, a pod that
        crashes and is replaced included, until the workload is gone or the
        caller stops iterating. Each line starts with the time it was written.
        """
        seen: dict[str, tuple[str, set[str]]] = {}
        since = _EPOCH
        while True:
            try:
                reply = await self.workload_logs(
                    kind,
                    name,
                    namespace=namespace,
                    container=container,
                    follow_seconds=window_seconds,
                    since_time=since,
                    timestamps=True,
                    max_bytes=_MAX_LOG_BYTES,
                )
            except ToolError as error:
                if error.status == "not_found":
                    return
                raise
            for entry in reply["logs"]:
                pod = entry["pod"]
                last, at_last = seen.get(pod, ("", set()))
                for line in entry.get("log", "").splitlines():
                    stamp = line.split(" ", 1)[0]
                    if stamp < last or (stamp == last and line in at_last):
                        continue
                    if stamp != last:
                        last, at_last = stamp, set()
                    at_last.add(line)
                    yield pod, line
                seen[pod] = (last, at_last)
            stamps = [last for last, _ in seen.values() if last]
            since = min(stamps) if stamps else _EPOCH


class Client(_Tools[Json]):
    """``AsyncClient`` for code with no event loop: every method blocks."""

    def __init__(
        self,
        server: str = "kueue",
        *,
        url: Optional[str] = None,
        token: Optional[str] = None,
        token_file: Optional[str] = None,
    ) -> None:
        self._native = _native(server, url, token, token_file)

    def call(self, tool: str, **arguments: Any) -> Json:
        try:
            return json.loads(
                self._native.call_blocking(tool, json.dumps(_given(**arguments)))
            )
        except NativeError as error:
            raise _raised(error) from None

    def submit(
        self,
        queue: str,
        manifest: Json,
        namespace: Optional[str] = None,
        expose: Optional[list[int]] = None,
    ) -> Json:
        """Plan ``manifest`` and submit it under the hash the plan answered.

        ``expose`` lists ports of a Deployment, StatefulSet or LeaderWorkerSet to
        reach in the cluster. The reply's ``service`` then holds ``host`` and
        ``ports``; the Service is deleted with the workload.
        """
        plan = self.workload_plan(queue, manifest, namespace, expose)
        return self.workload_submit(
            queue, manifest, plan["plan_hash"], namespace, expose
        )

    def wait(
        self,
        kind: Kind,
        name: str,
        until: Until = "finished",
        *,
        namespace: Optional[str] = None,
        timeout: float = 3600.0,
    ) -> Json:
        """Wait up to ``timeout`` seconds for the workload to reach ``until``.

        Raises ``WorkloadFailed`` when it finishes without succeeding or ends
        short of ``until``, ``WorkloadStuck`` when a pod cannot start, and
        ``WaitTimeout`` when time runs out.
        """
        try:
            return json.loads(
                self._native.wait_blocking(kind, name, until, namespace, timeout)
            )
        except NativeError as error:
            raise _raised(error) from None

    def run(
        self,
        queue: str,
        manifest: Json,
        *,
        namespace: Optional[str] = None,
        timeout: float = 3600.0,
        grep: Optional[str] = None,
        max_bytes: int = _MAX_LOG_BYTES,
    ) -> str:
        """Run a Job (or other batch workload) to its end and answer its log.

        Raises ``WorkloadFailed`` when it fails, ``WorkloadStuck`` when it cannot
        start and ``WaitTimeout`` when ``timeout`` passes; each carries the last
        status in ``body``.
        """
        job = self.submit(queue, manifest, namespace)
        kind, name = manifest["kind"], job["name"]
        self.wait(kind, name, "finished", namespace=namespace, timeout=timeout)
        logs = self.workload_logs(
            kind, name, namespace=namespace, grep=grep, max_bytes=max_bytes
        )
        return "\n".join(entry.get("log", "") for entry in logs["logs"])

    @contextlib.contextmanager
    def serving(
        self,
        queue: str,
        manifest: Json,
        port: int,
        *,
        namespace: Optional[str] = None,
        timeout: float = 3600.0,
    ) -> Iterator[str]:
        """Serve a Deployment, StatefulSet or LeaderWorkerSet and yield its URL.

        ``with kueue.serving(queue, manifest, 8000) as url:`` waits until the
        workload is ready, yields ``http://<host>:<port>`` reachable from tasks
        and from other workloads, and deletes the workload and its Service on
        the way out, whatever happened inside.
        """
        served = self.submit(queue, manifest, namespace, expose=[port])
        kind, name = manifest["kind"], served["name"]
        try:
            self.wait(kind, name, "ready", namespace=namespace, timeout=timeout)
            yield f"http://{served['service']['host']}:{port}"
        finally:
            self.workload_delete(kind, name, namespace)
