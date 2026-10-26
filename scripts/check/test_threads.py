import unittest
from collections import Counter

import threads


class ThreadRules(unittest.TestCase):
    def test_imported_and_aliased_apis_are_detected(self):
        cases = [
            ("use std::thread::{spawn, sleep};", "spawn(|| {}); sleep(d);", ["spawn", "sleep"]),
            ("use std::thread::{spawn as launch, sleep as nap};",
             "launch(|| {}); nap(d);", ["spawn", "sleep"]),
            ("use std::{thread as workers};", "workers::scope(|s| {});", ["scope"]),
            ("use std::thread::{self as workers, Builder as Worker};",
             "workers::spawn(|| {}); Worker::new().spawn(|| {});", ["spawn", "Builder"]),
            ("use system::thread as workers; use std as system; use workers::spawn as launch;",
             "launch(|| {});", ["spawn"]),
            ("use std::thread::*;", "spawn(|| {}); sleep(d); Builder::new();",
             ["spawn", "sleep", "Builder"]),
            ("use std::thread::spawn as r#launch;", "r#launch(|| {});", ["spawn"]),
            ("", "std::thread::spawn(|| {}); std::thread::sleep(d);", ["spawn", "sleep"]),
        ]
        for imports, body, expected in cases:
            with self.subTest(imports=imports, body=body):
                found = threads.findings(imports + "\nfn run() { " + body + " }")
                self.assertCountEqual([f.api for f in found], expected)
                self.assertTrue(all(f.function == "run" and f.line == 2 for f in found))

    def test_aliases_respect_lexical_scope(self):
        source = """
use std::thread::spawn as launch;
fn one() { use custom::launch; launch(); }
fn two() { launch(|| {}); }
fn three() { use std::thread::sleep as nap; nap(d); }
fn four() { nap(d); }
"""
        self.assertEqual([(f.function, f.api) for f in threads.findings(source)],
                         [("two", "spawn"), ("three", "sleep")])

    def test_comments_literals_and_test_items_are_not_calls(self):
        source = '''
// std::thread::spawn(|| {});
/* outer /* std::thread::sleep(d); */ std::thread::spawn(|| {}); */
fn text() {
    let normal = "std::thread::spawn(|| {})";
    let raw = r###"std::thread::sleep(d)"###;
    let brace = '}';
}
#[cfg(test)]
mod tests {
    use std::thread::spawn;
    fn test() { spawn(|| {}); }
}
#[cfg(all(unix, test))]
fn another_test() { std::thread::sleep(d); }
'''
        self.assertEqual(threads.findings(source), [])

    def test_an_exception_allows_only_its_annotated_call(self):
        source = """fn read_stdin(handler: fn()) {
// thread-exception: repl-stdin
std::thread::spawn(|| {});
std::thread::spawn(|| {});
}
"""
        found = threads.findings(source)
        used = Counter()
        unexpected = threads.exceptions("apps/cli/src/main.rs", source, found, used)
        self.assertEqual([f.line for f in unexpected], [4])
        self.assertEqual(used, {"repl-stdin": 1})

    def test_a_cfg_that_can_build_without_tests_is_checked(self):
        source = '''
#[cfg(any(test, feature = "live"))]
fn live() { std::thread::spawn(|| {}); }
#[cfg(not(test))]
fn release() { std::thread::sleep(d); }
'''
        self.assertEqual([(f.function, f.api) for f in threads.findings(source)],
                         [("live", "spawn"), ("release", "sleep")])

    def test_exception_cannot_change_api_function_file_or_be_reused(self):
        cases = [
            ("apps/cli/src/main.rs", "read_stdin", "sleep(d)"),
            ("apps/cli/src/main.rs", "other", "spawn(|| {})"),
            ("apps/cli/src/once.rs", "read_stdin", "spawn(|| {})"),
        ]
        for path, function, call in cases:
            source = (f"fn {function}() {{\n// thread-exception: repl-stdin\n"
                      f"std::thread::{call};\n}}")
            with self.subTest(path=path, function=function, call=call):
                used = Counter()
                self.assertEqual(len(threads.exceptions(path, source, threads.findings(source), used)), 1)
                self.assertFalse(used)
        source = """fn read_stdin() {
// thread-exception: repl-stdin
std::thread::spawn(|| {});
// thread-exception: repl-stdin
std::thread::spawn(|| {});
}
"""
        self.assertEqual(len(threads.exceptions("apps/cli/src/main.rs", source,
                                              threads.findings(source), Counter())), 1)

    def test_channel_timeouts_keep_their_call_site(self):
        source = """fn end() {
// thread-exception: terminal-deadline
let _ = wait.recv_timeout(DEADLINE);
let _ = other.recv_timeout(DEADLINE);
}"""
        found = threads.findings(source)
        self.assertEqual([f.api for f in found], ["recv_timeout", "recv_timeout"])
        self.assertEqual([f.line for f in threads.exceptions("apps/tui/src/term.rs", source,
                                                          found, Counter())], [4])


if __name__ == "__main__":
    unittest.main()
