//! Spotify's shuffle and repeat. The desktop app answers Windows' media-session commands for
//! them with "ok" and does nothing, so these two go through Spotify's Web API instead.
//!
//! The user connects once: the first press of either button opens Spotify's login in the
//! browser (PKCE, so the app holds no secret), and the refresh token is kept encrypted for their
//! Windows account (DPAPI). After that the buttons just work. Premium is Spotify's rule for
//! these calls. Everything here blocks (the login waits for the browser): run it on its own
//! thread, with [`run`].

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::platform::http::request;
use crate::platform::log::log;

/// The client ID of Pebbly's Spotify app (developer.spotify.com/dashboard). It isn't a secret: PKCE
/// needs no client secret, so this may sit in the code. `PEBBLY_SPOTIFY_CLIENT_ID` overrides it.
/// The app's redirect URI there must be exactly `http://127.0.0.1:20035/callback`.
const CLIENT_ID: &str = "6814067747154022a12fc5d49f080b5b";
const PORT: u16 = 20035;
const SCOPE: &str = "user-read-playback-state user-modify-playback-state";
/// How long the login waits for the user in the browser.
const CONNECT_WAIT: Duration = Duration::from_secs(180);

fn client_id() -> String {
    std::env::var("PEBBLY_SPOTIFY_CLIENT_ID").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| CLIENT_ID.to_string())
}

fn redirect() -> String {
    format!("http://127.0.0.1:{PORT}/callback")
}

/// Whether a media session's app is the Spotify desktop app.
pub fn is_spotify(app_id: &str) -> bool {
    app_id.to_ascii_lowercase().contains("spotify")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    Off,
    All,
    One,
}

impl Repeat {
    fn api(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::All => "context",
            Repeat::One => "track",
        }
    }

    fn from_api(s: &str) -> Repeat {
        match s {
            "context" => Repeat::All,
            "track" => Repeat::One,
            _ => Repeat::Off,
        }
    }

    /// The button's cycle: off, the list, the track, off.
    fn next(self) -> Repeat {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }
}

/// What Spotify's player is set to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Player {
    pub shuffle: bool,
    pub repeat: Repeat,
}

/// The last state seen or set, for the card's two buttons (the media session doesn't tell).
static PLAYER: Mutex<Option<Player>> = Mutex::new(None);
/// The access token and when it runs out.
static TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);
static CONNECTING: AtomicBool = AtomicBool::new(false);

pub fn player() -> Option<Player> {
    PLAYER.lock().ok().and_then(|p| *p)
}

/// Runs `job` on its own thread, logs how it went and wakes the UI.
pub fn run(job: fn() -> Result<(), String>) {
    std::thread::spawn(move || {
        if let Err(e) = job() {
            log(&format!("spotify: {e}"));
        }
        crate::platform::windows::post_media();
    });
}

/// The shuffle button.
pub fn toggle_shuffle() -> Result<(), String> {
    let token = access_token(true)?;
    let now = target(&token)?;
    let want = !now.player.shuffle;
    put(&token, &format!("/v1/me/player/shuffle?state={want}"), &now.device)?;
    set_player(Player { shuffle: want, ..now.player });
    Ok(())
}

/// The repeat button.
pub fn cycle_repeat() -> Result<(), String> {
    let token = access_token(true)?;
    let now = target(&token)?;
    let want = now.player.repeat.next();
    put(&token, &format!("/v1/me/player/repeat?state={}", want.api()), &now.device)?;
    set_player(Player { repeat: want, ..now.player });
    Ok(())
}

/// Reads the player's state for the card, if the user has connected already (never opens the
/// browser) and Spotify has a player going.
pub fn sync() -> Result<(), String> {
    if load_refresh().is_none() {
        return Ok(());
    }
    let token = access_token(false)?;
    if let Some(now) = active_player(&token)? {
        set_player(now);
    }
    Ok(())
}

/// Where a command goes: the state to start from, and the device to name when none is active.
struct Target {
    player: Player,
    device: Option<String>,
}

fn set_player(p: Player) {
    if let Ok(mut slot) = PLAYER.lock() {
        *slot = Some(p);
    }
}

fn get(token: &str, path: &str) -> Result<(u32, Vec<u8>), String> {
    request("GET", &format!("https://api.spotify.com{path}"), &[("Authorization", &format!("Bearer {token}"))], &[])
}

