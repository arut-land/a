"""Windows uses the same release budget measurements as the hosted workflow."""
import json
from pathlib import Path
import sys

from hosted import budgets

def assert_memory_budget(metrics):
    assert metrics["idle_charged_kib"]["max"] < 10240, metrics


def main():
    output = Path("target/budgets").resolve()
    output.mkdir(parents=True, exist_ok=True)
    metrics = budgets(output, Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())
    (output / "result.json").write_text(json.dumps(metrics, indent=2) + "\n")
    print(json.dumps(metrics, indent=2))
    assert_memory_budget(metrics)
    assert metrics["idle_daemon_cpu_ms"]["max"] == 0
    assert metrics["idle_line_client_cpu_ms"]["max"] == 0


if __name__ == "__main__":
    main()
