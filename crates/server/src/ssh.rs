//! Git over SSH: the same fetches and pushes as over HTTP, for clients that
//! show a key instead of a password.
//!
//! A client is let in with any key the application knows, and each command
//! is then put to the authorization hook with that key in place of an
//! `Authorization` header. Nothing but `git-upload-pack` and
//! `git-receive-pack` runs: there is no shell, no forwarding, no files.
//!
//! Fetches speak protocol version 2 only, which every git since 2.26 asks
//! for by itself. A push is the one protocol there is.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use axum::body::Body;
use bytes::Bytes;
use flate2::{Decompress, FlushDecompress, Status};
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};
use russh::server::{Auth, ChannelOpenHandle, Config, Handler, Msg, PublicKeyOffer, Server, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId};
use strata_git::pktline::{self, Pkt, Reader};
use strata_store::RefScope;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use crate::auth::{self, Access, Presented};
use crate::{receive_pack, repo_name, upload_pack, App, HttpError, MAX_FETCH_REQUEST};

/// Reads the node's host key, in OpenSSH's format. A file that is not there
/// is made, with a new Ed25519 key: fine for one node, but several behind
/// one name must all be given the same key, or clients will refuse the
/// second one they meet.
pub fn host_key(path: &Path) -> anyhow::Result<PrivateKey> {
    if path.exists() {
        return russh::keys::load_secret_key(path, None).with_context(|| format!("reading the SSH host key {}", path.display()));
    }
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).context("making an SSH host key")?;
    let text = key.to_openssh(LineEnding::LF).context("encoding the SSH host key")?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| format!("writing the SSH host key {}", path.display()))?;
    io::Write::write_all(&mut file, text.as_bytes())?;
    tracing::warn!("made a new SSH host key at {}: every node behind one name needs the same one", path.display());
    Ok(key)
}

/// Serves git over SSH on `listener` until `stop` resolves, then lets the
/// sessions in flight finish.
pub async fn serve_ssh(app: Arc<App>, listener: tokio::net::TcpListener, key: PrivateKey, stop: impl std::future::Future<Output = ()>) -> io::Result<()> {
    let config = Arc::new(Config {
        keys: vec![key],
        // A wrong key costs nothing to try, and clients try several.
        auth_rejection_time: std::time::Duration::from_millis(200),
        auth_rejection_time_initial: Some(std::time::Duration::ZERO),
        inactivity_timeout: Some(std::time::Duration::from_secs(600)),
        ..Config::default()
    });
    let mut server = Listener { app };
    let mut running = server.run_on_socket(config, &listener);
    let handle = running.handle();
    tokio::pin!(stop);
    tokio::select! {
        ended = &mut running => ended,
        () = &mut stop => {
            handle.shutdown("the server is stopping".into());
            running.await
        }
    }
}

struct Listener {
    app: Arc<App>,
}

impl Server for Listener {
    type Handler = Client;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Client {
        Client { app: self.app.clone(), key: None, channels: HashMap::new(), v2: HashSet::new() }
    }

    fn handle_session_error(&mut self, error: russh::Error) {
        // Scanners and dropped connections, mostly.
        tracing::debug!("ssh session ended: {error}");
    }
}

/// One connection.
struct Client {
    app: Arc<App>,
    /// The key the client proved to hold, as an `authorized_keys` line.
    key: Option<String>,
    /// Sessions opened and not yet given a command.
    channels: HashMap<ChannelId, Channel<Msg>>,
    /// Sessions whose client asked for protocol version 2.
    v2: HashSet<ChannelId>,
}

impl Client {
    /// Hands a session to `run`, which answers it and ends it.
    fn start(&mut self, id: ChannelId, session: &mut Session, command: String) -> Result<(), russh::Error> {
        let Some(channel) = self.channels.remove(&id) else {
            return session.channel_failure(id);
        };
        session.channel_success(id)?;
        tokio::spawn(run(self.app.clone(), channel, command, self.key.clone(), self.v2.contains(&id)));
        Ok(())
    }
}

