//! The Network section of the sidebar: saved servers, shares connected by
//! other means, and the Add… row that opens Connect to Server.
//!
//! A connected share is a folder under `/run/user/<uid>/gvfs`, so its row
//! behaves like a place: click to go there, drop files on it. A saved server
//! that is not connected is a muted row whose click connects it and then
//! goes there, the way an unmounted drive's does.

use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{AnyElement, ClickEvent, Context, MouseButton, MouseDownEvent, Window, div};
use gpui_component::{
    ActiveTheme as _, WindowExt as _,
    button::ButtonVariant,
    dialog::DialogButtonProps,
    input::{Input, InputState},
    notification::Notification,
};

use crate::{
    desktop::gvfs::{Location, Mount},
    network::Server,
};

use super::{
    Marcel,
    dialogs::{NameDialog, footer},
    menu::{clamp_to_window, menu_row, popover},
    pointer::painted_bounds,
    sidebar::{BOOKMARK_MENU_HEIGHT, BOOKMARK_MENU_WIDTH, sidebar_icon},
    state::{NetworkMenu, NetworkTarget},
};

/// One row of the section, saved server or bare mount alike.
struct NetworkRow {
    id: (&'static str, usize),
    label: String,
    /// What a click opens.
    location: Location,
    /// Where it is browsed from while connected; `None` means disconnected.
    directory: Option<PathBuf>,
    active: bool,
    busy: bool,
    target: NetworkTarget,
    /// The mount to disconnect, when there is one.
    mount: Option<Mount>,
}

impl Marcel {
    /// Connect a location and go there, or just go there if it is connected.
    fn open_location(&mut self, location: Location, window: &Window, cx: &mut Context<Self>) {
        let origin = Self::origin(window);
        let view = cx.entity();
        self.network.update(cx, |store, cx| {
            store.connect(
                location,
                origin,
                move |directory, cx| {
                    view.update(cx, |this, cx| this.navigate_to(directory, true, cx));
                },
                cx,
            );
        });
    }

    /// Leave a share before it goes away, as with a drive.
    fn leave_mount(&mut self, mount: &Mount, cx: &mut Context<Self>) {
        if mount.contains(&self.directory.current_dir) {
            self.navigate_to(self.home_dir.clone(), true, cx);
        }
    }

    fn disconnect_mount(&mut self, mount: Mount, window: &Window, cx: &mut Context<Self>) {
        self.leave_mount(&mount, cx);
        let origin = Self::origin(window);
        self.network.update(cx, |store, cx| store.disconnect(mount, origin, cx));
    }

    /// Connect what was typed into the location bar or the Connect dialog.
    pub(super) fn connect_to_address(
        &mut self,
        address: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let location = Location::parse(address)?;
        self.open_location(location, window, cx);
        Ok(())
    }

