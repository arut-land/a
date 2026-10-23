"""Compare matching hosted lanes; a missing baseline is explicit, never zero."""
import argparse
import io
import json
import os
from pathlib import Path
import urllib.error
import urllib.parse
import urllib.request
import zipfile

from stats import distribution, observations


class ArtifactRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, url):
        redirected = super().redirect_request(request, response, code, message, headers, url)
        before, after = map(urllib.parse.urlsplit, (request.full_url, url))
        if (before.scheme, before.netloc) != (after.scheme, after.netloc):
            # Artifact storage authenticates its signed URL, not the GitHub token.
            redirected.remove_header("Authorization")
        return redirected


def api(path, binary=False):
    request = urllib.request.Request("https://api.github.com/repos/" + os.environ["GITHUB_REPOSITORY"] + path,
                                     headers={"Authorization": "Bearer " + os.environ["GH_TOKEN"],
                                              "Accept": "application/vnd.github+json"})
    with urllib.request.build_opener(ArtifactRedirect()).open(request, timeout=60) as response:
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
                if "result.json" not in archive.namelist():
                    continue
                result = json.loads(archive.read("result.json"))
                if not explicit and (result["variant"] or result["revision"] != run["head_sha"]):
                    continue
                return observations(result), run["id"]
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
    now = observations(json.loads(args.result.read_text()))
    was, run_id = baseline(now["runner"], args.baseline_run)
    if was and (was["schema"], was["runner"], was["machine"]) != (now["schema"], now["runner"], now["machine"]):
        raise ValueError("baseline schema or architecture differs")
    lines = [f'## {now["runner"]}: `{now["revision"][:12]}`', "",
             f'Variant: `{json.dumps(now["variant"])}`. Baseline: ' + (str(run_id) if was else "no successful main artifact yet"), "",
             "Values are medians; spread is p10–p90. Noise means the median delta is no larger than either observed spread; this is not a significance test.", "",
             "| Axis (unit in name) | Median | p10–p90 | Min–max | Main delta |", "| --- | ---: | ---: | ---: | --- |"]
    comparisons = {}
    for name, value in now["metrics"].items():
        previous = was["metrics"].get(name) if was else None
        delta = compare(value, previous)
        comparisons[name] = delta
        lines.append(f'| {name} | {value["median"]:.3f} | {value["p10"]:.3f}–{value["p90"]:.3f} | {value["min"]:.3f}–{value["max"]:.3f} | {delta} |')
    lines += ["", "Idle CPU is accumulated OS user+kernel time over 10 seconds after 15 seconds settling in each of five fresh homes. RSS is sampled at three seconds. Zero means no counter increment at OS resolution. Client is the line REPL; native terminal first-paint/PTY gates remain in CI.", ""]
    paired_path = args.result.with_name("paired.json")
    if paired_path.exists():
        paired = json.loads(paired_path.read_text())
        lines += ["## Same-runner reference", "", f'Reference: `{paired["reference_revision"]}`. Planned five alternating AB/BA rounds; completed: {paired["completed_rounds"]}. Builds finish before these windows. Driver turns per process: {paired["driver_turns"]}; large JSON results: {paired["large_json_results"]}.', "",
                  "| Axis | Reference median | Candidate median | Delta |", "| --- | ---: | ---: | --- |"]
        for name, value in paired["roles"]["candidate"].items():
            value = distribution(value["samples"])
            previous_samples = paired["roles"]["reference"].get(name)
            previous = distribution(previous_samples["samples"]) if previous_samples else None
            previous_median = f'{previous["median"]:.3f}' if previous else "—"
            lines.append(f'| {name} | {previous_median} | {value["median"]:.3f} | {compare(value, previous)} |')
        lines += [""]
    summary = "\n".join(lines)
    args.result.with_name("summary.md").write_text(summary)
    args.result.with_name("comparison.json").write_text(json.dumps(dict(baseline_run=run_id, axes=comparisons), indent=2))
    with open(os.environ.get("GITHUB_STEP_SUMMARY", os.devnull), "a") as stream:
        stream.write(summary)
    print(summary)


if __name__ == "__main__":
    main()
