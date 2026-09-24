//! In-memory operator manual built only from the registries.
//!
//! Sources: `ACTION_DESCRIPTORS` (actions, keys, ex-commands),
//! `NORMAL_KEY_RESERVATIONS`, `OVERLAY_HELP_ROUTES` and
//! `OverlayHelpExemption::ALL`, `key_tables::KEY_TABLES`,
//! `UNTABULATED_KEY_SURFACES`, `FILE_VIEWER_COMMAND_DOCS`,
//! `settings_registry::SETTINGS` and the built-in theme table. The only
//! hand-written text here is `MANUAL_CHAPTERS` (titles and one intro
//! paragraph each) and the short captions that introduce generated tables.

use crate::action_registry::{
    ACTION_DESCRIPTORS, ActionDescriptor, ActionId, ActionRoute, CommandArgument, DocAnchor,
    NORMAL_KEY_RESERVATIONS, NormalReservationKind, OVERLAY_HELP_ROUTES, OverlayHelpClass,
    OverlayHelpExemption, OverlayHelpRoute, UNTABULATED_KEY_SURFACES,
};
use crate::settings_registry::{SETTINGS, SettingsGroup, SettingsSection};
use rsi_common::daemon_config_catalog::OperatorSurface;

/// First header of the no-TUI-editor gap table in the settings chapter.
const GAP_TABLE_FIRST_HEADER: &str = "Daemon field";

/// A whole manual: ordered chapters plus the typed Normal-mode binding list
/// the Normal key table was rendered from (the T6 cross-check input).
pub struct Manual {
    pub chapters: Vec<Chapter>,
    pub normal_bindings: Vec<(&'static str, ActionId)>,
}

pub struct Chapter {
    pub id: &'static str,
    pub title: &'static str,
    pub intro: &'static str,
    pub blocks: Vec<Block>,
}

pub enum Block {
    /// A section heading inside a chapter; `level` 0 is a section, 1 a
    /// subsection.
    Heading {
        level: u8,
        text: String,
    },
    Paragraph(String),
    Table(Table),
    /// A generated region: the same content is written between
    /// `rsi:generated` markers in `docs/keybindings.md`. Regions hold only
    /// paragraphs and tables so they never change that file's outline.
    Region {
        id: String,
        blocks: Vec<Self>,
    },
}

pub struct Table {
    pub headers: Vec<&'static str>,
    pub rows: Vec<Vec<String>>,
}

/// One manual chapter: title, intro, and the descriptor categories whose
/// actions it lists.
pub struct ManualChapter {
    pub id: &'static str,
    pub title: &'static str,
    pub intro: &'static str,
    pub categories: &'static [&'static str],
    /// Rendered only when at least one descriptor falls in its categories.
    pub only_when_nonempty: bool,
}

