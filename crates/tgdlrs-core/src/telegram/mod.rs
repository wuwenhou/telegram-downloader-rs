//! Telegram connectivity built on Grammers (MTProto).
//!
//! Only the worker owns a live [`TelegramConnection`]. The CLI and MCP server
//! talk to the worker through the local named pipe for anything that needs
//! Telegram, and use SQLite directly for queue control.

pub mod session;
pub mod source;

pub use grammers_session::types::PeerRef;

use std::sync::Arc;
use std::time::Duration;

use grammers_client::client::{LoginToken, PasswordToken};
use grammers_client::media::{Document, Media};
use grammers_client::message::Message;
use grammers_client::{Client, SignInError};
use grammers_mtsender::{InvocationError, SenderPool, SenderPoolFatHandle};
use grammers_session::types::PeerId;
use grammers_tl_types as tl;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{Error, Result};
use crate::models::FileEntry;
use session::FileSession;

/// Result of a successful `connect`.
pub struct TelegramConnection {
    client: Client,
    session: Arc<FileSession>,
    runner: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    handle: SenderPoolFatHandle,
}

impl TelegramConnection {
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn session(&self) -> &Arc<FileSession> {
        &self.session
    }

    /// Ask the sender pool to stop and persist the session.
    pub async fn shutdown(&self) {
        self.handle.quit();
        let runner = self.runner.lock().await.take();
        if let Some(runner) = runner {
            let _ = tokio::time::timeout(Duration::from_secs(5), runner).await;
        }
        let _ = self.session.persist();
    }
}

/// Connect to Telegram using the persisted session. Does not log in.
pub async fn connect(session: Arc<FileSession>, api_id: i32) -> Result<TelegramConnection> {
    let pool = SenderPool::new(Arc::clone(&session), api_id);
    let SenderPool {
        runner,
        handle,
        updates: _updates,
    } = pool;
    let runner = tokio::spawn(runner.run());
    let client = Client::new(handle.clone());
    Ok(TelegramConnection {
        client,
        session,
        runner: tokio::sync::Mutex::new(Some(runner)),
        handle,
    })
}

/// Login state reported to the CLI/MCP layer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoginStatus {
    pub authorized: bool,
    pub user: Option<String>,
    pub session_present: bool,
}

/// Outcome of submitting the OTP.
pub enum SignInOutcome {
    SignedIn { name: Option<String> },
    PasswordRequired(PasswordToken),
}

/// A parsed Telegram message link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedLink {
    pub raw: String,
    pub chat: LinkChat,
    pub message_id: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkChat {
    /// Private channel link: `t.me/c/<internal id>/<message>`.
    ChannelId(i64),
    /// Public username link: `t.me/<username>/<message>`.
    Username(String),
}

/// Everything needed to download one Telegram document.
pub struct TelegramFile {
    pub chat_id: i64,
    pub message_id: i32,
    pub document: Document,
    pub file_name: String,
    pub file_size: u64,
    pub mime_type: Option<String>,
    pub document_id: i64,
    pub peer_ref: PeerRef,
}

/// Convert a Grammers error into the downloader's error type.
pub fn map_invocation(error: InvocationError) -> Error {
    match error {
        InvocationError::Rpc(rpc) if rpc.code == 420 => {
            Error::FloodWait(rpc.value.unwrap_or(60).max(1) as u64)
        }
        InvocationError::Rpc(rpc) if rpc.code == 401 => Error::NotAuthorized,
        InvocationError::Rpc(rpc) => Error::Telegram(format!(
            "Telegram rejected the request: {} ({})",
            rpc.name, rpc.code
        )),
        other => Error::Telegram(other.to_string()),
    }
}

fn sign_in_error(error: SignInError) -> Error {
    match error {
        SignInError::PasswordRequired(token) => Error::Telegram(format!(
            "2FA password required (hint: {})",
            token.hint().unwrap_or("none")
        )),
        SignInError::InvalidCode => Error::InvalidInput("invalid login code".into()),
        SignInError::InvalidPassword(_) => Error::InvalidInput("invalid 2FA password".into()),
        SignInError::SignUpRequired => Error::Telegram(
            "this phone number has no Telegram account; create it in an official app first".into(),
        ),
        SignInError::Other(error) => map_invocation(error),
    }
}

