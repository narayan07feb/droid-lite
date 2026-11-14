//! Keyboard shortcuts as data: one table per keymap (Eclipse or Android Studio, both macOS flavour).

use egui::{Key, KeyboardShortcut, ModifierNames, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keymap {
    Eclipse,
    AndroidStudio,
}

impl Keymap {
    pub fn name(self) -> &'static str {
        match self {
            Keymap::Eclipse => "Eclipse (macOS)",
            Keymap::AndroidStudio => "Android Studio (macOS)",
        }
    }

    pub fn from_name(s: &str) -> Keymap {
        if s.trim() == Keymap::AndroidStudio.name() {
            Keymap::AndroidStudio
        } else {
            Keymap::Eclipse
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    GotoClass,
    GotoFile,
    GotoSymbol,
    Everywhere,
    Recent,
    Structure,
    GotoLine,
    GotoDefinition,
    References,
    Implementations,
    FindInFiles,
    Find,
    Replace,
    FindNext,
    FindPrev,
    Build,
    Settings,
    LocalHistory,
    LocateFile,
    Back,
    Forward,
    Complete,
    Rename,
    ExtractVariable,
    OptimizeImports,
    ToggleComment,
    DeleteLine,
    DuplicateLine,
    MoveLineUp,
    MoveLineDown,
    Save,
    CloseTab,
    Shortcuts,
    NewWindow,
    Palette,
    ToggleProject,
    ToggleRight,
    ToggleBottom,
    FocusMode,
    RunApp,
    RunTask,
    QuickFix,
    NewFile,
    Translations,
    DeepLink,
}

/// Every action, in the order the action palette lists them with an empty query.
pub const ALL: &[Action] = {
    use Action::*;
    &[
        RunApp,
        RunTask,
        Build,
        QuickFix,
        NewFile,
        Translations,
        DeepLink,
        Palette,
        Everywhere,
        GotoClass,
        GotoFile,
        GotoSymbol,
        Recent,
        Structure,
        GotoLine,
        GotoDefinition,
        References,
        Implementations,
        FindInFiles,
        Find,
        Replace,
        FindNext,
        FindPrev,
        Rename,
        ExtractVariable,
        OptimizeImports,
        ToggleComment,
        DeleteLine,
        DuplicateLine,
        MoveLineUp,
        MoveLineDown,
        Complete,
        Back,
        Forward,
        LocateFile,
        LocalHistory,
        ToggleProject,
        ToggleRight,
        ToggleBottom,
        FocusMode,
        Save,
        CloseTab,
        NewWindow,
        Settings,
        Shortcuts,
    ]
};

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::GotoClass => "Open type / class",
            Action::GotoFile => "Open file",
            Action::GotoSymbol => "Open symbol",
            Action::Everywhere => "Search everywhere",
            Action::Recent => "Recent files",
            Action::Structure => "File outline",
            Action::GotoLine => "Go to line",
            Action::GotoDefinition => "Go to declaration",
            Action::References => "Find references",
            Action::Implementations => "Go to implementations",
            Action::FindInFiles => "Find in files",
            Action::Find => "Find in file",
            Action::Replace => "Replace in file",
            Action::FindNext => "Find next",
            Action::FindPrev => "Find previous",
            Action::Build => "Build module (show compile errors)",
            Action::Settings => "Settings",
            Action::LocalHistory => "Local history of this file",
            Action::LocateFile => "Locate file in the Project tree",
            Action::Back => "Back",
            Action::Forward => "Forward",
            Action::Complete => "Autocomplete",
            Action::Rename => "Rename",
            Action::ExtractVariable => "Extract variable",
            Action::OptimizeImports => "Organize imports",
            Action::ToggleComment => "Comment / uncomment lines",
            Action::DeleteLine => "Delete line",
            Action::DuplicateLine => "Duplicate line",
            Action::MoveLineUp => "Move line up",
            Action::MoveLineDown => "Move line down",
            Action::Save => "Save",
            Action::CloseTab => "Close tab",
            Action::Shortcuts => "Keyboard shortcuts",
            Action::NewWindow => "New window (open a project)",
            Action::Palette => "Find action",
            Action::ToggleProject => "Show / hide the Project panel",
            Action::ToggleRight => "Show / hide the Compose panel",
            Action::ToggleBottom => "Show / hide the bottom panel",
            Action::FocusMode => "Focus mode (hide all panels)",
            Action::RunApp => "Run the app on the device",
            Action::RunTask => "Run the Gradle task",
            Action::QuickFix => "Quick fix (show fixes for this line)",
            Action::NewFile => "New Kotlin file (Composable, ViewModel, Activity…)",
            Action::Translations => "Translations (strings in every language)",
            Action::DeepLink => "Open a deep link on the device",
        }
    }
}

