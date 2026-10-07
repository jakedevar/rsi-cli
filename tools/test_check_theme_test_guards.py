"""Regression coverage for the theme-test gate's scope and Rust literals."""
import unittest

from tools.check_theme_test_guards import test_bodies, unpinned


def findings(source):
    return [name for name, _, body in test_bodies(source) if unpinned(body)]


class ThemeTestGuards(unittest.TestCase):
    def test_render_and_projected_colors_need_a_pin(self):
        self.assertEqual(findings("""
            #[test] fn render_color() {
                terminal.draw(|f| render(f));
                assert_eq!(cell.fg, theme::green());
            }
            #[test] fn projected_color() {
                let row = compute_row();
                assert_eq!(row.color, crate::ui::theme::status_running());
            }
        """), ["render_color", "projected_color"])

    def test_pin_and_wrapper_cover_reads(self):
        self.assertEqual(findings("""
            #[test] fn pinned() {
                let _theme = crate::ui::theme::pin_theme_state();
                terminal.draw(|f| render(f));
                assert_eq!(cell.fg, theme::green());
            }
            #[test] fn wrapped() {
                theme::with_theme_state(|| {
                    theme::set_theme_by_name("goth");
                    assert_eq!(cell.fg, theme::green());
                });
            }
            #[test] fn internal_guard() {
                let _guard = theme_test_guard();
                set_theme_by_index(0);
            }
        """), [])

    def test_ineffective_pins_and_outside_wrapper_are_rejected(self):
        for pin in [
            "theme::pin_theme_state();",
            "let _ = theme::pin_theme_state();",
            "{ let _theme = theme::pin_theme_state(); }",
            "theme::with_theme_state(|| { render(); });",
            "// let _theme = theme::pin_theme_state();\n",
        ]:
            with self.subTest(pin=pin):
                self.assertEqual(findings(
                    "#[test] fn unpinned() {" + pin + "theme::green();}"
                ), ["unpinned"])
        self.assertEqual(findings("""
            #[test] fn too_late() {
                theme::green();
                let _theme = theme::pin_theme_state();
            }
        """), ["too_late"])

    def test_async_attributes_multiline_signature_and_unqualified_setter(self):
        self.assertEqual(findings("""
            #[tokio::test(flavor = "current_thread")]
            #[ignore]
            async fn changes_theme()
            {
                set_theme_by_name("goth");
            }
        """), ["changes_theme"])

    def test_ignores_literals_comments_and_non_tests(self):
        self.assertEqual(findings(r'''
            fn production() { theme::green(); }
            #[test] fn literals() {
                let raw = r##"} theme::green() " {"##;
                let escaped = "quote \" theme::green() \\
                               continuation }";
                let chars = ('}', '\u{7b}', '"');
                // theme::set_theme_by_name("goth"); }
                /* theme::green(); { */
            }
            #[test] fn next_test() { theme::green(); }
        '''), ["next_test"])

    def test_catalog_lookups_and_types_do_not_need_a_pin(self):
        self.assertEqual(findings("""
            #[test] fn metadata() {
                assert_eq!(theme::theme_count(), 14);
                theme::theme_display_name(0);
                let role = theme::ThemeRole::PrimaryText;
            }
        """), [])


if __name__ == "__main__":
    unittest.main()