/// The state of Spotify's active player; `None` when nothing is active (Spotify forgets its
/// device after a while paused).
fn active_player(token: &str) -> Result<Option<Player>, String> {
    let (status, body) = get(token, "/v1/me/player")?;
    match status {
        200 => {
            let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("player state unreadable: {e}"))?;
            // What Spotify won't allow right now (a track played without a list can't shuffle).
            let off: Vec<&str> = v["actions"]["disallows"].as_object().map(|o| o.iter().filter(|(_, b)| b.as_bool() == Some(true)).map(|(k, _)| k.as_str()).collect()).unwrap_or_default();
            if !off.is_empty() {
                log(&format!("spotify: Spotify disallows right now: {}", off.join(", ")));
            }
            Ok(Some(parse_player(&body).ok_or("player state unreadable")?))
        }
        204 => Ok(None),
        s => Err(api_error(s, &body)),
    }
}

/// The player to act on: the active one, or, with none, the desktop app's own device (named in
/// the command, which Spotify takes for an inactive device too).
fn target(token: &str) -> Result<Target, String> {
    if let Some(player) = active_player(token)? {
        return Ok(Target { player, device: None });
    }
    let (status, body) = get(token, "/v1/me/player/devices")?;
    if status != 200 {
        return Err(api_error(status, &body));
    }
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("devices unreadable: {e}"))?;
    let devices = v["devices"].as_array().map(Vec::as_slice).unwrap_or_default();
    let pick = devices.iter().find(|d| d["is_active"].as_bool() == Some(true)).or_else(|| devices.iter().find(|d| d["type"].as_str() == Some("Computer"))).or_else(|| devices.first());
    let device = pick.ok_or("Spotify shows no device: open the Spotify app and play something once")?;
    log(&format!("spotify: no active player, using {} ({})", device["name"].as_str().unwrap_or("?"), device["type"].as_str().unwrap_or("?")));
    let id = device["id"].as_str().ok_or("that Spotify device has no id (it can't be controlled)")?.to_string();
    Ok(Target { player: player().unwrap_or(Player { shuffle: false, repeat: Repeat::Off }), device: Some(id) })
}

fn put(token: &str, path: &str, device: &Option<String>) -> Result<(), String> {
    let path = match device {
        Some(id) => format!("{path}&device_id={}", pct(id)),
        None => path.to_string(),
    };
    let (status, body) = request("PUT", &format!("https://api.spotify.com{path}"), &[("Authorization", &format!("Bearer {token}")), ("Content-Length", "0")], &[])?;
    match status {
        200 | 202 | 204 => Ok(()),
        s => Err(api_error(s, &body)),
    }
}

fn api_error(status: u32, body: &[u8]) -> String {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let said = v["error"]["message"].as_str().unwrap_or_default();
    let reason = v["error"]["reason"].as_str().unwrap_or_default();
    match status {
        403 => format!("Spotify says no (HTTP 403 {reason}: {said}). Premium is needed, and this account on the app's allow-list"),
        404 => format!("Spotify has no device to run it on (HTTP 404 {reason}: {said}). Play something in the Spotify app"),
        s => format!("HTTP {s} {reason} {said}"),
    }
}

fn parse_player(body: &[u8]) -> Option<Player> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    Some(Player { shuffle: v["shuffle_state"].as_bool()?, repeat: Repeat::from_api(v["repeat_state"].as_str().unwrap_or("off")) })
}

// ---- Login ----

