// Copyright 2020 The Druid Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Interactions with the system pasteboard on X11.

use std::cell::{Cell, RefCell};
use std::convert::TryFrom;
use std::rc::Rc;

use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, GetPropertyReply,
    GetPropertyType, Property, PropertyNotifyEvent, PropMode, SelectionClearEvent, SelectionNotifyEvent,
    SelectionRequestEvent, Timestamp, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::xcb_ffi::XCBConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::clipboard::{ClipboardFormat, FormatId};
use tracing::{error, warn};

x11rb::atom_manager! {
    ClipboardAtoms: ClipboardAtomsCookie {
        CLIPBOARD,
        TARGETS,
        INCR,
    }
}

#[derive(Debug, Clone)]
pub struct Clipboard(Rc<RefCell<ClipboardState>>);

impl Clipboard {
    pub(crate) fn new(connection: Rc<XCBConnection>, screen_num: usize, server_timestamp: Rc<Cell<Timestamp>>) -> Result<Self, ReplyError> {
        Ok(Self(Rc::new(RefCell::new(ClipboardState::new(connection, screen_num, server_timestamp)?))))
    }

    pub(crate) fn handle_clear(&self, event: &SelectionClearEvent) -> Result<(), ConnectionError> {
        self.0.borrow_mut().handle_clear(event)
    }

    pub(crate) fn handle_request(&self, event: &SelectionRequestEvent) -> Result<(), ReplyOrIdError> {
        self.0.borrow_mut().handle_request(event)
    }

    pub(crate) fn handle_property_notify(&self, event: &PropertyNotifyEvent) -> Result<(), ReplyOrIdError> {
        self.0.borrow_mut().handle_property_notify(event)
    }

    pub fn put_string(&mut self, s: impl AsRef<str>) {
        self.put_formats(&[ClipboardFormat::from(s.as_ref())]);
    }

    pub fn put_formats(&mut self, formats: &[ClipboardFormat]) {
        if let Err(err) = self.0.borrow_mut().put_formats(formats) {
            error!("Error in Clipboard::put_formats: {:?}", err);
        }
    }

    pub fn get_string(&self) -> Option<String> {
        // TODO(x11/clipboard): implement Clipboard::get_string
        warn!("Clipboard::set_string is currently unimplemented for X11 platforms.");
        None
    }

    pub fn preferred_format(&self, _formats: &[FormatId]) -> Option<FormatId> {
        // TODO(x11/clipboard): implement Clipboard::preferred_format
        warn!("Clipboard::preferred_format is currently unimplemented for X11 platforms.");
        None
    }

    pub fn get_format(&self, _format: FormatId) -> Option<Vec<u8>> {
        // TODO(x11/clipboard): implement Clipboard::get_format
        warn!("Clipboard::get_format is currently unimplemented for X11 platforms.");
        None
    }

    pub fn available_type_names(&self) -> Vec<String> {
        // TODO(x11/clipboard): implement Clipboard::available_type_names
        warn!("Clipboard::available_type_names is currently unimplemented for X11 platforms.");
        vec![]
    }
}

#[derive(Debug)]
struct IncrementalTransfer {
    requestor: Window,
    selection: Atom,
    target: Atom,
    property: Atom,
    time: Timestamp,
    data: Rc<Vec<u8>>,
    data_offset: usize,
}

impl IncrementalTransfer {
    fn new(connection: &XCBConnection, event: &SelectionRequestEvent, data: Rc<Vec<u8>>, incr: Atom) -> Result<Self, ConnectionError> {
        // We need PropertyChangeEvents on the window
        connection.change_window_attributes(
            event.requestor,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let length = u32::try_from(data.len()).unwrap_or(u32::MAX);
        connection.change_property32(
            PropMode::REPLACE,
            event.requestor,
            event.property,
            incr,
            &[length],
        )?;
        Ok(Self {
            requestor: event.requestor,
            selection: event.selection,
            target: event.target,
            property: event.property,
            time: event.time,
            data,
            data_offset: 0,
        })
    }

    // Continue an incremental transfer, returning true if the transfer is finished
    fn continue_incremental(&mut self, connection: &XCBConnection) -> Result<bool, ConnectionError> {
        let remaining = &self.data[self.data_offset..];
        let next_length = remaining.len().min(maximum_property_length(connection));
        connection.change_property8(
            PropMode::REPLACE,
            self.requestor,
            self.property,
            self.target,
            &remaining[..next_length],
        )?;
        self.data_offset += next_length;
        Ok(remaining.is_empty())
    }
}

#[derive(Debug)]
struct ClipboardContents {
    owner_window: Window,
    timestamp: Timestamp,
    data: Vec<(Atom, Rc<Vec<u8>>)>,
}

impl ClipboardContents {
    fn new(connection: &XCBConnection, screen_num: usize, timestamp: Timestamp, formats: &[ClipboardFormat]) -> Result<Self, ReplyOrIdError> {
        let owner_window = connection.generate_id()?;
        connection.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            owner_window,
            connection.setup().roots[screen_num].root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &Default::default(),
        )?;
        let data = formats
            .iter()
            .filter_map(|format| intern_atom(connection, format.identifier).map(|atom| (atom, Rc::new(format.data.clone()))))
            .collect();
        Ok(Self {
            owner_window,
            timestamp,
            data,
        })
    }