/// Parse `t.me` message links, including private `t.me/c/...` links.
pub fn parse_link(input: &str) -> Result<ParsedLink> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidInput("empty Telegram link".into()));
    }

    let normalized = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else if trimmed.starts_with("tg://") {
        return parse_tg_link(trimmed);
    } else {
        format!("https://{trimmed}")
    };

    let url = Url::parse(&normalized)
        .map_err(|error| Error::InvalidInput(format!("invalid Telegram link: {error}")))?;
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    if !matches!(host.as_str(), "t.me" | "telegram.me" | "telegram.dog") {
        return Err(Error::InvalidInput(format!(
            "unsupported link host: {host}"
        )));
    }

    let segments: Vec<String> = url
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .map(|segment| segment.to_string())
                .collect()
        })
        .unwrap_or_default();

    if segments.is_empty() {
        return Err(Error::InvalidInput("Telegram link has no path".into()));
    }

    if segments[0].eq_ignore_ascii_case("c") {
        // /c/<channel>/<message> or /c/<channel>/<topic>/<message>
        let channel = segments
            .get(1)
            .ok_or_else(|| Error::InvalidInput("private link is missing the channel id".into()))?
            .parse::<i64>()
            .map_err(|_| Error::InvalidInput("private link channel id is not numeric".into()))?;
        let message = segments
            .last()
            .and_then(|segment| segment.parse::<i32>().ok())
            .ok_or_else(|| Error::InvalidInput("private link is missing the message id".into()))?;
        if channel <= 0 || message <= 0 {
            return Err(Error::InvalidInput(
                "Telegram link ids must be positive".into(),
            ));
        }
        return Ok(ParsedLink {
            raw: input.to_string(),
            chat: LinkChat::ChannelId(channel),
            message_id: message,
        });
    }

    let username = segments[0].trim_start_matches('@');
    if username.is_empty() || username.eq_ignore_ascii_case("joinchat") {
        return Err(Error::InvalidInput(
            "this link does not point to a message".into(),
        ));
    }
    let message = segments
        .last()
        .and_then(|segment| segment.parse::<i32>().ok())
        .ok_or_else(|| Error::InvalidInput("link is missing the message id".into()))?;
    if message <= 0 {
        return Err(Error::InvalidInput(
            "Telegram link message id must be positive".into(),
        ));
    }

    Ok(ParsedLink {
        raw: input.to_string(),
        chat: LinkChat::Username(username.to_string()),
        message_id: message,
    })
}

fn parse_tg_link(input: &str) -> Result<ParsedLink> {
    let url = Url::parse(input)
        .map_err(|error| Error::InvalidInput(format!("invalid tg:// link: {error}")))?;
    let mut domain = None;
    let mut post = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "domain" => domain = Some(value.to_string()),
            "post" => post = value.parse::<i32>().ok(),
            _ => {}
        }
    }
    let username = domain.ok_or_else(|| Error::InvalidInput("tg:// link has no domain".into()))?;
    let message_id = post.ok_or_else(|| Error::InvalidInput("tg:// link has no post id".into()))?;
    Ok(ParsedLink {
        raw: input.to_string(),
        chat: LinkChat::Username(username),
        message_id,
    })
}

fn document_of(message: &Message) -> Option<Document> {
    match message.media()? {
        Media::Document(document) => Some(document),
        Media::Sticker(sticker) => Some(sticker.document),
        _ => None,
    }
}

fn file_name_of(document: &Document) -> String {
    document
        .name()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string())
        .unwrap_or_else(|| format!("document-{}.bin", document.id()))
}

fn file_entry_of(message: &Message, document: &Document, already_downloaded: bool) -> FileEntry {
    FileEntry {
        message_id: message.id(),
        file_name: file_name_of(document),
        file_size: document.size().unwrap_or(0) as u64,
        mime_type: document.mime_type().map(|value| value.to_string()),
        document_id: Some(document.id()),
        date: message.date().to_string(),
        already_downloaded,
    }
}

impl TelegramConnection {
    /// Whether the session is authorized, plus the logged-in user's name.
    pub async fn login_status(&self) -> Result<LoginStatus> {
        let session_present = self.session.has_persisted_data();
        match self.client.is_authorized().await {
            Ok(true) => {
                let user = match self.client.get_me().await {
                    Ok(user) => user.first_name().map(|name| name.to_string()),
                    Err(_) => None,
                };
                Ok(LoginStatus {
                    authorized: true,
                    user,
                    session_present,
                })
            }
            Ok(false) => Ok(LoginStatus {
                authorized: false,
                user: None,
                session_present,
            }),
            Err(error) => Err(map_invocation(error)),
        }
    }