/// A valid access token: the cached one, a refreshed one, or (with `connect`) one from a fresh
/// login in the browser.
fn access_token(connect: bool) -> Result<String, String> {
    if let Some((token, until)) = TOKEN.lock().ok().and_then(|t| t.clone()) {
        if Instant::now() < until {
            return Ok(token);
        }
    }
    if let Some(refresh) = load_refresh() {
        let body = form(&[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", &client_id())]);
        match token_call(&body) {
            Ok((access, new_refresh, life)) => {
                remember(&access, new_refresh, life);
                return Ok(access);
            }
            Err(e) => {
                // Revoked or expired for good: forget it, and ask again if we may.
                log(&format!("spotify: refresh failed ({e})"));
                let _ = std::fs::remove_file(token_path());
            }
        }
    }
    if !connect {
        return Err("not connected".into());
    }
    connect_in_browser()
}

fn remember(access: &str, refresh: Option<String>, life_s: u64) {
    if let Ok(mut slot) = TOKEN.lock() {
        *slot = Some((access.to_string(), Instant::now() + Duration::from_secs(life_s.saturating_sub(60))));
    }
    if let Some(r) = refresh {
        save_refresh(&r);
    }
}

fn token_call(body: &str) -> Result<(String, Option<String>, u64), String> {
    let (status, resp) = request("POST", "https://accounts.spotify.com/api/token", &[("Content-Type", "application/x-www-form-urlencoded")], body.as_bytes())?;
    let v: serde_json::Value = serde_json::from_slice(&resp).map_err(|e| format!("token reply unreadable: {e}"))?;
    if status != 200 {
        return Err(format!("HTTP {status} {}", v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("")));
    }
    let access = v["access_token"].as_str().ok_or("no access token in the reply")?.to_string();
    Ok((access, v["refresh_token"].as_str().map(String::from), v["expires_in"].as_u64().unwrap_or(3600)))
}

/// The browser login: Spotify's page, then back to a one-shot listener on this machine.
fn connect_in_browser() -> Result<String, String> {
    let id = client_id();
    if id.is_empty() {
        return Err("no Spotify client ID (see CLIENT_ID in src/activities/spotify.rs)".into());
    }
    if CONNECTING.swap(true, Ordering::AcqRel) {
        return Err("already waiting for the browser login".into());
    }
    let done = login(&id);
    CONNECTING.store(false, Ordering::Release);
    done
}

fn login(id: &str) -> Result<String, String> {
    let verifier = b64url(&random(64)?);
    let challenge = b64url(&sha256(verifier.as_bytes())?);
    let state = b64url(&random(16)?);
    // Listening before the browser opens, so a quick approval can't beat us to it.
    let listener = TcpListener::bind(("127.0.0.1", PORT)).map_err(|e| format!("port {PORT} is taken: {e}"))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    open_browser(&authorize_url(id, &challenge, &state));
    log("spotify: waiting for the browser login");
    let deadline = Instant::now() + CONNECT_WAIT;
    let code = loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                if !head.starts_with("GET /callback") {
                    // The browser's favicon and the like.
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let got = code_from_request(&head, &state);
                let page = match &got {
                    Ok(_) => "Pebbly is connected to Spotify. You can close this tab.",
                    Err(_) => "Pebbly could not connect to Spotify. You can close this tab and try again.",
                };
                let html = format!("<!doctype html><meta charset=utf-8><title>Pebbly</title><body style=\"font:16px system-ui;margin:3em\">{page}");
                let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len()).as_bytes());
                break got?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Err("the browser login timed out".into());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("login listener: {e}")),
        }
    };
    let body = form(&[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", &redirect()), ("client_id", id), ("code_verifier", &verifier)]);
    let (access, refresh, life) = token_call(&body)?;
    remember(&access, refresh, life);
    log("spotify: connected");
    Ok(access)
}

fn authorize_url(client_id: &str, challenge: &str, state: &str) -> String {
    format!(
        "https://accounts.spotify.com/authorize?{}",
        form(&[
            ("client_id", client_id),
            ("response_type", "code"),
            ("redirect_uri", &redirect()),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge),
            ("state", state),
            ("scope", SCOPE),
        ])
    )
}

/// The `code` in the browser's request to our listener, if its `state` is ours.
fn code_from_request(head: &str, state: &str) -> Result<String, String> {
    let line = head.lines().next().unwrap_or("");
    let query = line.split_whitespace().nth(1).and_then(|target| target.split_once('?')).map_or("", |(_, q)| q);
    let get = |key: &str| query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| unpct(v));
    if let Some(error) = get("error") {
        return Err(format!("Spotify said: {error}"));
    }
    if get("state").as_deref() != Some(state) {
        return Err("the login's state didn't match".into());
    }
    get("code").filter(|c| !c.is_empty()).ok_or_else(|| "no code in the login's reply".into())
}

fn open_browser(url: &str) {
    unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(None, windows::core::w!("open"), &windows::core::HSTRING::from(url), None, None, windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL);
    }
}

// ---- Small encoders ----

