// SPDX-License-Identifier: EUPL-1.2
//! Linux capture authorization. Portal grants live only as long as the
//! desktop holds this private D-Bus connection.

use super::{Capture, CaptureLimits, Source};
use ashpd::{
    Error, WindowIdentifier,
    desktop::{
        PersistMode, Session,
        camera::Camera,
        screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    },
    zbus,
};
use futures_util::FutureExt as _;
use std::{os::fd::OwnedFd, panic::AssertUnwindSafe, path::PathBuf, sync::Arc};

pub struct Grant {
    remote: OwnedFd,
    target: Target,
}

enum Target {
    Camera(zbus::Connection),
    Screen(ScreenGrant),
}

/// The portal session and its selected screen source remain tied together
/// while a call still intends to share that source.
#[derive(Clone)]
pub struct ScreenGrant(Arc<ScreenSession>);

struct ScreenSession {
    portal: Screencast,
    session: Session<Screencast>,
    connection: zbus::Connection,
    node_id: u32,
    monitor: bool,
}

impl ScreenGrant {
    /// Open a new PipeWire connection to the same session and node.
    pub async fn reopen(&self) -> Result<Grant, Error> {
        let remote = self
            .0
            .portal
            .open_pipe_wire_remote(&self.0.session, Default::default())
            .await?;
        Ok(Grant {
            remote,
            target: Target::Screen(self.clone()),
        })
    }
}

impl Grant {
    pub fn source(&self) -> Source {
        match self.target {
            Target::Camera(_) => Source::Camera,
            Target::Screen(_) => Source::Screen,
        }
    }

    /// A monitor or unspecified screen source can include the Nuschl window.
    pub fn can_include_app(&self) -> bool {
        matches!(&self.target, Target::Screen(screen) if screen.0.monitor)
    }

    /// The screen portal selects one node for this session. The camera
    /// portal grants access to the available devices as a group.
    pub fn screen_reopen(&self) -> Option<ScreenGrant> {
        match &self.target {
            Target::Screen(screen) => Some(screen.clone()),
            Target::Camera(_) => None,
        }
    }

    pub fn start(self, executable: PathBuf, bitrate_bps: u32) -> Result<Capture, super::Error> {
        let limits = CaptureLimits::for_source(self.source(), bitrate_bps);
        self.start_with_limits(executable, limits)
    }

    pub fn start_with_limits(
        self,
        executable: PathBuf,
        limits: CaptureLimits,
    ) -> Result<Capture, super::Error> {
        self.start_capture(executable, limits, None)
    }

    pub fn start_with_preview(
        self,
        executable: PathBuf,
        limits: CaptureLimits,
        preview: tokio::sync::mpsc::Sender<super::DecodedFrame>,
    ) -> Result<Capture, super::Error> {
        self.start_capture(executable, limits, Some(preview))
    }

    fn start_capture(
        self,
        executable: PathBuf,
        limits: CaptureLimits,
        preview: Option<tokio::sync::mpsc::Sender<super::DecodedFrame>>,
    ) -> Result<Capture, super::Error> {
        let source = self.source();
        let (connection, node_id) = match self.target {
            Target::Camera(connection) => (connection, None),
            Target::Screen(screen) => (screen.0.connection.clone(), Some(screen.0.node_id)),
        };
        let mut capture = Capture::start_inner(
            executable,
            self.remote,
            source,
            node_id,
            true,
            limits,
            preview,
        )?;
        capture.retain_grant(connection);
        Ok(capture)
    }
}

pub async fn request(source: Source, parent: Option<&WindowIdentifier>) -> Result<Grant, Error> {
    let connection = zbus::Connection::session().await?;
    if let Ok(app) = ashpd::AppID::try_from("org.nuschl.Nuschl") {
        // On Flatpak and newer portals the application can already be known.
        let _ = ashpd::register_host_app_with_connection(connection.clone(), app).await;
    }
    match source {
        Source::Camera => {
            let portal = Camera::with_connection(connection.clone()).await?;
            portal
                .request_access(Default::default())
                .await?
                .response()?;
            Ok(Grant {
                remote: portal.open_pipe_wire_remote(Default::default()).await?,
                target: Target::Camera(connection),
            })
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
            ScreenGrant(Arc::new(ScreenSession {
                portal,
                session,
                connection,
                node_id: stream.pipe_wire_node_id(),
                monitor: stream.source_type() != Some(SourceType::Window),
            }))
            .reopen()
            .await
        }
    }
}
