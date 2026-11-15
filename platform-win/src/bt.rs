//! Windows Bluetooth: RFCOMM server (Bluetooth Classic bootstrap channel) and
//! BLE advertising, mirroring google/nearby's Windows platform layer.

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisement, BluetoothLEAdvertisementDataSection, BluetoothLEAdvertisementPublisher,
    BluetoothLEAdvertisementPublisherStatus,
};
use windows::Devices::Bluetooth::{BluetoothAdapter, BluetoothDevice};
use windows::Devices::Bluetooth::Rfcomm::{RfcommServiceId, RfcommServiceProvider};
use windows::Foundation::TypedEventHandler;
use windows::Networking::Sockets::{
    SocketProtectionLevel, StreamSocket, StreamSocketListener, StreamSocketListenerConnectionReceivedEventArgs,
};
use windows::Storage::Streams::{ByteOrder, DataReader, DataWriter, InputStreamOptions, UnicodeEncoding};
use windows::core::{GUID, HSTRING};

const SDP_SERVICE_NAME_ATTRIBUTE_ID: u32 = 0x100;
const SDP_TEXT_STRING_1BYTE_LEN: u8 = (4 << 3) | 5;
const BLE_SERVICE_DATA_16: u8 = 0x16;

pub struct AdapterInfo {
    pub address: u64,
    pub extended_adv: bool,
}

impl AdapterInfo {
    /// MAC in display order (most significant byte first).
    pub fn mac(&self) -> [u8; 6] {
        self.address.to_be_bytes()[2..8].try_into().unwrap()
    }
    pub fn mac_string(&self) -> String {
        self.mac().iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
    }
}

pub fn adapter_info() -> Result<AdapterInfo> {
    let a = BluetoothAdapter::GetDefaultAsync()?.join().context("no Bluetooth adapter")?;
    Ok(AdapterInfo { address: a.BluetoothAddress()?, extended_adv: a.IsExtendedAdvertisingSupported()? })
}

/// Keeps the RFCOMM service registered while alive.
pub struct RfcommServer {
    provider: RfcommServiceProvider,
    _listener: StreamSocketListener,
}

impl Drop for RfcommServer {
    fn drop(&mut self) {
        let _ = self.provider.StopAdvertising();
    }
}

pub fn start_rfcomm(uuid: [u8; 16], service_name: &str, tx: mpsc::UnboundedSender<StreamSocket>) -> Result<RfcommServer> {
    let guid = GUID::from_u128(u128::from_be_bytes(uuid));
    let provider = RfcommServiceProvider::CreateAsync(&RfcommServiceId::FromUuid(guid)?)?
        .join()
        .context("RfcommServiceProvider::CreateAsync")?;
    let listener = StreamSocketListener::new()?;
    listener.ConnectionReceived(&TypedEventHandler::new(
        move |_, args: windows::core::Ref<StreamSocketListenerConnectionReceivedEventArgs>| {
            if let Some(args) = args.as_ref() {
                let _ = tx.send(args.Socket()?);
            }
            Ok(())
        },
    ))?;
    listener
        .BindServiceNameWithProtectionLevelAsync(&provider.ServiceId()?.AsString()?, SocketProtectionLevel::PlainSocket)?
        .join()
        .context("bind RFCOMM listener")?;

    let w = DataWriter::new()?;
    w.WriteByte(SDP_TEXT_STRING_1BYTE_LEN)?;
    w.WriteByte(service_name.len() as u8)?;
    w.SetUnicodeEncoding(UnicodeEncoding::Utf8)?;
    w.WriteString(&HSTRING::from(service_name))?;
    provider.SdpRawAttributes()?.Insert(SDP_SERVICE_NAME_ATTRIBUTE_ID, &w.DetachBuffer()?)?;
    provider.StartAdvertisingWithRadioDiscoverability(&listener, false)?;
    Ok(RfcommServer { provider, _listener: listener })
}

