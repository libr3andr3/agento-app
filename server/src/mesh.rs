//! yaya mesh — a post-quantum p2p VPN between agents.
//!
//! Every agent carries two extra keys next to its Ed25519 identity: a
//! WireGuard (X25519) key and an ML-KEM-768 (FIPS 203) encapsulation key.
//! A link between two agents is made over the relay they already talk
//! through (E2E encrypted, signed): the initiator encapsulates to the
//! peer's ML-KEM key and both sides derive the tunnel's **PreSharedKey**
//! from the shared secret (HKDF-SHA256). WireGuard's own Noise handshake
//! stays X25519, so the tunnel is a hybrid: classical + post-quantum, and
//! "harvest now, decrypt later" buys nothing.
//!
//! Rendezvous is the gateway (`/v1/mesh/*`): it allocates each agent one
//! address in `10.77.0.0/16`, keeps the published keys and reported
//! endpoints, and hands out relay port pairs on the public relay
//! (`yaya-relay`) so two phones behind NATs still meet. The relay never sees
//! plaintext — it forwards WireGuard datagrams.
//!
//! On the mesh every agent also answers **A2A** (Agent-to-Agent, JSON-RPC)
//! on `http://<mesh ip>:7770/a2a` with a card at `/.well-known/agent.json`,
//! so servers and phones talk agent to agent without the relay at all.
//!
//! Linux (the runtime) applies the WireGuard config itself when it can
//! (`MESH_APPLY=1`, root or `sudo -n`); Android reads `/api/mesh/config`
//! and drives the tunnel through `VpnService` (wireguard-android).

use std::sync::{atomic::{AtomicU64, Ordering}, Mutex};

use axum::{extract::{ConnectInfo, State}, http::StatusCode, response::IntoResponse, Json, Router};
use base64::Engine;
use ml_kem::{Decapsulate, Encapsulate, KeyExport, MlKem768, TryKeyInit};
use serde_json::{json, Value};

use crate::{AppState, SharedState};

pub const IFACE: &str = "yaya0";
pub const SUBNET: &str = "10.77.0.0/16";
pub const A2A_PORT: u16 = 7770;
pub const KEEPALIVE: u16 = 25;
const KIND_OFFER: &str = "mesh_offer";
const KIND_ACCEPT: &str = "mesh_accept";
const KIND_DECLINE: &str = "mesh_decline";
const REFLECT_MAGIC: &[u8] = b"YRFL";

type Dk = ml_kem::DecapsulationKey<MlKem768>;
type Ek = ml_kem::EncapsulationKey<MlKem768>;

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}
fn unb64(s: &str) -> anyhow::Result<Vec<u8>> {
    Ok(base64::engine::general_purpose::STANDARD.decode(s.trim())?)
}

/// This device's mesh keys: the WireGuard secret and the ML-KEM seed
/// (d‖z, 64 bytes) the decapsulation key is re-derived from.
#[derive(Clone)]
pub struct MeshKeys {
    wg: [u8; 32],
    seed: [u8; 64],
}

impl MeshKeys {
    pub fn generate() -> Self {
        let mut wg = [0u8; 32];
        let mut seed = [0u8; 64];
        getrandom_fill(&mut wg);
        getrandom_fill(&mut seed);
        // Clamp like WireGuard does.
        wg[0] &= 248;
        wg[31] &= 127;
        wg[31] |= 64;
        Self { wg, seed }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = self.wg.to_vec();
        v.extend_from_slice(&self.seed);
        v
    }
    pub fn from_bytes(b: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(b.len() == 96, "mesh keys: bad length {}", b.len());
        Ok(Self { wg: b[..32].try_into()?, seed: b[32..].try_into()? })
    }
    fn kem(&self) -> Dk {
        Dk::from_seed(ml_kem::Seed::from(self.seed))
    }
    pub fn wg_private_b64(&self) -> String {
        b64(&self.wg)
    }
    pub fn wg_public_b64(&self) -> String {
        let sk = x25519_dalek::StaticSecret::from(self.wg);
        b64(x25519_dalek::PublicKey::from(&sk).as_bytes())
    }
    pub fn kem_public_b64(&self) -> String {
        b64(self.kem().encapsulation_key().to_bytes().as_slice())
    }
    /// The shared secret behind `ct_b64`, encapsulated to our ML-KEM key.
    pub fn decapsulate(&self, ct_b64: &str) -> anyhow::Result<[u8; 32]> {
        let ct = unb64(ct_b64)?;
        let ss = self.kem().decapsulate_slice(&ct).map_err(|_| anyhow::anyhow!("bad ciphertext length"))?;
        Ok(ss.as_slice().try_into()?)
    }
}

/// Encapsulate to a peer's published ML-KEM key: `(ct_b64, shared_secret)`.
pub fn encapsulate(ek_b64: &str) -> anyhow::Result<(String, [u8; 32])> {
    let ek = unb64(ek_b64)?;
    let key = ml_kem::Key::<Ek>::try_from(ek.as_slice()).map_err(|_| anyhow::anyhow!("bad ML-KEM key ({} bytes)", ek.len()))?;
    let ek = <Ek as TryKeyInit>::new(&key).map_err(|_| anyhow::anyhow!("invalid ML-KEM key"))?;
    let (ct, ss) = ek.encapsulate();
    Ok((b64(ct.as_slice()), ss.as_slice().try_into()?))
}

/// The tunnel's PreSharedKey: HKDF-SHA256 over the ML-KEM secret, bound to
/// both identities and both WireGuard keys so a secret never serves two links.
pub fn psk_b64(ss: &[u8; 32], initiator: &str, responder: &str, wg_i: &str, wg_r: &str) -> String {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(b"yaya-mesh-psk-v1"), ss);
    let mut out = [0u8; 32];
    hk.expand(format!("{initiator}|{responder}|{wg_i}|{wg_r}").as_bytes(), &mut out).expect("32 bytes");
    b64(&out)
}

fn getrandom_fill(buf: &mut [u8]) {
    use rand_core::RngCore;
    rand_core::OsRng.fill_bytes(buf);
}

// ------------------------------------------------------ peer-supplied data
//
// Offers and accepts come from other agents: their address, key, name and
// endpoints end up in a WireGuard config and decide who an A2A caller is.
// Nothing from a peer is written until it has the one shape it may have.

/// A mesh address: plain IPv4 inside 10.77.0.0/16, nothing else.
pub fn valid_mesh_ip(s: &str) -> bool {
    s.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.octets()[0] == 10 && ip.octets()[1] == 77)
}

/// A WireGuard public key: base64 of exactly 32 bytes.
pub fn valid_wg_key(s: &str) -> bool {
    base64::engine::general_purpose::STANDARD.decode(s).is_ok_and(|b| b.len() == 32)
}

/// `host:port` with a hostname or IPv4 host, no spaces or control bytes.
pub fn valid_endpoint(s: &str) -> bool {
    let Some((host, port)) = s.rsplit_once(':') else { return false };
    !host.is_empty() && host.len() <= 253 && port.parse::<u16>().is_ok_and(|p| p > 0)
        && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// A display name: one line, printable, 60 characters at most.
pub fn clean_name(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect()
}

fn endpoints_of(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str()).filter(|e| valid_endpoint(e)).map(String::from).take(8).collect()).unwrap_or_default()
}

/// Whether another linked peer already holds `ip`.
async fn ip_taken(db: &crate::db::Db, ip: &str, by_other_than: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mesh_peers WHERE ip = $1 AND agent <> $2 AND status = 'linked'")
        .bind(ip).bind(by_other_than).fetch_one(db).await.map(|n| n > 0).unwrap_or(true)
}

