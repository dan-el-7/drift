//! JNI entry points for the Android companion (`app.drift.Core`), over the same JSON protocol.
//!
//! ```kotlin
//! external fun start(config: String): Long   // events arrive on onEvent(json) (core thread)
//! external fun command(handle: Long, json: String): Int
//! external fun stop(handle: Long)
//! ```

use std::ffi::CString;

use jni::JNIEnv;
use jni::JavaVM;
use jni::objects::{GlobalRef, JObject, JString, JValue};
use jni::sys::{jint, jlong};

struct Listener {
    vm: JavaVM,
    target: GlobalRef,
}

struct Handle {
    core: *mut crate::Core,
    listener: *mut Listener,
}

unsafe extern "C" fn on_event(json: *const u8, len: usize, user: isize) {
    let l = unsafe { &*(user as *const Listener) };
    let text = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(json, len) }).into_owned();
    let Ok(mut env) = l.vm.attach_current_thread_permanently() else { return };
    if let Ok(s) = env.new_string(text) {
        let _ = env.call_method(l.target.as_obj(), "onEvent", "(Ljava/lang/String;)V", &[JValue::Object(&s)]);
        let _ = env.delete_local_ref(s);
    }
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_drift_Core_start<'l>(mut env: JNIEnv<'l>, this: JObject<'l>, config: JString<'l>) -> jlong {
    let Ok(cfg) = env.get_string(&config).map(String::from) else { return 0 };
    let (Ok(vm), Ok(target)) = (env.get_java_vm(), env.new_global_ref(this)) else { return 0 };
    let listener = Box::into_raw(Box::new(Listener { vm, target }));
    let Ok(c) = CString::new(cfg) else { return 0 };
    let core = unsafe { crate::drift_start(c.as_ptr(), Some(on_event), listener as isize) };
    if core.is_null() {
        drop(unsafe { Box::from_raw(listener) });
        return 0;
    }
    Box::into_raw(Box::new(Handle { core, listener })) as jlong
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_drift_Core_command<'l>(mut env: JNIEnv<'l>, _this: JObject<'l>, handle: jlong, json: JString<'l>) -> jint {
    if handle == 0 {
        return -2;
    }
    let h = unsafe { &*(handle as *const Handle) };
    let Ok(text) = env.get_string(&json).map(String::from) else { return -1 };
    let Ok(c) = CString::new(text) else { return -1 };
    unsafe { crate::drift_command(h.core, c.as_ptr()) }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_app_drift_Core_stop<'l>(_env: JNIEnv<'l>, _this: JObject<'l>, handle: jlong) {
    if handle == 0 {
        return;
    }
    let h = unsafe { Box::from_raw(handle as *mut Handle) };
    unsafe { crate::drift_stop(h.core) }; // no callback runs after this
    drop(unsafe { Box::from_raw(h.listener) });
}

/// Bluetooth from Rust on Android: the core asks the app to open an RFCOMM link (`bt_connect`
/// event); the app connects, then dials our one-shot loopback port and writes the token first
/// (all zeros = the Bluetooth connect failed), and pumps bytes both ways.
pub(crate) fn install_bt_connector(emit: crate::Emit) {
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    let c: drift_app::nearby::BtConnector = Arc::new(move |mac: [u8; 6], uuid: [u8; 16]| {
        let emit = emit.clone();
        Box::pin(async move {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let port = l.local_addr()?.port();
            let token: [u8; 16] = rand::random();
            let hex = |b: &[u8], sep: &str| b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(sep);
            emit.send(serde_json::json!({
                "t": "bt_connect", "mac": hex(&mac, ":"), "uuid": drift_core::ble::uuid_string(&uuid),
                "port": port, "token": hex(&token, ""),
            }));
            let (mut s, peer) = tokio::time::timeout(std::time::Duration::from_secs(25), l.accept())
                .await
                .map_err(|_| anyhow::anyhow!("Bluetooth connect timed out"))??;
            anyhow::ensure!(peer.ip().is_loopback(), "bridge from {peer}");
            let mut got = [0u8; 16];
            tokio::time::timeout(std::time::Duration::from_secs(5), s.read_exact(&mut got))
                .await
                .map_err(|_| anyhow::anyhow!("Bluetooth bridge sent no token"))??;
            anyhow::ensure!(got != [0u8; 16], "Bluetooth connect failed");
            anyhow::ensure!(got == token, "Bluetooth bridge: wrong token");
            let _ = s.set_nodelay(true);
            tracing::info!(port, "bluetooth bridged");
            Ok(s)
        })
    });
    drift_app::nearby::set_bt_connector(Some(c));
}

