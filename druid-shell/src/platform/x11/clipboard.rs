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

use std::rc::Rc;
use std::cell::{Cell, RefCell};

use x11rb::connection::Connection;
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask, GetPropertyReply,
    GetPropertyType, Property, Timestamp, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::xcb_ffi::XCBConnection;

use crate::clipboard::{ClipboardFormat, FormatId};
use tracing::{error, warn};

#[derive(Debug, Clone)]
pub struct Clipboard(Rc<RefCell<ClipboardState>>);

impl Clipboard {
    pub(crate) fn new(connection: Rc<XCBConnection>, screen_num: usize, server_timestamp: Rc<Cell<Timestamp>>) -> Result<Self, ReplyError> {
        Ok(Self(Rc::new(RefCell::new(ClipboardState::new(connection, screen_num, server_timestamp)?))))
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
struct ClipboardContents {
    owner_window: Window,
    timestamp: Timestamp,
    formats: Vec<Rc<ClipboardFormat>>,
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
        let formats = formats
            .iter()
            .map(|format| Rc::new(format.clone()))
            .collect();
        Ok(Self {
            owner_window,
            timestamp,
            formats,
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
    clipboard: Atom,
    server_timestamp: Rc<Cell<Timestamp>>,
    contents: Option<ClipboardContents>,
}

impl ClipboardState {
    fn new(connection: Rc<XCBConnection>, screen_num: usize, server_timestamp: Rc<Cell<Timestamp>>) -> Result<Self, ReplyError> {
        let clipboard = connection.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
        Ok(Self {
            connection,
            screen_num,
            clipboard,
            server_timestamp,
            contents: None,
        })
    }

    // TODO: Remove & destroy() old contents object when no longer needed

    fn put_formats(&mut self, formats: &[ClipboardFormat]) -> Result<(), ReplyOrIdError> {
        let conn = &*self.connection;
        let contents = ClipboardContents::new(conn, self.screen_num, self.server_timestamp.get(), formats)?;

        conn.set_selection_owner(contents.owner_window, self.clipboard, contents.timestamp)?;

        // Check if we are the selection owner; this might e.g.fail if our timestamp is too old
        let owner = conn.get_selection_owner(self.clipboard)?.reply()?;
        if owner.owner == contents.owner_window {
            // We are the new selection owner! Remember the clipboard contents for later.
            if let Some(mut old_owner) = std::mem::replace(&mut self.contents, Some(contents)) {
                // We already where the owner before. Destroy the old contents.
                old_owner.destroy(conn)?;
            }
        }

        Ok(())
    }
}
