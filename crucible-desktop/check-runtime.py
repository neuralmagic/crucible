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


exe = Path(sys.argv[1]).resolve()
with socket.socket() as api, socket.socket() as hooks:
    api.bind(("127.0.0.1", 0))
    hooks.bind(("127.0.0.1", 0))
    port = api.getsockname()[1]
    hooks_port = hooks.getsockname()[1]
with tempfile.TemporaryDirectory(prefix="playbooks-runtime-") as root:
    env = {
        "HOME": root,
        "USER": "playbooks-test",
        "PATH": f"{exe.parent}:/usr/bin:/bin",
        "XDG_STATE_HOME": root,
        "CRUCIBLE_REPO": str(Path(root) / "absent-checkout"),
        "HOOKS_PORT": str(hooks_port),
    }
    log = Path(root) / "controller.log"
    with log.open("wb") as output:
        controller = subprocess.Popen(
            [exe, "--local-controller", str(port)],
            cwd=root,
            env=env,
            stdout=output,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            deadline = time.monotonic() + 300
            while True:
                if controller.poll() is not None:
                    raise RuntimeError(f"controller exited with {controller.returncode}")
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
            if controller.poll() is None:
                controller.terminate()
                try:
                    controller.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    os.killpg(controller.pid, signal.SIGKILL)
                    controller.wait()

print("bundled controller and UI started without a checkout or build tools")
