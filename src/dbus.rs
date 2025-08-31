use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use futures_util::stream::StreamExt;
use serde_repr::Deserialize_repr;
use tracing::error;
use zbus::zvariant::serialized::Context;
use zbus::zvariant::{
    self, Array, Endian, ObjectPath, OwnedObjectPath, OwnedValue, Str, Type, Value,
};
use zbus::{Connection, proxy};

use crate::Error;

/// Listen for WiFi events.
pub async fn wifi_listen<F, G, H>(
    status_changed: F,
    aps_changed: G,
    auth_failed: H,
) -> Result<(), Error>
where
    F: Fn(bool),
    G: Fn(Vec<AccessPoint>),
    H: Fn(),
{
    // Attempt to connect to the system DBus.
    let connection = Connection::system().await?;

    // Get the NetworkManager device used for WiFi.
    let device = wireless_device(&connection).await.ok_or(Error::NoWirelessDevice)?;

    // Request rescan once at startup.
    let _ = device.request_scan(HashMap::new()).await;

    // Set initial toggle button state.
    let network_manager = NetworkManagerProxy::new(&connection).await?;
    let wifi_enabled = network_manager.wireless_enabled().await.unwrap_or_default();
    status_changed(wifi_enabled);

    // Get device state change stream.
    let raw_device = DeviceProxy::builder(&connection).path(device.0.path())?.build().await?;
    let mut device_state_stream = raw_device.receive_state_changed().await?;

    tokio::join!(
        // Listen for changes in WiFi status.
        async {
            let mut onoff_stream = network_manager.receive_wireless_enabled_changed().await;
            while let Some(new_state) = onoff_stream.next().await {
                if let Ok(new_state) = new_state.get().await {
                    status_changed(new_state);
                }
            }
        },
        // Listen for changes in visible APs.
        async {
            let mut ap_change_stream = device.receive_access_points_changed().await;
            while ap_change_stream.next().await.is_some() {
                match access_points(&connection).await {
                    Ok(aps) => aps_changed(aps),
                    Err(err) => error!("Failed to update WiFi APs: {err}"),
                }
            }
        },
        // Listen for changes in active AP.
        async {
            let mut active_ap_change_stream = device.receive_active_access_point_changed().await;
            while active_ap_change_stream.next().await.is_some() {
                match access_points(&connection).await {
                    Ok(aps) => aps_changed(aps),
                    Err(err) => error!("Failed to update WiFi APs: {err}"),
                }
            }
        },
        // Listen for device changes to handle authentication errors.
        async {
            while let Some(device_state) = device_state_stream.next().await {
                match device_state.args() {
                    Ok(args) => {
                        if args.new_state == DeviceState::Failed {
                            error!("Wireless device entered failed state: {:?}", args.reason);

                            if args.reason == DeviceStateReason::NoSecrets {
                                auth_failed();
                            }
                        }
                    },
                    Err(err) => error!("Failed to parse device state change: {err}"),
                }
            }
        },
    );

    Ok(())
}

/// Rescan for active APs.
pub async fn refresh() -> Result<(), zbus::Error> {
    let connection = Connection::system().await?;
    if let Some(device) = wireless_device(&connection).await {
        device.request_scan(HashMap::new()).await?;
    }
    Ok(())
}

/// NetworkManager access point.
#[derive(Clone, Debug)]
pub struct AccessPoint {
    /// AP hardware address.
    pub bssid: Arc<String>,

    /// Access point name.
    pub ssid: Arc<String>,

    /// Signal strength in percent.
    pub strength: u8,

    /// Requires password authentication.
    pub private: bool,

    /// WiFi frequency in MHz.
    pub frequency: u32,

    /// Access point is currently active.
    pub connected: bool,

    /// DBus access point object path.
    pub path: Arc<OwnedObjectPath>,

    /// DBus path of the connection profile.
    pub profile: Arc<Option<OwnedObjectPath>>,
}