    fn destroy(&mut self, connection: &XCBConnection) -> Result<(), ConnectionError> {
        connection.destroy_window(std::mem::replace(&mut self.owner_window, x11rb::NONE))?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct ClipboardState {
    connection: Rc<XCBConnection>,
    screen_num: usize,
    atoms: ClipboardAtoms,
    server_timestamp: Rc<Cell<Timestamp>>,
    contents: Option<ClipboardContents>,
    incremental: Vec<IncrementalTransfer>,
}

impl ClipboardState {
    fn new(connection: Rc<XCBConnection>, screen_num: usize, server_timestamp: Rc<Cell<Timestamp>>) -> Result<Self, ReplyError> {
        let atoms = ClipboardAtoms::new(&*connection)?.reply()?;
        Ok(Self {
            connection,
            screen_num,
            atoms,
            server_timestamp,
            contents: None,
            incremental: Vec::new(),
        })
    }

    // TODO: Remove & destroy() old contents object when no longer needed

    fn put_formats(&mut self, formats: &[ClipboardFormat]) -> Result<(), ReplyOrIdError> {
        let conn = &*self.connection;
        let contents = ClipboardContents::new(conn, self.screen_num, self.server_timestamp.get(), formats)?;

        conn.set_selection_owner(contents.owner_window, self.atoms.CLIPBOARD, contents.timestamp)?;

        // Check if we are the selection owner; this might e.g.fail if our timestamp is too old
        let owner = conn.get_selection_owner(self.atoms.CLIPBOARD)?.reply()?;
        if owner.owner == contents.owner_window {
            // We are the new selection owner! Remember the clipboard contents for later.
            if let Some(mut old_owner) = std::mem::replace(&mut self.contents, Some(contents)) {
                // We already where the owner before. Destroy the old contents.
                old_owner.destroy(conn)?;
            }
        }

        Ok(())
    }

    fn handle_clear(&mut self, event: &SelectionClearEvent) -> Result<(), ConnectionError> {
        let window = self.contents.as_ref().map(|c| c.owner_window);
        if Some(event.owner) == window {
            // We lost ownership of the selection, clean up
            if let Some(mut contents) = self.contents.take() {
                contents.destroy(&*self.connection)?;
            }
        }
        Ok(())
    }

    fn handle_request(&mut self, event: &SelectionRequestEvent) -> Result<(), ReplyOrIdError> {
        let conn = &*self.connection;
        let contents = match &self.contents {
            Some(contents) if contents.owner_window == event.owner => contents,
            _ => {
                // Reject the transfer, we do not know what to do with it
                reject_transfer(conn, event)?;
                return Ok(());
            }
        };

        if event.target == self.atoms.TARGETS {
            // TARGETS is a special case since it replies with a list of u32
            let mut atoms = contents
                .data
                .iter()
                .map(|(atom, _)| *atom)
                .collect::<Vec<_>>();
            atoms.push(self.atoms.TARGETS);
            conn.change_property32(
                PropMode::REPLACE,
                event.requestor,
                event.property,
                AtomEnum::ATOM,
                &atoms,
            )?;
        } else {
            // Find the requested target
            let content = contents
                .data
                .iter()
                .find(|(atom, _)| *atom == event.target);
            match content {
                None => {
                    reject_transfer(conn, event)?;
                    return Ok(());
                }
                Some((atom, data)) => {
                    if data.len() > maximum_property_length(conn) {
                        // We need to do an INCR transfer. Sigh.
                        self.incremental.push(IncrementalTransfer::new(
                            conn,
                            event,
                            Rc::clone(&data),
                            self.atoms.INCR,
                        )?);
                    } else {
                        // We can provide the data directly
                        conn.change_property8(
                            PropMode::REPLACE,
                            event.requestor,
                            event.property,
                            *atom,
                            data,
                        )?;
                    }
                }
            }
        }

        // Inform the requestor that we sent the data
        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            requestor: event.requestor,
            selection: event.selection,
            target: event.target,
            property: event.property,
            time: event.time,
        };
        conn.send_event(false, event.requestor, EventMask::NO_EVENT, &event)?;

        Ok(())
    }

    fn handle_property_notify(&mut self, event: &PropertyNotifyEvent) -> Result<(), ReplyOrIdError> {
        fn matches(transfer: &IncrementalTransfer, event: &PropertyNotifyEvent) -> bool {
            transfer.requestor == event.window && transfer.property == event.atom
        }

        if event.state != Property::DELETE {
            return Ok(());
        }
        if let Some(transfer) = self
                .incremental
                .iter_mut()
                .find(|transfer| matches(transfer, event)) {
            let done = transfer.continue_incremental(&*self.connection)?;
            if done {
                // Transfer is done, remove it
                self.incremental.retain(|transfer| !matches(transfer, event));
            }
        }
        Ok(())
    }
}

fn maximum_property_length(connection: &XCBConnection) -> usize {
    let change_prop_header_size = 24;
    // Apply an arbitrary limit to the property size to not stress the server too much
    let max_request_length = connection.maximum_request_bytes().min(usize::from(u16::MAX));
    max_request_length - change_prop_header_size
}

fn intern_atom(connection: &XCBConnection, name: &str) -> Option<Atom> {
    fn intern_atom_impl(connection: &XCBConnection, name: &str) -> Result<Atom, ReplyError> {
        Ok(connection.intern_atom(false, name.as_bytes())?.reply()?.atom)
    }
    match intern_atom_impl(connection, name) {
        Ok(atom) => Some(atom),
        Err(err) => {
            error!("Error while interning clipboard atom: {:?}", err);
            None
        }
    }
}

fn reject_transfer(conn: &XCBConnection, event: &SelectionRequestEvent) -> Result<(), ConnectionError> {
    let event = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        requestor: event.requestor,
        selection: event.selection,
        target: event.target,
        property: x11rb::NONE,
        time: event.time,
    };
    conn.send_event(false, event.requestor, EventMask::NO_EVENT, &event)?;
    Ok(())
}