/// The phone's own Bluetooth "port": the core tells the app what to advertise (`bt_advertise`,
/// data null = stop) and where to bring accepted RFCOMM links (`bt_listen`: Drift's service UUID,
/// a loopback port and a token). The app writes the token and the peer's 6-byte address first.
pub(crate) fn install_bt_port(emit: crate::Emit, rt: &tokio::runtime::Runtime) {
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02X}")).collect::<String>();
    let adv_emit = emit.clone();
    drift_app::set_external_bt(Some(Arc::new(drift_app::ExternalBt {
        advertise: Box::new(move |adv: Option<Vec<u8>>| {
            adv_emit.send(serde_json::json!({ "t": "bt_advertise", "data": adv.map(|a| hex(&a)) }));
        }),
    })));
    rt.spawn(async move {
        let l = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) => return tracing::warn!("bt port: {e}"),
        };
        let Ok(port) = l.local_addr().map(|a| a.port()) else { return };
        let token: [u8; 16] = rand::random();
        let uuid = drift_core::ble::uuid_string(&drift_core::ble::name_uuid(drift_app::nearby::DRIFT_BT_SERVICE));
        emit.send(serde_json::json!({ "t": "bt_listen", "uuid": uuid, "port": port, "token": hex(&token) }));
        loop {
            let Ok((mut s, peer)) = l.accept().await else { continue };
            if !peer.ip().is_loopback() {
                continue;
            }
            tokio::spawn(async move {
                let mut head = [0u8; 22];
                let ok = tokio::time::timeout(std::time::Duration::from_secs(5), s.read_exact(&mut head)).await;
                if !matches!(ok, Ok(Ok(_))) || head[..16] != token {
                    return tracing::warn!("bt port: bad bridge");
                }
                let mac = head[16..].iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(":");
                let _ = s.set_nodelay(true);
                tracing::info!(%mac, "bluetooth link in");
                drift_app::bt_incoming(s, mac);
            });
        }
    });
}

// ------------------------------------------------------------------ Wi-Fi

static JOINS: std::sync::Mutex<Option<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<u64, String>>>>> =
    std::sync::Mutex::new(None);
static NEXT_JOIN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Only the app can join Wi-Fi on Android (WifiNetworkSpecifier). The core asks (`wifi_join`),
/// the app answers with the network's handle (`wifi_joined`); sockets are then bound to it.
pub(crate) struct AndroidPlatform {
    pub emit: crate::Emit,
}

/// Leaves the joined network when the share is done with it.
struct Leave {
    emit: crate::Emit,
    req: u64,
}
impl Drop for Leave {
    fn drop(&mut self) {
        self.emit.send(serde_json::json!({ "t": "wifi_leave", "req": self.req }));
    }
}

static CONNS: std::sync::Mutex<Option<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<i32, String>>>>> =
    std::sync::Mutex::new(None);

pub(crate) fn wifi_connected(conn: u64, fd: Option<i32>, error: Option<String>) {
    let tx = CONNS.lock().unwrap().as_mut().and_then(|m| m.remove(&conn));
    match (tx, fd) {
        (Some(tx), fd) => {
            let _ = tx.send(fd.ok_or_else(|| error.unwrap_or_else(|| "couldn't connect".into())));
        }
        // nobody waits any more: close it
        (None, Some(fd)) => drop(unsafe { <std::net::TcpStream as std::os::fd::FromRawFd>::from_raw_fd(fd) }),
        (None, None) => {}
    }
}

pub(crate) fn wifi_joined(req: u64, net: Option<u64>, error: Option<String>) {
    let tx = JOINS.lock().unwrap().as_mut().and_then(|m| m.remove(&req));
    if let Some(tx) = tx {
        let _ = tx.send(net.ok_or_else(|| error.unwrap_or_else(|| "couldn't join".into())));
    }
}

/// A group the app started for us (WifiP2pManager, as group owner on p2p0; always 192.168.49.1).
struct Hosted {
    ssid: String,
    password: String,
    device_name: Option<String>,
    frequency: i32,
}

static HOSTS: std::sync::Mutex<Option<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<Hosted, String>>>>> =
    std::sync::Mutex::new(None);

pub(crate) fn p2p_hosted(req: u64, creds: Option<(String, String)>, device_name: Option<String>, frequency: i32, error: Option<String>) {
    let tx = HOSTS.lock().unwrap().as_mut().and_then(|m| m.remove(&req));
    if let Some(tx) = tx {
        let _ = tx.send(match creds {
            Some((ssid, password)) => Ok(Hosted { ssid, password, device_name, frequency }),
            None => Err(error.unwrap_or_else(|| "couldn't start a Wi-Fi Direct group".into())),
        });
    }
}