    pub(super) fn open_connect_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ui.entry_menu = None;
        let view = cx.entity();
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("sftp://host/folder or smb://host/share")
        });
        let dialog_input = input.clone();
        let muted = cx.theme().colors.muted_foreground;
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            let input = dialog_input.clone();
            dialog
                .title("Connect to Server")
                .child(div().flex().flex_col().gap_2().child(Input::new(&input)).child(
                    div().text_sm().text_color(muted).child(
                        "A host on its own means SSH. Servers you want to keep can be added \
                             to Network from the row's menu once connected.",
                    ),
                ))
                .button_props(DialogButtonProps::default().ok_text("Connect").show_cancel(true))
                .footer(footer("Connect", ButtonVariant::Primary, true))
                .overlay_closable(false)
                .close_button(false)
                .on_ok(move |_, window, cx| {
                    let address = input.read(cx).value().trim().to_string();
                    let refused =
                        view.update(cx, |this, cx| this.connect_to_address(&address, window, cx));
                    if let Err(message) = refused {
                        window.push_notification(Notification::error(message), cx);
                        return false;
                    }
                    true
                })
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn save_mount(&mut self, mount: &Mount, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar.network_menu = None;
        let location = Location { spec: mount.spec.clone(), path: String::new() };
        let origin = Self::origin(window);
        let added = self
            .network
            .clone()
            .update(cx, |store, cx| store.add(location, Some(mount.name.clone()), origin, cx));
        if added {
            window.push_notification(
                Notification::success(format!("Added “{}” to Network", mount.name)),
                cx,
            );
        }
        cx.notify();
    }

    fn remove_server(
        &mut self,
        index: usize,
        expected: &Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.network_menu = None;
        let origin = Self::origin(window);
        let removed = self
            .network
            .clone()
            .update(cx, |store, cx| store.remove_at(index, expected, origin, cx));
        if let Some(server) = removed {
            window.push_notification(
                Notification::success(format!("Removed “{}” from Network", server.label())),
                cx,
            );
        }
        cx.notify();
    }

    fn rename_server(
        &mut self,
        index: usize,
        server: Server,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.network_menu = None;
        let input = cx.new(|cx| InputState::new(window, cx).default_value(server.label()));
        let location = server.location.clone();
        self.ask_name(
            window,
            cx,
            NameDialog { title: "Rename Server", input, action: "Rename" },
            |_| Ok(()),
            move |this, name, window, cx| {
                let origin = Self::origin(window);
                this.network.clone().update(cx, |store, cx| {
                    store.rename_at(index, &location, name, origin, cx);
                });
                cx.notify();
            },
        );
    }

    // -----------------------------------------------------------------------
    // Rendering.

    /// The saved server the window is inside, when it is inside one: the one
    /// whose folder is the deepest prefix of the current directory, so a
    /// server saved at a subfolder wins over the same host saved at its root.
    fn active_server(&self, cx: &Context<Self>) -> Option<usize> {
        let store = self.network.read(cx);
        store
            .servers()
            .iter()
            .enumerate()
            .filter_map(|(index, server)| {
                let directory =
                    store.mount_for(&server.location.spec)?.directory_for(&server.location.path)?;
                self.directory.current_dir.starts_with(&directory).then_some((index, directory))
            })
            .max_by_key(|(_, directory)| directory.as_os_str().len())
            .map(|(index, _)| index)
    }

    fn network_row(&self, row: NetworkRow, cx: &mut Context<Self>) -> AnyElement {
        let NetworkRow { id, label, location, directory, active, busy, target, mount } = row;
        let on_disconnect = mount;
        let colors = cx.theme().colors;
        let icon =
            sidebar_icon(self.network.read(cx).icon().map(Path::to_path_buf), colors.primary);
        let connected = directory.is_some();
        let place_drop_bounds = self.sidebar.place_drop_bounds.clone();
        // A connected share is a folder, so it takes drops like a place. A
        // disconnected server has nowhere to put them.
        let row = match &directory {
            Some(directory) => self.sidebar_row(id, directory, active, true, cx),
            None => self.sidebar_row(id, Path::new(""), active, false, cx).on_click(cx.listener(
                move |this, event: &ClickEvent, window, cx| {
                    if !event.is_right_click() {
                        this.open_location(location.clone(), window, cx);
                    }
                },
            )),
        };
        row.on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.ui.entry_menu = None;
                this.sidebar.bookmark_menu = None;
                this.sidebar.volume_menu = None;
                this.sidebar.network_menu =
                    Some(NetworkMenu { target: target.clone(), position: event.position });
                cx.notify();
            }),
        )
        .child(icon)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_base()
                .when(!connected, |this| this.text_color(colors.muted_foreground))
                .child(label),
        )
        .when(busy, |this| {
            this.child(div().text_xs().text_color(colors.muted_foreground).child("…"))
        })
        .when_some(on_disconnect.filter(|_| !busy), |this, mount| {
            this.child(
                div()
                    .id(("network-disconnect", id.1))
                    .flex_none()
                    .px_1()
                    .rounded(cx.theme().radius)
                    .text_color(colors.muted_foreground)
                    .hover(|this| this.text_color(colors.sidebar_accent_foreground))
                    .cursor_pointer()
                    .child("⏏")
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if !event.is_right_click() {
                            this.disconnect_mount(mount.clone(), window, cx);
                            cx.stop_propagation();
                        }
                    })),
            )
        })
        .when_some(directory, |this, directory| {
            this.child(painted_bounds(move |bounds| {
                place_drop_bounds.borrow_mut().insert(directory.clone(), bounds);
            }))
        })
        .into_any_element()
    }

    fn render_server(
        &self,
        index: usize,
        server: Server,
        active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.network.read(cx);
        let mount = store.mount_for(&server.location.spec).cloned();
        let directory = mount.as_ref().and_then(|mount| mount.directory_for(&server.location.path));
        let busy = store.is_busy(&server.location.spec);
        self.network_row(
            NetworkRow {
                id: ("server", index),
                label: server.label(),
                location: server.location.clone(),
                directory,
                active,
                busy,
                target: NetworkTarget::Server { index, location: server.location },
                mount,
            },
            cx,
        )
    }

    fn render_unsaved_mount(
        &self,
        index: usize,
        mount: Mount,
        active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.network.read(cx).is_busy(&mount.spec);
        let directory = mount.directory_for("");
        self.network_row(
            NetworkRow {
                id: ("network-mount", index),
                label: mount.name.clone(),
                location: Location { spec: mount.spec.clone(), path: String::new() },
                directory,
                active,
                busy,
                target: NetworkTarget::Mount(mount.spec.clone()),
                mount: Some(mount),
            },
            cx,
        )
    }

    /// The rows of the Network section, or `None` when there is no GVfs to
    /// connect through and the section should not exist.
    pub(super) fn render_network_rows(&self, cx: &mut Context<Self>) -> Option<Vec<AnyElement>> {
        let (available, loading, servers, unsaved, pending) = {
            let store = self.network.read(cx);
            (
                store.available(),
                store.is_loading(),
                store.servers().to_vec(),
                store.unsaved_mounts(),
                store.pending(),
            )
        };
        if !available {
            return None;
        }
        let colors = cx.theme().colors;
        let active_server = self.active_server(cx);
        let inside_unsaved = if active_server.is_none() {
            self.network.read(cx).mount_containing(&self.directory.current_dir).cloned()
        } else {
            None
        };
        let mut rows: Vec<AnyElement> = servers
            .into_iter()
            .enumerate()
            .map(|(index, server)| {
                self.render_server(index, server, active_server == Some(index), cx)
            })
            .collect();
        rows.extend(pending.into_iter().map(|spec| {
            let label = Location { spec, path: String::new() }.label();
            div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(colors.muted_foreground)
                .child(format!("Connecting to {label}…"))
                .into_any_element()
        }));
        if loading {
            rows.push(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .child("Loading servers…")
                    .into_any_element(),
            );
        }
        rows.extend(unsaved.into_iter().enumerate().map(|(index, mount)| {
            let active = inside_unsaved.as_ref() == Some(&mount);
            self.render_unsaved_mount(index, mount, active, cx)
        }));
        rows.push(
            self.sidebar_row(("network-connect", 0), Path::new(""), false, false, cx)
                .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                    if !event.is_right_click() {
                        this.open_connect_dialog(window, cx);
                    }
                }))
                .child(div().w(gpui::px(20.0)).text_color(colors.muted_foreground).child("+"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_base()
                        .text_color(colors.muted_foreground)
                        .child("Add…"),
                )
                .into_any_element(),
        );
        Some(rows)
    }

    pub(super) fn render_network_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.sidebar.network_menu.clone()?;
        let store = self.network.read(cx);
        // The menu names a server or a mount, not a slot: if the list moved
        // on since it opened, it no longer describes what a click would act
        // on, so it goes away instead.
        let (server, mount) = match &menu.target {
            NetworkTarget::Server { index, location } => {
                let server = store
                    .servers()
                    .get(*index)
                    .filter(|server| &server.location == location)?
                    .clone();
                let mount = store.mount_for(&server.location.spec).cloned();
                (Some((*index, server)), mount)
            }
            NetworkTarget::Mount(spec) => (None, Some(store.mount_for(spec)?.clone())),
        };
        let rows = usize::from(mount.is_some()) + if server.is_some() { 2 } else { 1 };
        let height = BOOKMARK_MENU_HEIGHT + 30.0 * (rows as f32 - 1.0);
        let (left, top) = clamp_to_window(menu.position, (BOOKMARK_MENU_WIDTH, height), window);
        let disconnect = mount.clone();
        let save = mount.clone();
        let remove = server.clone();
        let rename = server.clone();
        Some(
            popover("network-context-menu", left, top, BOOKMARK_MENU_WIDTH, cx)
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.sidebar.network_menu = None;
                    cx.notify();
                }))
                .when_some(disconnect, |this, mount| {
                    this.child(
                        menu_row(("network-menu-disconnect", 0), "Disconnect", true, cx).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.sidebar.network_menu = None;
                                this.disconnect_mount(mount.clone(), window, cx);
                            }),
                        ),
                    )
                })
                .when_some(rename, |this, (index, server)| {
                    this.child(menu_row(("network-menu-rename", 1), "Rename…", true, cx).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.rename_server(index, server.clone(), window, cx);
                        }),
                    ))
                })
                .when_some(remove, |this, (index, server)| {
                    this.child(
                        menu_row(("network-menu-remove", 2), "Remove from Network", true, cx)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove_server(index, &server.location, window, cx);
                            })),
                    )
                })
                .when(server.is_none(), |this| {
                    let Some(mount) = save else {
                        return this;
                    };
                    this.child(
                        menu_row(("network-menu-save", 3), "Add to Network", true, cx).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.save_mount(&mount, window, cx);
                            }),
                        ),
                    )
                })
                .into_any_element(),
        )
    }
}