// ------------------------------------------------------------ wg config

#[derive(Clone, Debug, serde::Serialize)]
pub struct PeerConf {
    pub name: String,
    pub public: String,
    pub psk: String,
    pub endpoint: Option<String>,
    pub allowed_ips: Vec<String>,
}

/// A wg-quick file. `for_sync` leaves out what `wg syncconf` rejects.
pub fn wg_config(private_b64: &str, address_cidr: Option<&str>, listen_port: Option<u16>, peers: &[PeerConf], for_sync: bool) -> String {
    let mut s = String::from("[Interface]\n");
    s.push_str(&format!("PrivateKey = {private_b64}\n"));
    if let Some(p) = listen_port {
        s.push_str(&format!("ListenPort = {p}\n"));
    }
    if !for_sync {
        if let Some(a) = address_cidr {
            s.push_str(&format!("Address = {a}\n"));
        }
        s.push_str("MTU = 1380\n");
    }
    for p in peers {
        // Defence in depth: whatever reached the table, a peer can only ever
        // fill its own lines.
        let ips_ok = p.allowed_ips.iter().all(|a| a.strip_suffix("/32").is_some_and(valid_mesh_ip));
        if !valid_wg_key(&p.public) || !valid_wg_key(&p.psk) || !ips_ok {
            continue;
        }
        s.push_str(&format!("\n# {}\n[Peer]\nPublicKey = {}\nPresharedKey = {}\n", clean_name(&p.name), p.public, p.psk));
        if let Some(e) = p.endpoint.as_deref().filter(|e| valid_endpoint(e)) {
            s.push_str(&format!("Endpoint = {e}\n"));
        }
        s.push_str(&format!("AllowedIPs = {}\nPersistentKeepalive = {KEEPALIVE}\n", p.allowed_ips.join(", ")));
    }
    s
}

// ----------------------------------------------------------------- state

pub struct Mesh {
    pub keys: MeshKeys,
    pub listen_port: u16,
    /// Our mesh address, once the gateway assigned one.
    pub ip: Mutex<Option<String>>,
    /// Bumped whenever the WireGuard config changes; the phone re-applies.
    pub version: AtomicU64,
    pub a2a_tasks: Mutex<std::collections::HashMap<String, Value>>,
    pub a2a_bound: Mutex<Option<String>>,
}

pub async fn load_or_create(db: &crate::db::Db) -> anyhow::Result<Mesh> {
    let row: Option<(Vec<u8>, i64)> = sqlx::query_as("SELECT keys, listen_port FROM mesh_keys WHERE id = 1").fetch_optional(db).await?;
    let (keys, port) = match row {
        Some((blob, port)) => (MeshKeys::from_bytes(&crate::identity::from_rest(&blob, Some(96))?)?, port as u16),
        None => {
            let k = MeshKeys::generate();
            let mut b = [0u8; 2];
            getrandom_fill(&mut b);
            let port = std::env::var("MESH_LISTEN_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(40000 + (u16::from_le_bytes(b) % 20000));
            sqlx::query("INSERT INTO mesh_keys (id, keys, listen_port) VALUES (1, $1, $2)")
                .bind(crate::identity::at_rest(&k.to_bytes())?).bind(port as i64).execute(db).await?;
            tracing::info!(wg = %k.wg_public_b64(), port, "mesh keys minted");
            (k, port)
        }
    };
    let ip: Option<(String,)> = sqlx::query_as("SELECT value FROM mesh_state WHERE key = 'ip'").fetch_optional(db).await?;
    Ok(Mesh { keys, listen_port: port, ip: Mutex::new(ip.map(|r| r.0)), version: AtomicU64::new(1), a2a_tasks: Default::default(), a2a_bound: Mutex::new(None) })
}

async fn set_state(db: &crate::db::Db, key: &str, value: &str) {
    let _ = sqlx::query("INSERT INTO mesh_state (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value").bind(key).bind(value).execute(db).await;
}
async fn get_state(db: &crate::db::Db, key: &str) -> Option<String> {
    sqlx::query_as::<_, (String,)>("SELECT value FROM mesh_state WHERE key = $1").bind(key).fetch_optional(db).await.ok().flatten().map(|r| r.0)
}

pub fn my_ip(state: &AppState) -> Option<String> {
    state.mesh.ip.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

// ------------------------------------------------------------- rendezvous

/// Ask the public reflector what our UDP `listen_port` looks like from the
/// internet. The probe uses the tunnel's own port so the NAT mapping WireGuard
/// inherits is the one we publish; while the tunnel is up the port is taken
/// and we keep the last answer.
pub async fn reflect(reflect_addr: &str, listen_port: u16) -> Option<String> {
    let sock = tokio::net::UdpSocket::bind(("0.0.0.0", listen_port)).await.ok()?;
    sock.send_to(REFLECT_MAGIC, reflect_addr).await.ok()?;
    let mut buf = [0u8; 64];
    let (n, _) = tokio::time::timeout(std::time::Duration::from_secs(3), sock.recv_from(&mut buf)).await.ok()?.ok()?;
    let s = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    s.contains(':').then_some(s)
}

/// Local (LAN) candidates: every non-loopback IPv4 with our listen port.
fn local_endpoints(listen_port: u16) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(s) = std::fs::read_to_string("/proc/net/fib_trie") {
        for line in s.lines() {
            let t = line.trim();
            if let Some(ip) = t.strip_prefix("|-- ").or_else(|| t.strip_prefix("+-- ")) {
                let ip = ip.split('/').next().unwrap_or("");
                if ip.starts_with("10.77.") || ip.starts_with("127.") || ip.ends_with(".0") || ip.ends_with(".255") || !ip.contains('.') { continue; }
                if ip.starts_with("192.168.") || ip.starts_with("10.") || ip.starts_with("172.") {
                    let e = format!("{ip}:{listen_port}");
                    if !out.contains(&e) { out.push(e); }
                }
            }
        }
    }
    out.truncate(4);
    out
}

/// Registers (or refreshes) this agent on the rendezvous: keys, port,
/// endpoints. Returns the gateway's answer (our mesh address included).
pub async fn register(state: &AppState) -> anyhow::Result<Value> {
    let info = state.registry.get("/v1/mesh/info", std::time::Duration::from_secs(15)).await?;
    let mut endpoints = local_endpoints(state.mesh.listen_port);
    if let Some(r) = info["reflect"].as_str() {
        if let Some(e) = reflect(r, state.mesh.listen_port).await {
            endpoints.insert(0, e);
        } else if let Some(prev) = get_state(&state.db, "public_endpoint").await {
            endpoints.insert(0, prev);
        }
    }
    if let Some(e) = endpoints.first() {
        if !e.starts_with("192.168.") && !e.starts_with("10.") && !e.starts_with("172.") {
            set_state(&state.db, "public_endpoint", e).await;
        }
    }
    let hostname = std::env::var("HOSTNAME").ok().or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()));
    let body = json!({
        "wg": state.mesh.keys.wg_public_b64(), "kem": state.mesh.keys.kem_public_b64(),
        "listenPort": state.mesh.listen_port, "endpoints": endpoints, "hostname": hostname,
        "a2a": A2A_PORT,
    });
    let v = state.registry.post("/v1/mesh/register", &body, std::time::Duration::from_secs(20)).await?;
    if let Some(ip) = v["ip"].as_str() {
        *state.mesh.ip.lock().unwrap_or_else(|e| e.into_inner()) = Some(ip.to_string());
        set_state(&state.db, "ip", ip).await;
        set_state(&state.db, "registered_at", &chrono::Utc::now().to_rfc3339()).await;
    }
    tracing::info!(ip = ?v["ip"].as_str(), endpoints = ?endpoints, "mesh registered");
    Ok(v)
}