impl AccessPoint {
    pub async fn from_nm_ap(
        connection: &Connection,
        path: OwnedObjectPath,
        active_bssid: Option<&str>,
    ) -> zbus::Result<Self> {
        let ap = AccessPointProxy::builder(connection).path(&path)?.build().await?;

        let ssid_bytes = ap.ssid().await?;
        let ssid = Arc::new(String::from_utf8(ssid_bytes).map_err(|_| zbus::Error::InvalidField)?);
        let private = ap.flags().await? != APFlags::None;
        let strength = ap.strength().await?;
        let frequency = ap.frequency().await?;
        let bssid = Arc::new(ap.hw_address().await?);
        let connected = active_bssid.is_some_and(|active| *bssid == active);

        Ok(Self {
            connected,
            frequency,
            strength,
            private,
            bssid,
            ssid,
            path: Arc::new(path),
            profile: Default::default(),
        })
    }
}

/// Set NetworkManager WiFi state.
pub async fn set_enabled(enabled: bool) -> zbus::Result<()> {
    let connection = Connection::system().await?;
    let network_manager = NetworkManagerProxy::new(&connection).await?;
    network_manager.set_wireless_enabled(enabled).await
}

/// Get all APs.
pub async fn access_points(connection: &Connection) -> zbus::Result<Vec<AccessPoint>> {
    // Get the WiFi device.
    let device = match wireless_device(connection).await {
        Some(device) => device,
        None => return Ok(Vec::new()),
    };

    // Get available AP profiles.
    let mut known_profiles = wifi_profiles(connection).await?;

    // Get the active access point.
    let active_ap = match device.active_access_point().await {
        // Filter out fallback AP `/`.
        Ok(path) if path.len() != 1 => AccessPoint::from_nm_ap(connection, path, None).await.ok(),
        _ => None,
    };
    let active_bssid = active_ap.as_ref().map(|ap| ap.bssid.as_str());

    // Get all access points.
    let aps = device.access_points().await?;

    // Collect required data from NetworkManager access points.
    let mut access_points = Vec::new();
    for ap in aps {
        if let Ok(mut access_point) = AccessPoint::from_nm_ap(connection, ap, active_bssid).await {
            access_point.profile = Arc::new(known_profiles.remove(&*access_point.bssid));
            access_points.push(access_point);
        }
    }

    // Sort by signal strength.
    access_points.sort_unstable_by(|a, b| match b.connected.cmp(&a.connected) {
        Ordering::Equal => b.strength.cmp(&a.strength),
        ordering => ordering,
    });

    Ok(access_points)
}

/// Get the wireless device.
pub async fn wireless_device(connection: &Connection) -> Option<WirelessDeviceProxy<'_>> {
    // Get network manager interface.
    let network_manager = NetworkManagerProxy::new(connection).await.ok()?;

    // Get realized network devices.
    let device_paths = network_manager.get_devices().await.ok()?;

    // Return the first wifi network device.
    for device_path in device_paths {
        let wireless_device = wireless_device_from_path(connection, device_path).await;
        if wireless_device.is_some() {
            return wireless_device;
        }
    }

    None
}

/// Try and convert a NetworkManager device path to a wireless device.
async fn wireless_device_from_path(
    connection: &Connection,
    device_path: OwnedObjectPath,
) -> Option<WirelessDeviceProxy<'_>> {
    // Resolve as generic device first.
    let device = DeviceProxy::builder(connection).path(&device_path).ok()?.build().await.ok()?;

    // Skip devices with incorrect type.
    if !matches!(device.device_type().await, Ok(DeviceType::Wifi)) {
        return None;
    }

    // Try ta resolve as wireless device.
    WirelessDeviceProxy::builder(connection).path(device_path).ok()?.build().await.ok()
}