/// Bridge a WinRT StreamSocket to a tokio duplex stream.
pub fn bridge(sock: StreamSocket) -> Result<DuplexStream> {
    // Keep this small: RFCOMM runs at ~100-200 KB/s, and every byte buffered
    // here sits in front of control frames (an upgrade offer, LAST_WRITE). A
    // 1 MB pipe delayed the upgrade hand-over by ~7 s.
    let (ours, theirs) = tokio::io::duplex(32 * 1024);
    let (mut rd, mut wr) = tokio::io::split(theirs);

    let reader = DataReader::CreateDataReader(&sock.InputStream()?)?;
    reader.SetInputStreamOptions(InputStreamOptions::Partial)?;
    let sock_r = sock.clone();
    tokio::spawn(async move {
        let _keep = sock_r;
        loop {
            let n = match reader.LoadAsync(64 * 1024) {
                Ok(op) => match op.await {
                    Ok(n) if n > 0 => n as usize,
                    _ => break,
                },
                Err(_) => break,
            };
            let mut buf = vec![0u8; n];
            if reader.ReadBytes(&mut buf).is_err() || wr.write_all(&buf).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    let writer = DataWriter::CreateDataWriter(&sock.OutputStream()?)?;
    tokio::spawn(async move {
        let _keep = sock;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if writer.WriteBytes(&buf[..n]).is_err() {
                break;
            }
            let t = std::time::Instant::now();
            match writer.StoreAsync() {
                Ok(op) => {
                    if op.await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
            if t.elapsed() > std::time::Duration::from_millis(300) {
                tracing::debug!(n, took = ?t.elapsed(), "slow rfcomm write");
            }
        }
    });
    Ok(ours)
}

pub struct BlePublisher(BluetoothLEAdvertisementPublisher);

impl BlePublisher {
    /// False once Windows stopped it under us (the Bluetooth radio was turned off and on: the
    /// publisher aborts and never comes back by itself).
    pub fn is_running(&self) -> bool {
        !matches!(
            self.0.Status(),
            Ok(BluetoothLEAdvertisementPublisherStatus::Aborted | BluetoothLEAdvertisementPublisherStatus::Stopped) | Err(_)
        )
    }
}

impl Drop for BlePublisher {
    fn drop(&mut self) {
        let _ = self.0.Stop();
    }
}

/// Advertise `service_data` under the 16-bit UUID `uuid16`.
pub fn start_ble(uuid16: u16, service_data: &[u8], extended: bool) -> Result<BlePublisher> {
    let adv = BluetoothLEAdvertisement::new()?;
    let w = DataWriter::new()?;
    w.SetByteOrder(ByteOrder::LittleEndian)?;
    w.WriteUInt16(uuid16)?;
    w.WriteBytes(service_data)?;
    let section = BluetoothLEAdvertisementDataSection::Create(BLE_SERVICE_DATA_16, &w.DetachBuffer()?)?;
    adv.DataSections()?.Append(&section)?;
    let p = BluetoothLEAdvertisementPublisher::Create(&adv)?;
    if extended {
        p.SetUseExtendedAdvertisement(true)?;
    }
    p.Start()?;
    for _ in 0..60 {
        match p.Status()? {
            BluetoothLEAdvertisementPublisherStatus::Started => return Ok(BlePublisher(p)),
            BluetoothLEAdvertisementPublisherStatus::Aborted => bail!("BLE publisher aborted"),
            _ => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    bail!("BLE publisher did not start (status {:?})", p.Status()?)
}

/// Connect to a remote device's RFCOMM service (the phone's Nearby bootstrap
/// listener) by Bluetooth address. No pairing: Nearby uses insecure RFCOMM.
pub async fn connect_rfcomm(address: u64, uuid: [u8; 16]) -> Result<StreamSocket> {
    use windows::Devices::Bluetooth::BluetoothCacheMode;
    let dev = BluetoothDevice::FromBluetoothAddressAsync(address)?.await.context("BluetoothDevice::FromBluetoothAddressAsync")?;
    let id = RfcommServiceId::FromUuid(GUID::from_u128(u128::from_be_bytes(uuid)))?;
    let result = dev.GetRfcommServicesForIdWithCacheModeAsync(&id, BluetoothCacheMode::Uncached)?.await?;
    let services = result.Services()?;
    if services.Size()? == 0 {
        bail!("phone has no Nearby RFCOMM service (SDP error {:?})", result.Error()?);
    }
    let svc = services.GetAt(0)?;
    let sock = StreamSocket::new()?;
    // Small send buffer: queued bytes delay control frames (see `bridge`).
    let _ = sock.Control().and_then(|c| c.SetOutboundBufferSizeInBytes(16 * 1024));
    sock.ConnectWithProtectionLevelAsync(
        &svc.ConnectionHostName()?,
        &svc.ConnectionServiceName()?,
        SocketProtectionLevel::BluetoothEncryptionAllowNullAuthentication,
    )?
    .await
    .context("RFCOMM connect")?;
    Ok(sock)
}

/// Blocking variant of [`connect_rfcomm`] for use from `spawn_blocking`
/// (some WinRT objects involved aren't `Send`, so the async version can't
/// live inside a spawned task).
pub fn connect_rfcomm_blocking(address: u64, uuid: [u8; 16]) -> Result<StreamSocket> {
    use windows::Devices::Bluetooth::BluetoothCacheMode;
    let dev = BluetoothDevice::FromBluetoothAddressAsync(address)?.join().context("BluetoothDevice::FromBluetoothAddressAsync")?;
    let id = RfcommServiceId::FromUuid(GUID::from_u128(u128::from_be_bytes(uuid)))?;
    let result = dev.GetRfcommServicesForIdWithCacheModeAsync(&id, BluetoothCacheMode::Uncached)?.join()?;
    let services = result.Services()?;
    if services.Size()? == 0 {
        bail!("phone has no Nearby RFCOMM service (SDP error {:?})", result.Error()?);
    }
    let svc = services.GetAt(0)?;
    let sock = StreamSocket::new()?;
    // Small send buffer: queued bytes delay control frames (see `bridge`).
    let _ = sock.Control().and_then(|c| c.SetOutboundBufferSizeInBytes(16 * 1024));
    sock.ConnectWithProtectionLevelAsync(
        &svc.ConnectionHostName()?,
        &svc.ConnectionServiceName()?,
        SocketProtectionLevel::BluetoothEncryptionAllowNullAuthentication,
    )?
    .join()
    .context("RFCOMM connect")?;
    Ok(sock)
}