// --------------------------------------------------------------- linking

#[derive(sqlx::FromRow)]
struct PeerRow {
    agent: String,
    name: Option<String>,
    wg: Option<String>,
    psk: Option<Vec<u8>>,
    ip: Option<String>,
    endpoint: Option<String>,
    endpoints: Option<String>,
    relay: Option<String>,
    offer: Option<String>,
    status: String,
    role: Option<String>,
    updated_at: String,
}

const PEER_COLS: &str = "agent, name, wg, psk, ip, endpoint, endpoints, relay, offer, status, role, updated_at";

async fn peer(db: &crate::db::Db, agent: &str) -> Option<PeerRow> {
    sqlx::query_as::<_, PeerRow>(&format!("SELECT {PEER_COLS} FROM mesh_peers WHERE agent = $1")).bind(agent).fetch_optional(db).await.ok().flatten()
}

async fn peers(db: &crate::db::Db) -> Vec<PeerRow> {
    sqlx::query_as::<_, PeerRow>(&format!("SELECT {PEER_COLS} FROM mesh_peers ORDER BY updated_at DESC")).fetch_all(db).await.unwrap_or_default()
}

/// Which endpoint we dial for a peer: the relay pair when we have one
/// (always reachable), else their best reported endpoint.
fn choose_endpoint(relay: Option<&Value>, reported: &[String], prefer_direct: bool) -> Option<String> {
    let relay_ep = relay.and_then(|r| Some(format!("{}:{}", r["host"].as_str()?, r["myPort"].as_u64()?))).filter(|e| valid_endpoint(e));
    let direct = reported.iter().filter(|e| valid_endpoint(e)).find(|e| !e.starts_with("192.168.") && !e.starts_with("10.") && !e.starts_with("172.")).cloned().or_else(|| reported.iter().find(|e| valid_endpoint(e)).cloned());
    if prefer_direct { direct.or(relay_ep) } else { relay_ep.or(direct) }
}

/// Owner action: link with `peer_agent`. Fetches the peer's mesh record,
/// encapsulates, asks the gateway for a relay pair, sends the offer.
pub async fn link(state: &SharedState, peer_agent: &str, prefer_direct: bool) -> anyhow::Result<Value> {
    if my_ip(state).is_none() {
        register(state).await?;
    }
    let me = state.identity.id();
    let rec = state.registry.get(&format!("/v1/mesh/agents/{peer_agent}"), std::time::Duration::from_secs(15)).await
        .map_err(|e| anyhow::anyhow!("peer is not on the mesh yet ({e})"))?;
    let (peer_wg, peer_kem, peer_ip) = (
        rec["wg"].as_str().ok_or_else(|| anyhow::anyhow!("peer has no WireGuard key"))?.to_string(),
        rec["kem"].as_str().ok_or_else(|| anyhow::anyhow!("peer has no ML-KEM key"))?.to_string(),
        rec["ip"].as_str().ok_or_else(|| anyhow::anyhow!("peer has no mesh address"))?.to_string(),
    );
    anyhow::ensure!(valid_wg_key(&peer_wg) && valid_mesh_ip(&peer_ip), "the registry record for {peer_agent} is malformed");
    let reported: Vec<String> = rec["endpoints"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let (ct, ss) = encapsulate(&peer_kem)?;
    let my_wg = state.mesh.keys.wg_public_b64();
    let psk = psk_b64(&ss, &me, peer_agent, &my_wg, &peer_wg);
    // Relay pair: the gateway allocates two ports on the public relay; we
    // dial `myPort`, the peer dials `peerPort`.
    let relay = state.registry.post("/v1/mesh/links", &json!({"peer": peer_agent}), std::time::Duration::from_secs(20)).await.ok()
        .filter(|v| v["relayHost"].is_string())
        .map(|v| json!({"host": v["relayHost"], "myPort": v["myPort"], "peerPort": v["peerPort"]}));
    let endpoint = choose_endpoint(relay.as_ref(), &reported, prefer_direct);
    let my_endpoints = my_endpoints(state).await;
    sqlx::query(&format!(
        "INSERT INTO mesh_peers (agent, name, wg, psk, ip, endpoint, endpoints, relay, status, role) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'offered','initiator') \
         ON CONFLICT (agent) DO UPDATE SET name = excluded.name, wg = excluded.wg, psk = excluded.psk, ip = excluded.ip, endpoint = excluded.endpoint, \
           endpoints = excluded.endpoints, relay = excluded.relay, status = 'offered', role = 'initiator', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')"))
        .bind(peer_agent).bind(rec["name"].as_str().map(clean_name)).bind(&peer_wg).bind(crate::identity::at_rest(&unb64(&psk)?)?).bind(&peer_ip).bind(&endpoint)
        .bind(rec["endpoints"].to_string()).bind(relay.as_ref().map(|r| r.to_string()))
        .execute(&state.db).await?;
    // Tell the peer: which relay port THEY dial, and where we are directly.
    let peer_relay = relay.as_ref().map(|r| json!({"host": r["host"], "myPort": r["peerPort"], "peerPort": r["myPort"]}));
    crate::network::send(state, peer_agent, &json!({
        "kind": KIND_OFFER, "wg": my_wg, "ct": ct, "ip": my_ip(state), "endpoints": my_endpoints,
        "relay": peer_relay, "preferDirect": prefer_direct, "name": business_name(state).await, "a2a": A2A_PORT,
    })).await?;
    tracing::info!(peer = %peer_agent, ip = %peer_ip, relay = relay.is_some(), "mesh offer sent");
    apply(state).await;
    Ok(status(state).await)
}

async fn my_endpoints(state: &AppState) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(p) = get_state(&state.db, "public_endpoint").await { v.push(p); }
    v.extend(local_endpoints(state.mesh.listen_port));
    v
}

async fn business_name(state: &AppState) -> Option<String> {
    sqlx::query_as::<_, (String,)>("SELECT name FROM businesses ORDER BY created_at ASC LIMIT 1").fetch_optional(&state.db).await.ok().flatten().map(|r| r.0)
}

/// Relay protocol. Returns true when the message was a mesh message.
pub async fn handle_inbox(state: &SharedState, from: &str, v: &Value, _msg_id: &Value) -> bool {
    match v["kind"].as_str() {
        Some(KIND_OFFER) => { receive_offer(state, from, v).await; true }
        Some(KIND_ACCEPT) => { receive_accept(state, from, v).await; true }
        Some(KIND_DECLINE) => {
            let _ = sqlx::query("UPDATE mesh_peers SET status = 'declined', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE agent = $1 AND status = 'offered'").bind(from).execute(&state.db).await;
            true
        }
        _ => false,
    }
}

fn auto_accept_env() -> bool {
    std::env::var("MESH_AUTO_ACCEPT").map(|v| v == "1").unwrap_or(false)
}