/// Connect to an AP with a new profile.
pub async fn connect(
    ap_path: impl Into<ObjectPath<'_>>,
    ssid: &str,
    password: Option<String>,
) -> zbus::Result<()> {
    let connection = Connection::system().await?;

    // Get path for our wireless device.
    let device = match wireless_device(&connection).await {
        Some(device) => device,
        None => return Ok(()),
    };
    let device_path = device.0.path().to_owned();

    let mut settings = HashMap::new();

    // Add connection settings.
    let mut connection_settings = HashMap::new();
    connection_settings.insert("id", Value::Str(Str::from(ssid)));
    connection_settings.insert("type", Value::Str(Str::from("802-11-wireless")));
    settings.insert("connection", connection_settings);

    // Convert SSID to byte array.
    let context = Context::new_dbus(Endian::Little, 0);
    let ssid_sliced = zvariant::to_bytes(context, ssid)?;

    // Add WiFi settings.
    let mut wifi_settings = HashMap::new();
    wifi_settings.insert("mode", Value::Str(Str::from("infrastructure")));
    wifi_settings.insert("ssid", Value::Array(Array::from(&*ssid_sliced)));

    // Add password settings.
    if let Some(password) = password {
        let mut security_settings = HashMap::new();
        security_settings.insert("auth-alg", Value::Str(Str::from("open")));
        security_settings.insert("psk", Value::Str(Str::from(password)));
        security_settings.insert("key-mgmt", Value::Str(Str::from("wpa-psk")));
        settings.insert("802-11-wireless-security", security_settings);
    }

    // Create and activate the profile.
    let network_manager = NetworkManagerProxy::new(&connection).await?;
    network_manager.add_and_activate_connection(settings, device_path, ap_path.into()).await?;

    Ok(())
}

/// Reconnect to a known AP.
pub async fn reconnect(
    ap_path: impl Into<ObjectPath<'_>>,
    profile: impl Into<ObjectPath<'static>>,
) -> zbus::Result<()> {
    let connection = Connection::system().await?;

    // Get path for our wireless device.
    let device = match wireless_device(&connection).await {
        Some(device) => device,
        None => return Ok(()),
    };
    let device_path = device.0.path().to_owned();

    let network_manager = NetworkManagerProxy::new(&connection).await?;
    network_manager.activate_connection(profile.into(), device_path, ap_path.into()).await?;

    Ok(())
}

/// Disconnect from an active connection.
pub async fn disconnect(ssid: &str) -> zbus::Result<()> {
    let connection = Connection::system().await?;
    let network_manager = NetworkManagerProxy::new(&connection).await?;

    let active_connections = network_manager.active_connections().await?;
    for path in active_connections {
        let active_connection =
            ActiveConnectionProxy::builder(&connection).path(&path)?.build().await?;
        let id = active_connection.id().await?;
        if id == ssid {
            network_manager.deactivate_connection(path.as_ref()).await?;
            break;
        }
    }

    Ok(())
}

/// Delete a WiFi profile.
pub async fn forget(profile_path: impl Into<ObjectPath<'_>>) -> zbus::Result<()> {
    let connection = Connection::system().await?;
    let profile = ConnectionProxy::builder(&connection).path(profile_path)?.build().await?;
    profile.delete().await
}

/// Get known WiFi connection settings by BSSID.
pub async fn wifi_profiles(
    connection: &Connection,
) -> zbus::Result<HashMap<String, OwnedObjectPath>> {
    // Get network profiles.
    let settings = SettingsProxy::new(connection).await?;
    let network_profiles = settings.list_connections().await?;

    // Get BSSIDs for all known profiles.
    let mut profiles = HashMap::new();
    for profile_path in network_profiles {
        for bssid in wifi_bssids(connection, &profile_path).await.unwrap_or_default() {
            profiles.insert(bssid, profile_path.clone());
        }
    }

    Ok(profiles)
}

/// Get BSSIDs for a WiFi connection setting.
async fn wifi_bssids(
    connection: &Connection,
    profile_path: &OwnedObjectPath,
) -> Option<Vec<String>> {
    // Extract BSSIDs from settings.
    let profile =
        ConnectionProxy::builder(connection).path(profile_path).ok()?.build().await.ok()?;
    let settings = profile.get_settings().await.ok()?;
    let wifi_settings = settings.get("802-11-wireless")?;
    let bssids_setting = wifi_settings.get("seen-bssids")?;

    // Convert BSSID array to Rust array.
    let bssid_values = match &**bssids_setting {
        Value::Array(array) => array,
        _ => return None,
    };

    // Convert BSSID value string to Rust string.
    let bssids = bssid_values
        .iter()
        .filter_map(|value| match value {
            Value::Str(bssid) => Some(bssid.as_str().to_owned()),
            _ => None,
        })
        .collect();

    Some(bssids)
}

#[proxy(assume_defaults = true)]
pub trait NetworkManager {
    /// Get the list of realized network devices.
    fn get_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    /// Activate a connection using the supplied device.
    fn activate_connection(
        &self,
        connection: ObjectPath<'_>,
        device: ObjectPath<'_>,
        specific_object: ObjectPath<'_>,
    ) -> zbus::Result<OwnedObjectPath>;