pub static MANUAL_CHAPTERS: &[ManualChapter] = &[
    ManualChapter {
        id: "getting-started",
        title: "Getting started",
        intro: "rsi is a keyboard-driven TUI over the rsid daemon, which runs and persists every agent session. Start the daemon, then the TUI. Press `?` for help on the focused view, `:` or `<Space>;` for the command palette, and `<Space>` as the leader for most commands. This manual is generated from the running build's registries, so what it lists is what the build does.",
        categories: &["DISCOVERY"],
        only_when_nonempty: false,
    },
    ManualChapter {
        id: "navigating",
        title: "Reading and navigating sessions",
        intro: "The session list has four zones (Main, TaskRabbit, Jobs, Archive), an attention queue for sessions waiting on you, a jumplist, tabs and splits, and in-place descent into Groups and Epics.",
        categories: &["NAVIGATION", "SESSION LIST"],
        only_when_nonempty: false,
    },
    ManualChapter {
        id: "inside-a-session",
        title: "Working inside a session",
        intro: "A session's detail view shows its transcript above an input bar. These actions type into the session, fold and filter events, and inspect it.",
        categories: &["TRANSCRIPT", "CURRENT VIEW"],
        only_when_nonempty: false,
    },
    ManualChapter {
        id: "files-and-prompts",
        title: "Files and prompts",
        intro: "The file explorer, fuzzy finder, recent files, git panel and prompt creator. The file viewer's `:` forms are listed in the key reference.",
        categories: &["FILES"],
        only_when_nonempty: false,
    },
    ManualChapter {
        id: "hierarchy",
        title: "Hierarchy, projects and cards",
        intro: "Groups contain Epics; Epics contain Story, Task and Bug leaves. Projects scope sessions, and cards hold entity facts.",
        categories: &["HIERARCHY", "PROJECT"],
        only_when_nonempty: false,
    },
    ManualChapter {
        id: "manager",
        title: "Orchestration and the manager",
        intro: "The harness manager coordinates Epic leads under an operator-owned scope and policy.",
        categories: &["MANAGER"],
        only_when_nonempty: false,
    },
    ManualChapter {
        id: "issues-and-scheduling",
        title: "Issues and scheduling",
        intro: "The Issues workspace tracks project issues; the scheduled-jobs browser runs recurring prompts.",
        categories: &["ISSUES", "SCHEDULED JOBS"],
        only_when_nonempty: false,
    },
    ManualChapter {
        id: "lifecycle",
        title: "Lifecycle and housekeeping",
        intro: "Launch, continue, interrupt, archive, delete, rotate and retry sessions, stop all spend, and manage panes and tabs.",
        categories: &["SESSION", "WINDOW"],
        only_when_nonempty: false,
    },
    ManualChapter {
        id: "tools",
        title: "Diagnostics and tools",
        intro: "Operator views: diagnostics, memory search, graph review, the recursive DAG browser, notifications and questions.",
        categories: &["OPERATOR VIEWS"],
        only_when_nonempty: false,
    },
    ManualChapter {
        id: "code-intelligence",
        title: "Code intelligence",
        intro: "Code-intelligence views and controls.",
        categories: &["CODE INTELLIGENCE"],
        only_when_nonempty: true,
    },
    ManualChapter {
        id: "theming",
        title: "Theming",
        intro: "Pick a built-in theme, override individual color roles, and edit the legacy message and editor colors.",
        categories: &["THEME & COLORS", "THEME ROLE"],
        only_when_nonempty: false,
    },
    ManualChapter {
        id: "settings",
        title: "Settings reference",
        intro: "Every settings row by group and section: what it does, what kind of value it is, where the value lives, and when a change applies.",
        categories: &["SETTINGS"],
        only_when_nonempty: false,
    },
    ManualChapter {
        id: "ex-commands",
        title: "Ex-command reference",
        intro: "Every `:` command, its aliases and argument form. Type a unique alias and Enter, or pick it from the palette.",
        categories: &[],
        only_when_nonempty: false,
    },
    ManualChapter {
        id: "key-reference",
        title: "Key reference by context",
        intro: "Every key, grouped by the context that decodes it: the Normal-mode keymap, registry surfaces, raw keys the event loop decodes itself, every overlay, and the surfaces documented by hand.",
        categories: &[],
        only_when_nonempty: false,
    },
];

/// The chapter that lists a descriptor category, if any.
#[must_use]
pub fn chapter_for_category(category: &str) -> Option<&'static ManualChapter> {
    MANUAL_CHAPTERS
        .iter()
        .find(|chapter| chapter.categories.contains(&category))
}

