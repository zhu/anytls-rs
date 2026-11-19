//! Certificate analysis and information extraction

use crate::util::{AnyTlsError, Result};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use x509_parser::prelude::*;

/// Certificate information
#[derive(Debug, Clone)]
pub struct CertificateInfo {
    /// Subject (certificate owner)
    pub subject: String,
    /// Issuer (who signed the certificate)
    pub issuer: String,
    /// Serial number
    pub serial_number: String,
    /// Valid from (Not Before)
    pub not_before: SystemTime,
    /// Valid until (Not After)
    pub not_after: SystemTime,
    /// Signature algorithm
    pub signature_algorithm: String,
    /// Public key algorithm
    pub public_key_algorithm: String,
    /// Subject Alternative Names
    pub san_names: Vec<String>,
    /// Whether certificate is self-signed
    pub is_self_signed: bool,
    /// Snapshot at parse time; use `days_remaining()` for live monitoring.
    pub days_until_expiry: i64,
}

/// Certificate status
#[derive(Debug, Clone, PartialEq)]
pub enum CertStatus {
    /// Certificate is valid
    Valid,
    /// Certificate is expiring soon (days remaining)
    ExpiringWarning(u64),
    /// Certificate has expired
    Expired,
    /// Certificate is invalid (reason)
    Invalid(String),
}

impl CertificateInfo {
    /// Parse certificate from PEM file
    pub fn from_pem_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut file = File::open(&path).map_err(|e| {
            AnyTlsError::Tls(format!(
                "Failed to open certificate file {:?}: {}",
                path.as_ref(),
                e
            ))
        })?;

        let mut pem_data = Vec::new();
        file.read_to_end(&mut pem_data).map_err(|e| {
            AnyTlsError::Tls(format!(
                "Failed to read certificate file {:?}: {}",
                path.as_ref(),
                e
            ))
        })?;

