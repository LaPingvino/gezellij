//! Loopback IPv6 (ULA) addressing for Gezellij services.
//!
//! The idea in one line: **give every service its own address instead of its own port**.
//!
//! Ports are a global, flat namespace on `127.0.0.1`: every service that wants to be "the web
//! thing" has to be talked out of `:8080` and into some arbitrary number nobody remembers. IPv6
//! makes that unnecessary. A single /64 out of the Unique Local Address range (`fd00::/8`, RFC
//! 4193) routed *locally* on `lo` gives us 2^64 loopback addresses, so every service can bind the
//! very same well-known port on an address of its own:
//!
//! ```text
//! api   -> [fd49:2e1c:0b7a::9f2c:...]:8080
//! blog  -> [fd49:2e1c:0b7a::41d8:...]:8080
//! ```
//!
//! The prefix is generated **once per installation** (RFC 4193 §3.2.2 asks for a pseudo-random
//! global ID so that two hosts that are later bridged do not collide) and persisted in
//! `<config dir>/network.json`. Nothing here is ever hardcoded.
//!
//! Making the prefix usable needs exactly one privileged command, once:
//!
//! ```text
//! sudo ip -6 route add local fdxx:xxxx:xxxx::/64 dev lo
//! ```
//!
//! After that the kernel accepts *any* address in the prefix as a local address, so services can
//! `bind()` them without `ip addr add` per service.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::net::{Ipv6Addr, SocketAddrV6, UdpSocket};
use std::path::{Path, PathBuf};

use super::services::ServiceDefinition;

/// Name of the file (inside the config dir) holding this installation's ULA prefix.
pub const NETWORK_FILE_NAME: &str = "network.json";

/// The well-known port every Gezellij service may bind on its own address.
pub fn default_port() -> u16 {
    8080
}

/// A /64 Unique Local Address prefix, as defined by RFC 4193:
///
/// ```text
/// | 7 bits |1|  40 bits   |  16 bits  |          64 bits           |
/// | 1111110|L| global ID  | subnet ID |        interface ID        |
/// ```
///
/// We always set `L` (locally assigned), which makes the first byte `0xfd`, and always use subnet
/// ID 0 - the per-service distinction lives in the interface ID, not in the subnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UlaPrefix {
    /// the 40-bit pseudo-random global ID
    global_id: [u8; 5],
    /// the 16-bit subnet ID (always 0 for us, but parsed and preserved)
    subnet: u16,
}

impl UlaPrefix {
    /// Generate a fresh prefix with a pseudo-random global ID.
    ///
    /// The randomness comes from a v4 UUID (`uuid` is already a dependency and is backed by the
    /// platform RNG), of which we keep 40 bits.
    pub fn generate() -> Self {
        let uuid = uuid::Uuid::new_v4();
        let bytes = uuid.as_bytes();
        let mut global_id = [0u8; 5];
        global_id.copy_from_slice(&bytes[..5]);
        UlaPrefix {
            global_id,
            subnet: 0,
        }
    }
    /// The first 8 bytes of every address in this prefix.
    pub fn network_bytes(&self) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        bytes[0] = 0xfd;
        bytes[1..6].copy_from_slice(&self.global_id);
        bytes[6..8].copy_from_slice(&self.subnet.to_be_bytes());
        bytes
    }
    /// The network address itself (`fdxx:xxxx:xxxx::`).
    pub fn network_address(&self) -> Ipv6Addr {
        let mut octets = [0u8; 16];
        octets[..8].copy_from_slice(&self.network_bytes());
        Ipv6Addr::from(octets)
    }
    /// The address with the given 64-bit interface id.
    pub fn address_with_interface_id(&self, interface_id: [u8; 8]) -> Ipv6Addr {
        let mut octets = [0u8; 16];
        octets[..8].copy_from_slice(&self.network_bytes());
        octets[8..].copy_from_slice(&interface_id);
        Ipv6Addr::from(octets)
    }
    /// `<prefix>::1` - the address we use to probe whether the prefix is routed.
    pub fn probe_address(&self) -> Ipv6Addr {
        self.address_with_interface_id([0, 0, 0, 0, 0, 0, 0, 1])
    }
}