/// The topical chapter an overlay class belongs to. Exhaustive: a new class
/// does not compile until it is given a chapter.
#[must_use]
pub const fn chapter_for_overlay_class(class: OverlayHelpClass) -> &'static str {
    use OverlayHelpClass as C;
    match class {
        C::SortPicker | C::Notifications | C::RecentCompletions | C::TrashBrowser => "navigating",
        C::CreateEntityNormal
        | C::CreateEntityInsert
        | C::CreateEntityBody
        | C::CreateEntityTopology
        | C::ParentPicker
        | C::ProjectPicker
        | C::ProjectForm
        | C::LabelPicker
        | C::LabelForm
        | C::CardEditor => "hierarchy",
        C::FileExplorer
        | C::FileExplorerFinder
        | C::FileExplorerViewerFocus
        | C::FileViewer
        | C::Telescope
        | C::PromptPreview => "files-and-prompts",
        C::ManagerScope
        | C::ManagerBoard
        | C::ManagerDecisions
        | C::ManagerPolicy
        | C::ManagerTextEntry => "manager",
        C::ScheduleForm => "issues-and-scheduling",
        C::RenameSession
        | C::Rating
        | C::SessionInfo
        | C::QuestionNormal
        | C::QuestionInsert
        | C::InputModal
        | C::ModelPicker
        | C::AiCommand
        | C::AiChat => "inside-a-session",
        C::Terminal | C::CommandPalette | C::SettlementBrowser => "lifecycle",
        C::GraphNavigate
        | C::GraphEmpty
        | C::GraphDetail
        | C::GraphEditField
        | C::GraphPicker
        | C::DagBrowser
        | C::Diagnostics
        | C::MemorySearch
        | C::Dialectic => "tools",
        C::ThemePicker | C::ColorCustomizer | C::TextAreaBgEditor => "theming",
        C::ProviderForm
        | C::MessageBridgeForm
        | C::HookForm
        | C::HookConflict
        | C::BudgetPolicyForm
        | C::SkillPreview => "settings",
    }
}

/// Region id of an overlay route's key table.
#[must_use]
pub fn overlay_region_id(route: &OverlayHelpRoute) -> String {
    let mut slug = String::from("overlay-");
    let mut dash = false;
    for c in route.title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !slug.ends_with('-') {
            slug.push('-');
            dash = true;
        }
    }
    slug.trim_end_matches('-').to_string()
}

/// Region ids that are not overlay routes, in manual order.
pub const FIXED_REGION_IDS: &[&str] = &[
    "themes",
    "command-mode",
    "normal",
    "settings",
    "issue-tracker",
    "schedule-browser",
    "theme-role-editor",
    "raw",
    "file-viewer",
    "overlay-exemptions",
    "untabulated",
];

/// Inline code span that survives embedded backticks.
#[must_use]
pub fn code(text: &str) -> String {
    if text.contains('`') {
        format!("`` {text} ``")
    } else {
        format!("`{text}`")
    }
}

const fn route_label(route: ActionRoute) -> &'static str {
    match route {
        ActionRoute::Normal => "Normal",
        ActionRoute::Settings => "Settings",
        ActionRoute::IssueTracker => "Issues",
        ActionRoute::ScheduleBrowser => "Scheduled jobs",
        ActionRoute::ThemeRoleEditor => "Theme role editor",
    }
}

const ROUTES: [ActionRoute; 5] = [
    ActionRoute::Normal,
    ActionRoute::Settings,
    ActionRoute::IssueTracker,
    ActionRoute::ScheduleBrowser,
    ActionRoute::ThemeRoleEditor,
];

/// Every key of a descriptor, each route after Normal tagged with its
/// context.
fn keys_cell(descriptor: &ActionDescriptor) -> String {
    let mut parts = Vec::new();
    for route in ROUTES {
        let keys: Vec<String> = descriptor
            .bindings
            .iter()
            .filter(|binding| binding.route == route)
            .map(|binding| code(binding.sequence))
            .collect();
        if keys.is_empty() {
            continue;
        }
        if route == ActionRoute::Normal {
            parts.push(keys.join(" "));
        } else {
            parts.push(format!("{}: {}", route_label(route), keys.join(" ")));
        }
    }
    parts.join(" · ")
}

fn command_forms(descriptor: &ActionDescriptor) -> Vec<String> {
    let suffix = match descriptor.command_argument {
        CommandArgument::None => "",
        CommandArgument::Optional => " [arg]",
        CommandArgument::Required => " <arg>",
    };
    descriptor
        .command_aliases
        .iter()
        .map(|alias| code(&format!(":{alias}{suffix}")))
        .collect()
}

