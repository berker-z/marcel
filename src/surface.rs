//! Which window speaks for work the application owns.
//!
//! Once operations and user data stopped belonging to the window that started
//! them, "tell the user" stopped having an obvious answer. This module supplies
//! it: the initiating window while it exists, another live window once it does
//! not, and nothing at all when there are no windows left.
//!
//! Nothing here waits. A caller that cannot find a surface refuses or stays
//! silent, because a background worker must never park on a reply that cannot
//! arrive.

use gpui::{AnyWindowHandle, App, AsyncApp, Window};
use gpui_component::{WindowExt as _, notification::Notification};

/// What background work has to say, for whichever window ends up saying it.
pub enum Report {
    Success(String),
    Error(String),
}

impl Report {
    pub fn show(self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Success(message) => window.push_notification(Notification::success(message), cx),
            Self::Error(message) => window.push_notification(Notification::error(message), cx),
        }
    }
}

/// Show a report on whichever window still speaks for `origin`.
pub fn deliver(origin: AnyWindowHandle, report: Option<Report>, cx: &mut AsyncApp) {
    let Some(report) = report else {
        return;
    };
    let Some(handle) = cx.update(|cx| current(Some(origin), cx)) else {
        return;
    };
    let _ = handle.update(cx, |_, window, cx| report.show(window, cx));
}

/// The window that answers for `origin` right now.
pub fn current(origin: Option<AnyWindowHandle>, cx: &App) -> Option<AnyWindowHandle> {
    pick(origin, cx.active_window(), &cx.windows())
}

/// Choose the window that speaks for a piece of background work.
///
/// The window that started it owns it for as long as it is open. Once that
/// window is gone the work is still the application's, so it moves to whichever
/// window the user is looking at rather than losing its voice entirely. With no
/// window at all there is nobody to ask or tell.
fn pick<H: Copy + PartialEq>(origin: Option<H>, active: Option<H>, live: &[H]) -> Option<H> {
    origin
        .filter(|origin| live.contains(origin))
        .or_else(|| active.filter(|active| live.contains(active)))
        .or_else(|| live.last().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Work reports to and asks the window that started it, for as long as that
    /// window exists.
    #[test]
    fn work_speaks_through_the_window_that_started_it() {
        assert_eq!(pick(Some(1), Some(2), &[1, 2, 3]), Some(1));
    }

    /// Closing the initiating window does not silence the work it started. The
    /// work is the application's, so it moves to the window the user is looking
    /// at.
    #[test]
    fn a_closed_origin_hands_the_work_to_a_live_window() {
        assert_eq!(pick(Some(9), Some(2), &[1, 2, 3]), Some(2));
        // With nothing focused, any live window is better than none.
        assert_eq!(pick(Some(9), None, &[1, 2, 3]), Some(3));
        // A stale active handle is no better than a stale origin.
        assert_eq!(pick(Some(9), Some(8), &[1, 2, 3]), Some(3));
    }

    /// With no window left there is nobody to ask. Callers must refuse rather
    /// than park a worker on an answer that cannot arrive.
    #[test]
    fn no_window_means_no_surface_rather_than_waiting() {
        assert_eq!(pick(Some(1), Some(1), &[] as &[u32]), None);
        assert_eq!(pick(None, None, &[] as &[u32]), None);
    }
}