const CMD: Modifiers = Modifiers::COMMAND;
const SHIFT: Modifiers = Modifiers::SHIFT;
const ALT: Modifiers = Modifiers::ALT;
const CTRL: Modifiers = Modifiers::CTRL;

fn sc(m: Modifiers, k: Key) -> KeyboardShortcut {
    KeyboardShortcut::new(m, k)
}

/// All bindings of a keymap, most specific (most modifiers) first, as egui's shortcut matching needs.
pub fn bindings(km: Keymap) -> Vec<(Action, KeyboardShortcut)> {
    use Action::*;
    let mut v = match km {
        Keymap::Eclipse => vec![
            (GotoClass, sc(CMD | SHIFT, Key::T)),
            (GotoFile, sc(CMD | SHIFT, Key::R)),
            (GotoSymbol, sc(CMD | ALT, Key::O)),
            (Everywhere, sc(CMD, Key::Num3)),
            (Recent, sc(CMD, Key::E)),
            (Structure, sc(CMD, Key::O)),
            (GotoLine, sc(CMD, Key::L)),
            (GotoDefinition, sc(Modifiers::NONE, Key::F3)),
            (References, sc(CMD | SHIFT, Key::G)),
            (References, sc(ALT, Key::F7)), // Android Studio habit, harmless here
            (Find, sc(CMD, Key::F)),
            (FindNext, sc(CMD, Key::K)),
            (FindPrev, sc(CMD | SHIFT, Key::K)),
            (Build, sc(CMD, Key::B)),
            (Implementations, sc(CMD, Key::T)),
            (FindInFiles, sc(CTRL, Key::H)),
            (Back, sc(CMD, Key::OpenBracket)),
            (Forward, sc(CMD, Key::CloseBracket)),
            (Complete, sc(CTRL, Key::Space)),
            (Rename, sc(CMD | ALT, Key::R)),
            (ExtractVariable, sc(CMD | ALT, Key::L)),
            (OptimizeImports, sc(CMD | SHIFT, Key::O)),
            (ToggleComment, sc(CMD, Key::Slash)),
            (DeleteLine, sc(CMD, Key::D)),
            (DuplicateLine, sc(CMD | ALT, Key::ArrowDown)),
            (MoveLineUp, sc(ALT, Key::ArrowUp)),
            (MoveLineDown, sc(ALT, Key::ArrowDown)),
            (Save, sc(CMD, Key::S)),
            (CloseTab, sc(CMD, Key::W)),
            (Shortcuts, sc(CMD | SHIFT, Key::L)),
            (NewWindow, sc(CMD | SHIFT, Key::N)),
            (Settings, sc(CMD, Key::Comma)),
            (LocateFile, sc(CMD | ALT, Key::W)), // Eclipse "Show In"
            (Palette, sc(CMD | SHIFT, Key::A)),
            (ToggleProject, sc(CMD, Key::Num1)),
            (ToggleBottom, sc(CMD, Key::Num4)),
            (ToggleRight, sc(CMD, Key::Num9)),
            (FocusMode, sc(CMD | SHIFT, Key::F12)),
            (RunApp, sc(CTRL, Key::R)),
            (RunTask, sc(CTRL | SHIFT, Key::R)),
            (QuickFix, sc(ALT, Key::Enter)),
            (NewFile, sc(CMD, Key::N)),
        ],
        Keymap::AndroidStudio => vec![
            (GotoClass, sc(CMD, Key::O)),
            (GotoFile, sc(CMD | SHIFT, Key::O)),
            (GotoSymbol, sc(CMD | ALT, Key::O)),
            (Recent, sc(CMD, Key::E)),
            (Structure, sc(CMD, Key::F12)),
            (GotoLine, sc(CMD, Key::L)),
            (GotoDefinition, sc(CMD, Key::B)),
            (References, sc(ALT, Key::F7)),
            (LocateFile, sc(ALT, Key::F1)), // Android Studio "Select In"
            (Implementations, sc(CMD | ALT, Key::B)),
            (Find, sc(CMD, Key::F)),
            (Replace, sc(CMD, Key::R)),
            (FindNext, sc(CMD, Key::G)),
            (FindPrev, sc(CMD | SHIFT, Key::G)),
            (Build, sc(CMD, Key::F9)),
            (FindInFiles, sc(CMD | SHIFT, Key::F)),
            (Back, sc(CMD, Key::OpenBracket)),
            (Forward, sc(CMD, Key::CloseBracket)),
            (Complete, sc(CTRL, Key::Space)),
            (Rename, sc(SHIFT, Key::F6)),
            (ExtractVariable, sc(CMD | ALT, Key::V)),
            (OptimizeImports, sc(CTRL | ALT, Key::O)),
            (ToggleComment, sc(CMD, Key::Slash)),
            (DeleteLine, sc(CMD, Key::Backspace)),
            (DuplicateLine, sc(CMD, Key::D)),
            (MoveLineUp, sc(ALT | SHIFT, Key::ArrowUp)),
            (MoveLineDown, sc(ALT | SHIFT, Key::ArrowDown)),
            (Save, sc(CMD, Key::S)),
            (CloseTab, sc(CMD, Key::W)),
            (Shortcuts, sc(CMD | SHIFT, Key::Slash)),
            (NewWindow, sc(CMD | SHIFT, Key::N)),
            (Settings, sc(CMD, Key::Comma)),
            (Palette, sc(CMD | SHIFT, Key::A)),
            (ToggleProject, sc(CMD, Key::Num1)),
            (ToggleBottom, sc(CMD, Key::Num4)),
            (ToggleRight, sc(CMD, Key::Num9)),
            (FocusMode, sc(CMD | SHIFT, Key::F12)),
            (RunApp, sc(CTRL, Key::R)),
            (RunTask, sc(CTRL | SHIFT, Key::R)),
            (QuickFix, sc(ALT, Key::Enter)),
            (NewFile, sc(CMD, Key::N)),
        ],
    };
    v.sort_by_key(|(_, s)| std::cmp::Reverse(count(s.modifiers)));
    v
}