fn descriptor_table(descriptors: &[&ActionDescriptor]) -> Table {
    Table {
        headers: vec!["Keys", "Command", "Action", "What it does"],
        rows: descriptors
            .iter()
            .map(|descriptor| {
                vec![
                    keys_cell(descriptor),
                    command_forms(descriptor).join(" "),
                    descriptor.label.to_string(),
                    descriptor.summary.to_string(),
                ]
            })
            .collect(),
    }
}

fn route_region(id: &str, route: ActionRoute, caption: &str) -> Block {
    let rows = ACTION_DESCRIPTORS
        .iter()
        .flat_map(|descriptor| {
            descriptor
                .bindings
                .iter()
                .filter(move |binding| binding.route == route)
                .map(move |binding| {
                    vec![
                        code(binding.sequence),
                        descriptor.label.to_string(),
                        descriptor.summary.to_string(),
                    ]
                })
        })
        .collect();
    Block::Region {
        id: id.to_string(),
        blocks: vec![
            Block::Paragraph(caption.to_string()),
            Block::Table(Table {
                headers: vec!["Keys", "Action", "What it does"],
                rows,
            }),
        ],
    }
}

fn doc_anchor_text(anchor: DocAnchor) -> String {
    match anchor {
        DocAnchor::GeneratedRegion(id) => format!("generated table `{id}`"),
        DocAnchor::Narrative(heading) => format!("keybindings.md § {heading}"),
    }
}

fn themes_region() -> Block {
    let count = crate::ui::theme::theme_count();
    Block::Region {
        id: "themes".to_string(),
        blocks: vec![
            Block::Paragraph(format!(
                "rsi ships {count} built-in themes, in picker order. `T` opens the picker with a live preview; `Esc` restores the theme that was active when it opened."
            )),
            Block::Table(Table {
                headers: vec!["#", "Theme"],
                rows: (0..count)
                    .map(|index| {
                        vec![
                            (index + 1).to_string(),
                            crate::ui::theme::theme_display_name(index).to_string(),
                        ]
                    })
                    .collect(),
            }),
        ],
    }
}

fn settings_blocks() -> Vec<Block> {
    let mut blocks = Vec::new();
    for group in SettingsGroup::ALL {
        blocks.push(Block::Heading {
            level: 0,
            text: format!("{} — {}", group.label(), group.summary()),
        });
        for section in SettingsSection::ALL
            .iter()
            .filter(|section| section.group() == *group)
        {
            blocks.push(Block::Heading {
                level: 1,
                text: section.label().to_string(),
            });
            blocks.push(Block::Paragraph(section.summary().to_string()));
            blocks.push(Block::Table(Table {
                headers: vec!["Setting", "What it does", "Kind", "Stored in", "Applies"],
                rows: SETTINGS
                    .iter()
                    .filter(|spec| spec.section == *section)
                    .map(|spec| {
                        let mut what = spec.summary.to_string();
                        if let Some(detail) = spec.detail {
                            what.push(' ');
                            what.push_str(detail);
                        }
                        if spec.destructive {
                            what.push_str(" **Destructive.**");
                        }
                        vec![
                            spec.label.to_string(),
                            what,
                            spec.kind.label().to_string(),
                            spec.owner.label(),
                            spec.apply.label().to_string(),
                        ]
                    })
                    .collect(),
            }));
        }
    }
    let gaps: Vec<Vec<String>> = rsi_common::daemon_config_catalog::DAEMON_CONFIG_FIELDS
        .iter()
        .filter_map(|spec| match spec.operator_surface {
            OperatorSurface::NoTuiEditor { set_via, tracking } => Some(vec![
                code(spec.field),
                set_via.to_string(),
                tracking.to_string(),
            ]),
            OperatorSurface::SettingsPage | OperatorSurface::Elsewhere { .. } => None,
        })
        .collect();
    blocks.push(Block::Heading {
        level: 0,
        text: "Persisted daemon settings without a TUI editor".to_string(),
    });
    blocks.push(Block::Paragraph(
        "These daemon settings are persisted and read by the daemon, but no TUI surface edits them yet. This is how each is set today.".to_string(),
    ));
    blocks.push(Block::Table(Table {
        headers: vec![GAP_TABLE_FIRST_HEADER, "How it is set today", "Tracking"],
        rows: gaps,
    }));
    blocks
}

