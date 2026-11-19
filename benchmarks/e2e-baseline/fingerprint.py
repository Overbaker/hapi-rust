"""Hash the same named source bytes in Git or in the synced measurement tree."""
import hashlib
import subprocess
import sys
from pathlib import Path

roots = ("benchmarks/e2e-baseline", "e2e/perf")

def included(name):
    return not {"results", "__pycache__"}.intersection(Path(name).parts)

def fingerprint(revision=None):
    if revision:
        listing = subprocess.check_output(["git", "ls-tree", "-rz", "--name-only", revision, "--", *roots])
        paths = [name.decode() for name in listing.split(b"\0") if name]
    else:
        paths = [path.as_posix() for root in roots for path in Path(root).rglob("*") if path.is_file()]
    digest = hashlib.sha256()
    for path in sorted(filter(included, paths)):
        contents = (subprocess.check_output(["git", "show", f"{revision}:{path}"])
                    if revision else Path(path).read_bytes())
        digest.update(hashlib.sha256(contents).digest() + path.encode() + b"\0")
    return digest.hexdigest()

if __name__ == "__main__":
    print(fingerprint(sys.argv[1] if len(sys.argv) > 1 else None))
