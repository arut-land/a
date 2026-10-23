"""Nearest-rank spreads include rare observations in small hosted cohorts."""
import math
import statistics


def distribution(samples):
    samples = sorted(samples)
    def percentile(p):
        return samples[max(0, math.ceil(len(samples) * p) - 1)]
    return dict(median=statistics.median(samples), min=min(samples), max=max(samples),
                p10=percentile(.1), p90=percentile(.9), samples=samples)


def observations(result):
    result["metrics"] = {name: distribution(value["samples"]) for name, value in result["metrics"].items()}
    return result
