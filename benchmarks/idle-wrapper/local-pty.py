"""Run the local wrapper on a PTY and drain terminal output during measurement."""
import fcntl
import os
import pty
import struct
import sys
import termios
import time

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 110, 0, 0))
pid = os.fork()
if pid == 0:
    os.close(master)
    os.setsid()
    fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
    for fd in range(3):
        os.dup2(slave, fd)
    os.close(slave)
    os.execv(sys.argv[1], sys.argv[1:])
os.close(slave)
with open("wrapper.pid.tmp", "w") as record:
    record.write(str(pid))
os.replace("wrapper.pid.tmp", "wrapper.pid")
trusted = False
prompted = False
try:
    while True:
        data = os.read(master, 65536)
        if not data:
            break
        os.write(1, data)
        if not trusted and b"Trust and continue" in data:
            os.write(master, b"\r")
            trusted = True
        if trusted and not prompted and b"context:" in data:
            os.write(master, b"idle-benchmark-prompt")
            time.sleep(0.15)
            os.write(master, b"\r")
            prompted = True
        if b"\x1b[6n" in data:
            os.write(master, b"\x1b[1;1R")
except OSError:
    pass
_, status = os.waitpid(pid, 0)
sys.exit(os.waitstatus_to_exitcode(status))