/// `key=value&…`, percent-encoded.
fn form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", pct(k), pct(v))).collect::<Vec<_>>().join("&")
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unpct(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Base64 without padding, URL-safe alphabet (RFC 4648 §5): what PKCE wants.
fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

// ---- Windows' crypto: random bytes, SHA-256, and DPAPI for the refresh token ----

fn random(n: usize) -> Result<Vec<u8>, String> {
    use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    let mut buf = vec![0u8; n];
    unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.ok().map_err(|e| format!("random: {e:?}"))?;
    Ok(buf)
}

fn sha256(data: &[u8]) -> Result<[u8; 32], String> {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) }.ok().map_err(|e| format!("sha256: {e:?}"))?;
    Ok(out)
}

fn token_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Pebbly").join("spotify.bin")
}

fn save_refresh(refresh: &str) {
    match protect(refresh.as_bytes()) {
        Some(sealed) => {
            let path = token_path();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(&path, sealed) {
                log(&format!("spotify: could not keep the login: {e}"));
            }
        }
        None => log("spotify: could not encrypt the login, not kept"),
    }
}

fn load_refresh() -> Option<String> {
    let sealed = std::fs::read(token_path()).ok()?;
    String::from_utf8(unprotect(&sealed)?).ok()
}

/// Encrypts for this Windows account (DPAPI): the file is useless on another account or machine.
fn protect(data: &[u8]) -> Option<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(&input, windows::core::w!("Pebbly Spotify"), None, None, None, 0, &mut out).ok()?;
        Some(take_blob(out))
    }
}

fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&input, None, None, None, None, 0, &mut out).ok()?;
        Some(take_blob(out))
    }
}

/// Copies a DPAPI output blob and frees it.
unsafe fn take_blob(blob: windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let bytes = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
    let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(blob.pbData as *mut _)));
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_the_rfc_example() {
        // RFC 7636, appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(b64url(&sha256(verifier.as_bytes()).unwrap()), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn base64url_has_no_padding() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn a_verifier_is_long_enough_and_url_safe() {
        let v = b64url(&random(64).unwrap());
        assert!((43..=128).contains(&v.len()), "{}", v.len());
        assert!(v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_ne!(v, b64url(&random(64).unwrap()));
    }

    #[test]
    fn the_token_survives_dpapi_and_is_not_plain_in_the_file() {
        let sealed = protect(b"refresh-token-123").unwrap();
        assert!(!String::from_utf8_lossy(&sealed).contains("refresh-token-123"));
        assert_eq!(unprotect(&sealed).unwrap(), b"refresh-token-123");
    }

    #[test]
    fn the_login_url_asks_for_pkce_and_the_two_scopes() {
        let url = authorize_url("abc", "chal", "st");
        assert!(url.starts_with("https://accounts.spotify.com/authorize?client_id=abc&response_type=code"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A20035%2Fcallback"));
        assert!(url.contains("code_challenge_method=S256&code_challenge=chal&state=st"));
        assert!(url.contains("scope=user-read-playback-state%20user-modify-playback-state"));
    }

    #[test]
    fn the_callback_gives_its_code_only_for_our_state() {
        let ok = "GET /callback?code=AQD%2Fxyz&state=st HTTP/1.1\r\nHost: 127.0.0.1:20035\r\n\r\n";
        assert_eq!(code_from_request(ok, "st"), Ok("AQD/xyz".into()));
        assert!(code_from_request(ok, "other").is_err(), "someone else's login");
        assert!(code_from_request("GET /callback?error=access_denied&state=st HTTP/1.1\r\n\r\n", "st").unwrap_err().contains("access_denied"));
        assert!(code_from_request("GET /callback?state=st HTTP/1.1\r\n\r\n", "st").is_err());
    }

    #[test]
    fn the_player_state_and_the_repeat_cycle() {
        let p = parse_player(br#"{"shuffle_state":true,"repeat_state":"track","is_playing":true}"#).unwrap();
        assert_eq!(p, Player { shuffle: true, repeat: Repeat::One });
        assert_eq!(parse_player(br#"{"shuffle_state":false,"repeat_state":"context"}"#).unwrap().repeat, Repeat::All);
        assert_eq!(Repeat::Off.next(), Repeat::All);
        assert_eq!(Repeat::All.next(), Repeat::One);
        assert_eq!(Repeat::One.next(), Repeat::Off);
        assert_eq!(Repeat::from_api(Repeat::One.api()), Repeat::One);
        assert!(parse_player(b"{}").is_none());
    }

    #[test]
    fn only_the_desktop_app_is_spotify() {
        assert!(is_spotify("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"));
        assert!(is_spotify("Spotify.exe"));
        assert!(!is_spotify("Chrome"));
    }
}
