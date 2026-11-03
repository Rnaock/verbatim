//! Chiffrement de la clé API.
//!
//! Sous Windows : DPAPI, portée utilisateur courant, avec la même entropie
//! applicative que les versions Python. Un fichier `key.dpapi` écrit par la v1
//! ou la v2 se relit donc tel quel.

use std::io;

#[cfg_attr(not(windows), allow(dead_code))]
const ENTROPY: &[u8] = b"verbatim/elevenlabs/v1";

#[cfg(windows)]
mod imp {
    use super::ENTROPY;
    use std::{io, ptr};
    use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    pub fn run(protect: bool, data: &[u8]) -> io::Result<Vec<u8>> {
        let input = blob(data);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
        // SAFETY : les blobs d'entrée pointent vers des tranches vivantes pendant
        // l'appel ; le blob de sortie est alloué par Windows et libéré par LocalFree.
        let ok = unsafe {
            if protect {
                CryptProtectData(&input, ptr::null(), &entropy, ptr::null(), ptr::null(), 0, &mut out)
            } else {
                CryptUnprotectData(&input, ptr::null_mut(), &entropy, ptr::null(), ptr::null(), 0, &mut out)
            }
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe { LocalFree(out.pbData as HLOCAL) };
        Ok(bytes)
    }
}

#[cfg(not(windows))]
mod imp {
    //! Remplaçant pour développer et tester sous Linux. Verbatim n'est livré
    //! que pour Windows : ce brouillage n'offre AUCUNE protection.
    use std::io;
    pub fn run(_protect: bool, data: &[u8]) -> io::Result<Vec<u8>> {
        Ok(data.iter().map(|b| b ^ 0x5A).collect())
    }
}

pub fn protect(data: &[u8]) -> io::Result<Vec<u8>> {
    imp::run(true, data)
}

pub fn unprotect(data: &[u8]) -> io::Result<Vec<u8>> {
    imp::run(false, data)
}

pub const IS_REAL: bool = cfg!(windows);