    /// Ask Telegram to send a login code.
    pub async fn request_login_code(&self, phone: &str, api_hash: &str) -> Result<LoginToken> {
        self.client
            .request_login_code(phone, api_hash)
            .await
            .map_err(map_invocation)
    }

    /// Submit the login code. The session is persisted immediately on success.
    pub async fn sign_in(&self, token: &LoginToken, code: &str) -> Result<SignInOutcome> {
        match self.client.sign_in(token, code).await {
            Ok(user) => {
                self.session.persist()?;
                Ok(SignInOutcome::SignedIn {
                    name: user.first_name().map(|name| name.to_string()),
                })
            }
            Err(SignInError::PasswordRequired(password)) => {
                Ok(SignInOutcome::PasswordRequired(password))
            }
            Err(error) => Err(sign_in_error(error)),
        }
    }

    /// Complete a 2FA login. The token is consumed by Grammers.
    pub async fn check_password(
        &self,
        token: PasswordToken,
        password: impl AsRef<[u8]>,
    ) -> Result<Option<String>> {
        match self.client.check_password(token, password).await {
            Ok(user) => {
                self.session.persist()?;
                Ok(user.first_name().map(|name| name.to_string()))
            }
            Err(error) => Err(sign_in_error(error)),
        }
    }

    /// Resolve a peer id that is stored in Bot API dialog format.
    async fn peer_ref_for_bot_api_id(&self, chat_id: i64) -> Result<PeerRef> {
        let peer_id = PeerId::from_bot_api_dialog_id(chat_id).ok_or_else(|| {
            Error::InvalidInput(format!("chat id {chat_id} is not a valid dialog id"))
        })?;
        self.peer_ref_for_peer_id(peer_id).await
    }

    async fn peer_ref_for_channel_id(&self, channel_id: i64) -> Result<PeerRef> {
        let peer_id = PeerId::channel(channel_id).ok_or_else(|| {
            Error::InvalidInput(format!("channel id {channel_id} is out of range"))
        })?;
        self.peer_ref_for_peer_id(peer_id).await
    }

    async fn peer_ref_for_peer_id(&self, peer_id: PeerId) -> Result<PeerRef> {
        match self.client.resolve_peer(peer_id.to_ambient_ref()).await {
            Ok(peer) => peer
                .to_ref()
                .await
                .map_err(|error| Error::Telegram(error.to_string()))?
                .ok_or_else(|| {
                    Error::NotFound(format!(
                        "Telegram peer {} is not cached",
                        peer_id.bare_id().unwrap_or(0)
                    ))
                }),
            Err(first_error) => {
                // Private channel access hashes are only present after the
                // dialog list has been fetched at least once.
                self.cache_dialogs().await?;
                let peer = self
                    .client
                    .resolve_peer(peer_id.to_ambient_ref())
                    .await
                    .map_err(|_| map_invocation(first_error))?;
                peer.to_ref()
                    .await
                    .map_err(|error| Error::Telegram(error.to_string()))?
                    .ok_or_else(|| {
                        Error::NotFound(format!(
                            "peer {} is not in the dialog list",
                            peer_id.bare_id().unwrap_or(0)
                        ))
                    })
            }
        }
    }

    /// Iterate the dialog list so private-channel access hashes are cached.
    pub async fn cache_dialogs(&self) -> Result<usize> {
        let mut dialogs = self.client.iter_dialogs();
        let mut count = 0usize;
        while let Some(dialog) = dialogs.next().await.map_err(map_invocation)? {
            count += 1;
            let _ = dialog;
            if count >= 3000 {
                break;
            }
        }
        Ok(count)
    }

