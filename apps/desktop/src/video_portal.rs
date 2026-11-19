// SPDX-License-Identifier: EUPL-1.2
//! Linux capture authorization. Portal grants live only as long as the
//! desktop holds this private D-Bus connection.

use ashpd::{
    Error, WindowIdentifier,
    desktop::{
        PersistMode,
        camera::Camera,
        screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    },
};
use futures_util::FutureExt as _;
use nuschl_ipc::video::Source;
use std::{
    os::fd::{AsFd as _, BorrowedFd, OwnedFd},
    panic::AssertUnwindSafe,
};

pub struct Grant {
    remote: OwnedFd,
    node_id: Option<u32>,
    _connection: zbus::Connection,
}

impl Grant {
    pub fn remote(&self) -> BorrowedFd<'_> {
        self.remote.as_fd()
    }

    pub fn node_id(&self) -> Option<u32> {
        self.node_id
    }
}

pub async fn request(source: Source, parent: Option<&WindowIdentifier>) -> Result<Grant, Error> {
    let connection = zbus::Connection::session().await?;
    if let Ok(app) = ashpd::AppID::try_from("org.nuschl.Nuschl") {
        // On Flatpak and newer portals the application can already be known.
        let _ = ashpd::register_host_app_with_connection(connection.clone(), app).await;
    }
    let (remote, node_id) = match source {
        Source::Camera => {
            let portal = Camera::with_connection(connection.clone()).await?;
            portal
                .request_access(Default::default())
                .await?
                .response()?;
            (
                portal.open_pipe_wire_remote(Default::default()).await?,
                None,
            )
        }
        Source::Screen => {
            let portal = Screencast::with_connection(connection.clone()).await?;
            // ashpd asserts that the portal returned the predicted session path.
            let session = AssertUnwindSafe(portal.create_session(Default::default()))
                .catch_unwind()
                .await
                .map_err(|_| Error::NoResponse)??;
            let sources =
                portal.available_source_types().await? & (SourceType::Monitor | SourceType::Window);
            if sources.is_empty() {
                return Err(Error::NoResponse);
            }
            let cursor = portal
                .available_cursor_modes()
                .await
                .ok()
                .filter(|modes| modes.contains(CursorMode::Embedded))
                .map(|_| CursorMode::Embedded);
            let selection = SelectSourcesOptions::default()
                .set_multiple(false)
                .set_sources(sources)
                .set_cursor_mode(cursor)
                .set_persist_mode(PersistMode::DoNot);
            portal
                .select_sources(&session, selection)
                .await?
                .response()?;
            let streams = portal
                .start(&session, parent, Default::default())
                .await?
                .response()?;
            let [stream] = streams.streams() else {
                return Err(Error::NoResponse);
            };
            let remote = portal
                .open_pipe_wire_remote(&session, Default::default())
                .await?;
            (remote, Some(stream.pipe_wire_node_id()))
        }
    };
    Ok(Grant {
        remote,
        node_id,
        _connection: connection,
    })
}