        Self::from_pem_bytes(&pem_data)
    }

    /// Parse certificate from PEM bytes
    pub fn from_pem_bytes(pem_data: &[u8]) -> Result<Self> {
        // Use rustls-pemfile to parse PEM
        let mut reader = BufReader::new(pem_data);
        let certs = rustls_pemfile::certs(&mut reader)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| AnyTlsError::Tls(format!("Failed to parse PEM certificates: {}", e)))?;

        if certs.is_empty() {
            return Err(AnyTlsError::Tls("No certificates found in PEM data".into()));
        }

        // Parse X.509 certificate from the first certificate
        let (_, cert) = X509Certificate::from_der(certs[0].as_ref())
            .map_err(|e| AnyTlsError::Tls(format!("Failed to parse X.509 certificate: {}", e)))?;

        Self::from_x509(&cert)
    }

    /// Extract information from X.509 certificate
    fn from_x509(cert: &X509Certificate) -> Result<Self> {
        // Extract subject
        let subject = cert.subject().to_string();

        // Extract issuer
        let issuer = cert.issuer().to_string();

        // Extract serial number
        let serial_number = cert
            .serial
            .to_str_radix(16)
            .chars()
            .collect::<Vec<_>>()
            .chunks(2)
            .map(|chunk| chunk.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join(":");

        // Extract validity period
        let not_before = asn1_time_to_system_time(&cert.validity().not_before)?;
        let not_after = asn1_time_to_system_time(&cert.validity().not_after)?;

        let days_until_expiry = remaining_days(not_after, SystemTime::now());

        // Extract signature algorithm
        let signature_algorithm = format!("{:?}", cert.signature_algorithm.algorithm);

        // Extract public key algorithm
        let public_key_algorithm = format!("{:?}", cert.public_key().algorithm.algorithm);

        // Extract SANs (Subject Alternative Names)
        let mut san_names = Vec::new();
        if let Ok(Some(san_ext)) = cert.subject_alternative_name() {
            for name in &san_ext.value.general_names {
                match name {
                    GeneralName::DNSName(dns) => {
                        san_names.push(dns.to_string());
                    }
                    GeneralName::IPAddress(ip) => {
                        san_names.push(format!("{:?}", ip));
                    }
                    _ => {}
                }
            }
        }

        // Check if self-signed
        let is_self_signed = cert.subject() == cert.issuer();

        Ok(CertificateInfo {
            subject,
            issuer,
            serial_number,
            not_before,
            not_after,
            signature_algorithm,
            public_key_algorithm,
            san_names,
            is_self_signed,
            days_until_expiry,
        })
    }

    /// Get certificate status using the current clock, including validity bounds.
    pub fn status(&self, warning_days: u64) -> CertStatus {
        self.status_at(SystemTime::now(), warning_days)
    }

    /// Evaluate validity at a particular time without relying on the parse-time snapshot.
    pub fn status_at(&self, now: SystemTime, warning_days: u64) -> CertStatus {
        if now >= self.not_after {
            CertStatus::Expired
        } else if now < self.not_before {
            CertStatus::Invalid("Certificate is not valid yet".into())
        } else {
            let days = remaining_days(self.not_after, now) as u64;
            if days < warning_days {
                CertStatus::ExpiringWarning(days)
            } else {
                CertStatus::Valid
            }
        }
    }

    /// Days remaining now (negative immediately after expiration).
    pub fn days_remaining(&self) -> i64 {
        remaining_days(self.not_after, SystemTime::now())
    }

    /// Check the absolute validity boundary, not a rounded day count.
    pub fn is_expired(&self) -> bool {
        SystemTime::now() >= self.not_after
    }

    /// Check if a currently valid certificate is expiring soon.
    pub fn is_expiring_soon(&self, warning_days: u64) -> bool {
        matches!(self.status(warning_days), CertStatus::ExpiringWarning(_))
    }

    /// Format certificate info for display
    pub fn display(&self) -> String {
        let mut output = String::new();
        output.push_str(&format!("Subject: {}\n", self.subject));
        output.push_str(&format!("Issuer: {}\n", self.issuer));
        output.push_str(&format!("Serial Number: {}\n", self.serial_number));
        output.push_str(&format!(
            "Valid From: {}\n",
            format_system_time(self.not_before)
        ));
        output.push_str(&format!(
            "Valid Until: {}\n",
            format_system_time(self.not_after)
        ));
        output.push_str(&format!("Days Until Expiry: {}\n", self.days_remaining()));
        output.push_str(&format!(
            "Signature Algorithm: {}\n",
            self.signature_algorithm
        ));
        output.push_str(&format!(
            "Public Key Algorithm: {}\n",
            self.public_key_algorithm
        ));
        output.push_str(&format!("Self-Signed: {}\n", self.is_self_signed));
        if !self.san_names.is_empty() {
            output.push_str(&format!("SANs: {}\n", self.san_names.join(", ")));
        }
        output
    }

    /// Format certificate info as a single-line summary
    pub fn summary(&self) -> String {
        format!(
            "CN={}, Issuer={}, expires in {} days",
            extract_cn(&self.subject).unwrap_or("unknown"),
            extract_cn(&self.issuer).unwrap_or("unknown"),
            self.days_remaining()
        )
    }
}

fn remaining_days(not_after: SystemTime, now: SystemTime) -> i64 {
    match not_after.duration_since(now) {
        Ok(duration) => (duration.as_secs() / 86400) as i64,
        // Rounding toward zero previously hid expiration for the first 24 hours.
        Err(error) => -(error.duration().as_secs().div_ceil(86400).max(1) as i64),
    }
}

/// Convert ASN.1 time to SystemTime
fn asn1_time_to_system_time(asn1_time: &ASN1Time) -> Result<SystemTime> {
    // Convert to Unix timestamp
    let unix_timestamp = asn1_time.timestamp();

    if unix_timestamp < 0 {
        return Err(AnyTlsError::Tls(
            "Invalid timestamp (before UNIX epoch)".into(),
        ));
    }

    Ok(UNIX_EPOCH + Duration::from_secs(unix_timestamp as u64))
}

/// Format SystemTime as human-readable string
fn format_system_time(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let secs = duration.as_secs();
            let datetime = chrono::DateTime::from_timestamp(secs as i64, 0)
                .unwrap_or(chrono::DateTime::UNIX_EPOCH);
            datetime.format("%Y-%m-%d %H:%M:%S UTC").to_string()
        }
        Err(_) => "Invalid time".to_string(),
    }
}