    /// Resolve a link into a concrete chat/message identity.
    pub async fn resolve_link(&self, input: &str) -> Result<crate::models::ResolvedLink> {
        let parsed = parse_link(input)?;
        let (peer_ref, title) = match &parsed.chat {
            LinkChat::ChannelId(channel_id) => {
                (self.peer_ref_for_channel_id(*channel_id).await?, None)
            }
            LinkChat::Username(username) => {
                let peer = self
                    .client
                    .resolve_username(username)
                    .await
                    .map_err(map_invocation)?
                    .ok_or_else(|| Error::NotFound(format!("username @{username}")))?;
                let title = peer.name().map(|name| name.to_string());
                let peer_ref = peer
                    .to_ref()
                    .await
                    .map_err(|error| Error::Telegram(error.to_string()))?
                    .ok_or_else(|| Error::NotFound(format!("username @{username}")))?;
                (peer_ref, title)
            }
        };

        let chat_id = peer_ref
            .id
            .bot_api_dialog_id()
            .ok_or_else(|| Error::InvalidInput("resolved peer has no dialog id".into()))?;

        Ok(crate::models::ResolvedLink {
            url: input.to_string(),
            chat_id,
            message_id: parsed.message_id,
            title,
        })
    }

    /// Fetch one document by chat id (Bot API format) and message id.
    pub async fn fetch_file(&self, chat_id: i64, message_id: i32) -> Result<TelegramFile> {
        let peer_ref = self.peer_ref_for_bot_api_id(chat_id).await?;
        self.fetch_file_with_peer(peer_ref, chat_id, message_id)
            .await
    }

    /// Fetch one document when the peer reference is already known.
    pub async fn fetch_file_with_peer(
        &self,
        peer_ref: PeerRef,
        chat_id: i64,
        message_id: i32,
    ) -> Result<TelegramFile> {
        let messages = self
            .client
            .get_messages_by_id(peer_ref, &[message_id])
            .await
            .map_err(map_invocation)?;
        let message =
            messages.into_iter().flatten().next().ok_or_else(|| {
                Error::NotFound(format!("message {message_id} in chat {chat_id}"))
            })?;
        if message.peer_id() != peer_ref.id {
            return Err(Error::NotFound(format!(
                "message {message_id} does not belong to chat {chat_id}"
            )));
        }
        let document = document_of(&message).ok_or_else(|| {
            Error::InvalidInput(format!("message {message_id} has no downloadable document"))
        })?;
        Ok(TelegramFile {
            chat_id,
            message_id,
            file_name: file_name_of(&document),
            file_size: document.size().unwrap_or(0) as u64,
            mime_type: document.mime_type().map(|value| value.to_string()),
            document_id: document.id(),
            document,
            peer_ref,
        })
    }

    /// List downloadable documents in a chat, newest first.
    pub async fn scan_chat(&self, chat_id: i64, limit: usize) -> Result<Vec<FileEntry>> {
        let peer_ref = self.peer_ref_for_bot_api_id(chat_id).await?;
        let mut messages = self
            .client
            .iter_messages(peer_ref)
            .limit(limit.clamp(1, 10_000));
        let mut files = Vec::new();
        while let Some(message) = messages.next().await.map_err(map_invocation)? {
            if message.peer_id() != peer_ref.id {
                continue;
            }
            if let Some(document) = document_of(&message) {
                files.push(file_entry_of(&message, &document, false));
            }
        }
        Ok(files)
    }

    /// Search documents in a chat by file name.
    pub async fn search_files(
        &self,
        chat_id: i64,
        query: &str,
        limit: usize,
    ) -> Result<Vec<FileEntry>> {
        if query.trim().is_empty() {
            return self.scan_chat(chat_id, limit).await;
        }
        let peer_ref = self.peer_ref_for_bot_api_id(chat_id).await?;
        let mut messages = self
            .client
            .search_messages(peer_ref)
            .query(query)
            .filter(tl::enums::MessagesFilter::InputMessagesFilterDocument)
            .limit(limit.clamp(1, 10_000));
        let mut files = Vec::new();
        while let Some(message) = messages.next().await.map_err(map_invocation)? {
            if message.peer_id() != peer_ref.id {
                continue;
            }
            if let Some(document) = document_of(&message) {
                files.push(file_entry_of(&message, &document, false));
            }
        }
        Ok(files)
    }

    /// Build a resumable chunk source positioned at `offset`.
    pub fn source(&self, document: &Document, offset: u64) -> source::TelegramChunkSource {
        source::TelegramChunkSource::new(self.client.clone(), document.clone(), offset)
    }

