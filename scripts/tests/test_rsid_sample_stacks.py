"""Deterministic perf UAPI fixtures; no live target, perf privilege, or Cargo."""

from contextlib import ExitStack, redirect_stderr, redirect_stdout
import ctypes
import errno
import importlib.machinery
import importlib.util
import io
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import Mock, patch


SCRIPT = Path(__file__).resolve().parents[1] / "rsid-sample-stacks"
LOADER = importlib.machinery.SourceFileLoader("rsid_sample_stacks", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
S = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(S)


def record(kind, payload, misc=2):
    return struct.pack("<IHH", kind, misc, len(payload) + 8) + payload


def sample_payload(words=(), dynamic=None, abi=2):
    stack = struct.pack("<" + "Q" * len(words), *words)
    # Fixed UAPI order: IP, PID/TID, ABI, selected SP, size, data, dyn_size.
    data = struct.pack("<QIIQ", 0x701045, 41, 42, abi)
    if abi:
        data += struct.pack("<Q", 0x7fff0000)
    data += struct.pack("<Q", len(stack)) + stack
    if stack:
        data += struct.pack("<Q", len(stack) if dynamic is None else dynamic)
    return data


class RingDecoderTests(unittest.TestCase):
    def place(self, data, position, size=256):
        mapping = bytearray(4096 + size)
        for index, byte in enumerate(data):
            mapping[4096 + (position + index) % size] = byte
        return mapping

    def test_header_and_stack_wrap_then_lost_record(self):
        data = record(9, sample_payload((0x701062, 0x701068)))
        data += record(2, struct.pack("<QQ", 99, 17))
        mapping = self.place(data, 252)
        records, tail, errors = S.ring_records(mapping, 4096, 256, 252, 252 + len(data))
        self.assertEqual((len(records), tail, errors), (2, 252 + len(data), 0))
        self.assertEqual(S.decode_sample(records[0][2], 4096),
                         (0x701045, 41, 42, (0x701062, 0x701068)))
        self.assertEqual(struct.unpack("<QQ", records[1][2]), (99, 17))

    def test_unpublished_partial_record_is_left_for_next_head(self):
        data = record(9, sample_payload((0x701062,)))
        mapping = self.place(data, 240)
        self.assertEqual(S.ring_records(mapping, 4096, 256, 240, 247), ([], 240, 0))
        self.assertEqual(S.ring_records(mapping, 4096, 256, 240, 240 + len(data) - 1),
                         ([], 240, 0))
        records, tail, errors = S.ring_records(mapping, 4096, 256, 240, 240 + len(data))
        self.assertEqual((len(records), tail, errors), (1, 240 + len(data), 0))

    def test_bad_header_and_overrun_drop_without_scanning_stack_for_headers(self):
        for length in (0, 7, 9, 264):
            with self.subTest(length=length):
                mapping = self.place(struct.pack("<IHH", 9, 2, length), 0)
                self.assertEqual(S.ring_records(mapping, 4096, 256, 0, 8), ([], 8, 1))
        self.assertEqual(S.ring_records(bytearray(4352), 4096, 256, 0, 257), ([], 257, 1))
        self.assertEqual(S.ring_records(bytearray(4352), 4096, 256, 80, 8), ([], 8, 1))

    def test_abi_none_and_empty_stack_omit_registers_and_dynamic_size(self):
        self.assertEqual(S.decode_sample(sample_payload(abi=0), 4096), (0x701045, 41, 42, ()))
        self.assertEqual(S.decode_sample(sample_payload(), 4096), (0x701045, 41, 42, ()))

    def test_dynamic_size_ignores_padding_and_partial_last_word(self):
        self.assertEqual(S.decode_sample(sample_payload((12, 34, 56), dynamic=15), 4096)[3], (12,))

    def test_truncated_excess_stack_invalid_abi_and_dynamic_size_are_rejected(self):
        payload = sample_payload((11, 22))
        for size in range(len(payload)):
            with self.subTest(size=size), self.assertRaises((ValueError, struct.error)):
                S.decode_sample(payload[:size], 4096)
        for bad in (sample_payload((11, 22), dynamic=24), sample_payload(abi=1), payload + b"extra"):
            with self.assertRaises(ValueError):
                S.decode_sample(bad, 4096)
        with self.assertRaises(ValueError):
            S.decode_sample(payload, 8)


class SymbolAndAggregationTests(unittest.TestCase):
    def setUp(self):
        self.symbols = S.Symbols(
            "00001040 00000020 t rsid::hot\n"
            "00001060 00000010 T rsid_store::parent\n"
            "00001080 00000010 t external::helper\n"
            "00001040 00000020 t alias\n"
            "00001100 00000000 t rsid::zero\n",
            [(0x701000, 0x702000, 0x1000)], 0x700000)

    def test_relocation_sizes_aliases_and_mapping_boundaries(self):
        expected = {0x701040: "rsid::hot", 0x70105f: "rsid::hot",
                    0x701060: "rsid_store::parent", 0x701070: None,
                    0x701080: "external::helper", 0x701100: None,
                    0x601040: None, 0x702000: None}
        for address, name in expected.items():
            self.assertEqual(self.symbols.lookup(address), name)

    def test_inclusive_deduplicates_and_uses_return_address_minus_one(self):
        counts = S.Counts()
        # 0x701060 returns immediately after hot; 0x701070 after parent.
        payload = sample_payload((0x701060, 0x701070, 0x701068, 0x701070, 0xdeadbeef))
        counts.consume((9, 2, payload), self.symbols, 41, 42, 4096)
        self.assertEqual(counts.samples, 1)
        self.assertEqual(counts.leaf, {"rsid::hot": 1})
        self.assertEqual(counts.inclusive, {"rsid::hot": 1, "rsid_store::parent": 1})
        counts.consume((2, 0, struct.pack("<QQ", 5, 17)), self.symbols, 41, 42, 4096)
        counts.consume((5, 0, b""), self.symbols, 41, 42, 4096)
        counts.consume((9, 2, payload), self.symbols, 41, 43, 4096)
        counts.consume((9, 1, payload), self.symbols, 41, 42, 4096)
        self.assertEqual((counts.samples, counts.lost, counts.throttled, counts.malformed), (1, 17, 1, 2))

    def test_external_leaf_still_contributes_rsid_callers(self):
        counts = S.Counts()
        payload = bytearray(sample_payload((0x701070,)))
        struct.pack_into("<Q", payload, 0, 0x701088)
        counts.consume((9, 2, payload), self.symbols, 41, 42, 4096)
        self.assertEqual((counts.samples, counts.unmatched), (1, 1))
        self.assertEqual(counts.inclusive, {"rsid_store::parent": 1})
        self.assertEqual(sum(counts.leaf.values()), 0)

    def test_stripped_symbols_fail_explicitly_and_names_are_printable(self):
        with self.assertRaisesRegex(S.SamplerError, "unstripped"):
            S.Symbols("00001000 00000020 t external::helper", [], 0)
        self.assertEqual(S.safe_symbol("rsid::foo\x1b\n"), "rsid::foo??")
        self.assertEqual(len(S.safe_symbol("x" * 300)), 240)

    def test_pie_and_non_pie_segments_with_nonzero_file_offset(self):
        with tempfile.TemporaryFile() as source:
            header = bytearray(64)
            header[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<HH", header, 16, 3, 62)
            struct.pack_into("<Q", header, 32, 64)
            struct.pack_into("<HH", header, 54, 56, 1)
            source.write(header + struct.pack("<IIQQQQQQ", 1, 5, 0x2000, 0x5000, 0, 0x3000, 0x3000, 0x1000))
            source.flush()
            segments = S.elf_segments(source)
            self.assertEqual(segments, [(0x2000, 0x5000, 0x3000)])
            self.assertEqual(S.load_bias([(0x705000, 0x708000, 0x2000)], segments, 4096), 0x700000)
            self.assertEqual(S.load_bias([(0x5000, 0x8000, 0x2000)], segments, 4096), 0)
            # Split executable mapping at a later segment page.
            self.assertEqual(S.load_bias([(0x705000, 0x706000, 0x2000),
                                          (0x706000, 0x708000, 0x3000)], segments, 4096), 0x700000)
            with self.assertRaises(S.SamplerError):
                S.load_bias([(0x705000, 0x706000, 0x2000),
                             (0x806000, 0x808000, 0x3000)], segments, 4096)
            source.seek(16)
            source.write(struct.pack("<H", 2))  # ET_EXEC uses the same segment accounting.
            source.flush()
            self.assertEqual(S.elf_segments(source), segments)

    def test_bad_elf_and_missing_mapping_fail_before_symbolization(self):
        with tempfile.TemporaryFile() as source:
            source.write(b"not an ELF")
            source.flush()
            with self.assertRaisesRegex(S.SamplerError, "ELF64"):
                S.elf_segments(source)
        with self.assertRaisesRegex(S.SamplerError, "mapping"):
            S.load_bias([(0x705000, 0x706000, 0x8000)], [(0x2000, 0x5000, 0x3000)], 4096)

    def test_maps_use_inode_device_and_executable_permissions(self):
        data = (b"701000-702000 r-xp 00001000 08:02 123 /some/rsid (deleted)\n"
                b"702000-703000 rw-p 00002000 08:02 123 /some/rsid\n"
                b"801000-802000 r-xp 00001000 08:02 456 /different/rsid\n")
        self.assertEqual(S.executable_maps(data, (8, 2, 123)), [(0x701000, 0x702000, 0x1000)])
        with self.assertRaises(S.SamplerError):
            S.executable_maps(data, (8, 3, 123))


class BudgetAndSafetyTests(unittest.TestCase):
    def test_516kib_counts_metadata_not_only_data_and_adapts(self):
        self.assertEqual(S.ring_plan(3, 4096, 4096, 516), (3, 32, 405504, 528384))
        # Four 32-page rings would need 528KiB, exceeding the 516KiB budget.
        self.assertEqual(S.ring_plan(4, 4096, 4096, 516), (4, 16, 278528, 528384))
        self.assertEqual(S.ring_plan(8, 8192, 4096, 40), (1, 8, 36864, 40960))
        with self.assertRaises(S.SamplerError):
            S.ring_plan(3, 4096, 4096, 19)

    def test_budget_grid_obeys_sysctl_hard_cap_power_of_two_and_record_fit(self):
        for page in (4096, 16384, 65536):
            for stack in (256, 4096, 8192):
                for threads in range(1, 9):
                    for kb in (0, 20, 64, 516, 100000):
                        try:
                            count, pages, allocation, budget = S.ring_plan(threads, stack, page, kb)
                        except S.SamplerError:
                            continue
                        self.assertLessEqual(allocation, min(kb * 1024, 528384))
                        self.assertEqual(allocation, count * (pages + 1) * page)
                        self.assertLessEqual(count, threads)
                        self.assertEqual(pages & (pages - 1), 0)
                        self.assertGreaterEqual(pages * page, 2 * (stack + 64))

    def test_hottest_thread_delta_rejects_reused_tid_and_is_bounded(self):
        self.assertEqual(S.hottest({10: (100, 1), 11: (20, 2), 12: (100, 3)},
                                  {10: (110, 1), 11: (100, 9), 12: (120, 3), 13: (900, 1)}, 2),
                         [(20, 12, 3), (10, 10, 1)])
        tail = ["S"] + ["0"] * 19
        tail[11], tail[12], tail[19] = "7", "9", "123"
        self.assertEqual(S.task_stat(("42 (name ) with spaces) " + " ".join(tail)).encode()), (16, 123))

    def test_foreign_uid_and_non_rsid_are_rejected_before_perf(self):
        with patch.object(S.os, "getuid", return_value=1000), patch.object(S, "bounded_read", return_value=
                b"Uid:\t1000\t0\t1000\t1000\nTgid:\t41\n"):
            with self.assertRaisesRegex(S.SamplerError, "same"):
                S.validate_process(41)
        with patch.object(S.os, "getuid", return_value=1000), patch.object(S, "bounded_read", return_value=
                b"Uid:\t1000\t1000\t1000\t1000\nTgid:\t41\n"), patch.object(S.os, "readlink", return_value="/bin/other"):
            with self.assertRaisesRegex(S.SamplerError, "rsid"):
                S.validate_process(41)

    def test_unsupported_platform_is_explicit_without_proc_access(self):
        for system, machine in (("Darwin", "arm64"), ("Windows", "AMD64"), ("Linux", "aarch64")):
            with patch.object(S.platform, "system", return_value=system), patch.object(S.platform, "machine", return_value=machine), \
                    patch.object(S, "bounded_read", side_effect=AssertionError("unexpected proc read")), redirect_stderr(io.StringIO()) as output:
                self.assertEqual(S.main(["--pid", "41"]), 1)
                self.assertIn("unsupported: requires Linux", output.getvalue())

    def test_nonfinite_and_out_of_bounds_inputs_are_refused(self):
        for option, value in (("--threads", "9"), ("--seconds", "nan"), ("--seconds", "31"),
                              ("--discover-seconds", "inf"), ("--stack-bytes", "257"),
                              ("--hz", "101"), ("--top", "51")):
            with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as caught:
                S.arguments(["--pid", "41", option, value])
            self.assertEqual(caught.exception.code, 2)

    def test_perf_denial_returns_numeric_failure_without_raw_error(self):
        with patch.object(S, "sample", side_effect=OSError(errno.EPERM, "private diagnostic")), \
                redirect_stderr(io.StringIO()) as output:
            self.assertEqual(S.main(["--pid", "41"]), 1)
            self.assertEqual(output.getvalue(), "rsid-sample-stacks: denied by perf/proc policy or shared mlock budget; "
                             "no permission or sysctl changes attempted\n")

    def test_mmap_setup_failure_closes_perf_fd(self):
        libc = Mock()
        libc.syscall.return_value = 77
        with patch.object(S.ctypes, "CDLL", return_value=libc), patch.object(S.mmap, "mmap", side_effect=OSError(errno.ENOMEM, "budget")), \
                patch.object(S.os, "close") as close:
            with self.assertRaises(OSError):
                S.Ring(42, S.make_attr(4096, 100), 32, 4096, Mock())
            close.assert_called_once_with(77)

    def test_later_event_failure_closes_previously_opened_rings(self):
        # Exercise the real ExitStack in sample(), failing the second allocation.
        exe = Mock()
        exe.fileno.return_value = 88
        exe.__enter__ = Mock(return_value=exe)
        exe.__exit__ = Mock(return_value=False)
        first = Mock()
        fake_stat = Mock(st_dev=0, st_ino=123)
        with patch.object(S, "supported"), patch.object(S.os, "sysconf", return_value=4096), \
                patch.object(S, "bounded_read", return_value=b"516"), patch.object(S, "validate_process", return_value=1), \
                patch.object(S, "snapshot_tasks", return_value={}), patch.object(S.time, "sleep"), \
                patch.object(S, "hottest", return_value=[(5, 42, 1), (4, 43, 1)]), \
                patch("builtins.open", return_value=exe), patch.object(S.os, "fstat", return_value=fake_stat), \
                patch.object(S, "executable_maps", return_value=[]), patch.object(S, "nm_symbols", return_value=""), \
                patch.object(S, "Symbols"), patch.object(S, "elf_segments", return_value=[]), patch.object(S, "load_bias", return_value=0), \
                patch.object(S, "Atomics"), patch.object(S, "task_stat", return_value=(5, 1)), \
                patch.object(S, "Ring", side_effect=[first, OSError(errno.EPERM, "denied")]), redirect_stdout(io.StringIO()):
            with self.assertRaises(OSError):
                S.sample(S.arguments(["--pid", "41"]))
            first.close.assert_called_once_with()

    def test_sampling_success_and_interrupt_close_all_rings(self):
        for interrupted in (False, True):
            with self.subTest(interrupted=interrupted):
                exe = Mock()
                exe.fileno.return_value = 88
                exe.__enter__ = Mock(return_value=exe)
                exe.__exit__ = Mock(return_value=False)
                ring = Mock()
                ring.drain.side_effect = (KeyboardInterrupt() if interrupted else
                                         [([(9, 2, sample_payload((0x701070,)))], 0), ([], 0)])
                symbols = S.Symbols("1040 20 t rsid::hot\n1060 10 t rsid_store::parent",
                                    [(0x701000, 0x702000, 0x1000)], 0x700000)
                with ExitStack() as patches, redirect_stdout(io.StringIO()) as stdout, redirect_stderr(io.StringIO()) as stderr:
                    for context in (
                            patch.object(S, "supported"), patch.object(S.os, "sysconf", return_value=4096),
                            patch.object(S, "bounded_read", return_value=b"516"), patch.object(S, "validate_process", return_value=1),
                            patch.object(S, "snapshot_tasks", return_value={}), patch.object(S.time, "sleep"),
                            patch.object(S, "hottest", return_value=[(5, 42, 1)]),
                            patch("builtins.open", return_value=exe), patch.object(S.os, "fstat", return_value=Mock(st_dev=0, st_ino=123)),
                            patch.object(S, "executable_maps", return_value=[]), patch.object(S, "nm_symbols", return_value=""),
                            patch.object(S, "Symbols", return_value=symbols), patch.object(S, "elf_segments", return_value=[]),
                            patch.object(S, "load_bias", return_value=0), patch.object(S, "Atomics"),
                            patch.object(S, "task_stat", return_value=(5, 1)), patch.object(S, "Ring", return_value=ring),
                            patch.object(S.time, "monotonic", side_effect=[0, 10])):
                        patches.enter_context(context)
                    self.assertEqual(S.main(["--pid", "41"]), 130 if interrupted else 0)
                    ring.close.assert_called_once_with()
                    if interrupted:
                        self.assertIn("interrupted", stderr.getvalue())
                    else:
                        self.assertIn("total_samples=1 records=1", stdout.getvalue())
                        self.assertIn("rsid_store::parent", stdout.getvalue())
                        self.assertIn("rsid::hot", stdout.getvalue())
                        self.assertIn("stale stack words", stdout.getvalue())
                        self.assertIn("top unmatched leaf", stdout.getvalue())
                        ring.ioctl.assert_any_call(ring.fd, S.DISABLE, 0)

    def test_attr_matches_v3_uapi_offsets_and_user_only_fixed_period(self):
        # Independent ctypes definition from local linux/perf_event.h.
        class Attr(ctypes.LittleEndianStructure):
            _fields_ = [("type", ctypes.c_uint32), ("size", ctypes.c_uint32)] + [
                (name, ctypes.c_uint64) for name in ("config", "period", "sample_type", "read_format", "flags")] + [
                ("wakeup", ctypes.c_uint32), ("bp_type", ctypes.c_uint32)] + [
                (name, ctypes.c_uint64) for name in ("config1", "config2", "branch", "regs")] + [
                ("stack", ctypes.c_uint32), ("clockid", ctypes.c_int32)]
        attr = Attr.from_buffer_copy(S.make_attr(4096, 100))
        self.assertEqual(ctypes.sizeof(Attr), 96)
        self.assertEqual((attr.type, attr.config, attr.period, attr.sample_type), (1, 1, 10000000, 12291))
        self.assertEqual((attr.flags, attr.regs, attr.stack, attr.wakeup), (97, 128, 4096, 1))


class ApprovedTargetTests(unittest.TestCase):
    """#332: rsi and rsi-agent-mcp are sampleable; nothing else is."""

    def proc(self, exe, uid="1000"):
        status = f"Uid:\t{uid}\t{uid}\t{uid}\t{uid}\nTgid:\t41\n".encode()
        stat = b"41 (x) " + b" ".join([b"S"] + [b"0"] * 18 + [b"77"])
        return patch.object(S.os, "getuid", return_value=1000), \
            patch.object(S, "bounded_read", side_effect=lambda path, limit=65536:
                         status if str(path).endswith("status") else stat), \
            patch.object(S.os, "readlink", return_value=exe)

    def validate(self, exe, target, uid="1000"):
        with ExitStack() as stack:
            for patcher in self.proc(exe, uid):
                stack.enter_context(patcher)
            return S.validate_process(41, target)

    def test_each_approved_target_accepts_only_its_own_executable_name(self):
        for target, good in (("rsid", "/opt/bin/rsid"), ("rsi", "/home/u/.local/bin/rsi"),
                             ("rsi-agent-mcp", "/home/u/.local/bin/rsi-agent-mcp (deleted)")):
            self.assertEqual(self.validate(good, target), 77)
            for other in S.TARGETS:
                if other == target:
                    continue
                with self.assertRaisesRegex(S.SamplerError, f"must be {other}"):
                    self.validate(good, other)

    def test_prefix_and_lookalike_executables_are_refused(self):
        for exe in ("/bin/rsid-supervisor", "/bin/rsi-rpc", "/bin/xrsi", "/bin/bash", "/bin/rsi.sh"):
            for target in S.TARGETS:
                with self.assertRaises(S.SamplerError):
                    self.validate(exe, target)

    def test_same_uid_fence_applies_to_every_target(self):
        for target, exe in (("rsi", "/bin/rsi"), ("rsi-agent-mcp", "/bin/rsi-agent-mcp")):
            with ExitStack() as stack:
                for patcher in self.proc(exe, uid="0"):
                    stack.enter_context(patcher)
                with self.assertRaisesRegex(S.SamplerError, "same"):
                    S.validate_process(41, target)

    def test_target_argument_is_a_closed_choice_defaulting_to_rsid(self):
        self.assertEqual(S.arguments(["--pid", "41"]).target, "rsid")
        for target in S.TARGETS:
            self.assertEqual(S.arguments(["--pid", "41", "--target", target]).target, target)
        for bad in ("bash", "rsid-supervisor", "RSI", ""):
            with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as caught:
                S.arguments(["--pid", "41", "--target", bad])
            self.assertEqual(caught.exception.code, 2)

    def test_symbol_prefixes_match_only_at_a_crate_path_boundary(self):
        rsi = S.TARGETS["rsi"][1]
        self.assertTrue(S.is_rsid("rsi::event::run_event_loop", rsi))
        self.assertTrue(S.is_rsid("core::ptr::drop_in_place<rsi::app::App>", rsi))
        self.assertTrue(S.is_rsid("<rsi::app::App>::mark_dirty", rsi))
        for name in ("rsid::rpc", "rsid_store::bus", "xrsi::a", "other::rsi::a", "rsi_common::x"):
            self.assertFalse(S.is_rsid(name, rsi), name)
        mcp = S.TARGETS["rsi-agent-mcp"][1]
        self.assertTrue(S.is_rsid("rsi_agent_mcp::serve", mcp))
        self.assertTrue(S.is_rsid("rsi_common::agent_control::call", mcp))
        self.assertFalse(S.is_rsid("rsid::rpc", mcp))
        self.assertFalse(S.is_rsid("rsi::event", mcp))

    def test_symbols_and_counts_follow_the_selected_target(self):
        symbols = S.Symbols("00001040 00000020 t rsi::event::run\n"
                            "00001060 00000010 T rsi_common::rpc::call\n"
                            "00001080 00000010 t rsid::other\n",
                            [(0x701000, 0x702000, 0x1000)], 0x700000, S.TARGETS["rsi"][1])
        counts = S.Counts()
        payload = bytearray(sample_payload((0x701070,)))
        struct.pack_into("<Q", payload, 0, 0x701050)
        counts.consume((9, 2, payload), symbols, 41, 42, 4096)
        self.assertEqual(counts.leaf, {"rsi::event::run": 1})
        # rsi_common frames belong to rsi-agent-mcp's report, not rsi's.
        self.assertEqual(counts.inclusive, {"rsi::event::run": 1})
        with self.assertRaisesRegex(S.SamplerError, "no rsi symbols"):
            S.Symbols("00001000 00000020 t rsid::only", [], 0, S.TARGETS["rsi"][1])

    def test_rsid_reports_workspace_crates_and_sqlite_not_just_rsid(self):
        symbols = S.Symbols("00001040 00000020 t rsi_codegraph::blocks::migrate_v9_to_v10\n"
                            "00001060 00000010 t sqlite3VdbeExec\n"
                            "00001080 00000010 t rsid_store::Store::digest\n"
                            "000010a0 00000010 t rsid::main\n"
                            "000010c0 00000010 t libc_helper\n",
                            [(0x701000, 0x702000, 0x1000)], 0x700000,
                            S.TARGETS["rsid"][1], S.CONTEXT_PREFIXES["rsid"])
        counts = S.Counts()
        for leaf in (0x701050, 0x701068, 0x701050):
            payload = bytearray(sample_payload((0x7010a8,)))
            struct.pack_into("<Q", payload, 0, leaf)
            counts.consume((9, 2, payload), symbols, 41, 42, 4096)
        self.assertEqual(counts.leaf, {"rsi_codegraph::blocks::migrate_v9_to_v10": 2,
                                       "sqlite3VdbeExec": 1})
        self.assertEqual(counts.unmatched, 0)
        self.assertEqual(counts.inclusive["rsid::main"], 3)

    def test_unmatched_leaf_names_the_symbol_outside_every_reported_crate(self):
        symbols = S.Symbols("00001040 00000020 t libc_helper\n"
                            "00001060 00000010 t rsid::caller\n",
                            [(0x701000, 0x702000, 0x1000)], 0x700000,
                            S.TARGETS["rsid"][1], S.CONTEXT_PREFIXES["rsid"])
        counts = S.Counts()
        for leaf in (0x701050, 0x701050, 0x900000):
            payload = bytearray(sample_payload((0x701068,)))
            struct.pack_into("<Q", payload, 0, leaf)
            counts.consume((9, 2, payload), symbols, 41, 42, 4096)
        self.assertEqual(counts.unmatched, 3)
        self.assertEqual(counts.unmatched_leaf, {"libc_helper": 2, S.UNRESOLVED_LEAF: 1})

    def test_context_prefixes_do_not_satisfy_the_unstripped_check(self):
        with self.assertRaisesRegex(S.SamplerError, "no rsid/rsid_store symbols"):
            S.Symbols("00001000 00000020 t sqlite3VdbeExec", [], 0,
                      S.TARGETS["rsid"][1], S.CONTEXT_PREFIXES["rsid"])

    def test_rsi_target_report_is_not_widened(self):
        self.assertNotIn("rsi", S.CONTEXT_PREFIXES)

    def test_default_target_output_and_duration_limits_are_unchanged(self):
        args = S.arguments(["--pid", "41", "--target", "rsi", "--seconds", "30"])
        self.assertEqual((args.seconds, args.hz, args.threads), (30, 100, 3))
        for option, value in (("--seconds", "31"), ("--threads", "9"), ("--stack-bytes", "9000")):
            with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                S.arguments(["--pid", "41", "--target", "rsi-agent-mcp", option, value])


if __name__ == "__main__":
    unittest.main()
