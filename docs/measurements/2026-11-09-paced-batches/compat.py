"""Run with binaries named serve/view-{new,v51,v52} in /tmp/pb-compat-bins."""
import os
from pathlib import Path
import socket
import subprocess

out = Path(__file__).resolve().parent
bins = Path('/tmp/pb-compat-bins')
env = dict(os.environ, LATCH_TEST_PATTERN='sweep', LATCH_DATA_DIR='/tmp/pb-compat-data')
for host, viewer in [('new', 'v51'), ('v51', 'new'), ('new', 'v52'), ('v52', 'new'), ('new', 'new')]:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
        udp.bind(('127.0.0.1', 0))
        port = udp.getsockname()[1]
    assert port != 47850
    name = f'compat-{host}-{viewer}'
    with (out / f'{name}-host.txt').open('w') as log:
        server = subprocess.Popen([str(bins / f'serve-{host}'), '--bind', '127.0.0.1', '--port', str(port)], env=env, stdout=log, stderr=subprocess.PIPE, text=True)
        try:
            # Serve prints this only after both listeners bind.
            while True:
                line = server.stderr.readline()
                log.write(line)
                if 'serving on' in line:
                    break
                if not line:
                    raise RuntimeError(f'{name}: host exited {server.poll()}')
            with (out / f'{name}-viewer.txt').open('w') as result:
                run = subprocess.run([str(bins / f'view-{viewer}'), '127.0.0.1', '--port', str(port), '--width', '640', '--height', '360', '--fps', '60', '--kbps', '20000', '--seconds', '3'], env=env, stdout=result, stderr=subprocess.STDOUT, timeout=60)
            print(name, 'port', port, 'exit', run.returncode, flush=True)
            assert run.returncode == 0
        finally:
            server.terminate()
            _, rest = server.communicate(timeout=10)
            log.write(rest)
