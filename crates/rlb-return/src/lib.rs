#![forbid(unsafe_code)]

//! Optional ordinary virtual participants for a room's return diagnostic.
//! Contact registration runs separately from the UDP owner. No audio device,
//! mixer, sample-rate converter or playout buffer is involved on this host.

mod media;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rlb_contact::{
    ContactClient, ContactEvent, DEFAULT_IO_TIMEOUT, JoinParams, KEEPALIVE_INTERVAL,
    MAX_MEDIA_CANDIDATES, MAX_NAME_CHARS, MediaSocket, MemberContact, ReturnAction, ReturnRequests,
    ReturnState, normalize_media_alternates,
};
use rlb_model::{MemberId, RunId};

pub struct ReturnHostConfig {
    pub contact: SocketAddr,
    pub bind: SocketAddr,
    pub room_name: String,
    pub password: String,
    /// Host-local interface index for IPv6 link-local media destinations.
    pub local_scope: u32,
}

#[must_use = "dropping the host stops its virtual participants"]
pub struct ReturnHost {
    address: SocketAddr,
    bindings: Arc<Mutex<media::BindingSnapshot>>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl ReturnHost {
    pub fn start(config: ReturnHostConfig, requests: ReturnRequests) -> Result<Self, String> {
        let media = media::MediaRuntime::start(scoped_bind(&config)?, config.local_scope)?;
        let address = media.address();
        let bindings = media.binding_handle();
        let stopped = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stopped);
        let thread = thread::Builder::new()
            .name("rlb-return-contact".into())
            .spawn(move || control_loop(config, requests, media, stopping))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            address,
            bindings,
            stopped,
            thread: Some(thread),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addrs().first().copied().unwrap_or(self.address)
    }

    /// All actual UDP listeners, sharing one port and one media owner.
    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .addresses
            .clone()
    }

    /// Source addresses the operating system could not bind; other paths remain usable.
    pub fn binding_warnings(&self) -> Vec<String> {
        self.bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .warnings
            .clone()
    }

    /// Allows the process supervisor to observe a failed background owner.
    pub fn is_running(&self) -> bool {
        !self.stopped.load(Ordering::Acquire)
            && self
                .thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
    }

    pub fn stop(&mut self) -> Result<(), String> {
        self.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            thread
                .join()
                .map_err(|_| "The return contact owner panicked.".to_owned())?
        } else {
            Ok(())
        }
    }
}