async fn receive_offer(state: &SharedState, from: &str, v: &Value) {
    let (wg, ip) = (v["wg"].as_str().unwrap_or(""), v["ip"].as_str().unwrap_or(""));
    if !valid_wg_key(wg) || !valid_mesh_ip(ip) {
        tracing::warn!(%from, "mesh offer refused: malformed key or address");
        return;
    }
    if ip_taken(&state.db, ip, from).await {
        tracing::warn!(%from, %ip, "mesh offer refused: address belongs to another peer");
        return;
    }
    let name = v["name"].as_str().map(clean_name).filter(|n| !n.is_empty());
    let endpoints = Value::from(endpoints_of(&v["endpoints"]));
    let existing = peer(&state.db, from).await;
    let trusted = crate::owner::is_owner_device(state, from).await
        || auto_accept_env()
        || existing.as_ref().is_some_and(|p| matches!(p.status.as_str(), "invited" | "linked" | "offered"));
    // Store the offer either way; accept now when trusted, else wait for the owner.
    let _ = sqlx::query(
        "INSERT INTO mesh_peers (agent, name, wg, ip, endpoints, relay, offer, status, role) VALUES ($1,$2,$3,$4,$5,$6,$7,'pending','responder') \
         ON CONFLICT (agent) DO UPDATE SET name = COALESCE(excluded.name, mesh_peers.name), wg = excluded.wg, ip = excluded.ip, endpoints = excluded.endpoints, \
           relay = excluded.relay, offer = excluded.offer, status = 'pending', role = 'responder', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')",
    ).bind(from).bind(&name).bind(wg).bind(ip).bind(endpoints.to_string())
    .bind(if v["relay"].is_object() { Some(v["relay"].to_string()) } else { None }).bind(v.to_string())
    .execute(&state.db).await;
    if trusted {
        if let Err(e) = accept(state, from).await {
            tracing::warn!(%from, error = %e, "mesh offer could not be accepted");
        }
    } else {
        tracing::info!(%from, "mesh offer pending the owner's approval");
    }
}

/// Accept a pending offer (owner action, or automatic for trusted peers):
/// decapsulate, derive the PSK, reply, apply.
pub async fn accept(state: &SharedState, from: &str) -> anyhow::Result<Value> {
    if my_ip(state).is_none() {
        register(state).await?;
    }
    let p = peer(&state.db, from).await.ok_or_else(|| anyhow::anyhow!("no offer from {from}"))?;
    let ip = p.ip.clone().filter(|ip| valid_mesh_ip(ip)).ok_or_else(|| anyhow::anyhow!("offer has no valid mesh address"))?;
    anyhow::ensure!(!ip_taken(&state.db, &ip, from).await, "{ip} belongs to another peer");
    let offer: Value = serde_json::from_str(p.offer.as_deref().unwrap_or("{}"))?;
    let (peer_wg, ct) = (offer["wg"].as_str().filter(|k| valid_wg_key(k)).ok_or_else(|| anyhow::anyhow!("offer has no wg key"))?.to_string(), offer["ct"].as_str().ok_or_else(|| anyhow::anyhow!("offer has no ciphertext"))?);
    let ss = state.mesh.keys.decapsulate(ct)?;
    let my_wg = state.mesh.keys.wg_public_b64();
    let psk = psk_b64(&ss, from, &state.identity.id(), &peer_wg, &my_wg);
    let reported: Vec<String> = offer["endpoints"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let relay = if offer["relay"].is_object() { Some(offer["relay"].clone()) } else { None };
    let endpoint = choose_endpoint(relay.as_ref(), &reported, offer["preferDirect"].as_bool().unwrap_or(false));
    sqlx::query("UPDATE mesh_peers SET psk = $1, endpoint = $2, status = 'linked', offer = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE agent = $3")
        .bind(crate::identity::at_rest(&unb64(&psk)?)?).bind(&endpoint).bind(from).execute(&state.db).await?;
    crate::network::send(state, from, &json!({
        "kind": KIND_ACCEPT, "wg": my_wg, "ip": my_ip(state), "endpoints": my_endpoints(state).await,
        "name": business_name(state).await, "a2a": A2A_PORT,
    })).await?;
    tracing::info!(peer = %from, ip = ?p.ip, "mesh link accepted");
    apply(state).await;
    Ok(status(state).await)
}

pub async fn decline(state: &SharedState, from: &str) -> anyhow::Result<Value> {
    sqlx::query("UPDATE mesh_peers SET status = 'declined', offer = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE agent = $1").bind(from).execute(&state.db).await?;
    let _ = crate::network::send(state, from, &json!({"kind": KIND_DECLINE})).await;
    Ok(status(state).await)
}

async fn receive_accept(state: &SharedState, from: &str, v: &Value) {
    let Some(p) = peer(&state.db, from).await else { return };
    if p.status != "offered" {
        tracing::debug!(%from, status = %p.status, "mesh accept for a peer not offered");
        return;
    }
    let reported = endpoints_of(&v["endpoints"]);
    // Our offer already fixed their key and address (from the registry);
    // an accept may not move them somewhere else.
    let name = v["name"].as_str().map(clean_name).filter(|n| !n.is_empty());
    let relay: Option<Value> = p.relay.as_deref().and_then(|r| serde_json::from_str(r).ok());
    // We chose the endpoint when offering; refresh direct candidates from
    // what the peer just reported in case we prefer direct.
    let endpoint = p.endpoint.clone().or_else(|| choose_endpoint(relay.as_ref(), &reported, false));
    let _ = sqlx::query("UPDATE mesh_peers SET status = 'linked', endpoints = $1, endpoint = $2, name = COALESCE($3, name), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE agent = $4")
        .bind(Value::from(reported.clone()).to_string()).bind(&endpoint).bind(&name).bind(from).execute(&state.db).await;
    tracing::info!(peer = %from, "mesh link established");
    apply(state).await;
}

/// Mark a peer as invited: an offer from it is accepted without asking.
pub async fn invite(state: &AppState, agent: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO mesh_peers (agent, status) VALUES ($1, 'invited') ON CONFLICT (agent) DO UPDATE SET status = CASE WHEN mesh_peers.status = 'linked' THEN 'linked' ELSE 'invited' END")
        .bind(agent).execute(&state.db).await?;
    Ok(())
}

pub async fn forget(state: &SharedState, agent: &str) -> anyhow::Result<Value> {
    sqlx::query("DELETE FROM mesh_peers WHERE agent = $1").bind(agent).execute(&state.db).await?;
    apply(state).await;
    Ok(status(state).await)
}

// ---------------------------------------------------------------- config

pub async fn peer_confs(state: &AppState) -> Vec<PeerConf> {
    let mut out = Vec::new();
    for p in peers(&state.db).await {
        if p.status != "linked" { continue; }
        let (Some(wg), Some(psk), Some(ip)) = (p.wg, p.psk, p.ip) else { continue };
        let Ok(psk) = crate::identity::from_rest(&psk, Some(32)) else { continue };
        out.push(PeerConf {
            name: format!("{} {}", p.name.unwrap_or_default(), p.agent).trim().to_string(),
            public: wg, psk: b64(&psk), endpoint: p.endpoint, allowed_ips: vec![format!("{ip}/32")],
        });
    }
    out
}

/// The wg-quick config for this device (contains the private key: owner only).
pub async fn config(state: &AppState, for_sync: bool) -> String {
    let ip = my_ip(state).map(|ip| format!("{ip}/16"));
    wg_config(&state.mesh.keys.wg_private_b64(), ip.as_deref(), Some(state.mesh.listen_port), &peer_confs(state).await, for_sync)
}

fn conf_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("MESH_CONF_PATH") { return p.into(); }
    let etc = std::path::Path::new("/etc/wireguard");
    if etc.is_dir() && std::fs::metadata(etc).map(|m| !m.permissions().readonly()).unwrap_or(false) && is_root() {
        return etc.join(format!("{IFACE}.conf"));
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join(".yaya").join("runtime").join(format!("{IFACE}.conf"))
}

fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status").map(|s| s.lines().any(|l| l.starts_with("Uid:") && l.split_whitespace().nth(1) == Some("0"))).unwrap_or(false)
}

