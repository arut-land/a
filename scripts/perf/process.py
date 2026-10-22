"""OS process accounting; psutil calls native APIs on each hosted platform."""
import json
import sys

import psutil


def snapshot(pid):
    process = psutil.Process(pid)
    cpu = process.cpu_times()
    return {"rss_kib": process.memory_info().rss / 1024,
            "cpu_seconds": cpu.user + cpu.system}


if __name__ == "__main__":
    record = snapshot(int(sys.argv[1]))
    print(int(record["rss_kib"]) if len(sys.argv) > 2 else json.dumps(record))
