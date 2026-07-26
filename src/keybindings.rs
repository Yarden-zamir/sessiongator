//! Sessiongator's keybinding vocabulary.
//!
//! The engine (chord parsing, layering, resolution) lives in gator; this file
//! defines only the contexts and actions sessiongator understands, their
//! defaults, and which actions make sense where.

use crossterm::event::{KeyCode, KeyModifiers};

/// Which panel a key press is interpreted in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BindingContext {
    /// Applies everywhere unless a more specific context overrides it.
    Global,
    /// The session list and its search input.
    List,
    /// The transcript pane.
    Transcript,
}

impl BindingContext {
    pub const ORDERED: [Self; 3] = [Self::Global, Self::List, Self::Transcript];
}

const fn context_as_str(context: BindingContext) -> &'static str {
    match context {
        BindingContext::Global => "global",
        BindingContext::List => "list",
        BindingContext::Transcript => "transcript",
    }
}

fn parse_context(value: &str) -> Option<BindingContext> {
    match value {
        "global" => Some(BindingContext::Global),
        "list" => Some(BindingContext::List),
        "transcript" => Some(BindingContext::Transcript),
        _ => None,
    }
}

fn fallback_contexts(context: BindingContext) -> &'static [BindingContext] {
    use BindingContext::*;
    match context {
        Global => &[Global],
        List => &[List, Global],
        Transcript => &[Transcript, Global],
    }
}

impl gator::keymap::BindingContext for BindingContext {
    fn as_str(self) -> &'static str {
        context_as_str(self)
    }

    fn parse(value: &str) -> Option<Self> {
        parse_context(value)
    }

    fn ordered() -> &'static [Self] {
        &Self::ORDERED
    }

    fn fallback_contexts(self) -> &'static [Self] {
        fallback_contexts(self)
    }
}

/// Everything sessiongator can do in response to a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CoreAction {
    /// Quit without selecting.
    Cancel,
    /// Resume the selected session in its original directory.
    Resume,
    /// Resume the selected session in the current directory.
    ResumeHere,
    /// Emit the session's store path instead of resuming.
    ShowPath,
    /// Convert the session into the default target tool's store.
    Convert,
    /// Copy the session id to the clipboard.
    CopyId,
    /// Toggle between title-only and full-content search.
    ToggleSearch,
    /// Cycle the sort order.
    CycleSort,
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    /// Select the first row.
    MoveHome,
    /// Select the last row.
    MoveEnd,
    /// Scroll the transcript up one page.
    PageUp,
    /// Scroll the transcript down one page.
    PageDown,
    /// Jump the transcript to the top.
    ScrollTop,
    /// Jump the transcript to the bottom.
    ScrollBottom,
}

const fn action_as_str(action: CoreAction) -> &'static str {
    match action {
        CoreAction::Cancel => "cancel",
        CoreAction::Resume => "resume",
        CoreAction::ResumeHere => "resume-here",
        CoreAction::ShowPath => "show-path",
        CoreAction::Convert => "convert",
        CoreAction::CopyId => "copy-id",
        CoreAction::ToggleSearch => "toggle-search",
        CoreAction::CycleSort => "cycle-sort",
        CoreAction::MoveUp => "move-up",
        CoreAction::MoveDown => "move-down",
        CoreAction::MoveLeft => "move-left",
        CoreAction::MoveRight => "move-right",
        CoreAction::MoveHome => "move-home",
        CoreAction::MoveEnd => "move-end",
        CoreAction::PageUp => "page-up",
        CoreAction::PageDown => "page-down",
        CoreAction::ScrollTop => "scroll-top",
        CoreAction::ScrollBottom => "scroll-bottom",
    }
}