/// After any change: bump the version (the phone re-applies through
/// VpnService), write the file, and on Linux with `MESH_APPLY=1` bring the
/// interface up or sync it. Never fails the caller.
pub async fn apply(state: &SharedState) {
    state.mesh.version.fetch_add(1, Ordering::SeqCst);
    let conf = config(state, false).await;
    let path = conf_path();
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    if let Err(e) = write_private(&path, &conf) {
        tracing::warn!(error = %e, path = %path.display(), "mesh config not written");
    }
    if std::env::var("MESH_APPLY").map(|v| v == "1").unwrap_or(false) {
        match apply_linux(&path, &config(state, true).await).await {
            Ok(msg) => tracing::info!(%msg, "mesh applied"),
            Err(e) => tracing::warn!(error = %e, "mesh not applied (bring it up with: yaya mesh up)"),
        }
    }
    ensure_a2a(state.clone());
}

pub(crate) fn write_private(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    // Created owner-only, never written first and restricted after: the key
    // must not sit in a readable file even for an instant.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        // An existing file keeps its old mode on open: tighten it too.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(content.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content)
    }
}

fn sudo() -> Vec<String> {
    if is_root() { vec![] } else { vec!["sudo".into(), "-n".into()] }
}

async fn run(args: &[String]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new(&args[0]).args(&args[1..]).output().await?;
    if !out.status.success() {
        anyhow::bail!("{}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn iface_up() -> bool {
    std::path::Path::new(&format!("/sys/class/net/{IFACE}")).exists()
}

/// Linux: `wg-quick up` the first time, `wg syncconf` + routes afterwards.
pub async fn apply_linux(conf_path: &std::path::Path, sync_conf: &str) -> anyhow::Result<String> {
    let pre = sudo();
    let cmd = |mut v: Vec<String>| { let mut a = pre.clone(); a.append(&mut v); a };
    if !iface_up() {
        run(&cmd(vec!["wg-quick".into(), "up".into(), conf_path.to_string_lossy().into()])).await?;
        return Ok(format!("{IFACE} up"));
    }
    let tmp = std::env::temp_dir().join(format!("{IFACE}-sync-{}.conf", std::process::id()));
    write_private(&tmp, sync_conf)?;
    let r = run(&cmd(vec!["wg".into(), "syncconf".into(), IFACE.into(), tmp.to_string_lossy().into()])).await;
    let _ = std::fs::remove_file(&tmp);
    r?;
    // syncconf does not touch routes: add one per peer address.
    for line in sync_conf.lines() {
        if let Some(ips) = line.strip_prefix("AllowedIPs = ") {
            for ip in ips.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let _ = run(&cmd(vec!["ip".into(), "route".into(), "replace".into(), ip.into(), "dev".into(), IFACE.into()])).await;
            }
        }
    }
    Ok(format!("{IFACE} synced"))
}

pub async fn status(state: &AppState) -> Value {
    let ps = peers(&state.db).await;
    let ip = my_ip(state);
    json!({
        "ip": ip, "subnet": SUBNET, "iface": IFACE, "listenPort": state.mesh.listen_port,
        "wg": state.mesh.keys.wg_public_b64(), "kem": state.mesh.keys.kem_public_b64(), "kemAlg": "ML-KEM-768",
        "registeredAt": get_state(&state.db, "registered_at").await, "publicEndpoint": get_state(&state.db, "public_endpoint").await,
        "configVersion": state.mesh.version.load(Ordering::SeqCst),
        "confPath": conf_path().to_string_lossy(), "ifaceUp": iface_up(),
        "a2a": ip.as_ref().map(|ip| format!("http://{ip}:{A2A_PORT}/a2a")),
        "a2aBound": state.mesh.a2a_bound.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        "peers": ps.into_iter().map(|p| json!({
            "agent": p.agent, "name": p.name, "ip": p.ip, "status": p.status, "role": p.role, "endpoint": p.endpoint,
            "relay": p.relay.as_deref().and_then(|r| serde_json::from_str::<Value>(r).ok()).map(|r| r["host"].clone()),
            "a2a": p.ip.as_ref().map(|ip| format!("http://{ip}:{A2A_PORT}/a2a")), "updatedAt": p.updated_at,
        })).collect::<Vec<_>>(),
    })
}

// ------------------------------------------------------------------- A2A

/// Serve A2A on our mesh address as soon as the interface carries it;
/// retried in the background until the bind succeeds. Idempotent.
pub fn ensure_a2a(state: SharedState) {
    let Some(ip) = my_ip(&state) else { return };
    if state.mesh.a2a_bound.lock().unwrap_or_else(|e| e.into_inner()).as_deref() == Some(ip.as_str()) {
        return;
    }
    tokio::spawn(async move {
        for _ in 0..360 {
            if state.mesh.a2a_bound.lock().unwrap_or_else(|e| e.into_inner()).as_deref() == Some(ip.as_str()) {
                return;
            }
            match tokio::net::TcpListener::bind((ip.as_str(), A2A_PORT)).await {
                Ok(l) => {
                    *state.mesh.a2a_bound.lock().unwrap_or_else(|e| e.into_inner()) = Some(ip.clone());
                    tracing::info!(%ip, port = A2A_PORT, "A2A listening on the mesh");
                    let app = a2a_router(state.clone());
                    if let Err(e) = axum::serve(l, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await {
                        tracing::warn!(error = %e, "A2A listener ended");
                    }
                    *state.mesh.a2a_bound.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    return;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_secs(10)).await,
            }
        }
    });
}

pub fn a2a_router(state: SharedState) -> Router {
    Router::new()
        .route("/.well-known/agent.json", axum::routing::get(a2a_card))
        .route("/a2a", axum::routing::get(a2a_card).post(a2a_rpc))
        .route("/health", axum::routing::get(|| async { "ok" }))
        .with_state(state)
}

/// The A2A Agent Card: the same truth as the network card, addressed on the mesh.
pub async fn agent_card_json(state: &AppState) -> Value {
    let card = crate::network::card(state).await;
    let ip = my_ip(state).unwrap_or_else(|| "0.0.0.0".into());
    json!({
        "name": card["name"].as_str().unwrap_or("agente"),
        "description": card["description"],
        "url": format!("http://{ip}:{A2A_PORT}/a2a"),
        "provider": {"organization": "yaya.tech", "url": "https://yaya.tech"},
        "version": env!("CARGO_PKG_VERSION"),
        "documentationUrl": "https://github.com/libr3andr3/agento.ceo/blob/ship/business-agent/docs/MESH.md",
        "capabilities": {"streaming": false, "pushNotifications": false, "stateTransitionHistory": false},
        "authentication": {"schemes": ["yaya-mesh"], "credentials": "reachable only over the post-quantum mesh; the peer is identified by its mesh address"},
        "defaultInputModes": ["text"], "defaultOutputModes": ["text"],
        "skills": card["skills"].as_array().map(|a| a.iter().map(|s| json!({"id": s["id"], "name": s["id"], "description": s["description"], "tags": ["yaya"], "inputModes": ["text"], "outputModes": ["text"]})).collect::<Vec<_>>()).unwrap_or_default(),
        "yaya": {"agent": state.identity.id(), "did": state.identity.did(), "meshIp": ip, "offer": card["offer"], "industry": card["industry"], "country": card["country"]},
    })
}

async fn a2a_card(State(state): State<SharedState>) -> Json<Value> {
    Json(agent_card_json(&state).await)
}

fn rpc_err(id: Value, code: i64, msg: &str) -> Json<Value> {
    Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}}))
}

