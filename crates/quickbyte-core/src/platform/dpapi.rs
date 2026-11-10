//! DPAPI (`CryptProtectData` / `CryptUnprotectData`), `CurrentUser` scope.

#[cfg(windows)]
pub fn protect(plain: &[u8], entropy: &[u8]) -> Option<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData};
    let input = CRYPT_INTEGER_BLOB { cbData: plain.len() as u32, pbData: plain.as_ptr() as *mut u8 };
    let ent = CRYPT_INTEGER_BLOB { cbData: entropy.len() as u32, pbData: entropy.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: the input blobs borrow live slices for the duration of the call;
    // the output blob is allocated by DPAPI and released with LocalFree below.
    unsafe {
        CryptProtectData(&input, None, Some(&ent), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output).ok()?;
        Some(take_blob(output))
    }
}

#[cfg(windows)]
pub fn unprotect(cipher: &[u8], entropy: &[u8]) -> Option<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData};
    let input = CRYPT_INTEGER_BLOB { cbData: cipher.len() as u32, pbData: cipher.as_ptr() as *mut u8 };
    let ent = CRYPT_INTEGER_BLOB { cbData: entropy.len() as u32, pbData: entropy.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `protect`.
    unsafe {
        CryptUnprotectData(&input, None, Some(&ent), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output).ok()?;
        Some(take_blob(output))
    }
}

#[cfg(windows)]
unsafe fn take_blob(blob: windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> Vec<u8> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    // SAFETY: DPAPI returned `cbData` valid bytes at `pbData`, owned by us until LocalFree.
    unsafe {
        let bytes = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(blob.pbData as *mut _)));
        bytes
    }
}

#[cfg(not(windows))]
pub fn protect(_plain: &[u8], _entropy: &[u8]) -> Option<Vec<u8>> {
    None
}

#[cfg(not(windows))]
pub fn unprotect(_cipher: &[u8], _entropy: &[u8]) -> Option<Vec<u8>> {
    None
}
