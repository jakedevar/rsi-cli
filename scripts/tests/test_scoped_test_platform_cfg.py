"""#574: check-touched-shards must derive only host-runnable scoped-test filters."""

from pathlib import Path
import re
import runpy
import unittest
from unittest.mock import patch

from scripts.tests import test_scoped_test as scoped_fixtures


SCRIPTS = Path(__file__).resolve().parents[1]
GATE = '#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]'
UNSUPPORTED = '#[cfg(not(any(target_os = "linux", target_os = "macos")))]'


def test_source(gate, name):
    return f'{gate}\n{GATE}\n#[test]\nfn {name}() {{}}\n'


class PlatformCfgSelectionTest(unittest.TestCase):
    def setUp(self):
        self.checker = runpy.run_path(str(SCRIPTS / 'check-touched-shards'))
        self.globals = self.checker['rust_functions'].__globals__
        self.globals['HOST_OS'] = 'linux'

    def names(self, source):
        return [name for name, *_ in self.checker['rust_functions'](source)]

    def test_linux_derivation_keeps_linux_and_unknown_tests(self):
        source = (test_source(UNSUPPORTED, 'unsupported')
                  + test_source('#[cfg(target_os = "linux")]', 'linux')
                  + test_source('#[cfg(not(custom_predicate))]', 'unknown'))
        path = 'crates/rsid/src/session/reaper.rs'
        files = {'crates/rsid/src/lib.rs': 'mod session;\n',
                 'crates/rsid/src/session.rs': 'mod reaper;\n', path: source}
        with patch.dict(self.globals, read_blobs=lambda *args: dict(files),
                        changed_line_numbers=lambda *args: ([], range(1, len(source.splitlines()) + 1))):
            tree = self.checker['SourceTree'](Path('.'), 'HEAD', list(files))
            filters = self.checker['changed_test_filters'](tree, Path('.'), 'base', 'HEAD', [path])
        plan = scoped_fixtures.RUNNER['selection_plan'](filters, {'session-03': ['rsid']})
        selections = plan['rsid']
        selected = [name for name in ('unsupported', 'linux', 'unknown')
                    if any(re.search(s['regex'], f'session::reaper::{name}') for s in selections)]
        self.assertEqual(selected, ['linux', 'unknown'])
        self.assertEqual(self.names(source), ['linux', 'unknown'])

    def test_nested_inline_and_external_module_gates_exclude_their_tests(self):
        source = (f'{UNSUPPORTED}\n#[allow(dead_code)]\nmod unsupported {{\n'
                  '    mod nested {\n        #[test]\n        fn hidden() {}\n    }\n}\n'
                  '#[cfg(all(unix, not(windows)))]\nmod native {\n'
                  '    #[test]\n    fn linux() {}\n}\n')
        self.assertEqual(self.names(source), ['linux'])
        root = 'crates/x/src/lib.rs'
        child = 'crates/x/src/unsupported.rs'
        files = {root: f'{UNSUPPORTED}\nmod unsupported;\n',
                 child: '#[test]\nfn hidden() {}\n'}
        excluded = set()
        self.checker['module_map'](files, excluded)
        self.assertEqual(excluded, {child})

    def test_multiline_cfgs_apply_to_functions_and_enclosing_modules(self):
        gate = '#[cfg(not(any(\n    target_os = "linux",\n    target_os = "macos",\n)))]'
        self.assertEqual(self.names(test_source(gate, 'hidden') + test_source('', 'live')), ['live'])
        source = f'{gate}\nmod other {{\n    #[test]\n    fn hidden() {{}}\n}}\n'
        self.assertFalse(self.checker['holds_runnable_test'](source))
        files = {'crates/x/src/lib.rs': f'{gate}\nmod other;\n', 'crates/x/src/other.rs': ''}
        excluded = set()
        self.checker['module_map'](files, excluded)
        self.assertEqual(excluded, {'crates/x/src/other.rs'})

    def test_attribute_comments_keep_existing_test_detection(self):
        source = '#[cfg(target_os = "linux")] // native only\n#[test] // still a test\nfn live() {}\n'
        self.assertEqual(self.names(source), ['live'])
        self.assertTrue(self.checker['holds_runnable_test'](source))

    def test_empty_filter_still_fails_validation(self):
        with self.assertRaisesRegex(ValueError, 'refusing unscoped filter'):
            scoped_fixtures.RUNNER['selection_plan'](['rsid='], {})

    def test_three_valued_cfgs_and_malformed_predicates_are_conservative(self):
        cases = {
            'not(any(target_os = "linux", target_os = "macos"))': True,
            'all(unix, not(windows), target_os = "linux")': False,
            'windows': True,
            'any()': True,
            'all()': False,
            'not(custom_predicate)': False,
            'any(windows, custom_predicate)': False,
            'all(unix, custom_predicate)': False,
            'all(windows, custom_predicate)': True,
            'target_arch = "unknown"': False,
            'unknown_operator(windows)': False,
            'not(unix, windows)': False,
            'not(unix) trailing': False,
            'not(unix': False,
        }
        for expression, excluded in cases.items():
            with self.subTest(expression=expression):
                self.assertEqual(self.checker['other_os_gate'](f'#[cfg({expression})]'), excluded)

    def test_reported_reaper_test_is_excluded_from_linux_source_inventory(self):
        source = (SCRIPTS.parent / 'crates/rsid/src/session/reaper.rs').read_text()
        functions = {function[0]: function for function in self.checker['_rust_functions'](source)}
        reported = functions['unsupported_platform_settlement_reapers_refuse_without_an_inventory']
        self.assertTrue(self.checker['function_gated_off'](source, reported[1], reported[3]))
        self.assertIn('bounded_proc_open_and_reader_preserve_inventory_errors', self.names(source))

    def test_all_eleven_reported_linux_exclusions_match_real_source_gates(self):
        cases = {
            'crates/rsid/src/project_files/listing/tests.rs': [
                'listings_refuse_platforms_without_safe_directory_handles'],
            'crates/rsid/src/project_files/read_safety_tests.rs': [
                'safe_reads_are_explicitly_unsupported'],
            'crates/rsid/src/project_files/write.rs': [
                'all_writes_explicitly_refuse_without_a_portable_no_follow_implementation'],
            'crates/rsid-store/src/sandbox/target_reclaim_unsupported.rs': [
                'refuses_reclaim_without_linux_openat2_containment'],
            'crates/rsid-store/src/sandbox/git_worktree.rs': [
                'quarantine_tree_proof_is_refused_where_xattrs_cannot_be_inventoried',
                'settlement_retains_the_stamp_without_ownership_proof_2615'],
            'crates/rsid/src/session/reaper.rs': [
                'unsupported_platform_settlement_reapers_refuse_without_an_inventory'],
            'crates/rsid/src/session/tests/worker_no_result.rs': [
                'a_host_without_a_process_table_reads_unknown'],
            'crates/rsid-store/src/store/tests.rs': [
                'v100_exact_v99_sources_converge_without_title_or_lineage_data_loss',
                'v100_unknown_or_hybrid_v99_catalog_fails_closed',
                'v100_failpoints_roll_back_both_exact_v99_sources_and_retry'],
        }
        # The reclaim shim has no local platform gate: its real parent mounts it
        # through a cfg-gated #[path] declaration. Exercise that propagation too.
        paths = [*cases, 'crates/rsid-store/src/lib.rs', 'crates/rsid-store/src/sandbox/mod.rs']
        files = {path: (SCRIPTS.parent / path).read_text() for path in paths}
        gated = set()
        modules = self.checker['module_map'](files, gated)
        shim = 'crates/rsid-store/src/sandbox/target_reclaim_unsupported.rs'
        self.assertEqual(modules[shim], 'sandbox::target_reclaim')
        self.assertIn(shim, gated)
        self.checker['mark_gated_off'](files, gated)
        for path, names in cases.items():
            source = files[path]
            functions = {function[0]: function for function in self.checker['_rust_functions'](source)}
            for name in names:
                with self.subTest(path=path, name=name):
                    function = functions[name]
                    excluded = (self.checker['file_gated_off'](source)
                                or self.checker['function_gated_off'](source, function[1], function[3]))
                    self.assertTrue(excluded)

    def test_misspelled_filter_still_reports_unmatched(self):
        fixture = scoped_fixtures.ScopedTest('test_genuinely_empty_filters_report_no_tests_selected')
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        missing = 'rsid=shard:store-01:test(misspelled_test)'
        code, output = fixture.invoke(linux=False, entrypoint=True, filters=[missing])
        self.assertEqual(code, 1, output)
        receipt = fixture.receipt_of(output)
        self.assertEqual(receipt['unmatched_filters'], [missing])
        self.assertEqual(receipt['packages'][0]['phase'], 'no_tests_selected')


if __name__ == '__main__':
    unittest.main()