/// JSON-RPC 2.0: `message/send` (A2A 0.2) and `tasks/send` (0.1) run one
/// customer turn as the peer at that mesh address; `tasks/get` reads it back.
async fn a2a_rpc(State(state): State<SharedState>, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>, body: axum::body::Bytes) -> impl IntoResponse {
    let req: Value = match serde_json::from_slice(&body) { Ok(v) => v, Err(_) => return (StatusCode::BAD_REQUEST, rpc_err(Value::Null, -32700, "parse error")) };
    let id = req["id"].clone();
    let method = req["method"].as_str().unwrap_or("");
    let params = &req["params"];
    match method {
        "tasks/get" => {
            let tid = params["id"].as_str().unwrap_or("");
            match state.mesh.a2a_tasks.lock().unwrap_or_else(|e| e.into_inner()).get(tid).cloned() {
                Some(t) => (StatusCode::OK, Json(json!({"jsonrpc": "2.0", "id": id, "result": t}))),
                None => (StatusCode::OK, rpc_err(id, -32001, "task not found")),
            }
        }
        "message/send" | "tasks/send" | "message/stream" => {
            let msg = if method == "tasks/send" { &params["message"] } else { params["message"].as_object().map(|_| &params["message"]).unwrap_or(params) };
            let text: String = msg["parts"].as_array().map(|a| a.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_default();
            if text.trim().is_empty() {
                return (StatusCode::OK, rpc_err(id, -32602, "message.parts[].text is required"));
            }
            let task_id = params["id"].as_str().or(msg["taskId"].as_str()).map(String::from).unwrap_or_else(|| format!("task_{}", uuid::Uuid::new_v4().simple()));
            let context_id = msg["contextId"].as_str().map(String::from).unwrap_or_else(|| task_id.clone());
            // Who is talking: the linked agent behind that mesh address.
            let ip = peer.ip().to_string();
            let who = sqlx::query_as::<_, (String,)>("SELECT agent FROM mesh_peers WHERE ip = $1 AND status = 'linked'").bind(&ip).fetch_optional(&state.db).await.ok().flatten()
                .map(|r| r.0).unwrap_or_else(|| format!("mesh:{ip}"));
            let Some(bid) = crate::network::business_id(&state).await else {
                return (StatusCode::OK, rpc_err(id, -32000, "this agent serves no business yet"));
            };
            let turn = match crate::routes::customer_turn(&state, bid, &who, &text).await {
                Ok(v) => v,
                Err((_, e)) => return (StatusCode::OK, rpc_err(id, -32000, e.0["error"].as_str().unwrap_or("turn failed"))),
            };
            let reply = turn["agentResponse"].as_str().unwrap_or("").to_string();
            let task = json!({
                "id": task_id, "contextId": context_id, "kind": "task",
                "status": {"state": "completed", "timestamp": chrono::Utc::now().to_rfc3339(),
                           "message": {"role": "agent", "kind": "message", "messageId": format!("msg_{}", uuid::Uuid::new_v4().simple()), "parts": [{"kind": "text", "type": "text", "text": reply}]}},
                "artifacts": [{"artifactId": "reply", "name": "reply", "parts": [{"kind": "text", "type": "text", "text": reply}]}],
                "metadata": {"action": turn["action"], "actionData": turn["actionData"], "peer": who},
            });
            state.mesh.a2a_tasks.lock().unwrap_or_else(|e| e.into_inner()).insert(task_id.clone(), task.clone());
            (StatusCode::OK, Json(json!({"jsonrpc": "2.0", "id": id, "result": task})))
        }
        "agent/card" | "agent/authenticatedExtendedCard" => (StatusCode::OK, Json(json!({"jsonrpc": "2.0", "id": id, "result": agent_card_json(&state).await}))),
        // A coins payment over the mesh: verified offline, receipt back.
        "yaya/coins" => match crate::coins::receive(&state, params, "mesh").await {
            Ok(r) => {
                let bg = state.clone();
                tokio::spawn(async move { let _ = crate::coins::refresh(&bg).await; });
                (StatusCode::OK, Json(json!({"jsonrpc": "2.0", "id": id, "result": r})))
            }
            Err(e) => (StatusCode::OK, rpc_err(id, -32010, &e.to_string())),
        },
        _ => (StatusCode::OK, rpc_err(id, -32601, "method not found")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_handshake_agrees_on_the_psk() {
        let a = MeshKeys::generate();
        let b = MeshKeys::generate();
        let (ct, ss_a) = encapsulate(&b.kem_public_b64()).unwrap();
        let ss_b = b.decapsulate(&ct).unwrap();
        assert_eq!(ss_a, ss_b);
        let p1 = psk_b64(&ss_a, "agent:a", "agent:b", &a.wg_public_b64(), &b.wg_public_b64());
        let p2 = psk_b64(&ss_b, "agent:a", "agent:b", &a.wg_public_b64(), &b.wg_public_b64());
        assert_eq!(p1, p2);
        assert_ne!(p1, psk_b64(&ss_a, "agent:b", "agent:a", &a.wg_public_b64(), &b.wg_public_b64()));
        assert_eq!(unb64(&p1).unwrap().len(), 32);
    }

    #[test]
    fn keys_round_trip_and_wg_key_is_clamped() {
        let k = MeshKeys::generate();
        let k2 = MeshKeys::from_bytes(&k.to_bytes()).unwrap();
        assert_eq!(k.wg_public_b64(), k2.wg_public_b64());
        assert_eq!(k.kem_public_b64(), k2.kem_public_b64());
        assert_eq!(unb64(&k.wg_private_b64()).unwrap().len(), 32);
        assert_eq!(unb64(&k.kem_public_b64()).unwrap().len(), 1184);
        assert!(MeshKeys::from_bytes(&[0u8; 10]).is_err());
    }

    #[test]
    fn config_has_psk_and_keepalive() {
        let (public, psk) = (MeshKeys::generate().wg_public_b64(), b64(&[7; 32]));
        let c = wg_config("PRIV", Some("10.77.0.2/16"), Some(41000), &[PeerConf { name: "b".into(), public, psk: psk.clone(), endpoint: Some("1.2.3.4:5".into()), allowed_ips: vec!["10.77.0.3/32".into()] }], false);
        assert!(c.contains(&format!("PresharedKey = {psk}")) && c.contains("Endpoint = 1.2.3.4:5") && c.contains("PersistentKeepalive = 25") && c.contains("Address = 10.77.0.2/16"));
        assert!(!wg_config("PRIV", Some("10.77.0.2/16"), None, &[], true).contains("Address"));
    }

    #[test]
    fn relay_is_preferred_unless_asked_otherwise() {
        let relay = json!({"host": "1.1.1.1", "myPort": 40001, "peerPort": 40002});
        let reported = vec!["192.168.1.5:41000".to_string(), "8.8.8.8:41000".to_string()];
        assert_eq!(choose_endpoint(Some(&relay), &reported, false).as_deref(), Some("1.1.1.1:40001"));
        assert_eq!(choose_endpoint(Some(&relay), &reported, true).as_deref(), Some("8.8.8.8:41000"));
        assert_eq!(choose_endpoint(None, &reported, false).as_deref(), Some("8.8.8.8:41000"));
        assert_eq!(choose_endpoint(None, &[], false), None);
    }

    use crate::testkit::{self, Mock};
    use serde_json::json;

    fn wg_pub() -> String { MeshKeys::generate().wg_public_b64() }

    /// An offer from `from` to the node behind `to_kem`, as link() builds it.
    fn offer(to: &AppState, ip: &str, name: &str) -> Value {
        let (ct, _) = encapsulate(&to.mesh.keys.kem_public_b64()).unwrap();
        json!({"kind": "mesh_offer", "wg": wg_pub(), "ct": ct, "ip": ip, "endpoints": ["203.0.113.5:40000"], "relay": null, "name": name, "a2a": A2A_PORT})
    }

    async fn trusted(s: &AppState, agent: &str) {
        invite(s, agent).await.unwrap();
    }

    #[test]
    fn a_peer_can_never_break_out_of_its_config_lines() {
        let evil = PeerConf {
            name: "Bodega\n[Peer]\nPublicKey = AAAA\nAllowedIPs = 0.0.0.0/0".into(),
            public: wg_pub(), psk: b64(&[1; 32]), endpoint: Some("1.2.3.4:5\nAllowedIPs = 0.0.0.0/0".into()),
            allowed_ips: vec!["10.77.0.9/32".into()],
        };
        let conf = wg_config(&MeshKeys::generate().wg_private_b64(), Some("10.77.0.1/16"), Some(40000), &[evil], false);
        let lines: Vec<&str> = conf.lines().collect();
        assert_eq!(lines.iter().filter(|l| l.trim() == "[Peer]").count(), 1, "{conf}");
        assert!(lines.iter().all(|l| !(l.starts_with("AllowedIPs") && l.contains("0.0.0.0"))), "{conf}");
        assert!(lines.iter().all(|l| !l.starts_with("PublicKey = AAAA")), "{conf}");
        assert!(!lines.iter().any(|l| l.starts_with("Endpoint")), "a malformed endpoint is dropped");
        // Peers whose key or address is malformed are left out entirely.
        let bad = PeerConf { name: "x".into(), public: "PUB".into(), psk: b64(&[1; 32]), endpoint: None, allowed_ips: vec!["10.77.0.9/32".into()] };
        assert!(!wg_config("K", None, None, &[bad], true).contains("[Peer]"));
    }

    #[tokio::test]
    async fn offers_with_forged_addresses_or_keys_are_refused() {
        let m = Mock::start().await;
        m.on("/inbox", json!({"id": "x"}));
        let s = testkit::state_on(&m).await;
        let from = crate::identity::Identity::ephemeral().id();
        trusted(&s, &from).await;
        for bad_ip in ["0.0.0.0/0", "8.8.8.8", "10.77.0.9/0", "10.77.0.9\nEndpoint = x", "not-an-ip"] {
            handle_inbox(&s, &from, &offer(&s, bad_ip, "x"), &Value::Null).await;
            let p = peer(&s.db, &from).await.unwrap();
            assert_ne!(p.status, "linked", "ip {bad_ip:?} must not link");
            assert_ne!(p.ip.as_deref(), Some(bad_ip));
        }
        let mut o = offer(&s, "10.77.0.9", "x");
        o["wg"] = json!("not a key\n[Peer]");
        handle_inbox(&s, &from, &o, &Value::Null).await;
        assert_ne!(peer(&s.db, &from).await.unwrap().status, "linked");
    }

    #[tokio::test]
    async fn a_peer_cannot_claim_another_peers_address() {
        let m = Mock::start().await;
        m.on("/inbox", json!({"id": "x"}));
        let s = testkit::state_on(&m).await;
        let (honest, thief) = (crate::identity::Identity::ephemeral().id(), crate::identity::Identity::ephemeral().id());
        trusted(&s, &honest).await;
        trusted(&s, &thief).await;
        *s.mesh.ip.lock().unwrap() = Some("10.77.0.1".into());
        handle_inbox(&s, &honest, &offer(&s, "10.77.1.1", "Honesto"), &Value::Null).await;
        assert_eq!(peer(&s.db, &honest).await.unwrap().status, "linked");
        handle_inbox(&s, &thief, &offer(&s, "10.77.1.1", "Ladrón"), &Value::Null).await;
        assert_ne!(peer(&s.db, &thief).await.unwrap().status, "linked", "the address belongs to someone else");
        let owners: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mesh_peers WHERE ip = '10.77.1.1' AND status = 'linked'").fetch_one(&s.db).await.unwrap();
        assert_eq!(owners, 1);
    }

    #[tokio::test]
    async fn names_are_single_line_and_bounded() {
        let m = Mock::start().await;
        m.on("/inbox", json!({"id": "x"}));
        let s = testkit::state_on(&m).await;
        let from = crate::identity::Identity::ephemeral().id();
        trusted(&s, &from).await;
        *s.mesh.ip.lock().unwrap() = Some("10.77.0.1".into());
        handle_inbox(&s, &from, &offer(&s, "10.77.2.2", &format!("Bodega\n[Peer]{}", "x".repeat(200))), &Value::Null).await;
        let name = peer(&s.db, &from).await.unwrap().name.unwrap();
        assert!(!name.contains('\n') && name.chars().count() <= 60, "{name:?}");
        let conf = config(&s, false).await;
        assert_eq!(conf.lines().filter(|l| l.trim() == "[Peer]").count(), 1, "{conf}");
    }

    #[tokio::test]
    async fn untrusted_offers_wait_for_the_owner_then_link() {
        let m = Mock::start().await;
        m.on("/inbox", json!({"id": "x"}));
        let s = testkit::state_on(&m).await;
        let from = crate::identity::Identity::ephemeral().id();
        handle_inbox(&s, &from, &offer(&s, "10.77.3.3", "Tienda"), &Value::Null).await;
        assert_eq!(peer(&s.db, &from).await.unwrap().status, "pending");
        *s.mesh.ip.lock().unwrap() = Some("10.77.0.1".into());
        let st = accept(&s, &from).await.unwrap();
        assert_eq!(st["peers"][0]["status"], "linked");
        let reply = m.seen_path(&format!("/v1/agents/{from}/inbox"));
        assert_eq!(reply.len(), 1, "the accept went back sealed");
        let confs = peer_confs(&s).await;
        assert_eq!(confs[0].allowed_ips, vec!["10.77.3.3/32"]);
        // Decline and forget.
        decline(&s, &from).await.unwrap();
        assert_eq!(peer(&s.db, &from).await.unwrap().status, "declined");
        forget(&s, &from).await.unwrap();
        assert!(peer(&s.db, &from).await.is_none());
        assert!(accept(&s, &from).await.is_err());
    }

    #[test]
    fn validators() {
        assert!(valid_mesh_ip("10.77.0.9") && !valid_mesh_ip("10.78.0.9") && !valid_mesh_ip("10.77.0.9/32") && !valid_mesh_ip(" 10.77.0.9"));
        assert!(valid_wg_key(&wg_pub()) && !valid_wg_key("AAAA") && !valid_wg_key("x\ny"));
        assert!(valid_endpoint("203.0.113.5:40000") && valid_endpoint("relay.yaya.tech:51820"));
        assert!(!valid_endpoint("1.2.3.4") && !valid_endpoint("1.2.3.4:0") && !valid_endpoint("a b:1") && !valid_endpoint("x\n:1"));
        assert_eq!(clean_name("  Bodega\n\tRosa  "), "Bodega Rosa");
        assert_eq!(endpoints_of(&json!(["1.2.3.4:5", "bad", 7])), vec!["1.2.3.4:5"]);
    }

    #[test]
    fn helpers() {
        assert_eq!(unb64(&b64(b"x")).unwrap(), b"x");
        assert!(unb64("%").is_err());
        let a = MeshKeys::generate();
        assert_eq!(MeshKeys::from_bytes(&a.to_bytes()).unwrap().wg_public_b64(), a.wg_public_b64());
        assert!(MeshKeys::from_bytes(&[0; 5]).is_err());
        assert!(a.decapsulate("!!").is_err());
        assert!(encapsulate("!!").is_err());
        assert_eq!(choose_endpoint(None, &["192.168.1.2:1".into()], false).as_deref(), Some("192.168.1.2:1"));
        assert_eq!(choose_endpoint(None, &[], true), None);
    }

    #[tokio::test]
    async fn register_records_the_assigned_address() {
        let m = Mock::start().await;
        m.on("/v1/mesh/info", json!({})).on("/v1/mesh/register", json!({"ip": "10.77.4.4"}));
        let s = testkit::state_on(&m).await;
        assert_eq!(my_ip(&s), None);
        register(&s).await.unwrap();
        assert_eq!(my_ip(&s).as_deref(), Some("10.77.4.4"));
        let b = &m.seen_path("/v1/mesh/register")[0].body;
        assert_eq!((b["wg"].clone(), b["listenPort"].clone(), b["a2a"].clone()), (json!(s.mesh.keys.wg_public_b64()), json!(s.mesh.listen_port), json!(A2A_PORT)));
        // Persisted: a restart keeps the address and the keys.
        let again = load_or_create(&s.db).await.unwrap();
        assert_eq!(again.ip.lock().unwrap().as_deref(), Some("10.77.4.4"));
        assert_eq!(again.keys.wg_public_b64(), s.mesh.keys.wg_public_b64());
        assert_eq!(again.listen_port, s.mesh.listen_port);
        assert!((40000..60000).contains(&s.mesh.listen_port));
    }

    #[tokio::test]
    async fn link_offers_and_the_accept_completes_it() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        let other = MeshKeys::generate();
        let peer_id = crate::identity::Identity::ephemeral().id();
        *s.mesh.ip.lock().unwrap() = Some("10.77.0.1".into());
        m.on(&format!("/v1/mesh/agents/{peer_id}"), json!({"wg": other.wg_public_b64(), "kem": other.kem_public_b64(), "ip": "10.77.5.5", "name": "Tienda\nX", "endpoints": ["203.0.113.7:41000"]}));
        m.on("/v1/mesh/links", json!({"relayHost": "relay.yaya.tech", "myPort": 50001, "peerPort": 50002}));
        m.on("/inbox", json!({"id": "b"}));
        let st = link(&s, &peer_id, false).await.unwrap();
        assert_eq!(st["peers"][0]["status"], "offered");
        let p = peer(&s.db, &peer_id).await.unwrap();
        assert_eq!((p.endpoint.as_deref(), p.name.as_deref()), (Some("relay.yaya.tech:50001"), Some("Tienda X")));
        // The peer's accept arrives: linked, keeping the registry's address.
        handle_inbox(&s, &peer_id, &json!({"kind": "mesh_accept", "wg": other.wg_public_b64(), "ip": "10.77.9.9", "endpoints": ["203.0.113.7:41000"], "name": "Tienda"}), &Value::Null).await;
        let p = peer(&s.db, &peer_id).await.unwrap();
        assert_eq!((p.status.as_str(), p.ip.as_deref(), p.name.as_deref()), ("linked", Some("10.77.5.5"), Some("Tienda")));
        let conf = config(&s, false).await;
        assert!(conf.contains("AllowedIPs = 10.77.5.5/32") && conf.contains("Endpoint = relay.yaya.tech:50001") && conf.contains("Address = 10.77.0.1/16"), "{conf}");
        assert!(!config(&s, true).await.contains("Address"));
        // An accept nobody asked for is ignored; a decline marks the offer.
        handle_inbox(&s, "agent:stranger", &json!({"kind": "mesh_accept"}), &Value::Null).await;
        assert!(peer(&s.db, "agent:stranger").await.is_none());
        assert!(!handle_inbox(&s, &peer_id, &json!({"kind": "chat"}), &Value::Null).await);
        // Unknown peer on the registry: a clear error.
        assert!(link(&s, "agent:nobody", false).await.unwrap_err().to_string().contains("not on the mesh"));
    }

    #[tokio::test]
    async fn status_and_invites() {
        let s = testkit::state().await;
        invite(&s, "agent:a").await.unwrap();
        let st = status(&s).await;
        assert_eq!((st["subnet"].clone(), st["kemAlg"].clone(), st["peers"][0]["status"].clone()), (json!(SUBNET), json!("ML-KEM-768"), json!("invited")));
        assert!(st["a2a"].is_null(), "no address, no A2A URL");
        sqlx::query("UPDATE mesh_peers SET status = 'linked'").execute(&s.db).await.unwrap();
        invite(&s, "agent:a").await.unwrap();
        assert_eq!(peer(&s.db, "agent:a").await.unwrap().status, "linked", "inviting a linked peer keeps it linked");
    }

    async fn rpc(s: &crate::SharedState, from: [u8; 4], body: Value) -> Value {
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder().method("POST").uri("/a2a").header("content-type", "application/json").body(axum::body::Body::from(body.to_string())).unwrap();
        req.extensions_mut().insert(ConnectInfo(std::net::SocketAddr::from((from, 1))));
        let r = a2a_router(s.clone()).oneshot(req).await.unwrap();
        serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a2a_turns_a_linked_peers_message_into_a_customer_turn() {
        let m = Mock::start().await;
        let s = testkit::state_on(&m).await;
        assert_eq!(rpc(&s, [10, 77, 5, 5], json!({"id": 1, "method": "message/send", "params": {"message": {"parts": [{"text": "hola"}]}}})).await["error"]["code"], -32000, "no business yet");
        testkit::onboard(&s, &m).await;
        m.on("/v1/credits", json!({"state": "ok"}));
        sqlx::query("INSERT INTO mesh_peers (agent, ip, status) VALUES ('agent:peer', '10.77.5.5', 'linked')").execute(&s.db).await.unwrap();
        m.say("¡Hola, vecino!");
        let r = rpc(&s, [10, 77, 5, 5], json!({"id": 1, "method": "message/send", "params": {"message": {"parts": [{"text": "hola"}], "contextId": "c1"}}})).await;
        assert_eq!(r["result"]["status"]["message"]["parts"][0]["text"], "¡Hola, vecino!");
        assert_eq!(r["result"]["metadata"]["peer"], "agent:peer", "identified by its mesh address");
        let tid = r["result"]["id"].as_str().unwrap().to_string();
        assert_eq!(rpc(&s, [10, 77, 5, 5], json!({"id": 2, "method": "tasks/get", "params": {"id": tid}})).await["result"]["contextId"], "c1");
        // An unlinked address is only a mesh address, never an agent.
        m.say("ok");
        let r = rpc(&s, [10, 77, 6, 6], json!({"id": 3, "method": "tasks/send", "params": {"message": {"parts": [{"text": "x"}]}}})).await;
        assert_eq!(r["result"]["metadata"]["peer"], "mesh:10.77.6.6");
        assert_eq!(rpc(&s, [10, 77, 5, 5], json!({"id": 4, "method": "message/send", "params": {"message": {"parts": []}}})).await["error"]["code"], -32602);
        assert_eq!(rpc(&s, [10, 77, 5, 5], json!({"id": 5, "method": "nope"})).await["error"]["code"], -32601);
        assert_eq!(rpc(&s, [10, 77, 5, 5], json!({"id": 6, "method": "tasks/get", "params": {"id": "x"}})).await["error"]["code"], -32001);
        assert_eq!(rpc(&s, [10, 77, 5, 5], json!({"id": 7, "method": "yaya/coins", "params": {}})).await["error"]["code"], -32010);
        let card = rpc(&s, [10, 77, 5, 5], json!({"id": 8, "method": "agent/card"})).await;
        assert_eq!(card["result"]["yaya"]["agent"], json!(s.identity.id()));
    }

    #[tokio::test]
    async fn apply_writes_a_private_config_and_bumps_the_version() {
        let s = testkit::state().await;
        let v0 = s.mesh.version.load(Ordering::SeqCst);
        apply(&s).await;
        assert_eq!(s.mesh.version.load(Ordering::SeqCst), v0 + 1);
        let path = conf_path();
        assert!(path.to_string_lossy().contains("test-dbs"), "tests never write the real config: {}", path.display());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
