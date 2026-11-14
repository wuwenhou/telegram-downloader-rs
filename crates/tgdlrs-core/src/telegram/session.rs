//! Persistent Grammers session storage.
//!
//! `grammers_session::SessionData` deliberately does not implement `serde`
//! (its fields do, but the container does not), so this module stores a small
//! snapshot with exactly the fields needed to resume an authorized session.
//! The snapshot is written atomically (temporary file + rename) and never
//! logged. It contains the permanent authorization key, so it must be treated
//! like a password.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use grammers_session::types::{DcOption, PeerId, PeerInfo, UpdateState, UpdatesState};
use grammers_session::{BoxFuture, Session, SessionData};
use serde::{Deserialize, Serialize};

/// Peer/update-state writes are cheap to lose, so they are throttled.
const CACHE_SAVE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionSnapshot {
    home_dc: i32,
    dc_options: HashMap<i32, DcOption>,
    peer_infos: HashMap<PeerId, PeerInfo>,
    updates_state: UpdatesState,
}

impl SessionSnapshot {
    fn capture(data: &SessionData) -> Self {
        Self {
            home_dc: data.home_dc,
            dc_options: data.dc_options.clone(),
            peer_infos: data.peer_infos.clone(),
            updates_state: data.updates_state.clone(),
        }
    }

    fn into_data(self) -> SessionData {
        SessionData {
            home_dc: self.home_dc,
            dc_options: self.dc_options,
            peer_infos: self.peer_infos,
            updates_state: self.updates_state,
        }
    }
}

/// A `Session` backed by a single JSON file.
pub struct FileSession {
    path: PathBuf,
    data: Mutex<SessionData>,
    last_cache_save: Mutex<Option<Instant>>,
}

impl FileSession {
    /// Load an existing session, or create an empty one.
    ///
    /// A corrupt session file is an error rather than a silent reset: silently
    /// discarding the authorization key would strand the user without warning.
    pub fn load(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let data = if path.as_os_str().is_empty() || !path.exists() {
            SessionData::default()
        } else {
            let bytes = fs::read(&path)?;
            if bytes.is_empty() {
                SessionData::default()
            } else {
                let snapshot: SessionSnapshot =
                    serde_json::from_slice(&bytes).map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "session file {} is not valid JSON: {error}; \
                             delete it and run `tgdlrs login` again",
                                path.display()
                            ),
                        )
                    })?;
                snapshot.into_data()
            }
        };

        Ok(Self {
            path,
            data: Mutex::new(data),
            last_cache_save: Mutex::new(None),
        })
    }

    /// In-memory session, useful for tests.
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            data: Mutex::new(SessionData::default()),
            last_cache_save: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn has_persisted_data(&self) -> bool {
        !self.path.as_os_str().is_empty()
            && fs::metadata(&self.path)
                .map(|metadata| metadata.len() > 2)
                .unwrap_or(false)
    }

    /// Force a synchronous save. Used on graceful shutdown.
    pub fn persist(&self) -> io::Result<()> {
        self.save_now()
    }

    fn lock_data(&self) -> std::sync::MutexGuard<'_, SessionData> {
        self.data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn save_now(&self) -> io::Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }

        let snapshot = SessionSnapshot::capture(&self.lock_data());
        let bytes = serde_json::to_vec(&snapshot)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, &bytes)?;
        match fs::rename(&temporary, &self.path) {
            Ok(()) => Ok(()),
            Err(_) => {
                // A virus scanner may briefly hold the destination. Copying is
                // less atomic but keeps the session usable.
                fs::copy(&temporary, &self.path)?;
                let _ = fs::remove_file(&temporary);
                Ok(())
            }
        }
    }

    fn save_cache_if_due(&self) -> io::Result<()> {
        let mut last = self
            .last_cache_save
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let due = last
            .map(|instant| instant.elapsed() >= CACHE_SAVE_INTERVAL)
            .unwrap_or(true);
        if due {
            *last = Some(Instant::now());
            drop(last);
            self.save_now()
        } else {
            Ok(())
        }
    }
}

fn apply_update(state: &mut UpdatesState, update: UpdateState) {
    match update {
        UpdateState::All(new_state) => *state = new_state,
        UpdateState::Primary { pts, date, seq } => {
            state.pts = pts;
            state.date = date;
            state.seq = seq;
        }
        UpdateState::Secondary { qts } => state.qts = qts,
        UpdateState::Channel { id, pts } => {
            if let Some(channel) = state.channels.iter_mut().find(|channel| channel.id == id) {
                channel.pts = pts;
            } else {
                state
                    .channels
                    .push(grammers_session::types::ChannelState { id, pts });
            }
        }
    }
}

impl Session for FileSession {
    type Error = io::Error;

    fn home_dc_id(&self) -> Result<i32, Self::Error> {
        Ok(self.lock_data().home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            self.lock_data().home_dc = dc_id;
            self.save_now()
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, Self::Error> {
        Ok(self.lock_data().dc_options.get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), Self::Error>> {
        let dc_option = dc_option.clone();
        Box::pin(async move {
            self.lock_data().dc_options.insert(dc_option.id, dc_option);
            self.save_now()
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, Self::Error>> {
        Box::pin(async move { Ok(self.lock_data().peer_infos.get(&peer).cloned()) })
    }

    fn cache_peer(&self, peer: PeerInfo) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            let id = peer_id_of(&peer);
            self.lock_data().peer_infos.insert(id, peer);
            self.save_cache_if_due()
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, Self::Error>> {
        Box::pin(async move { Ok(self.lock_data().updates_state.clone()) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            {
                let mut data = self.lock_data();
                apply_update(&mut data.updates_state, update);
            }
            self.save_cache_if_due()
        })
    }
}

fn peer_id_of(peer: &PeerInfo) -> PeerId {
    match peer {
        PeerInfo::User { id, .. } => PeerId::user_unchecked(*id),
        PeerInfo::Chat { id, .. } => PeerId::chat_unchecked(*id),
        PeerInfo::Channel { id, .. } => PeerId::channel_unchecked(*id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.json");

        let session = FileSession::load(&path).unwrap();
        assert!(!session.has_persisted_data());
        session.persist().unwrap();
        assert!(session.has_persisted_data());

        let reloaded = FileSession::load(&path).unwrap();
        assert_eq!(
            reloaded.home_dc_id().unwrap(),
            session.home_dc_id().unwrap()
        );
    }

    #[test]
    fn corrupt_session_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.json");
        fs::write(&path, b"not json").unwrap();
        assert!(FileSession::load(&path).is_err());
    }

    #[test]
    fn applies_channel_update() {
        let mut state = UpdatesState::default();
        apply_update(&mut state, UpdateState::Channel { id: 42, pts: 7 });
        apply_update(&mut state, UpdateState::Channel { id: 42, pts: 9 });
        assert_eq!(state.channels.len(), 1);
        assert_eq!(state.channels[0].pts, 9);
    }
}
