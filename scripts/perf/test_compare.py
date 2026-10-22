"""Decision rules must not manufacture gains from absent or noisy measurements."""
import unittest

from compare import compare


class Comparison(unittest.TestCase):
    def test_an_absent_baseline_never_becomes_zero(self):
        self.assertEqual(compare(dict(median=3, p10=2, p90=4), None), "no baseline")

    def test_either_runs_spread_can_explain_a_delta(self):
        quiet = dict(median=10, p10=10, p90=10)
        noisy = dict(median=12, p10=8, p90=16)
        self.assertIn("within noise", compare(quiet, noisy))
        self.assertIn("within noise", compare(noisy, quiet))
        self.assertIn("outside observed spread", compare(quiet, dict(median=12, p10=12, p90=12)))

    def test_zero_cpu_still_reports_an_absolute_regression(self):
        value = compare(dict(median=15.625, p10=15.625, p90=15.625), dict(median=0, p10=0, p90=0))
        self.assertIn("+15.625", value)
        self.assertIn("outside observed spread", value)
        self.assertNotIn("%", value)


if __name__ == "__main__":
    unittest.main()