impl std::fmt::Display for UlaPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/64", self.network_address())
    }
}

impl From<UlaPrefix> for String {
    fn from(prefix: UlaPrefix) -> String {
        prefix.to_string()
    }
}

impl std::str::FromStr for UlaPrefix {
    type Err = String;
    /// Accepts `fdxx:xxxx:xxxx::/64` as well as the bare network address.
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (address, len) = match s.split_once('/') {
            Some((address, len)) => (address, Some(len)),
            None => (s, None),
        };
        if let Some(len) = len {
            if len.trim() != "64" {
                return Err(format!("only /64 ULA prefixes are supported, got /{}", len));
            }
        }
        let address: Ipv6Addr = address
            .parse()
            .map_err(|e| format!("{:?} is not an IPv6 address: {}", address, e))?;
        let octets = address.octets();
        if octets[0] != 0xfd {
            return Err(format!(
                "{} is not a locally assigned ULA prefix (RFC 4193 requires it to start with fd)",
                address
            ));
        }
        if octets[8..].iter().any(|b| *b != 0) {
            return Err(format!(
                "{} has a non-zero interface id; a /64 prefix must end in ::",
                address
            ));
        }
        let mut global_id = [0u8; 5];
        global_id.copy_from_slice(&octets[1..6]);
        Ok(UlaPrefix {
            global_id,
            subnet: u16::from_be_bytes([octets[6], octets[7]]),
        })
    }
}