fn parse_action(value: &str) -> Option<CoreAction> {
    match value {
        "cancel" => Some(CoreAction::Cancel),
        "resume" => Some(CoreAction::Resume),
        "resume-here" => Some(CoreAction::ResumeHere),
        "show-path" => Some(CoreAction::ShowPath),
        "convert" => Some(CoreAction::Convert),
        "copy-id" => Some(CoreAction::CopyId),
        "toggle-search" => Some(CoreAction::ToggleSearch),
        "cycle-sort" => Some(CoreAction::CycleSort),
        "move-up" => Some(CoreAction::MoveUp),
        "move-down" => Some(CoreAction::MoveDown),
        "move-left" => Some(CoreAction::MoveLeft),
        "move-right" => Some(CoreAction::MoveRight),
        "move-home" => Some(CoreAction::MoveHome),
        "move-end" => Some(CoreAction::MoveEnd),
        "page-up" => Some(CoreAction::PageUp),
        "page-down" => Some(CoreAction::PageDown),
        "scroll-top" => Some(CoreAction::ScrollTop),
        "scroll-bottom" => Some(CoreAction::ScrollBottom),
        _ => None,
    }
}

impl gator::keymap::CoreAction for CoreAction {
    fn as_str(self) -> &'static str {
        action_as_str(self)
    }

    fn parse(value: &str) -> Option<Self> {
        parse_action(value)
    }
}

pub type BindingTarget = gator::keymap::BindingTarget<CoreAction>;
pub type Binding = gator::keymap::Binding<CoreAction>;
pub type Keymap = gator::keymap::Keymap<BindingContext, CoreAction>;

/// Whether `target` is meaningful in `context`. Sessiongator has no
/// configured (custom) actions, so only core actions and `none` are accepted.
pub fn target_is_compatible(context: BindingContext, target: &BindingTarget) -> bool {
    use BindingContext::*;
    use CoreAction::*;

    let BindingTarget::Core(action) = target else {
        return matches!(target, BindingTarget::Disabled);
    };

    let app_wide = matches!(
        action,
        Cancel
            | Resume
            | ResumeHere
            | ShowPath
            | Convert
            | CopyId
            | ToggleSearch
            | CycleSort
            | PageUp
            | PageDown
            | ScrollTop
            | ScrollBottom
    );
    match context {
        Global => app_wide,
        List => app_wide || matches!(action, MoveUp | MoveDown | MoveRight | MoveHome | MoveEnd),
        Transcript => app_wide || matches!(action, MoveUp | MoveDown | MoveLeft),
    }
}