impl Drop for ReturnHost {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct VirtualContact {
    origin: MemberContact,
    params: JoinParams,
    paths: Vec<media::ReturnPath>,
    client: Option<ContactClient>,
}

fn control_loop(
    config: ReturnHostConfig,
    requests: ReturnRequests,
    mut media: media::MediaRuntime,
    stopped: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut members = HashMap::<MemberId, VirtualContact>::new();
    let mut refresh = HashMap::<MemberId, MemberContact>::new();
    let mut last_keepalive = Instant::now();
    let mut last_interfaces = interface_signature();
    while !stopped.load(Ordering::Acquire) {
        if !media.is_running() {
            return media
                .stop()
                .and(Err("The return UDP owner stopped unexpectedly.".into()));
        }
        while let Some(request) = requests.try_recv() {
            if stopped.load(Ordering::Acquire) {
                break;
            }
            let key = request.origin.member_id;
            let result = match request.action {
                ReturnAction::Start => {
                    refresh.remove(&key);
                    start_member(&config, &mut media, &mut members, request.origin)
                }
                ReturnAction::Status => Ok(media.state(key, request.origin.run_id)),
                ReturnAction::Stop => {
                    if members
                        .get(&key)
                        .is_some_and(|v| v.origin.run_id == request.origin.run_id)
                    {
                        refresh.remove(&key);
                        remove_member(key, &mut members, &mut media);
                    }
                    Ok(empty_state())
                }
            };
            let state = result.unwrap_or_else(|error| ReturnState {
                member: None,
                mappings: Vec::new(),
                error: Some(bounded_error(error)),
            });
            let _ = requests.respond(request.id, state);
        }
        let keepalive = last_keepalive.elapsed() >= KEEPALIVE_INTERVAL;
        let mut retired = Vec::new();
        for (&id, member) in &mut members {
            if stopped.load(Ordering::Acquire) {
                break;
            }
            let mut disconnected = false;
            if let Some(client) = &mut member.client {
                loop {
                    match client.poll(Duration::ZERO) {
                        Ok(Some(ContactEvent::ExplicitLeave {
                            member_id, run_id, ..
                        })) if member_id == member.origin.member_id
                            && run_id == member.origin.run_id =>
                        {
                            retired.push(id)
                        }
                        Ok(Some(ContactEvent::Superseded | ContactEvent::LeaveAccepted)) => {
                            retired.push(id)
                        }
                        Ok(Some(ContactEvent::MemberVisible(origin)))
                            if origin.member_id == member.origin.member_id =>
                        {
                            if origin.run_id != member.origin.run_id {
                                retired.push(id);
                            } else if origin != member.origin {
                                refresh.insert(id, origin);
                            }
                        }
                        Ok(Some(ContactEvent::Snapshot(snapshot))) => {
                            if let Some(origin) = snapshot
                                .members
                                .into_iter()
                                .find(|value| value.member_id == member.origin.member_id)
                            {
                                if origin.run_id != member.origin.run_id {
                                    retired.push(id);
                                } else if origin != member.origin {
                                    refresh.insert(id, origin);
                                }
                            }
                        }
                        Ok(Some(ContactEvent::ServiceStopped)) | Err(_) => {
                            disconnected = true;
                            break;
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                    }
                }
                if keepalive && !disconnected && client.keepalive().is_err() {
                    disconnected = true;
                }
            }
            if disconnected {
                member.client = None;
            }
            if keepalive && member.client.is_none() {
                refresh.entry(id).or_insert_with(|| member.origin.clone());
            }
        }
        for id in retired {
            refresh.remove(&id);
            remove_member(id, &mut members, &mut media);
        }
        if keepalive {
            let signature = interface_signature();
            if signature != last_interfaces || media.needs_network_retry() {
                last_interfaces = signature;
                let _ = media.refresh_network();
                for (&id, member) in &members {
                    refresh.entry(id).or_insert_with(|| member.origin.clone());
                }
            }
            let pending = refresh.values().cloned().collect::<Vec<_>>();
            for origin in pending {
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                let id = origin.member_id;
                if start_member(&config, &mut media, &mut members, origin).is_ok() {
                    refresh.remove(&id);
                }
            }
            last_keepalive = Instant::now();
        }
        if stopped.load(Ordering::Acquire) {
            break;
        }
        thread::park_timeout(Duration::from_millis(20));
    }
    for id in members.keys().copied().collect::<Vec<_>>() {
        remove_member(id, &mut members, &mut media);
    }
    media.stop()
}

fn empty_state() -> ReturnState {
    ReturnState {
        member: None,
        mappings: Vec::new(),
        error: None,
    }
}

fn start_member(
    config: &ReturnHostConfig,
    media: &mut media::MediaRuntime,
    members: &mut HashMap<MemberId, VirtualContact>,
    origin: MemberContact,
) -> Result<ReturnState, String> {
    let bounds = media.addresses();
    let mut plan = route_plan(config, &bounds, &origin)?;
    if let Some(old) = members.get(&origin.member_id) {
        if let Some(ip) = old.params.media_ip {
            if let Ok(primary) =
                MediaSocket::explicit(ip, old.params.media_port, old.params.media_scope_id)
            {
                if let Some(at) = plan.local_media.iter().position(|entry| *entry == primary) {
                    let primary = plan.local_media.remove(at);
                    plan.local_media.insert(0, primary);
                }
            }
        }
    }
    let primary = plan.local_media[0];
    let alternates =
        normalize_media_alternates(primary.port(), Some(primary), &plan.local_media[1..])
            .map_err(|error| error.to_string())?;
    if let Some(existing) = members.get(&origin.member_id) {
        if existing.origin == origin
            && existing.paths == plan.paths
            && existing.client.is_some()
            && existing.params.media_ip == Some(primary.ip())
            && existing.params.media_scope_id == primary.scope_id()
            && existing.params.media_alternates == alternates
        {
            return Ok(media.state(origin.member_id, origin.run_id));
        }
    }
    let preserved = members
        .get(&origin.member_id)
        .filter(|v| v.origin.run_id == origin.run_id)
        .map(|v| (v.params.member_id, v.params.run_id));
    if preserved.is_none() {
        remove_member(origin.member_id, members, media);
    }
    let (member_id, run_id) = match preserved {
        Some(ids) => ids,
        None => (
            MemberId::generate().map_err(|e| e.to_string())?,
            RunId::generate().map_err(|e| e.to_string())?,
        ),
    };
    let suffix = " (回传)";
    let name: String = origin
        .display_name
        .chars()
        .take(MAX_NAME_CHARS.saturating_sub(suffix.chars().count()))
        .collect::<String>()
        + suffix;
    let params = JoinParams {
        room_name: config.room_name.clone(),
        password: config.password.clone(),
        member_id,
        run_id,
        display_name: name,
        media_port: primary.port(),
        media_ip: Some(primary.ip()),
        media_scope_id: primary.scope_id(),
        media_alternates: alternates,
    };
    let mut client =
        ContactClient::connect(config.contact, DEFAULT_IO_TIMEOUT).map_err(|e| e.to_string())?;
    let joined = client.join(params.clone()).map_err(|e| e.to_string())?;
    let member = joined.value.you;
    if let Err(error) = media.add(origin.clone(), member, plan.paths.clone()) {
        let _ = client.leave();
        return Err(error);
    }
    let state = media.state(origin.member_id, origin.run_id);
    members.insert(
        origin.member_id,
        VirtualContact {
            origin,
            params,
            paths: plan.paths,
            client: Some(client),
        },
    );
    Ok(state)
}

struct RoutePlan {
    paths: Vec<media::ReturnPath>,
    local_media: Vec<MediaSocket>,
}

fn is_link_local(ip: IpAddr) -> bool {
    matches!(ip,IpAddr::V6(ip) if (ip.segments()[0]&0xffc0)==0xfe80)
}

fn interface_signature() -> String {
    match if_addrs::get_if_addrs() {
        Ok(values) => {
            let mut signature = values
                .into_iter()
                .map(|value| {
                    format!(
                        "{}|{:?}|{}|{}",
                        value.name,
                        value.index,
                        value.ip(),
                        value.is_oper_up()
                    )
                })
                .collect::<Vec<_>>();
            signature.sort();
            signature.join(";")
        }
        Err(error) => format!("interface enumeration unavailable: {error}"),
    }
}

fn scoped_bind(config: &ReturnHostConfig) -> Result<SocketAddr, String> {
    let mut bind = config.bind;
    if let SocketAddr::V6(value) = &mut bind {
        if is_link_local(IpAddr::V6(*value.ip())) && value.scope_id() == 0 {
            let mut indices = if config.local_scope != 0 {
                vec![config.local_scope]
            } else {
                if_addrs::get_if_addrs()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter(|entry| entry.ip() == IpAddr::V6(*value.ip()) && entry.is_oper_up())
                    .filter_map(|entry| entry.index)
                    .collect::<Vec<_>>()
            };
            indices.sort_unstable();
            indices.dedup();
            if indices.len() != 1 {
                return Err(
                    "A fixed link-local return listener needs one explicit local interface scope."
                        .into(),
                );
            }
            value.set_scope_id(indices[0]);
        }
    }
    Ok(bind)
}

fn route_plan(
    config: &ReturnHostConfig,
    bounds: &[SocketAddr],
    origin: &MemberContact,
) -> Result<RoutePlan, String> {
    let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
    let loopback_origin = origin
        .media_candidates()
        .all(|candidate| candidate.ip().is_loopback());
    let allowed = bounds
        .iter()
        .copied()
        .filter(|bound| bound.ip().is_loopback() == loopback_origin)
        .collect::<Vec<_>>();
    if allowed.is_empty() {
        return Err("No return listener is available in this member's local or LAN scope.".into());
    }
    let mut indices = if config.local_scope != 0 {
        vec![config.local_scope]
    } else {
        interfaces
            .iter()
            .filter(|entry| {
                entry.ip().is_ipv6()
                    && entry.is_oper_up()
                    && allowed.iter().any(|bound| bound.ip() == entry.ip())
            })
            .filter_map(|entry| entry.index)
            .filter(|index| *index != 0)
            .collect::<Vec<_>>()
    };
    if config.local_scope == 0 {
        indices.extend(allowed.iter().filter_map(|bound| match bound {
            SocketAddr::V6(value) if value.scope_id() != 0 => Some(value.scope_id()),
            _ => None,
        }));
    }
    indices.sort_unstable();
    indices.dedup();
    let mut first = Vec::new();
    let mut fallback = Vec::new();
    let mut failures = Vec::new();
    for candidate in origin.media_candidates() {
        let scopes = if is_link_local(candidate.ip()) {
            indices.clone()
        } else {
            vec![0]
        };
        let mut choices = Vec::new();
        for scope in scopes {
            let remote = match candidate.socket_addr_on(scope) {
                Ok(value) => value,
                Err(error) => {
                    failures.push(error.to_string());
                    continue;
                }
            };
            let mut preferred = None;
            let unspecified = SocketAddr::new(
                if remote.is_ipv4() {
                    "0.0.0.0".parse().expect("literal IP")
                } else {
                    "::".parse().expect("literal IP")
                },
                0,
            );
            if let Ok(route) = UdpSocket::bind(unspecified) {
                if route.connect(remote).is_ok() {
                    if let Ok(local) = route.local_addr() {
                        preferred=allowed.iter().copied().find(|bound|bound.ip()==local.ip()
                            &&(!is_link_local(local.ip())||matches!(bound,SocketAddr::V6(value)if value.scope_id()==scope)));
                    }
                }
            }
            let mut locals = allowed
                .iter()
                .copied()
                .filter(|bound| bound.is_ipv4() == remote.is_ipv4())
                .collect::<Vec<_>>();
            locals.sort_by_key(|bound| Some(*bound) != preferred);
            for local in locals {
                if is_link_local(remote.ip()) {
                    let index = match local {
                        SocketAddr::V6(value) if value.scope_id() != 0 => Some(value.scope_id()),
                        _ => interfaces
                            .iter()
                            .find(|entry| entry.ip() == local.ip())
                            .and_then(|entry| entry.index),
                    };
                    if index.is_some_and(|index| index != scope)
                        || (index.is_none() && config.local_scope != scope)
                    {
                        continue;
                    }
                }
                let mut temporary = local;
                temporary.set_port(0);
                let usable = (|| -> std::io::Result<()> {
                    let route = UdpSocket::bind(temporary)?;
                    route.connect(remote)
                })();
                match usable {
                    Ok(()) => {
                        let path = media::ReturnPath { remote, local };
                        if !choices.contains(&path) {
                            choices.push(path);
                        }
                    }
                    Err(error) => failures.push(error.to_string()),
                }
            }
        }
        if !choices.is_empty() {
            let selected = choices.remove(0);
            if !first.contains(&selected) {
                first.push(selected);
            }
            fallback.extend(choices);
        }
    }
    for path in fallback {
        if first.len() >= media::MAX_RETURN_PATHS {
            break;
        }
        if !first.contains(&path) {
            first.push(path);
        }
    }
    first.truncate(media::MAX_RETURN_PATHS);
    if first.is_empty() {
        return Err(bounded_error(format!(
            "No return path matches the host listeners and origin candidates. {}",
            failures.into_iter().take(2).collect::<Vec<_>>().join("; ")
        )));
    }
    let mut local_media = Vec::new();
    let primary = first[0].local;
    for bound in std::iter::once(primary).chain(allowed.into_iter()) {
        let scope = match bound {
            SocketAddr::V6(value) if is_link_local(bound.ip()) => value.scope_id(),
            _ => 0,
        };
        let own = MediaSocket::explicit(bound.ip(), bound.port(), scope)
            .map_err(|error| error.to_string())?;
        if !local_media.contains(&own) {
            local_media.push(own);
        }
    }
    local_media.truncate(MAX_MEDIA_CANDIDATES);
    Ok(RoutePlan {
        paths: first,
        local_media,
    })
}

fn bounded_error(mut value: String) -> String {
    if value.len() > 900 {
        let mut end = 900;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        value.push_str("...");
    }
    value
}

fn remove_member(
    id: MemberId,
    members: &mut HashMap<MemberId, VirtualContact>,
    media: &mut media::MediaRuntime,
) {
    media.remove(id);
    if let Some(mut member) = members.remove(&id) {
        if let Some(client) = &mut member.client {
            let _ = client.leave();
        }
    }
}
