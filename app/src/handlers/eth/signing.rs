/*******************************************************************************
*   (c) 2022 Zondax GmbH
*
*  Licensed under the Apache License, Version 2.0 (the "License");
*  you may not use this file except in compliance with the License.
*  You may obtain a copy of the License at
*
*      http://www.apache.org/licenses/LICENSE-2.0
*
*  Unless required by applicable law or agreed to in writing, software
*  distributed under the License is distributed on an "AS IS" BASIS,
*  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
*  See the License for the specific language governing permissions and
*  limitations under the License.
********************************************************************************/
use core::mem::MaybeUninit;

use bolos::{
    crypto::bip32::BIP32Path,
    hash::{Hasher, Keccak},
};
use zemu_sys::{Show, ViewError, Viewable};

use crate::{
    constants::{ApduError as Error, MAX_BIP32_PATH_DEPTH},
    crypto::Curve,
    dispatcher::ApduHandler,
    handlers::resources::{BUFFER, NFT_INFO, PATH},
    parser::{DisplayableItem, EthTransaction, FromBytes},
    sys,
    utils::ApduBufferRead,
};

use super::utils::get_tx_rlp_len;
use super::utils::parse_bip32_eth;
use crate::utils::convert_der_to_rs;

pub struct Sign;

impl Sign {
    pub const SIGN_HASH_SIZE: usize = Keccak::<32>::DIGEST_LEN;

    fn get_derivation_info() -> Result<&'static BIP32Path<MAX_BIP32_PATH_DEPTH>, Error> {
        match unsafe { PATH.acquire(Self) } {
            Ok(Some(some)) => Ok(some),
            _ => Err(Error::ApduCodeConditionsNotSatisfied),
        }
    }

    //(actual_size, [u8; MAX_SIGNATURE_SIZE])
    #[inline(never)]
    pub fn sign<const LEN: usize>(
        path: &BIP32Path<LEN>,
        data: &[u8],
    ) -> Result<(usize, [u8; 100]), Error> {
        let sk = Curve.to_secret(path);

        let mut out = [0; 100];
        let sz = sk
            .sign(data, &mut out[..])
            .map_err(|_| Error::ExecutionError)?;

        Ok((sz, out))
    }

    #[inline(never)]
    fn digest(buffer: &[u8]) -> Result<[u8; Self::SIGN_HASH_SIZE], Error> {
        Keccak::<32>::digest(buffer).map_err(|_| Error::ExecutionError)
    }

    #[inline(never)]
    pub fn start_sign(txdata: &'static [u8], flags: &mut u32) -> Result<u32, Error> {
        let mut tx = MaybeUninit::uninit();
        let rem =
            EthTransaction::from_bytes_into(txdata, &mut tx).map_err(|_| Error::DataInvalid)?;

        // some applications might append data at the end of an encoded
        // transaction, so skip it to get the right hash.
        let to_hash = txdata.len() - rem.len();
        let to_hash = &txdata[..to_hash];
        let unsigned_hash = Self::digest(to_hash)?;

        let tx = unsafe { tx.assume_init() };

        let ui = SignUI {
            hash: unsigned_hash,
            tx,
        };

        crate::show_ui!(ui.show(flags))
    }
}

impl ApduHandler for Sign {
    #[inline(never)]
    fn handle<'apdu>(
        flags: &mut u32,
        tx: &mut u32,
        buffer: ApduBufferRead<'apdu>,
    ) -> Result<(), Error> {
        sys::zemu_log_stack("EthSign::handle\x00");

        *tx = 0;

        // hw-app-eth encodes the packet type in p1
        // with 0x00 being init and 0x80 being next
        //
        // the end of the transmission is implicit based on the received data
        // an eth transaction is RLP encoded (https://ethereum.org/en/developers/docs/data-structures-and-encoding/rlp/#definition)
        // with the first byte being the version (0x01 EIP2930 or 0x02 EIP1559 or legacy if neither/missing)
        // and then a RLP list
        //
        // therefore, the data received self-describes how many bytes the app can expect and
        // when all data has been received

        let packet_type = buffer.p1();

        match packet_type {
            //init
            0x00 => {
                let payload = buffer.payload().map_err(|_| Error::WrongLength)?;
                //parse path to verify it's the data we expect
                let (rest, bip32_path) =
                    parse_bip32_eth(payload).map_err(|_| Error::DataInvalid)?;

                unsafe {
                    PATH.lock(Self)?.replace(bip32_path);
                }

                //parse the length of the RLP message
                let (read, to_read) = get_tx_rlp_len(rest)?;
                let len = core::cmp::min(to_read as usize + read, rest.len());

                //write the rest to the swapping buffer so we persist this data
                let buffer = unsafe { BUFFER.lock(Self)? };
                buffer.reset();

                buffer
                    .write(&rest[..len])
                    .map_err(|_| Error::ExecutionError)?;

                //if the number of bytes read and the number of bytes to read
                // is the same as what we read...
                if to_read as usize + read - len == 0 {
                    //then we actually had all bytes in this tx!
                    // we should sign directly
                    *tx = Self::start_sign(buffer.read_exact(), flags)?;
                }

                Ok(())
            }
            //next
            0x80 => {
                let payload = buffer.payload().map_err(|_| Error::WrongLength)?;

                let buffer = unsafe { BUFFER.acquire(Self)? };

                //we could unwrap here as this data should be guaranteed correct
                // we read back what we wrote to see how many bytes we expect
                // to have to collect
                let (read, to_read) = get_tx_rlp_len(buffer.read_exact())?;

                // let's ignore the little header at the start
                let rlp_read = buffer.read_exact().len() - read;

                //either the entire buffer of the remaining bytes we expect
                let missing = to_read as usize - rlp_read;
                let len = core::cmp::min(missing, payload.len());

                buffer
                    .write(&payload[..len])
                    .map_err(|_| Error::ExecutionError)?;

                if missing - len == 0 {
                    //we read all the missing bytes so we can proceed with the signature
                    // nwo
                    *tx = Self::start_sign(buffer.read_exact(), flags)?;
                }

                Ok(())
            }
            _ => Err(Error::InvalidP1P2),
        }
    }
}