    /// Build a public-API reference for the dialog matching `title` (any case,
    /// substring match). Returns the peer and the exact title when exactly
    /// one dialog matches; fails with the list of candidates otherwise.
    pub async fn resolve_destination_by_title(
        &self,
        title: &str,
    ) -> Result<(PeerRef, Option<String>)> {
        use grammers_client::peer::Peer;
        let needle = title.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return Err(Error::InvalidInput("destination title is empty".into()));
        }
        let mut dialogs = self.client.iter_dialogs();
        let mut exact: Vec<(PeerRef, String)> = Vec::new();
        let mut substring: Vec<(PeerRef, String)> = Vec::new();
        let mut checked = 0usize;
        while let Some(dialog) = dialogs.next().await.map_err(map_invocation)? {
            checked += 1;
            if checked > 5000 {
                break;
            }
            let candidate_title = match dialog.peer() {
                Peer::Channel(channel) => channel.title().to_string(),
                Peer::Community(community) => community.title().to_string(),
                Peer::Group(group) => group
                    .title()
                    .map(|title| title.to_string())
                    .unwrap_or_default(),
                Peer::User(_) => continue,
            };
            let lower = candidate_title.to_ascii_lowercase();
            if lower == needle {
                exact.push((dialog.peer_ref(), candidate_title));
            } else if lower.contains(&needle) {
                substring.push((dialog.peer_ref(), candidate_title));
            }
        }
        if exact.len() == 1 {
            let (peer_ref, title) = exact.pop().expect("one exact candidate");
            return Ok((peer_ref, Some(title)));
        }
        if exact.len() > 1 {
            let listing = exact
                .iter()
                .map(|(peer_ref, title)| {
                    let dialog_id = peer_ref
                        .id
                        .bot_api_dialog_id()
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "?".into());
                    format!("- {title} (chat id {dialog_id})")
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(Error::InvalidInput(format!(
                "several dialogs match the exact title \"{title}\":\n{listing}"
            )));
        }
        if substring.len() == 1 {
            let (peer_ref, title) = substring.pop().expect("one substring candidate");
            return Ok((peer_ref, Some(title)));
        }
        let listing = substring
            .iter()
            .take(20)
            .map(|(peer_ref, title)| {
                let dialog_id = peer_ref
                    .id
                    .bot_api_dialog_id()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "?".into());
                format!("- {title} (chat id {dialog_id})")
            })
            .collect::<Vec<_>>()
            .join("\n");
        if listing.is_empty() {
            Err(Error::NotFound(format!(
                "no dialog found matching title \"{title}\""
            )))
        } else {
            Err(Error::InvalidInput(format!(
                "several dialogs match title \"{title}\":\n{listing}"
            )))
        }
    }

    /// Resolve a peer from a numeric Bot API dialog id (`-100...`).
    pub async fn peer_ref_for_dialog_id(&self, chat_id: i64) -> Result<PeerRef> {
        self.peer_ref_for_bot_api_id(chat_id).await
    }

    /// Resolve a destination for uploads.
    ///
    /// Accepted forms:
    /// - invite links (`https://t.me/+<hash>` or `https://t.me/joinchat/<hash>`),
    /// - public `t.me/<name>` links or `@name`,
    /// - private `t.me/c/<channel_id>/...` links.
    ///
    /// Returns the peer reference plus the channel title when available.
    /// The account must already be a member (invite links do not auto-join
    /// here); if it is not, a clear error is returned.
    pub async fn resolve_destination(&self, dest: &str) -> Result<(PeerRef, Option<String>)> {
        let trimmed = dest.trim();
        let lower = trimmed.to_ascii_lowercase();

        if let Some(hash) = split_invite_hash(&lower) {
            return self.resolve_invite(trimmed, &hash).await;
        }
        if let Some(channel_id) = channel_id_of(&lower) {
            let peer_ref = self.peer_ref_for_channel_id(channel_id).await?;
            return Ok((peer_ref, None));
        }
        let username = lower
            .replace("https://", "")
            .replace("http://", "")
            .replace("www.", "")
            .replace("t.me/", "")
            .replace("telegram.me/", "")
            .trim_start_matches('@')
            .split(['?', '/'])
            .next()
            .unwrap_or("")
            .trim_start_matches('@')
            .to_string();
        if username.is_empty() || username.eq_ignore_ascii_case("c") {
            return Err(Error::InvalidInput(format!(
                "cannot interpret destination: {trimmed}"
            )));
        }
        let peer = self
            .client
            .resolve_username(&username)
            .await
            .map_err(map_invocation)?
            .ok_or_else(|| Error::NotFound(format!("username @{username}")))?;
        let title = peer.name().map(|name| name.to_string());
        let peer_ref = peer
            .to_ref()
            .await
            .map_err(|error| Error::Telegram(error.to_string()))?
            .ok_or_else(|| Error::NotFound(format!("username @{username}")))?;
        Ok((peer_ref, title))
    }

    /// Resolve an invite link for an account that is already a member.
    async fn resolve_invite(&self, link: &str, hash: &str) -> Result<(PeerRef, Option<String>)> {
        // Fast path: importing the invite returns the joined chat (works for
        // most already-member accounts as well).
        match self.client.accept_invite_link(link).await {
            Ok(Some(peer)) => {
                let title = peer.name().map(|name| name.to_string());
                let peer_ref = peer
                    .to_ref()
                    .await
                    .map_err(|error| Error::Telegram(error.to_string()))?
                    .ok_or_else(|| {
                        Error::NotFound("invite resolved but peer reference is missing".into())
                    })?;
                return Ok((peer_ref, title));
            }
            Ok(None) => {} // fall through to the membership probe
            Err(_) => {}   // fall through to the membership probe
        }

        // Membership probe: `messages.checkChatInvite` returns the chat for
        // accounts that already joined (`chatInviteAlready`).
        let result = self
            .client
            .invoke(&tl::functions::messages::CheckChatInvite {
                hash: hash.to_string(),
            })
            .await;

        if let Ok(result) = result {
            match result {
                tl::enums::ChatInvite::Already(already) => {
                    let (channel_id, access_hash, title) = match already.chat {
                        tl::enums::Chat::Channel(channel) => {
                            (channel.id, channel.access_hash, Some(channel.title))
                        }
                        tl::enums::Chat::ChannelForbidden(channel) => {
                            (channel.id, None, Some(channel.title))
                        }
                        tl::enums::Chat::Forbidden(_) => {
                            return Err(Error::NotAuthorized);
                        }
                        _ => {
                            return Err(Error::Other(
                                "invite target is not a broadcast channel".into(),
                            ));
                        }
                    };
                    if let Some(access_hash) = access_hash {
                        let id = PeerId::channel(channel_id).ok_or_else(|| {
                            Error::InvalidInput("channel id is out of range".into())
                        })?;
                        return Ok((
                            PeerRef {
                                id,
                                auth: grammers_session::types::PeerAuth::from_hash(access_hash),
                            },
                            title,
                        ));
                    }
                    let peer_ref = self.peer_ref_for_channel_id(channel_id).await?;
                    return Ok((peer_ref, title));
                }
                tl::enums::ChatInvite::Peek(_) | tl::enums::ChatInvite::Invite(_) => {
                    return Err(Error::Other(
                        "this account is not a member of the destination channel; join it with the official Telegram app first".into(),
                    ));
                }
            }
        }

        // The invite hash is rejected by Telegram (e.g. INVITE_HASH_EXPIRED),
        // but the account may still be a member. Find the channel from the
        // dialog list: the destination is the channel whose recent posts are
        // .edz documents the user already uploaded.
        self.find_edz_destination().await
    }

    /// Fallback destination detection: pick the dialog (channel/group) whose
    /// recent posts are `.edz` documents, or whose title clearly hints at the
    /// EDZ archive. Ambiguity is reported with candidates instead of guessed.
    async fn find_edz_destination(&self) -> Result<(PeerRef, Option<String>)> {
        use grammers_client::peer::Peer;

        fn dialog_title(peer: &Peer) -> String {
            match peer {
                Peer::Channel(channel) => channel.title().to_string(),
                Peer::Group(group) => group
                    .title()
                    .map(|title| title.to_string())
                    .unwrap_or_else(|| "(unnamed group)".to_string()),
                Peer::Community(community) => community.title().to_string(),
                Peer::User(_) => String::new(),
            }
        }

        fn last_message_is_edz(last: &Option<Message>) -> bool {
            let Some(message) = last else {
                return false;
            };
            let Some(media) = message.media() else {
                return false;
            };
            match media {
                Media::Document(document) => document
                    .name()
                    .map(|name| name.to_ascii_lowercase().ends_with(".edz"))
                    .unwrap_or(false),
                _ => false,
            }
        }

        fn has_edz_hint(title: &str) -> bool {
            let lower = title.to_ascii_lowercase();
            ["edz", "eplan", "chint", "oumijia"]
                .iter()
                .any(|hint| lower.contains(hint))
        }

        let mut candidates: Vec<(PeerRef, String)> = Vec::new();
        let mut dialogs = self.client.iter_dialogs();
        let mut checked = 0usize;
        while let Some(dialog) = dialogs.next().await.map_err(map_invocation)? {
            checked += 1;
            if checked > 2500 {
                break;
            }
            let title = dialog_title(dialog.peer());
            let is_candidate = match dialog.peer() {
                Peer::Channel(_) | Peer::Community(_) => {
                    has_edz_hint(&title) || last_message_is_edz(&dialog.last_message)
                }
                Peer::Group(_) => last_message_is_edz(&dialog.last_message),
                Peer::User(_) => false,
            };
            if is_candidate {
                candidates.push((dialog.peer_ref(), title));
            }
        }

        match candidates.len() {
            0 => Err(Error::NotFound(
                "invite link is expired and no channel with recent .edz posts was found; \
                 pass --dest-chat-id <id> with the channel's -100... id instead"
                    .into(),
            )),
            1 => {
                let (peer_ref, title) = candidates.pop().expect("one candidate");
                eprintln!("destination auto-detected from dialog list: {title}");
                Ok((peer_ref, Some(title)))
            }
            _ => {
                // Narrow down by title hints before giving up.
                let hinted: Vec<(PeerRef, String)> = candidates
                    .iter()
                    .filter(|(_, title)| has_edz_hint(title))
                    .cloned()
                    .collect();
                if hinted.len() == 1 {
                    let (peer_ref, title) = hinted.into_iter().next().expect("one hint");
                    eprintln!("destination auto-detected from dialog list: {title}");
                    return Ok((peer_ref, Some(title)));
                }
                let listing = candidates
                    .iter()
                    .map(|(peer_ref, title)| {
                        let dialog_id = peer_ref
                            .id
                            .bot_api_dialog_id()
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| "?".into());
                        format!("- {title} (chat id {dialog_id})")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Err(Error::InvalidInput(format!(
                    "invite link is expired and several candidate channels were found; \
                     re-run with --dest-chat-id <id>:\n{listing}"
                )))
            }
        }
    }
}