impl Handler for Client {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, russh::Error> {
        // Only where nothing is asked of anyone over HTTP either.
        let open = self.app.options.authz_url.is_none() && self.app.options.admin_token.is_none();
        Ok(if open { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_publickey_offered(&mut self, _user: &str, offer: PublicKeyOffer<'_>) -> Result<Auth, russh::Error> {
        // A key nobody registered is turned down here, before the client
        // signs with it, so that it goes on to try its next one.
        let known = match offer.public_key.to_openssh() {
            Ok(line) => auth::knows_key(&self.app, line.trim()).await,
            Err(_) => false,
        };
        Ok(if known { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_publickey(&mut self, _user: &str, key: VerifiedPublicKey<'_>) -> Result<Auth, russh::Error> {
        self.key = key.public_key.to_openssh().ok().map(|line| line.trim().to_string());
        Ok(if self.key.is_some() { Auth::Accept } else { Auth::reject() })
    }

    async fn channel_open_session(&mut self, channel: Channel<Msg>, reply: ChannelOpenHandle, _session: &mut Session) -> Result<(), russh::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn env_request(&mut self, channel: ChannelId, name: &str, value: &str, session: &mut Session) -> Result<(), russh::Error> {
        // The one variable git sends: which protocol version it would like.
        if name == "GIT_PROTOCOL" && value.split(':').any(|kv| kv == "version=2") {
            self.v2.insert(channel);
            return session.channel_success(channel);
        }
        session.channel_failure(channel)
    }

    async fn exec_request(&mut self, channel: ChannelId, data: &[u8], session: &mut Session) -> Result<(), russh::Error> {
        self.start(channel, session, String::from_utf8_lossy(data).into_owned())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), russh::Error> {
        self.start(channel, session, String::new())
    }
}

#[derive(Clone, Copy)]
enum Service {
    Upload,
    Receive,
}

/// `git-upload-pack '/acme/app.git'`, as git sends it: the service and the
/// repository's path.
fn parse_command(command: &str) -> Option<(Service, &str)> {
    let command = command.trim();
    let command = command.strip_prefix("git ").map_or(command, |rest| rest.trim_start());
    let (service, path) = command.split_once(' ')?;
    let service = match service.strip_prefix("git-").unwrap_or(service) {
        "upload-pack" => Service::Upload,
        "receive-pack" => Service::Receive,
        _ => return None,
    };
    let path = path.trim();
    let path = path.strip_prefix('\'').and_then(|p| p.strip_suffix('\'')).or_else(|| path.strip_prefix('"').and_then(|p| p.strip_suffix('"'))).unwrap_or(path);
    Some((service, path.trim_start_matches('/')))
}

/// Why a command ended without doing what it was asked.
enum Stop {
    /// Told to the client, on its terminal.
    Refused(String),
    /// The client is gone.
    Hangup,
}

impl From<io::Error> for Stop {
    fn from(_: io::Error) -> Stop {
        Stop::Hangup
    }
}

impl From<HttpError> for Stop {
    fn from(e: HttpError) -> Stop {
        Stop::Refused(e.message)
    }
}

async fn run(app: Arc<App>, channel: Channel<Msg>, command: String, key: Option<String>, v2: bool) {
    let started = Instant::now();
    let (mut read, write) = channel.split();
    let mut input = read.make_reader();
    let mut output = write.make_writer();
    let presented = Presented { authorization: None, ssh_key: key.as_deref() };
    // git-lfs asks over SSH where large files go, and for something to show there.
    if let Some(arguments) = command.trim().strip_prefix("git-lfs-authenticate ") {
        let mut words = arguments.split_whitespace();
        let path = words.next().unwrap_or_default().trim_matches('\'').trim_start_matches('/');
        let outcome = match crate::lfs::authenticate(&app, presented, path, words.next().unwrap_or_default()).await {
            Ok(answer) => output.write_all(answer.as_bytes()).await.map_err(Stop::from),
            Err(e) => Err(e.into()),
        };
        return finish(&app, write, output, "lfs-authenticate", outcome, started).await;
    }
    let (service, outcome) = match parse_command(&command) {
        Some((Service::Upload, path)) => ("upload-pack", upload(&app, presented, path, v2, &mut input, &mut output).await),
        Some((Service::Receive, path)) => ("receive-pack", receive(&app, presented, path, &mut input, &mut output).await),
        None if command.is_empty() => ("other", Err(Stop::Refused("you are signed in, but there is no shell here: use git".into()))),
        None => ("other", Err(Stop::Refused("only git-upload-pack and git-receive-pack are served here".into()))),
    };
    finish(&app, write, output, service, outcome, started).await;
}

/// Ends a command: tells the client why it was refused, if it was, and how it went.
async fn finish(app: &App, write: russh::ChannelWriteHalf<Msg>, mut output: impl AsyncWrite + Unpin, service: &str, outcome: Result<(), Stop>, started: Instant) {
    let _ = output.flush().await;
    let (result, code) = match outcome {
        Ok(()) => ("ok", 0),
        Err(Stop::Hangup) => ("aborted", 1),
        Err(Stop::Refused(message)) => {
            // ssh passes this through to the user's terminal.
            let mut errors = write.make_writer_ext(Some(1));
            let _ = errors.write_all(format!("strata: {}\n", message.trim()).as_bytes()).await;
            let _ = errors.flush().await;
            ("refused", 1)
        }
    };
    app.metrics.ssh(service, result, started.elapsed());
    let _ = write.exit_status(code).await;
    let _ = write.eof().await;
    let _ = write.close().await;
}

/// One request of a client: packets up to and including a flush. `None` when
/// the client has nothing more to ask, which it says with a lone flush or by
/// closing its side.
async fn read_request(input: &mut (impl AsyncRead + Unpin)) -> Result<Option<Bytes>, Stop> {
    let malformed = || Stop::Refused("malformed request".into());
    let mut request = Vec::new();
    loop {
        let mut header = [0u8; 4];
        match input.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && request.is_empty() => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = std::str::from_utf8(&header).ok().and_then(|h| usize::from_str_radix(h, 16).ok()).ok_or_else(malformed)?;
        request.extend_from_slice(&header);
        match len {
            0 => return Ok((request.len() > 4).then(|| request.into())),
            // A delimiter between the parts of a request.
            1 | 2 => {}
            3 => return Err(malformed()),
            _ => {
                let at = request.len();
                request.resize(at + len - 4, 0);
                input.read_exact(&mut request[at..]).await?;
            }
        }
        if request.len() > MAX_FETCH_REQUEST {
            return Err(Stop::Refused("request too large".into()));
        }
    }
}

async fn upload(app: &App, presented: Presented<'_>, path: &str, v2: bool, input: &mut (impl AsyncRead + Unpin), output: &mut (impl AsyncWrite + Unpin)) -> Result<(), Stop> {
    let granted = auth::authorize_git(app, presented, repo_name(path)?, Access::Read).await?;
    if app.meta.get_repo(&granted.repo, &RefScope::none()).await.map_err(HttpError::from)?.is_none() {
        return Err(Stop::Refused("repository not found".into()));
    }
    if !v2 {
        // The older protocol keeps a conversation's state on the server
        // between rounds; over HTTP the client repeats it instead.
        return Err(Stop::Refused("fetching over SSH takes git protocol version 2 (git 2.26 or newer, or -c protocol.version=2); HTTP serves older clients".into()));
    }
    output.write_all(&upload_pack::advertise()).await?;
    output.flush().await?;
    while let Some(request) = read_request(input).await? {
        match upload_pack::handle(app, &granted.repo, request).await {
            Ok(body) => {
                let mut stream = body.into_data_stream();
                while let Some(chunk) = stream.next().await {
                    output.write_all(&chunk.map_err(|_| Stop::Hangup)?).await?;
                }
                output.flush().await?;
            }
            Err(e) => {
                // Inside the conversation, an error is a packet of its own.
                let mut packet = Vec::new();
                pktline::write(&mut packet, format!("ERR {}\n", e.message));
                output.write_all(&packet).await?;
                break;
            }
        }
    }
    Ok(())
}

/// Whether a push's commands are followed by a pack: they are unless all of them delete.
fn brings_a_pack(commands: &[u8]) -> bool {
    let mut reader = Reader::new(commands);
    while let Ok(Some(Pkt::Data(line))) = reader.read() {
        let line = pktline::text(line);
        let command = line.split(|b| *b == 0).next().unwrap_or_default();
        if command.starts_with(b"shallow ") {
            continue;
        }
        // "<old> <new> <ref>": all zeros for <new> deletes the ref.
        if command.get(41..81).is_some_and(|new| new.iter().any(|b| *b != b'0')) {
            return true;
        }
    }
    false
}

async fn receive(app: &App, presented: Presented<'_>, path: &str, input: &mut (impl AsyncRead + Unpin), output: &mut (impl AsyncWrite + Unpin)) -> Result<(), Stop> {
    let granted = auth::authorize_git(app, presented, repo_name(path)?, Access::Write).await?;
    let repo = app.meta.get_repo(&granted.repo, &RefScope::All).await.map_err(HttpError::from)?;
    if repo.is_none() && !app.options.auto_create {
        return Err(Stop::Refused("repository not found".into()));
    }
    output.write_all(&receive_pack::advertise_refs(repo.as_ref())).await?;
    output.flush().await?;
    drop(repo);

    // A client with nothing to push leaves after the list of refs.
    let Some(commands) = read_request(input).await? else {
        return Ok(());
    };
    // The same request as over HTTP is put together from the stream: the
    // commands, then the pack. Nothing marks the pack's end but the pack
    // itself, and the client waits for its answer without closing its side.
    let packed = brings_a_pack(&commands);
    let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(8);
    let pump = async move {
        if tx.send(Ok(commands)).await.is_err() || !packed {
            return;
        }
        let mut end = PackEnd::new();
        let mut chunk = vec![0u8; 64 << 10];
        loop {
            let piece = match input.read(&mut chunk).await {
                Ok(0) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the push was interrupted")),
                Ok(n) => end.feed(&chunk[..n]).map(|ended| (ended.unwrap_or(n), ended.is_some())),
                Err(e) => Err(e),
            };
            match piece {
                Ok((n, ended)) => {
                    if tx.send(Ok(Bytes::copy_from_slice(&chunk[..n]))).await.is_err() || ended {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }
    };
    let (report, ()) = tokio::join!(receive_pack::handle(app, &granted, Body::from_stream(ReceiverStream::new(rx))), pump);
    output.write_all(&report?).await?;
    output.flush().await?;
    Ok(())
}

/// Finds where a pack ends in a stream that goes on after it, by reading the
/// pack as far as that takes: its header says how many objects follow, and
/// each object's compressed data says where it stops.
struct PackEnd {
    state: Part,
    /// The pack's 12-byte header, as far as it has come.
    header: Vec<u8>,
    /// Objects still to come, the current one included.
    objects: u32,
    kind: u8,
    inflate: Decompress,
    /// Where inflated data goes: nowhere.
    sink: Vec<u8>,
}

#[derive(Clone, Copy)]
enum Part {
    Header,
    /// An object's type and size; `true` for the first byte, which holds the type.
    Entry(bool),
    /// The distance to a delta's base in the same pack.
    Offset,
    /// So many bytes to pass over: the id of a delta's base, or the pack's checksum.
    Skip(usize, bool),
    Data,
}

impl PackEnd {
    fn new() -> PackEnd {
        PackEnd { state: Part::Header, header: Vec::with_capacity(12), objects: 0, kind: 0, inflate: Decompress::new(true), sink: vec![0; 32 << 10] }
    }

    /// The part after an object or the header: the next object, or the checksum that closes the pack.
    fn next(&self) -> Part {
        if self.objects == 0 {
            Part::Skip(20, true)
        } else {
            Part::Entry(true)
        }
    }

    /// Takes the next bytes of the stream. `Some(n)`: the pack ends after `n` of them.
    fn feed(&mut self, data: &[u8]) -> io::Result<Option<usize>> {
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
        let mut pos = 0;
        while pos < data.len() {
            match self.state {
                Part::Header => {
                    let take = (12 - self.header.len()).min(data.len() - pos);
                    self.header.extend_from_slice(&data[pos..pos + take]);
                    pos += take;
                    if self.header.len() == 12 {
                        if &self.header[..4] != b"PACK" {
                            return Err(bad("what follows the commands is not a pack"));
                        }
                        self.objects = u32::from_be_bytes([self.header[8], self.header[9], self.header[10], self.header[11]]);
                        self.state = self.next();
                    }
                }
                Part::Entry(first) => {
                    let byte = data[pos];
                    pos += 1;
                    if first {
                        self.kind = (byte >> 4) & 7;
                    }
                    self.state = match (byte & 0x80 != 0, self.kind) {
                        (true, _) => Part::Entry(false),
                        (false, 1..=4) => Part::Data,
                        (false, 6) => Part::Offset,
                        (false, 7) => Part::Skip(20, false),
                        _ => return Err(bad("the pack has an object of no known type")),
                    };
                }
                Part::Offset => {
                    if data[pos] & 0x80 == 0 {
                        self.state = Part::Data;
                    }
                    pos += 1;
                }
                Part::Skip(left, last) => {
                    let take = left.min(data.len() - pos);
                    pos += take;
                    if take < left {
                        self.state = Part::Skip(left - take, last);
                    } else if last {
                        return Ok(Some(pos));
                    } else {
                        self.state = Part::Data;
                    }
                }
                Part::Data => {
                    let (read, written) = (self.inflate.total_in(), self.inflate.total_out());
                    let status = self.inflate.decompress(&data[pos..], &mut self.sink, FlushDecompress::None).map_err(|_| bad("the pack's data is damaged"))?;
                    let used = (self.inflate.total_in() - read) as usize;
                    pos += used;
                    if status == Status::StreamEnd {
                        self.inflate.reset(true);
                        self.objects -= 1;
                        self.state = self.next();
                    } else if used == 0 && self.inflate.total_out() == written {
                        return Err(bad("the pack's data is damaged"));
                    }
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_parsed_as_git_sends_them() {
        assert!(matches!(parse_command("git-upload-pack '/acme/app.git'"), Some((Service::Upload, "acme/app.git"))));
        assert!(matches!(parse_command("git-receive-pack 'acme/app'"), Some((Service::Receive, "acme/app"))));
        assert!(matches!(parse_command("git upload-pack \"/acme/app.git\""), Some((Service::Upload, "acme/app.git"))));
        assert!(parse_command("ls -la").is_none());
        assert!(parse_command("git-upload-archive '/acme/app.git'").is_none());
        assert!(parse_command("").is_none());
    }

    #[test]
    fn a_push_that_only_deletes_brings_no_pack() {
        let (zero, oid) = ("0".repeat(40), "1".repeat(40));
        let mut delete = Vec::new();
        pktline::write(&mut delete, format!("{oid} {zero} refs/heads/gone\0report-status"));
        pktline::flush(&mut delete);
        assert!(!brings_a_pack(&delete));
        let mut update = Vec::new();
        pktline::write(&mut update, format!("{oid} {zero} refs/heads/gone\0report-status"));
        pktline::write(&mut update, format!("{zero} {oid} refs/heads/new"));
        pktline::flush(&mut update);
        assert!(brings_a_pack(&update));
    }
}
