//! What a Marcel window holds when it is answering a file-chooser request.
//!
//! The pane is an ordinary Marcel pane; this is the extra state that turns it
//! into a dialog: the question, the controls the question needs, and the one
//! place the answer is sent from.

use async_channel::Sender;
use gpui::{Entity, SharedString, Subscription};
use gpui_component::{input::InputState, select::SelectState};

use crate::picker::{FileFilter, PickerMode, PickerRequest, PickerResponse};

pub struct PickerState {
    pub mode: PickerMode,
    pub multiple: bool,
    pub accept_label: String,
    pub filters: Vec<FileFilter>,
    /// Index into `filters` of the one currently applied.
    pub active_filter: Option<usize>,
    /// The name field of a save dialog.
    pub name_input: Option<Entity<InputState>>,
    pub _name_subscription: Option<Subscription>,
    pub filter_select: Option<Entity<SelectState<Vec<SharedString>>>>,
    pub _filter_subscription: Option<Subscription>,
    /// A background existence check is running; confirming again waits.
    pub confirming: bool,
    /// Present until the answer is sent. Exactly one answer per request.
    reply: Option<Sender<PickerResponse>>,
}

impl PickerState {
    pub fn new(
        request: PickerRequest,
        name_input: Option<(Entity<InputState>, Subscription)>,
        filter_select: Option<(Entity<SelectState<Vec<SharedString>>>, Subscription)>,
    ) -> Self {
        let accept_label = request
            .accept_label
            .clone()
            .unwrap_or_else(|| request.default_accept_label().to_string());
        let (name_input, name_subscription) = name_input.unzip();
        let (filter_select, filter_subscription) = filter_select.unzip();
        Self {
            // With filters offered and none chosen, the first one is what the
            // caller expects to see applied; that is what GTK does.
            active_filter: request
                .current_filter
                .or_else(|| (!request.filters.is_empty()).then_some(0)),
            mode: request.mode,
            multiple: request.multiple,
            accept_label,
            filters: request.filters,
            name_input,
            _name_subscription: name_subscription,
            filter_select,
            _filter_subscription: filter_subscription,
            confirming: false,
            reply: Some(request.reply),
        }
    }

    /// The filter currently applied to the listing, if any.
    pub fn active_filter(&self) -> Option<&FileFilter> {
        self.active_filter.and_then(|index| self.filters.get(index))
    }

    /// Send the answer. Only the first answer counts; the rest report `false`.
    pub fn answer(&mut self, response: PickerResponse) -> bool {
        let Some(reply) = self.reply.take() else {
            return false;
        };
        // The receiver is the D-Bus method waiting on us. A send can only fail
        // when it stopped waiting, and then there is nobody to tell.
        let _ = reply.try_send(response);
        true
    }
}

impl Drop for PickerState {
    /// A window that closes without answering — the title-bar button, the
    /// compositor killing it — answered "cancel". Anything else leaves the
    /// caller's dialog blocked on a reply that will never come.
    fn drop(&mut self) {
        self.answer(PickerResponse::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(reply: Sender<PickerResponse>) -> PickerRequest {
        let (_close, closed) = async_channel::bounded(1);
        PickerRequest {
            title: String::new(),
            mode: PickerMode::OpenFiles,
            multiple: false,
            accept_label: None,
            start_directory: None,
            current_name: None,
            filters: vec![FileFilter::new("All".to_string(), Vec::new())],
            current_filter: None,
            reply,
            closed,
        }
    }

    #[test]
    fn dropping_an_unanswered_picker_cancels_it_once() {
        let (reply, responses) = async_channel::bounded(1);
        let state = PickerState::new(request(reply), None, None);
        assert_eq!(state.active_filter, Some(0));
        assert_eq!(state.accept_label, "Open");
        drop(state);
        assert_eq!(responses.try_recv(), Ok(PickerResponse::Cancelled));
        assert!(responses.try_recv().is_err());
    }

    #[test]
    fn only_the_first_answer_is_sent() {
        let (reply, responses) = async_channel::bounded(1);
        let mut state = PickerState::new(request(reply), None, None);
        assert!(state.answer(PickerResponse::Closed));
        assert!(!state.answer(PickerResponse::Cancelled));
        drop(state);
        assert_eq!(responses.try_recv(), Ok(PickerResponse::Closed));
        assert!(responses.try_recv().is_err());
    }
}
