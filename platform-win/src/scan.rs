//! Discoverer-side scanning: BLE service data (receivers' Nearby adverts and
//! other senders' wake beacons) and Bluetooth Classic devices (Nearby encodes
//! the endpoint in the device name when `require_bt_advertising` is set).

use anyhow::Result;
use tokio::sync::mpsc;
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher, BluetoothLEScanningMode,
};
use windows::Devices::Bluetooth::BluetoothDevice;
use windows::Devices::Enumeration::{DeviceInformation, DeviceInformationUpdate, DeviceWatcher};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::DataReader;
use windows::core::{HSTRING, Ref};

#[derive(Debug, Clone)]
pub enum Seen {
    /// 16-bit-UUID service data from a BLE advertisement.
    Ble { address: u64, uuid16: u16, data: Vec<u8>, rssi: i16 },
    /// A Bluetooth Classic device (inquiry result) with its current name.
    Classic { id: String, name: String, address: Option<String> },
}

pub struct BleScanner(BluetoothLEAdvertisementWatcher);

impl Drop for BleScanner {
    fn drop(&mut self) {
        let _ = self.0.Stop();
    }
}

/// Report service data for any of `uuids` (16-bit) seen in BLE advertisements.
pub fn scan_ble(uuids: &'static [u16], tx: mpsc::UnboundedSender<Seen>) -> Result<BleScanner> {
    let w = BluetoothLEAdvertisementWatcher::new()?;
    w.SetScanningMode(BluetoothLEScanningMode::Active)?;
    let _ = w.SetAllowExtendedAdvertisements(true);
    w.Received(&TypedEventHandler::new(
        move |_: Ref<BluetoothLEAdvertisementWatcher>, args: Ref<BluetoothLEAdvertisementReceivedEventArgs>| {
            let Some(a) = args.as_ref() else { return Ok(()) };
            let address = a.BluetoothAddress()?;
            let rssi = a.RawSignalStrengthInDBm()?;
            for section in a.Advertisement()?.GetSectionsByType(0x16)? {
                let buf = section.Data()?;
                let len = buf.Length()? as usize;
                if len < 2 {
                    continue;
                }
                let mut bytes = vec![0u8; len];
                DataReader::FromBuffer(&buf)?.ReadBytes(&mut bytes)?;
                let uuid16 = u16::from_le_bytes([bytes[0], bytes[1]]);
                if uuids.contains(&uuid16) {
                    let _ = tx.send(Seen::Ble { address, uuid16, data: bytes[2..].to_vec(), rssi });
                }
            }
            Ok(())
        },
    ))?;
    w.Start()?;
    Ok(BleScanner(w))
}

pub struct ClassicScanner(DeviceWatcher);

impl Drop for ClassicScanner {
    fn drop(&mut self) {
        let _ = self.0.Stop();
    }
}

/// Bluetooth address (MSB-first hex) for a device id from the watcher.
fn address_of(id: &HSTRING) -> Option<String> {
    let dev = BluetoothDevice::FromIdAsync(id).ok()?.join().ok()?;
    let a = dev.BluetoothAddress().ok()?;
    Some(a.to_be_bytes()[2..].iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":"))
}

/// Bluetooth Classic inquiry for unpaired devices; reports names as they resolve.
pub fn scan_classic(tx: mpsc::UnboundedSender<Seen>) -> Result<ClassicScanner> {
    let selector = BluetoothDevice::GetDeviceSelectorFromPairingState(false)?;
    let w = DeviceInformation::CreateWatcherAqsFilter(&selector)?;
    let tx_added = tx.clone();
    w.Added(&TypedEventHandler::new(move |_: Ref<DeviceWatcher>, info: Ref<DeviceInformation>| {
        if let Some(i) = info.as_ref() {
            let _ = tx_added.send(Seen::Classic {
                id: i.Id()?.to_string(),
                name: i.Name()?.to_string(),
                address: address_of(&i.Id()?),
            });
        }
        Ok(())
    }))?;
    w.Updated(&TypedEventHandler::new(move |_: Ref<DeviceWatcher>, upd: Ref<DeviceInformationUpdate>| {
        if let Some(u) = upd.as_ref() {
            if let Ok(i) = DeviceInformation::CreateFromIdAsync(&u.Id()?).and_then(|op| op.join()) {
                let _ = tx.send(Seen::Classic { id: i.Id()?.to_string(), name: i.Name()?.to_string(), address: None });
            }
        }
        Ok(())
    }))?;
    w.Start()?;
    Ok(ClassicScanner(w))
}
