use crate::PublicKey;
use crate::hash::{Blake2bHash, Blake2bHasher, Hasher, SerializeContent};
use std::convert::From;
use std::char;
use std::io;
use std::iter::Iterator;
use hex::FromHex;

create_typed_array!(Address, u8, 20);
hash_typed_array!(Address);
add_hex_io_fns_typed_arr!(Address, Address::SIZE);

#[derive(Debug)]
pub enum FriendlyAddressError {
    WrongCountryCode,
    WrongLength,
    InvalidChecksum
}

impl Address {
    const CCODE: &'static str = "NQ";
    const NIMIQ_ALPHABET: &'static str = "0123456789ABCDEFGHJKLMNPQRSTUVXY";

    pub fn from_user_friendly_address(friendly_addr: &String) -> Result<Address, FriendlyAddressError> {
        let friendly_addr_wospace = str::replace(friendly_addr, " ", "");

        if friendly_addr_wospace[0..2].to_uppercase() != Address::CCODE {
            return Err(FriendlyAddressError::WrongCountryCode);
        }
        if friendly_addr_wospace.len() != 36 {
            return Err(FriendlyAddressError::WrongLength);
        }
        let mut twisted_str = String::with_capacity(friendly_addr_wospace.len());
        twisted_str.push_str(&friendly_addr_wospace[4..]);
        twisted_str.push_str(&friendly_addr_wospace[..4]);
        if Address::iban_check(&twisted_str) != 1 {
            return Err(FriendlyAddressError::InvalidChecksum);
        }

        let mut spec = data_encoding::Specification::new();
        spec.symbols.push_str(Address::NIMIQ_ALPHABET);
        let encoding = spec.encoding().unwrap();

        let b_vec = encoding.decode(friendly_addr_wospace[4..].as_bytes()).unwrap();
        let mut b = [0; 20];
        b.copy_from_slice(&b_vec[..b_vec.len()]);
        return Ok(Address(b));
    }

    pub fn to_user_friendly_address(&self) -> String {
        let mut spec = data_encoding::Specification::new();
        spec.symbols.push_str(Address::NIMIQ_ALPHABET);
        let encoding = spec.encoding().unwrap();

        let base32 = encoding.encode(&self.0);
        let check_string = "00".to_string() + &(98 - Address::iban_check(&(base32.clone() + Address::CCODE + "00"))).to_string();
        let check = check_string.chars().skip(check_string.len() - 2).take(2).collect::<String>();
        let friendly_addr = Address::CCODE.to_string() + &check + &base32;
        let mut friendly_spaces = String::with_capacity(36+8);
        for i in 0..9 {
            friendly_spaces.push_str(&friendly_addr.chars().skip(4*i).take(4).collect::<String>());
            if i != 8 {
                friendly_spaces.push_str(" ");
            }
        }
        return friendly_spaces;
    }

    fn iban_check(s: &String) -> u32 {
        let mut num = String::with_capacity(s.len() * 2);
        for c in s.chars() {
            let code = c.to_uppercase().collect::<String>().chars().next().unwrap() as u32; // TODO meh
            if code >= 48 && code <=57 {
                num.push(char::from_u32(code).unwrap());
            } else {
                num.push_str(&(code - 55).to_string());
            }
        }
        let mut tmp: String = "".to_string();
        for i in 0..(f32::ceil(num.len() as f32 / 6.0) as usize) {
            let num_substr = num.chars().skip(i*6).take(6).collect::<String>();
            let num_tmp_sub = tmp.clone() + &num_substr;
            tmp = (num_tmp_sub.parse::<u32>().unwrap() % 97).to_string();
        }

        return tmp.parse::<u32>().unwrap();
    }
}

impl From<Blake2bHash> for Address {
    fn from(hash: Blake2bHash) -> Self {
        let hash_arr: [u8; 32] = hash.into();
        return Address::from(&hash_arr[0..Address::len()]);
    }
}

impl<'a> From<&'a PublicKey> for Address {
    fn from(public_key: &'a PublicKey) -> Self {
        let hash = Blake2bHasher::default().digest(public_key.as_bytes());
        return Address::from(hash);
    }
}

#[test]
fn it_computes_friendly_addresses() {
    let mut addr = Address::from([0u8; Address::SIZE]);
    assert_eq!(addr.to_user_friendly_address(), "NQ07 0000 0000 0000 0000 0000 0000 0000 0000");

    let mut addr_bytes : [u8; Address::SIZE] = [0; Address::SIZE];
    addr_bytes.clone_from_slice(&::hex::decode("e9910f2452419823dc2e5534633210074ae9527f").unwrap()[0..]);
    addr = Address::from(addr_bytes);
    assert_eq!(addr.to_user_friendly_address(), "NQ97 V68G X92J 86C2 7P1E ALS6 6CGG 0V5E JLKY");

    addr_bytes.clone_from_slice(&::hex::decode("2987c28c1ff373ba1e18a9a2efe6dc101ee25ed9").unwrap()[0..]);
    addr = Address::from(addr_bytes);
    assert_eq!(addr.to_user_friendly_address(), "NQ05 563U 530Y XDRT L7GQ M6HE YRNU 20FE 4PNR");

    let addr2 = Address::from_user_friendly_address(&"NQ05 563U 530Y XDRT L7GQ M6HE YRNU 20FE 4PNR".to_string()).unwrap();
    assert_eq!(addr.0, addr2.0);
    assert_eq!(addr.to_user_friendly_address(), addr2.to_user_friendly_address());
}