fn count(m: Modifiers) -> u8 {
    [m.command || m.mac_cmd, m.shift, m.alt, m.ctrl]
        .into_iter()
        .filter(|b| *b)
        .count() as u8
}

/// "⇧⌘T" style text for menus and tooltips. Android Studio's ⇧⇧ is not a key chord, so it is added by hand.
pub fn text(km: Keymap, action: Action) -> String {
    // egui names arrows with ⏶⏷⏴⏵, which SF Mono lacks; use the arrows every font has
    let arrows = |t: String| {
        t.replace('⏶', "↑")
            .replace('⏷', "↓")
            .replace('⏴', "←")
            .replace('⏵', "→")
    };
    let mut out: Vec<String> = bindings(km)
        .iter()
        .filter(|(a, _)| *a == action)
        .map(|(_, s)| arrows(s.format(&ModifierNames::SYMBOLS, true)))
        .collect();
    if km == Keymap::AndroidStudio && action == Action::Everywhere {
        out.push("⇧⇧".into());
    }
    out.join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_keymap_binds_one_chord_twice() {
        for km in [Keymap::Eclipse, Keymap::AndroidStudio] {
            let b = bindings(km);
            for (i, (a, s)) in b.iter().enumerate() {
                for (a2, s2) in &b[i + 1..] {
                    assert!(s != s2, "{km:?}: {a:?} and {a2:?} share {s:?}");
                }
            }
        }
    }

    #[test]
    fn the_palette_lists_every_bound_action_once() {
        for km in [Keymap::Eclipse, Keymap::AndroidStudio] {
            for (a, _) in bindings(km) {
                assert_eq!(ALL.iter().filter(|x| **x == a).count(), 1, "{a:?}");
            }
        }
    }

    #[test]
    fn specific_chords_come_first() {
        // ⇧⌘O (organize imports) must be checked before ⌘O (outline) in Eclipse
        let b = bindings(Keymap::Eclipse);
        let pos = |a| b.iter().position(|(x, _)| *x == a).unwrap();
        assert!(pos(Action::OptimizeImports) < pos(Action::Structure));
        assert_eq!(text(Keymap::Eclipse, Action::GotoClass), "⇧⌘T");
        assert_eq!(text(Keymap::AndroidStudio, Action::Everywhere), "⇧⇧");
        assert_eq!(
            Keymap::from_name("Android Studio (macOS)\n"),
            Keymap::AndroidStudio
        );
        assert_eq!(Keymap::from_name("anything else"), Keymap::Eclipse);
        assert_eq!(text(Keymap::Eclipse, Action::MoveLineDown), "⌥↓");
    }
}
