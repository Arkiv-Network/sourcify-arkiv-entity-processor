//! The reference PostgreSQL database orders signatures using en_US.UTF-8,
//! not UTF-8 byte ordering. Never inherit the process's LANG/LC_COLLATE setting.
use anyhow::{ensure, Result};
use std::{cmp::Ordering, ffi::CString};

unsafe extern "C" {
    fn strcoll_l(
        left: *const libc::c_char,
        right: *const libc::c_char,
        locale: libc::locale_t,
    ) -> libc::c_int;
}

// Each projection owns its locale. No global locale mutation, shared raw
// pointers, or unsafe Send/Sync implementation is needed.
pub(crate) struct SignatureCollator(libc::locale_t);

impl SignatureCollator {
    pub(crate) fn new() -> Result<Self> {
        // SAFETY: a static NUL-terminated name, valid mask, and null base create
        // a new locale owned exclusively by this value.
        let locale = unsafe {
            libc::newlocale(
                libc::LC_COLLATE_MASK,
                c"en_US.UTF-8".as_ptr(),
                std::ptr::null_mut(),
            )
        };
        ensure!(
            !locale.is_null(),
            "signature collation requires the en_US.UTF-8 glibc locale; install locales and run localedef -i en_US -f UTF-8 en_US.UTF-8"
        );
        Ok(Self(locale))
    }

    pub(crate) fn sort(&self, signatures: impl IntoIterator<Item = String>) -> Result<Vec<String>> {
        let mut values = signatures
            .into_iter()
            .map(|signature| CString::new(signature).map_err(anyhow::Error::from))
            .collect::<Result<Vec<_>>>()?;
        values.sort_unstable_by(|left, right| {
            // SAFETY: both strings and this exclusively owned locale remain
            // alive throughout the call. strcoll_l only reads these arguments.
            let compared = unsafe { strcoll_l(left.as_ptr(), right.as_ptr(), self.0) };
            match compared.cmp(&0) {
                // PostgreSQL's deterministic collations break equal collation
                // weights by bytes. This also provides a total sort order.
                Ordering::Equal => left.as_bytes().cmp(right.as_bytes()),
                other => other,
            }
        });
        // CString was constructed from UTF-8 Rust strings, which were not changed.
        values.into_iter().map(|v| Ok(v.into_string()?)).collect()
    }
}

impl Drop for SignatureCollator {
    fn drop(&mut self) {
        // SAFETY: newlocale succeeded and this value uniquely owns the handle.
        unsafe { libc::freelocale(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn captured_database_order_including_case_and_punctuation() {
        let captures: Value =
            serde_json::from_str(include_str!("../tests/fixtures/signature-collation.json"))
                .unwrap();
        let collator = SignatureCollator::new().unwrap();
        let mut distinguished_from_binary = 0;
        for contract in captures["contracts"].as_array().unwrap() {
            for (kind, signatures) in contract["signatures"].as_object().unwrap() {
                let expected: Vec<String> = signatures
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s["signature"].as_str().unwrap().to_owned())
                    .collect();
                let mut binary = expected.clone();
                binary.sort();
                distinguished_from_binary += usize::from(binary != expected);
                assert_eq!(
                    collator.sort(binary).unwrap(),
                    expected,
                    "{} {kind}",
                    contract["address"]
                );
            }
        }
        assert!(distinguished_from_binary >= 10);
    }
}
