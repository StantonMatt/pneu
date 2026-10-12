use crate::{
    Hash, MAX_CANDIDATE_ADDRESSES, MAX_PAIRING_URI_BYTES, MAX_TIMESTAMP, ProtocolError, Result,
    require, string_limit,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    net::{IpAddr, Ipv6Addr},
};

const QUERY_ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingUri {
    pub version: u32,
    pub name: String,
    pub fingerprint: Hash,
    pub addresses: Vec<String>,
    pub token: Hash,
    pub expires_at: u64,
}

impl PairingUri {
    /// Syntactic validation only: callers enforce supported version and expiry.
    pub fn validate(&self) -> Result<()> {
        require(self.version > 0, "pairing version must be positive")?;
        string_limit(&self.name, 1, 128, "name")?;
        require(self.expires_at <= MAX_TIMESTAMP, "invalid expiry timestamp")?;
        require(
            !self.addresses.is_empty() && self.addresses.len() <= MAX_CANDIDATE_ADDRESSES,
            "expected 1..=8 candidate addresses",
        )?;
        let mut seen = HashSet::new();
        for address in &self.addresses {
            validate_address(address)?;
            require(seen.insert(address), "duplicate address")?;
        }
        Ok(())
    }

    /// Canonical query order is v, name, fp, repeated addr, token, exp.
    pub fn build(&self) -> Result<String> {
        self.validate()?;
        let encode = |s: &str| utf8_percent_encode(s, QUERY_ENCODE).to_string();
        let mut uri = format!(
            "pneu://pair?v={}&name={}&fp={}",
            self.version,
            encode(&self.name),
            self.fingerprint
        );
        for address in &self.addresses {
            uri.push_str(&format!("&addr={}", encode(address)));
        }
        uri.push_str(&format!("&token={}&exp={}", self.token, self.expires_at));
        require(uri.len() <= MAX_PAIRING_URI_BYTES, "pairing URI too long")?;
        Ok(uri)
    }

    pub fn parse(uri: &str) -> Result<Self> {
        let invalid = |reason: &str| ProtocolError::InvalidUri(reason.into());
        if uri.len() > MAX_PAIRING_URI_BYTES {
            return Err(invalid("too long"));
        }
        let query = uri
            .strip_prefix("pneu://pair?")
            .ok_or_else(|| invalid("expected pneu://pair? query"))?;
        let mut version = None;
        let mut name = None;
        let mut fingerprint = None;
        let mut addresses = Vec::new();
        let mut token = None;
        let mut expires_at = None;
        let mut seen = HashSet::new();
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').ok_or_else(|| invalid("missing '='"))?;
            let key = decode_component(key)?;
            let value = decode_component(value)?;
            if key != "addr" && !seen.insert(key.clone()) {
                return Err(invalid("duplicate singleton key"));
            }
            match key.as_str() {
                "v" => {
                    version = Some(
                        decimal(&value)?
                            .parse::<u32>()
                            .map_err(|_| invalid("invalid version"))?,
                    )
                }
                "name" => name = Some(value),
                "fp" => fingerprint = Some(Hash::new(value)?),
                "addr" => {
                    if addresses.len() == MAX_CANDIDATE_ADDRESSES {
                        return Err(invalid("too many addresses"));
                    }
                    addresses.push(value);
                }
                "token" => token = Some(Hash::new(value)?),
                "exp" => {
                    expires_at = Some(
                        decimal(&value)?
                            .parse::<u64>()
                            .map_err(|_| invalid("invalid expiry"))?,
                    )
                }
                _ => return Err(invalid("unknown query key")),
            }
        }
        let parsed = Self {
            version: version.ok_or_else(|| invalid("missing v"))?,
            name: name.ok_or_else(|| invalid("missing name"))?,
            fingerprint: fingerprint.ok_or_else(|| invalid("missing fp"))?,
            addresses,
            token: token.ok_or_else(|| invalid("missing token"))?,
            expires_at: expires_at.ok_or_else(|| invalid("missing exp"))?,
        };
        parsed.validate()?;
        Ok(parsed)
    }
}

fn decimal(value: &str) -> Result<&str> {
    require(
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        "expected decimal digits",
    )?;
    Ok(value)
}

fn decode_component(raw: &str) -> Result<String> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            require(
                i + 2 < bytes.len()
                    && bytes[i + 1].is_ascii_hexdigit()
                    && bytes[i + 2].is_ascii_hexdigit(),
                "malformed percent escape",
            )?;
            i += 3;
        } else {
            require(
                b.is_ascii_alphanumeric() || b"-._~+".contains(&b),
                "query component must be percent encoded",
            )?;
            i += 1;
        }
    }
    let raw = raw.replace('+', " ");
    percent_decode_str(&raw)
        .decode_utf8()
        .map(|s| s.into_owned())
        .map_err(|_| ProtocolError::InvalidUri("invalid UTF-8".into()))
}

/// Validates host:port without performing DNS or accepting URI/userinfo syntax.
pub fn validate_address(address: &str) -> Result<()> {
    let (host, port) = if let Some(rest) = address.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .ok_or_else(|| ProtocolError::InvalidUri("invalid IPv6 endpoint".into()))?;
        require(host.parse::<Ipv6Addr>().is_ok(), "invalid IPv6 literal")?;
        (host, port)
    } else {
        let (host, port) = address
            .rsplit_once(':')
            .ok_or_else(|| ProtocolError::InvalidUri("missing address port".into()))?;
        require(
            !host.contains(':') && !host.is_empty() && host.len() <= 253,
            "invalid host",
        )?;
        if host.parse::<IpAddr>().is_err() {
            require(
                !host.bytes().all(|b| b.is_ascii_digit() || b == b'.'),
                "invalid IPv4 literal",
            )?;
            for label in host.split('.') {
                require(
                    !label.is_empty()
                        && label.len() <= 63
                        && label.as_bytes()[0].is_ascii_alphanumeric()
                        && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                    "invalid DNS label",
                )?;
            }
        }
        (host, port)
    };
    require(!host.is_empty(), "empty host")?;
    decimal(port)?;
    require(
        port.parse::<u16>().is_ok_and(|v| v > 0),
        "port must be 1..=65535",
    )
}
