use super::{Sha512Hash, SHA512_LENGTH};
use super::hmac::compute_hmac_sha512;
use byteorder::{BigEndian, WriteBytesExt};
use std::io::{Write, Error};

#[derive(Debug)]
pub enum Pbkdf2Error {
    KeyTooLong,
    IOError(Error),
}

pub fn compute_pbkdf2_sha512(password: &[u8], salt: &[u8], iterations: usize, derived_key_length: usize) -> Result<Vec<u8>, Pbkdf2Error> {
    // Following https://www.ietf.org/rfc/rfc2898.txt
    if (derived_key_length as u64) > (u32::max_value() as u64) * (Sha512Hash::len() as u64) {
        return Err(Pbkdf2Error::KeyTooLong);
    }

    let mut l = derived_key_length / Sha512Hash::len();
    if derived_key_length % Sha512Hash::len() != 0 {
        l += 1;
    }
    let r = derived_key_length - (l - 1) * Sha512Hash::len();

    let mut derived_key = Vec::with_capacity(derived_key_length);
    for i in 1..l+1 {
        let mut u: Vec<u8> = Vec::with_capacity(salt.len() + 4);
        u.write(salt).map_err(|e| Pbkdf2Error::IOError(e))?;
        u.write_u32::<BigEndian>(i as u32).map_err(|e| Pbkdf2Error::IOError(e))?;

        let mut t: [u8; SHA512_LENGTH] = compute_hmac_sha512(password, u.as_slice()).into();
        let mut u = t;
        for _ in 1..iterations {
            u = compute_hmac_sha512(password, &u[..]).into();
            for k in 0..Sha512Hash::len() {
                t[k] ^= u[k];
            }
        }

        if i < l {
            derived_key.write(&t[..]).map_err(|e| Pbkdf2Error::IOError(e))?;
        } else {
            derived_key.write(&t[..r]).map_err(|e| Pbkdf2Error::IOError(e))?;
        }
    }
    Ok(derived_key)
}