    /// Adds a new connection using the given details (if any) as a template
    /// (automatically filling in missing settings with the capabilities of the
    /// given device and specific object), then activate the new connection.
    /// Cannot be used for VPN connections at this time.
    fn add_and_activate_connection(
        &self,
        connection: HashMap<&str, HashMap<&str, Value<'_>>>,
        device: ObjectPath<'_>,
        specific_object: ObjectPath<'_>,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;

    /// Deactivate an active connection.
    fn deactivate_connection(&self, connection: ObjectPath<'_>) -> zbus::Result<()>;

    /// Control whether overall networking is enabled or disabled. When
    /// disabled, all interfaces that NM manages are deactivated. When enabled,
    /// all managed interfaces are re-enabled and available to be activated.
    /// This command should be used by clients that provide to users the ability
    /// to enable/disable all networking.
    fn enable(&self, enable: bool) -> zbus::Result<()>;

    /// Indicates if wireless is currently enabled or not.
    #[zbus(property)]
    fn wireless_enabled(&self) -> zbus::Result<bool>;

    /// Set if wireless is currently enabled or not.
    #[zbus(property)]
    fn set_wireless_enabled(&self, enabled: bool) -> zbus::Result<()>;

    /// List of active connection object paths.
    #[zbus(property)]
    fn active_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/Device"
)]
trait Device {
    /// Disconnects a device and prevents the device from automatically
    /// activating further connections without user intervention.
    fn disconnect(&self) -> zbus::Result<()>;

    /// The general type of the network device; ie Ethernet, Wi-Fi, etc.
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<DeviceType>;

    /// Device state change emitter.
    #[zbus(signal)]
    fn state_changed(
        &self,
        new_state: DeviceState,
        old_state: DeviceState,
        reason: DeviceStateReason,
    ) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wireless",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/Device/Wireless"
)]
pub trait WirelessDevice {
    /// Request the device to scan. To know when the scan is finished, use the
    /// "PropertiesChanged" signal from "org.freedesktop.DBus.Properties" to
    /// listen to changes to the "LastScan" property.
    fn request_scan(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;

    /// List of object paths of access point visible to this wireless device.
    #[zbus(property)]
    fn access_points(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    /// Object path of the access point currently used by the wireless device.
    #[zbus(property)]
    fn active_access_point(&self) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.AccessPoint",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/AccessPoint"
)]
trait AccessPoint {
    /// Flags describing the capabilities of the access point.
    #[zbus(property)]
    fn flags(&self) -> zbus::Result<APFlags>;

    /// The Service Set Identifier identifying the access point.
    #[zbus(property)]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;

    /// The radio channel frequency in use by the access point, in MHz.
    #[zbus(property)]
    fn frequency(&self) -> zbus::Result<u32>;

    /// The hardware address (BSSID) of the access point.
    #[zbus(property)]
    fn hw_address(&self) -> zbus::Result<String>;

    /// The current signal quality of the access point, in percent.
    #[zbus(property)]
    fn strength(&self) -> zbus::Result<u8>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Settings",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/Settings"
)]
trait Settings {
    /// List the saved network connections known to NetworkManager.
    fn list_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Settings.Connection",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/Settings/Connection"
)]
trait Connection {
    /// Delete the connection.
    fn delete(&self) -> zbus::Result<()>;

    /// Get the settings maps describing this network configuration. This will
    /// never include any secrets required for connection to the network, as
    /// those are often protected. Secrets must be requested separately using
    /// the GetSecrets() call.
    fn get_settings(&self) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;

    /// Get the secrets belonging to this network configuration. Only secrets
    /// from persistent storage or a Secret Agent running in the requestor's
    /// session will be returned. The user will never be prompted for secrets as
    /// a result of this request.
    fn get_secrets(
        &self,
        setting_name: &str,
    ) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/ActiveConnection"
)]
trait ActiveConnection {
    /// The ID of the connection, provided as a convenience so that clients do
    /// not have to retrieve all connection details.
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
}

/// NMDeviceType values indicate the type of hardware represented by a device
/// object.
#[derive(Type, OwnedValue, PartialEq, Debug)]
#[repr(u32)]
pub enum DeviceType {
    Wifi = 2,
    Modem = 8,
}

