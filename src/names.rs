//! How a file's name is shown and offered for editing, wherever that happens.
//!
//! Both the window and the operation owner put names in front of the user —
//! a rename field, the save dialog, the conflict dialog — and neither should
//! have to reach into the other for the rules. They live here instead.

use std::path::Path;

use gpui::{App, Entity, Window};
use gpui_component::input::InputState;

use crate::browse::entries::display_filename;

/// The name of `path` as the user should read it, or the whole path when it
/// has no final component (the root).
pub fn display_path_name(path: &Path) -> String {
    path.file_name().map(display_filename).unwrap_or_else(|| path.display().to_string())
}

/// How much of a name is offered for replacement, as a byte offset: up to the
/// extension, or all of it for a folder, whose dots are not extensions.
pub fn rename_stem_end(name: &str, is_directory: bool) -> usize {
    if is_directory {
        return name.len();
    }
    name.rfind('.').filter(|index| *index > 0).unwrap_or(name.len())
}

/// Focus `input` with its stem selected, so typing replaces the name and
/// leaves the extension. Every field that offers a name uses this: inline
/// rename, the save dialog, Compress, and the conflict dialog.
///
/// The selection is set directly rather than through the input's
/// `SelectToStart` action: an action dispatches to whatever has focus, and
/// focus given in the same frame has not landed yet, so the action went
/// nowhere and the caret merely sat before the extension.
pub fn select_stem(
    input: Entity<InputState>,
    is_directory: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let value = input.read(cx).value().to_string();
    let stem_end = rename_stem_end(&value, is_directory);
    window.defer(cx, move |window, cx| {
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.set_selected_range(0..stem_end, cx);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_selection_preserves_a_file_extension_but_selects_directory_dots() {
        assert_eq!(rename_stem_end("report.final.txt", false), 12);
        assert_eq!(rename_stem_end(".bashrc", false), 7);
        assert_eq!(rename_stem_end("folder.with.dots", true), 16);
        assert_eq!(rename_stem_end("猫.txt", false), 3);
    }

    #[test]
    fn a_path_is_shown_by_its_last_component_and_the_root_by_itself() {
        assert_eq!(display_path_name(Path::new("/home/me/report.txt")), "report.txt");
        assert_eq!(display_path_name(Path::new("/")), "/");
    }
}