pub fn default_keymap() -> Keymap {
    use BindingContext::*;
    use CoreAction::*;

    let mut keymap = Keymap::default();
    let mut set = |context, code, modifiers, action| {
        keymap.set(
            context,
            Binding::new(
                gator::keymap::KeyChord::new(code, modifiers),
                BindingTarget::Core(action),
            ),
        );
    };
    let none = KeyModifiers::NONE;
    let ctrl = KeyModifiers::CONTROL;

    set(Global, KeyCode::Esc, none, Cancel);
    set(Global, KeyCode::Char('c'), ctrl, Cancel);
    set(Global, KeyCode::Enter, none, Resume);
    set(Global, KeyCode::Enter, ctrl, ResumeHere);
    set(Global, KeyCode::Char('o'), ctrl, ShowPath);
    set(Global, KeyCode::Char('t'), ctrl, Convert);
    set(Global, KeyCode::Char('y'), ctrl, CopyId);
    set(Global, KeyCode::Char('f'), ctrl, ToggleSearch);
    set(Global, KeyCode::Char('s'), ctrl, CycleSort);
    set(Global, KeyCode::PageUp, none, PageUp);
    set(Global, KeyCode::PageDown, none, PageDown);
    set(Global, KeyCode::Home, ctrl, ScrollTop);
    set(Global, KeyCode::End, ctrl, ScrollBottom);

    set(List, KeyCode::Up, none, MoveUp);
    set(List, KeyCode::Down, none, MoveDown);
    set(List, KeyCode::Right, none, MoveRight);
    set(List, KeyCode::Up, ctrl, MoveHome);
    set(List, KeyCode::Down, ctrl, MoveEnd);

    set(Transcript, KeyCode::Up, none, MoveUp);
    set(Transcript, KeyCode::Down, none, MoveDown);
    set(Transcript, KeyCode::Left, none, MoveLeft);
    set(Transcript, KeyCode::Home, none, ScrollTop);
    set(Transcript, KeyCode::End, none, ScrollBottom);
    set(Transcript, KeyCode::Up, ctrl, ScrollTop);
    set(Transcript, KeyCode::Down, ctrl, ScrollBottom);

    keymap
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use gator::keymap::BindingContext as _;

    fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn contexts_and_actions_round_trip() {
        for context in BindingContext::ordered() {
            assert_eq!(parse_context(context_as_str(*context)), Some(*context));
        }
        let actions = [
            CoreAction::Cancel,
            CoreAction::Resume,
            CoreAction::ResumeHere,
            CoreAction::ShowPath,
            CoreAction::Convert,
            CoreAction::CopyId,
            CoreAction::ToggleSearch,
            CoreAction::CycleSort,
            CoreAction::MoveUp,
            CoreAction::MoveDown,
            CoreAction::MoveLeft,
            CoreAction::MoveRight,
            CoreAction::MoveHome,
            CoreAction::MoveEnd,
            CoreAction::PageUp,
            CoreAction::PageDown,
            CoreAction::ScrollTop,
            CoreAction::ScrollBottom,
        ];
        for action in actions {
            assert_eq!(parse_action(action_as_str(action)), Some(action));
        }
    }

    #[test]
    fn defaults_preserve_documented_keys() {
        let keymap = default_keymap();
        let cases = [
            (BindingContext::List, "enter", CoreAction::Resume),
            (BindingContext::List, "ctrl-enter", CoreAction::ResumeHere),
            (BindingContext::List, "ctrl-o", CoreAction::ShowPath),
            (BindingContext::List, "ctrl-t", CoreAction::Convert),
            (BindingContext::List, "ctrl-y", CoreAction::CopyId),
            (BindingContext::List, "ctrl-f", CoreAction::ToggleSearch),
            (BindingContext::List, "ctrl-s", CoreAction::CycleSort),
            (BindingContext::List, "esc", CoreAction::Cancel),
            (BindingContext::List, "up", CoreAction::MoveUp),
            (BindingContext::List, "ctrl-up", CoreAction::MoveHome),
            (BindingContext::List, "right", CoreAction::MoveRight),
            (BindingContext::Transcript, "left", CoreAction::MoveLeft),
            (BindingContext::Transcript, "home", CoreAction::ScrollTop),
            (
                BindingContext::Transcript,
                "ctrl-down",
                CoreAction::ScrollBottom,
            ),
            // page keys and ctrl-home/end scroll the transcript from anywhere
            (BindingContext::List, "pageup", CoreAction::PageUp),
            (BindingContext::List, "ctrl-home", CoreAction::ScrollTop),
            (BindingContext::Transcript, "pagedown", CoreAction::PageDown),
        ];
        for (context, chord, action) in cases {
            let chord = gator::keymap::KeyChord::parse(chord).unwrap();
            assert_eq!(
                keymap.resolve(context, &event(chord.code, chord.modifiers)),
                Some(&BindingTarget::Core(action)),
                "{context:?} {chord}"
            );
        }
    }

    #[test]
    fn plain_text_keys_stay_unbound_so_they_reach_the_search_input() {
        let keymap = default_keymap();
        for chord in ["a", "r", "home", "end", "space"] {
            let chord = gator::keymap::KeyChord::parse(chord).unwrap();
            assert_eq!(
                keymap.resolve(BindingContext::List, &event(chord.code, chord.modifiers)),
                None,
                "{chord} must reach the input"
            );
        }
    }

    #[test]
    fn every_default_target_is_context_compatible() {
        default_keymap()
            .validate_targets(|context, target| {
                target_is_compatible(context, target)
                    .then_some(())
                    .ok_or_else(|| format!("{} is incompatible", target.as_str()))
            })
            .unwrap();
    }
}
