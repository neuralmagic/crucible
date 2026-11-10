"""The Python client against a real crucible-kueue router on loopback.

The server is `cargo run -p crucible-kueue-client --example serve`: the shipped
router and tool policy, a signer for task tokens, and a kube client whose API
server never answers. A call the server refuses fails with its refusal; one it
lets through fails only when it reaches Kubernetes.
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
from pathlib import Path
from typing import Iterator

import pytest

from crucible_kueue import AsyncClient, Client, KueueError, ToolError

REPO = Path(__file__).resolve().parents[2]


class Served:
    def __init__(self, url: str, token: str) -> None:
        self.url = url
        self.token = token


@pytest.fixture(scope="session")
def served() -> Iterator[Served]:
    process = subprocess.Popen(
        ["cargo", "run", "-q", "-p", "crucible-kueue-client", "--example", "serve"],
        cwd=REPO,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "SQLX_OFFLINE": "true"},
    )
    assert process.stdout is not None and process.stdin is not None
    line = process.stdout.readline()
    assert line, "the test server printed nothing"
    ready = json.loads(line)
    yield Served(ready["url"], ready["token"])
    process.stdin.close()
    process.wait(timeout=30)


def task_env(served: Served, token_file: Path) -> str:
    return json.dumps(
        [{"name": "kueue", "url": served.url, "token_file": str(token_file)}]
    )


def test_a_task_reaches_the_server_it_was_granted_as_the_run(
    served: Served, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    token_file = tmp_path / "kueue"
    token_file.write_text(served.token)
    monkeypatch.setenv("CRUCIBLE_TASK_SERVERS", task_env(served, token_file))

    who = Client().whoami()

    assert who["status"] == "ok"
    assert who["kube_user"] == "crucible:run:crucible-run-brave-otter"
    assert who["launched_by"] == "alice"
    assert who["namespaces"] == ["team-e2e"]


def test_each_call_reads_the_token_file_again(
    served: Served, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    token_file = tmp_path / "kueue"
    token_file.write_text("not-a-token")
    monkeypatch.setenv("CRUCIBLE_TASK_SERVERS", task_env(served, token_file))
    client = Client()

    with pytest.raises(KueueError) as refused:
        client.whoami()
    assert refused.value.kind == "refused"
    assert refused.value.detail["http_status"] == 401

    token_file.write_text(served.token + "\n")
    assert client.whoami()["status"] == "ok"

    token_file.unlink()
    with pytest.raises(KueueError) as gone:
        client.whoami()
    assert gone.value.kind == "token"


def test_a_refusal_is_a_tool_error_that_says_whether_to_retry(served: Served) -> None:
    client = Client(url=served.url, token=served.token)

    with pytest.raises(ToolError) as elsewhere:
        client.queue_describe("gpu", namespace="team-b")
    assert (elsewhere.value.status, elsewhere.value.retryable) == ("refused", False)
    assert "access check queue-get" in elsewhere.value.body["error"]

    with pytest.raises(ToolError) as unreachable:
        client.queue_describe("gpu")
    assert (unreachable.value.status, unreachable.value.retryable) == ("error", True)

    with pytest.raises(ToolError) as waited:
        client.wait("Job", "train", timeout=1)
    assert (waited.value.status, waited.value.retryable) == ("error", True)


def test_arguments_left_unset_are_not_sent(served: Served) -> None:
    client = Client(url=served.url, token=served.token)
    assert client.call("whoami", namespace=None)["namespace"] == "team-e2e"
    with pytest.raises(ToolError) as default:
        client.call("queue_describe", queue="gpu", namespace=None)
    assert default.value.status == "error", "the token's namespace, which the run may read"
    with pytest.raises(ToolError) as named:
        client.call("queue_describe", queue="gpu", namespace="team-b")
    assert named.value.status == "refused"


def test_a_task_without_the_grant_is_told_what_to_declare(
    served: Served, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("CRUCIBLE_TASK_SERVERS", raising=False)
    with pytest.raises(KueueError) as unset:
        Client()
    assert unset.value.kind == "not_granted"
    assert "[workflow] servers" in str(unset.value)

    monkeypatch.setenv(
        "CRUCIBLE_TASK_SERVERS", task_env(served, tmp_path / "kueue")
    )
    with pytest.raises(KueueError) as other:
        Client("buildit")
    assert other.value.kind == "not_granted"
    assert '["kueue"]' in str(other.value)


def test_a_token_needs_a_url_and_only_one_source(served: Served) -> None:
    with pytest.raises(ValueError):
        Client(token="t")
    with pytest.raises(ValueError):
        Client(url=served.url)
    with pytest.raises(ValueError):
        Client(url=served.url, token="t", token_file="/f")


def test_a_server_that_is_not_there_is_a_transport_error() -> None:
    client = Client(url="http://127.0.0.1:1/mcp", token="t")
    with pytest.raises(KueueError) as unreachable:
        client.whoami()
    assert unreachable.value.kind == "transport"


def test_an_async_task_awaits_its_calls_together(
    served: Served, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    token_file = tmp_path / "kueue"
    token_file.write_text(served.token)
    monkeypatch.setenv("CRUCIBLE_TASK_SERVERS", task_env(served, token_file))

    async def task() -> tuple[dict, BaseException, BaseException, BaseException]:
        kueue = AsyncClient()
        return await asyncio.gather(
            kueue.whoami(),
            kueue.queue_describe("gpu", namespace="team-b"),
            kueue.queue_describe("gpu"),
            kueue.wait("Job", "train", timeout=1),
            return_exceptions=True,
        )

    who, elsewhere, unreachable, waited = asyncio.run(task())

    assert who["kube_user"] == "crucible:run:crucible-run-brave-otter"
    assert isinstance(elsewhere, ToolError)
    assert (elsewhere.status, elsewhere.retryable) == ("refused", False)
    assert isinstance(unreachable, ToolError)
    assert (unreachable.status, unreachable.retryable) == ("error", True)
    assert isinstance(waited, ToolError)
    assert (waited.status, waited.retryable) == ("error", True)


def test_an_async_call_reads_the_token_file_each_time(
    served: Served, tmp_path: Path
) -> None:
    token_file = tmp_path / "kueue"
    token_file.write_text("not-a-token")

    async def task() -> tuple[int, str]:
        kueue = AsyncClient(url=served.url, token_file=str(token_file))
        with pytest.raises(KueueError) as refused:
            await kueue.whoami()
        token_file.write_text(served.token)
        return refused.value.detail["http_status"], (await kueue.whoami())["status"]

    assert asyncio.run(task()) == (401, "ok")


def test_a_cancelled_wait_stops_waiting(served: Served) -> None:
    async def task() -> bool:
        kueue = AsyncClient(url="http://10.255.255.1:9/mcp", token=served.token)
        waiting = asyncio.ensure_future(kueue.wait("Job", "train", timeout=600))
        await asyncio.sleep(0.2)
        waiting.cancel()
        try:
            await waiting
        except asyncio.CancelledError:
            return True
        return False

    assert asyncio.run(task())


def test_exposing_ports_is_part_of_the_plan_and_only_for_serving_kinds(
    served: Served,
) -> None:
    client = Client(url=served.url, token=served.token)
    job = {
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": "train"},
        "spec": {"template": {"spec": {"restartPolicy": "Never", "containers": [
            {"name": "c", "image": "ubi9"}
        ]}}},
    }
    with pytest.raises(ToolError) as batch:
        client.workload_plan("gpu", job, expose=[8000])
    assert batch.value.status == "error", "the queue is read before the manifest"