impl TryFrom<String> for UlaPrefix {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NetworkFile {
    prefix: UlaPrefix,
}

fn network_file(config_dir: &Path) -> PathBuf {
    config_dir.join(NETWORK_FILE_NAME)
}

/// Read the installation's prefix, if one was ever generated.
///
/// A present-but-broken `network.json` is an error rather than a reason to generate a new prefix:
/// silently rotating it would invalidate the `ip -6 route` the user already installed.
pub fn load_prefix(config_dir: &Path) -> io::Result<Option<UlaPrefix>> {
    match fs::read_to_string(network_file(config_dir)) {
        Ok(json) => {
            let parsed: NetworkFile = serde_json::from_str(&json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(Some(parsed.prefix))
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read the installation's prefix, generating (and persisting) one on first use.
pub fn load_or_create_prefix(config_dir: &Path) -> io::Result<UlaPrefix> {
    if let Some(prefix) = load_prefix(config_dir)? {
        return Ok(prefix);
    }
    let prefix = UlaPrefix::generate();
    save_prefix(config_dir, &prefix)?;
    Ok(prefix)
}

/// Persist a prefix (write-then-rename, so a crash never leaves half a file behind).
pub fn save_prefix(config_dir: &Path, prefix: &UlaPrefix) -> io::Result<PathBuf> {
    fs::create_dir_all(config_dir)?;
    let path = network_file(config_dir);
    let json = serde_json::to_string_pretty(&NetworkFile { prefix: *prefix })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = config_dir.join(format!(".{}.tmp", NETWORK_FILE_NAME));
    fs::write(&tmp, format!("{}\n", json))?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// The 64-bit interface id of a service, derived from its (opaque) service id.
///
/// The human-readable name deliberately plays no part: renaming a service should not move it, and
/// two installations with the same service names should not end up with related addresses. The
/// all-zero interface id is reserved (subnet-router anycast), so it is bumped to 1.
pub fn interface_id_for(service_id: &str) -> [u8; 8] {
    let mut hasher = Sha256::new();
    hasher.update(b"gezellij-service-address:");
    hasher.update(service_id.as_bytes());
    let digest = hasher.finalize();
    let mut interface_id = [0u8; 8];
    interface_id.copy_from_slice(&digest[..8]);
    if interface_id.iter().all(|b| *b == 0) {
        interface_id[7] = 1;
    }
    interface_id
}

/// The address a service with this id gets inside the prefix. Deterministic.
pub fn service_address(prefix: &UlaPrefix, service_id: &str) -> Ipv6Addr {
    prefix.address_with_interface_id(interface_id_for(service_id))
}

/// The socket a service should bind, or `None` when it did not ask for an address of its own.
pub fn bind_address_for(prefix: &UlaPrefix, service: &ServiceDefinition) -> Option<SocketAddrV6> {
    if !service.bind_ip {
        return None;
    }
    let address = service_address(prefix, &service.address_id());
    Some(SocketAddrV6::new(address, default_port(), 0, 0))
}

/// The URL form of a bind address (`http://[addr]:port`).
pub fn bind_url(address: &SocketAddrV6) -> String {
    format!("http://[{}]:{}", address.ip(), address.port())
}

/// Is the prefix actually routed to this machine?
///
/// The honest test is the one the services themselves will perform: try to `bind()` an address
/// from the prefix. Without `ip -6 route add local <prefix> dev lo` the kernel does not consider
/// those addresses local and the bind fails with `EADDRNOTAVAIL`; with the route in place any
/// address in the /64 binds. (Reading `/proc/net/ipv6_route` would only tell us what the routing
/// table *says*; binding tells us what the kernel will actually let a service do.) A UDP socket on
/// port 0 is used so nothing is disturbed and the probe never collides with a running service.
pub fn is_prefix_routed_locally(prefix: &UlaPrefix) -> bool {
    UdpSocket::bind(SocketAddrV6::new(prefix.probe_address(), 0, 0, 0)).is_ok()
}

/// The one-time (and permanent) root setup for a prefix.
pub fn setup_instructions(prefix: &UlaPrefix) -> String {
    format!(
        "\
Gezellij gives each service its own loopback IPv6 address out of {prefix}
(a locally assigned ULA prefix, RFC 4193, generated once for this installation).

For the kernel to accept those addresses as local, add the route once as root:

    sudo ip -6 route add local {prefix} dev lo

Check it with:

    ip -6 route show table local | grep {network}

To make it survive a reboot, install a tiny system unit
(/etc/systemd/system/gezellij-ula.service):

    [Unit]
    Description=Gezellij loopback ULA prefix
    After=network-pre.target
    Before=network.target

    [Service]
    Type=oneshot
    RemainAfterExit=yes
    ExecStart=/usr/bin/ip -6 route add local {prefix} dev lo
    ExecStop=/usr/bin/ip -6 route del local {prefix} dev lo

    [Install]
    WantedBy=multi-user.target

    sudo systemctl daemon-reload && sudo systemctl enable --now gezellij-ula.service

(If your network is managed by systemd-networkd you can instead add the same route to the `lo`
.network file; the unit above works regardless of who manages `lo`.)
",
        prefix = prefix,
        network = prefix.network_address(),
    )
}

/// The hostname we suggest for a service in exported reverse-proxy / hosts config.
pub fn hostname_for(service_name: &str) -> String {
    format!("{}.localhost", service_name)
}

/// A Caddyfile site block pointing a hostname at a service's address.
pub fn caddy_snippet(hostname: &str, address: &Ipv6Addr, port: u16) -> String {
    format!(
        "{hostname} {{\n\treverse_proxy [{address}]:{port}\n}}\n",
        hostname = hostname,
        address = address,
        port = port,
    )
}

/// An `/etc/hosts` line for a service's address.
pub fn hosts_line(address: &Ipv6Addr, name: &str) -> String {
    format!("{}\t{}\n", address, name)
}

/// Output format of `zellij service net-export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum, Default)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum NetExportFormat {
    /// Caddyfile site blocks (`<name>.localhost -> reverse_proxy [addr]:8080`)
    #[default]
    Caddy,
    /// `/etc/hosts` lines
    Hosts,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::command::RestartPolicy;

    #[test]
    fn generated_prefix_is_a_subnet_zero_ula() {
        for _ in 0..16 {
            let prefix = UlaPrefix::generate();
            let octets = prefix.network_address().octets();
            assert_eq!(octets[0], 0xfd, "must be inside fd00::/8");
            assert_eq!(&octets[6..8], &[0, 0], "subnet id must be 0");
            assert!(octets[8..].iter().all(|b| *b == 0), "must be a /64 network");
            assert!(prefix.to_string().ends_with("/64"));
        }
        // two prefixes in a row should differ (40 random bits)
        assert_ne!(UlaPrefix::generate(), UlaPrefix::generate());
    }

    #[test]
    fn prefix_display_parse_roundtrip() {
        let prefix = UlaPrefix::generate();
        let rendered = prefix.to_string();
        assert_eq!(rendered.parse::<UlaPrefix>().unwrap(), prefix);
        // the bare network address parses too
        assert_eq!(
            prefix
                .network_address()
                .to_string()
                .parse::<UlaPrefix>()
                .unwrap(),
            prefix
        );
        // and it round-trips through JSON
        let json = serde_json::to_string(&prefix).unwrap();
        assert_eq!(serde_json::from_str::<UlaPrefix>(&json).unwrap(), prefix);

        assert!("2001:db8::/64".parse::<UlaPrefix>().is_err());
        assert!("not an address".parse::<UlaPrefix>().is_err());
        assert!("fd00:1:2:3:4::/64".parse::<UlaPrefix>().is_err());
        assert!("fd00:1:2::/48".parse::<UlaPrefix>().is_err());
    }

    #[test]
    fn addresses_are_deterministic_per_id_and_distinct() {
        let prefix = UlaPrefix::generate();
        let one = service_address(&prefix, "1f0c8a1d");
        let two = service_address(&prefix, "1f0c8a1d");
        let other = service_address(&prefix, "b3de0091");
        assert_eq!(one, two, "the same id always maps to the same address");
        assert_ne!(one, other, "different ids map to different addresses");
        // the address is inside the prefix
        assert_eq!(&one.octets()[..8], &prefix.network_bytes());
        // and never the reserved all-zero interface id
        assert_ne!(one, prefix.network_address());
    }

    #[test]
    fn bind_address_follows_the_bind_ip_flag() {
        let prefix = UlaPrefix::generate();
        let mut service =
            ServiceDefinition::new("api", vec!["true".into()], None, RestartPolicy::No);
        assert_eq!(bind_address_for(&prefix, &service), None);
        service.bind_ip = true;
        let address = bind_address_for(&prefix, &service).unwrap();
        assert_eq!(address.port(), default_port());
        assert_eq!(
            *address.ip(),
            service_address(&prefix, &service.address_id())
        );
        assert_eq!(
            bind_url(&address),
            format!("http://[{}]:8080", address.ip())
        );
    }

    #[test]
    fn old_definitions_without_an_id_get_a_stable_fallback() {
        let parsed: ServiceDefinition =
            serde_json::from_str(r#"{"name":"legacy","command":["true"]}"#).unwrap();
        assert!(parsed.id.is_empty());
        assert_eq!(parsed.address_id(), "name:legacy");
        let prefix = UlaPrefix::generate();
        assert_eq!(
            service_address(&prefix, &parsed.address_id()),
            service_address(&prefix, "name:legacy")
        );
    }

    #[test]
    fn prefix_persists_in_the_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_prefix(dir.path()).unwrap(), None);
        let created = load_or_create_prefix(dir.path()).unwrap();
        assert!(dir.path().join(NETWORK_FILE_NAME).is_file());
        assert_eq!(load_prefix(dir.path()).unwrap(), Some(created));
        // a second call must never mint a new prefix
        assert_eq!(load_or_create_prefix(dir.path()).unwrap(), created);
        // a corrupt file is an error, not a reason to rotate the prefix
        fs::write(dir.path().join(NETWORK_FILE_NAME), "{ not json").unwrap();
        assert!(load_prefix(dir.path()).is_err());
    }

    #[test]
    fn exports_contain_the_bracketed_address() {
        let prefix = UlaPrefix::generate();
        let address = service_address(&prefix, "some-id");
        let snippet = caddy_snippet(&hostname_for("api"), &address, default_port());
        assert!(
            snippet.contains(&format!("[{}]:8080", address)),
            "{}",
            snippet
        );
        assert!(snippet.starts_with("api.localhost {"));
        assert!(snippet.contains("reverse_proxy"));
        assert_eq!(
            hosts_line(&address, "api.localhost"),
            format!("{}\tapi.localhost\n", address)
        );
    }

    #[test]
    fn setup_instructions_mention_the_route_command() {
        let prefix = UlaPrefix::generate();
        let instructions = setup_instructions(&prefix);
        assert!(instructions.contains(&format!("ip -6 route add local {} dev lo", prefix)));
        assert!(instructions.contains("ip -6 route show table local"));
    }
}
