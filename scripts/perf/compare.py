"""Compare matching hosted lanes; a missing baseline is explicit, never zero."""
import argparse
import io
import json
import os
from pathlib import Path
import urllib.error
import urllib.request
import zipfile


def api(path, binary=False):
    request = urllib.request.Request("https://api.github.com/repos/" + os.environ["GITHUB_REPOSITORY"] + path,
                                     headers={"Authorization": "Bearer " + os.environ["GH_TOKEN"],
                                              "Accept": "application/vnd.github+json"})
    with urllib.request.urlopen(request, timeout=60) as response:
        body = response.read()
        return body if binary else json.loads(body)


def baseline(runner, explicit):
    if explicit:
        runs = [{"id": explicit}]
    else:
        runs = api("/actions/workflows/perf.yml/runs?branch=main&status=success&per_page=20")["workflow_runs"]
    for run in runs:
        artifacts = api(f'/actions/runs/{run["id"]}/artifacts')["artifacts"]
        for artifact in artifacts:
            if artifact["name"] == "perf-" + runner and not artifact["expired"]:
                archive = zipfile.ZipFile(io.BytesIO(api(f'/actions/artifacts/{artifact["id"]}/zip', True)))
                return json.loads(archive.read("result.json")), run["id"]
    return None, None


def compare(now, was):
    if was is None:
        return "no baseline"
    delta = now["median"] - was["median"]
    spread = max(now["p90"] - now["p10"], was["p90"] - was["p10"])
    label = "within noise" if abs(delta) <= spread else "outside observed spread"
    percent = f' ({delta / was["median"] * 100:+.1f}%)' if was["median"] else ""
    return f"{delta:+.3f}{percent}; {label}"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("result", type=Path)
    parser.add_argument("--baseline-run", default="")
    args = parser.parse_args()
    now = json.loads(args.result.read_text())
    was, run_id = baseline(now["runner"], args.baseline_run)
    if was and (was["schema"], was["runner"], was["machine"]) != (now["schema"], now["runner"], now["machine"]):
        raise ValueError("baseline schema or architecture differs")
    lines = [f'## {now["runner"]}: `{now["revision"][:12]}`', "",
             f'Variant: `{json.dumps(now["variant"])}`. Baseline: ' + (str(run_id) if was else "no successful main artifact yet"), "",
             "Values are medians; spread is p10–p90. Noise means the median delta is no larger than either observed spread; this is not a significance test.", "",
             "| Axis (unit in name) | Median | p10–p90 | Main delta |", "| --- | ---: | ---: | --- |"]
    comparisons = {}
    for name, value in now["metrics"].items():
        previous = was["metrics"].get(name) if was else None
        delta = compare(value, previous)
        comparisons[name] = delta
        lines.append(f'| {name} | {value["median"]:.3f} | {value["p10"]:.3f}–{value["p90"]:.3f} | {delta} |')
    lines += ["", "Idle CPU is accumulated OS user+kernel time over 10 seconds after 15 seconds settling in each of five fresh homes. RSS is sampled at three seconds. Zero means no counter increment at OS resolution. Client is the line REPL; native terminal first-paint/PTY gates remain in CI.", ""]
    paired_path = args.result.with_name("paired.json")
    if paired_path.exists():
        paired = json.loads(paired_path.read_text())
        lines += ["## Same-runner reference", "", f'Reference: `{paired["reference_revision"]}`. Five alternating AB/BA rounds; builds finish before these windows.', "",
                  "| Axis | Reference median | Candidate median | Delta |", "| --- | ---: | ---: | --- |"]
        for name, value in paired["roles"]["candidate"].items():
            previous = paired["roles"]["reference"][name]
            lines.append(f'| {name} | {previous["median"]:.3f} | {value["median"]:.3f} | {compare(value, previous)} |')
        lines += [""]
    summary = "\n".join(lines)
    args.result.with_name("summary.md").write_text(summary)
    args.result.with_name("comparison.json").write_text(json.dumps(dict(baseline_run=run_id, axes=comparisons), indent=2))
    with open(os.environ.get("GITHUB_STEP_SUMMARY", os.devnull), "a") as stream:
        stream.write(summary)
    print(summary)


if __name__ == "__main__":
    main()