/// Ends the group when the share is done with it (or gave up waiting for it).
struct Unhost {
    emit: crate::Emit,
    req: u64,
}
impl Drop for Unhost {
    fn drop(&mut self) {
        self.emit.send(serde_json::json!({ "t": "p2p_unhost", "req": self.req }));
    }
}

impl drift_net::Platform for AndroidPlatform {
    /// The app hosts a Wi-Fi Direct group (a saved one, so a PC that joined before gets back in
    /// without a prompt); the core listens on it and offers it as WIFI_DIRECT.
    fn host_group(&self) -> drift_net::upgrade::BoxFuture<'_, anyhow::Result<drift_net::upgrade::HostedGroup>> {
        let emit = self.emit.clone();
        Box::pin(async move {
            let req = NEXT_JOIN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (tx, rx) = tokio::sync::oneshot::channel();
            HOSTS.lock().unwrap().get_or_insert_with(Default::default).insert(req, tx);
            tracing::info!(req, "asking the app to host a Wi-Fi Direct group");
            emit.send(serde_json::json!({ "t": "p2p_host", "req": req }));
            let guard = Unhost { emit, req };
            match tokio::time::timeout(std::time::Duration::from_secs(15), rx).await {
                Ok(Ok(Ok(h))) => {
                    tracing::info!(req, ssid = %h.ssid, freq = h.frequency, device = ?h.device_name, "group up");
                    Ok(drift_net::upgrade::HostedGroup {
                        ssid: h.ssid,
                        password: h.password,
                        ip: std::net::Ipv4Addr::new(192, 168, 49, 1),
                        device_name: h.device_name,
                        frequency: h.frequency,
                        guard: Box::new(guard),
                    })
                }
                Ok(Ok(Err(e))) => anyhow::bail!("{e}"),
                Ok(Err(_)) => anyhow::bail!("hosting cancelled"),
                Err(_) => anyhow::bail!("the app didn't start a group in 15 s"),
            }
        })
    }

    fn join_wifi(&self, ssid: String, password: String, frequency: i32) -> drift_net::upgrade::BoxFuture<'_, anyhow::Result<drift_net::upgrade::JoinedWifi>> {
        let emit = self.emit.clone();
        Box::pin(async move {
            let req = NEXT_JOIN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (tx, rx) = tokio::sync::oneshot::channel();
            JOINS.lock().unwrap().get_or_insert_with(Default::default).insert(req, tx);
            tracing::info!(req, %ssid, frequency, "asking the app to join Wi-Fi");
            emit.send(serde_json::json!({ "t": "wifi_join", "req": req, "ssid": ssid, "password": password, "frequency": frequency }));
            let leave = Leave { emit, req }; // also cancels the request if we give up
            let r = tokio::time::timeout(std::time::Duration::from_secs(60), rx).await;
            match r {
                Ok(Ok(Ok(net))) => {
                    tracing::info!(req, net, "joined");
                    let emit = leave.emit.clone();
                    // sockets on this network come from the app (Network.getSocketFactory), as fds
                    let connect: drift_net::upgrade::NetConnector = Box::new(move |addr: std::net::SocketAddrV4| {
                        let emit = emit.clone();
                        Box::pin(async move {
                            let conn = NEXT_JOIN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let (tx, rx) = tokio::sync::oneshot::channel();
                            CONNS.lock().unwrap().get_or_insert_with(Default::default).insert(conn, tx);
                            emit.send(serde_json::json!({ "t": "wifi_connect", "req": req, "conn": conn, "ip": addr.ip().to_string(), "port": addr.port() }));
                            let r = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;
                            CONNS.lock().unwrap().as_mut().map(|m| m.remove(&conn));
                            let fd = match r {
                                Ok(Ok(Ok(fd))) => fd,
                                Ok(Ok(Err(e))) => anyhow::bail!("{e}"),
                                _ => anyhow::bail!("connect timed out"),
                            };
                            use std::os::fd::FromRawFd;
                            let s = unsafe { std::net::TcpStream::from_raw_fd(fd) };
                            s.set_nonblocking(true)?;
                            Ok(tokio::net::TcpStream::from_std(s)?)
                        })
                    });
                    Ok(drift_net::upgrade::JoinedWifi { connect: Some(connect), guard: Box::new(leave) })
                }
                Ok(Ok(Err(e))) => anyhow::bail!("couldn't join {ssid}: {e}"),
                _ => {
                    JOINS.lock().unwrap().as_mut().map(|m| m.remove(&req));
                    anyhow::bail!("joining {ssid} timed out")
                }
            }
        })
    }
}