/// Extract Common Name (CN) from subject/issuer string
fn extract_cn(dn: &str) -> Option<&str> {
    // Simple extraction of CN= field
    for part in dn.split(',') {
        let part = part.trim();
        if let Some(cn) = part.strip_prefix("CN=") {
            return Some(cn);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_cn() {
        assert_eq!(extract_cn("CN=example.com, O=Test"), Some("example.com"));
        assert_eq!(
            extract_cn("O=Test, CN=example.com, C=US"),
            Some("example.com")
        );
        assert_eq!(extract_cn("O=Test, C=US"), None);
    }

    #[test]
    fn test_extract_cn_with_spaces() {
        // Leading/trailing spaces in the whole string
        assert_eq!(extract_cn(" CN=example.com "), Some("example.com"));
        // Spaces around commas
        assert_eq!(
            extract_cn("O=Test , CN=example.com , C=US"),
            Some("example.com")
        );
    }

    fn test_certificate(now: SystemTime) -> CertificateInfo {
        CertificateInfo {
            subject: "CN=test".into(),
            issuer: "CN=issuer".into(),
            serial_number: "123".into(),
            not_before: now - Duration::from_secs(86400),
            not_after: now + Duration::from_secs(86400 * 90),
            signature_algorithm: "SHA256".into(),
            public_key_algorithm: "RSA".into(),
            san_names: vec![],
            is_self_signed: false,
            days_until_expiry: 90,
        }
    }

    #[test]
    fn status_advances_without_reloading_certificate() {
        let now = SystemTime::now();
        let info = test_certificate(now);
        assert_eq!(info.status_at(now, 30), CertStatus::Valid);
        assert_eq!(
            info.status_at(now + Duration::from_secs(86400 * 70), 30),
            CertStatus::ExpiringWarning(20)
        );
        assert_eq!(info.status_at(info.not_after, 30), CertStatus::Expired);
        assert_eq!(
            info.status_at(info.not_after + Duration::from_secs(1), 30),
            CertStatus::Expired
        );
        assert!(matches!(
            info.status_at(info.not_before - Duration::from_secs(1), 30),
            CertStatus::Invalid(_)
        ));
        // The public compatibility snapshot remains unchanged throughout.
        assert_eq!(info.days_until_expiry, 90);
    }

    #[test]
    fn first_second_after_expiration_is_expired() {
        let now = SystemTime::now();
        let mut info = test_certificate(now);
        info.not_after = now - Duration::from_secs(1);
        assert!(info.is_expired());
        assert_eq!(info.days_remaining(), -1);
        assert!(!info.is_expiring_soon(30));
        assert!(info.display().contains("Days Until Expiry: -1"));
    }

    #[test]
    fn test_is_expiring_soon() {
        let info = CertificateInfo {
            subject: "CN=test".to_string(),
            issuer: "CN=issuer".to_string(),
            serial_number: "123".to_string(),
            not_before: SystemTime::now(),
            not_after: SystemTime::now() + Duration::from_secs(86400 * 20),
            signature_algorithm: "SHA256".to_string(),
            public_key_algorithm: "RSA".to_string(),
            san_names: vec![],
            is_self_signed: false,
            days_until_expiry: 20,
        };

        assert!(info.is_expiring_soon(30));
        assert!(!info.is_expiring_soon(10));
    }

    #[test]
    fn test_cert_summary() {
        let info = CertificateInfo {
            subject: "CN=example.com, O=Test Corp".to_string(),
            issuer: "CN=Test CA, O=Test".to_string(),
            serial_number: "123456".to_string(),
            not_before: SystemTime::now(),
            not_after: SystemTime::now() + Duration::from_secs(86400 * 90),
            signature_algorithm: "SHA256withRSA".to_string(),
            public_key_algorithm: "RSA".to_string(),
            san_names: vec!["example.com".to_string(), "www.example.com".to_string()],
            is_self_signed: false,
            days_until_expiry: 90,
        };

        let summary = info.summary();
        assert!(summary.contains("example.com"));
        assert!(summary.contains("Test CA"));
        assert!(summary.contains(&info.days_remaining().to_string()));
    }

    #[test]
    fn test_cert_display() {
        let info = CertificateInfo {
            subject: "CN=test.com".to_string(),
            issuer: "CN=CA".to_string(),
            serial_number: "abc123".to_string(),
            not_before: SystemTime::UNIX_EPOCH + Duration::from_secs(1000000),
            not_after: SystemTime::UNIX_EPOCH + Duration::from_secs(2000000),
            signature_algorithm: "SHA256".to_string(),
            public_key_algorithm: "RSA".to_string(),
            san_names: vec!["test.com".to_string()],
            is_self_signed: true,
            days_until_expiry: 100,
        };

        let display = info.display();
        assert!(display.contains("Subject: CN=test.com"));
        assert!(display.contains("Issuer: CN=CA"));
        assert!(display.contains("Serial Number: abc123"));
        assert!(display.contains("Self-Signed: true"));
        assert!(display.contains("SANs: test.com"));
    }
}