/// Extract the invite hash from a `t.me/+<hash>` or `t.me/joinchat/<hash>` link.
fn split_invite_hash(lower: &str) -> Option<String> {
    for marker in ["t.me/+", "t.me/joinchat/"] {
        if let Some(index) = lower.find(marker) {
            let start = index + marker.len();
            // The hash itself is base64url-ish; stop at the first delimiter.
            let hash: String = lower[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !hash.is_empty() {
                return Some(hash);
            }
        }
    }
    None
}

/// Extract a private channel id from `t.me/c/<id>/...` links.
fn channel_id_of(lower: &str) -> Option<i64> {
    let marker = "t.me/c/";
    let start = lower.find(marker)? + marker.len();
    let digits: String = lower[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_private_channel_links() {
        let link = parse_link("https://t.me/c/1188194766/1914").unwrap();
        assert_eq!(link.chat, LinkChat::ChannelId(1188194766));
        assert_eq!(link.message_id, 1914);

        let topic = parse_link("t.me/c/1188194766/77/1914").unwrap();
        assert_eq!(topic.chat, LinkChat::ChannelId(1188194766));
        assert_eq!(topic.message_id, 1914);
    }

    #[test]
    fn parses_public_and_tg_links() {
        let public = parse_link("https://t.me/example_channel/42?single").unwrap();
        assert_eq!(public.chat, LinkChat::Username("example_channel".into()));
        assert_eq!(public.message_id, 42);

        let tg = parse_link("tg://resolve?domain=example_channel&post=42").unwrap();
        assert_eq!(tg.chat, LinkChat::Username("example_channel".into()));
        assert_eq!(tg.message_id, 42);
    }

    #[test]
    fn rejects_non_message_links() {
        assert!(parse_link("https://t.me/example_channel").is_err());
        assert!(parse_link("https://example.com/foo/1").is_err());
        assert!(parse_link("https://t.me/joinchat/abc").is_err());
    }
}
