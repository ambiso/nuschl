// SPDX-License-Identifier: EUPL-1.2
//! Linux capture authorization. Portal grants live only as long as the
//! desktop holds this private D-Bus connection.

use super::{Capture, CaptureLimits, Source};
use ashpd::{
    Error, WindowIdentifier,
    desktop::{
        PersistMode,
        camera::Camera,
        screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    },
    zbus,
};
use futures_util::FutureExt as _;
use std::{os::fd::OwnedFd, panic::AssertUnwindSafe, path::PathBuf};

pub struct Grant {
    remote: OwnedFd,
    node_id: Option<u32>,
    connection: zbus::Connection,
    source: Source,
    monitor: bool,
}

impl Grant {
    pub fn source(&self) -> Source {
        self.source
    }

    /// A monitor or unspecified screen source can include the Nuschl window.
    pub fn can_include_app(&self) -> bool {
        self.monitor
    }

    pub fn start(self, executable: PathBuf, bitrate_bps: u32) -> Result<Capture, super::Error> {
        let limits = CaptureLimits::for_source(self.source, bitrate_bps);
        self.start_with_limits(executable, limits)
    }

    pub fn start_with_limits(
        self,
        executable: PathBuf,
        limits: CaptureLimits,
    ) -> Result<Capture, super::Error> {
        let mut capture = Capture::start_inner(
            executable,
            self.remote,
            self.source,
            self.node_id,
            true,
            limits,
            None,
        )?;
        capture.retain_grant(self.connection);
        Ok(capture)
    }

    pub fn start_with_preview(
        self,
        executable: PathBuf,
        limits: CaptureLimits,
        preview: tokio::sync::mpsc::Sender<super::DecodedFrame>,
    ) -> Result<Capture, super::Error> {
        let mut capture = Capture::start_with_preview(
            executable,
            self.remote,
            self.source,
            self.node_id,
            limits,
            preview,
        )?;
        capture.retain_grant(self.connection);
        Ok(capture)
    }
}

pub async fn request(source: Source, parent: Option<&WindowIdentifier>) -> Result<Grant, Error> {
    let connection = zbus::Connection::session().await?;
    if let Ok(app) = ashpd::AppID::try_from("org.nuschl.Nuschl") {
        // On Flatpak and newer portals the application can already be known.
        let _ = ashpd::register_host_app_with_connection(connection.clone(), app).await;
    }
    let (remote, node_id, monitor) = match source {
        Source::Camera => {
            let portal = Camera::with_connection(connection.clone()).await?;
            portal
                .request_access(Default::default())
                .await?
                .response()?;
            (
                portal.open_pipe_wire_remote(Default::default()).await?,
                None,
                false,
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
            (
                remote,
                Some(stream.pipe_wire_node_id()),
                stream.source_type() != Some(SourceType::Window),
            )
        }
    };
    Ok(Grant {
        remote,
        node_id,
        connection,
        source,
        monitor,
    })
}
