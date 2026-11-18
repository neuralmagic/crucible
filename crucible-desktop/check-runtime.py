import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


controller = Path(sys.argv[1]).resolve()
with socket.socket() as api, socket.socket() as hooks:
    api.bind(("127.0.0.1", 0))
    hooks.bind(("127.0.0.1", 0))
    port = api.getsockname()[1]
    hooks_port = hooks.getsockname()[1]
with tempfile.TemporaryDirectory(prefix="playbooks-runtime-") as root:
    env = {
        "HOME": root,
        "USER": "playbooks-test",
        "PATH": "/usr/bin:/bin",
        "XDG_STATE_HOME": root,
    }
    log = Path(root) / "controller.log"
    with log.open("wb") as output:
        running = subprocess.Popen(
            [controller, "local", "--port", str(port), "--hooks-port", str(hooks_port)],
            cwd=root,
            env=env,
            stdout=output,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            deadline = time.monotonic() + 300
            while True:
                if running.poll() is not None:
                    raise RuntimeError(f"controller exited with {running.returncode}")
                try:
                    with urllib.request.urlopen(
                        f"http://127.0.0.1:{port}/healthz", timeout=2
                    ) as response:
                        assert response.status == 200
                    break
                except (urllib.error.URLError, TimeoutError):
                    if time.monotonic() >= deadline:
                        raise TimeoutError("bundled controller did not become healthy")
                    time.sleep(0.25)
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/", timeout=5) as response:
                assert response.status == 200
                assert b"<html" in response.read()
            assert (Path(root) / "crucible-controller/postgres/data").is_dir()
        except Exception:
            print(log.read_text(errors="replace"), file=sys.stderr)
            raise
        finally:
            if running.poll() is None:
                running.terminate()
                try:
                    running.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    os.killpg(running.pid, signal.SIGKILL)
                    running.wait()

print("bundled controller and UI started without a checkout or build tools")
