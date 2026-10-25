//! Native menus built from the webview's action registry (`src/registry.ts`).
//!
//! The webview sends a menu model (submenus of action items, separators and
//! OS-provided items); this module validates it, builds it with Tauri v2's
//! menu API and forwards a chosen item's id back to the webview as a
//! `menu-action` event. Only `act:<action>` (menu bar) and `ctx:<action>`
//! (right-click) ids are accepted, so a menu event can only name a registry
//! action, never anything else.
//!
//! The bar installs menu by menu (#305): a top-level menu that breaks a cap
//! is logged and left out, and the rest still install; only a bar with no
//! menu that can be installed is refused.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use tauri::menu::{
    AboutMetadata, CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, MenuItemKind,
    PredefinedMenuItem, Submenu,
};
use tauri::{AppHandle, Emitter, Manager, Runtime, Window};

/// Event the webview listens to (`src/menus.ts`).
pub const MENU_EVENT_NAME: &str = "menu-action";
pub const APP_PREFIX: &str = "act";
pub const CONTEXT_PREFIX: &str = "ctx";

// Mirrored by MENU_LIMITS in src/registry.ts (tests/registry.test.mjs pins them).
const MAX_SUBMENUS: usize = 12;
const MAX_ITEMS: usize = 64;
const MAX_TEXT: usize = 96;
const MAX_ACTION_ID: usize = 48;
/// Menu levels: a top-level menu (1) and up to two levels of nested submenus.
const MAX_DEPTH: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predefined {
    About,
    Services,
    Hide,
    HideOthers,
    ShowAll,
    Quit,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Minimize,
    Maximize,
    Fullscreen,
    CloseWindow,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MenuNode {
    Item {
        id: String,
        text: String,
        enabled: bool,
        #[serde(default)]
        accelerator: Option<String>,
        /// Present = a check item with this tick.
        #[serde(default)]
        checked: Option<bool>,
    },
    Separator,
    Predefined {
        item: Predefined,
    },
    /// A nested submenu (menu bar only), e.g. View > Files.
    Submenu {
        text: String,
        items: Vec<MenuNode>,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SubmenuSpec {
    pub id: String,
    pub text: String,
    pub items: Vec<MenuNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct ItemState {
    pub enabled: bool,
    #[serde(default)]
    pub checked: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MenuActionEvent {
    pub id: String,
}

/// A top-level menu `set_app_menu` left out, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedMenu {
    pub id: String,
    pub reason: String,
}

/// What `set_app_menu` installed: the menu ids in the bar and the ones left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MenuInstallReport {
    pub installed: Vec<String>,
    pub skipped: Vec<SkippedMenu>,
}

/// `act:archive`, `ctx:copy-session-id`: the prefix, then 1..=48 of `[a-z0-9-]`.
pub fn is_action_id(id: &str, prefix: &str) -> bool {
    let Some(rest) = id.strip_prefix(prefix).and_then(|r| r.strip_prefix(':')) else {
        return false;
    };
    !rest.is_empty()
        && rest.len() <= MAX_ACTION_ID
        && rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Menu events the webview should hear about.
pub fn is_menu_action_id(id: &str) -> bool {
    is_action_id(id, APP_PREFIX) || is_action_id(id, CONTEXT_PREFIX)
}

fn check_text(what: &str, text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{what}: empty text"));
    }
    if text.chars().count() > MAX_TEXT {
        return Err(format!("{what}: text longer than {MAX_TEXT} characters"));
    }
    if text.chars().any(char::is_control) {
        return Err(format!("{what}: control character in text"));
    }
    // muda reads `&` as a mnemonic marker.
    if text.contains('&') {
        return Err(format!("{what}: `&` is not allowed in menu text"));
    }
    Ok(())
}

/// The only accelerator shape the registry emits: `CmdOrCtrl+` [`Shift+`] one
/// letter, digit or comma (Settings, Cmd-,).
pub fn valid_accelerator(accel: &str) -> bool {
    let Some(rest) = accel.strip_prefix("CmdOrCtrl+") else {
        return false;
    };
    let key = rest.strip_prefix("Shift+").unwrap_or(rest);
    key.len() == 1
        && key
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b',')
}

/// `depth` is this level's: 1 for a top-level menu or a right-click menu.
/// `app_bar`: predefined items and nested submenus are allowed (the menu bar).
fn validate_nodes(
    nodes: &[MenuNode],
    prefix: &str,
    app_bar: bool,
    depth: usize,
    seen: &mut HashSet<String>,
) -> Result<(), String> {
    if nodes.len() > MAX_ITEMS {
        return Err(format!("menu has more than {MAX_ITEMS} items"));
    }
    for node in nodes {
        match node {
            MenuNode::Item {
                id,
                text,
                accelerator,
                ..
            } => {
                if !is_action_id(id, prefix) {
                    return Err(format!("menu item id {id:?} is not `{prefix}:<action>`"));
                }
                if !seen.insert(id.clone()) {
                    return Err(format!("menu item id {id} appears twice"));
                }
                check_text(id, text)?;
                if let Some(a) = accelerator
                    && !valid_accelerator(a)
                {
                    return Err(format!("{id}: accelerator {a:?} is not CmdOrCtrl+<key>"));
                }
            }
            MenuNode::Separator => {}
            MenuNode::Predefined { item } => {
                if !app_bar {
                    return Err(format!("{item:?} is only allowed in the menu bar"));
                }
            }
            MenuNode::Submenu { text, items } => {
                if !app_bar {
                    return Err("nested submenus are only allowed in the menu bar".into());
                }
                if depth >= MAX_DEPTH {
                    return Err(format!("submenus nest deeper than {MAX_DEPTH} levels"));
                }
                check_text("submenu", text)?;
                if items.is_empty() {
                    return Err(format!("submenu {text:?} is empty"));
                }
                validate_nodes(items, prefix, app_bar, depth + 1, seen)
                    .map_err(|e| format!("{text}: {e}"))?;
            }
        }
    }
    Ok(())
}

/// One top-level menu. Its item ids join `seen` only when the whole menu is
/// valid, so a menu that is left out does not block the ids of a later one.
fn validate_submenu(sm: &SubmenuSpec, seen: &mut HashSet<String>) -> Result<(), String> {
    let id_ok =
        !sm.id.is_empty() && sm.id.len() <= 16 && sm.id.bytes().all(|b| b.is_ascii_lowercase());
    if !id_ok {
        return Err(format!("submenu id {:?} is not a lowercase word", sm.id));
    }
    check_text(&sm.id, &sm.text)?;
    let mut mine = seen.clone();
    validate_nodes(&sm.items, APP_PREFIX, true, 1, &mut mine)?;
    *seen = mine;
    Ok(())
}

/// Which top-level menus of the bar can be installed: the valid ones in order,
/// and each one left out with its reason. Refused only when the bar is empty,
/// has too many menus, or has no valid menu at all.
pub fn plan_app_menu(
    menu: &[SubmenuSpec],
) -> Result<(Vec<&SubmenuSpec>, Vec<SkippedMenu>), String> {
    if menu.is_empty() || menu.len() > MAX_SUBMENUS {
        return Err(format!("menu bar needs 1 to {MAX_SUBMENUS} submenus"));
    }
    let mut seen = HashSet::new();
    let mut subs = HashSet::new();
    let mut keep = Vec::new();
    let mut skipped = Vec::new();
    for sm in menu {
        let res = if subs.insert(sm.id.as_str()) {
            validate_submenu(sm, &mut seen)
        } else {
            Err(format!("submenu id {:?} appears twice", sm.id))
        };
        match res {
            Ok(()) => keep.push(sm),
            Err(reason) => skipped.push(SkippedMenu {
                id: sm.id.clone(),
                reason,
            }),
        }
    }
    if keep.is_empty() {
        let why: Vec<String> = skipped
            .iter()
            .map(|s| format!("{}: {}", s.id, s.reason))
            .collect();
        return Err(format!("no menu can be installed ({})", why.join("; ")));
    }
    Ok((keep, skipped))
}

/// The whole menu bar model, strictly: bounded, `act:` ids, unique ids, sane
/// text, every menu valid. `set_app_menu` itself installs what fits
/// (`plan_app_menu`).
pub fn validate_app_menu(menu: &[SubmenuSpec]) -> Result<(), String> {
    let (_, skipped) = plan_app_menu(menu)?;
    match skipped.first() {
        None => Ok(()),
        Some(s) => Err(format!("{}: {}", s.id, s.reason)),
    }
}

/// A right-click menu: `ctx:` items and separators only.
pub fn validate_context_menu(items: &[MenuNode]) -> Result<(), String> {
    if items.is_empty() {
        return Err("context menu is empty".into());
    }
    validate_nodes(items, CONTEXT_PREFIX, false, 1, &mut HashSet::new())
}

fn build_node<R: Runtime, M: Manager<R>>(
    m: &M,
    node: &MenuNode,
) -> tauri::Result<Box<dyn IsMenuItem<R>>> {
    Ok(match node {
        MenuNode::Item {
            id,
            text,
            enabled,
            accelerator,
            checked: Some(checked),
        } => Box::new(CheckMenuItem::with_id(
            m,
            id.as_str(),
            text,
            *enabled,
            *checked,
            accelerator.as_deref(),
        )?),
        MenuNode::Item {
            id,
            text,
            enabled,
            accelerator,
            checked: None,
        } => Box::new(MenuItem::with_id(
            m,
            id.as_str(),
            text,
            *enabled,
            accelerator.as_deref(),
        )?),
        MenuNode::Separator => Box::new(PredefinedMenuItem::separator(m)?),
        MenuNode::Predefined { item } => Box::new(predefined(m, *item)?),
        MenuNode::Submenu { text, items } => {
            let built = build_items(m, items)?;
            Box::new(Submenu::with_items(m, text, true, &refs(&built))?)
        }
    })
}

fn predefined<R: Runtime, M: Manager<R>>(
    m: &M,
    item: Predefined,
) -> tauri::Result<PredefinedMenuItem<R>> {
    match item {
        Predefined::About => {
            let info = m.package_info();
            let meta = AboutMetadata {
                name: Some(info.name.clone()),
                version: Some(info.version.to_string()),
                ..Default::default()
            };
            PredefinedMenuItem::about(m, None, Some(meta))
        }
        Predefined::Services => PredefinedMenuItem::services(m, None),
        Predefined::Hide => PredefinedMenuItem::hide(m, None),
        Predefined::HideOthers => PredefinedMenuItem::hide_others(m, None),
        Predefined::ShowAll => PredefinedMenuItem::show_all(m, None),
        Predefined::Quit => PredefinedMenuItem::quit(m, None),
        Predefined::Undo => PredefinedMenuItem::undo(m, None),
        Predefined::Redo => PredefinedMenuItem::redo(m, None),
        Predefined::Cut => PredefinedMenuItem::cut(m, None),
        Predefined::Copy => PredefinedMenuItem::copy(m, None),
        Predefined::Paste => PredefinedMenuItem::paste(m, None),
        Predefined::SelectAll => PredefinedMenuItem::select_all(m, None),
        Predefined::Minimize => PredefinedMenuItem::minimize(m, None),
        Predefined::Maximize => PredefinedMenuItem::maximize(m, None),
        Predefined::Fullscreen => PredefinedMenuItem::fullscreen(m, None),
        Predefined::CloseWindow => PredefinedMenuItem::close_window(m, None),
    }
}

fn build_items<R: Runtime, M: Manager<R>>(
    m: &M,
    nodes: &[MenuNode],
) -> tauri::Result<Vec<Box<dyn IsMenuItem<R>>>> {
    nodes.iter().map(|n| build_node(m, n)).collect()
}

fn refs<R: Runtime>(items: &[Box<dyn IsMenuItem<R>>]) -> Vec<&dyn IsMenuItem<R>> {
    items.iter().map(|b| b.as_ref()).collect()
}

/// The process-log line for a menu left out of the bar.
pub fn skipped_line(s: &SkippedMenu) -> String {
    format!(
        "rsi-desktop: menu {:?} not installed: {}",
        s.id,
        s.reason.replace(['\n', '\r'], " ")
    )
}

/// Install the menu bar (the macOS app menu; the window menu bar on Linux and
/// Windows), menu by menu: one that breaks a cap or that Tauri cannot build is
/// logged and left out, the others install. The report names both.
#[tauri::command]
pub async fn set_app_menu(
    app: AppHandle,
    menu: Vec<SubmenuSpec>,
) -> Result<MenuInstallReport, String> {
    let (keep, mut skipped) = plan_app_menu(&menu).inspect_err(|e| {
        eprintln!("rsi-desktop: menu bar not installed: {e}");
    })?;
    let bar = Menu::new(&app).map_err(|e| e.to_string())?;
    let mut installed = Vec::new();
    for sm in keep {
        let built = build_items(&app, &sm.items).and_then(|items| {
            Submenu::with_id_and_items(&app, sm.id.as_str(), &sm.text, true, &refs(&items))
        });
        match built.and_then(|sub| bar.append(&sub)) {
            Ok(()) => installed.push(sm.id.clone()),
            Err(e) => skipped.push(SkippedMenu {
                id: sm.id.clone(),
                reason: e.to_string(),
            }),
        }
    }
    for s in &skipped {
        eprintln!("{}", skipped_line(s));
    }
    if installed.is_empty() {
        return Err("no menu could be built".into());
    }
    app.set_menu(bar).map_err(|e| {
        eprintln!("rsi-desktop: menu bar not installed: {e}");
        e.to_string()
    })?;
    Ok(MenuInstallReport { installed, skipped })
}

/// Show a right-click menu at the cursor.
#[tauri::command]
pub async fn popup_context_menu(window: Window, items: Vec<MenuNode>) -> Result<(), String> {
    validate_context_menu(&items)?;
    let built = build_items(&window, &items).map_err(|e| e.to_string())?;
    let menu = Menu::with_items(&window, &refs(&built)).map_err(|e| e.to_string())?;
    window.popup_menu(&menu).map_err(|e| e.to_string())
}

fn apply_state<R: Runtime>(
    items: Vec<MenuItemKind<R>>,
    state: &HashMap<String, ItemState>,
) -> tauri::Result<()> {
    for item in items {
        match item {
            MenuItemKind::Submenu(sub) => apply_state(sub.items()?, state)?,
            MenuItemKind::MenuItem(it) => {
                if let Some(s) = state.get(it.id().as_ref()) {
                    it.set_enabled(s.enabled)?;
                }
            }
            MenuItemKind::Check(it) => {
                if let Some(s) = state.get(it.id().as_ref()) {
                    it.set_enabled(s.enabled)?;
                    if let Some(c) = s.checked {
                        it.set_checked(c)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Follow the webview's state: enable/disable items and set check ticks.
#[tauri::command]
pub async fn sync_menu_state(
    app: AppHandle,
    items: HashMap<String, ItemState>,
) -> Result<(), String> {
    if items.len() > MAX_SUBMENUS * MAX_ITEMS {
        return Err("too many menu items".into());
    }
    if items.keys().any(|k| !is_action_id(k, APP_PREFIX)) {
        return Err("menu state names an id that is not act:<action>".into());
    }
    let Some(menu) = app.menu() else {
        return Ok(());
    };
    let top = menu.items().map_err(|e| e.to_string())?;
    apply_state(top, &items).map_err(|e| e.to_string())
}

/// Quit the app (`ZQ`, `ZZ`, `:quit`, File > Quit on Linux and Windows).
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Global menu listener: forward registry item ids to the webview.
pub fn forward_menu_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    let id = event.id().as_ref();
    if is_menu_action_id(id) {
        let _ = app.emit(MENU_EVENT_NAME, MenuActionEvent { id: id.to_string() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_menu(v: serde_json::Value) -> Vec<SubmenuSpec> {
        serde_json::from_value(v).expect("menu model parses")
    }

    #[test]
    fn action_ids_are_prefixed_and_bounded() {
        assert!(is_action_id("act:archive", APP_PREFIX));
        assert!(is_action_id("ctx:copy-session-id", CONTEXT_PREFIX));
        assert!(!is_action_id("ctx:archive", APP_PREFIX));
        assert!(!is_action_id("act:", APP_PREFIX));
        assert!(!is_action_id("act:Archive", APP_PREFIX));
        assert!(!is_action_id("act:arch ive", APP_PREFIX));
        assert!(!is_action_id(
            &format!("act:{}", "a".repeat(49)),
            APP_PREFIX
        ));
        assert!(is_menu_action_id("act:quit"));
        // Tauri's own predefined items have opaque ids; they are not forwarded.
        assert!(!is_menu_action_id("1234"));
    }

    #[test]
    fn accelerators_are_cmd_or_ctrl_plus_one_key() {
        assert!(valid_accelerator("CmdOrCtrl+N"));
        assert!(valid_accelerator("CmdOrCtrl+Shift+K"));
        assert!(valid_accelerator("CmdOrCtrl+1"));
        assert!(valid_accelerator("CmdOrCtrl+,"));
        assert!(!valid_accelerator("CmdOrCtrl+;"));
        assert!(!valid_accelerator("Ctrl+N"));
        assert!(!valid_accelerator("CmdOrCtrl+n"));
        assert!(!valid_accelerator("CmdOrCtrl+F3"));
        assert!(!valid_accelerator("CmdOrCtrl+Alt+G"));
    }

    #[test]
    fn parses_the_registry_model_shape() {
        let menu = parse_menu(json!([
            { "id": "app", "text": "RSI Desktop", "items": [
                { "kind": "predefined", "item": "about" },
                { "kind": "separator" },
                { "kind": "predefined", "item": "hide_others" },
                { "kind": "predefined", "item": "quit" },
            ]},
            { "id": "file", "text": "File", "items": [
                { "kind": "item", "id": "act:launch", "text": "New session…", "enabled": true, "accelerator": "CmdOrCtrl+N" },
            ]},
            { "id": "view", "text": "View", "items": [
                { "kind": "item", "id": "act:toggle-vim-keys", "text": "Vim keys", "enabled": true, "checked": false },
            ]},
        ]));
        assert_eq!(
            menu[1].items[0],
            MenuNode::Item {
                id: "act:launch".into(),
                text: "New session…".into(),
                enabled: true,
                accelerator: Some("CmdOrCtrl+N".into()),
                checked: None,
            }
        );
        assert_eq!(
            menu[0].items[2],
            MenuNode::Predefined {
                item: Predefined::HideOthers
            }
        );
        validate_app_menu(&menu).unwrap();
    }

    #[test]
    fn refuses_bad_menu_bars() {
        let item = |id: &str, text: &str| json!({ "kind": "item", "id": id, "text": text, "enabled": true });
        let one = |items: serde_json::Value| {
            parse_menu(json!([{ "id": "file", "text": "File", "items": items }]))
        };
        assert!(validate_app_menu(&[]).is_err());
        assert!(validate_app_menu(&one(json!([item("ctx:launch", "New")]))).is_err());
        assert!(
            validate_app_menu(&one(json!([
                item("act:launch", "New"),
                item("act:launch", "Again")
            ])))
            .is_err()
        );
        assert!(validate_app_menu(&one(json!([item("act:launch", "  ")]))).is_err());
        assert!(validate_app_menu(&one(json!([item("act:launch", "Save & quit")]))).is_err());
        assert!(validate_app_menu(&one(json!([item("act:launch", "two\nlines")]))).is_err());
        let bad_accel = one(
            json!([{ "kind": "item", "id": "act:launch", "text": "New", "enabled": true, "accelerator": "Alt+F4" }]),
        );
        assert!(validate_app_menu(&bad_accel).is_err());
        let many: Vec<_> = (0..=MAX_ITEMS)
            .map(|i| item(&format!("act:a{i}"), "x"))
            .collect();
        assert!(validate_app_menu(&one(json!(many))).is_err());
        let dup_sub = parse_menu(json!([
            { "id": "file", "text": "File", "items": [] },
            { "id": "file", "text": "Again", "items": [] },
        ]));
        assert!(validate_app_menu(&dup_sub).is_err());
        assert!(serde_json::from_value::<Vec<SubmenuSpec>>(json!([
            { "id": "file", "text": "File", "items": [{ "kind": "predefined", "item": "format_disk" }] }
        ]))
        .is_err());
    }

    #[test]
    fn an_over_cap_menu_is_left_out_and_the_rest_install() {
        let item = |id: String| json!({ "kind": "item", "id": id, "text": "x", "enabled": true });
        let many: Vec<_> = (0..=MAX_ITEMS).map(|i| item(format!("act:v{i}"))).collect();
        let menu = parse_menu(json!([
            { "id": "file", "text": "File", "items": [item("act:launch".into())] },
            { "id": "view", "text": "View", "items": many },
            { "id": "help", "text": "Help", "items": [item("act:help".into())] },
        ]));
        let (keep, skipped) = plan_app_menu(&menu).expect("the bar still installs");
        let ids: Vec<&str> = keep.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["file", "help"]);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].id, "view");
        assert_eq!(
            skipped[0].reason,
            format!("menu has more than {MAX_ITEMS} items")
        );
        assert_eq!(
            skipped_line(&skipped[0]),
            format!(
                "rsi-desktop: menu \"view\" not installed: menu has more than {MAX_ITEMS} items"
            )
        );
        // The strict check still names the bad menu.
        assert!(validate_app_menu(&menu).unwrap_err().starts_with("view: "));
    }

    #[test]
    fn a_left_out_menu_does_not_claim_its_ids() {
        // View fails on its second item; File then reuses View's first id and still installs.
        let menu = parse_menu(json!([
            { "id": "view", "text": "View", "items": [
                { "kind": "item", "id": "act:launch", "text": "New", "enabled": true },
                { "kind": "item", "id": "act:bad", "text": "a & b", "enabled": true },
            ]},
            { "id": "file", "text": "File", "items": [
                { "kind": "item", "id": "act:launch", "text": "New", "enabled": true },
            ]},
            { "id": "file", "text": "File again", "items": [
                { "kind": "item", "id": "act:other", "text": "Other", "enabled": true },
            ]},
        ]));
        let (keep, skipped) = plan_app_menu(&menu).unwrap();
        assert_eq!(keep.len(), 1);
        assert_eq!(keep[0].id, "file");
        let reasons: Vec<&str> = skipped.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(reasons, ["view", "file"]);
        assert!(skipped[1].reason.contains("appears twice"));
    }

    #[test]
    fn a_bar_with_no_valid_menu_is_refused() {
        let menu = parse_menu(json!([
            { "id": "File", "text": "File", "items": [] },
        ]));
        let err = plan_app_menu(&menu).unwrap_err();
        assert!(err.starts_with("no menu can be installed"), "{err}");
        let too_many: Vec<_> = (0..=MAX_SUBMENUS)
            .map(|i| json!({ "id": format!("m{}", "x".repeat(i)), "text": "M", "items": [] }))
            .collect();
        assert!(plan_app_menu(&parse_menu(json!(too_many))).is_err());
    }

    #[test]
    fn nested_submenus_are_bounded() {
        let leaf = json!({ "kind": "item", "id": "act:files-save", "text": "Save file", "enabled": true, "accelerator": "CmdOrCtrl+S" });
        let nested = |items: serde_json::Value| json!({ "kind": "submenu", "text": "Files", "items": items });
        let bar = |items: serde_json::Value| {
            parse_menu(json!([{ "id": "view", "text": "View", "items": items }]))
        };
        validate_app_menu(&bar(json!([nested(json!([leaf.clone()]))]))).unwrap();
        // Two nested levels fit (3 in all); a third does not.
        validate_app_menu(&bar(json!([nested(json!([nested(json!(
            [leaf.clone()]
        ))]))])))
        .unwrap();
        assert!(
            validate_app_menu(&bar(json!([nested(json!([nested(json!([nested(
                json!([leaf.clone()])
            )]))]))])))
            .is_err()
        );
        assert!(validate_app_menu(&bar(json!([nested(json!([]))]))).is_err());
        // Ids stay unique across levels.
        assert!(
            validate_app_menu(&bar(json!([leaf.clone(), nested(json!([leaf.clone()]))]))).is_err()
        );
        // Each level has the item cap.
        let many: Vec<_> = (0..=MAX_ITEMS)
            .map(|i| json!({ "kind": "item", "id": format!("act:f{i}"), "text": "x", "enabled": true }))
            .collect();
        assert!(validate_app_menu(&bar(json!([nested(json!(many))]))).is_err());
        // Right-click menus stay flat.
        let ctx: Vec<MenuNode> = serde_json::from_value(json!([
            { "kind": "submenu", "text": "More", "items": [{ "kind": "item", "id": "ctx:archive", "text": "A", "enabled": true }] }
        ]))
        .unwrap();
        assert!(validate_context_menu(&ctx).is_err());
    }

    #[test]
    fn context_menus_take_ctx_items_and_separators_only() {
        let items: Vec<MenuNode> = serde_json::from_value(json!([
            { "kind": "item", "id": "ctx:archive", "text": "Archive session…", "enabled": false },
            { "kind": "separator" },
            { "kind": "item", "id": "ctx:copy-session-id", "text": "Copy session ID", "enabled": true },
        ]))
        .unwrap();
        validate_context_menu(&items).unwrap();
        assert!(validate_context_menu(&[]).is_err());
        let with_predef: Vec<MenuNode> =
            serde_json::from_value(json!([{ "kind": "predefined", "item": "quit" }])).unwrap();
        assert!(validate_context_menu(&with_predef).is_err());
        let app_id: Vec<MenuNode> = serde_json::from_value(
            json!([{ "kind": "item", "id": "act:archive", "text": "A", "enabled": true }]),
        )
        .unwrap();
        assert!(validate_context_menu(&app_id).is_err());
    }

    #[test]
    fn item_state_parses_with_optional_tick() {
        let s: HashMap<String, ItemState> = serde_json::from_value(json!({
            "act:archive": { "enabled": false },
            "act:toggle-tools": { "enabled": true, "checked": true },
        }))
        .unwrap();
        assert_eq!(
            s["act:archive"],
            ItemState {
                enabled: false,
                checked: None
            }
        );
        assert_eq!(s["act:toggle-tools"].checked, Some(true));
    }
}