fn command_region() -> Block {
    let rows = ACTION_DESCRIPTORS
        .iter()
        .filter(|descriptor| !descriptor.command_aliases.is_empty())
        .map(|descriptor| {
            let forms = command_forms(descriptor);
            vec![
                forms[0].clone(),
                forms[1..].join(" "),
                descriptor.label.to_string(),
                descriptor.summary.to_string(),
            ]
        })
        .collect();
    Block::Region {
        id: "command-mode".to_string(),
        blocks: vec![
            Block::Paragraph(
                "`[arg]` marks an optional argument and `<arg>` a required one. Window and tab commands are handled by the pane's own window commands.".to_string(),
            ),
            Block::Table(Table {
                headers: vec!["Command", "Aliases", "Action", "What it does"],
                rows,
            }),
        ],
    }
}

fn normal_region(normal_bindings: &mut Vec<(&'static str, ActionId)>) -> Block {
    let mut rows = Vec::new();
    for descriptor in ACTION_DESCRIPTORS {
        for binding in descriptor
            .bindings
            .iter()
            .filter(|binding| binding.route == ActionRoute::Normal)
        {
            normal_bindings.push((binding.sequence, descriptor.id));
            rows.push(vec![
                code(binding.sequence),
                descriptor.label.to_string(),
                descriptor.summary.to_string(),
            ]);
        }
    }
    let reserved = NORMAL_KEY_RESERVATIONS
        .iter()
        .map(|reservation| {
            vec![
                code(reservation.sequence),
                match reservation.kind {
                    NormalReservationKind::Prefix => "prefix".to_string(),
                    NormalReservationKind::Inert => "no-op".to_string(),
                },
                reservation.reason.to_string(),
            ]
        })
        .collect();
    Block::Region {
        id: "normal".to_string(),
        blocks: vec![
            Block::Paragraph(
                "Every Normal-mode chord, from the action registry. Vim motions such as `j`, `k`, `gg`, `G` and counts come from modalkit.".to_string(),
            ),
            Block::Table(Table {
                headers: vec!["Keys", "Action", "What it does"],
                rows,
            }),
            Block::Paragraph(
                "Reserved sequences: prefixes wait for the next key; no-ops keep retired chords from falling through to another key's action.".to_string(),
            ),
            Block::Table(Table {
                headers: vec!["Keys", "Reserved as", "Why"],
                rows: reserved,
            }),
        ],
    }
}

fn raw_region() -> Block {
    let mut blocks = vec![Block::Paragraph(
        "Keys the event loop decodes itself, before or beside the Vim keymap. Each table is consulted in order; the first row whose context holds wins.".to_string(),
    )];
    for table in crate::key_tables::KEY_TABLES {
        blocks.push(Block::Paragraph(format!(
            "**{}** — {}",
            table.title, table.summary
        )));
        blocks.push(Block::Table(Table {
            headers: vec!["Chord", "Context", "Effect"],
            rows: (table.rows)()
                .into_iter()
                .map(|row| {
                    vec![
                        code(&row.chord),
                        row.context.to_string(),
                        row.effect.to_string(),
                    ]
                })
                .collect(),
        }));
    }
    Block::Region {
        id: "raw".to_string(),
        blocks,
    }
}

fn file_viewer_region() -> Block {
    Block::Region {
        id: "file-viewer".to_string(),
        blocks: vec![
            Block::Paragraph(
                "The file viewer's own `:` command line (`:q` closes the viewer, not rsi)."
                    .to_string(),
            ),
            Block::Table(Table {
                headers: vec!["Command", "What it does"],
                rows: crate::file_viewer_commands::FILE_VIEWER_COMMAND_DOCS
                    .iter()
                    .map(|doc| vec![code(&format!(":{}", doc.example)), doc.summary.to_string()])
                    .collect(),
            }),
        ],
    }
}

fn overlay_blocks() -> Vec<Block> {
    let mut blocks = Vec::new();
    for chapter in MANUAL_CHAPTERS {
        let routes: Vec<&OverlayHelpRoute> = OVERLAY_HELP_ROUTES
            .iter()
            .filter(|route| chapter_for_overlay_class(route.class) == chapter.id)
            .collect();
        if routes.is_empty() {
            continue;
        }
        blocks.push(Block::Heading {
            level: 0,
            text: format!("Overlays: {}", chapter.title),
        });
        for route in routes {
            blocks.push(Block::Heading {
                level: 1,
                text: route.title.to_string(),
            });
            blocks.push(Block::Region {
                id: overlay_region_id(route),
                blocks: vec![Block::Table(Table {
                    headers: vec!["Keys", "Action"],
                    rows: route
                        .entries()
                        .map(|entry| vec![code(entry.keys), entry.label.to_string()])
                        .collect(),
                })],
            });
        }
    }
    blocks
}

fn exemptions_region() -> Block {
    Block::Region {
        id: "overlay-exemptions".to_string(),
        blocks: vec![
            Block::Paragraph(
                "Screens without an overlay key catalog, and where their keys are documented."
                    .to_string(),
            ),
            Block::Table(Table {
                headers: vec!["State", "Reason", "Keys documented at"],
                rows: OverlayHelpExemption::ALL
                    .iter()
                    .map(|exemption| {
                        vec![
                            exemption.state().to_string(),
                            exemption.reason().to_string(),
                            doc_anchor_text(exemption.documented_by()),
                        ]
                    })
                    .collect(),
            }),
        ],
    }
}

fn untabulated_region() -> Block {
    Block::Region {
        id: "untabulated".to_string(),
        blocks: vec![
            Block::Paragraph(
                "These surfaces decode keys in hand-written handlers. Their keys are not generated; see the named `docs/keybindings.md` section.".to_string(),
            ),
            Block::Table(Table {
                headers: vec!["Surface", "Why it is not generated", "Documented at"],
                rows: UNTABULATED_KEY_SURFACES
                    .iter()
                    .map(|surface| {
                        vec![
                            surface.surface.to_string(),
                            surface.reason.to_string(),
                            format!("keybindings.md § {}", surface.narrative_anchor),
                        ]
                    })
                    .collect(),
            }),
        ],
    }
}

fn key_reference_blocks(normal_bindings: &mut Vec<(&'static str, ActionId)>) -> Vec<Block> {
    let mut blocks = vec![
        Block::Heading {
            level: 0,
            text: "Normal mode".to_string(),
        },
        normal_region(normal_bindings),
        Block::Heading {
            level: 0,
            text: "Settings pane".to_string(),
        },
        route_region(
            "settings",
            ActionRoute::Settings,
            "Keys of the settings pane (category rail and items).",
        ),
        Block::Heading {
            level: 0,
            text: "Issues workspace".to_string(),
        },
        route_region(
            "issue-tracker",
            ActionRoute::IssueTracker,
            "Registry keys of the Issues workspace; some apply only in the tab or mode the action names.",
        ),
        Block::Heading {
            level: 0,
            text: "Scheduled jobs".to_string(),
        },
        route_region(
            "schedule-browser",
            ActionRoute::ScheduleBrowser,
            "Keys of the scheduled jobs browser.",
        ),
        Block::Heading {
            level: 0,
            text: "Theme role editor".to_string(),
        },
        route_region(
            "theme-role-editor",
            ActionRoute::ThemeRoleEditor,
            "Keys of the theme role editor.",
        ),
        Block::Heading {
            level: 0,
            text: "Raw keys".to_string(),
        },
        raw_region(),
        Block::Heading {
            level: 0,
            text: "File viewer commands".to_string(),
        },
        file_viewer_region(),
    ];
    blocks.extend(overlay_blocks());
    blocks.push(Block::Heading {
        level: 0,
        text: "Screens without an overlay catalog".to_string(),
    });
    blocks.push(exemptions_region());
    blocks.push(Block::Heading {
        level: 0,
        text: "Surfaces documented by hand".to_string(),
    });
    blocks.push(untabulated_region());
    blocks
}

/// Build the manual from the registries.
#[must_use]
pub fn build_manual() -> Manual {
    let mut normal_bindings = Vec::new();
    let mut chapters = Vec::new();
    for chapter in MANUAL_CHAPTERS {
        let descriptors: Vec<&ActionDescriptor> = ACTION_DESCRIPTORS
            .iter()
            .filter(|descriptor| chapter.categories.contains(&descriptor.category))
            .collect();
        if chapter.only_when_nonempty && descriptors.is_empty() {
            continue;
        }
        let mut blocks = Vec::new();
        for category in chapter.categories {
            let in_category: Vec<&ActionDescriptor> = descriptors
                .iter()
                .copied()
                .filter(|descriptor| descriptor.category == *category)
                .collect();
            if in_category.is_empty() {
                continue;
            }
            blocks.push(Block::Heading {
                level: 0,
                text: title_case(category),
            });
            blocks.push(Block::Table(descriptor_table(&in_category)));
        }
        match chapter.id {
            "theming" => blocks.insert(0, themes_region()),
            "settings" => blocks.extend(settings_blocks()),
            "ex-commands" => blocks.push(command_region()),
            "key-reference" => blocks.extend(key_reference_blocks(&mut normal_bindings)),
            _ => {}
        }
        chapters.push(Chapter {
            id: chapter.id,
            title: chapter.title,
            intro: chapter.intro,
            blocks,
        });
    }
    Manual {
        chapters,
        normal_bindings,
    }
}

fn title_case(category: &str) -> String {
    category
        .split(' ')
        .map(|word| {
            if word == "&" {
                return word.to_string();
            }
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_string() + &chars.as_str().to_ascii_lowercase()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Manual {
    /// Every generated region, in manual order.
    #[must_use]
    pub fn regions(&self) -> Vec<(&str, &[Block])> {
        fn walk<'a>(blocks: &'a [Block], out: &mut Vec<(&'a str, &'a [Block])>) {
            for block in blocks {
                if let Block::Region { id, blocks } = block {
                    out.push((id.as_str(), blocks.as_slice()));
                    walk(blocks, out);
                }
            }
        }
        let mut out = Vec::new();
        for chapter in &self.chapters {
            walk(&chapter.blocks, &mut out);
        }
        out
    }

    /// The first table of a region.
    #[must_use]
    pub fn region_tables(&self, id: &str) -> Vec<&Table> {
        self.regions()
            .into_iter()
            .filter(|(region, _)| *region == id)
            .flat_map(|(_, blocks)| blocks.iter())
            .filter_map(|block| match block {
                Block::Table(table) => Some(table),
                _ => None,
            })
            .collect()
    }

    /// Rows of the settings chapter's "without a TUI editor" gap table.
    #[must_use]
    pub fn daemon_field_gap_rows(&self) -> &[Vec<String>] {
        self.chapters
            .iter()
            .filter(|chapter| chapter.id == "settings")
            .flat_map(|chapter| chapter.blocks.iter())
            .find_map(|block| match block {
                Block::Table(table) if table.headers.first() == Some(&GAP_TABLE_FIRST_HEADER) => {
                    Some(table.rows.as_slice())
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Every heading text, in order.
    #[must_use]
    pub fn headings(&self) -> Vec<&str> {
        self.chapters
            .iter()
            .flat_map(|chapter| chapter.blocks.iter())
            .filter_map(|block| match block {
                Block::Heading { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}