/// 802.11 access point flags.
#[derive(Type, OwnedValue, PartialEq, Debug)]
#[repr(u32)]
pub enum APFlags {
    None = 0,
    Privacy = 1,
    Wps = 2,
    WpsPbc = 4,
    WpsPin = 8,
}

/// Device state.
#[derive(Deserialize_repr, Type, OwnedValue, PartialEq, Debug)]
#[repr(u32)]
pub enum DeviceState {
    // The device's state is unknown.
    Unknown = 0,
    // The device is recognized, but not managed by NetworkManager.
    Unmanaged = 10,
    // The device is managed by NetworkManager, but is not available for use. Reasons may include
    // the wireless switched off, missing firmware, no ethernet carrier, missing supplicant or
    // modem manager, etc.
    Unavailable = 20,
    // The device can be activated, but is currently idle and not connected to a network.
    Disconnected = 30,
    // The device is preparing the connection to the network. This may include operations like
    // changing the MAC address, setting physical link properties, and anything else required to
    // connect to the requested network.
    Prepare = 40,
    // The device is connecting to the requested network. This may include operations like
    // associating with the Wi-Fi AP, dialing the modem, connecting to the remote Bluetooth
    // device, etc.
    Config = 50,
    // The device requires more information to continue connecting to the requested network. This
    // includes secrets like WiFi passphrases, login passwords, PIN codes, etc.
    NeedAuth = 60,
    // The device is requesting IPv4 and/or IPv6 addresses and routing information from the
    // network.
    IpConfig = 70,
    // The device is checking whether further action is required for the requested network
    // connection. This may include checking whether only local network access is available,
    // whether a captive portal is blocking access to the Internet, etc.
    IpCheck = 80,
    // The device is waiting for a secondary connection (like a VPN) which must activated before
    // the device can be activated
    Secondaries = 90,
    // The device has a network connection, either local or global.
    Activated = 100,
    // A disconnection from the current network connection was requested, and the device is
    // cleaning up resources used for that connection. The network connection may still be valid.
    Deactivating = 110,
    // The device failed to connect to the requested network and is cleaning up the connection
    // request
    Failed = 120,
}

