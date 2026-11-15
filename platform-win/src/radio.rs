//! Bluetooth radio recovery.
//!
//! After a Windows Wi-Fi Direct client session the Realtek combo chip once
//! stopped accepting Bluetooth connections (phone saw PAGE_TIMEOUT) while still
//! advertising, until the radio was toggled. The app offers this reset; it only
//! does it unprompted when no other Bluetooth device is connected.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use windows::Devices::Bluetooth::BluetoothDevice;
use windows::Devices::Bluetooth::BluetoothConnectionStatus;
use windows::Devices::Enumeration::DeviceInformation;
use windows::Devices::Radios::{Radio, RadioAccessStatus, RadioKind, RadioState};

async fn bluetooth_radio() -> Result<Radio> {
    if Radio::RequestAccessAsync()?.await? != RadioAccessStatus::Allowed {
        bail!("radio access denied");
    }
    for r in Radio::GetRadiosAsync()?.await? {
        if r.Kind()? == RadioKind::Bluetooth {
            return Ok(r);
        }
    }
    bail!("no Bluetooth radio found")
}

/// Names of currently connected Bluetooth Classic devices (headphones, etc.).
pub async fn connected_devices() -> Result<Vec<String>> {
    let sel = BluetoothDevice::GetDeviceSelectorFromConnectionStatus(BluetoothConnectionStatus::Connected)?;
    let list = DeviceInformation::FindAllAsyncAqsFilter(&sel)?.await?;
    Ok(list.into_iter().filter_map(|d| d.Name().ok().map(|n| n.to_string())).collect())
}

/// Turn the Bluetooth radio off and on again.
pub async fn reset_bluetooth() -> Result<()> {
    let radio = bluetooth_radio().await?;
    radio.SetStateAsync(RadioState::Off)?.await.context("radio off")?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    radio.SetStateAsync(RadioState::On)?.await.context("radio on")?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    Ok(())
}