pub(crate) struct SignUI {
    hash: [u8; Sign::SIGN_HASH_SIZE],
    tx: EthTransaction<'static>,
}

impl Viewable for SignUI {
    fn num_items(&mut self) -> Result<u8, ViewError> {
        Ok(self.tx.num_items() as _)
    }

    #[inline(never)]
    fn render_item(
        &mut self,
        item_n: u8,
        title: &mut [u8],
        message: &mut [u8],
        page: u8,
    ) -> Result<u8, ViewError> {
        self.tx.render_item(item_n, title, message, page)
    }

    fn accept(&mut self, out: &mut [u8]) -> (usize, u16) {
        let path = match Sign::get_derivation_info() {
            Err(e) => return (0, e as _),
            Ok(k) => k,
        };

        let (sig_size, mut sig) = match Sign::sign(path, &self.hash[..]) {
            Err(e) => return (0, e as _),
            Ok(k) => k,
        };

        //reset globals to avoid skipping `Init`
        if let Err(e) = cleanup_globals() {
            return (0, e as _);
        }

        let mut tx = 0;

        //write signature as VRS
        //write V, which is the LSB of the firsts byte
        out[tx] = sig[0] & 0x01;
        tx += 1;

        //reset to 0x30 for the conversion
        sig[0] = 0x30;
        {
            let mut r = [0; 33];
            let mut s = [0; 33];

            //write as R S (V written earlier)
            // this will write directly to buffer
            match convert_der_to_rs(&sig[..sig_size], &mut r, &mut s) {
                Ok(_) => {
                    //format R and S by only having 32 bytes each,
                    // skipping the first byte if necessary
                    // if we have less than 32 bytes we just have 0s at the start
                    // this is consistent with the fact that in `convert_der_to_rs`
                    // we put the bytes at the end of the buffer first
                    let r = &r[1..];
                    let s = &s[1..];

                    out[tx..][..32].copy_from_slice(r);
                    tx += 32;

                    out[tx..][..32].copy_from_slice(s);
                    tx += 32;
                }
                Err(_) => return (0, Error::ExecutionError as _),
            }
        }
        // before returning It is necessary to write the right V
        // component as it depends on the chainID(lowest byte) and the
        // parity of the last byte of the S component, this procedure is
        // defined by EIP-155.
        let chain_id_byte = self.tx.chain_id_low_byte();

        if chain_id_byte == 0 {
            out[0] = out[tx - 1] & 0x01;
        } else {
            out[0] = (out[tx - 1] & 0x01) + (chain_id_byte << 1) + 35;
        }

        (tx, Error::Success as _)
    }

    fn reject(&mut self, _: &mut [u8]) -> (usize, u16) {
        let _ = cleanup_globals();
        (0, Error::CommandNotAllowed as _)
    }
}

fn cleanup_globals() -> Result<(), Error> {
    unsafe {
        if let Ok(path) = PATH.acquire(Sign) {
            path.take();

            //let's release the lock for the future
            let _ = PATH.release(Sign);
        }

        if let Ok(buffer) = BUFFER.acquire(Sign) {
            buffer.reset();

            //let's release the lock for the future
            let _ = BUFFER.release(Sign);
        }

        if let Ok(info) = NFT_INFO.acquire(Sign) {
            info.take();

            //let's release the lock for the future
            let _ = NFT_INFO.release(Sign);
        }
    }

    //if we failed to aquire then someone else is using it anyways
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rlp_decoder() {
        let data = hex::decode("02f878018402a8af41843b9aca00850d8c7b50e68303d090944a2962ac08962819a8a17661970e3c0db765565e8817addd0864728ae780c080a01e514f7fc78197c66589083cc8fd06376bae627a4080f5fb58d52d90c0df340da049b048717f215e622c93722ff5b1e38e1d1a4ab9e26a39183969a34a5f8dea75").unwrap();

        let (read, to_read) = get_tx_rlp_len(&data).expect("unable to minimally parse tx data");

        assert_eq!(read, 3);
        assert_eq!(to_read, 0x78);
    }
}