/// Reason for a device state change.
#[derive(Deserialize_repr, Type, OwnedValue, PartialEq, Debug)]
#[repr(u32)]
pub enum DeviceStateReason {
    // No reason given.
    None = 0,
    // Unknown error.
    Unknown = 1,
    // Device is now managed.
    NowManaged = 2,
    // Device is now unmanaged.
    NowUnmanaged = 3,
    // The device could not be readied for configuration.
    ConfigFailed = 4,
    // IP configuration could not be reserved (no available address, timeout, etc).
    IpConfigUnavailable = 5,
    // The IP config is no longer valid.
    IpConfigExpired = 6,
    // Secrets were required, but not provided.
    NoSecrets = 7,
    // 802.1x supplicant disconnected.
    SupplicantDisconnect = 8,
    // 802.1x supplicant configuration failed.
    SupplicantConfigFailed = 9,
    // 802.1x supplicant failed.
    SupplicantFailed = 10,
    // 802.1x supplicant took too long to authenticate.
    SupplicantTimeout = 11,
    // PPP service failed to start.
    PppStartFailed = 12,
    // PPP service disconnected.
    PppDisconnect = 13,
    // PPP failed.
    PppFailed = 14,
    // DHCP client failed to start.
    DhcpStartFailed = 15,
    // DHCP client error.
    DhcpError = 16,
    // DHCP client failed.
    DhcpFailed = 17,
    // Shared connection service failed to start.
    SharedStartFailed = 18,
    // Shared connection service failed.
    SharedFailed = 19,
    // AutoIP service failed to start.
    AutoipStartFailed = 20,
    // AutoIP service error.
    AutoipError = 21,
    // AutoIP service failed.
    AutoipFailed = 22,
    // The line is busy.
    ModemBusy = 23,
    // No dial tone.
    ModemNoDialTone = 24,
    // No carrier could be established.
    ModemNoCarrier = 25,
    // The dialing request timed out.
    ModemDialTimeout = 26,
    // The dialing attempt failed.
    ModemDialFailed = 27,
    // Modem initialization failed.
    ModemInitFailed = 28,
    // Failed to select the specified APN.
    GsmApnFailed = 29,
    // Not searching for networks.
    GsmRegistrationNotSearching = 30,
    // Network registration denied.
    GsmRegistrationDenied = 31,
    // Network registration timed out.
    GsmRegistrationTimeout = 32,
    // Failed to register with the requested network.
    GsmRegistrationFailed = 33,
    // PIN check failed.
    GsmPinCheckFailed = 34,
    // Necessary firmware for the device may be missing.
    FirmwareMissing = 35,
    // The device was removed.
    Removed = 36,
    // NetworkManager went to sleep.
    Sleeping = 37,
    // The device's active connection disappeared.
    ConnectionRemoved = 38,
    // Device disconnected by user or client.
    UserRequested = 39,
    // Carrier/link changed.
    Carrier = 40,
    // The device's existing connection was assumed.
    ConnectionAssumed = 41,
    // The supplicant is now available.
    SupplicantAvailable = 42,
    // The modem could not be found.
    ModemNotFound = 43,
    // The Bluetooth connection failed or timed out.
    BtFailed = 44,
    // GSM Modem's SIM Card not inserted.
    GsmSimNotInserted = 45,
    // GSM Modem's SIM Pin required.
    GsmSimPinRequired = 46,
    // GSM Modem's SIM Puk required.
    GsmSimPukRequired = 47,
    // GSM Modem's SIM wrong.
    GsmSimWrong = 48,
    // InfiniBand device does not support connected mode.
    InfinibandMode = 49,
    // A dependency of the connection failed.
    DependencyFailed = 50,
    // Problem with the RFC 2684 Ethernet over ADSL bridge.
    Br2684Failed = 51,
    // ModemManager not running.
    ModemManagerUnavailable = 52,
    // The Wi-Fi network could not be found.
    SsidNotFound = 53,
    // A secondary connection of the base connection failed.
    SecondaryConnectionFailed = 54,
    // DCB or FCoE setup failed.
    DcbFcoeFailed = 55,
    // teamd control failed.
    TeamdControlFailed = 56,
    // Modem failed or no longer available.
    ModemFailed = 57,
    // Modem now ready and available.
    ModemAvailable = 58,
    // SIM PIN was incorrect.
    SimPinIncorrect = 59,
    // New connection activation was enqueued.
    NewActivation = 60,
    // the device's parent changed.
    ParentChanged = 61,
    // the device parent's management changed.
    ParentManagedChanged = 62,
    // problem communicating with Open vSwitch database.
    OvsdbFailed = 63,
    // a duplicate IP address was detected.
    IpAddressDuplicate = 64,
    // The selected IP method is not supported.
    IpMethodUnsupported = 65,
    // configuration of SR-IOV parameters failed.
    SriovConfigurationFailed = 66,
    // The Wi-Fi P2P peer could not be found.
    PeerNotFound = 67,
    // The device handler dispatcher returned an error. Since: 1.46
    DeviceHandlerFailed = 68,
    // The device is unmanaged because the device type is unmanaged by default. Since: 1.48
    UnmanagedByDefault = 69,
    // The device is unmanaged because it is an external device and is unconfigured (down or
    // without addresses). Since: 1.48
    UnmanagedExternalDown = 70,
    // The device is unmanaged because the link is not initialized by udev. Since: 1.48
    UnmanagedLinkNotInit = 71,
    // The device is unmanaged because NetworkManager is quitting. Since: 1.48
    UnmanagedQuitting = 72,
    // The device is unmanaged because networking is disabled or the system is suspended. Since:
    // 1.48
    UnmanagedSleeping = 73,
    // The device is unmanaged by user decision in NetworkManager.conf ('unmanaged' in a [device*]
    // section). Since: 1.48
    UnmanagedUserConf = 74,
    // The device is unmanaged by explicit user decision (e.g. 'nmcli device set $DEV managed
    // no'). Since: 1.48
    UnmanagedUserExplicit = 75,
    // The device is unmanaged by user decision via settings plugin ('unmanaged-devices' for
    // keyfile or 'NMcONTROLLED=no' for ifcfg-rh). Since: 1.48
    UnmanagedUserSettings = 76,
    // The device is unmanaged via udev rule. Since: 1.48
    UnmanagedUserUdev = 77,
}
